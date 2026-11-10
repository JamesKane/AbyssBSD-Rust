// SPDX-License-Identifier: BSD-2-Clause
//! CIX's lookup tables (DRIVER-MODEL.md §5.5, ADR-0020): the Sky1 describes its
//! clocks, resets, resource names, device links, and reserved memory in
//! named AML objects outside the standard ones, and a `CIXA1019` device
//! says they are there. Any present device may carry them, and an entry
//! names its consumer, which need not be the device carrying it (the
//! Sky1's `DMA1` lists `DMA0`'s clock).
//!
//! - `CLKT`: `{clock ID, name, consumer}`; the clock is the SCMI clock
//!   `CIXHA010` serves through its AML methods. An empty name is the
//!   device's unnamed clock; a missing consumer is the carrier.
//! - `RSTL`: `{controller, index, consumer, name}`; the name is optional.
//! - `RSNL`: `{device, type, index, name}`, naming the `index`th `_CRS`
//!   resource of a type: 0x200 a memory range, 0x400 an interrupt (the
//!   Sky1's values; they are Linux's `IORESOURCE_MEM` and `IORESOURCE_IRQ`).
//! - `DLKL`: `{supplier, consumer, flags}`: the consumer may not attach
//!   before the supplier. The flags are 0 on the Sky1 and are ignored.
//! - `RSVL`: `{base, size, flags, device}`; flags `"no-map"`.
//!
//! References are names or path strings, resolved from the carrier.
//! Source: the clean-room specification `sky1-soc` 01 §1.4 and 02 §2.2–2.3,
//! held to the Sky1's tables in `tests/corpus/acpi/opi6p`.

use alloc::string::String;
use alloc::vec::Vec;

use acpi_platform::{Extracted, Malformed, Unresolved};
use aml::{Interpreter, NodeId, Value};
use devtree::platform::{
    ClockLine, DeviceLink, ReservedMemory, ResetLine, ResourceKind, ResourceName,
};

use crate::NAME;

/// The device whose presence says the tables are there.
pub const TABLES_HID: &str = "CIXA1019";
/// The clock provider `CLKT` entries refer to.
pub const CLOCKS_HID: &str = "CIXHA010";

/// A CIX table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Table {
    /// Clock lookups.
    Clkt,
    /// Reset lookups.
    Rstl,
    /// Resource names.
    Rsnl,
    /// Device links.
    Dlkl,
    /// Reserved memory.
    Rsvl,
}

impl Table {
    const ALL: [Self; 5] = [Self::Clkt, Self::Rstl, Self::Rsnl, Self::Dlkl, Self::Rsvl];

    /// The object's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Clkt => "CLKT",
            Self::Rstl => "RSTL",
            Self::Rsnl => "RSNL",
            Self::Dlkl => "DLKL",
            Self::Rsvl => "RSVL",
        }
    }
}

/// The first of `devices` whose `_HID` is `hid`.
pub(crate) fn find_hid(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    devices: &[NodeId],
    hid: &str,
) -> Option<NodeId> {
    devices
        .iter()
        .copied()
        .find(|&d| interp.hid(d, host).ok().flatten().as_deref() == Some(hid))
}

/// Reads the CIX tables of `devices` (the present ones).
pub(crate) fn extract(
    interp: &mut Interpreter,
    host: &mut dyn aml::Host,
    devices: &[NodeId],
) -> Extracted {
    let clock_provider = find_hid(interp, host, devices, CLOCKS_HID);
    let mut out = Extracted::default();
    for &d in devices {
        for t in Table::ALL {
            match interp.evaluate_child(d, t.name(), Vec::new(), host) {
                Ok(None) => {}
                Ok(Some(Value::Package(entries))) => {
                    for (k, e) in entries.iter().enumerate() {
                        let mut cx = Entry {
                            interp,
                            carrier: d,
                            table: t,
                            clock_provider,
                            out: &mut out,
                        };
                        if let Err(why) = cx.read(e) {
                            out.malformed.push(Malformed {
                                source: NAME,
                                table: t.name(),
                                carrier: d,
                                entry: Some(k),
                                why,
                            });
                        }
                    }
                }
                Ok(Some(_)) => out.malformed.push(Malformed {
                    source: NAME,
                    table: t.name(),
                    carrier: d,
                    entry: None,
                    why: String::from("not a package"),
                }),
                Err(e) => out.malformed.push(Malformed {
                    source: NAME,
                    table: t.name(),
                    carrier: d,
                    entry: None,
                    why: alloc::format!("evaluation failed: {e:?}"),
                }),
            }
        }
    }
    out
}

/// One table entry being read.
struct Entry<'a> {
    interp: &'a Interpreter,
    carrier: NodeId,
    table: Table,
    /// The clock provider (`CIXHA010`) the clock lines name.
    clock_provider: Option<NodeId>,
    out: &'a mut Extracted,
}

/// Why an entry was not read: a malformed field, or a reference to
/// nothing (already recorded).
type Read<T> = Result<Option<T>, String>;

impl Entry<'_> {
    fn read(&mut self, e: &Value) -> Result<(), String> {
        let Value::Package(f) = e else {
            return Err(String::from("not a package"));
        };
        match self.table {
            Table::Clkt => {
                let [id, name, rest @ ..] = f.as_slice() else {
                    return Err(arity(f.len()));
                };
                let id = u32_of(id, "clock ID")?;
                let name = string_of(name, "name")?;
                let consumer = match rest {
                    [] => Some(self.carrier),
                    [c] => self.node(c, "consumer")?,
                    _ => return Err(arity(f.len())),
                };
                let (Some(consumer), Some(provider)) = (consumer, self.clock_provider) else {
                    if self.clock_provider.is_none() {
                        self.unresolved(String::from(CLOCKS_HID));
                    }
                    return Ok(());
                };
                self.out.description.clocks.push(ClockLine {
                    consumer,
                    name: (!name.is_empty()).then_some(name),
                    provider,
                    id,
                });
            }
            Table::Rstl => {
                let (controller, index, consumer, name) = match f.as_slice() {
                    [c, i, d] => (c, i, d, None),
                    [c, i, d, n] => (c, i, d, Some(string_of(n, "name")?)),
                    _ => return Err(arity(f.len())),
                };
                let index = u32_of(index, "index")?;
                if let (Some(controller), Some(consumer)) = (
                    self.node(controller, "controller")?,
                    self.node(consumer, "consumer")?,
                ) {
                    self.out.description.resets.push(ResetLine {
                        consumer,
                        name: name.filter(|n| !n.is_empty()),
                        controller,
                        index,
                    });
                }
            }
            Table::Rsnl => {
                let [device, kind, index, name] = f.as_slice() else {
                    return Err(arity(f.len()));
                };
                let kind = match u32_of(kind, "type")? {
                    0x200 => ResourceKind::Memory,
                    0x400 => ResourceKind::Interrupt,
                    other => return Err(alloc::format!("resource type {other:#x}")),
                };
                let index = u32_of(index, "index")?;
                let name = string_of(name, "name")?;
                if let Some(device) = self.node(device, "device")? {
                    self.out.description.resource_names.push(ResourceName {
                        device,
                        kind,
                        index,
                        name,
                    });
                }
            }
            Table::Dlkl => {
                let [supplier, consumer, flags] = f.as_slice() else {
                    return Err(arity(f.len()));
                };
                u32_of(flags, "flags")?;
                if let (Some(supplier), Some(consumer)) = (
                    self.node(supplier, "supplier")?,
                    self.node(consumer, "consumer")?,
                ) {
                    self.out
                        .description
                        .links
                        .push(DeviceLink { supplier, consumer });
                }
            }
            Table::Rsvl => {
                let [base, size, flags, device] = f.as_slice() else {
                    return Err(arity(f.len()));
                };
                let (Value::Integer(base), Value::Integer(size)) = (base, size) else {
                    return Err(String::from("base or size not an integer"));
                };
                let no_map = match string_of(flags, "flags")?.as_str() {
                    "no-map" => true,
                    "" => false,
                    other => return Err(alloc::format!("flags {other:?}")),
                };
                if *size == 0 || base.checked_add(*size).is_none() {
                    return Err(alloc::format!("range {base:#x}+{size:#x}"));
                }
                if let Some(device) = self.node(device, "device")? {
                    self.out.description.reserved.push(ReservedMemory {
                        device,
                        base: *base,
                        size: *size,
                        no_map,
                    });
                }
            }
        }
        Ok(())
    }

    /// The device a reference or path string names, from the carrier.
    fn node(&mut self, v: &Value, what: &str) -> Read<NodeId> {
        let n = match v {
            Value::String(path) => match self.interp.resolve(self.carrier, path) {
                Some(n) => n,
                None => {
                    self.unresolved(path.clone());
                    return Ok(None);
                }
            },
            Value::Name(_, name) => match self.interp.resolve_value_name(v) {
                Some(n) => n,
                None => {
                    self.unresolved(alloc::format!("{name}"));
                    return Ok(None);
                }
            },
            Value::Reference(_) => self
                .interp
                .resolve_value_name(v)
                .ok_or_else(|| alloc::format!("{what}: a reference to no object"))?,
            _ => return Err(alloc::format!("{what} not a reference")),
        };
        Ok(Some(n))
    }

    fn unresolved(&mut self, name: String) {
        let u = Unresolved {
            carrier: self.carrier,
            table: self.table.name(),
            name,
        };
        if !self.out.unresolved.contains(&u) {
            self.out.unresolved.push(u);
        }
    }
}

fn arity(n: usize) -> String {
    alloc::format!("{n} elements")
}

fn u32_of(v: &Value, what: &str) -> Result<u32, String> {
    match v {
        Value::Integer(n) => u32::try_from(*n).map_err(|_| alloc::format!("{what} {n:#x}")),
        _ => Err(alloc::format!("{what} not an integer")),
    }
}

fn string_of(v: &Value, what: &str) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        _ => Err(alloc::format!("{what} not a string")),
    }
}
