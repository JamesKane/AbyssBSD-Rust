// SPDX-License-Identifier: BSD-2-Clause
//! `hcons`'s table through `hidmap`, on a media-key interface laid out as
//! the board keyboard's, and on the other forms Consumer Control takes.
//! Each expectation is what FreeBSD 15.1's `hidmap_intr` pushes to evdev
//! for the report.

extern crate std;

use alloc::vec;
use alloc::vec::Vec;

use abi::evdev as ev;
use hid::hidmap::{Hidmap, Recorder, probe};
use hid::{Descriptor, MEDIA_KEYS};

use super::MAP;

const SYN: (u16, u16, i32) = (0, 0, 1);

fn key(code: u16, value: i32) -> (u16, u16, i32) {
    (ev::EV_KEY, code, value)
}

/// `hcons` on collection `tlc` of `d`, attached, and what it told the
/// device it reports.
fn attach(d: &Descriptor, tlc: usize) -> (Hidmap, Recorder) {
    let mut m = Hidmap::new();
    m.add_map(d, tlc, &MAP).expect("hcons maps the collection");
    let mut r = Recorder::default();
    m.attach(d, tlc, &mut r);
    (m, r)
}

/// Report 1 with the two slots' usages.
fn slots(a: u16, b: u16) -> Vec<u8> {
    let (a, b) = (a.to_le_bytes(), b.to_le_bytes());
    vec![a[0], a[1], b[0], b[1]]
}

#[test]
fn media_keys_consumer_collection_is_hcons_only() {
    let d = Descriptor::parse(MEDIA_KEYS).unwrap();
    assert_eq!(d.collections.len(), 2);
    assert_eq!(d.collections[0].usage, super::CONSUMER_CONTROL);
    // Two array slots, each mapped.
    assert_eq!(probe(&d, 0, &MAP).0, 2);
    // The System Control collection has nothing of hcons's.
    assert_eq!(probe(&d, 1, &MAP).0, 0);
}

#[test]
fn supports_every_key_in_the_slots_range_once() {
    let d = Descriptor::parse(MEDIA_KEYS).unwrap();
    let (m, r) = attach(&d, 0);
    assert_eq!(r.types, [ev::EV_KEY]);
    // Every key entry (usages 0 to 0x3ff cover the table), each code
    // once, in the table's order.
    let mut want: Vec<u16> = Vec::new();
    for mi in &MAP {
        if let hid::hidmap::Action::Event {
            kind: ev::EV_KEY,
            code,
            ..
        } = mi.action
            && !want.contains(&code)
        {
            want.push(code);
        }
    }
    assert_eq!(r.keys, want);
    assert!(r.keys.contains(&ev::KEY_VOLUMEUP) && r.keys.contains(&ev::KEY_BRIGHTNESSUP));
    // A volume control is not an array's: no ABS_VOLUME.
    assert!(r.abs.is_empty() && r.rels.is_empty());
    // Sleep (0x32) and Sleep Mode (0x34) share KEY_SLEEP, among others.
    assert!(m.merges_keys());
    assert_eq!(m.items().len(), 2);
    assert!(m.items().iter().all(|i| i.id == 1));
}

#[test]
fn media_keys_reports_become_freebsds_events() {
    let d = Descriptor::parse(MEDIA_KEYS).unwrap();
    let (mut m, mut r) = attach(&d, 0);
    let mut send = |a: u16, b: u16| {
        m.intr(1, &slots(a, b), &mut r);
        r.take()
    };
    // Mute, then Volume Up beside it.
    assert_eq!(send(0xe2, 0), [key(ev::KEY_MUTE, 1), SYN]);
    assert_eq!(send(0xe2, 0xe9), [key(ev::KEY_VOLUMEUP, 1), SYN]);
    // Volume Up moves to the first slot as Mute goes: the second slot's
    // release of it and the first's press merge, so only Mute changes.
    assert_eq!(send(0xe9, 0), [key(ev::KEY_MUTE, 0), SYN]);
    assert_eq!(send(0, 0), [key(ev::KEY_VOLUMEUP, 0), SYN]);
    // Nothing changed: nothing reported, not even a sync.
    assert_eq!(send(0, 0), []);
    // Two usages of one key at once are one press, and one release.
    assert_eq!(send(0x32, 0x34), [key(ev::KEY_SLEEP, 1), SYN]);
    assert_eq!(send(0, 0), [key(ev::KEY_SLEEP, 0), SYN]);
    // Brightness, play/pause, the calculator, home. Merged keys go out
    // in code order, as `hidmap_sync_keys` walks them.
    assert_eq!(send(0x6f, 0), [key(ev::KEY_BRIGHTNESSUP, 1), SYN]);
    assert_eq!(
        send(0x70, 0xcd),
        [
            key(ev::KEY_PLAYPAUSE, 1),
            key(ev::KEY_BRIGHTNESSDOWN, 1),
            key(ev::KEY_BRIGHTNESSUP, 0),
            SYN
        ]
    );
    assert_eq!(
        send(0x192, 0x223),
        [
            key(ev::KEY_CALC, 1),
            key(ev::KEY_PLAYPAUSE, 0),
            key(ev::KEY_HOMEPAGE, 1),
            key(ev::KEY_BRIGHTNESSDOWN, 0),
            SYN
        ]
    );
    // A usage hcons has no key for, and a value past the logical maximum,
    // assert nothing: the keys they replace go up.
    assert_eq!(
        send(0x0e0, 0xffff),
        [key(ev::KEY_CALC, 0), key(ev::KEY_HOMEPAGE, 0), SYN]
    );
    assert_eq!(send(0, 0), []);
}

#[test]
fn other_reports_and_short_ones() {
    let d = Descriptor::parse(MEDIA_KEYS).unwrap();
    let (mut m, mut r) = attach(&d, 0);
    // Report 2 is System Control's.
    m.intr(2, &[0x01], &mut r);
    assert_eq!(r.take(), []);
    // A report cut short reads as zeros past its end (`hid_get_data`).
    m.intr(1, &[0xe9], &mut r);
    assert_eq!(r.take(), [key(ev::KEY_VOLUMEUP, 1), SYN]);
    m.intr(1, &[], &mut r);
    assert_eq!(r.take(), [key(ev::KEY_VOLUMEUP, 0), SYN]);
}

/// Consumer Control with a relative Volume (-1 to 1, two bits) and Mute
/// and Volume Up as bits, with no report IDs.
const RELATIVE_VOLUME: &[u8] = &[
    0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x09, 0xe0, 0x15, 0xff, 0x25, 0x01, 0x75, 0x02, 0x95, 0x01,
    0x81, 0x06, 0x09, 0xe2, 0x09, 0xe9, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02, 0x81, 0x02,
    0x95, 0x04, 0x81, 0x01, 0xc0,
];

#[test]
fn relative_volume_presses_volume_keys_per_step() {
    let d = Descriptor::parse(RELATIVE_VOLUME).unwrap();
    assert_eq!(probe(&d, 0, &MAP).0, 3);
    let (mut m, mut r) = attach(&d, 0);
    // The callback's keys, then the bits'.
    assert_eq!(
        r.keys,
        [
            ev::KEY_VOLUMEUP,
            ev::KEY_VOLUMEDOWN,
            ev::KEY_MUTE,
            ev::KEY_VOLUMEUP
        ]
    );
    assert!(!m.merges_keys());
    m.intr(0, &[0b01], &mut r);
    assert_eq!(
        r.take(),
        [key(ev::KEY_VOLUMEUP, 1), key(ev::KEY_VOLUMEUP, 0), SYN]
    );
    m.intr(0, &[0b11], &mut r);
    assert_eq!(
        r.take(),
        [key(ev::KEY_VOLUMEDOWN, 1), key(ev::KEY_VOLUMEDOWN, 0), SYN]
    );
    // No step: the callback reports nothing (ENOMSG), so no sync.
    m.intr(0, &[0b00], &mut r);
    assert_eq!(r.take(), []);
    // Mute held.
    m.intr(0, &[0b0100], &mut r);
    assert_eq!(r.take(), [key(ev::KEY_MUTE, 1), SYN]);
    m.intr(0, &[0b0000], &mut r);
    assert_eq!(r.take(), [key(ev::KEY_MUTE, 0), SYN]);
}

/// Consumer Control with an absolute Volume (0 to 100, a byte) and AC Pan
/// (relative, a byte), report 3.
const ABSOLUTE_VOLUME: &[u8] = &[
    0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x03, 0x09, 0xe0, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08,
    0x95, 0x01, 0x81, 0x02, 0x0a, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7f, 0x81, 0x06, 0xc0,
];

#[test]
fn absolute_volume_and_pan() {
    let d = Descriptor::parse(ABSOLUTE_VOLUME).unwrap();
    let (mut m, mut r) = attach(&d, 0);
    assert_eq!(r.types, [ev::EV_ABS, ev::EV_REL]);
    assert_eq!(r.abs, [(ev::ABS_VOLUME, 0, 100)]);
    assert_eq!(r.rels, [ev::REL_HWHEEL]);
    m.intr(3, &[40, 0], &mut r);
    assert_eq!(r.take(), [(ev::EV_ABS, ev::ABS_VOLUME, 40), SYN]);
    // The same volume again is not reported; a pan is, signed.
    m.intr(3, &[40, 0xfe], &mut r);
    assert_eq!(r.take(), [(ev::EV_REL, ev::REL_HWHEEL, -2), SYN]);
    m.intr(3, &[40, 0], &mut r);
    assert_eq!(r.take(), []);
}

/// Consumer Control as a list of variable bits (Play/Pause, Next, Previous,
/// Mute, Volume Up and Down, Mail, then two unused), report 4.
const BITMAP: &[u8] = &[
    0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x04, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08,
    0x09, 0xcd, 0x09, 0xb5, 0x09, 0xb6, 0x09, 0xe2, 0x09, 0xe9, 0x09, 0xea, 0x0a, 0x8a, 0x01, 0x0a,
    0x00, 0x03, 0x81, 0x02, 0xc0,
];

#[test]
fn variable_bits_report_each_change() {
    let d = Descriptor::parse(BITMAP).unwrap();
    // The eighth bit (usage 0x300) maps to nothing.
    assert_eq!(probe(&d, 0, &MAP).0, 7);
    let (mut m, mut r) = attach(&d, 0);
    assert_eq!(
        r.keys,
        [
            ev::KEY_PLAYPAUSE,
            ev::KEY_NEXTSONG,
            ev::KEY_PREVIOUSSONG,
            ev::KEY_MUTE,
            ev::KEY_VOLUMEUP,
            ev::KEY_VOLUMEDOWN,
            ev::KEY_MAIL
        ]
    );
    m.intr(4, &[0b0001_0001], &mut r);
    assert_eq!(
        r.take(),
        [key(ev::KEY_PLAYPAUSE, 1), key(ev::KEY_VOLUMEUP, 1), SYN]
    );
    m.intr(4, &[0b0100_0001], &mut r);
    assert_eq!(
        r.take(),
        [key(ev::KEY_VOLUMEUP, 0), key(ev::KEY_MAIL, 1), SYN]
    );
    // The unmapped bit changes nothing.
    m.intr(4, &[0b1100_0001], &mut r);
    assert_eq!(r.take(), []);
}
