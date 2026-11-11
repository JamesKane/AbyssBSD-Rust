// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/pci`'s logic: patterns, paging, `pci_conf`, BAR records, and
//! VPD, as FreeBSD's `pci_user.c` and `pci.c` compute them.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use abi::pciio::{PciConf, PciConfIo, PciMatchConf, PciSel, flags, status};

use super::{Fake, Shared, addr, device};
use crate::bar;
use crate::bus::{PciBusData, PciChild};
use crate::config::ConfigSpace;
use crate::user::{self, GetconfError};
use crate::vpd::{self, Keyword, Vpd};

fn named(bus: u8, slot: u8, vendor: u16, name: &str, unit: u64) -> PciConf {
    let mut c = PciConf {
        pc_sel: PciSel {
            pc_bus: bus,
            pc_dev: slot,
            ..PciSel::default()
        },
        pc_vendor: vendor,
        pc_class: 0x02,
        pd_unit: unit,
        ..PciConf::default()
    };
    c.pd_name[..name.len()].copy_from_slice(name.as_bytes());
    c
}

fn list() -> Vec<PciConf> {
    vec![
        named(0, 0, 0x8086, "hostb", 0),
        named(0, 1, 0x1af4, "virtio_pci", 0),
        named(0, 2, 0x1b36, "pcib", 1),
        named(1, 0, 0x1af4, "virtio_pci", 1),
        named(0, 3, 0x1b36, "", 0),
    ]
}

const CONF: u32 = 128;

/// Runs a request over [`list`], generation 7: the matches written.
fn run(io: &mut PciConfIo, pats: &[PciMatchConf]) -> Result<Vec<PciConf>, GetconfError<()>> {
    let l = list();
    let mut out = Vec::new();
    user::getconf(
        io,
        7,
        &l,
        |n| (n == pats.len()).then(|| pats.to_vec()),
        |c| *c,
        |i, c| {
            assert_eq!(i, out.len());
            out.push(*c);
            Ok(())
        },
    )?;
    Ok(out)
}

fn units(v: &[PciConf]) -> Vec<(String, u64)> {
    v.iter()
        .map(|c| {
            let end = c.pd_name.iter().position(|&b| b == 0).unwrap();
            (
                String::from_utf8(c.pd_name[..end].to_vec()).unwrap(),
                c.pd_unit,
            )
        })
        .collect()
}

#[test]
fn getconf_pages_through_the_list() {
    // Room for two (and a bit, rounded down).
    let mut io = PciConfIo {
        match_buf_len: 2 * CONF + 5,
        ..PciConfIo::default()
    };
    let got = run(&mut io, &[]).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!((io.num_matches, io.offset, io.generation), (2, 2, 7));
    assert_eq!(io.status, status::MORE_DEVS);
    let got = run(&mut io, &[]).unwrap();
    assert_eq!(got[0].pc_sel.pc_dev, 2);
    assert_eq!(
        (io.num_matches, io.offset, io.status),
        (2, 4, status::MORE_DEVS)
    );
    let got = run(&mut io, &[]).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!((io.offset, io.status), (5, status::LAST_DEVICE));
    // Past the end.
    assert!(run(&mut io, &[]).unwrap().is_empty());
    assert_eq!((io.num_matches, io.status), (0, status::LAST_DEVICE));
    // Another generation, from an offset: start again.
    io.offset = 2;
    io.generation = 6;
    assert!(run(&mut io, &[]).unwrap().is_empty());
    assert_eq!(io.status, status::LIST_CHANGED);
    // A whole buffer from the start takes everything.
    let mut io = PciConfIo {
        match_buf_len: 255 * CONF,
        ..PciConfIo::default()
    };
    assert_eq!(run(&mut io, &[]).unwrap(), list());
    assert_eq!((io.offset, io.status), (5, status::LAST_DEVICE));
    // No room at all stops at the first match, to be asked again.
    let mut io = PciConfIo::default();
    assert!(run(&mut io, &[]).unwrap().is_empty());
    assert_eq!((io.offset, io.status), (0, status::MORE_DEVS));
}

fn pattern(f: u32) -> PciMatchConf {
    PciMatchConf {
        flags: f,
        ..PciMatchConf::default()
    }
}

fn with_patterns(pats: &[PciMatchConf]) -> PciConfIo {
    PciConfIo {
        match_buf_len: 255 * CONF,
        num_patterns: u32::try_from(pats.len()).unwrap(),
        pat_buf_len: u32::try_from(core::mem::size_of_val(pats)).unwrap(),
        ..PciConfIo::default()
    }
}

#[test]
fn getconf_matches_patterns() {
    // `pciconf -l virtio_pci1`: name and unit.
    let mut p = pattern(flags::MATCH_NAME | flags::MATCH_UNIT);
    p.pd_name[..10].copy_from_slice(b"virtio_pci");
    p.pd_unit = 1;
    let mut io = with_patterns(&[p]);
    let got = run(&mut io, &[p]).unwrap();
    assert_eq!(units(&got), [(String::from("virtio_pci"), 1)]);
    assert_eq!((io.offset, io.status), (5, status::LAST_DEVICE));
    // A name is compared whole: "virtio" is not "virtio_pci".
    let mut short = pattern(flags::MATCH_NAME);
    short.pd_name[..6].copy_from_slice(b"virtio");
    assert!(
        run(&mut with_patterns(&[short]), &[short])
            .unwrap()
            .is_empty()
    );
    // `pciconf -l pci0:1:0:0`: the selector; any pattern may match.
    let mut sel =
        pattern(flags::MATCH_DOMAIN | flags::MATCH_BUS | flags::MATCH_DEV | flags::MATCH_FUNC);
    sel.pc_sel.pc_bus = 1;
    let mut vendor = pattern(flags::MATCH_VENDOR | flags::MATCH_CLASS);
    vendor.pc_vendor = 0x8086;
    vendor.pc_class = 0x02;
    let got = run(&mut with_patterns(&[sel, vendor]), &[sel, vendor]).unwrap();
    assert_eq!(
        units(&got),
        [(String::from("hostb"), 0), (String::from("virtio_pci"), 1)]
    );
    // The device ID; an unattached function has no name.
    let mut dev = pattern(flags::MATCH_VENDOR | flags::MATCH_NAME);
    dev.pc_vendor = 0x1b36;
    assert_eq!(
        units(&run(&mut with_patterns(&[dev]), &[dev]).unwrap()),
        [(String::new(), 0)]
    );
    // A pattern with no flags matches nothing.
    let none = pattern(flags::NO_MATCH);
    assert!(
        run(&mut with_patterns(&[none]), &[none])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn getconf_refuses_what_freebsd_refuses() {
    let p = pattern(flags::MATCH_BUS);
    // The buffer's length must be the patterns' exactly.
    let mut io = with_patterns(&[p]);
    io.pat_buf_len += 1;
    assert_eq!(run(&mut io, &[p]), Err(GetconfError::Invalid));
    assert_eq!(io.status, status::ERROR);
    // A count with no buffer, or a buffer with no count.
    let mut io = with_patterns(&[p]);
    io.pat_buf_len = 0;
    assert_eq!(run(&mut io, &[p]), Err(GetconfError::Invalid));
    let mut io = with_patterns(&[]);
    io.pat_buf_len = 56;
    assert_eq!(run(&mut io, &[]), Err(GetconfError::Invalid));
    assert_eq!(io.status, status::ERROR);
    // As many patterns as devices is refused too (FreeBSD's `<`).
    let five = [p; 5];
    let mut io = with_patterns(&five);
    assert_eq!(run(&mut io, &five), Err(GetconfError::Invalid));
    // Patterns that cannot be read.
    let mut io = with_patterns(&[p]);
    assert_eq!(run(&mut io, &[p, p]), Err(GetconfError::Invalid));
    // A failed copy-out stops the walk where it was.
    let l = list();
    let mut io = PciConfIo {
        match_buf_len: 255 * CONF,
        ..PciConfIo::default()
    };
    let r = user::getconf(
        &mut io,
        1,
        &l,
        |_| None,
        |c| *c,
        |i, _| if i == 1 { Err("fault") } else { Ok(()) },
    );
    assert_eq!(r, Err(GetconfError::Write("fault")));
    assert_eq!((io.num_matches, io.offset), (1, 1));
}

#[test]
fn conf_reports_the_function_and_its_driver() {
    // A bridge with a subsystem capability, as QEMU's root ports have.
    let fake = Fake::default();
    let mut f = device(0x1b36, 0x000c, 0x06_04_00, 0x81);
    f.regs.insert(crate::header::COMMAND, 0x10 << 16);
    f.regs.insert(0x18, 0x0003_0200);
    f.regs.insert(0x34, 0x40);
    f.regs.insert(0x40, 0x000d);
    f.regs.insert(0x44, 0x1100_1af4);
    fake.add(0, 2, 0, f.clone());
    let function = crate::enumerate::scan(&fake, 0, 0, 0).remove(0);
    assert_eq!(function.addr, addr(0, 2, 0));
    let regs = Shared(super::std::sync::Mutex::new([0; 1024]));
    for (&k, &v) in &f.regs {
        regs.0.lock().unwrap()[usize::from(k / 4)] = v;
    }
    let cfg: &'static Shared = alloc::boxed::Box::leak(alloc::boxed::Box::new(regs));
    let child = PciChild {
        vpd: sync::Once::new(),
        cfg,
        function,
        bus: Arc::new(PciBusData {
            cfg,
            adr_paths: Vec::new(),
            intx: Vec::new(),
            depth: 0,
            upstream: crate::requester::Upstream::Host,
        }),
    };
    let c = user::conf(&child, Some(("pcib", 1)));
    assert_eq!((c.pc_vendor, c.pc_device), (0x1b36, 0x000c));
    assert_eq!((c.pc_subvendor, c.pc_subdevice), (0x1af4, 0x1100));
    assert_eq!(
        (c.pc_class, c.pc_subclass, c.pc_progif, c.pc_revid),
        (6, 4, 0, 1)
    );
    // The header type without the multi-function bit.
    assert_eq!(c.pc_hdr, 1);
    assert_eq!((c.pc_secbus, c.pc_subbus), (2, 3));
    assert_eq!(&c.pd_name[..5], b"pcib\0");
    assert_eq!((c.pd_unit, c.pc_reported_len), (1, 66));
    // A driver moved the bus numbers: they are read again.
    cfg.write32(addr(0, 2, 0), 0x18, 0x0005_0400);
    let c = user::conf(&child, None);
    assert_eq!((c.pc_secbus, c.pc_subbus), (4, 5));
    assert_eq!((c.pd_name[0], c.pd_unit), (0, 0));
    // Names are cut at PCI_MAXNAMELEN, NUL kept.
    let c = user::conf(&child, Some(("a_very_long_driver_name", 3)));
    assert_eq!(&c.pd_name, b"a_very_long_driv\0");
}

#[test]
fn bars_are_recorded_as_freebsd_records_them() {
    // An unassigned I/O BAR is kept, with its size (`base 0, ...,
    // disabled`, as the Orange Pi's rge0 shows).
    let m = bar::map_of(0x10, 0x1, 0xffff_ff01).unwrap();
    assert_eq!((m.base(), m.size(), m.is_memory()), (0, 256, false));
    // Only 16 address bits decode: still 256 ports.
    assert_eq!(bar::map_of(0x10, 0x1001, 0xff01).unwrap().size(), 256);
    // The reserved I/O bit, and unimplemented BARs.
    assert_eq!(bar::map_of(0x14, 0x1, 0xffff_ff03), None);
    assert_eq!(bar::map_of(0x14, 0x0, 0x0), None);
    // A 64-bit BAR takes both halves.
    let m = bar::map_of(0x18, 0x0000_0001_4000_000c, 0xffff_ffff_ffff_c00c).unwrap();
    assert_eq!((m.base(), m.size()), (0x1_4000_0000, 0x4000));
}

/// VPD as a device presents it: the bytes, read a dword at a time.
fn reader(bytes: &[u8]) -> impl FnMut(u16) -> Option<u32> + '_ {
    move |at| {
        let at = usize::from(at);
        let mut d = [0xffu8; 4];
        for (i, b) in d.iter_mut().enumerate() {
            if let Some(&v) = bytes.get(at + i) {
                *b = v;
            }
        }
        Some(u32::from_le_bytes(d))
    }
}

/// An identifier, read-only keywords with a checksum, and a read-write
/// area.
fn sample(corrupt: bool) -> Vec<u8> {
    let mut v = vec![0x82, 5, 0, b'C', b'a', b'r', b'd', b'X'];
    let ro_body: &[u8] = &[b'P', b'N', 3, b'1', b'2', b'3'];
    v.extend_from_slice(&[0x90, u8::try_from(ro_body.len() + 5).unwrap(), 0]);
    v.extend_from_slice(ro_body);
    v.extend_from_slice(&[b'R', b'V', 2]);
    let sum = v.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    v.push(0u8.wrapping_sub(sum).wrapping_add(u8::from(corrupt)));
    v.push(0);
    v.extend_from_slice(&[0x91, 7, 0, b'Y', b'A', 1, b'z', b'R', b'W', 0]);
    v.push(0x78);
    v
}

#[test]
fn vpd_is_read_as_freebsd_reads_it() {
    let bytes = sample(false);
    let v = vpd::parse(reader(&bytes)).unwrap();
    let kw = |k: &[u8; 2], v: &[u8]| Keyword {
        keyword: *k,
        value: v.to_vec(),
    };
    let rv = v.read_only[1].value.clone();
    assert_eq!(
        v,
        Vpd {
            ident: b"CardX".to_vec(),
            read_only: vec![kw(b"PN", b"123"), kw(b"RV", &rv)],
            read_write: vec![kw(b"YA", b"z"), kw(b"RW", b"")],
        }
    );
    // What `PCIOCLISTVPD` copies out.
    let e = user::vpd_elements(&v);
    assert_eq!(&e[..9], &[0, 0, 1, 5, b'C', b'a', b'r', b'd', b'X']);
    assert_eq!(&e[9..16], &[b'P', b'N', 0, 3, b'1', b'2', b'3']);
    assert_eq!(
        &e[e.len() - 9..],
        &[b'Y', b'A', 2, 1, b'z', b'R', b'W', 2, 0]
    );
    // A bad checksum drops it all.
    assert_eq!(vpd::parse(reader(&sample(true))), None);
    // An identifier alone is kept.
    assert_eq!(
        vpd::parse(reader(&[0x82, 2, 0, b'h', b'i', 0x78])),
        Some(Vpd {
            ident: b"hi".to_vec(),
            ..Vpd::default()
        })
    );
    // No identifier, a device that stops answering, and lengths past
    // their resource. One that stops where the read-only section would
    // start leaves the identifier, as FreeBSD finds "no read-only VPD
    // data"; one that stops inside it, nothing.
    assert_eq!(vpd::parse(reader(&[0x90, 0, 0])), None);
    let stops_after = |dwords: u32| {
        let mut n = 0;
        let mut full = reader(&bytes);
        vpd::parse(move |at| {
            n += 1;
            if n <= dwords { full(at) } else { None }
        })
    };
    assert_eq!(stops_after(2).map(|v| v.read_only.len()), Some(0));
    assert_eq!(stops_after(3), None);
    assert_eq!(
        vpd::parse(reader(&[0x82, 1, 0, b'x', 0x90, 4, 0, b'P', b'N', 9])),
        None
    );
    // All-ones forever ends at the 15-bit space's end.
    assert_eq!(vpd::parse(|_| Some(u32::MAX)), None);
}
