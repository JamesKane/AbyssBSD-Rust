// SPDX-License-Identifier: BSD-2-Clause
//! Process tracing (docs/MULTIUSER.md §12.3): `ptrace(2)`, for `truss`,
//! debuggers, and kyua's `ptrace_test`.
//!
//! A traced process (`P_TRACED`) is its debugger's child for as long as
//! the tracing lasts: `PT_ATTACH` reparents it, remembering the real
//! parent (`p_oppid`), whose `wait` still finds it through its orphans,
//! and which gets it back once the debugger has seen it exit or detaches.
//! The process stops for its debugger (`ptracestop`) at every signal, at
//! system call entry and exit if asked (`PT_TO_SCE`, `PT_TO_SCX`,
//! `PT_SYSCALL`), after `exec` and `fork`, and at thread birth and death
//! with `PT_LWP_EVENTS`; a trace stop is a stop of every thread, which
//! the debugger's `wait` reports (`CLD_TRAPPED`). While it is stopped
//! (and waited for) the debugger reads and writes its registers and
//! memory, then lets it go on with a signal of its choice, or none.
//!
//! The thread that reports a stop is the process's `p_xthread`; one
//! attached to while asleep reports nothing and stays asleep
//! (`kern.ptrace_attach_transparent`, on in 15.1).
//!
//! Behavior reference: sys/kern/sys_process.c, sys/kern/kern_sig.c
//! (`ptracestop`, `sigprocess`, `sig_handle_first_stop`),
//! sys/kern/subr_syscall.c, sys/kern/kern_fork.c (`fork_return`),
//! sys/kern/kern_exit.c (`proc_reap`, the orphans), and each
//! architecture's `ptrace_machdep.c`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use abi::errno::Errno;
use abi::ptrace::*;
use abi::signal::{SIG_MAXSIG, SIGKILL, SIGSTOP, SIGTRAP, Siginfo};
use sched::Sleepable;
use sync::SpinLock;

use crate::arch::trap::TrapFrame;
use crate::proc::Process;
use crate::uaccess::{self, Plain, UserPtr};
use crate::uthread::UserThread;

// SAFETY: integers, signal sets, and byte arrays, the padding explicit
// (`abi-check` holds the C layouts).
unsafe impl Plain for PtraceIoDesc {}
// SAFETY: as above.
unsafe impl Plain for PtraceLwpinfo {}
// SAFETY: as above.
unsafe impl Plain for PtraceScRet {}
// SAFETY: as above.
unsafe impl Plain for PtraceVmEntry {}
// SAFETY: as above.
unsafe impl Plain for PtraceCoredump {}
// SAFETY: as above.
unsafe impl Plain for PtraceScRemote {}
// SAFETY: as above.
unsafe impl Plain for Reg {}
// SAFETY: as above.
unsafe impl Plain for Fpreg {}
// SAFETY: as above.
unsafe impl Plain for Dbreg {}
// SAFETY: as above.
unsafe impl Plain for Prstatus<Reg> {}
// SAFETY: as above.
unsafe impl Plain for Prpsinfo {}
// SAFETY: as above.
unsafe impl Plain for Thrmisc {}
#[cfg(target_arch = "x86_64")]
// SAFETY: as above.
unsafe impl Plain for abi::ptrace::regs_amd64::Segbasereg {}
#[cfg(target_arch = "aarch64")]
// SAFETY: as above.
unsafe impl Plain for abi::ptrace::regs_arm64::AddrMask {}

/// A thread's debugging state bits (`td_dbgflags`, `TDB_*`).
pub(crate) mod tdb {
    /// Suspended by `PT_SUSPEND`.
    pub(crate) const SUSPEND: u32 = 0x0000_0001;
    /// It has a stop to report (`td_xsig` valid).
    pub(crate) const XSIG: u32 = 0x0000_0002;
    /// The debugger wrote its registers or memory.
    pub(crate) const USERWR: u32 = 0x0000_0004;
    /// At system call entry.
    pub(crate) const SCE: u32 = 0x0000_0008;
    /// At system call exit.
    pub(crate) const SCX: u32 = 0x0000_0010;
    /// Its `exec` succeeded.
    pub(crate) const EXEC: u32 = 0x0000_0020;
    /// It forked a child the debugger follows (`td_dbg_forked`).
    pub(crate) const FORK: u32 = 0x0000_0040;
    /// A child to stop as it first returns to user mode.
    pub(crate) const STOPATFORK: u32 = 0x0000_0080;
    /// The child's stop.
    pub(crate) const CHILD: u32 = 0x0000_0100;
    /// A new thread (`PTRACE_LWP`).
    pub(crate) const BORN: u32 = 0x0000_0200;
    /// A thread exiting.
    pub(crate) const EXIT: u32 = 0x0000_0400;
    /// It `vfork`ed.
    pub(crate) const VFORK: u32 = 0x0000_0800;
    /// The attach's first stop is this thread's to report.
    pub(crate) const FSTP: u32 = 0x0000_1000;
    /// Single-stepping.
    pub(crate) const STEP: u32 = 0x0000_2000;
    /// Stopped in `ptracestop`.
    pub(crate) const SSWITCH: u32 = 0x0000_4000;
    /// Stopped at the system call boundary (`PT_SC_REMOTE` may run).
    pub(crate) const BOUNDARY: u32 = 0x0000_8000;
    /// The debugger asked it to dump core.
    pub(crate) const COREDUMPREQ: u32 = 0x0001_0000;
    /// The debugger asked it to make a system call.
    pub(crate) const SCREMOTEREQ: u32 = 0x0002_0000;
}

/// A process's tracing (`P_TRACED` and the fields that go with it).
#[derive(Default)]
pub(crate) struct Tracing {
    /// The real parent (`p_oppid`), while traced.
    pub(crate) oppid: i32,
    /// The events the debugger stops for (`p_ptevents`).
    pub(crate) events: u32,
    /// The attach's stop is still to come (`P2_PTRACE_FSTP`).
    pub(crate) first_stop: bool,
    /// The thread whose stop the debugger sees (`p_xthread`).
    pub(crate) xthread: Option<i32>,
    /// A request is being served (`P2_PTRACEREQ`).
    busy: bool,
    /// `PT_KILL` or `SIGKILL` came (`P_WKILLED`).
    pub(crate) killed: bool,
    /// A traced `vfork` child (`P_PPTRACE`): its signals wait.
    pub(crate) pptrace: bool,
}

/// A thread's debugging state: the `TDB_*` bits, read without a lock,
/// and the rest.
pub(crate) struct ThreadDebug {
    pub(crate) flags: AtomicU32,
    /// Where its user registers are while it is in the kernel: the frame
    /// it entered with (`td_frame`). Only it sets this; others read it
    /// only while it is stopped.
    pub(crate) frame: AtomicPtr<TrapFrame>,
    state: SpinLock<DebugState>,
}

#[derive(Default)]
struct DebugState {
    /// The signal it stopped with, then the one the debugger hands it
    /// (`td_xsig`).
    xsig: u32,
    /// The signal's information, while it stops for one (`td_si`).
    si: Siginfo,
    /// The child a followed fork made (`td_dbg_forked`).
    forked: i32,
    /// The system call it is in (`td_sa`): number and arguments.
    code: u32,
    args: [u64; 8],
    narg: u32,
    /// How the call ended (`td_errno`, `td_retval`).
    error: i32,
    retval: [u64; 2],
    /// A `PT_SC_REMOTE` call, and its result once made.
    remote: Option<RemoteCall>,
    /// A `PT_COREDUMP` request, and its outcome once served
    /// (`thr_coredump_req`).
    coredump: Option<CoreRequest>,
}

struct CoreRequest {
    file: Arc<crate::fs::file::File>,
    limit: u64,
    all: bool,
    done: Option<Result<(), Errno>>,
}

#[derive(Clone, Copy, Default)]
struct RemoteCall {
    code: u32,
    args: [u64; 8],
    nargs: u32,
    ret: PtraceScRet,
}

impl Default for ThreadDebug {
    fn default() -> Self {
        Self {
            flags: AtomicU32::new(0),
            frame: AtomicPtr::new(core::ptr::null_mut()),
            state: SpinLock::new(DebugState::default()),
        }
    }
}

impl ThreadDebug {
    pub(crate) fn has(&self, bits: u32) -> bool {
        self.flags.load(Ordering::Acquire) & bits != 0
    }

    pub(crate) fn set(&self, bits: u32) {
        self.flags.fetch_or(bits, Ordering::AcqRel);
    }

    pub(crate) fn clear(&self, bits: u32) {
        self.flags.fetch_and(!bits, Ordering::AcqRel);
    }

    /// Records the frame the thread entered the kernel with.
    pub(crate) fn entered(&self, frame: &mut TrapFrame) {
        self.frame.store(frame, Ordering::Release);
    }

    /// The current call, for `PT_GET_SC_ARGS` and `PT_LWPINFO`.
    pub(crate) fn set_call(&self, code: u32, args: &[u64]) {
        let mut st = self.state.lock();
        st.code = code;
        st.narg = u32::try_from(args.len()).unwrap_or(0);
        st.args = [0; 8];
        st.args[..args.len().min(8)].copy_from_slice(&args[..args.len().min(8)]);
    }

    /// A new thread's copy of its creator's call (`td_sa`).
    pub(crate) fn inherit_call(&self, from: &Self) {
        let (code, args, narg) = {
            let f = from.state.lock();
            (f.code, f.args, f.narg)
        };
        let mut st = self.state.lock();
        st.code = code;
        st.args = args;
        st.narg = narg;
    }

    /// How the call ended, for `PT_GET_SC_RET`.
    pub(crate) fn set_result(&self, error: i32, retval: [u64; 2]) {
        let mut st = self.state.lock();
        st.error = error;
        st.retval = retval;
    }
}

/// `kern.kill_on_debugger_exit`: a debugger that exits without
/// detaching takes the processes it traced with it.
pub(crate) static KILL_ON_DEBUGGER_EXIT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(true);

/// Adds `kern.kill_on_debugger_exit`.
pub(crate) fn sysctl_init(s: &Sleepable) {
    use sysctl::{Number, Value};
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        t.add_writable(
            kern,
            Number::Auto,
            "kill_on_debugger_exit",
            "Kill ptraced processes when debugger exits",
            0,
            Value::Int(alloc::boxed::Box::new(|| {
                i32::from(KILL_ON_DEBUGGER_EXIT.load(Ordering::Relaxed))
            })),
            sysctl::int_writer(|v| {
                KILL_ON_DEBUGGER_EXIT.store(v != 0, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.kill_on_debugger_exit");
    });
}

/// The current thread's process is traced: the cheap test.
fn traced_now() -> bool {
    Process::with_current(Process::is_traced).unwrap_or(false)
}

/// `ptracestop`: the current thread `t` stops for its debugger at an
/// event, with `sig` as the stop's signal (`SIGTRAP` for events,
/// `SIGSTOP` for a followed child's first stop). It goes on once the
/// debugger lets it; a signal the debugger hands it then is sent (to the
/// thread that takes it). Returns that signal.
pub(crate) fn stop(s: &Sleepable, t: &UserThread, sig: u32) -> u32 {
    let xsig = stop_for(s, t, sig);
    if xsig != 0 {
        let _ = inject(t.process(), xsig);
    }
    xsig
}

/// The stop of [`stop`], without sending the signal: the debugger's
/// choice of signal, or 0.
fn stop_for(s: &Sleepable, t: &UserThread, sig: u32) -> u32 {
    let p = t.process();
    t.dbg.state.lock().xsig = sig;
    t.dbg.set(tdb::XSIG);
    loop {
        if !p.is_traced() || !t.dbg.has(tdb::XSIG) {
            break;
        }
        if p.trace.lock().killed {
            // A `PT_KILL` came, or a `SIGKILL`: that is the outcome.
            t.dbg.clear(tdb::XSIG);
            t.dbg.state.lock().xsig = SIGKILL;
            p.trace.lock().events = 0;
            break;
        }
        if p.exiting_single() && !t.dbg.has(tdb::EXIT) {
            // The process is exiting: only thread exits still stop.
            t.dbg.clear(tdb::XSIG);
            return 0;
        }
        first_stop(p, Some(t), sig);
        t.dbg.clear(tdb::STOPATFORK);
        loop {
            t.dbg.set(tdb::SSWITCH);
            p.park_until(s, t, || t.dbg.has(tdb::SCREMOTEREQ | tdb::COREDUMPREQ));
            t.dbg.clear(tdb::SSWITCH);
            if t.dbg.has(tdb::SCREMOTEREQ | tdb::COREDUMPREQ) {
                remote_request(s, t);
                continue;
            }
            break;
        }
        {
            let mut tr = p.trace.lock();
            if tr.xthread == Some(t.tid()) {
                tr.xthread = None;
            }
        }
        if !p.is_traced() {
            break;
        }
        if t.dbg.has(tdb::SUSPEND) && !p.exiting_single() {
            // Held by `PT_SUSPEND`: it waits on (parked by
            // `suspension`) and looks again.
            continue;
        }
    }
    t.dbg.state.lock().xsig
}

/// `sig_handle_first_stop`: thread `t` (or none, for an attach that stops
/// the process directly) reports the stop with `sig`, unless another has
/// it or the attach's stop is someone else's to report; the others stop
/// too.
fn first_stop(p: &Process, t: Option<&UserThread>, sig: u32) {
    {
        let mut tr = p.trace.lock();
        if t.is_some_and(|t| !t.dbg.has(tdb::FSTP)) && (tr.first_stop || tr.xthread.is_some()) {
            return;
        }
        tr.xthread = t.map(UserThread::tid);
        tr.first_stop = false;
    }
    if let Some(t) = t {
        t.dbg.clear(tdb::FSTP);
    }
    p.begin_trace_stop(t, sig);
}

/// Sends `sig`, which the debugger handed on, to the thread that takes it
/// (`tdsendsignal` with `KSI_PTRACE`): it goes to its action without
/// another stop.
fn inject(p: &Arc<Process>, sig: u32) -> Option<Arc<UserThread>> {
    crate::signal::post_from_debugger(p, sig)
}

/// The signal part of `sigprocess` for a traced process: thread `t` (the
/// current one) took `sig` with `info` off its queues and stops for the
/// debugger to see it. Afterwards the signal is put back for its action
/// (marked handed on), replaced by the one the debugger chose, or gone.
pub(crate) fn signal_stop(s: &Sleepable, t: &UserThread, sig: u32, info: Siginfo) {
    let p = t.process();
    t.dbg.state.lock().si = info;
    let new = stop_for(s, t, sig);
    t.dbg.state.lock().si.si_signo = 0;
    if new == 0 {
        // Discarded.
        return;
    }
    // `ptracestop`'s tail: the same signal goes back on the thread's
    // queue as it was; another is sent, handed on, to the thread that
    // takes it, and is this thread's to act on only if that is this
    // thread.
    let info = Siginfo {
        si_signo: new.cast_signed(),
        ..info
    };
    let masked = t.sig.lock().mask.has(new);
    let traced = p.is_traced();
    if new == sig {
        // Acted on at once unless it is blocked now or no longer traced
        // (then it waits, as an ordinary signal).
        crate::signal::requeue(p, Some(t), info, !masked && traced);
    } else if !inject(p, new).is_some_and(|td2| core::ptr::eq(td2.as_ref(), t)) {
        return;
    }
    // `sigprocess`: one it blocks now waits on the process too; one no
    // longer traced, on its queue (FreeBSD's copies, which come back
    // through the debugger once more).
    if masked {
        crate::signal::requeue(p, None, info, false);
    } else if !traced && new != sig {
        crate::signal::requeue(p, Some(t), info, false);
    }
}

/// Whether a signal (not one the debugger handed on) must stop for the
/// debugger first: the process is traced, and not as a `vfork` child
/// before its exec (`P_PPTRACE`).
pub(crate) fn wants_signal(p: &Process) -> bool {
    p.is_traced() && !p.trace.lock().pptrace
}

/// A signal is sent to traced process `p`: with the attach's stop
/// pending, a stop signal stops it at once, no thread reporting
/// (`kern.ptrace_attach_transparent`). Returns whether the signal was
/// used up so.
pub(crate) fn signal_sent(p: &Process, sig: u32, stop_prop: bool) -> bool {
    let tr = p.trace.lock();
    if stop_prop && tr.first_stop {
        drop(tr);
        first_stop(p, None, sig);
        return true;
    }
    false
}

/// System call entry in a traced process (`syscallenter`): the call's
/// arguments are kept for the debugger, and it stops there if asked
/// (`PTRACE_SCE`). Returns whether the debugger wrote the registers, so
/// that the call must be read again.
pub(crate) fn syscall_enter(s: &Sleepable, code: u32, args: &[u64]) -> bool {
    let Some(t) = UserThread::current() else {
        return false;
    };
    let p = t.process();
    t.dbg.clear(tdb::USERWR);
    t.dbg.set(tdb::SCE);
    t.dbg.set_call(code, args);
    if p.trace.lock().events & PTRACE_SCE != 0 {
        stop(s, &t, SIGTRAP);
    }
    t.dbg.has(tdb::USERWR)
}

/// System call exit in a traced process (`syscallret`): after `exec` or
/// a followed `fork`, or if asked (`PTRACE_SCX`), it stops there, at the
/// boundary.
pub(crate) fn syscall_exit(s: &Sleepable, error: i32, retval: [u64; 2]) {
    let Some(t) = UserThread::current() else {
        return;
    };
    let p = t.process();
    t.dbg.clear(tdb::SCE | tdb::BOUNDARY);
    t.dbg.set_result(error, retval);
    if p.is_traced() {
        t.dbg.set(tdb::SCX);
        let events = p.trace.lock().events;
        if t.dbg.has(tdb::FORK | tdb::EXEC) || events & PTRACE_SCX != 0 {
            t.dbg.set(tdb::BOUNDARY);
            stop(s, &t, SIGTRAP);
        }
    }
    t.dbg
        .clear(tdb::SCX | tdb::EXEC | tdb::FORK | tdb::BOUNDARY);
}

/// A `vfork` parent's wait is over (`ast_vfork`): with `PTRACE_VFORK` it
/// stops to say so (`PL_FLAG_VFORK_DONE`).
pub(crate) fn vfork_done(s: &Sleepable) {
    let Some(t) = UserThread::current() else {
        return;
    };
    if !t.dbg.has(tdb::VFORK) {
        return;
    }
    let p = t.process();
    if p.is_traced() && p.trace.lock().events & PTRACE_VFORK != 0 {
        stop(s, &t, SIGTRAP);
    }
    t.dbg.clear(tdb::VFORK);
}

/// `fork` in process `parent` made `child` (`do_fork`): a `vfork` is
/// marked for its stops, and with `PTRACE_FORK` the debugger follows the
/// child, traced and its child from the start, stopping as it first
/// returns to user mode.
pub(crate) fn forked(
    parent: &Arc<Process>,
    child: &Arc<Process>,
    child_thread: &UserThread,
    vfork: bool,
) {
    let Some(t) = UserThread::current() else {
        return;
    };
    if vfork {
        t.dbg.set(tdb::VFORK);
    }
    let follow = parent.is_traced() && parent.trace.lock().events & PTRACE_FORK != 0;
    if !follow {
        return;
    }
    t.dbg.set(tdb::FORK);
    t.dbg.state.lock().forked = child.pid();
    child_thread.dbg.set(tdb::STOPATFORK);
    let debugger = parent.ppid_tree();
    child.set_traced(parent.pid(), true);
    crate::proc::reparent_traced(child, debugger);
}

/// A new thread `t` of a traced process with `PTRACE_LWP` is born
/// (`TDB_BORN`).
pub(crate) fn thread_made(p: &Process, t: &UserThread) {
    if p.is_traced() && p.trace.lock().events & PTRACE_LWP != 0 {
        t.dbg.set(tdb::BORN);
    }
}

/// A new thread's first return to user mode (`fork_return`): a followed
/// child stops with `SIGSTOP` for its debugger; a new thread of a traced
/// process reports the creating call's exit, if asked or born with
/// `PTRACE_LWP`.
pub(crate) fn thread_start(s: &Sleepable) {
    let Some(t) = UserThread::current() else {
        return;
    };
    let p = t.process();
    if t.dbg.has(tdb::STOPATFORK) {
        if p.is_traced() {
            t.dbg.set(tdb::CHILD | tdb::SCX | tdb::FSTP);
            stop(s, &t, SIGSTOP);
            t.dbg.clear(tdb::CHILD | tdb::SCX);
        } else {
            t.dbg.clear(tdb::STOPATFORK);
        }
    } else if p.is_traced() {
        t.dbg.set(tdb::SCX);
        if p.trace.lock().events & PTRACE_SCX != 0 || t.dbg.has(tdb::BORN) {
            stop(s, &t, SIGTRAP);
        }
        t.dbg.clear(tdb::SCX | tdb::BORN);
    }
}

/// Thread `t` (the current one) leaves its process, which goes on
/// (`kern_thr_exit`): with `PTRACE_LWP` it stops to say so.
pub(crate) fn thread_exit(s: &Sleepable, t: &UserThread) {
    t.dbg.set(tdb::EXIT);
    let p = t.process();
    if p.is_traced() && p.trace.lock().events & PTRACE_LWP != 0 {
        stop(s, t, SIGTRAP);
    }
}

/// `exec` succeeded in traced process `p`: with `PTRACE_EXEC` the
/// thread stops at the call's exit (`PL_FLAG_EXEC`).
pub(crate) fn exec_done(p: &Process, t: &UserThread) {
    if p.is_traced() && p.trace.lock().events & PTRACE_EXEC != 0 {
        t.dbg.set(tdb::EXEC);
    }
}

/// `p`'s debugger went away (`exit1`): no thread has a stop to report or
/// a hold, and none hands on a signal.
pub(crate) fn forget(p: &Process) {
    p.trace.lock().xthread = None;
    p.each_thread(|t| {
        t.dbg.clear(tdb::SUSPEND | tdb::XSIG | tdb::FSTP);
        t.dbg.state.lock().xsig = 0;
        true
    });
}

/// Lets stopped process `p` go on (`ptrace_unsuspend`).
fn unsuspend(p: &Process) {
    p.end_stop();
}

/// `proc_can_ptrace`: `me` may make a request of `p` now: it traces `p`,
/// which is stopped for it, every thread, and waited for.
fn can_ptrace(me: &Process, p: &Process) -> Result<(), Errno> {
    if p.has_exited() {
        return Err(Errno::ESRCH);
    }
    me.can_see(p)?;
    me.can_debug(p)?;
    if !p.is_traced() {
        return Err(Errno::EPERM);
    }
    if p.ppid_tree() != me.pid() {
        return Err(Errno::EBUSY);
    }
    if !p.trace_stopped_and_waited() {
        return Err(Errno::EBUSY);
    }
    Ok(())
}

/// The process (and thread) `pid` names: a pid, or past `PID_MAX` a
/// thread's id.
fn target(pid: i32) -> Result<(Arc<Process>, Option<Arc<UserThread>>), Errno> {
    if pid <= crate::proc::PID_MAX {
        let p = crate::proc::find(pid).ok_or(Errno::ESRCH)?;
        Ok((p, None))
    } else {
        let t = crate::uthread::find(pid).ok_or(Errno::ESRCH)?;
        Ok((Arc::clone(t.process()), Some(t)))
    }
}

/// `ptrace(req, pid, addr, data)` (`sys_ptrace`, `kern_ptrace`).
pub(crate) fn ptrace(
    s: &Sleepable,
    req: u64,
    pid: u64,
    addr: u64,
    data: u64,
) -> Result<u64, Errno> {
    let (req, pid, data) = (
        crate::syscall::int(req),
        crate::syscall::int(pid),
        crate::syscall::int(data),
    );
    if (PTINTERNAL_FIRST..=PTINTERNAL_LAST).contains(&req) {
        return Err(Errno::EINVAL);
    }
    let me = Process::current().ok_or(Errno::ESRCH)?;
    let (p, td2) = if req == PT_TRACE_ME {
        (Arc::clone(&me), None)
    } else {
        target(pid)?
    };
    if p.has_exited() {
        return Err(Errno::ESRCH);
    }
    me.can_see(&p)?;
    me.can_debug(&p)?;
    // The thread the request is about: the one named, else the one that
    // reported the stop, else the first.
    let td2 = match td2 {
        Some(t) => t,
        None => {
            let x = p.trace.lock().xthread;
            x.and_then(|tid| p.thread_by_tid(tid))
                .or_else(|| p.first_thread())
                .ok_or(Errno::ESRCH)?
        }
    };
    match req {
        PT_TRACE_ME => {
            if p.is_traced() {
                return Err(Errno::EBUSY);
            }
            if p.ppid_tree() == crate::proc::INIT {
                return Err(Errno::EPERM);
            }
        }
        PT_ATTACH => {
            if Arc::ptr_eq(&p, &me) {
                return Err(Errno::EINVAL);
            }
            if p.is_traced() {
                return Err(Errno::EBUSY);
            }
            // No tracing an ancestor while being traced.
            if me.is_traced() {
                let mut a = me.ppid_tree();
                for _ in 0..crate::proc::PID_MAX {
                    if a == p.pid() {
                        return Err(Errno::EINVAL);
                    }
                    match crate::proc::find(a) {
                        Some(q) if a > 1 => a = q.ppid_tree(),
                        _ => break,
                    }
                }
            }
        }
        PT_CLEARSTEP if UserThread::with_current(UserThread::tid) == Some(td2.tid()) => {}
        _ => {
            can_ptrace(&me, &p)?;
            // One request at a time (`P2_PTRACEREQ`).
            loop {
                {
                    let mut tr = p.trace.lock();
                    if !tr.busy {
                        tr.busy = true;
                        break;
                    }
                }
                crate::signal::sleep(s, p.trace_wait(), || !p.trace.lock().busy, None)?;
                can_ptrace(&me, &p)?;
            }
        }
    }
    // Every request holds the process for its length (`P2_PTRACEREQ`);
    // those that start tracing find none in progress.
    p.trace.lock().busy = true;
    let r = request(s, &me, &p, &td2, req, addr, data);
    {
        let mut tr = p.trace.lock();
        tr.busy = false;
    }
    p.trace_wait().wake_all();
    r
}

/// The request itself (`kern_ptrace`'s second switch).
fn request(
    s: &Sleepable,
    me: &Arc<Process>,
    p: &Arc<Process>,
    td2: &Arc<UserThread>,
    req: i32,
    addr: u64,
    data: i32,
) -> Result<u64, Errno> {
    match req {
        PT_TRACE_ME => {
            p.set_traced(p.ppid_tree(), false);
            if p.vforked() {
                p.trace.lock().pptrace = true;
            }
            Ok(0)
        }
        PT_ATTACH => {
            p.set_traced(p.ppid_tree(), true);
            crate::proc::reparent_traced(p, me.pid());
            // A stop from a signal ends first; the attach's comes next.
            p.end_stop();
            let _ = crate::signal::post(
                p,
                crate::signal::user_info(SIGSTOP, 0, abi::signal::code::SI_USER),
            );
            Ok(0)
        }
        PT_CLEARSTEP => {
            clear_step(td2);
            Ok(0)
        }
        PT_SETSTEP => {
            single_step(td2);
            Ok(0)
        }
        PT_SUSPEND => {
            td2.dbg.set(tdb::SUSPEND);
            td2.interrupt(crate::signal::AST_SUSPEND);
            Ok(0)
        }
        PT_RESUME => {
            td2.dbg.clear(tdb::SUSPEND);
            p.stop_wait.wake_all();
            Ok(0)
        }
        PT_FOLLOW_FORK | PT_LWP_EVENTS => {
            let bit = if req == PT_FOLLOW_FORK {
                PTRACE_FORK
            } else {
                PTRACE_LWP
            };
            let mut tr = p.trace.lock();
            if data != 0 {
                tr.events |= bit;
            } else {
                tr.events &= !bit;
            }
            Ok(0)
        }
        PT_GET_EVENT_MASK => {
            if usize::try_from(data).ok() != Some(size_of::<i32>()) {
                return Err(Errno::EINVAL);
            }
            let ev = p.trace.lock().events;
            UserPtr::<u32>::new(addr)
                .write(&ev)
                .map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        PT_SET_EVENT_MASK => {
            if usize::try_from(data).ok() != Some(size_of::<i32>()) {
                return Err(Errno::EINVAL);
            }
            let ev = UserPtr::<u32>::new(addr)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            let all =
                PTRACE_EXEC | PTRACE_SCE | PTRACE_SCX | PTRACE_FORK | PTRACE_LWP | PTRACE_VFORK;
            if ev & !all != 0 {
                return Err(Errno::EINVAL);
            }
            p.trace.lock().events = ev;
            Ok(0)
        }
        PT_GET_SC_ARGS => {
            let code = td2.syscall_code();
            if !td2.dbg.has(tdb::SCE | tdb::SCX) && code == 0 {
                return Err(Errno::EINVAL);
            }
            // As kept at a traced entry, else as the call's registers
            // still hold them (`td_sa`).
            let st = td2.dbg.state.lock();
            let kept = (st.code == code && td2.dbg.has(tdb::SCE | tdb::SCX)).then_some(st.args);
            drop(st);
            let n = crate::syscall::narg_of(code).min(8);
            let mut args = [0u64; 8];
            match kept {
                Some(a) => args[..n].copy_from_slice(&a[..n]),
                None => {
                    let all = with_frame(td2, |f| crate::syscall::args_of(f))?;
                    args[..n].copy_from_slice(&all[..n]);
                }
            }
            let len = usize::try_from(data)
                .unwrap_or(0)
                .min(size_of::<[u64; 8]>());
            uaccess::copy_out(addr, &uaccess::bytes_of(&args)[..len]).map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        PT_GET_SC_RET => {
            if !td2.dbg.has(tdb::SCX) {
                return Err(Errno::EINVAL);
            }
            let st = td2.dbg.state.lock();
            let mut r = PtraceScRet {
                sr_error: st.error,
                ..PtraceScRet::default()
            };
            if r.sr_error == 0 {
                r.sr_retval = st.retval;
            }
            drop(st);
            let len = usize::try_from(data)
                .unwrap_or(0)
                .min(size_of::<PtraceScRet>());
            uaccess::copy_out(addr, &uaccess::bytes_of(&r)[..len]).map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        PT_STEP | PT_CONTINUE | PT_TO_SCE | PT_TO_SCX | PT_SYSCALL | PT_DETACH => {
            let sig = u32::try_from(data)
                .ok()
                .filter(|&d| d <= SIG_MAXSIG)
                .ok_or(Errno::EINVAL)?;
            match req {
                PT_STEP => single_step(td2),
                PT_DETACH => detach(p),
                _ => {
                    if addr != 1 {
                        set_pc(td2, addr);
                        td2.dbg.set(tdb::USERWR);
                    }
                    let mut tr = p.trace.lock();
                    match req {
                        PT_TO_SCE => tr.events |= PTRACE_SCE,
                        PT_TO_SCX => tr.events |= PTRACE_SCX,
                        PT_SYSCALL => tr.events |= PTRACE_SYSCALL,
                        _ => {}
                    }
                }
            }
            send(p, sig);
            Ok(0)
        }
        PT_KILL => {
            send(p, SIGKILL);
            Ok(0)
        }
        PT_WRITE_I | PT_WRITE_D => {
            td2.dbg.set(tdb::USERWR);
            let word = data.to_ne_bytes();
            if rw_memory(s, p, addr, &mut word.clone(), true) != word.len() {
                return Err(Errno::ENOMEM);
            }
            Ok(0)
        }
        PT_READ_I | PT_READ_D => {
            let mut word = [0u8; 4];
            if rw_memory(s, p, addr, &mut word, false) != word.len() {
                return Err(Errno::ENOMEM);
            }
            Ok(u64::from(u32::from_ne_bytes(word)))
        }
        PT_IO => io(s, p, td2, addr),
        PT_GETREGS => {
            let r = read_regs(td2)?;
            UserPtr::<Reg>::new(addr)
                .write(&r)
                .map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        PT_SETREGS => {
            let r = UserPtr::<Reg>::new(addr)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            td2.dbg.set(tdb::USERWR);
            write_regs(td2, &r)?;
            Ok(0)
        }
        PT_GETFPREGS => {
            let r = read_fpregs(td2)?;
            UserPtr::<Fpreg>::new(addr)
                .write(&r)
                .map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        PT_SETFPREGS => {
            let r = UserPtr::<Fpreg>::new(addr)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            td2.dbg.set(tdb::USERWR);
            write_fpregs(td2, &r)?;
            Ok(0)
        }
        PT_GETDBREGS => {
            let r = read_dbregs();
            UserPtr::<Dbreg>::new(addr)
                .write(&r)
                .map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        PT_SETDBREGS => {
            let r = UserPtr::<Dbreg>::new(addr)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            td2.dbg.set(tdb::USERWR);
            write_dbregs(&r)?;
            Ok(0)
        }
        PT_GETREGSET | PT_SETREGSET => regset(td2, req == PT_SETREGSET, data, addr),
        PT_LWPINFO => lwpinfo(td2, addr, data),
        PT_GETNUMLWPS => Ok(p.thread_count() as u64),
        PT_GETLWPLIST => {
            if data <= 0 {
                return Err(Errno::EINVAL);
            }
            let tids: Vec<i32> = p
                .threads()
                .iter()
                .map(|t| t.tid())
                .take(usize::try_from(data).unwrap_or(0))
                .collect();
            for (i, tid) in tids.iter().enumerate() {
                UserPtr::<i32>::new(addr + 4 * i as u64)
                    .write(tid)
                    .map_err(|_| Errno::EFAULT)?;
            }
            Ok(tids.len() as u64)
        }
        PT_VM_TIMESTAMP => Ok(u64::from(p.space().timestamp())),
        PT_VM_ENTRY => vm_entry(s, p, addr),
        PT_SC_REMOTE => sc_remote(s, p, td2, addr, data),
        PT_COREDUMP => coredump(s, me, p, addr, data),
        _ if req >= PT_FIRSTMACH => machdep(td2, req, addr, data),
        _ => Err(Errno::EINVAL),
    }
}

/// The `sendsig` tail of the resuming requests: the reporting thread
/// gets `sig` to deliver (0 for none), and every thread goes on but those
/// `PT_SUSPEND` holds.
fn send(p: &Arc<Process>, sig: u32) {
    let x = p.trace.lock().xthread.take();
    if let Some(t) = x.and_then(|tid| p.thread_by_tid(tid)) {
        t.dbg.clear(tdb::XSIG);
        t.dbg.state.lock().xsig = sig;
    }
    if sig == SIGKILL {
        p.trace.lock().killed = true;
    }
    // A thread asleep whose registers the debugger changed wakes to take
    // the change (`pt_attach_transparent`).
    p.each_thread(|t| {
        if t.dbg.has(tdb::USERWR) && !t.dbg.has(tdb::SSWITCH) {
            t.dbg.clear(tdb::USERWR);
            t.interrupt(crate::signal::AST_SIGNAL);
        }
        true
    });
    unsuspend(p);
}

/// `PT_DETACH`: `p` goes back to its real parent (or its reaper), its
/// events and pending attach stops forgotten.
fn detach(p: &Arc<Process>) {
    let oppid = p.trace.lock().oppid;
    p.clear_traced();
    if oppid != p.ppid_tree() {
        crate::proc::reparent_back(p);
    }
    {
        let mut tr = p.trace.lock();
        tr.events = 0;
        tr.first_stop = false;
    }
    p.each_thread(|t| {
        if t.dbg.has(tdb::FSTP) {
            crate::signal::discard_thread(t, SIGSTOP);
        }
        t.dbg
            .clear(tdb::XSIG | tdb::FSTP | tdb::SUSPEND | tdb::BORN);
        true
    });
    crate::signal::discard_process(p, SIGSTOP);
    // The parent hears it go on (`childproc_continued`): it may have
    // stopped it before the debugger came.
    p.report_continued();
}

/// `PT_LWPINFO`: what thread `t` stopped for.
fn lwpinfo(t: &UserThread, addr: u64, data: i32) -> Result<u64, Errno> {
    let len = usize::try_from(data)
        .ok()
        .filter(|&d| d > 0 && d <= size_of::<PtraceLwpinfo>())
        .ok_or(Errno::EINVAL)?;
    let mut pl = PtraceLwpinfo {
        pl_lwpid: t.tid(),
        pl_event: PL_EVENT_NONE,
        ..PtraceLwpinfo::default()
    };
    let f = |b| t.dbg.has(b);
    let st = t.dbg.state.lock();
    if f(tdb::XSIG) {
        pl.pl_event = PL_EVENT_SIGNAL;
        let si_end = core::mem::offset_of!(PtraceLwpinfo, pl_siginfo) + size_of::<Siginfo>();
        if st.si.si_signo != 0 && len >= si_end {
            pl.pl_flags |= PL_FLAG_SI;
            pl.pl_siginfo = st.si;
        }
    }
    if f(tdb::SCE) {
        pl.pl_flags |= PL_FLAG_SCE;
    } else if f(tdb::SCX) {
        pl.pl_flags |= PL_FLAG_SCX;
    }
    if f(tdb::EXEC) {
        pl.pl_flags |= PL_FLAG_EXEC;
    }
    if f(tdb::FORK) {
        pl.pl_flags |= PL_FLAG_FORKED;
        pl.pl_child_pid = st.forked;
        if f(tdb::VFORK) {
            pl.pl_flags |= PL_FLAG_VFORKED;
        }
    } else if f(tdb::VFORK) && !f(tdb::SCX) {
        pl.pl_flags |= PL_FLAG_VFORK_DONE;
    }
    if f(tdb::CHILD) {
        pl.pl_flags |= PL_FLAG_CHILD;
    }
    if f(tdb::BORN) {
        pl.pl_flags |= PL_FLAG_BORN;
    }
    if f(tdb::EXIT) {
        pl.pl_flags |= PL_FLAG_EXITED;
    }
    drop(st);
    let code = t.syscall_code();
    if code != 0 {
        pl.pl_syscall_code = code;
        pl.pl_syscall_narg = u32::try_from(crate::syscall::narg_of(code)).unwrap_or(0);
    }
    {
        let ts = t.sig.lock();
        pl.pl_sigmask = ts.mask;
        pl.pl_siglist = ts.queue.pending();
    }
    let name = t.name();
    let name = name.as_bytes();
    pl.pl_tdname[..name.len()].copy_from_slice(name);
    uaccess::copy_out(addr, &uaccess::bytes_of(&pl)[..len]).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `proc_rwmem`: reads `buf.len()` bytes of `p`'s memory at `va` into
/// `buf`, or writes them there (giving a private mapping its own copy
/// first, as `VM_PROT_COPY` does, so that breakpoints go into text).
/// Returns how many moved before a page that could not be had.
fn rw_memory(s: &Sleepable, p: &Process, va: u64, buf: &mut [u8], write: bool) -> usize {
    p.space().debug_rw(s, va, buf, write)
}

/// `PT_IO`: a transfer between the debugger's buffer and `p`'s memory;
/// `piod_len` comes back as how much moved.
fn io(s: &Sleepable, p: &Process, td2: &UserThread, addr: u64) -> Result<u64, Errno> {
    let mut d = UserPtr::<PtraceIoDesc>::new(addr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if d.piod_len > i64::MAX as u64 {
        return Err(Errno::EINVAL);
    }
    let write = match d.piod_op {
        PIOD_READ_D | PIOD_READ_I => false,
        PIOD_WRITE_D | PIOD_WRITE_I => {
            td2.dbg.set(tdb::USERWR);
            true
        }
        _ => return Err(Errno::EINVAL),
    };
    let total = usize::try_from(d.piod_len).map_err(|_| Errno::EINVAL)?;
    let mut done = 0;
    let mut chunk = alloc::vec![0u8; total.min(64 * 1024)];
    let mut error = Ok(());
    while done < total {
        let n = (total - done).min(chunk.len());
        let buf = &mut chunk[..n];
        let at = d.piod_offs + done as u64;
        let user = d.piod_addr + done as u64;
        let moved = if write {
            if uaccess::copy_in(buf, user).is_err() {
                error = Err(Errno::EFAULT);
                break;
            }
            rw_memory(s, p, at, buf, true)
        } else {
            let m = rw_memory(s, p, at, buf, false);
            if uaccess::copy_out(user, &buf[..m]).is_err() {
                error = Err(Errno::EFAULT);
                break;
            }
            m
        };
        done += moved;
        if moved < n {
            if done == 0 {
                error = Err(Errno::EFAULT);
            }
            break;
        }
    }
    d.piod_len = done as u64;
    error?;
    UserPtr::<PtraceIoDesc>::new(addr)
        .write(&d)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `PT_VM_ENTRY`: the mapping at or after the one `pve_entry` names
/// (0 for the first), with its file's path if it maps one.
fn vm_entry(s: &Sleepable, p: &Process, addr: u64) -> Result<u64, Errno> {
    let mut pve = UserPtr::<PtraceVmEntry>::new(addr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let space = p.space();
    if pve.pve_entry != 0 && pve.pve_timestamp != space.timestamp().cast_signed() {
        return Err(Errno::EINVAL);
    }
    let want = usize::try_from(pve.pve_entry).map_err(|_| Errno::EINVAL)?;
    let e = crate::kinfo::vm::entry(s, p, want).ok_or(Errno::ENOENT)?;
    pve.pve_entry = pve.pve_entry.saturating_add(1);
    pve.pve_timestamp = space.timestamp().cast_signed();
    pve.pve_start = e.kve_start;
    pve.pve_end = e.kve_end - 1;
    pve.pve_offset = e.kve_offset;
    pve.pve_prot = e.kve_protection.cast_unsigned();
    let path_len = e.kve_path.iter().position(|&b| b == 0).unwrap_or(0);
    let is_vnode = path_len > 0;
    pve.pve_fileid = if is_vnode {
        e.kve_vn_fileid.cast_signed()
    } else {
        0
    };
    pve.pve_fsid = if is_vnode {
        u32::try_from(e.kve_type_spec & 0xffff_ffff).unwrap_or(u32::MAX)
    } else {
        u32::MAX
    };
    let room = pve.pve_pathlen as usize;
    if pve.pve_path != 0 && room > 0 && is_vnode {
        if path_len + 1 > room {
            pve.pve_pathlen = u32::try_from(path_len + 1).unwrap_or(u32::MAX);
            let _ = UserPtr::<PtraceVmEntry>::new(addr).write(&pve);
            return Err(Errno::ENAMETOOLONG);
        }
        uaccess::copy_out(pve.pve_path, &e.kve_path[..=path_len]).map_err(|_| Errno::EFAULT)?;
        pve.pve_pathlen = u32::try_from(path_len + 1).unwrap_or(0);
    } else {
        pve.pve_pathlen = 0;
    }
    UserPtr::<PtraceVmEntry>::new(addr)
        .write(&pve)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `PT_SC_REMOTE`: thread `td2`, stopped at the system call boundary,
/// makes system call `pscr_syscall` with `pscr_args` for the debugger
/// (`ptrace_syscallreq`), without going on.
fn sc_remote(
    s: &Sleepable,
    p: &Arc<Process>,
    td2: &UserThread,
    addr: u64,
    data: i32,
) -> Result<u64, Errno> {
    if usize::try_from(data).ok() != Some(size_of::<PtraceScRemote>()) {
        return Err(Errno::EINVAL);
    }
    let mut r = UserPtr::<PtraceScRemote>::new(addr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if r.pscr_nargs > 8 {
        return Err(Errno::EINVAL);
    }
    let mut args = [0u64; 8];
    for (i, a) in args.iter_mut().take(r.pscr_nargs as usize).enumerate() {
        *a = UserPtr::<u64>::new(r.pscr_args + 8 * i as u64)
            .read()
            .map_err(|_| Errno::EFAULT)?;
    }
    if !td2.dbg.has(tdb::BOUNDARY) {
        return Err(Errno::EBUSY);
    }
    td2.dbg.state.lock().remote = Some(RemoteCall {
        code: r.pscr_syscall,
        args,
        nargs: r.pscr_nargs,
        ret: PtraceScRet::default(),
    });
    td2.dbg.set(tdb::SCREMOTEREQ);
    p.stop_wait.wake_all();
    // The thread runs it and clears the request.
    crate::signal::sleep(s, p.trace_wait(), || !td2.dbg.has(tdb::SCREMOTEREQ), None)
        .map(|_| ())
        .or_else(|e| {
            // Not interruptible in FreeBSD (`PPAUSE`): wait it out.
            let _ = e;
            p.trace_wait()
                .wait_until(s, || !td2.dbg.has(tdb::SCREMOTEREQ));
            Ok::<(), Errno>(())
        })?;
    let ret = td2
        .dbg
        .state
        .lock()
        .remote
        .take()
        .map(|c| c.ret)
        .unwrap_or_default();
    r.pscr_ret = ret;
    let off = core::mem::offset_of!(PtraceScRemote, pscr_ret);
    uaccess::copy_out(addr + off as u64, uaccess::bytes_of(&r.pscr_ret))
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `PT_COREDUMP`: a thread stopped in `ptracestop` writes the process's
/// core into the debugger's descriptor `pc_fd` (a regular file open for
/// writing, `EPIPE` if not), within `pc_limit` (0 for none).
fn coredump(
    s: &Sleepable,
    me: &Arc<Process>,
    p: &Arc<Process>,
    addr: u64,
    data: i32,
) -> Result<u64, Errno> {
    if usize::try_from(data).ok() != Some(size_of::<PtraceCoredump>()) {
        return Err(Errno::EINVAL);
    }
    let pc = UserPtr::<PtraceCoredump>::new(addr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if pc.pc_flags & !(PC_COMPRESS | PC_ALL) != 0 {
        return Err(Errno::EINVAL);
    }
    let file = crate::syscall::file_of(s, me, pc.pc_fd, &crate::capsicum::rights::WRITE)?;
    if file.flags() & abi::fcntl::FWRITE == 0 {
        return Err(Errno::EBADF);
    }
    if !matches!(file.kind(), crate::fs::file::FileKind::Vnode(_))
        || file.vnode().is_none_or(|v| v.kind() != vfs::Kind::Regular)
    {
        return Err(Errno::EPIPE);
    }
    can_ptrace(me, p)?;
    // The thread that stopped in `ptracestop` (`TDB_SSWITCH`).
    let mut td2 = None;
    p.each_thread(|t| {
        if t.dbg.has(tdb::SSWITCH) {
            td2 = Some(Arc::clone(t));
            return false;
        }
        true
    });
    let td2 = td2.ok_or(Errno::EBUSY)?;
    td2.dbg.state.lock().coredump = Some(CoreRequest {
        file,
        limit: u64::try_from(pc.pc_limit)
            .ok()
            .filter(|&l| l != 0)
            .unwrap_or(u64::MAX),
        all: pc.pc_flags & PC_ALL != 0,
        done: None,
    });
    td2.dbg.set(tdb::COREDUMPREQ);
    p.stop_wait.wake_all();
    // Not interruptible in FreeBSD (`PPAUSE`).
    p.trace_wait()
        .wait_until(s, || !td2.dbg.has(tdb::COREDUMPREQ));
    let r = td2
        .dbg
        .state
        .lock()
        .coredump
        .take()
        .and_then(|c| c.done)
        .unwrap_or(Err(Errno::EIO));
    r.map(|()| 0)
}

/// The stopped thread `t` serves its debugger's request
/// (`ptrace_remotereq`), then stops on.
fn remote_request(s: &Sleepable, t: &UserThread) {
    if t.dbg.has(tdb::SCREMOTEREQ) {
        let call = t.dbg.state.lock().remote;
        if let Some(mut call) = call {
            call.ret = crate::syscall::remote(s, call.code, &call.args[..call.nargs as usize]);
            t.dbg.state.lock().remote = Some(call);
        }
        t.dbg.clear(tdb::SCREMOTEREQ);
    }
    if t.dbg.has(tdb::COREDUMPREQ) {
        let req = t.dbg.state.lock().coredump.take();
        if let Some(mut req) = req {
            let r = match req.file.vnode() {
                Some(v) => crate::coredump::dump_for_debugger(s, t, v, req.limit, req.all),
                None => Err(Errno::EPIPE),
            };
            req.done = Some(r);
            t.dbg.state.lock().coredump = Some(req);
        }
        t.dbg.clear(tdb::COREDUMPREQ);
    }
    // Stopped again, it is reported again (`thread_stopped`).
    t.process().rearm_stop();
    t.process().trace_wait().wake_all();
}

/// Runs `f` on the frame thread `t` entered the kernel with, to read or
/// change while it is stopped; `EBUSY` if it has none.
fn with_frame<R>(t: &UserThread, f: impl FnOnce(&mut TrapFrame) -> R) -> Result<R, Errno> {
    let p = t.dbg.frame.load(Ordering::Acquire);
    if p.is_null() {
        return Err(Errno::EBUSY);
    }
    // SAFETY: `t` set it as it entered the kernel from user mode, on its
    // kernel stack, which holds the frame until it returns there; it is
    // stopped (the request checked), so nothing else touches the frame
    // until the debugger lets it go, and the borrow ends with `f`.
    Ok(f(unsafe { &mut *p }))
}

/// `ptrace_set_pc`.
fn set_pc(t: &UserThread, pc: u64) {
    if with_frame(t, |f| machdep::set_pc(f, pc)).is_ok() {
        t.interrupt(crate::signal::AST_FULL_RESTORE);
    }
}

/// `ptrace_single_step`.
fn single_step(t: &UserThread) {
    if with_frame(t, |f| machdep::set_step(f, true)) == Ok(true) {
        t.dbg.set(tdb::STEP);
        t.interrupt(crate::signal::AST_FULL_RESTORE);
    }
}

/// `ptrace_clear_single_step`.
fn clear_step(t: &UserThread) {
    let _ = with_frame(t, |f| machdep::set_step(f, false));
    t.dbg.clear(tdb::STEP);
}

/// A step trap came to thread `t` (the current one): a debugger's step
/// ends there (`TDB_STEP`).
pub(crate) fn step_done(t: &UserThread, frame: &mut TrapFrame) {
    if t.dbg.has(tdb::STEP) {
        machdep::set_step(frame, false);
        t.dbg.clear(tdb::STEP);
    }
}

/// `fill_regs`.
pub(crate) fn read_regs(t: &UserThread) -> Result<Reg, Errno> {
    with_frame(t, |f| machdep::fill_regs(f))
}

/// `set_regs`: `EINVAL` for flags or segments user mode may not have.
fn write_regs(t: &UserThread, r: &Reg) -> Result<(), Errno> {
    with_frame(t, |f| machdep::set_regs(f, r))??;
    t.interrupt(crate::signal::AST_FULL_RESTORE);
    Ok(())
}

/// `fill_fpregs`: from the thread's saved FPU state.
pub(crate) fn read_fpregs(t: &UserThread) -> Result<Fpreg, Errno> {
    let st = t.sched().ok_or(Errno::EBUSY)?;
    let (ptr, len) = st.fpu_area().ok_or(Errno::EINVAL)?;
    // SAFETY: `t` is stopped for its debugger (the request checked), so
    // off CPU, its state saved in those `len` bytes; it stays stopped
    // while this reads, and nothing else uses the area.
    let area = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
    Ok(machdep::fill_fpregs(area))
}

/// `set_fpregs`.
fn write_fpregs(t: &UserThread, r: &Fpreg) -> Result<(), Errno> {
    let st = t.sched().ok_or(Errno::EBUSY)?;
    let (ptr, len) = st.fpu_area().ok_or(Errno::EINVAL)?;
    // SAFETY: as in `read_fpregs`; the thread loads the area as it next
    // runs.
    let area = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
    machdep::set_fpregs(area, r);
    Ok(())
}

/// Thread `t`'s user thread register `which` (0: amd64's FS base or
/// arm64's TPIDR_EL0; 1: amd64's GS base), as it stopped.
fn user_tls(t: &UserThread, which: usize) -> u64 {
    // SAFETY: `t` is stopped for its debugger, so off CPU.
    t.sched().map_or(0, |st| unsafe { st.user_tls() }[which])
}

/// `fill_dbregs`: no hardware breakpoints or watchpoints are given out.
fn read_dbregs() -> Dbreg {
    machdep::fill_dbregs()
}

/// `set_dbregs`: only the empty set.
fn write_dbregs(r: &Dbreg) -> Result<(), Errno> {
    if *r == machdep::fill_dbregs() {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

/// `PT_GETREGSET` and `PT_SETREGSET`: register set `note` through the
/// `struct iovec` at `addr` (a null base asks the size).
fn regset(t: &UserThread, set: bool, note: i32, addr: u64) -> Result<u64, Errno> {
    let mut iov = UserPtr::<abi::uio::Iovec>::new(addr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let bytes = machdep::regset_get(t, note)?;
    if set {
        if usize::try_from(iov.iov_len).ok() != Some(bytes.len()) {
            return Err(Errno::EINVAL);
        }
        let mut b = alloc::vec![0u8; bytes.len()];
        uaccess::copy_in(&mut b, iov.iov_base).map_err(|_| Errno::EFAULT)?;
        t.dbg.set(tdb::USERWR);
        return machdep::regset_set(t, note, &b).map(|()| 0);
    }
    if iov.iov_base == 0 {
        iov.iov_len = bytes.len() as u64;
    } else {
        if usize::try_from(iov.iov_len).unwrap_or(usize::MAX) < bytes.len() {
            return Err(Errno::EINVAL);
        }
        uaccess::copy_out(iov.iov_base, &bytes).map_err(|_| Errno::EFAULT)?;
        iov.iov_len = bytes.len() as u64;
    }
    UserPtr::<abi::uio::Iovec>::new(addr)
        .write(&iov)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `cpu_ptrace`: each architecture's own requests.
fn machdep(t: &UserThread, req: i32, addr: u64, data: i32) -> Result<u64, Errno> {
    machdep::request(t, req, addr, data)
}

/// `struct prstatus` of thread `t` (`NT_PRSTATUS`, also for core dumps).
pub(crate) fn prstatus(t: &UserThread, cursig: i32) -> Result<Vec<u8>, Errno> {
    let st = Prstatus {
        pr_version: PRSTATUS_VERSION,
        pr_statussz: size_of::<Prstatus<Reg>>() as u64,
        pr_gregsetsz: size_of::<Reg>() as u64,
        pr_fpregsetsz: size_of::<Fpreg>() as u64,
        pr_osreldate: abi::sysctl::identity::OSRELDATE,
        pr_cursig: cursig,
        pr_pid: t.tid(),
        pr_reg: read_regs(t)?,
        ..Prstatus::default()
    };
    Ok(crate::uaccess::bytes_of(&st).to_vec())
}

#[cfg(target_arch = "x86_64")]
mod machdep {
    use alloc::vec::Vec;

    use abi::errno::Errno;
    use abi::ptrace::*;

    use crate::arch::trap::TrapFrame;
    use crate::uthread::UserThread;

    /// `PSL_T`: trap after each instruction.
    const PSL_T: u64 = 0x100;
    /// The flags user mode may change (`PSL_USERCHANGE`: C, PF, AF, Z, N,
    /// T, D, V, NT, RF, AC, ID).
    const PSL_USERCHANGE: u64 = 0x0025_4dd5;
    /// User mode's code and stack selectors (`CS_SECURE`'s and the stack
    /// segment's).
    const UCODE: i64 = crate::arch::user::USER_CS as i64;
    const UDATA: i64 = crate::arch::user::USER_DS as i64;

    pub(super) fn set_pc(f: &mut TrapFrame, pc: u64) {
        f.rip = pc;
    }

    /// Turns `PSL_T` on or off; whether it was off before turning on.
    pub(super) fn set_step(f: &mut TrapFrame, on: bool) -> bool {
        let was = f.rflags & PSL_T != 0;
        if on {
            f.rflags |= PSL_T;
        } else {
            f.rflags &= !PSL_T;
        }
        on && !was
    }

    pub(super) fn fill_regs(f: &TrapFrame) -> Reg {
        let w = |v: u64| v.cast_signed();
        Reg {
            r_r15: w(f.r15),
            r_r14: w(f.r14),
            r_r13: w(f.r13),
            r_r12: w(f.r12),
            r_r11: w(f.r11),
            r_r10: w(f.r10),
            r_r9: w(f.r9),
            r_r8: w(f.r8),
            r_rdi: w(f.rdi),
            r_rsi: w(f.rsi),
            r_rbp: w(f.rbp),
            r_rbx: w(f.rbx),
            r_rdx: w(f.rdx),
            r_rcx: w(f.rcx),
            r_rax: w(f.rax),
            r_rip: w(f.rip),
            r_cs: w(f.cs),
            r_rflags: w(f.rflags),
            r_rsp: w(f.rsp),
            r_ss: w(f.ss),
            ..Reg::default()
        }
    }

    /// `set_regs`: the flags must keep what user mode may not change
    /// (`EFL_SECURE`), the code selector be user mode's (`CS_SECURE`).
    pub(super) fn set_regs(f: &mut TrapFrame, r: &Reg) -> Result<(), Errno> {
        let rflags = r.r_rflags.cast_unsigned() & 0xffff_ffff;
        if (rflags ^ f.rflags) & !PSL_USERCHANGE != 0 || r.r_cs != UCODE {
            return Err(Errno::EINVAL);
        }
        let u = |v: i64| v.cast_unsigned();
        f.r15 = u(r.r_r15);
        f.r14 = u(r.r_r14);
        f.r13 = u(r.r_r13);
        f.r12 = u(r.r_r12);
        f.r11 = u(r.r_r11);
        f.r10 = u(r.r_r10);
        f.r9 = u(r.r_r9);
        f.r8 = u(r.r_r8);
        f.rdi = u(r.r_rdi);
        f.rsi = u(r.r_rsi);
        f.rbp = u(r.r_rbp);
        f.rbx = u(r.r_rbx);
        f.rdx = u(r.r_rdx);
        f.rcx = u(r.r_rcx);
        f.rax = u(r.r_rax);
        f.rip = u(r.r_rip);
        f.cs = u(r.r_cs);
        f.rflags = rflags;
        f.rsp = u(r.r_rsp);
        f.ss = if r.r_ss == UDATA { u(r.r_ss) } else { f.ss };
        Ok(())
    }

    /// The FXSAVE part of the thread's XSAVE area.
    pub(super) fn fill_fpregs(area: &mut [u8]) -> Fpreg {
        let mut r = Fpreg::default();
        let n = size_of::<Fpreg>().min(area.len());
        crate::uaccess::bytes_of_mut(&mut r)[..n].copy_from_slice(&area[..n]);
        r
    }

    pub(super) fn set_fpregs(area: &mut [u8], r: &Fpreg) {
        let n = size_of::<Fpreg>().min(area.len());
        area[..n].copy_from_slice(&crate::uaccess::bytes_of(r)[..n]);
        // The legacy region is now valid in the XSAVE header's XSTATE_BV.
        if area.len() >= 520 {
            area[512] |= 0x3;
        }
    }

    pub(super) fn fill_dbregs() -> Dbreg {
        Dbreg::default()
    }

    pub(super) fn regset_get(t: &UserThread, note: i32) -> Result<Vec<u8>, Errno> {
        match note {
            NT_PRSTATUS => super::prstatus(t, 0),
            NT_FPREGSET => Ok(crate::uaccess::bytes_of(&super::read_fpregs(t)?).to_vec()),
            _ => Err(Errno::EINVAL),
        }
    }

    pub(super) fn regset_set(t: &UserThread, note: i32, b: &[u8]) -> Result<(), Errno> {
        match note {
            NT_PRSTATUS => {
                let off = core::mem::offset_of!(Prstatus<Reg>, pr_reg);
                let mut r = Reg::default();
                crate::uaccess::bytes_of_mut(&mut r)
                    .copy_from_slice(&b[off..off + size_of::<Reg>()]);
                super::write_regs(t, &r)
            }
            NT_FPREGSET => {
                let mut r = Fpreg::default();
                crate::uaccess::bytes_of_mut(&mut r).copy_from_slice(b);
                super::write_fpregs(t, &r)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    /// amd64's requests: the FS and GS bases, the XSAVE state.
    pub(super) fn request(t: &UserThread, req: i32, addr: u64, data: i32) -> Result<u64, Errno> {
        use abi::ptrace::amd64::*;
        let _ = data;
        match req {
            PT_GETFSBASE | PT_GETGSBASE | PT_GETTLSBASE => {
                let v = match req {
                    PT_GETFSBASE => super::user_tls(t, 0),
                    PT_GETGSBASE => super::user_tls(t, 1),
                    _ => t.tls_base().ok_or(Errno::ESRCH)?,
                };
                crate::uaccess::UserPtr::<u64>::new(addr)
                    .write(&v)
                    .map_err(|_| Errno::EFAULT)?;
                Ok(0)
            }
            PT_GETXMMREGS => {
                let r = super::read_fpregs(t)?;
                crate::uaccess::copy_out(addr, crate::uaccess::bytes_of(&r))
                    .map_err(|_| Errno::EFAULT)?;
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod machdep {
    use alloc::vec::Vec;

    use abi::errno::Errno;
    use abi::ptrace::*;

    use crate::arch::trap::TrapFrame;
    use crate::uthread::UserThread;

    /// `PSR_SS`: step one instruction.
    const PSR_SS: u64 = 1 << 21;
    /// What `set_regs` takes of the flags (`PSR_SETTABLE_64`): NZCV,
    /// DIT, SSBS... and the step bit.
    const PSR_SETTABLE_64: u64 = 0xf000_0000 | (1 << 24) | (1 << 12) | PSR_SS;
    /// `NT_ARM_ADDR_MASK`: the top byte is ignored (`TBI_ADDR_MASK`).
    const NT_ARM_ADDR_MASK: i32 = 0x406;
    const TBI_ADDR_MASK: u64 = 0xff00_0000_0000_0000;

    pub(super) fn set_pc(f: &mut TrapFrame, pc: u64) {
        f.elr = pc;
    }

    pub(super) fn set_step(f: &mut TrapFrame, on: bool) -> bool {
        let was = f.spsr & PSR_SS != 0;
        if on {
            f.spsr |= PSR_SS;
        } else {
            f.spsr &= !PSR_SS;
        }
        on && !was
    }

    pub(super) fn fill_regs(f: &TrapFrame) -> Reg {
        let mut r = Reg::default();
        r.x.copy_from_slice(&f.x[..30]);
        r.lr = f.x[30];
        r.sp = f.sp;
        r.elr = f.elr;
        r.spsr = f.spsr;
        r
    }

    pub(super) fn set_regs(f: &mut TrapFrame, r: &Reg) -> Result<(), Errno> {
        f.x[..30].copy_from_slice(&r.x);
        f.x[30] = r.lr;
        f.sp = r.sp;
        f.elr = r.elr;
        f.spsr = (f.spsr & !PSR_SETTABLE_64) | (r.spsr & PSR_SETTABLE_64);
        Ok(())
    }

    /// The Q registers and FPSR/FPCR from the thread's saved state (the
    /// HAL's layout: 32 Q registers, then FPSR and FPCR).
    pub(super) fn fill_fpregs(area: &mut [u8]) -> Fpreg {
        let mut r = Fpreg::default();
        for (i, q) in r.fp_q.iter_mut().enumerate() {
            if let Some(b) = area.get(16 * i..16 * i + 16) {
                *q = u128::from_ne_bytes(b.try_into().unwrap_or([0; 16]));
            }
        }
        let word = |at: usize| {
            area.get(at..at + 4)
                .map_or(0, |b| u32::from_ne_bytes(b.try_into().unwrap_or([0; 4])))
        };
        r.fp_sr = word(512);
        r.fp_cr = word(520);
        r
    }

    pub(super) fn set_fpregs(area: &mut [u8], r: &Fpreg) {
        for (i, q) in r.fp_q.iter().enumerate() {
            if let Some(b) = area.get_mut(16 * i..16 * i + 16) {
                b.copy_from_slice(&q.to_ne_bytes());
            }
        }
        if let Some(b) = area.get_mut(512..516) {
            b.copy_from_slice(&r.fp_sr.to_ne_bytes());
        }
        if let Some(b) = area.get_mut(520..524) {
            b.copy_from_slice(&r.fp_cr.to_ne_bytes());
        }
    }

    pub(super) fn fill_dbregs() -> Dbreg {
        Dbreg {
            db_debug_ver: 0,
            ..Dbreg::default()
        }
    }

    pub(super) fn regset_get(t: &UserThread, note: i32) -> Result<Vec<u8>, Errno> {
        match note {
            NT_PRSTATUS => super::prstatus(t, 0),
            NT_FPREGSET | NT_ARM_VFP => {
                Ok(crate::uaccess::bytes_of(&super::read_fpregs(t)?).to_vec())
            }
            NT_ARM_TLS => Ok(super::user_tls(t, 0).to_ne_bytes().to_vec()),
            NT_ARM_ADDR_MASK => {
                let mut b = TBI_ADDR_MASK.to_ne_bytes().to_vec();
                b.extend_from_slice(&TBI_ADDR_MASK.to_ne_bytes());
                Ok(b)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    pub(super) fn regset_set(t: &UserThread, note: i32, b: &[u8]) -> Result<(), Errno> {
        match note {
            NT_PRSTATUS => {
                let off = core::mem::offset_of!(Prstatus<Reg>, pr_reg);
                let mut r = Reg::default();
                crate::uaccess::bytes_of_mut(&mut r)
                    .copy_from_slice(&b[off..off + size_of::<Reg>()]);
                super::write_regs(t, &r)
            }
            NT_FPREGSET | NT_ARM_VFP => {
                let mut r = Fpreg::default();
                crate::uaccess::bytes_of_mut(&mut r).copy_from_slice(b);
                super::write_fpregs(t, &r)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    pub(super) fn request(
        _t: &UserThread,
        _req: i32,
        _addr: u64,
        _data: i32,
    ) -> Result<u64, Errno> {
        Err(Errno::EINVAL)
    }
}

/// Thread `t`'s `struct ptrace_lwpinfo` in a core (`get_lwpinfo`): its
/// name, masks, and the signal it took.
pub(crate) fn core_lwpinfo(t: &UserThread) -> PtraceLwpinfo {
    let mut pl = PtraceLwpinfo {
        pl_lwpid: t.tid(),
        pl_event: PL_EVENT_NONE,
        ..PtraceLwpinfo::default()
    };
    {
        let ts = t.sig.lock();
        pl.pl_sigmask = ts.mask;
        pl.pl_siglist = ts.queue.pending();
    }
    let si = t.dbg.state.lock().si;
    if si.si_signo != 0 {
        pl.pl_event = PL_EVENT_SIGNAL;
        pl.pl_flags |= PL_FLAG_SI;
        pl.pl_siginfo = si;
    }
    let name = t.name();
    let name = name.as_bytes();
    pl.pl_tdname[..name.len()].copy_from_slice(name);
    pl
}

/// Thread `t`'s user thread registers as it stopped: amd64's FS and GS
/// bases, arm64's TPIDR_EL0 (and 0).
pub(crate) fn user_bases(t: &UserThread) -> (u64, u64) {
    (user_tls(t, 0), user_tls(t, 1))
}

/// A copy of thread `t`'s saved XSAVE area, as it stopped.
#[cfg(target_arch = "x86_64")]
pub(crate) fn xsave_area(t: &UserThread) -> Option<Vec<u8>> {
    let (ptr, len) = t.sched()?.fpu_area()?;
    // SAFETY: `t` is stopped (at the boundary, for a core dump), so off
    // CPU, its state saved in those `len` bytes; nothing changes it while
    // this copies.
    Some(unsafe { core::slice::from_raw_parts(ptr, len) }.to_vec())
}

/// The tracer of `p` (its debugger's pid), 0 if none (`ki_tracer`).
pub(crate) fn tracer(p: &Process) -> i32 {
    if p.is_traced() { p.ppid_tree() } else { 0 }
}

/// Whether the current process is traced (for the syscall path).
pub(crate) fn current_traced() -> bool {
    traced_now()
}
