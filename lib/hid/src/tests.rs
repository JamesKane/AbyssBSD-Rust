// SPDX-License-Identifier: BSD-2-Clause
#![allow(clippy::cast_possible_truncation)] // test sizes are small

extern crate std;

use alloc::vec::Vec;

use crate::usage::{self, KEYBOARD_APP, MOUSE, WHEEL, X, Y};
use crate::*;

#[test]
fn boot_keyboard() {
    let d = Descriptor::parse(BOOT_KEYBOARD).unwrap();
    assert_eq!(
        d.collections,
        [Collection {
            usage: KEYBOARD_APP,
            kind: 1
        }]
    );
    assert!(!d.uses_ids);
    assert_eq!(d.report_len(Kind::Input, 0), 8);
    assert_eq!(d.report_len(Kind::Output, 0), 1);
    let ins: Vec<&Field> = d.fields.iter().filter(|f| f.kind == Kind::Input).collect();
    // Modifiers, reserved byte, six key slots.
    assert_eq!(ins.len(), 3);
    assert_eq!((ins[0].bit, ins[0].size, ins[0].count), (0, 1, 8));
    assert!(ins[0].is_variable());
    assert_eq!(ins[0].usages, Usages::Range(0x0007_00e0, 0x0007_00e7));
    assert!(ins[1].is_constant());
    assert_eq!((ins[2].bit, ins[2].size, ins[2].count), (16, 8, 6));
    assert!(!ins[2].is_variable());
    // Left shift and 'a' down.
    let r = [0x02, 0, 0x04, 0, 0, 0, 0, 0];
    assert_eq!(ins[0].value(&r, 1), Some(1));
    assert_eq!(ins[0].usage(1), Some(0x0007_00e1));
    assert_eq!(ins[2].value(&r, 0), Some(4));
    assert_eq!(ins[2].value(&r, 6), None);
    // LEDs: five bits in the output report.
    let leds = d
        .fields
        .iter()
        .find(|f| f.kind == Kind::Output && !f.is_constant())
        .unwrap();
    assert_eq!(leds.usage(1), Some(usage::LED_CAPS_LOCK));
}

#[test]
fn boot_mouse_signed_axes() {
    let d = Descriptor::parse(BOOT_MOUSE).unwrap();
    assert_eq!(d.collections[0].usage, MOUSE);
    let xy = d.fields.iter().find(|f| f.usage(0) == Some(X)).unwrap();
    assert!(xy.is_relative());
    assert_eq!(xy.usage(1), Some(Y));
    assert_eq!((xy.logical_min, xy.logical_max), (-127, 127));
    let r = [0x01, 0xfb, 0x07];
    assert_eq!(xy.value(&r, 0), Some(-5));
    assert_eq!(xy.value(&r, 1), Some(7));
    let buttons = d
        .fields
        .iter()
        .find(|f| f.usage(0) == Some(0x0009_0001))
        .unwrap();
    assert_eq!(buttons.value(&r, 0), Some(1));
}

/// A mouse with report IDs and a wheel, plus a consumer collection.
const COMPOSITE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03,
    0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0x80, 0x26, 0xff, 0x7f, 0x75, 0x10,
    0x95, 0x02, 0x81, 0x06, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0xc0, 0xc0, //
    0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x02, 0x19, 0x00, 0x2a, 0x3c, 0x02, 0x15, 0x00, 0x26,
    0x3c, 0x02, 0x95, 0x01, 0x75, 0x10, 0x81, 0x00, 0xc0,
];

#[test]
fn report_ids_and_collections() {
    let d = Descriptor::parse(COMPOSITE).unwrap();
    assert!(d.uses_ids);
    assert_eq!(d.collections.len(), 2);
    assert_eq!(d.collections[1].usage, 0x000c_0001);
    assert_eq!(d.report_len(Kind::Input, 1), 6);
    assert_eq!(d.report_len(Kind::Input, 2), 2);
    let xy = d.fields_of(0).find(|f| f.usage(0) == Some(X)).unwrap();
    assert_eq!((xy.report_id, xy.bit, xy.size), (1, 8, 16));
    let r = [0x00, 0x18, 0xfc, 0xff, 0x7f, 0x00];
    assert_eq!(xy.value(&r, 0), Some(-1000));
    let w = d.fields_of(0).find(|f| f.usage(0) == Some(WHEEL)).unwrap();
    assert_eq!(w.bit, 40);
    let c = d.fields_of(1).next().unwrap();
    assert_eq!(c.usages, Usages::Range(0x000c_0000, 0x000c_023c));
    assert_eq!(c.logical_max, 572);
}

#[test]
fn unbalanced_and_truncated() {
    assert_eq!(Descriptor::parse(&[0xa1, 0x01]), Err(Error::Unbalanced));
    assert_eq!(Descriptor::parse(&[0xc0]), Err(Error::Unbalanced));
    assert_eq!(Descriptor::parse(&[0xb4]), Err(Error::Unbalanced));
    assert_eq!(Descriptor::parse(&[0x26, 0xff]), Err(Error::Truncated));
    assert_eq!(Descriptor::parse(&[0xfe, 10, 0]), Err(Error::Truncated));
    assert_eq!(Descriptor::parse(&[]), Ok(Descriptor::default()));
}

#[test]
fn fuzzed_descriptors_never_panic() {
    let mut x = 0x0123_4567_89ab_cdefu64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..20_000 {
        let base = [BOOT_KEYBOARD, BOOT_MOUSE, COMPOSITE][round % 3];
        let mut b = base.to_vec();
        for _ in 0..(rnd() % 5) {
            let i = (rnd() as usize) % b.len();
            b[i] = rnd() as u8;
        }
        b.truncate((rnd() as usize) % (b.len() + 1));
        if let Ok(d) = Descriptor::parse(&b) {
            let report = [0xa5u8; 16];
            for f in &d.fields {
                for i in 0..f.count.min(8) {
                    let _ = f.value(&report, i);
                    let _ = f.usage(i);
                }
            }
            let _ = d.report_len(Kind::Input, 0);
        }
    }
}
