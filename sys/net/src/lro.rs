// SPDX-License-Identifier: BSD-2-Clause
//! Large receive offload in software (`tcp_lro`): a driver hands the TCP
//! frames of a receive pass to a [`Lro`] instead of up the stack, and
//! consecutive in-order segments of one connection go up as one frame,
//! with the count of segments it carries (`lro_nsegs`). TCP then runs once
//! per merged frame and, since the frame is longer than a segment,
//! acknowledges it at once (`DELAY_ACK`): at a gigabit a merge of a dozen
//! segments takes a dozen inputs and six ACKs down to one of each, as
//! stock FreeBSD's `re` gets with `IFCAP_LRO`.
//!
//! What merges is FreeBSD's choice (`tcp_lro_rx_common`,
//! `tcp_lro_condense`): IPv4 TCP whose checksums the device verified, no
//! IP options or fragments, no TCP options but timestamps, only ACK and
//! PSH set, the next sequence number, timestamps that do not go back, and
//! an ACK that is not a duplicate. Anything else of the connection goes up
//! after what is held of it, in order. Nothing merges while the VNET
//! forwards (`V_ipforwarding`).
//!
//! One difference: a segment without data goes up as it came (after what
//! is held of its connection), where `tcp_lro_condense` folds an ACK that
//! advances into the held frame. Folded, the ACKs of a connection this
//! side sends on made the peer see holes in what was sent: on the X870E
//! to the build server, 10,900 retransmissions in 5 s and 365 Mbit/s,
//! against 60 and 2 Gbit/s unfolded (2026-11-27; why stock does not
//! suffer it is not known yet).
//!
//! FreeBSD's drivers flush at the end of each receive pass, a pass of
//! about a dozen frames behind the device's interrupt moderation. A driver
//! that polls in short rounds instead keeps the [`Lro`] across them and
//! flushes by age, as `tcp_lro_flush_inactive` lets one (mlx5's
//! `lro_timeout`): [`Lro::flush_round`] sends up what holds only ACKs,
//! what carries a PSH, and what is older than the hold time,
//! [`Lro::flush_all`] the rest at the end.
//!
//! The merged frame's headers are rewritten whole: IP length and
//! checksum, the last ACK, window and timestamps, and a TCP checksum built
//! from each segment's own (its data's sum is its checksum less its
//! header's and the pseudo-header's, as `tcp_lro_rx_csum_data` finds it),
//! so `bpf` sees a frame that checks.
//!
//! Behavior reference: sys/netinet/tcp_lro.c (release/15.1.0).

use alloc::sync::Arc;
use alloc::vec::Vec;

use mbuf::Packet;
use mbuf::{csum, flags as mflags};
use sync::Once;

use crate::Vnet;

/// Connections held at once (`TCP_LRO_ENTRIES`).
pub const ENTRIES: usize = 8;

const ETHER_HLEN: usize = 14;
const ETHERTYPE_IP: [u8; 2] = [0x08, 0x00];
const IP_HLEN: usize = 20;
const PROTO_TCP: u8 = 6;
const TH_SYN: u8 = 0x02;
const TH_PUSH: u8 = 0x08;
const TH_ACK: u8 = 0x10;
/// NOP, NOP, timestamp (kind 8, length 10): the one option layout that
/// merges (`TCP_LRO_TS_OPTION`).
const TS_OPTION: [u8; 4] = [1, 1, 8, 10];
const TS_OPTLEN: usize = 12;
/// The largest IPv4 datagram.
const IP_MAXPACKET: usize = 65_535;

/// Whether a VNET forwards IPv4, as the IP layer answers it.
static FORWARDING: Once<fn(&Vnet) -> bool> = Once::new();

/// Sets how [`allowed`] learns whether a VNET forwards (once, by IP).
pub fn set_forwarding_hook(f: fn(&Vnet) -> bool) {
    let _ = FORWARDING.set(f);
}

/// Whether frames received in `vnet` may merge: not while it forwards.
#[must_use]
pub fn allowed(vnet: &Arc<Vnet>) -> bool {
    !FORWARDING.get().is_some_and(|f| f(vnet))
}

/// One connection's direction, as its frames carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Flow {
    src: [u8; 4],
    dst: [u8; 4],
    ports: [u8; 4],
}

/// A frame's headers, read.
#[derive(Clone, Copy, Debug)]
struct Seg {
    flow: Flow,
    tos: u8,
    /// TCP header length, options included.
    thlen: usize,
    data_len: usize,
    seq: u32,
    ack: u32,
    win: u16,
    flags: u8,
    /// `TSval`, `TSecr`, if the options are just the timestamp.
    ts: Option<(u32, u32)>,
    /// Options other than a lone timestamp.
    odd_options: bool,
    /// The sum of the data alone (ones' complement, unfolded).
    data_sum: u32,
}

/// A connection's frame being built.
#[derive(Debug)]
struct Entry {
    flow: Flow,
    m: Packet,
    tos: u8,
    thlen: usize,
    /// Data bytes held, and their sum.
    data_len: usize,
    data_sum: u32,
    next_seq: u32,
    ack: u32,
    win: u16,
    flags: u8,
    ts: Option<(u32, u32)>,
    nsegs: u16,
    /// Something was folded in: the headers need rewriting.
    merged: bool,
    /// When it was started (ns), for [`Lro::flush_round`].
    born: u64,
}

/// A receive pass's merging (`struct lro_ctrl`).
#[derive(Debug, Default)]
pub struct Lro {
    entries: Vec<Entry>,
    /// Frames merged into others, for the driver's counters.
    pub merged: u64,
    /// Merged frames sent up (`lro_flushed`).
    pub flushed: u64,
}

impl Lro {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            merged: 0,
            flushed: 0,
        }
    }

    /// Takes received frame `m` (`tcp_lro_rx`): held to merge, or sent up
    /// through `up` (after what is held of its connection) when it cannot
    /// be.
    pub fn rx(&mut self, mut m: Packet, now: u64, up: &mut impl FnMut(Packet)) {
        let Some(s) = parse(&mut m) else {
            up(m);
            return;
        };
        let at = self.entries.iter().position(|e| e.flow == s.flow);
        if s.flags & !(TH_ACK | TH_PUSH) != 0 || s.odd_options || s.data_len == 0 {
            // FIN, RST, URG, a SACK, an ACK alone: up in order, unmerged.
            if let Some(i) = at {
                self.flush(i, up);
            }
            up(m);
            return;
        }
        let Some(i) = at else {
            self.start(m, &s, now, up);
            return;
        };
        let e = &mut self.entries[i];
        if s.data_len == 0 && seq_lt(s.ack, e.ack) {
            // An ACK older than one held: nothing in it (`m_freem`).
            return;
        }
        let ts_ok = match (e.ts, s.ts) {
            (Some((held, _)), Some((tsval, _))) => !seq_lt(tsval, held),
            (None, None) => true,
            _ => false,
        };
        let dup_ack = s.data_len == 0 && s.ack == e.ack && s.win == e.win;
        let fits =
            IP_HLEN + e.thlen + e.data_len + s.data_len <= IP_MAXPACKET && e.nsegs < u16::MAX;
        if s.seq != e.next_seq || !ts_ok || dup_ack || s.tos != e.tos || !fits {
            // Out of order, a duplicate ACK, or full: what is held goes up
            // and this frame starts anew (`tcp_push_and_replace`).
            self.flush(i, up);
            self.start(m, &s, now, up);
            return;
        }
        if s.data_len != 0 || seq_lt(e.ack, s.ack) {
            e.ack = s.ack;
            e.win = s.win;
        } else if s.ack == e.ack && s.win > e.win {
            e.win = s.win;
        }
        if s.ts.is_some() {
            e.ts = s.ts;
        }
        e.flags |= s.flags & TH_PUSH;
        e.merged = true;
        self.merged += 1;
        if s.data_len == 0 {
            return;
        }
        // The data's sum lands at an odd offset byte-swapped.
        let sum = if e.data_len % 2 == 1 {
            swap16(fold(s.data_sum))
        } else {
            s.data_sum
        };
        e.data_sum = add(e.data_sum, sum);
        e.data_len += s.data_len;
        e.next_seq = e.next_seq.wrapping_add(len32(s.data_len));
        e.nsegs += 1;
        m.data.trim_front(ETHER_HLEN + IP_HLEN + s.thlen);
        e.m.data.append(m.data);
    }

    /// Holds `m`, read as `s`, as a new connection's first frame; the
    /// oldest held goes up if all are taken.
    fn start(&mut self, m: Packet, s: &Seg, now: u64, up: &mut impl FnMut(Packet)) {
        if self.entries.len() == ENTRIES {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.born)
                .map_or(0, |(i, _)| i);
            self.flush(oldest, up);
        }
        self.entries.push(Entry {
            flow: s.flow,
            m,
            tos: s.tos,
            thlen: s.thlen,
            data_len: s.data_len,
            data_sum: s.data_sum,
            next_seq: s.seq.wrapping_add(len32(s.data_len)),
            ack: s.ack,
            win: s.win,
            flags: s.flags,
            ts: s.ts,
            nsegs: 1,
            merged: false,
            born: now,
        });
    }

    /// Sends up what holds no data (ACKs wait for nothing), what carries a
    /// PSH (its sender has nothing more for now: a request waits for no
    /// more segments), and what was started `hold` ns or more before
    /// `now`.
    pub fn flush_round(&mut self, now: u64, hold: u64, up: &mut impl FnMut(Packet)) {
        let mut i = 0;
        while i < self.entries.len() {
            let e = &self.entries[i];
            if e.data_len == 0 || e.flags & TH_PUSH != 0 || now.saturating_sub(e.born) >= hold {
                self.flush(i, up);
            } else {
                i += 1;
            }
        }
    }

    /// Sends up everything held (`tcp_lro_flush_all`).
    pub fn flush_all(&mut self, up: &mut impl FnMut(Packet)) {
        while !self.entries.is_empty() {
            self.flush(self.entries.len() - 1, up);
        }
    }

    /// Whether anything is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn flush(&mut self, i: usize, up: &mut impl FnMut(Packet)) {
        let e = self.entries.swap_remove(i);
        if !e.merged {
            up(e.m);
            return;
        }
        self.flushed += 1;
        up(finish(e));
    }
}

/// The held frame with its headers rewritten for all it carries
/// (`tcp_flush_out_entry`, `tcp_lro_update_checksum`).
fn finish(mut e: Entry) -> Packet {
    let mut h = [0u8; IP_HLEN + 60];
    let hl = IP_HLEN + e.thlen;
    e.m.data.copy_out(ETHER_HLEN, &mut h[..hl]);
    let total = hl + e.data_len;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "checked below 65,536 when merged"
    )]
    h[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    h[10..12].fill(0);
    let ipsum = !fold(sum16(&h[..IP_HLEN]));
    h[10..12].copy_from_slice(&ipsum.to_be_bytes());
    let th = &mut h[IP_HLEN..hl];
    th[8..12].copy_from_slice(&e.ack.to_be_bytes());
    th[13] = e.flags;
    th[14..16].copy_from_slice(&e.win.to_be_bytes());
    if let Some((tsval, tsecr)) = e.ts {
        th[24..28].copy_from_slice(&tsval.to_be_bytes());
        th[28..32].copy_from_slice(&tsecr.to_be_bytes());
    }
    th[16..18].fill(0);
    let tcp_len = e.thlen + e.data_len;
    let mut src = [0u8; 4];
    let mut dst = [0u8; 4];
    src.copy_from_slice(&h[12..16]);
    dst.copy_from_slice(&h[16..20]);
    let s = add(
        add(pseudo(src, dst, tcp_len), sum16(&h[IP_HLEN..hl])),
        e.data_sum,
    );
    let thsum = !fold(s);
    h[IP_HLEN + 16..IP_HLEN + 18].copy_from_slice(&thsum.to_be_bytes());
    // The headers are in the frame's first mbuf, whose length they had.
    let _ = e.m.data.copy_in(ETHER_HLEN, &h[..hl]);
    e.m.hdr.csum_flags |= csum::IP_CHECKED | csum::IP_VALID | csum::DATA_VALID | csum::PSEUDO_HDR;
    e.m.hdr.csum_data = 0xffff;
    e.m.hdr.lro_nsegs = e.nsegs;
    e.m
}

/// Reads frame `m`'s headers if it is a TCP segment that could merge, its
/// Ethernet padding cut off.
fn parse(m: &mut Packet) -> Option<Seg> {
    let f = m.hdr.csum_flags;
    if f & (csum::DATA_VALID | csum::PSEUDO_HDR) != csum::DATA_VALID | csum::PSEUDO_HDR
        || m.hdr.csum_data != 0xffff
        || f & csum::IP_CHECKED == 0
        || f & csum::IP_VALID == 0
        || m.hdr.flags & mflags::VLANTAG != 0
    {
        return None;
    }
    let b = m.data.first()?;
    if b.len() < ETHER_HLEN + IP_HLEN + 20 || b[12..14] != ETHERTYPE_IP {
        return None;
    }
    let ip = &b[ETHER_HLEN..];
    // IPv4, no options, not a fragment, TCP.
    if ip[0] != 0x45 || u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 || ip[9] != PROTO_TCP {
        return None;
    }
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    let th = &ip[IP_HLEN..];
    let thlen = usize::from(th[12] >> 4) * 4;
    if thlen < 20 || total < IP_HLEN + thlen || th.len() < thlen {
        return None;
    }
    let flags = th[13];
    if flags & TH_SYN != 0 {
        return None;
    }
    let opts = &th[20..thlen];
    let (ts, odd_options) = match opts.len() {
        0 => (None, false),
        TS_OPTLEN if opts[..4] == TS_OPTION => {
            (Some((be32(&opts[4..8]), be32(&opts[8..12]))), false)
        }
        _ => (None, true),
    };
    let mut src = [0u8; 4];
    let mut dst = [0u8; 4];
    let mut ports = [0u8; 4];
    src.copy_from_slice(&ip[12..16]);
    dst.copy_from_slice(&ip[16..20]);
    ports.copy_from_slice(&th[..4]);
    let data_len = total - IP_HLEN - thlen;
    // The data's sum: the segment's checksum less its header's and the
    // pseudo-header's (`tcp_lro_rx_csum_data`); the header with its
    // checksum field counts that field, which the complement cancels.
    let whole = !u16::from_be_bytes([th[16], th[17]]);
    let hdr = sum16(&th[..16]) + sum16(&th[18..thlen]);
    let data_sum = sub(
        sub(u32::from(whole), pseudo(src, dst, thlen + data_len)),
        hdr,
    );
    let seg = Seg {
        flow: Flow { src, dst, ports },
        tos: ip[1],
        thlen,
        data_len,
        seq: be32(&th[4..8]),
        ack: be32(&th[8..12]),
        win: u16::from_be_bytes([th[14], th[15]]),
        flags,
        ts,
        odd_options,
        data_sum,
    };
    // Ethernet pads a short frame: not part of the segment.
    let end = ETHER_HLEN + total;
    if m.len() < end {
        return None;
    }
    if m.len() > end {
        let _ = m.data.split_off(end);
    }
    Some(seg)
}

/// A segment's data length (below 65,536: it came in one IPv4 datagram)
/// as sequence space.
#[allow(clippy::cast_possible_truncation, reason = "within one datagram")]
const fn len32(n: usize) -> u32 {
    n as u32
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// `a` before `b` in sequence space (`SEQ_LT`, `TSTMP_LT`).
#[allow(clippy::cast_possible_wrap, reason = "serial-number arithmetic")]
const fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// The ones' complement sum of `b` as big-endian 16-bit words, unfolded.
fn sum16(b: &[u8]) -> u32 {
    let (words, rest) = b.as_chunks::<2>();
    let mut s = words
        .iter()
        .map(|w| u32::from(u16::from_be_bytes(*w)))
        .sum::<u32>();
    if let [last] = rest {
        s += u32::from(*last) << 8;
    }
    s
}

/// The pseudo-header's sum for a TCP segment of `len` bytes.
fn pseudo(src: [u8; 4], dst: [u8; 4], len: usize) -> u32 {
    #[allow(clippy::cast_possible_truncation, reason = "a datagram's length")]
    let len = len as u32;
    sum16(&src) + sum16(&dst) + u32::from(PROTO_TCP) + (len >> 16) + (len & 0xffff)
}

#[allow(clippy::cast_possible_truncation, reason = "folded to 16 bits")]
fn fold(mut s: u32) -> u16 {
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    s as u16
}

fn add(a: u32, b: u32) -> u32 {
    u32::from(fold(a)) + u32::from(fold(b))
}

/// `a - b` in ones' complement.
fn sub(a: u32, b: u32) -> u32 {
    u32::from(fold(a)) + u32::from(!fold(b))
}

const fn swap16(v: u16) -> u32 {
    v.swap_bytes() as u32
}
