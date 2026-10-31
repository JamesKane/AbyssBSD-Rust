// SPDX-License-Identifier: BSD-2-Clause
//! Interfaces and the control plane (docs/NETWORKING.md §5, §10, §11):
//! VNETs, the interfaces in each with their addresses, the routes, and
//! the routing-socket and netlink messages that describe them.
//!
//! Every piece of network state lives in a [`Vnet`]; code reaches it
//! through the VNET of the socket, thread, or interface at hand, never a
//! global. The system's own is [`vnet0`]; a VNET jail has its own.
//!
//! Behavior reference: sys/net/vnet.c, sys/net/if.c, sys/net/if_loop.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod addr;
pub mod announce;
pub mod arp;
pub mod bpf;
pub mod clone;
pub mod ether;
pub mod ifnet;
pub mod link;
#[cfg(test)]
mod link_tests;
pub mod lltable;
pub mod netif;
pub mod netisr;
pub mod netlink;
pub mod rtmsg;
pub mod rtsock;
#[cfg(test)]
mod tests;
mod vnet;

pub use vnet::{Vnet, vnet0};

use core::sync::atomic::{AtomicU32, Ordering};

use abi::inet::{AF_INET, AF_INET6};

/// `net.fibs` (`rt_numfibs`): the routing tables every VNET has, set once
/// at boot.
static NUMFIBS: AtomicU32 = AtomicU32::new(1);

/// `RT_MAXFIBS`.
pub const MAXFIBS: u32 = 65536;

/// The number of routing tables.
#[must_use]
pub fn numfibs() -> u32 {
    NUMFIBS.load(Ordering::Relaxed)
}

/// Sets `net.fibs` from the boot tunable (1 to `RT_MAXFIBS`).
pub fn set_numfibs(n: u32) {
    NUMFIBS.store(n.clamp(1, MAXFIBS), Ordering::Relaxed);
}

/// The uptime clock in seconds, for entries that age.
static UPTIME: sync::Once<fn() -> u64> = sync::Once::new();

/// Sets the uptime clock.
pub fn set_uptime(f: fn() -> u64) {
    let _ = UPTIME.set(f);
}

/// Seconds since boot (`time_uptime`).
pub(crate) fn uptime() -> u64 {
    UPTIME.get().map_or(0, |f| f())
}

/// The time of day in seconds, for interfaces' attach times.
static REALTIME: sync::Once<fn() -> i64> = sync::Once::new();

/// Sets the time-of-day clock.
pub fn set_realtime(f: fn() -> i64) {
    let _ = REALTIME.set(f);
}

/// Seconds since the epoch (`time_second`).
pub(crate) fn realtime() -> i64 {
    REALTIME.get().map_or(0, |f| f())
}

/// Registers the link layer's protocols with `netisr`: Ethernet input,
/// and ARP behind it.
pub fn init() {
    netisr::register(netisr::Proto::Ether, ether::input);
    netisr::register(netisr::Proto::Arp, arp::input);
}

/// An address family the network layer carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    Inet,
    Inet6,
}

impl Family {
    /// Its `AF_*`.
    #[must_use]
    pub const fn af(self) -> u8 {
        match self {
            Self::Inet => AF_INET,
            Self::Inet6 => AF_INET6,
        }
    }

    /// The family of `AF_*` `af`.
    #[must_use]
    pub const fn from_af(af: u8) -> Option<Self> {
        match af {
            AF_INET => Some(Self::Inet),
            AF_INET6 => Some(Self::Inet6),
            _ => None,
        }
    }
}
