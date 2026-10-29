// SPDX-License-Identifier: BSD-2-Clause
//! The loopback network (docs/MULTIUSER.md §8.4): `lo0` attached at boot
//! with the addresses `rc.d/netif` would give it, the [`inet`] crate's
//! protocols wired to the kernel's clock and random source, the interface
//! ioctls, netlink sockets, and the `net` sysctls.
//!
//! Behavior reference: sys/net/if_loop.c (`vnet_loif_init`),
//! sys/netinet6/in6_ifattach.c.

pub(crate) mod ioctl;
pub(crate) mod netlink;
mod sysctl;

use sched::Sleepable;

// SAFETY: `if_data` is integers with no padding.
unsafe impl crate::uaccess::Plain for abi::net::IfData {}
// SAFETY: `nd_ifinfo` is integers and byte arrays, its tail padding an
// explicit field.
unsafe impl crate::uaccess::Plain for abi::net::NdIfinfo {}
// SAFETY: `in6_addrpolicy` is two `sockaddr_in6`s (integers and byte
// arrays) and integers, with no padding between or after them.
unsafe impl crate::uaccess::Plain for abi::net::In6Addrpolicy {}

fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    // Unseeded early on: a port still comes, from the clock.
    if crate::random::fill(&mut b).is_err() {
        #[allow(clippy::cast_possible_truncation, reason = "the low bits")]
        return crate::time::now_ns() as u32;
    }
    u32::from_ne_bytes(b)
}

/// Attaches `lo0` and adds the sysctls. Call once, after the sysctl tree
/// and `kern.features` exist.
pub(crate) fn init(s: &Sleepable) {
    inet::set_hooks(inet::Hooks {
        now: crate::time::now_ns,
        random: random_u32,
    });
    let now = i64::try_from(crate::time::realtime_ns().unwrap_or(0) / 1_000_000_000).unwrap_or(0);
    inet::ifnet::attach_loopback(now);
    sysctl::init(s);
}
