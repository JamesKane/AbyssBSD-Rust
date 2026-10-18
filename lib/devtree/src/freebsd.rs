// SPDX-License-Identifier: BSD-2-Clause
//! Differential tests against FreeBSD 15.1's own output, recorded in bhyve
//! with the P3 reference devices (`cargo xtask --remote freebsd-record`;
//! tests/corpus/freebsd/bhyve). For every PCI function `pciconf -lv`
//! reports, the pnpinfo and location strings built here from its numbers
//! must equal what `devinfo -rv` printed, byte for byte.

extern crate std;

use alloc::string::String;
use alloc::vec::Vec;

use crate::ids::{AcpiIdent, PciIdent, VirtioIdent};
use crate::strings::{PciAddress, acpi_pnpinfo, pci_location, pci_pnpinfo, virtio_pnpinfo};

const DEVINFO: &str = include_str!("../../../tests/corpus/freebsd/bhyve/devinfo-rv.txt");
const PCICONF: &str = include_str!("../../../tests/corpus/freebsd/bhyve/pciconf-lv.txt");

/// `name <desc> pnpinfo <pnp> [at <location>]` lines of `devinfo -rv`.
fn devinfo() -> Vec<(String, String, Option<String>)> {
    DEVINFO
        .lines()
        .map(str::trim)
        .filter_map(|l| {
            let name = l.split_whitespace().next()?;
            let pnp_at = l.find(" pnpinfo ")?;
            let rest = &l[pnp_at + " pnpinfo ".len()..];
            let (pnp, loc) = match rest.find(" at ") {
                Some(i) => (&rest[..i], Some(rest[i + 4..].into())),
                None => (rest, None),
            };
            Some((name.into(), pnp.into(), loc))
        })
        .collect()
}

fn field(line: &str, key: &str) -> u32 {
    let v = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} in {line}"));
    u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap()
}

/// `name@pciD:B:S:F: class=… rev=… hdr=… vendor=… device=… subvendor=… subdevice=…`.
fn pciconf() -> Vec<(String, PciAddress, PciIdent)> {
    PCICONF
        .lines()
        .filter(|l| l.contains("@pci"))
        .map(|l| {
            let (name, rest) = l.split_once("@pci").unwrap();
            let dbsf: Vec<u32> = rest
                .split(':')
                .take(4)
                .map(|n| n.trim().parse().unwrap())
                .collect();
            let addr = PciAddress {
                domain: dbsf[0],
                bus: u8::try_from(dbsf[1]).unwrap(),
                slot: u8::try_from(dbsf[2]).unwrap(),
                function: u8::try_from(dbsf[3]).unwrap(),
            };
            let n16 = |k| u16::try_from(field(l, k)).unwrap();
            let ident = PciIdent {
                vendor: n16("vendor="),
                device: n16("device="),
                subvendor: n16("subvendor="),
                subdevice: n16("subdevice="),
                class: field(l, "class="),
                revision: u8::try_from(field(l, "rev=")).unwrap(),
            };
            (name.into(), addr, ident)
        })
        .collect()
}

#[test]
fn pci_strings_match_freebsd() {
    let dev = devinfo();
    let functions = pciconf();
    assert!(functions.len() >= 7, "the reference devices are all there");
    for (name, addr, ident) in &functions {
        let (_, pnp, loc) = dev
            .iter()
            .find(|(n, _, _)| n == name)
            .unwrap_or_else(|| panic!("{name} missing from devinfo"));
        let mut ours = String::new();
        pci_pnpinfo(&mut ours, ident).unwrap();
        assert_eq!(&ours, pnp, "{name} pnpinfo");
        // An ACPI-described function's location adds its handle; take the
        // handle from FreeBSD's line and check the rest is ours.
        let loc = loc.as_deref().unwrap();
        let handle = loc.split(" handle=").nth(1);
        let mut ours = String::new();
        pci_location(&mut ours, addr, handle, None).unwrap();
        assert_eq!(ours, loc, "{name} location");
    }
}

#[test]
fn virtio_and_acpi_formats_match_freebsd() {
    let dev = devinfo();
    let (_, pnp, loc) = dev.iter().find(|(n, _, _)| n == "vtblk0").unwrap();
    let mut ours = String::new();
    virtio_pnpinfo(
        &mut ours,
        &VirtioIdent {
            vendor: 0x1af4,
            device: 0x1001,
            subvendor: 0x1af4,
            device_type: 2,
        },
    )
    .unwrap();
    assert_eq!(&ours, pnp);
    assert!(loc.is_none(), "virtio children have no location");
    // An ACPI device: COM1 is _HID PNP0501, _UID 1, no _CID.
    let (_, pnp, _) = dev.iter().find(|(n, _, _)| n == "uart0").unwrap();
    let mut ours = String::new();
    acpi_pnpinfo(
        &mut ours,
        &AcpiIdent {
            hid: Some("PNP0501".into()),
            cids: Vec::new(),
            uid: Some("1".into()),
        },
    )
    .unwrap();
    assert_eq!(&ours, pnp);
}
