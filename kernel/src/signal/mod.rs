// SPDX-License-Identifier: BSD-2-Clause
//! Signals: posting, delivery on the way back to user mode, default
//! actions, job control's stops, and the sleeps a signal interrupts.
//!
//! - **Posting** ([`post`]) does at once what cannot wait. `SIGCONT`
//!   continues a stopped process and cancels pending stops; a stop
//!   cancels a pending `SIGCONT`; `SIGKILL` ends a stop. A signal the
//!   process ignores is dropped; the rest are queued with their
//!   `siginfo`, and the thread is interrupted unless it blocks the signal.
//! - **Delivery** happens in the process's own thread on its way back to
//!   user mode ([`user_return`], FreeBSD's `ast`). Each deliverable
//!   signal, lowest number first, is ignored, stops the process, ends it,
//!   or gets a frame on the user stack that runs its handler and returns
//!   through `sigreturn` (the trampoline is in the shared page).
//! - **Sleeps** a signal interrupts ([`sleep`]) end with `EINTR`, or with
//!   `ERESTART` when the handler has `SA_RESTART`, which re-executes the
//!   system call after the handler returns. A stop happens in place: the
//!   sleep goes on once the process continues.
//! - **Faults** ([`trap`]) are delivered at once if caught; one that is
//!   blocked or ignored is reset to its default action, so a faulting
//!   process cannot loop.
//! - libthr's `sigfastblock` word blocks everything but `SIGKILL` and
//!   `SIGSTOP` while it is nonzero; a signal held back by it sets the
//!   word's pending bit, and libthr's `SIGFASTBLOCK_UNBLOCK` brings it in.
//!
//! The mask, pending set, and alternate stack are the process's while it
//! has one thread.
//!
//! Behavior reference: sys/kern/kern_sig.c, sys/kern/subr_sleepqueue.c.

#[cfg(target_arch = "aarch64")]
#[path = "aarch64.rs"]
mod machdep;
#[cfg(target_arch = "x86_64")]
#[path = "x86_64.rs"]
mod machdep;
mod trap;

pub(crate) use machdep::{get_context, set_context};
pub(crate) use trap::trap;

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::signal::*;
use abi::thr::{SIGFASTBLOCK_FLAGS, SIGFASTBLOCK_PEND};
use sched::{Sleepable, WaitQueue};

use crate::arch::trap::TrapFrame;
use crate::proc::{Exit, Process};
use crate::time::Wait;
use crate::uaccess::{Plain, UserPtr};

// SAFETY: integers only, padding explicit (`abi-check` holds the C layout).
unsafe impl Plain for Siginfo {}
// SAFETY: as above.
unsafe impl Plain for StackT {}

/// `ast` work bits: signals to look at, and a frame the kernel rewrote
/// (amd64 must then return with `iretq`, which restores every register).
pub(crate) const AST_SIGNAL: u32 = 1;
pub(crate) const AST_FULL_RESTORE: u32 = 2;

/// Queued signals a process may have beyond one of each (real-time
/// signals queue; FreeBSD's `max_pending_per_proc`).
const MAX_QUEUED: usize = 128;

/// Default-action properties (FreeBSD's `sigprop`).
const KILL: u8 = 1;
const CORE: u8 = 2;
const STOP: u8 = 4;
const TTYSTOP: u8 = 8;
const IGNORE: u8 = 16;
const CONT: u8 = 32;

/// What `sig` does by default. Signals past `SIGUSR2` end the process.
const fn prop(sig: u32) -> u8 {
    match sig {
        SIGHUP | SIGINT | SIGKILL | SIGPIPE | SIGALRM | SIGTERM | SIGXCPU | SIGXFSZ | SIGVTALRM
        | SIGPROF | SIGUSR1 | SIGUSR2 => KILL,
        SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGEMT | SIGFPE | SIGBUS | SIGSEGV | SIGSYS => {
            KILL | CORE
        }
        SIGURG | SIGCHLD | SIGIO | SIGWINCH | SIGINFO => IGNORE,
        SIGSTOP => STOP,
        SIGTSTP | SIGTTIN | SIGTTOU => STOP | TTYSTOP,
        SIGCONT => IGNORE | CONT,
        _ => 0,
    }
}

/// Whether `sig` is a valid signal number.
pub(crate) const fn valid(sig: u32) -> bool {
    sig >= 1 && sig <= SIG_MAXSIG
}

/// Real-time signals queue every instance.
const fn realtime(sig: u32) -> bool {
    sig >= SIGRTMIN && sig <= SIGRTMAX
}

fn set_of(sigs: &[u32]) -> Sigset {
    let mut s = Sigset::default();
    for &sig in sigs {
        s.add(sig);
    }
    s
}

/// What no mask blocks.
pub(crate) fn cant_mask() -> Sigset {
    set_of(&[SIGKILL, SIGSTOP])
}

/// What the `sigfastblock` word blocks.
fn fastblock_mask() -> Sigset {
    let mut s = Sigset {
        bits: [u32::MAX; SIG_WORDS],
    };
    s.remove(SIGKILL);
    s.remove(SIGSTOP);
    s
}

/// A process's signal state.
#[derive(Clone)]
pub(crate) struct Signals {
    /// Indexed by signal number minus one.
    actions: [Sigaction; SIG_MAXSIG as usize],
    /// Blocked signals.
    pub(crate) mask: Sigset,
    /// The `sigfastblock` word's address, 0 for none.
    pub(crate) fastblock: u64,
    pending: Sigset,
    /// The pending signals' information, oldest first: one entry per
    /// pending standard signal, any number per real-time one.
    queue: VecDeque<Siginfo>,
    /// The alternate signal stack (`ss_flags` `SS_DISABLE` when none).
    pub(crate) altstack: StackT,
    /// The mask to restore once a handler's frame is built (FreeBSD's
    /// `td_oldsigmask`, set by `sigsuspend` and `sigtimedwait`).
    saved_mask: Option<Sigset>,
}

impl Signals {
    /// The state of a process with nothing set.
    pub(crate) fn new() -> Self {
        Self {
            actions: [Sigaction::default(); SIG_MAXSIG as usize],
            mask: Sigset::default(),
            fastblock: 0,
            pending: Sigset::default(),
            queue: VecDeque::new(),
            altstack: StackT {
                ss_flags: SS_DISABLE,
                ..StackT::default()
            },
            saved_mask: None,
        }
    }

    /// A forked child's: the dispositions, mask, and alternate stack;
    /// nothing pending.
    pub(crate) fn forked(&self) -> Self {
        Self {
            actions: self.actions,
            mask: self.mask,
            fastblock: self.fastblock,
            altstack: self.altstack,
            ..Self::new()
        }
    }

    /// After exec: caught signals go back to their defaults, and the
    /// alternate stack and the `sigfastblock` word are forgotten; the mask
    /// and pending signals stay.
    pub(crate) fn exec(&mut self) {
        for a in &mut self.actions {
            if a.sa_handler != SIG_IGN {
                *a = Sigaction::default();
            }
        }
        self.fastblock = 0;
        self.altstack = StackT {
            ss_flags: SS_DISABLE,
            ..StackT::default()
        };
    }

    pub(crate) fn action(&self, sig: u32) -> Sigaction {
        self.actions[(sig - 1) as usize]
    }

    /// Sets `sig`'s action (`sigaction`); pending signals it now ignores
    /// are dropped.
    pub(crate) fn set_action(&mut self, sig: u32, mut a: Sigaction) {
        a.sa_mask = a.sa_mask.minus(cant_mask());
        self.actions[(sig - 1) as usize] = a;
        if self.ignored(sig) {
            self.discard(sig);
        }
    }

    /// Whether posting `sig` drops it (FreeBSD's `ps_sigignore`).
    fn ignored(&self, sig: u32) -> bool {
        let h = self.action(sig).sa_handler;
        h == SIG_IGN || (h == SIG_DFL && prop(sig) & IGNORE != 0)
    }

    /// Whether a handler catches `sig`.
    fn caught(&self, sig: u32) -> bool {
        self.action(sig).sa_handler > SIG_IGN
    }

    /// The signals ignored and the signals caught (`ki_sigignore`,
    /// `ki_sigcatch`).
    pub(crate) fn dispositions(&self) -> (Sigset, Sigset) {
        let (mut ignored, mut caught) = (Sigset::default(), Sigset::default());
        for sig in 1..=SIG_MAXSIG {
            if self.ignored(sig) {
                ignored.add(sig);
            } else if self.caught(sig) {
                caught.add(sig);
            }
        }
        (ignored, caught)
    }

    pub(crate) fn pending(&self) -> Sigset {
        self.pending
    }

    fn queue(&mut self, info: Siginfo) -> Result<(), Errno> {
        let sig = info.si_signo.cast_unsigned();
        if self.pending.has(sig) && !realtime(sig) {
            // Standard signals do not queue: the first one's information
            // stands.
            return Ok(());
        }
        if self.queue.len() >= MAX_QUEUED + 32 && realtime(sig) {
            return Err(Errno::EAGAIN);
        }
        self.queue.push_back(info);
        self.pending.add(sig);
        Ok(())
    }

    /// Takes one pending `sig` (its oldest information).
    fn take(&mut self, sig: u32) -> Siginfo {
        let signo = sig.cast_signed();
        let info = match self.queue.iter().position(|i| i.si_signo == signo) {
            Some(i) => self.queue.remove(i).unwrap_or_default(),
            None => Siginfo {
                si_signo: signo,
                ..Siginfo::default()
            },
        };
        if !self.queue.iter().any(|i| i.si_signo == signo) {
            self.pending.remove(sig);
        }
        info
    }

    /// Drops every pending `sig`.
    fn discard(&mut self, sig: u32) {
        let signo = sig.cast_signed();
        self.queue.retain(|i| i.si_signo != signo);
        self.pending.remove(sig);
    }
}

/// A copy of this thread's FP/SIMD registers (the kernel never uses
/// them, so the live ones are the user's), in the save-area format.
pub(super) struct FpuImage {
    ptr: core::ptr::NonNull<u8>,
    len: usize,
}

impl FpuImage {
    fn layout(len: usize) -> core::alloc::Layout {
        core::alloc::Layout::from_size_align(len.max(16), hal::fpu::ALIGN)
            .unwrap_or(core::alloc::Layout::new::<u8>())
    }

    /// The registers as they are now, or `None` without memory.
    pub(super) fn saved() -> Option<Self> {
        let len = hal::fpu::area_size();
        // Zeroed: XSAVE leaves most of its header alone (XCOMP_BV and the
        // reserved bytes must read as zero to be loaded again).
        // SAFETY: a nonzero size and a valid alignment.
        let ptr =
            core::ptr::NonNull::new(unsafe { alloc::alloc::alloc_zeroed(Self::layout(len)) })?;
        // SAFETY: a fresh area of the size and alignment the HAL needs.
        unsafe { hal::fpu::save(ptr.as_ptr()) };
        Some(Self { ptr, len })
    }

    /// The initial state.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub(super) fn initial() -> Option<Self> {
        let mut me = Self::saved()?;
        me.reset();
        Some(me)
    }

    /// Replaces the image with the initial state.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub(super) fn reset(&mut self) {
        // SAFETY: an area of the size and alignment the HAL needs.
        unsafe { hal::fpu::init(self.ptr.as_ptr()) };
    }

    pub(super) fn bytes(&self) -> &[u8] {
        // SAFETY: `len` initialized bytes, ours.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub(super) fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, borrowed mutably.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Loads the image into this CPU's registers, which are the thread's.
    ///
    /// # Safety
    ///
    /// The image must be one the CPU loads without faulting: a saved or
    /// initial state, changed only in ways the caller checked.
    pub(super) unsafe fn load(&self) {
        // SAFETY: per the contract.
        unsafe { hal::fpu::restore(self.ptr.as_ptr()) };
    }
}

impl Drop for FpuImage {
    fn drop(&mut self) {
        // SAFETY: allocated in `saved` with this layout.
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), Self::layout(self.len)) };
    }
}

/// What `sendsig` needs to build a handler's frame.
pub(super) struct Delivery<'a> {
    pub(super) sig: u32,
    pub(super) info: &'a Siginfo,
    pub(super) action: &'a Sigaction,
    /// The mask `sigreturn` restores.
    pub(super) mask: Sigset,
    pub(super) altstack: StackT,
    /// The trampoline in the shared page.
    pub(super) trampoline: u64,
}

impl Delivery<'_> {
    /// Whether the handler runs on the alternate stack, and the context's
    /// `uc_stack` (whose flags say where the thread was).
    pub(super) fn stack(&self, sp: u64) -> (Option<u64>, StackT) {
        let st = &self.altstack;
        let enabled = st.ss_flags & SS_DISABLE == 0;
        let on = enabled && sp.wrapping_sub(st.ss_sp) < st.ss_size;
        let switch = enabled && !on && self.action.sa_flags & SA_ONSTACK != 0;
        let flags = if !enabled {
            SS_DISABLE
        } else if on {
            SS_ONSTACK
        } else {
            0
        };
        (
            switch.then(|| st.ss_sp.wrapping_add(st.ss_size)),
            StackT {
                ss_flags: flags,
                ..*st
            },
        )
    }
}

/// A signal sent by a process (`kill`).
pub(crate) fn user_info(sig: u32, from: i32, code: i32) -> Siginfo {
    Siginfo {
        si_signo: sig.cast_signed(),
        si_code: code,
        si_pid: from,
        ..Siginfo::default()
    }
}

/// Sends `info.si_signo` to `p` (FreeBSD's `tdsendsignal`). Fails only
/// when a real-time signal's queue is full.
pub(crate) fn post(p: &Arc<Process>, info: Siginfo) -> Result<(), Errno> {
    let sig = info.si_signo.cast_unsigned();
    let prop = prop(sig);
    // Decided before taking the signal lock (it reads the process tree).
    let orphaned = prop & TTYSTOP != 0 && crate::proc::pgrp_orphaned(p.pgid());
    let (continued, interrupt) = {
        let mut st = p.signals.lock();
        if prop & CONT != 0 {
            for s in [SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU] {
                st.discard(s);
            }
        }
        if prop & STOP != 0 {
            st.discard(SIGCONT);
        }
        // SIGCONT continues even when caught, blocked, or ignored; SIGKILL
        // ends a stop to end the process.
        let continued = (sig == SIGCONT || sig == SIGKILL) && p.resume(sig == SIGCONT);
        if (st.ignored(sig) && sig != SIGKILL) || (orphaned && st.action(sig).sa_handler == SIG_DFL)
        {
            (continued, false)
        } else {
            st.queue(info)?;
            let stopped = p.job.lock().stopped != 0;
            (continued, !st.mask.has(sig) && (!stopped || sig == SIGKILL))
        }
    };
    if continued {
        notify_parent(p, code::CLD_CONTINUED, SIGCONT.cast_signed());
    }
    if interrupt && let Some(t) = p.thread() {
        t.interrupt(AST_SIGNAL);
    }
    Ok(())
}

/// Tells `child`'s parent of a stop, a continue, or an exit: its waiters
/// wake, and it gets `SIGCHLD` (not for stops and continues if it asked
/// for `SA_NOCLDSTOP`).
pub(crate) fn notify_parent(child: &Process, code: i32, status: i32) {
    let Some(parent) = child.parent() else {
        return;
    };
    let nocldstop = parent.signals.lock().action(SIGCHLD).sa_flags & SA_NOCLDSTOP != 0;
    if nocldstop && (code == code::CLD_STOPPED || code == code::CLD_CONTINUED) {
        parent.child_changed();
        return;
    }
    // The signal first, then the wakeup, as FreeBSD's exit does: a
    // parent waiting with a handler (and no `SA_RESTART`) sees `EINTR`.
    let _ = post(
        &parent,
        Siginfo {
            si_signo: SIGCHLD.cast_signed(),
            si_code: code,
            si_pid: child.pid(),
            si_status: status,
            ..Siginfo::default()
        },
    );
    parent.child_changed();
}

/// Sends `sig` from the kernel to every process in group `pgid`.
pub(crate) fn post_group(pgid: i32, sig: u32) {
    for p in crate::proc::all().iter().filter(|p| p.pgid() == pgid) {
        let _ = post(p, user_info(sig, 0, code::SI_KERNEL));
    }
}

/// Whether `p` ignores or blocks `sig` (a terminal then does not send
/// it, FreeBSD's `tty_wait_background`).
pub(crate) fn ignores_or_blocks(p: &Process, sig: u32) -> bool {
    let st = p.signals.lock();
    st.action(sig).sa_handler == SIG_IGN || st.mask.has(sig)
}

/// Whether `p`'s parent asked not to keep zombies (`SIGCHLD` ignored, or
/// `SA_NOCLDWAIT`).
pub(crate) fn parent_reaps_nothing(parent: &Process) -> bool {
    let a = parent.signals.lock().action(SIGCHLD);
    a.sa_handler == SIG_IGN || a.sa_flags & SA_NOCLDWAIT != 0
}

/// What the `sigfastblock` word blocks now (empty without one).
fn fastblocked(p: &Process) -> Sigset {
    let ptr = p.signals.lock().fastblock;
    if ptr == 0 {
        return Sigset::default();
    }
    match UserPtr::<u32>::new(ptr).read() {
        Ok(w) if w & !SIGFASTBLOCK_FLAGS != 0 => fastblock_mask(),
        _ => Sigset::default(),
    }
}

/// Marks the `sigfastblock` word: a signal waits for libthr's unblock.
fn set_fastblock_pending(p: &Process) {
    let ptr = p.signals.lock().fastblock;
    let w = UserPtr::<u32>::new(ptr);
    if let Ok(v) = w.read() {
        let _ = w.write(&(v | SIGFASTBLOCK_PEND));
    }
}

/// What to do about the next deliverable signal.
enum Next {
    /// Run its handler.
    Handle(u32, Siginfo, Sigaction),
    /// End the process.
    Kill(u32),
    /// Stop the process (done in place, after which delivery goes on).
    Stop(u32),
}

/// Takes the next signal that needs action, dropping ignored ones on the
/// way. `None` when nothing is deliverable.
fn next(p: &Process) -> Option<Next> {
    let fast = fastblocked(p);
    let orphaned = crate::proc::pgrp_orphaned(p.pgid());
    let mut st = p.signals.lock();
    loop {
        let ready = st.pending.minus(st.mask);
        let Some(sig) = ready.minus(fast).first() else {
            if !ready.is_empty() {
                drop(st);
                set_fastblock_pending(p);
            }
            return None;
        };
        let info = st.take(sig);
        let action = st.action(sig);
        if action.sa_handler == SIG_IGN {
            continue;
        }
        if action.sa_handler != SIG_DFL {
            return Some(Next::Handle(sig, info, action));
        }
        let prop = prop(sig);
        if prop & STOP != 0 {
            if prop & TTYSTOP != 0 && orphaned {
                continue;
            }
            return Some(Next::Stop(sig));
        }
        if prop & IGNORE != 0 {
            continue;
        }
        return Some(Next::Kill(sig));
    }
}

/// Stops `p` for `sig` until it is continued (or killed).
fn stop(s: &Sleepable, p: &Arc<Process>, sig: u32) {
    {
        let mut job = p.job.lock();
        job.stopped = sig;
        job.stop_report = true;
        job.cont_report = false;
    }
    notify_parent(p, code::CLD_STOPPED, sig.cast_signed());
    let t = sched::current();
    loop {
        let r = p
            .stop_wait
            .wait_until_interruptible(s, || p.job.lock().stopped == 0);
        if r.is_ok() {
            return;
        }
        // Other signals wait for the continue.
        t.clear_interrupted();
    }
}

/// The kernel's last word before a thread returns to user mode (the
/// `user_return` hook): pending signals. Interrupts masked on entry and
/// exit. Returns whether the frame was rewritten.
pub(crate) fn user_return(frame: &mut TrapFrame) -> bool {
    let t = sched::current();
    let mut rewritten = false;
    while t.ast_pending() {
        let bits = t.take_ast();
        hal::irq::enable();
        // SAFETY: a thread on its way back to user mode holds nothing.
        let s = unsafe { Sleepable::assume() };
        if bits & AST_FULL_RESTORE != 0 {
            rewritten = true;
        }
        if bits & AST_SIGNAL != 0 {
            rewritten |= deliver(&s, frame);
        }
        let _ = hal::irq::disable();
    }
    rewritten
}

/// Acts on every deliverable signal. Returns whether a handler's frame
/// was built.
fn deliver(s: &Sleepable, frame: &mut TrapFrame) -> bool {
    let Some(p) = Process::current() else {
        return false;
    };
    let t = sched::current();
    let mut built = false;
    loop {
        // Cleared before looking, so a signal posted after the look
        // interrupts the next sleep.
        t.clear_interrupted();
        match next(&p) {
            None => break,
            // The process goes with its last reference from this stack.
            Some(Next::Kill(sig)) => kill_self(s, p, sig),
            Some(Next::Stop(sig)) => stop(s, &p, sig),
            Some(Next::Handle(sig, info, action)) => {
                if !handle(s, &p, frame, sig, &info, &action) {
                    kill_self(s, p, SIGILL);
                }
                built = true;
            }
        }
    }
    // A `sigsuspend` whose signal reached no handler gets its mask back.
    let mut st = p.signals.lock();
    if let Some(m) = st.saved_mask.take() {
        st.mask = m;
    }
    built
}

/// Ends the current process by `sig` (FreeBSD's `sigexit`; there are no
/// core dumps). Takes the caller's reference: nothing on a stack that
/// never unwinds may keep the process.
pub(super) fn kill_self(s: &Sleepable, p: Arc<Process>, sig: u32) -> ! {
    p.exit(s, Exit::Signal(sig));
    drop(p);
    sched::exit()
}

/// Builds `sig`'s handler frame on the user stack and updates the mask
/// (FreeBSD's `postsig`). Returns `false` if the frame cannot be written:
/// the caller ends the process with `SIGILL` (with the reference it
/// holds, see [`kill_self`]).
#[must_use]
fn handle(
    _s: &Sleepable,
    p: &Arc<Process>,
    frame: &mut TrapFrame,
    sig: u32,
    info: &Siginfo,
    action: &Sigaction,
) -> bool {
    let (mask, altstack) = {
        let mut st = p.signals.lock();
        (st.saved_mask.take().unwrap_or(st.mask), st.altstack)
    };
    let d = Delivery {
        sig,
        info,
        action,
        mask,
        altstack,
        trampoline: crate::shared_page::sigcode_address(p.layout().shared_page),
    };
    if machdep::sendsig(frame, &d).is_err() {
        kprintln!(
            "pid {}: cannot write a signal frame for signal {sig}; killing it",
            p.pid()
        );
        return false;
    }
    let mut st = p.signals.lock();
    st.mask = st.mask.union(action.sa_mask);
    if action.sa_flags & SA_NODEFER == 0 {
        st.mask.add(sig);
    }
    st.mask = st.mask.minus(cant_mask());
    if action.sa_flags & SA_RESETHAND != 0 && sig != SIGILL && sig != SIGTRAP {
        st.actions[(sig - 1) as usize] = Sigaction::default();
    }
    true
}

/// After the current thread unblocked signals or changed its pending set:
/// a deliverable one is looked at before user mode, and ends the next
/// interruptible sleep (a signal posted while blocked interrupted
/// nothing).
pub(crate) fn recheck(p: &Process) {
    let deliverable = {
        let st = p.signals.lock();
        !st.pending.minus(st.mask).is_empty()
    };
    if deliverable {
        sched::current_thread().interrupt(AST_SIGNAL);
    }
}

/// Why an interrupted sleep ended, if a signal must be handled
/// (FreeBSD's `sleepq_catch_signals`): `EINTR`, or `ERESTART` for a
/// handler with `SA_RESTART`. `None` means sleep on: what interrupted it
/// was blocked, ignored, or a stop (done here, in place).
pub(crate) fn interruption(s: &Sleepable) -> Option<Errno> {
    interrupted_by(s).0
}

/// [`interruption`], and whether the process stopped (and has been
/// continued) meanwhile.
fn interrupted_by(s: &Sleepable) -> (Option<Errno>, bool) {
    let Some(p) = Process::current() else {
        return (None, false);
    };
    let t = sched::current();
    let mut stopped = false;
    loop {
        t.clear_interrupted();
        let fast = fastblocked(&p);
        let orphaned = crate::proc::pgrp_orphaned(p.pgid());
        let mut st = p.signals.lock();
        let Some(sig) = st.pending.minus(st.mask).minus(fast).first() else {
            return (None, stopped);
        };
        let action = st.action(sig);
        if st.ignored(sig) {
            st.discard(sig);
            continue;
        }
        if action.sa_handler == SIG_DFL && prop(sig) & STOP != 0 {
            st.take(sig);
            drop(st);
            if !(prop(sig) & TTYSTOP != 0 && orphaned) {
                stop(s, &p, sig);
                stopped = true;
            }
            continue;
        }
        // Delivery happens on the way out; the thread looks again there.
        t.request_ast(AST_SIGNAL);
        let e = if st.caught(sig) && action.sa_flags & SA_RESTART != 0 {
            Errno::ERESTART
        } else {
            Errno::EINTR
        };
        return (Some(e), stopped);
    }
}

/// How a [`sleep`] ended without a signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Woke {
    /// The condition held.
    Done,
    TimedOut,
}

/// Sleeps on `q` until `cond` holds or `ns` nanoseconds pass (`None`: no
/// limit), unless a signal must be handled first: then `EINTR` or
/// `ERESTART` (see [`interruption`]). Kernel threads sleep through
/// interruptions.
pub(crate) fn sleep(
    s: &Sleepable,
    q: &WaitQueue,
    mut cond: impl FnMut() -> bool,
    ns: Option<u64>,
) -> Result<Woke, Errno> {
    let deadline = ns.map(|ns| crate::time::now_ns().saturating_add(ns));
    loop {
        let left = deadline.map(|d| d.saturating_sub(crate::time::now_ns()));
        match crate::time::wait_interruptible(s, q, &mut cond, left) {
            Wait::Done => return Ok(Woke::Done),
            Wait::TimedOut => return Ok(Woke::TimedOut),
            Wait::Interrupted => {
                if let Some(e) = interruption(s) {
                    return Err(e);
                }
            }
        }
    }
}

/// [`sleep`] (with no timeout) for a thread that just sent its own group a
/// stop signal (a terminal's `SIGTTIN` or `SIGTTOU`): once the stop is
/// over, the sleep ends with `EINTR`, as FreeBSD's does.
pub(crate) fn sleep_after_stop_signal(
    s: &Sleepable,
    q: &WaitQueue,
    mut cond: impl FnMut() -> bool,
) -> Result<(), Errno> {
    loop {
        match crate::time::wait_interruptible(s, q, &mut cond, None) {
            Wait::Done => return Ok(()),
            Wait::TimedOut => {}
            Wait::Interrupted => match interrupted_by(s) {
                (Some(e), _) => return Err(e),
                (None, true) => return Err(Errno::EINTR),
                (None, false) => {}
            },
        }
    }
}

/// `sigsuspend`'s and `sigtimedwait`'s temporary mask: `mask` now, the
/// current one back once a handler's frame is built (or delivery finds
/// nothing to handle).
pub(crate) fn swap_mask(p: &Process, mask: Sigset) {
    {
        let mut st = p.signals.lock();
        st.saved_mask = Some(st.mask);
        st.mask = mask.minus(cant_mask());
    }
    recheck(p);
}

/// Undoes [`swap_mask`] when the call returns without a handler to run.
pub(crate) fn restore_mask(p: &Process) {
    let mut st = p.signals.lock();
    if let Some(m) = st.saved_mask.take() {
        st.mask = m;
    }
}

/// Takes a pending signal in `set`, if any (`sigtimedwait`).
pub(crate) fn take_from(p: &Process, set: Sigset) -> Option<Siginfo> {
    let mut st = p.signals.lock();
    let sig = st.pending.intersect(set).first()?;
    Some(st.take(sig))
}
