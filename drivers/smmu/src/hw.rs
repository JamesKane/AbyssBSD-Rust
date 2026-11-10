// SPDX-License-Identifier: BSD-2-Clause
//! What [`crate::unit::Unit`] programs the SMMU through: its register
//! frame, memory for its queues and tables, the barriers that order the
//! two, and a clock for its timeouts.
//!
//! The kernel's is [`crate::Kernel`] (the granted registers, `DmaCoherent`
//! memory from the unit's own direct tag, `hal`'s barriers); the host
//! tests' runs [`iommu::model::Model`] (docs/IOMMU.md §7).

/// The SMMU's registers and the memory it reads and writes.
pub trait Hw {
    /// A block of memory for the SMMU's structures.
    type Mem;

    /// Reads the 32-bit register at `off` (page 1's at `0x10000 + off`).
    fn read32(&self, off: u32) -> u32;
    /// Writes the 32-bit register at `off`.
    fn write32(&self, off: u32, v: u32);

    /// Writes a 64-bit register as two 32-bit halves, low first (IHI 0070
    /// §6.2 allows 32-bit accesses to either half).
    fn write64(&self, off: u32, v: u64) {
        let [a, b, c, d, e, f, g, h] = v.to_le_bytes();
        self.write32(off, u32::from_le_bytes([a, b, c, d]));
        self.write32(off + 4, u32::from_le_bytes([e, f, g, h]));
    }

    /// `bytes` of zeroed memory aligned to `align` (a power of two), the
    /// SMMU's to read and write; `None` if there is none.
    fn alloc(&self, bytes: u64, align: u64) -> Option<Self::Mem>;
    /// The physical address of `m`.
    fn pa(&self, m: &Self::Mem) -> u64;
    /// The little-endian 64-bit word `word` of `m`.
    fn load(&self, m: &Self::Mem, word: usize) -> u64;
    /// Stores the 64-bit word `word` of `m`, little-endian.
    fn store(&self, m: &Self::Mem, word: usize, v: u64);
    /// The 32-bit word at byte offset `off` of `m` (a sync's MSI slot).
    fn load32(&self, m: &Self::Mem, off: usize) -> u32;

    /// Orders earlier stores to memory before later ones, as the SMMU
    /// sees them: an STE's other words before the word holding V.
    fn write_barrier(&self);
    /// Completes earlier stores to memory before a later register write:
    /// commands before the PROD that publishes them (the board port's
    /// `1b89d7594db`).
    fn doorbell_barrier(&self);
    /// Orders a register read before later memory reads: PROD before the
    /// event records it covers.
    fn read_barrier(&self);

    /// A monotonic clock, in nanoseconds.
    fn now_ns(&self) -> u64;
    /// One turn of a spin wait.
    fn relax(&self);
}
