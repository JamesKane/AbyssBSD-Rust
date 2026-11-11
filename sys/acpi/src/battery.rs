// SPDX-License-Identifier: BSD-2-Clause
//! `hw.acpi.battery`: the batteries' combined state, as `acpi_battery.c`
//! reports it. The battery drivers (`acpi_cmbat`, `acpi_smbat`) come
//! later (milestone M-D), so there are no batteries yet: `units` is 0 and
//! the combined state stays as `acpi_battery_get_battinfo` leaves it when
//! it finds none (zero), which tools read as "no batteries".
//!
//! FreeBSD makes the tree when the first battery registers; AbyssBSD makes
//! it when ACPI attaches (docs/compat-deviations.md), so programs that ask
//! for `hw.acpi.battery.units` get their answer, 0, instead of `ENOENT`.
//!
//! Behavior reference: sys/dev/acpica/acpi_battery.c.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicI32, Ordering};

use sysctl::{Number, Tree, Value};

/// `ACPI_BATTERY_INFO_EXPIRE`: seconds a battery's information is kept.
const INFO_EXPIRE: i32 = 5;

/// `hw.acpi.battery.info_expire`.
static EXPIRE: AtomicI32 = AtomicI32::new(INFO_EXPIRE);

/// The batteries attached (`devclass_get_count("battery")`).
#[must_use]
pub fn units() -> i32 {
    0
}

/// Makes `hw.acpi.battery` and its leaves, in `acpi_battery_init`'s order
/// (making `hw.acpi` if nothing has). Once.
pub fn add_sysctl(t: &mut Tree) {
    let root = t.root();
    let Some(hw) = t.child(root, "hw") else {
        return;
    };
    let acpi = match t.child(hw, "acpi") {
        Some(a) => a,
        None => match t.add_node(hw, Number::Auto, "acpi", "") {
            Ok(a) => a,
            Err(_) => return,
        },
    };
    if t.child(acpi, "battery").is_some() {
        return;
    }
    let Ok(battery) = t.add_node(acpi, Number::Auto, "battery", "battery status and info") else {
        return;
    };
    // `acpi_battery_battinfo`, which `acpi_battery_get_battinfo` leaves
    // as it was (zero) when there is no battery.
    for (name, descr) in [
        ("life", "percent capacity remaining"),
        ("time", "remaining time in minutes"),
        ("rate", "present rate in mW"),
        ("state", "current status flags"),
    ] {
        let _ = t.add_value(battery, Number::Auto, name, descr, Value::int(0));
    }
    let _ = t.add_value(
        battery,
        Number::Auto,
        "units",
        "number of batteries",
        Value::Int(Box::new(units)),
    );
    let _ = t.add_writable(
        battery,
        Number::Auto,
        "info_expire",
        "time in seconds until info is refreshed",
        0,
        Value::Int(Box::new(|| EXPIRE.load(Ordering::Relaxed))),
        sysctl::int_writer(|x| {
            EXPIRE.store(x, Ordering::Relaxed);
            Ok(())
        }),
    );
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn no_batteries_reads_as_freebsd_without_one() {
        let mut t = Tree::new();
        add_sysctl(&mut t);
        add_sysctl(&mut t);
        let int = |t: &Tree, n: &str| {
            let oid = t.name2oid(n).unwrap();
            let mut out = alloc::vec::Vec::new();
            t.read(&oid, &mut out).unwrap();
            i32::from_ne_bytes(out[..4].try_into().unwrap())
        };
        assert_eq!(int(&t, "hw.acpi.battery.units"), 0);
        assert_eq!(int(&t, "hw.acpi.battery.state"), 0);
        assert_eq!(int(&t, "hw.acpi.battery.info_expire"), 5);
    }
}
