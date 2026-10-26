// SPDX-License-Identifier: BSD-2-Clause
//! The arm64 ID registers as user mode sees them, and the `AT_HWCAP` and
//! `AT_HWCAP2` bits they imply (FreeBSD's `identcpu.c`).
//!
//! FreeBSD gives user mode one view of the ID registers for the whole
//! system: each field merged over every CPU by its own rule (the lowest
//! value for most features, since a program may run on any CPU), and every
//! field FreeBSD does not expose replaced by a safe value. Bits outside
//! every known field pass through. User mode reads the view with `mrs`,
//! which traps and which the kernel emulates ([`MrsRead`]); the hwcaps are
//! the features the view's fields reach.
//!
//! The tables follow FreeBSD 16-CURRENT's (15.1's `HWCAP` bits are the
//! same). Only the AArch64 registers are kept: there is no 32-bit
//! compatibility, and `CTR_EL0` is only trapped on CPUs with errata.
//!
//! Behavior reference: sys/arm64/arm64/identcpu.c, sys/arm64/arm64/undefined.c,
//! sys/arm64/include/elf.h, sys/arm64/include/armreg.h (Arm ARM D23).

#![no_std]
#![forbid(unsafe_code)]

/// One of the AArch64 ID registers user mode may read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reg {
    Aa64Pfr0,
    Aa64Pfr1,
    Aa64Pfr2,
    Aa64Zfr0,
    Aa64Dfr0,
    Aa64Dfr1,
    Aa64Afr0,
    Aa64Afr1,
    Aa64Isar0,
    Aa64Isar1,
    Aa64Isar2,
    Aa64Mmfr0,
    Aa64Mmfr1,
    Aa64Mmfr2,
    Aa64Mmfr3,
    Aa64Mmfr4,
}

impl Reg {
    /// Every register, in [`IdRegs`]' order.
    pub const ALL: [Self; 16] = [
        Self::Aa64Pfr0,
        Self::Aa64Pfr1,
        Self::Aa64Pfr2,
        Self::Aa64Zfr0,
        Self::Aa64Dfr0,
        Self::Aa64Dfr1,
        Self::Aa64Afr0,
        Self::Aa64Afr1,
        Self::Aa64Isar0,
        Self::Aa64Isar1,
        Self::Aa64Isar2,
        Self::Aa64Mmfr0,
        Self::Aa64Mmfr1,
        Self::Aa64Mmfr2,
        Self::Aa64Mmfr3,
        Self::Aa64Mmfr4,
    ];

    const fn index(self) -> usize {
        self as usize
    }

    /// `CRm` and `op2` (`op0` 3, `op1` 0, `CRn` 0 for all of them).
    #[must_use]
    pub const fn encoding(self) -> (u8, u8) {
        match self {
            Self::Aa64Pfr0 => (4, 0),
            Self::Aa64Pfr1 => (4, 1),
            Self::Aa64Pfr2 => (4, 2),
            Self::Aa64Zfr0 => (4, 4),
            Self::Aa64Dfr0 => (5, 0),
            Self::Aa64Dfr1 => (5, 1),
            Self::Aa64Afr0 => (5, 4),
            Self::Aa64Afr1 => (5, 5),
            Self::Aa64Isar0 => (6, 0),
            Self::Aa64Isar1 => (6, 1),
            Self::Aa64Isar2 => (6, 2),
            Self::Aa64Mmfr0 => (7, 0),
            Self::Aa64Mmfr1 => (7, 1),
            Self::Aa64Mmfr2 => (7, 2),
            Self::Aa64Mmfr3 => (7, 3),
            Self::Aa64Mmfr4 => (7, 4),
        }
    }

    /// The register at `CRm`/`op2`, if it is one of these.
    #[must_use]
    pub fn from_encoding(crm: u8, op2: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.encoding() == (crm, op2))
    }

    /// Its fields, highest first.
    #[must_use]
    pub const fn fields(self) -> &'static [Field] {
        match self {
            Self::Aa64Pfr0 => PFR0,
            Self::Aa64Pfr1 => PFR1,
            Self::Aa64Zfr0 => ZFR0,
            Self::Aa64Dfr0 => DFR0,
            Self::Aa64Dfr1 => DFR1,
            Self::Aa64Isar0 => ISAR0,
            Self::Aa64Isar1 => ISAR1,
            Self::Aa64Isar2 => ISAR2,
            Self::Aa64Mmfr0 => MMFR0,
            Self::Aa64Mmfr1 => MMFR1,
            Self::Aa64Mmfr2 => MMFR2,
            Self::Aa64Mmfr3 => MMFR3,
            Self::Aa64Pfr2 | Self::Aa64Afr0 | Self::Aa64Afr1 | Self::Aa64Mmfr4 => &[],
        }
    }
}

/// How a field's values on several CPUs combine (FreeBSD's `MRS_*`
/// types; the registers kept here use only these).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    /// Always the safe value.
    Exact,
    /// The lowest.
    Lower,
    /// The highest.
    Higher,
}

/// Which hwcap word a bit is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Word {
    Hwcap,
    Hwcap2,
}

/// A hwcap bit a field's value reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    pub word: Word,
    pub bit: u64,
    /// The least field value that has the feature.
    pub min: u8,
}

/// One ID register field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub shift: u8,
    pub width: u8,
    /// Signed fields: `0b1111` (-1) means "not implemented".
    pub signed: bool,
    pub merge: Merge,
    /// What user mode sees of a field it is not shown, and what an
    /// [`Merge::Exact`] field always reads.
    pub safe: u8,
    /// Whether FreeBSD shows it to FreeBSD programs (`MRS_FREEBSD`).
    pub visible: bool,
    pub caps: &'static [Cap],
}

impl Field {
    const fn mask(&self) -> u64 {
        ((1u64 << self.width) - 1) << self.shift
    }

    /// Orders two field values (in place in their registers) as the
    /// field's sign says (FreeBSD's `mrs_field_cmp`).
    fn cmp(&self, a: u64, b: u64) -> core::cmp::Ordering {
        let m = (1u64 << self.width) - 1;
        let (mut a, mut b) = ((a >> self.shift) & m, (b >> self.shift) & m);
        if self.signed {
            // Flip the sign bit so negative values sort below positive.
            a ^= 1 << (self.width - 1);
            b ^= 1 << (self.width - 1);
        }
        a.cmp(&b)
    }

    /// Merges `other`'s value of this field into `reg` (FreeBSD's
    /// `update_special_reg_field`, with `merge` in place of the field's
    /// own rule).
    fn update(&self, reg: u64, merge: Merge, other: u64) -> u64 {
        use core::cmp::Ordering::{Greater, Less};
        let mask = self.mask();
        let mut cur = reg & mask;
        let new = other & mask;
        let safe = u64::from(self.safe) << self.shift;
        cur = match merge {
            Merge::Exact => safe,
            Merge::Lower if self.cmp(new, cur) == Less => new,
            Merge::Higher if self.cmp(new, cur) == Greater => new,
            Merge::Lower | Merge::Higher => cur,
        };
        (reg & !mask) | cur
    }
}

/// One CPU's ID registers, in [`Reg::ALL`]'s order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdRegs(pub [u64; 16]);

impl IdRegs {
    #[must_use]
    pub const fn get(&self, r: Reg) -> u64 {
        self.0[r.index()]
    }

    pub const fn set(&mut self, r: Reg, v: u64) {
        self.0[r.index()] = v;
    }
}

/// The system's user view of the ID registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserView(IdRegs);

impl UserView {
    /// The view of a system whose first CPU has `boot`.
    #[must_use]
    pub fn new(boot: &IdRegs) -> Self {
        let mut v = Self(*boot);
        v.add_cpu(boot);
        v
    }

    /// Takes another CPU's registers into the view (FreeBSD's
    /// `update_special_regs`).
    pub fn add_cpu(&mut self, cpu: &IdRegs) {
        for r in Reg::ALL {
            self.clear_set(r, u64::MAX, cpu.get(r));
        }
    }

    /// Clears the bits of `clear` in `r` and sets those of `set`, then
    /// merges the result with what the view had, field by field (FreeBSD's
    /// `clear_set_special_reg_idx`): how the kernel hides a feature it
    /// does not support.
    pub fn clear_set(&mut self, r: Reg, clear: u64, set: u64) {
        let old = self.0.get(r);
        let mut new = (old & !clear) | set;
        for f in r.fields() {
            let merge = if f.visible { f.merge } else { Merge::Exact };
            new = f.update(new, merge, old);
        }
        self.0.set(r, new);
    }

    /// Hides pointer authentication, which the kernel does not enable for
    /// user mode (FreeBSD's `ptrauth_disabled`; it leaves `GPA3`).
    pub fn hide_pointer_auth(&mut self) {
        let mask = |r: Reg, names: &[&str]| {
            r.fields()
                .iter()
                .filter(|f| names.contains(&f.name))
                .fold(0, |m, f| m | f.mask())
        };
        let isar1 = mask(Reg::Aa64Isar1, &["API", "APA", "GPA", "GPI"]);
        self.clear_set(Reg::Aa64Isar1, isar1, 0);
        let isar2 = mask(Reg::Aa64Isar2, &["APA3"]);
        self.clear_set(Reg::Aa64Isar2, isar2, 0);
    }

    /// What user mode reads of `r`.
    #[must_use]
    pub const fn get(&self, r: Reg) -> u64 {
        self.0.get(r)
    }

    /// `AT_HWCAP` and `AT_HWCAP2` (FreeBSD's `parse_cpu_features`), with
    /// `HWCAP_CPUID`: the registers themselves may be read.
    #[must_use]
    pub fn hwcaps(&self) -> (u64, u64) {
        let (mut hwcap, mut hwcap2) = (hwcap::CPUID, 0);
        for r in Reg::ALL {
            let v = self.get(r);
            for f in r.fields() {
                for c in f.caps {
                    if f.cmp(v, u64::from(c.min) << f.shift).is_ge() {
                        match c.word {
                            Word::Hwcap => hwcap |= c.bit,
                            Word::Hwcap2 => hwcap2 |= c.bit,
                        }
                    }
                }
            }
        }
        (hwcap, hwcap2)
    }

    /// What an emulated `mrs` of the ID register space reads: the view for
    /// a known register, zero for the rest of `CRm` 4–7 (read-as-zero),
    /// and for `CRm` 0 the CPU's own `MIDR_EL1`, `MPIDR_EL1`, or
    /// `REVIDR_EL1` (`op2` 0, 5, 6), which `own` supplies. `None` for
    /// anything else (FreeBSD's `user_idreg_handler`).
    #[must_use]
    pub fn read(&self, crm: u8, op2: u8, own: impl FnOnce(OwnReg) -> u64) -> Option<u64> {
        match crm {
            4..=7 => Some(Reg::from_encoding(crm, op2).map_or(0, |r| self.get(r))),
            0 => match op2 {
                0 => Some(own(OwnReg::Midr)),
                5 => Some(own(OwnReg::Mpidr)),
                6 => Some(own(OwnReg::Revidr)),
                _ => None,
            },
            _ => None,
        }
    }
}

/// The per-CPU registers user mode reads as they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnReg {
    Midr,
    Mpidr,
    Revidr,
}

/// A trapped system-register read from user mode that may be the ID
/// register space (`op0` 3, `op1` 0, `CRn` 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MrsRead {
    pub crm: u8,
    pub op2: u8,
    /// The destination; 31 is `xzr`.
    pub rt: u8,
}

impl MrsRead {
    /// From the ISS of an `EC` 0x18 (trapped `MSR`/`MRS`) syndrome.
    #[must_use]
    pub fn from_iss(iss: u64) -> Option<Self> {
        let bits =
            |shift: u32, width: u32| u8::try_from((iss >> shift) & ((1 << width) - 1)).unwrap_or(0);
        // Direction 1 is a read; op0, op2, op1, CRn, Rt, CRm, Dir.
        let (op0, op2, op1, crn, rt, crm, read) = (
            bits(20, 2),
            bits(17, 3),
            bits(14, 3),
            bits(10, 4),
            bits(5, 5),
            bits(1, 4),
            bits(0, 1) == 1,
        );
        (read && op0 == 3 && op1 == 0 && crn == 0).then_some(Self { crm, op2, rt })
    }

    /// From an instruction that raised an undefined-instruction exception
    /// (CPUs without FEAT_IDST): an `MRS` of that space (FreeBSD's
    /// `undef_sys_insn`).
    #[must_use]
    pub fn from_insn(insn: u32) -> Option<Self> {
        if insn & 0xfff0_0000 != 0xd530_0000 {
            return None;
        }
        let bits = |shift: u32, width: u32| {
            u8::try_from((insn >> shift) & ((1 << width) - 1)).unwrap_or(0)
        };
        let (op0, op1, crn, crm, op2, rt) = (
            bits(19, 2),
            bits(16, 3),
            bits(12, 4),
            bits(8, 4),
            bits(5, 3),
            bits(0, 5),
        );
        (op0 == 3 && op1 == 0 && crn == 0).then_some(Self { crm, op2, rt })
    }
}

/// `AT_HWCAP` bits (`HWCAP_*`, the same as Linux's).
pub mod hwcap {
    pub const FP: u64 = 1 << 0;
    pub const ASIMD: u64 = 1 << 1;
    pub const AES: u64 = 1 << 3;
    pub const PMULL: u64 = 1 << 4;
    pub const SHA1: u64 = 1 << 5;
    pub const SHA2: u64 = 1 << 6;
    pub const CRC32: u64 = 1 << 7;
    pub const ATOMICS: u64 = 1 << 8;
    pub const FPHP: u64 = 1 << 9;
    pub const ASIMDHP: u64 = 1 << 10;
    pub const CPUID: u64 = 1 << 11;
    pub const ASIMDRDM: u64 = 1 << 12;
    pub const JSCVT: u64 = 1 << 13;
    pub const FCMA: u64 = 1 << 14;
    pub const LRCPC: u64 = 1 << 15;
    pub const DCPOP: u64 = 1 << 16;
    pub const SHA3: u64 = 1 << 17;
    pub const SM3: u64 = 1 << 18;
    pub const SM4: u64 = 1 << 19;
    pub const ASIMDDP: u64 = 1 << 20;
    pub const SHA512: u64 = 1 << 21;
    pub const SVE: u64 = 1 << 22;
    pub const ASIMDFHM: u64 = 1 << 23;
    pub const DIT: u64 = 1 << 24;
    pub const USCAT: u64 = 1 << 25;
    pub const ILRCPC: u64 = 1 << 26;
    pub const FLAGM: u64 = 1 << 27;
    pub const SSBS: u64 = 1 << 28;
    pub const SB: u64 = 1 << 29;
    pub const PACA: u64 = 1 << 30;
    pub const PACG: u64 = 1 << 31;
}

/// `AT_HWCAP2` bits (`HWCAP2_*`).
pub mod hwcap2 {
    pub const DCPODP: u64 = 1 << 0;
    pub const SVE2: u64 = 1 << 1;
    pub const SVEAES: u64 = 1 << 2;
    pub const SVEPMULL: u64 = 1 << 3;
    pub const SVEBITPERM: u64 = 1 << 4;
    pub const SVESHA3: u64 = 1 << 5;
    pub const SVESM4: u64 = 1 << 6;
    pub const FLAGM2: u64 = 1 << 7;
    pub const FRINT: u64 = 1 << 8;
    pub const SVEI8MM: u64 = 1 << 9;
    pub const SVEF32MM: u64 = 1 << 10;
    pub const SVEF64MM: u64 = 1 << 11;
    pub const SVEBF16: u64 = 1 << 12;
    pub const I8MM: u64 = 1 << 13;
    pub const BF16: u64 = 1 << 14;
    pub const DGH: u64 = 1 << 15;
    pub const RNG: u64 = 1 << 16;
    pub const BTI: u64 = 1 << 17;
    pub const AFP: u64 = 1 << 20;
    pub const RPRES: u64 = 1 << 21;
    pub const WFXT: u64 = 1 << 31;
    pub const SVE_EBF16: u64 = 1 << 33;
    pub const SVE2P1: u64 = 1 << 36;
    pub const MOPS: u64 = 1 << 43;
}

const fn h(bit: u64, min: u8) -> Cap {
    Cap {
        word: Word::Hwcap,
        bit,
        min,
    }
}

const fn h2(bit: u64, min: u8) -> Cap {
    Cap {
        word: Word::Hwcap2,
        bit,
        min,
    }
}

/// A 4-bit field merged to the lowest value, hidden from user mode.
const fn hidden(name: &'static str, shift: u8) -> Field {
    Field {
        name,
        shift,
        width: 4,
        signed: false,
        merge: Merge::Lower,
        safe: 0,
        visible: false,
        caps: &[],
    }
}

/// A 4-bit field merged to the lowest value, shown to user mode.
const fn shown(name: &'static str, shift: u8, caps: &'static [Cap]) -> Field {
    Field {
        visible: true,
        caps,
        ..hidden(name, shift)
    }
}

const fn signed(f: Field) -> Field {
    Field { signed: true, ..f }
}

use hwcap as h1;

const PFR0: &[Field] = &[
    hidden("CSV3", 60),
    hidden("CSV2", 56),
    hidden("RME", 52),
    shown("DIT", 48, &[h(h1::DIT, 1)]),
    hidden("AMU", 44),
    hidden("MPAM", 40),
    hidden("SEL2", 36),
    shown("SVE", 32, &[h(h1::SVE, 1)]),
    hidden("RAS", 28),
    hidden("GIC", 24),
    signed(shown("AdvSIMD", 20, &[h(h1::ASIMD, 0), h(h1::ASIMDHP, 1)])),
    signed(shown("FP", 16, &[h(h1::FP, 0), h(h1::FPHP, 1)])),
    hidden("EL3", 12),
    hidden("EL2", 8),
    shown("EL1", 4, &[]),
    shown("EL0", 0, &[]),
];

const PFR1: &[Field] = &[
    hidden("PFAR", 60),
    hidden("DF2", 56),
    hidden("MTEX", 52),
    hidden("THE", 48),
    signed(hidden("MTE_frac", 40)),
    hidden("NMI", 36),
    hidden("CSV2_frac", 32),
    hidden("RNDR_trap", 28),
    hidden("SME", 24),
    hidden("MPAM_frac", 16),
    hidden("RAS_frac", 12),
    hidden("MTE", 8),
    shown("SSBS", 4, &[h(h1::SSBS, 1)]),
    shown("BT", 0, &[h2(hwcap2::BTI, 1)]),
];

const ZFR0: &[Field] = &[
    shown("F64MM", 56, &[h2(hwcap2::SVEF64MM, 1)]),
    shown("F32MM", 52, &[h2(hwcap2::SVEF32MM, 1)]),
    shown("I8MM", 44, &[h2(hwcap2::SVEI8MM, 1)]),
    shown("SM4", 40, &[h2(hwcap2::SVESM4, 1)]),
    shown("SHA3", 32, &[h2(hwcap2::SVESHA3, 1)]),
    shown(
        "BF16",
        20,
        &[h2(hwcap2::SVEBF16, 1), h2(hwcap2::SVE_EBF16, 1)],
    ),
    shown("BitPerm", 16, &[h2(hwcap2::SVEBITPERM, 1)]),
    shown("AES", 4, &[h2(hwcap2::SVEAES, 1), h2(hwcap2::SVEPMULL, 2)]),
    shown("SVEver", 0, &[h2(hwcap2::SVE2, 1), h2(hwcap2::SVE2P1, 2)]),
];

const DFR0: &[Field] = &[
    hidden("HPMN0", 60),
    hidden("BRBE", 52),
    signed(hidden("MTPMU", 48)),
    hidden("TraceBuffer", 44),
    hidden("TraceFilt", 40),
    signed(hidden("DoubleLock", 36)),
    hidden("PMSVer", 32),
    hidden("CTX_CMPs", 28),
    shown("WRPs", 20, &[]),
    hidden("PMSS", 16),
    shown("BRPs", 12, &[]),
    hidden("PMUVer", 8),
    hidden("TraceVer", 4),
    // Armv8.0's debug architecture.
    Field {
        safe: 6,
        ..hidden("DebugVer", 0)
    },
];

const DFR1: &[Field] = &[
    hidden("DPFZS", 52),
    hidden("PMICNTR", 36),
    hidden("SPMU", 32),
];

const ISAR0: &[Field] = &[
    shown("RNDR", 60, &[h2(hwcap2::RNG, 1)]),
    hidden("TLB", 56),
    shown("TS", 52, &[h(h1::FLAGM, 1), h2(hwcap2::FLAGM2, 2)]),
    shown("FHM", 48, &[h(h1::ASIMDFHM, 1)]),
    shown("DP", 44, &[h(h1::ASIMDDP, 1)]),
    shown("SM4", 40, &[h(h1::SM4, 1)]),
    shown("SM3", 36, &[h(h1::SM3, 1)]),
    shown("SHA3", 32, &[h(h1::SHA3, 1)]),
    shown("RDM", 28, &[h(h1::ASIMDRDM, 1)]),
    hidden("TME", 24),
    shown("Atomic", 20, &[h(h1::ATOMICS, 2)]),
    shown("CRC32", 16, &[h(h1::CRC32, 1)]),
    shown("SHA2", 12, &[h(h1::SHA2, 1), h(h1::SHA512, 2)]),
    shown("SHA1", 8, &[h(h1::SHA1, 1)]),
    shown("AES", 4, &[h(h1::AES, 1), h(h1::PMULL, 2)]),
];

const ISAR1: &[Field] = &[
    hidden("LS64", 60),
    hidden("XS", 56),
    shown("I8MM", 52, &[h2(hwcap2::I8MM, 1)]),
    shown("DGH", 48, &[h2(hwcap2::DGH, 1)]),
    shown("BF16", 44, &[h2(hwcap2::BF16, 1)]),
    hidden("SPECRES", 40),
    shown("SB", 36, &[h(h1::SB, 1)]),
    shown("FRINTTS", 32, &[h2(hwcap2::FRINT, 1)]),
    shown("GPI", 28, &[h(h1::PACG, 1)]),
    shown("GPA", 24, &[h(h1::PACG, 1)]),
    shown("LRCPC", 20, &[h(h1::LRCPC, 1), h(h1::ILRCPC, 2)]),
    shown("FCMA", 16, &[h(h1::FCMA, 1)]),
    shown("JSCVT", 12, &[h(h1::JSCVT, 1)]),
    shown("API", 8, &[h(h1::PACA, 1)]),
    shown("APA", 4, &[h(h1::PACA, 1)]),
    shown("DPB", 0, &[h(h1::DCPOP, 1), h2(hwcap2::DCPODP, 2)]),
];

const ISAR2: &[Field] = &[
    hidden("ATS1A", 60),
    hidden("CSSC", 52),
    hidden("RPRFM", 48),
    hidden("PRFMSLC", 40),
    hidden("CLRBHB", 28),
    hidden("PAC_frac", 24),
    hidden("BC", 20),
    shown("MOPS", 16, &[h2(hwcap2::MOPS, 1)]),
    shown("APA3", 12, &[h(h1::PACA, 1)]),
    shown("GPA3", 8, &[h(h1::PACG, 1)]),
    shown("RPRES", 4, &[h2(hwcap2::RPRES, 1)]),
    Field {
        caps: &[h2(hwcap2::WFXT, 2)],
        ..hidden("WFxT", 0)
    },
];

const MMFR0: &[Field] = &[
    hidden("ECV", 60),
    hidden("FGT", 56),
    hidden("ExS", 44),
    hidden("TGran4_2", 40),
    hidden("TGran64_2", 36),
    hidden("TGran16_2", 32),
    signed(hidden("TGran4", 28)),
    signed(hidden("TGran64", 24)),
    hidden("TGran16", 20),
    hidden("BigEndEL0", 16),
    hidden("SNSMem", 12),
    hidden("BigEnd", 8),
    hidden("ASIDBits", 4),
    hidden("PARange", 0),
];

const MMFR1: &[Field] = &[
    hidden("ECBHB", 60),
    hidden("CMOW", 56),
    hidden("TIDCP1", 52),
    hidden("nTLBPA", 48),
    Field {
        caps: &[h2(hwcap2::AFP, 1)],
        ..hidden("AFP", 44)
    },
    hidden("HCX", 40),
    hidden("ETS", 36),
    hidden("TWED", 32),
    hidden("XNX", 28),
    Field {
        merge: Merge::Higher,
        ..hidden("SpecSEI", 24)
    },
    hidden("PAN", 20),
    hidden("LO", 16),
    hidden("HPDS", 12),
    hidden("VH", 8),
    hidden("VMIDBits", 4),
    hidden("HAFDBS", 0),
];

const MMFR2: &[Field] = &[
    hidden("E0PD", 60),
    hidden("EVT", 56),
    hidden("BBM", 52),
    hidden("TTL", 48),
    hidden("FWB", 40),
    hidden("IDS", 36),
    shown("AT", 32, &[h(h1::USCAT, 1)]),
    hidden("ST", 28),
    hidden("NV", 24),
    hidden("CCIDX", 20),
    hidden("VARange", 16),
    hidden("IESB", 12),
    hidden("LSM", 8),
    hidden("UAO", 4),
    hidden("CnP", 0),
];

const MMFR3: &[Field] = &[
    hidden("Spec_FPACC", 60),
    hidden("ADERR", 56),
    hidden("SDERR", 52),
    hidden("ANERR", 44),
    hidden("SNERR", 40),
    hidden("MEC", 28),
    hidden("AIE", 24),
    hidden("S2POE", 20),
    hidden("S1POE", 16),
    hidden("S2PIE", 12),
    hidden("S1PIE", 8),
    hidden("SCTLRX", 4),
    hidden("TCRX", 0),
];

#[cfg(test)]
mod tests;
