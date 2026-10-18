// SPDX-License-Identifier: BSD-2-Clause
//! Device queries for the ACPI bus (ACPI 6.5 §6.1–6.2, §6.4, §6.5):
//! enumerating devices and evaluating their identification, status,
//! resources, PCI interrupt routing, and dependencies into typed values.
//!
//! Behavior reference: `sys/dev/acpica/acpi.c` (`_STA` default,
//! `_HID`/`_CID` handling, `_DEP`), `sys/dev/acpica/acpi_pcib.c` (`_PRT`).

use alloc::string::String;
use alloc::vec::Vec;

use crate::Error;
use crate::host::Host;
use crate::interp::Interpreter;
use crate::name::{AmlPath, NameString};
use crate::namespace::{NodeId, Object};
use crate::resource::{self, Resource};
use crate::value::Value;

/// Decodes a compressed EISA ID (`EisaId("PNP0A08")` is `0x080AD041`) to
/// its seven-character form.
#[must_use]
pub fn eisa_id_to_string(v: u32) -> String {
    let b = v.swap_bytes();
    let letter = |shift: u32| char::from(b'@' + ((b >> shift) & 0x1f).to_le_bytes()[0]);
    let mut s = String::new();
    s.push(letter(26));
    s.push(letter(21));
    s.push(letter(16));
    for shift in [12u32, 8, 4, 0] {
        let d = (b >> shift) & 0xf;
        s.push(char::from_digit(d, 16).map_or('?', |c| c.to_ascii_uppercase()));
    }
    s
}

/// Encodes a seven-character EISA ID (`"PNP0A08"`), as ASL's `EisaId`
/// does. `None` if the text is not three letters and four hex digits.
#[must_use]
pub fn eisa_id(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    if b.len() != 7 || !b[..3].iter().all(u8::is_ascii_uppercase) {
        return None;
    }
    let mut v: u32 = 0;
    for &c in &b[..3] {
        v = (v << 5) | u32::from(c - b'@');
    }
    for &c in &b[3..] {
        v = (v << 4) | char::from(c).to_digit(16)?;
    }
    Some(v.swap_bytes())
}

/// A `_UID`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Uid {
    /// An integer UID.
    Integer(u64),
    /// A string UID.
    String(String),
}

/// Where a PCI interrupt pin is routed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrtSource {
    /// Hard-wired to a global system interrupt.
    Gsi(u32),
    /// Through an interrupt link device, at an index into its resources.
    Link {
        /// The link device.
        node: NodeId,
        /// Index into the link's resources.
        index: u32,
    },
    /// A link name that does not resolve (a firmware bug).
    Unresolved {
        /// The name as written.
        name: String,
        /// Index into the link's resources.
        index: u32,
    },
}

/// One `_PRT` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrtEntry {
    /// PCI device number (the high word of the address).
    pub device: u16,
    /// The low word of the address (0xFFFF: all functions).
    pub function: u16,
    /// Interrupt pin: 0 is INTA# … 3 is INTD#.
    pub pin: u8,
    /// Where the pin goes.
    pub source: PrtSource,
}

/// One `_DEP` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepRef {
    /// A node the device depends on.
    Resolved(NodeId),
    /// A name that does not resolve (a firmware bug, such as a reference to
    /// a device the firmware never defines).
    Unresolved(String),
}

/// An ID as FreeBSD reports it: an EISA ID decoded, or a string in upper
/// case (bhyve's `Bhyve_V_Gen_Counter_V1` is `_HID=BHYVE_V_GEN_COUNTER_V1`
/// in FreeBSD's `devinfo`, tests/corpus/freebsd/bhyve).
fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::Integer(x) => Some(eisa_id_to_string(u32::try_from(*x & 0xffff_ffff).ok()?)),
        Value::String(s) => Some(s.to_ascii_uppercase()),
        _ => None,
    }
}

impl Interpreter {
    /// Every `Device` object in definition order, with `\_SB` itself
    /// left out.
    #[must_use]
    pub fn devices(&self) -> Vec<NodeId> {
        let sb = self.child(self.root(), "_SB");
        self.ns
            .walk(self.root())
            .into_iter()
            .filter(|&n| Some(n) != sb && matches!(self.ns.object(n), Some(Object::Device)))
            .collect()
    }

    /// Evaluates a child of `node`, dereferencing package names and
    /// references in the result's top level.
    fn eval_opt(
        &mut self,
        node: NodeId,
        name: &str,
        h: &mut dyn Host,
    ) -> Result<Option<Value>, Error> {
        let Some(v) = self.evaluate_child(node, name, Vec::new(), h)? else {
            return Ok(None);
        };
        Ok(Some(match v {
            Value::Reference(_) | Value::Name(..) => self.deref(h, v)?,
            v => v,
        }))
    }

    /// `_HID`, with an EISA ID integer decoded to text.
    ///
    /// # Errors
    ///
    /// Evaluation errors, or [`Error::TypeMismatch`] for a value that is
    /// neither an integer nor a string.
    pub fn hid(&mut self, node: NodeId, h: &mut dyn Host) -> Result<Option<String>, Error> {
        match self.eval_opt(node, "_HID", h)? {
            None => Ok(None),
            Some(v) => id_string(&v).map(Some).ok_or(Error::TypeMismatch),
        }
    }

    /// `_CID`: one ID or a package of them.
    ///
    /// # Errors
    ///
    /// Evaluation errors, or [`Error::TypeMismatch`] for an element that is
    /// neither an integer nor a string.
    pub fn cids(&mut self, node: NodeId, h: &mut dyn Host) -> Result<Vec<String>, Error> {
        match self.eval_opt(node, "_CID", h)? {
            None => Ok(Vec::new()),
            Some(Value::Package(p)) => p
                .iter()
                .map(|e| id_string(e).ok_or(Error::TypeMismatch))
                .collect(),
            Some(v) => Ok(alloc::vec![id_string(&v).ok_or(Error::TypeMismatch)?]),
        }
    }

    /// `_HID` and `_CID` together, ignoring errors (for matching).
    pub(crate) fn ids(&mut self, h: &mut dyn Host, node: NodeId) -> Vec<String> {
        let mut v = Vec::new();
        if let Ok(Some(hid)) = self.hid(node, h) {
            v.push(hid);
        }
        if let Ok(c) = self.cids(node, h) {
            v.extend(c);
        }
        v
    }

    /// `_UID`.
    ///
    /// # Errors
    ///
    /// Evaluation errors, or [`Error::TypeMismatch`].
    pub fn uid(&mut self, node: NodeId, h: &mut dyn Host) -> Result<Option<Uid>, Error> {
        Ok(match self.eval_opt(node, "_UID", h)? {
            None => None,
            Some(Value::Integer(x)) => Some(Uid::Integer(x)),
            Some(Value::String(s)) => Some(Uid::String(s)),
            Some(_) => return Err(Error::TypeMismatch),
        })
    }

    /// `_ADR`.
    ///
    /// # Errors
    ///
    /// Evaluation errors, or [`Error::TypeMismatch`].
    pub fn adr(&mut self, node: NodeId, h: &mut dyn Host) -> Result<Option<u64>, Error> {
        match self.eval_opt(node, "_ADR", h)? {
            None => Ok(None),
            Some(v) => v.to_integer(self.width).map(Some),
        }
    }

    /// `_STA`, or 0xF (present, enabled, shown, functioning) when the
    /// device has none.
    ///
    /// # Errors
    ///
    /// Evaluation errors, or [`Error::TypeMismatch`].
    pub fn sta(&mut self, node: NodeId, h: &mut dyn Host) -> Result<u64, Error> {
        match self.eval_opt(node, "_STA", h)? {
            None => Ok(0x0f),
            Some(v) => v.to_integer(self.width),
        }
    }

    /// Evaluates a resource method (`_CRS`, `_PRS`, `_AEI`) and decodes the
    /// template.
    ///
    /// # Errors
    ///
    /// Evaluation errors, [`Error::TypeMismatch`] if the result is not a
    /// buffer, or [`Error::BadResource`] for a malformed template.
    pub fn resources(
        &mut self,
        node: NodeId,
        method: &str,
        h: &mut dyn Host,
    ) -> Result<Option<Vec<Resource>>, Error> {
        match self.eval_opt(node, method, h)? {
            None => Ok(None),
            Some(Value::Buffer(b)) => resource::parse(&b).map(Some),
            Some(_) => Err(Error::TypeMismatch),
        }
    }

    /// `_CRS`, decoded.
    ///
    /// # Errors
    ///
    /// As for [`Interpreter::resources`].
    pub fn crs(&mut self, node: NodeId, h: &mut dyn Host) -> Result<Option<Vec<Resource>>, Error> {
        self.resources(node, "_CRS", h)
    }

    /// `_PRT`, decoded.
    ///
    /// # Errors
    ///
    /// Evaluation errors, or [`Error::TypeMismatch`] for a malformed table.
    pub fn prt(&mut self, node: NodeId, h: &mut dyn Host) -> Result<Option<Vec<PrtEntry>>, Error> {
        let Some(v) = self.eval_opt(node, "_PRT", h)? else {
            return Ok(None);
        };
        let Value::Package(entries) = v else {
            return Err(Error::TypeMismatch);
        };
        let mut out = Vec::new();
        for e in entries {
            let e = match e {
                Value::Reference(_) | Value::Name(..) => self.deref(h, e)?,
                e => e,
            };
            let Value::Package(f) = e else {
                return Err(Error::TypeMismatch);
            };
            if f.len() < 4 {
                return Err(Error::TypeMismatch);
            }
            let w = self.width;
            let address = f[0].to_integer(w)?;
            let pin = u8::try_from(f[1].to_integer(w)?).map_err(|_| Error::TypeMismatch)?;
            let index = u32::try_from(f[3].to_integer(w)? & 0xffff_ffff)
                .map_err(|_| Error::TypeMismatch)?;
            let source = match &f[2] {
                Value::Integer(_) => PrtSource::Gsi(index),
                Value::String(s) => match NameString::parse(s)
                    .ok()
                    .and_then(|n| self.ns.resolve(node, &n))
                {
                    Some(n) => PrtSource::Link { node: n, index },
                    None => PrtSource::Unresolved {
                        name: s.clone(),
                        index,
                    },
                },
                other => match self.resolve_value_name(other) {
                    Some(n) => PrtSource::Link { node: n, index },
                    None => PrtSource::Unresolved {
                        name: match other {
                            Value::Name(_, n) => alloc::format!("{n}"),
                            _ => String::from("?"),
                        },
                        index,
                    },
                },
            };
            let [lo0, lo1, hi0, hi1, ..] = address.to_le_bytes();
            out.push(PrtEntry {
                device: u16::from_le_bytes([hi0, hi1]),
                function: u16::from_le_bytes([lo0, lo1]),
                pin,
                source,
            });
        }
        Ok(Some(out))
    }

    /// `_DEP`: the objects the device depends on.
    ///
    /// # Errors
    ///
    /// Evaluation errors, or [`Error::TypeMismatch`] if the result is not a
    /// package.
    pub fn dep(&mut self, node: NodeId, h: &mut dyn Host) -> Result<Vec<DepRef>, Error> {
        let Some(v) = self.eval_opt(node, "_DEP", h)? else {
            return Ok(Vec::new());
        };
        let Value::Package(p) = v else {
            return Err(Error::TypeMismatch);
        };
        Ok(p.iter()
            .map(|e| match e {
                Value::String(s) => NameString::parse(s)
                    .ok()
                    .and_then(|n| self.ns.resolve(node, &n))
                    .map_or_else(|| DepRef::Unresolved(s.clone()), DepRef::Resolved),
                other => match self.resolve_value_name(other) {
                    Some(n) => DepRef::Resolved(n),
                    None => DepRef::Unresolved(match other {
                        Value::Name(_, n) => alloc::format!("{n}"),
                        _ => String::from("?"),
                    }),
                },
            })
            .collect())
    }

    /// Evaluates the object at a path given as text (`"\\_SB.PCI0._CRS"`).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidName`] for bad text, [`Error::NotFound`] if nothing
    /// is there, otherwise evaluation errors.
    pub fn evaluate_path(
        &mut self,
        path: &str,
        args: Vec<Value>,
        h: &mut dyn Host,
    ) -> Result<Value, Error> {
        let p = AmlPath::parse(path)?;
        self.evaluate(&p, args, h)
    }
}
