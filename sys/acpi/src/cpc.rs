// SPDX-License-Identifier: BSD-2-Clause
//! Collaborative processor performance control (`_CPC`, ACPI 6.5
//! §8.4.6.1): each processor's performance levels, from which the kernel
//! derives CPU capacities on asymmetric machines.
//!
//! Only the levels are read: an integer, or a register in system memory
//! or I/O space. Levels in functional fixed hardware or PCC (read through
//! a mailbox) are skipped for now.
//!
//! Behavior reference: sys/dev/acpica/acpi_cpu.c (processor objects), the
//! ACPI specification for the package layout.

use alloc::vec::Vec;

use aml::resource::Resource as AmlResource;
use aml::{Host, Interpreter, NodeId, ObjectType, Value};

use crate::Ops;

/// A processor's performance levels, in the platform's abstract units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cppc {
    /// The processor's ACPI UID (the MADT's processor UID).
    pub uid: u32,
    /// Highest performance.
    pub highest: u64,
    /// Nominal (sustained) performance.
    pub nominal: Option<u64>,
    /// Lowest performance.
    pub lowest: Option<u64>,
}

/// Package elements (after NumEntries and Revision).
const HIGHEST: usize = 2;
const NOMINAL: usize = 3;
const LOWEST: usize = 5;

/// One `_CPC` element's value.
fn level(ops: &dyn Ops, v: &Value) -> Option<u64> {
    match v {
        Value::Integer(n) => Some(*n),
        Value::Buffer(b) => {
            let r = aml::resource::parse(b).ok()?;
            let AmlResource::GenericRegister(g) = r.first()? else {
                return None;
            };
            let bytes = match g.bit_width {
                8 => 1,
                16 => 2,
                32 => 4,
                64 => 8,
                _ => return None,
            };
            let raw = match g.space {
                0 => ops.mem_read(g.address, bytes)?,
                1 => u64::from(ops.io_read(u16::try_from(g.address).ok()?, bytes.min(4))?),
                _ => return None,
            };
            Some(raw >> g.bit_offset)
        }
        _ => None,
    }
}

/// The processors: `ACPI0007` devices by `_UID`, and legacy `Processor`
/// objects by their ID.
fn processors(interp: &mut Interpreter, host: &mut dyn Host) -> Vec<(NodeId, u32)> {
    let mut out = Vec::new();
    for n in interp.nodes() {
        match interp.object_type(n) {
            Some(ObjectType::Processor) => {
                if let Some(p) = interp.processor_info(n) {
                    out.push((n, u32::from(p.id)));
                }
            }
            Some(ObjectType::Device)
                if interp.hid(n, host).ok().flatten().as_deref() == Some("ACPI0007") =>
            {
                if let Ok(Some(aml::Uid::Integer(u))) = interp.uid(n, host)
                    && let Ok(u) = u32::try_from(u)
                {
                    out.push((n, u));
                }
            }
            _ => {}
        }
    }
    out
}

/// Every processor's `_CPC` levels, where it has them.
pub fn levels(interp: &mut Interpreter, host: &mut dyn Host, ops: &dyn Ops) -> Vec<Cppc> {
    let mut out = Vec::new();
    for (node, uid) in processors(interp, host) {
        let Ok(Some(Value::Package(p))) = interp.evaluate_child(node, "_CPC", Vec::new(), host)
        else {
            continue;
        };
        let at = |i: usize| p.get(i).and_then(|v| level(ops, v));
        if let Some(highest) = at(HIGHEST).filter(|&h| h > 0) {
            out.push(Cppc {
                uid,
                highest,
                nominal: at(NOMINAL),
                lowest: at(LOWEST),
            });
        }
    }
    out
}
