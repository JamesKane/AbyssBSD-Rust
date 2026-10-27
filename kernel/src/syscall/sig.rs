// SPDX-License-Identifier: BSD-2-Clause
//! Signal system calls: dispositions and masks, sending (`kill`,
//! `thr_kill`, `sigqueue`), waiting (`sigsuspend`, the `sigwait` family),
//! the alternate stack, user contexts (`sigreturn`, `getcontext`,
//! `setcontext`, `swapcontext`), and `sigfastblock`. Delivery itself is in
//! [`crate::signal`].
//!
//! Behavior reference: sys/kern/kern_sig.c, sys/kern/kern_context.c,
//! sys/kern/kern_thr.c.

use abi::errno::Errno;
use abi::signal::code::{SI_LWP, SI_QUEUE, SI_USER};
use abi::signal::*;
use abi::thr::{
    SIGFASTBLOCK_PEND, SIGFASTBLOCK_SETPTR, SIGFASTBLOCK_UNBLOCK, SIGFASTBLOCK_UNSETPTR,
};
use abi::time::Timespec;
use abi::ucontext::{Mcontext, UCF_SWAPPED, Ucontext};
use sched::{Sleepable, WaitQueue};

use super::{SysResult, int, process, thread};
use crate::arch::trap::TrapFrame;
use crate::proc::Process;
use crate::signal::{self as signal, AST_FULL_RESTORE, AST_SIGNAL, post, user_info};
use crate::time::Wait;
use crate::uaccess::{Plain, UserPtr};
use crate::uthread::UserThread;

// SAFETY: integers only, no padding (`abi-check` holds the C layout).
unsafe impl Plain for Sigset {}
// SAFETY: as above; the C struct's tail padding is an explicit field.
unsafe impl Plain for Sigaction {}

fn signal_number(sig: u64) -> Result<u32, Errno> {
    u32::try_from(sig)
        .ok()
        .filter(|&s| signal::valid(s))
        .ok_or(Errno::EINVAL)
}

fn read_set(set: u64) -> Result<Sigset, Errno> {
    UserPtr::<Sigset>::new(set)
        .read()
        .map_err(|_| Errno::EFAULT)
}

/// `sigaction(sig, act, oact)`.
pub(super) fn sigaction(sig: u64, act: u64, oact: u64) -> SysResult {
    let sig = signal_number(sig)?;
    let new = if act == 0 {
        None
    } else {
        Some(
            UserPtr::<Sigaction>::new(act)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    if let Some(n) = &new
        && (sig == SIGKILL || sig == SIGSTOP)
        && n.sa_handler != SIG_DFL
    {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let old = match new {
        Some(mut n) => {
            n._pad = 0;
            signal::set_action(&p, sig, n)
        }
        None => p.signals.lock().action(sig),
    };
    if oact != 0 {
        UserPtr::<Sigaction>::new(oact)
            .write(&old)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `sigprocmask(how, set, oset)`.
pub(super) fn sigprocmask(how: u64, set: u64, oset: u64) -> SysResult {
    let new = if set == 0 { None } else { Some(read_set(set)?) };
    let t = thread()?;
    let old = t.sig.lock().mask;
    if let Some(n) = new {
        let mask = match u32::try_from(how).map_err(|_| Errno::EINVAL)? {
            SIG_BLOCK => old.union(n),
            SIG_UNBLOCK => old.minus(n),
            SIG_SETMASK => n,
            _ => return Err(Errno::EINVAL),
        };
        signal::set_mask(&t, mask);
    }
    if oset != 0 {
        UserPtr::<Sigset>::new(oset)
            .write(&old)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `sigpending(set)`: everything pending for the process or the
/// thread, blocked or not.
pub(super) fn sigpending(set: u64) -> SysResult {
    let t = thread()?;
    let pending = {
        let st = t.process().signals.lock();
        st.pending().union(t.sig.lock().queue.pending())
    };
    UserPtr::<Sigset>::new(set)
        .write(&pending)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `sigsuspend(sigmask)`: waits with `sigmask` until a signal is
/// handled; always `EINTR`. The handler runs with `sigmask` (plus its
/// own), and `sigreturn` brings back the mask from before.
pub(super) fn sigsuspend(s: &Sleepable, sigmask: u64) -> SysResult {
    let mask = read_set(sigmask)?;
    let t = thread()?;
    signal::swap_mask(&t, mask);
    let q = WaitQueue::new();
    loop {
        if signal::sleep(s, &q, || false, None).is_err() {
            return Err(Errno::EINTR);
        }
    }
}

/// `sigreturn(sigcntxp)`: back from a handler to the context its frame
/// saved (or any context user code built), checked first.
pub(super) fn sigreturn(frame: &mut TrapFrame, ucp: u64) -> SysResult {
    let uc = UserPtr::<Ucontext<Mcontext>>::new(ucp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    set_context(frame, &uc)?;
    Err(Errno::EJUSTRETURN)
}

/// Loads `uc` into the thread: registers, FPU, and mask.
fn set_context(frame: &mut TrapFrame, uc: &Ucontext<Mcontext>) -> Result<(), Errno> {
    signal::set_context(frame, &uc.uc_mcontext)?;
    let t = thread()?;
    sched::current().request_ast(AST_FULL_RESTORE);
    signal::set_mask(&t, uc.uc_sigmask);
    Ok(())
}

/// The current context, as `getcontext` saves it.
fn get_context(frame: &TrapFrame, t: &UserThread) -> Result<Ucontext<Mcontext>, Errno> {
    let (mask, stack) = {
        let ts = t.sig.lock();
        (ts.mask, stack_state(&ts, sp(frame)))
    };
    signal::get_context(frame, mask, stack)
}

/// `getcontext(ucp)`.
pub(super) fn getcontext(frame: &TrapFrame, ucp: u64) -> SysResult {
    let t = thread()?;
    let uc = get_context(frame, &t)?;
    UserPtr::<Ucontext<Mcontext>>::new(ucp)
        .write(&uc)
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `setcontext(ucp)`.
pub(super) fn setcontext(frame: &mut TrapFrame, ucp: u64) -> SysResult {
    let uc = UserPtr::<Ucontext<Mcontext>>::new(ucp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    set_context(frame, &uc)?;
    Err(Errno::EJUSTRETURN)
}

/// `swapcontext(oucp, ucp)`.
pub(super) fn swapcontext(frame: &mut TrapFrame, oucp: u64, ucp: u64) -> SysResult {
    if oucp == 0 || ucp == 0 {
        return Err(Errno::EINVAL);
    }
    let t = thread()?;
    let mut old = get_context(frame, &t)?;
    old.uc_flags |= UCF_SWAPPED;
    UserPtr::<Ucontext<Mcontext>>::new(oucp)
        .write(&old)
        .map_err(|_| Errno::EFAULT)?;
    let uc = UserPtr::<Ucontext<Mcontext>>::new(ucp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    set_context(frame, &uc)?;
    Err(Errno::EJUSTRETURN)
}

/// The user stack pointer.
const fn sp(frame: &TrapFrame) -> u64 {
    #[cfg(target_arch = "x86_64")]
    return frame.rsp;
    #[cfg(target_arch = "aarch64")]
    return frame.sp;
}

/// The alternate stack as `sigaltstack` and contexts report it: its
/// flags say whether it is enabled and in use.
fn stack_state(st: &signal::ThreadSignals, sp: u64) -> StackT {
    let a = st.altstack;
    let flags = if a.ss_flags & SS_DISABLE != 0 {
        SS_DISABLE
    } else if sp.wrapping_sub(a.ss_sp) < a.ss_size {
        SS_ONSTACK
    } else {
        0
    };
    StackT {
        ss_flags: flags,
        ..a
    }
}

/// `sigaltstack(ss, oss)`.
pub(super) fn sigaltstack(frame: &TrapFrame, ss: u64, oss: u64) -> SysResult {
    let new = if ss == 0 {
        None
    } else {
        Some(
            UserPtr::<StackT>::new(ss)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    let t = thread()?;
    let old = {
        let mut st = t.sig.lock();
        let old = stack_state(&st, sp(frame));
        if let Some(n) = new {
            if old.ss_flags == SS_ONSTACK {
                return Err(Errno::EPERM);
            }
            if n.ss_flags & !SS_DISABLE != 0 {
                return Err(Errno::EINVAL);
            }
            if n.ss_flags & SS_DISABLE == 0 {
                if n.ss_size < MINSIGSTKSZ {
                    return Err(Errno::ENOMEM);
                }
                st.altstack = StackT {
                    ss_flags: 0,
                    _pad: 0,
                    ..n
                };
            } else {
                st.altstack.ss_flags = SS_DISABLE;
            }
        }
        old
    };
    if oss != 0 {
        UserPtr::<StackT>::new(oss)
            .write(&old)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// Sends `sig` (0: only checks) to `target`.
fn send(target: &alloc::sync::Arc<Process>, sig: u32, info: Siginfo) -> Result<(), Errno> {
    if sig == 0 {
        return Ok(());
    }
    post(target, info)
}

/// `kill(pid, signum)`: a process, this group (0), every process but
/// init and the caller (-1), or group `-pid`. Everyone runs as root.
pub(super) fn kill(pid: u64, signum: u64) -> SysResult {
    let sig = u32::try_from(int(signum)).map_err(|_| Errno::EINVAL)?;
    if sig != 0 && !signal::valid(sig) {
        return Err(Errno::EINVAL);
    }
    let me = process()?;
    let info = user_info(sig, me.pid(), SI_USER);
    let pid = int(pid);
    if pid > 0 {
        let p = crate::proc::find(pid).ok_or(Errno::ESRCH)?;
        send(&p, sig, info)?;
        return Ok(0);
    }
    let targets: alloc::vec::Vec<_> = crate::proc::all()
        .into_iter()
        .filter(|p| match pid {
            -1 => p.pid() > 1 && p.pid() != me.pid(),
            0 => p.pgid() == me.pgid(),
            g => p.pgid() == -g,
        })
        .collect();
    if targets.is_empty() {
        return Err(Errno::ESRCH);
    }
    for p in &targets {
        let _ = send(p, sig, info);
    }
    Ok(0)
}

/// `thr_kill(id, sig)`: to thread `id` of this process, or with `id` -1
/// to every other one.
pub(super) fn thr_kill(id: u64, sig: u64) -> SysResult {
    let p = process()?;
    thread_kill(&p, id, sig)
}

/// `thr_kill2(pid, id, sig)`: to thread `id` (in process `pid`), or with
/// `id` -1 to every thread of `pid` but the caller.
pub(super) fn thr_kill2(pid: u64, id: u64, sig: u64) -> SysResult {
    if id.cast_signed() == -1 {
        let p = crate::proc::find(int(pid)).ok_or(Errno::ESRCH)?;
        return thread_kill(&p, id, sig);
    }
    // The thread names its process; `pid` must agree (FreeBSD's `tdfind`).
    let tid = i32::try_from(id.cast_signed()).map_err(|_| Errno::ESRCH)?;
    let t = crate::uthread::find(tid).ok_or(Errno::ESRCH)?;
    let pid = int(pid);
    if pid != -1 && t.process().pid() != pid {
        return Err(Errno::ESRCH);
    }
    let p = alloc::sync::Arc::clone(t.process());
    thread_kill(&p, id, sig)
}

/// FreeBSD's order: the thread is found first (`ESRCH`), then the
/// signal checked (`EINVAL`); signal 0 only checks.
fn thread_kill(p: &alloc::sync::Arc<Process>, id: u64, sig: u64) -> SysResult {
    let sig = int(sig);
    let me = thread()?;
    let info = user_info(sig.cast_unsigned(), me.process().pid(), SI_LWP);
    let valid = |sig: i32| u32::try_from(sig).is_ok_and(signal::valid);
    if id.cast_signed() == -1 {
        if sig != 0 && !valid(sig) {
            return Err(Errno::EINVAL);
        }
        let mut found = false;
        for t in p.threads().iter().filter(|t| t.tid() != me.tid()) {
            found = true;
            if sig == 0 {
                break;
            }
            let _ = signal::post_thread(t, info);
        }
        return if found { Ok(0) } else { Err(Errno::ESRCH) };
    }
    let tid = i32::try_from(id.cast_signed()).map_err(|_| Errno::ESRCH)?;
    let t = p.find_thread(tid).ok_or(Errno::ESRCH)?;
    if sig == 0 {
        return Ok(0);
    }
    if !valid(sig) {
        return Err(Errno::EINVAL);
    }
    signal::post_thread(&t, info)?;
    Ok(0)
}

/// `sigqueue(pid, signum, value)`; with `__SIGQUEUE_TID` in `signum`,
/// `pid` is a thread of this process (libthr's `pthread_sigqueue`).
pub(super) fn sigqueue(pid: u64, signum: u64, value: u64) -> SysResult {
    const SIGQUEUE_TID: u32 = 0x8000_0000;
    let flags = int(signum).cast_unsigned();
    let sig = flags & !SIGQUEUE_TID;
    if sig > SIG_MAXSIG {
        return Err(Errno::EINVAL);
    }
    let pid = int(pid);
    if pid <= 0 {
        return Err(Errno::EINVAL);
    }
    let me = process()?;
    let mut info = user_info(sig, me.pid(), SI_QUEUE);
    info.si_value = value;
    if flags & SIGQUEUE_TID != 0 {
        let t = me.find_thread(pid).ok_or(Errno::ESRCH)?;
        if sig != 0 {
            signal::post_thread(&t, info)?;
        }
        return Ok(0);
    }
    let p = crate::proc::find(pid).ok_or(Errno::ESRCH)?;
    send(&p, sig, info)?;
    Ok(0)
}

/// Waits for a signal in `set` and takes it (`sigtimedwait`): the set is
/// unblocked while waiting, so posting it wakes us, but nothing in it is
/// delivered; another signal that must be handled ends the wait with
/// `EINTR`. `EAGAIN` once `timeout` (nanoseconds) passes.
fn wait_for(s: &Sleepable, set: Sigset, timeout: Option<u64>) -> Result<Siginfo, Errno> {
    let set = set.minus(signal::cant_mask());
    let td = thread()?;
    let t = sched::current();
    let before = td.sig.lock().mask;
    signal::swap_mask(&td, before.minus(set));
    let q = WaitQueue::new();
    let deadline = timeout.map(|ns| crate::time::now_ns().saturating_add(ns));
    let r = loop {
        if let Some(info) = signal::take_from(&td, set) {
            break Ok(info);
        }
        let left = deadline.map(|d| d.saturating_sub(crate::time::now_ns()));
        if left == Some(0) {
            break Err(Errno::EAGAIN);
        }
        match crate::time::wait_interruptible(s, &q, || false, left) {
            Wait::TimedOut => {}
            Wait::Done => {}
            Wait::Interrupted => {
                t.clear_interrupted();
                if let Some(info) = signal::take_from(&td, set) {
                    break Ok(info);
                }
                if signal::interruption(s).is_some() {
                    break Err(Errno::EINTR);
                }
            }
        }
    };
    signal::restore_mask(&td);
    r
}

fn write_info(info: u64, si: &Siginfo) -> Result<(), Errno> {
    if info != 0 {
        UserPtr::<Siginfo>::new(info)
            .write(si)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

/// `sigtimedwait(set, info, timeout)`.
pub(super) fn sigtimedwait(s: &Sleepable, set: u64, info: u64, timeout: u64) -> SysResult {
    let set = read_set(set)?;
    let limit = if timeout == 0 {
        None
    } else {
        let t = UserPtr::<Timespec>::new(timeout)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        if t.tv_sec < 0 || !(0..1_000_000_000).contains(&t.tv_nsec) {
            return Err(Errno::EINVAL);
        }
        Some(
            t.tv_sec
                .cast_unsigned()
                .saturating_mul(1_000_000_000)
                .saturating_add(t.tv_nsec.cast_unsigned()),
        )
    };
    let si = wait_for(s, set, limit)?;
    write_info(info, &si)?;
    Ok(u64::from(si.si_signo.cast_unsigned()))
}

/// `sigwaitinfo(set, info)`.
pub(super) fn sigwaitinfo(s: &Sleepable, set: u64, info: u64) -> SysResult {
    let si = wait_for(s, read_set(set)?, None)?;
    write_info(info, &si)?;
    Ok(u64::from(si.si_signo.cast_unsigned()))
}

/// `sigwait(set, sig)`: returns the error rather than setting `errno`.
pub(super) fn sigwait(s: &Sleepable, set: u64, sig: u64) -> SysResult {
    let r = read_set(set).and_then(|set| wait_for(s, set, None));
    match r {
        Ok(si) => {
            UserPtr::<i32>::new(sig)
                .write(&si.si_signo)
                .map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        // libc's sigwait loops on EINTR itself.
        Err(e) => Ok(u64::from(e.get().unsigned_abs())),
    }
}

/// `sigfastblock(cmd, ptr)`: the word libthr uses to block signals
/// without a system call.
pub(super) fn sigfastblock(cmd: u64, ptr: u64) -> SysResult {
    let t = thread()?;
    match u32::try_from(cmd).map_err(|_| Errno::EINVAL)? {
        SIGFASTBLOCK_SETPTR => {
            let mut st = t.sig.lock();
            if st.fastblock != 0 {
                return Err(Errno::EBUSY);
            }
            if !ptr.is_multiple_of(4) || ptr == 0 {
                return Err(Errno::EINVAL);
            }
            st.fastblock = ptr;
        }
        SIGFASTBLOCK_UNBLOCK => {
            let at = t.sig.lock().fastblock;
            if at == 0 {
                return Err(Errno::EINVAL);
            }
            // Only a word holding nothing but the pending bit unblocks.
            let w = UserPtr::<u32>::new(at);
            if w.read().map_err(|_| Errno::EFAULT)? != SIGFASTBLOCK_PEND {
                return Err(Errno::EBUSY);
            }
            w.write(&0).map_err(|_| Errno::EFAULT)?;
            sched::current().request_ast(AST_SIGNAL);
        }
        SIGFASTBLOCK_UNSETPTR => {
            let mut st = t.sig.lock();
            if st.fastblock == 0 {
                return Err(Errno::EINVAL);
            }
            st.fastblock = 0;
            drop(st);
            signal::recheck(&t);
        }
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}
