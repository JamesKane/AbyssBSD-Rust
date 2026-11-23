// SPDX-License-Identifier: BSD-2-Clause
//! The transmitters as the Orange Pi 6 Plus firmware gives them.

use std::collections::BTreeMap;
use std::string::String;
use std::vec::Vec;

use acpi_platform::PlatformSource;

use super::*;

#[test]
fn the_phys_and_their_paths() {
    assert_eq!(phy_index(0), Some(0));
    assert_eq!(phy_index(2), None);
    assert_eq!(phy_index(3), Some(2));
    assert_eq!(phy_index(4), Some(3));
    assert_eq!(phy_path(3), "\\_SB_.UCP3");
}

/// Each unit's windows, clocks and reset, and the PHY it waits for exists
/// in the namespace under the name the driver gives.
#[test]
fn the_firmware_gives_each_transmitter_what_the_driver_asks_for() {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let devices = acpi::deps::devices(&mut i, &mut h);
    let x = cix_sky1::PLATFORM.extract(&mut i, &mut h, &devices);
    let paths: Vec<String> = devices.iter().map(|&d| i.path(d).to_text()).collect();
    let cores = [
        0x1406_4000u64,
        0x140D_4000,
        0x1414_4000,
        0x141B_4000,
        0x1422_4000,
    ];
    let mut seen = 0;
    for &d in &devices {
        if i.hid(d, &mut h).ok().flatten().as_deref() != Some(HID) {
            continue;
        }
        let Some(aml::Uid::Integer(uid)) = i.uid(d, &mut h).ok().flatten() else {
            panic!("a unit number");
        };
        let n = u32::try_from(uid).unwrap();
        let crs = i.crs(d, &mut h).ok().flatten().unwrap_or_default();
        let mem = acpi::ranges::consumed(&crs);
        // The core, the unused `dsc`, the PHY interface, the adapter.
        assert_eq!(mem.len(), 4, "unit {n}");
        assert_eq!(mem[0].base, cores[usize::try_from(uid).unwrap()]);
        assert_eq!(mem[1].base, cores[usize::try_from(uid).unwrap()] + 0x4000);
        let clocks: BTreeMap<_, _> = x
            .description
            .clocks
            .iter()
            .filter(|c| c.consumer == d)
            .map(|c| (c.name.clone().unwrap_or_default(), c.id))
            .collect();
        assert_eq!(clocks.get("apb_clk"), Some(&(59 + n)));
        assert_eq!(clocks.get("vid_clk0"), Some(&(49 + 2 * n)));
        assert_eq!(clocks.get("vid_clk1"), Some(&(50 + 2 * n)));
        let resets: BTreeMap<_, _> = x
            .description
            .resets
            .iter()
            .filter(|r| r.consumer == d)
            .map(|r| (r.name.clone().unwrap_or_default(), r.index))
            .collect();
        assert_eq!(resets.get("dp_reset"), Some(&(24 + n)));
        if let Some(p) = phy_index(n) {
            assert!(paths.contains(&phy_path(p)), "PHY {p} of unit {n}");
        }
        seen += 1;
    }
    assert_eq!(seen, 5);
}

#[test]
fn outputs_pass_between_drivers_by_unit() {
    struct E;
    impl EncoderHw for E {
        fn kind(&self) -> u32 {
            0
        }
    }
    struct C;
    impl ConnectorHw for C {
        fn kind(&self) -> u32 {
            0
        }
        fn detect(&self) -> Connection {
            Connection::Unknown
        }
        fn modes(&self) -> Vec<Mode> {
            Vec::new()
        }
    }
    let o = outputs::Output {
        encoder: Arc::new(E),
        connector: Arc::new(C),
    };
    assert!(outputs::lookup(77).is_none());
    outputs::publish(77, o.clone());
    assert!(outputs::lookup(77).is_some());
    outputs::publish(77, o);
    outputs::withdraw(77);
    assert!(outputs::lookup(77).is_none());
}
