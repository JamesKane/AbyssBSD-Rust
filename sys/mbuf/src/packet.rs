// SPDX-License-Identifier: BSD-2-Clause
//! A packet: FreeBSD's packet header (`struct pkthdr`) and its data.
//!
//! Behavior reference: sys/sys/mbuf.h (`struct pkthdr`, `CSUM_*`,
//! `M_HASHTYPE_*`), sys/kern/uipc_mbuf.c (`m_pullup`, `m_split`,
//! `m_copypacket`).

use crate::chain::Chain;
use crate::mbuf::{Cluster, Mbuf};
use crate::{MAX_HDR, MLEN, NoBufs};

/// Checksum and offload flags (`csum_flags`).
pub mod csum {
    /// Transmit requests.
    pub const IP: u32 = 0x0000_0001;
    pub const IP_UDP: u32 = 0x0000_0002;
    pub const IP_TCP: u32 = 0x0000_0004;
    pub const IP_TSO: u32 = 0x0000_0010;
    pub const IP6_UDP: u32 = 0x0000_0200;
    pub const IP6_TCP: u32 = 0x0000_0400;
    pub const IP6_TSO: u32 = 0x0000_1000;
    /// Receive results.
    pub const L3_CALC: u32 = 0x0100_0000;
    pub const L3_VALID: u32 = 0x0200_0000;
    pub const L4_CALC: u32 = 0x0400_0000;
    pub const L4_VALID: u32 = 0x0800_0000;
    pub const IP_CHECKED: u32 = L3_CALC;
    pub const IP_VALID: u32 = L3_VALID;
    pub const DATA_VALID: u32 = L4_VALID;
    pub const PSEUDO_HDR: u32 = L4_CALC;
}

/// Packet flags kept in the header (`M_BCAST` and the like).
pub mod flags {
    pub const EOR: u32 = 0x0000_0004;
    pub const BCAST: u32 = 0x0000_0010;
    pub const MCAST: u32 = 0x0000_0020;
    pub const PROMISC: u32 = 0x0000_0040;
    pub const VLANTAG: u32 = 0x0000_0080;
    pub const TSTMP: u32 = 0x0000_0400;
}

/// Flow hash types (`M_HASHTYPE_*`).
pub mod hash {
    pub const NONE: u8 = 0;
    pub const RSS_IPV4: u8 = 0x80 | 1;
    pub const RSS_TCP_IPV4: u8 = 0x80 | 2;
    pub const RSS_IPV6: u8 = 0x80 | 3;
    pub const RSS_TCP_IPV6: u8 = 0x80 | 4;
    pub const RSS_UDP_IPV4: u8 = 0x80 | 7;
    pub const RSS_UDP_IPV6: u8 = 0x80 | 9;
    pub const OPAQUE: u8 = 0x3f;
    pub const OPAQUE_HASH: u8 = 0x80 | OPAQUE;
}

/// What travels with a packet besides its data (`struct pkthdr`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PktHdr {
    /// The receiving interface's index, 0 for none (`rcvif`).
    pub rcvif: u16,
    /// The flow it belongs to, and how that was derived.
    pub flowid: u32,
    pub hash_type: u8,
    /// `CSUM_*` requests on output, results on input.
    pub csum_flags: u32,
    /// The checksum a device computed, or where to put one.
    pub csum_data: u32,
    /// The TCP segment size for TSO (`tso_segsz`).
    pub tso_segsz: u16,
    /// The VLAN tag, with `flags::VLANTAG`.
    pub vlan_tag: u16,
    /// `flags::*`.
    pub flags: u32,
    pub fib: u16,
    /// The VNET it belongs to.
    pub vnet: u32,
    /// Header lengths (`l2hlen` .. `l4hlen`).
    pub l2hlen: u8,
    pub l3hlen: u8,
    pub l4hlen: u8,
    /// The receive timestamp in nanoseconds, with `flags::TSTMP`.
    pub rcv_tstmp: u64,
}

/// A header a packet can be read as: `LEN` bytes, parsed or rejected.
pub trait Header: Sized {
    const LEN: usize;
    /// `b` is exactly `LEN` bytes.
    fn parse(b: &[u8]) -> Option<Self>;
}

/// Why a typed pullup failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pullup {
    /// The packet is shorter than the header.
    Short,
    /// The header's bytes do not parse.
    Malformed,
    NoBufs,
}

/// A packet: its header and its data.
#[derive(Debug, Default)]
pub struct Packet {
    pub hdr: PktHdr,
    pub data: Chain,
}

impl Packet {
    /// An empty packet whose first mbuf reserves `MAX_HDR` bytes for the
    /// headers prepended on output (`m_gethdr` and `M_ALIGN`), with a
    /// cluster if `room` bytes of data after them need one.
    ///
    /// # Errors
    ///
    /// `NoBufs` at a zone's limit.
    pub fn for_output(room: usize) -> Result<Self, NoBufs> {
        let need = room + MAX_HDR as usize;
        let mut m = if need <= MLEN as usize {
            Mbuf::new()?
        } else {
            Mbuf::with_cluster(Cluster::fitting(need).unwrap_or(Cluster::Jumbo16))?
        };
        m.reserve(MAX_HDR as usize);
        Ok(Self {
            hdr: PktHdr::default(),
            data: Chain::from(m),
        })
    }

    /// Its length (`m_pkthdr.len`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The first `n` bytes, contiguous (`m_pullup`).
    ///
    /// # Errors
    ///
    /// As [`Chain::pullup`].
    pub fn pullup(&mut self, n: usize) -> Result<&[u8], NoBufs> {
        self.data.pullup(n)
    }

    /// The header `H` at `off`, read from bytes made contiguous first.
    ///
    /// # Errors
    ///
    /// `Short` if the packet ends first, `Malformed` if it does not parse,
    /// `NoBufs` if it could not be made contiguous.
    pub fn header<H: Header>(&mut self, off: usize) -> Result<H, Pullup> {
        let end = off.checked_add(H::LEN).ok_or(Pullup::Short)?;
        if end > self.len() {
            return Err(Pullup::Short);
        }
        if end > MLEN as usize {
            let mut b = [0u8; MLEN as usize];
            let b = b.get_mut(..H::LEN).ok_or(Pullup::NoBufs)?;
            self.data.copy_out(off, b);
            return H::parse(b).ok_or(Pullup::Malformed);
        }
        let b = self.data.pullup(end).map_err(|_| Pullup::NoBufs)?;
        H::parse(&b[off..]).ok_or(Pullup::Malformed)
    }

    /// `n` bytes before its data, for a header (`M_PREPEND`).
    ///
    /// # Errors
    ///
    /// As [`Chain::prepend`].
    pub fn prepend(&mut self, n: usize) -> Result<&mut [u8], NoBufs> {
        self.data.prepend(n)
    }

    /// Splits it after `at` bytes; the rest gets a copy of the header
    /// (`m_split` of a packet).
    ///
    /// # Errors
    ///
    /// As [`Chain::split_off`].
    pub fn split_off(&mut self, at: usize) -> Result<Self, NoBufs> {
        Ok(Self {
            hdr: self.hdr,
            data: self.data.split_off(at)?,
        })
    }

    /// A copy by reference (`m_copypacket`).
    ///
    /// # Errors
    ///
    /// As [`Chain::copy`].
    pub fn copy(&self) -> Result<Self, NoBufs> {
        Ok(Self {
            hdr: self.hdr,
            data: self.data.copy(0, self.len())?,
        })
    }
}
