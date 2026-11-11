// SPDX-License-Identifier: BSD-2-Clause
//! POSIX asynchronous I/O: `aio_read`, `aio_write`, their vector forms,
//! `aio_fsync`, `aio_mlock`, and `lio_listio` queue requests that run
//! while the caller goes on; `aio_error`, `aio_return`, `aio_suspend`,
//! `aio_waitcomplete`, `aio_cancel`, a signal, or a kevent (`EVFILT_AIO`,
//! `EVFILT_LIO`) report them. The bookkeeping is [`kaio::Kaio`]'s; this
//! module does the I/O and the notifications.
//!
//! Requests run on one of two backends, as FreeBSD's do:
//! - Files and disks go to a pool of kernel threads (`aiodN`, FreeBSD's
//!   AIO daemons): `vfs.aio.target_aio_procs` start with a process's first
//!   request, more up to `vfs.aio.max_aio_procs` as requests wait, and an
//!   idle one beyond the target leaves after `vfs.aio.aiod_lifetime`. A
//!   worker takes on the requesting process's address space and does the
//!   transfer as `pread`/`pwrite` would (the file's offset with
//!   `LIO_FOFFSET`), a process running at most
//!   `vfs.aio.max_aio_per_proc` at once. Only regular files and
//!   directories on local file systems, and disks taking whole sectors,
//!   are safe: anything else (pipes, fifos, terminals, other devices)
//!   would tie a worker up indefinitely, and fails with `EOPNOTSUPP`
//!   unless `vfs.aio.enable_unsafe` is set.
//! - Sockets have their own queues, one per socket buffer: a request waits
//!   there until the socket is ready, and a thread of a second pool
//!   (`soaiodN`, `kern.ipc.aio.*`) then does it without blocking. A write
//!   on a blocking socket that went only in part waits again for the rest.
//!   `listen` fails while requests are queued, and a listening socket
//!   takes none.
//!
//! A request waiting in a queue can be cancelled; a running one cannot.
//! Exit and exec cancel what they can and wait for the rest
//! (`aio_proc_rundown`).
//!
//! Behavior reference: sys/kern/vfs_aio.c, sys/kern/sys_socket.c
//! (`soo_aio_queue`, `soaio_*`), sys/kern/uipc_socket.c
//! (`solisten_proto_check`).

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use abi::aio::{
    AIO_ALLDONE, AIO_CANCELED, AIO_NOTCANCELED, Aiocb, LIO_DSYNC, LIO_FOFFSET, LIO_MLOCK, LIO_NOP,
    LIO_NOWAIT, LIO_READ, LIO_READV, LIO_SYNC, LIO_VECTORED, LIO_WAIT, LIO_WRITE, LIO_WRITEV,
};
use abi::errno::Errno;
use abi::event::{
    EV_ADD, EV_CLEAR, EV_DISPATCH, EV_ENABLE, EV_EOF, EV_ONESHOT, EVFILT_AIO, EVFILT_LIO, Kevent,
    POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT, POLLRDNORM, POLLWRNORM, SIGEV_KEVENT, SIGEV_NONE,
    SIGEV_SIGNAL, SIGEV_THREAD_ID, Sigevent,
};
use abi::fcntl::{FREAD, FWRITE, O_DSYNC, O_EXEC, O_FSYNC, O_NONBLOCK};
use abi::time::Timespec;
use event::{Filter, Ident, Knote, Note, Notify, Source, Wake};
use kaio::{Backend, CancelStep, Done, JobId, LioId, Suspend};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use sysctl::{Number, Value};

use crate::fs::file::{File, FileKind};
use crate::fs::uio::UserIo;
use crate::mm::space::AddressSpace;
use crate::proc::Process;
use crate::uaccess::{Plain, UserPtr};

// SAFETY: integers and a `sigevent`, the padding explicit fields; any
// bits valid.
unsafe impl Plain for Aiocb {}

/// `MAX_AIO_PROCS`, `TARGET_AIO_PROCS`, ... (sys/aio.h, vfs_aio.c).
static ENABLE_UNSAFE: AtomicI32 = AtomicI32::new(0);
static UNSAFE_WARNINGS: AtomicU32 = AtomicU32::new(1);
static MAX_PROCS: AtomicI32 = AtomicI32::new(32);
static NUM_PROCS: AtomicI32 = AtomicI32::new(0);
static TARGET_PROCS: AtomicI32 = AtomicI32::new(4);
static MAX_QUEUE: AtomicI32 = AtomicI32::new(1024);
static NUM_QUEUE: AtomicI32 = AtomicI32::new(0);
static NUM_BUF: AtomicI32 = AtomicI32::new(0);
static NUM_UNMAPPED: AtomicI32 = AtomicI32::new(0);
/// `aiod_lifetime`, in ticks (`AIOD_LIFETIME_DEFAULT`, 30 s at `hz`
/// 1000).
static LIFETIME: AtomicI32 = AtomicI32::new(30_000);
static MAX_PER_PROC: AtomicI32 = AtomicI32::new(32);
static MAX_QUEUE_PER_PROC: AtomicI32 = AtomicI32::new(256);
static MAX_BUF: AtomicI32 = AtomicI32::new(16);
/// `kern.ipc.aio.*`: the socket pool's.
static SO_EMPTY_RESULTS: AtomicI32 = AtomicI32::new(0);
static SO_EMPTY_RETRIES: AtomicI32 = AtomicI32::new(0);
static SO_MAX_PROCS: AtomicI32 = AtomicI32::new(32);
static SO_NUM_PROCS: AtomicI32 = AtomicI32::new(0);
static SO_TARGET_PROCS: AtomicI32 = AtomicI32::new(4);
static SO_LIFETIME: AtomicI32 = AtomicI32::new(30_000);

/// `IOSIZE_MAX` with `debug.iosize_max_clamp` off.
const IOSIZE_MAX: u64 = i64::MAX as u64;
/// `maxphys`: the largest single disk transfer (`si_iosize_max` too).
pub(crate) const MAXPHYS: u64 = 1 << 20;
/// `_SIG_MAXSIG`.
const SIG_MAX: i32 = 128;

/// Requests' numbers (`jobseqno`).
static SEQNO: AtomicU64 = AtomicU64::new(1);

/// How a request asked to be told it finished.
#[derive(Clone, Copy, Debug)]
struct Sigev {
    notify: i32,
    signo: i32,
    value: u64,
    /// `sigev_notify_thread_id`.
    tid: i32,
}

impl Sigev {
    fn of(e: &Sigevent) -> Self {
        Self {
            notify: e.sigev_notify,
            signo: e.sigev_signo,
            value: e.sigev_value,
            tid: i32::try_from(e.sigev_un[0] & 0xffff_ffff).unwrap_or(0),
        }
    }

    const NONE: Self = Self {
        notify: SIGEV_NONE,
        signo: 0,
        value: 0,
        tid: 0,
    };

    fn signals(&self) -> bool {
        matches!(self.notify, SIGEV_SIGNAL | SIGEV_THREAD_ID)
    }
}

/// What a request does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Read,
    Write,
    /// `aio_fsync`: `O_SYNC` or `O_DSYNC` (the same here).
    Sync,
    Mlock,
}

/// Which socket buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Rcv,
    Snd,
}

/// A request's kernel half (`struct kaiocb` beyond the bookkeeping).
pub(crate) struct Job {
    id: JobId,
    p: Weak<Process>,
    kaio: Weak<Kaio>,
    /// The address space the transfer is in: the process's when it asked.
    space: Arc<AddressSpace>,
    file: Option<Arc<File>>,
    op: Op,
    /// `LIO_FOFFSET`: at the file's offset.
    foffset: bool,
    offset: i64,
    /// The buffers, taken by whoever does the transfer.
    io: SpinLock<Option<UserIo>>,
    /// `aio_mlock`'s range.
    buf: u64,
    nbytes: u64,
    sigev: Sigev,
    /// `EVFILT_AIO` knotes (`klist`).
    klist: Notify,
    /// For the knotes: finished, and the error to report.
    finished: AtomicBool,
    error: AtomicI32,
    /// A socket request's bytes so far (`aio_done`).
    sock_done: AtomicUsize,
    /// `inblock`, `outblock`, `msgsnd`, `msgrcv`.
    ru: [AtomicU64; 4],
    /// A disk's (`aio_qbio`): counted in `vfs.aio.num_unmapped_aio`.
    bio: AtomicBool,
}

/// A `lio_listio` group's kernel half.
struct LioData {
    klist: Notify,
    sigev: Sigev,
    kevent_posted: AtomicBool,
}

struct State {
    core: kaio::Kaio,
    jobs: BTreeMap<JobId, Arc<Job>>,
    lios: BTreeMap<LioId, Arc<LioData>>,
}

/// A process's requests (`kaioinfo`).
pub(crate) struct Kaio {
    st: SpinLock<State>,
    /// Who waits for a request to finish (`KAIO_WAKEUP`).
    wait: WaitQueue,
    p: Weak<Process>,
}

fn int(v: &AtomicI32) -> u32 {
    u32::try_from(v.load(Ordering::Relaxed).max(0)).unwrap_or(0)
}

/// `p`'s requests, if it made any.
fn kaio_of(p: &Process) -> Option<Arc<Kaio>> {
    p.aio.lock().clone()
}

/// `aio_init_aioinfo`: `p`'s requests, made on first use, with the
/// workers' pool started.
fn kaio_init(p: &Arc<Process>) -> Arc<Kaio> {
    let k = {
        let mut a = p.aio.lock();
        Arc::clone(a.get_or_insert_with(|| {
            Arc::new(Kaio {
                st: SpinLock::new(State {
                    core: kaio::Kaio::new(),
                    jobs: BTreeMap::new(),
                    lios: BTreeMap::new(),
                }),
                wait: WaitQueue::new(),
                p: Arc::downgrade(p),
            })
        }))
    };
    let want = int(&TARGET_PROCS).min(int(&MAX_PROCS));
    loop {
        let start = {
            let mut d = DAEMONS.lock();
            if d.procs + d.starting >= want {
                false
            } else {
                d.starting += 1;
                true
            }
        };
        if !start {
            break;
        }
        new_daemon();
    }
    k
}

// --- user-memory helpers ---

fn status_at(ujob: u64) -> UserPtr<i64> {
    UserPtr::new(ujob + core::mem::offset_of!(Aiocb, status) as u64)
}

fn error_at(ujob: u64) -> UserPtr<i64> {
    UserPtr::new(ujob + core::mem::offset_of!(Aiocb, error) as u64)
}

/// `store_status` and `store_error`: failures are ignored, as `suword`'s
/// are there.
fn store(ujob: u64, status: Option<i64>, error: Option<i32>) {
    if let Some(st) = status {
        let _ = status_at(ujob).write(&st);
    }
    if let Some(e) = error {
        let _ = error_at(ujob).write(&i64::from(e));
    }
}

// --- notification ---

/// `aio_sendsig`: the signal `sev` asks for, `SI_ASYNCIO`, to `p` or the
/// thread it names (nothing if that thread is not `p`'s).
fn send_signal(p: &Arc<Process>, sev: &Sigev) {
    let Ok(sig) = u32::try_from(sev.signo) else {
        return;
    };
    let mut info = crate::signal::user_info(sig, 0, abi::signal::code::SI_ASYNCIO);
    info.si_pid = 0;
    info.si_uid = 0;
    info.si_value = sev.value;
    if sev.notify == SIGEV_THREAD_ID {
        if let Some(t) = crate::uthread::find(sev.tid)
            && Arc::ptr_eq(t.process(), p)
        {
            let _ = crate::signal::post_thread(&t, info);
        }
    } else {
        let _ = crate::signal::post(p, info);
    }
}

impl Kaio {
    /// `aio_bio_done_notify`'s actions, the bookkeeping done.
    fn notified(self: &Arc<Self>, d: Done) {
        let (job, lio, lio_signal) = {
            let mut st = self.st.lock();
            let job = st.jobs.get(&d.id).cloned();
            if let (Some(j), Some(c)) = (&job, st.core.get(d.id)) {
                j.error
                    .store(c.error.map_or(0, Errno::get), Ordering::Relaxed);
                j.finished.store(true, Ordering::Release);
            }
            let lio = d
                .lio_done
                .and_then(|l| st.lios.get(&l).cloned().map(|x| (l, x)));
            // `LIOJ_SIGNAL` and not yet `LIOJ_SIGNAL_POSTED`.
            let lio_signal = match &lio {
                Some((l, data)) if d.notify && data.sigev.signals() => st
                    .core
                    .lio_mut(*l)
                    .filter(|g| g.signal && !g.signal_posted)
                    .map(|g| {
                        g.signal_posted = true;
                    })
                    .is_some(),
                _ => false,
            };
            (job, lio.map(|(_, x)| x), lio_signal)
        };
        if d.notify
            && let Some(j) = &job
        {
            if j.sigev.signals()
                && let Some(p) = self.p.upgrade()
            {
                send_signal(&p, &j.sigev);
            }
            j.klist.knote(1);
            if let Some(l) = &lio {
                if l.sigev.notify == SIGEV_KEVENT {
                    l.kevent_posted.store(true, Ordering::Release);
                    l.klist.knote(1);
                }
                if lio_signal && let Some(p) = self.p.upgrade() {
                    send_signal(&p, &l.sigev);
                }
            }
        }
        for s in d.fsync_ready {
            let j = self.st.lock().jobs.get(&s).cloned();
            if let Some(j) = j {
                // Already out of the sync queue, its hook cleared.
                schedule(&j);
            }
        }
        self.wait.wake_all();
    }

    /// `aio_complete`.
    fn complete(self: &Arc<Self>, id: JobId, status: i64, error: Option<Errno>) {
        let d = self.st.lock().core.complete(id, status, error);
        if let Some(d) = d {
            self.notified(d);
        }
    }

    /// `aio_cancel_job`: whether the request counts as cancelled.
    fn cancel_job(self: &Arc<Self>, id: JobId) -> bool {
        let step = self.st.lock().core.cancel_begin(id);
        let Some(job) = self.st.lock().jobs.get(&id).cloned() else {
            return false;
        };
        match step {
            CancelStep::Skip | CancelStep::Marked => false,
            CancelStep::Call(b) => {
                match b {
                    Backend::Daemon => {
                        DAEMONS.lock().jobs.retain(|j| !Arc::ptr_eq(j, &job));
                        self.complete(id, -1, Some(Errno::ECANCELED));
                    }
                    Backend::Sync => {
                        self.st.lock().core.unsync(id);
                        self.complete(id, -1, Some(Errno::ECANCELED));
                    }
                    Backend::Socket => socket_cancel(&job),
                }
                let (cancelled, d) = self.st.lock().core.cancel_end(id);
                if let Some(d) = d {
                    self.notified(d);
                }
                cancelled
            }
        }
    }

    /// `aio_free_entry` for what the bookkeeping freed: the request's
    /// knotes go, and its group's if it was the last, and the counts.
    fn freed(&self, job: Option<Arc<Job>>, lio: Option<LioId>) {
        NUM_QUEUE.fetch_sub(1, Ordering::Relaxed);
        if let Some(l) = lio {
            let data = self.st.lock().lios.remove(&l);
            if let Some(d) = data {
                d.klist.delete();
            }
        }
        if let Some(j) = job {
            j.klist.delete();
            if j.bio.load(Ordering::Relaxed) {
                NUM_UNMAPPED.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// Takes a finished request (`aio_return`'s or `aio_waitcomplete`'s):
    /// its `aiocb`, status, and error, its usage charged to `p`.
    fn take(&self, p: &Process, ujob: Option<u64>) -> Option<(u64, i64, Option<Errno>)> {
        let (core, job, lio) = {
            let mut st = self.st.lock();
            let (core, lio) = st.core.take_done(ujob)?;
            let job = st.jobs.remove(&core.id);
            (core, job, lio)
        };
        if let Some(j) = &job {
            for (c, r) in p.io_counts.iter().zip(&j.ru) {
                c.fetch_add(r.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
        self.freed(job, lio);
        Some((core.ujob, core.status, core.error))
    }
}

/// `aio_complete` for a job.
fn complete(job: &Job, status: i64, error: Option<Errno>) {
    if let Some(k) = job.kaio.upgrade() {
        k.complete(job.id, status, error);
    }
}

// --- the workers (AIO daemons) ---

struct Daemons {
    jobs: VecDeque<Arc<Job>>,
    procs: u32,
    starting: u32,
    idle: u32,
    next: u32,
}

static DAEMONS: SpinLock<Daemons> = SpinLock::new(Daemons {
    jobs: VecDeque::new(),
    procs: 0,
    starting: 0,
    idle: 0,
    next: 0,
});
static DAEMON_WAKE: WaitQueue = WaitQueue::new();
/// Bumped whenever a worker may find something new to take.
static DAEMON_GEN: AtomicU64 = AtomicU64::new(0);

/// `aio_newproc`: starts a worker, already counted in `starting`.
fn new_daemon() {
    let id = {
        let mut d = DAEMONS.lock();
        d.next += 1;
        d.next
    };
    let r = sched::spawn(&format!("aiod{id}"), sched::Priority::KERNEL, daemon);
    let mut d = DAEMONS.lock();
    d.starting -= 1;
    if r.is_ok() {
        d.procs += 1;
    }
    NUM_PROCS.store(
        i32::try_from(d.procs).unwrap_or(i32::MAX),
        Ordering::Relaxed,
    );
}

/// `aio_selectjob`: the first request whose process may run another.
fn select(d: &mut Daemons) -> Option<Arc<Job>> {
    let max = int(&MAX_PER_PROC);
    let mut i = 0;
    while i < d.jobs.len() {
        let j = Arc::clone(&d.jobs[i]);
        let Some(k) = j.kaio.upgrade() else {
            d.jobs.remove(i);
            continue;
        };
        let mut st = k.st.lock();
        if st.core.can_start(max) {
            d.jobs.remove(i);
            if !st.core.clear_cancel(j.id) {
                // Being cancelled: the cancel finishes it.
                continue;
            }
            st.core.started();
            return Some(j);
        }
        i += 1;
    }
    None
}

/// `aio_daemon`.
fn daemon(s: &Sleepable) {
    let lifetime_ns = || u64::from(int(&LIFETIME)) * 1_000_000;
    loop {
        let job = select(&mut DAEMONS.lock());
        if let Some(job) = job {
            run(s, &job);
            if let Some(k) = job.kaio.upgrade() {
                k.st.lock().core.stopped();
                // A rundown waits for this.
                k.wait.wake_all();
            }
            DAEMON_GEN.fetch_add(1, Ordering::Release);
            DAEMON_WAKE.wake_one();
            continue;
        }
        // Back in no process's space while idle.
        drop(sched::set_address_space(None));
        crate::mm::space::unload();
        let seen = DAEMON_GEN.load(Ordering::Acquire);
        DAEMONS.lock().idle += 1;
        let woke = crate::time::wait_timeout(
            s,
            &DAEMON_WAKE,
            || DAEMON_GEN.load(Ordering::Acquire) != seen,
            lifetime_ns(),
        );
        let mut d = DAEMONS.lock();
        d.idle -= 1;
        if !woke && d.jobs.is_empty() && d.procs > int(&TARGET_PROCS) {
            d.procs -= 1;
            NUM_PROCS.store(
                i32::try_from(d.procs).unwrap_or(i32::MAX),
                Ordering::Relaxed,
            );
            return;
        }
    }
}

/// `aio_schedule`: to the workers, cancellable while it waits.
fn schedule(job: &Arc<Job>) {
    let Some(k) = job.kaio.upgrade() else {
        return;
    };
    let spawn = {
        let mut d = DAEMONS.lock();
        let (ok, active) = {
            let mut st = k.st.lock();
            (
                st.core.set_cancel(job.id, Backend::Daemon),
                st.core.active(),
            )
        };
        if !ok {
            drop(d);
            k.complete(job.id, -1, Some(Errno::ECANCELED));
            return;
        }
        d.jobs.push_back(Arc::clone(job));
        // `aio_kick_nowait`: an idle worker, or a new one if allowed.
        let more = d.idle == 0
            && d.procs + d.starting < int(&MAX_PROCS)
            && active + d.starting < int(&MAX_PER_PROC);
        if more {
            d.starting += 1;
        }
        more
    };
    DAEMON_GEN.fetch_add(1, Ordering::Release);
    DAEMON_WAKE.wake_one();
    if spawn {
        new_daemon();
    }
}

/// Runs `job` in its process's address space.
fn run(s: &Sleepable, job: &Arc<Job>) {
    drop(sched::set_address_space(Some(
        Arc::clone(&job.space) as Arc<dyn sched::AddressSpace>
    )));
    match job.op {
        Op::Read | Op::Write => process_rw(s, job),
        Op::Sync => {
            let r = job
                .file
                .as_ref()
                .and_then(|f| f.vnode())
                .map_or(Ok(()), |v| v.fsync(s));
            match r {
                Ok(()) => complete(job, 0, None),
                Err(e) => complete(job, -1, Some(e)),
            }
        }
        Op::Mlock => {
            let r = job.p.upgrade().map_or(Err(Errno::ESRCH), |p| {
                crate::syscall::mlock_in(s, &p, job.buf, job.nbytes)
            });
            match r {
                Ok(()) => complete(job, 0, None),
                Err(e) => complete(job, -1, Some(e)),
            }
        }
    }
}

/// `SIGPIPE` to the requester for a write that found the reader gone
/// (`aio_process_rw`, `sousrsend`): not on a socket with `SO_NOSIGPIPE`.
fn sigpipe(job: &Job) {
    let quiet = match job.file.as_ref().map(|f| f.kind()) {
        Some(FileKind::Socket(so)) => so.so.nosigpipe(),
        Some(FileKind::Inet(so)) => so.so.nosigpipe(),
        _ => false,
    };
    if !quiet && let Some(p) = job.p.upgrade() {
        let _ = crate::signal::post(
            &p,
            crate::signal::user_info(abi::signal::SIGPIPE, 0, abi::signal::code::SI_KERNEL),
        );
    }
}

/// `aio_process_rw`: the transfer, as `pread`/`pwrite` (the offset is
/// ignored where there is none), or at the file's offset.
fn process_rw(s: &Sleepable, job: &Arc<Job>) {
    let Some(mut io) = job.io.lock().take() else {
        complete(job, -1, Some(Errno::EINVAL));
        return;
    };
    let Some(f) = job.file.as_ref() else {
        complete(job, -1, Some(Errno::EBADF));
        return;
    };
    let want = vfs::Uio::resid(&io);
    let read = job.op == Op::Read;
    let r = if read && want == 0 {
        Ok(0)
    } else if job.foffset {
        if read {
            f.read(s, &mut io)
        } else {
            f.write(s, &mut io)
        }
    } else {
        let r = if read {
            f.pread(s, &mut io, job.offset)
        } else {
            f.pwrite(s, &mut io, job.offset)
        };
        match r {
            Err(Errno::ESPIPE) if read => f.read(s, &mut io),
            Err(Errno::ESPIPE) => f.write(s, &mut io),
            r => r,
        }
    };
    let moved = want - vfs::Uio::resid(&io);
    let r = match r {
        Err(Errno::ERESTART | Errno::EINTR | Errno::EAGAIN) if moved > 0 => Ok(moved),
        r => r,
    };
    if job.bio.load(Ordering::Relaxed) {
        // `btodb`: 512-byte blocks.
        let blocks = (moved / 512) as u64;
        job.ru[if read { 0 } else { 1 }].fetch_add(blocks, Ordering::Relaxed);
    }
    match r {
        Ok(n) => complete(job, i64::try_from(n).unwrap_or(i64::MAX), None),
        Err(e) => {
            if e == Errno::EPIPE && !read {
                sigpipe(job);
            }
            complete(job, -1, Some(e));
        }
    }
}

// --- sockets ---

/// A socket's two queues (`sb_aiojobq`).
struct SockAio {
    rcv: SpinLock<SockQueue>,
    snd: SpinLock<SockQueue>,
    /// Woken by the socket (`sowakeup_aio`): to look at the side again.
    rcv_wake: Arc<SockWake>,
    snd_wake: Arc<SockWake>,
}

#[derive(Default)]
struct SockQueue {
    jobs: VecDeque<Arc<Job>>,
    /// A thread is working through it (`SB_AIO_RUNNING`).
    running: bool,
}

/// A socket buffer's wakeup: marks it for the socket pool. Allocates
/// nothing, so it may come from any context.
struct SockWake {
    pending: AtomicBool,
}

impl Wake for SockWake {
    fn wake(&self) {
        self.pending.store(true, Ordering::Release);
        SOAIO_GEN.fetch_add(1, Ordering::Release);
        SOAIO_WAKE.wake_one();
    }
}

impl SockAio {
    fn side(&self, side: Side) -> (&SpinLock<SockQueue>, &Arc<SockWake>) {
        match side {
            Side::Rcv => (&self.rcv, &self.rcv_wake),
            Side::Snd => (&self.snd, &self.snd_wake),
        }
    }

    fn busy(&self) -> bool {
        !self.rcv.lock().jobs.is_empty() || !self.snd.lock().jobs.is_empty()
    }
}

/// Sockets with requests, by socket.
static SOCKETS: SpinLock<BTreeMap<usize, Arc<SockAio>>> = SpinLock::new(BTreeMap::new());
static SOAIO_WAKE: WaitQueue = WaitQueue::new();
static SOAIO_GEN: AtomicU64 = AtomicU64::new(0);
/// The socket pool: threads, idle ones, ones starting, names given.
static SOAIO: SpinLock<(u32, u32, u32, u32)> = SpinLock::new((0, 0, 0, 0));

/// The socket a file is, by identity.
fn socket_key(f: &File) -> Option<usize> {
    match f.kind() {
        FileKind::Socket(so) => Some(Arc::as_ptr(&so.so).addr()),
        FileKind::Inet(so) => Some(Arc::as_ptr(&so.so).addr()),
        FileKind::Netlink(nl) => Some(core::ptr::from_ref(nl).addr()),
        _ => None,
    }
}

/// Whether `f` is a socket with requests queued (`listen` must fail:
/// `solisten_proto_check`).
pub(crate) fn socket_busy(f: &File) -> bool {
    let Some(key) = socket_key(f) else {
        return false;
    };
    let sa = SOCKETS.lock().get(&key).cloned();
    sa.is_some_and(|sa| sa.busy())
}

fn listening(f: &File) -> bool {
    match f.kind() {
        FileKind::Socket(so) => so.so.is_listening(),
        FileKind::Inet(so) => so.so.is_listening(),
        _ => false,
    }
}

/// Whether `f` is ready for `side` (`soaio_ready`): readable or
/// writable, or failed or shut down, which a transfer reports.
fn ready(s: &Sleepable, f: &File, side: Side, w: Option<&Arc<dyn Wake>>) -> bool {
    let ev = match side {
        Side::Rcv => POLLIN | POLLRDNORM,
        Side::Snd => POLLOUT | POLLWRNORM,
    };
    f.poll(s, ev, w) & (ev | POLLHUP | POLLERR | POLLNVAL) != 0
}

/// `soaio_queue_generic`.
fn socket_queue(s: &Sleepable, job: &Arc<Job>, f: &File) -> Result<(), Errno> {
    let side = match job.op {
        Op::Read => Side::Rcv,
        Op::Write => Side::Snd,
        _ => return Err(Errno::EINVAL),
    };
    if listening(f) {
        return Err(Errno::EINVAL);
    }
    let key = socket_key(f).ok_or(Errno::EINVAL)?;
    let sa = Arc::clone(SOCKETS.lock().entry(key).or_insert_with(|| {
        Arc::new(SockAio {
            rcv: SpinLock::new(SockQueue::default()),
            snd: SpinLock::new(SockQueue::default()),
            rcv_wake: Arc::new(SockWake {
                pending: AtomicBool::new(false),
            }),
            snd_wake: Arc::new(SockWake {
                pending: AtomicBool::new(false),
            }),
        })
    }));
    let k = job.kaio.upgrade().ok_or(Errno::ESRCH)?;
    let (q, w) = sa.side(side);
    {
        let mut q = q.lock();
        if !k.st.lock().core.set_cancel(job.id, Backend::Socket) {
            drop(q);
            k.complete(job.id, -1, Some(Errno::ECANCELED));
            return Ok(());
        }
        q.jobs.push_back(Arc::clone(job));
    }
    soaio_kick();
    // Ready now: to the pool; else when the socket says so.
    let wake: Arc<dyn Wake> = Arc::clone(w) as Arc<dyn Wake>;
    if ready(s, f, side, Some(&wake)) {
        w.wake();
    }
    Ok(())
}

/// `soo_aio_cancel`: out of its socket's queue; a request that moved
/// bytes finishes with them, else cancelled.
fn socket_cancel(job: &Arc<Job>) {
    if let Some(key) = job.file.as_deref().and_then(socket_key) {
        let sa = SOCKETS.lock().get(&key).cloned();
        if let Some(sa) = sa {
            for side in [Side::Rcv, Side::Snd] {
                sa.side(side).0.lock().jobs.retain(|j| !Arc::ptr_eq(j, job));
            }
        }
    }
    let done = job.sock_done.load(Ordering::Relaxed);
    if done != 0 {
        complete(job, i64::try_from(done).unwrap_or(i64::MAX), None);
    } else {
        complete(job, -1, Some(Errno::ECANCELED));
    }
}

/// Starts a socket-pool thread if work waits and none is idle
/// (`soaio_enqueue`'s `soaio_kproc_create`).
fn soaio_kick() {
    let start = {
        let mut p = SOAIO.lock();
        let (procs, idle, starting, _) = *p;
        let want = procs < int(&SO_TARGET_PROCS)
            || (idle == 0 && starting == 0 && procs < int(&SO_MAX_PROCS));
        if want {
            p.2 += 1;
            p.3 += 1;
        }
        want.then_some(p.3)
    };
    if let Some(id) = start {
        let r = sched::spawn(&format!("soaiod{id}"), sched::Priority::KERNEL, soaio_loop);
        let mut p = SOAIO.lock();
        p.2 -= 1;
        if r.is_ok() {
            p.0 += 1;
        }
        SO_NUM_PROCS.store(i32::try_from(p.0).unwrap_or(i32::MAX), Ordering::Relaxed);
    }
}

/// `soaio_kproc_loop`: works through the socket buffers marked ready.
fn soaio_loop(s: &Sleepable) {
    let lifetime_ns = || u64::from(int(&SO_LIFETIME)) * 1_000_000;
    loop {
        let mut did = false;
        let socks: Vec<Arc<SockAio>> = SOCKETS.lock().values().cloned().collect();
        for sa in socks {
            for side in [Side::Rcv, Side::Snd] {
                if sa.side(side).1.pending.swap(false, Ordering::AcqRel) {
                    did = true;
                    process_side(s, &sa, side);
                }
            }
        }
        // Sockets whose queues emptied leave the map.
        SOCKETS
            .lock()
            .retain(|_, sa| sa.busy() || Arc::strong_count(sa) > 1);
        if did {
            continue;
        }
        drop(sched::set_address_space(None));
        crate::mm::space::unload();
        let seen = SOAIO_GEN.load(Ordering::Acquire);
        SOAIO.lock().1 += 1;
        let woke = crate::time::wait_timeout(
            s,
            &SOAIO_WAKE,
            || SOAIO_GEN.load(Ordering::Acquire) != seen,
            lifetime_ns(),
        );
        let mut p = SOAIO.lock();
        p.1 -= 1;
        if !woke && p.0 > int(&SO_TARGET_PROCS) {
            p.0 -= 1;
            SO_NUM_PROCS.store(i32::try_from(p.0).unwrap_or(i32::MAX), Ordering::Relaxed);
            return;
        }
    }
}

/// `soaio_process_sb`: while the socket is ready, its requests in order.
fn process_side(s: &Sleepable, sa: &Arc<SockAio>, side: Side) {
    let (q, w) = sa.side(side);
    {
        let mut q = q.lock();
        if q.running {
            // The thread working through it looks again.
            w.pending.store(true, Ordering::Release);
            return;
        }
        q.running = true;
    }
    loop {
        let Some(front) = q.lock().jobs.front().cloned() else {
            break;
        };
        let Some(f) = front.file.clone() else {
            break;
        };
        if !ready(s, &f, side, None) {
            // Ask to hear when it is, then look once more, so a change
            // in between is not missed.
            let wake: Arc<dyn Wake> = Arc::clone(w) as Arc<dyn Wake>;
            if ready(s, &f, side, Some(&wake)) {
                continue;
            }
            break;
        }
        let job = {
            let mut q = q.lock();
            match q.jobs.front() {
                Some(j) if Arc::ptr_eq(j, &front) => q.jobs.pop_front(),
                _ => continue,
            }
        };
        let Some(job) = job else { break };
        let Some(k) = job.kaio.upgrade() else {
            continue;
        };
        if !k.st.lock().core.clear_cancel(job.id) {
            continue;
        }
        socket_job(s, sa, side, &job, &f);
    }
    // Requests left wait for the socket's wakeup, armed above.
    q.lock().running = false;
}

/// `soaio_process_job`: one transfer without blocking; a request a
/// blocking socket could not finish goes back to the head of its queue.
fn socket_job(s: &Sleepable, sa: &Arc<SockAio>, side: Side, job: &Arc<Job>, f: &Arc<File>) {
    drop(sched::set_address_space(Some(
        Arc::clone(&job.space) as Arc<dyn sched::AddressSpace>
    )));
    let Some(mut io) = job.io.lock().take() else {
        complete(job, -1, Some(Errno::EINVAL));
        return;
    };
    let nbio = f.flags() & O_NONBLOCK != 0;
    let mut retries = 0;
    loop {
        let before = vfs::Uio::resid(&io);
        // A send copies in more than it may send: the request goes on
        // from what the call says went, not from where its copy stopped.
        let saved = io.clone();
        let r = match (f.kind(), side) {
            (FileKind::Socket(so), Side::Rcv) => so.read(s, &mut io, true),
            (FileKind::Socket(so), Side::Snd) => so.write(s, &mut io, true),
            (FileKind::Inet(so), Side::Rcv) => so.read(s, &mut io, true),
            (FileKind::Inet(so), Side::Snd) => so.write(s, &mut io, true),
            (FileKind::Netlink(nl), Side::Rcv) => nl.read(s, &mut io, true),
            (FileKind::Netlink(nl), Side::Snd) => nl.write(&mut io),
            _ => Err(Errno::EOPNOTSUPP),
        };
        let moved = *r.as_ref().unwrap_or(&0);
        io = saved;
        io.skip(moved);
        let done = job.sock_done.fetch_add(moved, Ordering::Relaxed) + moved;
        // One message each way, however many calls it took.
        job.ru[if side == Side::Rcv { 3 } else { 2 }].store(1, Ordering::Relaxed);
        // A send that left some behind would have blocked: as
        // `EWOULDBLOCK` with progress there.
        let short = side == Side::Snd && r.is_ok() && moved < before;
        if (r == Err(Errno::EAGAIN) || short) && (done == 0 || !nbio) {
            SO_EMPTY_RESULTS.fetch_add(1, Ordering::Relaxed);
            // A socket that says it is ready and still is not gets a few
            // more tries, then waits for its wakeup like any other.
            retries += 1;
            if retries <= 16 && ready(s, f, side, None) {
                SO_EMPTY_RETRIES.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            *job.io.lock() = Some(io);
            let Some(k) = job.kaio.upgrade() else {
                return;
            };
            let (q, w) = sa.side(side);
            let mut ql = q.lock();
            if k.st.lock().core.set_cancel(job.id, Backend::Socket) {
                ql.jobs.push_front(Arc::clone(job));
                drop(ql);
                let wake: Arc<dyn Wake> = Arc::clone(w) as Arc<dyn Wake>;
                if ready(s, f, side, Some(&wake)) {
                    w.wake();
                }
            } else {
                drop(ql);
                if done != 0 {
                    complete(job, i64::try_from(done).unwrap_or(i64::MAX), None);
                } else {
                    complete(job, -1, Some(Errno::ECANCELED));
                }
            }
            return;
        }
        let r = match r {
            Err(Errno::ERESTART | Errno::EINTR | Errno::EAGAIN) if done != 0 => Ok(done),
            Ok(_) => Ok(done),
            r => r,
        };
        match r {
            Ok(n) => complete(job, i64::try_from(n).unwrap_or(i64::MAX), None),
            Err(e) => {
                if e == Errno::EPIPE && side == Side::Snd {
                    sigpipe(job);
                }
                complete(job, -1, Some(e));
            }
        }
        return;
    }
}

// --- kevents ---

/// `EVFILT_AIO` (`filt_aio`): fires once the request finishes, with
/// `EV_EOF`; `data` is its error.
struct AioFilter(Weak<Job>);

impl Filter for AioFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.0.upgrade().ok_or(Errno::EINVAL)?.klist.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        if let Some(j) = self.0.upgrade() {
            j.klist.remove(kn);
        }
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let Some(j) = self.0.upgrade() else {
            return false;
        };
        let finished = j.finished.load(Ordering::Acquire);
        note.kev.data = i64::from(if finished {
            j.error.load(Ordering::Relaxed)
        } else {
            Errno::EINPROGRESS.get()
        });
        if finished {
            note.kev.flags |= EV_EOF;
        }
        finished
    }
}

/// `EVFILT_LIO` (`filt_lio`): fires once the group's kevent is posted.
struct LioFilter(Weak<LioData>);

impl Filter for LioFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.0.upgrade().ok_or(Errno::EINVAL)?.klist.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        if let Some(l) = self.0.upgrade() {
            l.klist.remove(kn);
        }
    }

    fn event(&self, _kn: &Knote, _note: &mut Note, _hint: u64) -> bool {
        self.0
            .upgrade()
            .is_some_and(|l| l.kevent_posted.load(Ordering::Acquire))
    }
}

/// The kernel's own registration (`kqfd_register` with `EV_FLAG1`): the
/// one filter it hands over.
struct Registration(SpinLock<Option<Box<dyn Filter>>>);

impl Source for Registration {
    fn ident(&self, _kev: &Kevent) -> Result<Ident, Errno> {
        Ok(Ident::Other)
    }

    fn filter(&self, _kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        self.0.lock().take().ok_or(Errno::EINVAL)
    }
}

/// `kqfd_register`: `kev` with `filt` on the kqueue at descriptor `kqfd`
/// of `p`.
fn kq_register(
    s: &Sleepable,
    p: &Process,
    kqfd: i32,
    kev: &Kevent,
    filt: Box<dyn Filter>,
) -> Result<(), Errno> {
    let f = p
        .files
        .lock(s)
        .get_checked(kqfd, &crate::capsicum::rights::KQUEUE_CHANGE)?;
    // `kqueue_acquire`.
    let FileKind::Kqueue(k) = f.kind() else {
        return Err(Errno::EINVAL);
    };
    k.kq.register(kev, &Registration(SpinLock::new(Some(filt))))
}

/// User registrations of `EVFILT_AIO` and `EVFILT_LIO` are refused, as
/// FreeBSD's filters refuse a knote without the kernel's `EV_FLAG1`.
pub(crate) fn user_filter(filter: i16) -> Option<Errno> {
    matches!(filter, EVFILT_AIO | EVFILT_LIO).then_some(Errno::EPERM)
}

// --- queueing (`aio_aqueue`) ---

/// Whether a disk request has the shape `aio_qbio` takes: whole sectors,
/// no segment over `maxphys`, few enough segments. Others are not safe.
fn disk_ok(s: &Sleepable, f: &File, io: &UserIo) -> bool {
    let FileKind::Vnode(vf) = f.kind() else {
        return false;
    };
    let Some(d) = &vf.device else {
        return false;
    };
    if d.ops.dtype() != vfs::D_DISK {
        return false;
    }
    // `DIOCGSECTORSIZE`.
    let mut b = [0u8; 4];
    let sector = match d.ops.ioctl(s, 0x4004_6480, &mut b) {
        Ok(()) => u64::from(u32::from_ne_bytes(b)).max(1),
        Err(_) => 512,
    };
    let segs = io.segments();
    if segs.len() > int(&MAX_BUF) as usize {
        return false;
    }
    let total: u64 = segs.iter().map(|&(_, l)| l).sum();
    segs.iter().all(|&(_, l)| l % sector == 0 && l <= MAXPHYS) && total <= MAXPHYS
}

/// `vm_fault_quick_hold_pages`' check for a disk request: every page of
/// its buffers is there (`EFAULT` at once otherwise).
fn disk_buffers_mapped(io: &UserIo) -> bool {
    for (base, len) in io.segments() {
        let mut at = base & !(vm::PAGE_SIZE - 1);
        let end = base.saturating_add(len);
        while at < end {
            let mut b = [0u8; 1];
            let probe = at.max(base);
            if crate::uaccess::copy_in(&mut b, probe).is_err() {
                return false;
            }
            at += vm::PAGE_SIZE;
        }
    }
    true
}

/// Whether a file's requests are safe for the workers
/// (`aio_queue_file`'s test): regular files and directories on local
/// file systems; disks are judged by [`disk_ok`].
fn file_safe(f: &File) -> bool {
    let FileKind::Vnode(vf) = f.kind() else {
        return false;
    };
    let v = vf.vnode.vnode();
    matches!(v.kind(), vfs::Kind::Regular | vfs::Kind::Directory)
        && v.mount().flags() & abi::mount::MNT_LOCAL != 0
}

/// A file opened `O_PATH` (`path_fileops`).
fn path_only(f: &File) -> bool {
    matches!(f.kind(), FileKind::Vnode(_)) && f.flags() & (FREAD | FWRITE | O_EXEC) == 0
}

/// `counted_warning`.
fn unsafe_warning(p: &Process) {
    let c = UNSAFE_WARNINGS.load(Ordering::Relaxed);
    if c == 0
        || UNSAFE_WARNINGS
            .compare_exchange(c, c - 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let comm = p.image().comm;
    crate::klog::log(
        abi::syslog::LOG_INFO,
        format_args!(
            "pid {} ({}) is attempting to use unsafe AIO requests{}\n",
            p.pid(),
            core::str::from_utf8(&comm).unwrap_or("?"),
            if c > 1 { "" } else { " - not logging anymore" }
        ),
    );
}

/// `aio_aqueue`: queues the request at `ujob` (`ty` its operation, or
/// `LIO_NOP` to take the `aiocb`'s), in group `lio`. On failure the
/// error is also stored in the `aiocb`, as FreeBSD does.
fn aqueue(
    s: &Sleepable,
    p: &Arc<Process>,
    ujob: u64,
    lio: Option<LioId>,
    ty: i32,
) -> Result<(), Errno> {
    let k = kaio_init(p);
    store(ujob, Some(-1), Some(0));
    let r = aqueue_inner(s, p, &k, ujob, lio, ty);
    if let Err(e) = r {
        store(ujob, None, Some(e.get()));
    }
    r
}

fn aqueue_inner(
    s: &Sleepable,
    p: &Arc<Process>,
    k: &Arc<Kaio>,
    ujob: u64,
    lio: Option<LioId>,
    ty: i32,
) -> Result<(), Errno> {
    if NUM_QUEUE.load(Ordering::Relaxed) >= MAX_QUEUE.load(Ordering::Relaxed)
        || !k.st.lock().core.admits(int(&MAX_QUEUE_PER_PROC))
    {
        return Err(Errno::EAGAIN);
    }
    let mut cb = UserPtr::<Aiocb>::new(ujob)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let vty = if ty == LIO_NOP { cb.aio_lio_opcode } else { ty };
    let vectored = if vty & LIO_VECTORED != 0 {
        Some(UserIo::vector(cb.aio_buf, cb.aio_nbytes)?)
    } else {
        None
    };
    if cb.aio_nbytes > IOSIZE_MAX {
        return Err(Errno::EINVAL);
    }
    let sev = cb.aio_sigevent;
    if !matches!(
        sev.sigev_notify,
        SIGEV_KEVENT | SIGEV_SIGNAL | SIGEV_THREAD_ID | SIGEV_NONE
    ) {
        return Err(Errno::EINVAL);
    }
    if matches!(sev.sigev_notify, SIGEV_SIGNAL | SIGEV_THREAD_ID)
        && !(1..=SIG_MAX).contains(&sev.sigev_signo)
    {
        return Err(Errno::EINVAL);
    }
    let mut foffset = false;
    let opcode = if ty == LIO_NOP {
        match cb.aio_lio_opcode & !LIO_FOFFSET {
            o @ (LIO_WRITE | LIO_WRITEV | LIO_NOP | LIO_READ | LIO_READV) => {
                foffset = cb.aio_lio_opcode & LIO_FOFFSET != 0;
                o
            }
            _ => return Err(Errno::EINVAL),
        }
    } else {
        cb.aio_lio_opcode = ty;
        ty
    };
    let fd = cb.aio_fildes;
    let rights = &crate::capsicum::rights::NO;
    let file = match opcode {
        LIO_WRITE | LIO_WRITEV => {
            let f = p
                .files
                .lock(s)
                .get_checked(fd, &crate::capsicum::rights::PWRITE)?;
            if f.flags() & FWRITE == 0 {
                return Err(Errno::EBADF);
            }
            Some(f)
        }
        LIO_READ | LIO_READV => {
            let f = p
                .files
                .lock(s)
                .get_checked(fd, &crate::capsicum::rights::PREAD)?;
            if f.flags() & FREAD == 0 {
                return Err(Errno::EBADF);
            }
            Some(f)
        }
        LIO_SYNC | LIO_DSYNC => Some(
            p.files
                .lock(s)
                .get_checked(fd, &crate::capsicum::rights::FSYNC)?,
        ),
        LIO_MLOCK => None,
        LIO_NOP => Some(p.files.lock(s).get_checked(fd, rights)?),
        _ => return Err(Errno::EINVAL),
    };
    let rw = matches!(opcode, LIO_READ | LIO_READV | LIO_WRITE | LIO_WRITEV);
    if let Some(f) = &file {
        if opcode & LIO_SYNC != 0 && f.vnode().is_none() {
            return Err(Errno::EINVAL);
        }
        let chardev = f.vnode().is_some_and(|v| v.kind() == vfs::Kind::CharDevice);
        if rw && cb.aio_offset < 0 && !chardev {
            return Err(Errno::EINVAL);
        }
        if path_only(f) {
            return Err(Errno::EBADF);
        }
    }
    let id = SEQNO.fetch_add(1, Ordering::Relaxed);
    if opcode == LIO_NOP {
        return Ok(());
    }
    let op = match opcode {
        LIO_READ | LIO_READV => Op::Read,
        LIO_WRITE | LIO_WRITEV => Op::Write,
        LIO_SYNC | LIO_DSYNC => Op::Sync,
        _ => Op::Mlock,
    };
    let io = if rw {
        Some(match vectored {
            Some(v) => v,
            None => UserIo::one(cb.aio_buf, cb.aio_nbytes)?,
        })
    } else {
        None
    };
    let job = Arc::new(Job {
        id,
        p: Arc::downgrade(p),
        kaio: Arc::downgrade(k),
        space: p.space(),
        file: file.clone(),
        op,
        foffset,
        offset: cb.aio_offset,
        io: SpinLock::new(io),
        buf: cb.aio_buf,
        nbytes: cb.aio_nbytes,
        sigev: Sigev::of(&sev),
        klist: Notify::new(),
        finished: AtomicBool::new(false),
        error: AtomicI32::new(Errno::EINPROGRESS.get()),
        sock_done: AtomicUsize::new(0),
        ru: [const { AtomicU64::new(0) }; 4],
        bio: AtomicBool::new(false),
    });

    if sev.sigev_notify == SIGEV_KEVENT {
        // `sigev_notify_kevent_flags`: the union's first `unsigned short`.
        let evflags = u16::try_from(sev.sigev_un[0] & 0xffff).unwrap_or(u16::MAX);
        if evflags & !(EV_CLEAR | EV_DISPATCH | EV_ONESHOT) != 0 {
            return Err(Errno::EINVAL);
        }
        let kev = Kevent {
            ident: ujob,
            filter: EVFILT_AIO,
            flags: EV_ADD | EV_ENABLE | evflags,
            fflags: 0,
            data: 0,
            udata: sev.sigev_value,
            ext: [0; 4],
        };
        kq_register(
            s,
            p,
            sev.sigev_signo,
            &kev,
            Box::new(AioFilter(Arc::downgrade(&job))),
        )?;
    }

    store(ujob, None, Some(Errno::EINPROGRESS.get()));
    {
        let mut st = k.st.lock();
        let file_id = file.as_ref().map_or(0, |f| Arc::as_ptr(f).addr());
        st.core.begin(id, ujob, fd, file_id, op == Op::Sync, lio);
        st.jobs.insert(id, Arc::clone(&job));
    }
    let r = dispatch(s, p, k, &job);
    if let Err(e) = r {
        {
            let mut st = k.st.lock();
            st.core.abandon(id);
            st.jobs.remove(&id);
        }
        job.klist.delete();
        return Err(e);
    }
    NUM_QUEUE.fetch_add(1, Ordering::Relaxed);
    let d = k.st.lock().core.queued(id);
    if let Some(d) = d {
        k.notified(d);
    }
    Ok(())
}

/// Hands a new request to its backend (`fo_aio_queue`, `aio_queue_file`,
/// or the workers for `aio_mlock`).
fn dispatch(s: &Sleepable, p: &Arc<Process>, k: &Arc<Kaio>, job: &Arc<Job>) -> Result<(), Errno> {
    let Some(f) = &job.file else {
        schedule(job);
        return Ok(());
    };
    if socket_key(f).is_some() {
        return socket_queue(s, job, f);
    }
    let io_ok = job
        .io
        .lock()
        .as_ref()
        .is_some_and(|io| (job.op == Op::Read || job.op == Op::Write) && disk_ok(s, f, io));
    if io_ok {
        let mapped = job.io.lock().as_ref().is_some_and(disk_buffers_mapped);
        if !mapped {
            return Err(Errno::EFAULT);
        }
        job.bio.store(true, Ordering::Relaxed);
        NUM_UNMAPPED.fetch_add(1, Ordering::Relaxed);
        schedule(job);
        return Ok(());
    }
    if !file_safe(f) && ENABLE_UNSAFE.load(Ordering::Relaxed) == 0 {
        unsafe_warning(p);
        return Err(Errno::EOPNOTSUPP);
    }
    match job.op {
        Op::Read | Op::Write => schedule(job),
        Op::Sync => {
            let (waits, hooked) = {
                let mut st = k.st.lock();
                let waits = st.core.sync_deps(job.id) > 0;
                (waits, waits && st.core.set_cancel(job.id, Backend::Sync))
            };
            if !waits {
                schedule(job);
            } else if !hooked {
                // Cancelled before it could wait (`aio_cancel`).
                k.st.lock().core.unsync(job.id);
                k.complete(job.id, -1, Some(Errno::ECANCELED));
            }
        }
        Op::Mlock => return Err(Errno::EINVAL),
    }
    Ok(())
}

// --- the system calls ---

/// `aio_read`, `aio_readv`, `aio_write`, `aio_writev`, `aio_mlock`.
pub(crate) fn submit(s: &Sleepable, ujob: u64, op: i32) -> Result<u64, Errno> {
    let p = Process::current().ok_or(Errno::EINVAL)?;
    aqueue(s, &p, ujob, None, op).map(|()| 0)
}

/// `aio_fsync(op, aiocbp)`.
pub(crate) fn fsync(s: &Sleepable, op: i32, ujob: u64) -> Result<u64, Errno> {
    let ty = match u32::try_from(op).unwrap_or(0) {
        O_FSYNC => LIO_SYNC,
        O_DSYNC => LIO_DSYNC,
        _ => return Err(Errno::EINVAL),
    };
    submit(s, ujob, ty)
}

/// `aio_return(aiocbp)`: a finished request's result, freeing it.
pub(crate) fn aio_return(ujob: u64) -> Result<u64, Errno> {
    let p = Process::current().ok_or(Errno::EINVAL)?;
    let k = kaio_of(&p).ok_or(Errno::EINVAL)?;
    let (_, status, error) = k.take(&p, Some(ujob)).ok_or(Errno::EINVAL)?;
    store(ujob, Some(status), Some(error.map_or(0, Errno::get)));
    match error {
        Some(e) => Err(e),
        None => Ok(status.cast_unsigned()),
    }
}

/// `aio_error(aiocbp)`: never fails; the result is the error.
pub(crate) fn aio_error(ujob: u64) -> Result<u64, Errno> {
    let p = Process::current().ok_or(Errno::EINVAL)?;
    let r = match kaio_of(&p) {
        None => Errno::EINVAL.get(),
        Some(k) => {
            let e = k.st.lock().core.error_of(ujob);
            match e {
                Some(e) => e,
                // A request that failed to queue left -1 and its error.
                None => match status_at(ujob).read() {
                    Ok(-1) => error_at(ujob)
                        .read()
                        .map_or(-1, |e| i32::try_from(e).unwrap_or(-1)),
                    Err(_) => -1,
                    Ok(_) => Errno::EINVAL.get(),
                },
            }
        }
    };
    Ok(u64::from(r.cast_unsigned()))
}

/// A timeout's nanoseconds (`itimerfix`'s checks).
fn timeout_ns(ts: &Timespec) -> Result<u64, Errno> {
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    Ok(ts
        .tv_sec
        .cast_unsigned()
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec.cast_unsigned()))
}

/// `aio_suspend(list, nent, timeout)`.
pub(crate) fn suspend(s: &Sleepable, list: u64, nent: i32, timeout: u64) -> Result<u64, Errno> {
    let nent = usize::try_from(nent).map_err(|_| Errno::EINVAL)?;
    if nent > int(&MAX_QUEUE_PER_PROC) as usize {
        return Err(Errno::EINVAL);
    }
    let ns = if timeout == 0 {
        None
    } else {
        let ts = UserPtr::<Timespec>::new(timeout)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        Some(timeout_ns(&ts)?.max(1))
    };
    let mut jobs = Vec::new();
    jobs.try_reserve_exact(nent).map_err(|_| Errno::ENOMEM)?;
    for i in 0..nent {
        jobs.push(
            UserPtr::<u64>::new(list + 8 * i as u64)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        );
    }
    let p = Process::current().ok_or(Errno::EINVAL)?;
    let k = kaio_of(&p).ok_or(Errno::EAGAIN)?;
    if nent == 0 {
        return Ok(0);
    }
    let check = || k.st.lock().core.suspend(&jobs) != Suspend::Wait;
    match crate::signal::sleep(s, &k.wait, check, ns) {
        Ok(crate::signal::Woke::Done) => Ok(0),
        Ok(crate::signal::Woke::TimedOut) => Err(Errno::EAGAIN),
        Err(Errno::ERESTART) => Err(Errno::EINTR),
        Err(e) => Err(e),
    }
}

/// `aio_cancel(fd, aiocbp)`.
pub(crate) fn cancel(s: &Sleepable, fd: i32, ujob: u64) -> Result<u64, Errno> {
    let p = Process::current().ok_or(Errno::EINVAL)?;
    let f = p.files.lock(s).get(fd)?;
    let Some(k) = kaio_of(&p) else {
        return Ok(AIO_ALLDONE as u64);
    };
    if let FileKind::Vnode(vf) = f.kind()
        && vf
            .device
            .as_ref()
            .is_some_and(|d| d.ops.dtype() == vfs::D_DISK)
    {
        return Ok(AIO_NOTCANCELED as u64);
    }
    let one = (ujob != 0).then_some(ujob);
    let targets = k.st.lock().core.cancel_targets(fd, one);
    let (mut cancelled, mut notcancelled) = (0, 0);
    for id in targets {
        if k.cancel_job(id) {
            cancelled += 1;
        } else {
            notcancelled += 1;
        }
        if one.is_some() {
            break;
        }
    }
    let r = if one.is_some() && cancelled > 0 {
        AIO_CANCELED
    } else if notcancelled > 0 {
        AIO_NOTCANCELED
    } else if cancelled > 0 {
        AIO_CANCELED
    } else {
        AIO_ALLDONE
    };
    Ok(u64::from(r.cast_unsigned()))
}

/// `aio_waitcomplete(aiocbp, timeout)`: the next request to finish.
pub(crate) fn waitcomplete(s: &Sleepable, ujobp: u64, timeout: u64) -> Result<u64, Errno> {
    let _ = UserPtr::<u64>::new(ujobp).write(&0);
    // None: wait forever; Some(0): do not wait.
    let ns = if timeout == 0 {
        None
    } else {
        let ts = UserPtr::<Timespec>::new(timeout)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        if ts.tv_sec == 0 && ts.tv_nsec == 0 {
            Some(0)
        } else {
            Some(timeout_ns(&ts)?.max(1))
        }
    };
    let p = Process::current().ok_or(Errno::EINVAL)?;
    let k = kaio_init(&p);
    loop {
        if let Some((ujob, status, error)) = k.take(&p, None) {
            let _ = UserPtr::<u64>::new(ujobp).write(&ujob);
            store(ujob, Some(status), Some(error.map_or(0, Errno::get)));
            return match error {
                Some(e) => Err(e),
                None => Ok(status.cast_unsigned()),
            };
        }
        if ns == Some(0) {
            return Err(Errno::EAGAIN);
        }
        let check = || k.st.lock().core.has_done();
        match crate::signal::sleep(s, &k.wait, check, ns) {
            Ok(crate::signal::Woke::Done) => {}
            Ok(crate::signal::Woke::TimedOut) => return Err(Errno::EAGAIN),
            // With no time limit the call restarts, as FreeBSD's does.
            Err(Errno::ERESTART) if ns.is_some() => return Err(Errno::EINTR),
            Err(e) => return Err(e),
        }
    }
}

/// `lio_listio(mode, list, nent, sig)`.
pub(crate) fn lio_listio(
    s: &Sleepable,
    mode: i32,
    list: u64,
    nent: i32,
    sig: u64,
) -> Result<u64, Errno> {
    if mode != LIO_NOWAIT && mode != LIO_WAIT {
        return Err(Errno::EINVAL);
    }
    let n = usize::try_from(nent).map_err(|_| Errno::EINVAL)?;
    if n > int(&MAX_QUEUE_PER_PROC) as usize {
        return Err(Errno::EINVAL);
    }
    let sev = if sig != 0 && mode == LIO_NOWAIT {
        Some(
            UserPtr::<Sigevent>::new(sig)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    } else {
        None
    };
    let mut jobs = Vec::new();
    jobs.try_reserve_exact(n).map_err(|_| Errno::ENOMEM)?;
    for i in 0..n {
        jobs.push(
            UserPtr::<u64>::new(list + 8 * i as u64)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        );
    }
    let p = Process::current().ok_or(Errno::EINVAL)?;
    let k = kaio_init(&p);
    let sigev = sev.as_ref().map_or(Sigev::NONE, Sigev::of);
    match sigev.notify {
        SIGEV_NONE | SIGEV_KEVENT => {}
        SIGEV_SIGNAL | SIGEV_THREAD_ID if (1..=SIG_MAX).contains(&sigev.signo) => {}
        _ => return Err(Errno::EINVAL),
    }
    let data = Arc::new(LioData {
        klist: Notify::new(),
        sigev,
        kevent_posted: AtomicBool::new(false),
    });
    if sigev.notify == SIGEV_KEVENT {
        let kev = Kevent {
            ident: list,
            filter: EVFILT_LIO,
            flags: EV_ADD | EV_ENABLE,
            fflags: 0,
            data: 0,
            udata: sigev.value,
            ext: [0; 4],
        };
        kq_register(
            s,
            &p,
            sigev.signo,
            &kev,
            Box::new(LioFilter(Arc::downgrade(&data))),
        )?;
    }
    let l = {
        let mut st = k.st.lock();
        let l = st.core.lio_new(sigev.signals());
        st.lios.insert(l, Arc::clone(&data));
        l
    };
    let (mut nagain, mut nerror) = (0, 0);
    for &u in &jobs {
        if u == 0 {
            continue;
        }
        match aqueue(s, &p, u, Some(l), LIO_NOP) {
            Ok(()) => {}
            Err(Errno::EAGAIN) => nagain += 1,
            Err(_) => nerror += 1,
        }
    }
    let mut error = Ok(());
    if mode == LIO_WAIT {
        let check = || k.st.lock().core.lio_all_done(l);
        match crate::signal::sleep(s, &k.wait, check, None) {
            Ok(_) => {}
            Err(Errno::ERESTART) => error = Err(Errno::EINTR),
            Err(e) => error = Err(e),
        }
    } else {
        // `LIOJ_KEVENT_POSTED` and the signal, if every request already
        // finished (an empty list too, whose knote then goes with it).
        let (kevent, signal) = {
            let mut st = k.st.lock();
            if st.core.lio_all_done(l) {
                let signal = st
                    .core
                    .lio_mut(l)
                    .filter(|g| g.signal && !g.signal_posted)
                    .map(|g| g.signal_posted = true)
                    .is_some();
                (sigev.notify == SIGEV_KEVENT, signal)
            } else {
                (false, false)
            }
        };
        if kevent {
            data.kevent_posted.store(true, Ordering::Release);
            data.klist.knote(1);
        }
        if signal {
            send_signal(&p, &sigev);
        }
    }
    let freed = {
        let mut st = k.st.lock();
        let freed = st.core.lio_release(l);
        if freed {
            st.lios.remove(&l);
        }
        freed
    };
    if freed {
        data.klist.delete();
    }
    if nerror > 0 {
        Err(Errno::EIO)
    } else if nagain > 0 {
        Err(Errno::EAGAIN)
    } else {
        error.map(|()| 0)
    }
}

/// `aio_proc_rundown`: at exit or exec, `p`'s requests are cancelled
/// where they wait, waited for where they run, and freed.
pub(crate) fn rundown(s: &Sleepable, p: &Process) {
    let Some(k) = kaio_of(p) else {
        return;
    };
    loop {
        let targets = k.st.lock().core.begin_rundown();
        for id in targets {
            k.cancel_job(id);
        }
        if k.st.lock().core.idle() {
            break;
        }
        crate::time::wait_timeout(s, &k.wait, || k.st.lock().core.idle(), 1_000_000_000);
    }
    let freed = {
        let mut st = k.st.lock();
        let done = st.core.drain_done();
        done.into_iter()
            .map(|(j, lio)| (st.jobs.remove(&j.id), lio))
            .collect::<Vec<_>>()
    };
    for (job, lio) in freed {
        k.freed(job, lio);
    }
    let lios: Vec<_> = core::mem::take(&mut k.st.lock().lios)
        .into_values()
        .collect();
    for l in lios {
        l.klist.delete();
    }
    *p.aio.lock() = None;
}

// --- sysctls ---

fn rw_int(
    t: &mut sysctl::Tree,
    parent: sysctl::NodeId,
    name: &str,
    descr: &str,
    v: &'static AtomicI32,
) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        0,
        Value::Int(Box::new(|| v.load(Ordering::Relaxed))),
        sysctl::int_writer(|x| {
            v.store(x, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("an aio sysctl");
}

fn ro_int(
    t: &mut sysctl::Tree,
    parent: sysctl::NodeId,
    name: &str,
    descr: &str,
    v: &'static AtomicI32,
) {
    t.add_value(
        parent,
        Number::Auto,
        name,
        descr,
        Value::Int(Box::new(|| v.load(Ordering::Relaxed))),
    )
    .expect("an aio sysctl");
}

/// Adds `vfs.aio`, `kern.ipc.aio`, and `p1003_1b`'s AIO limits.
pub(crate) fn sysctl_init(s: &Sleepable) {
    crate::sysctl::with(s, |t| {
        let vfs = t.child(t.root(), "vfs").expect("vfs node");
        let a = t
            .add_node(vfs, Number::Auto, "aio", "Async IO management")
            .expect("vfs.aio");
        rw_int(
            t,
            a,
            "enable_unsafe",
            "Permit asynchronous IO on all file types, not just known-safe types",
            &ENABLE_UNSAFE,
        );
        t.add_writable(
            a,
            Number::Auto,
            "unsafe_warningcnt",
            "Warnings that will be triggered upon failed IO requests on unsafe files",
            0,
            Value::Uint(Box::new(|| UNSAFE_WARNINGS.load(Ordering::Relaxed))),
            alloc::sync::Arc::new(|b: &[u8]| {
                let v = b
                    .first_chunk::<4>()
                    .map(|c| u32::from_ne_bytes(*c))
                    .ok_or(Errno::EINVAL)?;
                UNSAFE_WARNINGS.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("vfs.aio.unsafe_warningcnt");
        rw_int(
            t,
            a,
            "max_aio_procs",
            "Maximum number of kernel processes to use for handling async IO ",
            &MAX_PROCS,
        );
        ro_int(
            t,
            a,
            "num_aio_procs",
            "Number of presently active kernel processes for async IO",
            &NUM_PROCS,
        );
        rw_int(
            t,
            a,
            "target_aio_procs",
            "Preferred number of ready kernel processes for async IO",
            &TARGET_PROCS,
        );
        rw_int(
            t,
            a,
            "max_aio_queue",
            "Maximum number of aio requests to queue, globally",
            &MAX_QUEUE,
        );
        ro_int(
            t,
            a,
            "num_queue_count",
            "Number of queued aio requests",
            &NUM_QUEUE,
        );
        ro_int(
            t,
            a,
            "num_buf_aio",
            "Number of aio requests presently handled by the buf subsystem",
            &NUM_BUF,
        );
        ro_int(
            t,
            a,
            "num_unmapped_aio",
            "Number of aio requests presently handled by unmapped I/O buffers",
            &NUM_UNMAPPED,
        );
        rw_int(
            t,
            a,
            "aiod_lifetime",
            "Maximum lifetime for idle aiod",
            &LIFETIME,
        );
        rw_int(
            t,
            a,
            "max_aio_per_proc",
            "Maximum active aio requests per process",
            &MAX_PER_PROC,
        );
        rw_int(
            t,
            a,
            "max_aio_queue_per_proc",
            "Maximum queued aio requests per process",
            &MAX_QUEUE_PER_PROC,
        );
        rw_int(
            t,
            a,
            "max_buf_aio",
            "Maximum buf aio requests per process",
            &MAX_BUF,
        );

        let kern = t.child(t.root(), "kern").expect("kern node");
        if let Some(ipc) = t.child(kern, "ipc") {
            let a = t
                .add_node(ipc, Number::Auto, "aio", "")
                .expect("kern.ipc.aio");
            ro_int(
                t,
                a,
                "empty_results",
                "socket operation returned EAGAIN",
                &SO_EMPTY_RESULTS,
            );
            ro_int(
                t,
                a,
                "empty_retries",
                "socket operation retries",
                &SO_EMPTY_RETRIES,
            );
            rw_int(
                t,
                a,
                "max_procs",
                "Maximum number of kernel processes to use for async socket IO",
                &SO_MAX_PROCS,
            );
            ro_int(
                t,
                a,
                "num_procs",
                "Number of active kernel processes for async socket IO",
                &SO_NUM_PROCS,
            );
            ro_int(
                t,
                a,
                "target_procs",
                "Preferred number of ready kernel processes for async socket IO",
                &SO_TARGET_PROCS,
            );
            rw_int(
                t,
                a,
                "lifetime",
                "Maximum lifetime for idle aiod",
                &SO_LIFETIME,
            );
        }

        // `p31b_setcfg`'s: POSIX's asynchronous I/O and its limits.
        if let Some(p) = t.child(t.root(), "p1003_1b") {
            for (n, name, descr, v) in [
                (1, "asynchronous_io", "", Some(200_112)),
                (
                    15,
                    "aio_listio_max",
                    "Maximum aio requests for a single lio_listio call",
                    None,
                ),
                (16, "aio_max", "", Some(1024)),
                (17, "aio_prio_delta_max", "", Some(0)),
            ] {
                let value = match v {
                    Some(v) => Value::int(v),
                    None => Value::Int(Box::new(|| MAX_QUEUE_PER_PROC.load(Ordering::Relaxed))),
                };
                let _ = t.add_value(p, Number::Fixed(n), name, descr, value);
            }
        }
    });
}
