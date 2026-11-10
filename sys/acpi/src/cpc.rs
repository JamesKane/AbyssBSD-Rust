// SPDX-License-Identifier: BSD-2-Clause
//! Collaborative processor performance control (`_CPC`, ACPI 6.5
//! §8.4.6.1): each processor's performance levels, from which the kernel
//! derives CPU capacities on asymmetric machines, and the registers
//! [`crate::cppc`] controls its frequency through. `_PSD` (§8.4.5.5)
//! groups processors into the domains whose frequency moves together.
//!
//! A level is an integer, or a register in system memory or I/O space.
//! Levels in functional fixed hardware or PCC (read through a mailbox)
//! are skipped for now.
//!
//! Behavior reference: sys/dev/acpica/acpi_cpu.c (processor objects), the
//! AbyssBSD Project's FreeBSD `acpi_cppc`, the ACPI specification for the
//! package layout.

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

/// Package elements: NumEntries, Revision, then the levels and registers.
const REVISION: usize = 1;
const HIGHEST: usize = 2;
const NOMINAL: usize = 3;
const LOWEST: usize = 5;
const DESIRED: usize = 7;
const REFERENCE_CTR: usize = 13;
const DELIVERED_CTR: usize = 14;
const REFERENCE_PERF: usize = 20;
const LOWEST_FREQ: usize = 21;
const NOMINAL_FREQ: usize = 22;
/// The elements of revision 3, the first with frequencies.
const ENTRIES_V3: usize = 23;

/// Address spaces (ACPI 6.5 table 5.33).
pub mod space {
    /// System memory.
    pub const MEMORY: u8 = 0;
    /// System I/O.
    pub const IO: u8 = 1;
    /// Functional fixed hardware.
    pub const FFH: u8 = 0x7f;
}

/// A `_CPC` register (a Generic Register descriptor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Register {
    /// The address space.
    pub space: u8,
    /// The address.
    pub address: u64,
    /// Its width in bits.
    pub width: u8,
    /// Its offset in bits.
    pub offset: u8,
}

impl Register {
    /// The register an element holds. A system memory register at
    /// address 0 is an absent one (ACPI 6.5 §8.4.6.1.1.1's NULL register).
    fn of(v: &Value) -> Option<Self> {
        let Value::Buffer(b) = v else { return None };
        let r = aml::resource::parse(b).ok()?;
        let AmlResource::GenericRegister(g) = r.first()? else {
            return None;
        };
        (g.space != space::MEMORY || g.address != 0).then_some(Self {
            space: g.space,
            address: g.address,
            width: g.bit_width,
            offset: g.bit_offset,
        })
    }

    /// The width of an access to a whole, aligned system memory register:
    /// 4 or 8 bytes, as the frequency controls need.
    #[must_use]
    pub fn memory_bytes(&self) -> Option<u8> {
        let bytes = match self.width {
            32 => 4,
            64 => 8,
            _ => return None,
        };
        (self.space == space::MEMORY
            && self.offset == 0
            && self.address.is_multiple_of(u64::from(bytes)))
        .then_some(bytes)
    }
}

/// What frequency control needs of a processor's `_CPC` (revision 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control {
    /// Highest, nominal, and lowest performance.
    pub highest: u64,
    pub nominal: u64,
    pub lowest: u64,
    /// The performance the reference counter counts at (the nominal one
    /// when firmware gives none).
    pub reference_perf: u64,
    /// The frequencies of the lowest and nominal performance, in MHz.
    pub lowest_mhz: u64,
    pub nominal_mhz: u64,
    /// The desired performance register.
    pub desired: Option<Register>,
    /// The reference and delivered performance counters.
    pub reference_ctr: Option<Register>,
    pub delivered_ctr: Option<Register>,
}

/// Why a `_CPC` cannot control frequency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlError {
    /// No `_CPC`, or not a package.
    Missing,
    /// Below revision 3 (no frequencies), or an element unreadable.
    Unsupported,
    /// Its bounds contradict each other.
    Inconsistent,
}

/// A processor's `_CPC`, read for frequency control. Levels held in
/// registers are read once, through `ops`.
///
/// # Errors
///
/// As [`ControlError`].
pub fn control(
    interp: &mut Interpreter,
    host: &mut dyn Host,
    ops: &dyn Ops,
    node: NodeId,
) -> Result<Control, ControlError> {
    let Ok(Some(Value::Package(p))) = interp.evaluate_child(node, "_CPC", Vec::new(), host) else {
        return Err(ControlError::Missing);
    };
    control_of(&p, ops)
}

fn control_of(p: &[Value], ops: &dyn Ops) -> Result<Control, ControlError> {
    let int = |i: usize| p.get(i).and_then(Value::as_integer);
    let at = |i: usize| p.get(i).and_then(|v| level(ops, v));
    let reg = |i: usize| p.get(i).and_then(Register::of);
    if p.len() < ENTRIES_V3 || int(REVISION).is_none_or(|r| r < 3) {
        return Err(ControlError::Unsupported);
    }
    let (Some(highest), Some(nominal), Some(lowest), Some(lowest_mhz), Some(nominal_mhz)) = (
        at(HIGHEST),
        at(NOMINAL),
        at(LOWEST),
        int(LOWEST_FREQ),
        int(NOMINAL_FREQ),
    ) else {
        return Err(ControlError::Unsupported);
    };
    if lowest == 0
        || nominal < lowest
        || highest < nominal
        || lowest_mhz == 0
        || nominal_mhz < lowest_mhz
    {
        return Err(ControlError::Inconsistent);
    }
    Ok(Control {
        highest,
        nominal,
        lowest,
        reference_perf: at(REFERENCE_PERF).filter(|&r| r != 0).unwrap_or(nominal),
        lowest_mhz,
        nominal_mhz,
        desired: reg(DESIRED),
        reference_ctr: reg(REFERENCE_CTR),
        delivered_ctr: reg(DELIVERED_CTR),
    })
}

/// A processor's `_PSD` domain number, if it has one.
pub fn domain(interp: &mut Interpreter, host: &mut dyn Host, node: NodeId) -> Option<u64> {
    let Ok(Some(Value::Package(p))) = interp.evaluate_child(node, "_PSD", Vec::new(), host) else {
        return None;
    };
    // One package: NumEntries, Revision, Domain, CoordType, NumProcessors.
    let Some(Value::Package(d)) = p.first() else {
        return None;
    };
    (d.len() >= 5).then(|| d[2].as_integer()).flatten()
}

/// Whether a processor's frequency is controlled through `_CPC`: it has
/// one, and no `_PSS` (which `acpi_perf` would use instead).
pub fn controls_frequency(interp: &Interpreter, node: NodeId) -> bool {
    interp.child(node, "_CPC").is_some() && interp.child(node, "_PSS").is_none()
}

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
pub fn processors(interp: &mut Interpreter, host: &mut dyn Host) -> Vec<(NodeId, u32)> {
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
