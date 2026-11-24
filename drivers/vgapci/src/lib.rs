// SPDX-License-Identifier: BSD-2-Clause
//! `vgapci`: the generic driver for a VGA-compatible display function on PCI
//! (class 03h subclass 00h, or the pre-class-code VGA, class 00h subclass
//! 01h).
//!
//! It changes nothing: the firmware's decoding and the boot framebuffer it
//! set up stay as they are, which is how the boot display (the UEFI GOP
//! framebuffer, `gop-kms`) keeps working. (The bus already lists the
//! function's BARs as held, as FreeBSD's `devinfo -rv` shows them under
//! `vgapciN`.) It gives the function's place in the tree, as `devinfo`
//! shows it on FreeBSD.
//!
//! A native display driver (M-G) attaches as its child, as FreeBSD's
//! `drmn0` does under `vgapci0`, not in its place. FreeBSD lets any display
//! driver claim a `vgapci` child by its PCI IDs; here `vgapci` adds the
//! `drmn` child itself for the vendors that have a native driver (so far
//! NVIDIA's), and maps the register aperture (BAR0) for it, handing the
//! child a weak reference so that this driver's detach is never held up.
//!
//! Behavior reference: sys/dev/pci/vga_pci.c (`vga_pci_probe`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, Resource, StartCx, StopCx,
    StopReason,
};
use devtree::ids::PciId;
use pci::bus::PciChild;
use regs::Bytes;

/// PCI class 03h, subclass 00h: a VGA-compatible controller.
const DISPLAY_VGA: PciId = PciId::subclass(0x03, 0x00);
/// PCI class 00h, subclass 01h: a VGA-compatible device from before class
/// codes were defined.
const OLD_VGA: PciId = PciId::subclass(0x00, 0x01);

/// NVIDIA's PCI vendor ID.
const VENDOR_NVIDIA: u16 = 0x10de;
/// The register aperture's size on the NVIDIA GPUs (BAR0: 16 MiB).
const NVIDIA_BAR0_LEN: u64 = 0x100_0000;

/// A GPU's register aperture, read-only to its driver for now.
pub struct Aperture {
    mmio: MmioGrant<Bytes>,
    len: usize,
}

impl core::fmt::Debug for Aperture {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Aperture").field("len", &self.len).finish()
    }
}

impl Aperture {
    /// The 32-bit register at byte `offset`, or `None` if it is not
    /// aligned or lies outside the aperture.
    #[must_use]
    pub fn read32(&self, offset: usize) -> Option<u32> {
        if !offset.is_multiple_of(4) || offset.checked_add(4)? > self.len {
            return None;
        }
        Some(self.mmio.read32(offset))
    }
}

/// What the `drmn` child is given: its parent's aperture, weakly.
#[derive(Debug)]
pub struct Registers(pub Weak<Aperture>);

/// `vgapciN`.
#[derive(Debug)]
pub struct VgaPci {
    /// The register aperture, for a function a native driver is wanted for.
    aperture: Option<Arc<Aperture>>,
}

impl Driver for VgaPci {
    const NAME: &'static str = "vgapci";
    const IDS: Ids = Ids::Pci(&[DISPLAY_VGA, OLD_VGA]);

    fn describe(_: &Ident) -> String {
        "VGA-compatible display".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let mut aperture = None;
        if child.function.header.ident.vendor == VENDOR_NVIDIA
            && matches!(
                cx.resources().first(),
                Some(Resource::Memory {
                    len: NVIDIA_BAR0_LEN,
                    ..
                })
            )
        {
            // Memory decoding is on already (firmware drives this display);
            // nothing is written to the function.
            if let Ok(mmio) = cx.mmio_bytes(0) {
                aperture = Some(Arc::new(Aperture {
                    mmio,
                    len: usize::try_from(NVIDIA_BAR0_LEN).unwrap_or(0),
                }));
            }
        }
        Ok(Self { aperture })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        if let Some(a) = &self.aperture {
            let ident = Ident::Channel {
                class: "drmn",
                channel: 0,
            };
            let link = Registers(Arc::downgrade(a));
            cx.add_child(ident, Some(Arc::new(link) as _), Vec::<Resource>::new());
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<VgaPci>();

#[cfg(test)]
mod tests {
    use super::*;
    use devtree::ids::PciIdent;
    use devtree::matching::Matches;

    fn ident(class: u32) -> PciIdent {
        PciIdent {
            vendor: 0x10de,
            device: 0x2786,
            subvendor: 0,
            subdevice: 0,
            class,
            revision: 0xa1,
        }
    }

    #[test]
    fn claims_vga_functions_only() {
        // The RTX 4070 and the Radeon 610M: 0300h.
        assert!(DISPLAY_VGA.rank_for(&ident(0x03_00_00)).is_some());
        assert!(OLD_VGA.rank_for(&ident(0x00_01_00)).is_some());
        // A 3D controller (0302h) and an HDA function (0403h) are not VGA.
        assert!(DISPLAY_VGA.rank_for(&ident(0x03_02_00)).is_none());
        assert!(DISPLAY_VGA.rank_for(&ident(0x04_03_00)).is_none());
    }
}
