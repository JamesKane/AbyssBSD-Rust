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

#[test]
fn uas_command_and_task_ius() {
    let u = uas::command(0x0102, 0, &Cdb::test_unit_ready());
    assert_eq!(&u[..4], [1, 0, 1, 2]);
    assert_eq!(&u[8..16], [0; 8]);
    assert_eq!(&u[16..22], [0; 6]);
    let u = uas::command(7, 3, &Cdb::rw(false, 0x10, 2));
    assert_eq!(u[9], 3);
    assert_eq!(&u[16..26], Cdb::rw(false, 0x10, 2).as_bytes());
    let u = uas::command(1, 0x123, &Cdb::test_unit_ready());
    assert_eq!(&u[8..10], [0x41, 0x23]);
    let t = uas::task(9, 0, uas::Function::AbortTask, 4);
    assert_eq!(t, [5, 0, 0, 9, 1, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(uas::task(9, 0, uas::Function::LogicalUnitReset, 0)[4], 8);
}

#[test]
fn uas_status_ius() {
    use uas::{Response, Status};
    // A sense IU: CHECK CONDITION with fixed-format sense, UNIT ATTENTION.
    let mut b = [0u8; 34];
    b[0] = 3;
    b[3] = 5;
    b[6] = status::CHECK_CONDITION;
    b[15] = 18;
    b[16] = 0x70;
    b[18] = key::UNIT_ATTENTION;
    b[28] = 0x29;
    let s = Status::parse(&b).unwrap();
    let Status::Sense { tag, status, sense } = s else {
        panic!("{s:?}");
    };
    assert_eq!((tag, status), (5, status::CHECK_CONDITION));
    assert_eq!(sense.unwrap().key, key::UNIT_ATTENTION);
    assert_eq!(sense.unwrap().asc, 0x29);
    // GOOD without sense data.
    let g = [3, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        Status::parse(&g),
        Some(Status::Sense {
            tag: 1,
            status: status::GOOD,
            sense: None
        })
    );
    // A sense length past the IU: what is there is used.
    let mut l = b;
    l[14] = 0xff;
    assert!(matches!(
        Status::parse(&l),
        Some(Status::Sense { sense: Some(_), .. })
    ));
    assert_eq!(
        Status::parse(&[4, 0, 0, 2, 0, 0, 0, 0]),
        Some(Status::Response {
            tag: 2,
            code: Response::Complete
        })
    );
    assert_eq!(
        Status::parse(&[4, 0, 0, 2, 0, 0, 0, 5]).map(|s| s.tag()),
        Some(2)
    );
    assert_eq!(Status::parse(&[6, 0, 0, 3]), Some(Status::ReadReady(3)));
    assert_eq!(Status::parse(&[7, 0, 1, 0]), Some(Status::WriteReady(256)));
    // Garbage and short IUs.
    assert_eq!(Status::parse(&[]), None);
    assert_eq!(Status::parse(&[3, 0, 0]), None);
    assert_eq!(Status::parse(&[3, 0, 0, 1, 0, 0, 0]), None);
    assert_eq!(Status::parse(&[9, 0, 0, 1]), None);
    assert_eq!(uas::Pipe::parse(&[2, 0]), Some(uas::Pipe::Status));
    assert_eq!(uas::Pipe::parse(&[5]), None);
}

#[test]
fn uas_status_parse_survives_any_bytes() {
    let mut x: u32 = 0x1234_5678;
    for n in 0..2000usize {
        let mut b = [0u8; 40];
        for v in &mut b {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *v = x.to_le_bytes()[0];
        }
        b[0] = [3, 4, 6, 7][n % 4];
        let _ = uas::Status::parse(&b[..n % 41]);
    }
}

#[test]
fn report_luns_data() {
    let c = Cdb::report_luns(256);
    assert_eq!(c.as_bytes(), [0xa0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]);
    let mut b = [0u8; 8 + 4 * 8];
    b[3] = 32;
    b[8 + 9] = 1; // LUN 1
    b[8 + 16] = 0x41; // flat 0x123
    b[8 + 17] = 0x23;
    b[8 + 24] = 0x80; // extended form: skipped
    let l: std::vec::Vec<u16> = report_luns(&b).collect();
    assert_eq!(l, [0, 1, 0x123]);
    // A list length past the data, and garbage, are bounded.
    b[0] = 0xff;
    assert_eq!(report_luns(&b).count(), 3);
    assert_eq!(report_luns(&b[..10]).count(), 0);
    assert_eq!(report_luns(&[]).count(), 0);
}
