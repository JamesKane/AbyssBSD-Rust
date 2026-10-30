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
//! programs (`kern.elf64.aslr.*`): a position-independent executable
//! linked at 0 loads at a random base in the lower half of the space above
//! the data limit, a fixed one stays where it was linked, anonymous
//! mappings start at a random `anon_loc` between the end of the data
//! segment's largest size and the middle of what is left, and the stack
//! and the shared page are placed above the data limit with random gaps;
//! the stack's top also moves within its page. The top page of the user
//! half is then a guard. A binary whose feature-control note disables
//! ASLR gets FreeBSD's fixed layout: a PIE at `ET_DYN_LOAD_ADDR`, the
//! shared page in the top page, and the stack below it.
//!
//! A dynamic executable names its interpreter (`PT_INTERP`, base's
//! `/libexec/ld-elf.so.1`), which is loaded where an `mmap` would put it,
//! at a random page above the data limit, and started in the program's
//! place; the auxiliary vector tells it where the program is (`AT_PHDR`,
//! `AT_ENTRY`) and where it is itself (`AT_BASE`).
//!
//! Behavior reference: sys/kern/imgact_elf.c, sys/kern/kern_exec.c,
//! sys/amd64/amd64/exec_machdep.c, sys/arm64/arm64/exec_machdep.c.

use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use abi::errno::Errno;
use abi::exec::{
    ELFOSABI_FREEBSD, MAXPAGESIZES, NT_FREEBSD_ABI_TAG, NT_FREEBSD_FEATURE_CTL, PsStrings, at,
    bsdf, fctl,
};
use abi::resource::{Rlimit, rlimit};
use addr::VirtAddr;
use elf::{Elf, FileType, Machine, SegmentKind};
use sched::Sleepable;
use vm::PAGE_SIZE;
use vm::map::{ProcStack, Prot, SGROWSIZ};

use crate::mm::space::{AddressSpace, AnonKind, SpaceError, USER_END, USER_START};

/// The top of what a program may map (FreeBSD's `USRSTACK`, which is
/// where the stack would end without randomization): the page above it is
/// a guard.
pub(crate) const USRSTACK: u64 = USER_END - PAGE_SIZE;
/// The room exec reserves for the stack, and `RLIMIT_STACK`'s largest
/// value (FreeBSD's `MAXSSIZ`); the stack grows into it on demand.
#[cfg(target_arch = "x86_64")]
pub(crate) const MAXSSIZ: u64 = 512 << 20;
#[cfg(target_arch = "aarch64")]
pub(crate) const MAXSSIZ: u64 = 1 << 30;
/// `RLIMIT_STACK`'s default (`DFLSSIZ`).
#[cfg(target_arch = "x86_64")]
pub(crate) const DFLSSIZ: u64 = 8 << 20;
#[cfg(target_arch = "aarch64")]
pub(crate) const DFLSSIZ: u64 = 128 << 20;
/// Pages below every stack it never grows into
/// (`security.bsd.stack_guard_page`).
const STACK_GUARD_PAGES: u64 = 1;

/// The guard below a stack a program with feature-control word `fctl0`
/// maps (none if it opts out with `NT_FREEBSD_FCTL_STKGAP_DISABLE`).
/// (none if it opts out with `NT_FREEBSD_FCTL_STKGAP_DISABLE`, or the
/// process with `PROC_STACKGAP_CTL`).
pub(crate) const fn stack_guard(fctl0: u32, flag2: u32) -> u64 {
    if fctl0 & fctl::STKGAP_DISABLE != 0 || flag2 & crate::proc::p2::STKGAP_DISABLE != 0 {
        0
    } else {
        STACK_GUARD_PAGES * PAGE_SIZE
    }
}
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
pub(crate) struct Image {
    pub(crate) entry: u64,
    /// The initial stack pointer.
    pub(crate) sp: u64,
    /// The first argument register: the address of `argc`.
    pub(crate) arg: u64,
    pub(crate) layout: Layout,
    /// The interpreter, held as text while the program runs.
    pub(crate) interp: Option<crate::proc::Text>,
    /// The auxiliary vector, through `AT_NULL` (`kern.proc.auxv`).
    pub(crate) auxv: Vec<u8>,
}

/// Where an image's randomized parts went, and what its notes said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Where `mmap` without an address starts looking (FreeBSD's
    /// `anon_loc`).
    pub(crate) anon_loc: u64,
    /// The top of the stack (`AT_USRSTACKBASE`).
    pub(crate) stack_top: u64,
    /// The shared page.
    pub(crate) shared_page: u64,
    /// The ABI tag note's `__FreeBSD_version` (FreeBSD's `p_osrel`; 0
    /// without one).
    pub(crate) osrel: i32,
    /// The feature-control note's word (FreeBSD's `p_fctl0`).
    pub(crate) fctl0: u32,
    /// The map is randomized (`MAP_ASLR`).
    pub(crate) aslr: bool,
}

impl Layout {
    /// Whether the map is randomized (`PROC_ASLR_ACTIVE`).
    pub(crate) const fn randomized(&self) -> bool {
        self.aslr
    }
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

/// The process limits [`load`] works under.
pub(crate) type Limits = [Rlimit; rlimit::NLIMITS];

/// Finds the run-time linker a dynamic executable names (the process's
/// own lookup, from its root and working directory).
pub(crate) type Lookup<'a> = &'a dyn Fn(&[u8]) -> Result<Arc<vfs::Vnode>, Errno>;

/// Loads `file` (found at `path`) into `space` (which must be empty) with
/// arguments `argv` and environment `envp`, under the process's `limits`;
/// a dynamic executable's interpreter is found with `lookup` (FreeBSD's
/// `exec_elf64_imgact`).
#[allow(
    clippy::too_many_arguments,
    reason = "execve's parts, as FreeBSD's image_params"
)]
pub(crate) fn load(
    s: &Sleepable,
    space: &AddressSpace,
    file: &vfs::Vnode,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    limits: &Limits,
    flag2: u32,
    lookup: Lookup<'_>,
) -> Result<Image, Errno> {
    let object = open(s, file)?;
    let head = read_head(s, file)?;
    let elf = Elf::parse_head(&head).map_err(|_| Errno::ENOEXEC)?;
    let pie = match elf.file_type {
        FileType::Executable => false,
        FileType::Shared => true,
        FileType::Other(_) => return Err(Errno::ENOEXEC),
    };
    if elf.machine != MACHINE {
        return Err(Errno::ENOEXEC);
    }
    let headers = Headers::scan(&elf)?;
    let notes = Notes::of(&elf);
    if elf.os_abi != ELFOSABI_FREEBSD && notes.is_none() {
        return Err(Errno::ENOEXEC);
    }
    let Notes { osrel, fctl0 } = notes.unwrap_or_default();

    // Randomize the layout: a PIE linked at 0 loads at a random base, and
    // any image gets a random stack, shared page, and mmap base, unless
    // its feature-control note or `PROC_ASLR_CTL` opts out (or the latter
    // opts in).
    use crate::proc::p2;
    let aslr_off = fctl0 & fctl::ASLR_DISABLE != 0 || flag2 & p2::ASLR_DISABLE != 0;
    let mut et_dyn_addr = 0;
    let mut random_base = false;
    if pie && headers.baddr == 0 {
        if aslr_off || !ASLR.pie_enable.load(Ordering::Relaxed) {
            et_dyn_addr = ET_DYN_LOAD_ADDR;
        } else {
            random_base = true;
        }
    }
    let aslr = !aslr_off
        && (random_base
            || flag2 & p2::ASLR_ENABLE != 0
            || !pie && ASLR.enable.load(Ordering::Relaxed));
    let aslr_stack = aslr && ASLR.stack.load(Ordering::Relaxed);
    let aslr_shared_page = aslr && ASLR.shared_page.load(Ordering::Relaxed);

    let data_max = limits[rlimit::DATA as usize].rlim_max;
    let maxv = if aslr_stack {
        USRSTACK
    } else {
        USRSTACK.saturating_sub(limits[rlimit::STACK as usize].rlim_max)
    };
    if headers.mapsz >= maxv.saturating_sub(USER_START) {
        return Err(Errno::ENOEXEC);
    }
    if random_base {
        // The lower half of the space, above the data limit: the upper is
        // the interpreter's.
        let min = USER_START
            .saturating_add(headers.mapsz)
            .saturating_add(data_max);
        et_dyn_addr = rnd_base(min, maxv / 2, headers.maxalign)?;
    }
    load_sections(s, space, &object, &elf, et_dyn_addr)?;
    let daddr = enforce_limits(&elf, et_dyn_addr, limits)?;

    // The interpreter loads where `mmap(0, …)` would put it, leaving the
    // heap room to grow to its limit.
    let addr = daddr
        .checked_add(data_max)
        .filter(|&a| a < maxv)
        .ok_or(Errno::ENOEXEC)?
        .next_multiple_of(PAGE_SIZE);
    let anon_loc = if aslr {
        rnd_base(addr, maxv / 2 + addr / 2, ANON_ALIGN)?
    } else {
        addr
    };
    let entry = elf.entry.wrapping_add(et_dyn_addr);
    let mut interp_text = None;
    let (base, start) = match headers.interp {
        Some(name) => {
            let at = if aslr {
                rnd_base(addr, maxv / 2 + addr / 2, PAGE_SIZE)?
            } else {
                addr
            };
            let interp = lookup(name)?;
            // Text from now on, as FreeBSD's text mappings make it.
            interp_text = Some(crate::proc::Text::new(&interp)?);
            load_interp(s, space, &interp, at)?
        }
        None => (et_dyn_addr, entry),
    };

    let stack_prot = headers.stack_prot.unwrap_or(DEFAULT_STACK_PROT);
    // The stack reserves MAXSSIZ and grows into it (FreeBSD's
    // `exec_map_stack`).
    let stack_base = if aslr_stack {
        place(s, space, addr, MAXSSIZ)?
    } else {
        USRSTACK - MAXSSIZ
    };
    let stack = crate::mm::space::Anon {
        fixed: Some(VirtAddr::new(stack_base)),
        exclusive: true,
        len: MAXSSIZ,
        prot: stack_prot,
        kind: AnonKind::Stack {
            guard: stack_guard(fctl0, flag2),
        },
        ..crate::mm::space::Anon::default()
    };
    space.map_anon(s, &stack).map_err(space_error)?;
    space.set_proc_stack(
        s,
        ProcStack {
            area: VirtAddr::new(stack_base)..VirtAddr::new(stack_base + MAXSSIZ),
            size: SGROWSIZ,
        },
        limits[rlimit::STACK as usize].rlim_cur,
    );
    let mut stack_top = stack_base + MAXSSIZ;
    if aslr_stack {
        // Randomized within the stack's top page, keeping pointer
        // alignment.
        stack_top -= (random_u64() % PAGE_SIZE) & !7;
    }
    // A randomized shared page leaves a guard at the top of the space;
    // otherwise it is the top page.
    let shared_page = if aslr_shared_page {
        let guard = crate::mm::space::Anon {
            fixed: Some(VirtAddr::new(USRSTACK)),
            exclusive: true,
            len: PAGE_SIZE,
            prot: Prot::NONE,
            max_prot: Prot::NONE,
            kind: AnonKind::Guard,
            ..crate::mm::space::Anon::default()
        };
        space.map_anon(s, &guard).map_err(space_error)?;
        place(s, space, addr, PAGE_SIZE)?
    } else {
        USRSTACK
    };
    crate::shared_page::map(s, space, shared_page).map_err(space_error)?;
    let layout = Layout {
        anon_loc,
        stack_top,
        shared_page,
        osrel,
        fctl0,
        aslr,
    };
    // W^X: only when asked (`kern.elf64.allow_wx` is 1).
    space.set_wxorx(flag2 & p2::WXORX_ENABLE_EXEC != 0);
    let aux = Aux {
        phdr: headers.proghdr.wrapping_add(et_dyn_addr),
        phnum: headers.phnum as u64,
        entry,
        base,
        flags: elf.flags,
        stack_prot,
        stack_lim: limits[rlimit::STACK as usize].rlim_cur,
    };
    let (stack, auxv) = build_stack(s, space, path, argv, envp, &aux, &layout)?;
    Ok(Image {
        auxv,
        ..registers(start, stack, layout, interp_text)
    })
}

/// Where a PIE linked at 0 loads when it is not randomized (FreeBSD's
/// `ET_DYN_LOAD_ADDR`).
#[cfg(target_arch = "x86_64")]
const ET_DYN_LOAD_ADDR: u64 = 0x0102_1000;
#[cfg(target_arch = "aarch64")]
const ET_DYN_LOAD_ADDR: u64 = 0x10_0000;
/// The stack's protection when the image has no `PT_GNU_STACK` (the
/// sysvec's `sv_stackprot`).
#[cfg(target_arch = "x86_64")]
const DEFAULT_STACK_PROT: Prot = Prot::ALL;
#[cfg(target_arch = "aarch64")]
const DEFAULT_STACK_PROT: Prot = Prot::RW;
/// A program header's size (`e_phentsize`, which the parser checks).
const PHENT: u64 = 56;
/// The most program headers an image may have (FreeBSD 16's
/// `kern.elf64.phnums`; 15.1 has no node for it).
const PHNUMS: usize = 128;
/// The largest `PT_LOAD` alignment accepted: the largest superpage.
/// Larger ones are corrupt or malicious.
const MAX_SEGMENT_ALIGN: u64 = 1 << 30;
/// The longest `PT_INTERP`, with its NUL (`MAXPATHLEN`).
const MAXPATHLEN: u64 = 1024;

/// `kern.elf64.aslr.*`: which parts of a 64-bit image are randomized.
pub(crate) struct Aslr {
    /// `enable`: non-PIE images.
    pub(crate) enable: AtomicBool,
    /// `pie_enable`: PIEs, including their base.
    pub(crate) pie_enable: AtomicBool,
    /// `stack`.
    pub(crate) stack: AtomicBool,
    /// `shared_page`.
    pub(crate) shared_page: AtomicBool,
}

/// FreeBSD 15's defaults for 64-bit programs: everything.
pub(crate) static ASLR: Aslr = Aslr {
    enable: AtomicBool::new(true),
    pie_enable: AtomicBool::new(true),
    stack: AtomicBool::new(true),
    shared_page: AtomicBool::new(true),
};

/// Whether programs are given the vdso (`AT_KPRELOAD`), FreeBSD's
/// `kern.elf64.vdso`: on amd64; arm64 has no vdso.
pub(crate) static VDSO: AtomicBool = AtomicBool::new(cfg!(target_arch = "x86_64"));

/// Adds exec's nodes: `kern.elf64.*` (read-only until sysctl writes:
/// they report what exec does), and the caller's own `kern.ps_strings`,
/// `kern.usrstack`, and `kern.stackprot`.
pub(crate) fn sysctl_init(s: &Sleepable) {
    use abi::sysctl::kern;
    use sysctl::{Number, Value};
    let on = |b: &'static AtomicBool| {
        Value::Int(alloc::boxed::Box::new(move || {
            i32::from(b.load(Ordering::Relaxed))
        }))
    };
    let mine = |f: fn(&Layout) -> u64| {
        Value::Ulong(alloc::boxed::Box::new(move || {
            crate::proc::Process::with_current(|p| f(&p.layout())).unwrap_or(0)
        }))
    };
    crate::sysctl::with(s, |t| {
        let root = t.root();
        let k = t.child(root, "kern").expect("kern node");
        let elf = t
            .add_node(k, Number::Auto, "elf64", "")
            .expect("kern.elf64");
        let aslr = t
            .add_node(elf, Number::Auto, "aslr", "")
            .expect("kern.elf64.aslr");
        let nodes: [(_, &str, &str, Value); 10] = [
            (
                elf,
                "fallback_brand",
                "ELF64 brand of last resort",
                Value::int(-1),
            ),
            (
                elf,
                "nxstack",
                "ELF64: support PT_GNU_STACK for non-executable stack control",
                Value::int(1),
            ),
            (
                elf,
                "pie_base",
                "PIE load base without randomization",
                Value::Ulong(alloc::boxed::Box::new(|| ET_DYN_LOAD_ADDR)),
            ),
            (
                elf,
                "sigfastblock",
                "enable sigfastblock for new processes",
                Value::int(1),
            ),
            (
                aslr,
                "enable",
                "ELF64: enable address map randomization",
                on(&ASLR.enable),
            ),
            (
                aslr,
                "pie_enable",
                "ELF64: enable address map randomization for PIE binaries",
                on(&ASLR.pie_enable),
            ),
            (
                aslr,
                "honor_sbrk",
                "ELF64: assume sbrk is used",
                Value::int(0),
            ),
            (
                aslr,
                "stack",
                "ELF64: enable stack address randomization",
                on(&ASLR.stack),
            ),
            (
                aslr,
                "shared_page",
                "ELF64: enable shared page address randomization",
                on(&ASLR.shared_page),
            ),
            (
                k,
                "stackprot",
                "Stack memory permissions",
                Value::int(DEFAULT_STACK_PROT.bits().cast_signed()),
            ),
        ];
        for (parent, name, descr, value) in nodes {
            t.add_value(parent, Number::Auto, name, descr, value)
                .expect("exec sysctl node");
        }
        #[cfg(target_arch = "x86_64")]
        t.add_value(
            elf,
            Number::Auto,
            "vdso",
            "ELF64: enable vdso preloading",
            on(&VDSO),
        )
        .expect("kern.elf64.vdso");
        t.add_value(
            k,
            Number::Fixed(kern::PS_STRINGS),
            "ps_strings",
            "Location of process' ps_strings structure",
            mine(|l| l.stack_top - size_of::<PsStrings>() as u64),
        )
        .expect("kern.ps_strings");
        t.add_value(
            k,
            Number::Fixed(kern::USRSTACK),
            "usrstack",
            "Top of process stack",
            mine(|l| l.stack_top.next_multiple_of(PAGE_SIZE)),
        )
        .expect("kern.usrstack");
    });
}

/// Checks that `file` is a regular file with an execute bit, not on a
/// `noexec` mount, that the caller may execute, as FreeBSD's
/// `exec_check_permissions` does before any image activator looks at it
/// (a script's too).
pub(crate) fn check_exec(s: &Sleepable, file: &vfs::Vnode) -> Result<(), Errno> {
    let a = file.attr(s)?;
    if file.kind() != vfs::Kind::Regular
        || file.mount().flags() & abi::mount::MNT_NOEXEC != 0
        || a.mode & 0o111 == 0
    {
        return Err(Errno::EACCES);
    }
    vfs::acl::access(s, file, &crate::fs::cred(), vfs::ops::VEXEC)
}

/// [`check_exec`], then `file`'s memory object.
fn open(s: &Sleepable, file: &vfs::Vnode) -> Result<Arc<vm::object::Object>, Errno> {
    check_exec(s, file)?;
    crate::fs::object(s, file).ok_or(Errno::ENOEXEC)
}

/// A script's `#!` line (FreeBSD's `exec_shell_imgact`): the interpreter
/// and its one optional argument (the rest of the line, trimmed).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Script {
    pub(crate) interp: alloc::vec::Vec<u8>,
    pub(crate) arg: Option<alloc::vec::Vec<u8>>,
}

/// The longest `#!` line read (`MAXSHELLCMDLEN`, a page).
const MAXSHELLCMDLEN: usize = 4096;
/// The longest interpreter path (`MAXINTERP`, `PATH_MAX`).
const MAXINTERP: usize = 1024;

/// Parses `head`, a file's first bytes: `None` unless it starts `#!`.
/// `ENOEXEC` for no interpreter or a line not ended within
/// `MAXSHELLCMDLEN`, `ENAMETOOLONG` for an interpreter path too long.
pub(crate) fn parse_script(head: &[u8]) -> Result<Option<Script>, Errno> {
    if !head.starts_with(b"#!") {
        return Ok(None);
    }
    let line = &head[..head.len().min(MAXSHELLCMDLEN)];
    let blank = |c: &u8| *c == b' ' || *c == b'\t';
    let mut i = 2;
    while i < line.len() && blank(&line[i]) {
        i += 1;
    }
    let start = i;
    while i < line.len() && !matches!(line[i], b' ' | b'\t' | b'\n' | 0) {
        i += 1;
    }
    let interp = &line[start..i];
    if interp.is_empty() {
        return Err(Errno::ENOEXEC);
    }
    if interp.len() >= MAXINTERP {
        return Err(Errno::ENAMETOOLONG);
    }
    while i < line.len() && blank(&line[i]) {
        i += 1;
    }
    let opt = i;
    while i < line.len() && !matches!(line[i], b'\n' | 0) {
        i += 1;
    }
    if i == line.len() {
        return Err(Errno::ENOEXEC);
    }
    let mut end = i;
    while end > opt && blank(&line[end - 1]) {
        end -= 1;
    }
    Ok(Some(Script {
        interp: interp.to_vec(),
        arg: (end > opt).then(|| line[opt..end].to_vec()),
    }))
}

/// [`parse_script`] on `file`'s first bytes.
pub(crate) fn script(s: &Sleepable, file: &vfs::Vnode) -> Result<Option<Script>, Errno> {
    parse_script(&read_head(s, file)?)
}

/// The start of `file`, where the headers, program headers, branding
/// notes, and interpreter path sit (the memory disk's files are there
/// already).
fn read_head(s: &Sleepable, file: &vfs::Vnode) -> Result<Cow<'static, [u8]>, Errno> {
    if let Some(d) = file.resident(s) {
        return Ok(Cow::Borrowed(d));
    }
    let size = usize::try_from(file.size(s)?).unwrap_or(usize::MAX);
    let len = size.min(HEAD_MAX);
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|_| Errno::ENOMEM)?;
    buf.resize(len, 0u8);
    let mut r = vfs::ReadBuf::new(&mut buf);
    let n = file.read_at(s, 0, &mut r)?;
    buf.truncate(n);
    Ok(Cow::Owned(buf))
}

/// What an image's program headers say, before anything is mapped.
struct Headers<'a> {
    phnum: usize,
    /// The first `PT_LOAD`'s address.
    baddr: u64,
    /// The largest `PT_LOAD` alignment.
    maxalign: u64,
    /// The `PT_LOAD`s' total size.
    mapsz: u64,
    /// The program headers' address, unrelocated (`AT_PHDR`).
    proghdr: u64,
    /// The interpreter's path, without its NUL.
    interp: Option<&'a [u8]>,
    /// From `PT_GNU_STACK`.
    stack_prot: Option<Prot>,
}

impl<'a> Headers<'a> {
    fn scan(elf: &Elf<'a>) -> Result<Self, Errno> {
        let (phoff, phnum) = elf.program_headers();
        if phnum > PHNUMS {
            return Err(Errno::ENOEXEC);
        }
        let phend = phoff + phnum as u64 * PHENT;
        let mut h = Self {
            phnum,
            baddr: 0,
            maxalign: PAGE_SIZE,
            mapsz: 0,
            proghdr: 0,
            interp: None,
            stack_prot: None,
        };
        let mut loads = 0;
        for seg in elf.segments() {
            let seg = seg.map_err(|_| Errno::ENOEXEC)?;
            match seg.kind {
                SegmentKind::Load => {
                    if loads == 0 {
                        h.baddr = seg.vaddr;
                    }
                    loads += 1;
                    if !seg.align.is_power_of_two() || seg.align > MAX_SEGMENT_ALIGN {
                        return Err(Errno::ENOEXEC);
                    }
                    h.maxalign = h.maxalign.max(seg.align);
                    h.mapsz = h.mapsz.checked_add(seg.mem_size).ok_or(Errno::ENOEXEC)?;
                    // The segment holding the program headers gives their
                    // address (static binaries usually have no PT_PHDR).
                    if seg.offset == 0 && phend <= seg.file_size {
                        h.proghdr = seg.vaddr + phoff;
                    }
                }
                SegmentKind::Interp => {
                    if h.interp.is_some() || !(2..=MAXPATHLEN).contains(&seg.file_size) {
                        return Err(Errno::ENOEXEC);
                    }
                    let path = elf.segment_data(&seg).map_err(|_| Errno::ENOEXEC)?;
                    let (&nul, path) = path.split_last().ok_or(Errno::ENOEXEC)?;
                    if nul != 0 {
                        return Err(Errno::ENOEXEC);
                    }
                    // A path ends at its first NUL.
                    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
                    h.interp = Some(&path[..end]);
                }
                SegmentKind::GnuStack => {
                    let prot = prot_of(seg.perms);
                    if !prot.contains(Prot::RW) {
                        return Err(Errno::ENOEXEC);
                    }
                    h.stack_prot = Some(prot);
                }
                SegmentKind::Phdr => h.proghdr = seg.vaddr,
                _ => {}
            }
        }
        Ok(h)
    }
}

/// What a FreeBSD image's notes say.
#[derive(Clone, Copy, Debug, Default)]
struct Notes {
    /// The ABI tag: the `__FreeBSD_version` it was built for.
    osrel: i32,
    /// The feature-control word (`fctl::*`).
    fctl0: u32,
}

impl Notes {
    /// The ABI tag note and, beside it, the feature-control note; `None`
    /// without the ABI tag (FreeBSD's `check_note`).
    fn of(elf: &Elf<'_>) -> Option<Self> {
        let freebsd = |kind: u32| {
            elf.segments()
                .flatten()
                .filter(|s| s.kind == SegmentKind::Note)
                .filter_map(|s| elf.notes(&s).ok())
                .flatten()
                .find(|n| n.name == b"FreeBSD" && n.kind == kind)
                .and_then(|n| n.desc.first_chunk::<4>().copied())
        };
        let osrel = i32::from_le_bytes(freebsd(NT_FREEBSD_ABI_TAG)?);
        let fctl0 = freebsd(NT_FREEBSD_FEATURE_CTL).map_or(0, u32::from_le_bytes);
        Some(Self { osrel, fctl0 })
    }
}

/// Loads the interpreter `file` at `addr` (if it is position-independent)
/// and returns its base and entry (FreeBSD's `load_interp_file`).
fn load_interp(
    s: &Sleepable,
    space: &AddressSpace,
    file: &vfs::Vnode,
    addr: u64,
) -> Result<(u64, u64), Errno> {
    let object = open(s, file)?;
    let head = read_head(s, file)?;
    let elf = Elf::parse_head(&head).map_err(|_| Errno::ENOEXEC)?;
    if elf.machine != MACHINE {
        return Err(Errno::ENOEXEC);
    }
    let rbase = match elf.file_type {
        FileType::Shared => addr,
        FileType::Executable => 0,
        FileType::Other(_) => return Err(Errno::ENOEXEC),
    };
    if elf.program_headers().1 > PHNUMS {
        return Err(Errno::ENOEXEC);
    }
    let base = load_sections(s, space, &object, &elf, rbase)?;
    Ok((base, elf.entry.wrapping_add(rbase)))
}

/// Maps `elf`'s `PT_LOAD` segments `rbase` above where they were linked;
/// returns the first one's page (FreeBSD's `load_sections`).
fn load_sections(
    s: &Sleepable,
    space: &AddressSpace,
    object: &Arc<vm::object::Object>,
    elf: &Elf<'_>,
    rbase: u64,
) -> Result<u64, Errno> {
    let mut base = None;
    for seg in elf.segments() {
        let seg = seg.map_err(|_| Errno::ENOEXEC)?;
        if seg.kind != SegmentKind::Load || seg.mem_size == 0 {
            continue;
        }
        map_segment(s, space, object, &seg, rbase)?;
        base.get_or_insert(seg.vaddr.wrapping_add(rbase) & !(PAGE_SIZE - 1));
    }
    Ok(base.unwrap_or(0))
}

/// Checks the image's sizes against the limits and returns the data
/// segment's start (FreeBSD's `vm_daddr`): the largest executable segment
/// is the text, and the last other one the data (FreeBSD's
/// `enforce_limits`).
fn enforce_limits(elf: &Elf<'_>, rbase: u64, limits: &Limits) -> Result<u64, Errno> {
    let (mut text_size, mut text_addr) = (0, 0);
    let (mut data_size, mut data_addr) = (0, 0);
    let mut total: u64 = 0;
    for seg in elf.segments().flatten() {
        if seg.kind != SegmentKind::Load || seg.mem_size == 0 {
            continue;
        }
        let start = seg.vaddr.wrapping_add(rbase);
        let addr = start & !(PAGE_SIZE - 1);
        let size = (start - addr + seg.mem_size).next_multiple_of(PAGE_SIZE);
        if seg.perms.exec && text_size < size {
            (text_size, text_addr) = (size, addr);
        } else {
            (data_size, data_addr) = (size, addr);
        }
        total = total.saturating_add(size);
    }
    if data_addr == 0 && data_size == 0 {
        (data_size, data_addr) = (text_size, text_addr);
    }
    if data_size > limits[rlimit::DATA as usize].rlim_cur
        || total > limits[rlimit::VMEM as usize].rlim_cur
    {
        return Err(Errno::ENOMEM);
    }
    Ok(data_addr)
}

/// How much of a file `exec` reads to parse its headers (FreeBSD reads a
/// page, then whatever the headers point at): everything it needs lies
/// in the first pages of any executable the base system has.
const HEAD_MAX: usize = 64 * 1024;

/// Maps one `PT_LOAD` segment, `rbase` above where it was linked.
fn map_segment(
    s: &Sleepable,
    space: &AddressSpace,
    object: &Arc<vm::object::Object>,
    seg: &elf::Segment,
    rbase: u64,
) -> Result<(), Errno> {
    let page = PAGE_SIZE;
    let vaddr = seg.vaddr.checked_add(rbase).ok_or(Errno::ENOEXEC)?;
    if seg.offset % page != vaddr % page {
        return Err(Errno::ENOEXEC);
    }
    let prot = prot_of(seg.perms);
    let start = vaddr - vaddr % page;
    let file_end = vaddr + seg.file_size;
    let mem_end = vaddr.checked_add(seg.mem_size).ok_or(Errno::ENOEXEC)?;
    if mem_end > USRSTACK || start < USER_START {
        return Err(Errno::ENOEXEC);
    }
    let up = |v: u64| v.next_multiple_of(page);
    if seg.file_size > 0 {
        space
            .map_private(
                s,
                VirtAddr::new(start)..VirtAddr::new(up(file_end)),
                Arc::clone(object),
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
    /// The interpreter's base (or a static PIE's).
    base: u64,
    flags: u32,
    stack_prot: Prot,
    /// `RLIMIT_STACK`'s current value.
    stack_lim: u64,
}

/// Builds the initial stack below `layout.stack_top`; returns the address
/// of `argc` and the auxiliary vector's bytes.
fn build_stack(
    s: &Sleepable,
    space: &AddressSpace,
    path: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    aux: &Aux,
    layout: &Layout,
) -> Result<(u64, Vec<u8>), Errno> {
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
    let mut entries = alloc::vec![
        (at::PHDR, aux.phdr),
        (at::PHENT, PHENT),
        (at::PHNUM, aux.phnum),
        (at::PAGESZ, PAGE_SIZE),
        (at::FLAGS, 0),
        (at::ENTRY, aux.entry),
        (at::BASE, aux.base),
        (at::EHDRFLAGS, u64::from(aux.flags)),
        (at::EXECPATH, execpath),
        (at::OSRELDATE, abi::sysctl::identity::OSRELDATE as u64),
        (at::CANARY, canary),
        (at::CANARYLEN, CANARY_LEN as u64),
        (at::NCPUS, ncpus),
        (at::PAGESIZES, pagesizes),
        (at::PAGESIZESLEN, (MAXPAGESIZES * 8) as u64),
        (
            at::TIMEKEEP,
            crate::shared_page::timekeep_address(layout.shared_page),
        ),
        (at::STACKPROT, u64::from(aux.stack_prot.bits())),
    ];
    // The CPU's features, on arm64 (amd64 programs ask CPUID).
    #[cfg(target_arch = "aarch64")]
    {
        let (hwcap, hwcap2) = crate::idregs::hwcaps();
        entries.extend([
            (at::HWCAP, hwcap),
            (at::HWCAP2, hwcap2),
            (at::HWCAP3, 0),
            (at::HWCAP4, 0),
        ]);
    }
    entries.extend([
        (at::BSDFLAGS, bsdf::SIGFASTBLK),
        (at::ARGC, argv.len() as u64),
        (at::ARGV, argv_at),
        (at::ENVC, envp.len() as u64),
        (at::ENVV, envp_at),
        (at::PS_STRINGS, ps_strings),
    ]);
    if let Some(vdso) = crate::shared_page::vdso_address(layout.shared_page) {
        entries.push((at::KPRELOAD, vdso));
    }
    entries.extend([
        // The top itself, `ps_strings` right below it, as FreeBSD 15.1
        // reports it (tests/user/aslr.c, which passes there).
        (at::USRSTACKBASE, top),
        (at::USRSTACKLIM, aux.stack_lim),
        (at::NULL, 0),
    ]);
    let mut vector = Vec::with_capacity(entries.len() * 16);
    for (i, (k, v)) in entries.into_iter().enumerate() {
        word(&mut img, auxv + i as u64 * 16, k);
        word(&mut img, auxv + i as u64 * 16 + 8, v);
        vector.extend_from_slice(&k.to_ne_bytes());
        vector.extend_from_slice(&v.to_ne_bytes());
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
    Ok((argc_at, vector))
}

/// The entry registers for a stack whose `argc` is at `stack`.
fn registers(entry: u64, stack: u64, layout: Layout, interp: Option<crate::proc::Text>) -> Image {
    #[cfg(target_arch = "x86_64")]
    let sp = ((stack - 8) & !0xf) + 8;
    #[cfg(target_arch = "aarch64")]
    let sp = stack & !0xf;
    Image {
        entry,
        sp,
        arg: stack,
        layout,
        interp,
        auxv: Vec::new(),
    }
}
