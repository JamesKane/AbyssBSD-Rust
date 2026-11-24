// SPDX-License-Identifier: MIT
// Copyright Advanced Micro Devices, Inc.
// Copyright (c) 2026 The AbyssBSD Project
//! The registers read before discovery has run, and the way VRAM is read
//! through them (specification 01, sections 3 and 4.1).

use crate::discovery;

/// The register window (BAR5) a driver reads and writes, as byte offsets.
pub trait Window {
    /// Reads the 32-bit register at byte `offset`.
    fn read32(&self, offset: usize) -> u32;
    /// Writes it.
    fn write32(&self, offset: usize, value: u32);
}

/// `MM_INDEX` (dword 0): low 31 bits of a VRAM byte offset, bit 31 set.
pub const MM_INDEX: usize = 0x00;
/// `MM_DATA` (dword 1).
pub const MM_DATA: usize = 0x04;
/// `MM_INDEX_HI` (dword 6): the VRAM offset shifted right by 31.
pub const MM_INDEX_HI: usize = 0x18;
/// `PCIE_INDEX2` (NBIF segment 0, dword 0x0e).
pub const PCIE_INDEX2: usize = 0x38;
/// `PCIE_DATA2` (NBIF segment 0, dword 0x0f).
pub const PCIE_DATA2: usize = 0x3c;
/// `RCC_CONFIG_MEMSIZE` (dword `0xde3`): VRAM size in MiB; `0xffffffff` if
/// the device does not answer.
pub const RCC_CONFIG_MEMSIZE: usize = 0x378c;
/// `MP0 C2PMSG_33` (dword `0x16061`): bit 31 is set when the platform
/// security processor has finished its boot-time work.
pub const MP0_C2PMSG_33: usize = 0x5_8184;
/// `DRIVER_SCRATCH_0`: low 32 bits of an explicit discovery-table offset.
pub const DRIVER_SCRATCH_0: usize = 0x250;
/// `DRIVER_SCRATCH_1`: its high 32 bits.
pub const DRIVER_SCRATCH_1: usize = 0x254;
/// `DRIVER_SCRATCH_2`: the table's size, or 0.
pub const DRIVER_SCRATCH_2: usize = 0x258;

/// Where the discovery table is in VRAM when the memory size is known: the
/// last 64 KiB. `None` for a size of 0 (the table is in system memory) or
/// `0xffffffff` (the device does not answer), or one that does not fit.
#[must_use]
pub fn table_offset(memsize_mib: u32) -> Option<u64> {
    if memsize_mib == 0 || memsize_mib == u32::MAX {
        return None;
    }
    // `memsize` MiB, less 64 KiB: always positive here.
    Some(u64::from(memsize_mib) * 1024 * 1024 - 64 * 1024)
}

/// Reads one VRAM dword through the index registers. The two index
/// registers are put back as they were found. The caller holds a lock that
/// keeps anyone else from using the registers meanwhile.
///
/// `offset` must be a multiple of 4.
pub fn read_vram<W: Window + ?Sized>(w: &W, offset: u64) -> u32 {
    let (old_lo, old_hi) = (w.read32(MM_INDEX), w.read32(MM_INDEX_HI));
    w.write32(MM_INDEX, (offset & 0x7fff_ffff) as u32 | 1 << 31);
    w.write32(MM_INDEX_HI, (offset >> 31) as u32);
    let value = w.read32(MM_DATA);
    w.write32(MM_INDEX, old_lo);
    w.write32(MM_INDEX_HI, old_hi);
    value
}

/// Why the discovery table could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The memory size register read all ones: the device is not answering.
    NotResponding,
    /// The table is not in VRAM at the usual place (size 0, so it is in
    /// system memory, or the signature is not there).
    NotInVram,
    /// The table's header does not parse.
    Table(discovery::Error),
}

/// The most the table can be: 10 KiB.
const MAX: usize = discovery::REGION_SIZE;

/// Reads and parses the discovery table from VRAM through the index
/// registers (01, section 4.1 steps 2 and 3): the memory size, the
/// signature at the usual offset, then the header's `binary_size`, then
/// exactly that many bytes.
///
/// # Errors
///
/// [`ReadError`].
pub fn read_discovery<W: Window + ?Sized>(w: &W) -> Result<discovery::Discovery, ReadError> {
    let size = w.read32(RCC_CONFIG_MEMSIZE);
    if size == u32::MAX {
        return Err(ReadError::NotResponding);
    }
    let base = table_offset(size).ok_or(ReadError::NotInVram)?;
    if read_vram(w, base) != discovery::SIGNATURE {
        return Err(ReadError::NotInVram);
    }
    let mut head = [0u8; 12];
    for i in 0..3u64 {
        let at = usize::try_from(4 * i).unwrap_or(0);
        head[at..at + 4].copy_from_slice(&read_vram(w, base + 4 * i).to_le_bytes());
    }
    let total = discovery::binary_size(&head).map_err(ReadError::Table)?;
    let words = total.div_ceil(4);
    let mut bytes = alloc::vec::Vec::with_capacity(words * 4);
    for i in 0..words {
        bytes.extend(read_vram(w, base + 4 * i as u64).to_le_bytes());
    }
    bytes.truncate(MAX);
    discovery::parse(&bytes[..total]).map_err(ReadError::Table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// A window over a VRAM image behind the index registers.
    struct Model {
        vram: Vec<u8>,
        base: u64,
        memsize: u32,
        index: RefCell<u32>,
        index_hi: RefCell<u32>,
        writes: RefCell<Vec<(usize, u32)>>,
    }

    impl Window for Model {
        fn read32(&self, offset: usize) -> u32 {
            match offset {
                RCC_CONFIG_MEMSIZE => self.memsize,
                MM_INDEX => *self.index.borrow(),
                MM_INDEX_HI => *self.index_hi.borrow(),
                MM_DATA => {
                    let i = *self.index.borrow();
                    assert!(i >> 31 == 1, "data read without the index's bit 31");
                    let at =
                        (u64::from(*self.index_hi.borrow()) << 31) | u64::from(i & 0x7fff_ffff);
                    let at = at
                        .checked_sub(self.base)
                        .map_or(usize::MAX, |a| usize::try_from(a).unwrap_or(usize::MAX));
                    self.vram
                        .get(at..at + 4)
                        .map_or(0, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                }
                _ => 0,
            }
        }
        fn write32(&self, offset: usize, value: u32) {
            self.writes.borrow_mut().push((offset, value));
            match offset {
                MM_INDEX => *self.index.borrow_mut() = value,
                MM_INDEX_HI => *self.index_hi.borrow_mut() = value,
                _ => {}
            }
        }
    }

    fn model(table: &[u8], memsize: u32) -> Model {
        Model {
            vram: table.to_vec(),
            base: table_offset(memsize).unwrap_or(0),
            memsize,
            index: RefCell::new(0x1234),
            index_hi: RefCell::new(7),
            writes: RefCell::new(Vec::new()),
        }
    }

    const FIRST: &[u8] = include_bytes!("../../../tests/corpus/x870e/linux/amdgpu/discovery.bin");

    #[test]
    fn the_table_sits_in_the_last_64_kib() {
        assert_eq!(table_offset(512), Some(0x1fff_0000));
        assert_eq!(table_offset(12272), Some(0x2_feff_0000));
        assert_eq!(table_offset(0), None);
        assert_eq!(table_offset(u32::MAX), None);
    }

    #[test]
    fn reads_the_raphael_table_through_the_index_registers() {
        let m = model(FIRST, 512);
        let d = read_discovery(&m).unwrap();
        assert_eq!(d.dies.len(), 1);
        // The index registers are as they were.
        assert_eq!((*m.index.borrow(), *m.index_hi.borrow()), (0x1234, 7));
        // Every write was to an index register.
        assert!(
            m.writes
                .borrow()
                .iter()
                .all(|&(o, _)| o == MM_INDEX || o == MM_INDEX_HI)
        );
        // Only the table was read: its size plus the header and signature,
        // not the 10 KiB region.
        let reads = m.writes.borrow().len() / 4;
        assert!(reads <= 1032 / 4 + 4, "{reads} reads");
    }

    #[test]
    fn a_device_that_does_not_answer_or_has_no_table_is_an_error() {
        assert_eq!(
            read_discovery(&model(FIRST, u32::MAX)).unwrap_err(),
            ReadError::NotResponding
        );
        assert_eq!(
            read_discovery(&model(FIRST, 0)).unwrap_err(),
            ReadError::NotInVram
        );
        // Memory of a size where the signature is not at the usual place.
        assert_eq!(
            read_discovery(&model(&[0; 64], 512)).unwrap_err(),
            ReadError::NotInVram
        );
    }

    #[test]
    fn a_hostile_binary_size_is_refused_before_it_is_read() {
        let mut t = FIRST.to_vec();
        // binary_size of 0xffff.
        t[0x0a] = 0xff;
        t[0x0b] = 0xff;
        let m = model(&t, 512);
        assert!(matches!(read_discovery(&m), Err(ReadError::Table(_))));
        assert!(m.writes.borrow().len() < 100, "read nothing like 64 KiB");
    }
}
