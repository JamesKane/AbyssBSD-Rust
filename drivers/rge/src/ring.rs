// SPDX-License-Identifier: BSD-2-Clause
//! The descriptor rings: 1024 transmit and 1024 receive descriptors, each
//! 32 bytes, handed between the driver and the controller by their
//! `OWN` bit.
//!
//! - Receive: descriptor `i` always describes slot `i`, a fixed
//!   2 KiB buffer, and all of them stay posted; a returned frame (one or
//!   more descriptors, `SOF` to `EOF`) is copied out and each descriptor
//!   posted again at once. Everything the controller writes is checked:
//!   lengths against the slot, the descriptor sequence against `SOF` and
//!   `EOF`.
//! - Transmit: a frame takes one descriptor per DMA segment; the first is
//!   handed over last, after the others, so the controller never sees a
//!   partial frame. Completions are reaped from the oldest frame on.
//!
//! The formats are FreeBSD's `rge_tx_desc` and `rge_rx_desc`, as eight
//! little-endian words.
//!
//! Behavior reference: sys/dev/rge/if_rge.c (`rge_newbuf`, `rge_rxeof`,
//! `rge_encap`, `rge_txeof`).

use alloc::vec::Vec;

/// Descriptors in each ring.
pub const DESCS: usize = 1024;
/// Words in a descriptor.
pub const WORDS: usize = 8;
/// A receive slot's size (`MCLBYTES`).
pub const SLOT: usize = 2048;
/// The most descriptors one transmitted frame may use (`RGE_TX_NSEGS`).
pub const TX_SEGS: usize = 32;
/// The shortest frame the controller sends correctly: it does not pad,
/// and its checksum offload is unreliable below this (the delta
/// specification's critical summary, item 3).
pub const MIN_FRAME: usize = 60;
/// Computes, in `frame` (an untagged Ethernet frame shorter than
/// [`MIN_FRAME`]), the IPv4 checksums `csum_flags` leave to the device:
/// the controller's are wrong for such frames (the delta specification,
/// 03 §4). `csum_data` is where the transport checksum is in its header;
/// the field holds the pseudo-header's sum, as for `in_delayed_cksum`.
/// Anything not IPv4, or cut short, is left as it is.
pub fn short_csum(frame: &mut [u8], csum_flags: u32, csum_data: u32) {
    use mbuf::csum;
    const ETH: usize = 14;
    if frame.len() < ETH + 20 || frame[12..14] != [0x08, 0x00] {
        return;
    }
    let hlen = usize::from(frame[ETH] & 0xf) * 4;
    let end = ETH + usize::from(u16::from_be_bytes([frame[ETH + 2], frame[ETH + 3]]));
    if hlen < 20 || end < ETH + hlen || end > frame.len() {
        return;
    }
    if csum_flags & csum::IP != 0 {
        frame[ETH + 10..ETH + 12].fill(0);
        let c = !sum16(&frame[ETH..ETH + hlen]);
        frame[ETH + 10..ETH + 12].copy_from_slice(&c.to_be_bytes());
    }
    if csum_flags & (csum::IP_TCP | csum::IP_UDP) != 0 {
        let l4 = ETH + hlen;
        let Some(at) = usize::try_from(csum_data)
            .ok()
            .and_then(|o| l4.checked_add(o))
        else {
            return;
        };
        if at + 2 > end {
            return;
        }
        let mut c = !sum16(&frame[l4..end]);
        // UDP sends a zero sum as all ones (0 is "none").
        if c == 0 && csum_flags & csum::IP_UDP != 0 {
            c = 0xffff;
        }
        frame[at..at + 2].copy_from_slice(&c.to_be_bytes());
    }
}

/// The one's-complement sum of `b`'s big-endian words (RFC 1071), folded;
/// an odd last byte is a word's high half.
fn sum16(b: &[u8]) -> u16 {
    let mut s: u32 = 0;
    for w in b.chunks(2) {
        s += u32::from(u16::from_be_bytes([w[0], w.get(1).copied().unwrap_or(0)]));
    }
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    #[allow(clippy::cast_possible_truncation, reason = "folded to 16 bits")]
    {
        s as u16
    }
}

/// The Ethernet CRC the controller leaves on received frames.
pub const CRC_LEN: usize = 4;

pub mod tx {
    pub const OWN: u32 = 0x8000_0000;
    pub const EOR: u32 = 0x4000_0000;
    pub const SOF: u32 = 0x2000_0000;
    pub const EOF: u32 = 0x1000_0000;
    pub const TXERR: u32 = 0x0080_0000;
    pub const EXCESSCOLL: u32 = 0x0010_0000;
    pub const COLL: u32 = 0x000f_0000;

    /// Extended status: the controller computes the IPv4 header's
    /// checksum, without which it computes neither of the others.
    pub const IPCSUM: u32 = 0x2000_0000;
    pub const TCPCSUM: u32 = 0x4000_0000;
    pub const UDPCSUM: u32 = 0x8000_0000;

    /// The extended status asking for the checksums `csum_flags` leave to
    /// the device (`rge_encap`).
    #[must_use]
    pub const fn csum_ext(csum_flags: u32) -> u32 {
        use mbuf::csum;
        if csum_flags & (csum::IP | csum::IP_TCP | csum::IP_UDP) == 0 {
            return 0;
        }
        let mut ext = IPCSUM;
        if csum_flags & csum::IP_TCP != 0 {
            ext |= TCPCSUM;
        }
        if csum_flags & csum::IP_UDP != 0 {
            ext |= UDPCSUM;
        }
        ext
    }

    /// Command word: the frame is cut into segments by the device
    /// (TCP over IPv4, or over IPv6), with the TCP header's offset from
    /// the frame's start in bits 24:18.
    pub const TSO4: u32 = 0x0400_0000;
    pub const TSO6: u32 = 0x0200_0000;
    pub const TSO_OFFSET_SHIFT: u32 = 18;
    /// The TCP header may start at most here (else software cuts it).
    pub const TSO_MAX_OFFSET: usize = 127;
    /// Extended status in TSO mode: the segment size in bits 28:18.
    pub const TSO_MSS_SHIFT: u32 = 18;
    pub const TSO_MAX_MSS: u32 = 0x7ff;

    /// The command and extended status words for a TSO frame of IPv4 TCP
    /// whose TCP header starts `tcp_offset` bytes into it, cut into
    /// segments of `mss` payload bytes; `None` if the controller cannot
    /// take it.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, reason = "at most 127, checked")]
    pub const fn tso4(tcp_offset: usize, mss: u32) -> Option<(u32, u32)> {
        if tcp_offset > TSO_MAX_OFFSET || mss == 0 || mss > TSO_MAX_MSS {
            return None;
        }
        Some((
            TSO4 | ((tcp_offset as u32) << TSO_OFFSET_SHIFT),
            mss << TSO_MSS_SHIFT,
        ))
    }

    /// Word 0 is the command and status; word 1 the extended status;
    /// words 2 and 3 the buffer's address.
    pub const CMDSTS: usize = 0;
    pub const EXTSTS: usize = 1;
    pub const ADDR: usize = 2;
}

pub mod rx {
    pub const OWN: u32 = 0x8000_0000;
    pub const EOR: u32 = 0x4000_0000;
    pub const SOF: u32 = 0x0200_0000;
    pub const EOF: u32 = 0x0100_0000;
    pub const RXERRSUM: u32 = 0x0010_0000;
    pub const FRAGLEN: u32 = 0x0000_3fff;

    pub const TCPCSUMERR: u32 = 0x0100_0000;
    pub const UDPCSUMERR: u32 = 0x0200_0000;
    pub const IPCSUMERR: u32 = 0x0400_0000;
    pub const TCPPKT: u32 = 0x1000_0000;
    pub const UDPPKT: u32 = 0x2000_0000;
    pub const IPV4: u32 = 0x4000_0000;
    pub const IPV6: u32 = 0x8000_0000;
    /// Words 4 and 5 are the buffer's address; word 6 the extended
    /// status; word 7 the command and status, written last by either
    /// side.
    pub const ADDR: usize = 4;
    pub const EXTSTS: usize = 6;
    pub const CMDSTS: usize = 7;
}

/// Where descriptors live: DMA memory, or a vector in tests. Word `w` of
/// descriptor `d`.
pub trait Words {
    /// Reads a word.
    fn get(&self, d: usize, w: usize) -> u32;
    /// Writes a word.
    fn set(&self, d: usize, w: usize, v: u32);
}

/// The ordering a hand-over needs: [`hal::dma::write_barrier`] and
/// [`hal::dma::read_barrier`] in the kernel.
pub trait Barriers {
    /// Earlier descriptor writes before later ones, as the device sees.
    fn write(&self);
    /// The `OWN` word's read before the rest.
    fn read(&self);
}

/// The kernel's barriers.
#[derive(Clone, Copy, Debug)]
pub struct Hal;

impl Barriers for Hal {
    fn write(&self) {
        hal::dma::write_barrier();
    }
    fn read(&self) {
        hal::dma::read_barrier();
    }
}

/// What a received frame's checksum status says (`rge_rxeof`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxCsum {
    /// An IPv4 header the controller checked, and whether it was right.
    pub ip_checked: bool,
    pub ip_valid: bool,
    /// A TCP or UDP checksum the controller verified as right.
    pub l4_valid: bool,
}

impl RxCsum {
    /// From a frame's last descriptor's extended status.
    #[must_use]
    pub fn of(ext: u32) -> Self {
        let ip = ext & (rx::IPV4 | rx::IPV6) != 0;
        let tcp_ok = ip && ext & rx::TCPPKT != 0 && ext & rx::TCPCSUMERR == 0;
        let udp_ok = ip && ext & rx::UDPPKT != 0 && ext & rx::UDPCSUMERR == 0;
        Self {
            ip_checked: ext & rx::IPV4 != 0,
            ip_valid: ext & rx::IPV4 != 0 && ext & rx::IPCSUMERR == 0,
            l4_valid: tcp_ok || udp_ok,
        }
    }
}

/// One descriptor's worth of a received frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxPiece {
    /// The slot its data is in.
    pub slot: usize,
    /// Its length.
    pub len: usize,
    /// It starts a frame.
    pub first: bool,
    /// It ends one.
    pub last: bool,
    /// The controller found the frame bad (CRC, length).
    pub error: bool,
    /// The extended status (meaningful on the last piece).
    pub ext: u32,
}

/// The receive ring's state.
#[derive(Debug)]
pub struct RxRing {
    /// The next descriptor the controller returns.
    next: usize,
}

impl Default for RxRing {
    fn default() -> Self {
        Self::new()
    }
}

impl RxRing {
    /// A ring about to be posted.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    /// Hands descriptor `d` (slot `d` at `bus`) to the controller.
    pub fn post(m: &impl Words, b: &impl Barriers, d: usize, bus: u64) {
        #[allow(clippy::cast_possible_truncation, reason = "the address's halves")]
        {
            m.set(d, rx::ADDR, bus as u32);
            m.set(d, rx::ADDR + 1, (bus >> 32) as u32);
        }
        m.set(d, rx::EXTSTS, 0);
        let eor = if d == DESCS - 1 { rx::EOR } else { 0 };
        #[allow(clippy::cast_possible_truncation, reason = "SLOT fits FRAGLEN")]
        let len = SLOT as u32;
        b.write();
        m.set(d, rx::CMDSTS, len | eor | rx::OWN);
    }

    /// Posts every descriptor, slot `i` at `bus(i)`, from the start.
    pub fn fill(&mut self, m: &impl Words, b: &impl Barriers, bus: impl Fn(usize) -> u64) {
        self.next = 0;
        for d in 0..DESCS {
            Self::post(m, b, d, bus(d));
        }
    }

    /// The head descriptor, if the controller has returned it (a look,
    /// for the stall check; nothing is taken).
    pub fn ready(&self, m: &impl Words) -> Option<usize> {
        (m.get(self.next, rx::CMDSTS) & rx::OWN == 0).then_some(self.next)
    }

    /// The next descriptor the controller has returned, if any; the caller
    /// copies its slot out, then posts it again.
    pub fn take(&mut self, m: &impl Words, b: &impl Barriers) -> Option<RxPiece> {
        let d = self.next;
        let cmd = m.get(d, rx::CMDSTS);
        if cmd & rx::OWN != 0 {
            return None;
        }
        b.read();
        self.next = (d + 1) % DESCS;
        let len = (cmd & rx::FRAGLEN) as usize;
        Some(RxPiece {
            slot: d,
            // Never past the slot, whatever the controller says.
            len: len.min(SLOT),
            first: cmd & rx::SOF != 0,
            last: cmd & rx::EOF != 0,
            error: cmd & rx::RXERRSUM != 0 || len > SLOT,
            ext: m.get(d, rx::EXTSTS),
        })
    }
}

/// Puts received pieces back together into frames (`rge_rxeof`'s head
/// and tail): a piece without `SOF` continues the frame before it; a
/// stray continuation, or a frame cut short by a new `SOF`, is dropped.
#[derive(Debug, Default)]
pub struct Assembler {
    /// The frame so far: its bytes, and whether any piece was bad.
    partial: Option<(Vec<u8>, bool)>,
}

/// What a piece made.
#[derive(Debug, PartialEq, Eq)]
pub enum Assembled {
    /// Nothing yet: the frame continues.
    More,
    /// A whole frame, its CRC removed, with the checksum status.
    Frame(Vec<u8>, RxCsum),
    /// A frame the controller marked bad, or a broken sequence.
    Dropped,
}

impl Assembler {
    /// Adds a piece whose `data` the caller copied from its slot.
    pub fn add(&mut self, p: &RxPiece, data: &[u8]) -> Assembled {
        let mut dropped = false;
        if p.first {
            // A frame still open is cut short.
            dropped = self.partial.take().is_some();
            self.partial = Some((Vec::with_capacity(p.len), false));
        }
        let Some((buf, bad)) = &mut self.partial else {
            return Assembled::Dropped;
        };
        buf.extend_from_slice(data);
        *bad |= p.error;
        if !p.last {
            return if dropped {
                Assembled::Dropped
            } else {
                Assembled::More
            };
        }
        let (mut buf, bad) = self.partial.take().unwrap_or_default();
        if bad || buf.len() < CRC_LEN {
            return Assembled::Dropped;
        }
        buf.truncate(buf.len() - CRC_LEN);
        Assembled::Frame(buf, RxCsum::of(p.ext))
    }

    /// Whether a frame is open: a piece has come that is not the last.
    #[must_use]
    pub fn open(&self) -> bool {
        self.partial.is_some()
    }

    /// Forgets a frame in progress (on reset).
    pub fn clear(&mut self) {
        self.partial = None;
    }
}

/// The transmit ring's state: where the next frame goes, and the frames
/// on the controller, oldest first, by (first descriptor, last
/// descriptor).
#[derive(Debug)]
pub struct TxRing {
    next: usize,
    used: usize,
    in_flight: alloc::collections::VecDeque<(usize, usize)>,
}

impl Default for TxRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a frame was not queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxFull {
    /// Not enough free descriptors.
    Full,
    /// More segments than a frame may use, or none.
    BadShape,
}

impl TxRing {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next: 0,
            used: 0,
            in_flight: alloc::collections::VecDeque::new(),
        }
    }

    /// Clears every descriptor but the last's end-of-ring mark
    /// (`rge_tx_list_init`).
    pub fn init(&mut self, m: &impl Words) {
        for d in 0..DESCS {
            for w in 0..WORDS {
                m.set(d, w, 0);
            }
        }
        m.set(DESCS - 1, tx::CMDSTS, tx::EOR);
        self.next = 0;
        self.used = 0;
        self.in_flight.clear();
    }

    /// Free descriptors. One stays unused, so a full ring is not mistaken
    /// for an empty one.
    #[must_use]
    pub fn free(&self) -> usize {
        DESCS - 1 - self.used
    }

    /// The descriptor the next frame starts at.
    #[must_use]
    pub fn next(&self) -> usize {
        self.next
    }

    /// Frames on the controller.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Queues a frame of `segs` (bus address, length) with extended
    /// status `ext`, handing the first descriptor over last
    /// (`rge_encap`); its first descriptor's index. The caller then rings
    /// the doorbell.
    ///
    /// # Errors
    ///
    /// [`TxFull`] if it does not fit or has no segments.
    pub fn push(
        &mut self,
        m: &impl Words,
        b: &impl Barriers,
        segs: &[(u64, u32)],
        ext: u32,
        frame_cmd: u32,
    ) -> Result<usize, TxFull> {
        if segs.is_empty() || segs.len() > TX_SEGS {
            return Err(TxFull::BadShape);
        }
        if segs.len() > self.free() {
            return Err(TxFull::Full);
        }
        let first = self.next;
        let n = segs.len();
        let write = |d: usize, (bus, len): (u64, u32), cmd: u32| {
            #[allow(clippy::cast_possible_truncation, reason = "the address's halves")]
            {
                m.set(d, tx::ADDR, bus as u32);
                m.set(d, tx::ADDR + 1, (bus >> 32) as u32);
            }
            m.set(d, tx::EXTSTS, ext);
            let eor = if d == DESCS - 1 { tx::EOR } else { 0 };
            b.write();
            m.set(d, tx::CMDSTS, cmd | eor | frame_cmd | (len & 0xffff));
        };
        let mut last = first;
        for (k, &seg) in segs.iter().enumerate().skip(1) {
            last = (first + k) % DESCS;
            let eof = if k == n - 1 { tx::EOF } else { 0 };
            write(last, seg, tx::OWN | eof);
        }
        let eof = if n == 1 { tx::EOF } else { 0 };
        write(first, segs[0], tx::SOF | eof);
        // Now the first: the whole frame is the controller's.
        b.write();
        let cmd = m.get(first, tx::CMDSTS);
        m.set(first, tx::CMDSTS, cmd | tx::OWN);
        self.next = (first + n) % DESCS;
        self.used += n;
        self.in_flight.push_back((first, last));
        Ok(first)
    }

    /// The oldest frame the controller has finished: its first
    /// descriptor's index (the caller frees what it held there) and
    /// whether it went out without error (`rge_txeof`).
    /// The first descriptor of the oldest frame in flight, if any.
    #[must_use]
    pub fn oldest(&self) -> Option<usize> {
        self.in_flight.front().map(|f| f.0)
    }

    pub fn reap(&mut self, m: &impl Words, b: &impl Barriers) -> Option<(usize, bool)> {
        let &(first, last) = self.in_flight.front()?;
        let cmd = m.get(last, tx::CMDSTS);
        if cmd & tx::OWN != 0 {
            return None;
        }
        b.read();
        self.in_flight.pop_front();
        let n = (last + DESCS - first) % DESCS + 1;
        self.used -= n;
        // A TSO frame's command word holds the header offset where the
        // error bit is: only a frame that is not one has a status to read.
        Some((
            first,
            cmd & tx::TXERR == 0 || cmd & (tx::TSO4 | tx::TSO6) != 0,
        ))
    }
}
