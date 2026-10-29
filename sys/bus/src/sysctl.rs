// SPDX-License-Identifier: BSD-2-Clause
//! The device tree in sysctl: `hw.bus.info` and `hw.bus.devices` for
//! `devinfo(8)`, `dev.<driver>.<unit>.%…` for each attached device, and
//! `kern.abyss.bus.*`.
//!
//! Behavior reference: sys/kern/subr_bus.c (`sysctl_bus_info`,
//! `sysctl_devices`, `device_sysctl_init`).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::bus::{BUS_USER_VERSION, UBusinfo};
use abi::sysctl::ctlflag;
use sched::Sleepable;
use sysctl::{NodeId, Number, ReadError, Tree, Value};

use crate::manager::Manager;

/// The strings a device's `dev.<driver>.<unit>` node shows.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeviceStrings {
    pub(crate) desc: String,
    pub(crate) driver: &'static str,
    pub(crate) location: String,
    pub(crate) pnpinfo: String,
    pub(crate) parent: String,
}

fn child_or_add(t: &mut Tree, parent: NodeId, name: &str, description: &str) -> Option<NodeId> {
    t.child(parent, name)
        .or_else(|| t.add_node(parent, Number::Auto, name, description).ok())
}

fn text(s: String) -> Value {
    Value::Str(Box::new(move |out| out.push_str(&s)))
}

/// Adds `hw.bus.info`, `hw.bus.devices`, and `kern.abyss.bus`. Values
/// are read under the tree's lock, so they must not take the topology
/// lock (ordered below it); only the handler does.
pub(crate) fn register(mgr: &'static Manager, s: &Sleepable, workers: usize) {
    mgr.platform().sysctl(s, &mut |t| {
        let root = t.root();
        if let Some(abyss) = t.child(root, "kern").and_then(|k| t.child(k, "abyss"))
            && let Some(bus) = child_or_add(t, abyss, "bus", "Device manager")
        {
            let w = i32::try_from(workers).unwrap_or(i32::MAX);
            let _ = t.add_value(
                bus,
                Number::Auto,
                "workers",
                "Attach worker threads",
                Value::int(w),
            );
            let _ = t.add_value(
                bus,
                Number::Auto,
                "generation",
                "Device tree generation",
                Value::Uint(Box::new(move || mgr.generation())),
            );
        }
        let Some(hw) = t.child(root, "hw") else {
            return;
        };
        let Some(bus) = child_or_add(t, hw, "bus", "") else {
            return;
        };
        let info = Value::Opaque {
            format: "S,u_businfo",
            read: Box::new(move |out| {
                let b = UBusinfo {
                    ub_version: BUS_USER_VERSION,
                    ub_generation: mgr.generation().cast_signed(),
                };
                out.extend_from_slice(&b.ub_version.to_ne_bytes());
                out.extend_from_slice(&b.ub_generation.to_ne_bytes());
            }),
        };
        let _ = t.add_value(bus, Number::Auto, "info", "bus-related data", info);
        let devices = Arc::new(move |rest: &[i32], out: &mut Vec<u8>| {
            let &[generation, index] = rest else {
                return Err(ReadError::Invalid);
            };
            if generation.cast_unsigned() != mgr.generation() {
                return Err(ReadError::Invalid);
            }
            let index = usize::try_from(index).map_err(|_| ReadError::NotFound)?;
            // SAFETY: sysctl values are read from thread context (the
            // syscall path), with no spin lock held: the kernel calls node
            // handlers after dropping the tree's lock.
            let s = unsafe { Sleepable::assume() };
            let d = mgr.udevice(&s, index).ok_or(ReadError::NotFound)?;
            devtree::udevice::to_bytes(&d, out);
            Ok(())
        });
        let _ = t.add_handler(bus, Number::Auto, "devices", "system device tree", devices);
        // kern_devctl.c's knobs (`CTLFLAG_RWTUN`: the kernel environment
        // sets them at boot, through `Options`).
        let _ = t.add_writable(
            bus,
            Number::Auto,
            "devctl_queue",
            "devctl queue length",
            ctlflag::TUN | ctlflag::MPSAFE,
            Value::Int(Box::new(move || {
                i32::try_from(mgr.devctl_queue()).unwrap_or(i32::MAX)
            })),
            sysctl::int_writer(move |q| mgr.set_devctl_queue(q)),
        );
        let _ = t.add_writable(
            bus,
            Number::Auto,
            "devctl_nomatch_enabled",
            "enable nomatch events",
            ctlflag::TUN,
            Value::U8(Box::new(move || u8::from(mgr.devctl_nomatch()))),
            sysctl::u8_writer(move |v| {
                mgr.set_devctl_nomatch(v != 0);
                Ok(())
            }),
        );
    });
}

/// Adds `dev.<driver>.<unit>` with its `%` nodes; returns the unit node.
pub(crate) fn add_device(
    mgr: &Manager,
    s: &Sleepable,
    unit: u32,
    strings: DeviceStrings,
) -> Option<NodeId> {
    let mut made = None;
    mgr.platform().sysctl(s, &mut |t| {
        let root = t.root();
        let Some(dev) = child_or_add(t, root, "dev", "") else {
            return;
        };
        let class = if let Some(c) = t.child(dev, strings.driver) {
            c
        } else {
            let Ok(c) = t.add_node(dev, Number::Auto, strings.driver, "") else {
                return;
            };
            let _ = t.add_value(c, Number::Auto, "%parent", "parent class", Value::text(""));
            c
        };
        let Ok(node) = t.add_node(
            class,
            Number::Fixed(unit.cast_signed()),
            &alloc::format!("{unit}"),
            "",
        ) else {
            return;
        };
        let d = strings.clone();
        for (name, descr, v) in [
            ("%desc", "device description", d.desc),
            ("%driver", "device driver name", String::from(d.driver)),
            (
                "%location",
                "device location relative to parent",
                d.location,
            ),
            ("%pnpinfo", "device identification", d.pnpinfo),
            ("%parent", "parent device", d.parent),
            (
                "%iommu",
                "iommu unit handling the device requests",
                String::new(),
            ),
        ] {
            let _ = t.add_value(node, Number::Auto, name, descr, text(v));
        }
        made = Some(node);
    });
    made
}

/// Removes a device's node.
pub(crate) fn remove_device(mgr: &Manager, s: &Sleepable, node: NodeId) {
    mgr.platform().sysctl(s, &mut |t| {
        t.remove(node);
    });
}
