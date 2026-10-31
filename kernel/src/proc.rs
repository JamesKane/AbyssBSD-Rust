// SPDX-License-Identifier: BSD-2-Clause
//! Processes: an address space and the threads that run in it, in a tree
//! of parents and children (FreeBSD's `struct proc`).
//!
//! A process has a pid from the process table, a process group and
//! session, its signal dispositions and the signals sent to it, resource
//! limits, a randomized base for `mmap` placement, its descriptor table,
//! its working directory, and its threads ([`UserThread`]), which keep
//! what is each one's own.
//!
//! Suspension stops threads at the user boundary, or in place in an
//! interruptible sleep (FreeBSD's `thread_suspend_check`): for a stop
//! signal (every thread parks until the process continues; the parent
//! hears once the last one has), and for single-threading
//! (`thread_single`), which exit and exec use to have the calling thread
//! alone. To exit, the other threads leave at the boundary, and sleeps
//! end with `EINTR`; for exec they park at the boundary (sleeps end with
//! `ERESTART`), and leave once the new image is in, or go on if it fails.
//!
//! A child made by `pdfork` has a process descriptor (a [`ProcDesc`] file)
//! while one is open: its exit sends its parent no `SIGCHLD`, and only a
//! wait naming its pid or group collects it. Closing the last descriptor
//! reaps it if it has exited, and otherwise hands it to init and, unless
//! it is a daemon (`PD_DAEMON`), kills it.
//!
//! The tree is one map under one lock (FreeBSD's `proctree_lock`): each
//! process's parent, and each parent's children, which it holds until it
//! reaps them. A process that exits becomes a zombie holding its status;
//! its children go to init (pid 1), and its parent's waiters wake. A
//! parent's `wait4` sleeps on a generation count its children's changes
//! bump, so it never takes the tree lock inside a wait-queue condition.
//!
//! Behavior reference: sys/kern/kern_fork.c, sys/kern/kern_exit.c,
//! sys/kern/kern_exec.c, sys/kern/sys_procdesc.c.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::resource::{RLIM_INFINITY, Rlimit, WNOHANG, WNOWAIT, exit_status, rlimit};
use abi::resource::{WCONTINUED, WTRAPPED, WUNTRACED};
use abi::signal::code::{CLD_EXITED, CLD_KILLED};
use sched::lock::{LockClass, Mutex};
use sched::{Sleepable, WaitQueue};
use sync::{PreemptLock, SpinLock};
use vfs::Vnode;

use crate::arch;
use crate::arch::trap::TrapFrame;
use crate::cred::{Priv, Ucred};
use crate::exec::Layout;
use crate::fs::fdtable::FdTable;
use crate::mm::space::{self, AddressSpace};
use crate::signal::{Signals, ThreadSignals};
use crate::uthread::{Name, UserThread};

/// The highest pid (`PID_MAX`); allocation wraps to 2 past it.
pub(crate) const PID_MAX: i32 = 99_999;
/// init's pid, which inherits orphans.
pub(crate) const INIT: i32 = 1;

/// A process's [`Layout`], without a lock: only the process's own thread
/// writes it (at exec) or reads it (`mmap`, signal delivery), and `mmap`
/// reads it on every call.
#[derive(Debug)]
struct SharedLayout {
    anon_loc: AtomicU64,
    stack_top: AtomicU64,
    shared_page: AtomicU64,
    osrel: AtomicI32,
    fctl0: AtomicU32,
    aslr: AtomicBool,
}

impl SharedLayout {
    fn new(l: Layout) -> Self {
        Self {
            anon_loc: AtomicU64::new(l.anon_loc),
            stack_top: AtomicU64::new(l.stack_top),
            shared_page: AtomicU64::new(l.shared_page),
            osrel: AtomicI32::new(l.osrel),
            fctl0: AtomicU32::new(l.fctl0),
            aslr: AtomicBool::new(l.aslr),
        }
    }

    fn get(&self) -> Layout {
        Layout {
            anon_loc: self.anon_loc.load(Ordering::Relaxed),
            stack_top: self.stack_top.load(Ordering::Relaxed),
            shared_page: self.shared_page.load(Ordering::Relaxed),
            osrel: self.osrel.load(Ordering::Relaxed),
            fctl0: self.fctl0.load(Ordering::Relaxed),
            aslr: self.aslr.load(Ordering::Relaxed),
        }
    }

    fn set(&self, l: Layout) {
        self.anon_loc.store(l.anon_loc, Ordering::Relaxed);
        self.stack_top.store(l.stack_top, Ordering::Relaxed);
        self.shared_page.store(l.shared_page, Ordering::Relaxed);
        self.osrel.store(l.osrel, Ordering::Relaxed);
        self.fctl0.store(l.fctl0, Ordering::Relaxed);
        self.aslr.store(l.aslr, Ordering::Relaxed);
    }
}

static FILEDESC_LOCK: LockClass = LockClass::new("filedesc", 200);

/// Whether process group `pgid` is orphaned: no member has a parent in
/// another group of the same session (POSIX; FreeBSD's `pg_jobc == 0`).
/// Stops from the terminal do not stop an orphaned group.
pub(crate) fn pgrp_orphaned(pgid: i32) -> bool {
    !all().iter().any(|m| {
        m.pgid() == pgid
            && !m.exited.load(Ordering::Acquire)
            && m.parent()
                .is_some_and(|parent| parent.pgid() != pgid && parent.sid() == m.sid())
    })
}

/// Every live process. The only way to look through the table: a process
/// whose last reference goes takes the table's lock to leave it, so no
/// reference may be dropped while it is held. Here none is (each moves
/// into the list); callers drop theirs after.
pub(crate) fn all() -> Vec<Arc<Process>> {
    PROCS.lock().values().filter_map(Weak::upgrade).collect()
}

/// Live processes by pid.
static PROCS: SpinLock<BTreeMap<i32, Weak<Process>>> = SpinLock::new(BTreeMap::new());
/// The pid handed out last.
static LAST_PID: AtomicI32 = AtomicI32::new(0);

/// Parents and children, and reapers (`procctl(PROC_REAP_*)`): each
/// process has a reaper, init unless a process above it acquired the
/// role, which inherits it when its parent goes.
struct Tree {
    parent: BTreeMap<i32, i32>,
    children: BTreeMap<i32, Vec<Arc<Process>>>,
    /// Each process's reaper, besides init's (`p_reaper`); init when
    /// absent.
    reaper: BTreeMap<i32, i32>,
    /// The pid of the reaper's direct child each process descends from
    /// (`p_reapsubtree`); 1 when absent.
    subtree: BTreeMap<i32, i32>,
    /// Each reaper's descendants, newest first (`p_reaplist`).
    reaplist: BTreeMap<i32, Vec<i32>>,
    /// The reapers besides init (`P_TREE_REAPER`).
    reapers: alloc::collections::BTreeSet<i32>,
    /// Each parent's children a debugger took (`p_orphans`): its `wait`
    /// waits for them.
    orphans: BTreeMap<i32, Vec<i32>>,
}

static TREE: SpinLock<Tree> = SpinLock::new(Tree {
    parent: BTreeMap::new(),
    children: BTreeMap::new(),
    reaper: BTreeMap::new(),
    subtree: BTreeMap::new(),
    reaplist: BTreeMap::new(),
    reapers: alloc::collections::BTreeSet::new(),
    orphans: BTreeMap::new(),
});

impl Tree {
    fn is_reaper(&self, pid: i32) -> bool {
        pid == INIT || self.reapers.contains(&pid)
    }

    fn reaper_of(&self, pid: i32) -> i32 {
        self.reaper.get(&pid).copied().unwrap_or(INIT)
    }

    fn subtree_of(&self, pid: i32) -> i32 {
        self.subtree.get(&pid).copied().unwrap_or(1)
    }

    /// Records `child`, just forked by `parent` (`fork1`): its reaper is
    /// the parent if that is one, else the parent's.
    fn forked(&mut self, parent: i32, child: i32) {
        let r = if self.is_reaper(parent) {
            parent
        } else {
            self.reaper_of(parent)
        };
        self.reaper.insert(child, r);
        let sub = if r == parent && parent != INIT {
            child
        } else {
            self.subtree_of(parent)
        };
        self.subtree.insert(child, sub);
        self.reaplist.entry(r).or_default().insert(0, child);
    }

    /// Takes `pid` off its real parent's orphans (`proc_clear_orphan`).
    fn clear_orphan(&mut self, pid: i32) {
        for list in self.orphans.values_mut() {
            list.retain(|&o| o != pid);
        }
        self.orphans.retain(|_, l| !l.is_empty());
    }

    /// Moves `child` to be `parent`'s child (`proc_reparent`); a traced
    /// one stays its old parent's orphan.
    fn reparent(&mut self, child: &Arc<Process>, parent: i32) {
        self.clear_orphan(child.pid);
        if child.is_traced()
            && let Some(&old) = self.parent.get(&child.pid)
        {
            self.orphans.entry(old).or_default().push(child.pid);
        }
        if let Some(old) = self.parent.insert(child.pid, parent)
            && let Some(kids) = self.children.get_mut(&old)
        {
            kids.retain(|c| c.pid != child.pid);
        }
        self.children
            .entry(parent)
            .or_default()
            .push(Arc::clone(child));
    }

    /// `reaper_abandon_children`: reaper `p`'s descendants go to its own
    /// reaper, its direct children too if it is `exiting` (reaped).
    fn abandon(&mut self, p: i32, exiting: bool) {
        if !self.reapers.remove(&p) {
            return;
        }
        let r1 = self.reaper_of(p);
        let sub = self.subtree_of(p);
        for d in self.reaplist.remove(&p).unwrap_or_default() {
            self.reaper.insert(d, r1);
            self.subtree.insert(d, sub);
            self.reaplist.entry(r1).or_default().insert(0, d);
            if exiting
                && self.parent.get(&d) == Some(&p)
                && let Some(c) = find(d)
            {
                self.reparent(&c, r1);
            }
        }
    }

    /// A zombie is reaped (`proc_reap`): it leaves its reaper's list, and
    /// what it reaped for goes to its own reaper.
    fn reaped(&mut self, p: i32) {
        self.abandon(p, true);
        let r = self.reaper_of(p);
        if let Some(l) = self.reaplist.get_mut(&r) {
            l.retain(|&d| d != p);
        }
        self.reaper.remove(&p);
        self.subtree.remove(&p);
    }
}

/// `procctl(PROC_REAP_ACQUIRE)`: `p` becomes a reaper (`EBUSY` if it is
/// one).
pub(crate) fn reap_acquire(p: &Process) -> Result<(), Errno> {
    let mut tree = TREE.lock();
    if tree.is_reaper(p.pid) {
        return Err(Errno::EBUSY);
    }
    tree.reapers.insert(p.pid);
    Ok(())
}

/// `procctl(PROC_REAP_RELEASE)`: `p` stops being a reaper; init cannot.
pub(crate) fn reap_release(p: &Process) -> Result<(), Errno> {
    let mut tree = TREE.lock();
    if p.pid == INIT || !tree.reapers.contains(&p.pid) {
        return Err(Errno::EINVAL);
    }
    tree.abandon(p.pid, false);
    Ok(())
}

/// One of a reaper's descendants, as `PROC_REAP_GETPIDS` and
/// `PROC_REAP_KILL` see them.
#[derive(Clone)]
pub(crate) struct Descendant {
    pub(crate) process: Arc<Process>,
    pub(crate) subtree: i32,
    /// Its parent is the reaper.
    pub(crate) child: bool,
    /// It is a reaper itself.
    pub(crate) reaper: bool,
}

/// The reaper `p` answers to (itself if it is one), whether it is one,
/// and its descendants, newest first.
pub(crate) fn reaper_view(p: &Process) -> (i32, bool, Vec<Descendant>) {
    let tree = TREE.lock();
    let owned = tree.is_reaper(p.pid);
    let r = if owned { p.pid } else { tree.reaper_of(p.pid) };
    let list = tree
        .reaplist
        .get(&r)
        .map(|l| {
            l.iter()
                .filter_map(|&d| {
                    let process = PROCS.lock().get(&d).and_then(Weak::upgrade)?;
                    Some(Descendant {
                        process,
                        subtree: tree.subtree_of(d),
                        child: tree.parent.get(&d) == Some(&r),
                        reaper: tree.is_reaper(d),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (r, owned, list)
}

/// `pid`'s first child, newest first as FreeBSD lists `p_children`.
pub(crate) fn first_child(pid: i32) -> Option<i32> {
    TREE.lock()
        .children
        .get(&pid)
        .and_then(|k| k.last())
        .map(|c| c.pid)
}

/// `pid`'s descendants, each before its own (the walk of
/// `ktrsetchildren`); `pid` itself is not among them. Collected with the
/// tree locked, dropped after.
pub(crate) fn descendants(pid: i32) -> Vec<Arc<Process>> {
    let tree = TREE.lock();
    let mut out = Vec::new();
    let mut stack = alloc::vec![pid];
    while let Some(p) = stack.pop() {
        if let Some(kids) = tree.children.get(&p) {
            for c in kids.iter().rev() {
                out.push(Arc::clone(c));
                stack.push(c.pid);
            }
        }
    }
    drop(tree);
    out
}

/// The current thread's `vfork` child, if it left one to wait for
/// (`TDP_RFPPWAIT`): it waits until the child execs or exits.
pub(crate) fn vfork_wait(s: &Sleepable) {
    let Some(t) = UserThread::current() else {
        return;
    };
    let Some(child) = t.vfork_child.lock().take() else {
        return;
    };
    t.process()
        .child_wait
        .wait_until(s, || !child.vforked.load(Ordering::Acquire));
}

/// Traced `child` becomes debugger `debugger`'s child, its real parent's
/// orphan (`proc_reparent` for `PT_ATTACH` and a followed fork).
pub(crate) fn reparent_traced(child: &Arc<Process>, debugger: i32) {
    TREE.lock().reparent(child, debugger);
}

/// `proc_realparent`: the real parent of traced `child` if it is still
/// there, else its reaper.
fn real_parent(tree: &Tree, child: &Process) -> i32 {
    let oppid = child.trace.lock().oppid;
    let alive = find(oppid).is_some_and(|p| !p.is_zombie());
    if alive {
        oppid
    } else {
        tree.reaper_of(child.pid)
    }
}

/// `child`, no longer traced, goes back to its real parent (or its
/// reaper) (`PT_DETACH`).
pub(crate) fn reparent_back(child: &Arc<Process>) {
    let mut tree = TREE.lock();
    let to = real_parent(&tree, child);
    child.trace.lock().oppid = 0;
    tree.reparent(child, to);
}

/// The reaper `pid` answers to, and its subtree (`ki_reaper`,
/// `ki_reapsubtree`).
pub(crate) fn reaper_of(pid: i32) -> (i32, i32) {
    let tree = TREE.lock();
    (tree.reaper_of(pid), tree.subtree_of(pid))
}

/// The pid given last (`kern.lastpid`).
pub(crate) fn last_pid() -> i32 {
    LAST_PID.load(Ordering::Relaxed)
}

/// The next free pid after the last one, recorded for `p`.
fn alloc_pid(p: &Weak<Process>) -> i32 {
    let mut procs = PROCS.lock();
    let mut pid = LAST_PID.load(Ordering::Relaxed);
    loop {
        pid = if pid >= PID_MAX { 2 } else { pid + 1 };
        if !procs.contains_key(&pid) {
            break;
        }
    }
    LAST_PID.store(pid, Ordering::Relaxed);
    procs.insert(pid, Weak::clone(p));
    pid
}

/// The live process with pid `pid`.
pub(crate) fn find(pid: i32) -> Option<Arc<Process>> {
    PROCS.lock().get(&pid)?.upgrade()
}

/// What `wait` found: a child's pid and its status, and whether that is
/// a stop for this debugger (`CLD_TRAPPED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Waited {
    pub(crate) pid: i32,
    pub(crate) status: i32,
    pub(crate) trapped: bool,
    /// An exited child's usage and its children's; none for a stop or a
    /// continue (`report_alive_proc` leaves the `rusage` zero).
    pub(crate) usage: Option<(crate::kinfo::Usage, crate::kinfo::Usage)>,
}

impl Waited {
    const fn new(pid: i32, status: i32) -> Self {
        Self {
            pid,
            status,
            trapped: false,
            usage: None,
        }
    }

    fn exited(c: &Process, status: i32) -> Self {
        Self {
            usage: Some(crate::kinfo::exit_usage(c)),
            ..Self::new(c.pid, status)
        }
    }
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    /// `_exit(code)`.
    Code(i32),
    /// Killed by a signal.
    Signal(u32),
    /// Killed by a signal, having dumped core (`WCOREFLAG`).
    Dumped(u32),
}

impl Exit {
    /// The status `wait4` reports.
    const fn status(self) -> i32 {
        match self {
            Self::Code(c) => exit_status(c),
            Self::Signal(sig) => (sig & 0x7f).cast_signed(),
            Self::Dumped(sig) => (sig & 0x7f).cast_signed() | 0x80,
        }
    }

    const fn from_status(st: i32) -> Self {
        if st & 0x7f == 0 {
            // The whole code, as `_exit` gave it (`WEXITSTATUS` would keep
            // 8 bits).
            Self::Code(st >> 8)
        } else if st & 0x80 != 0 {
            Self::Dumped((st & 0x7f).cast_unsigned())
        } else {
            Self::Signal((st & 0x7f).cast_unsigned())
        }
    }
}

/// Job control and suspension: whether the process is stopped, what its
/// parent's `wait4` has yet to hear, and which threads are parked.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Job {
    /// The signal that stopped the process, 0 while it runs (FreeBSD's
    /// `P_STOPPED_SIG` and `p_xsig`).
    pub(crate) stopped: u32,
    /// A stop `wait4(WUNTRACED)` has not reported (FreeBSD's `P_WAITED`
    /// clear).
    pub(crate) stop_report: bool,
    /// A continue `wait4(WCONTINUED)` has not reported (`P_CONTINUED`).
    pub(crate) cont_report: bool,
    /// The parent has heard of the stop in progress: every thread parked.
    stop_told: bool,
    /// The stop is for the debugger (`P_STOPPED_TRACE`).
    pub(crate) trace: bool,
    /// Single-threading in progress (`P_STOPPED_SINGLE`).
    single: Option<Single>,
    /// Threads parked at the boundary or in a sleep (`p_suspcount`).
    parked: usize,
}

/// Single-threading (FreeBSD's `thread_single` modes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The other threads leave (`SINGLE_EXIT`).
    Exit,
    /// The other threads park at the user boundary (`SINGLE_BOUNDARY`).
    Boundary,
}

#[derive(Clone, Copy, Debug)]
struct Single {
    /// The thread that wants to be alone.
    by: i32,
    mode: Mode,
}

/// What holds a thread back at the boundary or in a sleep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Suspension {
    /// Nothing.
    None,
    /// The process is stopped: park, in place in a sleep.
    Stop,
    /// Another thread is single-threading: at the boundary, leave (`Exit`)
    /// or park (`Boundary`); a sleep ends with `EINTR` or `ERESTART` to get
    /// there.
    Single(Mode),
}

/// `kern.maxfilesperproc`: the most `RLIMIT_NOFILE` may be.
pub(crate) const MAXFILESPERPROC: u64 = 116_964;
/// `kern.maxprocperuid`: the most `RLIMIT_NPROC` may be.
pub(crate) const MAXPROCPERUID: u64 = 6_000;

/// The CPU time a process may use before the next `SIGXCPU`, in seconds
/// (`p_cpulimit`), checked once a second while one is finite.
static CPU_WATCH: sync::Once<core::pin::Pin<Box<crate::softclock::Callout>>> = sync::Once::new();

/// `RLIMIT_CPU`'s soft limit of `p` became `secs` (`p_cpulimit`): the
/// once-a-second check starts if it was not running.
pub(crate) fn cpu_limit_set(p: &Process, secs: u64) {
    p.cpulimit.store(secs, Ordering::Relaxed);
    if secs != RLIM_INFINITY {
        if CPU_WATCH.get().is_none() {
            let _ = CPU_WATCH.set(crate::softclock::Callout::new());
        }
        if !CPU_ARMED.swap(true, Ordering::AcqRel)
            && let Some(c) = CPU_WATCH.get()
        {
            c.as_ref().reset(1_000_000_000, cpu_watch, 0);
        }
    }
}

static CPU_ARMED: AtomicBool = AtomicBool::new(false);

/// `lim_cb` for every process with a finite CPU limit: past its hard
/// limit it is killed; past `p_cpulimit` it gets `SIGXCPU`, and five more
/// seconds before the next.
fn cpu_watch(_: &Sleepable, _: usize) {
    let mut any = false;
    for p in all() {
        let lim = p.cpulimit.load(Ordering::Relaxed);
        if lim == RLIM_INFINITY || p.exited.load(Ordering::Relaxed) {
            continue;
        }
        any = true;
        let used = crate::time::ticks_to_ns(p.runtime()) / 1_000_000_000;
        if used <= lim {
            continue;
        }
        let max = p.limits.lock()[rlimit::CPU as usize].rlim_max;
        let sig = if used >= max {
            kprintln!(
                "pid {} ({}), jid {}, uid {}: exceeded maximum CPU limit",
                p.pid,
                p.with_comm(|c| alloc::string::String::from_utf8_lossy(c).into_owned()),
                p.cred().jail_id(),
                p.cred().uid
            );
            abi::signal::SIGKILL
        } else {
            if lim < max {
                p.cpulimit.store(lim + 5, Ordering::Relaxed);
            }
            abi::signal::SIGXCPU
        };
        let _ = crate::signal::post(
            &p,
            crate::signal::user_info(sig, 0, abi::signal::code::SI_KERNEL),
        );
    }
    CPU_ARMED.store(any, Ordering::Release);
    if any && let Some(c) = CPU_WATCH.get() {
        c.as_ref().reset(1_000_000_000, cpu_watch, 0);
    }
}

/// FreeBSD's default limits, as `ulimit -a` shows them on a 15.1 system,
/// except the stack, which matches what exec maps.
pub(crate) fn default_limits() -> [Rlimit; rlimit::NLIMITS] {
    let inf = Rlimit {
        rlim_cur: RLIM_INFINITY,
        rlim_max: RLIM_INFINITY,
    };
    let mut l = [inf; rlimit::NLIMITS];
    l[rlimit::STACK as usize] = Rlimit {
        rlim_cur: crate::exec::DFLSSIZ,
        rlim_max: crate::exec::MAXSSIZ,
    };
    l[rlimit::DATA as usize] = Rlimit {
        rlim_cur: crate::exec::MAXDSIZ,
        rlim_max: crate::exec::MAXDSIZ,
    };
    l[rlimit::CORE as usize] = inf;
    l[rlimit::NOFILE as usize] = Rlimit {
        rlim_cur: MAXFILESPERPROC,
        rlim_max: MAXFILESPERPROC,
    };
    l[rlimit::NPROC as usize] = Rlimit {
        rlim_cur: MAXPROCPERUID,
        rlim_max: MAXPROCPERUID,
    };
    l
}

/// A process.
pub(crate) struct Process {
    pid: i32,
    space: SpinLock<Arc<AddressSpace>>,
    /// This process borrows its parent's address space (`vfork`) until it
    /// execs or exits; the parent sleeps meanwhile.
    vforked: AtomicBool,
    /// Where exec put the randomized parts of the image.
    layout: SharedLayout,
    pub(crate) signals: SpinLock<Signals>,
    pub(crate) limits: PreemptLock<[Rlimit; rlimit::NLIMITS]>,
    /// The `exterrctl` buffer, 0 for none.
    pub(crate) exterr: AtomicU64,
    pgid: AtomicI32,
    sid: AtomicI32,
    /// Its session's shared state (`p_session`).
    session: SpinLock<Arc<Session>>,
    /// The exit status, as `wait` reports it, valid once `exited` is set.
    status: AtomicI32,
    exited: AtomicBool,
    /// Set with the tree locked once exit has decided whom to tell (a
    /// zombie); a closing process descriptor decides by it.
    zombie: AtomicBool,
    /// A process descriptor is open for it (changes with the tree locked).
    procdesc: AtomicBool,
    /// Knotes watching it: `EVFILT_PROC`, `EVFILT_SIGNAL` (`p_klist`).
    pub(crate) klist: event::Notify,
    /// Pollers and knotes of its process descriptor (`pd_selinfo`).
    pub(crate) pd_sel: event::Notify,
    /// The pid of the child it forked last (`pd_last_child`).
    last_child: AtomicI32,
    /// Its interval and POSIX timers, made on first use (`p_itimers`).
    timers: sync::Once<Box<crate::timers::Timers>>,
    /// Kernel code waiting for this process to exit (it may have no
    /// parent).
    waiters: WaitQueue,
    /// Bumped when a child exits, stops, continues, or stops borrowing
    /// the address space; `wait4` and `vfork` sleep on it.
    child_gen: AtomicU64,
    child_wait: WaitQueue,
    /// Its threads, in creation order (FreeBSD's `p_threads`). A thread
    /// joins before it runs and leaves as it exits.
    threads: SpinLock<Vec<Arc<UserThread>>>,
    pub(crate) job: SpinLock<Job>,
    /// Where parked threads wait.
    pub(crate) stop_wait: WaitQueue,
    /// Where a single-threading thread waits for the others.
    single_wait: WaitQueue,
    /// It has had more than one thread (FreeBSD's `P_HADTHREADS`).
    had_threads: AtomicBool,
    /// CPU time of its threads that have left, in platform time units.
    dead_runtime: AtomicU64,
    /// Their voluntary and involuntary context switches (`p_ru`'s
    /// `ru_nvcsw` and `ru_nivcsw`, which `thread_exit` adds to).
    dead_switches: [AtomicU64; 2],
    /// The shortest timeout a umtx wait takes, in nanoseconds
    /// (`UMTX_OP_SET_MIN_TIMEOUT`, FreeBSD's `p_umtx_min_timeout`).
    pub(crate) umtx_min_timeout: AtomicU64,
    /// Its nice value, -20 to 20 (`p_nice`), which `setpriority` sets.
    pub(crate) nice: AtomicI32,
    /// Open descriptors. Shared by the process's threads (when there are
    /// several), so behind a sleepable lock (FreeBSD's `fd_sx`).
    /// Shared with other processes after `rfork` without `RFFDG`.
    pub(crate) files: Arc<Mutex<FdTable>>,
    /// The working directory.
    pub(crate) cwd: PreemptLock<vfs::VnodeUse>,
    /// The root directory, if not the system's (`chroot`, a jail's).
    pub(crate) root: PreemptLock<Option<vfs::VnodeUse>>,
    /// The file creation mask.
    pub(crate) umask: AtomicU16,
    /// What `ps` shows of the program it runs.
    image: SpinLock<Image>,
    /// When it was created, in nanoseconds since boot.
    started: u64,
    /// It has called `execve` (FreeBSD's `P_EXEC`).
    execed: AtomicBool,
    /// Its credential; inherited.
    cred: PreemptLock<Arc<Ucred>>,
    /// The program it runs, while it runs it.
    /// The program and its interpreter.
    text: SpinLock<Vec<Text>>,
    /// Its credential changed since it last ran `execve` (FreeBSD's
    /// `P_SUGID`, which `issetugid` reports); inherited.
    sugid: AtomicBool,
    /// `P2_*` flags (`p_flag2`), most of which `procctl` sets.
    pub(crate) flag2: AtomicU32,
    /// The signal it gets when its parent exits (`p_pdeathsig`), 0 for
    /// none.
    pub(crate) pdeathsig: AtomicU32,
    /// The CPU seconds after which it next gets `SIGXCPU` (`p_cpulimit`).
    cpulimit: AtomicU64,
    /// Its credential is in capability mode: a copy of the credential's
    /// flag, read without its lock on every system call not allowed
    /// there. It never clears.
    capmode: AtomicBool,
    /// Its ktrace state (`p_traceflag`, `p_ktrioparms`, `p_ktr`).
    pub(crate) ktrace: crate::ktrace::Trace,
    /// The numbered CPU set it is in (`cpuset(2)`); inherited.
    cpuset: AtomicI32,
    /// Traced by a debugger (`P_TRACED`): a copy of what `trace` says,
    /// read without its lock on every system call.
    traced: AtomicBool,
    /// Its tracing (`p_oppid`, `p_ptevents`, `p_xthread`, ...).
    pub(crate) trace: SpinLock<crate::ptrace::Tracing>,
    /// Where `ptrace` requests wait for one another and for a stopped
    /// thread to serve one.
    trace_wait: WaitQueue,
    /// What its reaped children used, theirs included (`p_cru`).
    pub(crate) child_usage: SpinLock<crate::kinfo::Usage>,
}

/// `p_flag2` bits (sys/sys/proc.h).
pub(crate) mod p2 {
    pub(crate) const NOTRACE: u32 = 0x0000_0002;
    pub(crate) const NOTRACE_EXEC: u32 = 0x0000_0004;
    pub(crate) const TRAPCAP: u32 = 0x0000_0020;
    pub(crate) const ASLR_ENABLE: u32 = 0x0000_0040;
    pub(crate) const ASLR_DISABLE: u32 = 0x0000_0080;
    pub(crate) const ASLR_IGNSTART: u32 = 0x0000_0100;
    pub(crate) const PROTMAX_ENABLE: u32 = 0x0000_0200;
    pub(crate) const PROTMAX_DISABLE: u32 = 0x0000_0400;
    pub(crate) const STKGAP_DISABLE: u32 = 0x0000_0800;
    pub(crate) const STKGAP_DISABLE_EXEC: u32 = 0x0000_1000;
    pub(crate) const NO_NEW_PRIVS: u32 = 0x0000_8000;
    pub(crate) const WXORX_DISABLE: u32 = 0x0001_0000;
    pub(crate) const WXORX_ENABLE_EXEC: u32 = 0x0002_0000;
    pub(crate) const LOGSIGEXIT_ENABLE: u32 = 0x0080_0000;
    pub(crate) const LOGSIGEXIT_CTL: u32 = 0x0100_0000;
    /// What a child inherits (`fork1`).
    pub(crate) const INHERITED: u32 = ASLR_DISABLE
        | ASLR_ENABLE
        | ASLR_IGNSTART
        | NOTRACE
        | NOTRACE_EXEC
        | PROTMAX_ENABLE
        | PROTMAX_DISABLE
        | TRAPCAP
        | STKGAP_DISABLE
        | STKGAP_DISABLE_EXEC
        | NO_NEW_PRIVS
        | WXORX_DISABLE
        | WXORX_ENABLE_EXEC
        | LOGSIGEXIT_CTL
        | LOGSIGEXIT_ENABLE;
}

/// What a session's processes share (FreeBSD's `struct session`, besides
/// its leader and terminal).
#[derive(Debug)]
pub(crate) struct Session {
    /// The login name `setlogin` set (`s_login`), without its NUL.
    login: SpinLock<Vec<u8>>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            login: SpinLock::new(Vec::new()),
        }
    }
}

/// `MAXLOGNAME`: a login name's room, with its NUL.
pub(crate) const MAXLOGNAME: usize = 33;

/// The program a process runs, as `kern.proc` reports it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Image {
    /// The command name (`ki_comm`): the file name it was run as.
    pub(crate) comm: Vec<u8>,
    /// The arguments, each NUL-terminated (`kern.proc.args`).
    pub(crate) args: Arc<[u8]>,
    /// The executable's path (`kern.proc.pathname`).
    pub(crate) path: Arc<[u8]>,
    /// The auxiliary vector exec gave it (`kern.proc.auxv`).
    pub(crate) auxv: Arc<[u8]>,
}

impl Image {
    /// For the program at `path` (as given, from directory `cwd`) run with
    /// `argv`.
    fn new(s: &Sleepable, cwd: &Arc<Vnode>, path: &[u8], argv: &[&[u8]]) -> Self {
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
        let comm = name[..name.len().min(abi::kinfo::MAXCOMLEN)].to_vec();
        let mut args = Vec::new();
        for a in argv {
            args.extend_from_slice(a);
            args.push(0);
        }
        let full = if path.first() == Some(&b'/') {
            path.to_vec()
        } else {
            let mut dir = crate::fs::namespace()
                .path_of(s, cwd, &crate::fs::root())
                .unwrap_or_else(|_| b"/".to_vec());
            if dir.last() != Some(&b'/') {
                dir.push(b'/');
            }
            dir.extend_from_slice(path);
            dir
        };
        Self {
            comm,
            args: args.into(),
            path: full.into(),
            auxv: Arc::from([]),
        }
    }
}

/// A program a process runs, or its interpreter, counted on its vnode
/// while it runs, so the file cannot be opened for writing meanwhile
/// (FreeBSD's `VV_TEXT`, which exec's mappings of both set).
pub(crate) struct Text(Arc<Vnode>);

impl Text {
    /// `ETXTBSY` if a file has it open for writing.
    pub(crate) fn new(v: &Arc<Vnode>) -> Result<Self, Errno> {
        v.add_text()?;
        Ok(Self(Arc::clone(v)))
    }
}

impl Clone for Text {
    fn clone(&self) -> Self {
        // Running already, it has no writers to refuse it.
        let _ = self.0.add_text();
        Self(Arc::clone(&self.0))
    }
}

impl Drop for Text {
    fn drop(&mut self) {
        self.0.drop_text();
    }
}

/// How a user thread enters user mode.
struct Start {
    thread: Arc<UserThread>,
    frame: TrapFrame,
    tls: [u64; 2],
    context: Option<crate::signal::Prepared>,
}

/// What `exec` runs: the program at a path (`execve`), or the file open
/// as a descriptor (`fexecve`).
pub(crate) enum Program<'a> {
    Path(&'a [u8]),
    File(Arc<Vnode>, i32),
}

/// An image `exec` loaded, and the program and arguments it runs with
/// (a script's interpreter's).
struct Loaded {
    /// The credential it runs with, if not the caller's.
    cred: Option<Ucred>,
    /// It runs set-id (`credential_setid`).
    setid: bool,
    text: Text,
    file: Arc<Vnode>,
    space: Arc<AddressSpace>,
    image: crate::exec::Image,
    path: Vec<u8>,
    argv: Vec<Vec<u8>>,
}

/// A new thread's state, besides its registers.
pub(crate) struct NewThread {
    /// Its user thread registers (`hal::user::tls`, `tls2`).
    pub(crate) tls: [u64; 2],
    /// The TLS base `sysarch` records (amd64).
    pub(crate) tls_base: Option<u64>,
    pub(crate) sig: ThreadSignals,
    /// A copy of the caller's FP state (`fork`), rather than the initial
    /// one.
    pub(crate) fork_fpu: bool,
    /// The CPUs it may run on (its `cpuset`); bound if that is one.
    pub(crate) cpus: crate::uthread::Cpus,
    /// Its class and priority.
    pub(crate) rtprio: abi::thr::Rtprio,
    /// A context it loads first (`thr_create`): FP state, a thread pointer.
    pub(crate) context: Option<crate::signal::Prepared>,
}

impl NewThread {
    /// A process's first thread, made by the kernel: on `cpu` alone, if
    /// given. That is the kernel's placement, not a mask of the thread's
    /// own: a change to its set replaces it.
    fn first(cpu: Option<hal::CpuId>) -> Self {
        Self {
            tls: [0; 2],
            tls_base: None,
            sig: ThreadSignals::new(),
            fork_fpu: false,
            cpus: crate::uthread::Cpus {
                mask: cpu.map_or(u64::MAX, |c| 1 << c.0),
                own: false,
            },
            rtprio: crate::uthread::NORMAL,
            context: None,
        }
    }
}

impl Process {
    /// A process running in `space` with descriptors `files`, with no
    /// threads yet: a child of `parent` (inheriting its signal state,
    /// limits, layout, group, session, working directory, and umask), or a
    /// new session's leader laid out as `layout` says, in the root.
    fn create(
        space: Arc<AddressSpace>,
        layout: Layout,
        parent: Option<&Arc<Self>>,
        files: Arc<Mutex<FdTable>>,
    ) -> Arc<Self> {
        let (signals, limits) = match parent {
            Some(p) => (p.signals.lock().forked(), *p.limits.lock()),
            None => (Signals::new(), default_limits()),
        };
        let cwd = parent.map_or_else(|| crate::fs::root().use_ref(), |p| p.cwd.lock().clone());
        let umask = parent.map_or(0o022, |p| p.umask.load(Ordering::Relaxed));
        let image = parent.map(|p| p.image.lock().clone()).unwrap_or_default();
        let execed = parent.is_some_and(|p| p.execed.load(Ordering::Relaxed));
        let cred = parent.map_or_else(crate::cred::root_shared, |p| p.cred());
        let sugid = parent.is_some_and(|p| p.sugid.load(Ordering::Relaxed));
        let text = parent.map(|p| p.text.lock().clone()).unwrap_or_default();
        let p = Arc::new_cyclic(|me| {
            let pid = alloc_pid(me);
            Self {
                pid,
                space: SpinLock::new(space),
                vforked: AtomicBool::new(false),
                layout: SharedLayout::new(parent.map_or(layout, |p| p.layout())),
                signals: SpinLock::new(signals),
                limits: PreemptLock::new(limits),
                exterr: AtomicU64::new(parent.map_or(0, |p| p.exterr.load(Ordering::Relaxed))),
                pgid: AtomicI32::new(parent.map_or(pid, |p| p.pgid())),
                sid: AtomicI32::new(parent.map_or(pid, |p| p.sid())),
                // A session the kernel starts is logged in as root, as
                // init's own is (it calls `setlogin("root")`).
                session: SpinLock::new(parent.map_or_else(
                    || {
                        Arc::new(Session {
                            login: SpinLock::new(b"root".to_vec()),
                        })
                    },
                    |p| p.session(),
                )),
                status: AtomicI32::new(0),
                exited: AtomicBool::new(false),
                zombie: AtomicBool::new(false),
                procdesc: AtomicBool::new(false),
                klist: event::Notify::new(),
                pd_sel: event::Notify::new(),
                last_child: AtomicI32::new(0),
                timers: sync::Once::new(),
                waiters: WaitQueue::new(),
                child_gen: AtomicU64::new(0),
                child_wait: WaitQueue::new(),
                threads: SpinLock::new(Vec::new()),
                job: SpinLock::new(Job::default()),
                stop_wait: WaitQueue::new(),
                single_wait: WaitQueue::new(),
                had_threads: AtomicBool::new(false),
                dead_runtime: AtomicU64::new(0),
                dead_switches: [AtomicU64::new(0), AtomicU64::new(0)],
                nice: AtomicI32::new(parent.map_or(0, |p| p.nice.load(Ordering::Relaxed))),
                umtx_min_timeout: AtomicU64::new(
                    parent.map_or(0, |p| p.umtx_min_timeout.load(Ordering::Relaxed)),
                ),
                files,
                cwd: PreemptLock::new(cwd),
                root: PreemptLock::new(parent.and_then(|p| p.root.lock().clone())),
                umask: AtomicU16::new(umask),
                image: SpinLock::new(image),
                started: crate::time::now_ns(),
                execed: AtomicBool::new(execed),
                cred: PreemptLock::new(cred),
                text: SpinLock::new(text),
                sugid: AtomicBool::new(sugid),
                flag2: AtomicU32::new(
                    parent.map_or(0, |p| p.flag2.load(Ordering::Relaxed) & p2::INHERITED),
                ),
                pdeathsig: AtomicU32::new(0),
                cpulimit: AtomicU64::new(
                    parent.map_or(RLIM_INFINITY, |p| p.cpulimit.load(Ordering::Relaxed)),
                ),
                capmode: AtomicBool::new(parent.is_some_and(|p| p.in_capability_mode())),
                ktrace: crate::ktrace::Trace::default(),
                cpuset: AtomicI32::new(
                    parent.map_or(crate::cpuset::DEFAULT, |p| p.cpuset.load(Ordering::Relaxed)),
                ),
                traced: AtomicBool::new(false),
                trace: SpinLock::new(crate::ptrace::Tracing::default()),
                trace_wait: WaitQueue::new(),
                child_usage: SpinLock::new(crate::kinfo::Usage::default()),
            }
        });
        crate::prison::proc_joined(&p.cred());
        if let Some(parent) = parent {
            let mut tree = TREE.lock();
            tree.parent.insert(p.pid, parent.pid);
            tree.children
                .entry(parent.pid)
                .or_default()
                .push(Arc::clone(&p));
            tree.forked(parent.pid, p.pid);
        }
        p
    }

    /// Whether this is a `vfork` child its parent waits for (FreeBSD's
    /// `P_PPWAIT`).
    pub(crate) fn vforked(&self) -> bool {
        self.vforked.load(Ordering::Acquire)
    }

    /// Its credential.
    pub(crate) fn cred(&self) -> Arc<Ucred> {
        Arc::clone(&self.cred.lock())
    }

    /// Its credential's jail (`None` on the host), without taking the
    /// whole credential.
    pub(crate) fn jail(&self) -> Option<Arc<dyn security::Jail>> {
        self.cred.lock().jail.clone()
    }

    fn session(&self) -> Arc<Session> {
        Arc::clone(&self.session.lock())
    }

    /// Its session's login name (`getlogin`).
    pub(crate) fn login(&self) -> Vec<u8> {
        self.session().login.lock().clone()
    }

    /// Sets its session's login name (`setlogin`).
    pub(crate) fn set_login(&self, name: &[u8]) {
        *self.session().login.lock() = name.to_vec();
    }

    /// Whether its credential holds privilege `p` (`priv_check`): `EPERM`
    /// if not.
    pub(crate) fn priv_check(&self, p: Priv) -> Result<(), Errno> {
        self.cred.lock().priv_check(p)
    }

    /// `p_cansee`: whether it may see `p` (`ESRCH` if not).
    pub(crate) fn can_see(&self, p: &Self) -> Result<(), Errno> {
        if core::ptr::eq(self, p) {
            return Ok(());
        }
        self.cred().can_see(&p.cred())
    }

    /// `p_cansignal`: whether it may send `sig` to `p`. Any process may
    /// continue one in its session.
    pub(crate) fn can_signal(&self, p: &Self, sig: u32) -> Result<(), Errno> {
        if core::ptr::eq(self, p) || (sig == abi::signal::SIGCONT && self.sid() == p.sid()) {
            return Ok(());
        }
        self.cred().can_signal(&p.cred(), p.sugid(), sig)
    }

    /// `p_cansched`: whether it may change `p`'s scheduling.
    pub(crate) fn can_sched(&self, p: &Self) -> Result<(), Errno> {
        if core::ptr::eq(self, p) {
            return Ok(());
        }
        self.cred().can_sched(&p.cred())
    }

    /// `p_candebug`: whether it may debug `p`. init is out of reach above
    /// securelevel 0.
    pub(crate) fn can_debug(&self, p: &Self) -> Result<(), Errno> {
        if core::ptr::eq(self, p) {
            return Ok(());
        }
        let cred = self.cred();
        cred.can_debug(&p.cred(), p.sugid())?;
        if p.pid == 1 {
            cred.securelevel_gt(0)?;
        }
        // Refused by `PROC_TRACE_CTL`, but to `PRIV_DEBUG_DENIED`.
        if p.flag2.load(Ordering::Relaxed) & p2::NOTRACE != 0 {
            cred.priv_check(Priv::DebugDenied)?;
        }
        Ok(())
    }

    /// Changes its credential as `change` says, from the current one; the
    /// process is then set-id tainted. Only its own thread changes it.
    pub(crate) fn change_cred(
        &self,
        change: impl FnOnce(&Ucred) -> Result<Ucred, Errno>,
    ) -> Result<(), Errno> {
        let old = self.cred();
        let new = change(&old)?;
        if new != *old {
            *self.cred.lock() = Arc::new(new);
            self.sugid.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Replaces its credential with `change`'s, leaving it untainted (what
    /// FreeBSD's `proc_set_cred` alone does: `setloginclass`, `cap_enter`,
    /// `jail_attach`). Only its own thread changes it.
    pub(crate) fn replace_cred(
        &self,
        change: impl FnOnce(&Ucred) -> Result<Ucred, Errno>,
    ) -> Result<(), Errno> {
        let new = change(&self.cred())?;
        *self.cred.lock() = Arc::new(new);
        Ok(())
    }

    /// Whether it is in capability mode.
    pub(crate) fn in_capability_mode(&self) -> bool {
        self.capmode.load(Ordering::Relaxed)
    }

    /// `cap_enter`: its credential enters capability mode, for good.
    pub(crate) fn enter_capability_mode(&self) {
        if self.in_capability_mode() {
            return;
        }
        let _ = self.replace_cred(|c| {
            let mut c = c.clone();
            c.flags |= security::CRED_FLAG_CAPMODE;
            Ok(c)
        });
        self.capmode.store(true, Ordering::Relaxed);
    }

    /// How many processes, zombies too, have real uid `ruid` (FreeBSD's
    /// `ui_proccnt`).
    pub(crate) fn count_of(ruid: u32) -> usize {
        all().iter().filter(|p| p.cred().ruid == ruid).count()
    }

    /// Whether its credential changed since it last ran `execve`.
    pub(crate) fn sugid(&self) -> bool {
        self.sugid.load(Ordering::Relaxed)
    }

    /// The program it runs.
    pub(crate) fn image(&self) -> Image {
        self.image.lock().clone()
    }

    /// Replaces its arguments as `kern.proc.args` shows them
    /// (`setproctitle`).
    pub(crate) fn set_args(&self, args: &[u8]) {
        self.image.lock().args = args.into();
    }

    /// Calls `f` with its command name (`ki_comm`), without copying the
    /// rest of the image.
    pub(crate) fn with_comm<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(&self.image.lock().comm)
    }

    /// When it was created, in nanoseconds since boot.
    pub(crate) const fn started(&self) -> u64 {
        self.started
    }

    /// The program it runs (`p_textvp`).
    pub(crate) fn text_vnode(&self) -> Option<Arc<Vnode>> {
        self.text.lock().first().map(|t| Arc::clone(&t.0))
    }

    /// Whether it has called `execve`.
    pub(crate) fn execed(&self) -> bool {
        self.execed.load(Ordering::Relaxed)
    }

    /// The CPUs its threads may run on, one bit each: the union of theirs
    /// (FreeBSD's `CPU_WHICH_PID` answer).
    pub(crate) fn cpus(&self) -> u64 {
        let mut mask = 0;
        self.each_thread(|t| {
            mask |= t.cpus();
            true
        });
        mask
    }

    /// Gives every thread `mask` as CPUs of its own (`cpuset_setaffinity`
    /// for a process).
    pub(crate) fn set_own_cpus(&self, mask: u64) {
        for t in self.threads() {
            t.set_own_cpus(mask);
        }
    }

    /// Whether it has exited (a zombie, until reaped).
    pub(crate) fn is_zombie(&self) -> bool {
        self.zombie.load(Ordering::Acquire)
    }

    /// Whether its exit status is set (`P_WEXIT` past `p_xexit`).
    pub(crate) fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// The status `wait` reports, once it has exited (`KW_EXITCODE`).
    pub(crate) fn wait_status(&self) -> i32 {
        self.status.load(Ordering::Relaxed)
    }

    /// The pid of the child it forked last.
    pub(crate) fn last_child(&self) -> i32 {
        self.last_child.load(Ordering::Relaxed)
    }

    /// Its timers (`itimers_alloc` on first use).
    pub(crate) fn timers(self: &Arc<Self>) -> &crate::timers::Timers {
        loop {
            if let Some(t) = self.timers.get() {
                return t;
            }
            let _ = self.timers.set(crate::timers::Timers::new(self));
        }
    }

    /// The address space the process runs in now.
    pub(crate) fn space(&self) -> Arc<AddressSpace> {
        Arc::clone(&self.space.lock())
    }

    pub(crate) const fn pid(&self) -> i32 {
        self.pid
    }

    pub(crate) fn pgid(&self) -> i32 {
        self.pgid.load(Ordering::Relaxed)
    }

    pub(crate) fn sid(&self) -> i32 {
        self.sid.load(Ordering::Relaxed)
    }

    /// The parent's pid as the process sees it (`p_oppid`): while it is
    /// traced, its real parent's, not its debugger's.
    pub(crate) fn ppid(&self) -> i32 {
        if self.is_traced() {
            return self.trace.lock().oppid;
        }
        self.ppid_tree()
    }

    /// The parent's pid in the tree (`p_pptr`: the debugger while
    /// traced; 1 for an orphan, 0 for a process the kernel started).
    pub(crate) fn ppid_tree(&self) -> i32 {
        TREE.lock().parent.get(&self.pid).copied().unwrap_or(0)
    }

    /// The numbered CPU set it is in.
    pub(crate) fn cpuset(&self) -> i32 {
        self.cpuset.load(Ordering::Relaxed)
    }

    pub(crate) fn set_cpuset(&self, id: i32) {
        self.cpuset.store(id, Ordering::Relaxed);
    }

    /// Whether a debugger traces it (`P_TRACED`).
    pub(crate) fn is_traced(&self) -> bool {
        self.traced.load(Ordering::Acquire)
    }

    /// Starts its tracing (`proc_set_traced`): `oppid` its real parent,
    /// the default events, and with `first_stop` the attach's stop to
    /// come.
    pub(crate) fn set_traced(&self, oppid: i32, first_stop: bool) {
        {
            let mut tr = self.trace.lock();
            tr.oppid = oppid;
            tr.events = abi::ptrace::PTRACE_DEFAULT;
            tr.first_stop = first_stop;
            tr.killed = false;
        }
        self.traced.store(true, Ordering::Release);
    }

    /// Ends its tracing (`P_TRACED` cleared); the reporting thread
    /// stays for the resuming request to hand its signal.
    pub(crate) fn clear_traced(&self) {
        self.traced.store(false, Ordering::Release);
        self.trace.lock().pptrace = false;
    }

    /// Where `ptrace` requests on it wait.
    pub(crate) fn trace_wait(&self) -> &WaitQueue {
        &self.trace_wait
    }

    /// Its thread `tid`.
    pub(crate) fn thread_by_tid(&self, tid: i32) -> Option<Arc<UserThread>> {
        self.threads.lock().iter().find(|t| t.tid() == tid).cloned()
    }

    /// Single-threading to exit is under way (`P_SINGLE_EXIT`).
    pub(crate) fn exiting_single(&self) -> bool {
        self.job.lock().single.is_some_and(|x| x.mode == Mode::Exit)
    }

    /// It is stopped for its debugger, every thread, and the debugger
    /// has waited for the stop (`proc_can_ptrace`'s test).
    pub(crate) fn trace_stopped_and_waited(&self) -> bool {
        let job = self.job.lock();
        job.trace && job.stopped != 0 && job.stop_told && !job.stop_report
    }

    /// Where `mmap` without an address starts looking.
    pub(crate) fn mmap_base(&self) -> u64 {
        self.layout.anon_loc.load(Ordering::Relaxed)
    }

    /// The process the current thread runs for.
    pub(crate) fn current() -> Option<Arc<Self>> {
        UserThread::with_current(|t| Arc::clone(t.process()))
    }

    /// Whether the calling thread is the process's only one, so that only
    /// it can change what the process's threads share (its descriptor
    /// table).
    pub(crate) fn sole_thread(&self) -> bool {
        self.threads.lock().len() <= 1
    }

    /// Whether its one thread is all that reaches its descriptor table
    /// (not shared by `rfork`), which may then be read without the lock.
    pub(crate) fn sole_user_of_files(&self) -> bool {
        self.sole_thread() && Arc::strong_count(&self.files) == 1
    }

    /// `rfork(RFPROC)`: a child sharing the descriptor table.
    pub(crate) fn fork_shared_files(
        self: &Arc<Self>,
        s: &Sleepable,
        frame: &TrapFrame,
    ) -> Result<i32, Errno> {
        self.fork_with(s, frame, false, None, false, true)
            .map(|(pid, _)| pid)
    }

    /// Runs `f` on the current process, borrowed (no reference taken):
    /// `None` on a kernel thread.
    pub(crate) fn with_current<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        UserThread::with_current(|t| f(t.process()))
    }

    /// Its threads, in creation order.
    pub(crate) fn threads(&self) -> Vec<Arc<UserThread>> {
        self.threads.lock().clone()
    }

    /// Calls `f` on each thread, in creation order, with the list locked:
    /// `f` must not take the process's own locks but a thread's.
    pub(crate) fn each_thread(&self, mut f: impl FnMut(&Arc<UserThread>) -> bool) {
        for t in self.threads.lock().iter() {
            if !f(t) {
                break;
            }
        }
    }

    /// Its first thread (FreeBSD's `FIRST_THREAD_IN_PROC`).
    pub(crate) fn first_thread(&self) -> Option<Arc<UserThread>> {
        self.threads.lock().first().cloned()
    }

    /// How many threads it has.
    pub(crate) fn thread_count(&self) -> usize {
        self.threads.lock().len()
    }

    /// Whether it has had more than one thread.
    pub(crate) fn had_threads(&self) -> bool {
        self.had_threads.load(Ordering::Relaxed)
    }

    /// The CPU time its threads used, those gone included, in platform
    /// time units.
    pub(crate) fn runtime(&self) -> u64 {
        let mut ticks = self.dead_runtime.load(Ordering::Relaxed);
        self.each_thread(|t| {
            ticks += t.sched().map_or(0, |s| s.runtime());
            true
        });
        ticks
    }

    /// The context switches of its threads that have left: voluntary,
    /// involuntary.
    pub(crate) fn dead_switches(&self) -> (u64, u64) {
        (
            self.dead_switches[0].load(Ordering::Relaxed),
            self.dead_switches[1].load(Ordering::Relaxed),
        )
    }

    /// The current thread, leaving, leaves its time and its context
    /// switches with the process (`thread_exit`'s `ruxagg` and `rucollect`).
    fn bank_current_usage(&self) {
        self.dead_runtime
            .fetch_add(sched::current_runtime(), Ordering::Relaxed);
        let (v, i) = sched::current().context_switches();
        self.dead_switches[0].fetch_add(v, Ordering::Relaxed);
        self.dead_switches[1].fetch_add(i, Ordering::Relaxed);
    }

    /// Its thread `tid`.
    pub(crate) fn find_thread(&self, tid: i32) -> Option<Arc<UserThread>> {
        crate::uthread::find(tid).filter(|t| core::ptr::eq(t.process().as_ref(), self))
    }

    /// A new process running the program at `path` (from the root), with
    /// arguments `argv` and environment `envp`, started by the kernel (no
    /// parent: a session leader, as init is) with the console open on
    /// descriptors 0 to 2; its thread is bound to `cpu` if given.
    pub(crate) fn spawn(
        s: &Sleepable,
        path: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
        cpu: Option<hal::CpuId>,
    ) -> Result<Arc<Self>, Errno> {
        Self::spawn_session(s, path, argv, envp, cpu, None)
    }

    /// As [`Process::spawn`], with `ctty` as the new session's controlling
    /// terminal from the start, as init gives a shell its terminal.
    pub(crate) fn spawn_session(
        s: &Sleepable,
        path: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
        cpu: Option<hal::CpuId>,
        ctty: Option<&crate::tty::Tty>,
    ) -> Result<Arc<Self>, Errno> {
        let root = crate::fs::root();
        let file = crate::fs::namespace().lookup(
            s,
            &vfs::namei::Lookup {
                start: &root,
                root: &root,
                follow: true,
                beneath: false,
                create: false,
                cred: &crate::fs::cred(),
            },
            path,
        )?;
        let space = AddressSpace::new().map_err(|_| Errno::ENOMEM)?;
        let lookup = |p: &[u8]| {
            crate::fs::namespace().lookup(
                s,
                &vfs::namei::Lookup {
                    start: &root,
                    root: &root,
                    follow: true,
                    beneath: false,
                    create: false,
                    cred: &crate::fs::cred(),
                },
                p,
            )
        };
        let image = crate::exec::load(
            s,
            &space,
            &file,
            path,
            argv,
            envp,
            &default_limits(),
            0,
            &lookup,
        )?;
        let p = Self::create(space, image.layout, None, own(crate::fs::console_table()));
        *p.image.lock() = Image {
            auxv: image.auxv.into(),
            ..Image::new(s, &root, path, argv)
        };
        *p.text.lock() = core::iter::once(Text::new(&file)?)
            .chain(image.interp)
            .collect();
        if let Some(t) = ctty {
            crate::tty::make_controlling(t, &p);
        }
        let frame = arch::user::user_frame(image.entry, image.sp, image.arg);
        p.start(frame, NewThread::first(cpu))?;
        Ok(p)
    }

    /// A process running `space` from machine code the kernel placed there
    /// (tests), started at `entry` with stack `sp`.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub(crate) fn spawn_code(
        space: Arc<AddressSpace>,
        entry: u64,
        sp: u64,
        cpu: Option<hal::CpuId>,
    ) -> Arc<Self> {
        let layout = Layout {
            anon_loc: 64 << 30,
            stack_top: sp,
            ..Layout::default()
        };
        let p = Self::create(space, layout, None, own(crate::fs::console_table()));
        let frame = arch::user::user_frame(entry, sp, 0);
        p.start(frame, NewThread::first(cpu))
            .expect("a thread for the test process");
        p
    }

    /// Starts a thread of the process with `frame`'s registers and the
    /// rest as `new` says; it is named after the program. `EAGAIN` when
    /// no tid or kernel thread is to be had.
    pub(crate) fn start(
        self: &Arc<Self>,
        frame: TrapFrame,
        new: NewThread,
    ) -> Result<Arc<UserThread>, Errno> {
        self.start_with(frame, new, |_, _| Ok(()))
    }

    /// [`Process::start`], with `init` given the thread (its tid known)
    /// and its registers first: an error from it is the call's, and the
    /// thread never runs (FreeBSD's `thread_create` initializer).
    pub(crate) fn start_with(
        self: &Arc<Self>,
        mut frame: TrapFrame,
        new: NewThread,
        init: impl FnOnce(&UserThread, &mut TrapFrame) -> Result<(), Errno>,
    ) -> Result<Arc<UserThread>, Errno> {
        let name = self.with_comm(Name::new);
        let t = UserThread::new(self, new.sig, name, new.tls_base, new.rtprio, new.cpus)?;
        // The call that made it returns in it too (`fork_return`), as a
        // copy of the creating thread's `td_sa`.
        UserThread::with_current(|me| {
            t.set_syscall_code(me.syscall_code());
            t.dbg.inherit_call(&me.dbg);
        });
        init(&t, &mut frame)?;
        {
            let mut threads = self.threads.lock();
            threads.push(Arc::clone(&t));
            if threads.len() > 1 {
                self.had_threads.store(true, Ordering::Relaxed);
            }
        }
        let start = alloc::boxed::Box::new(Start {
            thread: Arc::clone(&t),
            frame,
            tls: new.tls,
            context: new.context,
        });
        let arg = alloc::boxed::Box::into_raw(start) as usize;
        let mut b = sched::Builder::new("user").priority(crate::uthread::priority(new.rtprio));
        b = if new.fork_fpu {
            b.fpu_inherit()
        } else {
            b.fpu()
        };
        if let Some(cpu) = crate::uthread::bound(new.cpus.mask) {
            b = b.bind(cpu);
        }
        match b.spawn_raw(enter, arg) {
            Ok(_) => Ok(t),
            Err(_) => {
                // SAFETY: the thread never started, so `arg` is still ours.
                drop(unsafe { alloc::boxed::Box::from_raw(arg as *mut Start) });
                self.threads.lock().retain(|o| !Arc::ptr_eq(o, &t));
                Err(Errno::EAGAIN)
            }
        }
    }

    /// `fork` (or `vfork`, sharing the address space until the child execs
    /// or exits, the parent sleeping meanwhile): a child running a copy of
    /// this process from `frame`, the caller's system-call frame. Returns
    /// the child's pid.
    pub(crate) fn fork(
        self: &Arc<Self>,
        s: &Sleepable,
        frame: &TrapFrame,
        vfork: bool,
    ) -> Result<i32, Errno> {
        self.fork_with(s, frame, vfork, None, false, false)
            .map(|(pid, _)| pid)
    }

    /// `rfork(RFSPAWN)`, which `posix_spawn` uses: `vfork`, with the
    /// child's caught signals back to their defaults.
    pub(crate) fn spawn_fork(
        self: &Arc<Self>,
        s: &Sleepable,
        frame: &TrapFrame,
    ) -> Result<i32, Errno> {
        self.fork_with(s, frame, true, None, true, false)
            .map(|(pid, _)| pid)
    }

    /// `pdfork`: `fork`, with a process descriptor for the child installed
    /// in this process's table (not the child's). Returns the child's pid
    /// and the descriptor.
    pub(crate) fn pdfork(
        self: &Arc<Self>,
        s: &Sleepable,
        frame: &TrapFrame,
        daemon: bool,
        cloexec: bool,
        limit: usize,
    ) -> Result<(i32, i32), Errno> {
        let (pid, fd) = self.fork_with(
            s,
            frame,
            false,
            Some((daemon, cloexec, limit)),
            false,
            false,
        )?;
        Ok((pid, fd.unwrap_or(-1)))
    }

    /// `pdrfork`: [`pdfork`](Self::pdfork) in `rfork`'s forms, `how`
    /// being (`vfork`, caught signals to their defaults, the descriptor
    /// table shared) and `pd` (`PD_DAEMON`, `PD_CLOEXEC`, the table's
    /// limit). Returns the child's pid and the descriptor.
    pub(crate) fn pdrfork(
        self: &Arc<Self>,
        s: &Sleepable,
        frame: &TrapFrame,
        how: (bool, bool, bool),
        pd: (bool, bool, usize),
    ) -> Result<(i32, i32), Errno> {
        let (vfork, drop_caught, share_files) = how;
        let (pid, fd) = self.fork_with(s, frame, vfork, Some(pd), drop_caught, share_files)?;
        Ok((pid, fd.unwrap_or(-1)))
    }

    fn fork_with(
        self: &Arc<Self>,
        s: &Sleepable,
        frame: &TrapFrame,
        vfork: bool,
        pd: Option<(bool, bool, usize)>,
        drop_caught: bool,
        share_files: bool,
    ) -> Result<(i32, Option<i32>), Errno> {
        // A user's processes, zombies too, are held to `RLIMIT_NPROC`,
        // counted by real user, the superuser's included (`fork1`'s
        // `chgproccnt`).
        let max = self.limits.lock()[rlimit::NPROC as usize].rlim_cur;
        if max != RLIM_INFINITY && Self::count_of(self.cred().ruid) as u64 >= max {
            return Err(Errno::EAGAIN);
        }
        let space = if vfork {
            self.space()
        } else {
            self.space().fork(s).map_err(|_| Errno::ENOMEM)?
        };
        let (files, kqueues) = if share_files {
            (Arc::clone(&self.files), Vec::new())
        } else {
            let (t, k) = self.files.lock(s).fork()?;
            (own(t), k)
        };
        let child = Self::create(space, Layout::default(), Some(self), files);
        // `KQUEUE_CPONFORK` kqueues' knotes, for the descriptors the child
        // has (`kqueue_fork_copy`).
        if !kqueues.is_empty() {
            let table = child.files.lock(s);
            for (from, to) in &kqueues {
                from.fork_into(to, &|fd| {
                    i32::try_from(fd).is_ok_and(|fd| table.get(fd).is_ok())
                });
            }
        }
        child.vforked.store(vfork, Ordering::Relaxed);
        // `rfork(RFSPAWN)`: the child catches nothing (`FR2_DROPSIG_CAUGHT`).
        if drop_caught {
            child.signals.lock().exec();
        }
        // The descriptor exists before the child runs, so its exit is a
        // descriptor's from the start.
        let fd = match pd {
            Some((daemon, cloexec, limit)) => {
                child.procdesc.store(true, Ordering::Release);
                let file = crate::fs::file::File::new(
                    crate::fs::file::FileKind::Process(ProcDesc {
                        process: Arc::clone(&child),
                        daemon,
                    }),
                    // `procdesc_finit`.
                    abi::fcntl::FREAD | abi::fcntl::FWRITE,
                );
                let flags = crate::fs::fdtable::FdFlags {
                    cloexec,
                    clofork: false,
                    resolve_beneath: false,
                };
                let installed = self.files.lock(s).alloc(file, flags, 0, limit);
                match installed {
                    Ok(fd) => Some(fd),
                    Err(e) => {
                        // The descriptor's file is gone; the child never ran.
                        child.detach();
                        return Err(e);
                    }
                }
            }
            None => None,
        };
        let mut cf = *frame;
        arch_fork_return(&mut cf);
        let (sig, rtprio, cpus) =
            UserThread::with_current(|t| (t.sig.lock().forked(), t.rtprio(), t.affinity()))
                .unwrap_or((
                    ThreadSignals::new(),
                    crate::uthread::NORMAL,
                    crate::uthread::Cpus {
                        mask: u64::MAX,
                        own: false,
                    },
                ));
        let new = NewThread {
            tls: [hal::user::tls(), hal::user::tls2()],
            // The child's FS base is the parent's, but no TLS base is
            // recorded for `AMD64_GET_TLSBASE` (`cpu_fork` clears
            // `PCB_TLSBASE`).
            tls_base: None,
            sig,
            fork_fpu: true,
            cpus,
            rtprio,
            context: None,
        };
        // The child's thread keeps the caller's robust-mutex lists (they
        // are in the copied part of FreeBSD's `struct thread`).
        let robust = UserThread::with_current(|t| *t.umtx().robust.lock()).unwrap_or_default();
        crate::ktrace::fork(self, &child);
        let parent = Arc::clone(self);
        let started = child.start_with(cf, new, |t, _| {
            *t.umtx().robust.lock() = robust;
            crate::ktrace::proc_ctor_of(t);
            crate::ptrace::forked(&parent, &child, t, vfork);
            Ok(())
        });
        if started.is_err() {
            if let Some(fd) = fd {
                let closed = self.files.lock(s).close(fd);
                drop(closed);
            }
            child.detach();
            return Err(Errno::EAGAIN);
        }
        self.last_child.store(child.pid, Ordering::Relaxed);
        crate::event::proc_forked(self, child.pid);
        if vfork {
            // A traced parent waits after its debugger has seen the fork
            // ([`vfork_wait`]).
            match UserThread::current().filter(|_| self.is_traced()) {
                Some(t) => *t.vfork_child.lock() = Some(Arc::clone(&child)),
                None => self
                    .child_wait
                    .wait_until(s, || !child.vforked.load(Ordering::Acquire)),
            }
        }
        Ok((child.pid, fd))
    }

    /// Takes a child that could not start out of the tree.
    fn detach(&self) {
        let mut tree = TREE.lock();
        if let Some(ppid) = tree.parent.remove(&self.pid)
            && let Some(kids) = tree.children.get_mut(&ppid)
        {
            kids.retain(|c| c.pid != self.pid);
        }
        tree.reaped(self.pid);
    }

    /// Ends a `vfork` borrow: the parent may run again.
    fn release_parent(&self) {
        if self.vforked.swap(false, Ordering::AcqRel)
            && let Some(parent) = find(self.ppid())
        {
            parent.child_gen.fetch_add(1, Ordering::AcqRel);
            parent.child_wait.wake_all();
        }
    }

    /// Replaces the image with `program` (the current thread
    /// is this process's): a new address space, caught signals reset,
    /// close-on-exec descriptors closed, and `frame` set to start the new
    /// program.
    pub(crate) fn exec(
        &self,
        s: &Sleepable,
        frame: &mut TrapFrame,
        program: Program<'_>,
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<(), Errno> {
        let me = UserThread::current().ok_or(Errno::EINVAL)?;
        if self.ktrace.flags() != 0 {
            crate::ktrace::exec_args(s, argv, envp);
        }
        // The other threads wait at the boundary meanwhile (FreeBSD's
        // `pre_execve`); they leave once the new image is in.
        self.single(s, &me, Mode::Boundary)?;
        let image = self.load_image(s, program, argv, envp).and_then(|l| {
            // A set-id program starts with descriptors 0 to 2 open
            // (`fdcheckstd`).
            if l.setid {
                crate::fs::check_std(s, self)?;
            }
            Ok(l)
        });
        let Loaded {
            cred: new_cred,
            setid,
            text,
            file,
            space,
            image,
            path,
            argv,
        } = match image {
            Ok(i) => i,
            Err(e) => {
                self.end_single(&me);
                return Err(e);
            }
        };
        let argv: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
        let (path, argv) = (&path[..], &argv[..]);
        let _ = self.single(s, &me, Mode::Exit);
        // The old image's robust mutexes go before its memory does.
        crate::umtx::exec(s, &me);
        // No way back from here.
        let old = core::mem::replace(&mut *self.space.lock(), Arc::clone(&space));
        sched::set_address_space(Some(space as Arc<dyn sched::AddressSpace>));
        if self.vforked.load(Ordering::Acquire) {
            // The old space is the parent's. A child that asked to be
            // traced takes its signals to the debugger from now on
            // (`P_PPTRACE` goes with `P_PPWAIT`).
            self.trace.lock().pptrace = false;
            self.release_parent();
        } else if Arc::strong_count(&old) > 1 {
            // Another CPU still has it loaded: empty it now, so its memory
            // does not wait for that CPU to load another.
            old.clear(s);
        }
        // Dropping the last reference frees the tables and the pages.
        drop(old);
        self.signals.lock().exec();
        me.sig.lock().exec();
        let closed = self.files.lock(s).exec();
        crate::fs::closed(self.pid, &closed);
        drop(closed);
        let cwd = Arc::clone(self.cwd.lock().vnode());
        *self.image.lock() = Image {
            auxv: image.auxv.into(),
            ..Image::new(s, &cwd, path, argv)
        };
        self.execed.store(true, Ordering::Relaxed);
        *self.text.lock() = core::iter::once(text).chain(image.interp).collect();
        // A set-id program taints the process, and is not traced unless
        // its tracer may (`ktrprocexec`); otherwise the taint goes once
        // the effective ids are the real ones again.
        if setid {
            self.sugid.store(true, Ordering::Relaxed);
            crate::ktrace::exec_set_id(self);
        } else {
            let c = self.cred();
            if c.uid == c.ruid && c.gid == c.rgid {
                self.sugid.store(false, Ordering::Relaxed);
            }
        }
        if let Some(c) = new_cred {
            *self.cred.lock() = Arc::new(c);
        }
        let f2 = self.flag2.load(Ordering::Relaxed);
        let mut clear = 0;
        if f2 & p2::NOTRACE_EXEC == 0 {
            clear |= p2::NOTRACE;
        }
        if f2 & p2::STKGAP_DISABLE_EXEC == 0 {
            clear |= p2::STKGAP_DISABLE;
        }
        self.flag2.fetch_and(!clear, Ordering::Relaxed);
        self.exterr.store(0, Ordering::Relaxed);
        crate::event::proc_execed(self);
        if let Some(t) = self.timers.get() {
            t.exec();
        }
        me.set_tls_base(None);
        me.set_name(self.with_comm(Name::new));
        // SAFETY: this thread runs in the new image; the kernel does not
        // use the thread registers.
        unsafe {
            hal::user::set_tls(0);
            hal::user::set_tls2(0);
        }
        self.layout.set(image.layout);
        *frame = arch::user::user_frame(image.entry, image.sp, image.arg);
        drop(file);
        if self.ktrace.flags() & abi::ktrace::KTRFAC_MASK != 0 {
            crate::ktrace::proc_ctor_of(&me);
        }
        crate::ptrace::exec_done(self, &me);
        Ok(())
    }

    /// The program at `path`, loaded into a new address space for `exec`.
    /// A script runs its `#!` interpreter instead (FreeBSD's
    /// `exec_shell_imgact`), with arguments the interpreter, the line's
    /// argument if any, `path`, and `argv` past its first; the
    /// interpreter is then the program (its text, its name), and it may
    /// not be a script itself.
    fn load_image(
        &self,
        s: &Sleepable,
        program: Program<'_>,
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<Loaded, Errno> {
        let lookup =
            |p: &[u8]| crate::fs::lookup(s, self, crate::fs::At::Cwd, p, crate::fs::Walk::FOLLOW);
        // A program run from a descriptor goes by its path if it has one
        // to find, and a script's interpreter is handed `/dev/fd/N`
        // (`imgact_shell`).
        let (mut file, mut path, script_name) = match program {
            Program::Path(p) => {
                if crate::ktrace::cap_tracing() {
                    crate::ktrace::capfail(crate::ktrace::CapFail::Namei(p));
                }
                (lookup(p)?, p.to_vec(), p.to_vec())
            }
            Program::File(v, fd) => {
                let dev_fd = alloc::format!("/dev/fd/{fd}").into_bytes();
                let path = crate::fs::namespace()
                    .path_of(s, &v, &crate::fs::root_of(self))
                    .unwrap_or_else(|_| dev_fd.clone());
                (v, path, dev_fd)
            }
        };
        let mut argv: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
        crate::exec::check_exec(s, &file)?;
        if let Some(sc) = crate::exec::script(s, &file)? {
            // No interpreter in capability mode (`kern_execve`).
            if self.in_capability_mode() {
                return Err(Errno::ECAPMODE);
            }
            let mut new = alloc::vec![sc.interp.clone()];
            new.extend(sc.arg);
            new.push(script_name);
            new.extend(argv.into_iter().skip(1));
            argv = new;
            if crate::ktrace::cap_tracing() {
                crate::ktrace::capfail(crate::ktrace::CapFail::Namei(&sc.interp));
            }
            file = lookup(&sc.interp)?;
            path = sc.interp;
        }
        let (cred, setid) = self.exec_cred(s, &file)?;
        let text = Text::new(&file)?;
        let space = AddressSpace::new().map_err(|_| Errno::ENOMEM)?;
        let limits = *self.limits.lock();
        let args: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
        // A set-id program takes no preferences its caller set (`imgact`).
        if setid {
            self.flag2.fetch_and(
                !(p2::ASLR_ENABLE | p2::ASLR_DISABLE | p2::WXORX_DISABLE | p2::WXORX_ENABLE_EXEC),
                Ordering::Relaxed,
            );
        }
        let flag2 = self.flag2.load(Ordering::Relaxed);
        let image = crate::exec::load(
            s, &space, &file, &path, &args, envp, &limits, flag2, &lookup,
        )?;
        Ok(Loaded {
            cred,
            setid,
            text,
            file,
            space,
            image,
            path,
            argv,
        })
    }

    /// The credential the program `file` runs with (`do_execve`): its
    /// set-uid and set-gid bits make the effective ids its owner's, unless
    /// its mount is `nosuid`, the caller is in capability mode, or it set
    /// `PROC_NO_NEW_PRIVS`; the saved ids become the effective ones
    /// either way. `None` if that changes nothing; the flag is whether the
    /// program runs set-id.
    fn exec_cred(&self, s: &Sleepable, file: &Vnode) -> Result<(Option<Ucred>, bool), Errno> {
        use abi::stat::{S_ISGID, S_ISUID};
        let a = file.attr(s)?;
        let old = self.cred();
        let suid = a.mode & S_ISUID != 0;
        let sgid = a.mode & S_ISGID != 0;
        let changing = (suid && old.uid != a.uid) || (sgid && old.gid != a.gid);
        if changing {
            self.pdeathsig.store(0, Ordering::Relaxed);
        }
        let setid = changing
            && !old.in_capability_mode()
            && file.mount().flags() & abi::mount::MNT_NOSUID == 0
            && self.flag2.load(Ordering::Relaxed) & p2::NO_NEW_PRIVS == 0;
        let mut c = Ucred::clone(&old);
        if setid {
            if suid {
                c.uid = a.uid;
            }
            if sgid {
                c.gid = a.gid;
            }
        } else if old.svuid == old.uid && old.svgid == old.gid {
            return Ok((None, false));
        }
        c.svuid = c.uid;
        c.svgid = c.gid;
        Ok((Some(c), setid))
    }

    /// What thread `t` must do about suspension now.
    pub(crate) fn suspension(&self, t: &UserThread) -> Suspension {
        let job = self.job.lock();
        match job.single {
            // A thread reporting its exit to its debugger waits for it
            // (`TDB_EXIT` stops even then).
            Some(x)
                if x.by != t.tid()
                    && x.mode == Mode::Exit
                    && job.trace
                    && job.stopped != 0
                    && t.dbg.has(crate::ptrace::tdb::EXIT) =>
            {
                Suspension::Stop
            }
            Some(x) if x.by != t.tid() => Suspension::Single(x.mode),
            _ if job.stopped != 0 => Suspension::Stop,
            // Held by its debugger (`PT_SUSPEND`).
            _ if self.is_traced() && t.dbg.has(crate::ptrace::tdb::SUSPEND) => Suspension::Stop,
            _ => Suspension::None,
        }
    }

    /// Parks the current thread `t` until the stop or single-threading
    /// that asked for it is over, or turns into one it must leave for.
    pub(crate) fn park(&self, s: &Sleepable, t: &UserThread) {
        self.park_until(s, t, || false);
    }

    /// [`park`](Self::park), or until `early` holds (a stopped thread's
    /// debugger has a request for it).
    pub(crate) fn park_until(&self, s: &Sleepable, t: &UserThread, early: impl Fn() -> bool) {
        self.job.lock().parked += 1;
        self.suspension_changed();
        self.stop_wait.wait_until(s, || {
            early()
                || matches!(
                    self.suspension(t),
                    Suspension::None | Suspension::Single(Mode::Exit)
                )
        });
        self.job.lock().parked -= 1;
    }

    /// After a thread parked or left: a stop every thread has reached is
    /// reported to the parent (once; FreeBSD's `thread_stopped`), and a
    /// single-threading thread looks again.
    fn suspension_changed(&self) {
        let (told, single) = {
            let mut job = self.job.lock();
            let n = self.threads.lock().len();
            let tell = job.stopped != 0 && !job.stop_told && job.parked >= n;
            if tell {
                job.stop_told = true;
            }
            (
                tell.then_some((job.stopped, job.trace)),
                job.single.is_some(),
            )
        };
        if let Some((sig, trace)) = told {
            // A traced process's stops are its debugger's (`CLD_TRAPPED`).
            let code = if trace || self.is_traced() {
                abi::signal::code::CLD_TRAPPED
            } else {
                abi::signal::code::CLD_STOPPED
            };
            crate::signal::notify_parent(self, code, sig.cast_signed());
        }
        if single {
            self.single_wait.wake_all();
        }
    }

    /// Starts a stop for the debugger with `sig` (`P_STOPPED_TRACE`):
    /// every thread but `t` (the reporting one, which parks itself) is
    /// told to park. With no thread reporting (a transparent attach),
    /// every thread parks; asleep, in place.
    pub(crate) fn begin_trace_stop(&self, t: Option<&UserThread>, sig: u32) {
        {
            let mut job = self.job.lock();
            job.stopped = sig;
            job.trace = true;
            job.stop_report = true;
            job.cont_report = false;
            job.stop_told = false;
        }
        self.each_thread(|o| {
            if t.is_none_or(|t| o.tid() != t.tid()) {
                o.interrupt(crate::signal::AST_SUSPEND);
            }
            true
        });
        // Every thread may be parked already (asleep, then stopped).
        self.suspension_changed();
    }

    /// A stopped thread ran for its debugger and stops again: the stop is
    /// reported again once every thread is stopped (`thread_stopped`).
    pub(crate) fn rearm_stop(&self) {
        let mut job = self.job.lock();
        if job.stopped != 0 {
            job.stop_told = false;
            job.stop_report = true;
        }
    }

    /// Tells the parent the process goes on (`childproc_continued`).
    pub(crate) fn report_continued(&self) {
        self.job.lock().cont_report = true;
        crate::signal::notify_parent(
            self,
            abi::signal::code::CLD_CONTINUED,
            abi::signal::SIGCONT.cast_signed(),
        );
    }

    /// Ends any stop (`ptrace_unsuspend`): every thread goes on, those
    /// its debugger holds excepted.
    pub(crate) fn end_stop(&self) {
        {
            let mut job = self.job.lock();
            job.stopped = 0;
            job.trace = false;
            job.stop_report = false;
            job.stop_told = false;
        }
        self.stop_wait.wake_all();
    }

    /// Starts a stop for `sig` (the caller then parks): every other thread
    /// is told to park too.
    pub(crate) fn begin_stop(&self, t: &UserThread, sig: u32) {
        {
            let mut job = self.job.lock();
            job.stopped = sig;
            job.stop_report = true;
            job.cont_report = false;
            job.stop_told = false;
        }
        self.each_thread(|o| {
            if o.tid() != t.tid() {
                o.interrupt(crate::signal::AST_SUSPEND);
            }
            true
        });
    }

    /// Makes the current thread `t` the process's only running one
    /// (FreeBSD's `thread_single`): the others leave (`Exit`), or park at
    /// the boundary (`Boundary`) until [`Process::end_single`]. A thread
    /// already alone in `Boundary` may move to `Exit`. `ERESTART` if
    /// another thread is single-threading: the caller goes back to the
    /// boundary, where it parks or leaves.
    pub(crate) fn single(&self, s: &Sleepable, t: &UserThread, mode: Mode) -> Result<(), Errno> {
        {
            let mut job = self.job.lock();
            if self.threads.lock().len() == 1 && job.single.is_none() {
                return Ok(());
            }
            match &mut job.single {
                Some(x) if x.by != t.tid() => return Err(Errno::ERESTART),
                Some(x) => x.mode = mode,
                None => job.single = Some(Single { by: t.tid(), mode }),
            }
        }
        // Parked threads look again (to leave, for `Exit`); the others
        // come to the boundary.
        self.stop_wait.wake_all();
        self.each_thread(|o| {
            if o.tid() != t.tid() {
                o.interrupt(crate::signal::AST_SUSPEND);
            }
            true
        });
        // Waiting, it counts as stopped, as FreeBSD's suspends itself in
        // `thread_single`: a debugger's stop of the others is complete.
        self.job.lock().parked += 1;
        self.suspension_changed();
        self.single_wait.wait_until(s, || {
            let parked = self.job.lock().parked;
            let n = self.threads.lock().len();
            match mode {
                Mode::Exit => n == 1,
                Mode::Boundary => parked >= n,
            }
        });
        self.job.lock().parked -= 1;
        if mode == Mode::Exit {
            // Alone for good: nothing is left to hold back.
            self.job.lock().single = None;
        }
        Ok(())
    }

    /// Ends `t`'s single-threading: parked threads go on.
    pub(crate) fn end_single(&self, t: &UserThread) {
        {
            let mut job = self.job.lock();
            if job.single.is_some_and(|x| x.by == t.tid()) {
                job.single = None;
            }
        }
        self.stop_wait.wake_all();
    }

    /// Takes thread `t` out of the process unless it is the last one
    /// (`thr_exit`'s test, made with the list locked so two threads
    /// leaving at once cannot both pass it). Returns whether it was taken.
    pub(crate) fn remove_thread(&self, t: &UserThread, unless_last: bool) -> bool {
        let mut threads = self.threads.lock();
        if unless_last && threads.len() <= 1 {
            return false;
        }
        threads.retain(|o| o.tid() != t.tid());
        true
    }

    /// Ends the current thread `t`, already out of the process's list
    /// ([`Process::remove_thread`]): its signals go, and whoever waits on
    /// the thread count looks again. Takes the caller's references:
    /// nothing on a stack that never unwinds may keep them.
    pub(crate) fn thread_exit(t: Arc<UserThread>) -> ! {
        let p = Arc::clone(t.process());
        p.bank_current_usage();
        crate::signal::thread_gone(&t);
        t.set_sched(None);
        drop(t);
        p.suspension_changed();
        drop(p);
        drop(sched::set_owner(None));
        sched::exit()
    }

    /// Ends the process as `how` says (the caller's thread then exits):
    /// the memory goes now, the children go to init, and the parent gets
    /// `SIGCHLD` (or, if it keeps no zombies, init gets the zombie).
    pub(crate) fn exit(&self, s: &Sleepable, how: Exit) {
        // No debugger stops it any more (`P_TRACED` cleared); one that
        // took it still reaps it first (`p_oppid` stays).
        if self.is_traced() {
            self.traced.store(false, Ordering::Release);
        }
        // Its semaphore adjustments are undone (`semexit`).
        crate::sysv::sem_exited(self.pid);
        // A vfork parent waits on until this is a zombie (below).
        let vforked = self.vforked.load(Ordering::Acquire);
        // This CPU lets go of the space too (others it ran on hold it
        // until they load another).
        sched::set_address_space(None);
        space::unload();
        // The process keeps an empty space; the old one goes now. Dropping
        // its last reference frees the tables and pages at once; if some
        // other CPU still has it loaded, it is emptied first so the memory
        // does not wait for that CPU. (The parent's, after a vfork, stays.)
        match AddressSpace::new() {
            Ok(empty) => {
                let old = core::mem::replace(&mut *self.space.lock(), empty);
                if !vforked && Arc::strong_count(&old) > 1 {
                    old.clear(s);
                }
                drop(old);
            }
            Err(_) if !vforked => self.space().clear(s),
            Err(_) => {}
        }
        // The program may be written again.
        drop(core::mem::take(&mut *self.text.lock()));
        // Descriptors close before the parent hears: a pipe's reader sees
        // the end first.
        // A table another process shares stays theirs (`fdescfree`).
        let closed = if Arc::strong_count(&self.files) == 1 {
            self.files.lock(s).take_all()
        } else {
            Vec::new()
        };
        crate::fs::closed(self.pid, &closed);
        drop(closed);
        crate::ktrace::exit(s);
        if self.pid == self.sid() {
            crate::tty::session_ended(s, self.pid);
        }
        // Its last thread holds the process (through its owner) and the
        // process holds it; this breaks the cycle.
        if let Some(t) = UserThread::current().filter(|t| core::ptr::eq(t.process().as_ref(), self))
        {
            // Its time so far counts, as any thread's that leaves does
            // (`thread_exit`), for `wait`'s usage and the parent's.
            self.bank_current_usage();
            t.set_sched(None);
        }
        let gone = core::mem::take(&mut *self.threads.lock());
        drop(gone);
        // The groups this exit may orphan (its own, its children's):
        // those not orphaned yet.
        let groups: Vec<i32> = {
            let tree = TREE.lock();
            let mut g: Vec<i32> = tree
                .children
                .get(&self.pid)
                .map(|kids| kids.iter().map(|c| c.pgid()).collect())
                .unwrap_or_default();
            g.push(self.pgid());
            g.sort_unstable();
            g.dedup();
            g
        };
        let groups: Vec<i32> = groups.into_iter().filter(|&g| !pgrp_orphaned(g)).collect();
        // Its timers go (`itimers_exit`, and the real one's callout).
        if let Some(t) = self.timers.get() {
            t.exit();
        }
        self.status.store(how.status(), Ordering::Relaxed);
        // Release: the status is visible to whoever sees `exited`.
        self.exited.store(true, Ordering::Release);
        // Its jail may end with it (`prison_proc_free`).
        crate::prison::proc_left(&self.cred());
        crate::event::proc_exited(self);
        let parent = self.parent();
        let reaps_nothing = parent
            .as_ref()
            .is_some_and(|p| crate::signal::parent_reaps_nothing(p));
        // Reapers told of zombies they inherit, and children told their
        // parent died (`p_pdeathsig`), once the tree is unlocked.
        let mut reaper_told: Vec<(Arc<Self>, Arc<Self>)> = Vec::new();
        let mut death_signals: Vec<(Arc<Self>, u32)> = Vec::new();
        // Processes this one traced, to let go once the tree is unlocked.
        let mut untraced: Vec<Arc<Self>> = Vec::new();
        let described = {
            let mut tree = TREE.lock();
            self.zombie.store(true, Ordering::Release);
            // A process descriptor holds this zombie for its parent: no
            // SIGCHLD, and init does not take it.
            let described = self.procdesc.load(Ordering::Acquire);
            let reaps_nothing = reaps_nothing && !described;
            let init = (self.pid != INIT).then(|| find(INIT)).flatten();
            // Each child goes to its reaper (init unless one took the
            // role); those this process reaps for stay until it is
            // reaped itself.
            // Children this process traced go back to their real parents
            // (or their reapers), untraced; those a debugger took from it
            // now answer to their reapers (`exit1`).
            if let Some(taken) = tree.orphans.remove(&self.pid) {
                for pid in taken {
                    if let Some(q) = find(pid) {
                        q.trace.lock().oppid = tree.reaper_of(pid);
                        let pdeathsig = q.pdeathsig.load(Ordering::Relaxed);
                        if pdeathsig != 0 {
                            death_signals.push((q, pdeathsig));
                        }
                    }
                }
            }
            if let Some(orphans) = tree.children.remove(&self.pid) {
                for o in orphans {
                    if o.is_traced() {
                        let to = real_parent(&tree, &o);
                        let to = if to == self.pid {
                            tree.reaper_of(o.pid)
                        } else {
                            to
                        };
                        o.clear_traced();
                        {
                            let mut tr = o.trace.lock();
                            tr.oppid = 0;
                            tr.events = 0;
                            tr.first_stop = false;
                        }
                        tree.reparent(&o, to);
                        untraced.push(o);
                        continue;
                    }
                    let r = tree.reaper_of(o.pid);
                    let pdeathsig = o.pdeathsig.load(Ordering::Relaxed);
                    let zombie = o.zombie.load(Ordering::Acquire);
                    if r == self.pid {
                        tree.children
                            .entry(self.pid)
                            .or_default()
                            .push(Arc::clone(&o));
                    } else if let Some(rp) = find(r).filter(|_| r != INIT || init.is_some()) {
                        tree.reparent(&o, rp.pid);
                        if zombie {
                            reaper_told.push((rp, Arc::clone(&o)));
                        }
                    } else {
                        tree.parent.remove(&o.pid);
                    }
                    if !zombie && pdeathsig != 0 {
                        death_signals.push((o, pdeathsig));
                    }
                }
            }
            if reaps_nothing && let Some(parent) = &parent {
                // FreeBSD's `PS_NOCLDWAIT`: init reaps this one.
                let me = tree.children.get_mut(&parent.pid).and_then(|kids| {
                    kids.iter()
                        .position(|c| c.pid == self.pid)
                        .map(|i| kids.remove(i))
                });
                match (&init, me) {
                    (Some(init), Some(me)) => {
                        tree.parent.insert(self.pid, init.pid);
                        tree.children.entry(init.pid).or_default().push(me);
                    }
                    _ => {
                        tree.parent.remove(&self.pid);
                    }
                }
            }
            described
        };
        // The vfork parent goes on once this is a zombie, as FreeBSD's
        // wakes it under its lock with the state set: a `waitpid` with
        // `WNOHANG` right after finds it (`posix_spawn` reaps a child that
        // failed so).
        if vforked
            && self.vforked.swap(false, Ordering::AcqRel)
            && let Some(p) = &parent
        {
            p.child_gen.fetch_add(1, Ordering::AcqRel);
            p.child_wait.wake_all();
        }
        if let Some(init) = find(INIT) {
            init.child_changed();
        }
        // A process it traced forgets its debugger, and is killed
        // (`kern.kill_on_debugger_exit`), or else goes on if stopped,
        // the stop's `SIGTRAP` gone.
        for q in untraced {
            crate::ptrace::forget(&q);
            if crate::ptrace::KILL_ON_DEBUGGER_EXIT.load(Ordering::Relaxed) {
                q.job.lock().trace = false;
                let _ = crate::signal::post(
                    &q,
                    crate::signal::user_info(abi::signal::SIGKILL, 0, abi::signal::code::SI_KERNEL),
                );
            } else if q.job.lock().stopped != 0 {
                crate::signal::discard_process(&q, abi::signal::SIGTRAP);
                q.end_stop();
            }
        }
        // Each with the zombie's own exit, as its parent would have been
        // (`childproc_exited` as `proc_reparent` hands it over).
        for (r, z) in reaper_told {
            let st = z.status.load(Ordering::Relaxed);
            let (code, status) = if st & 0x7f == 0 {
                (CLD_EXITED, (st >> 8) & 0xff)
            } else {
                (CLD_KILLED, st & 0x7f)
            };
            let _ = crate::signal::post(
                &r,
                abi::signal::Siginfo {
                    si_signo: abi::signal::SIGCHLD.cast_signed(),
                    si_code: code,
                    si_pid: z.pid,
                    si_status: status,
                    ..abi::signal::Siginfo::default()
                },
            );
            r.child_changed();
        }
        for (c, sig) in death_signals {
            let _ = crate::signal::post(
                &c,
                crate::signal::user_info(sig, 0, abi::signal::code::SI_KERNEL),
            );
        }
        // A group this exit orphaned, with stopped members, is told to
        // hang up and continue: nothing could ever continue it otherwise
        // (FreeBSD's `orphanpg`).
        for g in groups {
            let stopped = all()
                .iter()
                .any(|p| p.pgid() == g && p.job.lock().stopped != 0);
            if stopped && pgrp_orphaned(g) {
                crate::signal::post_group(g, abi::signal::SIGHUP);
                crate::signal::post_group(g, abi::signal::SIGCONT);
            }
        }
        if let Some(parent) = parent {
            if described || reaps_nothing {
                parent.child_changed();
            } else {
                let (code, status) = match how {
                    Exit::Code(c) => (CLD_EXITED, c),
                    Exit::Signal(sig) => (CLD_KILLED, sig.cast_signed()),
                    Exit::Dumped(sig) => (abi::signal::code::CLD_DUMPED, sig.cast_signed()),
                };
                crate::signal::notify_parent(self, code, status);
            }
        }
        self.waiters.wake_all();
    }

    /// Ends the current thread's process as `how` says, and the thread with
    /// it; first every other thread leaves (FreeBSD's `exit1`). Callers
    /// hold no references: nothing on a stack that never unwinds may keep
    /// them.
    pub(crate) fn exit_current(s: &Sleepable, how: Exit) -> ! {
        loop {
            let alone = UserThread::with_current(|t| t.process().single(s, t, Mode::Exit));
            match alone {
                Some(Err(_)) => crate::signal::suspend_check(s),
                _ => break,
            }
        }
        // Alone now: its robust mutexes go as at any thread's exit
        // (`exit_onexit`).
        UserThread::with_current(|t| crate::umtx::thread_exit(s, t));
        if let Some(p) = Self::current() {
            p.exit(s, how);
        }
        drop(sched::set_owner(None));
        sched::exit()
    }

    /// Waits for the process to exit (kernel code); returns how it ended.
    pub(crate) fn wait(&self, s: &Sleepable) -> Exit {
        self.waiters
            .wait_until(s, || self.exited.load(Ordering::Acquire));
        Exit::from_status(self.status.load(Ordering::Relaxed))
    }

    /// The parent, if it is still around.
    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        let ppid = TREE.lock().parent.get(&self.pid).copied()?;
        find(ppid)
    }

    /// A child exited, stopped, or continued: wakes `wait4`.
    pub(crate) fn child_changed(&self) {
        self.child_gen.fetch_add(1, Ordering::AcqRel);
        self.child_wait.wake_all();
    }

    /// Where exec put the randomized parts of the image.
    pub(crate) fn layout(&self) -> Layout {
        self.layout.get()
    }

    /// Ends a stop (for `SIGCONT`, reported to `wait4(WCONTINUED)` if
    /// `report`). Returns whether the process was stopped.
    pub(crate) fn resume(&self, report: bool) -> bool {
        {
            let mut job = self.job.lock();
            if job.stopped == 0 {
                return false;
            }
            job.stopped = 0;
            job.stop_report = false;
            job.cont_report = report;
        }
        self.stop_wait.wake_all();
        true
    }

    /// `wait4`'s search: a child that exited (or, as `options` asks,
    /// stopped or continued), among those `pid` selects (a pid, -1 for
    /// any, 0 for this group, below -1 for that group), with its status.
    /// `Ok(None)` with `WNOHANG` when none has. Interrupted by a signal:
    /// `EINTR` or `ERESTART`. With `WTRAPPED`, a traced child's stop for
    /// this debugger too ([`Waited::trapped`]).
    pub(crate) fn wait_child(
        &self,
        s: &Sleepable,
        pid: i32,
        options: u32,
    ) -> Result<Option<Waited>, Errno> {
        let selects = |c: &Process| match pid {
            // Children with a process descriptor answer only to their pid
            // or group, but to a debugger tracing them (`proc_to_reap`).
            -1 => !c.procdesc.load(Ordering::Acquire) || c.is_traced(),
            0 => c.pgid() == self.pgid(),
            p if p > 0 => c.pid == p,
            p => c.pgid() == -p,
        };
        loop {
            let generation = self.child_gen.load(Ordering::Acquire);
            {
                let mut tree = TREE.lock();
                let kids = tree.children.entry(self.pid).or_default();
                if !kids.iter().any(|c| selects(c)) {
                    // Its children a debugger took are still waited for
                    // (`p_orphans`).
                    let orphaned = tree.orphans.get(&self.pid).is_some_and(|o| {
                        o.iter().any(|&pid| find(pid).is_some_and(|c| selects(&c)))
                    });
                    if !orphaned {
                        return Err(Errno::ECHILD);
                    }
                } else if let Some(i) = kids
                    .iter()
                    .position(|c| selects(c) && c.exited.load(Ordering::Acquire))
                {
                    let c = Arc::clone(&kids[i]);
                    let status = c.status.load(Ordering::Relaxed);
                    if options & WNOWAIT == 0 {
                        // A child the debugger took goes back to its real
                        // parent, which waits for it in turn (`proc_reap`).
                        let oppid = c.trace.lock().oppid;
                        if oppid != 0 && oppid != self.pid {
                            let to = real_parent(&tree, &c);
                            c.trace.lock().oppid = 0;
                            tree.reparent(&c, to);
                            drop(tree);
                            if let Some(rp) = find(to) {
                                crate::signal::notify_exit(&rp, &c);
                            }
                            return Ok(Some(Waited::exited(&c, status)));
                        }
                        kids.remove(i);
                        tree.parent.remove(&c.pid);
                        tree.clear_orphan(c.pid);
                        tree.reaped(c.pid);
                        drop(tree);
                        let w = Waited::exited(&c, status);
                        crate::kinfo::reaped(self, &c);
                        crate::event::proc_reaped(&c);
                        return Ok(Some(w));
                    }
                    return Ok(Some(Waited::exited(&c, status)));
                }
                let kids = tree.children.entry(self.pid).or_default();
                let mut alive = None;
                for c in kids.iter().filter(|c| selects(c)) {
                    let mut job = c.job.lock();
                    // A traced child's stop, once every thread is stopped
                    // (`wait6_check_alive`'s `WTRAPPED`).
                    if options & WTRAPPED != 0
                        && c.is_traced()
                        && job.stopped != 0
                        && job.stop_told
                        && job.stop_report
                    {
                        if options & WNOWAIT == 0 {
                            job.stop_report = false;
                        }
                        alive = Some(Waited {
                            trapped: true,
                            ..Waited::new(c.pid, (job.stopped.cast_signed() << 8) | 0o177)
                        });
                        break;
                    }
                    if options & WUNTRACED != 0 && job.stopped != 0 && job.stop_report {
                        if options & WNOWAIT == 0 {
                            job.stop_report = false;
                        }
                        // WIFSTOPPED: the signal, then 0177.
                        alive = Some(Waited::new(c.pid, (job.stopped.cast_signed() << 8) | 0o177));
                        break;
                    }
                    if options & WCONTINUED != 0 && job.cont_report {
                        if options & WNOWAIT == 0 {
                            job.cont_report = false;
                        }
                        // WIFCONTINUED: SIGCONT's number.
                        alive = Some(Waited::new(c.pid, abi::signal::SIGCONT.cast_signed()));
                        break;
                    }
                }
                if let Some(w) = alive {
                    drop(tree);
                    // The child's `SIGCHLD` for what was just reported goes
                    // too (`report_alive_proc`, `kern.wait_dequeue_sigchld`).
                    if options & WNOWAIT == 0
                        && crate::signal::WAIT_DEQUEUE_SIGCHLD.load(Ordering::Relaxed)
                    {
                        crate::signal::dequeue_child(self, w.pid);
                    }
                    return Ok(Some(w));
                }
            }
            if options & WNOHANG != 0 {
                return Ok(None);
            }
            let _w = sched::wait_message("wait");
            crate::signal::sleep(
                s,
                &self.child_wait,
                || self.child_gen.load(Ordering::Acquire) != generation,
                None,
            )?;
        }
    }

    /// Makes this process the leader of a new session and group
    /// (`setsid`); fails for a group leader.
    pub(crate) fn setsid(&self) -> Result<i32, Errno> {
        let leads = self.pgid() == self.pid || all().iter().any(|p| p.pgid() == self.pid);
        if leads {
            return Err(Errno::EPERM);
        }
        self.pgid.store(self.pid, Ordering::Relaxed);
        self.sid.store(self.pid, Ordering::Relaxed);
        // A new session starts with its leader's login name (FreeBSD's
        // `enterpgrp` copies `s_login`).
        let login = self.login();
        *self.session.lock() = Arc::new(Session {
            login: SpinLock::new(login),
        });
        Ok(self.pid)
    }

    /// Moves `pid` (this process or a child) to group `pgid` in this
    /// session (`setpgid`).
    pub(crate) fn setpgid(&self, pid: i32, pgid: i32) -> Result<(), Errno> {
        let target = if pid == 0 || pid == self.pid {
            find(self.pid).ok_or(Errno::ESRCH)?
        } else {
            let is_child = TREE.lock().parent.get(&pid) == Some(&self.pid);
            if !is_child {
                return Err(Errno::ESRCH);
            }
            find(pid).ok_or(Errno::ESRCH)?
        };
        if target.sid() != self.sid() {
            return Err(Errno::EPERM);
        }
        if target.pid == target.sid() {
            return Err(Errno::EPERM);
        }
        let pgid = if pgid == 0 { target.pid } else { pgid };
        if pgid < 0 {
            return Err(Errno::EINVAL);
        }
        if pgid != target.pid {
            // Joining a group: it must exist in this session.
            let exists = all()
                .iter()
                .any(|p| p.pgid() == pgid && p.sid() == self.sid());
            if !exists {
                return Err(Errno::EPERM);
            }
        }
        target.pgid.store(pgid, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // See `all`: nothing holds the table's lock while a reference
        // drops.
        PROCS.lock().remove(&self.pid);
    }
}

/// A forked child's view of the system call: it returns 0, with
/// FreeBSD's second value (1 on amd64) and success.
fn arch_fork_return(f: &mut TrapFrame) {
    #[cfg(target_arch = "x86_64")]
    {
        f.rax = 0;
        f.rdx = 1;
        f.rflags &= !1;
    }
    #[cfg(target_arch = "aarch64")]
    {
        f.x[0] = 0;
        f.x[1] = 0;
        f.spsr &= !(1 << 29);
    }
}

/// A process descriptor (`pdfork`): the file behind the descriptor.
/// Dropping it is closing the last descriptor.
pub(crate) struct ProcDesc {
    process: Arc<Process>,
    daemon: bool,
}

impl ProcDesc {
    pub(crate) fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// `fstat`: a regular file, owner-accessible while the process lives;
    /// its times are when it started (unrecorded yet: zero).
    pub(crate) fn stat(&self) -> abi::stat::Stat {
        let alive = !self.process.zombie.load(Ordering::Acquire);
        abi::stat::Stat {
            st_mode: abi::stat::S_IFREG | if alive { 0o700 } else { 0 },
            ..abi::stat::Stat::default()
        }
    }
}

impl Drop for ProcDesc {
    fn drop(&mut self) {
        let p = &self.process;
        let kill = {
            let mut tree = TREE.lock();
            p.procdesc.store(false, Ordering::Release);
            if p.zombie.load(Ordering::Acquire) {
                // Reaped now, unless its parent already collected it.
                if let Some(ppid) = tree.parent.remove(&p.pid)
                    && let Some(kids) = tree.children.get_mut(&ppid)
                {
                    kids.retain(|c| c.pid != p.pid);
                }
                tree.reaped(p.pid);
                false
            } else if p.is_traced() {
                // Its debugger keeps it; its reaper is its real parent from
                // now on, the one it goes back to (`procdesc_close`).
                let r = tree.reaper_of(p.pid);
                tree.clear_orphan(p.pid);
                p.trace.lock().oppid = r;
                tree.orphans.entry(r).or_default().push(p.pid);
                !self.daemon
            } else {
                // To its reaper (init unless one took the role), which
                // reaps it when it goes (`procdesc_close`).
                let r = tree.reaper_of(p.pid);
                if tree.parent.get(&p.pid) != Some(&r)
                    && let Some(rp) = find(r)
                {
                    tree.reparent(p, r);
                    drop(tree);
                    rp.child_changed();
                }
                !self.daemon
            }
        };
        if kill {
            let _ = crate::signal::post(
                p,
                crate::signal::user_info(abi::signal::SIGKILL, 0, abi::signal::code::SI_KERNEL),
            );
        }
        // Whoever polls the descriptor finds it closed (`procdesc_close`'s
        // `seldrain`).
        p.pd_sel.drain();
    }
}

/// A new user thread's first code: joins its process and leaves for user
/// mode. Nothing on this stack outlives the jump.
fn enter(s: &Sleepable, arg: usize) -> ! {
    // SAFETY: `start` made `arg` with `Box::into_raw` and handed it over.
    let start = unsafe { alloc::boxed::Box::from_raw(arg as *mut Start) };
    let Start {
        thread,
        frame,
        tls,
        context,
    } = *start;
    sched::set_address_space(Some(
        thread.process().space() as Arc<dyn sched::AddressSpace>
    ));
    // Signals posted from here on interrupt the thread; ones posted before,
    // and a stop or single-threading under way, are looked at below.
    thread.set_sched(Some(sched::current_thread()));
    sched::set_owner(Some(thread));
    sched::current().request_ast(crate::signal::AST_SUSPEND);
    UserThread::with_current(crate::signal::recheck);
    let mut frame = frame;
    // SAFETY: this thread runs in its process's space; the kernel does not
    // use the thread registers.
    unsafe {
        hal::user::set_tls(tls[0]);
        hal::user::set_tls2(tls[1]);
    }
    // A new thread's first return to user mode is a debugger's event
    // (`fork_return`), its registers where the debugger finds them.
    UserThread::with_current(|t| t.dbg.entered(&mut frame));
    crate::ptrace::thread_start(s);
    // A new thread's creating call returns in it too (`fork_return`).
    if crate::ktrace::point(abi::ktrace::KTR_SYSRET) {
        let code = UserThread::with_current(UserThread::syscall_code).unwrap_or(0);
        crate::ktrace::sysret(s, code, 0, 0);
    }
    crate::ktrace::user_return(s);
    let _ = hal::irq::disable();
    if let Some(c) = context {
        c.load();
    }
    crate::signal::user_return(&mut frame);
    // SAFETY: the thread now runs in the process's space, its kernel stack
    // is set (`set_address_space` loads it), interrupts are masked, and
    // everything this function owned was moved or dropped.
    unsafe { arch::user::enter_user(&frame) }
}

/// Where a test process's stack ends (just below the top of the user
/// half, as FreeBSD's `USRSTACK` is).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) const STACK_TOP: u64 = space::USER_END - (1 << 21);

/// A descriptor table of its own.
fn own(t: FdTable) -> Arc<Mutex<FdTable>> {
    Arc::new(Mutex::new(&FILEDESC_LOCK, t))
}
