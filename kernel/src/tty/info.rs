// SPDX-License-Identifier: BSD-2-Clause
//! The terminal status line (`^T`, `VSTATUS`, and `TIOCSTAT`): the load
//! average, then the most interesting process of the terminal's
//! foreground group: its command, pid, what its most interesting thread
//! is doing (running, runnable, or its wait channel), its real, user, and
//! system times, its CPU share, and its resident size.
//!
//! `load: 0.08  cmd: sleep 1234 [nanslp] 2.53r 0.00u 0.00s 0% 1860k`
//!
//! Behavior reference: sys/kern/tty_info.c (`tty_info`, `proc_compare`,
//! `thread_compare`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::proc::Process;
use crate::uthread::UserThread;

/// Whether `b` is more interesting than `a` (`thread_compare`): a
/// runnable one, then the busier, then one in a sleep signals cannot
/// interrupt, then the later.
fn thread_better(a: &sched::Report, b: &sched::Report) -> bool {
    let run = |r: &sched::Report| matches!(r.state, sched::State::Running | sched::State::Runnable);
    match (run(a), run(b)) {
        (true, false) => return false,
        (false, true) => return true,
        _ => {}
    }
    if b.pctcpu != a.pctcpu {
        return b.pctcpu > a.pctcpu;
    }
    match (a.interruptible, b.interruptible) {
        (true, false) => return false,
        (false, true) => return true,
        _ => {}
    }
    true
}

/// A process's runnable count and CPU share (`proc_sum`).
fn proc_sum(p: &Process) -> (bool, u32) {
    let mut run = false;
    let mut pct = 0u32;
    p.each_thread(|t| {
        if let Some(s) = t.sched() {
            let r = s.report();
            run |= matches!(r.state, sched::State::Running | sched::State::Runnable);
            pct = pct.saturating_add(r.pctcpu);
        }
        true
    });
    (run, pct)
}

/// Whether `b` is more interesting than `a` (`proc_compare`): a
/// runnable one, the busier, not a zombie, then the higher pid.
fn proc_better(a: &Process, b: &Process) -> bool {
    let ((ra, ea), (rb, eb)) = (proc_sum(a), proc_sum(b));
    match (ra, rb) {
        (true, false) => return false,
        (false, true) => return true,
        _ => {}
    }
    if eb != ea {
        return eb > ea;
    }
    match (a.is_zombie(), b.is_zombie()) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    b.pid() > a.pid()
}

/// Seconds and hundredths of `us` microseconds, as `%ld.%02ld`.
fn secs(out: &mut String, us: u64) {
    let _ = write!(out, "{}.{:02}", us / 1_000_000, us % 1_000_000 / 10_000);
}

/// The status line for a terminal of session `sid` whose foreground
/// group is `pgrp` (0 for none); `at_column0` if the output column is
/// at the start of a line.
pub(crate) fn status_line(sid: Option<i32>, pgrp: i32, at_column0: bool) -> Vec<u8> {
    let mut out = String::new();
    let load = (crate::statclock::loadavg()[0] * 100 + crate::statclock::FSCALE / 2)
        >> crate::statclock::FSHIFT;
    let _ = write!(
        out,
        "{}load: {}.{:02} ",
        if at_column0 { "" } else { "\n" },
        load / 100,
        load % 100
    );
    if sid.is_none() {
        out.push_str("not a controlling terminal\n");
        return out.into_bytes();
    }
    if pgrp == 0 {
        out.push_str("no foreground process group\n");
        return out.into_bytes();
    }
    let members: Vec<Arc<Process>> = crate::proc::all()
        .into_iter()
        .filter(|p| p.pgid() == pgrp)
        .collect();
    let Some(p) = members
        .iter()
        .fold(None::<&Arc<Process>>, |best, p| match best {
            Some(b) if !proc_better(b, p) => Some(b),
            _ => Some(p),
        })
    else {
        out.push_str("empty foreground process group\n");
        return out.into_bytes();
    };
    let mut pick: Option<(Arc<UserThread>, sched::Report)> = None;
    for t in p.threads() {
        let Some(s) = t.sched() else {
            continue;
        };
        let r = s.report();
        if pick.as_ref().is_none_or(|(_, a)| thread_better(a, &r)) {
            pick = Some((t, r));
        }
    }
    let (state, pctcpu): (String, u32) = match &pick {
        _ if p.is_zombie() => ("zombie".into(), 0),
        None => ("unknown".into(), 0),
        Some((_, r)) => (
            match r.state {
                sched::State::Running => "running".into(),
                sched::State::Runnable | sched::State::New => "runnable".into(),
                sched::State::Sleeping if p.job.lock().stopped != 0 => "suspended".into(),
                sched::State::Sleeping if !r.wmesg.is_empty() => r.wmesg.into(),
                sched::State::Sleeping => "sleeping without queue".into(),
                sched::State::Dead => "unknown".into(),
            },
            r.pctcpu,
        ),
    };
    let pct =
        (u64::from(pctcpu) * 10_000 + crate::statclock::FSCALE / 2) >> crate::statclock::FSHIFT;
    let s = crate::kinfo::sleepable();
    let rss_kb = if p.is_zombie() {
        0
    } else {
        let space = p.space();
        space
            .entries(&s)
            .iter()
            .map(|e| space.resident(e.range()))
            .sum::<u64>()
            * 4
    };
    let real_us = crate::time::now_ns().saturating_sub(p.started()) / 1000;
    let u = crate::kinfo::process_usage(p);
    let comm = p.image().comm;
    let _ = write!(
        out,
        " cmd: {} {} [{}] ",
        String::from_utf8_lossy(&comm),
        p.pid(),
        state
    );
    secs(&mut out, real_us);
    out.push_str("r ");
    secs(&mut out, u.utime_us);
    out.push_str("u ");
    secs(&mut out, u.stime_us);
    let _ = writeln!(out, "s {}% {}k", pct / 100, rss_kb);
    out.into_bytes()
}
