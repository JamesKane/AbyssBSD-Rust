// SPDX-License-Identifier: BSD-2-Clause
//! SMMUv3 registers (Arm IHI 0070 §6): offsets, the ID registers decoded
//! into [`Features`], the errata FreeBSD applies, and the fields of the
//! control, base, and error registers the driver programs.
//!
//! The event queue's PROD and CONS live in page 1 (offset 0x10000), as the
//! architecture places them; everything else is in page 0.
//!
//! Behavior reference: sys/arm64/iommu/smmureg.h, smmu.c
//! (`smmu_check_features`, `smmu_check_errata`).

use crate::{PhysAddr, bit, field, lo8, lo16, lo32, put};

/// Register offsets.
pub mod off {
    pub const IDR0: u32 = 0x000;
    pub const IDR1: u32 = 0x004;
    pub const IDR2: u32 = 0x008;
    pub const IDR3: u32 = 0x00c;
    pub const IDR4: u32 = 0x010;
    pub const IDR5: u32 = 0x014;
    pub const IIDR: u32 = 0x018;
    pub const AIDR: u32 = 0x01c;
    pub const CR0: u32 = 0x020;
    pub const CR0ACK: u32 = 0x024;
    pub const CR1: u32 = 0x028;
    pub const CR2: u32 = 0x02c;
    pub const STATUSR: u32 = 0x040;
    pub const GBPA: u32 = 0x044;
    pub const AGBPA: u32 = 0x048;
    pub const IRQ_CTRL: u32 = 0x050;
    pub const IRQ_CTRLACK: u32 = 0x054;
    pub const GERROR: u32 = 0x060;
    pub const GERRORN: u32 = 0x064;
    /// 64-bit.
    pub const GERROR_IRQ_CFG0: u32 = 0x068;
    pub const GERROR_IRQ_CFG1: u32 = 0x070;
    pub const GERROR_IRQ_CFG2: u32 = 0x074;
    /// 64-bit.
    pub const STRTAB_BASE: u32 = 0x080;
    pub const STRTAB_BASE_CFG: u32 = 0x088;
    /// 64-bit.
    pub const CMDQ_BASE: u32 = 0x090;
    pub const CMDQ_PROD: u32 = 0x098;
    pub const CMDQ_CONS: u32 = 0x09c;
    /// 64-bit.
    pub const EVENTQ_BASE: u32 = 0x0a0;
    /// 64-bit.
    pub const EVENTQ_IRQ_CFG0: u32 = 0x0b0;
    pub const EVENTQ_IRQ_CFG1: u32 = 0x0b8;
    pub const EVENTQ_IRQ_CFG2: u32 = 0x0bc;
    /// 64-bit.
    pub const PRIQ_BASE: u32 = 0x0c0;
    /// Page 1.
    pub const EVENTQ_PROD: u32 = 0x100a8;
    /// Page 1.
    pub const EVENTQ_CONS: u32 = 0x100ac;
    /// Page 1.
    pub const PRIQ_PROD: u32 = 0x100c8;
    /// Page 1.
    pub const PRIQ_CONS: u32 = 0x100cc;
    /// The register frame's size: two 64 KiB pages (FreeBSD's resource
    /// is `0x20000`).
    pub const SIZE: u32 = 0x20000;
}

/// IDR0.ST_LEVEL: the stream-table forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StLevel {
    /// Linear only.
    Linear,
    /// Linear and two-level.
    TwoLevel,
    /// A reserved encoding.
    Reserved(u8),
}

/// IDR0.TTF: the translation table formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ttf {
    /// None (a reserved encoding).
    None,
    /// AArch32 LPAE only.
    Aarch32,
    /// AArch64 only.
    Aarch64,
    /// Both.
    Both,
}

impl Ttf {
    /// Whether AArch64 tables are supported.
    #[must_use]
    pub const fn aarch64(self) -> bool {
        matches!(self, Self::Aarch64 | Self::Both)
    }
}

/// IDR0.HTTU: hardware table updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Httu {
    /// None.
    None,
    /// Access flag.
    Access,
    /// Access flag and dirty state.
    AccessDirty,
    /// A reserved encoding.
    Reserved,
}

/// IDR0.STALL_MODEL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallModel {
    /// Stall and terminate.
    StallAndTerminate,
    /// Terminate only.
    Terminate,
    /// Stall forced.
    Forced,
    /// A reserved encoding.
    Reserved,
}

/// The output (physical) address size: IDR5.OAS, and the CD's IPS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Oas {
    /// 32 bits.
    Bits32 = 0,
    /// 36 bits.
    Bits36 = 1,
    /// 40 bits.
    Bits40 = 2,
    /// 42 bits.
    Bits42 = 3,
    /// 44 bits.
    Bits44 = 4,
    /// 48 bits.
    Bits48 = 5,
    /// 52 bits (SMMUv3.1).
    Bits52 = 6,
}

impl Oas {
    /// Decodes the 3-bit field; reserved encodings read as 48 bits, the
    /// largest a 4 KiB granule reaches without the 52-bit extension.
    #[must_use]
    pub const fn from_bits(v: u64) -> Self {
        match v & 7 {
            0 => Self::Bits32,
            1 => Self::Bits36,
            2 => Self::Bits40,
            3 => Self::Bits42,
            4 => Self::Bits44,
            6 => Self::Bits52,
            _ => Self::Bits48,
        }
    }

    /// The encoding.
    #[must_use]
    pub const fn encoding(self) -> u64 {
        self as u64
    }

    /// The number of address bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        match self {
            Self::Bits32 => 32,
            Self::Bits36 => 36,
            Self::Bits40 => 40,
            Self::Bits42 => 42,
            Self::Bits44 => 44,
            Self::Bits48 => 48,
            Self::Bits52 => 52,
        }
    }
}

/// IDR0, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "a register of flags")]
pub struct Idr0 {
    pub st_level: StLevel,
    pub term_model: bool,
    pub stall_model: StallModel,
    /// TTENDIAN: 0 mixed, 2 little, 3 big.
    pub ttendian: u8,
    pub vatos: bool,
    pub cd2l: bool,
    pub vmid16: bool,
    pub vmw: bool,
    pub pri: bool,
    pub atos: bool,
    pub sev: bool,
    pub msi: bool,
    pub asid16: bool,
    pub ns1ats: bool,
    pub ats: bool,
    pub hyp: bool,
    pub dormhint: bool,
    pub httu: Httu,
    pub btm: bool,
    pub cohacc: bool,
    pub ttf: Ttf,
    pub s1p: bool,
    pub s2p: bool,
}

impl Idr0 {
    /// Decodes the register.
    #[must_use]
    pub fn decode(v: u32) -> Self {
        let v = v as u64;
        Self {
            st_level: match field(v, 27, 2) {
                0 => StLevel::Linear,
                1 => StLevel::TwoLevel,
                x => StLevel::Reserved(lo8(x)),
            },
            term_model: bit(v, 26),
            stall_model: match field(v, 24, 2) {
                0 => StallModel::StallAndTerminate,
                1 => StallModel::Terminate,
                2 => StallModel::Forced,
                _ => StallModel::Reserved,
            },
            ttendian: lo8(field(v, 21, 2)),
            vatos: bit(v, 20),
            cd2l: bit(v, 19),
            vmid16: bit(v, 18),
            vmw: bit(v, 17),
            pri: bit(v, 16),
            atos: bit(v, 15),
            sev: bit(v, 14),
            msi: bit(v, 13),
            asid16: bit(v, 12),
            ns1ats: bit(v, 11),
            ats: bit(v, 10),
            hyp: bit(v, 9),
            dormhint: bit(v, 8),
            httu: match field(v, 6, 2) {
                0 => Httu::None,
                1 => Httu::Access,
                2 => Httu::AccessDirty,
                _ => Httu::Reserved,
            },
            btm: bit(v, 5),
            cohacc: bit(v, 4),
            ttf: match field(v, 2, 2) {
                0 => Ttf::None,
                1 => Ttf::Aarch32,
                2 => Ttf::Aarch64,
                _ => Ttf::Both,
            },
            s1p: bit(v, 1),
            s2p: bit(v, 0),
        }
    }

    /// Encodes the register (for the model and tests).
    #[must_use]
    pub fn encode(&self) -> u32 {
        let b = |f: bool, n: u32| (f as u64) << n;
        let v = put(
            match self.st_level {
                StLevel::Linear => 0,
                StLevel::TwoLevel => 1,
                StLevel::Reserved(x) => x as u64,
            },
            27,
            2,
        ) | b(self.term_model, 26)
            | put(
                match self.stall_model {
                    StallModel::StallAndTerminate => 0,
                    StallModel::Terminate => 1,
                    StallModel::Forced => 2,
                    StallModel::Reserved => 3,
                },
                24,
                2,
            )
            | put(self.ttendian as u64, 21, 2)
            | b(self.vatos, 20)
            | b(self.cd2l, 19)
            | b(self.vmid16, 18)
            | b(self.vmw, 17)
            | b(self.pri, 16)
            | b(self.atos, 15)
            | b(self.sev, 14)
            | b(self.msi, 13)
            | b(self.asid16, 12)
            | b(self.ns1ats, 11)
            | b(self.ats, 10)
            | b(self.hyp, 9)
            | b(self.dormhint, 8)
            | put(
                match self.httu {
                    Httu::None => 0,
                    Httu::Access => 1,
                    Httu::AccessDirty => 2,
                    Httu::Reserved => 3,
                },
                6,
                2,
            )
            | b(self.btm, 5)
            | b(self.cohacc, 4)
            | put(
                match self.ttf {
                    Ttf::None => 0,
                    Ttf::Aarch32 => 1,
                    Ttf::Aarch64 => 2,
                    Ttf::Both => 3,
                },
                2,
                2,
            )
            | b(self.s1p, 1)
            | b(self.s2p, 0);
        lo32(v)
    }
}

/// IDR1, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Idr1 {
    pub tables_preset: bool,
    pub queues_preset: bool,
    pub rel: bool,
    pub attr_types_ovr: bool,
    pub attr_perms_ovr: bool,
    /// log2 of the largest command queue.
    pub cmdqs: u8,
    /// log2 of the largest event queue.
    pub eventqs: u8,
    /// log2 of the largest PRI queue.
    pub priqs: u8,
    /// SubstreamID bits.
    pub ssidsize: u8,
    /// StreamID bits.
    pub sidsize: u8,
}

impl Idr1 {
    /// Decodes the register.
    #[must_use]
    pub fn decode(v: u32) -> Self {
        let v = v as u64;
        Self {
            tables_preset: bit(v, 30),
            queues_preset: bit(v, 29),
            rel: bit(v, 28),
            attr_types_ovr: bit(v, 27),
            attr_perms_ovr: bit(v, 26),
            cmdqs: lo8(field(v, 21, 5)),
            eventqs: lo8(field(v, 16, 5)),
            priqs: lo8(field(v, 11, 5)),
            ssidsize: lo8(field(v, 6, 5)),
            sidsize: lo8(field(v, 0, 6)),
        }
    }

    /// Encodes the register.
    #[must_use]
    pub fn encode(&self) -> u32 {
        let b = |f: bool, n: u32| (f as u64) << n;
        lo32(
            b(self.tables_preset, 30)
                | b(self.queues_preset, 29)
                | b(self.rel, 28)
                | b(self.attr_types_ovr, 27)
                | b(self.attr_perms_ovr, 26)
                | put(self.cmdqs as u64, 21, 5)
                | put(self.eventqs as u64, 16, 5)
                | put(self.priqs as u64, 11, 5)
                | put(self.ssidsize as u64, 6, 5)
                | put(self.sidsize as u64, 0, 6),
        )
    }
}

/// IDR3.RIL: range invalidation (SMMUv3.2).
pub const IDR3_RIL: u32 = 1 << 10;

/// IDR5, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Idr5 {
    pub stall_max: u16,
    /// VAX: 0 for 48-bit virtual addresses, 1 for 52.
    pub vax: u8,
    pub gran64k: bool,
    pub gran16k: bool,
    pub gran4k: bool,
    pub oas: Oas,
}

impl Idr5 {
    /// Decodes the register.
    #[must_use]
    pub fn decode(v: u32) -> Self {
        let v = v as u64;
        Self {
            stall_max: lo16(field(v, 16, 16)),
            vax: lo8(field(v, 10, 2)),
            gran64k: bit(v, 6),
            gran16k: bit(v, 5),
            gran4k: bit(v, 4),
            oas: Oas::from_bits(field(v, 0, 3)),
        }
    }

    /// Encodes the register.
    #[must_use]
    pub fn encode(&self) -> u32 {
        let b = |f: bool, n: u32| (f as u64) << n;
        lo32(
            put(self.stall_max as u64, 16, 16)
                | put(self.vax as u64, 10, 2)
                | b(self.gran64k, 6)
                | b(self.gran16k, 5)
                | b(self.gran4k, 4)
                | self.oas.encoding(),
        )
    }
}

/// IIDR, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Iidr {
    pub product_id: u16,
    pub variant: u8,
    pub revision: u8,
    pub implementer: u16,
}

/// The JEP106 code Arm's IIDR carries.
pub const IMPLEMENTER_ARM: u16 = 0x43b;
/// Arm's CoreLink MMU-600.
pub const PRODUCT_MMU_600: u16 = 0x483;

impl Iidr {
    /// Decodes the register.
    #[must_use]
    pub fn decode(v: u32) -> Self {
        let v = v as u64;
        Self {
            product_id: lo16(field(v, 20, 12)),
            variant: lo8(field(v, 16, 4)),
            revision: lo8(field(v, 12, 4)),
            implementer: lo16(field(v, 0, 12)),
        }
    }

    /// Encodes the register.
    #[must_use]
    pub fn encode(&self) -> u32 {
        lo32(
            put(self.product_id as u64, 20, 12)
                | put(self.variant as u64, 16, 4)
                | put(self.revision as u64, 12, 4)
                | put(self.implementer as u64, 0, 12),
        )
    }
}

/// An erratum that changes what the driver may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Erratum {
    /// Arm erratum 1076982: MMU-600 before r1 loses SEV after `CMD_SYNC`,
    /// so SEV is not used (FreeBSD `smmu_check_errata`).
    Mmu600Sev,
}

/// What the driver learns from the ID registers, with errata applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "feature flags")]
pub struct Features {
    pub idr0: Idr0,
    pub idr1: Idr1,
    pub idr5: Idr5,
    pub iidr: Iidr,
    /// Range invalidation (IDR3.RIL).
    pub range_invalidation: bool,
    /// Two-level stream tables.
    pub two_level: bool,
    /// SEV on sync completion, after errata.
    pub sev: bool,
    /// MSI writes (sync completion to memory).
    pub msi: bool,
    /// The SMMU's accesses are coherent (IDR0.COHACC or the IORT's
    /// override).
    pub coherent: bool,
    /// 16-bit ASIDs.
    pub asid16: bool,
    /// The errata found.
    pub erratum: Option<Erratum>,
}

/// Why the SMMU cannot translate for this design.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// No stage 1 (IDR0.S1P).
    Stage1,
    /// No AArch64 tables (IDR0.TTF).
    Aarch64Tables,
    /// No 4 KiB granule (IDR5.GRAN4K).
    Granule4K,
    /// A stream-table form in a reserved encoding.
    StreamTable,
    /// Queues too small to use (`CMDQS` or `EVENTQS` below 1).
    Queues,
}

impl Features {
    /// Decodes IDR0, IDR1, IDR3, IDR5, and IIDR. `cohacc_override` is the
    /// IORT node's COHACC override flag.
    #[must_use]
    pub fn decode(
        idr0: u32,
        idr1: u32,
        idr3: u32,
        idr5: u32,
        iidr: u32,
        cohacc_override: bool,
    ) -> Self {
        let i0 = Idr0::decode(idr0);
        let ii = Iidr::decode(iidr);
        let erratum = if ii.implementer == IMPLEMENTER_ARM
            && ii.product_id == PRODUCT_MMU_600
            && ii.variant < 1
        {
            Some(Erratum::Mmu600Sev)
        } else {
            None
        };
        Self {
            idr0: i0,
            idr1: Idr1::decode(idr1),
            idr5: Idr5::decode(idr5),
            iidr: ii,
            range_invalidation: idr3 & IDR3_RIL != 0,
            two_level: matches!(i0.st_level, StLevel::TwoLevel),
            sev: i0.sev && erratum.is_none(),
            msi: i0.msi,
            coherent: i0.cohacc || cohacc_override,
            asid16: i0.asid16,
            erratum,
        }
    }

    /// Whether everything the design needs is there: stage 1 with AArch64
    /// tables and a 4 KiB granule, a known stream-table form, and queues.
    pub fn check(&self) -> Result<(), Missing> {
        if !self.idr0.s1p {
            return Err(Missing::Stage1);
        }
        if !self.idr0.ttf.aarch64() {
            return Err(Missing::Aarch64Tables);
        }
        if !self.idr5.gran4k {
            return Err(Missing::Granule4K);
        }
        if matches!(self.idr0.st_level, StLevel::Reserved(_)) {
            return Err(Missing::StreamTable);
        }
        if self.idr1.cmdqs == 0 || self.idr1.eventqs == 0 {
            return Err(Missing::Queues);
        }
        Ok(())
    }

    /// StreamID bits.
    #[must_use]
    pub const fn sid_bits(&self) -> u8 {
        self.idr1.sidsize
    }

    /// ASID bits.
    #[must_use]
    pub const fn asid_bits(&self) -> u8 {
        if self.asid16 { 16 } else { 8 }
    }
}

/// CR0 bits (and CR0ACK's).
pub mod cr0 {
    pub const SMMUEN: u32 = 1 << 0;
    pub const PRIQEN: u32 = 1 << 1;
    pub const EVENTQEN: u32 = 1 << 2;
    pub const CMDQEN: u32 = 1 << 3;
    pub const ATSCHK: u32 = 1 << 4;
}

/// CR2 bits.
pub mod cr2 {
    pub const E2H: u32 = 1 << 0;
    pub const RECINVSID: u32 = 1 << 1;
    pub const PTM: u32 = 1 << 2;
}

/// IRQ_CTRL bits (and IRQ_CTRLACK's).
pub mod irq_ctrl {
    pub const GERROR_IRQEN: u32 = 1 << 0;
    pub const PRIQ_IRQEN: u32 = 1 << 1;
    pub const EVENTQ_IRQEN: u32 = 1 << 2;
}

/// The cacheability of a structure access: the 2-bit encoding shared by
/// CR1, the STE's S1CIR/S1COR, and the CD's IR0/OR0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cacheability {
    /// Non-cacheable.
    NonCacheable = 0,
    /// Write-back, read- and write-allocate.
    WriteBack = 1,
    /// Write-through, read-allocate.
    WriteThrough = 2,
    /// Write-back, read-allocate, no write-allocate.
    WriteBackNoWa = 3,
}

impl Cacheability {
    /// Decodes two bits.
    #[must_use]
    pub const fn from_bits(v: u64) -> Self {
        match v & 3 {
            0 => Self::NonCacheable,
            1 => Self::WriteBack,
            2 => Self::WriteThrough,
            _ => Self::WriteBackNoWa,
        }
    }
}

/// Shareability: the 2-bit encoding of CR1, STE S1CSH, and CD SH0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shareability {
    /// Non-shareable.
    Non = 0,
    /// Reserved; treated as outer by hardware's usual reading.
    Reserved = 1,
    /// Outer shareable.
    Outer = 2,
    /// Inner shareable.
    Inner = 3,
}

impl Shareability {
    /// Decodes two bits.
    #[must_use]
    pub const fn from_bits(v: u64) -> Self {
        match v & 3 {
            0 => Self::Non,
            1 => Self::Reserved,
            2 => Self::Outer,
            _ => Self::Inner,
        }
    }
}

/// How the SMMU accesses one kind of structure in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemAttrs {
    /// Inner cacheability.
    pub inner: Cacheability,
    /// Outer cacheability.
    pub outer: Cacheability,
    /// Shareability.
    pub share: Shareability,
}

impl MemAttrs {
    /// Write-back, inner shareable: for a coherent SMMU (docs/IOMMU.md
    /// §4.7, the board port's `67d6727be10`).
    pub const COHERENT: Self = Self {
        inner: Cacheability::WriteBack,
        outer: Cacheability::WriteBack,
        share: Shareability::Inner,
    };
    /// Non-cacheable, outer shareable: for a non-coherent one.
    pub const NON_COHERENT: Self = Self {
        inner: Cacheability::NonCacheable,
        outer: Cacheability::NonCacheable,
        share: Shareability::Outer,
    };

    /// The attributes for a coherent SMMU or not.
    #[must_use]
    pub const fn for_coherency(coherent: bool) -> Self {
        if coherent {
            Self::COHERENT
        } else {
            Self::NON_COHERENT
        }
    }

    /// `[ic, oc, sh]` packed as `ic | oc << 2 | sh << 4` (the layout CR1
    /// uses for each structure kind).
    #[must_use]
    pub const fn pack(&self) -> u64 {
        self.inner as u64 | (self.outer as u64) << 2 | (self.share as u64) << 4
    }

    /// The inverse of [`MemAttrs::pack`].
    #[must_use]
    pub const fn unpack(v: u64) -> Self {
        Self {
            inner: Cacheability::from_bits(v),
            outer: Cacheability::from_bits(v >> 2),
            share: Shareability::from_bits(v >> 4),
        }
    }
}

/// CR1: how the SMMU accesses queues (bits 5:0) and tables (bits 11:6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cr1 {
    /// Queue accesses.
    pub queue: MemAttrs,
    /// Stream table, CD, and translation table accesses.
    pub table: MemAttrs,
}

impl Cr1 {
    /// Both kinds of access for a coherent SMMU or not.
    #[must_use]
    pub const fn for_coherency(coherent: bool) -> Self {
        let a = MemAttrs::for_coherency(coherent);
        Self { queue: a, table: a }
    }

    /// Encodes the register.
    #[must_use]
    pub fn encode(&self) -> u32 {
        lo32(self.queue.pack() | self.table.pack() << 6)
    }

    /// Decodes the register.
    #[must_use]
    pub fn decode(v: u32) -> Self {
        let v = v as u64;
        Self {
            queue: MemAttrs::unpack(v),
            table: MemAttrs::unpack(v >> 6),
        }
    }
}

/// GBPA: what a disabled SMMU does with incoming traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gbpa(pub u32);

impl Gbpa {
    /// UPDATE: set by software to apply a write, cleared by the SMMU.
    pub const UPDATE: u32 = 1 << 31;
    /// ABORT: terminate incoming traffic rather than bypassing it.
    pub const ABORT: u32 = 1 << 20;

    /// Whether traffic is aborted.
    #[must_use]
    pub const fn aborts(self) -> bool {
        self.0 & Self::ABORT != 0
    }

    /// Whether an update is still pending.
    #[must_use]
    pub const fn updating(self) -> bool {
        self.0 & Self::UPDATE != 0
    }

    /// The value to write to apply these settings.
    #[must_use]
    pub const fn update(self) -> u32 {
        self.0 | Self::UPDATE
    }

    /// The same settings with ABORT set or clear.
    #[must_use]
    pub const fn with_abort(self, abort: bool) -> Self {
        Self(if abort {
            self.0 | Self::ABORT
        } else {
            self.0 & !Self::ABORT
        })
    }
}

/// GERROR / GERRORN bits. A bit is active while GERROR and GERRORN differ;
/// software acknowledges by writing GERRORN equal to GERROR.
pub mod gerror {
    pub const CMDQ_ERR: u32 = 1 << 0;
    pub const EVTQ_ABT_ERR: u32 = 1 << 2;
    pub const PRIQ_ABT_ERR: u32 = 1 << 3;
    pub const MSI_CMDQ_ABT_ERR: u32 = 1 << 4;
    pub const MSI_EVTQ_ABT_ERR: u32 = 1 << 5;
    pub const MSI_PRIQ_ABT_ERR: u32 = 1 << 6;
    pub const MSI_GERROR_ABT_ERR: u32 = 1 << 7;
    pub const SFM_ERR: u32 = 1 << 8;
    /// Every defined bit.
    pub const ALL: u32 = 0x1fd;

    /// The active errors.
    #[must_use]
    pub const fn active(gerror: u32, gerrorn: u32) -> u32 {
        (gerror ^ gerrorn) & ALL
    }

    /// The architecture's name for one bit.
    #[must_use]
    pub const fn name(bit: u32) -> &'static str {
        match bit {
            CMDQ_ERR => "CMDQ_ERR",
            EVTQ_ABT_ERR => "EVTQ_ABT_ERR",
            PRIQ_ABT_ERR => "PRIQ_ABT_ERR",
            MSI_CMDQ_ABT_ERR => "MSI_CMDQ_ABT_ERR",
            MSI_EVTQ_ABT_ERR => "MSI_EVTQ_ABT_ERR",
            MSI_PRIQ_ABT_ERR => "MSI_PRIQ_ABT_ERR",
            MSI_GERROR_ABT_ERR => "MSI_GERROR_ABT_ERR",
            SFM_ERR => "SFM_ERR",
            _ => "unknown",
        }
    }
}

/// CMDQ_CONS.ERR: why the command at CONS was not executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdError {
    /// No error.
    None,
    /// An illegal command (CERROR_ILL).
    Illegal,
    /// The command fetch aborted (CERROR_ABT).
    Abort,
    /// An ATC invalidation sync failed (CERROR_ATC_INV_SYNC).
    AtcInvSync,
    /// Another encoding.
    Other(u8),
}

impl CmdError {
    /// The 7-bit field's value.
    #[must_use]
    pub const fn encoding(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Illegal => 1,
            Self::Abort => 2,
            Self::AtcInvSync => 3,
            Self::Other(x) => x as u32 & 0x7f,
        }
    }

    /// Decodes a CMDQ_CONS value's ERR field (bits 30:24).
    #[must_use]
    pub const fn from_cons(cons: u32) -> Self {
        match (cons >> 24) & 0x7f {
            0 => Self::None,
            1 => Self::Illegal,
            2 => Self::Abort,
            3 => Self::AtcInvSync,
            x => Self::Other(lo8(x as u64)),
        }
    }
}

/// A queue's base register: CMDQ_BASE, EVENTQ_BASE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueBase {
    /// The queue's address, aligned to its size (at least 32 bytes).
    pub addr: PhysAddr,
    /// log2 of the number of entries.
    pub log2size: u8,
    /// RA (command queue) or WA (event queue): an allocation hint.
    pub alloc_hint: bool,
}

impl QueueBase {
    const ADDR_MASK: u64 = ((1 << 52) - 1) & !0x1f;

    /// Encodes the register.
    #[must_use]
    pub fn encode(&self) -> u64 {
        (self.alloc_hint as u64) << 62
            | (self.addr.get() & Self::ADDR_MASK)
            | put(self.log2size as u64, 0, 5)
    }

    /// Decodes the register.
    #[must_use]
    pub fn decode(v: u64) -> Self {
        Self {
            addr: PhysAddr::new(v & Self::ADDR_MASK),
            log2size: lo8(field(v, 0, 5)),
            alloc_hint: bit(v, 62),
        }
    }
}

/// STRTAB_BASE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrtabBase {
    /// The table's address (64-byte aligned at least; a linear table is
    /// aligned to its size).
    pub addr: PhysAddr,
    /// RA: read-allocate hint.
    pub read_alloc: bool,
}

impl StrtabBase {
    const ADDR_MASK: u64 = ((1 << 52) - 1) & !0x3f;

    /// Encodes the register.
    #[must_use]
    pub fn encode(&self) -> u64 {
        (self.read_alloc as u64) << 62 | (self.addr.get() & Self::ADDR_MASK)
    }

    /// Decodes the register.
    #[must_use]
    pub fn decode(v: u64) -> Self {
        Self {
            addr: PhysAddr::new(v & Self::ADDR_MASK),
            read_alloc: bit(v, 62),
        }
    }
}

/// STRTAB_BASE_CFG: the stream table's form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrtabCfg {
    /// One array of `2^log2size` STEs.
    Linear {
        /// log2 of the number of STEs.
        log2size: u8,
    },
    /// Level-1 descriptors, each covering `2^split` STEs.
    TwoLevel {
        /// log2 of the number of StreamIDs covered.
        log2size: u8,
        /// The StreamID bits each level-2 table resolves: 6, 8, or 10.
        split: u8,
    },
    /// A reserved format encoding.
    Reserved(u32),
}

impl StrtabCfg {
    /// Encodes the register.
    #[must_use]
    pub fn encode(&self) -> u32 {
        match *self {
            Self::Linear { log2size } => lo32(put(log2size as u64, 0, 6)),
            Self::TwoLevel { log2size, split } => {
                lo32(1 << 16 | put(split as u64, 6, 5) | put(log2size as u64, 0, 6))
            }
            Self::Reserved(v) => v,
        }
    }

    /// Decodes the register.
    #[must_use]
    pub fn decode(v: u32) -> Self {
        let w = v as u64;
        let log2size = lo8(field(w, 0, 6));
        match field(w, 16, 2) {
            0 => Self::Linear { log2size },
            1 => Self::TwoLevel {
                log2size,
                split: lo8(field(w, 6, 5)),
            },
            _ => Self::Reserved(v),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr0_fields() {
        // Bit by bit against IHI 0070 §6.3.1 (and smmureg.h).
        type Case = (u32, fn(&Idr0) -> bool);
        let cases: [Case; 19] = [
            (1 << 0, |i| i.s2p),
            (1 << 1, |i| i.s1p),
            (2 << 2, |i| i.ttf == Ttf::Aarch64),
            (1 << 4, |i| i.cohacc),
            (1 << 5, |i| i.btm),
            (2 << 6, |i| i.httu == Httu::AccessDirty),
            (1 << 8, |i| i.dormhint),
            (1 << 9, |i| i.hyp),
            (1 << 10, |i| i.ats),
            (1 << 11, |i| i.ns1ats),
            (1 << 12, |i| i.asid16),
            (1 << 13, |i| i.msi),
            (1 << 14, |i| i.sev),
            (1 << 15, |i| i.atos),
            (1 << 16, |i| i.pri),
            (1 << 19, |i| i.cd2l),
            (2 << 24, |i| i.stall_model == StallModel::Forced),
            (1 << 26, |i| i.term_model),
            (1 << 27, |i| i.st_level == StLevel::TwoLevel),
        ];
        let zero = Idr0::decode(0);
        for (v, f) in cases {
            let d = Idr0::decode(v);
            assert!(f(&d), "{v:#x}");
            assert!(!f(&zero) || v == 0, "{v:#x} set at zero");
            assert_eq!(d.encode(), v);
        }
        // Every value round-trips through decode and encode.
        // Bit 23 and bits 31:29 are RES0.
        for v in [0u32, 0x1f7f_ffff, 0x0a5a_5a5a, 0x1234_5678] {
            assert_eq!(Idr0::decode(v).encode(), v);
        }
    }

    #[test]
    fn idr1_idr5_iidr_fields() {
        let i = Idr1::decode((19 << 21) | (19 << 16) | (8 << 11) | (20 << 6) | 32 | (1 << 30));
        assert_eq!(
            (
                i.cmdqs,
                i.eventqs,
                i.priqs,
                i.ssidsize,
                i.sidsize,
                i.tables_preset
            ),
            (19, 19, 8, 20, 32, true)
        );
        for v in [0, 0x7fff_ffff_u32, 0x5555_5555 & 0x7fff_ffff] {
            assert_eq!(Idr1::decode(v).encode(), v);
        }
        let i = Idr5::decode((0xffff << 16) | (1 << 10) | (1 << 4) | 2);
        assert_eq!(
            (i.stall_max, i.vax, i.gran4k, i.gran16k, i.oas),
            (0xffff, 1, true, false, Oas::Bits40)
        );
        assert_eq!(i.oas.bits(), 40);
        for v in [0, 0xffff_0c75_u32, 0x0001_0476] {
            assert_eq!(Idr5::decode(v).encode(), v);
        }
        let ii = Iidr::decode(0x4830_143b);
        assert_eq!(
            ii,
            Iidr {
                product_id: 0x483,
                variant: 0,
                revision: 1,
                implementer: 0x43b
            }
        );
        assert_eq!(ii.encode(), 0x4830_143b);
        for (enc, bits) in [
            (0, 32),
            (1, 36),
            (2, 40),
            (3, 42),
            (4, 44),
            (5, 48),
            (6, 52),
        ] {
            assert_eq!(Oas::from_bits(enc).bits(), bits);
            assert_eq!(Oas::from_bits(enc).encoding(), enc);
        }
        assert_eq!(Oas::from_bits(7), Oas::Bits48);
    }

    #[test]
    fn features_and_errata() {
        let idr0 = Idr0 {
            st_level: StLevel::TwoLevel,
            sev: true,
            msi: true,
            asid16: true,
            ttf: Ttf::Aarch64,
            s1p: true,
            ..Idr0::decode(0)
        }
        .encode();
        let idr1 = (8 << 21) | (8 << 16) | 32;
        let idr5 = (1 << 4) | 2;
        let mmu600_r0 = Iidr {
            product_id: PRODUCT_MMU_600,
            variant: 0,
            revision: 2,
            implementer: IMPLEMENTER_ARM,
        }
        .encode();
        let f = Features::decode(idr0, idr1, 0, idr5, mmu600_r0, true);
        assert_eq!(f.erratum, Some(Erratum::Mmu600Sev));
        assert!(!f.sev && f.msi && f.coherent && f.two_level && f.asid16);
        assert_eq!((f.sid_bits(), f.asid_bits()), (32, 16));
        assert_eq!(f.check(), Ok(()));
        let r1 = Iidr {
            variant: 1,
            ..Iidr::decode(mmu600_r0)
        }
        .encode();
        let f = Features::decode(idr0, idr1, IDR3_RIL, idr5, r1, false);
        assert!(f.sev && f.erratum.is_none() && !f.coherent && f.range_invalidation);

        let no_s1 = idr0 & !(1 << 1);
        assert_eq!(
            Features::decode(no_s1, idr1, 0, idr5, 0, false).check(),
            Err(Missing::Stage1)
        );
        let aa32 = (idr0 & !(3 << 2)) | (1 << 2);
        assert_eq!(
            Features::decode(aa32, idr1, 0, idr5, 0, false).check(),
            Err(Missing::Aarch64Tables)
        );
        assert_eq!(
            Features::decode(idr0, idr1, 0, 2, 0, false).check(),
            Err(Missing::Granule4K)
        );
        assert_eq!(
            Features::decode(idr0 | (3 << 27), idr1, 0, idr5, 0, false).check(),
            Err(Missing::StreamTable)
        );
        assert_eq!(
            Features::decode(idr0, 32, 0, idr5, 0, false).check(),
            Err(Missing::Queues)
        );
    }

    #[test]
    fn control_and_base_registers() {
        // CR1 as FreeBSD writes it for a coherent SMMU: table and queue
        // accesses write-back inner shareable.
        let c = Cr1::for_coherency(true);
        assert_eq!(
            c.encode(),
            (3 << 10) | (1 << 8) | (1 << 6) | (3 << 4) | (1 << 2) | 1
        );
        assert_eq!(Cr1::decode(c.encode()), c);
        let n = Cr1::for_coherency(false);
        assert_eq!(n.encode(), (2 << 10) | (2 << 4));
        assert_eq!(Cr1::decode(n.encode()), n);

        let q = QueueBase {
            addr: PhysAddr::new(0x8_1234_5600),
            log2size: 19,
            alloc_hint: true,
        };
        assert_eq!(q.encode(), (1 << 62) | 0x8_1234_5600 | 19);
        assert_eq!(QueueBase::decode(q.encode()), q);
        let s = StrtabBase {
            addr: PhysAddr::new(0x80_0000_0040),
            read_alloc: true,
        };
        assert_eq!(s.encode(), (1 << 62) | 0x80_0000_0040);
        assert_eq!(StrtabBase::decode(s.encode()), s);

        for c in [
            StrtabCfg::Linear { log2size: 16 },
            StrtabCfg::TwoLevel {
                log2size: 32,
                split: 8,
            },
        ] {
            assert_eq!(StrtabCfg::decode(c.encode()), c);
        }
        assert_eq!(
            StrtabCfg::TwoLevel {
                log2size: 32,
                split: 8
            }
            .encode(),
            (1 << 16) | (8 << 6) | 32
        );
        assert_eq!(StrtabCfg::decode(2 << 16), StrtabCfg::Reserved(2 << 16));

        let g = Gbpa(0).with_abort(true);
        assert!(g.aborts() && !g.updating());
        assert_eq!(g.update(), (1 << 31) | (1 << 20));
        assert!(!g.with_abort(false).aborts());

        assert_eq!(gerror::active(0b101, 0b001), gerror::EVTQ_ABT_ERR);
        assert_eq!(gerror::active(0x1fd, 0), gerror::ALL);
        assert_eq!(gerror::name(gerror::SFM_ERR), "SFM_ERR");
        for e in [
            CmdError::None,
            CmdError::Illegal,
            CmdError::Abort,
            CmdError::AtcInvSync,
            CmdError::Other(0x55),
        ] {
            assert_eq!(CmdError::from_cons(e.encoding() << 24 | 0x1234), e);
        }
    }
}
