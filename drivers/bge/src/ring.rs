// SPDX-License-Identifier: BSD-2-Clause
//! The rings and the status block, as 32-bit words in host order
//! (`if_bgereg.h`'s little-endian layouts, which `BGE_MODE_CTL`'s word
//! swaps make the chip's view): each word holds two 16-bit fields, the
//! first in the low half.
//!
//! - The standard producer ring ([`STD_RX`] descriptors): the driver posts
//!   buffer slots, each descriptor naming its slot in `idx`.
//! - The return ring ([`RETURN`]): the chip returns received frames, with
//!   the slot they landed in; its producer index is in the status block.
//! - The send ring ([`TX`]): the driver's frames; the chip's consumer
//!   index is in the status block.
//!
//! As FreeBSD's `bge_newbuf_std`, ring positions and buffer slots are
//! decoupled; here every slot is a fixed 2 KiB buffer mapped at attach, so
//! a frame is copied out of its slot and the slot posted again.

#![allow(
    clippy::cast_possible_truncation,
    reason = "descriptor fields are 16 bits, ring indices below 1024, addresses split in halves"
)]

/// Standard producer ring descriptors (`BGE_STD_RX_RING_CNT`).
pub const STD_RX: usize = 512;
/// Return ring descriptors (`BGE_RETURN_RING_CNT`).
pub const RETURN: usize = 1024;
/// Send ring descriptors (`BGE_TX_RING_CNT`).
pub const TX: usize = 512;
/// Words in a receive descriptor (32 bytes) and a send descriptor (16).
pub const RX_WORDS: usize = 8;
pub const TX_WORDS: usize = 4;
/// Words in the status block (32 bytes).
pub const STATUS_WORDS: usize = 8;
/// A receive slot: a cluster, as `MCLBYTES`.
pub const SLOT: usize = 2048;
/// What a receive slot is posted with: the cluster less `ETHER_ALIGN`,
/// whose two bytes put the IP header on a four-byte boundary.
pub const RX_ALIGN: usize = 2;
pub const RX_LEN: usize = SLOT - RX_ALIGN;
/// The CRC the chip leaves at a received frame's end.
pub const CRC_LEN: usize = 4;
/// The shortest frame sent: the chip pads, but its checksums of a padded
/// frame are wrong, so the driver pads (`bge_cksum_pad`).
pub const MIN_FRAME: usize = 60;

/// Receive descriptor flags (`BGE_RXBDFLAG_*`).
pub mod rx_flags {
    pub const END: u16 = 0x0004;
    pub const JUMBO_RING: u16 = 0x0020;
    pub const VLAN_TAG: u16 = 0x0040;
    pub const ERROR: u16 = 0x0400;
    pub const IP_CSUM: u16 = 0x1000;
    pub const TCP_UDP_CSUM: u16 = 0x2000;
    pub const IPV6: u16 = 0x8000;
}

/// `BGE_RXERRFLAG_IP_CSUM_NOK` (5717 class).
pub const RX_ERR_IP_CSUM_NOK: u16 = 0x1000;

/// Send descriptor flags (`BGE_TXBDFLAG_*`).
pub mod tx_flags {
    pub const TCP_UDP_CSUM: u16 = 0x0001;
    pub const IP_CSUM: u16 = 0x0002;
    pub const END: u16 = 0x0004;
}

/// Status block flags (`BGE_STATFLAG_*`).
pub mod status {
    pub const UPDATED: u32 = 0x1;
    pub const LINKSTATE_CHANGED: u32 = 0x2;
    pub const ERROR: u32 = 0x4;
}

/// Word access to DMA memory (host order).
pub trait Words {
    fn get(&self, w: usize) -> u32;
    fn set(&self, w: usize, v: u32);
}

/// The ordering a hand-over needs.
pub trait Barriers {
    /// Before the chip is told of what was written.
    fn write(&self);
    /// After an index from the chip, before what it covers is read.
    fn read(&self);
}

const fn lo(w: u32) -> u16 {
    w as u16
}
const fn hi(w: u32) -> u16 {
    (w >> 16) as u16
}
const fn pair(lo: u16, hi: u16) -> u32 {
    (hi as u32) << 16 | lo as u32
}

/// Writes a producer descriptor at position `pos`: slot `slot`'s buffer,
/// `len` bytes, `END` (`bge_newbuf_std`).
pub fn post_rx(ring: &impl Words, pos: usize, bus: u64, len: u16, slot: u16) {
    let w = pos * RX_WORDS;
    ring.set(w, (bus >> 32) as u32);
    ring.set(w + 1, bus as u32);
    ring.set(w + 2, pair(len, slot));
    ring.set(w + 3, pair(rx_flags::END, 0));
}

/// A returned frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Returned {
    /// The slot it landed in.
    pub slot: usize,
    /// Its length, CRC included.
    pub len: usize,
    pub flags: u16,
    pub tcp_udp_csum: u16,
    pub error: u16,
}

/// Reads the return descriptor at `pos`.
#[must_use]
pub fn returned(ring: &impl Words, pos: usize) -> Returned {
    let w = pos * RX_WORDS;
    let (len_idx, flags, csum, err) = (
        ring.get(w + 2),
        ring.get(w + 3),
        ring.get(w + 4),
        ring.get(w + 5),
    );
    Returned {
        slot: usize::from(hi(len_idx)),
        len: usize::from(lo(len_idx)),
        flags: lo(flags),
        tcp_udp_csum: lo(csum),
        error: hi(err),
    }
}

/// What the chip checked of a received frame (`bge_rxcsum`, 5717 class).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxCsum {
    pub ip_checked: bool,
    pub ip_valid: bool,
    /// The TCP/UDP checksum's value, for `CSUM_DATA_VALID |
    /// CSUM_PSEUDO_HDR`.
    pub l4: Option<u16>,
}

impl Returned {
    /// The checksums, for IPv4 frames only.
    #[must_use]
    pub fn csum(&self) -> RxCsum {
        if self.flags & rx_flags::IPV6 != 0 {
            return RxCsum::default();
        }
        let ip = self.flags & rx_flags::IP_CSUM != 0;
        RxCsum {
            ip_checked: ip,
            ip_valid: ip && self.error & RX_ERR_IP_CSUM_NOK == 0,
            l4: (self.flags & rx_flags::TCP_UDP_CSUM != 0).then_some(self.tcp_udp_csum),
        }
    }
}

/// Writes a send descriptor at `pos`.
pub fn post_tx(ring: &impl Words, pos: usize, bus: u64, len: u16, flags: u16) {
    let w = pos * TX_WORDS;
    ring.set(w, (bus >> 32) as u32);
    ring.set(w + 1, bus as u32);
    ring.set(w + 2, pair(flags, len));
    ring.set(w + 3, 0);
}

/// What the status block says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub flags: u32,
    /// The tag to acknowledge with (tagged status).
    pub tag: u8,
    /// The return ring's producer index.
    pub rx_prod: usize,
    /// The send ring's consumer index.
    pub tx_cons: usize,
}

/// Reads the status block and clears its flags word, so the next update
/// shows (`bge_intr_task` 4605-4625). Indices out of range read as the
/// previous ones' worth: the caller bounds them.
pub fn take_status(block: &impl Words, b: &impl Barriers) -> Status {
    b.read();
    let flags = block.get(0);
    let tag = block.get(1) as u8;
    let idx = block.get(4);
    block.set(0, 0);
    b.write();
    Status {
        flags,
        tag,
        rx_prod: usize::from(lo(idx)),
        tx_cons: usize::from(hi(idx)),
    }
}

/// The standard producer ring's bookkeeping: where the next posted
/// descriptor goes (`bge_std`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StdRing {
    pub next: usize,
}

impl StdRing {
    /// Posts every slot, in order (`bge_init_rx_ring_std`): the producer to
    /// publish is one behind (511).
    pub fn fill(
        &mut self,
        ring: &impl Words,
        b: &impl Barriers,
        bus: impl Fn(usize) -> u64,
    ) -> u32 {
        for s in 0..STD_RX {
            post_rx(ring, s, bus(s) + RX_ALIGN as u64, RX_LEN as u16, s as u16);
        }
        self.next = 0;
        b.write();
        self.producer()
    }

    /// Posts `slot` again at the next position (`bge_rxreuse_std`).
    pub fn repost(&mut self, ring: &impl Words, slot: usize, bus: u64) {
        post_rx(
            ring,
            self.next,
            bus + RX_ALIGN as u64,
            RX_LEN as u16,
            slot as u16,
        );
        self.next = (self.next + 1) % STD_RX;
    }

    /// The producer index to publish: one behind `next`.
    #[must_use]
    pub fn producer(&self) -> u32 {
        ((self.next + STD_RX - 1) % STD_RX) as u32
    }
}

/// The return ring's consumer (`bge_rx_saved_considx`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReturnRing {
    pub cons: usize,
}

impl ReturnRing {
    /// The frames between our consumer and the chip's producer.
    pub fn take(
        &mut self,
        ring: &impl Words,
        prod: usize,
        max: usize,
    ) -> impl Iterator<Item = Returned> {
        let mut out = alloc::vec::Vec::new();
        if prod < RETURN {
            while self.cons != prod && out.len() < max {
                out.push(returned(ring, self.cons));
                self.cons = (self.cons + 1) % RETURN;
            }
        }
        out.into_iter()
    }
}

/// The send ring's bookkeeping (`bge_tx_prodidx`, `bge_tx_saved_considx`,
/// `bge_txcnt`): one descriptor a frame, since every frame fits a slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxRing {
    pub prod: usize,
    pub cons: usize,
    pub in_flight: usize,
}

/// Frames kept free on the send ring (`bge_start_locked`'s 16).
pub const TX_RESERVE: usize = 16;

impl TxRing {
    /// Whether a frame can be queued.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.in_flight < TX - TX_RESERVE
    }

    /// Takes the next position for a frame, advancing the producer.
    pub fn push(&mut self) -> usize {
        let p = self.prod;
        self.prod = (self.prod + 1) % TX;
        self.in_flight += 1;
        p
    }

    /// Reaps up to the chip's consumer `cons` (`bge_txeof`): how many.
    pub fn reap(&mut self, cons: usize) -> usize {
        if cons >= TX {
            return 0;
        }
        let mut n = 0;
        while self.cons != cons && self.in_flight > 0 {
            self.cons = (self.cons + 1) % TX;
            self.in_flight -= 1;
            n += 1;
        }
        n
    }

    /// Empties the ring (`bge_init_tx_ring`).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}
