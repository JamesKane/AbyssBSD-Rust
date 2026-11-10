// SPDX-License-Identifier: BSD-2-Clause
//! The SMMUv3's identification registers (Arm IHI 0070, §6.3.1–§6.3.6):
//! what an implementation supports, decided once at attach.
//!
//! [`Features::decode`] reads `SMMU_IDR0`, `IDR1`, `IDR3`, and `IDR5` as
//! FreeBSD's `smmu_check_features` does, and says which feature the
//! driver needs is missing: little-endian or mixed-endian table walks,
//! the AArch64 table format, stage 1, and tables and queues at addresses
//! of the driver's choosing (not an embedded implementation's preset or
//! relative ones). [`Iidr`] identifies the implementation for the errata
//! table (`smmu_check_errata`).
//!
//! Behavior reference: sys/arm64/iommu/smmu.c, smmureg.h.

/// Register offsets in page 0.
pub mod reg {
    /// Identification register 0: the features.
    pub const IDR0: usize = 0x000;
    /// Identification register 1: table and queue sizes.
    pub const IDR1: usize = 0x004;
    /// Identification register 2: the VATOS page.
    pub const IDR2: usize = 0x008;
    /// Identification register 3: further features.
    pub const IDR3: usize = 0x00c;
    /// Identification register 4: implementation defined.
    pub const IDR4: usize = 0x010;
    /// Identification register 5: output size and granules.
    pub const IDR5: usize = 0x014;
    /// Implementation identification.
    pub const IIDR: usize = 0x018;
    /// Architecture version.
    pub const AIDR: usize = 0x01c;
    /// Control register 0.
    pub const CR0: usize = 0x020;
    /// Global bypass attributes.
    pub const GBPA: usize = 0x044;
}

/// `CR0.SMMUEN`: translation (and the queues) enabled.
pub const CR0_SMMUEN: u32 = 1;

/// The raw identification registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Idr {
    /// `SMMU_IDR0`.
    pub idr0: u32,
    /// `SMMU_IDR1`.
    pub idr1: u32,
    /// `SMMU_IDR3`.
    pub idr3: u32,
    /// `SMMU_IDR5`.
    pub idr5: u32,
    /// `SMMU_IIDR`.
    pub iidr: u32,
}

/// Table walk endianness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    /// Either, per context.
    Mixed,
    /// Little-endian only.
    Little,
    /// Big-endian only.
    Big,
}

/// How a stalled transaction can be handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stall {
    /// Stall and terminate both supported.
    Supported,
    /// Terminate only.
    None,
    /// Stall is forced.
    Forced,
}

/// A feature the driver needs that the SMMU lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// `IDR0.TTENDIAN` is reserved.
    Endianness,
    /// No AArch64 translation table format (`IDR0.TTF`).
    Aarch64Tables,
    /// No stage 1 translation (`IDR0.S1P`).
    Stage1,
    /// An embedded implementation: preset table or queue addresses, or
    /// relative ones (`IDR1.TABLES_PRESET`, `QUEUES_PRESET`, `REL`).
    Embedded,
    /// Big-endian table walks only: the CPU's tables are little-endian.
    LittleEndian,
}

impl Missing {
    /// FreeBSD's message for it, where it has one.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Endianness => "Unsupported endianness.",
            Self::Aarch64Tables => "No AArch64 table format support.",
            Self::Stage1 => "No stage 1 translation support.",
            Self::Embedded => "Embedded implementations not supported by this driver.",
            Self::LittleEndian => "Big endian supported only.",
        }
    }
}

/// What the SMMU supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one per IDR feature bit, as the specification lists them"
)]
pub struct Features {
    /// Two-level stream tables, and more stream ID bits than one level
    /// of [`STRTAB_SPLIT`] covers (FreeBSD clears the feature otherwise).
    pub two_level_streams: bool,
    /// Two-level context descriptor tables.
    pub two_level_cds: bool,
    /// Table walk endianness.
    pub endian: Endian,
    /// WFE wake-up on queue updates.
    pub sev: bool,
    /// Message-signalled interrupts.
    pub msi: bool,
    /// Hypervisor stage 1 contexts.
    pub hyp: bool,
    /// PCIe ATS.
    pub ats: bool,
    /// The PRI queue.
    pub pri: bool,
    /// Stall handling.
    pub stall: Stall,
    /// Stage 1 translation.
    pub stage1: bool,
    /// Stage 2 translation.
    pub stage2: bool,
    /// Coherent table walks and queue accesses (`IDR0.COHACC`).
    pub coherent: bool,
    /// Hardware access-flag (and dirty) updates: 0 none, 1 A, 2 A and D.
    pub httu: u8,
    /// ASID bits: 8 or 16.
    pub asid_bits: u8,
    /// VMID bits: 8 or 16.
    pub vmid_bits: u8,
    /// log2 of the command queue's largest size, in entries.
    pub cmdq_bits: u8,
    /// log2 of the event queue's largest size.
    pub evtq_bits: u8,
    /// log2 of the PRI queue's largest size.
    pub priq_bits: u8,
    /// Substream ID bits.
    pub ssid_bits: u8,
    /// Stream ID bits.
    pub sid_bits: u8,
    /// Range invalidation (`IDR3.RIL`).
    pub range_invalidation: bool,
    /// Output address size in bits, if the field is not reserved.
    pub oas: Option<u8>,
    /// The translation granules, as a mask of page sizes in bytes.
    pub granules: u32,
    /// 52-bit virtual addresses.
    pub va52: bool,
}

/// FreeBSD's `STRTAB_SPLIT`: the stream ID bits a level-2 stream table
/// span covers.
pub const STRTAB_SPLIT: u8 = 8;

fn bits(v: u32, shift: u32, width: u32) -> u32 {
    (v >> shift) & ((1 << width) - 1)
}

fn field(v: u32, shift: u32, width: u32) -> u8 {
    bits(v, shift, width).to_le_bytes()[0]
}

impl Features {
    /// Decodes the identification registers: the features, or the first
    /// one missing that the driver needs. Errata are applied ([`Iidr`]).
    ///
    /// # Errors
    ///
    /// The missing feature.
    pub fn decode(idr: &Idr) -> Result<Self, Missing> {
        let (i0, i1) = (idr.idr0, idr.idr1);
        let endian = match bits(i0, 21, 2) {
            0 => Endian::Mixed,
            2 => Endian::Little,
            3 => Endian::Big,
            _ => return Err(Missing::Endianness),
        };
        if endian == Endian::Big {
            return Err(Missing::LittleEndian);
        }
        if bits(i0, 2, 2) & 2 == 0 {
            return Err(Missing::Aarch64Tables);
        }
        if i1 & (7 << 28) != 0 {
            return Err(Missing::Embedded);
        }
        let stage1 = i0 & (1 << 1) != 0;
        if !stage1 {
            return Err(Missing::Stage1);
        }
        let sid_bits = field(i1, 0, 6);
        let i5 = idr.idr5;
        let oas = match bits(i5, 0, 3) {
            0 => Some(32),
            1 => Some(36),
            2 => Some(40),
            3 => Some(42),
            4 => Some(44),
            5 => Some(48),
            6 => Some(52),
            _ => None,
        };
        let granules = [(4, 4096), (5, 16_384), (6, 65_536)]
            .iter()
            .filter(|&&(b, _)| i5 & (1 << b) != 0)
            .fold(0, |m, &(_, size)| m | size);
        let pri = i0 & (1 << 16) != 0;
        Ok(Self {
            two_level_streams: bits(i0, 27, 2) == 1 && sid_bits > STRTAB_SPLIT,
            two_level_cds: i0 & (1 << 19) != 0,
            endian,
            sev: i0 & (1 << 14) != 0 && !Iidr(idr.iidr).loses_sev(),
            msi: i0 & (1 << 13) != 0,
            hyp: i0 & (1 << 9) != 0,
            ats: i0 & (1 << 10) != 0,
            pri,
            stall: match bits(i0, 24, 2) {
                0 => Stall::Supported,
                2 => Stall::Forced,
                _ => Stall::None,
            },
            stage1,
            stage2: i0 & 1 != 0,
            coherent: i0 & (1 << 4) != 0,
            httu: field(i0, 6, 2),
            asid_bits: if i0 & (1 << 12) != 0 { 16 } else { 8 },
            vmid_bits: if i0 & (1 << 18) != 0 { 16 } else { 8 },
            cmdq_bits: field(i1, 21, 5),
            evtq_bits: field(i1, 16, 5),
            priq_bits: if pri { field(i1, 11, 5) } else { 0 },
            ssid_bits: field(i1, 6, 5),
            sid_bits,
            range_invalidation: idr.idr3 & (1 << 10) != 0,
            oas,
            granules,
            va52: bits(i5, 10, 2) == 1,
        })
    }
}

/// `SMMU_IIDR`: who made the SMMU, and which revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Iidr(pub u32);

impl Iidr {
    /// Arm's JEP106 code, as `IIDR.Implementer` holds it.
    pub const ARM: u16 = 0x43b;
    /// The MMU-600's product ID.
    pub const MMU_600: u16 = 0x483;

    /// The implementer.
    #[must_use]
    pub fn implementer(self) -> u16 {
        u16::try_from(bits(self.0, 0, 12)).unwrap_or(0)
    }

    /// The product.
    #[must_use]
    pub fn product(self) -> u16 {
        u16::try_from(bits(self.0, 20, 12)).unwrap_or(0)
    }

    /// The variant (major revision).
    #[must_use]
    pub fn variant(self) -> u8 {
        field(self.0, 16, 4)
    }

    /// The revision (minor).
    #[must_use]
    pub fn revision(self) -> u8 {
        field(self.0, 12, 4)
    }

    /// Arm erratum 1076982: an MMU-600 before variant 1 must not rely on
    /// SEV (FreeBSD's `smmu_check_errata`).
    #[must_use]
    pub fn loses_sev(self) -> bool {
        self.implementer() == Self::ARM && self.product() == Self::MMU_600 && self.variant() < 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QEMU 10.2's SMMUv3, as `virt,iommu=smmuv3` reads (the driver's
    /// `bootverbose` line): two-level stream tables, terminate model,
    /// little-endian, AArch64 tables, stages 1 and 2, coherent, ASID16,
    /// VMID16, 16-bit stream IDs; range invalidation; 44-bit output, every
    /// granule.
    const QEMU: Idr = Idr {
        idr0: 0x0d44_101b,
        idr1: 0x0273_0010,
        idr3: 0x1414,
        idr5: 0x74,
        iidr: 0,
    };

    #[test]
    fn decodes_the_features() {
        let f = Features::decode(&QEMU).unwrap();
        assert!(f.two_level_streams && !f.two_level_cds);
        assert_eq!(f.endian, Endian::Little);
        assert!(f.stage1 && f.stage2 && f.coherent && f.range_invalidation);
        assert!(!f.msi && !f.pri && !f.ats && !f.sev && !f.hyp);
        assert_eq!((f.asid_bits, f.vmid_bits), (16, 16));
        assert_eq!((f.cmdq_bits, f.evtq_bits, f.priq_bits), (19, 19, 0));
        assert_eq!((f.ssid_bits, f.sid_bits), (0, 16));
        assert_eq!(f.oas, Some(44));
        assert_eq!(f.granules, 4096 | 16_384 | 65_536);
        assert_eq!(f.stall, Stall::None);
        assert_eq!(f.httu, 0);
        assert!(!f.va52);
    }

    #[test]
    fn a_small_stream_space_is_one_level() {
        // Two levels offered, but 8 stream ID bits fit one level-2 span.
        let idr = Idr { idr1: 8, ..QEMU };
        assert!(!Features::decode(&idr).unwrap().two_level_streams);
    }

    #[test]
    fn missing_features() {
        let without = |bit: u32| Idr {
            idr0: QEMU.idr0 & !bit,
            ..QEMU
        };
        assert_eq!(Features::decode(&without(1 << 1)), Err(Missing::Stage1));
        assert_eq!(
            Features::decode(&without(2 << 2)),
            Err(Missing::Aarch64Tables)
        );
        let reserved = Idr {
            idr0: QEMU.idr0 & !(3 << 21) | 1 << 21,
            ..QEMU
        };
        assert_eq!(Features::decode(&reserved), Err(Missing::Endianness));
        let big = Idr {
            idr0: QEMU.idr0 | 3 << 21,
            ..QEMU
        };
        assert_eq!(Features::decode(&big), Err(Missing::LittleEndian));
        for b in [28, 29, 30] {
            let embedded = Idr {
                idr1: QEMU.idr1 | 1 << b,
                ..QEMU
            };
            assert_eq!(Features::decode(&embedded), Err(Missing::Embedded));
        }
    }

    #[test]
    fn mmu_600_before_variant_1_loses_sev() {
        let with_sev = |iidr: u32| Idr {
            idr0: QEMU.idr0 | 1 << 14,
            iidr,
            ..QEMU
        };
        let mmu600 = |variant: u32| 0x483 << 20 | variant << 16 | 0x43b;
        assert!(!Features::decode(&with_sev(mmu600(0))).unwrap().sev);
        assert!(Features::decode(&with_sev(mmu600(1))).unwrap().sev);
        assert!(Features::decode(&with_sev(0)).unwrap().sev);
        let i = Iidr(mmu600(2) | 3 << 12);
        assert_eq!(
            (i.implementer(), i.product(), i.variant(), i.revision()),
            (0x43b, 0x483, 2, 3)
        );
    }
}
