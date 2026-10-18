// SPDX-License-Identifier: BSD-2-Clause
//! Signals. Behavior reference: sys/sys/signal.h, sys/sys/_sigset.h.

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
}
