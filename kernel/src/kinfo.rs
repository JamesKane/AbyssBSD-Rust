// SPDX-License-Identifier: BSD-2-Clause
//! `kern.proc`: processes as `ps` reads them. `kern.proc.all`,
//! `kern.proc.proc`, and the selections by pid, process group, session,
//! terminal, and user return `struct kinfo_proc`s: one per process,
//! describing its first thread, or with `KERN_PROC_INC_THREAD` (the
//! `_td` names) one per thread; `kern.proc.args.<pid>` returns the
//! arguments, `kern.proc.pathname.<pid>` the executable's path, and
//! `kern.proc.auxv.<pid>` the auxiliary vector exec gave it.
//!
//! `kern.pid_max` sizes `ps`'s pid column.
//!
//! Behavior reference: sys/kern/kern_proc.c (`fill_kinfo_proc`,
//! `sysctl_kern_proc`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::kinfo::*;
use abi::time::Timeval;
use sched::Sleepable;
use sysctl::{Number, ReadError};

use crate::proc::Process;
use crate::uaccess::Plain;

// SAFETY: integers, signal sets, timevals, and byte arrays, with no
// padding (abi's `no_implicit_padding` test; `abi-check` the layout).
unsafe impl Plain for KinfoProc {}

/// Copies `src` into a NUL-terminated field.
fn text(dst: &mut [u8], src: &[u8]) {
    let n = src.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&src[..n]);
}

/// `p` as `struct kinfo_proc`, with thread `t`'s part (FreeBSD's
/// `fill_kinfo_proc` and `fill_kinfo_thread`): for one entry per thread
/// (`per_thread`), the signals pending for `t` and its run time, else the
/// process's.
fn fill(p: &Process, t: Option<&crate::uthread::UserThread>, per_thread: bool) -> KinfoProc {
    let mut k = KinfoProc {
        ki_pid: p.pid(),
        ki_ppid: p.ppid(),
        ki_pgid: p.pgid(),
        ki_sid: p.sid(),
        ki_tdev: NODEV,
        ki_tdev_freebsd11: u32::MAX,
        ki_numthreads: i32::try_from(p.thread_count()).unwrap_or(i32::MAX),
        ..KinfoProc::default()
    };
    (k.ki_reaper, k.ki_reapsubtree) = crate::proc::reaper_of(p.pid());
    k.ki_flag = P_INMEM;
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
        k.ki_jid = c.jail_id();
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
    if p.had_threads() {
        k.ki_flag |= P_HADTHREADS;
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
    if let Some(t) = t {
        let ts = t.sig.lock();
        k.ki_sigmask = ts.mask;
        if per_thread {
            k.ki_siglist = ts.queue.pending();
        }
    }
    let image = p.image();
    text(&mut k.ki_comm, &image.comm);
    // A name past `TDNAMLEN` goes on in `ki_moretdname`.
    let name = t.map(|t| t.name());
    let name = name.as_ref().map_or(&image.comm[..], |n| n.as_bytes());
    let (head, more) = name.split_at(name.len().min(TDNAMLEN));
    text(&mut k.ki_tdname, head);
    text(&mut k.ki_moretdname, more);
    text(&mut k.ki_emul, b"FreeBSD ELF64");
    text(&mut k.ki_login, &p.login());
    let start_ns = crate::time::realtime_base_ns() + p.started();
    k.ki_start = Timeval {
        tv_sec: i64::try_from(start_ns / 1_000_000_000).unwrap_or(i64::MAX),
        tv_usec: i64::try_from(start_ns % 1_000_000_000 / 1000).unwrap_or(0),
    };
    let stopped = p.job.lock().stopped != 0;
    k.ki_stat = if p.is_zombie() {
        SZOMB
    } else if stopped {
        SSTOP
    } else {
        match t.and_then(|t| t.sched()).map(|t| t.state()) {
            Some(sched::State::New) => SIDL,
            Some(sched::State::Sleeping) => SSLEEP,
            _ => SRUN,
        }
    };
    if let Some(ut) = t
        && let Some(t) = ut.sched()
    {
        let ticks = if per_thread { t.runtime() } else { p.runtime() };
        let us = crate::time::ticks_to_ns(ticks) / 1000;
        k.ki_runtime = us;
        k.ki_rusage.ru_utime = Timeval {
            tv_sec: i64::try_from(us / 1_000_000).unwrap_or(i64::MAX),
            tv_usec: i64::try_from(us % 1_000_000).unwrap_or(0),
        };
        k.ki_tid = ut.tid();
        let cpu = i32::try_from(t.last_cpu().0).unwrap_or(0);
        k.ki_lastcpu = cpu;
        k.ki_lastcpu_old = u8::try_from(cpu).unwrap_or(u8::MAX);
        k.ki_oncpu = if t.state() == sched::State::Running {
            cpu
        } else {
            -1
        };
        k.ki_oncpu_old = u8::try_from(k.ki_oncpu).unwrap_or(u8::MAX);
    } else {
        k.ki_oncpu = -1;
        k.ki_oncpu_old = u8::MAX;
    }
    // The timesharing class at its base user priority.
    k.ki_pri = Priority {
        pri_class: 3,
        pri_level: 120,
        pri_native: 120,
        pri_user: 120,
    };
    k
}

/// The bytes of `p`'s `kinfo_proc`: one, or one per thread (a zombie has
/// none, and gets one).
fn entries(out: &mut Vec<u8>, p: &Process, per_thread: bool) {
    let threads = if per_thread {
        p.threads()
    } else {
        p.first_thread().into_iter().collect()
    };
    if threads.is_empty() {
        out.extend_from_slice(crate::uaccess::bytes_of(&fill(p, None, per_thread)));
    }
    for t in &threads {
        out.extend_from_slice(crate::uaccess::bytes_of(&fill(p, Some(t), per_thread)));
    }
}

/// The bytes of `kinfo_proc`s for the processes `keep` selects.
fn select(out: &mut Vec<u8>, per_thread: bool, keep: impl Fn(&Process) -> bool) {
    let mut procs = crate::proc::all();
    procs.sort_unstable_by_key(|p| p.pid());
    // Only what the caller may see (`p_cansee`).
    let me = Process::current();
    let visible = |p: &Process| me.as_ref().is_none_or(|me| me.can_see(p).is_ok());
    for p in procs.iter().filter(|p| keep(p) && visible(p)) {
        entries(out, p, per_thread);
    }
}

/// A selection's handler: the name components after it, whether it wants
/// an entry per thread, and the reply.
type Selection = fn(&[i32], bool, &mut Vec<u8>) -> Result<(), ReadError>;

/// The one argument a selection takes.
fn arg(rest: &[i32]) -> Result<i32, ReadError> {
    match rest {
        [a] => Ok(*a),
        _ => Err(ReadError::Invalid),
    }
}

/// The process `pid` names (-1 for the caller), or `ESRCH`.
fn process(pid: i32) -> Result<Arc<Process>, ReadError> {
    let p = if pid == -1 {
        Process::current()
    } else {
        crate::proc::find(pid)
    };
    let p = p.ok_or(ReadError::Failed(Errno::ESRCH))?;
    if let Some(me) = Process::current() {
        me.can_see(&p).map_err(ReadError::Failed)?;
    }
    Ok(p)
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
            (select::ALL, "all", |r, td, out| {
                if !r.is_empty() {
                    return Err(ReadError::Invalid);
                }
                select(out, td, |_| true);
                Ok(())
            }),
            (select::PROC, "proc", |r, td, out| {
                if !r.is_empty() {
                    return Err(ReadError::Invalid);
                }
                select(out, td, |_| true);
                Ok(())
            }),
            (select::PID, "pid", |r, td, out| {
                let p = process(arg(r)?)?;
                entries(out, &p, td);
                Ok(())
            }),
            (select::PGRP, "pgrp", |r, td, out| {
                let g = arg(r)?;
                select(out, td, |p| p.pgid() == g);
                Ok(())
            }),
            (select::SESSION, "session", |r, td, out| {
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
        t.add_handler(
            procs,
            Number::Fixed(select::ARGS),
            "args",
            "Process argument list",
            Arc::new(|r: &[i32], out: &mut Vec<u8>| {
                let p = process(arg(r)?)?;
                out.extend_from_slice(&p.image().args);
                Ok(())
            }),
        )
        .expect("kern.proc.args");
        t.add_handler(
            procs,
            Number::Fixed(select::PATHNAME),
            "pathname",
            "Process executable path",
            Arc::new(|r: &[i32], out: &mut Vec<u8>| {
                let p = process(arg(r)?)?;
                out.extend_from_slice(&p.image().path);
                out.push(0);
                Ok(())
            }),
        )
        .expect("kern.proc.pathname");
        t.add_handler(
            procs,
            Number::Fixed(select::AUXV),
            "auxv",
            "Process ELF auxiliary vector",
            Arc::new(|r: &[i32], out: &mut Vec<u8>| {
                let p = process(arg(r)?)?;
                // Another's auxiliary vector is for its debuggers
                // (`PGET_CANDEBUG`).
                if let Some(me) = Process::current() {
                    me.can_debug(&p).map_err(ReadError::Failed)?;
                }
                out.extend_from_slice(&p.image().auxv);
                Ok(())
            }),
        )
        .expect("kern.proc.auxv");
        t.add_handler(
            procs,
            Number::Fixed(43),
            "nfds",
            "Number of open file descriptors",
            Arc::new(|r: &[i32], out: &mut Vec<u8>| {
                // `KERN_PROC_NFDS`: only of the caller, named 0.
                if r != [0] {
                    return Err(ReadError::Invalid);
                }
                let p = crate::proc::Process::current().ok_or(ReadError::Invalid)?;
                // SAFETY: sysctl handlers run in the calling thread, which
                // may sleep.
                let s = unsafe { sched::Sleepable::assume() };
                let n = i32::try_from(p.files.lock(&s).count()).unwrap_or(i32::MAX);
                out.extend_from_slice(&n.to_ne_bytes());
                Ok(())
            }),
        )
        .expect("kern.proc.nfds");
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
