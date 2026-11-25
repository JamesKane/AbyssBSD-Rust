// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! Scan-out from system memory (specification 08 section 2.3.1): the display
//! hub's context 0 is programmed as a copy of the GART, and a surface whose
//! address is a GART address is translated through it. The entries for such
//! pages are valid, system, readable, writeable, executable, uncached and
//! not snooped (the display does not snoop the processor's caches, so the
//! pages are written back before they are shown).

use crate::regs::Window;
use crate::vm::{self, Flags, MemoryType};

/// `DCN_VM_CONTEXT0_CNTL`: depth 0 (one flat level), block size 0.
pub const CONTEXT0_CNTL: usize = 0xe864;
/// `DCN_VM_CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32` and `_LO32`: the table's
/// directory entry; the low half is written last and commits.
pub const CONTEXT0_BASE_HI: usize = 0xe868;
pub const CONTEXT0_BASE_LO: usize = 0xe86c;
/// `_START_ADDR_HI32` and `_LO32`: the first GART page number.
pub const CONTEXT0_START_HI: usize = 0xe870;
pub const CONTEXT0_START_LO: usize = 0xe874;
/// `_END_ADDR_HI32` and `_LO32`: the last GART page number, inclusive.
pub const CONTEXT0_END_HI: usize = 0xe878;
pub const CONTEXT0_END_LO: usize = 0xe87c;
/// `HUBPREQ0_DCN_VM_MX_L1_TLB_CNTL`: the pipe's translation buffer on, in
/// system access mode 3.
pub const MX_L1_TLB_CNTL: usize = 0xec0c;
/// The value: `ENABLE_L1_TLB` (bit 0) and `SYSTEM_ACCESS_MODE` 3 (bits 4:3).
pub const MX_L1_TLB_VALUE: u32 = 1 | (3 << 3);

/// The GART's last page number for its 1 GiB: `0x3ffff`.
pub const GART_LAST_PAGE: u32 = 0x3_ffff;

/// Why the display hub's context did not come up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The base register's valid bit did not read back as set.
    NotValid,
}

/// Programs context 0 of the display hub as a copy of the GART whose table
/// is at physical address `table` (the value the memory hub's context 0
/// holds, 05 section 4.3), and turns the pipe's translation buffer on.
///
/// # Errors
///
/// [`Error::NotValid`] if the base register's valid bit stays clear for
/// `polls` looks.
#[allow(clippy::cast_possible_truncation)]
pub fn program_context0<W: Window + ?Sized>(
    w: &W,
    table: u64,
    polls: u32,
    pause: &dyn Fn(),
) -> Result<(), Error> {
    w.write32(CONTEXT0_CNTL, 0);
    w.write32(CONTEXT0_START_HI, 0);
    w.write32(CONTEXT0_START_LO, 0);
    w.write32(CONTEXT0_END_HI, 0);
    w.write32(CONTEXT0_END_LO, GART_LAST_PAGE);
    // The directory entry: the table's physical address, valid.
    let entry = (table & !0xfff) | 1;
    w.write32(CONTEXT0_BASE_HI, (entry >> 32) as u32);
    // The low half commits the context.
    w.write32(CONTEXT0_BASE_LO, entry as u32);
    for _ in 0..polls {
        if w.read32(CONTEXT0_BASE_LO) & 1 != 0 {
            w.write32(MX_L1_TLB_CNTL, MX_L1_TLB_VALUE);
            return Ok(());
        }
        pause();
    }
    Err(Error::NotValid)
}

/// The GART entry for scan-out page `phys`.
#[must_use]
pub fn entry(phys: u64) -> u64 {
    vm::pte(
        phys,
        Flags {
            readable: true,
            writeable: true,
            executable: true,
            system: true,
            snooped: false,
            memory_type: MemoryType::Uc,
        },
    )
}

/// How many 4 KiB pages a surface of `bytes` takes.
#[must_use]
pub const fn pages_for(bytes: u64) -> u64 {
    bytes.div_ceil(4096)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    struct Model {
        regs: RefCell<alloc::vec::Vec<(usize, u32)>>,
        /// Reads of the base register before its valid bit shows.
        slow: u32,
        looked: RefCell<u32>,
    }

    impl Window for Model {
        fn read32(&self, o: usize) -> u32 {
            if o == CONTEXT0_BASE_LO {
                let mut n = self.looked.borrow_mut();
                *n += 1;
                if *n <= self.slow {
                    return 0;
                }
            }
            self.regs
                .borrow()
                .iter()
                .rev()
                .find(|(a, _)| *a == o)
                .map_or(0, |(_, v)| *v)
        }
        fn write32(&self, o: usize, v: u32) {
            self.regs.borrow_mut().push((o, v));
        }
    }

    fn model(slow: u32) -> Model {
        Model {
            regs: RefCell::new(alloc::vec::Vec::new()),
            slow,
            looked: RefCell::new(0),
        }
    }

    #[test]
    fn the_context_is_a_copy_of_the_gart_with_the_low_base_written_last() {
        let m = model(0);
        program_context0(&m, 0x18_4900_0000, 10, &|| {}).unwrap();
        let log = m.regs.borrow().clone();
        // The base's low half is the last of the context's registers, then the
        // buffer is turned on.
        let at = |o: usize| log.iter().position(|(a, _)| *a == o).unwrap();
        assert!(at(CONTEXT0_BASE_HI) < at(CONTEXT0_BASE_LO));
        assert!(at(CONTEXT0_CNTL) < at(CONTEXT0_BASE_HI));
        assert!(at(CONTEXT0_END_LO) < at(CONTEXT0_BASE_LO));
        assert_eq!(log.last(), Some(&(MX_L1_TLB_CNTL, 0x19)));
        let value = |o: usize| log.iter().rev().find(|(a, _)| *a == o).unwrap().1;
        assert_eq!(value(CONTEXT0_CNTL), 0);
        assert_eq!(value(CONTEXT0_START_LO), 0);
        assert_eq!(value(CONTEXT0_END_LO), 0x3ffff);
        // The table's physical address with VALID.
        assert_eq!(value(CONTEXT0_BASE_HI), 0x18);
        assert_eq!(value(CONTEXT0_BASE_LO), 0x4900_0001);
    }

    #[test]
    fn the_valid_bit_is_waited_for_and_its_absence_is_an_error() {
        let m = model(3);
        assert_eq!(program_context0(&m, 0x1000, 10, &|| {}), Ok(()));
        let m = model(100);
        assert_eq!(
            program_context0(&m, 0x1000, 5, &|| {}),
            Err(Error::NotValid)
        );
        // The buffer is not turned on then.
        assert!(m.regs.borrow().iter().all(|(a, _)| *a != MX_L1_TLB_CNTL));
    }

    #[test]
    fn a_scan_out_entry_is_uncached_unsnooped_system_memory() {
        let e = entry(0x1_2345_6000);
        // VALID, SYSTEM, EXECUTABLE, READABLE, WRITEABLE; not SNOOPED.
        assert_eq!(e & 0x7f, 0x73);
        assert_eq!(e & 0x4, 0);
        assert_eq!((e >> 48) & 7, 3);
        assert_eq!(e & 0x0000_ffff_ffff_f000, 0x1_2345_6000);
        assert_eq!(pages_for(1920 * 1080 * 4), 2025);
        assert_eq!(pages_for(4096), 1);
        assert_eq!(pages_for(4097), 2);
    }
}
