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
pub mod clone;
pub mod ifnet;
pub mod link;
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
