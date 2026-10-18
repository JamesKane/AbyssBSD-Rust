// SPDX-License-Identifier: BSD-2-Clause
//! Names, package lengths, the namespace, and loading several tables.

extern crate std;

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::util::*;
use crate::name::{Anchor, NameSeg};
use crate::stream::Cursor;
use crate::{AmlPath, Error, Interpreter, NameString, ObjectType, Value};

fn cursor(b: &[u8]) -> Cursor {
    let data: Arc<[u8]> = Arc::from(b);
    Cursor::new(data, 0, b.len())
}

#[test]
fn pkg_length_encodings() {
    // One byte: the length is the low six bits and counts itself.
    let mut c = cursor(&[0x05, 0, 0, 0, 0, 0]);
    assert_eq!(c.pkg_end().unwrap(), 5);
    // Two bytes: low nibble, then bits 4..11.
    let mut b = std::vec![0x41, 0x01];
    b.resize(0x11, 0);
    let mut c = cursor(&b);
    assert_eq!(c.pkg_end().unwrap(), 0x11);
    // Three and four bytes, raw values.
    assert_eq!(
        cursor(&[0x82, 0x34, 0x12]).pkg_length_value().unwrap(),
        0x12342
    );
    assert_eq!(
        cursor(&[0xc3, 0x21, 0x43, 0x65])
            .pkg_length_value()
            .unwrap(),
        0x0654_3213
    );
    // Bits 4 and 5 must be clear when bytes follow.
    assert_eq!(
        cursor(&[0x51, 0x01]).pkg_length_value(),
        Err(Error::BadPkgLength)
    );
    // A length reaching past the enclosing scope is rejected.
    assert_eq!(cursor(&[0x3f, 0, 0]).pkg_end(), Err(Error::BadPkgLength));
    // Truncated encodings.
    assert_eq!(
        cursor(&[0x81]).pkg_length_value(),
        Err(Error::UnexpectedEnd)
    );
    assert_eq!(cursor(&[]).pkg_length_value(), Err(Error::UnexpectedEnd));
}

#[test]
fn name_string_forms() {
    let parse = |b: &[u8]| cursor(b).name_string();
    let n = parse(b"_SB_").unwrap();
    assert_eq!((n.anchor, n.segs.len()), (Anchor::Relative(0), 1));
    assert!(n.uses_search_rules());
    let n = parse(b"\\_SB_").unwrap();
    assert_eq!(n.anchor, Anchor::Root);
    assert!(!n.uses_search_rules());
    let n = parse(b"^^FOO_").unwrap();
    assert_eq!(n.anchor, Anchor::Relative(2));
    let n = parse(&cat(&[b"\\", &[0x2e], b"_SB_PCI0"])).unwrap();
    assert_eq!(alloc::format!("{n}"), "\\_SB_.PCI0");
    let n = parse(&cat(&[&[0x2f, 3], b"_SB_PCI0ISA_"])).unwrap();
    assert_eq!(n.segs.len(), 3);
    let n = parse(&[0]).unwrap();
    assert!(n.segs.is_empty());
    let n = parse(&[b'\\', 0]).unwrap();
    assert_eq!((n.anchor, n.segs.len()), (Anchor::Root, 0));
    // Bad characters, truncation, and an overlong multi-name.
    assert_eq!(parse(b"1ABC").unwrap_err(), Error::InvalidName);
    assert_eq!(parse(b"ab_C").unwrap_err(), Error::InvalidName);
    assert_eq!(parse(b"AB").unwrap_err(), Error::UnexpectedEnd);
    assert_eq!(
        parse(&cat(&[&[0x2f, 200], b"_SB_"])).unwrap_err(),
        Error::UnexpectedEnd
    );
}

#[test]
fn asl_text_names() {
    let n = NameString::parse("\\_SB.PCI0").unwrap();
    assert_eq!(n.segs, [NameSeg(*b"_SB_"), NameSeg(*b"PCI0")]);
    assert_eq!(alloc::format!("{n}"), "\\_SB_.PCI0");
    let p = AmlPath::parse("\\_SB.PCI0._HID").unwrap();
    assert_eq!(p.to_text(), "\\_SB_.PCI0._HID");
    assert_eq!(AmlPath::parse("\\").unwrap(), AmlPath::default());
    assert!(AmlPath::parse("^FOO").is_err());
    assert!(NameString::parse("TOOLONG").is_err());
    assert!(NameString::parse("a").is_err());
}

#[test]
fn upward_search_applies_to_single_segments_only() {
    let aml = cat(&[
        &name_op("VAL", &int(1)),
        &device(
            "\\_SB.DEV",
            &cat(&[
                &device("SUB", &method("GETV", 0, &ret(&name("VAL")))),
                &name_op("VAL2", &int(2)),
                // A two-segment relative name does not search upward; inside
                // a method, the scope is the method itself.
                &method("BAD", 0, &ret(&name("DEV.VAL2"))),
                &method("GOOD", 0, &ret(&name("^^DEV.VAL2"))),
            ]),
        ),
    ]);
    let (mut i, mut h) = load(&aml);
    assert_eq!(eval_int(&mut i, &mut h, "\\_SB.DEV.SUB.GETV"), 1);
    assert_eq!(eval_int(&mut i, &mut h, "\\_SB.DEV.GOOD"), 2);
    let e = i
        .evaluate_path("\\_SB.DEV.BAD", Vec::new(), &mut h)
        .unwrap_err();
    assert!(matches!(e, Error::Undefined(_)), "{e:?}");
    // The API resolver follows the same rules.
    let sub = i.lookup(&AmlPath::parse("\\_SB.DEV.SUB").unwrap()).unwrap();
    assert_eq!(
        i.resolve(sub, "VAL"),
        i.lookup(&AmlPath::parse("\\VAL").unwrap())
    );
    assert_eq!(i.resolve(sub, "DEV.VAL2"), None);
}

#[test]
fn scopes_objects_and_types() {
    let aml = cat(&[
        &scope(
            "\\_SB",
            &cat(&[
                &device(
                    "PCI0",
                    &cat(&[
                        &name_op("_HID", &int(0x080a_d041)),
                        &name_op("_UID", &string("root")),
                        &method("_STA", 0, &ret(&int(0x0b))),
                    ]),
                ),
                // PowerResource (PRS0, 3, 7) {}
                &pkg(
                    &[0x5b, 0x84],
                    &cat(&[&name("PRS0"), &[3], &7u16.to_le_bytes()]),
                ),
                // Processor (CPU0, 1, 0x810, 6) {}
                &pkg(
                    &[0x5b, 0x83],
                    &cat(&[&name("CPU0"), &[1], &0x810u32.to_le_bytes(), &[6]]),
                ),
                // ThermalZone (TZ0) {}
                &pkg(&[0x5b, 0x85], &name("TZ0")),
                // Mutex (MTX0, 5), Event (EVT0)
                &cat(&[&[0x5b, 0x01], &name("MTX0"), &[5]]),
                &cat(&[&[0x5b, 0x02], &name("EVT0")]),
                // Alias (PCI0, PCIA)
                &cat(&[&[0x06], &name("PCI0"), &name("PCIA")]),
            ]),
        ),
        &method("\\M3", 3 | 8 | (2 << 4), &ret(&[ARG0 + 2])),
    ]);
    let (mut i, mut h) = load(&aml);
    let get = |i: &Interpreter, p: &str| i.lookup(&AmlPath::parse(p).unwrap()).unwrap();
    let pci0 = get(&i, "\\_SB.PCI0");
    assert_eq!(i.object_type(pci0), Some(ObjectType::Device));
    assert_eq!(i.hid(pci0, &mut h).unwrap().as_deref(), Some("PNP0A08"));
    assert_eq!(
        i.uid(pci0, &mut h).unwrap(),
        Some(crate::Uid::String("root".into()))
    );
    assert_eq!(i.sta(pci0, &mut h).unwrap(), 0x0b);
    let prs = get(&i, "\\_SB.PRS0");
    assert_eq!(i.object_type(prs), Some(ObjectType::PowerResource));
    let info = i.power_resource_info(prs).unwrap();
    assert_eq!((info.system_level, info.resource_order), (3, 7));
    let cpu = get(&i, "\\_SB.CPU0");
    let p = i.processor_info(cpu).unwrap();
    assert_eq!((p.id, p.pblk, p.pblk_len), (1, 0x810, 6));
    assert_eq!(
        i.object_type(get(&i, "\\_SB.TZ0")),
        Some(ObjectType::ThermalZone)
    );
    assert_eq!(
        i.object_type(get(&i, "\\_SB.MTX0")),
        Some(ObjectType::Mutex)
    );
    assert_eq!(
        i.object_type(get(&i, "\\_SB.EVT0")),
        Some(ObjectType::Event)
    );
    // The alias resolves to its target.
    assert_eq!(get(&i, "\\_SB.PCIA"), pci0);
    let m = i.method_info(get(&i, "\\M3")).unwrap();
    assert_eq!((m.args, m.serialized, m.sync_level), (3, true, 2));
    let r = eval(
        &mut i,
        &mut h,
        "\\M3",
        std::vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)],
    );
    assert_eq!(r, Value::Integer(3));
    // Definition order is enumeration order.
    let names: Vec<_> = i
        .children(get(&i, "\\_SB"))
        .iter()
        .map(|&c| std::string::String::from(i.name(c).unwrap().as_str()))
        .collect();
    assert_eq!(
        names,
        ["PCI0", "PRS0", "CPU0", "TZ0_", "MTX0", "EVT0", "PCIA"]
    );
}

#[test]
fn ssdt_extends_the_dsdt_namespace() {
    let dsdt = cat(&[
        &device("\\_SB.PCI0", &name_op("BASE", &int(0x40))),
        &cat(&[&[0x15], &name("\\_SB.PCI0.LATE"), &[8, 1]]),
    ]);
    // The SSDT adds a device under PCI0, defines the method the DSDT
    // declared External, and refers to the DSDT's names.
    let ssdt = cat(&[
        &[0x15],
        &name("\\_SB.PCI0"),
        &[6, 0],
        &scope(
            "\\_SB.PCI0",
            &cat(&[
                &device("GFX0", &name_op("_ADR", &int(0x0002_0000))),
                &method(
                    "LATE",
                    1,
                    &ret(&cat(&[&[0x72], &name("BASE"), &[ARG0], &[0]])),
                ),
            ]),
        ),
    ]);
    let mut i = Interpreter::new(test_config());
    let mut h = FakeHost::default();
    let r = i.load_table(&table(b"DSDT", 2, &dsdt), &mut h).unwrap();
    assert!(r.warnings.is_empty());
    // Before the SSDT, the External placeholder is not callable.
    let e = i
        .evaluate_path("\\_SB.PCI0.LATE", std::vec![Value::Integer(1)], &mut h)
        .unwrap_err();
    assert!(matches!(e, Error::Undefined(_)), "{e:?}");
    let r = i.load_table(&table(b"SSDT", 2, &ssdt), &mut h).unwrap();
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let v = eval(
        &mut i,
        &mut h,
        "\\_SB.PCI0.LATE",
        std::vec![Value::Integer(2)],
    );
    assert_eq!(v, Value::Integer(0x42));
    let gfx = i
        .lookup(&AmlPath::parse("\\_SB.PCI0.GFX0").unwrap())
        .unwrap();
    assert_eq!(i.adr(gfx, &mut h).unwrap(), Some(0x0002_0000));
}

#[test]
fn duplicate_names_are_load_warnings() {
    // The second Name fails; the rest of that scope is skipped, and the
    // next scope still loads.
    let aml = cat(&[
        &scope(
            "\\_SB",
            &cat(&[
                &name_op("AAAA", &int(1)),
                &name_op("AAAA", &int(2)),
                &name_op("SKIP", &int(3)),
            ]),
        ),
        &name_op("NEXT", &int(4)),
    ]);
    let mut i = Interpreter::new(test_config());
    let mut h = FakeHost::default();
    let r = i.load_table(&table(b"DSDT", 2, &aml), &mut h).unwrap();
    assert_eq!(r.warnings.len(), 1);
    assert_eq!(r.warnings[0].error, Error::AlreadyExists);
    assert_eq!(eval_int(&mut i, &mut h, "\\_SB.AAAA"), 1);
    assert!(i.lookup(&AmlPath::parse("\\_SB.SKIP").unwrap()).is_none());
    assert_eq!(eval_int(&mut i, &mut h, "\\NEXT"), 4);
}

#[test]
fn method_objects_are_temporary() {
    let aml = method(
        "\\TMP",
        0,
        &cat(&[
            &name_op("LOCN", &int(7)),
            &device("TDEV", &name_op("INNR", &int(1))),
            &ret(&name("LOCN")),
        ]),
    );
    let (mut i, mut h) = load(&aml);
    let before = i.node_count();
    for _ in 0..3 {
        assert_eq!(eval_int(&mut i, &mut h, "\\TMP"), 7);
    }
    assert_eq!(i.node_count(), before);
    assert!(i.lookup(&AmlPath::parse("\\TMP.LOCN").unwrap()).is_none());
}

#[test]
fn bad_tables_are_rejected() {
    let mut i = Interpreter::new(test_config());
    let mut h = FakeHost::default();
    let mut t = table(b"DSDT", 2, &name_op("X", &int(1)));
    assert!(matches!(
        i.load_table(&t[..20], &mut h),
        Err(Error::BadTable(_))
    ));
    t[40] ^= 1;
    assert_eq!(i.load_table(&t, &mut h), Err(Error::BadTable("checksum")));
    let mut t = table(b"DSDT", 2, &[]);
    t[4] = 0xff;
    assert!(matches!(i.load_table(&t, &mut h), Err(Error::BadTable(_))));
}

#[test]
fn malformed_aml_is_a_warning_not_a_panic() {
    for aml in [
        std::vec![0x5b],                                  // truncated extended opcode
        std::vec![0x10, 0x3f],                            // scope length past the table
        std::vec![0x08, b'A'],                            // truncated name
        std::vec![0xa1, 0x01],                            // orphan Else
        std::vec![0x5b, 0xfe],                            // unknown extended opcode
        std::vec![0x14, 0x06, b'M', b'_', b'_', b'_', 0], // method, no body
        cat(&[&[0x70], &int(1), &name("NOPE")]),          // store to undefined
    ] {
        let mut i = Interpreter::new(test_config());
        let mut h = FakeHost::default();
        let r = i.load_table(&table(b"DSDT", 2, &aml), &mut h).unwrap();
        // A method with an empty body is fine; everything else warns.
        if aml[0] != 0x14 {
            assert_eq!(r.warnings.len(), 1, "{aml:x?}");
        }
    }
}
