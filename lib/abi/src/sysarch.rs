// SPDX-License-Identifier: BSD-2-Clause
//! `sysarch(2)` operations (`machine/sysarch.h`).

use crate::layout::{Arches, ConstValue};

/// amd64's operations on the thread's segment bases.
pub mod amd64 {
    pub const GET_FSBASE: u64 = 128;
    pub const SET_FSBASE: u64 = 129;
    pub const GET_GSBASE: u64 = 130;
    pub const SET_GSBASE: u64 = 131;
    /// The base libc's start-up sets: the FS base, also remembered as the
    /// thread's TLS base (restored for signal handlers).
    pub const GET_TLSBASE: u64 = 135;
    pub const SET_TLSBASE: u64 = 136;
    pub const DISABLE_TLSBASE: u64 = 137;
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("AMD64_GET_FSBASE", amd64::GET_FSBASE),
    c("AMD64_SET_FSBASE", amd64::SET_FSBASE),
    c("AMD64_GET_GSBASE", amd64::GET_GSBASE),
    c("AMD64_SET_GSBASE", amd64::SET_GSBASE),
    c("AMD64_GET_TLSBASE", amd64::GET_TLSBASE),
    c("AMD64_SET_TLSBASE", amd64::SET_TLSBASE),
    c("AMD64_DISABLE_TLSBASE", amd64::DISABLE_TLSBASE),
];

#[allow(clippy::cast_possible_wrap)] // small operation numbers
const fn c(c_name: &'static str, value: u64) -> ConstValue {
    ConstValue {
        c_name,
        header: "machine/sysarch.h",
        value: value as i64,
        arches: Arches::Amd64,
    }
}
