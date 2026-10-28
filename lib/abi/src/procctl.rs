// SPDX-License-Identifier: BSD-2-Clause
//! `procctl(2)` (`sys/procctl.h`): the commands, their arguments, and the
//! reaper structures.

use crate::layout::{Arches, ConstValue};

/// `idtype_t` values `procctl` takes (`sys/wait.h`).
pub const P_PID: u32 = 0;
pub const P_PGID: u32 = 2;

/// Machine-dependent commands start here.
pub const PROC_PROCCTL_MD_MIN: u32 = 0x1000_0000;

pub const PROC_SPROTECT: u32 = 1;
pub const PROC_REAP_ACQUIRE: u32 = 2;
pub const PROC_REAP_RELEASE: u32 = 3;
pub const PROC_REAP_STATUS: u32 = 4;
pub const PROC_REAP_GETPIDS: u32 = 5;
pub const PROC_REAP_KILL: u32 = 6;
pub const PROC_TRACE_CTL: u32 = 7;
pub const PROC_TRACE_STATUS: u32 = 8;
pub const PROC_TRAPCAP_CTL: u32 = 9;
pub const PROC_TRAPCAP_STATUS: u32 = 10;
pub const PROC_PDEATHSIG_CTL: u32 = 11;
pub const PROC_PDEATHSIG_STATUS: u32 = 12;
pub const PROC_ASLR_CTL: u32 = 13;
pub const PROC_ASLR_STATUS: u32 = 14;
pub const PROC_PROTMAX_CTL: u32 = 15;
pub const PROC_PROTMAX_STATUS: u32 = 16;
pub const PROC_STACKGAP_CTL: u32 = 17;
pub const PROC_STACKGAP_STATUS: u32 = 18;
pub const PROC_NO_NEW_PRIVS_CTL: u32 = 19;
pub const PROC_NO_NEW_PRIVS_STATUS: u32 = 20;
pub const PROC_WXMAP_CTL: u32 = 21;
pub const PROC_WXMAP_STATUS: u32 = 22;
pub const PROC_LOGSIGEXIT_CTL: u32 = 23;
pub const PROC_LOGSIGEXIT_STATUS: u32 = 24;

/// `PROC_SPROTECT`'s argument.
pub const PPROT_SET: i32 = 1;
pub const PPROT_CLEAR: i32 = 2;
pub const PPROT_DESCEND: i32 = 0x10;
pub const PPROT_INHERIT: i32 = 0x20;

pub const REAPER_STATUS_OWNED: u32 = 0x1;
pub const REAPER_STATUS_REALINIT: u32 = 0x2;

pub const REAPER_PIDINFO_VALID: u32 = 0x01;
pub const REAPER_PIDINFO_CHILD: u32 = 0x02;
pub const REAPER_PIDINFO_REAPER: u32 = 0x04;
pub const REAPER_PIDINFO_ZOMBIE: u32 = 0x08;
pub const REAPER_PIDINFO_STOPPED: u32 = 0x10;
pub const REAPER_PIDINFO_EXITING: u32 = 0x20;

pub const REAPER_KILL_CHILDREN: u32 = 0x1;
pub const REAPER_KILL_SUBTREE: u32 = 0x2;

pub const PROC_TRACE_CTL_ENABLE: i32 = 1;
pub const PROC_TRACE_CTL_DISABLE: i32 = 2;
pub const PROC_TRACE_CTL_DISABLE_EXEC: i32 = 3;

pub const PROC_TRAPCAP_CTL_ENABLE: i32 = 1;
pub const PROC_TRAPCAP_CTL_DISABLE: i32 = 2;

pub const PROC_ASLR_FORCE_ENABLE: i32 = 1;
pub const PROC_ASLR_FORCE_DISABLE: i32 = 2;
pub const PROC_ASLR_NOFORCE: i32 = 3;
pub const PROC_ASLR_ACTIVE: i32 = 0x8000_0000_u32.cast_signed();

pub const PROC_PROTMAX_FORCE_ENABLE: i32 = 1;
pub const PROC_PROTMAX_FORCE_DISABLE: i32 = 2;
pub const PROC_PROTMAX_NOFORCE: i32 = 3;
pub const PROC_PROTMAX_ACTIVE: i32 = 0x8000_0000_u32.cast_signed();

pub const PROC_STACKGAP_ENABLE: i32 = 0x1;
pub const PROC_STACKGAP_DISABLE: i32 = 0x2;
pub const PROC_STACKGAP_ENABLE_EXEC: i32 = 0x4;
pub const PROC_STACKGAP_DISABLE_EXEC: i32 = 0x8;

pub const PROC_NO_NEW_PRIVS_ENABLE: i32 = 1;
pub const PROC_NO_NEW_PRIVS_DISABLE: i32 = 2;

pub const PROC_WX_MAPPINGS_PERMIT: i32 = 0x1;
pub const PROC_WX_MAPPINGS_DISALLOW_EXEC: i32 = 0x2;
pub const PROC_WXORX_ENFORCE: i32 = 0x8000_0000_u32.cast_signed();

pub const PROC_LOGSIGEXIT_CTL_NOFORCE: i32 = 1;
pub const PROC_LOGSIGEXIT_CTL_FORCE_ENABLE: i32 = 2;
pub const PROC_LOGSIGEXIT_CTL_FORCE_DISABLE: i32 = 3;

/// `struct procctl_reaper_status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ReaperStatus {
    pub rs_flags: u32,
    pub rs_children: u32,
    pub rs_descendants: u32,
    pub rs_reaper: i32,
    pub rs_pid: i32,
    pub rs_pad0: [u32; 15],
}

/// `struct procctl_reaper_pidinfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ReaperPidinfo {
    pub pi_pid: i32,
    pub pi_subtree: i32,
    pub pi_flags: u32,
    pub pi_pad0: [u32; 15],
}

/// `struct procctl_reaper_pids`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ReaperPids {
    pub rp_count: u32,
    pub rp_pad0: [u32; 15],
    pub rp_pids: u64,
}

/// `struct procctl_reaper_kill`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ReaperKill {
    pub rk_sig: i32,
    pub rk_flags: u32,
    pub rk_subtree: i32,
    pub rk_killed: u32,
    pub rk_fpid: i32,
    pub rk_pad0: [u32; 15],
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("PROC_PROCCTL_MD_MIN", PROC_PROCCTL_MD_MIN as i64),
    c("PROC_SPROTECT", PROC_SPROTECT as i64),
    c("PROC_REAP_ACQUIRE", PROC_REAP_ACQUIRE as i64),
    c("PROC_REAP_RELEASE", PROC_REAP_RELEASE as i64),
    c("PROC_REAP_STATUS", PROC_REAP_STATUS as i64),
    c("PROC_REAP_GETPIDS", PROC_REAP_GETPIDS as i64),
    c("PROC_REAP_KILL", PROC_REAP_KILL as i64),
    c("PROC_TRACE_CTL", PROC_TRACE_CTL as i64),
    c("PROC_TRACE_STATUS", PROC_TRACE_STATUS as i64),
    c("PROC_TRAPCAP_CTL", PROC_TRAPCAP_CTL as i64),
    c("PROC_TRAPCAP_STATUS", PROC_TRAPCAP_STATUS as i64),
    c("PROC_PDEATHSIG_CTL", PROC_PDEATHSIG_CTL as i64),
    c("PROC_PDEATHSIG_STATUS", PROC_PDEATHSIG_STATUS as i64),
    c("PROC_ASLR_CTL", PROC_ASLR_CTL as i64),
    c("PROC_ASLR_STATUS", PROC_ASLR_STATUS as i64),
    c("PROC_PROTMAX_CTL", PROC_PROTMAX_CTL as i64),
    c("PROC_PROTMAX_STATUS", PROC_PROTMAX_STATUS as i64),
    c("PROC_STACKGAP_CTL", PROC_STACKGAP_CTL as i64),
    c("PROC_STACKGAP_STATUS", PROC_STACKGAP_STATUS as i64),
    c("PROC_NO_NEW_PRIVS_CTL", PROC_NO_NEW_PRIVS_CTL as i64),
    c("PROC_NO_NEW_PRIVS_STATUS", PROC_NO_NEW_PRIVS_STATUS as i64),
    c("PROC_WXMAP_CTL", PROC_WXMAP_CTL as i64),
    c("PROC_WXMAP_STATUS", PROC_WXMAP_STATUS as i64),
    c("PROC_LOGSIGEXIT_CTL", PROC_LOGSIGEXIT_CTL as i64),
    c("PROC_LOGSIGEXIT_STATUS", PROC_LOGSIGEXIT_STATUS as i64),
    c("PPROT_SET", PPROT_SET as i64),
    c("PPROT_CLEAR", PPROT_CLEAR as i64),
    c("PPROT_DESCEND", PPROT_DESCEND as i64),
    c("PPROT_INHERIT", PPROT_INHERIT as i64),
    c("REAPER_STATUS_OWNED", REAPER_STATUS_OWNED as i64),
    c("REAPER_STATUS_REALINIT", REAPER_STATUS_REALINIT as i64),
    c("REAPER_PIDINFO_VALID", REAPER_PIDINFO_VALID as i64),
    c("REAPER_PIDINFO_CHILD", REAPER_PIDINFO_CHILD as i64),
    c("REAPER_PIDINFO_REAPER", REAPER_PIDINFO_REAPER as i64),
    c("REAPER_PIDINFO_ZOMBIE", REAPER_PIDINFO_ZOMBIE as i64),
    c("REAPER_PIDINFO_STOPPED", REAPER_PIDINFO_STOPPED as i64),
    c("REAPER_PIDINFO_EXITING", REAPER_PIDINFO_EXITING as i64),
    c("REAPER_KILL_CHILDREN", REAPER_KILL_CHILDREN as i64),
    c("REAPER_KILL_SUBTREE", REAPER_KILL_SUBTREE as i64),
    c("PROC_TRACE_CTL_ENABLE", PROC_TRACE_CTL_ENABLE as i64),
    c("PROC_TRACE_CTL_DISABLE", PROC_TRACE_CTL_DISABLE as i64),
    c(
        "PROC_TRACE_CTL_DISABLE_EXEC",
        PROC_TRACE_CTL_DISABLE_EXEC as i64,
    ),
    c("PROC_TRAPCAP_CTL_ENABLE", PROC_TRAPCAP_CTL_ENABLE as i64),
    c("PROC_TRAPCAP_CTL_DISABLE", PROC_TRAPCAP_CTL_DISABLE as i64),
    c("PROC_ASLR_FORCE_ENABLE", PROC_ASLR_FORCE_ENABLE as i64),
    c("PROC_ASLR_FORCE_DISABLE", PROC_ASLR_FORCE_DISABLE as i64),
    c("PROC_ASLR_NOFORCE", PROC_ASLR_NOFORCE as i64),
    c(
        "PROC_PROTMAX_FORCE_ENABLE",
        PROC_PROTMAX_FORCE_ENABLE as i64,
    ),
    c(
        "PROC_PROTMAX_FORCE_DISABLE",
        PROC_PROTMAX_FORCE_DISABLE as i64,
    ),
    c("PROC_PROTMAX_NOFORCE", PROC_PROTMAX_NOFORCE as i64),
    c("PROC_STACKGAP_ENABLE", PROC_STACKGAP_ENABLE as i64),
    c("PROC_STACKGAP_DISABLE", PROC_STACKGAP_DISABLE as i64),
    c(
        "PROC_STACKGAP_ENABLE_EXEC",
        PROC_STACKGAP_ENABLE_EXEC as i64,
    ),
    c(
        "PROC_STACKGAP_DISABLE_EXEC",
        PROC_STACKGAP_DISABLE_EXEC as i64,
    ),
    c("PROC_NO_NEW_PRIVS_ENABLE", PROC_NO_NEW_PRIVS_ENABLE as i64),
    c(
        "PROC_NO_NEW_PRIVS_DISABLE",
        PROC_NO_NEW_PRIVS_DISABLE as i64,
    ),
    c("PROC_WX_MAPPINGS_PERMIT", PROC_WX_MAPPINGS_PERMIT as i64),
    c(
        "PROC_WX_MAPPINGS_DISALLOW_EXEC",
        PROC_WX_MAPPINGS_DISALLOW_EXEC as i64,
    ),
    c(
        "PROC_LOGSIGEXIT_CTL_NOFORCE",
        PROC_LOGSIGEXIT_CTL_NOFORCE as i64,
    ),
    c(
        "PROC_LOGSIGEXIT_CTL_FORCE_ENABLE",
        PROC_LOGSIGEXIT_CTL_FORCE_ENABLE as i64,
    ),
    c(
        "PROC_LOGSIGEXIT_CTL_FORCE_DISABLE",
        PROC_LOGSIGEXIT_CTL_FORCE_DISABLE as i64,
    ),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/procctl.h",
        value,
        arches: Arches::All,
    }
}
