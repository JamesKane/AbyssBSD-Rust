// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! Doorbells (specification 05 section 10): the aperture (BAR2), its enable,
//! and the interrupt handler's range. The ranges Raphael's bus interface
//! keeps behind a port index/data pair are reached the way the specification
//! describes: write the register's byte address to the index, read the index
//! back, then the data.

use crate::regs::Window;

/// `RCC_DEV0_EPF0_RCC_DOORBELL_APER_EN`: bit 0 enables the aperture.
pub const APER_EN: usize = 0x3780;
/// `BIF_BX_PF0_RSMU_INDEX`.
pub const RSMU_INDEX: usize = 0x50;
/// `BIF_BX_PF0_RSMU_DATA`.
pub const RSMU_DATA: usize = 0x54;
/// `GDC0_BIF_IH_DOORBELL_RANGE`'s byte address through the index/data pair
/// (dword `0x500ef2`, 05 section 10.3).
pub const IH_RANGE_RSMU: u32 = 0x50_0ef2 * 4;
/// `GDC0_BIF_SDMA0_DOORBELL_RANGE` through the same pair (dword `0x500ef0`,
/// 05 section 10.3), and engine 0's doorbell as a dword index (slot `0x100`).
pub const SDMA0_RANGE_RSMU: u32 = 0x50_0ef0 * 4;
pub const SDMA0_DOORBELL_DWORD: u32 = 0x200;
/// The interrupt handler's doorbell, as a dword index (slot `0x178`, 05
/// section 10.2).
pub const IH_DOORBELL_DWORD: u32 = 0x2f0;
/// `IH_DOORBELL_RPTR`'s enable bit.
pub const IH_DOORBELL_ENABLE: u32 = 1 << 28;

/// Enables the doorbell aperture.
pub fn enable_aperture<W: Window + ?Sized>(w: &W) {
    w.write32(APER_EN, w.read32(APER_EN) | 1);
}

/// Whether the aperture is enabled.
pub fn aperture_enabled<W: Window + ?Sized>(w: &W) -> bool {
    w.read32(APER_EN) & 1 != 0
}

/// Reads a register behind the index/data pair at byte address `addr`.
pub fn rsmu_read<W: Window + ?Sized>(w: &W, addr: u32) -> u32 {
    w.write32(RSMU_INDEX, addr);
    // Read back, so the index has landed before the data is read.
    let _ = w.read32(RSMU_INDEX);
    w.read32(RSMU_DATA)
}

/// Writes a register behind the index/data pair at byte address `addr`.
pub fn rsmu_write<W: Window + ?Sized>(w: &W, addr: u32, value: u32) {
    w.write32(RSMU_INDEX, addr);
    let _ = w.read32(RSMU_INDEX);
    w.write32(RSMU_DATA, value);
}

/// The value of a bus interface doorbell range register: the first dword
/// index (bits 11:2) and the size in dwords (bits 20:16).
#[must_use]
pub const fn range_value(first_dword: u32, size_dwords: u32) -> u32 {
    (first_dword & 0x3ff) << 2 | (size_dwords & 0x1f) << 16
}

/// The `IH_DOORBELL_RPTR` value: the doorbell's dword index and the enable.
#[must_use]
pub const fn ih_doorbell_rptr(dword: u32) -> u32 {
    (dword & 0x03ff_ffff) | IH_DOORBELL_ENABLE
}

/// Points the bus interface's interrupt handler range at the handler's
/// doorbell (two dwords) and enables the aperture. The index/data pair's
/// own index is put back as it was found.
pub fn program_ih_range<W: Window + ?Sized>(w: &W) {
    enable_aperture(w);
    let old = w.read32(RSMU_INDEX);
    rsmu_write(w, IH_RANGE_RSMU, range_value(IH_DOORBELL_DWORD, 2));
    w.write32(RSMU_INDEX, old);
}

/// Points the bus interface's engine 0 range at its doorbells (20 dwords from
/// the ring's) and enables the aperture.
pub fn program_sdma0_range<W: Window + ?Sized>(w: &W) {
    enable_aperture(w);
    let old = w.read32(RSMU_INDEX);
    // Only the offset (11:2) and size (20:16) fields change.
    let keep = rsmu_read(w, SDMA0_RANGE_RSMU) & !(0x3ff << 2) & !(0x1f << 16);
    rsmu_write(
        w,
        SDMA0_RANGE_RSMU,
        keep | range_value(SDMA0_DOORBELL_DWORD, 20),
    );
    w.write32(RSMU_INDEX, old);
}

/// Reads the interrupt handler's range back.
pub fn ih_range<W: Window + ?Sized>(w: &W) -> u32 {
    let old = w.read32(RSMU_INDEX);
    let v = rsmu_read(w, IH_RANGE_RSMU);
    w.write32(RSMU_INDEX, old);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// A bus interface with the port index/data pair and one range register
    /// behind it.
    struct Model {
        regs: RefCell<BTreeMap<usize, u32>>,
        behind: RefCell<BTreeMap<u32, u32>>,
        log: RefCell<Vec<(usize, u32)>>,
    }

    impl Window for Model {
        fn read32(&self, o: usize) -> u32 {
            if o == RSMU_DATA {
                let at = *self.regs.borrow().get(&RSMU_INDEX).unwrap_or(&0);
                return *self.behind.borrow().get(&at).unwrap_or(&0);
            }
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
        fn write32(&self, o: usize, v: u32) {
            self.log.borrow_mut().push((o, v));
            if o == RSMU_DATA {
                let at = *self.regs.borrow().get(&RSMU_INDEX).unwrap_or(&0);
                self.behind.borrow_mut().insert(at, v);
            } else {
                self.regs.borrow_mut().insert(o, v);
            }
        }
    }

    fn model() -> Model {
        Model {
            regs: RefCell::new(BTreeMap::new()),
            behind: RefCell::new(BTreeMap::new()),
            log: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn the_range_is_behind_the_port_not_in_the_window() {
        // 05 section 10.3: dword 0x500ef2, byte 0x1403bc8.
        assert_eq!(IH_RANGE_RSMU, 0x140_3bc8);
        let m = model();
        program_ih_range(&m);
        assert!(aperture_enabled(&m));
        // Offset 0x2f0 in bits 11:2, size 2 in bits 20:16.
        assert_eq!(m.behind.borrow().get(&IH_RANGE_RSMU), Some(&0x2_0bc0));
        assert_eq!(ih_range(&m), 0x2_0bc0);
        // No write ever went to the range's own address in the window.
        assert!(m.log.borrow().iter().all(|w| w.0 != IH_RANGE_RSMU as usize));
    }

    #[test]
    fn the_index_is_put_back_and_read_back_before_the_data() {
        let m = model();
        m.regs.borrow_mut().insert(RSMU_INDEX, 0x1234);
        program_ih_range(&m);
        assert_eq!(m.regs.borrow().get(&RSMU_INDEX), Some(&0x1234));
        // The sequence: index write, (read back), data write.
        let log = m.log.borrow();
        let index = log
            .iter()
            .position(|w| *w == (RSMU_INDEX, IH_RANGE_RSMU))
            .unwrap();
        let data = log.iter().position(|w| w.0 == RSMU_DATA).unwrap();
        assert!(index < data);
    }

    #[test]
    fn the_doorbell_values() {
        assert_eq!(range_value(0x2f0, 2), 0x2_0bc0);
        assert_eq!(
            range_value(0x2f0 | 0x400, 2),
            0x2_0bc0,
            "the field is ten bits"
        );
        assert_eq!(ih_doorbell_rptr(0x2f0), 0x1000_02f0);
        // The doorbell's byte offset in BAR2 is four times its dword index.
        assert_eq!(IH_DOORBELL_DWORD * 4, 0xbc0);
    }
}
