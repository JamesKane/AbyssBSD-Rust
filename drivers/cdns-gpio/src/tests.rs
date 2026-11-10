// SPDX-License-Identifier: BSD-2-Clause
//! The controller over a model of its registers, and the Sky1's
//! controllers as firmware describes them.

extern crate std;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use std::sync::Mutex;

use acpi_platform::PlatformSource;

use super::*;

/// The registers' semantics (§1.2): the mask has set and clear ports,
/// the status is write-1-to-clear, the input is the pads.
#[derive(Default)]
struct Model {
    r: Mutex<BTreeMap<usize, u32>>,
    /// Every write, in order.
    log: Mutex<Vec<(usize, u32)>>,
}

impl Model {
    fn get(&self, off: usize) -> u32 {
        self.r.lock().unwrap().get(&off).copied().unwrap_or(0)
    }
    fn put(&self, off: usize, v: u32) {
        self.r.lock().unwrap().insert(off, v);
    }
    fn writes(&self) -> Vec<(usize, u32)> {
        core::mem::take(&mut *self.log.lock().unwrap())
    }
}

impl Words for Arc<Model> {
    fn read32(&self, off: usize) -> u32 {
        self.get(off)
    }
    fn write32(&self, off: usize, v: u32) {
        self.log.lock().unwrap().push((off, v));
        match off {
            reg::IRQ_EN => self.put(reg::IRQ_MASK, self.get(reg::IRQ_MASK) & !v),
            reg::IRQ_DIS => self.put(reg::IRQ_MASK, self.get(reg::IRQ_MASK) | v),
            reg::IRQ_STATUS => self.put(reg::IRQ_STATUS, self.get(reg::IRQ_STATUS) & !v),
            reg::INPUT_VALUE | reg::IRQ_MASK => panic!("read-only register written"),
            _ => self.put(off, v),
        }
    }
}

fn controller(lines: u32) -> (Arc<Model>, Controller<Arc<Model>>) {
    let m = Arc::new(Model::default());
    // Firmware's state: two lines pending, line 3 in bypass.
    m.put(reg::IRQ_STATUS, 0b1_0001);
    m.put(reg::BYPASS_MODE, 0b1000);
    m.put(reg::DIRECTION_MODE, u32::MAX);
    let c = Controller::new(Arc::clone(&m), lines);
    (m, c)
}

#[test]
fn attach_masks_and_acknowledges_every_line() {
    let (m, c) = controller(17);
    assert_eq!(
        m.writes(),
        [(reg::IRQ_DIS, 0x1_ffff), (reg::IRQ_STATUS, 0b1_0001)]
    );
    assert_eq!(m.get(reg::IRQ_MASK), 0x1_ffff);
    assert_eq!(m.get(reg::IRQ_STATUS), 0);
    // Nothing else is touched: firmware's directions and bypass stand.
    assert_eq!(m.get(reg::DIRECTION_MODE), u32::MAX);
    assert_eq!(c.lines(), 17);
    let (m, _) = controller(32);
    assert_eq!(m.get(reg::IRQ_MASK), u32::MAX);
}

#[test]
fn an_output_gets_its_level_before_its_direction() {
    let (m, c) = controller(32);
    m.writes();
    c.set_mode(3, PinMode::Output { high: true }).unwrap();
    assert_eq!(
        m.writes(),
        [
            (reg::OUTPUT_VALUE, 0b1000),
            (reg::OUTPUT_EN, 0b1000),
            (reg::DIRECTION_MODE, !0b1000),
            (reg::BYPASS_MODE, 0),
        ]
    );
    c.drive(3, false).unwrap();
    assert_eq!(m.get(reg::OUTPUT_VALUE), 0);
    m.put(reg::INPUT_VALUE, 1 << 9);
    assert_eq!(c.level(9), Ok(true));
    assert_eq!(c.level(8), Ok(false));
    assert_eq!(c.level(32), Err(ProviderError::NoSuchLine));
    // Released, it goes back to bypass as firmware left it.
    c.release(3);
    assert_eq!(m.get(reg::BYPASS_MODE), 0b1000);
    c.set_mode(5, PinMode::Input).unwrap();
    assert_eq!(m.get(reg::DIRECTION_MODE), !0b1000);
    c.release(5);
    assert_eq!(m.get(reg::BYPASS_MODE), 0b1000);
    // A bias is the pad's, not this controller's.
    // SAFETY: host tests run on a thread that may block.
    let s = unsafe { Sleepable::assume() };
    let p = Pins::new(Arc::new(c), &NO_DOMAIN, "0".into());
    assert_eq!(
        p.configure(&s, 1, PinMode::Input, Pull::Up),
        Err(ProviderError::Unsupported)
    );
    assert_eq!(p.configure(&s, 1, PinMode::Input, Pull::Default), Ok(()));
}

#[test]
fn pins_report_their_state_and_toggle() {
    let (m, c) = controller(32);
    m.put(reg::BYPASS_MODE, 0b100);
    m.put(reg::DIRECTION_MODE, 0b011);
    assert_eq!(c.state(0), Ok(PinState::Input));
    assert_eq!(c.state(2), Ok(PinState::Peripheral));
    assert_eq!(c.state(3), Ok(PinState::Output));
    assert_eq!(c.state(32), Err(ProviderError::NoSuchLine));
    m.put(reg::OUTPUT_VALUE, 0b1000);
    c.toggle(3).unwrap();
    c.toggle(4).unwrap();
    assert_eq!(m.get(reg::OUTPUT_VALUE), 0b1_0000);
    // SAFETY: host tests run on a thread that may block.
    let s = unsafe { Sleepable::assume() };
    let p = Pins::new(Arc::new(c), &NO_DOMAIN, "4".into());
    assert_eq!(p.name(7), "gpio4.7");
    assert_eq!(p.state(&s, 3), Ok(PinState::Output));
}

#[test]
fn triggers_set_all_three_registers() {
    let (m, c) = controller(32);
    let t = |c: &Controller<Arc<Model>>, tr, p| {
        c.set_trigger(7, tr, p).map(|()| {
            (
                m.get(reg::IRQ_TYPE) >> 7 & 1,
                m.get(reg::IRQ_VALUE) >> 7 & 1,
                m.get(reg::IRQ_ANY_EDGE) >> 7 & 1,
            )
        })
    };
    // Both edges first: a single edge after it must clear the any-edge
    // bit (§1.2.1).
    assert_eq!(t(&c, Trigger::Edge, Polarity::Both), Ok((0, 0, 1)));
    assert_eq!(t(&c, Trigger::Edge, Polarity::High), Ok((0, 1, 0)));
    assert_eq!(t(&c, Trigger::Edge, Polarity::Low), Ok((0, 0, 0)));
    assert_eq!(t(&c, Trigger::Level, Polarity::High), Ok((1, 1, 0)));
    assert_eq!(t(&c, Trigger::Level, Polarity::Low), Ok((1, 0, 0)));
    assert_eq!(
        t(&c, Trigger::Level, Polarity::Both),
        Err(IrqError::Unsupported)
    );
    assert_eq!(
        c.set_trigger(32, Trigger::Edge, Polarity::High),
        Err(IrqError::NoSuchInput)
    );
    // Configured masked, with an old edge dropped.
    m.put(reg::IRQ_STATUS, 1 << 7);
    c.unmask(7);
    c.set_trigger(7, Trigger::Edge, Polarity::High).unwrap();
    assert_eq!(m.get(reg::IRQ_MASK) >> 7 & 1, 1);
    assert_eq!(m.get(reg::IRQ_STATUS), 0);
}

#[test]
fn edges_are_acknowledged_before_their_handler_and_levels_after() {
    let (m, c) = controller(32);
    c.set_trigger(2, Trigger::Edge, Polarity::Both).unwrap();
    c.set_trigger(6, Trigger::Level, Polarity::Low).unwrap();
    c.set_trigger(9, Trigger::Edge, Polarity::High).unwrap();
    c.set_trigger(12, Trigger::Level, Polarity::High).unwrap();
    for l in [2, 6, 9] {
        c.unmask(l);
    }
    // 12 is pending but masked; 20 pending with no trigger (masked).
    m.put(
        reg::IRQ_STATUS,
        1 << 2 | 1 << 6 | 1 << 9 | 1 << 12 | 1 << 20,
    );
    m.writes();
    let mut seen = Vec::new();
    let handled = c.service(|line| {
        // What the status says while the handler runs.
        seen.push((line, m.get(reg::IRQ_STATUS) >> line & 1));
        // Line 9 has no handler.
        line != 9
    });
    assert!(handled);
    assert_eq!(seen, [(2, 0), (6, 1), (9, 0)]);
    assert_eq!(
        m.writes(),
        [
            (reg::IRQ_STATUS, 1 << 2),
            (reg::IRQ_STATUS, 1 << 6),
            (reg::IRQ_STATUS, 1 << 9),
            (reg::IRQ_DIS, 1 << 9),
        ]
    );
    // The unhandled line is masked; the masked ones stay pending.
    assert_eq!(m.get(reg::IRQ_MASK) >> 9 & 1, 1);
    assert_eq!(m.get(reg::IRQ_STATUS), 1 << 12 | 1 << 20);
    assert!(!c.service(|_| panic!("nothing unmasked is pending")));
}

#[test]
fn lines_past_the_count_are_never_serviced() {
    let (m, c) = controller(10);
    m.put(reg::IRQ_MASK, 0);
    m.put(reg::IRQ_STATUS, 1 << 12);
    assert!(!c.service(|_| panic!("line 12 does not exist")));
    assert_eq!(
        c.set_mode(10, PinMode::Input),
        Err(ProviderError::NoSuchLine)
    );
}

/// A domain for tests that never interrupt.
struct NoDomain;

impl IrqDomain for NoDomain {
    fn name(&self) -> &'static str {
        "none"
    }
    fn connect(&self, _: u32, _: Trigger, _: Polarity, _: hal::CpuId) -> Result<u32, IrqError> {
        Err(IrqError::NoSuchInput)
    }
    fn disconnect(&self, _: u32, _: hal::CpuId, _: u32) {}
    fn mask(&self, _: u32) {}
    fn unmask(&self, _: u32) {}
}

static NO_DOMAIN: NoDomain = NoDomain;

/// A controller as firmware gives it: its base, its line count, its
/// clocks, and its reset lines (controller, index).
type Found = (u64, u32, Vec<u32>, Vec<(String, u32)>);

/// The Sky1's seven controllers: their line counts by address, and the
/// clock and reset lines firmware gives them.
#[test]
fn sky1_controllers() {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let devices = acpi::deps::devices(&mut i, &mut h);
    let x = cix_sky1::PLATFORM.extract(&mut i, &mut h, &devices);
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    for &d in &devices {
        let hid = i.hid(d, &mut h).ok().flatten();
        if !hid.as_deref().is_some_and(|h| HIDS.contains(&h)) {
            continue;
        }
        let crs = i.crs(d, &mut h).ok().flatten().unwrap_or_default();
        let base = acpi::ranges::consumed(&crs)
            .first()
            .map(|r| r.base)
            .expect("registers");
        let clocks = x
            .description
            .clocks
            .iter()
            .filter(|c| c.consumer == d)
            .map(|c| c.id)
            .collect();
        let resets = x
            .description
            .resets
            .iter()
            .filter(|r| r.consumer == d)
            .map(|r| (i.path(r.controller).to_text(), r.index))
            .collect();
        found.insert(i.path(d).to_text(), (base, lines_at(base), clocks, resets));
    }
    let rst1 = || alloc::vec![(String::from("\\_SB_.RST1"), 26)];
    let s5 = Vec::new;
    let want: BTreeMap<String, Found> = [
        ("\\_SB_.GPI0", 0x0412_0000, 32, rst1()),
        ("\\_SB_.GPI1", 0x0413_0000, 32, rst1()),
        ("\\_SB_.GPI2", 0x0414_0000, 32, rst1()),
        ("\\_SB_.GPI3", 0x0415_0000, 17, rst1()),
        ("\\_SB_.GPI4", 0x1600_4000, 32, s5()),
        ("\\_SB_.GPI5", 0x1600_5000, 10, s5()),
        ("\\_SB_.GPI6", 0x1600_6000, 10, s5()),
    ]
    .into_iter()
    .map(|(p, b, n, r)| (String::from(p), (b, n, alloc::vec![262], r)))
    .collect();
    assert_eq!(found, want);
}
