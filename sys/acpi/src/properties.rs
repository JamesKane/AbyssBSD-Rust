// SPDX-License-Identifier: BSD-2-Clause
//! A device's own `_DSD` device properties (daffd814-…), for its driver:
//! FreeBSD's `acpi_dsd_property` / `device_get_property` for ACPI.

use aml::Value;

use crate::bus::AcpiDevice;
use crate::service::service;

/// The value of `node`'s `_DSD` device property `name`, if it has one.
pub fn property(
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
