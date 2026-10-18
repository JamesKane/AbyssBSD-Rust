// SPDX-License-Identifier: BSD-2-Clause
//! The GICv3 interrupt controller: the distributor (shared peripheral
//! interrupts), one redistributor per CPU (private interrupts), and the
//! system-register CPU interface.
//!
//! All interrupts are non-secure Group 1, routed with affinity routing.
//! Interrupt IDs: 0–15 SGIs (IPIs), 16–31 PPIs (per-CPU devices such as
//! the timer), 32–1019 SPIs.
//!
//! Behavior reference: sys/arm64/arm64/gic_v3.c (the GICv3 architecture
//! specification, IHI 0069, for the registers).

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

const GICD_CTLR: usize = 0x0;
const GICD_TYPER: usize = 0x4;
const GICD_IIDR: usize = 0x8;
const GICD_IGROUPR: usize = 0x80;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ICFGR: usize = 0xc00;
const GICD_IROUTER: usize = 0x6000;
const GICD_CTLR_RWP: u32 = 1 << 31;
const GICD_CTLR_ARE_NS: u32 = 1 << 4;
const GICD_CTLR_ENABLE_G1: u32 = 1 << 1;

const GICR_TYPER: usize = 0x8;
const GICR_WAKER: usize = 0x14;
const GICR_SGI: usize = 0x1_0000;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_TYPER_VLPIS: u64 = 1 << 1;
const GICR_WAKER_SLEEP: u32 = 1 << 1;
const GICR_WAKER_ASLEEP: u32 = 1 << 2;

/// Default priority for every interrupt (lower is more urgent).
const PRIORITY: u8 = 0xa0;

/// First and one-past-last spurious interrupt IDs.
const SPURIOUS: core::ops::Range<u32> = 1020..1024;

/// The distributor window (kernel virtual address). Set once at boot.
static GICD: AtomicU64 = AtomicU64::new(0);

fn mmio32(base: u64, off: usize) -> *mut u32 {
    (base + off as u64) as *mut u32
}

fn rd(base: u64, off: usize) -> u32 {
    // SAFETY: callers pass a mapped GIC window and a register offset in it.
    unsafe { mmio32(base, off).read_volatile() }
}

fn wr(base: u64, off: usize, v: u32) {
    // SAFETY: as in `rd`.
    unsafe { mmio32(base, off).write_volatile(v) }
}

/// Sets up the distributor: all SPIs Group 1, disabled, default
/// priority, routed to the CPU with affinity `boot_mpidr`.
///
/// # Safety
///
/// `gicd` must map the distributor uncached for the kernel's lifetime.
/// Call once, on the boot CPU, with interrupts masked.
pub unsafe fn init_distributor(gicd: u64, boot_mpidr: u64) -> u32 {
    GICD.store(gicd, Ordering::Relaxed);
    wr(gicd, GICD_CTLR, 0);
    wait_rwp(gicd);
    let lines = ((rd(gicd, GICD_TYPER) & 0x1f) + 1) * 32;
    let lines = lines.min(1020);
    let mut i = 32;
    while i < lines {
        let word = usize::try_from(i / 32).unwrap_or(0) * 4;
        wr(gicd, GICD_IGROUPR + word, u32::MAX);
        wr(gicd, GICD_ICENABLER + word, u32::MAX);
        i += 32;
    }
    for i in 32..lines {
        let i = usize::try_from(i).unwrap_or(0);
        // SAFETY: byte-wide priority registers within the window.
        unsafe { ((gicd + (GICD_IPRIORITYR + i) as u64) as *mut u8).write_volatile(PRIORITY) };
        // SAFETY: 64-bit routing registers within the window.
        unsafe {
            ((gicd + (GICD_IROUTER + 8 * i) as u64) as *mut u64)
                .write_volatile(affinity(boot_mpidr));
        }
    }
    wr(gicd, GICD_CTLR, GICD_CTLR_ARE_NS | GICD_CTLR_ENABLE_G1);
    wait_rwp(gicd);
    lines
}

fn wait_rwp(gicd: u64) {
    while rd(gicd, GICD_CTLR) & GICD_CTLR_RWP != 0 {
        core::hint::spin_loop();
    }
}

/// The affinity fields of an MPIDR, in IROUTER / GICR_TYPER order.
fn affinity(mpidr: u64) -> u64 {
    mpidr & 0xff_00ff_ffff
}

/// The distributor's implementer and product (`GICD_IIDR`), which keys
/// interrupt-controller errata.
#[must_use]
pub fn distributor_iidr() -> u32 {
    rd(GICD.load(Ordering::Relaxed), GICD_IIDR)
}

/// Finds the redistributor for the CPU with `mpidr` in the region at
/// `base` (a kernel virtual address) of `len` bytes.
#[must_use]
pub fn find_redistributor(base: u64, len: u64, mpidr: u64) -> Option<u64> {
    // GICR_TYPER's affinity is Aff3.Aff2.Aff1.Aff0 in 32 bits.
    let want = (((mpidr >> 32) & 0xff) << 24) | (mpidr & 0xff_ffff);
    let mut rd_base = base;
    while rd_base + 0x2_0000 <= base + len {
        // SAFETY: a mapped redistributor frame in the region.
        let typer = unsafe { ((rd_base + GICR_TYPER as u64) as *const u64).read_volatile() };
        if typer >> 32 == want {
            return Some(rd_base);
        }
        if typer & GICR_TYPER_LAST != 0 {
            return None;
        }
        rd_base += if typer & GICR_TYPER_VLPIS != 0 {
            0x4_0000
        } else {
            0x2_0000
        };
    }
    None
}

/// Wakes this CPU's redistributor, configures its SGIs and PPIs (Group 1,
/// disabled, default priority), and enables the CPU interface.
///
/// # Safety
///
/// `gicr` must map this CPU's redistributor uncached for the kernel's
/// lifetime. Call once per CPU, with interrupts masked, after
/// [`init_distributor`].
pub unsafe fn init_cpu(gicr: u64) {
    let waker = rd(gicr, GICR_WAKER);
    wr(gicr, GICR_WAKER, waker & !GICR_WAKER_SLEEP);
    while rd(gicr, GICR_WAKER) & GICR_WAKER_ASLEEP != 0 {
        core::hint::spin_loop();
    }
    let sgi = gicr + GICR_SGI as u64;
    wr(sgi, GICD_IGROUPR, u32::MAX);
    wr(sgi, GICD_ICENABLER, u32::MAX);
    for i in 0..32 {
        // SAFETY: byte-wide priority registers in the SGI frame.
        unsafe { ((sgi + (GICD_IPRIORITYR + i) as u64) as *mut u8).write_volatile(PRIORITY) };
    }
    // SAFETY: the CPU interface system registers exist on a GICv3 system;
    // enabling them is what the caller asked for.
    unsafe {
        // ICC_SRE_EL1.SRE: use the system-register interface.
        asm!(
            "mrs {t}, S3_0_C12_C12_5",
            "orr {t}, {t}, #1",
            "msr S3_0_C12_C12_5, {t}",
            "isb",
            t = out(reg) _,
            options(nostack),
        );
        // ICC_PMR_EL1: accept every priority. ICC_BPR1_EL1: no grouping.
        asm!("msr S3_0_C4_C6_0, {}", in(reg) 0xffu64, options(nostack));
        asm!("msr S3_0_C12_C12_3, {}", in(reg) 0u64, options(nostack));
        // ICC_CTLR_EL1.EOImode = 0: EOIR both drops priority and
        // deactivates.
        asm!(
            "mrs {t}, S3_0_C12_C12_4",
            "bic {t}, {t}, #2",
            "msr S3_0_C12_C12_4, {t}",
            t = out(reg) _,
            options(nostack),
        );
        // ICC_IGRPEN1_EL1: enable Group 1.
        asm!("msr S3_0_C12_C12_7, {}", "isb", in(reg) 1u64, options(nostack));
    }
}

/// Enables a private interrupt (SGI or PPI, `id < 32`) on this CPU, whose
/// redistributor is at `gicr`.
pub fn enable_private(gicr: u64, id: u32, edge: bool) {
    let sgi = gicr + GICR_SGI as u64;
    if (16..32).contains(&id) {
        let shift = (id % 16) * 2;
        let cfg = rd(sgi, GICD_ICFGR + 4);
        let cfg = if edge {
            cfg | (2 << shift)
        } else {
            cfg & !(2 << shift)
        };
        wr(sgi, GICD_ICFGR + 4, cfg);
    }
    wr(sgi, GICD_ISENABLER, 1 << (id % 32));
}

/// Enables shared interrupt `id` (32 or above) in the distributor.
pub fn enable_shared(id: u32, edge: bool) {
    let gicd = GICD.load(Ordering::Relaxed);
    let reg = usize::try_from(id / 16).unwrap_or(0) * 4;
    let shift = (id % 16) * 2;
    let cfg = rd(gicd, GICD_ICFGR + reg);
    let cfg = if edge {
        cfg | (2 << shift)
    } else {
        cfg & !(2 << shift)
    };
    wr(gicd, GICD_ICFGR + reg, cfg);
    wr(
        gicd,
        GICD_ISENABLER + usize::try_from(id / 32).unwrap_or(0) * 4,
        1 << (id % 32),
    );
}

/// Acknowledges the highest-priority pending interrupt, if any.
#[must_use]
pub fn ack() -> Option<u32> {
    let id: u64;
    // SAFETY: reading ICC_IAR1_EL1 acknowledges; the caller must EOI.
    unsafe { asm!("mrs {}, S3_0_C12_C12_0", out(reg) id, options(nostack)) };
    let id = u32::try_from(id & 0xff_ffff).unwrap_or(1023);
    (!SPURIOUS.contains(&id)).then_some(id)
}

/// Ends interrupt `id` (from [`ack`]).
pub fn eoi(id: u32) {
    // SAFETY: writing ICC_EOIR1_EL1 for an acknowledged interrupt.
    unsafe { asm!("msr S3_0_C12_C12_1, {}", in(reg) u64::from(id), options(nostack)) };
}

/// Sends SGI `id` (0–15) to the CPU with `mpidr`.
pub fn send_sgi(mpidr: u64, id: u8) {
    let aff0 = mpidr & 0xff;
    let aff1 = (mpidr >> 8) & 0xff;
    let aff2 = (mpidr >> 16) & 0xff;
    let aff3 = (mpidr >> 32) & 0xff;
    // TargetList holds Aff0 modulo 16; RS selects the block of 16.
    let v = (1u64 << (aff0 % 16))
        | (aff1 << 16)
        | (u64::from(id & 0xf) << 24)
        | (aff2 << 32)
        | ((aff0 / 16) << 44)
        | (aff3 << 48);
    // SAFETY: writing ICC_SGI1R_EL1 generates the SGI; the barrier makes
    // earlier stores visible to the target first.
    unsafe { asm!("dsb ishst", "msr S3_0_C12_C11_5, {}", "isb", in(reg) v, options(nostack)) };
}
