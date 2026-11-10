// SPDX-License-Identifier: BSD-2-Clause
//! Context descriptors (Arm IHI 0070 §5.4): one per domain, holding its
//! ASID, its TTB0 and translation controls, and its MAIR.
//!
//! The design's descriptor (docs/IOMMU.md §4.7): AArch64 tables, 4 KiB
//! granule, TTB0 with T0SZ 16 (48-bit input), TTB1 disabled (EPD1), IPS
//! from the SMMU's OAS, a table walk that is write-back inner-shareable on
//! a coherent SMMU (the board port's `67d6727be10`), faults recorded (R)
//! and aborted (A), the ASID private to the SMMU (ASET), and the MAIR of
//! `pt::smmu`.
//!
//! Behavior reference: sys/arm64/iommu/smmu.c (`smmu_init_cd`), smmureg.h.

use crate::regs::{MemAttrs, Oas};
use crate::{Asid, PhysAddr, bit, field, lo8, lo16, put};

/// CD size in 64-bit words.
pub const CD_WORDS: usize = 8;
/// CD size in bytes.
pub const CD_BYTES: u64 = 64;

const V: u32 = 31;
const EPD0: u32 = 14;
const ENDI: u32 = 15;
const EPD1: u32 = 30;
const AFFD: u32 = 35;
const AA64: u32 = 41;
const R: u32 = 45;
const A: u32 = 46;
const ASET: u32 = 47;
const TTB_MASK: u64 = ((1 << 52) - 1) & !0xf;

/// The smallest and largest T0SZ with a 4 KiB granule and 48-bit
/// addresses.
pub const T0SZ_RANGE: core::ops::RangeInclusive<u8> = 16..=39;

/// A stage 1 context descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "descriptor flags")]
pub struct ContextDescriptor {
    /// The domain's ASID.
    pub asid: Asid,
    /// The level-0 (or first) table's address.
    pub ttb0: PhysAddr,
    /// T0SZ: the input range is `[0, 2^(64 - t0sz))`.
    pub t0sz: u8,
    /// IPS: the output address size.
    pub ips: Oas,
    /// How the SMMU reads the translation tables (IR0, OR0, SH0).
    pub walk: MemAttrs,
    /// The memory attributes the leaves' AttrIndx selects.
    pub mair: u64,
    /// R: record faults as events.
    pub record: bool,
    /// A: terminate a faulting transaction with an abort (else it reads
    /// as zero and its writes are ignored).
    pub abort: bool,
    /// ASET: the ASID is not shared with the CPU's broadcast TLB
    /// maintenance.
    pub aset: bool,
    /// AFFD: access flag faults disabled.
    pub affd: bool,
}

/// Why a CD cannot be used: `C_BAD_CD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdError {
    /// V = 0.
    Invalid,
    /// AA64 = 0: AArch32 tables.
    Aarch32,
    /// A granule other than 4 KiB (the TG0 encoding).
    Granule(u8),
    /// T0SZ outside [`T0SZ_RANGE`].
    T0sz(u8),
    /// EPD0 set: TTB0 walks disabled. (Hardware would fault each access
    /// as a translation fault; nothing here sets it.)
    Tt0Disabled,
    /// Big-endian tables.
    BigEndian,
}

impl ContextDescriptor {
    /// The design's descriptor for a domain with tables at `ttb0`.
    #[must_use]
    pub const fn new(asid: Asid, ttb0: PhysAddr, ips: Oas, coherent: bool) -> Self {
        Self {
            asid,
            ttb0,
            t0sz: 16,
            ips,
            walk: MemAttrs::for_coherency(coherent),
            mair: pt::smmu::MAIR,
            record: true,
            abort: true,
            aset: true,
            affd: false,
        }
    }

    /// The number of input address bits.
    #[must_use]
    pub const fn input_bits(&self) -> u32 {
        64 - self.t0sz as u32
    }

    /// Encodes the descriptor. Word 0, which holds V, is the one to write
    /// last.
    #[must_use]
    pub fn encode(&self) -> [u64; CD_WORDS] {
        let b = |f: bool, n: u32| (f as u64) << n;
        let w0 = put(self.t0sz as u64, 0, 6)
            // TG0 = 0b00: 4 KiB.
            | put(self.walk.inner as u64, 8, 2)
            | put(self.walk.outer as u64, 10, 2)
            | put(self.walk.share as u64, 12, 2)
            | 1 << EPD1
            | 1 << V
            | put(self.ips.encoding(), 32, 3)
            | b(self.affd, AFFD)
            | 1 << AA64
            | b(self.record, R)
            | b(self.abort, A)
            | b(self.aset, ASET)
            | (self.asid.0 as u64) << 48;
        [w0, self.ttb0.get() & TTB_MASK, 0, self.mair, 0, 0, 0, 0]
    }

    /// Decodes a descriptor as the SMMU reads it.
    pub fn decode(w: &[u64; CD_WORDS]) -> Result<Self, CdError> {
        let w0 = w[0];
        if !bit(w0, V) {
            return Err(CdError::Invalid);
        }
        if !bit(w0, AA64) {
            return Err(CdError::Aarch32);
        }
        let tg0 = field(w0, 6, 2);
        if tg0 != 0 {
            return Err(CdError::Granule(lo8(tg0)));
        }
        let t0sz = lo8(field(w0, 0, 6));
        if t0sz < *T0SZ_RANGE.start() || t0sz > *T0SZ_RANGE.end() {
            return Err(CdError::T0sz(t0sz));
        }
        if bit(w0, EPD0) {
            return Err(CdError::Tt0Disabled);
        }
        if bit(w0, ENDI) {
            return Err(CdError::BigEndian);
        }
        Ok(Self {
            asid: Asid(lo16(w0 >> 48)),
            ttb0: PhysAddr::new(w[1] & TTB_MASK),
            t0sz,
            ips: Oas::from_bits(field(w0, 32, 3)),
            walk: MemAttrs {
                inner: crate::regs::Cacheability::from_bits(field(w0, 8, 2)),
                outer: crate::regs::Cacheability::from_bits(field(w0, 10, 2)),
                share: crate::regs::Shareability::from_bits(field(w0, 12, 2)),
            },
            mair: w[3],
            record: bit(w0, R),
            abort: bit(w0, A),
            aset: bit(w0, ASET),
            affd: bit(w0, AFFD),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_designs_descriptor() {
        let cd = ContextDescriptor::new(
            Asid(0x1234),
            PhysAddr::new(0x80_1234_5000),
            Oas::Bits40,
            true,
        );
        let w = cd.encode();
        // Against IHI 0070 §5.4 / smmureg.h: T0SZ 16, TG0 4 KiB, IR0/OR0
        // WB-RWA (1), SH0 inner (3), EPD1, V, IPS 40-bit (2), AA64, R, A,
        // ASET, ASID.
        assert_eq!(
            w[0],
            16 | (1 << 8)
                | (1 << 10)
                | (3 << 12)
                | (1 << 30)
                | (1 << 31)
                | (2 << 32)
                | (1 << 41)
                | (1 << 45)
                | (1 << 46)
                | (1 << 47)
                | (0x1234 << 48)
        );
        assert_eq!(w[1], 0x80_1234_5000);
        assert_eq!(w[2], 0);
        assert_eq!(w[3], 0x44_04ff);
        assert_eq!(cd.input_bits(), 48);
        assert_eq!(ContextDescriptor::decode(&w), Ok(cd));

        // Non-coherent: a non-cacheable, outer-shareable walk.
        let nc = ContextDescriptor::new(Asid(1), PhysAddr::new(0x1000), Oas::Bits48, false);
        let w = nc.encode();
        assert_eq!(field(w[0], 8, 6), 2 << 4);
        assert_eq!(ContextDescriptor::decode(&w), Ok(nc));
        let three_level = ContextDescriptor {
            t0sz: 25,
            affd: true,
            record: false,
            abort: false,
            aset: false,
            ..nc
        };
        assert_eq!(
            ContextDescriptor::decode(&three_level.encode()),
            Ok(three_level)
        );
        assert_eq!(three_level.input_bits(), 39);
    }

    #[test]
    fn bad_descriptors() {
        let good =
            ContextDescriptor::new(Asid(7), PhysAddr::new(0x1000), Oas::Bits40, true).encode();
        let with = |f: fn(&mut [u64; 8])| {
            let mut w = good;
            f(&mut w);
            ContextDescriptor::decode(&w)
        };
        assert_eq!(with(|w| w[0] &= !(1 << 31)), Err(CdError::Invalid));
        assert_eq!(with(|w| w[0] &= !(1 << 41)), Err(CdError::Aarch32));
        assert_eq!(with(|w| w[0] |= 1 << 6), Err(CdError::Granule(1)));
        assert_eq!(with(|w| w[0] = (w[0] & !0x3f) | 15), Err(CdError::T0sz(15)));
        assert_eq!(with(|w| w[0] = (w[0] & !0x3f) | 40), Err(CdError::T0sz(40)));
        assert_eq!(with(|w| w[0] |= 1 << 14), Err(CdError::Tt0Disabled));
        assert_eq!(with(|w| w[0] |= 1 << 15), Err(CdError::BigEndian));
        assert_eq!(ContextDescriptor::decode(&[0; 8]), Err(CdError::Invalid));
    }
}
