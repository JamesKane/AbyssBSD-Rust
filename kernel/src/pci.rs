// SPDX-License-Identifier: BSD-2-Clause
//! PCI configuration space: the ECAM windows MCFG describes, mapped once
//! and kept from drivers (DRIVER-MODEL.md §8.1). The PCI bus driver
//! (P3.6) enumerates through them.

use alloc::vec::Vec;

use addr::PhysAddr;
use pci::{ConfigSpace, Ecam, PciAddress};
use sync::Once;

static WINDOWS: Once<Vec<Ecam>> = Once::new();

/// Maps every ECAM window in MCFG. Call once, before the device manager.
pub(crate) fn init() {
    let mut windows = Vec::new();
    let regions = crate::acpi::find(*b"MCFG").and_then(|t| acpi_tables::mcfg::parse(&t).ok());
    for r in regions.iter().flat_map(|m| m.regions()) {
        let Ok(r) = r else {
            kprintln!("pci: ignoring a malformed MCFG entry");
            continue;
        };
        // The table's base is bus 0's; the window starts at `start_bus`.
        let Some(pa) = r.function_address(r.start_bus, 0, 0) else {
            continue;
        };
        let len = u64::from(r.bus_count()) << 20;
        match crate::devices::map_reserved(PhysAddr::new(pa), len) {
            Ok(va) => {
                kprintln!(
                    "pci: segment {} buses {}-{} ECAM at {pa:#x}",
                    r.segment,
                    r.start_bus,
                    r.end_bus
                );
                // SAFETY: just mapped as device memory for the window's
                // buses, never unmapped, and reserved from drivers.
                windows.push(unsafe {
                    Ecam::new(
                        usize::try_from(va.get()).unwrap_or(0),
                        u32::from(r.segment),
                        r.start_bus,
                        r.end_bus,
                    )
                });
            }
            Err(e) => kprintln!("pci: cannot map ECAM at {pa:#x}: {e:?}"),
        }
    }
    let _ = WINDOWS.set(windows);
}

/// The ECAM windows.
pub(crate) fn windows() -> &'static [Ecam] {
    WINDOWS.get().map_or(&[], Vec::as_slice)
}

/// The window serving `a`.
pub(crate) fn config_for(a: PciAddress) -> Option<&'static (dyn ConfigSpace + Sync)> {
    windows()
        .iter()
        .find(|w| w.segment() == a.domain && w.buses().contains(&a.bus))
        .map(|w| w as &(dyn ConfigSpace + Sync))
}
