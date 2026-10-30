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
    amd64("KINFO_FILE_SIZE", KINFO_FILE_SIZE),
    amd64("KINFO_VMENTRY_SIZE", KINFO_VMENTRY_SIZE),
    c("KKST_MAXLEN", "sys/user.h", KKST_MAXLEN as i64),
    amd64("KINFO_KSTACK_SIZE", KINFO_KSTACK_SIZE),
    c("KF_TYPE_NONE", "sys/user.h", KF_TYPE_NONE as i64),
    c("KF_TYPE_VNODE", "sys/user.h", KF_TYPE_VNODE as i64),
    c("KF_TYPE_SOCKET", "sys/user.h", KF_TYPE_SOCKET as i64),
    c("KF_TYPE_PIPE", "sys/user.h", KF_TYPE_PIPE as i64),
    c("KF_TYPE_FIFO", "sys/user.h", KF_TYPE_FIFO as i64),
    c("KF_TYPE_KQUEUE", "sys/user.h", KF_TYPE_KQUEUE as i64),
    c("KF_TYPE_MQUEUE", "sys/user.h", KF_TYPE_MQUEUE as i64),
    c("KF_TYPE_SHM", "sys/user.h", KF_TYPE_SHM as i64),
    c("KF_TYPE_SEM", "sys/user.h", KF_TYPE_SEM as i64),
    c("KF_TYPE_PTS", "sys/user.h", KF_TYPE_PTS as i64),
    c("KF_TYPE_PROCDESC", "sys/user.h", KF_TYPE_PROCDESC as i64),
    c("KF_TYPE_DEV", "sys/user.h", KF_TYPE_DEV as i64),
    c("KF_TYPE_EVENTFD", "sys/user.h", KF_TYPE_EVENTFD as i64),
    c("KF_TYPE_TIMERFD", "sys/user.h", KF_TYPE_TIMERFD as i64),
    c("KF_TYPE_INOTIFY", "sys/user.h", KF_TYPE_INOTIFY as i64),
    c("KF_TYPE_JAILDESC", "sys/user.h", KF_TYPE_JAILDESC as i64),
    c("KF_TYPE_UNKNOWN", "sys/user.h", KF_TYPE_UNKNOWN as i64),
    c("KF_VTYPE_VNON", "sys/user.h", KF_VTYPE_VNON as i64),
    c("KF_VTYPE_VREG", "sys/user.h", KF_VTYPE_VREG as i64),
    c("KF_VTYPE_VDIR", "sys/user.h", KF_VTYPE_VDIR as i64),
    c("KF_VTYPE_VBLK", "sys/user.h", KF_VTYPE_VBLK as i64),
    c("KF_VTYPE_VCHR", "sys/user.h", KF_VTYPE_VCHR as i64),
    c("KF_VTYPE_VLNK", "sys/user.h", KF_VTYPE_VLNK as i64),
    c("KF_VTYPE_VSOCK", "sys/user.h", KF_VTYPE_VSOCK as i64),
    c("KF_VTYPE_VFIFO", "sys/user.h", KF_VTYPE_VFIFO as i64),
    c("KF_VTYPE_VBAD", "sys/user.h", KF_VTYPE_VBAD as i64),
    c("KF_VTYPE_UNKNOWN", "sys/user.h", KF_VTYPE_UNKNOWN as i64),
    c("KF_FD_TYPE_CWD", "sys/user.h", KF_FD_TYPE_CWD as i64),
    c("KF_FD_TYPE_ROOT", "sys/user.h", KF_FD_TYPE_ROOT as i64),
    c("KF_FD_TYPE_JAIL", "sys/user.h", KF_FD_TYPE_JAIL as i64),
    c("KF_FD_TYPE_TRACE", "sys/user.h", KF_FD_TYPE_TRACE as i64),
    c("KF_FD_TYPE_TEXT", "sys/user.h", KF_FD_TYPE_TEXT as i64),
    c("KF_FD_TYPE_CTTY", "sys/user.h", KF_FD_TYPE_CTTY as i64),
    c("KF_FLAG_READ", "sys/user.h", KF_FLAG_READ as i64),
    c("KF_FLAG_WRITE", "sys/user.h", KF_FLAG_WRITE as i64),
    c("KF_FLAG_APPEND", "sys/user.h", KF_FLAG_APPEND as i64),
    c("KF_FLAG_ASYNC", "sys/user.h", KF_FLAG_ASYNC as i64),
    c("KF_FLAG_FSYNC", "sys/user.h", KF_FLAG_FSYNC as i64),
    c("KF_FLAG_NONBLOCK", "sys/user.h", KF_FLAG_NONBLOCK as i64),
    c("KF_FLAG_DIRECT", "sys/user.h", KF_FLAG_DIRECT as i64),
    c("KF_FLAG_HASLOCK", "sys/user.h", KF_FLAG_HASLOCK as i64),
    c("KF_FLAG_SHLOCK", "sys/user.h", KF_FLAG_SHLOCK as i64),
    c("KF_FLAG_EXLOCK", "sys/user.h", KF_FLAG_EXLOCK as i64),
    c("KF_FLAG_NOFOLLOW", "sys/user.h", KF_FLAG_NOFOLLOW as i64),
    c("KF_FLAG_CREAT", "sys/user.h", KF_FLAG_CREAT as i64),
    c("KF_FLAG_TRUNC", "sys/user.h", KF_FLAG_TRUNC as i64),
    c("KF_FLAG_EXCL", "sys/user.h", KF_FLAG_EXCL as i64),
    c("KF_FLAG_EXEC", "sys/user.h", KF_FLAG_EXEC as i64),
    c("KF_ATTR_VALID", "sys/user.h", KF_ATTR_VALID as i64),
    c("KVME_TYPE_NONE", "sys/user.h", KVME_TYPE_NONE as i64),
    c("KVME_TYPE_DEFAULT", "sys/user.h", KVME_TYPE_DEFAULT as i64),
    c("KVME_TYPE_VNODE", "sys/user.h", KVME_TYPE_VNODE as i64),
    c("KVME_TYPE_SWAP", "sys/user.h", KVME_TYPE_SWAP as i64),
    c("KVME_TYPE_DEVICE", "sys/user.h", KVME_TYPE_DEVICE as i64),
    c("KVME_TYPE_PHYS", "sys/user.h", KVME_TYPE_PHYS as i64),
    c("KVME_TYPE_DEAD", "sys/user.h", KVME_TYPE_DEAD as i64),
    c("KVME_TYPE_SG", "sys/user.h", KVME_TYPE_SG as i64),
    c(
        "KVME_TYPE_MGTDEVICE",
        "sys/user.h",
        KVME_TYPE_MGTDEVICE as i64,
    ),
    c("KVME_TYPE_GUARD", "sys/user.h", KVME_TYPE_GUARD as i64),
    c("KVME_TYPE_UNKNOWN", "sys/user.h", KVME_TYPE_UNKNOWN as i64),
    c("KVME_PROT_READ", "sys/user.h", KVME_PROT_READ as i64),
    c("KVME_PROT_WRITE", "sys/user.h", KVME_PROT_WRITE as i64),
    c("KVME_PROT_EXEC", "sys/user.h", KVME_PROT_EXEC as i64),
    c(
        "KVME_MAX_PROT_READ",
        "sys/user.h",
        KVME_MAX_PROT_READ as i64,
    ),
    c(
        "KVME_MAX_PROT_WRITE",
        "sys/user.h",
        KVME_MAX_PROT_WRITE as i64,
    ),
    c(
        "KVME_MAX_PROT_EXEC",
        "sys/user.h",
        KVME_MAX_PROT_EXEC as i64,
    ),
    c("KVME_FLAG_COW", "sys/user.h", KVME_FLAG_COW as i64),
    c(
        "KVME_FLAG_NEEDS_COPY",
        "sys/user.h",
        KVME_FLAG_NEEDS_COPY as i64,
    ),
    c(
        "KVME_FLAG_NOCOREDUMP",
        "sys/user.h",
        KVME_FLAG_NOCOREDUMP as i64,
    ),
    c("KVME_FLAG_SUPER", "sys/user.h", KVME_FLAG_SUPER as i64),
    c(
        "KVME_FLAG_GROWS_UP",
        "sys/user.h",
        KVME_FLAG_GROWS_UP as i64,
    ),
    c(
        "KVME_FLAG_GROWS_DOWN",
        "sys/user.h",
        KVME_FLAG_GROWS_DOWN as i64,
    ),
    c(
        "KVME_FLAG_USER_WIRED",
        "sys/user.h",
        KVME_FLAG_USER_WIRED as i64,
    ),
    c("KVME_FLAG_SYSVSHM", "sys/user.h", KVME_FLAG_SYSVSHM as i64),
    c(
        "KVME_FLAG_POSIXSHM",
        "sys/user.h",
        KVME_FLAG_POSIXSHM as i64,
    ),
    c(
        "KKST_STATE_STACKOK",
        "sys/user.h",
        KKST_STATE_STACKOK as i64,
    ),
    c(
        "KKST_STATE_SWAPPED",
        "sys/user.h",
        KKST_STATE_SWAPPED as i64,
    ),
    c(
        "KKST_STATE_RUNNING",
        "sys/user.h",
        KKST_STATE_RUNNING as i64,
    ),
    c(
        "KMAP_FLAG_WIREFUTURE",
        "sys/user.h",
        KMAP_FLAG_WIREFUTURE as i64,
    ),
    c("KMAP_FLAG_ASLR", "sys/user.h", KMAP_FLAG_ASLR as i64),
    c(
        "KMAP_FLAG_ASLR_IGNSTART",
        "sys/user.h",
        KMAP_FLAG_ASLR_IGNSTART as i64,
    ),
    c("KMAP_FLAG_WXORX", "sys/user.h", KMAP_FLAG_WXORX as i64),
    c(
        "KMAP_FLAG_ASLR_STACK",
        "sys/user.h",
        KMAP_FLAG_ASLR_STACK as i64,
    ),
    c(
        "KMAP_FLAG_ASLR_SHARED_PAGE",
        "sys/user.h",
        KMAP_FLAG_ASLR_SHARED_PAGE as i64,
    ),
    c(
        "KNOTE_STATUS_ACTIVE",
        "sys/user.h",
        KNOTE_STATUS_ACTIVE as i64,
    ),
    c(
        "KNOTE_STATUS_QUEUED",
        "sys/user.h",
        KNOTE_STATUS_QUEUED as i64,
    ),
    c(
        "KNOTE_STATUS_DISABLED",
        "sys/user.h",
        KNOTE_STATUS_DISABLED as i64,
    ),
    c(
        "KNOTE_STATUS_DETACHED",
        "sys/user.h",
        KNOTE_STATUS_DETACHED as i64,
    ),
    c(
        "KNOTE_STATUS_KQUEUE",
        "sys/user.h",
        KNOTE_STATUS_KQUEUE as i64,
    ),
    c(
        "KNOTE_EXTDATA_NONE",
        "sys/user.h",
        KNOTE_EXTDATA_NONE as i64,
    ),
    c(
        "KNOTE_EXTDATA_VNODE",
        "sys/user.h",
        KNOTE_EXTDATA_VNODE as i64,
    ),
    c(
        "KNOTE_EXTDATA_PIPE",
        "sys/user.h",
        KNOTE_EXTDATA_PIPE as i64,
    ),
    c("KLOCKF_RW_READ", "sys/user.h", KLOCKF_RW_READ as i64),
    c("KLOCKF_RW_WRITE", "sys/user.h", KLOCKF_RW_WRITE as i64),
    c("KLOCKF_TYPE_FLOCK", "sys/user.h", KLOCKF_TYPE_FLOCK as i64),
    c("KLOCKF_TYPE_PID", "sys/user.h", KLOCKF_TYPE_PID as i64),
    c(
        "KLOCKF_TYPE_REMOTE",
        "sys/user.h",
        KLOCKF_TYPE_REMOTE as i64,
    ),
    c("KI_LOCKBLOCK", "sys/user.h", KI_LOCKBLOCK),
    c("PS_INMEM", "sys/user.h", PS_INMEM),
    c("TDF_INMEM", "sys/proc.h", TDF_INMEM),
    c("TDF_SINTR", "sys/proc.h", TDF_SINTR),
    c("TDF_IDLETD", "sys/proc.h", TDF_IDLETD),
    c("P_ADVLOCK", "sys/proc.h", P_ADVLOCK),
    c("P_KPROC", "sys/proc.h", P_KPROC),
    c("P_SYSTEM", "sys/proc.h", P_SYSTEM),
    c("P_TRACED", "sys/proc.h", P_TRACED),
    c("P_JAILED", "sys/proc.h", P_JAILED),
    c("P_STOPPED_SIG", "sys/proc.h", P_STOPPED_SIG),
    c("P_STOPPED_TRACE", "sys/proc.h", P_STOPPED_TRACE),
    c("PRI_ITHD", "sys/priority.h", PRI_ITHD as i64),
    c("PRI_REALTIME", "sys/priority.h", PRI_REALTIME as i64),
    c("PRI_TIMESHARE", "sys/priority.h", PRI_TIMESHARE as i64),
    c("PRI_IDLE", "sys/priority.h", PRI_IDLE as i64),
    c(
        "PRI_MIN_REALTIME",
        "sys/priority.h",
        PRI_MIN_REALTIME as i64,
    ),
    c("PRI_MIN_KERN", "sys/priority.h", PRI_MIN_KERN as i64),
    c(
        "PRI_MIN_TIMESHARE",
        "sys/priority.h",
        PRI_MIN_TIMESHARE as i64,
    ),
    c(
        "PRI_MAX_TIMESHARE",
        "sys/priority.h",
        PRI_MAX_TIMESHARE as i64,
    ),
    c("PRI_MIN_IDLE", "sys/priority.h", PRI_MIN_IDLE as i64),
    c("PRI_MAX_IDLE", "sys/priority.h", PRI_MAX_IDLE as i64),
    c("PZERO", "sys/priority.h", PZERO as i64),
    c("PUSER", "sys/priority.h", PUSER as i64),
    c("KERN_PROC_SV_NAME", "sys/sysctl.h", node::SV_NAME as i64),
    c("KERN_PROC_OVMMAP", "sys/sysctl.h", node::OVMMAP as i64),
    c(
        "KERN_PROC_OFILEDESC",
        "sys/sysctl.h",
        node::OFILEDESC as i64,
    ),
    c("KERN_PROC_KSTACK", "sys/sysctl.h", node::KSTACK as i64),
    c("KERN_PROC_VMMAP", "sys/sysctl.h", node::VMMAP as i64),
    c("KERN_PROC_FILEDESC", "sys/sysctl.h", node::FILEDESC as i64),
    c("KERN_PROC_GROUPS", "sys/sysctl.h", node::GROUPS as i64),
    c("KERN_PROC_ENV", "sys/sysctl.h", node::ENV as i64),
    c("KERN_PROC_RLIMIT", "sys/sysctl.h", node::RLIMIT as i64),
    c(
        "KERN_PROC_PS_STRINGS",
        "sys/sysctl.h",
        node::PS_STRINGS as i64,
    ),
    c("KERN_PROC_UMASK", "sys/sysctl.h", node::UMASK as i64),
    c("KERN_PROC_OSREL", "sys/sysctl.h", node::OSREL as i64),
    c("KERN_PROC_SIGTRAMP", "sys/sysctl.h", node::SIGTRAMP as i64),
    c("KERN_PROC_CWD", "sys/sysctl.h", node::CWD as i64),
    c("KERN_PROC_NFDS", "sys/sysctl.h", node::NFDS as i64),
    c(
        "KERN_PROC_SIGFASTBLK",
        "sys/sysctl.h",
        node::SIGFASTBLK as i64,
    ),
    c(
        "KERN_PROC_VM_LAYOUT",
        "sys/sysctl.h",
        node::VM_LAYOUT as i64,
    ),
    c(
        "KERN_PROC_RLIMIT_USAGE",
        "sys/sysctl.h",
        node::RLIMIT_USAGE as i64,
    ),
    c("KERN_PROC_KQUEUE", "sys/sysctl.h", node::KQUEUE as i64),
    c("KERN_LOCKF", "sys/sysctl.h", KERN_LOCKF as i64),
];

/// A record size `sys/user.h` defines for amd64 only.
const fn amd64(c_name: &'static str, value: usize) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/user.h",
        value: value as i64,
        arches: Arches::Amd64,
    }
}

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}

/// `PATH_MAX`: the paths records end in.
pub const PATH_MAX: usize = 1024;

/// `struct kinfo_file`'s fixed part and its per-type part `U` (one of the
/// `kf_un` union's members, each padded to the union's 304 bytes), as
/// `kern.proc.filedesc` and `F_KINFO` return it (`sys/user.h`). A record
/// is cut after its path's NUL and rounded up to 8 bytes, its
/// `kf_structsize` saying how long it is (`pack_kinfo`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KinfoFile<U> {
    pub kf_structsize: i32,
    pub kf_type: i32,
    pub kf_fd: i32,
    pub kf_ref_count: i32,
    pub kf_flags: i32,
    pub kf_pad0: i32,
    pub kf_offset: i64,
    pub kf_un: U,
    pub kf_status: u16,
    pub kf_pad1: u16,
    pub _kf_ispare0: i32,
    pub kf_cap_rights: crate::capsicum::CapRights,
    pub _kf_cap_spare: u64,
    pub kf_path: [u8; PATH_MAX],
}

/// `KINFO_FILE_SIZE`.
pub const KINFO_FILE_SIZE: usize = 1392;
/// `kf_structsize` of a whole record.
const KF_STRUCTSIZE: i32 = 1392;
/// The size of `kf_un`.
pub const KF_UN_SIZE: usize = 304;

impl<U: Default> Default for KinfoFile<U> {
    fn default() -> Self {
        Self {
            kf_structsize: KF_STRUCTSIZE,
            kf_type: KF_TYPE_NONE,
            kf_fd: 0,
            kf_ref_count: 0,
            kf_flags: 0,
            kf_pad0: 0,
            kf_offset: 0,
            kf_un: U::default(),
            kf_status: 0,
            kf_pad1: 0,
            _kf_ispare0: 0,
            kf_cap_rights: crate::capsicum::CapRights { cr_rights: [0; 2] },
            _kf_cap_spare: 0,
            kf_path: [0; PATH_MAX],
        }
    }
}

/// A `struct sockaddr_storage`, as bytes.
pub type SockaddrStorage = [u8; 128];

/// `kf_un.kf_sock`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KfSock {
    pub kf_sock_sendq: u32,
    pub kf_sock_domain0: i32,
    pub kf_sock_type0: i32,
    pub kf_sock_protocol0: i32,
    pub kf_sa_local: SockaddrStorage,
    pub kf_sa_peer: SockaddrStorage,
    pub kf_sock_pcb: u64,
    pub kf_sock_inpcb: u64,
    pub kf_sock_unpconn: u64,
    pub kf_sock_snd_sb_state: u16,
    pub kf_sock_rcv_sb_state: u16,
    pub kf_sock_recvq: u32,
}

impl Default for KfSock {
    fn default() -> Self {
        Self {
            kf_sock_sendq: 0,
            kf_sock_domain0: 0,
            kf_sock_type0: 0,
            kf_sock_protocol0: 0,
            kf_sa_local: [0; 128],
            kf_sa_peer: [0; 128],
            kf_sock_pcb: 0,
            kf_sock_inpcb: 0,
            kf_sock_unpconn: 0,
            kf_sock_snd_sb_state: 0,
            kf_sock_rcv_sb_state: 0,
            kf_sock_recvq: 0,
        }
    }
}

/// `kf_un.kf_file`: vnodes (and FIFOs, devices).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfFile {
    pub kf_file_type: i32,
    pub kf_spareint: [i32; 3],
    pub kf_spareint64: [u64; 29],
    pub kf_file_nlink: u64,
    pub kf_file_fsid: u64,
    pub kf_file_rdev: u64,
    pub kf_file_fileid: u64,
    pub kf_file_size: u64,
    pub kf_file_fsid_freebsd11: u32,
    pub kf_file_rdev_freebsd11: u32,
    pub kf_file_mode: u16,
    pub kf_file_pad0: u16,
    pub kf_file_pad1: u32,
}

/// `kf_un.kf_sem` (POSIX semaphores; padded to the union's size).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfSem {
    pub kf_spareint: [u32; 4],
    pub kf_spareint64: [u64; 32],
    pub kf_sem_value: u32,
    pub kf_sem_mode: u16,
    pub _pad: [u16; 13],
}

/// `kf_un.kf_pipe`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfPipe {
    pub kf_spareint: [u32; 4],
    pub kf_spareint64: [u64; 32],
    pub kf_pipe_addr: u64,
    pub kf_pipe_peer: u64,
    pub kf_pipe_buffer_cnt: u32,
    pub kf_pipe_buffer_in: u32,
    pub kf_pipe_buffer_out: u32,
    pub kf_pipe_buffer_size: u32,
}

/// `kf_un.kf_pts`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfPts {
    pub kf_spareint: [u32; 4],
    pub kf_spareint64: [u64; 32],
    pub kf_pts_dev_freebsd11: u32,
    pub kf_pts_pad0: u32,
    pub kf_pts_dev: u64,
    pub kf_pts_pad1: [u32; 4],
}

/// `kf_un.kf_proc` (process descriptors; padded).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfProc {
    pub kf_spareint: [u32; 4],
    pub kf_spareint64: [u64; 32],
    pub kf_pid: i32,
    pub _pad: [u32; 7],
}

/// `kf_un.kf_eventfd` (padded).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfEventfd {
    pub kf_eventfd_value: u64,
    pub kf_eventfd_flags: u32,
    pub kf_eventfd_spareint: [u32; 3],
    pub kf_eventfd_addr: u64,
    pub _pad: KfPad<34>,
}

/// `kf_un.kf_inotify` (padded).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfInotify {
    pub kf_inotify_npending: u64,
    pub kf_inotify_nbpending: u64,
    pub _pad: KfPad<36>,
}

/// `kf_un.kf_timerfd` (padded).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfTimerfd {
    pub kf_timerfd_clockid: u32,
    pub kf_timerfd_flags: u32,
    pub kf_timerfd_addr: u64,
    pub _pad: KfPad<36>,
}

/// `kf_un.kf_jail` (padded).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfJail {
    pub kf_jid: i32,
    pub _pad0: u32,
    pub _pad: KfPad<37>,
}

/// `kf_un.kf_kqueue` (padded).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfKqueue {
    pub kf_kqueue_addr: u64,
    pub kf_kqueue_count: i32,
    pub kf_kqueue_state: i32,
    pub _pad: KfPad<36>,
}

/// No per-type part (`KF_TYPE_NONE`, `KF_TYPE_SHM`'s is in `kf_file`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KfNone {
    pub _pad: KfPad<38>,
}

/// `N` words of padding, zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KfPad<const N: usize>(pub [u64; N]);

impl<const N: usize> Default for KfPad<N> {
    fn default() -> Self {
        Self([0; N])
    }
}

const _: () = {
    assert!(size_of::<KfSock>() == KF_UN_SIZE);
    assert!(size_of::<KfFile>() == KF_UN_SIZE);
    assert!(size_of::<KfSem>() == KF_UN_SIZE);
    assert!(size_of::<KfPipe>() == KF_UN_SIZE);
    assert!(size_of::<KfPts>() == KF_UN_SIZE);
    assert!(size_of::<KfProc>() == KF_UN_SIZE);
    assert!(size_of::<KfEventfd>() == KF_UN_SIZE);
    assert!(size_of::<KfTimerfd>() == KF_UN_SIZE);
    assert!(size_of::<KfInotify>() == KF_UN_SIZE);
    assert!(size_of::<KfJail>() == KF_UN_SIZE);
    assert!(size_of::<KfKqueue>() == KF_UN_SIZE);
    assert!(size_of::<KfNone>() == KF_UN_SIZE);
    assert!(size_of::<KinfoFile<KfNone>>() == KINFO_FILE_SIZE);
    assert!(KF_STRUCTSIZE as usize == KINFO_FILE_SIZE);
};

/// `kf_type`.
pub const KF_TYPE_NONE: i32 = 0;
pub const KF_TYPE_VNODE: i32 = 1;
pub const KF_TYPE_SOCKET: i32 = 2;
pub const KF_TYPE_PIPE: i32 = 3;
pub const KF_TYPE_FIFO: i32 = 4;
pub const KF_TYPE_KQUEUE: i32 = 5;
pub const KF_TYPE_MQUEUE: i32 = 7;
pub const KF_TYPE_SHM: i32 = 8;
pub const KF_TYPE_SEM: i32 = 9;
pub const KF_TYPE_PTS: i32 = 10;
pub const KF_TYPE_PROCDESC: i32 = 11;
pub const KF_TYPE_DEV: i32 = 12;
pub const KF_TYPE_EVENTFD: i32 = 13;
pub const KF_TYPE_TIMERFD: i32 = 14;
pub const KF_TYPE_INOTIFY: i32 = 15;
pub const KF_TYPE_JAILDESC: i32 = 16;
pub const KF_TYPE_UNKNOWN: i32 = 255;

/// `kf_file_type` / `kf_vnode_type` (`KF_VTYPE_*`).
pub const KF_VTYPE_VNON: i32 = 0;
pub const KF_VTYPE_VREG: i32 = 1;
pub const KF_VTYPE_VDIR: i32 = 2;
pub const KF_VTYPE_VBLK: i32 = 3;
pub const KF_VTYPE_VCHR: i32 = 4;
pub const KF_VTYPE_VLNK: i32 = 5;
pub const KF_VTYPE_VSOCK: i32 = 6;
pub const KF_VTYPE_VFIFO: i32 = 7;
pub const KF_VTYPE_VBAD: i32 = 8;
pub const KF_VTYPE_UNKNOWN: i32 = 255;

/// `kf_fd` for what is not a descriptor.
pub const KF_FD_TYPE_CWD: i32 = -1;
pub const KF_FD_TYPE_ROOT: i32 = -2;
pub const KF_FD_TYPE_JAIL: i32 = -3;
pub const KF_FD_TYPE_TRACE: i32 = -4;
pub const KF_FD_TYPE_TEXT: i32 = -5;
pub const KF_FD_TYPE_CTTY: i32 = -6;

/// `kf_flags`.
pub const KF_FLAG_READ: i32 = 0x0000_0001;
pub const KF_FLAG_WRITE: i32 = 0x0000_0002;
pub const KF_FLAG_APPEND: i32 = 0x0000_0004;
pub const KF_FLAG_ASYNC: i32 = 0x0000_0008;
pub const KF_FLAG_FSYNC: i32 = 0x0000_0010;
pub const KF_FLAG_NONBLOCK: i32 = 0x0000_0020;
pub const KF_FLAG_DIRECT: i32 = 0x0000_0040;
pub const KF_FLAG_HASLOCK: i32 = 0x0000_0080;
pub const KF_FLAG_SHLOCK: i32 = 0x0000_0100;
pub const KF_FLAG_EXLOCK: i32 = 0x0000_0200;
pub const KF_FLAG_NOFOLLOW: i32 = 0x0000_0400;
pub const KF_FLAG_CREAT: i32 = 0x0000_0800;
pub const KF_FLAG_TRUNC: i32 = 0x0000_1000;
pub const KF_FLAG_EXCL: i32 = 0x0000_2000;
pub const KF_FLAG_EXEC: i32 = 0x0000_4000;

/// `kf_status`.
pub const KF_ATTR_VALID: u16 = 0x0001;

/// `struct kinfo_vmentry` (`kern.proc.vmmap`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KinfoVmentry {
    pub kve_structsize: i32,
    pub kve_type: i32,
    pub kve_start: u64,
    pub kve_end: u64,
    pub kve_offset: u64,
    pub kve_vn_fileid: u64,
    pub kve_vn_fsid_freebsd11: u32,
    pub kve_flags: i32,
    pub kve_resident: i32,
    pub kve_private_resident: i32,
    pub kve_protection: i32,
    pub kve_ref_count: i32,
    pub kve_shadow_count: i32,
    pub kve_vn_type: i32,
    pub kve_vn_size: u64,
    pub kve_vn_rdev_freebsd11: u32,
    pub kve_vn_mode: u16,
    pub kve_status: u16,
    /// `kve_vn_fsid` for a vnode, `kve_obj` otherwise.
    pub kve_type_spec: u64,
    pub kve_vn_rdev: u64,
    pub _kve_ispare: [i32; 8],
    pub kve_path: [u8; PATH_MAX],
}

/// `KINFO_VMENTRY_SIZE`.
pub const KINFO_VMENTRY_SIZE: usize = 1160;
/// `kve_structsize` of a whole record.
const KVE_STRUCTSIZE: i32 = 1160;

impl Default for KinfoVmentry {
    fn default() -> Self {
        Self {
            kve_structsize: KVE_STRUCTSIZE,
            kve_type: KVME_TYPE_NONE,
            kve_start: 0,
            kve_end: 0,
            kve_offset: 0,
            kve_vn_fileid: 0,
            kve_vn_fsid_freebsd11: 0,
            kve_flags: 0,
            kve_resident: 0,
            kve_private_resident: 0,
            kve_protection: 0,
            kve_ref_count: 0,
            kve_shadow_count: 0,
            kve_vn_type: 0,
            kve_vn_size: 0,
            kve_vn_rdev_freebsd11: 0,
            kve_vn_mode: 0,
            kve_status: 0,
            kve_type_spec: 0,
            kve_vn_rdev: 0,
            _kve_ispare: [0; 8],
            kve_path: [0; PATH_MAX],
        }
    }
}

const _: () = assert!(size_of::<KinfoVmentry>() == KINFO_VMENTRY_SIZE);
const _: () = assert!(KVE_STRUCTSIZE as usize == KINFO_VMENTRY_SIZE);

/// `kve_type`.
pub const KVME_TYPE_NONE: i32 = 0;
pub const KVME_TYPE_DEFAULT: i32 = 1;
pub const KVME_TYPE_VNODE: i32 = 2;
pub const KVME_TYPE_SWAP: i32 = 3;
pub const KVME_TYPE_DEVICE: i32 = 4;
pub const KVME_TYPE_PHYS: i32 = 5;
pub const KVME_TYPE_DEAD: i32 = 6;
pub const KVME_TYPE_SG: i32 = 7;
pub const KVME_TYPE_MGTDEVICE: i32 = 8;
pub const KVME_TYPE_GUARD: i32 = 9;
pub const KVME_TYPE_UNKNOWN: i32 = 255;

/// `kve_protection`.
pub const KVME_PROT_READ: i32 = 0x0000_0001;
pub const KVME_PROT_WRITE: i32 = 0x0000_0002;
pub const KVME_PROT_EXEC: i32 = 0x0000_0004;
pub const KVME_MAX_PROT_READ: i32 = 0x0001_0000;
pub const KVME_MAX_PROT_WRITE: i32 = 0x0002_0000;
pub const KVME_MAX_PROT_EXEC: i32 = 0x0004_0000;

/// `kve_flags`.
pub const KVME_FLAG_COW: i32 = 0x0000_0001;
pub const KVME_FLAG_NEEDS_COPY: i32 = 0x0000_0002;
pub const KVME_FLAG_NOCOREDUMP: i32 = 0x0000_0004;
pub const KVME_FLAG_SUPER: i32 = 0x0000_0008;
pub const KVME_FLAG_GROWS_UP: i32 = 0x0000_0010;
pub const KVME_FLAG_GROWS_DOWN: i32 = 0x0000_0020;
pub const KVME_FLAG_USER_WIRED: i32 = 0x0000_0040;
pub const KVME_FLAG_SYSVSHM: i32 = 0x0000_0080;
pub const KVME_FLAG_POSIXSHM: i32 = 0x0000_0100;

/// `struct kinfo_kstack` (`kern.proc.kstack`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KinfoKstack {
    pub kkst_tid: i32,
    pub kkst_state: i32,
    pub kkst_trace: [u8; KKST_MAXLEN],
    pub _kkst_ispare: [i32; 16],
}

/// `KKST_MAXLEN` and `KINFO_KSTACK_SIZE`.
pub const KKST_MAXLEN: usize = 1024;
pub const KINFO_KSTACK_SIZE: usize = 1096;

impl Default for KinfoKstack {
    fn default() -> Self {
        Self {
            kkst_tid: 0,
            kkst_state: KKST_STATE_STACKOK,
            kkst_trace: [0; KKST_MAXLEN],
            _kkst_ispare: [0; 16],
        }
    }
}

const _: () = assert!(size_of::<KinfoKstack>() == KINFO_KSTACK_SIZE);

/// `kkst_state`.
pub const KKST_STATE_STACKOK: i32 = 0;
pub const KKST_STATE_SWAPPED: i32 = 1;
pub const KKST_STATE_RUNNING: i32 = 2;

/// `struct kinfo_sigtramp` (`kern.proc.sigtramp`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KinfoSigtramp {
    pub ksigtramp_start: u64,
    pub ksigtramp_end: u64,
    pub ksigtramp_spare: [u64; 4],
}

/// `struct kinfo_vm_layout` (`kern.proc.vm_layout`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KinfoVmLayout {
    pub kvm_min_user_addr: u64,
    pub kvm_max_user_addr: u64,
    pub kvm_text_addr: u64,
    pub kvm_text_size: u64,
    pub kvm_data_addr: u64,
    pub kvm_data_size: u64,
    pub kvm_stack_addr: u64,
    pub kvm_stack_size: u64,
    pub kvm_map_flags: i32,
    pub _pad: i32,
    pub kvm_shp_addr: u64,
    pub kvm_shp_size: u64,
    pub kvm_spare: [u64; 12],
}

/// `kvm_map_flags`.
pub const KMAP_FLAG_WIREFUTURE: i32 = 0x01;
pub const KMAP_FLAG_ASLR: i32 = 0x02;
pub const KMAP_FLAG_ASLR_IGNSTART: i32 = 0x04;
pub const KMAP_FLAG_WXORX: i32 = 0x08;
pub const KMAP_FLAG_ASLR_STACK: i32 = 0x10;
pub const KMAP_FLAG_ASLR_SHARED_PAGE: i32 = 0x20;

/// `struct kinfo_knote` (`kern.proc.kqueue`), with the vnode part of its
/// union (the larger; a pipe's `knt_pipe_ino` is its first word).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KinfoKnote {
    pub knt_kq_fd: i32,
    pub _pad0: i32,
    pub knt_event: crate::event::Kevent,
    pub knt_status: i32,
    pub knt_extdata: i32,
    pub knt_spare0: [u64; 4],
    pub knt_vnode_type: i32,
    pub _pad1: i32,
    pub knt_vnode_fsid: u64,
    pub knt_vnode_fileid: u64,
    pub knt_vnode_fullpath: [u8; PATH_MAX],
}

impl Default for KinfoKnote {
    fn default() -> Self {
        Self {
            knt_kq_fd: 0,
            _pad0: 0,
            knt_event: crate::event::Kevent::default(),
            knt_status: 0,
            knt_extdata: KNOTE_EXTDATA_NONE,
            knt_spare0: [0; 4],
            knt_vnode_type: 0,
            _pad1: 0,
            knt_vnode_fsid: 0,
            knt_vnode_fileid: 0,
            knt_vnode_fullpath: [0; PATH_MAX],
        }
    }
}

/// `knt_status`.
pub const KNOTE_STATUS_ACTIVE: i32 = 0x0000_0001;
pub const KNOTE_STATUS_QUEUED: i32 = 0x0000_0002;
pub const KNOTE_STATUS_DISABLED: i32 = 0x0000_0004;
pub const KNOTE_STATUS_DETACHED: i32 = 0x0000_0008;
pub const KNOTE_STATUS_KQUEUE: i32 = 0x0000_0010;

/// `knt_extdata`.
pub const KNOTE_EXTDATA_NONE: i32 = 0;
pub const KNOTE_EXTDATA_VNODE: i32 = 1;
pub const KNOTE_EXTDATA_PIPE: i32 = 2;

/// `struct kinfo_lockf` (`kern.lockf`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KinfoLockf {
    pub kl_structsize: i32,
    pub kl_rw: i32,
    pub kl_type: i32,
    pub kl_pid: i32,
    pub kl_sysid: i32,
    pub kl_pad0: i32,
    pub kl_file_fsid: u64,
    pub kl_file_rdev: u64,
    pub kl_file_fileid: u64,
    pub kl_start: i64,
    pub kl_len: i64,
    pub kl_path: [u8; PATH_MAX],
}

impl Default for KinfoLockf {
    fn default() -> Self {
        Self {
            kl_structsize: i32::try_from(size_of::<Self>()).unwrap_or(0),
            kl_rw: 0,
            kl_type: 0,
            kl_pid: 0,
            kl_sysid: 0,
            kl_pad0: 0,
            kl_file_fsid: 0,
            kl_file_rdev: 0,
            kl_file_fileid: 0,
            kl_start: 0,
            kl_len: 0,
            kl_path: [0; PATH_MAX],
        }
    }
}

/// `kl_rw` and `kl_type`.
pub const KLOCKF_RW_READ: i32 = 0x01;
pub const KLOCKF_RW_WRITE: i32 = 0x02;
pub const KLOCKF_TYPE_FLOCK: i32 = 0x01;
pub const KLOCKF_TYPE_PID: i32 = 0x02;
pub const KLOCKF_TYPE_REMOTE: i32 = 0x03;

/// `struct loadavg` (`vm.loadavg`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Loadavg {
    pub ldavg: [u32; 3],
    pub _pad: u32,
    pub fscale: i64,
}

/// `struct vmtotal` (`vm.vmtotal`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vmtotal {
    pub t_vm: u64,
    pub t_avm: u64,
    pub t_rm: u64,
    pub t_arm: u64,
    pub t_vmshr: u64,
    pub t_avmshr: u64,
    pub t_rmshr: u64,
    pub t_armshr: u64,
    pub t_free: u64,
    pub t_rq: i16,
    pub t_dw: i16,
    pub t_pw: i16,
    pub t_sl: i16,
    pub t_sw: i16,
    pub t_pad: [u16; 3],
}

/// `struct clockinfo` (`kern.clockrate`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clockinfo {
    pub hz: i32,
    pub tick: i32,
    pub spare: i32,
    pub stathz: i32,
    pub profhz: i32,
}

/// `TDF_*`: `ki_tdflags` bits `ps` reads.
pub const TDF_INMEM: i64 = 0x0000_0004;
pub const TDF_SINTR: i64 = 0x0000_0008;
pub const TDF_IDLETD: i64 = 0x0000_0020;

/// `KI_LOCKBLOCK`.
pub const KI_LOCKBLOCK: i64 = 0x4;
/// `PS_INMEM` (`ki_sflag`).
pub const PS_INMEM: i64 = 0x1;
/// `P_SYSTEM`, `P_TRACED`, `P_JAILED`, `P_ADVLOCK`, `P_KPROC` (`ki_flag`).
pub const P_ADVLOCK: i64 = 0x0000_0001;
pub const P_KPROC: i64 = 0x0000_0004;
pub const P_SYSTEM: i64 = 0x0000_0200;
pub const P_TRACED: i64 = 0x0000_0800;
pub const P_JAILED: i64 = 0x0100_0000;
pub const P_STOPPED_SIG: i64 = 0x0002_0000;
pub const P_STOPPED_TRACE: i64 = 0x0004_0000;

/// `struct priority`'s classes (`PRI_*`).
pub const PRI_ITHD: u8 = 1;
pub const PRI_REALTIME: u8 = 2;
pub const PRI_TIMESHARE: u8 = 3;
pub const PRI_IDLE: u8 = 4;
/// FreeBSD 15's priority ranges (`sys/priority.h`).
pub const PRI_MIN_REALTIME: u8 = 8;
pub const PRI_MIN_KERN: u8 = 40;
pub const PRI_MIN_TIMESHARE: u8 = 56;
pub const PRI_MAX_TIMESHARE: u8 = 223;
pub const PRI_MIN_IDLE: u8 = 224;
pub const PRI_MAX_IDLE: u8 = 255;
pub const PZERO: u8 = 45;
pub const PUSER: u8 = 56;

/// `kern.proc` selectors beyond the process lists.
pub mod node {
    pub const SV_NAME: i32 = 9;
    pub const OVMMAP: i32 = 13;
    pub const OFILEDESC: i32 = 14;
    pub const KSTACK: i32 = 15;
    pub const VMMAP: i32 = 32;
    pub const FILEDESC: i32 = 33;
    pub const GROUPS: i32 = 34;
    pub const ENV: i32 = 35;
    pub const RLIMIT: i32 = 37;
    pub const PS_STRINGS: i32 = 38;
    pub const UMASK: i32 = 39;
    pub const OSREL: i32 = 40;
    pub const SIGTRAMP: i32 = 41;
    pub const CWD: i32 = 42;
    pub const NFDS: i32 = 43;
    pub const SIGFASTBLK: i32 = 44;
    pub const VM_LAYOUT: i32 = 45;
    pub const RLIMIT_USAGE: i32 = 46;
    pub const KQUEUE: i32 = 47;
}

/// `kern.lockf` (`KERN_LOCKF`).
pub const KERN_LOCKF: i32 = 39;

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
