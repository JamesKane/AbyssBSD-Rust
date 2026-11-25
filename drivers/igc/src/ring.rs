// SPDX-License-Identifier: BSD-2-Clause
//! The descriptor rings (advanced descriptors, one queue each way): what a
//! posted receive buffer and a queued frame look like, and the cursors
//! that follow the controller around the ring. Pure code over [`Words`], so
//! the tests run it against a model of the controller.
//!
//! A descriptor is four little-endian words.
//!
//! - Receive, posted: the buffer's bus address in words 0 and 1, and zero
//!   in the header address (words 2 and 3). The controller writes it back
//!   with word 2 the status and errors and word 3 the length (low half) and
//!   VLAN tag.
//! - Transmit: the buffer's address in words 0 and 1, word 2 the length
//!   with the command bits, and word 3 the payload length. The controller
//!   sets the done bit of word 3 in the last descriptor of a frame whose
//!   command asked for a status report.
//!
//! The controller owns the descriptors from its head up to, not including,
//! the tail the driver writes: a ring of [`DESCS`] holds at most
//! `DESCS - 1`, so that an empty ring and a full one differ.
//!
//! Behavior reference: sys/dev/igc/igc_txrx.c.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::regs as r;

/// Descriptors in each ring.
pub const DESCS: usize = 256;
/// Words in a descriptor.
pub const WORDS: usize = 4;
/// Bytes of a descriptor.
pub const DESC_BYTES: usize = WORDS * 4;
/// Bytes of a ring, as the controller is told.
pub const RING_BYTES: u32 = 4096;
const _: () = assert!(DESCS * DESC_BYTES == RING_BYTES as usize);
/// A buffer: the receive buffer size the controller is told, and the
/// largest frame sent (1500 and the Ethernet and VLAN headers).
pub const SLOT: usize = 2048;
/// The longest frame sent, without its CRC.
pub const MAX_FRAME: usize = 1518;

/// A ring's descriptors, a word at a time.
pub trait Words {
    fn get(&self, desc: usize, word: usize) -> u32;
    fn set(&self, desc: usize, word: usize, v: u32);
}

/// What the controller says of a received frame's checksums
/// (`igc_rx_checksum`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxCsum {
    pub ip_checked: bool,
    pub ip_valid: bool,
    pub l4_valid: bool,
}

impl RxCsum {
    /// From a descriptor's status and error word: nothing is claimed for a
    /// frame the controller did not check, or one with a bad checksum.
    #[must_use]
    pub fn of(staterr: u32) -> Self {
        if staterr & r::RXD_STAT_IXSM != 0 || staterr & (r::RXD_ERR_IPE | r::RXD_ERR_L4E) != 0 {
            return Self::default();
        }
        let ip = staterr & r::RXD_STAT_IPCS != 0;
        Self {
            ip_checked: ip,
            ip_valid: ip,
            l4_valid: staterr & (r::RXD_STAT_TCPCS | r::RXD_STAT_UDPCS) != 0,
        }
    }
}

/// A receive descriptor the controller has returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taken {
    /// The descriptor's index, which is its buffer's.
    pub slot: usize,
    /// The bytes the controller put in the buffer.
    pub len: usize,
    /// The last descriptor of its frame.
    pub eop: bool,
    /// The frame had a receive error (`IGC_RXD_ERR_RXE`).
    pub bad: bool,
    pub csum: RxCsum,
}

/// The receive ring's cursors.
#[derive(Clone, Copy, Debug, Default)]
pub struct RxRing {
    /// The next descriptor the controller returns.
    next: usize,
    /// The next descriptor to give it: the controller fills up to, not
    /// including, this one.
    tail: usize,
}

impl RxRing {
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 0, tail: 0 }
    }

    fn post_at(descs: &impl Words, i: usize, bus: u64) {
        descs.set(i, 0, r::lo(bus));
        descs.set(i, 1, r::hi(bus));
        descs.set(i, 2, 0);
        descs.set(i, 3, 0);
    }

    /// Posts the first `DESCS - 1` buffers and returns the tail to write
    /// (`slot_bus` gives a descriptor's buffer's bus address).
    pub fn fill(&mut self, descs: &impl Words, slot_bus: impl Fn(usize) -> u64) -> u32 {
        self.next = 0;
        for i in 0..DESCS - 1 {
            Self::post_at(descs, i, slot_bus(i));
        }
        // The one descriptor left over is not posted until its turn.
        for w in 0..WORDS {
            descs.set(DESCS - 1, w, 0);
        }
        self.tail = DESCS - 1;
        u32::try_from(self.tail).unwrap_or(0)
    }

    /// The next returned descriptor, if the controller has returned one.
    pub fn take(&mut self, descs: &impl Words) -> Option<Taken> {
        let i = self.next;
        let st = descs.get(i, 2);
        if st & r::RXD_STAT_DD == 0 {
            return None;
        }
        let len = (descs.get(i, 3) & 0xffff) as usize;
        self.next = (i + 1) % DESCS;
        let eop = st & r::RXD_STAT_EOP != 0;
        Some(Taken {
            slot: i,
            len,
            eop,
            bad: eop && st & r::RXD_ERR_RXE != 0,
            csum: RxCsum::of(st),
        })
    }

    /// Posts the descriptor at the tail (its buffer is free: the one that
    /// was taken a turn ago, or the one left over) and moves the tail on:
    /// the new tail to write. Called once for each descriptor taken.
    pub fn refill(&mut self, descs: &impl Words, slot_bus: impl Fn(usize) -> u64) -> u32 {
        Self::post_at(descs, self.tail, slot_bus(self.tail));
        self.tail = (self.tail + 1) % DESCS;
        u32::try_from(self.tail).unwrap_or(0)
    }

    /// The descriptor at the head, if the controller has returned it
    /// without it being taken: for the stall check.
    #[must_use]
    pub fn ready(&self, descs: &impl Words) -> Option<usize> {
        (descs.get(self.next, 2) & r::RXD_STAT_DD != 0).then_some(self.next)
    }
}

/// A received frame being put together from its descriptors' buffers.
#[derive(Debug, Default)]
pub struct Assembler {
    buf: Vec<u8>,
    /// The frame was too long for [`MAX_FRAME`] plus slack, or came in
    /// pieces after a bad one.
    dropped: bool,
}

/// What [`Assembler::add`] made of a descriptor.
#[derive(Debug, PartialEq, Eq)]
pub enum Assembled {
    /// More descriptors to come.
    More,
    /// The frame is lost (a receive error).
    Dropped,
    /// The whole frame, and what is known of its checksums.
    Frame(Vec<u8>, RxCsum),
}

impl Assembler {
    /// Forgets a frame in progress.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.dropped = false;
    }

    /// Adds descriptor `t`'s `data`.
    pub fn add(&mut self, t: &Taken, data: &[u8]) -> Assembled {
        // A VLAN-tagged frame and a little slack; more is not a frame this
        // driver asked for.
        if self.buf.len() + data.len() > SLOT * 4 {
            self.dropped = true;
        } else {
            self.buf.extend_from_slice(data);
        }
        if !t.eop {
            return Assembled::More;
        }
        let dropped = core::mem::take(&mut self.dropped) || t.bad;
        let frame = core::mem::take(&mut self.buf);
        if dropped {
            Assembled::Dropped
        } else {
            Assembled::Frame(frame, t.csum)
        }
    }
}

/// A frame queued, by its first and last descriptors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Queued {
    first: usize,
    last: usize,
    count: usize,
}

/// The transmit ring's cursors.
#[derive(Debug, Default)]
pub struct TxRing {
    /// The next descriptor to fill.
    tail: usize,
    /// Descriptors the controller holds.
    used: usize,
    frames: VecDeque<Queued>,
}

/// The ring has no room for the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

impl TxRing {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Empties the ring: every descriptor cleared, nothing queued.
    pub fn init(&mut self, descs: &impl Words) {
        for d in 0..DESCS {
            for w in 0..WORDS {
                descs.set(d, w, 0);
            }
        }
        self.tail = 0;
        self.used = 0;
        self.frames.clear();
    }

    /// Descriptors free for a frame.
    #[must_use]
    pub fn free(&self) -> usize {
        DESCS - 1 - self.used
    }

    /// Frames the controller holds.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.frames.len()
    }

    /// The next descriptor a one-descriptor frame takes (its slot's).
    #[must_use]
    pub fn next(&self) -> usize {
        self.tail
    }

    /// Queues a frame of `segs` (each a bus address and length; `total`
    /// bytes in all) and returns its first descriptor's index and the tail
    /// to write.
    ///
    /// # Errors
    ///
    /// [`Full`] if the descriptors do not fit.
    pub fn push(&mut self, descs: &impl Words, segs: &[(u64, u32)]) -> Result<(usize, u32), Full> {
        if segs.is_empty() || segs.len() > self.free() {
            return Err(Full);
        }
        let total: u32 = segs.iter().map(|s| s.1).sum();
        let first = self.tail;
        for (i, &(bus, len)) in segs.iter().enumerate() {
            let last = i + 1 == segs.len();
            let d = self.tail;
            let mut cmd = len | r::ADVTXD_DTYP_DATA | r::ADVTXD_DCMD_IFCS | r::ADVTXD_DCMD_DEXT;
            if last {
                cmd |= r::ADVTXD_DCMD_EOP | r::ADVTXD_DCMD_RS;
            }
            descs.set(d, 0, r::lo(bus));
            descs.set(d, 1, r::hi(bus));
            descs.set(d, 2, cmd);
            descs.set(
                d,
                3,
                if i == 0 {
                    total << r::ADVTXD_PAYLEN_SHIFT
                } else {
                    0
                },
            );
            self.tail = (self.tail + 1) % DESCS;
        }
        let last = (self.tail + DESCS - 1) % DESCS;
        self.used += segs.len();
        self.frames.push_back(Queued {
            first,
            last,
            count: segs.len(),
        });
        Ok((first, u32::try_from(self.tail).unwrap_or(0)))
    }

    /// The oldest frame the controller has sent, if any: its first
    /// descriptor's index.
    pub fn reap(&mut self, descs: &impl Words) -> Option<usize> {
        let q = *self.frames.front()?;
        if descs.get(q.last, 3) & r::TXD_STAT_DD == 0 {
            return None;
        }
        self.frames.pop_front();
        self.used -= q.count;
        Some(q.first)
    }
}
