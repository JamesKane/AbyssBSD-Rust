// SPDX-License-Identifier: BSD-2-Clause
//! The devices firmware puts on an I2C controller, as FreeBSD's
//! `acpi_iicbus` finds them for the `iicbus` under that controller.
//!
//! The whole namespace is walked, in order, and a device is on the bus
//! when it is present (`_STA`, or no `_STA`), has a `_HID` or `_CID`, and
//! the first `I2cSerialBus` descriptor of its `_CRS` has a nonzero
//! address and names the controller (resolved from the root, as
//! `AcpiGetHandle` does; a name that does not resolve leaves the device
//! out). The bus runs at the slowest of their connection speeds, or
//! FreeBSD's 100 kHz when there are none.
//!
//! The `iicbus` driver moves `acpi0`'s child for each such device under
//! itself (`StartCx::take_child`), as `acpi_iicbus` deletes `acpi0`'s and
//! adds its own: the device keeps its [`crate::AcpiDevice`] bus data, its
//! lines (`cx.i2c(0)`, its `GpioInt`s), and its dependencies, so a driver
//! for it runs AML and reaches its bus as any ACPI device does.
//!
//! `acpi0` already put every device in D0 before any driver saw it, which
//! is `acpi_iicbus_set_power_children(D0)`'s work.
//!
//! Behavior reference: sys/dev/iicbus/acpi_iicbus.c
//! (`acpi_iicbus_enumerate_child`, `acpi_iicbus_get_i2cres`).

use alloc::string::String;
use alloc::vec::Vec;

use aml::resource::{Resource, SerialBusKind};

/// One device on a controller's bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I2cChild {
    /// Its namespace node.
    pub node: aml::NodeId,
    /// Its path (`\_SB_.I2C3.RTC0`).
    pub path: String,
    /// The address its first `I2cSerialBus` gives.
    pub address: u16,
    /// Its connection speed in Hz.
    pub speed: u32,
    /// 10-bit addressing.
    pub ten_bit: bool,
}

/// FreeBSD's bus clock when no device names one (`iicbus_init_frequency`).
pub const DEFAULT_FREQUENCY: u32 = 100_000;

/// The devices on `controller`'s bus, in namespace order.
pub fn children(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    controller: aml::NodeId,
) -> Vec<I2cChild> {
    let mut out = Vec::new();
    for node in interp.devices() {
        // A failed `_STA` counts as present, as a missing one does.
        if interp.sta(node, host).is_ok_and(|sta| sta & 1 == 0) {
            continue;
        }
        let has_id = interp.hid(node, host).ok().flatten().is_some()
            || !interp.cids(node, host).unwrap_or_default().is_empty();
        if !has_id {
            continue;
        }
        let Some(crs) = interp.crs(node, host).ok().flatten() else {
            continue;
        };
        let Some((name, address, speed, ten_bit)) = crs.iter().find_map(|r| match r {
            Resource::SerialBus(b) => match b.bus {
                SerialBusKind::I2c {
                    ten_bit,
                    speed,
                    address,
                } => Some((&b.source.name, address, speed, ten_bit)),
                _ => None,
            },
            _ => None,
        }) else {
            continue;
        };
        if address == 0 {
            continue;
        }
        if interp.resolve(interp.root(), name) != Some(controller) {
            continue;
        }
        out.push(I2cChild {
            node,
            path: alloc::format!("{}", interp.path(node)),
            address,
            speed,
            ten_bit,
        });
    }
    out
}

/// The bus clock `acpi_iicbus` sets: the slowest device's speed, or
/// [`DEFAULT_FREQUENCY`] (a speed of 0 counts, as FreeBSD's
/// `bus_freq == 0` test then takes the next one).
#[must_use]
pub fn bus_frequency(children: &[I2cChild]) -> u32 {
    let mut f = 0;
    for c in children {
        if f == 0 || f > c.speed {
            f = c.speed;
        }
    }
    if f == 0 { DEFAULT_FREQUENCY } else { f }
}
