// SPDX-License-Identifier: BSD-2-Clause
//! The IOMMU framework (docs/IOMMU.md §4.1): FreeBSD's sys/dev/iommu and
//! arm64/iommu/iommu.c. So far its knobs (IOMMU.md §4.12); units,
//! domains, contexts, and I/O page tables come with stage 3.
//!
//! - `hw.iommu.dma` (tunable): DMA translation. 1 by default on arm64,
//!   where translation is what keeps a board's DMA correct (FreeBSD's
//!   default is 0; docs/compat-deviations.md); 0 elsewhere, as FreeBSD.
//! - `hw.iommu.relaxed_ordering` (`CTLFLAG_RDTUN`, 0): keep PCIe relaxed
//!   ordering on functions the IOMMU translates (the board's FreeBSD).
//! - `hw.iommu.check_free` (`CTLFLAG_RWTUN`, 0): FreeBSD's `iommu_gas`
//!   self-check, kept for parity.
//!
//! Behavior reference: sys/dev/iommu/busdma_iommu.c (`iommu_init_busdma`),
//! sys/dev/iommu/iommu_gas.c, the user's sys/arm64/iommu/iommu.c
//! (branch orangepi-6-plus).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::sysctl::ctlflag;
use sysctl::{Number, Tree, Value};

/// `hw.iommu.dma`, or -1 before [`set_tunables`]: the ISA's default.
static DMA: AtomicI32 = AtomicI32::new(-1);
static RELAXED_ORDERING: AtomicI32 = AtomicI32::new(0);
static CHECK_FREE: AtomicI32 = AtomicI32::new(0);

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
}
