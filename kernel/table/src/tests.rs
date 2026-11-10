// SPDX-License-Identifier: BSD-2-Clause

extern crate alloc;
extern crate std;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use std::{format, println};

use bus::{DriverDecl, Ids};
use devtree::ids::{AcpiIdent, HidIdent, PciIdent, UsbIdent, VirtioIdent};
use devtree::matching::{Candidate, Matches, Witness, find_ambiguities};

use crate::DRIVERS;

const PCICONF: &str = include_str!("../../../tests/corpus/freebsd/bhyve/pciconf-lv.txt");
const DEVINFO: &str = include_str!("../../../tests/corpus/freebsd/bhyve/devinfo-rv.txt");

/// `key=0x…` in a line, as a number.
fn hex(line: &str, key: &str) -> Option<u32> {
    let v = line.split_whitespace().find_map(|w| w.strip_prefix(key))?;
    u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()
}

/// The PCI functions `pciconf -lv` recorded.
fn recorded_pci() -> Vec<PciIdent> {
    PCICONF
        .lines()
        .filter(|l| l.contains("@pci"))
        .filter_map(|l| {
            Some(PciIdent {
                vendor: u16::try_from(hex(l, "vendor=")?).ok()?,
                device: u16::try_from(hex(l, "device=")?).ok()?,
                subvendor: u16::try_from(hex(l, "subvendor=")?).ok()?,
                subdevice: u16::try_from(hex(l, "subdevice=")?).ok()?,
                class: hex(l, "class=")?,
                revision: u8::try_from(hex(l, "rev=")?).ok()?,
            })
        })
        .collect()
}

/// The ACPI devices `devinfo -rv` recorded.
fn recorded_acpi() -> Vec<AcpiIdent> {
    let field = |l: &str, key: &str| {
        l.split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .filter(|v| *v != "none")
            .map(ToString::to_string)
    };
    DEVINFO
        .lines()
        .filter(|l| l.contains(" _HID="))
        .map(|l| AcpiIdent {
            hid: field(l, "_HID="),
            cids: field(l, "_CID=").into_iter().collect(),
            uid: field(l, "_UID="),
        })
        .collect()
}

/// The virtio devices `devinfo -rv` recorded.
fn recorded_virtio() -> Vec<VirtioIdent> {
    DEVINFO
        .lines()
        .filter(|l| l.contains(" device_type="))
        .filter_map(|l| {
            Some(VirtioIdent {
                vendor: hex(l, "vendor=")?,
                device: u16::try_from(hex(l, "device=")?).ok()?,
                subvendor: u16::try_from(hex(l, "subvendor=")?).ok()?,
                device_type: hex(l, "device_type=")?,
            })
        })
        .collect()
}

/// The ties among the drivers whose tables `pick` selects, over the
/// recorded identities and one witness per table entry.
fn ties<I: core::fmt::Debug, Id: Matches<I> + Witness<I> + 'static>(
    drivers: &[DriverDecl],
    pick: impl Fn(&Ids) -> Option<&'static [Id]>,
    recorded: Vec<I>,
) -> Vec<String> {
    let candidates: Vec<Candidate<'_, Id>> = drivers
        .iter()
        .filter_map(|d| {
            Some(Candidate {
                name: d.name,
                ids: pick(&d.ids)?,
                yields: d.yields,
            })
        })
        .collect();
    let mut corpus = recorded;
    corpus.extend(
        candidates
            .iter()
            .flat_map(|c| c.ids.iter().map(Witness::witness)),
    );
    find_ambiguities(&candidates, &corpus)
        .into_iter()
        .map(|a| {
            format!(
                "{} and {} both match {:?} at rank {}",
                a.drivers.0, a.drivers.1, corpus[a.ident], a.rank.0
            )
        })
        .collect()
}

/// Every unresolved tie in `drivers`.
fn check(drivers: &[DriverDecl]) -> Vec<String> {
    let mut out = ties(
        drivers,
        |i| if let Ids::Pci(t) = i { Some(*t) } else { None },
        recorded_pci(),
    );
    out.extend(ties(
        drivers,
        |i| if let Ids::Acpi(t) = i { Some(*t) } else { None },
        recorded_acpi(),
    ));
    out.extend(ties(
        drivers,
        |i| {
            if let Ids::Virtio(t) = i {
                Some(*t)
            } else {
                None
            }
        },
        recorded_virtio(),
    ));
    out.extend(ties(
        drivers,
        |i| if let Ids::Usb(t) = i { Some(*t) } else { None },
        Vec::<UsbIdent>::new(),
    ));
    out.extend(ties(
        drivers,
        |i| if let Ids::Hid(t) = i { Some(*t) } else { None },
        Vec::<HidIdent>::new(),
    ));
    // Name-matched and bus-kind drivers: one per name or kind.
    let mut nexus: Vec<&str> = Vec::new();
    for d in drivers {
        if let Ids::Nexus(names) = d.ids {
            for n in names {
                if nexus.contains(n) {
                    out.push(format!("two drivers claim nexus child {n:?}"));
                }
                nexus.push(n);
            }
        }
    }
    if drivers
        .iter()
        .filter(|d| matches!(d.ids, Ids::PciBus))
        .count()
        > 1
    {
        out.push("two drivers claim PCI buses".into());
    }
    out
}

#[test]
fn the_driver_table_has_no_ties() {
    let ties = check(DRIVERS);
    for t in &ties {
        println!("driver table: {t}");
    }
    assert!(
        ties.is_empty(),
        "{} unresolved tie(s) in the driver table",
        ties.len()
    );
}

#[test]
fn the_corpus_parses() {
    let pci = recorded_pci();
    assert!(pci.len() >= 8, "{} PCI functions", pci.len());
    assert!(
        pci.iter()
            .any(|i| i.vendor == 0x1af4 && i.device == 0x1005 && i.subdevice == 4)
    );
    let acpi = recorded_acpi();
    assert!(acpi.iter().any(|i| i.hid.as_deref() == Some("PNP0A03")));
    assert!(recorded_virtio().iter().any(|i| i.device_type == 4));
}

#[test]
fn a_duplicate_is_caught() {
    // The legacy transport at the modern one's rank: a tie on every virtio
    // function.
    let mut legacy = virtio::pci::LEGACY;
    legacy.ids = virtio::pci::MODERN.ids;
    let ties = check(&[virtio::pci::MODERN, legacy]);
    assert!(
        ties.iter().any(|t| t.contains("virtio_pci and virtio_pci")),
        "{ties:?}"
    );
    // A second driver for a device type: tied on the recorded device and
    // on both tables' witnesses.
    let mut other = virtio_random::DRIVER;
    other.name = "vtrnd2";
    let ties = check(&[virtio_random::DRIVER, other]);
    assert_eq!(ties.len(), 3, "{ties:?}");
    assert!(ties.iter().all(|t| t.starts_with("vtrnd and vtrnd2")));
}

/// Every corpus machine with every built-in platform, as the kernel
/// builds its graph (ADR-0020): at most one platform claims a machine,
/// no required or power edge closes a cycle, every `_DSD` reference is
/// classified, and every platform entry reads.
#[test]
fn every_machine_with_every_platform() {
    use acpi_platform::FirmwareIdentity;
    use devtree::graph::EdgeKind;

    for (name, _) in acpi_corpus::machines() {
        let (mut i, mut h) = acpi_corpus::load(name);
        let devices = acpi::deps::devices(&mut i, &mut h);
        let dma: Vec<acpi::DmaController> = acpi_corpus::dma_controllers(name)
            .into_iter()
            .map(|(mmio_base, requests)| acpi::DmaController {
                mmio_base,
                requests,
            })
            .collect();
        let id = FirmwareIdentity::default();
        let deps = acpi::deps::build(&mut i, &mut h, &devices, &dma, crate::PLATFORMS, &id);
        assert!(deps.platforms.len() <= 1, "{name}: {:?}", deps.platforms);
        let unclassified: Vec<_> = deps
            .unclassified
            .iter()
            .map(|u| u.property.clone())
            .collect();
        assert!(unclassified.is_empty(), "{name}: {unclassified:?}");
        assert!(deps.malformed.is_empty(), "{name}: {:?}", deps.malformed);
        let mut g = deps.graph(&i);
        let dropped = g.break_cycles().unwrap_or_else(|c| panic!("{name}: {c:?}"));
        for e in dropped {
            assert!(
                matches!(e.kind, EdgeKind::Optional | EdgeKind::Order),
                "{name}: {} -> {} {:?}",
                i.path(e.supplier),
                i.path(e.consumer),
                e.kind
            );
        }
        g.topo_order().unwrap();
        println!(
            "{name}: platforms {:?}, {} edges",
            deps.platforms,
            deps.edges.len()
        );
    }
}

/// The devices of `name` that have a driver in the build but would wait
/// forever: each with the suppliers no driver serves that hold it back
/// (directly or through a supplier that waits itself).
fn stuck(name: &str) -> Vec<(String, Vec<String>)> {
    use acpi::deps::Provider;
    use acpi_platform::FirmwareIdentity;
    use alloc::collections::{BTreeMap, BTreeSet};

    let (mut i, mut h) = acpi_corpus::load(name);
    let devices = acpi::deps::devices(&mut i, &mut h);
    let dma: Vec<acpi::DmaController> = acpi_corpus::dma_controllers(name)
        .into_iter()
        .map(|(mmio_base, requests)| acpi::DmaController {
            mmio_base,
            requests,
        })
        .collect();
    let id = FirmwareIdentity::default();
    let deps = acpi::deps::build(&mut i, &mut h, &devices, &dma, crate::PLATFORMS, &id);
    let mut driven = BTreeSet::new();
    for &d in &devices {
        let ident = AcpiIdent {
            hid: i.hid(d, &mut h).ok().flatten(),
            cids: i.cids(d, &mut h).unwrap_or_default(),
            uid: None,
        };
        let has = DRIVERS.iter().any(|drv| match drv.ids {
            Ids::Acpi(t) => t.iter().any(|e| e.rank_for(&ident).is_some()),
            _ => false,
        });
        if has {
            driven.insert(d);
        }
    }
    // Each device's gating suppliers: the device serving each, or `None`
    // for one nothing will serve.
    let mut needs: BTreeMap<aml::NodeId, BTreeSet<Option<aml::NodeId>>> = BTreeMap::new();
    for e in deps.edges.iter().filter(|e| e.kind.gates_attach()) {
        let server = match e.provider {
            Provider::Device(n) | Provider::Sub { owner: n, .. } => Some(n),
            Provider::Bus(_) => continue,
            Provider::Absent(_) => None,
        };
        needs.entry(e.consumer).or_default().insert(server);
    }
    // The unserved suppliers behind each device, to a fixed point.
    let mut roots: BTreeMap<aml::NodeId, BTreeSet<String>> = BTreeMap::new();
    loop {
        let mut changed = false;
        for (&d, sups) in &needs {
            let mut mine: BTreeSet<String> = BTreeSet::new();
            for s in sups {
                match s {
                    None => {
                        mine.insert("(absent)".into());
                    }
                    Some(s) if !driven.contains(s) => {
                        mine.insert(i.path(*s).to_text());
                    }
                    Some(s) => mine.extend(roots.get(s).into_iter().flatten().cloned()),
                }
            }
            if roots.get(&d) != Some(&mine) && !mine.is_empty() {
                roots.insert(d, mine);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut out: Vec<(String, Vec<String>)> = roots
        .into_iter()
        .filter(|(d, _)| driven.contains(d))
        .map(|(d, r)| (i.path(d).to_text(), r.into_iter().collect()))
        .collect();
    out.sort();
    out
}

/// No device with a driver in the build waits forever, on any corpus
/// machine, except those listed with the suppliers that hold them: the
/// ledger of what P8 still owes (ROADMAP: no node stays `Waiting` without
/// a named reason).
#[test]
fn nothing_with_a_driver_waits_forever() {
    let pair = |d: &str, r: &[&str]| {
        (
            String::from(d),
            r.iter().map(|s| String::from(*s)).collect::<Vec<_>>(),
        )
    };
    // COM0, COM1, and COM3 name the FCH CRU (`sky1,fch_cru`), which has
    // no driver yet.
    let sky1 = alloc::vec![
        pair("\\_SB_.COM0", &["\\_SB_.UCRU"]),
        pair("\\_SB_.COM1", &["\\_SB_.UCRU"]),
        pair("\\_SB_.COM3", &["\\_SB_.UCRU"]),
    ];
    // The Q8B (Tier 2): two PCIe hosts wait for Qualcomm's power
    // framework device and its PCIe power device.
    let q8b = alloc::vec![
        pair("\\_SB_.PCI2", &["\\_SB_.PEP0", "\\_SB_.QPPX"]),
        pair("\\_SB_.PCI4", &["\\_SB_.PEP0", "\\_SB_.QPPX"]),
    ];
    for (name, _) in acpi_corpus::machines() {
        let want = match name {
            "opi6p" => sky1.clone(),
            "radxa-q8b" => q8b.clone(),
            _ => Vec::new(),
        };
        assert_eq!(stuck(name), want, "{name}");
    }
}
