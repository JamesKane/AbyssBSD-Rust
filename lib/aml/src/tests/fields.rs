// SPDX-License-Identifier: BSD-2-Clause
//! Operation regions, field units, and buffer fields.

extern crate std;

use alloc::vec::Vec;

use super::util::*;
use crate::{AmlPath, Config, Error, PciAddress, RegionSpace, Value};

const MEM: RegionSpace = RegionSpace::SystemMemory;

/// `OperationRegion (name, space, base, len)`.
fn region(n: &str, space: u8, base: u64, len: u64) -> Vec<u8> {
    cat(&[&[0x5b, 0x80], &name(n), &[space], &int(base), &int(len)])
}

/// One field list element.
enum F<'a> {
    Named(&'a str, usize),
    Skip(usize),
    AccessAs(u8, u8),
    Connect(&'a str),
}

fn field_list(list: &[F<'_>]) -> Vec<u8> {
    let mut v = Vec::new();
    for f in list {
        match *f {
            F::Named(n, bits) => {
                let mut seg = [b'_'; 4];
                seg[..n.len()].copy_from_slice(n.as_bytes());
                v.extend_from_slice(&seg);
                v.extend(field_len(bits));
            }
            F::Skip(bits) => {
                v.push(0);
                v.extend(field_len(bits));
            }
            F::AccessAs(t, a) => v.extend_from_slice(&[1, t, a]),
            F::Connect(n) => {
                v.push(2);
                v.extend(name(n));
            }
        }
    }
    v
}

/// A field bit length: a package-length encoding of the raw value.
fn field_len(bits: usize) -> Vec<u8> {
    if bits < 0x40 {
        std::vec![u8::try_from(bits).unwrap()]
    } else {
        std::vec![
            0x40 | u8::try_from(bits & 0xf).unwrap(),
            u8::try_from(bits >> 4).unwrap()
        ]
    }
}

fn field(region: &str, flags: u8, list: &[F<'_>]) -> Vec<u8> {
    pkg(
        &[0x5b, 0x81],
        &cat(&[&name(region), &[flags], &field_list(list)]),
    )
}

const BYTE: u8 = 1;
const DWORD: u8 = 3;
const ANY: u8 = 0;
const BUFFER_ACC: u8 = 5;
const LOCK: u8 = 0x10;
const ONES: u8 = 0x20;
const ZEROS: u8 = 0x40;

fn read_method(n: &str, f: &str) -> Vec<u8> {
    method(n, 0, &ret(&name(f)))
}

fn write_method(n: &str, f: &str) -> Vec<u8> {
    method(n, 1, &store(&[ARG0], &name(f)))
}

fn accesses(h: &FakeHost) -> Vec<(bool, u64, u64, u64)> {
    h.log
        .iter()
        .map(|a| (a.write, a.address, a.bytes, a.value))
        .collect()
}

#[test]
fn byte_fields_and_preserve() {
    let aml = cat(&[
        &region("REG", 0, 0x1000, 0x10),
        &field(
            "REG",
            BYTE,
            &[
                F::Named("F0", 8),
                F::Named("F1", 4),
                F::Named("F2", 4),
                F::Named("F3", 16),
            ],
        ),
        &read_method("RF1", "F1"),
        &read_method("RF2", "F2"),
        &read_method("RF3", "F3"),
        &write_method("WF1", "F1"),
    ]);
    let (mut i, mut h) = load(&aml);
    h.poke(MEM, 0x1000, &[0x12, 0xab, 0x78, 0x56]);
    assert_eq!(eval_int(&mut i, &mut h, "\\RF1"), 0xb);
    assert_eq!(eval_int(&mut i, &mut h, "\\RF2"), 0xa);
    h.log.clear();
    assert_eq!(eval_int(&mut i, &mut h, "\\RF3"), 0x5678);
    assert_eq!(
        accesses(&h),
        [(false, 0x1002, 1, 0x78), (false, 0x1003, 1, 0x56)]
    );
    h.log.clear();
    eval(&mut i, &mut h, "\\WF1", std::vec![Value::Integer(0x15)]);
    // Read-modify-write of one byte; only the low four bits are taken.
    assert_eq!(
        accesses(&h),
        [(false, 0x1001, 1, 0xab), (true, 0x1001, 1, 0xa5)]
    );
}

#[test]
fn fields_spanning_access_units() {
    let aml = cat(&[
        &region("REG", 0, 0x1000, 0x10),
        &field("REG", DWORD, &[F::Skip(24), F::Named("G0", 16)]),
        &read_method("RG0", "G0"),
        &write_method("WG0", "G0"),
    ]);
    let (mut i, mut h) = load(&aml);
    h.poke(
        MEM,
        0x1000,
        &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
    );
    assert_eq!(eval_int(&mut i, &mut h, "\\RG0"), 0x5544);
    assert_eq!(
        accesses(&h),
        [
            (false, 0x1000, 4, 0x4433_2211),
            (false, 0x1004, 4, 0x8877_6655)
        ]
    );
    h.log.clear();
    eval(&mut i, &mut h, "\\WG0", std::vec![Value::Integer(0xbeef)]);
    assert_eq!(
        accesses(&h),
        [
            (false, 0x1000, 4, 0x4433_2211),
            (true, 0x1000, 4, 0xef33_2211),
            (false, 0x1004, 4, 0x8877_6655),
            (true, 0x1004, 4, 0x8877_66be),
        ]
    );
}

#[test]
fn update_rules() {
    let aml = cat(&[
        &region("REG", 0, 0x1000, 0x10),
        &field("REG", BYTE | ONES, &[F::Skip(64), F::Named("H0", 4)]),
        &field(
            "REG",
            BYTE | ZEROS,
            &[F::Skip(72), F::Skip(4), F::Named("H1", 4)],
        ),
        &write_method("WH0", "H0"),
        &write_method("WH1", "H1"),
    ]);
    let (mut i, mut h) = load(&aml);
    h.poke(MEM, 0x1008, &[0x5a, 0x5a]);
    eval(&mut i, &mut h, "\\WH0", std::vec![Value::Integer(3)]);
    eval(&mut i, &mut h, "\\WH1", std::vec![Value::Integer(3)]);
    // No reads: the other bits are ones, then zeros.
    assert_eq!(
        accesses(&h),
        [(true, 0x1008, 1, 0xf3), (true, 0x1009, 1, 0x30)]
    );
}

#[test]
fn any_access_picks_a_width() {
    let aml = cat(&[
        &region("REG", 0, 0x1000, 0x10),
        &field(
            "REG",
            ANY,
            &[
                F::Skip(8),
                F::Named("A1", 16),
                F::Skip(8),
                F::Named("A4", 32),
            ],
        ),
        &read_method("RA1", "A1"),
        &read_method("RA4", "A4"),
    ]);
    let (mut i, mut h) = load(&aml);
    eval(&mut i, &mut h, "\\RA1", Vec::new());
    eval(&mut i, &mut h, "\\RA4", Vec::new());
    // A1 (bits 8..24) fits one aligned dword; A4 is one aligned dword.
    assert_eq!(accesses(&h), [(false, 0x1000, 4, 0), (false, 0x1004, 4, 0)]);
}

#[test]
fn wide_fields_are_buffers() {
    let aml = cat(&[
        &region("REG", 0, 0x1000, 0x20),
        &field(
            "REG",
            BYTE,
            &[
                F::Named("BIG", 96),
                F::Named("W40", 40),
                F::Named("W32", 32),
            ],
        ),
        &read_method("RBIG", "BIG"),
        &read_method("RW40", "W40"),
        &read_method("RW32", "W32"),
        &write_method("WBIG", "BIG"),
    ]);
    let (mut i, mut h) = load(&aml);
    let bytes: Vec<u8> = (1..=12).collect();
    h.poke(MEM, 0x1000, &bytes);
    assert_eq!(
        eval(&mut i, &mut h, "\\RBIG", Vec::new()),
        Value::Buffer(bytes)
    );
    assert_eq!(
        eval(&mut i, &mut h, "\\RW40", Vec::new()),
        Value::Integer(0)
    );
    eval(
        &mut i,
        &mut h,
        "\\WBIG",
        std::vec![Value::Buffer(std::vec![0xee; 3])],
    );
    assert_eq!(h.peek(MEM, 0x1000, 4), 0x00ee_eeee);
    // With 32-bit integers, a 40-bit field is a buffer.
    let (mut i, mut h) = load_rev(&aml, 1);
    assert_eq!(
        eval(&mut i, &mut h, "\\RW40", Vec::new()),
        Value::Buffer(std::vec![0; 5])
    );
    assert_eq!(
        eval(&mut i, &mut h, "\\RW32", Vec::new()),
        Value::Integer(0)
    );
}

#[test]
fn index_and_bank_fields() {
    let index_field = pkg(
        &[0x5b, 0x86],
        &cat(&[
            &name("IDX"),
            &name("DAT"),
            &[BYTE],
            &field_list(&[F::Skip(0x80), F::Named("R10", 8), F::Named("R11", 8)]),
        ]),
    );
    let bank_field = pkg(
        &[0x5b, 0x87],
        &cat(&[
            &name("BREG"),
            &name("BSEL"),
            &int(2),
            &[BYTE],
            &field_list(&[F::Skip(32), F::Named("BF0", 8)]),
        ]),
    );
    let aml = cat(&[
        &region("IOR", 1, 0x70, 2),
        &field("IOR", BYTE, &[F::Named("IDX", 8), F::Named("DAT", 8)]),
        &index_field,
        &region("BREG", 0, 0x2000, 0x10),
        &field("BREG", BYTE, &[F::Named("BSEL", 8)]),
        &bank_field,
        &read_method("RR11", "R11"),
        &write_method("WR10", "R10"),
        &read_method("RBF0", "BF0"),
    ]);
    let (mut i, mut h) = load(&aml);
    h.poke(RegionSpace::SystemIo, 0x71, &[0x42]);
    assert_eq!(eval_int(&mut i, &mut h, "\\RR11"), 0x42);
    assert_eq!(
        accesses(&h),
        [(true, 0x70, 1, 0x11), (false, 0x71, 1, 0x42)]
    );
    h.log.clear();
    eval(&mut i, &mut h, "\\WR10", std::vec![Value::Integer(0x5a)]);
    assert_eq!(accesses(&h), [(true, 0x70, 1, 0x10), (true, 0x71, 1, 0x5a)]);
    h.log.clear();
    eval(&mut i, &mut h, "\\RBF0", Vec::new());
    assert_eq!(accesses(&h), [(true, 0x2000, 1, 2), (false, 0x2004, 1, 0)]);
    assert!(h.log.iter().all(|a| a.space != RegionSpace::SystemCmos));
}

#[test]
fn lock_rule_takes_the_global_lock() {
    let aml = cat(&[
        &region("REG", 0, 0x1000, 0x10),
        &field("REG", BYTE | LOCK, &[F::Named("LK", 8)]),
        &read_method("RLK", "LK"),
        &write_method("WLK", "LK"),
    ]);
    let (mut i, mut h) = load(&aml);
    eval(&mut i, &mut h, "\\RLK", Vec::new());
    eval(&mut i, &mut h, "\\WLK", std::vec![Value::Integer(1)]);
    assert_eq!((h.gl_acquires, h.gl_releases), (2, 2));
}

#[test]
fn region_limits() {
    let aml = cat(&[
        &region("SMAL", 0, 0x3000, 2),
        &field("SMAL", BYTE, &[F::Skip(16), F::Named("OUT", 8)]),
        &region("ODD", 0, 0x3100, 3),
        &field("ODD", DWORD, &[F::Named("X3", 8)]),
        &read_method("ROUT", "OUT"),
        &read_method("RX3", "X3"),
    ]);
    let (mut i, mut h) = load(&aml);
    assert_eq!(
        i.evaluate_path("\\ROUT", Vec::new(), &mut h),
        Err(Error::RegionLimit)
    );
    // Slack mode lets a dword access cover a 3-byte region.
    assert_eq!(eval_int(&mut i, &mut h, "\\RX3"), 0);
    let strict = Config {
        slack: false,
        ..test_config()
    };
    let (mut i, mut h) = load_cfg(&aml, 2, strict);
    assert_eq!(
        i.evaluate_path("\\RX3", Vec::new(), &mut h),
        Err(Error::RegionLimit)
    );
}

#[test]
fn pci_config_addresses() {
    let cfg_region = |n: &str| {
        cat(&[
            &region(n, 2, 0, 0x100),
            &field(n, DWORD, &[F::Named("VID", 16), F::Named("DID", 16)]),
        ])
    };
    let aml = device(
        "\\_SB.PCI0",
        &cat(&[
            &name_op("_HID", &int(0x080a_d041)),
            &name_op("_BBN", &int(2)),
            &name_op("_SEG", &int(1)),
            &device(
                "DEV1",
                &cat(&[
                    &name_op("_ADR", &int(0x0003_0001)),
                    &cfg_region("CFG"),
                    &read_method("RVID", "VID"),
                ]),
            ),
            &device(
                "BRG0",
                &cat(&[
                    &name_op("_ADR", &int(0x0001_0000)),
                    &device(
                        "EP",
                        &cat(&[
                            &name_op("_ADR", &int(0)),
                            &cfg_region("ECFG"),
                            &read_method("RDID", "DID"),
                        ]),
                    ),
                ]),
            ),
        ]),
    );
    let (mut i, mut h) = load(&aml);
    let key = |p: PciAddress, off: u64| {
        (
            2u8,
            (u64::from(p.segment) << 48)
                | (u64::from(p.bus) << 40)
                | (u64::from(p.device) << 32)
                | (u64::from(p.function) << 24)
                | off,
        )
    };
    let dev1 = PciAddress {
        segment: 1,
        bus: 2,
        device: 3,
        function: 1,
    };
    h.mem.insert(key(dev1, 0), 0x86);
    h.mem.insert(key(dev1, 1), 0x80);
    assert_eq!(eval_int(&mut i, &mut h, "\\_SB.PCI0.DEV1.RVID"), 0x8086);
    // The bridge at 2:1.0 is type 1 with secondary bus 5.
    let brg = PciAddress {
        segment: 1,
        bus: 2,
        device: 1,
        function: 0,
    };
    h.mem.insert(key(brg, 0x0e), 0x01);
    h.mem.insert(key(brg, 0x19), 0x05);
    let ep = PciAddress {
        segment: 1,
        bus: 5,
        device: 0,
        function: 0,
    };
    h.mem.insert(key(ep, 2), 0x34);
    h.mem.insert(key(ep, 3), 0x12);
    assert_eq!(eval_int(&mut i, &mut h, "\\_SB.PCI0.BRG0.EP.RDID"), 0x1234);
}

#[test]
fn embedded_controller_and_cmos_pass_through() {
    let aml = device(
        "\\_SB.EC0",
        &cat(&[
            &region("ECOR", 3, 0, 0x100),
            &field("ECOR", BYTE, &[F::Skip(0x80), F::Named("TMP", 8)]),
            &write_method("WTMP", "TMP"),
        ]),
    );
    let (mut i, mut h) = load(&aml);
    eval(
        &mut i,
        &mut h,
        "\\_SB.EC0.WTMP",
        std::vec![Value::Integer(0x33)],
    );
    let a = h.log[0];
    assert_eq!(
        (a.space, a.address, a.write, a.value),
        (RegionSpace::EmbeddedControl, 0x10, true, 0x33)
    );
}

#[test]
fn smbus_fields_transfer_buffers() {
    let aml = cat(&[
        &region("SMB", 4, 0, 0x100),
        &field(
            "SMB",
            BUFFER_ACC,
            &[
                F::Skip(0x0b * 8),
                F::AccessAs(BUFFER_ACC, 0x08),
                F::Named("SBW", 8),
            ],
        ),
        &read_method("RSB", "SBW"),
        &method("WSB", 1, &ret(&store(&[ARG0], &name("SBW")))),
    ]);
    let (mut i, mut h) = load(&aml);
    let r = eval(&mut i, &mut h, "\\RSB", Vec::new());
    assert_eq!(r, Value::Buffer(std::vec![0, 2, 0x34, 0x12]));
    let r = eval(
        &mut i,
        &mut h,
        "\\WSB",
        std::vec![Value::Buffer(std::vec![0, 2, 1, 2])],
    );
    // Store returns the transfer's result buffer.
    assert_eq!(r, Value::Buffer(std::vec![0, 2, 0x34, 0x12]));
    assert_eq!(
        h.transfers,
        [(0x0b, None), (0x0b, Some(std::vec![0, 2, 1, 2]))]
    );
}

#[test]
fn gpio_fields_use_the_connection() {
    // GpioIo (Exclusive, PullUp, 0, 0, IoRestrictionNone, "\\_SB.GPI0")
    // { 7, 8, 9 }, hand-assembled.
    let gpio_io = {
        let mut d = std::vec![0x8c, 0, 0, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0];
        let pin_off = 23u16;
        let name_off = pin_off + 6;
        let src = b"\\_SB.GPI0\0";
        let vendor_off = name_off + u16::try_from(src.len()).unwrap();
        d.extend_from_slice(&pin_off.to_le_bytes());
        d.push(0);
        d.extend_from_slice(&name_off.to_le_bytes());
        d.extend_from_slice(&vendor_off.to_le_bytes());
        d.extend_from_slice(&0u16.to_le_bytes());
        for p in [7u16, 8, 9] {
            d.extend_from_slice(&p.to_le_bytes());
        }
        d.extend_from_slice(src);
        let len = u16::try_from(d.len() - 3).unwrap();
        d[1..3].copy_from_slice(&len.to_le_bytes());
        d.extend_from_slice(&[0x79, 0]);
        d
    };
    let aml = cat(&[
        &name_op("GPB", &buffer(&gpio_io)),
        &region("GPO", 8, 0, 1),
        &field(
            "GPO",
            BYTE,
            &[F::Connect("GPB"), F::Skip(1), F::Named("P1", 2)],
        ),
        &read_method("RP1", "P1"),
        &write_method("WP1", "P1"),
    ]);
    let (mut i, mut h) = load(&aml);
    eval(&mut i, &mut h, "\\WP1", std::vec![Value::Integer(2)]);
    assert_eq!(h.gpio.get(&1), Some(&false));
    assert_eq!(h.gpio.get(&2), Some(&true));
    assert_eq!(eval_int(&mut i, &mut h, "\\RP1"), 2);
    // A GPIO field without a connection is an error.
    let aml = cat(&[
        &region("GPO", 8, 0, 1),
        &field("GPO", BYTE, &[F::Named("P0", 1)]),
        &read_method("RP0", "P0"),
    ]);
    let (mut i, mut h) = load(&aml);
    assert_eq!(
        i.evaluate_path("\\RP0", Vec::new(), &mut h),
        Err(Error::NoConnection)
    );
}

#[test]
fn buffer_fields() {
    let create =
        |op: &[u8], src: &[u8], index: u64, n: &str| cat(&[op, src, &int(index), &name(n)]);
    let aml = cat(&[
        &name_op(
            "BUF",
            &buffer(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]),
        ),
        &create(&[0x8d], &name("BUF"), 4, "BIT4"),
        &create(&[0x8c], &name("BUF"), 1, "BYT1"),
        &create(&[0x8b], &name("BUF"), 2, "WRD2"),
        &create(&[0x8a], &name("BUF"), 4, "DW4"),
        &create(&[0x8f], &name("BUF"), 0, "QW0"),
        &cat(&[&[0x5b, 0x13], &name("BUF"), &int(4), &int(12), &name("ODD")]),
        &method(
            "RD",
            0,
            &ret(&cat(&[&[0x73], &name("WRD2"), &name("ODD"), &[0]])),
        ),
        &read_method("RQW", "QW0"),
        &read_method("RBIT", "BIT4"),
        &read_method("RDW", "DW4"),
        &write_method("WODD", "ODD"),
        &write_method("WBYT", "BYT1"),
        // Fields over a local buffer, as _CRS methods build templates.
        &method(
            "LOC",
            0,
            &cat(&[
                &store(&buffer(&[0; 4]), &[LOCAL0]),
                &create(&[0x8b], &[LOCAL0], 1, "LW"),
                &store(&int(0xabcd), &name("LW")),
                &ret(&[LOCAL0]),
            ]),
        ),
        &method("OOB", 0, &create(&[0x8a], &name("BUF"), 6, "BAD")),
    ]);
    let (mut i, mut h) = load(&aml);
    assert_eq!(eval_int(&mut i, &mut h, "\\RQW"), 0x8877_6655_4433_2211);
    assert_eq!(eval_int(&mut i, &mut h, "\\RBIT"), 1);
    assert_eq!(eval_int(&mut i, &mut h, "\\RDW"), 0x8877_6655);
    // WRD2 = 0x4433 and ODD (bits 4..16) = 0x221 concatenate as integers.
    let Value::Buffer(b) = eval(&mut i, &mut h, "\\RD", Vec::new()) else {
        panic!()
    };
    assert_eq!(&b[..2], &[0x33, 0x44]);
    assert_eq!(&b[8..10], &[0x21, 0x02]);
    eval(&mut i, &mut h, "\\WODD", std::vec![Value::Integer(0xfff)]);
    eval(&mut i, &mut h, "\\WBYT", std::vec![Value::Integer(0x1aa)]);
    let Value::Buffer(b) = eval(&mut i, &mut h, "\\BUF", Vec::new()) else {
        panic!()
    };
    assert_eq!(&b[..3], &[0xf1, 0xaa, 0x33]);
    assert_eq!(
        eval(&mut i, &mut h, "\\LOC", Vec::new()),
        Value::Buffer(std::vec![0, 0xcd, 0xab, 0])
    );
    assert_eq!(
        i.evaluate_path("\\OOB", Vec::new(), &mut h),
        Err(Error::BufferLimit)
    );
    assert!(i.lookup(&AmlPath::parse("\\LW").unwrap()).is_none());
}
