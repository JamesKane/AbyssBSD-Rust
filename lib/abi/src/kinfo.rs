// SPDX-License-Identifier: BSD-2-Clause
//! Process information for `ps` and friends (`sys/user.h`): `struct
//! kinfo_proc`, as `kern.proc` returns it, and its flags and states.
//!
//! The kernel addresses FreeBSD reports (`ki_paddr` and the like) are
//! left 0; userland only uses them as identities, and libkvm only on
//! crash dumps.

use crate::layout::{Arches, ConstValue};
use crate::resource::Rusage;
use crate::signal::Sigset;
use crate::time::Timeval;

/// `KI_NGROUPS`.
pub const KI_NGROUPS: usize = 16;
/// The string fields' lengths, NUL excluded.
pub const WMESGLEN: usize = 8;
pub const LOCKNAMELEN: usize = 8;
pub const TDNAMLEN: usize = 16;
pub const COMMLEN: usize = 19;
pub const KI_EMULNAMELEN: usize = 16;
pub const LOGNAMELEN: usize = 17;
pub const LOGINCLASSLEN: usize = 17;
/// `MAXCOMLEN`.
pub const MAXCOMLEN: usize = 19;
pub const KI_NSPARE_INT: usize = 2;
pub const KI_NSPARE_LONG: usize = 12;
pub const KI_NSPARE_PTR: usize = 4;
/// `KINFO_PROC_SIZE` (the same on amd64 and arm64).
pub const KINFO_PROC_SIZE: usize = 1088;
/// `ki_structsize`'s value: the size, as the `int` it is stored in.
const STRUCTSIZE: i32 = 1088;

/// `struct priority`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Priority {
    pub pri_class: u8,
    pub pri_level: u8,
    pub pri_native: u8,
    pub pri_user: u8,
}

/// `struct kinfo_proc`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KinfoProc {
    pub ki_structsize: i32,
    pub ki_layout: i32,
    pub ki_args: u64,
    pub ki_paddr: u64,
    pub ki_addr: u64,
    pub ki_tracep: u64,
    pub ki_textvp: u64,
    pub ki_fd: u64,
    pub ki_vmspace: u64,
    pub ki_wchan: u64,
    pub ki_pid: i32,
    pub ki_ppid: i32,
    pub ki_pgid: i32,
    pub ki_tpgid: i32,
    pub ki_sid: i32,
    pub ki_tsid: i32,
    pub ki_jobc: i16,
    pub ki_spare_short1: i16,
    pub ki_tdev_freebsd11: u32,
    pub ki_siglist: Sigset,
    pub ki_sigmask: Sigset,
    pub ki_sigignore: Sigset,
    pub ki_sigcatch: Sigset,
    pub ki_uid: u32,
    pub ki_ruid: u32,
    pub ki_svuid: u32,
    pub ki_rgid: u32,
    pub ki_svgid: u32,
    pub ki_ngroups: i16,
    pub ki_spare_short2: i16,
    pub ki_groups: [u32; KI_NGROUPS],
    pub ki_size: u64,
    pub ki_rssize: i64,
    pub ki_swrss: i64,
    pub ki_tsize: i64,
    pub ki_dsize: i64,
    pub ki_ssize: i64,
    pub ki_xstat: u16,
    pub ki_acflag: u16,
    pub ki_pctcpu: u32,
    pub ki_estcpu: u32,
    pub ki_slptime: u32,
    pub ki_swtime: u32,
    pub ki_cow: u32,
    pub ki_runtime: u64,
    pub ki_start: Timeval,
    pub ki_childtime: Timeval,
    pub ki_flag: i64,
    pub ki_kiflag: i64,
    pub ki_traceflag: i32,
    pub ki_stat: u8,
    pub ki_nice: i8,
    pub ki_lock: u8,
    pub ki_rqindex: u8,
    pub ki_oncpu_old: u8,
    pub ki_lastcpu_old: u8,
    pub ki_tdname: [u8; TDNAMLEN + 1],
    pub ki_wmesg: [u8; WMESGLEN + 1],
    pub ki_login: [u8; LOGNAMELEN + 1],
    pub ki_lockname: [u8; LOCKNAMELEN + 1],
    pub ki_comm: [u8; COMMLEN + 1],
    pub ki_emul: [u8; KI_EMULNAMELEN + 1],
    pub ki_loginclass: [u8; LOGINCLASSLEN + 1],
    pub ki_moretdname: [u8; MAXCOMLEN - TDNAMLEN + 1],
    pub ki_sparestrings: [u8; 38],
    pub ki_spareints: [i32; KI_NSPARE_INT],
    pub ki_reaper: i32,
    pub ki_reapsubtree: i32,
    pub ki_tdev: u64,
    pub ki_oncpu: i32,
    pub ki_lastcpu: i32,
    pub ki_tracer: i32,
    pub ki_flag2: i32,
    pub ki_fibnum: i32,
    pub ki_cr_flags: u32,
    pub ki_jid: i32,
    pub ki_numthreads: i32,
    pub ki_tid: i32,
    pub ki_pri: Priority,
    pub ki_rusage: Rusage,
    pub ki_rusage_ch: Rusage,
    pub ki_pcb: u64,
    pub ki_kstack: u64,
    pub ki_udata: u64,
    pub ki_tdaddr: u64,
    pub ki_pd: u64,
    pub ki_uerrmsg: u64,
    pub ki_spareptrs: [u64; KI_NSPARE_PTR],
    pub ki_sparelongs: [i64; KI_NSPARE_LONG],
    pub ki_sflag: i64,
    pub ki_tdflags: i64,
}

impl Default for KinfoProc {
    fn default() -> Self {
        Self {
            ki_structsize: STRUCTSIZE,
            ki_layout: 0,
            ki_args: 0,
            ki_paddr: 0,
            ki_addr: 0,
            ki_tracep: 0,
            ki_textvp: 0,
            ki_fd: 0,
            ki_vmspace: 0,
            ki_wchan: 0,
            ki_pid: 0,
            ki_ppid: 0,
            ki_pgid: 0,
            ki_tpgid: 0,
            ki_sid: 0,
            ki_tsid: 0,
            ki_jobc: 0,
            ki_spare_short1: 0,
            ki_tdev_freebsd11: 0,
            ki_siglist: Sigset::default(),
            ki_sigmask: Sigset::default(),
            ki_sigignore: Sigset::default(),
            ki_sigcatch: Sigset::default(),
            ki_uid: 0,
            ki_ruid: 0,
            ki_svuid: 0,
            ki_rgid: 0,
            ki_svgid: 0,
            ki_ngroups: 0,
            ki_spare_short2: 0,
            ki_groups: [0; KI_NGROUPS],
            ki_size: 0,
            ki_rssize: 0,
            ki_swrss: 0,
            ki_tsize: 0,
            ki_dsize: 0,
            ki_ssize: 0,
            ki_xstat: 0,
            ki_acflag: 0,
            ki_pctcpu: 0,
            ki_estcpu: 0,
            ki_slptime: 0,
            ki_swtime: 0,
            ki_cow: 0,
            ki_runtime: 0,
            ki_start: Timeval::default(),
            ki_childtime: Timeval::default(),
            ki_flag: 0,
            ki_kiflag: 0,
            ki_traceflag: 0,
            ki_stat: 0,
            ki_nice: 0,
            ki_lock: 0,
            ki_rqindex: 0,
            ki_oncpu_old: 0,
            ki_lastcpu_old: 0,
            ki_tdname: [0; TDNAMLEN + 1],
            ki_wmesg: [0; WMESGLEN + 1],
            ki_login: [0; LOGNAMELEN + 1],
            ki_lockname: [0; LOCKNAMELEN + 1],
            ki_comm: [0; COMMLEN + 1],
            ki_emul: [0; KI_EMULNAMELEN + 1],
            ki_loginclass: [0; LOGINCLASSLEN + 1],
            ki_moretdname: [0; MAXCOMLEN - TDNAMLEN + 1],
            ki_sparestrings: [0; 38],
            ki_spareints: [0; KI_NSPARE_INT],
            ki_reaper: 0,
            ki_reapsubtree: 0,
            ki_tdev: 0,
            ki_oncpu: 0,
            ki_lastcpu: 0,
            ki_tracer: 0,
            ki_flag2: 0,
            ki_fibnum: 0,
            ki_cr_flags: 0,
            ki_jid: 0,
            ki_numthreads: 0,
            ki_tid: 0,
            ki_pri: Priority::default(),
            ki_rusage: Rusage::default(),
            ki_rusage_ch: Rusage::default(),
            ki_pcb: 0,
            ki_kstack: 0,
            ki_udata: 0,
            ki_tdaddr: 0,
            ki_pd: 0,
            ki_uerrmsg: 0,
            ki_spareptrs: [0; KI_NSPARE_PTR],
            ki_sparelongs: [0; KI_NSPARE_LONG],
            ki_sflag: 0,
            ki_tdflags: 0,
        }
    }
}

/// `ki_stat`: process states.
pub const SIDL: u8 = 1;
pub const SRUN: u8 = 2;
pub const SSLEEP: u8 = 3;
pub const SSTOP: u8 = 4;
pub const SZOMB: u8 = 5;
pub const SWAIT: u8 = 6;
pub const SLOCK: u8 = 7;

/// `ki_flag` (`P_*`).
pub const P_CONTROLT: i64 = 0x0000_0002;
pub const P_PPWAIT: i64 = 0x0000_0010;
pub const P_SUGID: i64 = 0x0000_0100;
pub const P_HADTHREADS: i64 = 0x0000_0080;
pub const P_WEXIT: i64 = 0x0000_2000;
pub const P_EXEC: i64 = 0x0000_4000;
pub const P_INMEM: i64 = 0x1000_0000;

/// `ki_kiflag`.
pub const KI_CTTY: i64 = 0x1;
pub const KI_SLEADER: i64 = 0x2;

/// `ki_cr_flags`.
pub const KI_CRF_CAPABILITY_MODE: u32 = 0x0000_0001;
pub const KI_CRF_GRP_OVERFLOW: u32 = 0x8000_0000;

/// `NODEV`: no device (`ki_tdev` without a terminal).
pub const NODEV: u64 = u64::MAX;

/// `kern.proc`'s selectors (`KERN_PROC_*`), the third name component.
pub mod select {
    pub const ALL: i32 = 0;
    pub const PID: i32 = 1;
    pub const PGRP: i32 = 2;
    pub const SESSION: i32 = 3;
    pub const TTY: i32 = 4;
    pub const UID: i32 = 5;
    pub const RUID: i32 = 6;
    pub const ARGS: i32 = 7;
    pub const PROC: i32 = 8;
    pub const RGID: i32 = 10;
    pub const GID: i32 = 11;
    pub const PATHNAME: i32 = 12;
    pub const AUXV: i32 = 36;
    /// Or'd in: one entry per thread.
    pub const INC_THREAD: i32 = 0x10;
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("KI_NGROUPS", "sys/user.h", KI_NGROUPS as i64),
    c("WMESGLEN", "sys/user.h", WMESGLEN as i64),
    c("LOCKNAMELEN", "sys/user.h", LOCKNAMELEN as i64),
    c("TDNAMLEN", "sys/user.h", TDNAMLEN as i64),
    c("COMMLEN", "sys/user.h", COMMLEN as i64),
    c("KI_EMULNAMELEN", "sys/user.h", KI_EMULNAMELEN as i64),
    c("LOGNAMELEN", "sys/user.h", LOGNAMELEN as i64),
    c("LOGINCLASSLEN", "sys/user.h", LOGINCLASSLEN as i64),
    c("MAXCOMLEN", "sys/param.h", MAXCOMLEN as i64),
    c("KI_NSPARE_INT", "sys/user.h", KI_NSPARE_INT as i64),
    c("KI_NSPARE_LONG", "sys/user.h", KI_NSPARE_LONG as i64),
    c("KI_NSPARE_PTR", "sys/user.h", KI_NSPARE_PTR as i64),
    c("KINFO_PROC_SIZE", "sys/user.h", KINFO_PROC_SIZE as i64),
    c("SIDL", "sys/proc.h", SIDL as i64),
    c("SRUN", "sys/proc.h", SRUN as i64),
    c("SSLEEP", "sys/proc.h", SSLEEP as i64),
    c("SSTOP", "sys/proc.h", SSTOP as i64),
    c("SZOMB", "sys/proc.h", SZOMB as i64),
    c("SWAIT", "sys/proc.h", SWAIT as i64),
    c("SLOCK", "sys/proc.h", SLOCK as i64),
    c("P_CONTROLT", "sys/proc.h", P_CONTROLT),
    c("P_PPWAIT", "sys/proc.h", P_PPWAIT),
    c("P_SUGID", "sys/proc.h", P_SUGID),
    c(
        "KI_CRF_CAPABILITY_MODE",
        "sys/user.h",
        KI_CRF_CAPABILITY_MODE as i64,
    ),
    c(
        "KI_CRF_GRP_OVERFLOW",
        "sys/user.h",
        KI_CRF_GRP_OVERFLOW as i64,
    ),
    c("P_HADTHREADS", "sys/proc.h", P_HADTHREADS),
    c("P_WEXIT", "sys/proc.h", P_WEXIT),
    c("P_EXEC", "sys/proc.h", P_EXEC),
    c("P_INMEM", "sys/proc.h", P_INMEM),
    c("KI_CTTY", "sys/user.h", KI_CTTY),
    c("KI_SLEADER", "sys/user.h", KI_SLEADER),
    c("KERN_PROC_ALL", "sys/sysctl.h", select::ALL as i64),
    c("KERN_PROC_PID", "sys/sysctl.h", select::PID as i64),
    c("KERN_PROC_PGRP", "sys/sysctl.h", select::PGRP as i64),
    c("KERN_PROC_SESSION", "sys/sysctl.h", select::SESSION as i64),
    c("KERN_PROC_TTY", "sys/sysctl.h", select::TTY as i64),
    c("KERN_PROC_UID", "sys/sysctl.h", select::UID as i64),
    c("KERN_PROC_RUID", "sys/sysctl.h", select::RUID as i64),
    c("KERN_PROC_ARGS", "sys/sysctl.h", select::ARGS as i64),
    c("KERN_PROC_RGID", "sys/sysctl.h", select::RGID as i64),
    c("KERN_PROC_GID", "sys/sysctl.h", select::GID as i64),
    c("KERN_PROC_PROC", "sys/sysctl.h", select::PROC as i64),
    c(
        "KERN_PROC_PATHNAME",
        "sys/sysctl.h",
        select::PATHNAME as i64,
    ),
    c("KERN_PROC_AUXV", "sys/sysctl.h", select::AUXV as i64),
    c(
        "KERN_PROC_INC_THREAD",
        "sys/sysctl.h",
        select::INC_THREAD as i64,
    ),
    c("NODEV", "sys/param.h", NODEV as i64),
];

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No padding the compiler adds: every byte is a field's, so the
    /// structure can be copied out as bytes.
    #[test]
    fn no_implicit_padding() {
        let k = KinfoProc::default();
        let fields: usize = [
            size_of_val(&k.ki_structsize) * 2,
            8 * 8,
            4 * 6,
            2 * 2,
            4,
            size_of::<Sigset>() * 4,
            4 * 5,
            2 * 2,
            size_of_val(&k.ki_groups),
            8 * 6,
            2 * 2,
            4 * 5,
            8,
            size_of::<Timeval>() * 2,
            8 * 2,
            4,
            // ki_stat to ki_lastcpu_old.
            6,
            size_of_val(&k.ki_tdname)
                + size_of_val(&k.ki_wmesg)
                + size_of_val(&k.ki_login)
                + size_of_val(&k.ki_lockname)
                + size_of_val(&k.ki_comm)
                + size_of_val(&k.ki_emul)
                + size_of_val(&k.ki_loginclass)
                + size_of_val(&k.ki_moretdname)
                + size_of_val(&k.ki_sparestrings),
            size_of_val(&k.ki_spareints),
            4 * 2,
            8,
            4 * 9,
            size_of::<Priority>(),
            size_of::<Rusage>() * 2,
            8 * 6,
            size_of_val(&k.ki_spareptrs),
            size_of_val(&k.ki_sparelongs),
            8 * 2,
        ]
        .iter()
        .sum();
        assert_eq!(size_of::<KinfoProc>(), KINFO_PROC_SIZE);
        assert_eq!(usize::try_from(STRUCTSIZE), Ok(KINFO_PROC_SIZE));
        assert_eq!(fields, KINFO_PROC_SIZE);
    }
}
