// SPDX-License-Identifier: BSD-2-Clause
//! Core dumps (docs/MULTIUSER.md §12.3): a process a core-dumping signal
//! ends writes an ELF core file (`ET_CORE`) of itself first, every thread
//! stopped at the boundary so that their registers are where they left
//! user mode. The file holds a note segment (the process, then each
//! thread, the dumping one first: `NT_PRSTATUS`, its other register sets,
//! `NT_THRMISC`, `NT_PTLWPINFO`; then the process information `procstat`
//! reads from cores, `NT_PROCSTAT_*`) and a `PT_LOAD` segment for each
//! mapping that has memory, which `gdb`, `lldb`, and `procstat` read.
//!
//! The file is `kern.corefile` (`%N.core` in the working directory),
//! created mode 0600 for the effective user, who must own it; it is
//! truncated, and no dump is made past `RLIMIT_CORE`, for a set-id
//! process (without `kern.sugid_coredump`), or one that disabled tracing.
//!
//! Behavior reference: sys/kern/kern_ucoredump.c (`sigexit`,
//! `coredump`), sys/kern/coredump_vnode.c (`corefile_open`,
//! `coredump_vnode`), sys/kern/imgact_elf.c (`__elfN(coredump)` and its
//! notes).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use abi::errno::Errno;
use abi::ptrace::{PRARGSZ, PRFNAMESZ, Prpsinfo, Thrmisc};
use sched::Sleepable;
use sync::SpinLock;

use crate::proc::Process;
use crate::uaccess::bytes_of;
use crate::uthread::UserThread;

/// `kern.coredump`.
static DO_COREDUMP: AtomicBool = AtomicBool::new(true);
/// `kern.sugid_coredump`.
static SUGID_COREDUMP: AtomicBool = AtomicBool::new(false);
/// `kern.capmode_coredump`: a process in capability mode may name its
/// core file (otherwise its lookup from the working directory fails).
static CAPMODE_COREDUMP: AtomicBool = AtomicBool::new(false);
/// `kern.nodump_coredump`: core files get `UF_NODUMP`.
static NODUMP_COREDUMP: AtomicBool = AtomicBool::new(false);
/// `debug.ncores`: how many files `%I` cycles through.
static NUM_CORES: AtomicI32 = AtomicI32::new(5);
/// `kern.corefile`.
static COREFILE: SpinLock<Vec<u8>> = SpinLock::new(Vec::new());

/// The default `kern.corefile`.
const DEFAULT_COREFILE: &[u8] = b"%N.core";

/// ELF notes' owner (`FREEBSD_ABI_VENDOR`).
const VENDOR: &[u8] = b"FreeBSD\0";

/// Note types (`sys/elf_common.h`).
const NT_PRSTATUS: u32 = 1;
const NT_FPREGSET: u32 = 2;
const NT_PRPSINFO: u32 = 3;
const NT_THRMISC: u32 = 7;
const NT_PROCSTAT_PROC: u32 = 8;
const NT_PROCSTAT_FILES: u32 = 9;
const NT_PROCSTAT_VMMAP: u32 = 10;
const NT_PROCSTAT_GROUPS: u32 = 11;
const NT_PROCSTAT_UMASK: u32 = 12;
const NT_PROCSTAT_RLIMIT: u32 = 13;
const NT_PROCSTAT_OSREL: u32 = 14;
const NT_PROCSTAT_PSSTRINGS: u32 = 15;
const NT_PROCSTAT_AUXV: u32 = 16;
const NT_PTLWPINFO: u32 = 17;
const NT_PROCSTAT_KQUEUES: u32 = 18;
#[cfg(target_arch = "x86_64")]
const NT_X86_SEGBASES: u32 = 0x200;
#[cfg(target_arch = "x86_64")]
const NT_X86_XSTATE: u32 = 0x202;
#[cfg(target_arch = "aarch64")]
const NT_ARM_TLS: u32 = 0x401;
#[cfg(target_arch = "aarch64")]
const NT_ARM_ADDR_MASK: u32 = 0x406;

const PAGE: u64 = 4096;
const PAGE_BYTES: usize = 4096;

/// Where a core goes: bytes at an offset.
type CoreWriter<'a> = &'a mut dyn FnMut(u64, &[u8]) -> Result<(), Errno>;

/// What the ELF header names the machine (`EM_*`).
#[cfg(target_arch = "x86_64")]
const EM: u16 = 62;
#[cfg(target_arch = "aarch64")]
const EM: u16 = 183;

/// Why no core was made, as `sigexit` logs it.
pub(crate) struct NoCore {
    pub(crate) error: Errno,
    pub(crate) why: Option<&'static str>,
}

impl NoCore {
    const fn new(error: Errno) -> Self {
        Self { error, why: None }
    }

    const fn because(error: Errno, why: &'static str) -> Self {
        Self {
            error,
            why: Some(why),
        }
    }

    /// The reason in `sigexit`'s words.
    pub(crate) fn reason(&self) -> &'static str {
        self.why.unwrap_or(match self.error {
            Errno::EFAULT => "bad address",
            Errno::EINVAL => "invalild argument",
            Errno::EFBIG => "too large",
            _ => "other error",
        })
    }
}

/// `coredump`: the current thread `t` dumps its process, the other
/// threads stopped at the boundary, for signal `sig`.
pub(crate) fn dump(s: &Sleepable, t: &UserThread, sig: u32) -> Result<(), NoCore> {
    let p = t.process();
    if !DO_COREDUMP.load(Ordering::Relaxed) {
        return Err(NoCore::because(Errno::EFAULT, "denied by kern.coredump"));
    }
    if p.flag2.load(Ordering::Relaxed) & crate::proc::p2::NOTRACE != 0 {
        return Err(NoCore::because(Errno::EFAULT, "process has trace disabled"));
    }
    if p.sugid() && !SUGID_COREDUMP.load(Ordering::Relaxed) {
        return Err(NoCore::because(
            Errno::EFAULT,
            "sugid process denied by kern.sugid_coredump",
        ));
    }
    let limit = p.limits.lock()[abi::resource::rlimit::CORE as usize].rlim_cur;
    if limit == 0 {
        return Err(NoCore::because(Errno::EFBIG, "coredumpsize limit is 0"));
    }
    let name = corefile_name(p, sig);
    let cred = p.cred();
    let file = crate::syscall::open_core_file(
        s,
        p,
        &name,
        p.sugid(),
        CAPMODE_COREDUMP.load(Ordering::Relaxed),
    )
    .map_err(NoCore::new)?;
    let v = file.vnode().ok_or(NoCore::new(Errno::EFAULT))?;
    // Only a plain file of one link, the dumper's own.
    let attr = v.attr(s).map_err(|_| NoCore::new(Errno::EFAULT))?;
    if v.kind() != vfs::Kind::Regular || attr.nlink != 1 || attr.uid != cred.uid {
        return Err(NoCore::new(Errno::EFAULT));
    }
    let empty = vfs::SetAttr {
        size: Some(0),
        flags: NODUMP_COREDUMP
            .load(Ordering::Relaxed)
            .then_some(abi::stat::UF_NODUMP),
        ..vfs::SetAttr::default()
    };
    vfs::ops::setattr(s, v, &empty, &cred, false).map_err(NoCore::new)?;
    write_core(s, t, sig, limit, false, &mut |at, b| {
        let n = v.write_at(s, at, &mut vfs::WriteBuf::new(b))?;
        if n < b.len() {
            Err(Errno::ENOSPC)
        } else {
            Ok(())
        }
    })
    .map_err(NoCore::new)
}

/// `corefile_open`'s name: `kern.corefile` with `%N` the command, `%P`
/// the pid, `%U` the effective uid, `%H` the host name, `%S` the signal,
/// `%I` an index (the first of `debug.ncores`, as FreeBSD's first try).
fn corefile_name(p: &Process, sig: u32) -> Vec<u8> {
    use core::fmt::Write;
    let format = {
        let f = COREFILE.lock();
        if f.is_empty() {
            DEFAULT_COREFILE.to_vec()
        } else {
            f.clone()
        }
    };
    let mut out = String::new();
    let mut it = format.iter().copied();
    while let Some(c) = it.next() {
        if c != b'%' {
            out.push(char::from(c));
            continue;
        }
        let _ = match it.next() {
            Some(b'%') => write!(out, "%"),
            Some(b'N') => {
                let comm = p.with_comm(|n| String::from_utf8_lossy(n).into_owned());
                write!(out, "{comm}")
            }
            Some(b'P') => write!(out, "{}", p.pid()),
            Some(b'U') => write!(out, "{}", p.cred().uid),
            Some(b'S') => write!(out, "{sig}"),
            Some(b'I') => write!(out, "{}", NUM_CORES.load(Ordering::Relaxed) - 1),
            Some(b'H') => {
                let host = crate::prison::of(&p.cred()).settings().hostname;
                write!(out, "{}", String::from_utf8_lossy(&host))
            }
            _ => Ok(()),
        };
    }
    out.into_bytes()
}

/// A mapping a core holds.
struct Segment {
    start: u64,
    len: u64,
    prot: u32,
}

/// `__elfN(coredump)`: the ELF core of thread `t`'s process, written with
/// `write(offset, bytes)`; `EFAULT` past `limit` bytes.
pub(crate) fn write_core(
    s: &Sleepable,
    t: &UserThread,
    sig: u32,
    limit: u64,
    all: bool,
    write: CoreWriter<'_>,
) -> Result<(), Errno> {
    let p = t.process();
    let space = p.space();
    let mut segments = Vec::new();
    for i in 0.. {
        let Some(e) = space.entry_info(s, i) else {
            break;
        };
        // With `PC_ALL` (`SVC_ALL`), inaccessible mappings too.
        if (e.prot != 0 || all) && e.dumpable {
            segments.push(Segment {
                start: e.start,
                len: e.end - e.start,
                prot: e.prot,
            });
        }
    }
    let mut measured = Notes::measure();
    notes(s, t, sig, &mut measured);
    let notes_len = measured.len;
    // Past `PN_XNUM` program headers the count goes in a section header.
    const PN_XNUM: usize = 0xffff;
    let nph = segments.len() + 1;
    let extended = nph >= PN_XNUM;
    let hdrsize = 64 + 56 * nph + if extended { 64 } else { 0 };
    let notes_end = hdrsize as u64 + notes_len;
    let data_at = notes_end.next_multiple_of(PAGE);
    let total: u64 = data_at + segments.iter().map(|g| g.len).sum::<u64>();
    if total >= limit {
        return Err(Errno::EFAULT);
    }
    let mut hdr = Vec::with_capacity(hdrsize);
    // `Elf64_Ehdr`.
    hdr.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 9, 0, 0, 0, 0, 0, 0, 0, 0]);
    hdr.extend_from_slice(&4u16.to_le_bytes()); // ET_CORE
    hdr.extend_from_slice(&EM.to_le_bytes());
    hdr.extend_from_slice(&1u32.to_le_bytes()); // EV_CURRENT
    hdr.extend_from_slice(&0u64.to_le_bytes()); // e_entry
    hdr.extend_from_slice(&64u64.to_le_bytes()); // e_phoff
    let shoff = if extended { (64 + 56 * nph) as u64 } else { 0 };
    hdr.extend_from_slice(&shoff.to_le_bytes());
    hdr.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    hdr.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
    hdr.extend_from_slice(&56u16.to_le_bytes()); // e_phentsize
    let phnum = if extended { PN_XNUM } else { nph };
    hdr.extend_from_slice(&u16::try_from(phnum).unwrap_or(u16::MAX).to_le_bytes());
    hdr.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
    hdr.extend_from_slice(&u16::from(extended).to_le_bytes()); // e_shnum
    hdr.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    // `Elf64_Phdr`s: the notes, then each segment.
    let mut phdr = |ty: u32, flags: u32, off: u64, va: u64, filesz: u64, memsz: u64, align: u64| {
        hdr.extend_from_slice(&ty.to_le_bytes());
        hdr.extend_from_slice(&flags.to_le_bytes());
        for v in [off, va, 0, filesz, memsz, align] {
            hdr.extend_from_slice(&v.to_le_bytes());
        }
    };
    phdr(4, 4, hdrsize as u64, 0, notes_len, 0, 4);
    let mut at = data_at;
    for g in &segments {
        // `__elfN(untrans_prot)`: `PF_X`, `PF_W`, `PF_R`.
        let flags = u32::from(g.prot & 4 != 0)
            | u32::from(g.prot & 2 != 0) << 1
            | u32::from(g.prot & 1 != 0) << 2;
        phdr(1, flags, at, g.start, g.len, g.len, PAGE);
        at += g.len;
    }
    if extended {
        // The first section header holds the real count (`sh_info`).
        let mut sh = [0u8; 64];
        sh[44..48].copy_from_slice(&u32::try_from(nph).unwrap_or(u32::MAX).to_le_bytes());
        hdr.extend_from_slice(&sh);
    }
    write(0, &hdr)?;
    // The notes again, written this time, then zeros to the first page
    // of memory.
    let mut out = Notes::writer(measured, &mut *write, hdrsize as u64);
    notes(s, t, sig, &mut out);
    out.finish(notes_len)?;
    let pad = data_at - notes_end;
    if pad > 0 {
        write(
            notes_end,
            &alloc::vec![0u8; usize::try_from(pad).unwrap_or(0)],
        )?;
    }
    // The memory, a page at a time; what cannot be read is zeros.
    let mut at = data_at;
    let mut page = alloc::vec![0u8; PAGE_BYTES];
    for g in &segments {
        let mut off = 0;
        while off < g.len {
            let n = (g.len - off).min(PAGE) as usize;
            let got = space.debug_rw(s, g.start + off, &mut page[..n], false);
            page[got..n].fill(0);
            write(at + off, &page[..n])?;
            off += n as u64;
        }
        at += g.len;
    }
    Ok(())
}

/// `PT_COREDUMP` (`ptrace_coredumpreq`): the current thread `t`,
/// stopped for its debugger, writes its process's core into `v` (from
/// offset 0) within `limit` bytes, inaccessible mappings too with `all`.
/// `PC_COMPRESS` compresses as `kern.compress_user_cores` says, which
/// is always 0 here (no compressor), so it changes nothing.
pub(crate) fn dump_for_debugger(
    s: &Sleepable,
    t: &UserThread,
    v: &vfs::Vnode,
    limit: u64,
    all: bool,
) -> Result<(), Errno> {
    write_core(s, t, 0, limit, all, &mut |at, b| {
        let n = v.write_at(s, at, &mut vfs::WriteBuf::new(b))?;
        if n < b.len() {
            Err(Errno::ENOSPC)
        } else {
            Ok(())
        }
    })
}

/// The notes' bytes staged before each write.
const STAGE: usize = 16 * PAGE_BYTES;

/// Where a core's notes go, as FreeBSD's `sbuf` with `sbuf_count_drain`
/// and then `sbuf_drain_core_output` (`__elfN(coredump)`): measured first,
/// each note's size kept (`struct note_info`), so the memory can be placed
/// after them; then written from `at`, [`STAGE`] bytes at a time. Never
/// held whole: a process with thousands of descriptors has megabytes of
/// notes. The first write error sticks.
struct Notes<'w> {
    write: Option<CoreWriter<'w>>,
    /// Each note's description size, from the measuring pass.
    sizes: Vec<usize>,
    next: usize,
    len: u64,
    /// The end of the description being written: what passes it is
    /// dropped, as a note may not outgrow its measured size.
    clip: Option<u64>,
    at: u64,
    stage: Vec<u8>,
    err: Option<Errno>,
}

impl<'w> Notes<'w> {
    fn measure() -> Self {
        Self {
            write: None,
            sizes: Vec::new(),
            next: 0,
            len: 0,
            clip: None,
            at: 0,
            stage: Vec::new(),
            err: None,
        }
    }

    /// The writing pass, at offset `at`, with the sizes `measured` found.
    fn writer(measured: Self, write: CoreWriter<'w>, at: u64) -> Self {
        Self {
            write: Some(write),
            sizes: measured.sizes,
            at,
            stage: Vec::with_capacity(STAGE),
            ..Self::measure()
        }
    }

    fn put(&mut self, mut b: &[u8]) {
        if let Some(end) = self.clip {
            let room = usize::try_from(end.saturating_sub(self.len)).unwrap_or(usize::MAX);
            b = &b[..b.len().min(room)];
        }
        self.len += b.len() as u64;
        if self.write.is_some() {
            self.stage.extend_from_slice(b);
            if self.stage.len() >= STAGE {
                self.flush();
            }
        }
    }

    fn zeros(&mut self, n: u64) {
        const Z: [u8; 64] = [0; 64];
        let mut n = n;
        while n > 0 {
            let k = usize::try_from(n).map_or(Z.len(), |n| n.min(Z.len()));
            self.put(&Z[..k]);
            n -= k as u64;
        }
    }

    fn pad4(&mut self) {
        self.zeros(self.len.next_multiple_of(4) - self.len);
    }

    fn flush(&mut self) {
        if let Some(w) = self.write.as_mut()
            && !self.stage.is_empty()
        {
            if self.err.is_none()
                && let Err(e) = w(self.at, &self.stage)
            {
                self.err = Some(e);
            }
            self.at += self.stage.len() as u64;
            self.stage.clear();
        }
    }

    /// The writing pass's end: written out, at the length measured.
    fn finish(mut self, total: u64) -> Result<(), Errno> {
        if self.len < total {
            self.zeros(total - self.len);
        }
        self.flush();
        self.err.map_or(Ok(()), Err)
    }
}

/// One ELF note: header, owner, and the description `desc` puts, each
/// part padded to 4 (`__elfN(putnote)`). Its size is what the measuring
/// pass found; written, it is clipped or padded with zeros to that.
fn note_with(out: &mut Notes<'_>, ty: u32, desc: impl FnOnce(&mut Notes<'_>)) {
    let size = if out.write.is_none() {
        None
    } else {
        let Some(&size) = out.sizes.get(out.next) else {
            // A note more than measured: left out, as the space is not
            // there.
            return;
        };
        out.next += 1;
        Some(size)
    };
    out.put(&u32::try_from(VENDOR.len()).unwrap_or(0).to_le_bytes());
    out.put(&u32::try_from(size.unwrap_or(0)).unwrap_or(0).to_le_bytes());
    out.put(&ty.to_le_bytes());
    out.put(VENDOR);
    out.pad4();
    let start = out.len;
    match size {
        None => {
            desc(out);
            let measured = usize::try_from(out.len - start).unwrap_or(0);
            out.sizes.push(measured);
        }
        Some(size) => {
            let end = start + size as u64;
            out.clip = Some(end);
            desc(out);
            out.clip = None;
            out.zeros(end - out.len);
        }
    }
    out.pad4();
}

fn note(out: &mut Notes<'_>, ty: u32, desc: &[u8]) {
    note_with(out, ty, |o| o.put(desc));
}

/// `__elfN(prepare_notes)`: the process's, then each thread's (the
/// dumping one first), then the process information.
fn notes(s: &Sleepable, t: &UserThread, sig: u32, out: &mut Notes<'_>) {
    let p = t.process();
    note(out, NT_PRPSINFO, bytes_of(&prpsinfo(p)));
    let mut threads = alloc::vec![Arc::clone(
        &t.process().thread_by_tid(t.tid()).unwrap_or_else(|| {
            // The dumping thread is its process's.
            unreachable!("the dumping thread left its process")
        })
    )];
    threads.extend(p.threads().into_iter().filter(|o| o.tid() != t.tid()));
    for th in &threads {
        thread_notes(out, th, sig);
    }
    procstat_notes(s, out, p);
}

/// `struct prpsinfo`: the command and its arguments, spaces between.
fn prpsinfo(p: &Process) -> Prpsinfo {
    let mut ps = Prpsinfo {
        pr_pid: p.pid(),
        ..Prpsinfo::default()
    };
    let image = p.image();
    let n = image.comm.len().min(PRFNAMESZ);
    ps.pr_fname[..n].copy_from_slice(&image.comm[..n]);
    let args = &image.args;
    let len = args.len().saturating_sub(1).min(PRARGSZ);
    if len == 0 {
        ps.pr_psargs[..n].copy_from_slice(&image.comm[..n]);
    } else {
        for (d, &c) in ps.pr_psargs.iter_mut().zip(&args[..len]) {
            *d = if c == 0 { b' ' } else { c };
        }
    }
    ps
}

/// A thread's notes: `NT_PRSTATUS` first, its other register sets, its
/// name, and its `ptrace_lwpinfo`.
fn thread_notes(out: &mut Notes<'_>, t: &UserThread, sig: u32) {
    if let Ok(st) = crate::ptrace::prstatus(t, sig.cast_signed()) {
        note(out, NT_PRSTATUS, &st);
    }
    if let Ok(fp) = crate::ptrace::read_fpregs(t) {
        note(out, NT_FPREGSET, crate::uaccess::bytes_of(&fp));
    }
    let mut misc = Thrmisc::default();
    let name = t.name();
    let name = name.as_bytes();
    let n = name.len().min(misc.pr_tname.len() - 1);
    misc.pr_tname[..n].copy_from_slice(&name[..n]);
    note(out, NT_THRMISC, bytes_of(&misc));
    let mut pl = Vec::with_capacity(4 + size_of::<abi::ptrace::PtraceLwpinfo>());
    pl.extend_from_slice(
        &i32::try_from(size_of::<abi::ptrace::PtraceLwpinfo>())
            .unwrap_or(0)
            .to_le_bytes(),
    );
    pl.extend_from_slice(crate::uaccess::bytes_of(&crate::ptrace::core_lwpinfo(t)));
    note(out, NT_PTLWPINFO, &pl);
    machine_notes(out, t);
}

#[cfg(target_arch = "x86_64")]
fn machine_notes(out: &mut Notes<'_>, t: &UserThread) {
    let (r_fsbase, r_gsbase) = crate::ptrace::user_bases(t);
    let bases = abi::ptrace::regs_amd64::Segbasereg { r_fsbase, r_gsbase };
    note(out, NT_X86_SEGBASES, bytes_of(&bases));
    // The XSAVE area, the enabled features recorded in its software part
    // (`X86_XSTATE_XCR0_OFFSET`).
    if let Some(mut x) = crate::ptrace::xsave_area(t) {
        if x.len() >= 472 {
            let xcr0 = crate::arch::fpu::describe().1;
            x[464..472].copy_from_slice(&xcr0.to_le_bytes());
        }
        note(out, NT_X86_XSTATE, &x);
    }
}

#[cfg(target_arch = "aarch64")]
fn machine_notes(out: &mut Notes<'_>, t: &UserThread) {
    let (tls, _) = crate::ptrace::user_bases(t);
    note(out, NT_ARM_TLS, &tls.to_le_bytes());
    // The top byte is ignored.
    let mask = abi::ptrace::regs_arm64::AddrMask {
        code: 0xff00_0000_0000_0000,
        data: 0xff00_0000_0000_0000,
    };
    note(out, NT_ARM_ADDR_MASK, bytes_of(&mask));
}

/// The process information notes, each its structure's size then the
/// data, as `procstat` reads them from a core.
fn procstat_notes(s: &Sleepable, out: &mut Notes<'_>, p: &Process) {
    let with_size = |size: usize, data: &[u8]| {
        let mut b = i32::try_from(size).unwrap_or(0).to_le_bytes().to_vec();
        b.extend_from_slice(data);
        b
    };
    let struct_size = |size: usize| i32::try_from(size).unwrap_or(0).to_le_bytes();
    // One `kinfo_proc` per thread (`kern_proc_out` without
    // `KERN_PROC_NOTHREADS`); files and mappings packed. Each record goes
    // out as it is made.
    note_with(out, NT_PROCSTAT_PROC, |o| {
        o.put(&struct_size(abi::kinfo::KINFO_PROC_SIZE));
        crate::kinfo::proc_entries_to(s, p, true, &mut |b| o.put(b));
    });
    note_with(out, NT_PROCSTAT_FILES, |o| {
        o.put(&struct_size(abi::kinfo::KINFO_FILE_SIZE));
        crate::kinfo::file::files_to(s, p, &mut |b| o.put(b));
    });
    note_with(out, NT_PROCSTAT_VMMAP, |o| {
        o.put(&struct_size(abi::kinfo::KINFO_VMENTRY_SIZE));
        crate::kinfo::vm::vmmap_to(s, p, &mut |b| o.put(b));
    });
    let cred = p.cred();
    let mut groups = cred.gid.to_le_bytes().to_vec();
    for g in cred.groups() {
        groups.extend_from_slice(&g.to_le_bytes());
    }
    note(out, NT_PROCSTAT_GROUPS, &with_size(4, &groups));
    let umask = p.umask.load(Ordering::Relaxed);
    note(out, NT_PROCSTAT_UMASK, &with_size(2, &umask.to_le_bytes()));
    let limits = *p.limits.lock();
    let mut rl = Vec::new();
    for l in &limits {
        rl.extend_from_slice(&l.rlim_cur.to_le_bytes());
        rl.extend_from_slice(&l.rlim_max.to_le_bytes());
    }
    note(out, NT_PROCSTAT_RLIMIT, &with_size(16, &rl));
    let osrel = p.layout().osrel;
    note(out, NT_PROCSTAT_OSREL, &with_size(4, &osrel.to_le_bytes()));
    let ps = crate::kinfo::misc::ps_strings(p);
    note(out, NT_PROCSTAT_PSSTRINGS, &with_size(8, &ps.to_le_bytes()));
    let auxv = p.image().auxv;
    note(out, NT_PROCSTAT_AUXV, &with_size(16, &auxv));
    let kq = crate::kinfo::misc::kqueues(s, p, None).unwrap_or_default();
    note(
        out,
        NT_PROCSTAT_KQUEUES,
        &with_size(size_of::<abi::kinfo::KinfoKnote>(), &kq),
    );
}

/// Adds the core dump sysctls.
pub(crate) fn sysctl_init(s: &Sleepable) {
    use sysctl::{Number, Value};
    *COREFILE.lock() = DEFAULT_COREFILE.to_vec();
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        let flag = |t: &mut sysctl::Tree, name: &str, descr: &str, v: &'static AtomicBool| {
            t.add_writable(
                kern,
                Number::Auto,
                name,
                descr,
                0,
                Value::Int(alloc::boxed::Box::new(|| {
                    i32::from(v.load(Ordering::Relaxed))
                })),
                sysctl::int_writer(|n| {
                    v.store(n != 0, Ordering::Relaxed);
                    Ok(())
                }),
            )
            .expect("kern core dump flag");
        };
        flag(t, "coredump", "Enable/Disable coredumps", &DO_COREDUMP);
        flag(
            t,
            "sugid_coredump",
            "Allow setuid and setgid processes to dump core",
            &SUGID_COREDUMP,
        );
        flag(
            t,
            "capmode_coredump",
            "Allow processes in capability mode to dump core",
            &CAPMODE_COREDUMP,
        );
        flag(
            t,
            "nodump_coredump",
            "Enable setting the NODUMP flag on coredump files",
            &NODUMP_COREDUMP,
        );
        t.add_writable(
            kern,
            Number::Auto,
            "corefile",
            "Process corefile name format string",
            0,
            Value::ByteStr(alloc::boxed::Box::new(|out| {
                out.extend_from_slice(&COREFILE.lock());
            })),
            sysctl::str_writer(abi::fcntl::PATH_MAX, |b| {
                *COREFILE.lock() = b.to_vec();
                Ok(())
            }),
        )
        .expect("kern.corefile");
        t.add_writable(
            kern,
            Number::Auto,
            "compress_user_cores",
            "Enable compression of user corefiles (1 = gzip, 2 = zstd)",
            0,
            Value::int(0),
            // No compressor is built in.
            sysctl::int_writer(|n| if n == 0 { Ok(()) } else { Err(Errno::EINVAL) }),
        )
        .expect("kern.compress_user_cores");
        let debug = t.child(t.root(), "debug").or_else(|| {
            t.add_node(t.root(), Number::Auto, "debug", "Debugging")
                .ok()
        });
        if let Some(debug) = debug {
            let _ = t.add_writable(
                debug,
                Number::Auto,
                "ncores",
                "Maximum number of generated process corefiles while using index format",
                0,
                Value::Int(alloc::boxed::Box::new(|| NUM_CORES.load(Ordering::Relaxed))),
                sysctl::int_writer(|n| {
                    if !(0..=100_000).contains(&n) {
                        return Err(Errno::EINVAL);
                    }
                    NUM_CORES.store(n, Ordering::Relaxed);
                    Ok(())
                }),
            );
        }
    });
}
