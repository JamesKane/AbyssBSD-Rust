// SPDX-License-Identifier: BSD-2-Clause
//! What the Orange Pi 6 Plus's firmware tells the driver: the timer's
//! window, the two clocks and the reset it asks for by name.

extern crate std;

use std::collections::BTreeMap;

use acpi_platform::PlatformSource;

use crate::{APB_CLOCK, FUNC_CLOCK, RESET};

#[test]
fn the_firmware_gives_the_timer_the_lines_the_driver_asks_for() {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let devices = acpi::deps::devices(&mut i, &mut h);
    let x = cix_sky1::PLATFORM.extract(&mut i, &mut h, &devices);
    let timers: std::vec::Vec<_> = devices
        .iter()
        .copied()
        .filter(|&d| i.hid(d, &mut h).ok().flatten().as_deref() == Some("CIXH1007"))
        .collect();
    // One instance is described: the specification's GPT 3.
    assert_eq!(timers.len(), 1);
    let d = timers[0];
    let crs = i.crs(d, &mut h).ok().flatten().unwrap_or_default();
    let mem = acpi::ranges::consumed(&crs);
    assert_eq!(
        mem.first().map(|r| (r.base, r.len)),
        Some((0x0411_6000, 0x2000))
    );
    let clocks: BTreeMap<_, _> = x
        .description
        .clocks
        .iter()
        .filter(|c| c.consumer == d)
        .map(|c| (c.name.clone().unwrap_or_default(), c.id))
        .collect();
    // The specification's CLK_TREE_FCH_TIMER_APB and CLK_TREE_FCH_TIMER_FUN.
    assert_eq!(clocks.get(APB_CLOCK), Some(&261));
    assert_eq!(clocks.get(FUNC_CLOCK), Some(&242));
    let resets: BTreeMap<_, _> = x
        .description
        .resets
        .iter()
        .filter(|r| r.consumer == d)
        .map(|r| (r.name.clone().unwrap_or_default(), r.index))
        .collect();
    // FCH controller index 8, shared by the four instances.
    assert_eq!(resets.get(RESET), Some(&8));
}

#[test]
fn the_measured_rate_must_be_near_the_clocks() {
    assert!(crate::rate_agrees(25_000_000, 25_000_000));
    assert!(crate::rate_agrees(24_000_000, 25_000_000));
    assert!(!crate::rate_agrees(12_500_000, 25_000_000));
    assert!(!crate::rate_agrees(50_000_000, 25_000_000));
}
