// SPDX-License-Identifier: BSD-2-Clause
#![allow(clippy::cast_possible_truncation)] // test sizes are small

extern crate std;

use alloc::vec;
use alloc::vec::Vec;

use crate::hub::{self, HubDescriptor, PortSpeed, PortStatus, feature};
use crate::*;

/// A boot keyboard's configuration: interface, HID descriptor, interrupt
/// IN endpoint.
const KEYBOARD_CONFIG: [u8; 34] = [
    9, 2, 34, 0, 1, 1, 6, 0xa0, 50, //
    9, 4, 0, 0, 1, 3, 1, 1, 0, //
    9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0, //
    7, 5, 0x81, 3, 8, 0, 7,
];

/// A USB 3 bulk-only disk: two bulk endpoints with companions.
const DISK_CONFIG: [u8; 44] = [
    9, 2, 44, 0, 1, 1, 0, 0x80, 50, //
    9, 4, 0, 0, 2, 8, 6, 0x50, 0, //
    7, 5, 0x81, 2, 0, 4, 0, //
    6, 0x30, 15, 0, 0, 0, //
    7, 5, 0x02, 2, 0, 4, 0, //
    6, 0x30, 15, 4, 0, 0,
];

#[test]
fn device_descriptor() {
    let b = [
        18, 1, 0x00, 0x02, 0, 0, 0, 8, 0x27, 0x06, 0x01, 0x00, 0x00, 0x01, 1, 4, 11, 1,
    ];
    let d = DeviceDescriptor::parse(&b).unwrap();
    assert_eq!((d.vendor, d.product, d.release), (0x0627, 0x0001, 0x0100));
    assert_eq!((d.manufacturer, d.product_string, d.serial), (1, 4, 11));
    assert_eq!(d.ep0_max_packet(), 8);
    let p = DeviceDescriptor::parse_prefix(&b[..8]).unwrap();
    assert_eq!((p.usb, p.max_packet0, p.vendor), (0x0200, 8, 0));
    assert_eq!(DeviceDescriptor::parse(&b[..17]), Err(Error::Short));
    let mut wrong = b;
    wrong[1] = 2;
    assert_eq!(DeviceDescriptor::parse(&wrong), Err(Error::WrongType(2)));
    // USB 3: an exponent.
    let mut ss = b;
    ss[2..4].copy_from_slice(&0x0320u16.to_le_bytes());
    ss[7] = 9;
    assert_eq!(DeviceDescriptor::parse(&ss).unwrap().ep0_max_packet(), 512);
}

#[test]
fn keyboard_configuration() {
    assert_eq!(configuration_total(&KEYBOARD_CONFIG[..9]), Ok(34));
    let c = Configuration::parse(&KEYBOARD_CONFIG).unwrap();
    assert_eq!((c.value, c.attributes, c.max_power), (1, 0xa0, 50));
    let i = c.interface(0, 0).unwrap();
    assert_eq!((i.class, i.subclass, i.protocol), (3, 1, 1));
    assert_eq!(i.extra, vec![(kind::HID, 18..27)]);
    let e = &i.endpoints[0];
    assert!(e.is_in());
    assert_eq!(e.number(), 1);
    assert_eq!(e.transfer_type(), TransferType::Interrupt);
    assert_eq!((e.max_packet(), e.interval), (8, 7));
    assert_eq!(c.defaults().count(), 1);
}

#[test]
fn disk_configuration_with_companions() {
    let c = Configuration::parse(&DISK_CONFIG).unwrap();
    let i = c.interface(0, 0).unwrap();
    assert_eq!((i.class, i.subclass, i.protocol), (8, 6, 0x50));
    let [a, b] = &i.endpoints[..] else {
        panic!("two endpoints")
    };
    assert_eq!((a.address, b.address), (0x81, 0x02));
    assert_eq!(a.max_packet(), 1024);
    assert_eq!(a.companion.unwrap().max_burst, 15);
    assert_eq!(a.companion.unwrap().max_streams(), 0);
    assert_eq!(b.companion.unwrap().max_streams(), 16);
}

#[test]
fn truncated_and_hostile_configurations() {
    // Truncated mid-descriptor: what fits is kept.
    let c = Configuration::parse(&KEYBOARD_CONFIG[..30]).unwrap();
    assert!(c.interface(0, 0).unwrap().endpoints.is_empty());
    // A zero length byte is an error, not a loop.
    let mut z = KEYBOARD_CONFIG;
    z[9] = 0;
    assert_eq!(Configuration::parse(&z), Err(Error::BadLength));
    // wTotalLength beyond the buffer is clamped.
    let mut long = KEYBOARD_CONFIG;
    long[2] = 0xff;
    assert_eq!(Configuration::parse(&long).unwrap().interfaces.len(), 1);
    // Endpoints before any interface are dropped.
    let mut e = vec![9u8, 2, 16, 0, 1, 1, 0, 0x80, 50];
    e.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 7]);
    assert!(Configuration::parse(&e).unwrap().interfaces.is_empty());
}

#[test]
fn fuzzed_descriptors_never_panic() {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..20_000 {
        let base: &[u8] = if round % 2 == 0 {
            &KEYBOARD_CONFIG
        } else {
            &DISK_CONFIG
        };
        let mut b: Vec<u8> = base.to_vec();
        for _ in 0..(rnd() % 6) {
            let i = (rnd() as usize) % b.len();
            b[i] = rnd() as u8;
        }
        b.truncate((rnd() as usize) % (b.len() + 1));
        let _ = Configuration::parse(&b);
        let _ = DeviceDescriptor::parse(&b);
        let _ = string(&b);
        let _ = bos(&b);
        let _ = HubDescriptor::parse(&b);
    }
}

#[test]
fn strings() {
    let b = [10, 3, b'Q', 0, b'E', 0, b'M', 0, b'U', 0];
    assert_eq!(string(&b).unwrap(), "QEMU");
    // An odd length drops the half unit; a lone surrogate is replaced.
    assert_eq!(string(&[5, 3, b'A', 0, b'B']).unwrap(), "A");
    assert_eq!(string(&[4, 3, 0x00, 0xd8]).unwrap(), "\u{fffd}");
    assert_eq!(languages(&[4, 3, 0x09, 0x04]).unwrap(), [LANG_EN_US]);
}

#[test]
fn binary_object_store() {
    let b = [
        5, 15, 22, 0, 2, //
        7, 16, 2, 2, 0, 0, 0, //
        10, 16, 3, 0, 0x0e, 0, 1, 10, 0xff, 0x07,
    ];
    assert_eq!(bos_total(&b), Ok(22));
    assert_eq!(
        bos(&b).unwrap(),
        [
            Capability::Usb2Extension { attributes: 2 },
            Capability::SuperSpeed {
                speeds: 0x0e,
                u1_exit: 10,
                u2_exit: 0x07ff,
            },
        ]
    );
}

#[test]
fn requests() {
    let s = Setup::get_descriptor(kind::DEVICE, 0, 0, 18);
    assert_eq!(
        (s.request_type, s.request, s.value, s.length),
        (0x80, 6, 0x0100, 18)
    );
    assert!(s.is_in());
    let s = Setup::get_descriptor(kind::STRING, 2, LANG_EN_US, 255);
    assert_eq!((s.value, s.index), (0x0302, 0x0409));
    let s = Setup::get_interface_descriptor(kind::REPORT, 0, 1, 63);
    assert_eq!((s.request_type, s.value, s.index), (0x81, 0x2200, 1));
    let s = Setup::clear_halt(0x81);
    assert_eq!((s.request_type, s.request, s.index), (0x02, 1, 0x81));
    assert!(!Setup::set_configuration(1).is_in());
}

#[test]
fn hubs() {
    let d = HubDescriptor::parse(&[9, 0x29, 4, 0x48, 0x00, 50, 100, 0, 0xff]).unwrap();
    assert_eq!(
        (d.ports, d.power_on_ms, d.tt_think_time(), d.ss),
        (4, 100, 2, None)
    );
    let ss = HubDescriptor::parse(&[12, 0x2a, 4, 0, 0, 50, 0, 1, 0x10, 0, 0, 0]).unwrap();
    assert_eq!(ss.ss, Some((1, 16)));
    let s = hub::get_port_status(3);
    assert_eq!((s.request_type, s.index, s.length), (0xa3, 3, 4));
    let s = hub::clear_port_feature(2, feature::C_PORT_RESET);
    assert_eq!((s.request_type, s.request, s.value), (0x23, 1, 20));

    // High-speed device connected and enabled, connect and reset changed.
    let p = PortStatus::parse([0x03, 0x05, 0x11, 0x00], false);
    assert!(p.connected() && p.enabled() && p.powered());
    assert_eq!(p.speed(), PortSpeed::High);
    let c: Vec<u16> = p.changes().collect();
    assert_eq!(c, [feature::C_PORT_CONNECTION, feature::C_PORT_RESET]);
    // SuperSpeed hub port: power is bit 9, warm reset change bit 5.
    let p = PortStatus::parse([0x01, 0x02, 0x20, 0x00], true);
    assert!(p.powered());
    assert_eq!(p.speed(), PortSpeed::Super);
    assert_eq!(p.changes().collect::<Vec<_>>(), [feature::C_BH_PORT_RESET]);
}

#[test]
fn route_strings() {
    // Root port's hub (depth 0) port 3, then that hub's port 12.
    let r = route(0, 0, 3);
    assert_eq!(r, 0x3);
    assert_eq!(route(r, 1, 12), 0xc3);
    assert_eq!(route(0, 0, 20), 0xf);
    assert_eq!(route(0x1_2345, 5, 1), 0x1_2345);
}
