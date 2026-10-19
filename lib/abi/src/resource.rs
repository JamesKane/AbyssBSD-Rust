// SPDX-License-Identifier: BSD-2-Clause
//! Process resources and waiting: `wait4` options and status
//! (`sys/wait.h`), `struct rusage`, and resource limits
//! (`sys/resource.h`).

use crate::layout::{Arches, ConstValue};
use crate::time::Timeval;

pub const WNOHANG: u32 = 1;
pub const WUNTRACED: u32 = 2;
pub const WCONTINUED: u32 = 4;
pub const WNOWAIT: u32 = 8;
pub const WEXITED: u32 = 16;
pub const WTRAPPED: u32 = 32;

/// A `wait` status for a normal exit with `code` (`W_EXITCODE(code, 0)`:
/// not masked; `WEXITSTATUS` takes the low 8 bits).
#[must_use]
pub const fn exit_status(code: i32) -> i32 {
    code.wrapping_shl(8)
}

/// `struct rusage`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rusage {
    pub ru_utime: Timeval,
    pub ru_stime: Timeval,
    pub ru_maxrss: i64,
    pub ru_ixrss: i64,
    pub ru_idrss: i64,
    pub ru_isrss: i64,
    pub ru_minflt: i64,
    pub ru_majflt: i64,
    pub ru_nswap: i64,
    pub ru_inblock: i64,
    pub ru_oublock: i64,
    pub ru_msgsnd: i64,
    pub ru_msgrcv: i64,
    pub ru_nsignals: i64,
    pub ru_nvcsw: i64,
    pub ru_nivcsw: i64,
}

/// Resource limit numbers.
pub mod rlimit {
    pub const CPU: u32 = 0;
    pub const FSIZE: u32 = 1;
    pub const DATA: u32 = 2;
    pub const STACK: u32 = 3;
    pub const CORE: u32 = 4;
    pub const RSS: u32 = 5;
    pub const MEMLOCK: u32 = 6;
    pub const NPROC: u32 = 7;
    pub const NOFILE: u32 = 8;
    pub const SBSIZE: u32 = 9;
    pub const VMEM: u32 = 10;
    pub const NPTS: u32 = 11;
    pub const SWAP: u32 = 12;
    pub const KQUEUES: u32 = 13;
    pub const UMTXP: u32 = 14;
    pub const PIPEBUF: u32 = 15;
    /// How many there are (`RLIM_NLIMITS`; 15.x has no `RLIMIT_VMM`).
    pub const NLIMITS: usize = 16;
}

/// `RLIM_INFINITY`.
pub const RLIM_INFINITY: u64 = (1 << 63) - 1;

/// `struct rlimit`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("WNOHANG", WNOHANG as i64, "sys/wait.h"),
    c("WUNTRACED", WUNTRACED as i64, "sys/wait.h"),
    c("WCONTINUED", WCONTINUED as i64, "sys/wait.h"),
    c("WNOWAIT", WNOWAIT as i64, "sys/wait.h"),
    c("WEXITED", WEXITED as i64, "sys/wait.h"),
    c("WTRAPPED", WTRAPPED as i64, "sys/wait.h"),
    c("W_EXITCODE(3, 0)", exit_status(3) as i64, "sys/wait.h"),
    c("RLIMIT_CPU", rlimit::CPU as i64, "sys/resource.h"),
    c("RLIMIT_FSIZE", rlimit::FSIZE as i64, "sys/resource.h"),
    c("RLIMIT_DATA", rlimit::DATA as i64, "sys/resource.h"),
    c("RLIMIT_STACK", rlimit::STACK as i64, "sys/resource.h"),
    c("RLIMIT_CORE", rlimit::CORE as i64, "sys/resource.h"),
    c("RLIMIT_RSS", rlimit::RSS as i64, "sys/resource.h"),
    c("RLIMIT_MEMLOCK", rlimit::MEMLOCK as i64, "sys/resource.h"),
    c("RLIMIT_NPROC", rlimit::NPROC as i64, "sys/resource.h"),
    c("RLIMIT_NOFILE", rlimit::NOFILE as i64, "sys/resource.h"),
    c("RLIMIT_SBSIZE", rlimit::SBSIZE as i64, "sys/resource.h"),
    c("RLIMIT_VMEM", rlimit::VMEM as i64, "sys/resource.h"),
    c("RLIMIT_NPTS", rlimit::NPTS as i64, "sys/resource.h"),
    c("RLIMIT_SWAP", rlimit::SWAP as i64, "sys/resource.h"),
    c("RLIMIT_KQUEUES", rlimit::KQUEUES as i64, "sys/resource.h"),
    c("RLIMIT_UMTXP", rlimit::UMTXP as i64, "sys/resource.h"),
    c("RLIMIT_PIPEBUF", rlimit::PIPEBUF as i64, "sys/resource.h"),
    c("RLIM_NLIMITS", rlimit::NLIMITS as i64, "sys/resource.h"),
    c("RLIM_INFINITY", RLIM_INFINITY as i64, "sys/resource.h"),
];

const fn c(c_name: &'static str, value: i64, header: &'static str) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}
