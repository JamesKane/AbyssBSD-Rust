// SPDX-License-Identifier: BSD-2-Clause
//! Device power: a device's `_PR0` power resources are on and its `_PS0`
//! has run (D0) before its driver attaches, as FreeBSD's
//! `acpi_pwr_switch_consumer` brings a device to D0.
//!
//! Power resources are shared: one is switched on (`_ON`, unless its
//! `_STA` says it already is) for its first consumer and counted after
//! that. Nothing is switched off yet: no device leaves D0 before suspend
//! exists.
//!
//! Behavior reference: sys/dev/acpica/acpi_powerres.c.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use aml::{Host, Interpreter, NodeId, ObjectType, Value};
use sync::SpinLock;

/// Consumers of each power resource that is on. Only the AML thread
/// touches it.
static REFS: SpinLock<BTreeMap<NodeId, u32>> = SpinLock::new(BTreeMap::new());

/// What [`to_d0`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoweredUp {
    /// Power resources it switched on.
    pub switched_on: u32,
    /// Whether `_PS0` ran.
    pub ps0: bool,
}

/// Brings `node` to D0: its `_PR0` power resources on, then `_PS0`.
pub fn to_d0(interp: &mut Interpreter, node: NodeId, host: &mut dyn Host) -> PoweredUp {
    let mut out = PoweredUp::default();
    let resources: Vec<NodeId> = match interp.evaluate_child(node, "_PR0", Vec::new(), host) {
        Ok(Some(Value::Package(refs))) => refs
            .iter()
            .filter_map(|r| interp.resolve_value_name(r))
            .filter(|&r| interp.object_type(r) == Some(ObjectType::PowerResource))
            .collect(),
        _ => Vec::new(),
    };
    for r in resources {
        let first = {
            let mut refs = REFS.lock();
            let n = refs.entry(r).or_insert(0);
            *n += 1;
            *n == 1
        };
        if !first {
            continue;
        }
        let on = matches!(
            interp.evaluate_child(r, "_STA", Vec::new(), host),
            Ok(Some(Value::Integer(s))) if s & 1 != 0
        );
        if !on && interp.evaluate_child(r, "_ON", Vec::new(), host).is_ok() {
            out.switched_on += 1;
        }
    }
    if interp.child(node, "_PS0").is_some() {
        out.ps0 = interp
            .evaluate_child(node, "_PS0", Vec::new(), host)
            .is_ok();
    }
    out
}

/// Consumers counted for power resource `r` (tests and diagnostics).
#[must_use]
pub fn consumers(r: NodeId) -> u32 {
    REFS.lock().get(&r).copied().unwrap_or(0)
}
