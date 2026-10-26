// SPDX-License-Identifier: BSD-2-Clause
use super::*;

/// A CPU like QEMU's `max`, in the fields the tests use: FP and AdvSIMD
/// with half precision, AES with PMULL, SHA-2 with SHA-512, LSE atomics,
/// SVE, pointer authentication (`APA`), and a 52-bit physical range.
fn cpu() -> IdRegs {
    let mut r = IdRegs::default();
    r.set(Reg::Aa64Pfr0, 1 << 32 | 1 << 20 | 1 << 16 | 1 << 4 | 1);
    r.set(Reg::Aa64Isar0, 2 << 20 | 2 << 12 | 2 << 4);
    r.set(Reg::Aa64Isar1, 1 << 4);
    r.set(Reg::Aa64Mmfr0, 6);
    r
}

#[test]
fn hwcaps_follow_the_fields() {
    let (caps, caps2) = UserView::new(&cpu()).hwcaps();
    for bit in [
        hwcap::FP,
        hwcap::FPHP,
        hwcap::ASIMD,
        hwcap::ASIMDHP,
        hwcap::AES,
        hwcap::PMULL,
        hwcap::SHA2,
        hwcap::SHA512,
        hwcap::ATOMICS,
        hwcap::SVE,
        hwcap::PACA,
        hwcap::CPUID,
    ] {
        assert_ne!(caps & bit, 0, "{bit:#x}");
    }
    assert_eq!(caps & (hwcap::SHA1 | hwcap::CRC32 | hwcap::PACG), 0);
    assert_eq!(caps2, 0);
}

#[test]
fn signed_fields_minus_one_is_absent() {
    // FP and AdvSIMD 0b1111: not implemented.
    let mut r = cpu();
    r.set(Reg::Aa64Pfr0, 0xf << 20 | 0xf << 16 | 1);
    let (caps, _) = UserView::new(&r).hwcaps();
    assert_eq!(caps & (hwcap::FP | hwcap::ASIMD | hwcap::FPHP), 0);
}

#[test]
fn features_merge_to_the_least_cpu() {
    let mut little = cpu();
    // AES without PMULL, no SVE.
    little.set(Reg::Aa64Isar0, 2 << 20 | 2 << 12 | 1 << 4);
    little.set(Reg::Aa64Pfr0, 1 << 20 | 1 << 16 | 1 << 4 | 1);
    let mut v = UserView::new(&cpu());
    v.add_cpu(&little);
    let (caps, _) = v.hwcaps();
    assert_ne!(caps & hwcap::AES, 0);
    assert_eq!(caps & (hwcap::PMULL | hwcap::SVE), 0);
    assert_eq!(v.get(Reg::Aa64Isar0) >> 4 & 0xf, 1);
}

#[test]
fn signed_fields_merge_by_value() {
    // One CPU without FP (-1) makes the system's FP -1, not 1.
    let mut none = cpu();
    none.set(Reg::Aa64Pfr0, 1 << 20 | 0xf << 16 | 1 << 4 | 1);
    let mut v = UserView::new(&cpu());
    v.add_cpu(&none);
    assert_eq!(v.get(Reg::Aa64Pfr0) >> 16 & 0xf, 0xf);
    assert_eq!(v.hwcaps().0 & hwcap::FP, 0);
}

#[test]
fn hidden_fields_read_safe_values() {
    let mut r = cpu();
    r.set(Reg::Aa64Dfr0, 9 | 3 << 12 | 5 << 8);
    let v = UserView::new(&r);
    // PARange is hidden; DebugVer reads Armv8.0's; BRPs is shown; PMUVer
    // is hidden.
    assert_eq!(v.get(Reg::Aa64Mmfr0), 0);
    assert_eq!(v.get(Reg::Aa64Dfr0), 6 | 3 << 12);
}

#[test]
fn unknown_bits_pass_through() {
    // ISAR2 bits 32-39 are in no field.
    let mut r = cpu();
    r.set(Reg::Aa64Isar2, 0xab << 32 | 1 << 52);
    let v = UserView::new(&r);
    assert_eq!(v.get(Reg::Aa64Isar2), 0xab << 32);
}

#[test]
fn hidden_pointer_auth() {
    let mut r = cpu();
    // APA3 and GPA3 too.
    r.set(Reg::Aa64Isar2, 1 << 12 | 1 << 8);
    let mut v = UserView::new(&r);
    v.hide_pointer_auth();
    assert_eq!(v.get(Reg::Aa64Isar1) & 0xff, 0);
    assert_eq!(v.get(Reg::Aa64Isar2), 1 << 8, "FreeBSD leaves GPA3");
    let (caps, _) = v.hwcaps();
    assert_eq!(caps & hwcap::PACA, 0);
    assert_ne!(caps & hwcap::PACG, 0);
}

#[test]
fn mrs_from_an_instruction() {
    // mrs x3, id_aa64isar0_el1
    let m = MrsRead::from_insn(0xd538_0603).unwrap();
    assert_eq!(
        m,
        MrsRead {
            crm: 6,
            op2: 0,
            rt: 3
        }
    );
    // mrs x0, midr_el1
    assert_eq!(
        MrsRead::from_insn(0xd538_0000),
        Some(MrsRead {
            crm: 0,
            op2: 0,
            rt: 0
        })
    );
    // mrs x0, ctr_el0 (op1 3) and msr id_aa64isar0_el1, x0: not reads of
    // the space.
    assert_eq!(MrsRead::from_insn(0xd53b_0020), None);
    assert_eq!(MrsRead::from_insn(0xd518_0600), None);
}

#[test]
fn mrs_from_a_syndrome() {
    // op0 3, op2 1, op1 0, CRn 0, Rt 30, CRm 4, read: ID_AA64PFR1_EL1.
    let iss = 3 << 20 | 1 << 17 | 30 << 5 | 4 << 1 | 1;
    assert_eq!(
        MrsRead::from_iss(iss),
        Some(MrsRead {
            crm: 4,
            op2: 1,
            rt: 30
        })
    );
    // The same as a write.
    assert_eq!(MrsRead::from_iss(iss & !1), None);
}

#[test]
fn reads_of_the_space() {
    let v = UserView::new(&cpu());
    let own = |r| match r {
        OwnReg::Midr => 0x410f_d0c0,
        OwnReg::Mpidr => 0x8000_0001,
        OwnReg::Revidr => 0,
    };
    assert_eq!(v.read(6, 0, own), Some(v.get(Reg::Aa64Isar0)));
    // Unassigned ID space reads as zero.
    assert_eq!(v.read(7, 7, own), Some(0));
    assert_eq!(v.read(0, 0, own), Some(0x410f_d0c0));
    assert_eq!(v.read(0, 5, own), Some(0x8000_0001));
    // CRm 1-3 (AArch32) and other CRm 0 registers are not emulated.
    assert_eq!(v.read(2, 0, own), None);
    assert_eq!(v.read(0, 1, own), None);
}

#[test]
fn fields_are_well_formed() {
    for r in Reg::ALL {
        let mut seen = 0u64;
        for f in r.fields() {
            assert!(f.width > 0 && u32::from(f.shift) + u32::from(f.width) <= 64);
            assert_eq!(seen & f.mask(), 0, "{r:?}.{} overlaps", f.name);
            seen |= f.mask();
            assert!(f.caps.is_empty() || f.width == 4);
        }
    }
}
