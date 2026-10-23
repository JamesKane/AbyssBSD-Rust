// SPDX-License-Identifier: BSD-2-Clause
extern crate std;

use crate::bbb::{self, Csw, Status};
use crate::*;

#[test]
fn read_write_pick_the_short_form_when_it_reaches() {
    let c = Cdb::rw(false, 0x1234, 8);
    assert_eq!(c.as_bytes(), [0x28, 0, 0, 0, 0x12, 0x34, 0, 0, 8, 0]);
    let c = Cdb::rw(true, 1 << 32, 8);
    assert_eq!(c.as_bytes().len(), 16);
    assert_eq!(c.as_bytes()[0], op::WRITE_16);
    assert_eq!(&c.as_bytes()[2..10], &(1u64 << 32).to_be_bytes());
    // A 10-byte READ cannot end past 2^32 blocks.
    assert_eq!(Cdb::rw(false, u64::from(u32::MAX), 2).as_bytes().len(), 16);
    assert_eq!(Cdb::rw(false, 0, 70_000).as_bytes().len(), 16);
}

#[test]
fn inquiry_and_capacity() {
    let mut b = [0u8; 36];
    b[1] = 0x80;
    b[2] = 5;
    b[8..16].copy_from_slice(b"QEMU    ");
    b[16..32].copy_from_slice(b"QEMU HARDDISK   ");
    b[32..36].copy_from_slice(b"2.5+");
    let i = Inquiry::parse(&b).unwrap();
    assert_eq!(i.device_type, DeviceType::Direct);
    assert!(i.removable && !i.absent);
    assert_eq!(
        (ident(&i.vendor), ident(&i.product), ident(&i.revision)),
        ("QEMU", "QEMU HARDDISK", "2.5+")
    );
    assert_eq!(Inquiry::parse(&b[..35]), None);
    let c = Capacity::parse_10(&[0, 0, 0xff, 0xff, 0, 0, 2, 0]).unwrap();
    assert_eq!((c.blocks, c.block_size), (65536, 512));
    assert_eq!(Capacity::parse_10(&[0xff; 8]), None);
    let mut b16 = [0u8; 32];
    b16[0..8].copy_from_slice(&0x1_0000_0000u64.to_be_bytes());
    b16[8..12].copy_from_slice(&4096u32.to_be_bytes());
    assert_eq!(
        Capacity::parse_16(&b16),
        Some(Capacity {
            blocks: 0x1_0000_0001,
            block_size: 4096
        })
    );
}

#[test]
fn sense_formats() {
    let mut fixed = [0u8; 18];
    fixed[0] = 0x70;
    fixed[2] = key::NOT_READY;
    fixed[12] = 0x3a;
    let s = Sense::parse(&fixed).unwrap();
    assert!(s.no_medium());
    let desc = [0x72, key::UNIT_ATTENTION, 0x29, 0x00];
    assert_eq!(
        Sense::parse(&desc),
        Some(Sense {
            key: key::UNIT_ATTENTION,
            asc: 0x29,
            ascq: 0
        })
    );
    assert_eq!(Sense::parse(&[0x00, 1, 2]), None);
}

#[test]
fn bulk_only_wrappers() {
    let w = bbb::cbw(7, 512, true, 0, &Cdb::rw(false, 0, 1));
    assert_eq!(&w[0..4], b"USBC");
    assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 7);
    assert_eq!(u32::from_le_bytes(w[8..12].try_into().unwrap()), 512);
    assert_eq!((w[12], w[13], w[14], w[15]), (0x80, 0, 10, 0x28));
    let mut csw = [0u8; 13];
    csw[0..4].copy_from_slice(b"USBS");
    csw[4..8].copy_from_slice(&7u32.to_le_bytes());
    csw[8..12].copy_from_slice(&12u32.to_le_bytes());
    csw[12] = 1;
    assert_eq!(
        Csw::parse(&csw),
        Some(Csw {
            tag: 7,
            residue: 12,
            status: Status::Failed
        })
    );
    csw[12] = 2;
    assert_eq!(Csw::parse(&csw).unwrap().status, Status::Phase);
    csw[0] = b'X';
    assert_eq!(Csw::parse(&csw), None);
    assert_eq!(Csw::parse(&csw[..12]), None);
}

#[test]
fn vpd_inquiry_and_pages() {
    assert_eq!(
        Cdb::inquiry_vpd(vpd::UNIT_SERIAL, vpd::LEN).as_bytes(),
        &[op::INQUIRY, 1, 0x80, 0, 255, 0]
    );
    let list = [0, 0, 0, 3, 0x00, 0x80, 0x83];
    assert!(vpd::supports(&list, vpd::UNIT_SERIAL));
    assert!(!vpd::supports(&list, 0xb0));
    // A list that claims more than it holds is cut to what is there.
    assert!(vpd::supports(&[0, 0, 0, 200, 0x80], vpd::UNIT_SERIAL));
    assert!(!vpd::supports(&[0, 0x80, 0, 1, 0x80], vpd::UNIT_SERIAL));
}

#[test]
fn vpd_serial_is_trimmed() {
    let page = |s: &[u8]| {
        let mut b = std::vec![0, 0x80, 0, u8::try_from(s.len()).unwrap()];
        b.extend_from_slice(s);
        b
    };
    assert_eq!(vpd::serial(&page(b"  abyssbbb  ")), Some(&b"abyssbbb"[..]));
    assert_eq!(vpd::serial(&page(b"    ")), None);
    assert_eq!(vpd::serial(&page(b"")), None);
    assert_eq!(vpd::serial(&[0, 0x83, 0, 1, b'x']), None);
}
