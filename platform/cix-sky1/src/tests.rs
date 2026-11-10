// SPDX-License-Identifier: BSD-2-Clause
//! The CIX tables against a synthetic namespace and against the Sky1's
//! firmware (`tests/corpus/acpi/opi6p`).

extern crate std;

use alloc::string::String;
use alloc::vec::Vec;

use acpi::deps::{FirmwareDeps, Via, build};
use acpi_platform::{FirmwareIdentity, Malformed, PlatformSource};
use aml::asm::{cat, device, int, name, name_op, package, table};
use aml::host::{Host, HostError, RegionAccess};
use aml::{AmlPath, Config, Interpreter};
use devtree::graph::EdgeKind;
use devtree::platform::ResourceKind;

use crate::{NAME, PLATFORM};

const PLATFORMS: &[&dyn PlatformSource] = &[&PLATFORM];

fn via(table: &'static str) -> Via {
    Via::Platform {
        source: NAME,
        table,
    }
}

/// Regions read as zero; writes are dropped.
struct Quiet;

impl Host for Quiet {
    fn read(&mut self, _: &RegionAccess) -> Result<u64, HostError> {
        Ok(0)
    }

    fn write(&mut self, _: &RegionAccess, _: u64) -> Result<(), HostError> {
        Ok(())
    }
}

fn node(i: &Interpreter, p: &str) -> aml::NodeId {
    i.lookup(&AmlPath::parse(p).unwrap()).unwrap()
}

/// The rules the Sky1 does not exercise: references as path strings, an
/// entry without a consumer or reset name, malformed entries reported
/// with their position, two lines under one name, and nothing read
/// without `CIXA1019`.
#[test]
fn synthetic_tables() {
    let hid = |h: &str| name_op("_HID", &aml::asm::string(h));
    let s = |t: &str| aml::asm::string(t);
    let build_with = |tables: bool| {
        let body = aml::asm::scope(
            "\\_SB",
            &cat(&[
                &device("REST", &hid(if tables { "CIXA1019" } else { "XNOT0001" })),
                &device("CCLK", &hid("CIXHA010")),
                &device("RST1", &hid("CIXHA021")),
                &device(
                    "UAR0",
                    &cat(&[
                        &hid("XUAR0001"),
                        // No consumer: the carrier. An empty name, twice.
                        &name_op(
                            "CLKT",
                            &package(&[
                                package(&[int(0x107), s("")]),
                                package(&[int(0x108), s("")]),
                            ]),
                        ),
                        // Path strings; no name.
                        &name_op(
                            "RSTL",
                            &package(&[package(&[s("^RST1"), int(4), s("\\_SB.UAR0")])]),
                        ),
                        &name_op(
                            "RSNL",
                            &package(&[
                                package(&[name("UAR0"), int(0x400), int(0), s("irq")]),
                                // An unknown resource type.
                                package(&[name("UAR0"), int(0x100), int(0), s("io")]),
                            ]),
                        ),
                        // A link to nothing, and a short entry.
                        &name_op(
                            "DLKL",
                            &package(&[
                                package(&[s("\\_SB.NONE"), name("UAR0"), int(0)]),
                                package(&[name("RST1")]),
                            ]),
                        ),
                        &name_op("RSVL", &int(5)),
                    ]),
                ),
            ]),
        );
        let mut c = Config::default();
        c.limits.stack_budget = 1 << 20;
        let mut i = Interpreter::new(c);
        i.load_table(&table(b"DSDT", 2, &body), &mut Quiet).unwrap();
        let devs: Vec<aml::NodeId> = ["REST", "CCLK", "RST1", "UAR0"]
            .iter()
            .map(|d| node(&i, &alloc::format!("\\_SB.{d}")))
            .collect();
        let id = FirmwareIdentity::default();
        let deps = build(&mut i, &mut Quiet, &devs, &[], PLATFORMS, &id);
        (deps, devs)
    };

    let (deps, _) = build_with(false);
    assert!(deps.platforms.is_empty());
    assert!(deps.platform.is_empty() && deps.edges.is_empty() && deps.malformed.is_empty());

    let (deps, devs) = build_with(true);
    assert_eq!(deps.platforms, [NAME]);
    let [_, cclk, rst1, uar0] = devs[..] else {
        unreachable!()
    };
    let p = &deps.platform;
    let c = p.clock(&uar0, None).unwrap();
    assert_eq!((c.provider, c.id, c.name.as_deref()), (cclk, 0x107, None));
    let r = p.reset(&uar0, None).unwrap();
    assert_eq!((r.controller, r.index, r.name.as_deref()), (rst1, 4, None));
    assert_eq!(p.resource(&uar0, ResourceKind::Interrupt, "irq"), Some(0));
    assert!(p.links.is_empty() && p.reserved.is_empty());
    let edges: Vec<_> = deps
        .edges
        .iter()
        .map(|e| (e.supplier, e.consumer, e.kind, e.via.clone()))
        .collect();
    assert_eq!(
        edges,
        [
            (cclk, uar0, EdgeKind::Required, via("clocks")),
            (rst1, uar0, EdgeKind::Required, via("resets")),
        ]
    );
    let unresolved: Vec<_> = deps
        .unresolved
        .iter()
        .map(|u| (u.consumer, u.via.clone(), u.name.clone()))
        .collect();
    assert_eq!(
        unresolved,
        [(uar0, via("DLKL"), String::from("\\_SB.NONE"))]
    );
    let m = |source, table, entry: Option<usize>, why: &str| Malformed {
        source,
        table,
        carrier: uar0,
        entry,
        why: String::from(why),
    };
    assert_eq!(
        deps.malformed,
        [
            m(NAME, "RSNL", Some(1), "resource type 0x100"),
            m(NAME, "DLKL", Some(1), "1 elements"),
            m(NAME, "RSVL", None, "not a package"),
            m("platform", "clocks", None, "two lines named None"),
        ]
    );
}

/// The Sky1's dependencies with the platform registered.
fn opi6p() -> (Interpreter, FirmwareDeps) {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let devices = acpi::deps::devices(&mut i, &mut h);
    let dma: Vec<acpi::DmaController> = acpi_corpus::dma_controllers("opi6p")
        .into_iter()
        .map(|(mmio_base, requests)| acpi::DmaController {
            mmio_base,
            requests,
        })
        .collect();
    let id = FirmwareIdentity::default();
    let deps = build(&mut i, &mut h, &devices, &dma, PLATFORMS, &id);
    (i, deps)
}

/// The platform applies to the Sky1 alone among the corpus machines.
#[test]
fn applies_to_the_sky1_only() {
    for (name, _) in acpi_corpus::machines() {
        let (mut i, mut h) = acpi_corpus::load(name);
        let devices = acpi::deps::devices(&mut i, &mut h);
        let applies = PLATFORM.applies(&FirmwareIdentity::default(), &mut i, &mut h, &devices);
        assert_eq!(applies, name == "opi6p", "{name}");
    }
}

/// The Sky1's lookup tables (`\_SB.REST` is `CIXA1019`): every entry
/// reads, every name resolves, and every `_DSD` reference is classified.
#[test]
fn sky1_tables() {
    let (i, deps) = opi6p();
    let p = &deps.platform;
    assert!(deps.malformed.is_empty(), "{:?}", deps.malformed);
    assert!(
        deps.unresolved
            .iter()
            .all(|u| !matches!(u.via, Via::Platform { .. })),
        "{:?}",
        deps.unresolved
    );
    assert!(deps.unclassified.is_empty(), "{:?}", deps.unclassified);
    let counts = (
        p.clocks.len(),
        p.resets.len(),
        p.resource_names.len(),
        p.links.len(),
        p.reserved.len(),
    );
    assert_eq!(counts, (183, 109, 130, 37, 3));
    let edges = |table| deps.edges.iter().filter(|e| e.via == via(table)).count();
    assert_eq!(
        (edges("clocks"), edges("resets"), edges("links")),
        (69, 65, 37)
    );
    let node = |path: &str| node(&i, path);
    let cclk = node("\\_SB.CCLK");
    // A clock line names its consumer, which need not carry the table:
    // DMA0's clock is in DMA1's CLKT.
    let dma0 = node("\\_SB.DMA0");
    let c = p.clock(&dma0, None).unwrap();
    assert_eq!((c.provider, c.id, c.name.as_deref()), (cclk, 0xef, None));
    let com0 = node("\\_SB.COM0");
    assert_eq!(p.clock(&com0, Some("uartclk")).map(|c| c.id), Some(0x107));
    assert!(p.clock(&com0, None).is_none(), "two named clocks");
    // Resets: the FCH controller's I2C line 18, the audio subsystem's own
    // controller for HDA.
    let r = p.reset(&node("\\_SB.I2C0"), Some("i2c_reset")).unwrap();
    assert_eq!((r.controller, r.index), (node("\\_SB.RST1"), 18));
    let r = p.reset(&node("\\_SB.HDA"), None).unwrap();
    assert_eq!((r.controller, r.index), (node("\\_SB.ADSS.ARST"), 14));
    // Resource names: 0x200 counts memory ranges, 0x400 interrupts.
    let prc0 = node("\\_SB.PRC0");
    assert_eq!(p.resource(&prc0, ResourceKind::Memory, "rcsu"), Some(1));
    assert_eq!(
        p.resource(&prc0, ResourceKind::Interrupt, "phy_int"),
        Some(4)
    );
    // Firmware's carveouts for the audio DMA, HDA, and DSP.
    let reserved: Vec<_> = p
        .reserved
        .iter()
        .map(|r| (i.path(r.device).to_text(), r.base, r.size, r.no_map))
        .collect();
    assert_eq!(
        reserved,
        [
            (String::from("\\_SB_.DMA1"), 0xd000_0000, 0x70_0000, true),
            (String::from("\\_SB_.HDA_"), 0xd070_0000, 0x70_0000, true),
            (String::from("\\_SB_.DSP_"), 0xcde0_8000, 0x10_0000, true),
        ]
    );
    // The edges they make.
    for (consumer, supplier, table) in [
        ("\\_SB_.COM0", "\\_SB_.CCLK", "clocks"),
        ("\\_SB_.DMA0", "\\_SB_.CCLK", "clocks"),
        ("\\_SB_.I2C0", "\\_SB_.RST1", "resets"),
        ("\\_SB_.HDA_", "\\_SB_.ADSS.ARST", "resets"),
        ("\\_SB_.HDA_", "\\_SB_.ADSS.ACLK", "links"),
        ("\\_SB_.PRC0", "\\_SB_.PCP0", "links"),
        ("\\_SB_.EDP0", "\\_SB_.DPBL", "links"),
    ] {
        let e = deps.edges.iter().find(|e| {
            e.via == via(table)
                && i.path(e.consumer).to_text() == consumer
                && i.path(e.supplier).to_text() == supplier
        });
        assert_eq!(
            e.map(|e| e.kind),
            Some(EdgeKind::Required),
            "{supplier} -> {consumer}"
        );
    }
    // MAC1 is absent (`_STA` 0), so its tables are not read.
    let mac1 = node("\\_SB.MAC1");
    assert!(p.clocks.iter().all(|c| c.consumer != mac1));
    assert!(deps.edges.iter().all(|e| e.consumer != mac1));
}

/// The vendor property names are not generic ones, and are unique.
#[test]
fn reference_names() {
    let names = PLATFORM.reference_kinds();
    for (k, (a, _)) in names.iter().enumerate() {
        assert!(names[k + 1..].iter().all(|(b, _)| a != b), "{a}");
        assert_eq!(devtree::dsd::reference_kind(a), None, "{a} is generic");
    }
}

/// The SCMI shared memory is declared shared with its mailbox: those two
/// overlaps, and no other, become declared, so each entry still applies.
#[test]
fn scmi_shared_memory_is_declared() {
    use acpi::ranges::{Party, consumed, declared, overlaps};
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let devices = acpi::deps::devices(&mut i, &mut h);
    let x = PLATFORM.extract(&mut i, &mut h, &devices);
    let shared = &x.description.shared;
    let paths: Vec<String> = shared.iter().map(|s| i.path(s.device).to_text()).collect();
    assert_eq!(paths, ["\\_SB_.SHM0", "\\_SB_.SHM1"]);
    let ranges: Vec<_> = devices
        .iter()
        .map(|&d| {
            (
                d,
                consumed(&i.crs(d, &mut h).ok().flatten().unwrap_or_default()),
            )
        })
        .collect();
    let declared = declared(shared, &ranges);
    let got: Vec<(u64, u64)> = declared.iter().map(|r| (r.base, r.len)).collect();
    assert_eq!(got, [(0x0659_0000, 0x80), (0x065a_0000, 0x80)]);
    let pairs: Vec<(String, String)> = overlaps(&ranges, &[], &declared)
        .into_iter()
        .filter_map(|o| match o.with {
            Party::Declared(n) => Some((i.path(o.device).to_text(), i.path(n).to_text())),
            _ => None,
        })
        .collect();
    assert_eq!(
        pairs,
        [
            (String::from("\\_SB_.MBX6"), String::from("\\_SB_.SHM0")),
            (String::from("\\_SB_.MBX7"), String::from("\\_SB_.SHM1")),
        ]
    );
}
