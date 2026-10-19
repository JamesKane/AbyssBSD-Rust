// SPDX-License-Identifier: BSD-2-Clause
//! The captured firmware tables in `tests/corpus/acpi`.

extern crate std;

use alloc::vec::Vec;
use std::println;

use super::util::{FakeHost, test_config};
use crate::resource::{
    Address, AddressKind, AddressWidth, Caching, Io, Polarity, Resource, SerialBusKind, Trigger,
};
use crate::{
    AmlPath, Config, IntWidth, Interpreter, NodeId, PrtEntry, PrtSource, RegionSpace, Value,
};

macro_rules! corpus {
    ($path:literal) => {
        include_bytes!(concat!("../../../../tests/corpus/acpi/", $path)).as_slice()
    };
}

pub(crate) const DSDTS: &[(&str, &[u8])] = &[
    ("qemu-q35", corpus!("qemu-q35/dsdt.dat")),
    ("qemu-pc", corpus!("qemu-pc/dsdt.dat")),
    ("qemu-virt", corpus!("qemu-virt/dsdt.dat")),
    ("bhyve", corpus!("bhyve/dsdt.dat")),
    ("radxa-q8b", corpus!("radxa-q8b/dsdt.dat")),
];

pub(crate) fn load(dsdt: &[u8]) -> (Interpreter, FakeHost) {
    let mut i = Interpreter::new(test_config());
    let mut h = FakeHost::default();
    let r = i.load_table(dsdt, &mut h).unwrap();
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    (i, h)
}

fn dsdt(name: &str) -> &'static [u8] {
    DSDTS.iter().find(|(n, _)| *n == name).unwrap().1
}

fn node(i: &Interpreter, path: &str) -> NodeId {
    i.lookup(&AmlPath::parse(path).unwrap())
        .unwrap_or_else(|| panic!("{path} missing"))
}

fn link(i: &Interpreter, e: &PrtEntry) -> std::string::String {
    match &e.source {
        PrtSource::Link { node, index: 0 } => i.path(*node).to_text(),
        PrtSource::Gsi(g) => alloc::format!("GSI {g}"),
        other => alloc::format!("{other:?}"),
    }
}

fn producer(width: AddressWidth, kind: AddressKind, min: u64, max: u64, tra: u64) -> Resource {
    Resource::Address(Address {
        width,
        kind,
        consumer: false,
        subtractive: false,
        min_fixed: true,
        max_fixed: true,
        granularity: 0,
        min,
        max,
        translation: tra,
        length: max - min + 1,
        attributes: None,
        source: None,
    })
}

const MEM_UC: AddressKind = AddressKind::Memory {
    writeable: true,
    caching: Caching::NonCacheable,
    memory_type: 0,
    translation: false,
};
const MEM_WB: AddressKind = AddressKind::Memory {
    writeable: true,
    caching: Caching::Cacheable,
    memory_type: 0,
    translation: false,
};
const IO_ALL: AddressKind = AddressKind::Io {
    range: 3,
    translation: false,
    sparse: false,
};

#[test]
fn every_dsdt_loads_without_warnings() {
    // Node counts include the root and the nine predefined objects.
    for (name, t) in DSDTS {
        let (i, _) = load(t);
        assert!(i.node_count() > 50, "{name}: {}", i.node_count());
    }
}

#[test]
fn integer_width_follows_dsdt_revision() {
    // QEMU's x86 DSDTs are revision 1 (32-bit integers); virt and bhyve 2.
    for (name, w) in [
        ("qemu-q35", IntWidth::W32),
        ("qemu-pc", IntWidth::W32),
        ("qemu-virt", IntWidth::W64),
        ("bhyve", IntWidth::W64),
        ("radxa-q8b", IntWidth::W64),
    ] {
        let (i, _) = load(dsdt(name));
        assert_eq!(i.int_width(), w, "{name}");
    }
}

#[test]
fn predefined_objects_answer_as_freebsd() {
    let (mut i, mut h) = load(dsdt("qemu-virt"));
    let osi = |i: &mut Interpreter, h: &mut FakeHost, s: &str| {
        i.evaluate_path("\\_OSI", alloc::vec![Value::String(s.into())], h)
            .unwrap()
    };
    assert_eq!(
        osi(&mut i, &mut h, "Windows 2015"),
        Value::Integer(u64::MAX)
    );
    assert_eq!(
        osi(&mut i, &mut h, "Windows 2022"),
        Value::Integer(u64::MAX)
    );
    assert_eq!(osi(&mut i, &mut h, "Linux"), Value::Integer(0));
    assert_eq!(osi(&mut i, &mut h, "Darwin"), Value::Integer(0));
    assert_eq!(osi(&mut i, &mut h, "Module Device"), Value::Integer(0));
    assert_eq!(
        i.evaluate_path("\\_REV", Vec::new(), &mut h).unwrap(),
        Value::Integer(2)
    );
    assert_eq!(
        i.evaluate_path("\\_OS", Vec::new(), &mut h).unwrap(),
        Value::String("Microsoft Windows NT".into())
    );
    // In a 32-bit namespace, true is 32 ones.
    let (mut i, mut h) = load(dsdt("qemu-q35"));
    assert_eq!(
        osi(&mut i, &mut h, "Windows 2009"),
        Value::Integer(0xffff_ffff)
    );
}

#[test]
fn q35_host_bridge() {
    let (mut i, mut h) = load(dsdt("qemu-q35"));
    let pci0 = node(&i, "\\_SB.PCI0");
    assert_eq!(i.hid(pci0, &mut h).unwrap().as_deref(), Some("PNP0A08"));
    assert_eq!(i.cids(pci0, &mut h).unwrap(), ["PNP0A03"]);
    assert_eq!(i.sta(pci0, &mut h).unwrap(), 0x0f);
    // Decoded by hand from the Name (_CRS, Buffer (0xB6) {...}) bytes.
    let want = alloc::vec![
        producer(AddressWidth::Word, AddressKind::BusNumber, 0, 0xff, 0),
        Resource::Io(Io {
            decode16: true,
            min: 0xcf8,
            max: 0xcf8,
            alignment: 1,
            length: 8,
        }),
        producer(AddressWidth::Word, IO_ALL, 0, 0xcf7, 0),
        producer(AddressWidth::Word, IO_ALL, 0xd00, 0xffff, 0),
        producer(AddressWidth::DWord, MEM_WB, 0xa_0000, 0xb_ffff, 0),
        producer(AddressWidth::DWord, MEM_UC, 0x4000_0000, 0xdfff_ffff, 0),
        producer(AddressWidth::DWord, MEM_UC, 0xf000_0000, 0xfebf_ffff, 0),
        producer(
            AddressWidth::QWord,
            MEM_WB,
            0x3800_0000_0000,
            0x3807_ffff_ffff,
            0
        ),
    ];
    assert_eq!(i.crs(pci0, &mut h).unwrap().unwrap(), want);
    // The MMCONFIG reservation matches the MCFG (0xE0000000, 256 buses).
    let drac = node(&i, "\\_SB.DRAC");
    assert_eq!(i.hid(drac, &mut h).unwrap().as_deref(), Some("PNP0C01"));
    assert_eq!(
        i.crs(drac, &mut h).unwrap().unwrap(),
        [producer(
            AddressWidth::DWord,
            MEM_UC,
            0xe000_0000,
            0xefff_ffff,
            0
        )]
    );
}

#[test]
fn q35_prt_follows_pic_mode() {
    let (mut i, mut h) = load(dsdt("qemu-q35"));
    let pci0 = node(&i, "\\_SB.PCI0");
    // PIC mode (PICF = 0): PRTP, through LNKE..LNKH for slot 0.
    let prt = i.prt(pci0, &mut h).unwrap().unwrap();
    assert_eq!(prt.len(), 128);
    let first: Vec<_> = prt[..4].iter().map(|e| link(&i, e)).collect();
    assert_eq!(
        first,
        ["\\_SB_.LNKE", "\\_SB_.LNKF", "\\_SB_.LNKG", "\\_SB_.LNKH"]
    );
    assert!(prt.iter().all(|e| e.function == 0xffff && e.pin < 4));
    // After _PIC(1), as FreeBSD calls it in APIC mode: PRTA, GSIE..GSIH.
    i.evaluate_path("\\_PIC", alloc::vec![Value::Integer(1)], &mut h)
        .unwrap();
    let prt = i.prt(pci0, &mut h).unwrap().unwrap();
    let first: Vec<_> = prt[..4].iter().map(|e| link(&i, e)).collect();
    assert_eq!(
        first,
        ["\\_SB_.GSIE", "\\_SB_.GSIF", "\\_SB_.GSIG", "\\_SB_.GSIH"]
    );
    // GSIA..GSIH carry GSIs 16..23.
    let gsie = node(&i, "\\_SB.GSIE");
    let crs = i.crs(gsie, &mut h).unwrap().unwrap();
    let Resource::ExtendedIrq(irq) = &crs[0] else {
        panic!("{crs:?}")
    };
    assert_eq!(irq.interrupts, [20]);
    assert_eq!(
        (irq.trigger, irq.polarity),
        (Trigger::Level, Polarity::ActiveHigh)
    );
}

#[test]
fn q35_osc_grants_and_masks() {
    let (mut i, mut h) = load(dsdt("qemu-q35"));
    // PCI Express _OSC UUID 33DB4D5B-1FF7-401C-9657-7441C03DD766.
    let uuid = alloc::vec![
        0x5b, 0x4d, 0xdb, 0x33, 0xf7, 0x1f, 0x1c, 0x40, 0x96, 0x57, 0x74, 0x41, 0xc0, 0x3d, 0xd7,
        0x66,
    ];
    let caps = |c1: u32, c2: u32, c3: u32| {
        let mut b = Vec::new();
        for c in [c1, c2, c3] {
            b.extend_from_slice(&c.to_le_bytes());
        }
        Value::Buffer(b)
    };
    let args = alloc::vec![
        Value::Buffer(uuid.clone()),
        Value::Integer(1),
        Value::Integer(3),
        caps(0, 0x1f, 0x1f)
    ];
    let r = i.evaluate_path("\\_SB.PCI0._OSC", args, &mut h).unwrap();
    // The method masks control to 0x1E and reports "capabilities masked".
    assert_eq!(r, caps(0x10, 0x1f, 0x1e));
    let mut wrong = uuid;
    wrong[0] ^= 1;
    let args = alloc::vec![
        Value::Buffer(wrong),
        Value::Integer(1),
        Value::Integer(3),
        caps(0, 0x1f, 0x1f)
    ];
    let r = i.evaluate_path("\\_SB.PCI0._OSC", args, &mut h).unwrap();
    assert_eq!(r, caps(0x04, 0x1f, 0x1f), "unrecognized UUID");
}

#[test]
fn pc_host_bridge_and_static_prt() {
    let (mut i, mut h) = load(dsdt("qemu-pc"));
    let pci0 = node(&i, "\\_SB.PCI0");
    assert_eq!(i.hid(pci0, &mut h).unwrap().as_deref(), Some("PNP0A03"));
    assert!(i.cids(pci0, &mut h).unwrap().is_empty());
    let prt = i.prt(pci0, &mut h).unwrap().unwrap();
    assert_eq!(prt.len(), 128);
    // Slot 1 INTA goes to LNKS (the PIIX4 power management function).
    let got: Vec<_> = prt[..8].iter().map(|e| link(&i, e)).collect();
    assert_eq!(
        got,
        [
            "\\_SB_.LNKD",
            "\\_SB_.LNKA",
            "\\_SB_.LNKB",
            "\\_SB_.LNKC",
            "\\_SB_.LNKS",
            "\\_SB_.LNKB",
            "\\_SB_.LNKC",
            "\\_SB_.LNKD"
        ]
    );
    assert_eq!((prt[4].device, prt[4].pin), (1, 0));
}

#[test]
fn virt_host_bridge_ecam_and_prt() {
    let (mut i, mut h) = load(dsdt("qemu-virt"));
    let pci0 = node(&i, "\\_SB.PCI0");
    assert_eq!(i.hid(pci0, &mut h).unwrap().as_deref(), Some("PNP0A08"));
    assert_eq!(i.cids(pci0, &mut h).unwrap(), ["PNP0A03"]);
    assert_eq!(
        i.crs(pci0, &mut h).unwrap().unwrap(),
        [
            producer(AddressWidth::Word, AddressKind::BusNumber, 0, 0xff, 0),
            producer(AddressWidth::DWord, MEM_UC, 0x1000_0000, 0x3efe_ffff, 0),
            // Port I/O is memory-mapped at 0x3EFF0000.
            producer(AddressWidth::DWord, IO_ALL, 0, 0xffff, 0x3eff_0000),
            producer(
                AddressWidth::QWord,
                MEM_UC,
                0x80_0000_0000,
                0xff_ffff_ffff,
                0
            ),
        ]
    );
    // The ECAM reservation is the MCFG's window: 0x4010000000, 256 buses.
    let res0 = node(&i, "\\_SB.PCI0.RES0");
    assert_eq!(i.hid(res0, &mut h).unwrap().as_deref(), Some("PNP0C02"));
    assert_eq!(
        i.crs(res0, &mut h).unwrap().unwrap(),
        [producer(
            AddressWidth::QWord,
            MEM_UC,
            0x40_1000_0000,
            0x40_1fff_ffff,
            0
        )]
    );
    // Four entries per slot, swizzled over L000..L003.
    let prt = i.prt(pci0, &mut h).unwrap().unwrap();
    assert_eq!(prt.len(), 128);
    for slot in 0..32u16 {
        let entries: Vec<_> = prt.iter().filter(|e| e.device == slot).collect();
        assert_eq!(entries.len(), 4, "slot {slot}");
        for e in entries {
            let want = alloc::format!("\\_SB_.L00{}", (slot + u16::from(e.pin)) % 4);
            assert_eq!(link(&i, e), want);
        }
    }
    // The links carry GSIs 35..38.
    for (n, gsi) in [("L000", 35), ("L001", 36), ("L002", 37), ("L003", 38)] {
        let l = node(&i, &alloc::format!("\\_SB.{n}"));
        let crs = i.crs(l, &mut h).unwrap().unwrap();
        let Resource::ExtendedIrq(irq) = &crs[0] else {
            panic!("{crs:?}")
        };
        assert_eq!(irq.interrupts, [gsi], "{n}");
    }
    // The PL011 console matches the SPCR (0x09000000, GSI 33).
    let com0 = node(&i, "\\_SB.COM0");
    assert_eq!(i.hid(com0, &mut h).unwrap().as_deref(), Some("ARMH0011"));
    let crs = i.crs(com0, &mut h).unwrap().unwrap();
    assert!(
        matches!(&crs[0], Resource::FixedMemory32(m) if m.min == 0x0900_0000 && m.length == 0x1000)
    );
    assert!(matches!(&crs[1], Resource::ExtendedIrq(x) if x.interrupts == [33]));
}

#[test]
fn bhyve_devices_and_pci_config_region() {
    let (mut i, mut h) = load(dsdt("bhyve"));
    assert_eq!(i.devices().len(), 22);
    let pc00 = node(&i, "\\_SB.PC00");
    assert_eq!(i.hid(pc00, &mut h).unwrap().as_deref(), Some("PNP0A03"));
    // String IDs come back in upper case, as FreeBSD's devinfo shows them
    // ("_HID=BHYVE_V_GEN_COUNTER_V1 ... _CID=VM_GEN_COUNTER").
    let genc = node(&i, "\\_SB.GENC");
    assert_eq!(
        i.hid(genc, &mut h).unwrap().as_deref(),
        Some("BHYVE_V_GEN_COUNTER_V1")
    );
    assert_eq!(i.cids(genc, &mut h).unwrap(), ["VM_GEN_COUNTER"]);
    // PIC mode routes through ISA_.LNKH/LNKA; APIC mode hard-wires GSIs.
    let prt = i.prt(pc00, &mut h).unwrap().unwrap();
    let got: Vec<_> = prt.iter().map(|e| (e.device, link(&i, e))).collect();
    assert_eq!(
        got,
        [
            (2, "\\_SB_.PC00.ISA_.LNKH".into()),
            (3, "\\_SB_.PC00.ISA_.LNKA".into())
        ]
    );
    i.evaluate_path("\\_PIC", alloc::vec![Value::Integer(1)], &mut h)
        .unwrap();
    let prt = i.prt(pc00, &mut h).unwrap().unwrap();
    let got: Vec<_> = prt.iter().map(|e| (e.device, e.source.clone())).collect();
    assert_eq!(got, [(2, PrtSource::Gsi(23)), (3, PrtSource::Gsi(16))]);
    // LNKA's _STA reads PIRA from the LPC bridge's configuration space:
    // device 31 function 0 on _BBN 0, offset 0x60.
    h.log.clear();
    let lnka = node(&i, "\\_SB.PC00.ISA.LNKA");
    let _ = i.sta(lnka, &mut h).unwrap();
    let a = h.log.first().copied().unwrap();
    assert_eq!(
        (a.space, a.address, a.write),
        (RegionSpace::PciConfig, 0x60, false)
    );
    let key = (2u8, (31u64 << 32) | 0x60);
    assert!(!h.mem.contains_key(&key));
    // A routed link reads back from config space.
    h.mem.insert(key, 0x0b);
    assert_eq!(i.sta(lnka, &mut h).unwrap(), 0x0b);
    let crs = i.crs(lnka, &mut h).unwrap().unwrap();
    assert!(
        matches!(&crs[0], Resource::Irq(irq) if irq.mask == 1 << 11),
        "{crs:?}"
    );
}

#[test]
fn q8b_every_device_reports_status_and_resources() {
    let (mut i, mut h) = load(dsdt("radxa-q8b"));
    let devices = i.devices();
    // 163 Device operators in the table (each `5B 82` pair is one).
    assert_eq!(devices.len(), 163);
    let mut with_hid = 0;
    let mut with_crs = 0;
    for &d in &devices {
        i.sta(d, &mut h)
            .unwrap_or_else(|e| panic!("{}: _STA {e:?}", i.path(d)));
        if i.hid(d, &mut h).unwrap().is_some() {
            with_hid += 1;
        }
        if i.crs(d, &mut h)
            .unwrap_or_else(|e| panic!("{}: _CRS {e:?}", i.path(d)))
            .is_some()
        {
            with_crs += 1;
        }
        let _ = i.cids(d, &mut h).unwrap();
        let _ = i.uid(d, &mut h).unwrap();
        let _ = i.adr(d, &mut h).unwrap();
    }
    assert!(with_hid > 100 && with_crs > 50, "{with_hid} {with_crs}");
    let ic13 = node(&i, "\\_SB.IC13");
    assert_eq!(i.hid(ic13, &mut h).unwrap().as_deref(), Some("QCOM0610"));
    assert_eq!(i.cids(ic13, &mut h).unwrap(), ["QCOMFFEA"]);
}

#[test]
fn q8b_serial_and_gpio_resources() {
    let (mut i, mut h) = load(dsdt("radxa-q8b"));
    let mut i2c = 0;
    let mut spi = 0;
    let mut uart = 0;
    let mut gpio = 0;
    for d in i.devices() {
        let Ok(Some(crs)) = i.crs(d, &mut h) else {
            continue;
        };
        for r in crs {
            match r {
                Resource::SerialBus(s) => match s.bus {
                    SerialBusKind::I2c { .. } => i2c += 1,
                    SerialBusKind::Spi { .. } => spi += 1,
                    SerialBusKind::Uart { baud, .. } => {
                        assert_eq!(baud, 115_200);
                        uart += 1;
                    }
                    SerialBusKind::Other { .. } => {}
                },
                Resource::Gpio(g) => {
                    assert!(g.source.name.starts_with("\\_SB."), "{:?}", g.source);
                    gpio += 1;
                }
                _ => {}
            }
        }
    }
    assert!(
        i2c > 0 && spi > 0 && uart > 0 && gpio > 0,
        "{i2c} {spi} {uart} {gpio}"
    );
}

/// The default limits (a 10 KiB stack budget, for a 16 KiB kernel thread
/// stack) serve the QEMU and bhyve firmware; the Q8B's AML nests deeper
/// and runs within the budget for a 64 KiB stack.
#[test]
fn stack_budgets_for_real_firmware() {
    for (name, t) in DSDTS {
        let limits = if *name == "radxa-q8b" {
            crate::Limits::for_stack(64 * 1024)
        } else {
            crate::Limits::default()
        };
        let config = Config {
            limits,
            ..Config::default()
        };
        let mut i = Interpreter::new(config);
        let mut h = FakeHost::default();
        let r = i.load_table(t, &mut h).unwrap();
        assert!(r.warnings.is_empty(), "{name}: {:?}", r.warnings);
        assert!(i.initialize(&mut h).is_empty(), "{name}");
        for d in i.devices() {
            i.sta(d, &mut h).unwrap();
            i.crs(d, &mut h).unwrap();
            i.prt(d, &mut h).unwrap();
        }
        assert!(i.peak_stack() <= limits.stack_budget, "{name}");
    }
}

#[test]
fn initialize_runs_ini_and_reg_without_errors() {
    for (name, t) in DSDTS {
        let (mut i, mut h) = load(t);
        let errors = i.initialize(&mut h);
        assert!(errors.is_empty(), "{name}: {errors:?}");
        for space in [
            RegionSpace::SystemMemory,
            RegionSpace::SystemIo,
            RegionSpace::PciConfig,
        ] {
            let (_, errors) = i.run_reg(space, &mut h);
            assert!(errors.is_empty(), "{name}: {errors:?}");
        }
    }
}

#[test]
#[ignore = "prints the namespace for inspection"]
fn dump() {
    let which = std::env::var("AML_DUMP").unwrap_or_else(|_| "qemu-q35".into());
    let (_, dsdt) = DSDTS.iter().find(|(n, _)| *n == which).unwrap();
    let mut i = Interpreter::new(test_config());
    let mut h = FakeHost::default();
    let r = i.load_table(dsdt, &mut h).unwrap();
    println!("warnings: {:?}", r.warnings);
    for n in i.nodes() {
        println!("{} {:?}", i.path(n), i.object_type(n));
    }
    for d in i.devices() {
        let hid = i.hid(d, &mut h);
        let cid = i.cids(d, &mut h);
        let sta = i.sta(d, &mut h);
        let crs = i.crs(d, &mut h);
        println!("{} hid={hid:?} cid={cid:?} sta={sta:?}", i.path(d));
        println!("    crs={crs:?}");
        match i.prt(d, &mut h) {
            Ok(None) => {}
            Ok(Some(p)) => {
                let mut line = std::string::String::new();
                for e in &p {
                    let src = match &e.source {
                        crate::PrtSource::Link { node, index } => {
                            alloc::format!("{}[{index}]", i.path(*node))
                        }
                        other => alloc::format!("{other:?}"),
                    };
                    line.push_str(&alloc::format!(" {}.{}={src}", e.device, e.pin));
                }
                println!("    prt({}):{line}", p.len());
            }
            Err(e) => println!("    prt error {e:?}"),
        }
    }
}

#[test]
#[ignore = "prints the outcome of invoking every method"]
fn exercise_methods() {
    for (name, dsdt) in DSDTS {
        let (mut i, mut h) = load(dsdt);
        let mut ok = 0;
        let mut errs: std::collections::BTreeMap<std::string::String, usize> =
            std::collections::BTreeMap::new();
        for n in i.nodes() {
            let Some(m) = i.method_info(n) else { continue };
            let args = (0..m.args).map(|_| crate::Value::Integer(0)).collect();
            match i.evaluate_node(n, args, &mut h) {
                Ok(_) => ok += 1,
                Err(e) => {
                    println!("{name}: {} -> {e:?}", i.path(n));
                    *errs.entry(alloc::format!("{e:?}")).or_default() += 1;
                }
            }
        }
        println!(
            "{name}: {ok} ok, peak depth {}, peak stack {}, errors {errs:?}",
            i.peak_depth(),
            i.peak_stack()
        );
        for d in i.devices() {
            if let Ok(deps) = i.dep(d, &mut h) {
                for dep in deps {
                    if let crate::DepRef::Unresolved(s) = dep {
                        println!("{name}: {} _DEP unresolved {s}", i.path(d));
                    }
                }
            }
        }
    }
}

/// The `aml.load` benchmark's payload (q35's DSDT moved under `\KBNS`)
/// loads beside bhyve's namespace without warnings.
#[test]
fn bench_ssdt_loads_beside_bhyve() {
    let (mut i, mut h) = load(dsdt("bhyve"));
    let before = i.node_count();
    let ssdt = include_bytes!("../../../../tests/perf/kbench/ssdt-q35.aml");
    let r = i.load_table(ssdt, &mut h).unwrap();
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert!(
        i.node_count() > before + 250,
        "{before} -> {}",
        i.node_count()
    );
    node(&i, "\\KBNS.KSB.PCI0");
    node(&i, "\\KBNS.KSB.HPET");
}
