// SPDX-License-Identifier: BSD-2-Clause
//! `kern.proc`: processes as `ps` reads them. `kern.proc.all`,
//! `kern.proc.proc`, and the selections by pid, process group, session,
//! terminal, and user return `struct kinfo_proc`s (one per process, each
//! of which has one thread); `kern.proc.args.<pid>` returns the
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

/// `p` as `struct kinfo_proc`.
fn fill(p: &Process) -> KinfoProc {
    let mut k = KinfoProc {
        ki_pid: p.pid(),
        ki_ppid: p.ppid(),
        ki_pgid: p.pgid(),
        ki_sid: p.sid(),
        ki_tdev: NODEV,
        ki_tdev_freebsd11: u32::MAX,
        ki_ngroups: 1,
        ki_numthreads: 1,
        ki_reaper: 1,
        ki_reapsubtree: 1,
        ..KinfoProc::default()
    };
    k.ki_flag = P_INMEM;
    if p.execed() {
        k.ki_flag |= P_EXEC;
    }
    if p.vforked() {
        k.ki_flag |= P_PPWAIT;
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
        k.ki_sigmask = st.mask;
        k.ki_sigignore = ignored;
        k.ki_sigcatch = caught;
    }
    let image = p.image();
    text(&mut k.ki_comm, &image.comm);
    text(&mut k.ki_tdname, &image.comm);
    text(&mut k.ki_emul, b"FreeBSD ELF64");
    text(&mut k.ki_login, b"root");
    text(&mut k.ki_loginclass, b"default");
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
        match p.thread().map(|t| t.state()) {
            Some(sched::State::New) => SIDL,
            Some(sched::State::Sleeping) => SSLEEP,
            _ => SRUN,
        }
    };
    if let Some(t) = p.thread() {
        let us = crate::time::ticks_to_ns(t.runtime()) / 1000;
        k.ki_runtime = us;
        k.ki_rusage.ru_utime = Timeval {
            tv_sec: i64::try_from(us / 1_000_000).unwrap_or(i64::MAX),
            tv_usec: i64::try_from(us % 1_000_000).unwrap_or(0),
        };
        k.ki_tid = i32::try_from(t.tid().0).unwrap_or(i32::MAX);
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

/// The bytes of `kinfo_proc`s for the processes `keep` selects.
fn select(out: &mut Vec<u8>, keep: impl Fn(&Process) -> bool) {
    let mut procs = crate::proc::all();
    procs.sort_unstable_by_key(|p| p.pid());
    for p in procs.iter().filter(|p| keep(p)) {
        let k = fill(p);
        out.extend_from_slice(crate::uaccess::bytes_of(&k));
    }
}

/// A selection's handler: the name components after it, and the reply.
type Selection = fn(&[i32], &mut Vec<u8>) -> Result<(), ReadError>;

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
    p.ok_or(ReadError::Failed(Errno::ESRCH))
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
        let procs = t
            .add_node(
                kern,
                Number::Fixed(abi::sysctl::kern::PROC),
                "proc",
                "Process table",
            )
            .expect("kern.proc");
        let selections: [(i32, &str, Selection); 8] = [
            (select::ALL, "all", |r, out| {
                if !r.is_empty() {
                    return Err(ReadError::Invalid);
                }
                select(out, |_| true);
                Ok(())
            }),
            (select::PROC, "proc", |r, out| {
                if !r.is_empty() {
                    return Err(ReadError::Invalid);
                }
                select(out, |_| true);
                Ok(())
            }),
            (select::PID, "pid", |r, out| {
                let p = process(arg(r)?)?;
                out.extend_from_slice(crate::uaccess::bytes_of(&fill(&p)));
                Ok(())
            }),
            (select::PGRP, "pgrp", |r, out| {
                let g = arg(r)?;
                select(out, |p| p.pgid() == g);
                Ok(())
            }),
            (select::SESSION, "session", |r, out| {
                let sid = arg(r)?;
                select(out, |p| p.sid() == sid);
                Ok(())
            }),
            (select::TTY, "tty", |r, out| {
                let dev = i64::from(arg(r)?);
                select(out, |p| {
                    crate::tty::of_session(p.sid())
                        .is_some_and(|(rdev, _)| i64::try_from(rdev).is_ok_and(|d| d == dev))
                });
                Ok(())
            }),
            (select::UID, "uid", |r, out| {
                // Everyone is root.
                let uid = arg(r)?;
                select(out, |_| uid == 0);
                Ok(())
            }),
            (select::RUID, "ruid", |r, out| {
                let uid = arg(r)?;
                select(out, |_| uid == 0);
                Ok(())
            }),
        ];
        for (number, name, handler) in selections {
            // Each but `all` has a twin with one entry per thread
            // (`pid_td`), the same while processes have one thread.
            let mut names = alloc::vec![(number, String::from(name))];
            if number != select::ALL {
                names.push((number | select::INC_THREAD, alloc::format!("{name}_td")));
            }
            for (n, name) in names {
                t.add_handler(
                    procs,
                    Number::Fixed(n),
                    &name,
                    "Process table",
                    Arc::new(handler),
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
                out.extend_from_slice(&p.image().auxv);
                Ok(())
            }),
        )
        .expect("kern.proc.auxv");
    });
}
