// SPDX-License-Identifier: BSD-2-Clause
//! The Darwin `_OSI` answer, against a DSDT in Apple's pattern: `\_SB._INI`
//! tests `_OSI("Darwin")` and then Windows strings, each match setting
//! `OSYS`, so a Windows answer after Darwin's would overwrite it. The
//! Mac Pro's own tables replace this once captured (`tests/corpus/macpro61`,
//! docs/MACPRO.md MP0).

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
            &when("Darwin", 10000),
            &when("Windows 2001", 2001),
            &when("Windows 2009", 2009),
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
    assert_eq!(osys(&made_by("Apple Inc."), Some("0")), 2009);
}

#[test]
fn other_makers_keep_freebsds_answers() {
    assert_eq!(osys(&made_by("QEMU"), None), 2009);
    assert_eq!(osys(&FirmwareIdentity::default(), None), 2009);
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
