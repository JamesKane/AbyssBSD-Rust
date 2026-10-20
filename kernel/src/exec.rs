// SPDX-License-Identifier: BSD-2-Clause
//! The ELF image activator: loads a static FreeBSD ELF64 executable into an
//! address space and builds its initial stack (FreeBSD's `imgact_elf` and
//! `exec_copyout_strings`).
//!
//! Text and data map the file's memory object privately: its pages are
//! shared by every process running the file until one writes. The part
//! of the last file page past the data is zeroed (it is `bss`), and the
//! rest of `bss` is anonymous memory.
//!
//! The stack, from the top down, matches FreeBSD's: `struct ps_strings`,
//! the executable's path, the SSP canary, the page-size array, the
//! argument and environment strings, room for the auxiliary vector, the
//! `argv` and `envp` vectors, and `argc`, where the new program's stack
//! pointer (and first argument register) points.
//!
//! The layout is randomized as FreeBSD 15's defaults do for 64-bit
//! programs (`kern.elf64.aslr.*`): the image stays where it was linked,
//! anonymous mappings start at a random `anon_loc` between the end of the
//! data segment's largest size and the middle of the user half, and the
//! stack and the shared page are placed above the data limit with random
//! gaps; the stack's top also moves within its page. The top page of the
//! user half is a guard.
//!
//! Dynamic executables (`PT_INTERP`) wait for the run-time linker (P6).
//!
//! Behavior reference: sys/kern/imgact_elf.c, sys/kern/kern_exec.c,
//! sys/amd64/amd64/exec_machdep.c, sys/arm64/arm64/exec_machdep.c.

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::exec::{ELFOSABI_FREEBSD, MAXPAGESIZES, NT_FREEBSD_ABI_TAG, PsStrings, at};
use addr::VirtAddr;
use elf::{Elf, FileType, Machine, SegmentKind};
use sched::Sleepable;
use vm::PAGE_SIZE;
use vm::map::Prot;

use crate::mm::space::{AddressSpace, SpaceError, USER_END};

/// The top of what a program may map (FreeBSD's `USRSTACK`, which is
/// where the stack would end without randomization): the page above it is
/// a guard.
pub(crate) const USRSTACK: u64 = USER_END - PAGE_SIZE;
/// The stack (FreeBSD grows the stack on demand up to `RLIMIT_STACK`; ours
/// is mapped whole, and faulted in as used).
pub(crate) const STACK_SIZE: u64 = 8 << 20;
/// The default and largest `RLIMIT_DATA` (FreeBSD's `MAXDSIZ`); the
/// randomized layout starts above it.
#[cfg(target_arch = "x86_64")]
pub(crate) const MAXDSIZ: u64 = 32 << 30;
#[cfg(target_arch = "aarch64")]
pub(crate) const MAXDSIZ: u64 = 1 << 30;
/// Pages of random gap below a randomized stack or shared page (FreeBSD's
/// `aslr_pages_rnd_64[0]`).
const ASLR_GAP_PAGES: u64 = 0x1000;
/// `anon_loc`'s alignment: a superpage.
const ANON_ALIGN: u64 = 2 << 20;
/// FreeBSD's `ARG_MAX`: the most argument and environment bytes.
const ARG_MAX: usize = 2 * 256 * 1024;
/// Bytes of SSP canary (`sizeof(long) * 8`).
const CANARY_LEN: usize = 64;

#[cfg(target_arch = "x86_64")]
const MACHINE: Machine = Machine::X86_64;
#[cfg(target_arch = "aarch64")]
const MACHINE: Machine = Machine::Aarch64;

/// Where the new program starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Image {
    pub(crate) entry: u64,
    /// The initial stack pointer.
    pub(crate) sp: u64,
    /// The first argument register: the address of `argc`.
    pub(crate) arg: u64,
    pub(crate) layout: Layout,
}

/// Where an image's randomized parts went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Where `mmap` without an address starts looking (FreeBSD's
    /// `anon_loc`).
    pub(crate) anon_loc: u64,
    /// The top of the stack (`AT_USRSTACKBASE`).
    pub(crate) stack_top: u64,
    /// The shared page.
    pub(crate) shared_page: u64,
}

fn random_u64() -> u64 {
    let mut r = [0u8; 8];
    // Before the RNG is seeded, the layout is not randomized (FreeBSD
    // would block).
    let _ = crate::random::fill(&mut r);
    u64::from_le_bytes(r)
}

/// A random multiple of `align` in `[min, max)` (FreeBSD's `rnd_base`).
fn rnd_base(min: u64, max: u64, align: u64) -> Result<u64, Errno> {
    if min >= max || min.saturating_add(align) >= max {
        return Err(Errno::ENOEXEC);
    }
    let mut v = (min.next_multiple_of(align) + random_u64() % (max - min)) & !(align - 1);
    if v >= max {
        v -= align;
    }
    Ok(v)
}

/// Maps `len` bytes for a randomized region at or above `from`: the
/// lowest free range with room for a random gap below it, at a random
/// offset in that gap (FreeBSD's `vm_map_find` with `MAP_ASLR`), or else
/// the lowest free range.
fn place(s: &Sleepable, space: &AddressSpace, from: u64, len: u64) -> Result<u64, Errno> {
    let gap = ASLR_GAP_PAGES * PAGE_SIZE;
    let at = space
        .find_space(s, len + gap, VirtAddr::new(from))
        .filter(|a| a.get() >= from && a.get() + len + gap <= USRSTACK)
        .map(|a| a.get() + (random_u64() % ASLR_GAP_PAGES) * PAGE_SIZE)
        .or_else(|| {
            space
                .find_space(s, len, VirtAddr::new(from))
                .map(VirtAddr::get)
        })
        .filter(|&a| a + len <= USRSTACK)
        .ok_or(Errno::ENOMEM)?;
    Ok(at)
}

fn space_error(e: SpaceError) -> Errno {
    match e {
        SpaceError::OutOfMemory => Errno::ENOMEM,
        SpaceError::Map(_) => Errno::ENOEXEC,
        SpaceError::Io => Errno::EIO,
    }
}

const fn prot_of(p: elf::Perms) -> Prot {
    let mut prot = Prot::NONE;
    if p.read {
        prot = prot.union(Prot::READ);
    }
    if p.write {
        prot = prot.union(Prot::WRITE);
    }
    if p.exec {
        prot = prot.union(Prot::EXEC);
    }
    prot
}

/// Whether `elf` is a FreeBSD binary: by `EI_OSABI`, or by the ABI tag
/// note FreeBSD's toolchain also writes.
fn branded(elf: &Elf<'_>) -> bool {
    elf.os_abi == ELFOSABI_FREEBSD
        || elf.segments().flatten().any(|s| {
            s.kind == SegmentKind::Note
                && elf.notes(&s).is_ok_and(|mut n| {
                    n.any(|n| n.name == b"FreeBSD" && n.kind == NT_FREEBSD_ABI_TAG)
                })
        })
}

/// Loads `file` (found at `path`) into `space` (which must be empty) with
/// arguments `argv` and environment `envp`; `data_max` is the process's
/// hard `RLIMIT_DATA`.
pub(crate) fn load(
    s: &Sleepable,
    space: &AddressSpace,
    file: &vfs::Vnode,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    data_max: u64,
) -> Result<Image, Errno> {
    // A regular file the caller may execute (root needs an execute bit
    // too).
    if file.kind() != vfs::Kind::Regular {
        return Err(Errno::EACCES);
    }
    vfs::ops::access(
        &file.attr(s)?,
        file.kind(),
        &crate::fs::cred(),
        vfs::ops::VEXEC,
    )?;
    let object = crate::fs::object(s, file).ok_or(Errno::ENOEXEC)?;
    // The headers, program headers, and branding note sit at the start:
    // read that much (the memory disk's files are there already).
    let head;
    let data = match file.resident(s) {
        Some(d) => d,
        None => {
            let size = usize::try_from(file.size(s)?).unwrap_or(usize::MAX);
            let mut buf = alloc::vec![0u8; size.min(HEAD_MAX)];
            let mut r = vfs::ReadBuf::new(&mut buf);
            let n = file.read_at(s, 0, &mut r)?;
            buf.truncate(n);
            head = buf;
            &head[..]
        }
    };
    let elf = Elf::parse_head(data).map_err(|_| Errno::ENOEXEC)?;
    if elf.machine != MACHINE || elf.file_type != FileType::Executable || !branded(&elf) {
        return Err(Errno::ENOEXEC);
    }
    let (phoff, phnum) = elf.program_headers();
    let mut phdr = None;
    let mut stack_prot = Prot::RW;
    // The data segment's start (FreeBSD's `vm_daddr`): the writable
    // segment, or else the text.
    let (mut text, mut data) = (None, None);
    for seg in elf.segments() {
        let seg = seg.map_err(|_| Errno::ENOEXEC)?;
        match seg.kind {
            SegmentKind::Interp => return Err(Errno::ENOEXEC),
            SegmentKind::Phdr => phdr = Some(seg.vaddr),
            SegmentKind::GnuStack if seg.perms.exec => stack_prot = Prot::ALL,
            SegmentKind::Load => {
                map_segment(s, space, &object, &seg)?;
                let slot = if seg.perms.write {
                    &mut data
                } else {
                    &mut text
                };
                slot.get_or_insert(seg.vaddr);
                if phdr.is_none() && seg.offset <= phoff && phoff < seg.offset + seg.file_size {
                    phdr = Some(seg.vaddr + (phoff - seg.offset));
                }
            }
            _ => {}
        }
    }
    let daddr = data.or(text).ok_or(Errno::ENOEXEC)?;
    let brk_end = daddr
        .checked_add(data_max)
        .ok_or(Errno::ENOEXEC)?
        .next_multiple_of(PAGE_SIZE);
    let anon_loc = rnd_base(brk_end, USRSTACK / 2 + brk_end / 2, ANON_ALIGN)?;
    let guard = crate::mm::space::Anon {
        fixed: Some(VirtAddr::new(USRSTACK)),
        exclusive: true,
        len: PAGE_SIZE,
        prot: Prot::NONE,
        max_prot: Prot::NONE,
        ..crate::mm::space::Anon::default()
    };
    space.map_anon(s, &guard).map_err(space_error)?;
    let stack_base = place(s, space, brk_end, STACK_SIZE)?;
    space
        .map_anonymous(
            s,
            Some(VirtAddr::new(stack_base)),
            VirtAddr::new(stack_base),
            STACK_SIZE,
            stack_prot,
        )
        .map_err(space_error)?;
    // Randomized within the stack's top page, keeping pointer alignment.
    let stack_top = stack_base + STACK_SIZE - ((random_u64() % PAGE_SIZE) & !7);
    let shared_page = place(s, space, brk_end, PAGE_SIZE)?;
    crate::shared_page::map(s, space, shared_page).map_err(space_error)?;
    let layout = Layout {
        anon_loc,
        stack_top,
        shared_page,
    };
    let aux = Aux {
        phdr: phdr.unwrap_or(0),
        phnum: phnum as u64,
        entry: elf.entry,
        flags: elf.flags,
        stack_prot,
    };
    let stack = build_stack(s, space, path, argv, envp, &aux, &layout)?;
    Ok(registers(elf.entry, stack, layout))
}

/// How much of a file `exec` reads to parse its headers (FreeBSD reads a
/// page, then whatever the headers point at): everything it needs lies
/// in the first pages of any executable the base system has.
const HEAD_MAX: usize = 64 * 1024;

/// Maps one `PT_LOAD` segment.
fn map_segment(
    s: &Sleepable,
    space: &AddressSpace,
    object: &alloc::sync::Arc<vm::object::Object>,
    seg: &elf::Segment,
) -> Result<(), Errno> {
    let page = PAGE_SIZE;
    if seg.offset % page != seg.vaddr % page {
        return Err(Errno::ENOEXEC);
    }
    let prot = prot_of(seg.perms);
    let start = seg.vaddr - seg.vaddr % page;
    let file_end = seg.vaddr + seg.file_size;
    let mem_end = seg.vaddr + seg.mem_size;
    if mem_end > USRSTACK || start < crate::mm::space::USER_START {
        return Err(Errno::ENOEXEC);
    }
    let up = |v: u64| v.next_multiple_of(page);
    if seg.file_size > 0 {
        space
            .map_private(
                s,
                VirtAddr::new(start)..VirtAddr::new(up(file_end)),
                alloc::sync::Arc::clone(object),
                seg.offset / page,
                prot,
                Prot::ALL,
            )
            .map_err(space_error)?;
    }
    if mem_end > file_end {
        // The rest of the last file page is bss: zero it (a private copy).
        if seg.file_size > 0 && !file_end.is_multiple_of(page) {
            if !prot.contains(Prot::WRITE) {
                return Err(Errno::ENOEXEC);
            }
            let n = usize::try_from(up(file_end).min(mem_end) - file_end)
                .map_err(|_| Errno::ENOEXEC)?;
            space
                .poke(s, VirtAddr::new(file_end), &alloc::vec![0; n])
                .map_err(|_| Errno::ENOMEM)?;
        }
        let anon = if seg.file_size > 0 {
            up(file_end)
        } else {
            start
        };
        if up(mem_end) > anon {
            space
                .map_anonymous(
                    s,
                    Some(VirtAddr::new(anon)),
                    VirtAddr::new(anon),
                    up(mem_end) - anon,
                    prot,
                )
                .map_err(space_error)?;
        }
    }
    Ok(())
}

/// What the auxiliary vector reports about the image.
struct Aux {
    phdr: u64,
    phnum: u64,
    entry: u64,
    flags: u32,
    stack_prot: Prot,
}

/// Builds the initial stack below `layout.stack_top`; returns the address
/// of `argc`.
fn build_stack(
    s: &Sleepable,
    space: &AddressSpace,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &Aux,
    layout: &Layout,
) -> Result<u64, Errno> {
    let top = layout.stack_top;
    let strings: usize = argv.iter().chain(envp).map(|a| a.len() + 1).sum();
    if strings > ARG_MAX {
        return Err(Errno::E2BIG);
    }
    let down8 = |v: u64| v & !7;
    let ps_strings = top - size_of::<PsStrings>() as u64;
    let execpath = down8(ps_strings - (path.len() as u64 + 1));
    let canary = execpath - CANARY_LEN as u64;
    let pagesizes = down8(canary - (MAXPAGESIZES * 8) as u64);
    let strings_at = down8(pagesizes - strings as u64);
    let auxv = down8(strings_at - at::COUNT * 16);
    let nvec = (argv.len() + 1 + envp.len() + 1) as u64;
    let vectors = auxv - nvec * 8;
    let argc_at = vectors - 8;

    // The image of [argc_at, top), written in one go.
    let len = usize::try_from(top - argc_at).map_err(|_| Errno::E2BIG)?;
    let mut img = alloc::vec![0u8; len];
    let put = |img: &mut Vec<u8>, at: u64, bytes: &[u8]| {
        let o = usize::try_from(at - argc_at).unwrap_or(usize::MAX);
        img[o..o + bytes.len()].copy_from_slice(bytes);
    };
    let word = |img: &mut Vec<u8>, at: u64, v: u64| put(img, at, &v.to_le_bytes());

    word(&mut img, argc_at, argv.len() as u64);
    // Strings, and the vectors pointing at them.
    let mut str_at = strings_at;
    let mut vec_at = vectors;
    for (i, a) in argv.iter().chain(envp).enumerate() {
        if i == argv.len() {
            // The NULL ending argv.
            vec_at += 8;
        }
        put(&mut img, str_at, a);
        word(&mut img, vec_at, str_at);
        str_at += a.len() as u64 + 1;
        vec_at += 8;
    }
    let argv_at = vectors;
    let envp_at = vectors + (argv.len() as u64 + 1) * 8;

    put(&mut img, execpath, path);
    let mut c = [0u8; CANARY_LEN];
    // Before the RNG is seeded, the canary is zeros (FreeBSD would block).
    let _ = crate::random::fill(&mut c);
    put(&mut img, canary, &c);
    // Base pages only: no superpages for user memory yet.
    word(&mut img, pagesizes, PAGE_SIZE);

    let ncpus = sched::online_cpus() as u64;
    let entries = [
        (at::PHDR, aux.phdr),
        (at::PHENT, 56),
        (at::PHNUM, aux.phnum),
        (at::PAGESZ, PAGE_SIZE),
        (at::FLAGS, 0),
        (at::ENTRY, aux.entry),
        (at::BASE, 0),
        (at::EHDRFLAGS, u64::from(aux.flags)),
        (at::EXECPATH, execpath),
        (at::OSRELDATE, abi::sysctl::identity::OSRELDATE as u64),
        (at::CANARY, canary),
        (at::CANARYLEN, CANARY_LEN as u64),
        (at::NCPUS, ncpus),
        (at::PAGESIZES, pagesizes),
        (at::PAGESIZESLEN, (MAXPAGESIZES * 8) as u64),
        (at::STACKPROT, u64::from(aux.stack_prot.bits())),
        (at::BSDFLAGS, 0),
        (at::ARGC, argv.len() as u64),
        (at::ARGV, argv_at),
        (at::ENVC, envp.len() as u64),
        (at::ENVV, envp_at),
        (at::PS_STRINGS, ps_strings),
        (
            at::TIMEKEEP,
            crate::shared_page::timekeep_address(layout.shared_page),
        ),
        (at::USRSTACKBASE, top),
        (at::USRSTACKLIM, STACK_SIZE),
        (at::NULL, 0),
    ];
    for (i, (k, v)) in entries.into_iter().enumerate() {
        word(&mut img, auxv + i as u64 * 16, k);
        word(&mut img, auxv + i as u64 * 16 + 8, v);
    }

    let ps = PsStrings {
        ps_argvstr: argv_at,
        ps_nargvstr: u32::try_from(argv.len()).map_err(|_| Errno::E2BIG)?,
        ps_envstr: envp_at,
        ps_nenvstr: u32::try_from(envp.len()).map_err(|_| Errno::E2BIG)?,
        ..PsStrings::default()
    };
    word(&mut img, ps_strings, ps.ps_argvstr);
    put(&mut img, ps_strings + 8, &ps.ps_nargvstr.to_le_bytes());
    word(&mut img, ps_strings + 16, ps.ps_envstr);
    put(&mut img, ps_strings + 24, &ps.ps_nenvstr.to_le_bytes());

    space
        .poke(s, VirtAddr::new(argc_at), &img)
        .map_err(|_| Errno::ENOMEM)?;
    Ok(argc_at)
}

/// The entry registers for a stack whose `argc` is at `stack`.
const fn registers(entry: u64, stack: u64, layout: Layout) -> Image {
    #[cfg(target_arch = "x86_64")]
    let sp = ((stack - 8) & !0xf) + 8;
    #[cfg(target_arch = "aarch64")]
    let sp = stack & !0xf;
    Image {
        entry,
        sp,
        arg: stack,
        layout,
    }
}
