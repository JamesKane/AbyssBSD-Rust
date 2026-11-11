// SPDX-License-Identifier: BSD-2-Clause
//! The NTP kernel interface: `ntp_adjtime`'s `struct timex`,
//! `ntp_gettime`'s `struct ntptimeval`, and their mode, status, and state
//! values.
//!
//! Behavior reference: sys/sys/timex.h.

use crate::layout::ConstValue;
use crate::time::Timespec;

/// `NTP_API`: the interface version.
pub const NTP_API: i32 = 4;

/// `MAXPHASE`: the largest phase error the loop takes, in nanoseconds.
pub const MAXPHASE: i64 = 500_000_000;
/// `MAXFREQ`: the largest frequency error, in nanoseconds per second.
pub const MAXFREQ: i64 = 500_000;
/// `MINSEC`: the shortest update interval for the FLL, in seconds.
pub const MINSEC: i64 = 256;
/// `MAXSEC`: the longest update interval for the PLL, in seconds.
pub const MAXSEC: i64 = 2048;
/// `NANOSECOND`.
pub const NANOSECOND: i64 = 1_000_000_000;
/// `SCALE_PPM`: nanoseconds per second to scaled parts per million,
/// crudely (65536 / 1000).
pub const SCALE_PPM: i64 = 65536 / 1000;
/// `MAXTC`: the largest time constant.
pub const MAXTC: i64 = 10;

/// `timex.modes` bits.
pub mod modes {
    pub const MOD_OFFSET: u32 = 0x0001;
    pub const MOD_FREQUENCY: u32 = 0x0002;
    pub const MOD_MAXERROR: u32 = 0x0004;
    pub const MOD_ESTERROR: u32 = 0x0008;
    pub const MOD_STATUS: u32 = 0x0010;
    pub const MOD_TIMECONST: u32 = 0x0020;
    pub const MOD_PPSMAX: u32 = 0x0040;
    pub const MOD_TAI: u32 = 0x0080;
    pub const MOD_MICRO: u32 = 0x1000;
    pub const MOD_NANO: u32 = 0x2000;
    pub const MOD_CLKB: u32 = 0x4000;
    pub const MOD_CLKA: u32 = 0x8000;
}

/// `timex.status` bits.
pub mod status {
    pub const STA_PLL: i32 = 0x0001;
    pub const STA_PPSFREQ: i32 = 0x0002;
    pub const STA_PPSTIME: i32 = 0x0004;
    pub const STA_FLL: i32 = 0x0008;
    pub const STA_INS: i32 = 0x0010;
    pub const STA_DEL: i32 = 0x0020;
    pub const STA_UNSYNC: i32 = 0x0040;
    pub const STA_FREQHOLD: i32 = 0x0080;
    pub const STA_PPSSIGNAL: i32 = 0x0100;
    pub const STA_PPSJITTER: i32 = 0x0200;
    pub const STA_PPSWANDER: i32 = 0x0400;
    pub const STA_PPSERROR: i32 = 0x0800;
    pub const STA_CLOCKERR: i32 = 0x1000;
    pub const STA_NANO: i32 = 0x2000;
    pub const STA_MODE: i32 = 0x4000;
    pub const STA_CLK: i32 = 0x8000;
    /// The bits only the kernel sets (`STA_RONLY`).
    pub const STA_RONLY: i32 = STA_PPSSIGNAL
        | STA_PPSJITTER
        | STA_PPSWANDER
        | STA_PPSERROR
        | STA_CLOCKERR
        | STA_NANO
        | STA_MODE
        | STA_CLK;
}

/// The clock states `ntp_gettime` and `ntp_adjtime` return.
pub mod state {
    pub const TIME_OK: i32 = 0;
    pub const TIME_INS: i32 = 1;
    pub const TIME_DEL: i32 = 2;
    pub const TIME_OOP: i32 = 3;
    pub const TIME_WAIT: i32 = 4;
    pub const TIME_ERROR: i32 = 5;
}

/// `struct ntptimeval`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ntptimeval {
    /// The time of day (nanoseconds).
    pub time: Timespec,
    /// The maximum error, in microseconds.
    pub maxerror: i64,
    /// The estimated error, in microseconds.
    pub esterror: i64,
    /// The TAI offset, in seconds.
    pub tai: i64,
    /// The clock state (`TIME_*`).
    pub time_state: i32,
    /// Tail padding, explicit.
    pub _pad: i32,
}

/// `struct timex`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timex {
    /// `MOD_*` bits (write only).
    pub modes: u32,
    pub _pad0: u32,
    /// The time offset (nanoseconds with `STA_NANO`, else microseconds).
    pub offset: i64,
    /// The frequency offset, in parts per million scaled by 2^16.
    pub freq: i64,
    /// The maximum error, in microseconds.
    pub maxerror: i64,
    /// The estimated error, in microseconds.
    pub esterror: i64,
    /// `STA_*` bits.
    pub status: i32,
    pub _pad1: i32,
    /// The poll interval (log2 seconds), or the TAI offset with `MOD_TAI`.
    pub constant: i64,
    /// The clock precision (nanoseconds or microseconds; read only).
    pub precision: i64,
    /// The frequency tolerance, in scaled parts per million (read only).
    pub tolerance: i64,
    /// The PPS frequency, in scaled parts per million (read only).
    pub ppsfreq: i64,
    /// The PPS jitter (nanoseconds or microseconds; read only).
    pub jitter: i64,
    /// The PPS averaging interval (log2 seconds; read only).
    pub shift: i32,
    pub _pad2: i32,
    /// The PPS stability, in scaled parts per million (read only).
    pub stabil: i64,
    /// Jitter limit exceeded (read only).
    pub jitcnt: i64,
    /// Calibration intervals (read only).
    pub calcnt: i64,
    /// Calibration errors (read only).
    pub errcnt: i64,
    /// Stability limit exceeded (read only).
    pub stbcnt: i64,
}

const _: () = assert!(size_of::<Ntptimeval>() == 48);
const _: () = assert!(size_of::<Timex>() == 136);

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/timex.h",
        value,
        arches: crate::layout::Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("NTP_API", NTP_API as i64),
    c("MAXPHASE", MAXPHASE),
    c("MAXFREQ", MAXFREQ),
    c("MINSEC", MINSEC),
    c("MAXSEC", MAXSEC),
    c("NANOSECOND", NANOSECOND),
    c("SCALE_PPM", SCALE_PPM),
    c("MAXTC", MAXTC),
    c("MOD_OFFSET", modes::MOD_OFFSET as i64),
    c("MOD_FREQUENCY", modes::MOD_FREQUENCY as i64),
    c("MOD_MAXERROR", modes::MOD_MAXERROR as i64),
    c("MOD_ESTERROR", modes::MOD_ESTERROR as i64),
    c("MOD_STATUS", modes::MOD_STATUS as i64),
    c("MOD_TIMECONST", modes::MOD_TIMECONST as i64),
    c("MOD_PPSMAX", modes::MOD_PPSMAX as i64),
    c("MOD_TAI", modes::MOD_TAI as i64),
    c("MOD_MICRO", modes::MOD_MICRO as i64),
    c("MOD_NANO", modes::MOD_NANO as i64),
    c("MOD_CLKB", modes::MOD_CLKB as i64),
    c("MOD_CLKA", modes::MOD_CLKA as i64),
    c("STA_PLL", status::STA_PLL as i64),
    c("STA_PPSFREQ", status::STA_PPSFREQ as i64),
    c("STA_PPSTIME", status::STA_PPSTIME as i64),
    c("STA_FLL", status::STA_FLL as i64),
    c("STA_INS", status::STA_INS as i64),
    c("STA_DEL", status::STA_DEL as i64),
    c("STA_UNSYNC", status::STA_UNSYNC as i64),
    c("STA_FREQHOLD", status::STA_FREQHOLD as i64),
    c("STA_PPSSIGNAL", status::STA_PPSSIGNAL as i64),
    c("STA_PPSJITTER", status::STA_PPSJITTER as i64),
    c("STA_PPSWANDER", status::STA_PPSWANDER as i64),
    c("STA_PPSERROR", status::STA_PPSERROR as i64),
    c("STA_CLOCKERR", status::STA_CLOCKERR as i64),
    c("STA_NANO", status::STA_NANO as i64),
    c("STA_MODE", status::STA_MODE as i64),
    c("STA_CLK", status::STA_CLK as i64),
    c("STA_RONLY", status::STA_RONLY as i64),
    c("TIME_OK", state::TIME_OK as i64),
    c("TIME_INS", state::TIME_INS as i64),
    c("TIME_DEL", state::TIME_DEL as i64),
    c("TIME_OOP", state::TIME_OOP as i64),
    c("TIME_WAIT", state::TIME_WAIT as i64),
    c("TIME_ERROR", state::TIME_ERROR as i64),
];
