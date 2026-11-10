// SPDX-License-Identifier: BSD-2-Clause
//! `smmu`: the Arm SMMUv3 (Arm IHI 0070) as the IORT describes it, a
//! child of `acpi0` (docs/IOMMU.md).
//!
//! So far (IOMMU.md §8, stage 0) the driver probes, claims its registers
//! and its event, sync, and gerror interrupts (not PRI, as FreeBSD),
//! reads the identification registers, and leaves the hardware as
//! firmware left it: no register is written, nothing is translated, and
//! `dev.smmu.N.dma` reads 0. The queues, stream tables, and translation
//! come with stages 2 and 3.
//!
//! - Probe takes the generic SMMUv3 only (IORT model 0), as FreeBSD's
//!   `smmu_acpi_probe`.
//! - An SMMU without a feature the driver needs still attaches, with
//!   translation off (IOMMU.md §4.7): what it would translate then runs
//!   untranslated, as on FreeBSD when its attach fails.
//! - It refuses a voluntary detach: the devices it translates depend on
//!   it (`devctl detach` of a unit is FreeBSD's too, but nothing there
//!   holds its consumers to it).
//! - `hw.smmu.bypass_named` (`CTLFLAG_RDTUN`, 0): what becomes of the
//!   streams of named components without a driver (IOMMU.md §4.9).
//!
//! Behavior reference: sys/arm64/iommu/smmu.c, smmu_acpi.c (and the
//! user's branch orangepi-6-plus of them).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::sysctl::ctlflag;
use acpi::iort::{SMMU_DESC, SmmuNode};
use acpi_tables::iort::SmmuV3;
use bus::{
    AttachCx, AttachError, Busy, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, StartCx,
    StopCx, StopReason,
};
use iommu::smmu::{Endian, Features, Idr, Missing, reg};
use regs::Bytes;
use sysctl::{Number, Tree, Value};

/// `hw.smmu.bypass_named`.
static BYPASS_NAMED: AtomicI32 = AtomicI32::new(0);

/// Takes the boot-time tunables (`TUNABLE_INT_FETCH`): `get` gives a
/// name's integer value, if set. The kernel calls it before drivers
/// attach.
pub fn set_tunables(get: &dyn Fn(&str) -> Option<i32>) {
    if let Some(v) = get("hw.smmu.bypass_named") {
        // Relaxed: a tunable, read at attach.
        BYPASS_NAMED.store(v, Ordering::Relaxed);
    }
}

/// Whether named components without a driver have their streams
/// bypassed rather than aborted.
#[must_use]
pub fn bypass_named() -> bool {
    BYPASS_NAMED.load(Ordering::Relaxed) != 0
}

/// Adds `hw.smmu` and its knob to `t` (`smmu_acpi.c`'s static sysctls).
pub fn sysctl_init(t: &mut Tree) {
    let root = t.root();
    let Some(hw) = t.child(root, "hw") else {
        return;
    };
    let Some(node) = t
        .child(hw, "smmu")
        .or_else(|| t.add_node(hw, Number::Auto, "smmu", "SMMU").ok())
    else {
        return;
    };
    if let Ok(id) = t.add_value(
        node,
        Number::Auto,
        "bypass_named",
        "ACPI devices' streams bypass the SMMU (no translation)",
        Value::Int(Box::new(|| BYPASS_NAMED.load(Ordering::Relaxed))),
    ) {
        t.set_flags(id, ctlflag::TUN);
    }
}

/// The registers the driver reads.
pub trait Regs: Send + Sync {
    /// The 32-bit register at `off`.
    fn read32(&self, off: usize) -> u32;
}

impl Regs for MmioGrant<Bytes> {
    fn read32(&self, off: usize) -> u32 {
        regs::Mmio::read32(self, off)
    }
}

/// What the SMMU says it is, read once at attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The identification registers as read.
    pub idr: Idr,
    /// What it supports, or what it lacks that the driver needs.
    pub features: Result<Features, Missing>,
    /// Whether its accesses are coherent: the IORT's COHACC override, or
    /// `IDR0.COHACC`.
    pub coherent: bool,
}

impl Identity {
    /// Reads the identification registers.
    pub fn read(r: &impl Regs, firmware: &SmmuV3) -> Self {
        let idr = Idr {
            idr0: r.read32(reg::IDR0),
            idr1: r.read32(reg::IDR1),
            idr3: r.read32(reg::IDR3),
            idr5: r.read32(reg::IDR5),
            iidr: r.read32(reg::IIDR),
        };
        let features = Features::decode(&idr);
        let coherent = firmware.coherent() || features.as_ref().is_ok_and(|f| f.coherent);
        Self {
            idr,
            features,
            coherent,
        }
    }

    /// The lines FreeBSD's `smmu_check_features` prints under
    /// `bootverbose`, in its order, and the raw registers.
    pub fn verbose(&self, mut line: impl FnMut(core::fmt::Arguments<'_>)) {
        let i = &self.idr;
        line(format_args!(
            "IDR0 {:#x} IDR1 {:#x} IDR3 {:#x} IDR5 {:#x} IIDR {:#x}",
            i.idr0, i.idr1, i.idr3, i.idr5, i.iidr
        ));
        let Ok(f) = &self.features else {
            return;
        };
        if f.two_level_streams {
            line(format_args!("2-level stream table supported."));
        }
        if f.two_level_cds {
            line(format_args!("2-level CD table supported."));
        }
        line(format_args!(
            "{}",
            match f.endian {
                Endian::Mixed => "Mixed endianness supported.",
                Endian::Little => "Little endian supported only.",
                Endian::Big => "Big endian supported only.",
            }
        ));
        if f.msi {
            line(format_args!("MSI feature present."));
        }
        if f.hyp {
            line(format_args!("HYP feature present."));
        }
        if f.stage1 {
            line(format_args!("Stage 1 translation supported."));
        }
        if f.stage2 {
            line(format_args!("Stage 2 translation supported."));
        }
        line(format_args!("ASID bits {}", f.asid_bits));
        line(format_args!("CMD queue bits {}", f.cmdq_bits));
        line(format_args!("EVENT queue bits {}", f.evtq_bits));
        if f.pri {
            line(format_args!("PRI queue bits {}", f.priq_bits));
        }
        line(format_args!("SSID bits {}", f.ssid_bits));
        line(format_args!("SID bits {}", f.sid_bits));
    }
}

/// An `smmuN` instance.
pub struct Smmu {
    unit: u32,
    identity: Identity,
    /// Its registers, held for stage 2; not written.
    _regs: MmioGrant<Bytes>,
    /// The event, sync, and gerror lines: granted, no handlers yet (the
    /// sync line never gets one, as on FreeBSD).
    _lines: [IrqLine; 3],
}

impl core::fmt::Debug for Smmu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Smmu")
            .field("unit", &self.unit)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl Driver for Smmu {
    const NAME: &'static str = "smmu";
    const IDS: Ids = Ids::Nexus(&["smmu"]);

    fn describe(_: &Ident) -> String {
        SMMU_DESC.into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let node = cx.bus_data::<SmmuNode>().ok_or(AttachError::Decline)?;
        // FreeBSD's probe: the generic model only.
        if node.smmu.model != SmmuV3::MODEL_GENERIC {
            return Err(AttachError::Decline);
        }
        let firmware = node.smmu;
        let regs = cx.mmio_bytes(0)?;
        // Interrupt lines are event, PRI, sync, gerror; PRI is not used.
        let lines = [cx.irq(0)?, cx.irq(2)?, cx.irq(3)?];
        let identity = Identity::read(&regs, &firmware);
        Ok(Self {
            unit: cx.unit(),
            identity,
            _regs: regs,
            _lines: lines,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let m = bus::manager();
        let unit = self.unit;
        if bus::bootverbose() {
            self.identity
                .verbose(|l| m.log(format_args!("smmu{unit}: {l}")));
        }
        if let Err(missing) = self.identity.features {
            m.log(format_args!("smmu{unit}: {}", missing.describe()));
            m.log(format_args!(
                "smmu{unit}: Some features are required but not supported by hardware."
            ));
        } else if !iommu_framework::dma() {
            m.log(format_args!(
                "smmu{unit}: left disabled: DMA translation is off (hw.iommu.dma)"
            ));
        }
        // `dev.smmu.N.dma`: whether this unit translates DMA; none does
        // yet (IOMMU.md §8, stage 3).
        m.sysctl(cx.sleepable(), &mut |t| {
            if let Some(n) = bus::device_node(t, Self::NAME, unit) {
                let _ = t.add_value(n, Number::Auto, "dma", "DMA ops enabled", Value::int(0));
            }
        });
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        // Nothing to stop: the hardware is as firmware left it.
    }

    fn may_detach(&self) -> Result<(), Busy> {
        Err(Busy)
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Smmu>();

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec::Vec;
    use std::collections::BTreeMap;

    use super::*;

    struct Fake(BTreeMap<usize, u32>);

    impl Regs for Fake {
        fn read32(&self, off: usize) -> u32 {
            self.0.get(&off).copied().unwrap_or(0)
        }
    }

    fn firmware(flags: u32) -> SmmuV3 {
        SmmuV3 {
            base: 0x905_0000,
            flags,
            model: 0,
            event_gsiv: 106,
            pri_gsiv: 107,
            gerr_gsiv: 109,
            sync_gsiv: 108,
            id_mapping_index: Some(0),
        }
    }

    /// Two-level stream tables, little-endian, AArch64 tables, stages 1
    /// and 2, ASID16, 16-bit stream IDs, 44-bit output.
    fn fake(cohacc: bool) -> Fake {
        let idr0 = 1 << 27 | 2 << 21 | 1 << 12 | u32::from(cohacc) << 4 | 2 << 2 | 3;
        Fake(BTreeMap::from([
            (reg::IDR0, idr0),
            (reg::IDR1, 19 << 21 | 19 << 16 | 16),
            (reg::IDR5, 1 << 4 | 4),
        ]))
    }

    #[test]
    fn identifies_and_takes_coherency_from_either_source() {
        let id = Identity::read(&fake(false), &firmware(1));
        assert!(id.coherent, "the IORT's override");
        let f = id.features.unwrap();
        assert_eq!((f.sid_bits, f.oas), (16, Some(44)));
        assert!(Identity::read(&fake(true), &firmware(0)).coherent);
        assert!(!Identity::read(&fake(false), &firmware(0)).coherent);
        let mut lines = Vec::new();
        id.verbose(|l| lines.push(alloc::format!("{l}")));
        assert_eq!(
            lines,
            [
                "IDR0 0x840100b IDR1 0x2730010 IDR3 0x0 IDR5 0x14 IIDR 0x0",
                "2-level stream table supported.",
                "Little endian supported only.",
                "Stage 1 translation supported.",
                "Stage 2 translation supported.",
                "ASID bits 16",
                "CMD queue bits 19",
                "EVENT queue bits 19",
                "SSID bits 0",
                "SID bits 16",
            ]
        );
    }

    #[test]
    fn a_missing_feature_is_reported() {
        let mut f = fake(false);
        f.0.insert(reg::IDR0, f.0[&reg::IDR0] & !(1 << 1));
        let id = Identity::read(&f, &firmware(1));
        assert_eq!(id.features, Err(Missing::Stage1));
    }

    #[test]
    fn tunables_and_sysctls() {
        set_tunables(&|n| (n == "hw.smmu.bypass_named").then_some(1));
        assert!(bypass_named());
        set_tunables(&|n| (n == "hw.smmu.bypass_named").then_some(0));
        assert!(!bypass_named());
        let mut t = Tree::new();
        sysctl_init(&mut t);
        assert!(t.name2oid("hw.smmu.bypass_named").is_some());
    }
}
