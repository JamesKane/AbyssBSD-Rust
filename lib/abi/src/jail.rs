// SPDX-License-Identifier: BSD-2-Clause
//! Jails (`sys/jail.h`): the old `jail(2)`'s structure. `jail_set` and
//! `jail_get` take their parameters as `struct iovec` pairs instead.

/// `JAIL_API_VERSION`: the only `struct jail` `jail(2)` takes.
pub const JAIL_API_VERSION: u32 = 2;

/// `struct jail`, version 2.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Jail {
    pub version: u32,
    pub _pad: u32,
    /// The root, host name, and name (user addresses).
    pub path: u64,
    pub hostname: u64,
    pub jailname: u64,
    pub ip4s: u32,
    pub ip6s: u32,
    /// The addresses, `ip4s` and `ip6s` of them (user addresses).
    pub ip4: u64,
    pub ip6: u64,
}
