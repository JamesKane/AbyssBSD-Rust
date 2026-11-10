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

struct Mem(RefCell<Vec<[u32; 8]>>);

impl Mem {
    fn new() -> Self {
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
        .push(&m, &B, &[(0x1000, 60), (0x2000, 40), (0x3000, 20)], 0)
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
        t.push(&m, &B, &[(i as u64 * 64, 64)], 0).unwrap();
    }
    assert_eq!(t.free(), 0);
    assert_eq!(t.push(&m, &B, &[(0, 64)], 0), Err(TxFull::Full));
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
    let first = t.push(&m, &B, &[(0xa, 10), (0xb, 20)], 0).unwrap();
    assert_eq!(first, DESCS - 1);
    assert_eq!(
        m.get(DESCS - 1, tx::CMDSTS),
        tx::OWN | tx::SOF | tx::EOR | 10
    );
    assert_eq!(m.get(0, tx::CMDSTS), tx::OWN | tx::EOF | 20);
    assert_eq!(t.push(&m, &B, &[], 0), Err(TxFull::BadShape));
}
