// SPDX-License-Identifier: BSD-2-Clause
//! Unit numbers within each devclass (`nvme0`, `nvme1`, …).
//!
//! A device takes the lowest free unit of its devclass when its driver is
//! chosen, before attach, so names never depend on attach timing
//! (DRIVER-MODEL.md §4.3). Units follow FreeBSD's rules:
//!
//! - A failed attach gives the unit back (the device leaves the devclass),
//!   unless `hw.bus.disable_failed_devices` keeps it, or the devclass was
//!   fixed when the device was created.
//! - A device disabled by a `hint.<driver>.<unit>.disabled` hint keeps its
//!   unit, so later devices are numbered as if it had attached.
//! - Wiring hints (`at`) are not supported, so no unit is skipped for one.
//!
//! Behavior reference: sys/kern/subr_bus.c (`devclass_alloc_unit`,
//! `device_attach`).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;

/// A wired unit is already taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitTaken;

/// The units in use, per devclass.
#[derive(Clone, Debug, Default)]
pub struct Units {
    classes: BTreeMap<String, BTreeSet<u32>>,
}

impl Units {
    /// No units in use.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            classes: BTreeMap::new(),
        }
    }

    /// Takes `wired`, or the lowest free unit of `class`.
    pub fn alloc(&mut self, class: &str, wired: Option<u32>) -> Result<u32, UnitTaken> {
        let used = self.classes.entry(String::from(class)).or_default();
        let unit = match wired {
            Some(u) if used.contains(&u) => return Err(UnitTaken),
            Some(u) => u,
            None => (0..).find(|u| !used.contains(u)).expect("a free unit"),
        };
        used.insert(unit);
        Ok(unit)
    }

    /// The lowest free unit of `class` not in `skip` (units hints wire to
    /// other devices).
    pub fn alloc_skipping(&mut self, class: &str, skip: &[u32]) -> u32 {
        let used = self.classes.entry(String::from(class)).or_default();
        let unit = (0..)
            .find(|u| !used.contains(u) && !skip.contains(u))
            .expect("a free unit");
        used.insert(unit);
        unit
    }

    /// Gives a unit back.
    pub fn release(&mut self, class: &str, unit: u32) {
        if let Some(used) = self.classes.get_mut(class) {
            used.remove(&unit);
        }
    }

    /// Whether `unit` of `class` is in use.
    #[must_use]
    pub fn in_use(&self, class: &str, unit: u32) -> bool {
        self.classes.get(class).is_some_and(|u| u.contains(&unit))
    }

    /// The highest unit in use plus one (`devclass_get_maxunit`).
    #[must_use]
    pub fn max_unit(&self, class: &str) -> u32 {
        self.classes
            .get(class)
            .and_then(|u| u.last())
            .map_or(0, |u| u + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowest_free_and_reuse() {
        let mut u = Units::new();
        assert_eq!(u.alloc("nvme", None), Ok(0));
        assert_eq!(u.alloc("nvme", None), Ok(1));
        assert_eq!(u.alloc("ahci", None), Ok(0), "per devclass");
        // A failed attach gives its unit back; the next device takes it.
        u.release("nvme", 0);
        assert_eq!(u.alloc("nvme", None), Ok(0));
        assert_eq!(u.alloc("nvme", Some(1)), Err(UnitTaken));
        assert_eq!(u.alloc("nvme", Some(7)), Ok(7));
        assert_eq!(u.alloc("nvme", None), Ok(2));
        assert_eq!(u.max_unit("nvme"), 8);
        assert!(u.in_use("nvme", 7));
        assert_eq!(u.max_unit("vtblk"), 0);
    }

    #[test]
    fn hinted_units_are_kept_for_their_devices() {
        // COM2 found first takes its hinted unit; an unhinted port skips
        // both hinted ones; COM1 then takes 0.
        let mut u = Units::new();
        assert_eq!(u.alloc("uart", Some(1)), Ok(1));
        assert_eq!(u.alloc_skipping("uart", &[0, 1]), 2);
        assert_eq!(u.alloc("uart", Some(0)), Ok(0));
        assert_eq!(u.alloc_skipping("uart", &[0, 1]), 3);
    }
}
