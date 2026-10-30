// SPDX-License-Identifier: BSD-2-Clause
//! Process information: `kern.proc` as `ps`, `top`, `w`, `procstat`, and
//! `fstat` read it (docs/MULTIUSER.md §12.1).
//!
//! - `kern.proc.all`, `kern.proc.proc`, and the selections by pid,
//!   process group, session, terminal, and user return `struct
//!   kinfo_proc`s: one per process, describing its first thread, or with
//!   `KERN_PROC_INC_THREAD` (the `_td` names) one per thread, with sizes
//!   and resident pages, the wait channel and message, the scheduler's
//!   CPU share and priorities, resource usage, and the children's times.
//! - `kern.proc.filedesc`, `kern.proc.cwd` ([`file`]): `struct
//!   kinfo_file`s for every descriptor, which `fcntl(F_KINFO)` shares;
//!   `kern.proc.vmmap` ([`vm`]): `struct kinfo_vmentry`s; `kern.proc.kstack`
//!   ([`stack`]): symbolized kernel stacks; and the rest of `procstat`'s
//!   nodes ([`misc`]): `args`, `env`, `auxv`, `pathname`, `rlimit`,
//!   `rlimit_usage`, `umask`, `osrel`, `sigtramp`, `ps_strings`, `groups`,
//!   `sigfastblk`, `vm_layout`, `sv_name`, `kqueue`, `nfds`, and
//!   `kern.lockf`.
//!
//! A process is found by pid, or by a thread's id, as `pget` finds it,
//! and seen only as `p_cansee` allows; its environment, auxiliary vector,
//! map, kernel stacks, and the like only as `p_candebug` allows, as
//! kern_proc.c asks for each.
//!
//! The fillers take a `&Process` so others can use them: core dumps'
//! `NT_PROCSTAT_*` notes and `ptrace`'s `PT_VM_ENTRY` ([`file::files`],
//! [`vm::vmmap`], [`vm::entry`], [`proc_entries`]).
//!
//! Behavior reference: sys/kern/kern_proc.c (`fill_kinfo_proc`,
//! `fill_kinfo_thread`, `sysctl_kern_proc`), sys/kern/kern_resource.c
//! (`calcru`, `ruadd`).

pub(crate) mod file;
pub(crate) mod misc;
pub(crate) mod stack;
pub(crate) mod vm;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::kinfo::*;
use abi::resource::Rusage;
use abi::time::Timeval;
use sched::Sleepable;
use sysctl::{Number, ReadError};

use crate::proc::Process;
use crate::uaccess::Plain;
use crate::uthread::UserThread;

// SAFETY: integers, signal sets, timevals, and byte arrays, with no
// padding (abi's `no_implicit_padding` test; `abi-check` the layout).
unsafe impl Plain for KinfoProc {}

/// Copies `src` into a NUL-terminated field.
pub(crate) fn text(dst: &mut [u8], src: &[u8]) {
    let n = src.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&src[..n]);
}

/// A thread-context token for a handler, which runs in the calling
/// thread.
pub(crate) fn sleepable() -> Sleepable {
    // SAFETY: sysctl handlers run in the calling thread, which may sleep.
    unsafe { Sleepable::assume() }
}

/// A zeroed `T` on the heap: the records are a kilobyte or more each,
/// too much for a kernel stack several deep.
pub(crate) fn zeroed<T: Plain>() -> alloc::boxed::Box<T> {
    // SAFETY: a `Plain` type has no invalid bit patterns, so all zeros is
    // a value of it.
    unsafe { alloc::boxed::Box::<T>::new_zeroed().assume_init() }
}

/// Microseconds as a `timeval`.
pub(crate) fn timeval(us: u64) -> Timeval {
    Timeval {
        tv_sec: i64::try_from(us / 1_000_000).unwrap_or(i64::MAX),
        tv_usec: i64::try_from(us % 1_000_000).unwrap_or(0),
    }
}

/// Resource use, as FreeBSD's `struct rusage` keeps it, plus the run time
/// behind its times: a process's children's (`p_cru`), added to as each
/// is reaped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) utime_us: u64,
    pub(crate) stime_us: u64,
    pub(crate) maxrss_kb: i64,
    pub(crate) nvcsw: u64,
    pub(crate) nivcsw: u64,
}

impl Usage {
    /// `ruadd`: sums, and the larger peak.
    fn add(&mut self, o: &Self) {
        self.utime_us += o.utime_us;
        self.stime_us += o.stime_us;
        self.maxrss_kb = self.maxrss_kb.max(o.maxrss_kb);
        self.nvcsw += o.nvcsw;
        self.nivcsw += o.nivcsw;
    }

    pub(crate) fn rusage(&self) -> Rusage {
        Rusage {
            ru_utime: timeval(self.utime_us),
            ru_stime: timeval(self.stime_us),
            ru_maxrss: self.maxrss_kb,
            ru_nvcsw: i64::try_from(self.nvcsw).unwrap_or(i64::MAX),
            ru_nivcsw: i64::try_from(self.nivcsw).unwrap_or(i64::MAX),
            ..Rusage::default()
        }
    }
}

/// A thread's usage: its run time divided into user and system time by
/// its statistics ticks (`calcru`), its context switches.
fn thread_usage(t: &sched::Thread) -> Usage {
    let us = crate::time::ticks_to_ns(t.runtime()) / 1000;
    let (user, sys) = crate::statclock::split_runtime(us, t.stat_ticks());
    let (nvcsw, nivcsw) = t.context_switches();
    Usage {
        utime_us: user,
        stime_us: sys,
        maxrss_kb: 0,
        nvcsw,
        nivcsw,
    }
}

/// What `p`'s threads used, those gone included (their run time, as
/// system time).
pub(crate) fn process_usage(p: &Process) -> Usage {
    let mut u = Usage::default();
    let mut live_ticks = 0;
    p.each_thread(|t| {
        if let Some(s) = t.sched() {
            live_ticks += s.runtime();
            u.add(&thread_usage(&s));
        }
        true
    });
    let gone = crate::time::ticks_to_ns(p.runtime().saturating_sub(live_ticks)) / 1000;
    u.stime_us += gone;
    u
}

/// `p`'s child `c` was reaped: what it and its own children used goes to
/// `p`'s children's usage (`proc_reap`'s `ruadd`).
pub(crate) fn reaped(p: &Process, c: &Process) {
    let (mut u, children) = exit_usage(c);
    u.add(&children);
    p.child_usage.lock().add(&u);
}

/// What an exited `c` used, and what its reaped children did: `wait6`'s
/// `__wrusage` (`proc_reap`'s `wru_self` and `wru_children`).
pub(crate) fn exit_usage(c: &Process) -> (Usage, Usage) {
    (process_usage(c), *c.child_usage.lock())
}

/// `p`'s address space, as kinfo_proc counts it: the bytes it maps but
/// guard entries (`vm_map.size`), the pages resident (`pmap_resident_count`),
/// and its stack's pages (`vm_ssize`).
fn space_sizes(s: &Sleepable, p: &Process) -> (u64, u64, u64) {
    let space = p.space();
    let entries = space.entries(s);
    let size = entries
        .iter()
        .filter(|e| matches!(e.role(), ::vm::map::Role::Normal | ::vm::map::Role::Stack))
        .map(|e| e.end().get() - e.start().get())
        .sum();
    let resident = entries.iter().map(|e| space.resident(e.range())).sum();
    let stack = space
        .proc_stack(s)
        .map_or(0, |st| st.size.div_ceil(::vm::PAGE_SIZE));
    (size, resident, stack)
}

/// FreeBSD 15's priority for one of the scheduler's (`ki_pri`): the
/// time-sharing range maps onto FreeBSD's `PRI_MIN_TIMESHARE` up, the
/// rest keeps its place below it.
fn freebsd_priority(p: sched::Priority) -> u8 {
    // `v` in `[from, to)` onto `[at, at + width)`.
    let scale = |v: u8, from: u8, to: u8, at: u8, width: u8| {
        let off = u32::from(v - from) * u32::from(width) / u32::from(to - from).max(1);
        at.saturating_add(u8::try_from(off).unwrap_or(u8::MAX))
    };
    let v = p.get();
    let ts = sched::Priority::TIMESHARE.get();
    let idle = sched::Priority::IDLE.get();
    let kernel = sched::Priority::KERNEL.get();
    let realtime = sched::Priority::REALTIME.get();
    if v >= idle {
        v
    } else if v >= ts {
        scale(
            v,
            ts,
            idle,
            PRI_MIN_TIMESHARE,
            PRI_MAX_TIMESHARE - PRI_MIN_TIMESHARE,
        )
    } else if v >= kernel {
        scale(
            v,
            kernel,
            ts,
            PRI_MIN_KERN,
            PRI_MIN_TIMESHARE - PRI_MIN_KERN,
        )
    } else if v >= realtime {
        scale(
            v,
            realtime,
            kernel,
            PRI_MIN_REALTIME,
            PRI_MIN_KERN - PRI_MIN_REALTIME,
        )
    } else {
        scale(v, 0, realtime, 0, PRI_MIN_REALTIME)
    }
}

/// The process part of `p`'s `kinfo_proc` (`fill_kinfo_proc_only`).
fn fill_process(s: &Sleepable, p: &Process, k: &mut KinfoProc) {
    k.ki_pid = p.pid();
    k.ki_ppid = p.ppid();
    k.ki_pgid = p.pgid();
    k.ki_sid = p.sid();
    k.ki_tdev = NODEV;
    k.ki_tdev_freebsd11 = u32::MAX;
    k.ki_numthreads = i32::try_from(p.thread_count()).unwrap_or(i32::MAX);
    (k.ki_reaper, k.ki_reapsubtree) = crate::proc::reaper_of(p.pid());
    k.ki_flag = P_INMEM;
    k.ki_sflag = PS_INMEM;
    {
        let c = p.cred();
        k.ki_uid = c.uid;
        k.ki_ruid = c.ruid;
        k.ki_svuid = c.svuid;
        k.ki_rgid = c.rgid;
        k.ki_svgid = c.svgid;
        // The effective gid first, then as many supplementary groups as
        // fit (`fill_kinfo_proc`'s `cru2x` order).
        k.ki_groups[0] = c.gid;
        let more = c.groups().len().min(KI_NGROUPS - 1);
        k.ki_groups[1..=more].copy_from_slice(&c.groups()[..more]);
        k.ki_ngroups = i16::try_from(more + 1).unwrap_or(i16::MAX);
        if c.groups().len() > more {
            k.ki_cr_flags |= KI_CRF_GRP_OVERFLOW;
        }
        if c.in_capability_mode() {
            k.ki_cr_flags |= KI_CRF_CAPABILITY_MODE;
        }
        // A jailed process is shown with its jail from outside it, and
        // as jail 0 from inside (`fill_kinfo_proc_only`).
        let jid = c.jail_id();
        if jid != 0 {
            k.ki_flag |= P_JAILED;
            let mine = Process::current().map_or(0, |me| me.cred().jail_id());
            if mine != jid {
                k.ki_jid = jid;
            }
        }
        text(&mut k.ki_loginclass, c.loginclass.name());
    }
    if p.sugid() {
        k.ki_flag |= P_SUGID;
    }
    if p.execed() {
        k.ki_flag |= P_EXEC;
    }
    if p.vforked() {
        k.ki_flag |= P_PPWAIT;
    }
    // Traced: by whom (`ki_tracer`), and what ktrace records
    // (`ki_traceflag`).
    if p.is_traced() {
        k.ki_flag |= P_TRACED;
        k.ki_tracer = crate::ptrace::tracer(p);
    }
    k.ki_traceflag = p.ktrace.flags().cast_signed();
    if p.had_threads() {
        k.ki_flag |= P_HADTHREADS;
    }
    if p.has_exited() && !p.is_zombie() {
        k.ki_flag |= P_WEXIT;
    }
    if p.pid() == p.sid() {
        k.ki_kiflag |= KI_SLEADER;
    }
    if let Some((rdev, fg)) = crate::tty::of_session(p.sid()) {
        k.ki_tdev = rdev;
        k.ki_tdev_freebsd11 = u32::try_from(rdev).unwrap_or(u32::MAX);
        k.ki_tpgid = fg;
        k.ki_tsid = p.sid();
        k.ki_flag |= P_CONTROLT;
        k.ki_kiflag |= KI_CTTY;
    }
    {
        let st = p.signals.lock();
        let (ignored, caught) = st.dispositions();
        k.ki_siglist = st.pending();
        k.ki_sigignore = ignored;
        k.ki_sigcatch = caught;
    }
    let image = p.image();
    text(&mut k.ki_comm, &image.comm);
    text(&mut k.ki_emul, b"FreeBSD ELF64");
    text(&mut k.ki_login, &p.login());
    let start_ns = crate::time::realtime_base_ns() + p.started();
    k.ki_start = timeval(start_ns / 1000);
    // Seconds since it was made (`p_swtick`).
    k.ki_swtime = u32::try_from(crate::time::now_ns().saturating_sub(p.started()) / 1_000_000_000)
        .unwrap_or(u32::MAX);
    k.ki_nice = i8::try_from(p.nice.load(core::sync::atomic::Ordering::Relaxed)).unwrap_or(0);
    // `KW_EXITCODE(p_xexit, p_xsig)`: a stopped process's signal, or how
    // a zombie ended.
    let (stop, trace_stop) = {
        let job = p.job.lock();
        (job.stopped, job.trace)
    };
    if stop != 0 {
        k.ki_flag |= P_STOPPED_SIG;
    }
    if trace_stop {
        k.ki_flag |= P_STOPPED_TRACE;
    }
    k.ki_xstat = if stop != 0 {
        u16::try_from(stop).unwrap_or(0)
    } else if p.is_zombie() {
        u16::try_from(p.wait_status() & 0xffff).unwrap_or(0)
    } else {
        0
    };
    if p.is_zombie() {
        k.ki_stat = SZOMB;
    } else {
        let (size, resident, stack) = space_sizes(s, p);
        k.ki_size = size;
        // Its threads' kernel stacks are resident too.
        let kstacks = u64::try_from(p.thread_count()).unwrap_or(0) * crate::threads::STACK_PAGES;
        k.ki_rssize = i64::try_from(resident + kstacks).unwrap_or(i64::MAX);
        let (tsize, dsize) = misc::image_sizes(s, p);
        k.ki_tsize = i64::try_from(tsize).unwrap_or(0);
        k.ki_dsize = i64::try_from(dsize).unwrap_or(0);
        k.ki_ssize = i64::try_from(stack).unwrap_or(0);
    }
    let mut usage = process_usage(p);
    usage.maxrss_kb = k.ki_rssize * 4;
    k.ki_rusage = usage.rusage();
    k.ki_runtime = usage.utime_us + usage.stime_us;
    let children = *p.child_usage.lock();
    k.ki_rusage_ch = children.rusage();
    k.ki_childtime = timeval(children.utime_us + children.stime_us);
}

/// The thread part (`fill_kinfo_thread`): for one entry per thread
/// (`per_thread`), the thread's own signals, usage, and CPU share.
fn fill_thread(p: &Process, ut: &UserThread, per_thread: bool, k: &mut KinfoProc) {
    {
        let ts = ut.sig.lock();
        k.ki_sigmask = ts.mask;
        if per_thread {
            k.ki_siglist = ts.queue.pending();
        }
    }
    // A name past `TDNAMLEN` goes on in `ki_moretdname`.
    let name = ut.name();
    let name = name.as_bytes();
    let (head, more) = name.split_at(name.len().min(TDNAMLEN));
    text(&mut k.ki_tdname, head);
    text(&mut k.ki_moretdname, more);
    k.ki_tid = ut.tid();
    let Some(t) = ut.sched() else {
        k.ki_oncpu = -1;
        k.ki_oncpu_old = u8::MAX;
        return;
    };
    let r = t.report();
    text(&mut k.ki_wmesg, r.wmesg.as_bytes());
    k.ki_wchan = u64::try_from(r.wchan).unwrap_or(0);
    let stopped = p.job.lock().stopped != 0;
    if !p.is_zombie() {
        k.ki_stat = match r.state {
            sched::State::New => SIDL,
            sched::State::Runnable | sched::State::Running => SRUN,
            _ if stopped => SSTOP,
            sched::State::Sleeping => SSLEEP,
            sched::State::Dead => SWAIT,
        };
    }
    k.ki_tdflags = TDF_INMEM;
    if r.interruptible {
        k.ki_tdflags |= TDF_SINTR;
    }
    let now = sched::now();
    // `td_slptick` is 0 while awake: the seconds since boot, as FreeBSD's.
    let since = if r.slept_at == 0 { 0 } else { r.slept_at };
    k.ki_slptime =
        u32::try_from(crate::time::ticks_to_ns(now.saturating_sub(since)) / 1_000_000_000)
            .unwrap_or(u32::MAX);
    let cpu = i32::try_from(r.cpu.0).unwrap_or(0);
    k.ki_lastcpu = cpu;
    k.ki_lastcpu_old = u8::try_from(cpu).unwrap_or(u8::MAX);
    k.ki_oncpu = if r.state == sched::State::Running {
        cpu
    } else {
        -1
    };
    k.ki_oncpu_old = u8::try_from(k.ki_oncpu).unwrap_or(u8::MAX);
    let class = match ut.rtprio().type_ {
        abi::thr::RTP_PRIO_REALTIME | abi::thr::RTP_PRIO_FIFO => PRI_REALTIME,
        abi::thr::RTP_PRIO_IDLE => PRI_IDLE,
        _ => PRI_TIMESHARE,
    };
    k.ki_pri = Priority {
        pri_class: class,
        pri_level: freebsd_priority(r.priority),
        pri_native: freebsd_priority(r.base),
        pri_user: freebsd_priority(r.user),
    };
    if per_thread {
        let u = thread_usage(&t);
        k.ki_runtime = u.utime_us + u.stime_us;
        k.ki_rusage = Rusage {
            ru_maxrss: k.ki_rusage.ru_maxrss,
            ..u.rusage()
        };
        k.ki_pctcpu = r.pctcpu;
    }
}

/// The CPU share of all of `p`'s threads (`fill_kinfo_aggregate`).
fn aggregate_pctcpu(p: &Process) -> u32 {
    let mut sum = 0u32;
    p.each_thread(|t| {
        if let Some(s) = t.sched() {
            sum = sum.saturating_add(s.report().pctcpu);
        }
        true
    });
    sum
}

/// `p` as `struct kinfo_proc`, with thread `t`'s part.
fn fill(s: &Sleepable, p: &Process, t: Option<&UserThread>, per_thread: bool) -> KinfoProc {
    let mut k = KinfoProc::default();
    fill_process(s, p, &mut k);
    if let Some(t) = t {
        fill_thread(p, t, per_thread, &mut k);
    } else {
        // A zombie has no thread left; it is named for its program.
        text(&mut k.ki_tdname, &p.image().comm);
        k.ki_oncpu = -1;
        k.ki_oncpu_old = u8::MAX;
    }
    if !per_thread {
        k.ki_pctcpu = aggregate_pctcpu(p);
    }
    k
}

/// The bytes of `p`'s `kinfo_proc`: one, or one per thread (a zombie has
/// none, and gets one). For core dumps' `NT_PROCSTAT_PROC` too.
#[allow(dead_code, reason = "for core dumps' notes")]
pub(crate) fn proc_entries(s: &Sleepable, p: &Process, per_thread: bool) -> Vec<u8> {
    let mut out = Vec::new();
    entries(s, &mut out, p, per_thread);
    out
}

fn entries(s: &Sleepable, out: &mut Vec<u8>, p: &Process, per_thread: bool) {
    let threads = if per_thread {
        p.threads()
    } else {
        p.first_thread().into_iter().collect()
    };
    if threads.is_empty() {
        out.extend_from_slice(crate::uaccess::bytes_of(&fill(s, p, None, per_thread)));
    }
    for t in &threads {
        out.extend_from_slice(crate::uaccess::bytes_of(&fill(s, p, Some(t), per_thread)));
    }
}

/// The bytes of `kinfo_proc`s for the processes `keep` selects.
fn select(out: &mut Vec<u8>, per_thread: bool, keep: impl Fn(&Process) -> bool) {
    let s = sleepable();
    let mut procs = crate::proc::all();
    procs.sort_unstable_by_key(|p| p.pid());
    // Only what the caller may see (`p_cansee`).
    let me = Process::current();
    let visible = |p: &Process| me.as_ref().is_none_or(|me| me.can_see(p).is_ok());
    for p in procs.iter().filter(|p| keep(p) && visible(p)) {
        entries(&s, out, p, per_thread);
    }
}

/// A selection's handler: the name components after it, whether it wants
/// an entry per thread, and the reply.
type Selection = fn(&[i32], bool, &mut Vec<u8>) -> Result<(), ReadError>;

/// The one argument a node takes.
pub(crate) fn arg(rest: &[i32]) -> Result<i32, ReadError> {
    match rest {
        [a] => Ok(*a),
        _ => Err(ReadError::Invalid),
    }
}

/// What a lookup asks of the caller (`pget`'s `PGET_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// `PGET_CANSEE`.
    See,
    /// `PGET_CANDEBUG`.
    Debug,
}

/// The process `id` names (a pid or a thread id), or `ESRCH`, if the
/// caller may look at it as `access` asks (`pget`). -1 names none.
pub(crate) fn process(id: i32, access: Access) -> Result<Arc<Process>, ReadError> {
    let p = if id > crate::proc::PID_MAX {
        crate::uthread::find(id).map(|t| Arc::clone(t.process()))
    } else {
        crate::proc::find(id)
    };
    let p = p.ok_or(ReadError::Failed(Errno::ESRCH))?;
    if let Some(me) = Process::current() {
        match access {
            Access::See => me.can_see(&p),
            Access::Debug => me.can_debug(&p),
        }
        .map_err(ReadError::Failed)?;
    }
    Ok(p)
}

/// [`process`], with -1 for the caller: the nodes that take it
/// (`kern.proc.args`, `pathname`, and `groups`).
pub(crate) fn process_or_self(id: i32, access: Access) -> Result<Arc<Process>, ReadError> {
    if id == -1 {
        return Process::current().ok_or(ReadError::Failed(Errno::ESRCH));
    }
    process(id, access)
}

/// Adds `kern.proc` to the tree.
pub(crate) fn init(s: &Sleepable) {
    crate::sysctl::with(s, |t| {
        let root = t.root();
        let kern = t.child(root, "kern").expect("kern node");
        t.add_value(
            kern,
            Number::Auto,
            "pid_max",
            "Maximum allowed pid",
            sysctl::Value::int(crate::proc::PID_MAX),
        )
        .expect("kern.pid_max");
        t.add_value(
            kern,
            Number::Auto,
            "maxthread",
            "Maximum number of threads",
            sysctl::Value::int(crate::uthread::MAX_THREAD),
        )
        .expect("kern.maxthread");
        let threads = t
            .add_node(kern, Number::Auto, "threads", "thread allocation")
            .expect("kern.threads");
        t.add_value(
            threads,
            Number::Auto,
            "max_threads_per_proc",
            "Limit on threads per proc",
            sysctl::Value::int(i32::try_from(crate::uthread::MAX_PER_PROC).unwrap_or(i32::MAX)),
        )
        .expect("kern.threads.max_threads_per_proc");
        t.add_value(
            threads,
            Number::Auto,
            "max_threads_hits",
            "kern.threads.max_threads_per_proc hit count",
            sysctl::Value::Int(alloc::boxed::Box::new(|| {
                i32::try_from(crate::uthread::MAX_HITS.load(core::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(i32::MAX)
            })),
        )
        .expect("kern.threads.max_threads_hits");
        let procs = t
            .add_node(
                kern,
                Number::Fixed(abi::sysctl::kern::PROC),
                "proc",
                "Process table",
            )
            .expect("kern.proc");
        let selections: [(i32, &str, Selection); 10] = [
            // `all` and `proc` take no argument, or one they ignore.
            (select::ALL, "all", |r, td, out| {
                if r.len() > 1 {
                    return Err(ReadError::Invalid);
                }
                select(out, td, |_| true);
                Ok(())
            }),
            (select::PROC, "proc", |r, td, out| {
                if r.len() > 1 {
                    return Err(ReadError::Invalid);
                }
                select(out, td, |_| true);
                Ok(())
            }),
            (select::PID, "pid", |r, td, out| {
                let p = process(arg(r)?, Access::See)?;
                entries(&sleepable(), out, &p, td);
                Ok(())
            }),
            (select::PGRP, "pgrp", |r, td, out| {
                let g = arg(r)?;
                select(out, td, |p| p.pgid() == g);
                Ok(())
            }),
            (select::SESSION, "sid", |r, td, out| {
                let sid = arg(r)?;
                select(out, td, |p| p.sid() == sid);
                Ok(())
            }),
            (select::TTY, "tty", |r, td, out| {
                let dev = i64::from(arg(r)?);
                select(out, td, |p| {
                    crate::tty::of_session(p.sid())
                        .is_some_and(|(rdev, _)| i64::try_from(rdev).is_ok_and(|d| d == dev))
                });
                Ok(())
            }),
            (select::UID, "uid", |r, td, out| {
                let uid = arg(r)?.cast_unsigned();
                select(out, td, |p| p.cred().uid == uid);
                Ok(())
            }),
            (select::RUID, "ruid", |r, td, out| {
                let uid = arg(r)?.cast_unsigned();
                select(out, td, |p| p.cred().ruid == uid);
                Ok(())
            }),
            (select::RGID, "rgid", |r, td, out| {
                let gid = arg(r)?.cast_unsigned();
                select(out, td, |p| p.cred().rgid == gid);
                Ok(())
            }),
            (select::GID, "gid", |r, td, out| {
                let gid = arg(r)?.cast_unsigned();
                select(out, td, |p| p.cred().gid == gid);
                Ok(())
            }),
        ];
        for (number, name, handler) in selections {
            // Each but `all` (which has an entry per thread already, as
            // FreeBSD's does) has a twin with one entry per thread
            // (`pid_td`).
            let mut names = alloc::vec![(number, String::from(name), number == select::ALL)];
            if number != select::ALL {
                names.push((
                    number | select::INC_THREAD,
                    alloc::format!("{name}_td"),
                    true,
                ));
            }
            for (n, name, td) in names {
                t.add_handler(
                    procs,
                    Number::Fixed(n),
                    &name,
                    "Process table",
                    Arc::new(move |r: &[i32], out: &mut Vec<u8>| handler(r, td, out)),
                )
                .expect("kern.proc selection");
            }
        }
        misc::add_nodes(t, kern, procs);
        file::add_nodes(t, kern, procs);
        vm::add_nodes(t, procs);
        stack::add_nodes(t, procs);
        t.add_value(
            kern,
            Number::Auto,
            "openfiles",
            "System-wide number of open files",
            sysctl::Value::Int(alloc::boxed::Box::new(|| {
                crate::fs::file::OPEN_FILES.load(core::sync::atomic::Ordering::Relaxed)
            })),
        )
        .expect("kern.openfiles");
    });
}
