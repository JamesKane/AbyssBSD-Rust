// SPDX-License-Identifier: BSD-2-Clause
//! `procctl(idtype, id, com, data)` (docs/MULTIUSER.md §10.6): reapers,
//! and the per-process controls kept in `p_flag2` (tracing, capability
//! traps, ASLR, implied `PROT_MAX`, the stack gap, no new privileges, W^X,
//! signal-exit logging) with their status queries, plus the parent-death
//! signal.
//!
//! Each command names its argument's size, whether it acts on one process
//! only, whether a missing process is `EINVAL`, and whether its target
//! must be one the caller may debug or only see (FreeBSD's
//! `procctl_cmds_info`).
//!
//! Behavior reference: sys/kern/kern_procctl.c.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::procctl::*;

use super::{SysResult, int, process};
use crate::cred::Priv;
use crate::proc::{Process, p2};
use crate::uaccess::{self, Plain, UserPtr};

// SAFETY: integers and an address, no padding (`abi-check`).
unsafe impl Plain for ReaperStatus {}
// SAFETY: as above.
unsafe impl Plain for ReaperPidinfo {}
// SAFETY: as above.
unsafe impl Plain for ReaperPids {}
// SAFETY: as above.
unsafe impl Plain for ReaperKill {}

/// What a command takes and gives (`procctl_cmd_info`).
#[derive(Clone, Copy)]
struct Info {
    /// Only `P_PID` names its target.
    one_proc: bool,
    /// A pid that names no process is `EINVAL`, not `ESRCH`.
    esrch_is_einval: bool,
    /// The target must be one the caller may debug, not only see.
    need_candebug: bool,
    /// It takes no argument, and a non-null one is `EINVAL`.
    no_data: bool,
}

const fn info(one_proc: bool, need_candebug: bool) -> Info {
    Info {
        one_proc,
        esrch_is_einval: false,
        need_candebug,
        no_data: false,
    }
}

fn info_of(com: u32) -> Option<Info> {
    Some(match com {
        PROC_SPROTECT => info(false, false),
        PROC_REAP_ACQUIRE | PROC_REAP_RELEASE => Info {
            no_data: true,
            ..info(true, false)
        },
        PROC_REAP_STATUS | PROC_REAP_GETPIDS | PROC_REAP_KILL => info(true, false),
        PROC_TRACE_CTL | PROC_TRAPCAP_CTL => info(false, true),
        PROC_PDEATHSIG_CTL | PROC_PDEATHSIG_STATUS => Info {
            esrch_is_einval: true,
            ..info(true, false)
        },
        PROC_ASLR_CTL
        | PROC_PROTMAX_CTL
        | PROC_STACKGAP_CTL
        | PROC_NO_NEW_PRIVS_CTL
        | PROC_WXMAP_CTL
        | PROC_LOGSIGEXIT_CTL => info(true, true),
        PROC_TRACE_STATUS
        | PROC_TRAPCAP_STATUS
        | PROC_ASLR_STATUS
        | PROC_PROTMAX_STATUS
        | PROC_STACKGAP_STATUS
        | PROC_NO_NEW_PRIVS_STATUS
        | PROC_WXMAP_STATUS
        | PROC_LOGSIGEXIT_STATUS => info(true, false),
        _ => return None,
    })
}

/// A command's argument, copied in (`sys_procctl`'s union).
#[derive(Clone, Copy)]
enum Arg {
    None,
    Int(i32),
    Pids(ReaperPids),
    Kill(ReaperKill),
}

/// What a command hands back.
enum Out {
    None,
    Int(i32),
    Status(ReaperStatus),
    Kill(ReaperKill),
}

/// `procctl(idtype, id, com, data)`.
pub(super) fn procctl(idtype: u64, id: u64, com: u64, data: u64) -> SysResult {
    let com = u32::try_from(com & 0xffff_ffff).unwrap_or(u32::MAX);
    if com >= PROC_PROCCTL_MD_MIN {
        // No machine-dependent commands (amd64's are for the TSC and
        // `PROC_KPTI_*`, not in the generic kernel).
        crate::firstuse::report(crate::firstuse::Kind::Procctl, format_args!("{com:#x}"));
        return Err(Errno::EINVAL);
    }
    let Some(i) = info_of(com) else {
        crate::firstuse::report(crate::firstuse::Kind::Procctl, format_args!("{com}"));
        return Err(Errno::EINVAL);
    };
    let read_int = || UserPtr::<i32>::new(data).read().map_err(|_| Errno::EFAULT);
    let arg = match com {
        PROC_SPROTECT
        | PROC_TRACE_CTL
        | PROC_TRAPCAP_CTL
        | PROC_PDEATHSIG_CTL
        | PROC_ASLR_CTL
        | PROC_PROTMAX_CTL
        | PROC_STACKGAP_CTL
        | PROC_NO_NEW_PRIVS_CTL
        | PROC_WXMAP_CTL
        | PROC_LOGSIGEXIT_CTL => Arg::Int(read_int()?),
        PROC_REAP_GETPIDS => Arg::Pids(
            UserPtr::<ReaperPids>::new(data)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        ),
        PROC_REAP_KILL => Arg::Kill(
            UserPtr::<ReaperKill>::new(data)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        ),
        _ if i.no_data && data != 0 => return Err(Errno::EINVAL),
        _ => Arg::None,
    };
    let (r, out) = run(
        i,
        u32::try_from(idtype).unwrap_or(u32::MAX),
        int(id),
        com,
        arg,
    );
    // `PROC_REAP_KILL` reports what it did even when it fails.
    let copy = match &out {
        Out::None => Ok(()),
        Out::Int(v) => UserPtr::<i32>::new(data).write(v),
        Out::Status(s) => UserPtr::<ReaperStatus>::new(data).write(s),
        Out::Kill(k) => UserPtr::<ReaperKill>::new(data).write(k),
    };
    r?;
    copy.map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `kern_procctl`: the command on the process `id` names, or on every
/// member of group `id` the caller may see (or debug), succeeding if one
/// does.
fn run(i: Info, idtype: u32, id: i32, com: u32, arg: Arg) -> (Result<(), Errno>, Out) {
    let Ok(me) = process() else {
        return (Err(Errno::EINVAL), Out::None);
    };
    if idtype != P_PID && i.one_proc {
        return (Err(Errno::EINVAL), Out::None);
    }
    let allowed = |p: &Process| {
        if i.need_candebug {
            me.can_debug(p)
        } else {
            me.can_see(p)
        }
    };
    match idtype {
        P_PID => {
            let p = if id == 0 {
                Arc::clone(&me)
            } else {
                match crate::proc::find(id) {
                    Some(p) => p,
                    None if i.esrch_is_einval => return (Err(Errno::EINVAL), Out::None),
                    None => return (Err(Errno::ESRCH), Out::None),
                }
            };
            if id != 0
                && let Err(e) = allowed(&p)
            {
                return (Err(e), Out::None);
            }
            single(&me, &p, com, arg)
        }
        P_PGID => {
            let members: Vec<_> = crate::proc::all()
                .into_iter()
                .filter(|p| p.pgid() == id)
                .collect();
            if members.is_empty() {
                return (Err(Errno::ESRCH), Out::None);
            }
            let (mut ok, mut first) = (false, None);
            for p in members
                .iter()
                .filter(|p| !p.is_zombie() && allowed(p).is_ok())
            {
                match single(&me, p, com, arg).0 {
                    Ok(()) => ok = true,
                    Err(e) => {
                        first.get_or_insert(e);
                    }
                }
            }
            let r = if ok {
                Ok(())
            } else {
                Err(first.unwrap_or(Errno::ESRCH))
            };
            (r, Out::None)
        }
        _ => (Err(Errno::EINVAL), Out::None),
    }
}

/// Sets the `p_flag2` bits `set` and clears `clear` on `p`.
fn flags(p: &Process, set: u32, clear: u32) {
    p.flag2.fetch_and(!clear, Ordering::Relaxed);
    p.flag2.fetch_or(set, Ordering::Relaxed);
}

fn single(me: &Arc<Process>, p: &Arc<Process>, com: u32, arg: Arg) -> (Result<(), Errno>, Out) {
    let f2 = p.flag2.load(Ordering::Relaxed);
    let state = match arg {
        Arg::Int(v) => v,
        _ => 0,
    };
    let done = |r: Result<(), Errno>| (r, Out::None);
    let status = |v: i32| (Ok(()), Out::Int(v));
    let mine = Arc::ptr_eq(me, p);
    match com {
        // No process is protected from the pager: there is no pager.
        PROC_SPROTECT => done(protect(me, state)),
        PROC_REAP_ACQUIRE => done(if mine {
            crate::proc::reap_acquire(p)
        } else {
            Err(Errno::EPERM)
        }),
        PROC_REAP_RELEASE => done(if mine {
            crate::proc::reap_release(p)
        } else {
            Err(Errno::EPERM)
        }),
        PROC_REAP_STATUS => (Ok(()), Out::Status(reap_status(p))),
        PROC_REAP_GETPIDS => match arg {
            Arg::Pids(rp) => done(reap_getpids(p, &rp)),
            _ => done(Err(Errno::EINVAL)),
        },
        PROC_REAP_KILL => match arg {
            Arg::Kill(rk) => {
                let mut rk = rk;
                let r = reap_kill(me, p, &mut rk);
                (r, Out::Kill(rk))
            }
            _ => done(Err(Errno::EINVAL)),
        },
        PROC_TRACE_CTL => done(match state {
            PROC_TRACE_CTL_ENABLE if !mine => Err(Errno::EPERM),
            PROC_TRACE_CTL_ENABLE => {
                flags(p, 0, p2::NOTRACE | p2::NOTRACE_EXEC);
                Ok(())
            }
            PROC_TRACE_CTL_DISABLE_EXEC => {
                flags(p, p2::NOTRACE_EXEC | p2::NOTRACE, 0);
                Ok(())
            }
            PROC_TRACE_CTL_DISABLE if f2 & p2::NOTRACE_EXEC != 0 => {
                if mine {
                    flags(p, 0, p2::NOTRACE_EXEC);
                    Ok(())
                } else {
                    Err(Errno::EPERM)
                }
            }
            PROC_TRACE_CTL_DISABLE => {
                flags(p, p2::NOTRACE, 0);
                Ok(())
            }
            _ => Err(Errno::EINVAL),
        }),
        // Nothing traces yet (ptrace is P6.8).
        PROC_TRACE_STATUS => status(if f2 & p2::NOTRACE != 0 { -1 } else { 0 }),
        PROC_TRAPCAP_CTL => done(match state {
            PROC_TRAPCAP_CTL_ENABLE => {
                flags(p, p2::TRAPCAP, 0);
                Ok(())
            }
            PROC_TRAPCAP_CTL_DISABLE => {
                flags(p, 0, p2::TRAPCAP);
                Ok(())
            }
            _ => Err(Errno::EINVAL),
        }),
        PROC_TRAPCAP_STATUS => status(if f2 & p2::TRAPCAP != 0 {
            PROC_TRAPCAP_CTL_ENABLE
        } else {
            PROC_TRAPCAP_CTL_DISABLE
        }),
        PROC_PDEATHSIG_CTL => {
            let valid = u32::try_from(state).is_ok_and(|s| s == 0 || crate::signal::valid(s));
            if !mine || !valid {
                return done(Err(Errno::EINVAL));
            }
            p.pdeathsig.store(state.cast_unsigned(), Ordering::Relaxed);
            done(Ok(()))
        }
        PROC_PDEATHSIG_STATUS => {
            if !mine {
                return done(Err(Errno::EINVAL));
            }
            status(p.pdeathsig.load(Ordering::Relaxed).cast_signed())
        }
        PROC_ASLR_CTL => done(force(p, state, p2::ASLR_ENABLE, p2::ASLR_DISABLE)),
        PROC_ASLR_STATUS => {
            let mut d = forced(f2, p2::ASLR_ENABLE, p2::ASLR_DISABLE);
            if p.layout().randomized() {
                d |= PROC_ASLR_ACTIVE;
            }
            status(d)
        }
        PROC_PROTMAX_CTL => done(force(p, state, p2::PROTMAX_ENABLE, p2::PROTMAX_DISABLE)),
        PROC_PROTMAX_STATUS => {
            let mut d = forced(f2, p2::PROTMAX_ENABLE, p2::PROTMAX_DISABLE);
            // `vm.imply_prot_max` is 0: only a forced process has it.
            if f2 & p2::PROTMAX_ENABLE != 0 {
                d |= PROC_PROTMAX_ACTIVE;
            }
            status(d)
        }
        PROC_STACKGAP_CTL => done(stackgap(p, state)),
        PROC_STACKGAP_STATUS => {
            let mut d = if f2 & p2::STKGAP_DISABLE != 0 {
                PROC_STACKGAP_DISABLE
            } else {
                PROC_STACKGAP_ENABLE
            };
            d |= if f2 & p2::STKGAP_DISABLE_EXEC != 0 {
                PROC_STACKGAP_DISABLE_EXEC
            } else {
                PROC_STACKGAP_ENABLE_EXEC
            };
            status(d)
        }
        PROC_NO_NEW_PRIVS_CTL => done(if state == PROC_NO_NEW_PRIVS_ENABLE {
            flags(p, p2::NO_NEW_PRIVS, 0);
            Ok(())
        } else {
            Err(Errno::EINVAL)
        }),
        PROC_NO_NEW_PRIVS_STATUS => status(if f2 & p2::NO_NEW_PRIVS != 0 {
            PROC_NO_NEW_PRIVS_ENABLE
        } else {
            PROC_NO_NEW_PRIVS_DISABLE
        }),
        PROC_WXMAP_CTL => done(match state {
            _ if p.has_exited() => Err(Errno::ESRCH),
            PROC_WX_MAPPINGS_PERMIT => {
                flags(p, p2::WXORX_DISABLE, 0);
                p.space().set_wxorx(false);
                Ok(())
            }
            PROC_WX_MAPPINGS_DISALLOW_EXEC => {
                flags(p, p2::WXORX_ENABLE_EXEC, 0);
                Ok(())
            }
            _ => Err(Errno::EINVAL),
        }),
        PROC_WXMAP_STATUS => {
            if p.has_exited() {
                return done(Err(Errno::ESRCH));
            }
            let mut d = 0;
            if f2 & p2::WXORX_DISABLE != 0 {
                d |= PROC_WX_MAPPINGS_PERMIT;
            }
            if f2 & p2::WXORX_ENABLE_EXEC != 0 {
                d |= PROC_WX_MAPPINGS_DISALLOW_EXEC;
            }
            if p.space().wxorx() {
                d |= PROC_WXORX_ENFORCE;
            }
            status(d)
        }
        PROC_LOGSIGEXIT_CTL => done(match state {
            PROC_LOGSIGEXIT_CTL_NOFORCE => {
                flags(p, 0, p2::LOGSIGEXIT_CTL | p2::LOGSIGEXIT_ENABLE);
                Ok(())
            }
            PROC_LOGSIGEXIT_CTL_FORCE_ENABLE => {
                flags(p, p2::LOGSIGEXIT_CTL | p2::LOGSIGEXIT_ENABLE, 0);
                Ok(())
            }
            PROC_LOGSIGEXIT_CTL_FORCE_DISABLE => {
                flags(p, p2::LOGSIGEXIT_CTL, p2::LOGSIGEXIT_ENABLE);
                Ok(())
            }
            _ => Err(Errno::EINVAL),
        }),
        PROC_LOGSIGEXIT_STATUS => status(if f2 & p2::LOGSIGEXIT_CTL == 0 {
            PROC_LOGSIGEXIT_CTL_NOFORCE
        } else if f2 & p2::LOGSIGEXIT_ENABLE != 0 {
            PROC_LOGSIGEXIT_CTL_FORCE_ENABLE
        } else {
            PROC_LOGSIGEXIT_CTL_FORCE_DISABLE
        }),
        _ => done(Err(Errno::EINVAL)),
    }
}

/// `PROC_SPROTECT` (`protect_set`): checked as FreeBSD checks it; there
/// is no pager to protect a process from, so setting it changes nothing.
fn protect(me: &Process, flags: i32) -> Result<(), Errno> {
    if !matches!(flags & 0xf, PPROT_SET | PPROT_CLEAR)
        || (flags & !0xf) & !(PPROT_DESCEND | PPROT_INHERIT) != 0
    {
        return Err(Errno::EINVAL);
    }
    me.priv_check(Priv::VmMadvProtect)
}

/// A `*_FORCE_ENABLE`, `*_FORCE_DISABLE`, or `*_NOFORCE` control on the
/// pair of `p_flag2` bits.
fn force(p: &Process, state: i32, enable: u32, disable: u32) -> Result<(), Errno> {
    match state {
        1 => flags(p, enable, disable),
        2 => flags(p, disable, enable),
        3 => flags(p, 0, enable | disable),
        _ => return Err(Errno::EINVAL),
    }
    Ok(())
}

/// Which of `force`'s states the bits are in.
fn forced(f2: u32, enable: u32, disable: u32) -> i32 {
    if f2 & enable != 0 {
        1
    } else if f2 & disable != 0 {
        2
    } else {
        3
    }
}

/// `stackgap_ctl`: once disabled, the stack gap stays off for this
/// image.
fn stackgap(p: &Process, state: i32) -> Result<(), Errno> {
    let all = PROC_STACKGAP_ENABLE
        | PROC_STACKGAP_DISABLE
        | PROC_STACKGAP_ENABLE_EXEC
        | PROC_STACKGAP_DISABLE_EXEC;
    if state & !all != 0 {
        return Err(Errno::EINVAL);
    }
    let f2 = p.flag2.load(Ordering::Relaxed);
    match state & (PROC_STACKGAP_ENABLE | PROC_STACKGAP_DISABLE) {
        PROC_STACKGAP_ENABLE if f2 & p2::STKGAP_DISABLE != 0 => return Err(Errno::EINVAL),
        PROC_STACKGAP_ENABLE | 0 => {}
        PROC_STACKGAP_DISABLE => flags(p, p2::STKGAP_DISABLE, 0),
        _ => return Err(Errno::EINVAL),
    }
    match state & (PROC_STACKGAP_ENABLE_EXEC | PROC_STACKGAP_DISABLE_EXEC) {
        PROC_STACKGAP_ENABLE_EXEC => flags(p, 0, p2::STKGAP_DISABLE_EXEC),
        PROC_STACKGAP_DISABLE_EXEC => flags(p, p2::STKGAP_DISABLE_EXEC, 0),
        0 => {}
        _ => return Err(Errno::EINVAL),
    }
    Ok(())
}

/// `reap_status`: the reaper `p` answers to, its children and
/// descendants, and the first of them.
fn reap_status(p: &Process) -> ReaperStatus {
    let (r, owned, list) = crate::proc::reaper_view(p);
    let mut rs = ReaperStatus {
        rs_reaper: r,
        rs_pid: -1,
        ..ReaperStatus::default()
    };
    if owned {
        rs.rs_flags |= REAPER_STATUS_OWNED;
    }
    if r == 1 {
        rs.rs_flags |= REAPER_STATUS_REALINIT;
    }
    if !list.is_empty() {
        // The reaper's first child if it has one, else its first
        // descendant.
        rs.rs_pid = crate::proc::first_child(r).unwrap_or_else(|| list[0].process.pid());
        for d in &list {
            if d.child {
                rs.rs_children += 1;
            }
            rs.rs_descendants += 1;
        }
    }
    rs
}

/// `reap_getpids`: up to `rp_count` descendants into `rp_pids`.
fn reap_getpids(p: &Process, rp: &ReaperPids) -> Result<(), Errno> {
    let (_, _, list) = crate::proc::reaper_view(p);
    let n = list.len().min(rp.rp_count as usize);
    let mut out = Vec::with_capacity(n * size_of::<ReaperPidinfo>());
    for d in &list[..n] {
        let q = &d.process;
        let mut pi = ReaperPidinfo {
            pi_pid: q.pid(),
            pi_subtree: d.subtree,
            pi_flags: REAPER_PIDINFO_VALID,
            ..ReaperPidinfo::default()
        };
        if d.child {
            pi.pi_flags |= REAPER_PIDINFO_CHILD;
        }
        if d.reaper {
            pi.pi_flags |= REAPER_PIDINFO_REAPER;
        }
        if q.job.lock().stopped != 0 {
            pi.pi_flags |= REAPER_PIDINFO_STOPPED;
        }
        if q.is_zombie() {
            pi.pi_flags |= REAPER_PIDINFO_ZOMBIE;
        } else if q.has_exited() {
            pi.pi_flags |= REAPER_PIDINFO_EXITING;
        }
        out.extend_from_slice(uaccess::bytes_of(&pi));
    }
    uaccess::copy_out(rp.rp_pids, &out).map_err(|_| Errno::EFAULT)
}

/// `reap_kill`: `rk_sig` to the reaper's children, or to every
/// descendant (of one subtree), recursing into the reapers among them.
/// As in FreeBSD, a children-only kill keeps `ESRCH` unless one fails (it
/// then reports that failure and its pid); a descendant kill reports the
/// first failure after a success, and a later success clears it. `ESRCH`
/// failures are never reported, so this is no oracle for visibility.
fn reap_kill(me: &Arc<Process>, p: &Process, rk: &mut ReaperKill) -> Result<(), Errno> {
    crate::capsicum::check_capmode()?;
    let both = REAPER_KILL_CHILDREN | REAPER_KILL_SUBTREE;
    if rk.rk_sig <= 0
        || rk.rk_sig > abi::signal::SIG_MAXSIG.cast_signed()
        || rk.rk_flags & !both != 0
        || rk.rk_flags & both == both
    {
        return Err(Errno::EINVAL);
    }
    let sig = rk.rk_sig.cast_unsigned();
    let info = crate::signal::user_info(sig, me.pid(), abi::signal::code::SI_USER);
    rk.rk_killed = 0;
    rk.rk_fpid = -1;
    let (reaper, _, list) = crate::proc::reaper_view(p);
    // `None` is no error (0).
    let mut error = Some(Errno::ESRCH);
    if rk.rk_flags & REAPER_KILL_CHILDREN != 0 {
        for d in list.iter().filter(|d| d.child && !d.process.has_exited()) {
            match me.can_signal(&d.process, sig) {
                Err(e) => {
                    if error == Some(Errno::ESRCH) {
                        rk.rk_fpid = d.process.pid();
                        error = Some(e);
                    }
                }
                Ok(()) => {
                    let _ = crate::signal::post(&d.process, info);
                    rk.rk_killed += 1;
                }
            }
        }
    } else {
        let cred = me.cred();
        let mut done: Vec<i32> = Vec::new();
        let mut reapers = alloc::vec![(reaper, list)];
        while let Some((r, list)) = reapers.pop() {
            for d in list {
                if r == reaper
                    && rk.rk_flags & REAPER_KILL_SUBTREE != 0
                    && d.subtree != rk.rk_subtree
                {
                    continue;
                }
                let q = &d.process;
                if d.reaper {
                    let (_, _, sub) = crate::proc::reaper_view(q);
                    reapers.push((q.pid(), sub));
                }
                if done.contains(&q.pid()) || q.has_exited() {
                    continue;
                }
                done.push(q.pid());
                match cred.can_signal(&q.cred(), q.sugid(), sig) {
                    Err(e) => {
                        if e != Errno::ESRCH && error.is_none() {
                            rk.rk_fpid = q.pid();
                            error = Some(e);
                        }
                    }
                    Ok(()) => {
                        let _ = crate::signal::post(q, info);
                        rk.rk_killed += 1;
                        error = None;
                    }
                }
            }
        }
    }
    error.map_or(Ok(()), Err)
}
