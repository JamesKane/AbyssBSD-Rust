// SPDX-License-Identifier: BSD-2-Clause
//! Video memory: how much there is, where the firmware's workspaces and the
//! protected region sit, and what the GPU system processor's heap needs
//! (specification 01, sections 4 and 5), plus the register offsets read to
//! learn them.

#![allow(
    clippy::cast_possible_truncation,
    reason = "register fields are masked first"
)]

/// Usable video memory in MiB, filled in by the GPU's boot firmware (Ampere
/// GA102 and later).
pub const NV_USABLE_FB_SIZE_IN_MB: usize = 0x0011_83a4;
/// Whether the display engine is fused off (Ampere GA100 and later, bit 0).
pub const NV_FUSE_STATUS_OPT_DISPLAY: usize = 0x0082_0c04;
/// The display engine's VGA workspace base.
pub const NV_PDISP_VGA_WORKSPACE_BASE: usize = 0x0062_5f04;
/// Lower bound of the second write-protected region.
pub const NV_PFB_PRI_MMU_WPR2_ADDR_LO: usize = 0x001f_a824;
/// Upper bound of the second write-protected region.
pub const NV_PFB_PRI_MMU_WPR2_ADDR_HI: usize = 0x001f_a828;
/// Which privilege levels may read the boot-progress register.
pub const NV_PGC6_AON_SECURE_SCRATCH_GROUP_05_PRIV_LEVEL_MASK: usize = 0x0011_8128;
/// The boot firmware's progress (low byte `0xff` when done).
pub const NV_PGC6_AON_SECURE_SCRATCH_GROUP_05_0_GFW_BOOT: usize = 0x0011_8234;

const MIB: u64 = 1 << 20;
const KIB: u64 = 1 << 10;

/// Usable video memory in bytes, from `NV_USABLE_FB_SIZE_IN_MB`.
#[must_use]
pub const fn usable_fb_bytes(register: u32) -> u64 {
    (register as u64) * MIB
}

/// Where the boot firmware says it is done: the privilege mask lets the
/// host read the progress register, and the progress is `0xff`. `None` if
/// the host may not read it yet (the progress register means nothing then).
#[must_use]
pub const fn gfw_boot_done(priv_level_mask: u32, progress: u32) -> Option<bool> {
    if priv_level_mask & 1 == 0 {
        return None;
    }
    Some(progress & 0xff == 0xff)
}

/// Where the VGA workspace starts in video memory.
///
/// With the display fused off, or the workspace register not valid (bit 3
/// clear), it is the top 1 MiB. Otherwise the register's address is used,
/// except that an address below the top 1 MiB is clamped to the top 128 KiB.
#[must_use]
pub const fn vga_workspace_base(fb_size: u64, display_disabled: bool, register: u32) -> u64 {
    let top = fb_size.saturating_sub(MIB);
    if display_disabled || register & 8 == 0 {
        return top;
    }
    let address = ((register >> 8) as u64) << 16;
    if address < top {
        fb_size.saturating_sub(0x20000)
    } else {
        address
    }
}

/// The second write-protected region's bounds, when it is set up: the
/// inclusive lower and exclusive upper physical addresses. `None` when the
/// upper register is zero.
///
/// The upper register holds the base of the region's last 128 KiB granule
/// (bits 31:4, as address >> 12), so the exclusive end is that plus 128 KiB.
/// Measured on the RTX 4070: after FRTS the registers read 0x2ff800000 and
/// 0x2ff8e0000, and the region ends at the FRTS region's end, 0x2ff900000.
#[must_use]
pub const fn wpr2_bounds(lo: u32, hi: u32) -> Option<(u64, u64)> {
    if hi >> 4 == 0 {
        return None;
    }
    let last_granule = ((hi >> 4) as u64) << 12;
    Some((((lo >> 4) as u64) << 12, last_granule + 0x2_0000))
}

/// How a chip family sizes the firmware's heap and reserves memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heap {
    /// The operating-system carveout (22 MiB for the third generation of
    /// the firmware's operating system, none for the second).
    pub os_carveout: u64,
    /// The base working memory of the resource manager.
    pub base_working: u64,
    /// The lowest the total may be clamped to.
    pub min: u64,
    /// The highest the total may be clamped to.
    pub max: u64,
}

impl Heap {
    /// Ampere GA102 on, through Ada (8 MiB of base working memory).
    pub const ADA: Self = Self {
        os_carveout: 22 * MIB,
        base_working: 8 * MIB,
        min: 88 * MIB,
        max: 280 * MIB - 1,
    };
    /// Hopper and Blackwell (14 MiB of base working memory).
    pub const BLACKWELL: Self = Self {
        os_carveout: 22 * MIB,
        base_working: 14 * MIB,
        min: 88 * MIB,
        max: 280 * MIB - 1,
    };

    /// The heap size for a card with `fb_size` bytes of video memory and
    /// no virtual functions: the sum of the carveout, the base working
    /// memory, 96 MiB of client allocations, and 96 KiB per started GiB of
    /// video memory (in whole MiB), clamped.
    #[must_use]
    pub const fn size(self, fb_size: u64) -> u64 {
        let gib = fb_size.div_ceil(1 << 30);
        let management = (gib * 96 * KIB).next_multiple_of(MIB);
        let total = self.os_carveout + self.base_working + 96 * MIB + management;
        if total < self.min {
            self.min
        } else if total > self.max {
            self.max
        } else {
            total
        }
    }
}

/// A region of video memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// First address.
    pub start: u64,
    /// One past the last.
    pub end: u64,
}

/// The firmware's regions in video memory, which grow downward from the top
/// of the framebuffer (Turing, Ampere, and Ada, where the host computes
/// them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The whole framebuffer.
    pub fb: Region,
    /// The VGA workspace.
    pub vga_workspace: Region,
    /// The firmware runtime security region.
    pub frts: Region,
    /// The GPU system processor's bootloader.
    pub boot: Region,
    /// The firmware image.
    pub fw_image: Region,
    /// The write-protected region's heap.
    pub wpr2_heap: Region,
    /// The write-protected region (its metadata header sits at its start).
    pub wpr2: Region,
    /// The heap outside the write-protected region.
    pub non_wpr_heap: Region,
}

/// Why a layout could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutError;

const fn align_down(v: u64, a: u64) -> u64 {
    v & !(a - 1)
}

/// The layout for `fb_size` bytes of video memory (the host computes it on
/// Turing through Ada): the VGA workspace at `vga_base`, a bootloader of
/// `bootloader_size` bytes, a firmware image of `fw_image_size` bytes, and a
/// write-protected heap of `heap_size`. The firmware runtime security region
/// is 1 MiB and the heap outside the protected region 1 MiB.
///
/// # Errors
///
/// [`LayoutError`] if the regions do not fit in the memory.
pub fn layout(
    fb_size: u64,
    vga_base: u64,
    bootloader_size: u64,
    fw_image_size: u64,
    heap_size: u64,
) -> Result<Layout, LayoutError> {
    const FRTS_SIZE: u64 = MIB;
    const NON_WPR_HEAP: u64 = MIB;
    if vga_base > fb_size {
        return Err(LayoutError);
    }
    let frts_end = align_down(vga_base, 128 * KIB);
    let frts_start = frts_end.checked_sub(FRTS_SIZE).ok_or(LayoutError)?;
    let boot_start = align_down(
        frts_start.checked_sub(bootloader_size).ok_or(LayoutError)?,
        4 * KIB,
    );
    let fw_start = align_down(
        boot_start.checked_sub(fw_image_size).ok_or(LayoutError)?,
        64 * KIB,
    );
    let heap_start = align_down(fw_start.checked_sub(heap_size).ok_or(LayoutError)?, MIB);
    let heap_end = align_down(fw_start, MIB);
    // Room for the 256-byte metadata header before the heap.
    let wpr2_start = align_down(heap_start.checked_sub(256).ok_or(LayoutError)?, MIB);
    let non_wpr_start = wpr2_start.checked_sub(NON_WPR_HEAP).ok_or(LayoutError)?;
    Ok(Layout {
        fb: Region {
            start: 0,
            end: fb_size,
        },
        vga_workspace: Region {
            start: vga_base,
            end: fb_size,
        },
        frts: Region {
            start: frts_start,
            end: frts_end,
        },
        boot: Region {
            start: boot_start,
            end: boot_start + bootloader_size,
        },
        fw_image: Region {
            start: fw_start,
            end: fw_start + fw_image_size,
        },
        wpr2_heap: Region {
            start: heap_start,
            end: heap_end,
        },
        wpr2: Region {
            start: wpr2_start,
            end: frts_end,
        },
        non_wpr_heap: Region {
            start: non_wpr_start,
            end: wpr2_start,
        },
    })
}

/// The firmware runtime security region for a VGA workspace at `vga_base`:
/// 1 MiB ending at `vga_base` rounded down to 128 KiB. `None` if it would
/// start below zero.
#[must_use]
pub const fn frts_region(vga_base: u64) -> Option<Region> {
    let end = align_down(vga_base, 128 * KIB);
    match end.checked_sub(MIB) {
        Some(start) => Some(Region { start, end }),
        None => None,
    }
}

/// The system memory flush page's two register values on Ampere and Ada:
/// `(high, low)`, written high first. `None` if `pa` does not fit the
/// 47-bit DMA mask or is not 4 KiB aligned.
#[must_use]
pub const fn flush_registers_ada(pa: u64) -> Option<(u32, u32)> {
    if pa & 0xfff != 0 || pa >> 47 != 0 {
        return None;
    }
    Some((
        ((pa >> 40) & 0x00ff_ffff) as u32,
        ((pa >> 8) & 0xffff_ffff) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn sizes_and_progress() {
        assert_eq!(usable_fb_bytes(12 * 1024), 12 * GIB);
        assert_eq!(usable_fb_bytes(u32::MAX), 0xffff_ffff << 20);
        assert_eq!(gfw_boot_done(0, 0xff), None, "the host may not read yet");
        assert_eq!(gfw_boot_done(1, 0xff), Some(true));
        assert_eq!(gfw_boot_done(1, 0x7f), Some(false));
        assert_eq!(
            gfw_boot_done(1, 0x1_00ff),
            Some(true),
            "only the low byte counts"
        );
    }

    #[test]
    fn the_vga_workspace() {
        let fb = 12 * GIB;
        // Fused off, or not valid: the top megabyte.
        assert_eq!(vga_workspace_base(fb, true, 0xffff_ffff), fb - MIB);
        assert_eq!(vga_workspace_base(fb, false, 0x0), fb - MIB);
        // A valid address in the top megabyte stands (field 31:8, shifted
        // 16: 0x2ff0_0000 >> 8 << 16 style).
        let top = fb - MIB;
        let reg = (((top >> 16) as u32) << 8) | 8;
        assert_eq!(vga_workspace_base(fb, false, reg), top);
        // One below it is clamped to the top 128 KiB.
        let low = (((top >> 16) as u32 - 1) << 8) | 8;
        assert_eq!(vga_workspace_base(fb, false, low), fb - 0x20000);
        // A tiny framebuffer cannot underflow.
        assert_eq!(vga_workspace_base(0x1000, false, 0), 0);
        assert_eq!(vga_workspace_base(0x1000, false, 8), 0);
    }

    #[test]
    fn the_protected_region() {
        assert_eq!(wpr2_bounds(0x1234_5670, 0), None);
        // The upper register is the base of the last 128 KiB granule.
        assert_eq!(
            wpr2_bounds(0x0000_0010, 0x0000_0020),
            Some((0x1000, 0x2000 + 0x2_0000))
        );
        // The low four bits are not part of the field.
        assert_eq!(wpr2_bounds(0x1f, 0x2f), Some((0x1000, 0x2000 + 0x2_0000)));
        // The RTX 4070 after FRTS: 0x2ff800000..0x2ff900000.
        assert_eq!(
            wpr2_bounds(0x2ff8_0000 >> 8 << 4, 0x2ff8_e000 >> 8 << 4),
            Some((0x2_ff80_0000, 0x2_ff90_0000))
        );
    }

    #[test]
    fn the_heap_for_a_12_gib_card() {
        // 22 + 8 + 96 + 2 MiB = 128 MiB, the specification's example.
        assert_eq!(Heap::ADA.size(12 * GIB), 128 * MIB);
        assert_eq!(Heap::BLACKWELL.size(16 * GIB), 134 * MIB);
        // No video memory still needs the carveout and the clients' share
        // (the lower clamp never binds), and an absurd amount is clamped down.
        assert_eq!(Heap::ADA.size(0), 126 * MIB);
        assert_eq!(Heap::ADA.size(u64::MAX / 2), 280 * MIB - 1);
    }

    #[test]
    fn the_layout_grows_downward_and_stays_aligned() {
        let fb = 12 * GIB;
        let vga = fb - MIB;
        let l = layout(fb, vga, 0x8000, 0x120_0000, Heap::ADA.size(fb)).unwrap();
        assert_eq!(
            l.frts,
            Region {
                start: vga - MIB,
                end: vga
            }
        );
        assert!(l.boot.end <= l.frts.start && l.boot.start.is_multiple_of(4096));
        assert!(l.fw_image.end <= l.boot.start && l.fw_image.start.is_multiple_of(65536));
        assert!(l.wpr2_heap.end <= l.fw_image.start + 0xffff);
        assert_eq!(l.wpr2_heap.start % MIB, 0);
        assert_eq!(l.wpr2.end, l.frts.end);
        assert!(l.wpr2.start < l.wpr2_heap.start);
        assert_eq!(l.non_wpr_heap.end, l.wpr2.start);
        assert_eq!(l.non_wpr_heap.end - l.non_wpr_heap.start, MIB);
        assert_eq!(
            l.vga_workspace,
            Region {
                start: vga,
                end: fb
            }
        );
    }

    #[test]
    fn a_layout_that_does_not_fit_is_an_error() {
        let fb = 256 * MIB;
        assert_eq!(layout(fb, fb - MIB, 0, 0, 300 * MIB), Err(LayoutError));
        assert_eq!(layout(fb, fb + 1, 0, 0, 0), Err(LayoutError));
        assert_eq!(layout(0, 0, 0, 0, 0), Err(LayoutError));
        assert_eq!(layout(fb, fb, u64::MAX, 0, 0), Err(LayoutError));
        assert_eq!(layout(fb, fb, 0, u64::MAX, 0), Err(LayoutError));
    }

    #[test]
    fn the_runtime_security_region() {
        // The 4070's: workspace in the top megabyte of 12282 MiB.
        let vga = 12282 * MIB - MIB;
        let r = frts_region(vga).unwrap();
        assert_eq!((r.start, r.end), (vga - MIB, vga));
        assert_eq!(frts_region(vga + 0x1_0000).unwrap().end, vga);
        assert_eq!(frts_region(MIB - 1), None);
        assert_eq!(frts_region(0), None);
    }

    #[test]
    fn the_flush_page_registers() {
        assert_eq!(
            flush_registers_ada(0x100_2345_6000),
            Some((0x1, 0x0023_4560))
        );
        assert_eq!(
            flush_registers_ada(0x0000_7fff_ffff_f000).map(|r| r.0),
            Some(0x7f)
        );
        assert_eq!(flush_registers_ada(0x8000_0000_0000), None, "over 47 bits");
        assert_eq!(flush_registers_ada(0x1001), None, "not 4 KiB aligned");
    }
}
