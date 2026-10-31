// SPDX-License-Identifier: BSD-2-Clause
//! Ethernet (docs/NETWORKING.md §7): frames in from a device, through the
//! tap and the destination filter to their protocol; frames out with
//! their header.
//!
//! Behavior reference: sys/net/if_ethersubr.c (`ether_input_internal`,
//! `ether_demux`, `ether_output_frame`, `ether_requestencap`).

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::net::iff;
use mbuf::{Packet, flags};

use crate::Vnet;
use crate::bpf::Way;
use crate::link::Link;
use crate::netisr::{self, Proto};

/// `ETHER_ADDR_LEN`, `ETHER_HDR_LEN`, `ETHER_VLAN_ENCAP_LEN`.
pub const ADDR_LEN: usize = 6;
pub const HDR_LEN: usize = 14;
pub const VLAN_ENCAP_LEN: usize = 4;
/// `etherbroadcastaddr`.
pub const BROADCAST: [u8; ADDR_LEN] = [0xff; ADDR_LEN];

/// EtherTypes (`ETHERTYPE_*`).
pub mod ty {
    pub const IP: u16 = 0x0800;
    pub const ARP: u16 = 0x0806;
    pub const REVARP: u16 = 0x8035;
    pub const VLAN: u16 = 0x8100;
    pub const IPV6: u16 = 0x86dd;
    pub const QINQ: u16 = 0x88a8;
}

/// `ETHER_IS_MULTICAST`.
#[must_use]
pub const fn is_multicast(a: &[u8; ADDR_LEN]) -> bool {
    a[0] & 1 != 0
}

/// A frame's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub dst: [u8; ADDR_LEN],
    pub src: [u8; ADDR_LEN],
    pub ty: u16,
}

impl Header {
    /// Its bytes.
    #[must_use]
    pub fn bytes(&self) -> [u8; HDR_LEN] {
        let mut b = [0u8; HDR_LEN];
        b[..6].copy_from_slice(&self.dst);
        b[6..12].copy_from_slice(&self.src);
        b[12..].copy_from_slice(&self.ty.to_be_bytes());
        b
    }
}

impl mbuf::Header for Header {
    const LEN: usize = HDR_LEN;

    fn parse(b: &[u8]) -> Option<Self> {
        Some(Self {
            dst: b.get(..6)?.try_into().ok()?,
            src: b.get(6..12)?.try_into().ok()?,
            ty: u16::from_be_bytes([*b.get(12)?, *b.get(13)?]),
        })
    }
}

/// A received frame (`ether_input_internal`, by way of `netisr`'s
/// `NETISR_ETHER`): its interface is `m.hdr.rcvif`.
pub fn input(vnet: &Arc<Vnet>, mut m: Packet) {
    let index = m.hdr.rcvif;
    let (ifflags, lladdr, tap, fib, stats) = {
        let n = vnet.net.lock();
        let Some(i) = n.by_index(index) else {
            return;
        };
        (
            i.flags,
            i.lladdr,
            i.bpf.clone(),
            i.fib,
            Arc::clone(&i.stats),
        )
    };
    if ifflags & iff::UP == 0 {
        return;
    }
    let Ok(mut eh) = m.header::<Header>(0) else {
        stats.ierrors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if is_multicast(&eh.dst) {
        m.hdr.flags |= if eh.dst == BROADCAST {
            flags::BCAST
        } else {
            flags::MCAST
        };
        stats.imcasts.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(t) = &tap {
        t.mtap(&m, Way::In);
    }
    // An 802.1Q header the device did not take off, as the device would
    // have: its tag goes in the packet header.
    if m.hdr.flags & flags::VLANTAG == 0 && (eh.ty == ty::VLAN || eh.ty == ty::QINQ) {
        let mut v = [0u8; HDR_LEN + VLAN_ENCAP_LEN];
        if m.data.copy_out(0, &mut v) != v.len() {
            stats.ierrors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        m.hdr.vlan_tag = u16::from_be_bytes([v[14], v[15]]);
        m.hdr.flags |= flags::VLANTAG;
        eh.ty = u16::from_be_bytes([v[16], v[17]]);
        // The addresses move up over the tag.
        let mut hdr = [0u8; HDR_LEN];
        hdr[..12].copy_from_slice(&v[..12]);
        hdr[12..].copy_from_slice(&eh.ty.to_be_bytes());
        m.data.trim_front(HDR_LEN + VLAN_ENCAP_LEN);
        match m.prepend(HDR_LEN) {
            Ok(b) => b.copy_from_slice(&hdr),
            Err(_) => return,
        }
    }
    m.hdr.fib = fib;
    // Not for this interface's address: only for the tap and whoever asked
    // for promiscuous frames.
    if !is_multicast(&eh.dst) && lladdr.is_some_and(|l| l != eh.dst) {
        m.hdr.flags |= flags::PROMISC;
    }
    demux(vnet, ifflags, &stats, eh.ty, m);
}

/// `ether_demux`: to the protocol for its type, the header off.
fn demux(vnet: &Arc<Vnet>, ifflags: u32, stats: &crate::ifnet::Stats, ety: u16, mut m: Packet) {
    // A VLAN other than 0 with no trunk here.
    if m.hdr.flags & flags::VLANTAG != 0 && m.hdr.vlan_tag & 0xfff != 0 {
        stats.noproto.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if ifflags & iff::PPROMISC == 0 && m.hdr.flags & flags::PROMISC != 0 {
        return;
    }
    m.hdr.flags &= !(flags::VLANTAG | flags::PROMISC);
    let proto = match ety {
        ty::IP => Proto::Ip,
        ty::ARP if ifflags & iff::NOARP == 0 => Proto::Arp,
        ty::IPV6 => Proto::Ipv6,
        _ => return,
    };
    m.data.trim_front(HDR_LEN);
    netisr::dispatch(vnet, proto, m);
}

/// Sends frame `m`, its header on, out of interface `index` through
/// `link` (`ether_output_frame` and `if_transmit`).
///
/// # Errors
///
/// `ENETDOWN` if it is not up and running, `ENOBUFS` if the device
/// cannot take it.
pub fn output_frame(
    vnet: &Arc<Vnet>,
    index: u16,
    link: &Arc<dyn Link>,
    ifflags: u32,
    m: Packet,
) -> Result<(), Errno> {
    if ifflags & (iff::UP | iff::RUNNING) != iff::UP | iff::RUNNING {
        return Err(Errno::ENETDOWN);
    }
    link.transmit(vnet, index, None, m).map_err(Errno::from)
}

/// Puts header `h` before `m`'s data (`M_PREPEND` of the link header).
///
/// # Errors
///
/// `ENOBUFS` without buffers.
pub fn encap(m: &mut Packet, h: &Header) -> Result<(), Errno> {
    m.prepend(HDR_LEN)?.copy_from_slice(&h.bytes());
    Ok(())
}
