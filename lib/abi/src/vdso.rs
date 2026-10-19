// SPDX-License-Identifier: BSD-2-Clause
//! The time keeping the kernel publishes in the shared page, which libc's
//! `clock_gettime` and `gettimeofday` read without a system call:
//! `struct vdso_timekeep` and its `struct vdso_timehands` slots.
//!
//! The common part of a timehands slot is the same on every architecture;
//! the machine-dependent tail differs, so each layout is spelled out (both
//! are defined everywhere so `abi-check` can check each one), and
//! [`Timehands`] names the one for the target.
//!
//! Behavior reference: sys/sys/vdso.h, sys/x86/include/vdso.h,
//! sys/arm64/include/vdso.h, lib/libsys/__vdso_gettimeofday.c.

use crate::layout::{Arches, ConstValue};

/// `struct bintime`: seconds plus a binary fraction of a second
/// (`frac / 2^64`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bintime {
    pub sec: crate::time::TimeT,
    pub frac: u64,
}

/// `struct vdso_timekeep`, without its trailing `tk_th[]` array (which
/// starts at its size).
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timekeep {
    /// [`TK_VER_CURR`].
    pub tk_ver: u32,
    /// Whether readers may use the timehands (otherwise they make the
    /// system call).
    pub tk_enabled: u32,
    /// The index of the current slot in `tk_th`.
    pub tk_current: u32,
}

/// `struct vdso_timehands` on amd64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimehandsX86 {
    /// How to read the counter ([`TH_ALGO_X86_TSC`]).
    pub th_algo: u32,
    /// Changes on every update of the slot; 0 while one is in progress.
    pub th_gen: u32,
    /// Seconds per counter tick, as a fraction of 2^64.
    pub th_scale: u64,
    /// The (shifted, truncated) counter at `th_offset`.
    pub th_offset_count: u32,
    pub th_counter_mask: u32,
    /// The uptime at `th_offset_count`.
    pub th_offset: Bintime,
    /// The time of day at uptime zero.
    pub th_boottime: Bintime,
    /// The TSC is read shifted right this far (`rdtsc; shrd`).
    pub th_x86_shift: u32,
    pub th_x86_hpet_idx: u32,
    pub th_x86_pvc_last_systime: u64,
    pub th_x86_pvc_stable_mask: u8,
    pub th_res: [u8; 15],
}

/// `struct vdso_timehands` on arm64.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimehandsArm {
    /// How to read the counter ([`TH_ALGO_ARM_GENTIM`]).
    pub th_algo: u32,
    pub th_gen: u32,
    pub th_scale: u64,
    pub th_offset_count: u32,
    pub th_counter_mask: u32,
    pub th_offset: Bintime,
    pub th_boottime: Bintime,
    /// Nonzero: read the physical count (`cntpct_el0`), not the virtual.
    pub th_physical: u32,
    pub th_res: [u32; 7],
}

/// The target's `struct vdso_timehands`.
#[cfg(target_arch = "x86_64")]
pub type Timehands = TimehandsX86;
/// The target's `struct vdso_timehands`.
#[cfg(target_arch = "aarch64")]
pub type Timehands = TimehandsArm;

pub const TK_VER_CURR: u32 = 1;
/// The TSC (amd64).
pub const TH_ALGO_X86_TSC: u32 = 1;
/// The generic timer (arm64).
pub const TH_ALGO_ARM_GENTIM: u32 = 1;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("VDSO_TK_VER_CURR", TK_VER_CURR, Arches::All),
    c("VDSO_TH_ALGO_X86_TSC", TH_ALGO_X86_TSC, Arches::Amd64),
    c("VDSO_TH_ALGO_ARM_GENTIM", TH_ALGO_ARM_GENTIM, Arches::Arm64),
];

const fn c(c_name: &'static str, value: u32, arches: Arches) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/vdso.h",
        value: value as i64,
        arches,
    }
}
