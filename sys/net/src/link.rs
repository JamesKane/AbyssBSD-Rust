// SPDX-License-Identifier: BSD-2-Clause
//! The device side of an interface: what sends its packets
//! (`if_transmit`), reports its media, and is destroyed with it. `lo` and
//! `epair` are software ones; drivers come with the net class (P7.3).
//!
//! Behavior reference: sys/net/if_var.h (`if_transmit`, `if_ioctl`),
//! sys/net/if_media.c, sys/net/if_ethersubr.c (`ether_gen_addr_byname`).

use alloc::sync::Arc;
use core::any::Any;

use mbuf::{NoBufs, Packet};

use crate::{Family, Vnet};

/// `IFM_*` words as `SIOCGIFMEDIA` returns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Media {
    pub current: i32,
    pub active: i32,
    pub status: i32,
    /// The one supported word (`ifm_ulist`), these drivers having one.
    pub supported: i32,
}

/// `IFM_ETHER`, `IFM_10G_T`, `IFM_FDX`, `IFM_AVALID`, `IFM_ACTIVE`.
pub mod ifm {
    pub const ETHER: i32 = 0x20;
    pub const T10G: i32 = 26;
    pub const FDX: i32 = 0x0010_0000;
    pub const AVALID: i32 = 0x1;
    pub const ACTIVE: i32 = 0x2;
}

/// An interface's device.
pub trait Link: Send + Sync {
    /// Takes a packet out of interface `index` of `vnet` (`if_output`
    /// and `if_transmit`): a frame for an Ethernet device, or for a
    /// loopback one a packet of `family`.
    ///
    /// # Errors
    ///
    /// `NoBufs` when it cannot take it (a full queue, or an end down);
    /// the packet is dropped and counted.
    fn transmit(
        &self,
        vnet: &Arc<Vnet>,
        index: u16,
        family: Option<Family>,
        m: Packet,
    ) -> Result<(), NoBufs>;

    /// Its media, if it has any (`SIOCGIFMEDIA`).
    fn media(&self) -> Option<Media> {
        None
    }

    fn as_any(&self) -> &dyn Any;
}

/// An interface's device, if it has one.
#[derive(Clone, Default)]
pub struct LinkRef(pub Option<Arc<dyn Link>>);

impl core::fmt::Debug for LinkRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(if self.0.is_some() {
            "LinkRef(..)"
        } else {
            "LinkRef(None)"
        })
    }
}

/// The address `ether_gen_addr_byname` makes for `name`: in FreeBSD's
/// generated range, from the SHA-1 of `uuid-name-jail`; `None` (a random
/// address instead) while the host UUID is the default.
#[must_use]
pub fn gen_addr(uuid: &str, name: &str, jail: &str) -> Option<[u8; 6]> {
    if uuid == "00000000-0000-0000-0000-000000000000" {
        return None;
    }
    let mut h = crypto::sha1::Sha1::new();
    h.update(uuid.as_bytes());
    h.update(b"-");
    h.update(name.as_bytes());
    h.update(b"-");
    h.update(jail.as_bytes());
    let d = h.finish();
    Some([0x58, 0x9c, 0xfc, 0x10, d[0], d[1]])
}

/// A random unicast, locally administered address (`ether_gen_addr`'s
/// fallback).
#[must_use]
pub fn random_addr(mut r: [u8; 6]) -> [u8; 6] {
    r[0] &= 0xfe;
    r[0] |= 0x02;
    r
}
