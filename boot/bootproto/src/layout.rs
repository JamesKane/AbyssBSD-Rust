// SPDX-License-Identifier: BSD-2-Clause
//! The kernel virtual address layout, identical on every architecture.
//!
//! Only the top 128 TiB is used, which is the whole upper half on x86_64
//! (4-level paging) and the upper half of TTBR1's 256 TiB on aarch64 with a
//! 48-bit VA.
//!
//! ```text
//! 0xffff_8000_0000_0000  DMAP: all RAM at phys + DMAP_BASE (64 TiB)
//! 0xffff_c000_0000_0000  (kernel virtual memory, from P2)
//! 0xffff_f000_0000_0000  device window: loader-made MMIO mappings (64 GiB)
//! 0xffff_ffff_8000_0000  kernel image link address, + KASLR slide
//! ```

use addr::{PhysAddr, VirtAddr};

/// Base of the physical direct map.
pub const DMAP_BASE: u64 = 0xffff_8000_0000_0000;
/// Size of the DMAP window. RAM above this is not mapped.
pub const DMAP_SIZE: u64 = 1 << 46;

/// Base of the window the loader puts early device mappings in.
pub const DEVICE_WINDOW_BASE: u64 = 0xffff_f000_0000_0000;
/// Size of the device window.
pub const DEVICE_WINDOW_SIZE: u64 = 1 << 36;

/// The address the kernel ELF is linked at.
pub const KERNEL_LINK_BASE: u64 = 0xffff_ffff_8000_0000;
/// KASLR slides are multiples of this, which is also the physical
/// alignment of the image.
pub const KASLR_ALIGN: u64 = 2 << 20;
/// Number of possible slides: `slide = n * KASLR_ALIGN, n < KASLR_SLOTS`.
pub const KASLR_SLOTS: u64 = 256;
/// Largest kernel image the layout leaves room for.
pub const KERNEL_MAX_SIZE: u64 = 1 << 30;

/// The DMAP address of `p`, or `None` if `p` is beyond the DMAP.
#[must_use]
pub const fn dmap(p: PhysAddr) -> Option<VirtAddr> {
    if p.get() < DMAP_SIZE {
        Some(VirtAddr::new(DMAP_BASE + p.get()))
    } else {
        None
    }
}

/// The physical address behind DMAP address `v`, or `None` if `v` is not in
/// the DMAP.
#[must_use]
pub const fn dmap_to_phys(v: VirtAddr) -> Option<PhysAddr> {
    match v.get().checked_sub(DMAP_BASE) {
        Some(off) if off < DMAP_SIZE => Some(PhysAddr::new(off)),
        _ => None,
    }
}

// The windows must not overlap and must stay in the top 128 TiB.
const _: () = {
    assert!(DMAP_BASE == 0xffff_8000_0000_0000);
    assert!(DMAP_BASE + DMAP_SIZE <= DEVICE_WINDOW_BASE);
    assert!(DEVICE_WINDOW_BASE + DEVICE_WINDOW_SIZE <= KERNEL_LINK_BASE);
    assert!(
        KERNEL_LINK_BASE
            .checked_add(KASLR_ALIGN * KASLR_SLOTS + KERNEL_MAX_SIZE - 1)
            .is_some()
    );
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmap_round_trip() {
        let p = PhysAddr::new(0x1234_5000);
        let v = dmap(p).unwrap();
        assert_eq!(v.get(), 0xffff_8000_1234_5000);
        assert_eq!(dmap_to_phys(v), Some(p));
        assert_eq!(dmap(PhysAddr::new(DMAP_SIZE)), None);
        assert_eq!(dmap_to_phys(VirtAddr::new(KERNEL_LINK_BASE)), None);
    }
}
