// SPDX-License-Identifier: BSD-2-Clause
//! The `_DSD` reference properties the dependency graph knows
//! (DRIVER-MODEL.md §6.1): for a property whose value names another
//! namespace object, the kind of edge from that object to the device.
//!
//! Firmware borrows devicetree's names, so the table follows their
//! devicetree meaning. It holds only generic names (devicetree's core
//! bindings, standard ACPI `_DSD` properties); a vendor's bindings
//! (`cix,usb_syscon`) belong to its platform crate (ADR-0020). A name no
//! table knows is reported, not guessed at: the board corpus tests fail
//! on one, so each new name is classified once.

use crate::graph::EdgeKind;

/// Properties by exact name.
const NAMES: &[(&str, EdgeKind)] = &[
    // Power domains and supplies keep their supplier powered.
    ("power-domains", EdgeKind::Power),
    // Firmware channels and the memory they share.
    ("mboxes", EdgeKind::Required),
    ("shmem", EdgeKind::Required),
    // Clocks and PWMs a device consumes.
    ("clocks", EdgeKind::Required),
    ("pwms", EdgeKind::Required),
    // PHYs a controller drives.
    ("phys", EdgeKind::Required),
    // A MAC finds its PHY on its own MDIO bus after attaching, as
    // FreeBSD's miibus does.
    ("phy-handle", EdgeKind::Optional),
    // A panel's backlight: it works without one, and is told when it
    // appears.
    ("backlight", EdgeKind::Optional),
    // Ordering only: a media pipeline's links, a USB4 port's host
    // (Microsoft's USB4 `_DSD` property).
    ("remote-endpoint", EdgeKind::Order),
    ("usb4-host-interface", EdgeKind::Order),
];

/// The edge a reference in property `name` makes, or `None` for a name
/// the table does not know.
#[must_use]
pub fn reference_kind(name: &str) -> Option<EdgeKind> {
    if let Some(&(_, kind)) = NAMES.iter().find(|(n, _)| *n == name) {
        return Some(kind);
    }
    // devicetree's GPIO and supply bindings: `reset-gpios`, `gpio`,
    // `vcc-pcie-supply`.
    if name == "gpio" || name == "gpios" || name.ends_with("-gpio") || name.ends_with("-gpios") {
        return Some(EdgeKind::Required);
    }
    if name.ends_with("-supply") {
        return Some(EdgeKind::Power);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(reference_kind("power-domains"), Some(EdgeKind::Power));
        assert_eq!(reference_kind("vcc-pcie-supply"), Some(EdgeKind::Power));
        assert_eq!(reference_kind("power-supply"), Some(EdgeKind::Power));
        assert_eq!(reference_kind("reset-gpios"), Some(EdgeKind::Required));
        assert_eq!(reference_kind("oc-gpio"), Some(EdgeKind::Required));
        assert_eq!(reference_kind("gpio"), Some(EdgeKind::Required));
        assert_eq!(reference_kind("remote-endpoint"), Some(EdgeKind::Order));
        assert_eq!(reference_kind("phy-handle"), Some(EdgeKind::Optional));
        assert_eq!(reference_kind("compatible"), None);
        assert_eq!(reference_kind("supply"), None);
    }

    #[test]
    fn names_are_unique() {
        for (i, (a, _)) in NAMES.iter().enumerate() {
            assert!(NAMES[i + 1..].iter().all(|(b, _)| a != b), "{a}");
        }
    }
}
