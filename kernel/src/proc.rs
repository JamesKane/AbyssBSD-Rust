// SPDX-License-Identifier: BSD-2-Clause
//! Processes: an address space and the threads that run in it, in a tree
//! of parents and children (FreeBSD's `struct proc`).
//!
//! A process has one thread so far. It has a pid from the process table,
//! a process group and session, its signal state, resource limits, a
//! randomized base for `mmap` placement, its descriptor table, and its
//! working directory.
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

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::resource::{RLIM_INFINITY, Rlimit, WNOHANG, WNOWAIT, exit_status, rlimit};
use abi::resource::{WCONTINUED, WUNTRACED};
use abi::signal::code::{CLD_EXITED, CLD_KILLED};
use sched::lock::{LockClass, Mutex};
use sched::{Sleepable, WaitQueue};
use sync::{PreemptLock, SpinLock};
use vfs::Vnode;

use crate::arch;
use crate::arch::trap::TrapFrame;
use crate::cred::Ucred;
use crate::exec::Layout;
use crate::fs::fdtable::FdTable;
use crate::mm::space::{self, AddressSpace};
use crate::signal::Signals;

/// The highest pid (`PID_MAX`); allocation wraps to 2 past it.
pub(crate) const PID_MAX: i32 = 99_999;
/// init's pid, which inherits orphans.
const INIT: i32 = 1;

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
}

impl SharedLayout {
    fn new(l: Layout) -> Self {
        Self {
            anon_loc: AtomicU64::new(l.anon_loc),
            stack_top: AtomicU64::new(l.stack_top),
            shared_page: AtomicU64::new(l.shared_page),
            osrel: AtomicI32::new(l.osrel),
            fctl0: AtomicU32::new(l.fctl0),
        }
    }

    fn get(&self) -> Layout {
        Layout {
            anon_loc: self.anon_loc.load(Ordering::Relaxed),
            stack_top: self.stack_top.load(Ordering::Relaxed),
            shared_page: self.shared_page.load(Ordering::Relaxed),
            osrel: self.osrel.load(Ordering::Relaxed),
            fctl0: self.fctl0.load(Ordering::Relaxed),
        }
    }

    fn set(&self, l: Layout) {
        self.anon_loc.store(l.anon_loc, Ordering::Relaxed);
        self.stack_top.store(l.stack_top, Ordering::Relaxed);
        self.shared_page.store(l.shared_page, Ordering::Relaxed);
        self.osrel.store(l.osrel, Ordering::Relaxed);
        self.fctl0.store(l.fctl0, Ordering::Relaxed);
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

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    /// `_exit(code)`.
    Code(i32),
    /// Killed by a signal.
    Signal(u32),
}

impl Exit {
    /// The status `wait4` reports.
    const fn status(self) -> i32 {
        match self {
            Self::Code(c) => exit_status(c),
            Self::Signal(sig) => (sig & 0x7f).cast_signed(),
        }
    }

    const fn from_status(st: i32) -> Self {
        if st & 0x7f == 0 {
            // The whole code, as `_exit` gave it (`WEXITSTATUS` would keep
            // 8 bits).
            Self::Code(st >> 8)
        } else {
            Self::Signal((st & 0x7f).cast_unsigned())
        }
    }
}

/// Job control: whether the process is stopped, and what its parent's
/// `wait4` has yet to hear.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Job {
    /// The signal that stopped the process, 0 while it runs.
    pub(crate) stopped: u32,
    /// A stop `wait4(WUNTRACED)` has not reported (FreeBSD's `P_WAITED`
    /// clear).
    pub(crate) stop_report: bool,
    /// A continue `wait4(WCONTINUED)` has not reported (`P_CONTINUED`).
    pub(crate) cont_report: bool,
}

/// FreeBSD's default limits, as `ulimit -a` shows them on a 15.1 system,
/// except the stack, which matches what exec maps.
pub(crate) fn default_limits() -> [Rlimit; rlimit::NLIMITS] {
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
    l[rlimit::DATA as usize] = Rlimit {
        rlim_cur: crate::exec::MAXDSIZ,
        rlim_max: crate::exec::MAXDSIZ,
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
    /// Where exec put the randomized parts of the image.
    layout: SharedLayout,
    pub(crate) signals: SpinLock<Signals>,
    pub(crate) limits: PreemptLock<[Rlimit; rlimit::NLIMITS]>,
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
    /// Set with the tree locked once exit has decided whom to tell (a
    /// zombie); a closing process descriptor decides by it.
    zombie: AtomicBool,
    /// A process descriptor is open for it (changes with the tree locked).
    procdesc: AtomicBool,
    /// Kernel code waiting for this process to exit (it may have no
    /// parent).
    waiters: WaitQueue,
    /// Bumped when a child exits, stops, continues, or stops borrowing
    /// the address space; `wait4` and `vfork` sleep on it.
    child_gen: AtomicU64,
    child_wait: WaitQueue,
    /// The process's thread, for signals to interrupt; set by the thread
    /// itself, and cleared when it exits.
    thread: SpinLock<Option<Arc<sched::Thread>>>,
    pub(crate) job: SpinLock<Job>,
    /// Where the thread waits while the process is stopped.
    pub(crate) stop_wait: WaitQueue,
    /// Open descriptors. Shared by the process's threads (when there are
    /// several), so behind a sleepable lock (FreeBSD's `fd_sx`).
    pub(crate) files: Mutex<FdTable>,
    /// The working directory.
    pub(crate) cwd: PreemptLock<vfs::VnodeUse>,
    /// The file creation mask.
    pub(crate) umask: AtomicU16,
    /// What `ps` shows of the program it runs.
    image: SpinLock<Image>,
    /// When it was created, in nanoseconds since boot.
    started: u64,
    /// It has called `execve` (FreeBSD's `P_EXEC`).
    execed: AtomicBool,
    /// The CPUs it may run on, one bit each (its `cpuset`); inherited.
    cpus: AtomicU64,
    /// Its credential; inherited.
    cred: PreemptLock<Arc<Ucred>>,
    /// The program it runs, while it runs it.
    text: SpinLock<Option<Text>>,
    /// Its credential changed since it last ran `execve` (FreeBSD's
    /// `P_SUGID`, which `issetugid` reports); inherited.
    sugid: AtomicBool,
}

/// The program a process runs, as `kern.proc` reports it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Image {
    /// The command name (`ki_comm`): the file name it was run as.
    pub(crate) comm: Vec<u8>,
    /// The arguments, each NUL-terminated (`kern.proc.args`).
    pub(crate) args: Arc<[u8]>,
    /// The executable's path (`kern.proc.pathname`).
    pub(crate) path: Arc<[u8]>,
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
        }
    }
}

/// A program a process runs, counted on its vnode while it runs, so the
/// file cannot be opened for writing meanwhile (FreeBSD's `VV_TEXT`).
pub(crate) struct Text(Arc<Vnode>);

impl Text {
    /// `ETXTBSY` if a file has it open for writing.
    fn new(v: &Arc<Vnode>) -> Result<Self, Errno> {
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

/// How a process's thread enters user mode.
struct Start {
    process: Arc<Process>,
    frame: TrapFrame,
    tls: u64,
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
        files: FdTable,
    ) -> Arc<Self> {
        let (signals, limits) = match parent {
            Some(p) => (p.signals.lock().forked(), *p.limits.lock()),
            None => (Signals::new(), default_limits()),
        };
        let cwd = parent.map_or_else(|| crate::fs::root().use_ref(), |p| p.cwd.lock().clone());
        let umask = parent.map_or(0o022, |p| p.umask.load(Ordering::Relaxed));
        let image = parent.map(|p| p.image.lock().clone()).unwrap_or_default();
        let execed = parent.is_some_and(|p| p.execed.load(Ordering::Relaxed));
        let cpus = parent.map_or(u64::MAX, |p| p.cpus.load(Ordering::Relaxed));
        let cred = parent.map_or_else(Ucred::root_shared, |p| p.cred());
        let sugid = parent.is_some_and(|p| p.sugid.load(Ordering::Relaxed));
        let text = parent.and_then(|p| p.text.lock().clone());
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
                tls_base: AtomicU64::new(
                    parent.map_or(u64::MAX, |p| p.tls_base.load(Ordering::Relaxed)),
                ),
                pgid: AtomicI32::new(parent.map_or(pid, |p| p.pgid())),
                sid: AtomicI32::new(parent.map_or(pid, |p| p.sid())),
                status: AtomicI32::new(0),
                exited: AtomicBool::new(false),
                zombie: AtomicBool::new(false),
                procdesc: AtomicBool::new(false),
                waiters: WaitQueue::new(),
                child_gen: AtomicU64::new(0),
                child_wait: WaitQueue::new(),
                thread: SpinLock::new(None),
                job: SpinLock::new(Job::default()),
                stop_wait: WaitQueue::new(),
                files: Mutex::new(&FILEDESC_LOCK, files),
                cwd: PreemptLock::new(cwd),
                umask: AtomicU16::new(umask),
                image: SpinLock::new(image),
                started: crate::time::now_ns(),
                execed: AtomicBool::new(execed),
                cpus: AtomicU64::new(cpus),
                cred: PreemptLock::new(cred),
                text: SpinLock::new(text),
                sugid: AtomicBool::new(sugid),
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

    /// Whether this is a `vfork` child its parent waits for (FreeBSD's
    /// `P_PPWAIT`).
    pub(crate) fn vforked(&self) -> bool {
        self.vforked.load(Ordering::Acquire)
    }

    /// Its credential.
    pub(crate) fn cred(&self) -> Arc<Ucred> {
        Arc::clone(&self.cred.lock())
    }

    /// Whether its credential is the superuser's.
    pub(crate) fn is_root(&self) -> bool {
        self.cred.lock().is_root()
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

    /// Whether its credential changed since it last ran `execve`.
    pub(crate) fn sugid(&self) -> bool {
        self.sugid.load(Ordering::Relaxed)
    }

    /// The program it runs.
    pub(crate) fn image(&self) -> Image {
        self.image.lock().clone()
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

    /// Whether it has called `execve`.
    pub(crate) fn execed(&self) -> bool {
        self.execed.load(Ordering::Relaxed)
    }

    /// The CPUs it may run on, one bit each.
    pub(crate) fn cpus(&self) -> u64 {
        self.cpus.load(Ordering::Relaxed)
    }

    /// Restricts it to the CPUs in `mask` (`cpuset_setaffinity`). Its
    /// thread is bound when that is one CPU; otherwise it runs anywhere
    /// (the scheduler binds to one CPU or none). The caller's own thread
    /// moves at once.
    pub(crate) fn set_cpus(&self, mask: u64) {
        self.cpus.store(mask, Ordering::Relaxed);
        let one = (mask.count_ones() == 1).then(|| hal::CpuId(mask.trailing_zeros()));
        let mine = Process::current().is_some_and(|p| p.pid == self.pid);
        if mine {
            sched::move_to(one);
        }
    }

    /// The CPU its threads are bound to, if its set is one CPU.
    fn bound_cpu(&self) -> Option<hal::CpuId> {
        let mask = self.cpus();
        (mask.count_ones() == 1).then(|| hal::CpuId(mask.trailing_zeros()))
    }

    /// Whether it has exited (a zombie, until reaped).
    pub(crate) fn is_zombie(&self) -> bool {
        self.zombie.load(Ordering::Acquire)
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

    /// Where `mmap` without an address starts looking.
    pub(crate) fn mmap_base(&self) -> u64 {
        self.layout.anon_loc.load(Ordering::Relaxed)
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

    /// Whether the calling thread is the process's only one, so that only
    /// it can change what the process's threads share (its descriptor
    /// table). Every process has one thread until P6 brings more; this
    /// must count them then.
    pub(crate) fn sole_thread(&self) -> bool {
        true
    }

    /// Runs `f` on the current process, borrowed (no reference taken):
    /// `None` on a kernel thread.
    pub(crate) fn with_current<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        sched::with_owner(|o| o?.downcast_ref::<Self>().map(f))
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
            &lookup,
        )?;
        let p = Self::create(space, image.layout, None, crate::fs::console_table());
        *p.image.lock() = Image::new(s, &root, path, argv);
        *p.text.lock() = Some(Text::new(&file)?);
        if let Some(t) = ctty {
            crate::tty::make_controlling(t, &p);
        }
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
        let layout = Layout {
            anon_loc: 64 << 30,
            stack_top: sp,
            ..Layout::default()
        };
        let p = Self::create(space, layout, None, crate::fs::console_table());
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
        self.fork_with(s, frame, vfork, None).map(|(pid, _)| pid)
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
        let (pid, fd) = self.fork_with(s, frame, false, Some((daemon, cloexec, limit)))?;
        Ok((pid, fd.unwrap_or(-1)))
    }

    fn fork_with(
        self: &Arc<Self>,
        s: &Sleepable,
        frame: &TrapFrame,
        vfork: bool,
        pd: Option<(bool, bool, usize)>,
    ) -> Result<(i32, Option<i32>), Errno> {
        let space = if vfork {
            self.space()
        } else {
            self.space().fork(s).map_err(|_| Errno::ENOMEM)?
        };
        let files = self.files.lock(s).fork()?;
        let child = Self::create(space, Layout::default(), Some(self), files);
        child.vforked.store(vfork, Ordering::Relaxed);
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
                    0,
                );
                let flags = crate::fs::fdtable::FdFlags {
                    cloexec,
                    clofork: false,
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
        if child
            .start(cf, hal::user::tls(), true, child.bound_cpu())
            .is_err()
        {
            if let Some(fd) = fd {
                let closed = self.files.lock(s).close(fd);
                drop(closed);
            }
            child.detach();
            return Err(Errno::EAGAIN);
        }
        if vfork {
            self.child_wait
                .wait_until(s, || !child.vforked.load(Ordering::Acquire));
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

    /// Replaces the image with the program at `path` (the current thread
    /// is this process's): a new address space, caught signals reset,
    /// close-on-exec descriptors closed, and `frame` set to start the new
    /// program.
    pub(crate) fn exec(
        &self,
        s: &Sleepable,
        frame: &mut TrapFrame,
        path: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
    ) -> Result<(), Errno> {
        let file = crate::fs::lookup(s, self, crate::fs::At::Cwd, path, crate::fs::Walk::FOLLOW)?;
        let text = Text::new(&file)?;
        let space = AddressSpace::new().map_err(|_| Errno::ENOMEM)?;
        let limits = *self.limits.lock();
        let lookup =
            |p: &[u8]| crate::fs::lookup(s, self, crate::fs::At::Cwd, p, crate::fs::Walk::FOLLOW);
        let image = crate::exec::load(s, &space, &file, path, argv, envp, &limits, &lookup)?;
        // No way back from here.
        let old = core::mem::replace(&mut *self.space.lock(), Arc::clone(&space));
        sched::set_address_space(Some(space as Arc<dyn sched::AddressSpace>));
        if self.vforked.load(Ordering::Acquire) {
            // The old space is the parent's.
            self.release_parent();
        } else if Arc::strong_count(&old) > 1 {
            // Another CPU still has it loaded: empty it now, so its memory
            // does not wait for that CPU to load another.
            old.clear(s);
        }
        // Dropping the last reference frees the tables and the pages.
        drop(old);
        self.signals.lock().exec();
        let closed = self.files.lock(s).exec();
        crate::fs::closed(self.pid, &closed);
        drop(closed);
        let cwd = Arc::clone(self.cwd.lock().vnode());
        *self.image.lock() = Image::new(s, &cwd, path, argv);
        self.execed.store(true, Ordering::Relaxed);
        *self.text.lock() = Some(text);
        // The saved ids become the effective ones (no set-id programs
        // until P6).
        let mut cred = self.cred.lock();
        if cred.svuid != cred.euid() || cred.svgid != cred.egid() {
            let mut c = Ucred::clone(&cred);
            c.svuid = c.euid();
            c.svgid = c.egid();
            *cred = Arc::new(c);
        }
        drop(cred);
        self.sugid.store(false, Ordering::Relaxed);
        self.exterr.store(0, Ordering::Relaxed);
        self.set_tls_base(None);
        // SAFETY: this thread runs in the new image; the kernel does not
        // use the thread pointer.
        unsafe { hal::user::set_tls(0) };
        self.layout.set(image.layout);
        *frame = arch::user::user_frame(image.entry, image.sp, image.arg);
        Ok(())
    }

    /// Ends the process as `how` says (the caller's thread then exits):
    /// the memory goes now, the children go to init, and the parent gets
    /// `SIGCHLD` (or, if it keeps no zombies, init gets the zombie).
    pub(crate) fn exit(&self, s: &Sleepable, how: Exit) {
        let vforked = self.vforked.load(Ordering::Acquire);
        if vforked {
            self.release_parent();
        }
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
        drop(self.text.lock().take());
        // Descriptors close before the parent hears: a pipe's reader sees
        // the end first.
        let closed = self.files.lock(s).take_all();
        crate::fs::closed(self.pid, &closed);
        drop(closed);
        if self.pid == self.sid() {
            crate::tty::session_ended(self.pid);
        }
        // The thread holds the process (its owner); this breaks the cycle.
        *self.thread.lock() = None;
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
        self.status.store(how.status(), Ordering::Relaxed);
        // Release: the status is visible to whoever sees `exited`.
        self.exited.store(true, Ordering::Release);
        let parent = self.parent();
        let reaps_nothing = parent
            .as_ref()
            .is_some_and(|p| crate::signal::parent_reaps_nothing(p));
        let described = {
            let mut tree = TREE.lock();
            self.zombie.store(true, Ordering::Release);
            // A process descriptor holds this zombie for its parent: no
            // SIGCHLD, and init does not take it.
            let described = self.procdesc.load(Ordering::Acquire);
            let reaps_nothing = reaps_nothing && !described;
            let init = (self.pid != INIT).then(|| find(INIT)).flatten();
            if let Some(orphans) = tree.children.remove(&self.pid) {
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
        if let Some(init) = find(INIT) {
            init.child_changed();
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
                };
                crate::signal::notify_parent(self, code, status);
            }
        }
        self.waiters.wake_all();
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

    /// The thread, if it has started.
    pub(crate) fn thread(&self) -> Option<Arc<sched::Thread>> {
        self.thread.lock().clone()
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
    /// `EINTR` or `ERESTART`.
    pub(crate) fn wait_child(
        &self,
        s: &Sleepable,
        pid: i32,
        options: u32,
    ) -> Result<Option<(i32, i32)>, Errno> {
        let selects = |c: &Process| match pid {
            // Children with a process descriptor answer only to their pid
            // or group.
            -1 => !c.procdesc.load(Ordering::Acquire),
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
                for c in kids.iter().filter(|c| selects(c)) {
                    let mut job = c.job.lock();
                    if options & WUNTRACED != 0 && job.stopped != 0 && job.stop_report {
                        if options & WNOWAIT == 0 {
                            job.stop_report = false;
                        }
                        // WIFSTOPPED: the signal, then 0177.
                        return Ok(Some((c.pid, (job.stopped.cast_signed() << 8) | 0o177)));
                    }
                    if options & WCONTINUED != 0 && job.cont_report {
                        if options & WNOWAIT == 0 {
                            job.cont_report = false;
                        }
                        // WIFCONTINUED: SIGCONT's number.
                        return Ok(Some((c.pid, abi::signal::SIGCONT.cast_signed())));
                    }
                }
            }
            if options & WNOHANG != 0 {
                return Ok(None);
            }
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
                false
            } else {
                // To init, which reaps it when it goes.
                if let Some(ppid) = tree.parent.get(&p.pid).copied()
                    && ppid != INIT
                    && let Some(init) = find(INIT)
                {
                    if let Some(kids) = tree.children.get_mut(&ppid) {
                        kids.retain(|c| c.pid != p.pid);
                    }
                    tree.parent.insert(p.pid, INIT);
                    tree.children.entry(INIT).or_default().push(Arc::clone(p));
                    drop(tree);
                    init.child_changed();
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
    // Signals posted from here on interrupt the thread; ones posted before
    // are looked at below.
    *process.thread.lock() = Some(sched::current_thread());
    crate::signal::recheck(&process);
    sched::set_owner(Some(process));
    let mut frame = frame;
    let _ = hal::irq::disable();
    // SAFETY: this thread runs in its process's space; the kernel does not
    // use the thread pointer.
    unsafe { hal::user::set_tls(tls) };
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
