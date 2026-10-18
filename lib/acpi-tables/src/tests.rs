// SPDX-License-Identifier: BSD-2-Clause

extern crate std;
use std::vec::Vec;

use crate::testutil::{Mem, gas};
use crate::*;

fn spcr_body(interface: u8, g: [u8; 12]) -> Vec<u8> {
    let mut body = std::vec![0u8; 80 - 36];
    body[0] = interface;
    body[4..16].copy_from_slice(&g);
    body[22] = 7; // 115200
    body
}

fn dbg2_body(subtype: u16, g: [u8; 12], port_type: u16) -> Vec<u8> {
    // Header: info offset, count; then one device of 22 + 12 + 4 bytes.
    let mut body = Vec::new();
    body.extend_from_slice(&44u32.to_le_bytes());
    body.extend_from_slice(&2u32.to_le_bytes());
    for (ty, st) in [(0x8001u16, 0u16), (port_type, subtype)] {
        let mut dev = std::vec![0u8; 22 + 12 + 4];
        dev[0] = 0;
        dev[1..3].copy_from_slice(&38u16.to_le_bytes());
        dev[3] = 1;
        dev[12..14].copy_from_slice(&ty.to_le_bytes());
        dev[14..16].copy_from_slice(&st.to_le_bytes());
        dev[18..20].copy_from_slice(&22u16.to_le_bytes());
        dev[20..22].copy_from_slice(&34u16.to_le_bytes());
        dev[22..34].copy_from_slice(&g);
        body.extend_from_slice(&dev);
    }
    body
}

fn firmware(tables: &[(&[u8; 4], Vec<u8>)]) -> Mem {
    let mut m = Mem::new();
    let mut xsdt = Vec::new();
    for (i, (sig, body)) in tables.iter().enumerate() {
        let at = 0x400 + i * 0x200;
        m.table(at, sig, 2, body);
        xsdt.extend_from_slice(&u64::try_from(at).unwrap().to_le_bytes());
    }
    m.table(0x100, b"XSDT", 1, &xsdt);
    m.rsdp(0x40, 0x100);
    m
}

#[test]
fn finds_spcr_pl011() {
    let m = firmware(&[
        (b"FACP", std::vec![0; 8]),
        (b"SPCR", spcr_body(3, gas(0, 32, 3, 0x0900_0000))),
    ]);
    let t = Tables::new(&m, 0x40).unwrap();
    assert_eq!(&t.oem_id, b"ABYSS ");
    assert_eq!(t.iter().count(), 2);
    let spcr = spcr::parse(&t.find(*b"SPCR").unwrap()).unwrap();
    assert_eq!(spcr.baud, Some(115_200));
    assert_eq!(spcr.port.kind, UartKind::Pl011);
    assert_eq!(spcr.port.base.space, AddressSpace::Memory);
    assert_eq!(spcr.port.base.address, 0x0900_0000);
    assert_eq!(spcr.port.base.access_bytes(), Some(4));
    assert_eq!(console_uart(&t), Some(spcr.port));
}

#[test]
fn falls_back_to_dbg2() {
    let m = firmware(&[(b"DBG2", dbg2_body(0x12, gas(1, 8, 1, 0x3f8), 0x8000))]);
    let t = Tables::new(&m, 0x40).unwrap();
    let port = console_uart(&t).unwrap();
    assert_eq!(port.kind, UartKind::Ns16550);
    assert_eq!(port.base.space, AddressSpace::Io);
    assert_eq!(port.base.address, 0x3f8);
    // A DBG2 with no serial port gives nothing.
    let m = firmware(&[(b"DBG2", dbg2_body(0x12, gas(1, 8, 1, 0x3f8), 0x8002))]);
    assert_eq!(console_uart(&Tables::new(&m, 0x40).unwrap()), None);
}

#[test]
fn rejects_bad_checksums_and_signatures() {
    let mut m = firmware(&[(b"SPCR", spcr_body(0, gas(1, 8, 1, 0x3f8)))]);
    m.0[0x400 + 50] ^= 1;
    let t = Tables::new(&m, 0x40).unwrap();
    assert_eq!(t.iter().next(), Some(Err(Error::BadChecksum(*b"SPCR"))));
    assert_eq!(t.find(*b"SPCR"), None);

    let mut m = firmware(&[]);
    m.0[0x40] = b'X';
    assert_eq!(Tables::new(&m, 0x40).unwrap_err(), Error::BadSignature);
    let mut m = firmware(&[]);
    m.0[0x40 + 30] ^= 1;
    assert_eq!(
        Tables::new(&m, 0x40).unwrap_err(),
        Error::BadChecksum(*b"RSD ")
    );
    let m = firmware(&[]);
    assert_eq!(
        Tables::new(&m, 0x1ff0).unwrap_err(),
        Error::Unreachable(0x1ff0)
    );
}

#[test]
fn malformed_dbg2_entry_ends_walk() {
    let mut body = dbg2_body(0x0e, gas(0, 32, 3, 0x1000), 0x8000);
    // First device claims a length past the end of the table.
    body[8 + 1..8 + 3].copy_from_slice(&0xfffu16.to_le_bytes());
    let m = firmware(&[(b"DBG2", body)]);
    let t = Tables::new(&m, 0x40).unwrap();
    let ports: Vec<_> = dbg2::serial_ports(&t.find(*b"DBG2").unwrap()).collect();
    assert_eq!(ports, [Err(Error::Malformed("DBG2 device entry"))]);
}

#[test]
fn every_truncation_is_handled() {
    let m = firmware(&[
        (b"SPCR", spcr_body(3, gas(0, 32, 3, 0x0900_0000))),
        (b"DBG2", dbg2_body(0x0e, gas(0, 32, 3, 0x1000), 0x8000)),
    ]);
    for n in 0..m.0.len() {
        let cut = Mem(m.0[..n].to_vec());
        if let Ok(t) = Tables::new(&cut, 0x40) {
            let _ = console_uart(&t);
            for table in t.iter().flatten() {
                let _ = spcr::parse(&table);
                let _ = dbg2::serial_ports(&table).count();
            }
        }
    }
}
