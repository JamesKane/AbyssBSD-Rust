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

/// The top of the stack: the page above it is kept for the shared page
/// (signal trampoline, time keeping), as FreeBSD's `USRSTACK`.
pub(crate) const USRSTACK: u64 = USER_END - PAGE_SIZE;
/// The stack mapped below it (FreeBSD grows the stack on demand up to
/// `RLIMIT_STACK`; ours is mapped whole, and faulted in as used).
pub(crate) const STACK_SIZE: u64 = 8 << 20;
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
}

fn space_error(e: SpaceError) -> Errno {
    match e {
        SpaceError::OutOfMemory => Errno::ENOMEM,
        SpaceError::Map(_) => Errno::ENOEXEC,
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

/// Loads the memory-disk file at `path` into `space` (which must be empty)
/// with arguments `argv` and environment `envp`.
pub(crate) fn load(
    s: &Sleepable,
    space: &AddressSpace,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
) -> Result<Image, Errno> {
    let file = crate::md::file(path).ok_or(Errno::ENOENT)?;
    if file.mode & 0o111 == 0 {
        return Err(Errno::EACCES);
    }
    let elf = Elf::parse(file.data).map_err(|_| Errno::ENOEXEC)?;
    if elf.machine != MACHINE || elf.file_type != FileType::Executable || !branded(&elf) {
        return Err(Errno::ENOEXEC);
    }
    let object = crate::md::object(&file);
    let (phoff, phnum) = elf.program_headers();
    let mut phdr = None;
    let mut stack_prot = Prot::RW;
    for seg in elf.segments() {
        let seg = seg.map_err(|_| Errno::ENOEXEC)?;
        match seg.kind {
            SegmentKind::Interp => return Err(Errno::ENOEXEC),
            SegmentKind::Phdr => phdr = Some(seg.vaddr),
            SegmentKind::GnuStack if seg.perms.exec => stack_prot = Prot::ALL,
            SegmentKind::Load => {
                map_segment(s, space, &object, &seg)?;
                if phdr.is_none() && seg.offset <= phoff && phoff < seg.offset + seg.file_size {
                    phdr = Some(seg.vaddr + (phoff - seg.offset));
                }
            }
            _ => {}
        }
    }
    crate::shared_page::map(s, space).map_err(space_error)?;
    let stack_base = VirtAddr::new(USRSTACK - STACK_SIZE);
    space
        .map_anonymous(s, Some(stack_base), stack_base, STACK_SIZE, stack_prot)
        .map_err(space_error)?;
    let aux = Aux {
        phdr: phdr.unwrap_or(0),
        phnum: phnum as u64,
        entry: elf.entry,
        flags: elf.flags,
        stack_prot,
    };
    let stack = build_stack(s, space, path, argv, envp, &aux)?;
    Ok(registers(elf.entry, stack))
}

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
    if mem_end > USER_END || start < crate::mm::space::USER_START {
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

/// Builds the initial stack below [`USRSTACK`]; returns the address of
/// `argc`.
fn build_stack(
    s: &Sleepable,
    space: &AddressSpace,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &Aux,
) -> Result<u64, Errno> {
    let strings: usize = argv.iter().chain(envp).map(|a| a.len() + 1).sum();
    if strings > ARG_MAX {
        return Err(Errno::E2BIG);
    }
    let down8 = |v: u64| v & !7;
    let ps_strings = USRSTACK - size_of::<PsStrings>() as u64;
    let execpath = down8(ps_strings - (path.len() as u64 + 1));
    let canary = execpath - CANARY_LEN as u64;
    let pagesizes = down8(canary - (MAXPAGESIZES * 8) as u64);
    let strings_at = down8(pagesizes - strings as u64);
    let auxv = down8(strings_at - at::COUNT * 16);
    let nvec = (argv.len() + 1 + envp.len() + 1) as u64;
    let vectors = auxv - nvec * 8;
    let argc_at = vectors - 8;

    // The image of [argc_at, USRSTACK), written in one go.
    let len = usize::try_from(USRSTACK - argc_at).map_err(|_| Errno::E2BIG)?;
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
        (at::TIMEKEEP, crate::shared_page::timekeep_address()),
        (at::USRSTACKBASE, USRSTACK),
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
const fn registers(entry: u64, stack: u64) -> Image {
    #[cfg(target_arch = "x86_64")]
    let sp = ((stack - 8) & !0xf) + 8;
    #[cfg(target_arch = "aarch64")]
    let sp = stack & !0xf;
    Image {
        entry,
        sp,
        arg: stack,
    }
}
