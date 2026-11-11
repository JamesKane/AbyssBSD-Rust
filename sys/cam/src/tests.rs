// SPDX-License-Identifier: BSD-2-Clause
//! Host tests: the CCB's offsets against FreeBSD 15.1's (as its compiler
//! put them), and `XPT_DEV_MATCH` over a table like QEMU's, printed as
//! `camcontrol devlist -v` prints it.

extern crate std;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::mem::offset_of;

use abi::cam::{
    CAM_LUN_WILDCARD, CAM_TARGET_WILDCARD, CAM_XPT_PATH_ID, DevMatchPattern, PeriphMatchPattern,
    dev_pos, dm_status, match_type, periph_match, proto,
};

use crate::ccb::{Ccb, Path, Position};
use crate::edt::{DeviceInfo, Edt, Periph, SimName};
use crate::matching::{MatchResult, PATTERN_SIZE, Pattern, dev_match};

/// The offset of the first byte `f` changes in a zeroed CCB.
fn offset_written(f: impl FnOnce(&mut Ccb)) -> usize {
    let mut c = Ccb::default();
    f(&mut c);
    c.as_bytes()
        .iter()
        .position(|&b| b != 0)
        .expect("nothing written")
}

#[test]
fn ccb_offsets_are_freebsds() {
    // offsetof() values from FreeBSD 15.1's cc for amd64 (arm64's are the
    // same; abi-check holds both).
    assert_eq!(offset_written(|c| c.set_func_code(0xff)), 80);
    assert_eq!(offset_written(|c| c.set_status(0xff)), 84);
    assert_eq!(offset_written(|c| c.set_path_id(0xff)), 96);
    assert_eq!(offset_written(|c| c.set_target_id(0xff)), 100);
    assert_eq!(offset_written(|c| c.set_target_lun(0xff)), 104);
    assert_eq!(offset_written(|c| c.set_flags(0xff)), 112);
    assert_eq!(offset_written(|c| c.set_timeout(0xff)), 176);
    assert_eq!(offset_written(|c| c.set_cgd_protocol(0xff)), 200);
    assert_eq!(offset_written(|c| c.set_cgd(&[], &[], 0, b"S")), 972);
    assert_eq!(offset_written(|c| c.set_cgdl_unit(0xff)), 216);
    assert_eq!(offset_written(|c| c.set_cgdl_status(0xff)), 228);
    assert_eq!(offset_written(|c| c.set_cdm_patterns(0xff)), 216);
    assert_eq!(offset_written(|c| c.set_cdm_num_matches(0xff)), 224);
    assert_eq!(offset_written(|c| c.set_cdm_matches(0xff)), 232);
    assert_eq!(offset_written(|c| c.set_csio_data_ptr(0xff)), 216);
    assert_eq!(offset_written(|c| c.set_csio_sense(&[0x70])), 228);
    assert_eq!(offset_written(|c| c.set_csio_cdb_len(0xff)), 481);
    assert_eq!(offset_written(|c| c.set_csio_scsi_status(0xff)), 484);
    assert_eq!(offset_written(|c| c.set_csio_resid(0xff)), 488);
    assert_eq!(offset_written(|c| c.set_csio_cdb_bytes(&[0x12])), 496);
    assert_eq!(
        offset_written(|c| c.set_ataio_cmd(&abi::cam::AtaCmd {
            command: 0xec,
            ..Default::default()
        })),
        209
    );
    assert_eq!(offset_written(|c| c.set_ataio_data_ptr(0xff)), 240);
    assert_eq!(offset_written(|c| c.set_nvmeio_data_ptr(0xff)), 288);
    assert_eq!(
        offset_written(|c| c.set_nvmeio_cpl(&abi::nvme::NvmeCompletion {
            status: 1,
            ..Default::default()
        })),
        272 + 14
    );
    assert_eq!(offset_written(|c| c.set_cdai_buf(0xff)), 224);
    assert_eq!(offset_written(|c| c.set_cdai_provsiz(0xff)), 216);
}

#[test]
fn header_keeps_what_the_user_may_set() {
    let mut c = Ccb::default();
    c.set_func_code(abi::cam::func::PATH_INQ);
    c.set_flags(abi::cam::flags::DIR_NONE);
    c.set_timeout(5000);
    c.set_retry_count(3);
    c.set_path_id(77);
    c.set_path(Path {
        path_id: 2,
        target_id: 0,
        target_lun: 1,
    });
    assert_eq!(c.func_code(), abi::cam::func::PATH_INQ);
    assert_eq!(c.flags(), abi::cam::flags::DIR_NONE);
    assert_eq!((c.timeout(), c.retry_count()), (5000, 3));
    assert_eq!(c.path().path_id, 2);
    assert_eq!(c.path().target_lun, 1);
}

#[test]
fn nvme_command_round_trips() {
    let cmd = abi::nvme::NvmeCommand {
        opc: 6,
        nsid: 1,
        cdw10: 1,
        cdw15: 0xdead_beef,
        prp2: 0x1234_5678_9abc,
        ..Default::default()
    };
    let mut b = [0u8; 64];
    crate::ccb::put_nvme_command(&mut b, &cmd);
    assert_eq!(b[0], 6);
    assert_eq!(&b[40..44], &1u32.to_le_bytes());
    assert_eq!(crate::ccb::nvme_command(&b), cmd);
}

fn ident(model: &str, rev: &str) -> alloc::boxed::Box<[u8; 512]> {
    let mut b = alloc::boxed::Box::new([0u8; 512]);
    b[54..54 + model.len()].copy_from_slice(model.as_bytes());
    b[46..46 + rev.len()].copy_from_slice(rev.as_bytes());
    b
}

fn inquiry(vendor: &str, product: &str, rev: &str) -> alloc::boxed::Box<[u8; 256]> {
    let mut b = alloc::boxed::Box::new([0x20u8; 256]);
    b[0] = 0;
    b[1] = 0x80;
    b[8..8 + vendor.len()].copy_from_slice(vendor.as_bytes());
    b[16..16 + product.len()].copy_from_slice(product.as_bytes());
    b[32..32 + rev.len()].copy_from_slice(rev.as_bytes());
    b
}

fn sim(name: &'static str, unit: u32) -> SimName {
    SimName {
        name,
        unit,
        bus_id: 0,
    }
}

/// Adds a configured device with its peripherals.
fn device(e: &mut Edt, path: u32, lun: u64, info: DeviceInfo, periphs: &[(&'static str, u32)]) {
    let id = e.add_device(path, 0, lun, Some(info));
    let d = e.device_mut(id).unwrap();
    d.periphs = periphs
        .iter()
        .map(|&(name, unit)| Periph { name, unit })
        .collect();
    d.generation += 2;
}

/// QEMU's x86 shell VM: an AHCI disk on the first of six channels, the
/// NVMe boot disk, and a USB stick.
fn qemu_table() -> Edt {
    let mut e = Edt::new();
    for ch in 0..6 {
        e.add_bus(ch, sim("ahcich", ch), None);
        if ch == 0 {
            let mut i = DeviceInfo::new(0, 0, proto::ATA, ("ada", 0));
            i.ident_data = ident("QEMU HARDDISK", "2.5+");
            device(&mut e, 0, 0, i, &[("pass", 0), ("ada", 0)]);
        }
        e.add_device(ch, CAM_TARGET_WILDCARD, CAM_LUN_WILDCARD, None);
    }
    e.add_bus(6, sim("nvme", 0), None);
    device(
        &mut e,
        6,
        1,
        DeviceInfo::new(0, 1, proto::NVME, ("nda", 0)),
        &[("pass", 1), ("nda", 0)],
    );
    e.add_device(6, CAM_TARGET_WILDCARD, CAM_LUN_WILDCARD, None);
    e.add_bus(7, sim("umass-sim", 0), None);
    let mut i = DeviceInfo::new(0, 0, proto::SCSI, ("da", 0));
    i.inq_data = inquiry("QEMU", "QEMU HARDDISK", "2.5+");
    device(&mut e, 7, 0, i, &[("da", 0), ("pass", 2)]);
    e
}

/// `cam_strvis` for the plain ASCII here: up to the first NUL, trailing
/// spaces dropped.
fn vis(b: &[u8]) -> String {
    let b = crate::ccb::cstr(b);
    String::from_utf8_lossy(b).trim_end().into()
}

/// What `camcontrol devlist -v` prints for `results` (NVMe devices'
/// names, which it asks the pass device for, given as "NVME").
fn devlist(results: &[MatchResult]) -> String {
    let mut out = String::new();
    let mut need_close = 0;
    for r in results {
        match r {
            MatchResult::Bus {
                path_id,
                name,
                unit,
                bus_id,
            } => {
                if need_close > 0 {
                    out.push_str(")\n");
                    need_close = 0;
                }
                let _ = writeln!(
                    out,
                    "scbus{} on {name}{unit} bus {bus_id}:",
                    *path_id as i32
                );
            }
            MatchResult::Device {
                path_id,
                target_id,
                lun,
                protocol,
                inq,
                ident,
                ..
            } => {
                let s = match *protocol {
                    proto::SCSI => alloc::format!(
                        "<{} {} {}>",
                        vis(&inq[8..16]),
                        vis(&inq[16..32]),
                        vis(&inq[32..36])
                    ),
                    proto::ATA => {
                        alloc::format!("<{} {}>", vis(&ident[54..94]), vis(&ident[46..54]))
                    }
                    proto::NVME => "<NVME>".into(),
                    _ => "<>".into(),
                };
                if need_close > 0 {
                    out.push_str(")\n");
                }
                let _ = write!(
                    out,
                    "{s:<33}  at scbus{} target {} lun {lun:x} (",
                    *path_id as i32, *target_id as i32
                );
                need_close = 1;
            }
            MatchResult::Periph { name, unit, .. } => {
                if need_close > 1 {
                    out.push(',');
                }
                let _ = write!(out, "{name}{unit}");
                need_close += 1;
            }
        }
    }
    if need_close > 0 {
        out.push_str(")\n");
    }
    out
}

const QEMU_DEVLIST: &str = "\
scbus0 on ahcich0 bus 0:
<QEMU HARDDISK 2.5+>               at scbus0 target 0 lun 0 (pass0,ada0)
<>                                 at scbus0 target -1 lun ffffffff ()
scbus1 on ahcich1 bus 0:
<>                                 at scbus1 target -1 lun ffffffff ()
scbus2 on ahcich2 bus 0:
<>                                 at scbus2 target -1 lun ffffffff ()
scbus3 on ahcich3 bus 0:
<>                                 at scbus3 target -1 lun ffffffff ()
scbus4 on ahcich4 bus 0:
<>                                 at scbus4 target -1 lun ffffffff ()
scbus5 on ahcich5 bus 0:
<>                                 at scbus5 target -1 lun ffffffff ()
scbus6 on nvme0 bus 0:
<NVME>                             at scbus6 target 0 lun 1 (pass1,nda0)
<>                                 at scbus6 target -1 lun ffffffff ()
scbus7 on umass-sim0 bus 0:
<QEMU QEMU HARDDISK 2.5+>          at scbus7 target 0 lun 0 (da0,pass2)
scbus-1 on xpt0 bus 0:
<>                                 at scbus-1 target -1 lun ffffffff (xpt0)
";

#[test]
fn dev_match_lists_the_table_as_freebsd() {
    let e = qemu_table();
    let m = dev_match(&e, &[], 100, Position::default());
    assert_eq!(m.status, dm_status::LAST);
    assert_eq!(devlist(&m.results), QEMU_DEVLIST);
}

#[test]
fn dev_match_pages_through_a_small_buffer() {
    let e = qemu_table();
    let all = dev_match(&e, &[], 100, Position::default()).results;
    for room in [1, 2, 3, 7] {
        let mut got = Vec::new();
        let mut pos = Position::default();
        loop {
            let m = dev_match(&e, &[], room, pos);
            assert!(m.results.len() <= room);
            got.extend(m.results);
            match m.status {
                dm_status::LAST => break,
                dm_status::MORE => pos = m.pos,
                s => panic!("status {s}"),
            }
        }
        assert_eq!(got, all, "{room} at a time");
    }
}

#[test]
fn dev_match_notices_a_changed_list() {
    let mut e = qemu_table();
    let m = dev_match(&e, &[], 3, Position::default());
    assert_eq!(m.status, dm_status::MORE);
    // A bus goes between the calls.
    e.remove_bus(7);
    let again = dev_match(&e, &[], 3, m.pos);
    assert_eq!(again.status, dm_status::LIST_CHANGED);
}

fn periph_pattern(name: &str, path: u32, target: u32, lun: u64, flags: u32) -> Pattern {
    let mut b = [0u8; PATTERN_SIZE];
    b[offset_of!(DevMatchPattern, type_)..][..4].copy_from_slice(&match_type::PERIPH.to_ne_bytes());
    let p = offset_of!(DevMatchPattern, pattern);
    type P = PeriphMatchPattern;
    b[p + offset_of!(P, periph_name)..][..name.len()].copy_from_slice(name.as_bytes());
    b[p + offset_of!(P, path_id)..][..4].copy_from_slice(&path.to_ne_bytes());
    b[p + offset_of!(P, target_id)..][..4].copy_from_slice(&target.to_ne_bytes());
    b[p + offset_of!(P, target_lun)..][..8].copy_from_slice(&lun.to_ne_bytes());
    b[p + offset_of!(P, flags)..][..4].copy_from_slice(&flags.to_ne_bytes());
    Pattern::parse(&b).unwrap()
}

#[test]
fn cam_open_btl_finds_the_pass_device() {
    // libcam's `cam_open_btl`: the pass peripheral at a bus, target, LUN.
    let e = qemu_table();
    let all = periph_match::PATH | periph_match::TARGET | periph_match::LUN | periph_match::NAME;
    let pat = periph_pattern("pass", 6, 0, 1, all);
    let m = dev_match(&e, &[pat], 1, Position::default());
    assert_eq!(m.status, dm_status::LAST);
    assert_eq!(
        m.results,
        [MatchResult::Periph {
            name: "pass",
            unit: 1,
            path_id: 6,
            target_id: 0,
            lun: 1
        }]
    );
    // A periph pattern alone walks the drivers' lists, by unit.
    let every_pass = periph_pattern("pass", 0, 0, 0, periph_match::NAME);
    let m = dev_match(
        &e,
        core::slice::from_ref(&every_pass),
        100,
        Position::default(),
    );
    let units: Vec<u32> = m
        .results
        .iter()
        .map(|r| match r {
            MatchResult::Periph { unit, .. } => *unit,
            _ => panic!("{r:?}"),
        })
        .collect();
    assert_eq!(units, [0, 1, 2]);
    // And pages through them.
    let first = dev_match(
        &e,
        core::slice::from_ref(&every_pass),
        1,
        Position::default(),
    );
    assert_eq!(first.status, dm_status::MORE);
    assert_eq!(first.pos.position_type & dev_pos::PDRV, dev_pos::PDRV);
    let second = dev_match(&e, &[every_pass], 2, first.pos);
    assert_eq!(second.status, dm_status::LAST);
    assert_eq!(second.results.len(), 2);
    // The transport layer's own peripheral.
    let xpt = periph_pattern("xpt", 0, 0, 0, periph_match::NAME);
    let m = dev_match(&e, &[xpt], 4, Position::default());
    assert_eq!(
        m.results,
        [MatchResult::Periph {
            name: "xpt",
            unit: 0,
            path_id: CAM_XPT_PATH_ID,
            target_id: CAM_TARGET_WILDCARD,
            lun: CAM_LUN_WILDCARD,
        }]
    );
}

#[test]
fn bus_patterns_stop_at_buses() {
    let e = qemu_table();
    let mut b = [0u8; PATTERN_SIZE];
    b[..4].copy_from_slice(&match_type::BUS.to_ne_bytes());
    let p = offset_of!(DevMatchPattern, pattern);
    type B = abi::cam::BusMatchPattern;
    b[p + offset_of!(B, dev_name)..][..4].copy_from_slice(b"nvme");
    b[p + offset_of!(B, flags)..][..4].copy_from_slice(&abi::cam::bus_match::NAME.to_ne_bytes());
    let m = dev_match(&e, &[Pattern::parse(&b).unwrap()], 100, Position::default());
    assert_eq!(
        m.results,
        [MatchResult::Bus {
            path_id: 6,
            name: "nvme",
            unit: 0,
            bus_id: 0
        }]
    );
}

#[test]
fn inquiry_patterns_glob() {
    let e = qemu_table();
    let mut b = [0u8; PATTERN_SIZE];
    b[..4].copy_from_slice(&match_type::DEVICE.to_ne_bytes());
    let p = offset_of!(DevMatchPattern, pattern);
    type D = abi::cam::DeviceMatchPattern;
    type I = abi::cam::ScsiStaticInquiryPattern;
    b[p + offset_of!(D, flags)..][..4].copy_from_slice(&abi::cam::dev_match::INQUIRY.to_ne_bytes());
    let d = p + offset_of!(D, data);
    b[d + offset_of!(I, type_)] = 0xff;
    b[d + offset_of!(I, media_type)] = 0x03;
    b[d + offset_of!(I, vendor)] = b'Q';
    b[d + offset_of!(I, vendor) + 1] = b'*';
    b[d + offset_of!(I, product)..][..3].copy_from_slice(b"*HA");
    b[d + offset_of!(I, product) + 3] = b'*';
    b[d + offset_of!(I, revision)] = b'*';
    let m = dev_match(&e, &[Pattern::parse(&b).unwrap()], 100, Position::default());
    assert_eq!(m.results.len(), 1, "{:?}", m.results);
    assert!(matches!(
        m.results[0],
        MatchResult::Device { path_id: 7, .. }
    ));
}
