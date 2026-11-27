// SPDX-License-Identifier: BSD-2-Clause
//! The Darwin `_OSI` answer, against a DSDT in the Mac Pro (Late 2013)'s
//! own pattern (`tests/corpus/macpro61/freebsd/acpidump.dsl`,
//! `\_SB.PCI0._INI`): `OSYS` starts at 2012, then `_OSI("Darwin")` sets
//! 10000, `Linux` 1000, `Windows 2009` 2009 and `Windows 2012` 2012, so a
//! Windows answer after Darwin's would overwrite it. The firmware's 296
//! `OSDW()` checks key on `OSYS == 10000`. The raw tables replace this
//! DSDT once captured (`tests/corpus/acpi/macpro61`).

extern crate std;

use std::string::String;
use std::vec::Vec;

use acpi_platform::{FirmwareIdentity, osi_interfaces};
use aml::asm::{cat, if_, int, method, name, name_op, scope, store, string, table};
use aml::host::{Host, HostError, RegionAccess};
use aml::{Config, FREEBSD_OSI, Interpreter, Value};

use crate::{DARWIN_OSI_TUNABLE, PLATFORM};

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

/// `If (\_OSI (s)) { Store (v, \OSYS) }`.
fn when(s: &str, v: u64) -> Vec<u8> {
    if_(
        &cat(&[&name("\\_OSI"), &string(s)]),
        &store(&int(v), &name("\\OSYS")),
    )
}

fn apple_dsdt() -> Vec<u8> {
    let ini = method(
        "_INI",
        0,
        &cat(&[
            &store(&int(2012), &name("\\OSYS")),
            &when("Darwin", 10000),
            &when("Linux", 1000),
            &when("Windows 2009", 2009),
            &when("Windows 2012", 2012),
        ]),
    );
    table(
        b"DSDT",
        2,
        &cat(&[&name_op("OSYS", &int(0)), &scope("\\_SB", &ini)]),
    )
}

fn made_by(maker: &str) -> FirmwareIdentity {
    let mut id = FirmwareIdentity::default();
    id.system.maker = String::from(maker);
    id.system.product = String::from("MacPro6,1");
    id
}

/// `OSYS` after loading the DSDT and running `_INI`, with `_OSI` as the
/// platforms leave it for `id` and the tunable at `tunable`.
fn osys(id: &FirmwareIdentity, tunable: Option<&str>) -> u64 {
    let out = osi_interfaces(FREEBSD_OSI, &[&PLATFORM], id, |t| {
        assert_eq!(t, DARWIN_OSI_TUNABLE);
        tunable.and_then(|v| v.parse::<i64>().ok()).map(|n| n != 0)
    });
    let mut i = Interpreter::new(Config {
        osi: out.interfaces,
        ..Config::default()
    });
    let mut h = Quiet;
    i.load_table(&apple_dsdt(), &mut h).expect("DSDT loads");
    assert!(i.initialize(&mut h).is_empty(), "_INI runs cleanly");
    match i.evaluate_path("\\OSYS", Vec::new(), &mut h) {
        Ok(Value::Integer(n)) => n,
        other => panic!("OSYS: {other:?}"),
    }
}

#[test]
fn apple_firmware_sees_darwin_and_no_windows() {
    assert_eq!(osys(&made_by("Apple Inc."), None), 10000);
    assert_eq!(osys(&made_by("Apple Computer, Inc."), None), 10000);
    // Set to anything but 0, it stays on.
    assert_eq!(osys(&made_by("Apple Inc."), Some("1")), 10000);
}

#[test]
fn the_tunable_at_zero_gives_freebsds_windows_answers() {
    assert_eq!(osys(&made_by("Apple Inc."), Some("0")), 2012);
}

#[test]
fn other_makers_keep_freebsds_answers() {
    assert_eq!(osys(&made_by("QEMU"), None), 2012);
    assert_eq!(osys(&FirmwareIdentity::default(), None), 2012);
}

#[test]
fn the_feature_group_string_survives() {
    let out = osi_interfaces(FREEBSD_OSI, &[&PLATFORM], &made_by("Apple Inc."), |_| None);
    assert_eq!(
        out.interfaces,
        ["Extended Address Space Descriptor", "Darwin"].map(String::from)
    );
    assert_eq!(out.applied, [crate::NAME]);
}

#[test]
fn only_the_mac_pro_gets_smc_properties() {
    use acpi_platform::PlatformSource;
    let props = PLATFORM.properties(&made_by("Apple Inc."));
    assert_eq!(props, crate::MACPRO6_1);
    assert!(
        props
            .iter()
            .all(|p| p.hid == crate::SMC_HID && !p.why.is_empty())
    );
    let mut other = made_by("Apple Inc.");
    other.system.product = String::from("MacBookPro11,1");
    assert!(PLATFORM.properties(&other).is_empty());
    assert!(PLATFORM.properties(&made_by("QEMU")).is_empty());
}

/// The Mac Pro's own AML (`tests/corpus/acpi/macpro61`), with `_OSI` as
/// this platform and the tunable leave it: `\_SB.PCI0._INI` sets `OSYS`.
fn macpro(tunable: Option<&str>) -> (aml::Interpreter, acpi_corpus::Machine) {
    let out = osi_interfaces(FREEBSD_OSI, &[&PLATFORM], &made_by("Apple Inc."), |_| {
        tunable.and_then(|v| v.parse::<i64>().ok()).map(|n| n != 0)
    });
    let (_, tables) = acpi_corpus::machines()
        .into_iter()
        .find(|(n, _)| *n == "macpro61")
        .expect("the Mac Pro corpus");
    acpi_corpus::load_tables_with_osi("macpro61", &tables, &out.interfaces)
}

#[test]
fn the_mac_pros_firmware_sees_darwin() {
    for (tunable, osys, osdw) in [
        (None, 10000, 1),
        (Some("1"), 10000, 1),
        (Some("0"), 2012, 0),
    ] {
        let (mut i, mut h) = macpro(tunable);
        assert_eq!(
            i.evaluate_path("\\OSYS", Vec::new(), &mut h).unwrap(),
            Value::Integer(osys),
            "{tunable:?}"
        );
        assert_eq!(
            i.evaluate_path("\\OSDW", Vec::new(), &mut h).unwrap(),
            Value::Integer(osdw),
            "{tunable:?}"
        );
    }
}

/// Every property `MACPRO6_1` states is for a device the Mac Pro's
/// namespace has (`sys/acpi` reports a property for an absent one).
#[test]
fn the_mac_pros_smc_is_where_its_properties_go() {
    let (mut i, mut h) = macpro(None);
    let hids: std::collections::BTreeSet<_> = i
        .devices()
        .into_iter()
        .filter_map(|d| i.hid(d, &mut h).ok().flatten())
        .collect();
    for p in crate::MACPRO6_1 {
        assert!(hids.contains(p.hid), "{} for {}", p.name, p.hid);
    }
}
