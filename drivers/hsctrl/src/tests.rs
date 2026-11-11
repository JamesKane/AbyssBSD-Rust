// SPDX-License-Identifier: BSD-2-Clause
//! `hsctrl`'s table through `hidmap`, on the board keyboard's media-key
//! interface and on an array form. Each expectation is what
//! FreeBSD 15.1's `hidmap_intr` pushes to evdev for the report.

extern crate std;

use abi::evdev as ev;
use hid::hidmap::{Hidmap, Recorder, probe};
use hid::{Descriptor, OPI6P_KEYBOARD_MEDIA};

use super::MAP;

const SYN: (u16, u16, i32) = (0, 0, 1);

fn key(code: u16, value: i32) -> (u16, u16, i32) {
    (ev::EV_KEY, code, value)
}

fn attach(d: &Descriptor, tlc: usize) -> (Hidmap, Recorder) {
    let mut m = Hidmap::new();
    m.add_map(d, tlc, &MAP).expect("hsctrl maps the collection");
    let mut r = Recorder::default();
    m.attach(d, tlc, &mut r);
    (m, r)
}

#[test]
fn board_system_collection_is_hsctrl_only() {
    let d = Descriptor::parse(OPI6P_KEYBOARD_MEDIA).unwrap();
    assert_eq!(d.collections[1].usage, super::SYSTEM_CONTROL);
    // Power Down, Sleep, and Wake Up; the padding is not a value. The
    // bits are relative (Input 0x06), which hsctrl's keys take: their
    // entries are `HIDMAP_KEY`, relative or absolute alike.
    assert_eq!(probe(&d, 1, &MAP).0, 3);
    assert_eq!(probe(&d, 0, &MAP).0, 0);
}

#[test]
fn power_sleep_and_wake_bits() {
    let d = Descriptor::parse(OPI6P_KEYBOARD_MEDIA).unwrap();
    let (mut m, mut r) = attach(&d, 1);
    assert_eq!(r.types, [ev::EV_KEY]);
    assert_eq!(r.keys, [ev::KEY_POWER, ev::KEY_SLEEP, ev::KEY_WAKEUP]);
    assert!(!m.merges_keys());
    assert!(m.items().iter().all(|i| i.id == 2));
    // The power key, pressed and released: evdev events, nothing else. A
    // key compares with its last value even when relative, so a held bit
    // is not pressed again.
    m.intr(2, &[0b001], &mut r);
    assert_eq!(r.take(), [key(ev::KEY_POWER, 1), SYN]);
    m.intr(2, &[0b000], &mut r);
    assert_eq!(r.take(), [key(ev::KEY_POWER, 0), SYN]);
    m.intr(2, &[0b110], &mut r);
    assert_eq!(
        r.take(),
        [key(ev::KEY_SLEEP, 1), key(ev::KEY_WAKEUP, 1), SYN]
    );
    // The padding bits are not looked at.
    m.intr(2, &[0b1111_1110], &mut r);
    assert_eq!(r.take(), []);
    m.intr(2, &[0], &mut r);
    assert_eq!(
        r.take(),
        [key(ev::KEY_SLEEP, 0), key(ev::KEY_WAKEUP, 0), SYN]
    );
    // Consumer Control's report is not this collection's.
    m.intr(1, &[0xe9, 0, 0, 0], &mut r);
    assert_eq!(r.take(), []);
}

/// System Control as an array: one byte, 1 to 3 naming Power Down, Sleep,
/// and Wake Up, 0 none (out of range), report 3.
const ARRAY: &[u8] = &[
    0x05, 0x01, 0x09, 0x80, 0xa1, 0x01, 0x85, 0x03, 0x19, 0x81, 0x29, 0x83, 0x15, 0x01, 0x25, 0x03,
    0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xc0,
];

#[test]
fn array_of_system_controls() {
    let d = Descriptor::parse(ARRAY).unwrap();
    let (mut m, mut r) = attach(&d, 0);
    assert_eq!(r.keys, [ev::KEY_POWER, ev::KEY_SLEEP, ev::KEY_WAKEUP]);
    m.intr(3, &[1], &mut r);
    assert_eq!(r.take(), [key(ev::KEY_POWER, 1), SYN]);
    m.intr(3, &[2], &mut r);
    assert_eq!(
        r.take(),
        [key(ev::KEY_POWER, 0), key(ev::KEY_SLEEP, 1), SYN]
    );
    m.intr(3, &[0], &mut r);
    assert_eq!(r.take(), [key(ev::KEY_SLEEP, 0), SYN]);
}
