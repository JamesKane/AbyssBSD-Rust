// SPDX-License-Identifier: BSD-2-Clause
//! `sky1_pinctrl`: the CIX Sky1's two pin controllers, `MUX0` (`CIXHA016`,
//! the FCH's 138 pads) and `MUX1` (`CIXHA017`, the always-on S5 domain's
//! 66 pads). Most SoC devices name a group of one in their `_CRS`
//! (`PinGroupFunction`), so they wait for its driver.
//!
//! Each pad is one 32-bit register at pad × 4: `FUNC` (the mux mode) in
//! bits 8:7, then pull-up, pull-down, Schmitt trigger, and a drive
//! strength code; the bits above are unknown and preserved
//! (clean-room `sky1-peripherals` specification, §5.2).
//!
//! The controller's own `_CRS` declares its groups (`PinGroup`). Each
//! pad's vendor data is four bytes: the pad register's offset and the
//! value firmware programs into its low half, both big-endian 16-bit
//! ([`groups`]). Firmware has already programmed every pad, as CIX's own
//! ACPI driver assumes, so attach writes nothing: it reads each grouped
//! pad and logs any that differs from its declaration. Re-applying groups
//! (after the FCH loses power in suspend) comes with suspend.
//!
//! FreeBSD has no driver for them; the name is recorded in
//! docs/compat-deviations.md.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use acpi::AcpiDevice;
use aml::resource::Resource;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, StopCx, StopReason};
use devtree::ids::AcpiId;
use regs::Bytes;

#[cfg(test)]
mod tests;

/// The FCH controller's `_HID`.
pub const FCH_HID: &str = "CIXHA016";
/// The S5 controller's `_HID`.
pub const S5_HID: &str = "CIXHA017";

/// The bits of a pad register firmware's declarations cover: `FUNC` and
/// the configuration.
pub const PAD_BITS: u32 = 0x1ff;

/// Which controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Controller {
    /// The FCH controller (`MUX0`).
    Fch,
    /// The S5 controller (`MUX1`).
    S5,
}

impl Controller {
    /// The controller a `_HID` names.
    #[must_use]
    pub fn of(hid: &str) -> Option<Self> {
        match hid {
            FCH_HID => Some(Self::Fch),
            S5_HID => Some(Self::S5),
            _ => None,
        }
    }

    /// How many pads it has.
    #[must_use]
    pub const fn pads(self) -> u16 {
        match self {
            Self::Fch => 138,
            Self::S5 => 66,
        }
    }
}

/// A pad as a group declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pad {
    /// Its number.
    pub pad: u16,
    /// The value of its register's low bits ([`PAD_BITS`]).
    pub value: u32,
}

impl Pad {
    /// The mux mode.
    #[must_use]
    pub const fn function(self) -> u32 {
        self.value >> 7 & 3
    }
}

/// A group the controller declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Its label, which consumers name.
    pub label: String,
    /// Its pads.
    pub pads: Vec<Pad>,
}

/// Why a declaration could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed {
    /// The group.
    pub label: String,
    /// What is wrong.
    pub why: &'static str,
}

/// The groups the controller's `_CRS` declares, for `controller`.
///
/// # Errors
///
/// The first group whose vendor data does not give one (offset, value)
/// pair per pad, names another pad's register, or names a pad the
/// controller lacks.
pub fn groups(controller: Controller, crs: &[Resource]) -> Result<Vec<Group>, Malformed> {
    let mut out = Vec::new();
    for r in crs {
        let Resource::PinGroup(g) = r else {
            continue;
        };
        let bad = |why| Malformed {
            label: g.label.clone(),
            why,
        };
        let (data, rest) = g.vendor.as_chunks::<4>();
        if !rest.is_empty() || data.len() != g.pins.len() {
            return Err(bad("vendor data is not four bytes per pad"));
        }
        let mut pads = Vec::with_capacity(g.pins.len());
        for (&pad, d) in g.pins.iter().zip(data) {
            let offset = u16::from_be_bytes([d[0], d[1]]);
            if pad >= controller.pads() {
                return Err(bad("pad out of range"));
            }
            if u32::from(offset) != u32::from(pad) * 4 {
                return Err(bad("register offset is not the pad's"));
            }
            let value = u32::from(u16::from_be_bytes([d[2], d[3]]));
            if value & !PAD_BITS != 0 {
                return Err(bad("value sets unknown bits"));
            }
            pads.push(Pad { pad, value });
        }
        out.push(Group {
            label: g.label.clone(),
            pads,
        });
    }
    Ok(out)
}

/// A grouped pad whose register differs from its declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Differs {
    /// The group.
    pub label: String,
    /// The pad and its declared value.
    pub declared: Pad,
    /// What the register holds ([`PAD_BITS`] of it).
    pub found: u32,
}

/// The grouped pads whose registers (read by `read`, by offset) differ
/// from their declarations.
pub fn differences(groups: &[Group], read: impl Fn(usize) -> u32) -> Vec<Differs> {
    let mut out = Vec::new();
    for g in groups {
        for &p in &g.pads {
            let found = read(usize::from(p.pad) * 4) & PAD_BITS;
            if found != p.value {
                out.push(Differs {
                    label: g.label.clone(),
                    declared: p,
                    found,
                });
            }
        }
    }
    out
}

/// `sky1_pinctrlN`.
pub struct Sky1Pinctrl {
    _regs: MmioGrant<Bytes>,
    groups: Vec<Group>,
}

impl core::fmt::Debug for Sky1Pinctrl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sky1Pinctrl")
            .field("groups", &self.groups.len())
            .finish_non_exhaustive()
    }
}

fn controller(ident: &Ident) -> Option<Controller> {
    match ident {
        Ident::Acpi(id, _) => Controller::of(id.hid.as_deref()?),
        _ => None,
    }
}

impl Driver for Sky1Pinctrl {
    const NAME: &'static str = "sky1_pinctrl";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new(FCH_HID), AcpiId::new(S5_HID)]);

    fn describe(ident: &Ident) -> String {
        match controller(ident) {
            Some(Controller::S5) => "CIX Sky1 S5 pin controller".into(),
            _ => "CIX Sky1 FCH pin controller".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let c = controller(cx.ident()).ok_or(AttachError::Decline)?;
        let dev = cx.bus_data::<AcpiDevice>().ok_or(AttachError::Decline)?;
        let groups = groups(c, &dev.crs).map_err(|m| {
            bus::manager().log(format_args!(
                "sky1_pinctrl{}: group {}: {}",
                cx.unit(),
                m.label,
                m.why
            ));
            AttachError::Device("malformed pin group")
        })?;
        let regs = cx.mmio_bytes(0)?;
        if regs.len() < usize::from(c.pads()) * 4 {
            return Err(AttachError::Device("register window too small"));
        }
        // What the firmware left differs from what it declares in many
        // places, all harmless so far: a bring-up note (`bootverbose`).
        let notes = if bus::bootverbose() {
            differences(&groups, |off| regs::Mmio::read32(&regs, off))
        } else {
            Vec::new()
        };
        for d in notes {
            bus::manager().log(format_args!(
                "sky1_pinctrl{}: {} pad {}: firmware declares {:#05x}, the pad holds {:#05x}",
                cx.unit(),
                d.label,
                d.declared.pad,
                d.declared.value,
                d.found
            ));
        }
        Ok(Self {
            _regs: regs,
            groups,
        })
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Sky1Pinctrl>();
