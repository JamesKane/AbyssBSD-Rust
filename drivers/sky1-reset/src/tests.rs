// SPDX-License-Identifier: BSD-2-Clause
//! The index maps against the specification and the Sky1's `RSTL`
//! tables, and the lines over fake registers.

extern crate std;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use std::sync::Mutex;

use acpi_platform::PlatformSource;

use super::*;

fn s() -> Sleepable {
    // SAFETY: host tests run on a thread that may block.
    unsafe { Sleepable::assume() }
}

/// Lines the specification names, by controller.
#[test]
fn index_maps() {
    let b = |c: Controller, i| c.bit(i).map(|b| (b.offset, b.bit));
    // S5: CSU PM, GIC, the last DP PHY line, GMAC0, PCIe4, the USB words'
    // ends, the RCSU words' ends.
    for (i, want) in [
        (0, (0x304, 0)),
        (4, (0x400, 1)),
        (29, (0x400, 26)),
        (44, (0x404, 14)),
        (50, (0x404, 20)),
        (51, (0x408, 0)),
        (70, (0x408, 19)),
        (90, (0x40c, 19)),
        (91, (0x800, 0)),
        (122, (0x800, 31)),
        (142, (0x804, 19)),
    ] {
        assert_eq!(b(Controller::S5, i), Some(want), "S5 {i}");
    }
    assert_eq!(b(Controller::S5, 143), None);
    // FCH: UART0 functional, the timer, the DMA, UART3 APB, I2C0 and
    // I2C7, GPIO, XSPI.
    for (i, want) in [
        (4, (0x008, 4)),
        (8, (0x008, 20)),
        (11, (0x00c, 2)),
        (15, (0x00c, 7)),
        (17, (0x00c, 9)),
        (18, (0x00c, 12)),
        (25, (0x00c, 19)),
        (26, (0x00c, 21)),
        (28, (0x010, 1)),
    ] {
        assert_eq!(b(Controller::Fch, i), Some(want), "FCH {i}");
    }
    assert_eq!(b(Controller::Fch, 29), None);
    // No two lines share a bit, and every word is inside the span.
    for c in [Controller::S5, Controller::Fch] {
        let all: Vec<Bit> = (0..200).filter_map(|i| c.bit(i)).collect();
        for (k, x) in all.iter().enumerate() {
            assert!(!all[..k].contains(x), "{c:?} {x:?}");
            assert!(x.offset + 4 <= c.span() && x.bit < 32);
        }
    }
}

/// Every line the Sky1's firmware names on `RST0` or `RST1` is one the
/// map has.
#[test]
fn sky1_rstl_lines_exist() {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let devices = acpi::deps::devices(&mut i, &mut h);
    let x = cix_sky1::PLATFORM.extract(&mut i, &mut h, &devices);
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for r in &x.description.resets {
        let ctl = i.path(r.controller).to_text();
        let c = match ctl.as_str() {
            "\\_SB_.RST0" => Controller::S5,
            "\\_SB_.RST1" => Controller::Fch,
            _ => continue,
        };
        assert!(c.bit(r.index).is_some(), "{ctl} line {}", r.index);
        *by.entry(ctl).or_default() += 1;
    }
    // Both controllers serve lines, and only the audio subsystem's own
    // controller serves the rest.
    assert!(by["\\_SB_.RST0"] > 0 && by["\\_SB_.RST1"] > 0, "{by:?}");
}

/// Registers as a word map.
#[derive(Default)]
struct Fake(Mutex<BTreeMap<usize, u32>>);

impl Words for Fake {
    fn read32(&self, off: usize) -> u32 {
        self.0.lock().unwrap().get(&off).copied().unwrap_or(0)
    }
    fn write32(&self, off: usize, v: u32) {
        self.0.lock().unwrap().insert(off, v);
    }
}

#[test]
fn lines_are_active_low_bits() {
    let fake = Fake::default();
    // Everything out of reset but UART1's APB line.
    fake.0.lock().unwrap().insert(0x00c, !(1 << 5));
    let l = Lines::new(Controller::Fch, fake);
    let s = s();
    assert_eq!(l.asserted(13), Ok(true));
    assert_eq!(l.asserted(18), Ok(false));
    l.deassert(&s, 13).unwrap();
    assert_eq!(l.words.lock().read32(0x00c), u32::MAX);
    // Asserting I2C0 clears its bit alone.
    l.assert(&s, 18).unwrap();
    assert_eq!(l.words.lock().read32(0x00c), !(1 << 12));
    assert_eq!(l.asserted(18), Ok(true));
    assert_eq!(l.assert(&s, 29), Err(ProviderError::NoSuchLine));
    assert!(!l.group_safe(26));
}
