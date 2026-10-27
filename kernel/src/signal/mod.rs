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
//! Each thread has its mask, alternate stack, `sigfastblock` word, and
//! the signals sent to it (faults, `thr_kill`); the process has the
//! dispositions and the signals sent to it as a whole, which go to the
//! first thread that does not block them (`sigtd`), or to another if it
//! blocks them before they are delivered. Stops stop every thread.
//!
//! Behavior reference: sys/kern/kern_sig.c, sys/kern/subr_sleepqueue.c.

#[cfg(target_arch = "aarch64")]
#[path = "aarch64.rs"]
mod machdep;
#[cfg(target_arch = "x86_64")]
#[path = "x86_64.rs"]
mod machdep;
mod trap;

pub(crate) use machdep::{get_context, prepare_context};
pub(crate) use trap::trap;

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::signal::*;
use abi::thr::{SIGFASTBLOCK_FLAGS, SIGFASTBLOCK_PEND};
use sched::{Sleepable, WaitQueue};

use core::ptr;

use crate::arch::trap::TrapFrame;
use crate::proc::{Exit, Mode, Process, Suspension};
use crate::time::Wait;
use crate::uaccess::{Plain, UserPtr};
use crate::uthread::UserThread;

// SAFETY: integers only, padding explicit (`abi-check` holds the C layout).
unsafe impl Plain for Siginfo {}
// SAFETY: as above.
unsafe impl Plain for StackT {}

/// `ast` work bits: signals to look at, and a frame the kernel rewrote
/// (amd64 must then return with `iretq`, which restores every register).
pub(crate) const AST_SIGNAL: u32 = 1;
pub(crate) const AST_FULL_RESTORE: u32 = 2;
/// I/O port access to give (`KDENABIO`) or take away (`KDDISABIO`).
const AST_IO_ON: u32 = 4;
const AST_IO_OFF: u32 = 8;
/// A stop or single-threading wants the thread at the boundary.
pub(crate) const AST_SUSPEND: u32 = 16;
/// Another thread changed this one's CPUs: it moves.
pub(crate) const AST_AFFINITY: u32 = 32;

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

/// Signals waiting for delivery, and each one's information (FreeBSD's
/// `sigqueue_t`): a process has one for signals sent to it, and each
/// thread one for signals sent to that thread.
#[derive(Clone, Default)]
pub(crate) struct Queue {
    pending: Sigset,
    /// Oldest first: one entry per pending standard signal, any number
    /// per real-time one.
    info: VecDeque<Siginfo>,
}

impl Queue {
    pub(crate) fn pending(&self) -> Sigset {
        self.pending
    }

    fn add(&mut self, info: Siginfo) -> Result<(), Errno> {
        let sig = info.si_signo.cast_unsigned();
        if self.pending.has(sig) && !realtime(sig) {
            // Standard signals do not queue: the first one's information
            // stands.
            return Ok(());
        }
        if self.info.len() >= MAX_QUEUED + 32 && realtime(sig) {
            return Err(Errno::EAGAIN);
        }
        self.info.push_back(info);
        self.pending.add(sig);
        Ok(())
    }

    /// Takes one pending `sig` (its oldest information), if there is one.
    fn take(&mut self, sig: u32) -> Option<Siginfo> {
        if !self.pending.has(sig) {
            return None;
        }
        let signo = sig.cast_signed();
        let info = match self.info.iter().position(|i| i.si_signo == signo) {
            Some(i) => self.info.remove(i).unwrap_or_default(),
            None => Siginfo {
                si_signo: signo,
                ..Siginfo::default()
            },
        };
        if !self.info.iter().any(|i| i.si_signo == signo) {
            self.pending.remove(sig);
        }
        Some(info)
    }

    /// Drops every pending `sig`.
    fn discard(&mut self, sig: u32) {
        let signo = sig.cast_signed();
        self.info.retain(|i| i.si_signo != signo);
        self.pending.remove(sig);
    }
}

/// A process's signal state: the dispositions (FreeBSD's `sigacts`) and
/// the signals sent to the process as a whole.
#[derive(Clone)]
pub(crate) struct Signals {
    /// Indexed by signal number minus one.
    actions: [Sigaction; SIG_MAXSIG as usize],
    pub(crate) queue: Queue,
}

impl Signals {
    /// The state of a process with nothing set.
    pub(crate) fn new() -> Self {
        Self {
            actions: [Sigaction::default(); SIG_MAXSIG as usize],
            queue: Queue::default(),
        }
    }

    /// A forked child's: the dispositions; nothing pending.
    pub(crate) fn forked(&self) -> Self {
        Self {
            actions: self.actions,
            queue: Queue::default(),
        }
    }

    /// After exec: caught signals go back to their defaults; pending
    /// signals stay.
    pub(crate) fn exec(&mut self) {
        for a in &mut self.actions {
            if a.sa_handler != SIG_IGN {
                *a = Sigaction::default();
            }
        }
    }

    pub(crate) fn action(&self, sig: u32) -> Sigaction {
        self.actions[(sig - 1) as usize]
    }

    /// Sets `sig`'s action (`sigaction`). The caller drops pending signals
    /// it now ignores ([`discard_ignored`]).
    fn set_action(&mut self, sig: u32, mut a: Sigaction) {
        a.sa_mask = a.sa_mask.minus(cant_mask());
        self.actions[(sig - 1) as usize] = a;
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
        self.queue.pending
    }
}

/// A thread's signal state, under its own lock, taken after the
/// process's [`Signals`] lock when both are held.
#[derive(Clone)]
pub(crate) struct ThreadSignals {
    /// Blocked signals.
    pub(crate) mask: Sigset,
    /// The `sigfastblock` word's address, 0 for none.
    pub(crate) fastblock: u64,
    /// Signals sent to this thread.
    pub(crate) queue: Queue,
    /// The alternate signal stack (`ss_flags` `SS_DISABLE` when none).
    pub(crate) altstack: StackT,
    /// The mask to restore once a handler's frame is built (FreeBSD's
    /// `td_oldsigmask`, set by `sigsuspend` and `sigtimedwait`).
    saved_mask: Option<Sigset>,
}

impl ThreadSignals {
    /// A thread with nothing blocked and no alternate stack.
    pub(crate) fn new() -> Self {
        Self {
            mask: Sigset::default(),
            fastblock: 0,
            queue: Queue::default(),
            altstack: StackT {
                ss_flags: SS_DISABLE,
                ..StackT::default()
            },
            saved_mask: None,
        }
    }

    /// A forked child's thread: the mask, the alternate stack, and the
    /// `sigfastblock` word; nothing pending.
    pub(crate) fn forked(&self) -> Self {
        Self {
            mask: self.mask,
            fastblock: self.fastblock,
            altstack: self.altstack,
            ..Self::new()
        }
    }

    /// A thread `thr_new` creates: the creator's mask (FreeBSD's
    /// `td_startcopy`); no alternate stack and no `sigfastblock` word
    /// (`TDP_SIGFASTBLOCK` is not copied; libthr sets the new thread's).
    pub(crate) fn created(&self) -> Self {
        Self {
            mask: self.mask,
            ..Self::new()
        }
    }

    /// After exec: the alternate stack and the `sigfastblock` word are
    /// forgotten; the mask and pending signals stay.
    pub(crate) fn exec(&mut self) {
        self.fastblock = 0;
        self.altstack = StackT {
            ss_flags: SS_DISABLE,
            ..StackT::default()
        };
    }
}

/// A copy of this thread's FP/SIMD registers (the kernel never uses
/// them, so the live ones are the user's), in the save-area format.
pub(crate) struct FpuImage {
    ptr: core::ptr::NonNull<u8>,
    len: usize,
}

impl FpuImage {
    fn layout(len: usize) -> core::alloc::Layout {
        core::alloc::Layout::from_size_align(len.max(16), hal::fpu::ALIGN)
            .unwrap_or(core::alloc::Layout::new::<u8>())
    }

    /// The registers as they are now, or `None` without memory.
    pub(crate) fn saved() -> Option<Self> {
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

    pub(crate) fn bytes(&self) -> &[u8] {
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

/// A context checked and put in a frame (`prepare_context`), with what
/// else it sets for the thread that loads it: its FP state, and on amd64
/// its FS base.
pub(crate) struct Prepared {
    /// Checked to load without faulting.
    fpu: Option<FpuImage>,
    tls: Option<u64>,
    /// The second thread register (amd64's GS base).
    tls2: Option<u64>,
}

impl Prepared {
    /// The user thread pointer it sets, if any.
    pub(crate) const fn tls(&self) -> Option<u64> {
        self.tls
    }

    /// The second user thread register it sets, if any.
    pub(crate) const fn tls2(&self) -> Option<u64> {
        self.tls2
    }

    /// Loads it into the current thread.
    pub(crate) fn load(self) {
        if let Some(f) = &self.fpu {
            // SAFETY: `prepare_context` checked the image as the CPU will.
            unsafe { f.load() };
        }
        // SAFETY: canonical user addresses; the kernel does not use the
        // thread registers.
        unsafe {
            if let Some(t) = self.tls {
                hal::user::set_tls(t);
            }
            if let Some(t) = self.tls2 {
                hal::user::set_tls2(t);
            }
        }
    }
}

/// Checks `mc` and loads it into `frame` and the current thread (`sigreturn`,
/// `setcontext`). On an error nothing has changed.
pub(crate) fn set_context(
    frame: &mut TrapFrame,
    mc: &abi::ucontext::Mcontext,
) -> Result<(), Errno> {
    prepare_context(frame, mc)?.load();
    Ok(())
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

/// The thread to take `sig`, sent to `p` (FreeBSD's `sigtd`): the
/// current thread if it is `p`'s and does not block it, else the first
/// that does not, else the first. `None` for a process with no threads.
fn sigtd(p: &Process, sig: u32) -> Option<Arc<UserThread>> {
    let mine = UserThread::current()
        .filter(|t| ptr::eq(t.process().as_ref(), p) && !t.sig.lock().mask.has(sig));
    if mine.is_some() {
        return mine;
    }
    let mut pick = None;
    p.each_thread(|t| {
        if t.sig.lock().mask.has(sig) {
            return true;
        }
        pick = Some(Arc::clone(t));
        false
    });
    pick.or_else(|| p.first_thread())
}

/// Drops every pending `sig` sent to `p` or to any of its threads
/// (FreeBSD's `sigqueue_delete_proc`); `st` is `p`'s, locked.
fn discard_everywhere(p: &Process, st: &mut Signals, sig: u32) {
    st.queue.discard(sig);
    p.each_thread(|t| {
        t.sig.lock().queue.discard(sig);
        true
    });
}

/// Sets `sig`'s action in `p` (`sigaction`); pending signals it now
/// ignores are dropped, the process's and its threads'.
pub(crate) fn set_action(p: &Process, sig: u32, a: Sigaction) -> Sigaction {
    let mut st = p.signals.lock();
    let old = st.action(sig);
    st.set_action(sig, a);
    if st.ignored(sig) {
        discard_everywhere(p, &mut st, sig);
    }
    old
}

/// Sends `info.si_signo` to `p` (FreeBSD's `tdsendsignal` without a
/// thread): it is queued for the process, and [`sigtd`]'s pick is
/// interrupted. Fails only when a real-time signal's queue is full.
pub(crate) fn post(p: &Arc<Process>, info: Siginfo) -> Result<(), Errno> {
    send(p, None, info)
}

/// Sends `info.si_signo` to thread `t` (`tdsendsignal` with a thread:
/// `thr_kill`, faults, `SIGPIPE`): it is queued for that thread.
pub(crate) fn post_thread(t: &Arc<UserThread>, info: Siginfo) -> Result<(), Errno> {
    send(t.process(), Some(t), info)
}

/// Sends a signal (see [`post`]) and does at once what cannot wait:
/// `SIGCONT` continues a stopped process and cancels pending stops; a stop
/// cancels a pending `SIGCONT`; `SIGKILL` ends a stop. A signal the
/// process ignores is dropped, and one to a zombie succeeds doing nothing.
fn send(p: &Arc<Process>, to: Option<&Arc<UserThread>>, info: Siginfo) -> Result<(), Errno> {
    if p.is_zombie() {
        return Ok(());
    }
    let sig = info.si_signo.cast_unsigned();
    crate::event::signal_sent(p, sig);
    let prop = prop(sig);
    // Decided before taking the signal lock (it reads the process tree).
    let orphaned = prop & TTYSTOP != 0 && crate::proc::pgrp_orphaned(p.pgid());
    let (continued, interrupt) = {
        let mut st = p.signals.lock();
        if prop & CONT != 0 {
            for s in [SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU] {
                discard_everywhere(p, &mut st, s);
            }
        }
        if prop & STOP != 0 {
            discard_everywhere(p, &mut st, SIGCONT);
        }
        // SIGCONT continues even when caught, blocked, or ignored; SIGKILL
        // ends a stop to end the process.
        let continued = (sig == SIGCONT || sig == SIGKILL) && p.resume(sig == SIGCONT);
        if (st.ignored(sig) && sig != SIGKILL) || (orphaned && st.action(sig).sa_handler == SIG_DFL)
        {
            (continued, None)
        } else {
            let target = match to {
                Some(t) => {
                    t.sig.lock().queue.add(info)?;
                    Some(Arc::clone(t))
                }
                None => {
                    st.queue.add(info)?;
                    sigtd(p, sig)
                }
            };
            let stopped = p.job.lock().stopped != 0;
            let interrupt =
                target.filter(|t| !t.sig.lock().mask.has(sig) && (!stopped || sig == SIGKILL));
            (continued, interrupt)
        }
    };
    if continued {
        notify_parent(p, code::CLD_CONTINUED, SIGCONT.cast_signed());
    }
    if let Some(t) = interrupt {
        t.interrupt(AST_SIGNAL);
    }
    Ok(())
}

/// Process-directed signals in `blocked`, which a thread no longer takes
/// (it blocked them or left), go to another thread that can (FreeBSD's
/// `reschedule_signals`).
fn reschedule(p: &Process, blocked: Sigset) {
    let pending = p.signals.lock().pending().intersect(blocked);
    if pending.is_empty() {
        return;
    }
    for sig in 1..=SIG_MAXSIG {
        if pending.has(sig)
            && let Some(t) = sigtd(p, sig)
            && !t.sig.lock().mask.has(sig)
        {
            t.interrupt(AST_SIGNAL);
        }
    }
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

/// Whether `p` ignores `sig` or the current thread blocks it (a terminal
/// then does not send it, FreeBSD's `tty_wait_background`).
pub(crate) fn ignores_or_blocks(p: &Process, sig: u32) -> bool {
    p.signals.lock().action(sig).sa_handler == SIG_IGN
        || UserThread::with_current(|t| t.sig.lock().mask.has(sig)).unwrap_or(false)
}

/// Whether `p`'s parent asked not to keep zombies (`SIGCHLD` ignored, or
/// `SA_NOCLDWAIT`).
pub(crate) fn parent_reaps_nothing(parent: &Process) -> bool {
    let a = parent.signals.lock().action(SIGCHLD);
    a.sa_handler == SIG_IGN || a.sa_flags & SA_NOCLDWAIT != 0
}

/// What thread `t`'s `sigfastblock` word blocks now (empty without one).
fn fastblocked(t: &UserThread) -> Sigset {
    let ptr = t.sig.lock().fastblock;
    if ptr == 0 {
        return Sigset::default();
    }
    match UserPtr::<u32>::new(ptr).read() {
        Ok(w) if w & !SIGFASTBLOCK_FLAGS != 0 => fastblock_mask(),
        _ => Sigset::default(),
    }
}

/// Marks `t`'s `sigfastblock` word: a signal waits for libthr's unblock.
fn set_fastblock_pending(t: &UserThread) {
    let ptr = t.sig.lock().fastblock;
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

/// Takes the next signal that needs action from those sent to thread `t`
/// or its process (the thread's first), dropping ignored ones on the way.
/// `None` when nothing is deliverable.
fn next(t: &UserThread) -> Option<Next> {
    let p = t.process();
    let fast = fastblocked(t);
    let orphaned = crate::proc::pgrp_orphaned(p.pgid());
    let mut st = p.signals.lock();
    let mut ts = t.sig.lock();
    loop {
        let ready = st.pending().union(ts.queue.pending()).minus(ts.mask);
        let Some(sig) = ready.minus(fast).first() else {
            if !ready.is_empty() {
                drop(ts);
                drop(st);
                set_fastblock_pending(t);
            }
            return None;
        };
        let info = ts
            .queue
            .take(sig)
            .or_else(|| st.queue.take(sig))
            .unwrap_or_default();
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

/// Stops the current thread `t`'s process for `sig`: every thread parks
/// until it is continued (or killed); the parent hears once all have.
fn stop(s: &Sleepable, t: &UserThread, sig: u32) {
    let p = t.process();
    p.begin_stop(t, sig);
    p.park(s, t);
}

/// The current thread at the user boundary: it parks while the process
/// is stopped or another thread single-threads for exec, and leaves if
/// another single-threads to exit (FreeBSD's `thread_suspend_check(0)`).
/// The caller holds no references (see [`leave`]).
pub(crate) fn suspend_check(s: &Sleepable) {
    loop {
        let leave_now = UserThread::with_current(|t| {
            let p = t.process();
            match p.suspension(t) {
                Suspension::None => Some(false),
                Suspension::Single(Mode::Exit) => Some(true),
                Suspension::Stop | Suspension::Single(Mode::Boundary) => {
                    p.park(s, t);
                    None
                }
            }
        });
        match leave_now {
            Some(None) => {}
            Some(Some(true)) => leave(s),
            _ => return,
        }
    }
}

/// Ends the current thread, which another's single-threading sent away.
/// Nothing on the caller's stack may hold a reference: it never unwinds.
fn leave(s: &Sleepable) -> ! {
    if let Some(t) = UserThread::current() {
        crate::umtx::thread_exit(s, &t);
        t.process().remove_thread(&t, false);
        Process::thread_exit(t);
    }
    sched::exit()
}

/// After thread `t` left its process: the signals sent to it go, and
/// those sent to the process that it did not block go to another thread
/// (FreeBSD's `tdsigcleanup`).
pub(crate) fn thread_gone(t: &UserThread) {
    let all = Sigset {
        bits: [u32::MAX; SIG_WORDS],
    };
    let unblocked = {
        let mut ts = t.sig.lock();
        ts.queue = Queue::default();
        let u = all.minus(ts.mask);
        ts.mask = all;
        u
    };
    reschedule(t.process(), unblocked);
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
        if bits & AST_AFFINITY != 0 {
            sched::move_to(UserThread::with_current(UserThread::bound_cpu).flatten());
        }
        if bits & (AST_IO_ON | AST_IO_OFF) != 0 {
            machdep::set_io_access(frame, bits & AST_IO_ON != 0);
            rewritten = true;
        }
        if bits & (AST_SIGNAL | AST_SUSPEND) != 0 {
            rewritten |= deliver(&s, frame);
        }
        let _ = hal::irq::disable();
    }
    rewritten
}

/// Gives the current thread access to the I/O ports (`KDENABIO`), or
/// takes it away (`KDDISABIO`), as it returns to user mode: on amd64,
/// IOPL 3 in its flags (FreeBSD sets `PSL_IOPL` in `td_frame`); aarch64
/// has no ports.
///
/// # Errors
///
/// `EPERM` unless root (`PRIV_IO`). There is no securelevel, as
/// FreeBSD's default -1 would have it.
pub(crate) fn io_access(on: bool) -> Result<(), Errno> {
    if on && !Process::current().is_some_and(|p| p.cred().is_root()) {
        return Err(Errno::EPERM);
    }
    sched::current().request_ast(if on { AST_IO_ON } else { AST_IO_OFF });
    Ok(())
}

/// Acts on every deliverable signal, after any suspension. Returns
/// whether a handler's frame was built.
fn deliver(s: &Sleepable, frame: &mut TrapFrame) -> bool {
    let me = sched::current();
    let mut built = false;
    loop {
        // Cleared before looking, so a signal posted after the look
        // interrupts the next sleep.
        me.clear_interrupted();
        suspend_check(s);
        let Some(t) = UserThread::current() else {
            return false;
        };
        match next(&t) {
            None => {
                // A `sigsuspend` whose signal reached no handler gets its
                // mask back.
                let mut ts = t.sig.lock();
                if let Some(m) = ts.saved_mask.take() {
                    ts.mask = m;
                }
                return built;
            }
            // Nothing on this stack may keep a reference.
            Some(Next::Kill(sig)) => {
                drop(t);
                kill_self(s, sig)
            }
            Some(Next::Stop(sig)) => stop(s, &t, sig),
            Some(Next::Handle(sig, info, action)) => {
                if !handle(s, &t, frame, sig, &info, &action) {
                    drop(t);
                    kill_self(s, SIGILL);
                }
                built = true;
            }
        }
    }
}

/// Ends the current process by `sig` (FreeBSD's `sigexit`; there are no
/// core dumps). The caller holds no references: nothing on a stack that
/// never unwinds may keep them.
pub(super) fn kill_self(s: &Sleepable, sig: u32) -> ! {
    Process::exit_current(s, Exit::Signal(sig))
}

/// Builds `sig`'s handler frame on the user stack and updates thread
/// `t`'s mask (FreeBSD's `postsig`). Returns `false` if the frame cannot
/// be written: the caller ends the process with `SIGILL` (dropping its
/// references first, see [`kill_self`]).
#[must_use]
fn handle(
    _s: &Sleepable,
    t: &UserThread,
    frame: &mut TrapFrame,
    sig: u32,
    info: &Siginfo,
    action: &Sigaction,
) -> bool {
    let p = t.process();
    let (mask, altstack) = {
        let mut ts = t.sig.lock();
        (ts.saved_mask.take().unwrap_or(ts.mask), ts.altstack)
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
    let blocked = {
        let mut st = p.signals.lock();
        let mut ts = t.sig.lock();
        let old = ts.mask;
        let mut m = old.union(action.sa_mask);
        if action.sa_flags & SA_NODEFER == 0 {
            m.add(sig);
        }
        ts.mask = m.minus(cant_mask());
        if action.sa_flags & SA_RESETHAND != 0 && sig != SIGILL && sig != SIGTRAP {
            st.actions[(sig - 1) as usize] = Sigaction::default();
        }
        ts.mask.minus(old)
    };
    reschedule(p, blocked);
    true
}

/// After the current thread `t` unblocked signals or changed its pending
/// set: a deliverable one is looked at before user mode, and ends the next
/// interruptible sleep (a signal posted while blocked interrupted
/// nothing).
pub(crate) fn recheck(t: &UserThread) {
    let deliverable = {
        let st = t.process().signals.lock();
        let ts = t.sig.lock();
        !st.pending()
            .union(ts.queue.pending())
            .minus(ts.mask)
            .is_empty()
    };
    if deliverable {
        sched::current_thread().interrupt(AST_SIGNAL);
    }
}

/// Sets the current thread `t`'s mask to `mask` (less what cannot be
/// blocked), as `sigprocmask` and contexts do; returns the old one.
/// Process-directed signals it now blocks go to another thread.
pub(crate) fn set_mask(t: &UserThread, mask: Sigset) -> Sigset {
    let (old, new) = {
        let mut ts = t.sig.lock();
        let old = ts.mask;
        ts.mask = mask.minus(cant_mask());
        (old, ts.mask)
    };
    reschedule(t.process(), new.minus(old));
    recheck(t);
    old
}

/// Why an interrupted sleep ended, if a signal must be handled
/// (FreeBSD's `sleepq_catch_signals`): `EINTR`, or `ERESTART` for a
/// handler with `SA_RESTART`; also `EINTR` when another thread
/// single-threads to exit, and `ERESTART` to exec, so this one gets to
/// the boundary. `None` means sleep on: what interrupted it was blocked,
/// ignored, or a stop (done here, in place).
pub(crate) fn interruption(s: &Sleepable) -> Option<Errno> {
    interrupted_by(s).0
}

/// [`interruption`], and whether the process stopped (and has been
/// continued) meanwhile.
fn interrupted_by(s: &Sleepable) -> (Option<Errno>, bool) {
    let Some(t) = UserThread::current() else {
        return (None, false);
    };
    let p = t.process();
    let me = sched::current();
    let mut stopped = false;
    loop {
        me.clear_interrupted();
        match p.suspension(&t) {
            Suspension::None => {}
            Suspension::Single(Mode::Exit) => return (Some(Errno::EINTR), stopped),
            Suspension::Single(Mode::Boundary) => return (Some(Errno::ERESTART), stopped),
            Suspension::Stop => {
                p.park(s, &t);
                stopped = true;
                continue;
            }
        }
        let fast = fastblocked(&t);
        let orphaned = crate::proc::pgrp_orphaned(p.pgid());
        let mut st = p.signals.lock();
        let mut ts = t.sig.lock();
        let ready = st.pending().union(ts.queue.pending()).minus(ts.mask);
        let Some(sig) = ready.minus(fast).first() else {
            return (None, stopped);
        };
        let action = st.action(sig);
        if st.ignored(sig) {
            ts.queue.discard(sig);
            st.queue.discard(sig);
            continue;
        }
        if action.sa_handler == SIG_DFL && prop(sig) & STOP != 0 {
            if ts.queue.take(sig).is_none() {
                st.queue.take(sig);
            }
            drop(ts);
            drop(st);
            if !(prop(sig) & TTYSTOP != 0 && orphaned) {
                stop(s, &t, sig);
                stopped = true;
            }
            continue;
        }
        // Delivery happens on the way out; the thread looks again there.
        me.request_ast(AST_SIGNAL);
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
pub(crate) fn swap_mask(t: &UserThread, mask: Sigset) {
    {
        let mut ts = t.sig.lock();
        ts.saved_mask = Some(ts.mask);
    }
    set_mask(t, mask);
}

/// Undoes [`swap_mask`] when the call returns without a handler to run.
pub(crate) fn restore_mask(t: &UserThread) {
    let saved = t.sig.lock().saved_mask.take();
    if let Some(m) = saved {
        set_mask(t, m);
    }
}

/// Whether a signal from POSIX timer `id` waits for delivery to `p` or
/// one of its threads (`KSI_ONQ` of the timer's `ksiginfo`).
pub(crate) fn timer_queued(p: &Process, id: i32) -> bool {
    let mine = |i: &Siginfo| i.si_code == code::SI_TIMER && i.si_trapno == id;
    if p.signals.lock().queue.info.iter().any(mine) {
        return true;
    }
    p.threads()
        .iter()
        .any(|t| t.sig.lock().queue.info.iter().any(mine))
}

/// Takes a pending signal in `set` sent to thread `t` or its process (the
/// thread's first), if any (`sigtimedwait`).
pub(crate) fn take_from(t: &UserThread, set: Sigset) -> Option<Siginfo> {
    let mut st = t.process().signals.lock();
    let mut ts = t.sig.lock();
    let sig = st
        .pending()
        .union(ts.queue.pending())
        .intersect(set)
        .first()?;
    ts.queue.take(sig).or_else(|| st.queue.take(sig))
}
