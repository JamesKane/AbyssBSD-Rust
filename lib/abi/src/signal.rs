// SPDX-License-Identifier: BSD-2-Clause
//! Signals. Behavior reference: sys/sys/signal.h, sys/sys/_sigset.h.

use crate::layout::{Arches, ConstValue};

/// The highest signal number (`_SIG_MAXSIG`).
pub const SIG_MAXSIG: u32 = 128;
pub const SIGKILL: u32 = 9;
pub const SIGSTOP: u32 = 17;

/// `sigprocmask` operations.
pub const SIG_BLOCK: u32 = 1;
pub const SIG_UNBLOCK: u32 = 2;
pub const SIG_SETMASK: u32 = 3;

/// Handlers that are not addresses.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("_SIG_MAXSIG", SIG_MAXSIG),
    c("SIGKILL", SIGKILL),
    c("SIGSTOP", SIGSTOP),
    c("SIG_BLOCK", SIG_BLOCK),
    c("SIG_UNBLOCK", SIG_UNBLOCK),
    c("SIG_SETMASK", SIG_SETMASK),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "signal.h",
        value: value as i64,
        arches: Arches::All,
    }
}

/// Words in a `sigset_t` (`_SIG_WORDS`).
pub const SIG_WORDS: usize = 4;

/// `sigset_t`: one bit per signal, 128 signals.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sigset {
    pub bits: [u32; SIG_WORDS],
}

/// `struct sigaction`.
///
/// The handler is a union of two function pointers in C; here it is the raw
/// user-space address, which also encodes `SIG_DFL` (0) and `SIG_IGN` (1).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sigaction {
    pub sa_handler: u64,
    pub sa_flags: i32,
    pub sa_mask: Sigset,
    /// The C struct's tail padding, explicit so copies never carry stray
    /// kernel bytes.
    pub _pad: u32,
}

const _: () = assert!(size_of::<Sigaction>() == 32);
