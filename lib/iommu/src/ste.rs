// SPDX-License-Identifier: BSD-2-Clause
//! Stream table entries, level-1 stream table descriptors, and where a
//! StreamID's STE lives (Arm IHI 0070 §5.1, §5.2, §3.3).
//!
//! The configurations the design uses (docs/IOMMU.md §4.7, §4.9):
//! invalid (the SMMU raises `C_BAD_STE`), abort (traffic terminated
//! silently), bypass (RMR streams and pass-through contexts), and stage 1
//! through a single context descriptor, stage 2 bypassed. Substreams,
//! stage 2, and ATS are not used: EATS is 0 (FreeBSD sets full ATS; the
//! design keeps ATS off, §9), S1CDMax 0.
//!
//! Behavior reference: sys/arm64/iommu/smmu.c (`smmu_init_ste_bypass`,
//! `smmu_init_ste_s1`, `smmu_init_l1_entry`), smmureg.h.

use crate::regs::MemAttrs;
use crate::{PhysAddr, bit, field, lo8, put};

/// STE size in 64-bit words.
pub const STE_WORDS: usize = 8;
/// STE size in bytes.
pub const STE_BYTES: u64 = 64;

/// STE.Config values.
mod config {
    pub(super) const ABORT: u64 = 0b000;
    pub(super) const BYPASS: u64 = 0b100;
    pub(super) const S1_TRANS: u64 = 0b101;
    pub(super) const S2_TRANS: u64 = 0b110;
    pub(super) const ALL_TRANS: u64 = 0b111;
}

/// The S1 context pointer's bits (51:6).
const CTX_PTR_MASK: u64 = ((1 << 52) - 1) & !0x3f;

/// A stage 1 configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stage1 {
    /// The context descriptor's address, 64-byte aligned.
    pub cd: PhysAddr,
    /// How the SMMU fetches the CD (S1CIR, S1COR, S1CSH).
    pub cd_attrs: MemAttrs,
    /// S1STALLD: stalls disabled (set when the SMMU could stall but the
    /// design does not use stalls).
    pub stall_disable: bool,
}

/// A stream table entry, as the design uses them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ste {
    /// V = 0: traffic raises `C_BAD_STE` and is terminated.
    Invalid,
    /// Config abort: traffic is terminated without an event.
    Abort,
    /// Config bypass: addresses pass through untranslated, with the
    /// incoming attributes (SHCFG "use incoming").
    Bypass,
    /// Stage 1 translation through one context descriptor.
    Stage1(Stage1),
}

/// Why an STE cannot be used: `C_BAD_STE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteError {
    /// A reserved Config value.
    ReservedConfig(u8),
    /// Stage 2 translation (not implemented by this SMMU or design).
    Stage2,
    /// Substreams (S1CDMax not 0), or a CD table format other than linear.
    Substreams,
}

impl Ste {
    /// Encodes the entry. Word 0, which holds V, is the one to write last
    /// (IHI 0070 §3.21.3: the SMMU may read the words in any order).
    #[must_use]
    pub fn encode(&self) -> [u64; STE_WORDS] {
        let mut w = [0; STE_WORDS];
        match *self {
            Self::Invalid => {}
            Self::Abort => w[0] = 1 | config::ABORT << 1,
            Self::Bypass => {
                w[0] = 1 | config::BYPASS << 1;
                // SHCFG = 0b01: use the incoming shareability.
                w[1] = 1 << 44;
            }
            Self::Stage1(s) => {
                w[0] = 1 | config::S1_TRANS << 1 | (s.cd.get() & CTX_PTR_MASK);
                w[1] = put(s.cd_attrs.inner as u64, 2, 2)
                    | put(s.cd_attrs.outer as u64, 4, 2)
                    | put(s.cd_attrs.share as u64, 6, 2)
                    | (s.stall_disable as u64) << 27;
            }
        }
        w
    }

    /// Decodes an entry as the SMMU reads it.
    pub fn decode(w: &[u64; STE_WORDS]) -> Result<Self, SteError> {
        let w0 = w[0];
        if !bit(w0, 0) {
            return Ok(Self::Invalid);
        }
        let cfg = field(w0, 1, 3);
        match cfg {
            config::ABORT => Ok(Self::Abort),
            config::BYPASS => Ok(Self::Bypass),
            config::S1_TRANS => {
                // S1Fmt (bits 5:4) and S1CDMax (63:59).
                if field(w0, 4, 2) != 0 || field(w0, 59, 5) != 0 {
                    return Err(SteError::Substreams);
                }
                let w1 = w[1];
                Ok(Self::Stage1(Stage1 {
                    cd: PhysAddr::new(w0 & CTX_PTR_MASK),
                    cd_attrs: MemAttrs {
                        inner: crate::regs::Cacheability::from_bits(field(w1, 2, 2)),
                        outer: crate::regs::Cacheability::from_bits(field(w1, 4, 2)),
                        share: crate::regs::Shareability::from_bits(field(w1, 6, 2)),
                    },
                    stall_disable: bit(w1, 27),
                }))
            }
            config::S2_TRANS | config::ALL_TRANS => Err(SteError::Stage2),
            x => Err(SteError::ReservedConfig(lo8(x))),
        }
    }
}

/// A level-1 stream table descriptor: a level-2 table of `2^(span - 1)`
/// STEs, or none (span 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1Desc {
    /// The level-2 table, aligned to its size.
    pub l2: PhysAddr,
    /// Span: 0 for no table, else 1 + log2 of its STE count (at most 12).
    pub span: u8,
}

impl L1Desc {
    /// No level-2 table: StreamIDs here are `C_BAD_STREAMID`.
    pub const INVALID: Self = Self {
        l2: PhysAddr::ZERO,
        span: 0,
    };
    const ADDR_MASK: u64 = ((1 << 52) - 1) & !0x3f;

    /// A descriptor for a level-2 table of `2^split` STEs.
    #[must_use]
    pub const fn new(l2: PhysAddr, split: u8) -> Self {
        Self {
            l2,
            span: split + 1,
        }
    }

    /// Encodes the descriptor.
    #[must_use]
    pub fn encode(&self) -> u64 {
        (self.l2.get() & Self::ADDR_MASK) | put(self.span as u64, 0, 5)
    }

    /// Decodes a descriptor.
    #[must_use]
    pub fn decode(v: u64) -> Self {
        Self {
            l2: PhysAddr::new(v & Self::ADDR_MASK),
            span: lo8(field(v, 0, 5)),
        }
    }

    /// The number of STEs the level-2 table holds.
    #[must_use]
    pub const fn ste_count(&self) -> u32 {
        if self.span == 0 || self.span > 12 {
            0
        } else {
            1 << (self.span - 1)
        }
    }
}

/// Where a StreamID's STE is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteSlot {
    /// In a linear table: byte offset from the base.
    Linear(u64),
    /// In a two-level table: the level-1 descriptor's byte offset from the
    /// base, and the STE's byte offset in the level-2 table.
    TwoLevel {
        /// Offset of the level-1 descriptor.
        l1: u64,
        /// Offset of the STE in its level-2 table.
        l2: u64,
        /// Index of the STE in its level-2 table.
        index: u32,
    },
}

/// The stream table's shape, as STRTAB_BASE_CFG describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Geometry {
    /// `2^log2size` STEs in one table.
    Linear {
        /// log2 of the STE count.
        log2size: u8,
    },
    /// `2^log2size` StreamIDs, `2^split` per level-2 table.
    TwoLevel {
        /// log2 of the StreamIDs covered.
        log2size: u8,
        /// log2 of the STEs per level-2 table.
        split: u8,
    },
}

impl Geometry {
    /// FreeBSD's split (`STRTAB_SPLIT`): 256 STEs, 16 KiB, per level-2
    /// table.
    pub const SPLIT: u8 = 8;

    /// The form for `sid_bits` StreamID bits: linear when the table is at
    /// most `max_linear_log2` STEs, else two-level with [`Self::SPLIT`]
    /// (docs/IOMMU.md §4.7). `two_level` is whether the SMMU supports it.
    #[must_use]
    pub const fn choose(sid_bits: u8, two_level: bool, max_linear_log2: u8) -> Self {
        if !two_level || sid_bits <= max_linear_log2 {
            Self::Linear { log2size: sid_bits }
        } else {
            Self::TwoLevel {
                log2size: sid_bits,
                split: Self::SPLIT,
            }
        }
    }

    /// The STRTAB_BASE_CFG value.
    #[must_use]
    pub const fn cfg(&self) -> crate::regs::StrtabCfg {
        match *self {
            Self::Linear { log2size } => crate::regs::StrtabCfg::Linear { log2size },
            Self::TwoLevel { log2size, split } => {
                crate::regs::StrtabCfg::TwoLevel { log2size, split }
            }
        }
    }

    /// The bytes of the table STRTAB_BASE points to.
    #[must_use]
    pub const fn base_bytes(&self) -> u64 {
        match *self {
            Self::Linear { log2size } => STE_BYTES << log2size,
            Self::TwoLevel { log2size, split } => 8 << log2size.saturating_sub(split),
        }
    }

    /// Where `sid`'s STE is, or `None` past the table.
    #[must_use]
    pub const fn slot(&self, sid: u32) -> Option<SteSlot> {
        let sid = sid as u64;
        match *self {
            Self::Linear { log2size } => {
                if log2size < 32 && sid >> log2size != 0 {
                    return None;
                }
                Some(SteSlot::Linear(sid * STE_BYTES))
            }
            Self::TwoLevel { log2size, split } => {
                if log2size < 32 && sid >> log2size != 0 {
                    return None;
                }
                let index = sid & ((1 << split) - 1);
                Some(SteSlot::TwoLevel {
                    l1: (sid >> split) * 8,
                    l2: index * STE_BYTES,
                    index: crate::lo32(index),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regs::{Cacheability, Shareability, StrtabCfg};

    fn s1() -> Ste {
        Ste::Stage1(Stage1 {
            cd: PhysAddr::new(0x8_0012_3440),
            cd_attrs: MemAttrs::COHERENT,
            stall_disable: true,
        })
    }

    #[test]
    fn encodings() {
        assert_eq!(Ste::Invalid.encode(), [0; 8]);
        assert_eq!(Ste::Abort.encode()[0], 0b0001);
        // Config bypass 0b100 at bits 3:1; SHCFG incoming at 45:44.
        let b = Ste::Bypass.encode();
        assert_eq!((b[0], b[1]), (0b1001, 1 << 44));
        // Stage 1: V, Config 0b101, S1ContextPtr; S1CIR/S1COR WB-RA (1),
        // S1CSH inner (3), S1STALLD.
        let w = s1().encode();
        assert_eq!(w[0], 0x8_0012_3440 | 0b1011);
        assert_eq!(w[1], (1 << 2) | (1 << 4) | (3 << 6) | (1 << 27));
        assert_eq!(w[2..], [0; 6]);
        // EATS (bits 29:28 of word 1) and S1CDMax stay 0.
        assert_eq!(w[1] & (3 << 28), 0);
        assert_eq!(w[0] >> 59, 0);
    }

    #[test]
    fn round_trips_and_errors() {
        let nc = Ste::Stage1(Stage1 {
            cd: PhysAddr::new(0x40),
            cd_attrs: MemAttrs {
                inner: Cacheability::NonCacheable,
                outer: Cacheability::WriteThrough,
                share: Shareability::Outer,
            },
            stall_disable: false,
        });
        for s in [Ste::Invalid, Ste::Abort, Ste::Bypass, s1(), nc] {
            assert_eq!(Ste::decode(&s.encode()), Ok(s));
        }
        // V = 0 is invalid whatever else is set.
        let mut w = s1().encode();
        w[0] &= !1;
        assert_eq!(Ste::decode(&w), Ok(Ste::Invalid));
        for (cfg, err) in [
            (0b001, SteError::ReservedConfig(1)),
            (0b010, SteError::ReservedConfig(2)),
            (0b011, SteError::ReservedConfig(3)),
            (0b110, SteError::Stage2),
            (0b111, SteError::Stage2),
        ] {
            assert_eq!(Ste::decode(&[1 | cfg << 1, 0, 0, 0, 0, 0, 0, 0]), Err(err));
        }
        let mut w = s1().encode();
        w[0] |= 1 << 59;
        assert_eq!(Ste::decode(&w), Err(SteError::Substreams));
        let mut w = s1().encode();
        w[0] |= 1 << 4;
        assert_eq!(Ste::decode(&w), Err(SteError::Substreams));
    }

    #[test]
    fn level1_descriptors() {
        let d = L1Desc::new(PhysAddr::new(0x8_0000_4000), 8);
        assert_eq!(d.encode(), 0x8_0000_4000 | 9);
        assert_eq!(L1Desc::decode(d.encode()), d);
        assert_eq!(d.ste_count(), 256);
        assert_eq!(L1Desc::INVALID.encode(), 0);
        assert_eq!(L1Desc::INVALID.ste_count(), 0);
        assert_eq!(L1Desc::decode(13).ste_count(), 0);
        assert_eq!(L1Desc::decode(1).ste_count(), 1);
    }

    #[test]
    fn geometry() {
        // The board: 32 SID bits, two levels (docs/IOMMU.md §3.2).
        let g = Geometry::choose(32, true, 16);
        assert_eq!(
            g,
            Geometry::TwoLevel {
                log2size: 32,
                split: 8
            }
        );
        assert_eq!(
            g.cfg(),
            StrtabCfg::TwoLevel {
                log2size: 32,
                split: 8
            }
        );
        assert_eq!(g.base_bytes(), 8 << 24);
        assert_eq!(
            g.slot(0xc100),
            Some(SteSlot::TwoLevel {
                l1: 0xc1 * 8,
                l2: 0,
                index: 0
            })
        );
        assert_eq!(
            g.slot(0xffff_ffff),
            Some(SteSlot::TwoLevel {
                l1: 0xff_ffff * 8,
                l2: 255 * 64,
                index: 255
            })
        );
        // QEMU: 16 SID bits fit a linear table.
        let g = Geometry::choose(16, true, 16);
        assert_eq!(g, Geometry::Linear { log2size: 16 });
        assert_eq!(g.base_bytes(), 64 << 16);
        assert_eq!(g.slot(0xffff), Some(SteSlot::Linear(0xffff * 64)));
        assert_eq!(g.slot(0x1_0000), None);
        // No two-level support: linear whatever the size.
        assert_eq!(
            Geometry::choose(20, false, 16),
            Geometry::Linear { log2size: 20 }
        );
        let g = Geometry::TwoLevel {
            log2size: 12,
            split: 6,
        };
        assert_eq!(g.slot(1 << 12), None);
        assert_eq!(g.base_bytes(), 8 << 6);
    }
}
