// SPDX-License-Identifier: BSD-2-Clause
//! Threads and their scheduling: `thr_new`'s parameters (`sys/thr.h`),
//! `rtprio` (`sys/rtprio.h`), cpu sets (`sys/cpuset.h`), `sigfastblock`
//! (`sys/signalvar.h`), and `exterrctl` (`sys/exterrvar.h`).

use crate::layout::{Arches, ConstValue};

/// `struct thr_param`: a new thread's entry, stack, TLS, and where its
/// tid goes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThrParam {
    pub start_func: u64,
    pub arg: u64,
    pub stack_base: u64,
    pub stack_size: u64,
    pub tls_base: u64,
    pub tls_size: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub flags: i32,
    pub _pad: u32,
    pub rtp: u64,
    pub spare: [u64; 3],
}

/// `thr_param.flags`.
pub const THR_SUSPENDED: i32 = 0x1;
pub const THR_SYSTEM_SCOPE: i32 = 0x2;
pub const THR_C_RUNTIME: i32 = 0x4;

pub const RTP_LOOKUP: u32 = 0;
pub const RTP_SET: u32 = 1;
/// `RTP_PRIO_REALTIME` (`PRI_REALTIME`).
pub const RTP_PRIO_REALTIME: u16 = 2;
/// `RTP_PRIO_NORMAL` (`PRI_TIMESHARE`).
pub const RTP_PRIO_NORMAL: u16 = 3;
/// `RTP_PRIO_IDLE` (`PRI_IDLE`).
pub const RTP_PRIO_IDLE: u16 = 4;
/// `RTP_PRIO_FIFO_BIT` (`PRI_FIFO_BIT`): the class's scheduling is first
/// in, first out.
pub const RTP_PRIO_FIFO_BIT: u16 = 8;
/// `RTP_PRIO_FIFO` (`PRI_FIFO`: real time, first in first out).
pub const RTP_PRIO_FIFO: u16 = 10;
/// The lowest real-time or idle priority (`RTP_PRIO_MAX`).
pub const RTP_PRIO_MAX: u16 = 31;

/// `struct rtprio`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rtprio {
    pub type_: u16,
    pub prio: u16,
}

pub const CPU_LEVEL_ROOT: u32 = 1;
pub const CPU_LEVEL_CPUSET: u32 = 2;
pub const CPU_LEVEL_WHICH: u32 = 3;
pub const CPU_WHICH_TID: u32 = 1;
pub const CPU_WHICH_PID: u32 = 2;
pub const CPU_WHICH_TIDPID: u32 = 9;
/// Bits in a `cpuset_t` (`CPU_SETSIZE`).
pub const CPU_SETSIZE: u32 = 1024;
/// Bytes in a `cpuset_t`.
pub const CPUSET_BYTES: usize = CPU_SETSIZE as usize / 8;

pub const SIGFASTBLOCK_SETPTR: u32 = 1;
pub const SIGFASTBLOCK_UNBLOCK: u32 = 2;
pub const SIGFASTBLOCK_UNSETPTR: u32 = 3;
/// The word's low bits: a signal waits for `SIGFASTBLOCK_UNBLOCK`.
pub const SIGFASTBLOCK_PEND: u32 = 0x1;
pub const SIGFASTBLOCK_FLAGS: u32 = 0xf;
/// What libthr adds to the word per nested block.
pub const SIGFASTBLOCK_INC: u32 = 0x10;

pub const EXTERRCTL_ENABLE: u32 = 1;
pub const EXTERRCTL_DISABLE: u32 = 2;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("RTP_LOOKUP", RTP_LOOKUP, "sys/rtprio.h"),
    c("RTP_SET", RTP_SET, "sys/rtprio.h"),
    c(
        "RTP_PRIO_REALTIME",
        RTP_PRIO_REALTIME as u32,
        "sys/rtprio.h",
    ),
    c("RTP_PRIO_NORMAL", RTP_PRIO_NORMAL as u32, "sys/rtprio.h"),
    c("RTP_PRIO_IDLE", RTP_PRIO_IDLE as u32, "sys/rtprio.h"),
    c(
        "RTP_PRIO_FIFO_BIT",
        RTP_PRIO_FIFO_BIT as u32,
        "sys/rtprio.h",
    ),
    c("RTP_PRIO_FIFO", RTP_PRIO_FIFO as u32, "sys/rtprio.h"),
    c("RTP_PRIO_MAX", RTP_PRIO_MAX as u32, "sys/rtprio.h"),
    c("THR_SUSPENDED", THR_SUSPENDED as u32, "sys/thr.h"),
    c("THR_SYSTEM_SCOPE", THR_SYSTEM_SCOPE as u32, "sys/thr.h"),
    c("THR_C_RUNTIME", THR_C_RUNTIME as u32, "sys/thr.h"),
    c("CPU_LEVEL_ROOT", CPU_LEVEL_ROOT, "sys/cpuset.h"),
    c("CPU_LEVEL_CPUSET", CPU_LEVEL_CPUSET, "sys/cpuset.h"),
    c("CPU_LEVEL_WHICH", CPU_LEVEL_WHICH, "sys/cpuset.h"),
    c("CPU_WHICH_TID", CPU_WHICH_TID, "sys/cpuset.h"),
    c("CPU_WHICH_PID", CPU_WHICH_PID, "sys/cpuset.h"),
    c("CPU_WHICH_TIDPID", CPU_WHICH_TIDPID, "sys/cpuset.h"),
    c("CPU_SETSIZE", CPU_SETSIZE, "sys/cpuset.h"),
    c(
        "SIGFASTBLOCK_SETPTR",
        SIGFASTBLOCK_SETPTR,
        "sys/signalvar.h",
    ),
    c(
        "SIGFASTBLOCK_UNBLOCK",
        SIGFASTBLOCK_UNBLOCK,
        "sys/signalvar.h",
    ),
    c(
        "SIGFASTBLOCK_UNSETPTR",
        SIGFASTBLOCK_UNSETPTR,
        "sys/signalvar.h",
    ),
    c("SIGFASTBLOCK_PEND", SIGFASTBLOCK_PEND, "sys/signalvar.h"),
    c("SIGFASTBLOCK_FLAGS", SIGFASTBLOCK_FLAGS, "sys/signalvar.h"),
    c("SIGFASTBLOCK_INC", SIGFASTBLOCK_INC, "sys/signalvar.h"),
    c("EXTERRCTL_ENABLE", EXTERRCTL_ENABLE, "sys/exterrvar.h"),
    c("EXTERRCTL_DISABLE", EXTERRCTL_DISABLE, "sys/exterrvar.h"),
];

const fn c(c_name: &'static str, value: u32, header: &'static str) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value: value as i64,
        arches: Arches::All,
    }
}
