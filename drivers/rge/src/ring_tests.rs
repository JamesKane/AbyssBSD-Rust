// SPDX-License-Identifier: BSD-2-Clause
//! The rings against a simulated controller on the other side of the
//! `OWN` bits.

#![allow(
    clippy::cast_possible_truncation,
    reason = "test lengths and addresses are small"
)]

extern crate std;

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::ring::{
    self, Assembled, Assembler, Barriers, DESCS, RxCsum, RxRing, SLOT, TxFull, TxRing, Words, rx,
    tx,
};

pub(crate) struct Mem(RefCell<Vec<[u32; 8]>>);

impl Mem {
    pub(crate) fn new() -> Self {
        Self(RefCell::new(vec![[0; 8]; DESCS]))
    }
}

impl Words for Mem {
    fn get(&self, d: usize, w: usize) -> u32 {
        self.0.borrow()[d][w]
    }
    fn set(&self, d: usize, w: usize, v: u32) {
        self.0.borrow_mut()[d][w] = v;
    }
}

struct NoBarriers;

impl Barriers for NoBarriers {
    fn write(&self) {}
    fn read(&self) {}
}

const B: NoBarriers = NoBarriers;

/// The controller returning descriptor `d` with `len` bytes.
fn complete_rx(m: &Mem, d: usize, len: u32, flags: u32, ext: u32) {
    assert_ne!(m.get(d, rx::CMDSTS) & rx::OWN, 0, "not posted");
    m.set(d, rx::EXTSTS, ext);
    m.set(d, rx::CMDSTS, flags | len);
}

#[test]
fn rx_posts_every_slot() {
    let m = Mem::new();
    let mut r = RxRing::new();
    r.fill(&m, &B, |i| 0x1000_0000 + (i * SLOT) as u64);
    for d in [0, 1, DESCS - 1] {
        let cmd = m.get(d, rx::CMDSTS);
        assert_eq!(cmd & rx::OWN, rx::OWN);
        assert_eq!(cmd & rx::FRAGLEN, SLOT as u32);
        assert_eq!(cmd & rx::EOR != 0, d == DESCS - 1);
        assert_eq!(m.get(d, rx::ADDR), 0x1000_0000 + (d * SLOT) as u32);
    }
    // Nothing returned yet.
    assert_eq!(r.take(&m, &B), None);
}

#[test]
fn frames_reassemble() {
    let m = Mem::new();
    let mut r = RxRing::new();
    r.fill(&m, &B, |i| (i * SLOT) as u64);
    let mut a = Assembler::default();
    // A one-piece frame of 64 bytes and its 4-byte CRC, TCP over IPv4
    // with good checksums.
    complete_rx(&m, 0, 68, rx::SOF | rx::EOF, rx::IPV4 | rx::TCPPKT);
    let p = r.take(&m, &B).unwrap();
    assert_eq!((p.slot, p.len, p.first, p.last), (0, 68, true, true));
    let Assembled::Frame(f, c) = a.add(&p, &[7; 68]) else {
        panic!()
    };
    assert_eq!(f.len(), 64);
    assert_eq!(
        c,
        RxCsum {
            ip_checked: true,
            ip_valid: true,
            l4_valid: true
        }
    );
    RxRing::post(&m, &B, p.slot, 0);
    // A jumbo frame over three slots.
    complete_rx(&m, 1, SLOT as u32, rx::SOF, 0);
    complete_rx(&m, 2, SLOT as u32, 0, 0);
    complete_rx(&m, 3, 1004, rx::EOF, rx::IPV4 | rx::UDPPKT | rx::UDPCSUMERR);
    let mut out = None;
    while let Some(p) = r.take(&m, &B) {
        match a.add(&p, &vec![1; p.len]) {
            Assembled::More => {}
            other => out = Some(other),
        }
    }
    let Some(Assembled::Frame(f, c)) = out else {
        panic!()
    };
    assert_eq!(f.len(), 2 * SLOT + 1000);
    assert!(!c.l4_valid, "a bad UDP checksum is not vouched for");
}

#[test]
fn bad_sequences_are_dropped() {
    let mut a = Assembler::default();
    let piece = |first, last, error| ring::RxPiece {
        slot: 0,
        len: 100,
        first,
        last,
        error,
        ext: 0,
    };
    // A continuation with no start.
    assert_eq!(
        a.add(&piece(false, true, false), &[0; 100]),
        Assembled::Dropped
    );
    // A CRC error.
    assert_eq!(
        a.add(&piece(true, true, true), &[0; 100]),
        Assembled::Dropped
    );
    // A frame cut short by the next start: the open one is dropped, the
    // new one goes on.
    assert_eq!(
        a.add(&piece(true, false, false), &[0; 100]),
        Assembled::More
    );
    assert_eq!(
        a.add(&piece(true, false, false), &[0; 100]),
        Assembled::Dropped
    );
    assert!(
        matches!(a.add(&piece(false, true, false), &[0; 100]), Assembled::Frame(f, _) if f.len() == 196)
    );
    // A length past the slot is clamped and marked bad.
    let m = Mem::new();
    let mut r = RxRing::new();
    r.fill(&m, &B, |_| 0);
    complete_rx(&m, 0, 0x3000, rx::SOF | rx::EOF, 0);
    let p = r.take(&m, &B).unwrap();
    assert_eq!((p.len, p.error), (SLOT, true));
}

#[test]
fn tx_hands_over_the_first_descriptor_last() {
    let m = Mem::new();
    let mut t = TxRing::new();
    t.init(&m);
    assert_eq!(m.get(DESCS - 1, tx::CMDSTS), tx::EOR);
    let first = t
        .push(&m, &B, &[(0x1000, 60), (0x2000, 40), (0x3000, 20)], 0, 0)
        .unwrap();
    assert_eq!(first, 0);
    let c = |d| m.get(d, tx::CMDSTS);
    assert_eq!(c(0), tx::OWN | tx::SOF | 60);
    assert_eq!(c(1), tx::OWN | 40);
    assert_eq!(c(2), tx::OWN | tx::EOF | 20);
    assert_eq!(t.free(), DESCS - 1 - 3);
    // Not done until the last descriptor is.
    m.set(0, tx::CMDSTS, c(0) & !tx::OWN);
    assert_eq!(t.reap(&m, &B), None);
    m.set(1, tx::CMDSTS, c(1) & !tx::OWN);
    m.set(2, tx::CMDSTS, c(2) & !tx::OWN);
    assert_eq!(t.reap(&m, &B), Some((0, true)));
    assert_eq!(t.free(), DESCS - 1);
    assert_eq!(t.in_flight(), 0);
}

#[test]
fn tx_wraps_and_fills() {
    let m = Mem::new();
    let mut t = TxRing::new();
    t.init(&m);
    // Fill to one short of the ring, one descriptor each.
    for i in 0..DESCS - 1 {
        t.push(&m, &B, &[(i as u64 * 64, 64)], 0, 0).unwrap();
    }
    assert_eq!(t.free(), 0);
    assert_eq!(t.push(&m, &B, &[(0, 64)], 0, 0), Err(TxFull::Full));
    // The last descriptor carries the end-of-ring mark with its frame.
    assert_eq!(
        m.get(DESCS - 2, tx::CMDSTS) & tx::EOR,
        0,
        "only the last descriptor ends the ring"
    );
    // The controller sends the first two; one fails.
    m.set(0, tx::CMDSTS, tx::SOF | tx::EOF | 64);
    m.set(1, tx::CMDSTS, tx::SOF | tx::EOF | tx::TXERR | 64);
    assert_eq!(t.reap(&m, &B), Some((0, true)));
    assert_eq!(t.reap(&m, &B), Some((1, false)));
    assert_eq!(t.reap(&m, &B), None);
    // A frame now wraps from the last descriptor to the first.
    let first = t.push(&m, &B, &[(0xa, 10), (0xb, 20)], 0, 0).unwrap();
    assert_eq!(first, DESCS - 1);
    assert_eq!(
        m.get(DESCS - 1, tx::CMDSTS),
        tx::OWN | tx::SOF | tx::EOR | 10
    );
    assert_eq!(m.get(0, tx::CMDSTS), tx::OWN | tx::EOF | 20);
    assert_eq!(t.push(&m, &B, &[], 0, 0), Err(TxFull::BadShape));
}

/// The checksums the stack leaves to the controller become extended
/// status bits; the IPv4 header's always comes with a transport one.
#[test]
fn tx_csum_ext() {
    use mbuf::csum;
    assert_eq!(tx::csum_ext(0), 0);
    assert_eq!(tx::csum_ext(csum::IP), tx::IPCSUM);
    assert_eq!(
        tx::csum_ext(csum::IP | csum::IP_TCP),
        tx::IPCSUM | tx::TCPCSUM
    );
    assert_eq!(tx::csum_ext(csum::IP_UDP), tx::IPCSUM | tx::UDPCSUM);
    // Neither IPv6's nor a finished checksum's flags ask for anything.
    assert_eq!(tx::csum_ext(csum::IP_CHECKED | csum::IP_VALID), 0);
}

/// TSO's words: the command word names the mode and the TCP header's
/// offset (bits 24:18), the extended status the segment size (bits 28:18),
/// and a frame the controller cannot take is refused.
#[test]
fn tx_tso_words() {
    // An Ethernet header and a 20-byte IPv4 header: the TCP header at 34.
    assert_eq!(
        tx::tso4(34, 1448),
        Some((0x0400_0000 | (34 << 18), 1448 << 18))
    );
    // The largest segment size, and the farthest header offset.
    assert_eq!(
        tx::tso4(127, 0x7ff),
        Some((0x0400_0000 | (127 << 18), 0x7ff << 18))
    );
    assert_eq!(tx::tso4(128, 1448), None);
    assert_eq!(tx::tso4(34, 0x800), None);
    assert_eq!(tx::tso4(34, 0), None);
    // The offset field stays inside bits 24:18 and the mode bits apart.
    let (cmd, ext) = tx::tso4(127, 0x7ff).unwrap();
    assert_eq!(cmd & !(tx::TSO4 | (0x7f << 18)), 0);
    assert_eq!(ext & !(0x7ff << 18), 0);
}

/// A TSO frame's words go in every descriptor, and its completion is not
/// read as an error by the offset bits that sit where the error bit does.
#[test]
fn tx_tso_every_descriptor() {
    let m = Mem::new();
    let mut t = TxRing::new();
    t.init(&m);
    let (cmd, ext) = tx::tso4(34, 1448).unwrap();
    t.push(
        &m,
        &B,
        &[(0x1000, 4096), (0x2000, 4096), (0x3000, 100)],
        ext,
        cmd,
    )
    .unwrap();
    for d in 0..3 {
        assert_eq!(m.get(d, tx::EXTSTS), ext, "descriptor {d}");
        assert_eq!(
            m.get(d, tx::CMDSTS) & (tx::TSO4 | (0x7f << 18)),
            cmd,
            "descriptor {d}"
        );
    }
    assert_ne!(m.get(0, tx::CMDSTS) & tx::SOF, 0);
    assert_ne!(m.get(2, tx::CMDSTS) & tx::EOF, 0);
    // Offset 34 has bit 5 set: the error bit's place. Completed with the
    // command bits still there, it is a good frame.
    let done = m.get(2, tx::CMDSTS) & !tx::OWN;
    m.set(2, tx::CMDSTS, done);
    m.set(0, tx::CMDSTS, m.get(0, tx::CMDSTS) & !tx::OWN);
    assert_eq!(t.reap(&m, &B), Some((0, true)));
}

/// The next frame starts where the last one ended.
#[test]
fn tx_next() {
    let m = Mem::new();
    let mut t = TxRing::new();
    t.init(&m);
    assert_eq!(t.next(), 0);
    t.push(&m, &B, &[(0xa, 10), (0xb, 20)], 0, 0).unwrap();
    assert_eq!(t.next(), 2);
}

/// A UDP datagram of 4 bytes (iperf3's hello), its checksums left to the
/// device: both come out right, as a receiver checks them.
#[test]
fn short_csum_udp() {
    use mbuf::csum;
    let sum = |b: &[u8]| -> u32 {
        let mut s: u32 = b
            .chunks(2)
            .map(|w| u32::from(u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)])))
            .sum();
        while s > 0xffff {
            s = (s & 0xffff) + (s >> 16);
        }
        s
    };
    let (src, dst) = ([192, 168, 0, 19], [192, 168, 0, 14]);
    let mut f = vec![0u8; 46];
    f[12..14].copy_from_slice(&[0x08, 0x00]);
    f[14..24].copy_from_slice(&[0x45, 0, 0, 32, 0x12, 0x34, 0x40, 0, 64, 17]);
    f[26..30].copy_from_slice(&src);
    f[30..34].copy_from_slice(&dst);
    f[34..42].copy_from_slice(&[0x9c, 0x40, 0x14, 0x51, 0, 12, 0, 0]);
    f[42..46].copy_from_slice(&[0x39, 0x38, 0x37, 0x36]);
    // The stack's seed: the pseudo-header's folded sum.
    let mut pseudo = Vec::new();
    pseudo.extend_from_slice(&src);
    pseudo.extend_from_slice(&dst);
    pseudo.extend_from_slice(&[0, 17, 0, 12]);
    let seed = sum(&pseudo) as u16;
    f[40..42].copy_from_slice(&seed.to_be_bytes());
    ring::short_csum(&mut f, csum::IP | csum::IP_UDP, 6);
    assert_eq!(sum(&f[14..34]), 0xffff, "IPv4 header checksum");
    let mut check = pseudo.clone();
    check.extend_from_slice(&f[34..46]);
    assert_eq!(sum(&check), 0xffff, "UDP checksum");
    // Not IPv4: untouched.
    let mut arp = vec![0u8; 42];
    arp[12..14].copy_from_slice(&[0x08, 0x06]);
    let before = arp.clone();
    ring::short_csum(&mut arp, csum::IP | csum::IP_UDP, 6);
    assert_eq!(arp, before);
}
