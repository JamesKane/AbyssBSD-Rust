// SPDX-License-Identifier: BSD-2-Clause
//! The `iicbus` enumeration against the Orange Pi 6 Plus: each Cadence
//! controller's devices, in order, with the location and pnpinfo strings
//! FreeBSD's `devinfo -rv` recorded under its `iicbusN`, and the bus
//! clock its `dev.iicbus.N.frequency` reported.

extern crate std;

use alloc::string::String;
use alloc::vec::Vec;

use crate::iicbus::{bus_frequency, children};

const DEVINFO: &str = include_str!("../../../tests/corpus/opi6p/freebsd/devinfo-rv.txt");
const SYSCTL: &str = include_str!("../../../tests/corpus/opi6p/freebsd/sysctl-a.txt");

/// One `iicbus` as FreeBSD recorded it: its controller's handle, its
/// unit, and its ACPI children's pnpinfo and location, in order.
struct Recorded {
    controller: String,
    unit: u32,
    children: Vec<(String, String)>,
}

fn after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_once(key).map(|(_, r)| r)
}

/// The `iicbus` subtrees of `devinfo -rv`.
fn recorded() -> Vec<Recorded> {
    let mut out: Vec<Recorded> = Vec::new();
    let mut controller: Option<String> = None;
    for line in DEVINFO.lines() {
        let t = line.trim_start();
        if t.starts_with("cdnc_i2c") {
            controller = after(t, " at handle=").map(String::from);
        } else if let Some(rest) = t.strip_prefix("iicbus") {
            let unit = rest
                .split_whitespace()
                .next()
                .and_then(|u| u.parse().ok())
                .expect("iicbus unit");
            out.push(Recorded {
                controller: controller.clone().expect("iicbus under a controller"),
                unit,
                children: Vec::new(),
            });
        } else if t.contains(" at addr=") && t.contains(" handle=") {
            let pnp = after(t, " pnpinfo ")
                .and_then(|r| r.split(" at addr=").next())
                .expect("pnpinfo");
            let loc = after(t, " at ").expect("location");
            out.last_mut()
                .expect("child under an iicbus")
                .children
                .push((String::from(pnp), String::from(loc)));
        }
    }
    out
}

fn frequency(unit: u32) -> u32 {
    let key = alloc::format!("dev.iicbus.{unit}.frequency: ");
    SYSCTL
        .lines()
        .find_map(|l| l.strip_prefix(key.as_str()))
        .and_then(|v| v.trim().parse().ok())
        .expect("frequency recorded")
}

#[test]
fn opi6p_buses_match_freebsd() {
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let buses = recorded();
    assert_eq!(buses.len(), 7, "seven iicbus recorded");
    for bus in &buses {
        let path = aml::AmlPath::parse(&bus.controller).expect("path");
        let ctl = i.lookup(&path).expect("controller in the namespace");
        let found = children(&mut i, &mut h, ctl);
        let mut got = Vec::new();
        for c in &found {
            let ident = crate::bus::enumerated(&mut i, &mut h, c.node).expect("present");
            let mut pnp = String::new();
            devtree::strings::acpi_pnpinfo(&mut pnp, &ident).unwrap();
            let mut loc = String::new();
            devtree::strings::iic_location(&mut loc, c.address, Some(&c.path)).unwrap();
            got.push((pnp, loc));
        }
        assert_eq!(
            got, bus.children,
            "iicbus{} on {}",
            bus.unit, bus.controller
        );
        assert_eq!(
            bus_frequency(&found),
            frequency(bus.unit),
            "iicbus{} frequency",
            bus.unit
        );
    }
}
