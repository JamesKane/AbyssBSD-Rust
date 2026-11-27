// SPDX-License-Identifier: BSD-2-Clause
//! A device's own `_DSD` device properties (daffd814-…), for its driver:
//! FreeBSD's `acpi_dsd_property` / `device_get_property` for ACPI. Where
//! the `_DSD` lacks one, a property an applying platform states for the
//! device's `_HID` stands in (ADR-0027 §3): the firmware wins.

use aml::Value;

use crate::bus::AcpiDevice;
use crate::service::service;

/// The value of `node`'s device property `name`: its `_DSD`'s, else an
/// applying platform's for its `_HID`.
pub fn property(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    node: aml::NodeId,
    name: &str,
) -> Option<Value> {
    dsd_property(interp, host, node, name).or_else(|| {
        let hid = interp.hid(node, host).ok()??;
        let d = service()?.platform_property(&hid, name)?;
        Some(to_value(d.value))
    })
}

/// A platform property's value as AML would have given it.
#[must_use]
pub fn to_value(v: acpi_platform::PropertyValue) -> Value {
    use acpi_platform::PropertyValue as P;
    match v {
        P::Integer(n) => Value::Integer(n),
        P::String(s) => Value::String(s.into()),
        P::Integers(ns) => Value::Package(ns.iter().map(|&n| Value::Integer(n)).collect()),
        P::Strings(ss) => Value::Package(ss.iter().map(|&s| Value::String(s.into())).collect()),
    }
}

/// The value of `node`'s `_DSD` device property `name`, if it has one.
fn dsd_property(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    node: aml::NodeId,
    name: &str,
) -> Option<Value> {
    let dsd = interp
        .evaluate_child(node, "_DSD", alloc::vec::Vec::new(), host)
        .ok()??;
    let Value::Package(items) = dsd else {
        return None;
    };
    for pair in items.chunks(2) {
        let [Value::Buffer(uuid), Value::Package(entries)] = pair else {
            continue;
        };
        if uuid.as_slice() != crate::deps::PROPERTIES_UUID {
            continue;
        }
        for e in entries {
            if let Value::Package(kv) = e
                && let [Value::String(k), v] = kv.as_slice()
                && k == name
            {
                return Some(v.clone());
            }
        }
    }
    None
}

/// Device property `name` of the device being attached.
pub fn value(cx: &bus::AttachCx<'_>, name: &'static str) -> Option<Value> {
    let node = cx.bus_data::<AcpiDevice>()?.node;
    let svc = service()?;
    svc.run(cx.sleepable(), move |interp, host| {
        property(interp, host, node, name)
    })
}

/// The integer `_DSD` property `name` of the device being attached.
pub fn integer(cx: &bus::AttachCx<'_>, name: &'static str) -> Option<u64> {
    let node = cx.bus_data::<AcpiDevice>()?.node;
    let svc = service()?;
    svc.run(cx.sleepable(), move |interp, host| {
        match property(interp, host, node, name)? {
            Value::Integer(n) => Some(n),
            _ => None,
        }
    })
}
