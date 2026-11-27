// SPDX-License-Identifier: BSD-2-Clause
//! [`osi_interfaces`]: the `_OSI` answers after the platforms' changes.

extern crate std;

use alloc::string::String;
use alloc::vec::Vec;

use aml::{FREEBSD_OSI, Host, Interpreter, NodeId};

use crate::{FirmwareIdentity, OsiChanges, PlatformSource, osi_interfaces};

/// A platform that makes `changes` when the maker is `maker`.
struct Fake {
    name: &'static str,
    maker: &'static str,
    changes: OsiChanges,
}

impl PlatformSource for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn applies(
        &self,
        _: &FirmwareIdentity,
        _: &mut Interpreter,
        _: &mut dyn Host,
        _: &[NodeId],
    ) -> bool {
        false
    }
    fn osi(&self, id: &FirmwareIdentity) -> OsiChanges {
        if id.system.maker == self.maker {
            self.changes
        } else {
            OsiChanges::NONE
        }
    }
}

const DARWIN: OsiChanges = OsiChanges {
    drop_vendor_strings: true,
    add: &["Darwin"],
    tunable: Some("hw.acpi.osi_test"),
};

fn made_by(maker: &str) -> FirmwareIdentity {
    let mut id = FirmwareIdentity::default();
    id.system.maker = String::from(maker);
    id
}

fn unset(_: &str) -> Option<bool> {
    None
}

#[test]
fn no_platform_keeps_freebsds_defaults() {
    let out = osi_interfaces(FREEBSD_OSI, &[], &made_by("Apple Inc."), unset);
    let want: Vec<String> = FREEBSD_OSI.iter().map(|s| String::from(*s)).collect();
    assert_eq!(out.interfaces, want);
    assert!(out.applied.is_empty() && out.vetoed.is_empty() && out.conflicts.is_empty());
}

#[test]
fn dropping_vendor_strings_keeps_the_feature_groups() {
    let p = Fake {
        name: "fake",
        maker: "Maker",
        changes: DARWIN,
    };
    let out = osi_interfaces(FREEBSD_OSI, &[&p], &made_by("Maker"), unset);
    assert_eq!(
        out.interfaces,
        ["Extended Address Space Descriptor", "Darwin"].map(String::from)
    );
    assert_eq!(out.applied, ["fake"]);
    // Another maker: nothing changes.
    let out = osi_interfaces(FREEBSD_OSI, &[&p], &made_by("Other"), unset);
    assert_eq!(out.interfaces.len(), FREEBSD_OSI.len());
    assert!(out.applied.is_empty());
}

#[test]
fn the_tunable_at_zero_vetoes_the_change() {
    let p = Fake {
        name: "fake",
        maker: "Maker",
        changes: DARWIN,
    };
    let off = |t: &str| (t == "hw.acpi.osi_test").then_some(false);
    let out = osi_interfaces(FREEBSD_OSI, &[&p], &made_by("Maker"), off);
    assert_eq!(out.interfaces.len(), FREEBSD_OSI.len());
    assert!(!out.interfaces.iter().any(|s| s == "Darwin"));
    assert_eq!(out.vetoed, [("fake", "hw.acpi.osi_test")]);
    // Set to anything but 0, it stays on.
    let on = |_: &str| Some(true);
    let out = osi_interfaces(FREEBSD_OSI, &[&p], &made_by("Maker"), on);
    assert!(out.interfaces.iter().any(|s| s == "Darwin"));
}

#[test]
fn a_vendor_string_added_against_a_drop_is_a_conflict() {
    let darwin = Fake {
        name: "darwin",
        maker: "Maker",
        changes: DARWIN,
    };
    let windows = Fake {
        name: "windows",
        maker: "Maker",
        changes: OsiChanges {
            drop_vendor_strings: false,
            add: &["Windows 2015"],
            tunable: None,
        },
    };
    for order in [[&darwin, &windows], [&windows, &darwin]] {
        let platforms: Vec<&dyn PlatformSource> =
            order.iter().map(|p| *p as &dyn PlatformSource).collect();
        let out = osi_interfaces(FREEBSD_OSI, &platforms, &made_by("Maker"), unset);
        assert_eq!(out.conflicts, [("windows", "Windows 2015")]);
        // Drops first, then additions: the same set either way.
        let mut got = out.interfaces.clone();
        got.sort();
        assert_eq!(
            got,
            [
                "Darwin",
                "Extended Address Space Descriptor",
                "Windows 2015"
            ]
            .map(String::from)
        );
    }
}
