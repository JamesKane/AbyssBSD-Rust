// SPDX-License-Identifier: BSD-2-Clause
//! Processes: an address space and the threads that run in it, in a tree
//! of parents and children (FreeBSD's `struct proc`).
//!
//! A process has one thread so far. It has a pid from the process table,
//! a process group and session, the signal state system calls set
//! (delivered from P4.4), resource limits, and a randomized base for
//! `mmap` placement.
//!
//! The tree is one map under one lock (FreeBSD's `proctree_lock`): each
//! process's parent, and each parent's children, which it holds until it
//! reaps them. A process that exits becomes a zombie holding its status;
//! its children go to init (pid 1), and its parent's waiters wake. A
//! parent's `wait4` sleeps on a generation count its children's changes
//! bump, so it never takes the tree lock inside a wait-queue condition.
//!
//! Behavior reference: sys/kern/kern_fork.c, sys/kern/kern_exit.c,
//! sys/kern/kern_exec.c.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::resource::{RLIM_INFINITY, Rlimit, WNOHANG, WNOWAIT, exit_status, rlimit};
use abi::signal::{SIG_DFL, SIG_IGN, SIG_MAXSIG, Sigaction, Sigset};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;

use crate::arch;
use crate::arch::trap::TrapFrame;
use crate::mm::space::{self, AddressSpace};

/// The highest pid (`PID_MAX`); allocation wraps to 2 past it.
const PID_MAX: i32 = 99_999;
/// init's pid, which inherits orphans.
const INIT: i32 = 1;

/// Live processes by pid.
static PROCS: SpinLock<BTreeMap<i32, Weak<Process>>> = SpinLock::new(BTreeMap::new());
/// The pid handed out last.
static LAST_PID: AtomicI32 = AtomicI32::new(0);

/// Parents and children.
struct Tree {
    parent: BTreeMap<i32, i32>,
    children: BTreeMap<i32, Vec<Arc<Process>>>,
}

static TREE: SpinLock<Tree> = SpinLock::new(Tree {
    parent: BTreeMap::new(),
    children: BTreeMap::new(),
});

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

/// Signal dispositions and the blocked set (delivery comes with P4.4).
#[derive(Clone)]
pub(crate) struct Signals {
    /// Indexed by signal number minus one.
    pub(crate) actions: [Sigaction; SIG_MAXSIG as usize],
    /// Blocked signals (per thread once processes have several).
    pub(crate) mask: Sigset,
    /// The `sigfastblock` word's address, 0 for none.
    pub(crate) fastblock: u64,
}

/// FreeBSD's default limits, as `ulimit -a` shows them on a 15.1 system,
/// except the stack, which matches what exec maps.
fn default_limits() -> [Rlimit; rlimit::NLIMITS] {
    let inf = Rlimit {
        rlim_cur: RLIM_INFINITY,
        rlim_max: RLIM_INFINITY,
    };
    let mut l = [inf; rlimit::NLIMITS];
    let stack = crate::exec::STACK_SIZE;
    l[rlimit::STACK as usize] = Rlimit {
        rlim_cur: stack,
        rlim_max: stack,
    };
    l[rlimit::CORE as usize] = inf;
    l[rlimit::NOFILE as usize] = Rlimit {
        rlim_cur: 116_964,
        rlim_max: 116_964,
    };
    l[rlimit::NPROC as usize] = Rlimit {
        rlim_cur: 6_000,
        rlim_max: 6_000,
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
    /// Where `mmap` without an address starts looking (randomized).
    mmap_base: AtomicU64,
    pub(crate) signals: SpinLock<Signals>,
    pub(crate) limits: SpinLock<[Rlimit; rlimit::NLIMITS]>,
    /// The `exterrctl` buffer, 0 for none.
    pub(crate) exterr: AtomicU64,
    /// The TLS base `sysarch` recorded (amd64), `u64::MAX` for none. Per
    /// thread once processes have several (P6).
    tls_base: AtomicU64,
    pgid: AtomicI32,
    sid: AtomicI32,
    /// The exit status, as `wait` reports it, valid once `exited` is set.
    status: AtomicI32,
    exited: AtomicBool,
    /// Kernel code waiting for this process to exit (it may have no
    /// parent).
    waiters: WaitQueue,
    /// Bumped when a child exits or stops borrowing the address space;
    /// `wait4` and `vfork` sleep on it.
    child_gen: AtomicU64,
    child_wait: WaitQueue,
}

/// How a process's thread enters user mode.
struct Start {
    process: Arc<Process>,
    frame: TrapFrame,
    tls: u64,
}

fn random_mmap_base() -> u64 {
    let mut r = [0u8; 8];
    // Unseeded (early boot) leaves the base unrandomized.
    let _ = crate::random::fill(&mut r);
    // 64 GiB up, plus up to 8 TiB in 2 MiB steps.
    (64 << 30) + (u64::from_le_bytes(r) % (1 << 22)) * (2 << 20)
}

impl Process {
    /// A process running in `space`, with no threads yet: a child of
    /// `parent` (inheriting its signal state, limits, group and session),
    /// or a new session's leader.
    fn create(space: Arc<AddressSpace>, parent: Option<&Arc<Self>>) -> Arc<Self> {
        let (signals, limits) = match parent {
            Some(p) => (p.signals.lock().clone(), *p.limits.lock()),
            None => (
                Signals {
                    actions: [Sigaction::default(); SIG_MAXSIG as usize],
                    mask: Sigset::default(),
                    fastblock: 0,
                },
                default_limits(),
            ),
        };
        let p = Arc::new_cyclic(|me| {
            let pid = alloc_pid(me);
            Self {
                pid,
                space: SpinLock::new(space),
                vforked: AtomicBool::new(false),
                mmap_base: AtomicU64::new(
                    parent.map_or_else(random_mmap_base, |p| p.mmap_base.load(Ordering::Relaxed)),
                ),
                signals: SpinLock::new(signals),
                limits: SpinLock::new(limits),
                exterr: AtomicU64::new(parent.map_or(0, |p| p.exterr.load(Ordering::Relaxed))),
                tls_base: AtomicU64::new(
                    parent.map_or(u64::MAX, |p| p.tls_base.load(Ordering::Relaxed)),
                ),
                pgid: AtomicI32::new(parent.map_or(pid, |p| p.pgid())),
                sid: AtomicI32::new(parent.map_or(pid, |p| p.sid())),
                status: AtomicI32::new(0),
                exited: AtomicBool::new(false),
                waiters: WaitQueue::new(),
                child_gen: AtomicU64::new(0),
                child_wait: WaitQueue::new(),
            }
        });
        if let Some(parent) = parent {
            let mut tree = TREE.lock();
            tree.parent.insert(p.pid, parent.pid);
            tree.children
                .entry(parent.pid)
                .or_default()
                .push(Arc::clone(&p));
        }
        p
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

    /// The parent's pid (1 for an orphan, 0 for a process the kernel
    /// started).
    pub(crate) fn ppid(&self) -> i32 {
        TREE.lock().parent.get(&self.pid).copied().unwrap_or(0)
    }

    pub(crate) fn mmap_base(&self) -> u64 {
        self.mmap_base.load(Ordering::Relaxed)
    }

    /// The TLS base `sysarch(AMD64_SET_TLSBASE)` recorded.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub(crate) fn tls_base(&self) -> Option<u64> {
        let v = self.tls_base.load(Ordering::Relaxed);
        (v != u64::MAX).then_some(v)
    }

    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    pub(crate) fn set_tls_base(&self, v: Option<u64>) {
        self.tls_base
            .store(v.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// The process the current thread runs for.
    pub(crate) fn current() -> Option<Arc<Self>> {
        sched::owner()?.downcast::<Self>().ok()
    }

    /// A new process running the memory-disk program at `path`, with
    /// arguments `argv` and environment `envp`, started by the kernel (no
    /// parent: a session leader, as init is); its thread is bound to
    /// `cpu` if given.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // boot-to-shell (P4.7)
    pub(crate) fn spawn(
        s: &Sleepable,
        path: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
        cpu: Option<hal::CpuId>,
    ) -> Result<Arc<Self>, Errno> {
        let space = AddressSpace::new().map_err(|_| Errno::ENOMEM)?;
        let image = crate::exec::load(s, &space, path, argv, envp)?;
        let p = Self::create(space, None);
        let frame = arch::user::user_frame(image.entry, image.sp, image.arg);
        p.start(frame, 0, false, cpu).map_err(|_| Errno::EAGAIN)?;
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
        let p = Self::create(space, None);
        let frame = arch::user::user_frame(entry, sp, 0);
        p.start(frame, 0, false, cpu)
            .expect("a thread for the test process");
        p
    }

    /// Starts the process's thread with `frame`'s registers and user
    /// thread pointer `tls`; `fork` gives it a copy of the caller's FP
    /// state.
    fn start(
        self: &Arc<Self>,
        frame: TrapFrame,
        tls: u64,
        fork: bool,
        cpu: Option<hal::CpuId>,
    ) -> Result<(), sched::SpawnError> {
        let start = Arc::new(Start {
            process: Arc::clone(self),
            frame,
            tls,
        });
        let arg = Arc::into_raw(start) as usize;
        let mut b = sched::Builder::new("user").priority(sched::Priority::TIMESHARE);
        b = if fork { b.fpu_inherit() } else { b.fpu() };
        if let Some(cpu) = cpu {
            b = b.bind(cpu);
        }
        b.spawn_raw(enter, arg).map(drop).inspect_err(|_| {
            // SAFETY: the thread never started, so `arg` is still ours.
            drop(unsafe { Arc::from_raw(arg as *const Start) });
        })
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
        let space = if vfork {
            self.space()
        } else {
            self.space().fork(s).map_err(|_| Errno::ENOMEM)?
        };
        let child = Self::create(space, Some(self));
        child.vforked.store(vfork, Ordering::Relaxed);
        let mut cf = *frame;
        arch_fork_return(&mut cf);
        if child.start(cf, hal::user::tls(), true, None).is_err() {
            child.detach();
            return Err(Errno::EAGAIN);
        }
        if vfork {
            self.child_wait
                .wait_until(s, || !child.vforked.load(Ordering::Acquire));
        }
        Ok(child.pid)
    }

    /// Takes a child that could not start out of the tree.
    fn detach(&self) {
        let mut tree = TREE.lock();
        if let Some(ppid) = tree.parent.remove(&self.pid)
            && let Some(kids) = tree.children.get_mut(&ppid)
        {
            kids.retain(|c| c.pid != self.pid);
        }
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

    /// Replaces the image with the memory-disk program at `path` (the
    /// current thread is this process's): a new address space, caught
    /// signals reset, and `frame` set to start the new program.
    pub(crate) fn exec(
        &self,
        s: &Sleepable,
        frame: &mut TrapFrame,
        path: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<(), Errno> {
        let space = AddressSpace::new().map_err(|_| Errno::ENOMEM)?;
        let image = crate::exec::load(s, &space, path, argv, envp)?;
        // No way back from here.
        let old = core::mem::replace(&mut *self.space.lock(), Arc::clone(&space));
        sched::set_address_space(Some(space as Arc<dyn sched::AddressSpace>));
        if self.vforked.load(Ordering::Acquire) {
            // The old space is the parent's.
            self.release_parent();
        } else {
            old.clear(s);
        }
        drop(old);
        {
            let mut sigs = self.signals.lock();
            for a in &mut sigs.actions {
                if a.sa_handler != SIG_IGN {
                    *a = Sigaction {
                        sa_handler: SIG_DFL,
                        ..Sigaction::default()
                    };
                }
            }
            sigs.fastblock = 0;
        }
        self.exterr.store(0, Ordering::Relaxed);
        self.set_tls_base(None);
        // SAFETY: this thread runs in the new image; the kernel does not
        // use the thread pointer.
        unsafe { hal::user::set_tls(0) };
        self.mmap_base.store(random_mmap_base(), Ordering::Relaxed);
        *frame = arch::user::user_frame(image.entry, image.sp, image.arg);
        Ok(())
    }

    /// Exits with `code` (the caller's thread then exits): the memory
    /// goes now, the children go to init, and the parent (or the kernel)
    /// is told.
    pub(crate) fn exit(&self, s: &Sleepable, code: i32) {
        if self.vforked.load(Ordering::Acquire) {
            self.release_parent();
        } else {
            self.space().clear(s);
        }
        // This CPU lets go of the space too (others it ran on hold it
        // until they load another).
        sched::set_address_space(None);
        space::unload();
        self.status.store(exit_status(code), Ordering::Relaxed);
        // Release: the status is visible to whoever sees `exited`.
        self.exited.store(true, Ordering::Release);
        let parent = {
            let mut tree = TREE.lock();
            if let Some(orphans) = tree.children.remove(&self.pid) {
                let init = (self.pid != INIT).then(|| find(INIT)).flatten();
                for o in orphans {
                    match &init {
                        Some(init) => {
                            tree.parent.insert(o.pid, init.pid);
                            tree.children.entry(init.pid).or_default().push(o);
                        }
                        None => {
                            tree.parent.remove(&o.pid);
                        }
                    }
                }
            }
            tree.parent.get(&self.pid).copied()
        };
        if let Some(init) = find(INIT) {
            init.child_gen.fetch_add(1, Ordering::AcqRel);
            init.child_wait.wake_all();
        }
        if let Some(parent) = parent.and_then(find) {
            parent.child_gen.fetch_add(1, Ordering::AcqRel);
            parent.child_wait.wake_all();
        }
        self.waiters.wake_all();
    }

    /// Waits for the process to exit (kernel code); returns its exit code.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // boot-to-shell (P4.7)
    pub(crate) fn wait(&self, s: &Sleepable) -> i32 {
        self.waiters
            .wait_until(s, || self.exited.load(Ordering::Acquire));
        self.status.load(Ordering::Relaxed) >> 8
    }

    /// `wait4`'s search: a child that exited, among those `pid` selects
    /// (a pid, -1 for any, 0 for this group, below -1 for that group).
    /// `Ok(None)` with `WNOHANG` when none has exited yet.
    pub(crate) fn wait_child(
        &self,
        s: &Sleepable,
        pid: i32,
        options: u32,
    ) -> Result<Option<(i32, i32)>, Errno> {
        let selects = |c: &Process| match pid {
            -1 => true,
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
                    return Err(Errno::ECHILD);
                }
                if let Some(i) = kids
                    .iter()
                    .position(|c| selects(c) && c.exited.load(Ordering::Acquire))
                {
                    let c = Arc::clone(&kids[i]);
                    if options & WNOWAIT == 0 {
                        kids.remove(i);
                        tree.parent.remove(&c.pid);
                    }
                    drop(tree);
                    return Ok(Some((c.pid, c.status.load(Ordering::Relaxed))));
                }
            }
            if options & WNOHANG != 0 {
                return Ok(None);
            }
            self.child_wait
                .wait_until(s, || self.child_gen.load(Ordering::Acquire) != generation);
        }
    }

    /// Makes this process the leader of a new session and group
    /// (`setsid`); fails for a group leader.
    pub(crate) fn setsid(&self) -> Result<i32, Errno> {
        let procs = PROCS.lock();
        let leads = self.pgid() == self.pid
            || procs
                .values()
                .filter_map(Weak::upgrade)
                .any(|p| p.pgid() == self.pid);
        if leads {
            return Err(Errno::EPERM);
        }
        self.pgid.store(self.pid, Ordering::Relaxed);
        self.sid.store(self.pid, Ordering::Relaxed);
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
            let exists = PROCS
                .lock()
                .values()
                .filter_map(Weak::upgrade)
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

/// A new user thread's first code: joins its process and leaves for user
/// mode. Nothing on this stack outlives the jump.
fn enter(_s: &Sleepable, arg: usize) -> ! {
    // SAFETY: `start` made `arg` with `Arc::into_raw` and handed it over.
    let start = unsafe { Arc::from_raw(arg as *const Start) };
    let process = Arc::clone(&start.process);
    let frame = start.frame;
    let tls = start.tls;
    drop(start);
    sched::set_address_space(Some(process.space() as Arc<dyn sched::AddressSpace>));
    sched::set_owner(Some(process));
    let _ = hal::irq::disable();
    // SAFETY: this thread runs in its process's space; the kernel does not
    // use the thread pointer.
    unsafe { hal::user::set_tls(tls) };
    // SAFETY: the thread now runs in the process's space, its kernel stack
    // is set (`set_address_space` loads it), interrupts are masked, and
    // everything this function owned was moved or dropped.
    unsafe { arch::user::enter_user(&frame) }
}

/// Where a test process's stack ends (just below the top of the user
/// half, as FreeBSD's `USRSTACK` is).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) const STACK_TOP: u64 = space::USER_END - (1 << 21);
