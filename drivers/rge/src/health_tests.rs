// SPDX-License-Identifier: BSD-2-Clause
//! The tick's decisions, and the watchdog against the register model and
//! a simulated transmitter: a stalled ring is declared after five ticks
//! without a completion, the chip is reinitialized, and frames flow
//! again.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the model's registers are read back at their own widths"
)]

extern crate std;

use alloc::string::String;
use alloc::vec::Vec;

use crate::health::{self, RxWatch, TX_TIMEOUT_TICKS, TxWatchdog};
use crate::hw::{Chip, HwError, MacType, Regs};
use crate::regs;
use crate::ring::{self, DESCS, RxRing, SLOT, TxRing, Words, rx, tx};
use crate::ring_tests::Mem;
use crate::tests::Model;

#[test]
fn watchdog_counts_down_from_five() {
    let mut wd = TxWatchdog::default();
    assert!(!wd.tick(), "idle: nothing to time");
    wd.queued();
    assert!(wd.armed());
    for _ in 1..TX_TIMEOUT_TICKS {
        assert!(!wd.tick());
    }
    assert!(wd.tick(), "the fifth second without a completion");
    assert!(!wd.armed(), "it fires once");
    assert!(!wd.tick());
}

#[test]
fn watchdog_rearms_on_progress_only() {
    let mut wd = TxWatchdog::default();
    wd.queued();
    for _ in 0..3 {
        wd.tick();
    }
    // New frames do not hold off a stalled ring.
    wd.queued();
    wd.reaped(false, true);
    wd.tick();
    assert!(wd.tick(), "five ticks since the countdown began");
    // A completion with frames still out starts it over.
    wd.queued();
    for _ in 0..4 {
        wd.tick();
    }
    wd.reaped(true, true);
    for _ in 1..TX_TIMEOUT_TICKS {
        assert!(!wd.tick());
    }
    assert!(wd.tick());
    // An empty ring disarms it.
    wd.queued();
    wd.reaped(true, false);
    assert!(!wd.armed());
    for _ in 0..10 {
        assert!(!wd.tick());
    }
    wd.queued();
    wd.clear();
    assert!(!wd.armed());
}

#[test]
fn rx_stall_needs_two_ticks_without_progress() {
    let mut w = RxWatch::default();
    // Nothing returned.
    assert_eq!(w.check(None, 0), None);
    // A frame at the head: maybe its interrupt is on the way.
    assert_eq!(w.check(Some(7), 10), None);
    // Taken meanwhile (the same index after a lap is still progress).
    assert_eq!(w.check(Some(7), 1034), None);
    // A whole tick with it waiting and nothing taken.
    assert_eq!(w.check(Some(7), 1034), Some(1));
    // The caller took it; the next one starts afresh.
    assert_eq!(w.check(Some(8), 1035), None);
    assert_eq!(w.check(Some(8), 1035), Some(2));
    w.clear();
    assert_eq!(w.check(Some(8), 1035), None);
}

#[test]
fn reports_thin_out() {
    let shown: Vec<u64> = (1..=20).filter(|&n| health::report(n)).collect();
    assert_eq!(shown, [1, 2, 4, 8, 16]);
}

#[test]
fn all_ones_is_gone() {
    assert!(health::responding(0x64a0_0000));
    assert!(health::responding(0));
    assert!(!health::responding(u32::MAX));
    let m = Model::default();
    m.write32(regs::TXCFG, 0x64a0_0000);
    let c = Chip::new(&m, MacType::R26_2);
    assert!(c.responding());
    m.0.borrow_mut().gone = true;
    assert!(!c.responding());
    // What the interrupt thread acknowledges reads all ones too, which it
    // must not take for work (it would loop forever).
    assert!(!health::responding(c.ack()));
    // A reinitialization of a gone chip ends, in error, rather than wait.
    let r = c.init_locked([2, 0, 0, 0, 0, 1], 0, 0, true, || {
        panic!("the rings are not touched before the chip stops")
    });
    assert_eq!(r, Err(HwError::ResetTimeout));
}

const LLADDR: [u8; 6] = [0xc0, 0x74, 0x2b, 0xf9, 0x42, 0x34];
const RX_BUS: u64 = 0x8000_0000;
const TX_BUS: u64 = 0x8001_0000;

/// The transmitter on the other side of the `OWN` bits: completes what it
/// owns, in order, unless stalled. A soft reset clears a stall and starts
/// it again at the ring's first descriptor.
#[derive(Default)]
struct Transmitter {
    next: usize,
    resets: u32,
    stalled: bool,
}

impl Transmitter {
    fn run(&mut self, model: &Model, m: &Mem) -> usize {
        let (resets, cmd) = {
            let s = model.0.borrow();
            (s.resets, s.get(regs::CMD, 1) as u8)
        };
        if resets != self.resets {
            self.resets = resets;
            self.next = 0;
            self.stalled = false;
        }
        if self.stalled || cmd & regs::CMD_TXENB == 0 {
            return 0;
        }
        let mut n = 0;
        loop {
            let c = m.get(self.next, tx::CMDSTS);
            if c & tx::OWN == 0 {
                return n;
            }
            m.set(self.next, tx::CMDSTS, c & !tx::OWN);
            self.next = (self.next + 1) % DESCS;
            n += 1;
        }
    }
}

/// The driver's rings, watchdog, and tick over the model (what
/// `driver::Shared` does with DMA memory and locks).
struct Nic<'a> {
    chip: Chip<&'a Model>,
    txm: Mem,
    rxm: Mem,
    tx: TxRing,
    rx: RxRing,
    wd: TxWatchdog,
    rxw: RxWatch,
    taken: u64,
    sent: u64,
    oerrors: u64,
    log: Vec<String>,
}

impl<'a> Nic<'a> {
    fn new(model: &'a Model) -> Self {
        let mut n = Self {
            chip: Chip::new(model, MacType::R26_2),
            txm: Mem::new(),
            rxm: Mem::new(),
            tx: TxRing::new(),
            rx: RxRing::new(),
            wd: TxWatchdog::default(),
            rxw: RxWatch::default(),
            taken: 0,
            sent: 0,
            oerrors: 0,
            log: Vec::new(),
        };
        n.init().unwrap();
        n
    }

    /// `rge_init_locked`, as `Shared::reinit`.
    fn init(&mut self) -> Result<(), HwError> {
        let Self {
            chip,
            txm,
            rxm,
            tx,
            rx,
            wd,
            rxw,
            ..
        } = self;
        wd.clear();
        rxw.clear();
        chip.init_locked(LLADDR, RX_BUS, TX_BUS, true, || {
            tx.init(txm);
            rx.fill(rxm, &ring::Hal, |s| RX_BUS + (s * SLOT) as u64);
        })?;
        chip.set_filter(false, [0; 2]);
        chip.start();
        Ok(())
    }

    fn send(&mut self) -> bool {
        let bus = 0x9000_0000 + self.sent * 0x1000;
        if self
            .tx
            .push(&self.txm, &ring::Hal, &[(bus, 60)], 0)
            .is_err()
        {
            return false;
        }
        self.sent += 1;
        self.wd.queued();
        self.chip.kick_tx();
        true
    }

    /// `Tx::reap`; frames completed.
    fn reap(&mut self) -> usize {
        let mut n = 0;
        while let Some((_, ok)) = self.tx.reap(&self.txm, &ring::Hal) {
            assert!(ok);
            n += 1;
        }
        self.wd.reaped(n > 0, self.tx.in_flight() > 0);
        n
    }

    /// `Shared::tick`'s decisions.
    fn tick(&mut self) {
        if !self.chip.responding() {
            self.log.push(health::MSG_GONE.into());
            return;
        }
        self.reap();
        let fire = self.wd.tick();
        let stall = self.rxw.check(self.rx.ready(&self.rxm), self.taken);
        if fire {
            self.log.push(health::MSG_TX_TIMEOUT.into());
            self.oerrors += 1;
            if let Err(e) = self.init() {
                self.log
                    .push(alloc::format!("{}: {e:?}", health::MSG_INIT_FAILED));
            }
        } else if let Some(n) = stall {
            self.log
                .push(alloc::format!("{} ({n} so far)", health::MSG_RX_STALL));
            while self.rx.take(&self.rxm, &ring::Hal).is_some() {
                self.taken += 1;
            }
        }
    }
}

/// A transmitter that stops completing frames is declared after five
/// ticks, as FreeBSD's `rge0: [ERR] TX timeout (watchdog)`, the chip is
/// reinitialized with an empty ring, and frames flow again.
#[test]
fn tx_stall_is_reset_and_frames_flow_again() {
    let model = Model::default();
    let mut nic = Nic::new(&model);
    let mut hw = Transmitter::default();
    let resets = model.0.borrow().resets;
    // Healthy: sent, completed, reaped; the watchdog disarms.
    for _ in 0..3 {
        assert!(nic.send());
    }
    assert_eq!(hw.run(&model, &nic.txm), 3);
    assert_eq!(nic.reap(), 3);
    assert!(!nic.wd.armed());
    for _ in 0..10 {
        nic.tick();
    }
    assert!(nic.log.is_empty(), "{:?}", nic.log);

    // The transmitter wedges with frames on it.
    hw.stalled = true;
    assert!(nic.send() && nic.send());
    for t in 1..TX_TIMEOUT_TICKS {
        // Frames keep coming; they do not hold the watchdog off.
        assert!(nic.send());
        assert_eq!(hw.run(&model, &nic.txm), 0);
        nic.tick();
        assert!(nic.log.is_empty(), "fired early, at tick {t}");
    }
    assert_eq!(nic.tx.in_flight(), 6);
    nic.tick();
    assert_eq!(nic.log, [health::MSG_TX_TIMEOUT]);
    assert_eq!(health::MSG_TX_TIMEOUT, "[ERR] TX timeout (watchdog)");
    assert_eq!(nic.oerrors, 1);

    // Reinitialized: reset, the rings reprogrammed and empty, the
    // transmitter and receiver on, interrupts unmasked.
    {
        let s = model.0.borrow();
        assert!(s.resets > resets, "no soft reset");
        assert_eq!(s.get(regs::TXDESC_ADDR_LO, 4), TX_BUS as u32);
        assert_eq!(s.get(regs::RXDESC_ADDR_LO, 4), RX_BUS as u32);
        assert_eq!(s.get(regs::CMD, 1) as u8, regs::CMD_TXENB | regs::CMD_RXENB);
        assert_eq!(s.get(regs::IMR, 4), regs::INTRS);
        assert_eq!(
            (s.get(regs::MAC0, 4), s.get(regs::MAC4, 4)),
            (0xf92b_74c0, 0x3442),
            "station address"
        );
    }
    assert_eq!(nic.tx.in_flight(), 0);
    assert_eq!(nic.tx.free(), DESCS - 1);
    assert!(!nic.wd.armed());
    for d in 0..DESCS {
        assert_eq!(nic.txm.get(d, tx::CMDSTS) & tx::OWN, 0);
        assert_ne!(nic.rxm.get(d, rx::CMDSTS) & rx::OWN, 0, "rx {d} not posted");
    }

    // Frames flow again from the ring's start, and nothing fires.
    for _ in 0..5 {
        assert!(nic.send());
    }
    assert_eq!(hw.run(&model, &nic.txm), 5);
    assert_eq!(nic.reap(), 5);
    for _ in 0..10 {
        nic.tick();
    }
    assert_eq!(nic.log, [health::MSG_TX_TIMEOUT]);
    // Across the ring's end, many times over.
    for _ in 0..3 * DESCS {
        assert!(nic.send());
        assert_eq!(hw.run(&model, &nic.txm), 1);
        assert_eq!(nic.reap(), 1);
    }
    nic.tick();
    assert_eq!(nic.log.len(), 1);
}

/// A received frame whose interrupt never comes is noticed on the second
/// tick and taken.
#[test]
fn rx_stall_is_noticed_and_taken() {
    let model = Model::default();
    let mut nic = Nic::new(&model);
    nic.tick();
    // The controller returns descriptor 0.
    nic.rxm.set(0, rx::CMDSTS, rx::SOF | rx::EOF | 68);
    nic.tick();
    assert!(nic.log.is_empty());
    nic.tick();
    assert_eq!(
        nic.log,
        [alloc::format!("{} (1 so far)", health::MSG_RX_STALL)]
    );
    assert_eq!(nic.taken, 1);
    assert_eq!(nic.rx.ready(&nic.rxm), None);
}

/// A device gone from the bus is reported and left alone.
#[test]
fn gone_device_is_reported() {
    let model = Model::default();
    let mut nic = Nic::new(&model);
    assert!(nic.send());
    model.0.borrow_mut().gone = true;
    let resets = model.0.borrow().resets;
    for _ in 0..10 {
        nic.tick();
    }
    assert_eq!(nic.log[0], health::MSG_GONE);
    assert!(
        !nic.log.iter().any(|l| l == health::MSG_TX_TIMEOUT),
        "no reset of a gone device"
    );
    assert_eq!(model.0.borrow().resets, resets);
}
