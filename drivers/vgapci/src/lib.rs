// SPDX-License-Identifier: BSD-2-Clause
//! `vgapci`: the generic driver for a VGA-compatible display function on PCI
//! (class 03h subclass 00h, or the pre-class-code VGA, class 00h subclass
//! 01h).
//!
//! It maps nothing and changes nothing: the firmware's decoding and the
//! boot framebuffer it set up stay as they are, which is how the boot
//! display (the UEFI GOP framebuffer, `gop-kms`) keeps working. It does list
//! the function's memory BARs as held, as FreeBSD's `devinfo -rv` shows them
//! under `vgapciN`, so nothing else is offered them. It gives
//! the function's place in the tree (`vgapciN`, as `devinfo` shows it on
//! FreeBSD), at the generic rank, which a native display driver (M-G.6)
//! outranks by matching the vendor and device.
//!
//! Behavior reference: sys/dev/pci/vga_pci.c (`vga_pci_probe`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, RmanKind, StartCx, StopCx,
    StopReason,
};
use devtree::ids::PciId;

/// PCI class 03h, subclass 00h: a VGA-compatible controller.
const DISPLAY_VGA: PciId = PciId::subclass(0x03, 0x00);
/// PCI class 00h, subclass 01h: a VGA-compatible device from before class
/// codes were defined.
const OLD_VGA: PciId = PciId::subclass(0x00, 0x01);

/// `vgapciN`.
#[derive(Debug)]
pub struct VgaPci {
    /// The function's memory BARs, first to last address.
    bars: Vec<(u64, u64)>,
}

/// The inclusive ranges of the memory resources among `resources`.
fn memory_ranges(resources: &[Resource]) -> Vec<(u64, u64)> {
    resources
        .iter()
        .filter_map(|r| match *r {
            Resource::Memory { base, len, .. } if len > 0 => Some((base, base + (len - 1))),
            _ => None,
        })
        .collect()
}

impl Driver for VgaPci {
    const NAME: &'static str = "vgapci";
    const IDS: Ids = Ids::Pci(&[DISPLAY_VGA, OLD_VGA]);

    fn describe(_: &Ident) -> String {
        "VGA-compatible display".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self {
            bars: memory_ranges(cx.resources()),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        for &range in &self.bars {
            cx.hold(RmanKind::Memory, range, None);
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
    fn holds_the_memory_bars_only() {
        let r = [
            Resource::Memory {
                base: 0x8000_0000,
                len: 0x100_0000,
                prefetchable: true,
            },
            Resource::IoPort {
                base: 0x3c0,
                len: 32,
            },
            Resource::Memory {
                base: 0x8121_4000,
                len: 0x1000,
                prefetchable: false,
            },
        ];
        // The QEMU VGA's, as FreeBSD's devinfo shows them.
        assert_eq!(
            memory_ranges(&r),
            [(0x8000_0000, 0x80ff_ffff), (0x8121_4000, 0x8121_4fff)]
        );
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
