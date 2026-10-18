// SPDX-License-Identifier: BSD-2-Clause
//! CPU errata: a table keyed by CPU identity (MIDR on aarch64, CPUID
//! signature on x86) of known defects, checked on every CPU as it comes
//! up. Each entry either applies a workaround or records why none is
//! needed yet; matches are reported once per kind of CPU.
//!
//! Interrupt-controller errata are keyed by the controller's own ID
//! register and live with its driver; for the record, the GICv3 driver
//! logs `GICD_IIDR` at boot. (Arm erratum 2941627, flagged for the Sky1's
//! GIC-700, is to be assessed against Arm's published errata notice when
//! that hardware is brought up; no table entry guesses at it.)
//!
//! Behavior reference: sys/arm64/arm64/cpu_errata.c, sys/x86/x86/
//! cpu_machdep.c.

use hal::CpuId;
use sync::SpinLock;

use crate::topology::Ident;

/// What to do about an erratum.
#[derive(Clone, Copy, Debug)]
enum Action {
    /// Run this on each affected CPU.
    #[allow(dead_code, reason = "no erratum in the table needs one yet")]
    Workaround(fn()),
    /// Nothing yet, and why.
    Deferred(&'static str),
}

#[derive(Clone, Copy, Debug)]
struct Erratum {
    /// The vendor's erratum number or name.
    id: &'static str,
    summary: &'static str,
    affects: fn(&Ident) -> bool,
    action: Action,
}

const fn arm(part: u16) -> impl Fn(&Ident) -> bool {
    move |i: &Ident| matches!(i, Ident::Arm { implementer: 0x41, part: p, .. } if *p == part)
}

static TABLE: &[Erratum] = &[
    Erratum {
        id: "Cortex-A520 2966298",
        summary: "speculatively executed unprivileged loads may leak data",
        affects: |i| arm(0xd80)(i),
        action: Action::Deferred("matters once user mode exists (Phase 4)"),
    },
    Erratum {
        id: "Cortex-A76 1165522",
        summary: "speculative address translation can use stale entries",
        affects: |i| arm(0xd0b)(i),
        action: Action::Deferred("matters once address spaces switch (Phase 4)"),
    },
];

/// Kinds of CPU already reported, so each is reported once.
static SEEN: SpinLock<[Option<Ident>; 8]> = SpinLock::new([None; 8]);

/// Checks this CPU against the table and applies workarounds.
pub(crate) fn apply(cpu: CpuId, ident: &Ident) {
    let first = {
        let mut seen = SEEN.lock();
        if seen.iter().flatten().any(|s| s == ident) {
            false
        } else {
            if let Some(slot) = seen.iter_mut().find(|s| s.is_none()) {
                *slot = Some(*ident);
            }
            true
        }
    };
    for e in TABLE.iter().filter(|e| (e.affects)(ident)) {
        match e.action {
            Action::Workaround(f) => f(),
            Action::Deferred(_) => {}
        }
        if first {
            let note = match e.action {
                Action::Workaround(_) => "workaround applied",
                Action::Deferred(why) => why,
            };
            kprintln!("errata: {} ({cpu}): {}: {note}", e.id, e.summary);
        }
    }
    if first {
        kprintln!("cpu: {cpu}: {ident}");
    }
}
