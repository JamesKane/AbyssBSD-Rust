// SPDX-License-Identifier: BSD-2-Clause
//! The CIX Sky1 platform (ADR-0020): what its firmware says outside the
//! standard ACPI objects.
//!
//! - Its lookup tables (`CLKT`, `RSTL`, `RSNL`, `DLKL`, `RSVL`), present
//!   when a `CIXA1019` device is ([`tables`]).
//! - The `_DSD` reference properties its firmware uses that are not
//!   generic devicetree names: CIX's and Cadence's vendor bindings, and
//!   the unprefixed names its drivers' bindings use.
//!
//! - The ranges two devices' `_CRS` give on purpose: the SCMI shared
//!   memory inside its mailbox's window.
//!
//! No other corrections yet: they come with the drivers that need them.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use acpi_platform::{Extracted, FirmwareIdentity, PlatformSource};
use aml::{Host, Interpreter, NodeId};
use devtree::graph::EdgeKind;

pub mod tables;

/// The platform's name in diagnostics and edge origins.
pub const NAME: &str = "cix-sky1";

/// The Sky1's vendor `_DSD` reference properties, by the devicetree
/// meaning of the bindings firmware borrowed them from.
const REFERENCES: &[(&str, EdgeKind)] = &[
    // Register blocks leased from another device: CRUs, syscons, FCH
    // and audio subsystem control.
    ("uartclk", EdgeKind::Required),
    ("cru-ctrl", EdgeKind::Required),
    ("cdns,cru-ctrl", EdgeKind::Required),
    ("audss_cru", EdgeKind::Required),
    ("sky1,fch_cru", EdgeKind::Required),
    ("cix,hw", EdgeKind::Required),
    ("cix,usb_syscon", EdgeKind::Required),
    ("cix,usbphy_syscon", EdgeKind::Required),
    ("cix,gmac-ctrl", EdgeKind::Required),
    ("cix,dsp-ctrl", EdgeKind::Required),
    ("arm,remote-ctrl", EdgeKind::Required),
    // PHYs a controller drives.
    ("dp_phy", EdgeKind::Required),
    ("cdns,pcie-phy", EdgeKind::Required),
    ("cdnsp,usb3-phy", EdgeKind::Required),
    // The GPU's memory managers and TrustZone guard (Arm's kbase names).
    ("physical-memory-group-manager", EdgeKind::Required),
    ("protected-memory-allocator", EdgeKind::Required),
    ("tzgt", EdgeKind::Required),
    // Things a device works without, and is told of when they appear.
    ("edp-panel", EdgeKind::Optional),
    ("isp-src", EdgeKind::Optional),
    ("actuator-src", EdgeKind::Optional),
];

/// Devices whose memory lies inside another device's window by design,
/// by `_HID` (DRIVER-MODEL.md §8.1).
pub(crate) const SHARED: &[(&str, &str)] = &[
    (
        "CIXHA004",
        "SCMI tx shared memory: the AP-to-PM mailbox's 32 message registers (MBX6)",
    ),
    (
        "CIXHA005",
        "SCMI rx shared memory: the PM-to-AP mailbox's 32 message registers (MBX7)",
    ),
];

/// The platform.
#[derive(Debug)]
pub struct CixSky1;

impl PlatformSource for CixSky1 {
    fn name(&self) -> &'static str {
        NAME
    }

    /// A present `CIXA1019` says the lookup tables are there.
    fn applies(
        &self,
        _: &FirmwareIdentity,
        interp: &mut Interpreter,
        host: &mut dyn Host,
        devices: &[NodeId],
    ) -> bool {
        tables::find_hid(interp, host, devices, tables::TABLES_HID).is_some()
    }

    fn extract(
        &self,
        interp: &mut Interpreter,
        host: &mut dyn Host,
        devices: &[NodeId],
    ) -> Extracted {
        let mut x = tables::extract(interp, host, devices);
        for &(hid, why) in SHARED {
            if let Some(device) = tables::find_hid(interp, host, devices, hid) {
                x.description
                    .shared
                    .push(devtree::platform::SharedRange { device, why });
            }
        }
        // Two `CIXHA018` windows lie inside a reset controller's: the FCH
        // CRU (`CRU0`, unit 4) is the S5 reset controller's whole window,
        // and the USB-C PHY's (`UCRU`, unit 2) is `0x9C..0x11C` of the FCH
        // reset controller's. Their clients lease fields of them.
        for &d in devices {
            if interp.hid(d, host).ok().flatten().as_deref() != Some("CIXHA018") {
                continue;
            }
            let why = match interp.uid(d, host) {
                Ok(Some(aml::Uid::Integer(4))) => {
                    "FCH CRU: the same 4 KiB as the S5 reset controller (RST0); clients lease fields"
                }
                Ok(Some(aml::Uid::Integer(2))) => {
                    "USB-C PHY CRU: inside the FCH reset controller's window (RST1); clients lease fields"
                }
                _ => continue,
            };
            x.description
                .shared
                .push(devtree::platform::SharedRange { device: d, why });
        }
        x
    }

    fn register_windows(&self) -> &'static [&'static str] {
        &[
            "cix,usbphy_syscon",
            "cix,usb_syscon",
            "cix,gmac-ctrl",
            "cix,dsp-ctrl",
            "sky1,fch_cru",
            "audss_cru",
            "cru-ctrl",
            "cdns,cru-ctrl",
        ]
    }

    fn reference_kinds(&self) -> &'static [(&'static str, EdgeKind)] {
        REFERENCES
    }
}

/// The platform, for `kernel/table`'s list.
pub static PLATFORM: CixSky1 = CixSky1;

#[cfg(test)]
mod tests;
