// SPDX-License-Identifier: BSD-2-Clause
//! The IOMMU framework (docs/IOMMU.md §4.1): FreeBSD's sys/dev/iommu and
//! arm64/iommu/iommu.c. Translation domains with their IOVA spaces and
//! I/O page tables ([`domain`]), which `vm::dma`'s IOMMU backend maps
//! through; the unit driver (`drivers/smmu`) makes them, one per context,
//! and owns its stream table, context descriptors, and command queue. And
//! the knobs (IOMMU.md §4.12):
//!
//! - `hw.iommu.dma` (tunable): DMA translation. 1 by default on arm64,
//!   where translation is what keeps a board's DMA correct (FreeBSD's
//!   default is 0; docs/compat-deviations.md); 0 elsewhere, as FreeBSD.
//! - `hw.iommu.relaxed_ordering` (`CTLFLAG_RDTUN`, 0): keep PCIe relaxed
//!   ordering on functions the IOMMU translates (the board's FreeBSD).
//! - `hw.iommu.check_free` (`CTLFLAG_RWTUN`, 0): FreeBSD's `iommu_gas`
//!   self-check, kept for parity.
//! - `hw.busdma.default` and `hw.busdma.pciD.B.S.F` (environment):
//!   `bounce` gives the requester a pass-through context, `iommu` (or
//!   `dmar`) a translated one ([`requester_disabled`]).
//!
//! A translated function's MSI writes go through its domain too: the
//! interrupt controller's doorbell page is mapped once per domain
//! ([`domain::Domain`]'s `doorbell`, FreeBSD's `iommu_map_msi`), and the
//! PCI layer turns the message address into that mapping
//! (`iommu_translate_msi`; docs/IOMMU.md §4.8, §17).
//!
//! The page-table writes of [`domain`] are this crate's only `unsafe`.
//!
//! Behavior reference: sys/dev/iommu/busdma_iommu.c (`iommu_init_busdma`,
//! `iommu_bus_dma_is_dev_disabled`), sys/dev/iommu/iommu_gas.c, the user's
//! sys/arm64/iommu/iommu.c (branch orangepi-6-plus).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod domain;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::sysctl::ctlflag;
use sysctl::{Number, Tree, Value};

/// `hw.iommu.dma`, or -1 before [`set_tunables`]: the ISA's default.
static DMA: AtomicI32 = AtomicI32::new(-1);
static RELAXED_ORDERING: AtomicI32 = AtomicI32::new(0);
static CHECK_FREE: AtomicI32 = AtomicI32::new(0);
/// The kernel environment, for `hw.busdma.*`.
static ENV: sync::Once<fn(&str) -> Option<&'static str>> = sync::Once::new();

/// `hw.iommu.dma`'s default: on for arm64 (docs/IOMMU.md §13 decision 2),
/// off elsewhere (FreeBSD's).
pub const DMA_DEFAULT: bool = cfg!(target_arch = "aarch64");

/// Takes the boot-time tunables (`TUNABLE_INT_FETCH`): `get` gives a
/// name's integer value, if set. The kernel calls it before units attach.
pub fn set_tunables(get: &dyn Fn(&str) -> Option<i32>) {
    // Relaxed: tunables, set before any reader runs.
    DMA.store(
        get("hw.iommu.dma").unwrap_or(i32::from(DMA_DEFAULT)),
        Ordering::Relaxed,
    );
    if let Some(v) = get("hw.iommu.relaxed_ordering") {
        RELAXED_ORDERING.store(v, Ordering::Relaxed);
    }
    if let Some(v) = get("hw.iommu.check_free") {
        CHECK_FREE.store(v, Ordering::Relaxed);
    }
}

/// Whether units translate DMA (`hw.iommu.dma` non-zero).
#[must_use]
pub fn dma() -> bool {
    match DMA.load(Ordering::Relaxed) {
        -1 => DMA_DEFAULT,
        v => v != 0,
    }
}

/// Whether translated PCIe functions keep relaxed ordering.
#[must_use]
pub fn relaxed_ordering() -> bool {
    RELAXED_ORDERING.load(Ordering::Relaxed) != 0
}

/// Gives the framework the kernel environment, for `hw.busdma.*`.
pub fn set_env(get: fn(&str) -> Option<&'static str>) {
    let _ = ENV.set(get);
}

/// Whether the requester `rid` in PCI segment `segment` is to have a
/// pass-through context: FreeBSD's `iommu_bus_dma_is_dev_disabled`, from
/// `hw.busdma.pciD.B.S.F`, else `hw.busdma.default` (`bounce` disables,
/// `iommu` and `dmar` translate; anything else keeps the default, which
/// is to translate).
#[must_use]
pub fn requester_disabled(segment: u32, rid: u16) -> bool {
    let Some(get) = ENV.get() else {
        return false;
    };
    disabled_by(*get, segment, rid)
}

/// [`requester_disabled`] over the environment `get`.
pub(crate) fn disabled_by(
    get: impl Fn(&str) -> Option<&'static str>,
    segment: u32,
    rid: u16,
) -> bool {
    let decide = |v: Option<&str>, default: bool| match v {
        Some("bounce") => true,
        Some("iommu" | "dmar") => false,
        _ => default,
    };
    let default = decide(get("hw.busdma.default"), false);
    let (bus, slot, func) = (rid >> 8, (rid >> 3) & 0x1f, rid & 7);
    let key = alloc::format!("hw.busdma.pci{segment}.{bus}.{slot}.{func}");
    decide(get(&key), default)
}

/// Adds `hw.iommu` and its knobs to `t`.
pub fn sysctl_init(t: &mut Tree) {
    let root = t.root();
    let Some(hw) = t.child(root, "hw") else {
        return;
    };
    let Some(node) = t
        .child(hw, "iommu")
        .or_else(|| t.add_node(hw, Number::Auto, "iommu", "").ok())
    else {
        return;
    };
    if let Ok(id) = t.add_value(
        node,
        Number::Auto,
        "relaxed_ordering",
        "Leave PCIe relaxed ordering enabled on devices the IOMMU translates",
        Value::Int(Box::new(|| RELAXED_ORDERING.load(Ordering::Relaxed))),
    ) {
        t.set_flags(id, ctlflag::TUN);
    }
    let _ = t.add_writable(
        node,
        Number::Auto,
        "check_free",
        "Check the GPA RBtree for free_down and free_after validity",
        ctlflag::TUN,
        Value::Int(Box::new(|| CHECK_FREE.load(Ordering::Relaxed))),
        sysctl::int_writer(|v| {
            CHECK_FREE.store(v, Ordering::Relaxed);
            Ok(())
        }),
    );
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn tunables_and_sysctls() {
        set_tunables(&|n| (n == "hw.iommu.relaxed_ordering").then_some(1));
        assert_eq!(dma(), DMA_DEFAULT);
        assert!(relaxed_ordering());
        set_tunables(&|n| (n == "hw.iommu.dma").then_some(0));
        assert!(!dma());
        let mut t = Tree::new();
        sysctl_init(&mut t);
        let oid = t.name2oid("hw.iommu.check_free").expect("check_free");
        assert_eq!(oid.len(), 3);
        assert!(t.name2oid("hw.iommu.relaxed_ordering").is_some());
    }

    #[test]
    fn busdma_knobs() {
        // rid 0x18 is 0:3:0.
        let none = |_: &str| None;
        assert!(!disabled_by(none, 0, 0x18));
        let def = |k: &str| (k == "hw.busdma.default").then_some("bounce");
        assert!(disabled_by(def, 0, 0x18));
        let one = |k: &str| match k {
            "hw.busdma.default" => Some("bounce"),
            "hw.busdma.pci0.0.3.0" => Some("iommu"),
            "hw.busdma.pci1.193.0.1" => Some("bounce"),
            _ => None,
        };
        assert!(!disabled_by(one, 0, 0x18));
        assert!(disabled_by(one, 0, 0x20));
        assert!(disabled_by(one, 1, 0xc101));
        let odd = |k: &str| (k == "hw.busdma.pci0.0.3.0").then_some("dmar");
        assert!(!disabled_by(odd, 0, 0x18));
    }
}
