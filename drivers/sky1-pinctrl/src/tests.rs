// SPDX-License-Identifier: BSD-2-Clause
//! The Sky1's pin groups (`tests/corpus/acpi/opi6p`): every declaration
//! reads, and every group a consumer names exists but for the firmware's
//! known defect.

extern crate std;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::*;

/// Each controller's groups, by path.
type Controllers = BTreeMap<String, Vec<Group>>;
/// Group references: (consumer, controller, group).
type Uses = Vec<(String, String, String)>;

/// Each controller's groups and each consumer's group references.
fn sky1() -> (Controllers, Uses) {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let mut controllers = BTreeMap::new();
    let mut uses = Vec::new();
    for d in acpi::deps::devices(&mut i, &mut h) {
        let path = i.path(d).to_text();
        let crs = i.crs(d, &mut h).ok().flatten().unwrap_or_default();
        let hid = i.hid(d, &mut h).ok().flatten().unwrap_or_default();
        if let Some(c) = Controller::of(&hid) {
            controllers.insert(path.clone(), groups(c, &crs).unwrap());
        }
        for r in &crs {
            if let Resource::PinGroupFunction(f) = r {
                // `\_SB.MUX0` as written; the walk's paths pad segments.
                let ctl = f.source.name.replace("\\_SB.", "\\_SB_.");
                uses.push((path.clone(), ctl, f.group.clone()));
            }
        }
    }
    (controllers, uses)
}

#[test]
fn sky1_groups() {
    let (controllers, _) = sky1();
    let count = |p: &str| controllers[p].len();
    assert_eq!((count("\\_SB_.MUX0"), count("\\_SB_.MUX1")), (38, 24));
    let group = |p: &str, label: &str| {
        controllers[p]
            .iter()
            .find(|g| g.label == label)
            .unwrap_or_else(|| panic!("{label}"))
            .pads
            .iter()
            .map(|p| (p.pad, p.value))
            .collect::<Vec<_>>()
    };
    // The console: UART2's TXD and RXD, mode 0, pull-down, drive 7.
    assert_eq!(
        group("\\_SB_.MUX0", "pinctrl_fch_uart2"),
        [(87, 0x27), (88, 0x27)]
    );
    // The RTC's I2C3: pull-up, Schmitt trigger, drive 12.
    assert_eq!(
        group("\\_SB_.MUX0", "pinctrl_fch_i2c3"),
        [(37, 0x5c), (38, 0x5c)]
    );
    // A pad in mode 1: UART0_TXD as PWM0.
    let pwm1 = group("\\_SB_.MUX0", "pinctrl_fch_pwm1");
    assert_eq!(pwm1, [(79, 0xb7)]);
    assert_eq!(
        Pad {
            pad: 79,
            value: 0xb7
        }
        .function(),
        1
    );
    // The PCIe x8 root complex's reset pad on the S5 controller.
    assert_eq!(group("\\_SB_.MUX1", "pinctrl_pcie_x8_rc"), [(1, 0x24)]);
}

/// Consumers name groups their controller declares, except the four
/// I2S controllers', which ask for `*_dbg` groups firmware never declared.
#[test]
fn consumers_name_declared_groups() {
    let (controllers, uses) = sky1();
    let mut missing: Vec<String> = uses
        .iter()
        .filter(|(_, ctl, g)| !controllers[ctl].iter().any(|x| &x.label == g))
        .map(|(c, _, g)| alloc::format!("{c} {g}"))
        .collect();
    missing.sort();
    assert_eq!(
        missing,
        [
            "\\_SB_.I2S5 pinctrl_substrate_i2s5_dbg",
            "\\_SB_.I2S6 pinctrl_substrate_i2s6_dbg",
            "\\_SB_.I2S8 pinctrl_substrate_i2s8_dbg",
            "\\_SB_.I2S9 pinctrl_substrate_i2s9_dbg",
        ]
    );
    assert!(uses.len() > 40, "{}", uses.len());
}

#[test]
fn malformed_declarations() {
    use aml::resource::PinGroup;
    let g = |pins: &[u16], vendor: &[u8]| {
        alloc::vec![Resource::PinGroup(PinGroup {
            revision: 1,
            flags: 0,
            pins: pins.to_vec(),
            label: "g".into(),
            vendor: vendor.to_vec(),
        })]
    };
    let why = |r: Result<Vec<Group>, Malformed>| r.unwrap_err().why;
    assert!(groups(Controller::Fch, &g(&[1], &[0, 4, 0, 0x27])).is_ok());
    assert_eq!(
        why(groups(Controller::Fch, &g(&[1], &[0, 8, 0, 0x27]))),
        "register offset is not the pad's"
    );
    assert_eq!(
        why(groups(Controller::Fch, &g(&[1, 2], &[0, 4, 0, 0x27]))),
        "vendor data is not four bytes per pad"
    );
    assert_eq!(
        why(groups(Controller::S5, &g(&[66], &[1, 8, 0, 0]))),
        "pad out of range"
    );
    assert_eq!(
        why(groups(Controller::Fch, &g(&[1], &[0, 4, 2, 0]))),
        "value sets unknown bits"
    );
}

#[test]
fn differences_are_reported() {
    let groups = alloc::vec![Group {
        label: "uart".into(),
        pads: alloc::vec![
            Pad {
                pad: 87,
                value: 0x27
            },
            Pad {
                pad: 88,
                value: 0x27
            }
        ],
    }];
    // Pad 88 was left in mode 1; the unknown high bits are ignored.
    let regs = |off: usize| match off {
        0x15c => 0xdead_0027 & !0x1ff | 0x27,
        0x160 => 0xa7,
        _ => 0,
    };
    let d = differences(&groups, regs);
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].declared.pad, d[0].found), (88, 0xa7));
}
