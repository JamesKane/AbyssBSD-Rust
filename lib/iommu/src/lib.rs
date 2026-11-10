// SPDX-License-Identifier: BSD-2-Clause
//! The IOMMU's pure, ISA-neutral logic (docs/IOMMU.md §4.1): everything that
//! does not touch hardware, so it is tested on the host.
//!
//! - [`gas`]: the IOVA allocator, FreeBSD's guest address space
//!   (`iommu_gas`): address-ordered first fit, guard pages, reserved and
//!   identity regions.
//! - The SMMUv3 data formats (Arm IHI 0070), as typed values with encoders
//!   and decoders: [`regs`] (register offsets and fields, ID register
//!   decoding, errata), [`queue`] (queue index and wrap arithmetic),
//!   [`ste`] (stream table entries, level-1 descriptors, table geometry),
//!   [`cd`] (context descriptors), [`cmd`] (commands), [`event`] (event
//!   records).
//! - [`model`]: a host model of an SMMUv3 that consumes the command queue,
//!   walks stream table → context descriptor → page tables, caches what
//!   hardware may cache, and produces events, for the driver's and the
//!   framework's tests (docs/IOMMU.md §7).
//!
//! Behavior references: sys/dev/iommu/iommu_gas.c, sys/arm64/iommu/smmu.c,
//! smmureg.h.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cd;
pub mod cmd;
pub mod event;
pub mod gas;
pub mod model;
pub mod queue;
pub mod regs;
pub mod ste;

use core::fmt;

pub use addr::PhysAddr;

/// The I/O page size: the SMMU's 4 KiB granule.
pub const PAGE: u64 = 4096;

/// An I/O virtual address: a bus address in a translation domain.
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Iova(u64);

impl Iova {
    /// Address 0.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw address.
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// The raw address.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for Iova {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Iova({:#x})", self.0)
    }
}

impl fmt::LowerHex for Iova {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

/// A StreamID: the requester as the SMMU sees it (for PCI, the RID through
/// the IORT's root-complex mapping).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(pub u32);

/// An address space identifier, tagging a domain's TLB entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Asid(pub u16);

/// Bits `[lo, lo + width)` of `v`.
pub(crate) const fn field(v: u64, lo: u32, width: u32) -> u64 {
    (v >> lo) & ((1 << width) - 1)
}

/// Bit `n` of `v`.
pub(crate) const fn bit(v: u64, n: u32) -> bool {
    v & (1 << n) != 0
}

/// `v` placed at bits `[lo, lo + width)`, excess bits dropped.
pub(crate) const fn put(v: u64, lo: u32, width: u32) -> u64 {
    (v & ((1 << width) - 1)) << lo
}

/// The low 32 bits.
#[allow(clippy::cast_possible_truncation, reason = "truncation is the point")]
pub(crate) const fn lo32(v: u64) -> u32 {
    v as u32
}

/// The low 16 bits.
#[allow(clippy::cast_possible_truncation, reason = "truncation is the point")]
pub(crate) const fn lo16(v: u64) -> u16 {
    v as u16
}

/// The low 8 bits.
#[allow(clippy::cast_possible_truncation, reason = "truncation is the point")]
pub(crate) const fn lo8(v: u64) -> u8 {
    v as u8
}

#[cfg(test)]
pub(crate) mod testutil {
    /// A small PRNG (xorshift), seeded per test so runs repeat.
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        pub(crate) fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next() % n }
        }

        pub(crate) fn chance(&mut self, one_in: u64) -> bool {
            self.below(one_in) == 0
        }
    }
}
