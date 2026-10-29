// SPDX-License-Identifier: BSD-2-Clause
//! Kernel tracing (docs/MULTIUSER.md §12.2): `ktrace(2)` attaches a trace
//! file and a facility mask to processes, and each traced event becomes a
//! record (`struct ktr_header`, its type's part, a payload) that unmodified
//! `kdump` reads.
//!
//! As FreeBSD's: a record made where the thread may sleep is written at
//! once, in the thread, after any queued before it (`ktr_submitrequest`);
//! one made where it may not (signals, context switches, capability
//! failures, faults) is queued on the process and written on the way back
//! to user mode (`ktr_enqueuerequest`, `ktruserret`). Requests come from a
//! pool of `kern.ktrace.request_pool`; with none left a record is dropped
//! and the next carries `KTR_DROP`. A failed write stops the process's
//! tracing. A thread making a record traces nothing meanwhile
//! (`TDP_INKTRACE`).
//!
//! Behavior reference: sys/kern/kern_ktrace.c.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use abi::capsicum::CapRights;
use abi::errno::Errno;
use abi::ktrace::*;
use abi::signal::Sigset;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;

use crate::cred::{Priv, Ucred};
use crate::fs::file::File;
use crate::proc::Process;
use crate::uaccess::Plain;
use crate::uthread::UserThread;

// SAFETY: integers, byte arrays, a timespec, and a signal set, with their
// padding explicit (`abi-check` holds the C layouts).
unsafe impl Plain for KtrHeader {}
// SAFETY: as above.
unsafe impl Plain for KtrSyscall {}
// SAFETY: as above.
unsafe impl Plain for KtrSysret {}
// SAFETY: as above.
unsafe impl Plain for KtrGenio {}
// SAFETY: as above.
unsafe impl Plain for KtrPsig {}
// SAFETY: as above.
unsafe impl Plain for KtrCsw {}
// SAFETY: as above.
unsafe impl Plain for KtrProcCtor {}
// SAFETY: as above.
unsafe impl Plain for KtrCapFail {}
// SAFETY: as above.
unsafe impl Plain for KtrFault {}
// SAFETY: as above.
unsafe impl Plain for KtrFaultend {}
// SAFETY: as above.
unsafe impl Plain for KtrStructArray {}

/// `KTRACE_REQUEST_POOL`.
const REQUEST_POOL: u32 = 100;

/// `kern.ktrace.request_pool`: how many requests there are.
static POOL_SIZE: AtomicU32 = AtomicU32::new(REQUEST_POOL);
/// How many of them are free.
static POOL_FREE: AtomicI32 = AtomicI32::new(REQUEST_POOL as i32);
/// "Out of ktrace request objects." is printed once per pool size.
static PRINT_MESSAGE: AtomicBool = AtomicBool::new(true);
/// `kern.ktrace.genio_size`: the most data a `KTR_GENIO` record keeps.
static GENIO_SIZE: AtomicU32 = AtomicU32::new(4096);
/// `kern.ktrace.filesize_limit_signal`: whether a trace past its
/// tracer's `RLIMIT_FSIZE` sends `SIGXFSZ` to the traced process.
static FILESIZE_LIMIT_SIGNAL: AtomicBool = AtomicBool::new(false);

/// Held while records are written (`ktrace_sx`).
static WRITE_LOCK: LockClass = LockClass::new("ktrace_sx", 60);
static WRITE: Mutex<()> = Mutex::new(&WRITE_LOCK, ());

/// Where a process's records go (`struct ktr_io_params`): the file, the
/// tracer's credential, and its `RLIMIT_FSIZE`. Shared by the processes
/// one `ktrace` traces into it, and closed with the last.
pub(crate) struct TraceFile {
    file: Arc<File>,
    cred: Arc<Ucred>,
    limit: u64,
}

impl TraceFile {
    fn same(&self, other: &Self) -> bool {
        match (self.file.vnode(), other.file.vnode()) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// A process's tracing: `p_traceflag`, `p_ktrioparms`, `p_ktr`.
pub(crate) struct Trace {
    /// The facilities and `KTRFAC_ROOT`/`INHERIT`/`DROP`; read without
    /// the lock to test a trace point.
    flags: AtomicU32,
    inner: SpinLock<Inner>,
}

struct Inner {
    io: Option<Arc<TraceFile>>,
    /// Records waiting for the way back to user mode.
    queue: Vec<Request>,
}

impl Default for Trace {
    fn default() -> Self {
        Self {
            flags: AtomicU32::new(0),
            inner: SpinLock::new(Inner {
                io: None,
                queue: Vec::new(),
            }),
        }
    }
}

impl Trace {
    /// `p_traceflag`.
    pub(crate) fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// Whether facility `ty` is traced (`KTRCHECK`).
    fn traces(&self, ty: u16) -> bool {
        self.flags() & (1 << ty) != 0
    }

    /// Ends tracing (`ktr_freeproc`): the file, for the caller to drop
    /// outside the lock.
    fn free(&self, inner: &mut Inner) -> Option<Arc<TraceFile>> {
        self.flags.store(0, Ordering::Relaxed);
        inner.queue.clear();
        inner.io.take()
    }
}

/// One record (`struct ktr_request`): the header, the type's part, and
/// the payload. Holding one holds a place in the pool.
struct Request {
    header: KtrHeader,
    part: Vec<u8>,
    payload: Vec<u8>,
}

impl Drop for Request {
    fn drop(&mut self) {
        POOL_FREE.fetch_add(1, Ordering::Relaxed);
    }
}

/// The current thread is making a record (`TDP_INKTRACE`) until this
/// goes.
struct Entered(Arc<UserThread>);

impl Entered {
    /// `ktrace_enter`, unless the current thread is already in.
    fn new() -> Option<Self> {
        let t = UserThread::current()?;
        (!t.in_ktrace.swap(true, Ordering::Acquire)).then_some(Self(t))
    }

    fn process(&self) -> &Arc<Process> {
        self.0.process()
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        self.0.in_ktrace.store(false, Ordering::Release);
    }
}

/// Whether the current process traces facility `ty` (`KTRPOINT`, the
/// recursion check left to the record's maker).
pub(crate) fn point(ty: u16) -> bool {
    Process::with_current(|p| p.ktrace.traces(ty)).unwrap_or(false)
}

/// `CAP_TRACING`: capability failures are traced.
pub(crate) fn cap_tracing() -> bool {
    point(KTR_CAPFAIL)
}

/// A request for thread `t`'s record of type `ty`, if `t`'s process
/// traces it and the pool has one (`ktr_getrequest_entered`).
fn request(t: &UserThread, ty: u16) -> Option<Request> {
    let p = t.process();
    let mut ktype = ty | KTR_VERSIONED;
    {
        let guard = p.ktrace.inner.lock();
        if !p.ktrace.traces(ty) {
            return None;
        }
        if POOL_FREE.fetch_sub(1, Ordering::Relaxed) <= 0 {
            POOL_FREE.fetch_add(1, Ordering::Relaxed);
            p.ktrace.flags.fetch_or(KTRFAC_DROP, Ordering::Relaxed);
            drop(guard);
            if PRINT_MESSAGE.swap(false, Ordering::Relaxed) {
                kprintln!("Out of ktrace request objects.");
            }
            return None;
        }
        if p.ktrace.flags.fetch_and(!KTRFAC_DROP, Ordering::Relaxed) & KTRFAC_DROP != 0 {
            ktype |= KTR_DROP;
        }
    }
    let now = crate::fs::now();
    let mut header = KtrHeader {
        ktr_type: ktype.cast_signed(),
        ktr_version: KTR_VERSION1,
        ktr_pid: p.pid(),
        ktr_time: now,
        ktr_tid: i64::from(t.tid()),
        ktr_cpu: i32::try_from(sched::this_cpu().id().0).unwrap_or(0),
        ..KtrHeader::default()
    };
    let name = t.name();
    let name = name.as_bytes();
    header.ktr_comm[..name.len()].copy_from_slice(name);
    Some(Request {
        header,
        part: Vec::new(),
        payload: Vec::new(),
    })
}

/// Writes `req` now, after what the process has queued
/// (`ktr_submitrequest`).
fn submit(s: &Sleepable, e: &Entered, req: Request) {
    let _w = WRITE.lock(s);
    drain_locked(s, e.process());
    write(s, e.process(), req);
}

/// Queues `req` for thread `t`'s way back to user mode
/// (`ktr_enqueuerequest`); without a trace file it is dropped.
fn enqueue(t: &UserThread, req: Request) {
    let p = t.process();
    {
        let mut inner = p.ktrace.inner.lock();
        if inner.io.is_none() {
            return;
        }
        inner.queue.push(req);
    }
    if let Some(st) = t.sched() {
        st.request_ast(crate::signal::AST_KTRACE);
    }
}

/// Writes what `p` has queued (`ktr_drain`), the write lock held.
fn drain_locked(s: &Sleepable, p: &Process) {
    let queued = core::mem::take(&mut p.ktrace.inner.lock().queue);
    for req in queued {
        write(s, p, req);
    }
}

/// Writes what the current process has queued (`ktruserret`): on the way
/// back to user mode.
pub(crate) fn user_return(s: &Sleepable) {
    if Process::with_current(|p| p.ktrace.flags() == 0).unwrap_or(true) {
        return;
    }
    let Some(e) = Entered::new() else {
        return;
    };
    if e.process().ktrace.inner.lock().queue.is_empty() {
        return;
    }
    let _w = WRITE.lock(s);
    drain_locked(s, e.process());
}

/// Appends `req` to `p`'s trace file (`ktr_writerequest`). A failure
/// stops `p`'s tracing into that file.
fn write(s: &Sleepable, p: &Process, mut req: Request) {
    let Some(io) = p.ktrace.inner.lock().io.clone() else {
        return;
    };
    let Some(v) = io.file.vnode() else {
        return;
    };
    let len = req.part.len() + req.payload.len();
    req.header.ktr_len = i32::try_from(len).unwrap_or(i32::MAX);
    let mut bytes = Vec::with_capacity(size_of::<KtrHeader>() + len);
    bytes.extend_from_slice(crate::uaccess::bytes_of(&req.header));
    bytes.extend_from_slice(&req.part);
    bytes.extend_from_slice(&req.payload);
    let result = (|| {
        let at = v.size(s)?;
        // The tracer's `RLIMIT_FSIZE` (`td_ktr_io_lim`).
        if at.saturating_add(bytes.len() as u64) > io.limit {
            if FILESIZE_LIMIT_SIGNAL.load(Ordering::Relaxed) {
                let info =
                    crate::signal::user_info(abi::signal::SIGXFSZ, 0, abi::signal::code::SI_KERNEL);
                if let Some(p) = crate::proc::find(p.pid()) {
                    let _ = crate::signal::post(&p, info);
                }
            }
            return Err(Errno::EFBIG);
        }
        let n = v.write_at(s, at, &mut vfs::WriteBuf::new(&bytes))?;
        if n < bytes.len() {
            return Err(Errno::ENOSPC);
        }
        Ok(())
    })();
    if let Err(e) = result {
        kprintln!(
            "ktrace write failed, errno {}, tracing stopped for pid {}",
            e.get(),
            p.pid()
        );
        let old = {
            let mut inner = p.ktrace.inner.lock();
            match &inner.io {
                Some(cur) if cur.same(&io) => p.ktrace.free(&mut inner),
                _ => None,
            }
        };
        drop(old);
    }
}

/// A record of `ty` for the current thread with type part `part` and
/// payload `payload`, written now.
fn now_record(s: &Sleepable, ty: u16, part: &[u8], payload: impl FnOnce() -> Vec<u8>) {
    let Some(e) = Entered::new() else {
        return;
    };
    if !e.process().ktrace.traces(ty) {
        return;
    }
    let payload = payload();
    let Some(mut req) = request(&e.0, ty) else {
        return;
    };
    req.part = part.to_vec();
    req.payload = payload;
    submit(s, &e, req);
}

/// A record of `ty` for the current thread, queued.
fn queued_record(ty: u16, part: &[u8]) {
    let Some(e) = Entered::new() else {
        return;
    };
    if !e.process().ktrace.traces(ty) {
        return;
    }
    let Some(mut req) = request(&e.0, ty) else {
        return;
    };
    req.part = part.to_vec();
    enqueue(&e.0, req);
}

/// `ktrsyscall`: system call `code` entered with `args`.
pub(crate) fn syscall(s: &Sleepable, code: u32, args: &[u64]) {
    let part = KtrSyscall {
        ktr_code: i16::try_from(code.cast_signed()).unwrap_or(i16::MAX),
        ktr_narg: i16::try_from(args.len()).unwrap_or(0),
        ..KtrSyscall::default()
    };
    now_record(
        s,
        KTR_SYSCALL,
        &crate::uaccess::bytes_of(&part)[..KTR_SYSCALL_HEAD],
        || args.iter().flat_map(|a| a.to_ne_bytes()).collect(),
    );
}

/// `ktrsysret`: system call `code` returns `retval` or fails with
/// `error`.
pub(crate) fn sysret(s: &Sleepable, code: u32, error: i32, retval: u64) {
    let part = KtrSysret {
        // `short`, as FreeBSD's: -1 for none.
        ktr_code: i16::try_from(code.cast_signed()).unwrap_or(i16::MAX),
        ktr_eosys: 0,
        ktr_error: error,
        ktr_retval: if error == 0 { retval.cast_signed() } else { 0 },
    };
    now_record(s, KTR_SYSRET, crate::uaccess::bytes_of(&part), Vec::new);
}

/// `ktrnamei`: a path a lookup starts with.
pub(crate) fn namei(s: &Sleepable, path: &[u8]) {
    now_record(s, KTR_NAMEI, &[], || path.to_vec());
}

/// A copy of the buffers of a read or write about to be made, if the
/// current process traces `KTR_GENIO` (`cloneuio`).
pub(crate) fn genio_start(uio: &crate::fs::uio::UserIo) -> Option<crate::fs::uio::UserIo> {
    point(KTR_GENIO).then(|| uio.clone())
}

/// `ktrgenio`: after a read or write on descriptor `fd` that moved
/// `result`'s bytes, through buffers `before` copied as it started; up to
/// `kern.ktrace.genio_size` of the data. A failed read, or a write that
/// faulted, makes no record; another failed write records nothing moved.
pub(crate) fn genio(
    s: &Sleepable,
    fd: u64,
    write: bool,
    before: Option<crate::fs::uio::UserIo>,
    result: &Result<usize, Errno>,
) {
    let Some(before) = before else {
        return;
    };
    let n = match *result {
        Ok(n) => n,
        Err(e) if write && e != Errno::EFAULT => 0,
        Err(_) => return,
    };
    let part = KtrGenio {
        ktr_fd: crate::syscall::int(fd),
        ktr_rw: if write { UIO_WRITE } else { UIO_READ },
    };
    let max = GENIO_SIZE.load(Ordering::Relaxed) as usize;
    now_record(s, KTR_GENIO, crate::uaccess::bytes_of(&part), || {
        before.first_bytes(n, max)
    });
}

/// `ktrpsig`: signal `sig` acted on with handler `action` (0 for
/// `SIG_DFL`, 1 for `SIG_IGN`), `mask` blocked, code `code`. Queued.
pub(crate) fn psig(sig: u32, action: u64, mask: &Sigset, code: i32) {
    let part = KtrPsig {
        // `(char)sig`.
        signo: i32::from(u8::try_from(sig).unwrap_or(0).cast_signed()),
        action,
        code,
        mask: *mask,
        ..KtrPsig::default()
    };
    queued_record(KTR_PSIG, crate::uaccess::bytes_of(&part));
}

/// The scheduler's [`sched::SleepHook`]: a traced thread slept on
/// `wmesg` from `since` (platform time) until now, which is two
/// `KTR_CSW` records (`_sleep`'s `ktrcsw_out` and `ktrcsw`), queued.
fn slept(wmesg: &'static str, since: u64) {
    if !point(KTR_CSW) {
        return;
    }
    let Some(e) = Entered::new() else {
        return;
    };
    let mut part = KtrCsw {
        out: 1,
        user: 0,
        wmesg: [0; 8],
    };
    let n = wmesg.len().min(7);
    part.wmesg[..n].copy_from_slice(&wmesg.as_bytes()[..n]);
    if let Some(mut req) = request(&e.0, KTR_CSW) {
        // When it switched out.
        let ns = crate::time::realtime_base_ns() + crate::time::ticks_to_ns(since);
        req.header.ktr_time = abi::time::Timespec {
            tv_sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
            tv_nsec: i64::try_from(ns % 1_000_000_000).unwrap_or(0),
        };
        req.part = crate::uaccess::bytes_of(&part).to_vec();
        enqueue(&e.0, req);
    }
    part.out = 0;
    if let Some(mut req) = request(&e.0, KTR_CSW) {
        req.part = crate::uaccess::bytes_of(&part).to_vec();
        enqueue(&e.0, req);
    }
}

/// `utrace`'s record: `KTR_USER`. `EINVAL` past `KTR_USER_MAXLEN`,
/// `ENOMEM` with no request free.
pub(crate) fn user(s: &Sleepable, addr: u64, len: usize) -> Result<(), Errno> {
    if !point(KTR_USER) {
        return Ok(());
    }
    if len > KTR_USER_MAXLEN {
        return Err(Errno::EINVAL);
    }
    let mut data = alloc::vec![0u8; len];
    crate::uaccess::copy_in(&mut data, addr).map_err(|_| Errno::EFAULT)?;
    let Some(e) = Entered::new() else {
        return Ok(());
    };
    let mut req = request(&e.0, KTR_USER).ok_or(Errno::ENOMEM)?;
    req.payload = data;
    submit(s, &e, req);
    Ok(())
}

/// `ktrstruct`: structure `name` (`stat`, `sockaddr`, `caprights`, ...)
/// with contents `data`.
pub(crate) fn structure(s: &Sleepable, name: &str, data: &[u8]) {
    now_record(s, KTR_STRUCT, &[], || {
        let mut b = Vec::with_capacity(name.len() + 1 + data.len());
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        b.extend_from_slice(data);
        b
    });
}

/// [`structure`] of `v`, if the current process traces `KTR_STRUCT`
/// (`ktrstat`, `ktrcaprights`, `ktritimerval`, `ktrthrparam`).
pub(crate) fn structure_of<T: Plain>(s: &Sleepable, name: &str, v: &T) {
    if point(KTR_STRUCT) {
        structure(s, name, crate::uaccess::bytes_of(v));
    }
}

/// `ktrsockaddr`: a socket address, its first `sa_len` bytes, if the
/// current process traces `KTR_STRUCT`.
pub(crate) fn sockaddr(s: &Sleepable, sa: &[u8]) {
    if point(KTR_STRUCT) {
        let n = sa.first().map_or(0, |&l| usize::from(l)).min(sa.len());
        structure(s, "sockaddr", &sa[..n]);
    }
}

/// `ktrstructarray`: `data`, an array of structures `name` of `size`
/// bytes each, trimmed to `kern.ktrace.genio_size` (at least one).
pub(crate) fn structure_array(s: &Sleepable, name: &str, size: usize, data: &[u8]) {
    if size == 0 {
        return;
    }
    let max = (GENIO_SIZE.load(Ordering::Relaxed) as usize / size).max(1);
    let n = (data.len() / size).min(max);
    let part = KtrStructArray {
        struct_size: size as u64,
    };
    now_record(s, KTR_STRUCT_ARRAY, crate::uaccess::bytes_of(&part), || {
        let mut b = Vec::with_capacity(name.len() + 1 + n * size);
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        b.extend_from_slice(&data[..n * size]);
        b
    });
}

/// [`structure_array`] of `n` structures of `size` bytes at user address
/// `addr` (`ktrstructarray` from `UIO_USERSPACE`), if the current process
/// traces `KTR_STRUCT_ARRAY`; nothing if they cannot be read.
pub(crate) fn structure_array_user(s: &Sleepable, name: &str, size: usize, addr: u64, n: usize) {
    if !point(KTR_STRUCT_ARRAY) || size == 0 {
        return;
    }
    let max = (GENIO_SIZE.load(Ordering::Relaxed) as usize / size).max(1);
    let mut data = alloc::vec![0u8; n.min(max) * size];
    if crate::uaccess::copy_in(&mut data, addr).is_ok() {
        structure_array(s, name, size, &data);
    }
}

/// `ktrsysctl`: the name of the MIB a `sysctl` names, NUL-terminated.
pub(crate) fn sysctl(s: &Sleepable, name: &[u8]) {
    now_record(s, KTR_SYSCTL, &[], || {
        let mut b = name.to_vec();
        b.push(0);
        b
    });
}

/// `ktrdata`: exec's arguments (`KTR_ARGS`) or environment
/// (`KTR_ENVS`), each string NUL-terminated.
pub(crate) fn data(s: &Sleepable, ty: u16, bytes: &[u8]) {
    now_record(s, ty, &[], || bytes.to_vec());
}

/// `ktrfault`: a page fault at `vaddr` of access `ty` (`VM_PROT_*`).
/// Queued.
pub(crate) fn fault(vaddr: u64, ty: i32) {
    let part = KtrFault {
        vaddr,
        r#type: ty,
        _pad: 0,
    };
    queued_record(KTR_FAULT, crate::uaccess::bytes_of(&part));
}

/// `ktrfaultend`: the fault's result (`KERN_*`). Queued.
pub(crate) fn fault_end(result: i32) {
    let part = KtrFaultend { result };
    queued_record(KTR_FAULTEND, crate::uaccess::bytes_of(&part));
}

/// A capability violation (`enum ktr_cap_violation`, with its data).
#[derive(Clone, Copy, Debug)]
pub(crate) enum CapFail<'a> {
    /// A descriptor lacks rights: those needed and those held.
    NotCapable(CapRights, CapRights),
    /// `cap_rights_limit` would widen them.
    Increase(CapRights, CapRights),
    /// A call capability mode forbids; `fcntl` gives its command.
    Syscall(Option<i32>),
    /// A signal to another process.
    Signal(i32),
    /// An address capability mode forbids (`struct sockaddr`'s bytes).
    Sockaddr(&'a [u8]),
    /// A lookup outside a descriptor's subtree: its path.
    Namei(&'a [u8]),
    /// Another process's CPU set.
    Cpuset,
}

/// `ktrcapfail`: the current thread's violation `v`, from system call
/// `code`. Only a call capability mode allows records anything but
/// `Syscall`. Queued.
pub(crate) fn capfail(v: CapFail<'_>) {
    let Some(e) = Entered::new() else {
        return;
    };
    if !e.process().ktrace.traces(KTR_CAPFAIL) {
        return;
    }
    let code = e.0.syscall_code();
    let enabled = abi::syscall::lookup(u64::from(code))
        .and_then(|s| match s {
            abi::syscall::Slot::Live(n) => Some(n.info().cap_enabled),
            _ => None,
        })
        .unwrap_or(false);
    if !matches!(v, CapFail::Syscall(_)) && !enabled {
        return;
    }
    let mut part = KtrCapFail {
        cap_code: i16::try_from(code).unwrap_or(i16::MAX),
        cap_svflags: SV_FLAGS,
        ..KtrCapFail::default()
    };
    let rights = |d: &mut [u8], needed: &CapRights, held: &CapRights| {
        let n = size_of::<CapRights>();
        d[..n].copy_from_slice(crate::uaccess::bytes_of(needed));
        d[n..2 * n].copy_from_slice(crate::uaccess::bytes_of(held));
    };
    let int = |d: &mut [u8], v: i32| d[..4].copy_from_slice(&v.to_ne_bytes());
    let d = &mut part.cap_data;
    part.cap_type = match v {
        CapFail::NotCapable(n, h) => {
            rights(d, &n, &h);
            CAPFAIL_NOTCAPABLE
        }
        CapFail::Increase(n, h) => {
            rights(d, &n, &h);
            CAPFAIL_INCREASE
        }
        CapFail::Syscall(cmd) => {
            if let Some(c) = cmd {
                int(d, c);
            }
            CAPFAIL_SYSCALL
        }
        CapFail::Signal(sig) => {
            int(d, sig);
            CAPFAIL_SIGNAL
        }
        CapFail::Sockaddr(sa) => {
            // `sa_len` bytes, at most a `struct sockaddr`'s.
            let n = sa
                .len()
                .min(sa.first().map_or(0, |&l| usize::from(l)))
                .min(16);
            d[..n].copy_from_slice(&sa[..n]);
            CAPFAIL_SOCKADDR
        }
        CapFail::Namei(path) => {
            let n = path.len().min(d.len() - 1);
            d[..n].copy_from_slice(&path[..n]);
            CAPFAIL_NAMEI
        }
        CapFail::Cpuset => CAPFAIL_CPUSET,
    };
    let Some(mut req) = request(&e.0, KTR_CAPFAIL) else {
        return;
    };
    req.part = crate::uaccess::bytes_of(&part).to_vec();
    enqueue(&e.0, req);
}

/// `ktrprocctor_entered`: `p` is traced from now on, from its first
/// thread's view. Queued on it.
fn proc_ctor(p: &Process) {
    if let Some(t) = p.first_thread() {
        proc_ctor_of(&t);
    }
}

/// `ktrprocctor`, from thread `t`'s view (a new child's, or the one that
/// just exec'd). Queued on it.
pub(crate) fn proc_ctor_of(t: &UserThread) {
    let Some(mut req) = request(t, KTR_PROCCTOR) else {
        return;
    };
    req.part = crate::uaccess::bytes_of(&KtrProcCtor { sv_flags: SV_FLAGS }).to_vec();
    enqueue(t, req);
}

/// exec's arguments and environment (`KTR_ARGS`, `KTR_ENVS`), each
/// string NUL-terminated.
pub(crate) fn exec_args(s: &Sleepable, argv: &[&[u8]], envp: &[&[u8]]) {
    for (ty, strings) in [(KTR_ARGS, argv), (KTR_ENVS, envp)] {
        if point(ty) {
            let mut b = Vec::new();
            for a in strings {
                b.extend_from_slice(a);
                b.push(0);
            }
            data(s, ty, &b);
        }
    }
}

/// `ktrprocfork`: `child`, just forked from `parent`, inherits its
/// tracing with `KTRFAC_INHERIT`.
pub(crate) fn fork(parent: &Process, child: &Process) {
    if parent.ktrace.flags() == 0 {
        return;
    }
    {
        let pi = parent.ktrace.inner.lock();
        let flags = parent.ktrace.flags();
        if flags & KTRFAC_INHERIT == 0 {
            return;
        }
        let mut ci = child.ktrace.inner.lock();
        ci.io.clone_from(&pi.io);
        child.ktrace.flags.store(flags, Ordering::Relaxed);
    }
}

/// `ktrprocexec`: the current process execs a set-id program. Tracing
/// stops unless its tracer may debug any credential
/// (`PRIV_DEBUG_DIFFCRED`).
pub(crate) fn exec_set_id(p: &Process) {
    let old = {
        let mut inner = p.ktrace.inner.lock();
        match &inner.io {
            Some(io) if io.cred.priv_check(Priv::DebugDiffcred).is_err() => {
                p.ktrace.free(&mut inner)
            }
            _ => None,
        }
    };
    drop(old);
}

/// `ktrprocexit`: the current process exits; its `KTR_PROCDTOR` and what
/// it queued are written, and tracing ends.
pub(crate) fn exit(s: &Sleepable) {
    let Some(e) = Entered::new() else {
        return;
    };
    let p = e.process();
    if p.ktrace.flags() == 0 {
        return;
    }
    if let Some(req) = request(&e.0, KTR_PROCDTOR) {
        enqueue(&e.0, req);
    }
    {
        let _w = WRITE.lock(s);
        drain_locked(s, p);
    }
    let old = p.ktrace.free(&mut p.ktrace.inner.lock());
    drop(old);
}

/// `ktrcanset`: whether `me` may change `p`'s tracing. Only root changes
/// what root set.
fn can_set(me: &Process, p: &Process) -> bool {
    if p.ktrace.flags() & KTRFAC_ROOT != 0 && me.priv_check(Priv::Ktrace).is_err() {
        return false;
    }
    me.can_debug(p).is_ok()
}

/// `ktrops`: operation `ops` with facilities `facs` and trace file `io`
/// on `p`. Whether `me` was allowed to.
fn ops_on(me: &Process, p: &Arc<Process>, op: u32, facs: u32, io: Option<&Arc<TraceFile>>) -> bool {
    if !can_set(me, p) || me.can_see(p).is_err() {
        return false;
    }
    if p.has_exited() {
        return true;
    }
    let old = {
        let mut inner = p.ktrace.inner.lock();
        let mut old = None;
        if op == KTROP_SET {
            let Some(io) = io else {
                return false;
            };
            if inner.io.as_ref().is_some_and(|cur| !cur.same(io)) {
                old = inner.io.take();
            }
            if inner.io.is_none() {
                inner.io = Some(Arc::clone(io));
            }
            let mut f = p.ktrace.flags() | facs;
            if me.priv_check(Priv::Ktrace).is_ok() {
                f |= KTRFAC_ROOT;
            }
            p.ktrace.flags.store(f, Ordering::Relaxed);
        } else {
            let f = p.ktrace.flags() & !facs;
            p.ktrace.flags.store(f, Ordering::Relaxed);
            if f & KTRFAC_MASK == 0 {
                old = p.ktrace.free(&mut inner);
            }
        }
        old
    };
    if p.ktrace.flags() & KTRFAC_MASK != 0 {
        proc_ctor(p);
    }
    drop(old);
    true
}

/// `ktrsetchildren`: [`ops_on`] `top` and all its descendants.
fn ops_descend(
    me: &Process,
    top: &Arc<Process>,
    op: u32,
    facs: u32,
    io: Option<&Arc<TraceFile>>,
) -> bool {
    let mut ret = ops_on(me, top, op, facs, io);
    for p in crate::proc::descendants(top.pid()) {
        ret |= ops_on(me, &p, op, facs, io);
    }
    ret
}

/// `ktrace(fname, ops, facs, pid)`: by pid, or by process group with a
/// negative `pid`.
pub(crate) fn ktrace(
    s: &Sleepable,
    fname: u64,
    ops: u64,
    facs: u64,
    pid: u64,
) -> Result<u64, Errno> {
    let me = Process::current().ok_or(Errno::ESRCH)?;
    let int = |a| crate::syscall::int(a).cast_unsigned();
    let facs = int(facs) & !KTRFAC_ROOT;
    let op = int(ops) & KTROP_MASK;
    let descend = int(ops) & KTRFLAG_DESCEND != 0;
    let pid = crate::syscall::int(pid);
    if op != KTROP_CLEARFILE && facs == 0 {
        return Err(Errno::EINVAL);
    }
    let io = if op == KTROP_CLEAR {
        None
    } else {
        let file = crate::syscall::open_trace_file(s, &me, fname)?;
        let limit = me.limits.lock()[abi::resource::rlimit::FSIZE as usize].rlim_cur;
        Some(Arc::new(TraceFile {
            file,
            cred: me.cred(),
            limit,
        }))
    };
    let _e = Entered::new();
    if op == KTROP_CLEARFILE {
        let Some(io) = io else {
            return Err(Errno::EINVAL);
        };
        let mut error = Ok(0);
        for p in crate::proc::all() {
            let uses = p
                .ktrace
                .inner
                .lock()
                .io
                .as_ref()
                .is_some_and(|cur| cur.same(&io));
            if !uses {
                continue;
            }
            if can_set(&me, &p) {
                let old = p.ktrace.free(&mut p.ktrace.inner.lock());
                drop(old);
            } else {
                error = Err(Errno::EPERM);
            }
        }
        return error;
    }
    let ret = if pid < 0 {
        let members: Vec<_> = crate::proc::all()
            .into_iter()
            .filter(|p| p.pgid() == -pid && !p.is_zombie())
            .collect();
        if members.is_empty() {
            return Err(Errno::ESRCH);
        }
        let mut ret = false;
        for p in &members {
            ret |= if descend {
                ops_descend(&me, p, op, facs, io.as_ref())
            } else {
                ops_on(&me, p, op, facs, io.as_ref())
            };
        }
        ret
    } else {
        let p = crate::proc::find(pid)
            .filter(|p| !p.is_zombie())
            .ok_or(Errno::ESRCH)?;
        if descend {
            ops_descend(&me, &p, op, facs, io.as_ref())
        } else {
            ops_on(&me, &p, op, facs, io.as_ref())
        }
    };
    if ret { Ok(0) } else { Err(Errno::EPERM) }
}

/// Adds `kern.ktrace`, and hooks context switches.
pub(crate) fn sysctl_init(s: &Sleepable) {
    sched::set_sleep_hook(slept);
    use sysctl::{Number, Value};
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        let k = t
            .add_node(kern, Number::Auto, "ktrace", "KTRACE options")
            .expect("kern.ktrace");
        t.add_writable(
            k,
            Number::Auto,
            "request_pool",
            "Pool buffer size for ktrace(1)",
            0,
            Value::Uint(alloc::boxed::Box::new(|| POOL_SIZE.load(Ordering::Relaxed))),
            Arc::new(|b: &[u8]| {
                let want = b
                    .first_chunk::<4>()
                    .map(|c| u32::from_ne_bytes(*c))
                    .ok_or(Errno::EINVAL)?;
                resize_pool(want)
            }),
        )
        .expect("kern.ktrace.request_pool");
        t.add_writable(
            k,
            Number::Auto,
            "genio_size",
            "Maximum size of genio event payload",
            0,
            Value::Uint(alloc::boxed::Box::new(|| {
                GENIO_SIZE.load(Ordering::Relaxed)
            })),
            Arc::new(|b: &[u8]| {
                let v = b
                    .first_chunk::<4>()
                    .map(|c| u32::from_ne_bytes(*c))
                    .ok_or(Errno::EINVAL)?;
                GENIO_SIZE.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.ktrace.genio_size");
        t.add_writable(
            k,
            Number::Auto,
            "filesize_limit_signal",
            "Send SIGXFSZ to the traced process when the log size limit is exceeded",
            0,
            Value::Int(alloc::boxed::Box::new(|| {
                i32::from(FILESIZE_LIMIT_SIGNAL.load(Ordering::Relaxed))
            })),
            sysctl::int_writer(|v| {
                FILESIZE_LIMIT_SIGNAL.store(v != 0, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.ktrace.filesize_limit_signal");
    });
}

/// `ktrace_resize_pool`: grows the pool to `want`, or shrinks it by as
/// many as are free. `ENOSPC` if it could not grow that far (it always
/// can here).
fn resize_pool(want: u32) -> Result<(), Errno> {
    PRINT_MESSAGE.store(true, Ordering::Relaxed);
    let old = POOL_SIZE.load(Ordering::Relaxed);
    if want >= old {
        let more = want - old;
        POOL_FREE.fetch_add(i32::try_from(more).unwrap_or(i32::MAX), Ordering::Relaxed);
        POOL_SIZE.store(want, Ordering::Relaxed);
    } else {
        let mut gone = 0;
        while gone < old - want
            && POOL_FREE
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |f| {
                    (f > 0).then(|| f - 1)
                })
                .is_ok()
        {
            gone += 1;
        }
        POOL_SIZE.store(old - gone, Ordering::Relaxed);
    }
    Ok(())
}
