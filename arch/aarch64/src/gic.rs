// SPDX-License-Identifier: BSD-2-Clause
//! The GICv3 interrupt controller: the distributor (shared peripheral
//! interrupts), one redistributor per CPU (private interrupts), and the
//! system-register CPU interface.
//!
//! All interrupts are non-secure Group 1, routed with affinity routing.
//! Interrupt IDs: 0–15 SGIs (IPIs), 16–31 PPIs (per-CPU devices such as
//! the timer), 32–1019 SPIs, and from 8192 LPIs (message-signalled, through
//! an ITS), configured in memory tables each redistributor points at.
//!
//! Behavior reference: sys/arm64/arm64/gic_v3.c (the GICv3 architecture
//! specification, IHI 0069, for the registers).

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use regs::{Bits, Field, Mmio};

regs::register_block! {
    /// The distributor (IHI 0069 §12.9).
    struct Gicd: 0x1_0000 {
        CTLR @ 0x0000 => u32, Rw;
        TYPER @ 0x0004 => u32, Ro;
        IIDR @ 0x0008 => u32, Ro;
        IGROUPR @ 0x0080 => [u32; 32], Rw;
        ISENABLER @ 0x0100 => [u32; 32], Rw;
        ICENABLER @ 0x0180 => [u32; 32], Rw;
        IPRIORITYR @ 0x0400 => [u8; 1020], Rw;
        ICFGR @ 0x0c00 => [u32; 64], Rw;
        IROUTER @ 0x6000 => [u64; 1020], Rw;
    }
}

regs::register_block! {
    /// A redistributor's control frame (IHI 0069 §12.11).
    struct GicrRd: 0x1_0000 {
        CTLR @ 0x0000 => u32, Rw;
        TYPER @ 0x0008 => u64, Ro;
        WAKER @ 0x0014 => u32, Rw;
        PROPBASER @ 0x0070 => u64, Rw;
        PENDBASER @ 0x0078 => u64, Rw;
    }
}

regs::register_block! {
    /// A redistributor's SGI and PPI frame, the second 64 KiB (§12.11).
    struct GicrSgi: 0x1_0000 {
        IGROUPR0 @ 0x0080 => u32, Rw;
        ISENABLER0 @ 0x0100 => u32, Rw;
        ICENABLER0 @ 0x0180 => u32, Rw;
        IPRIORITYR @ 0x0400 => [u8; 32], Rw;
        ICFGR @ 0x0c00 => [u32; 2], Rw;
    }
}

const CTLR_RWP: Field<u32> = Field::new(31, 1);
const CTLR_ARE_NS: Field<u32> = Field::new(4, 1);
const CTLR_ENABLE_G1: Field<u32> = Field::new(1, 1);
const TYPER_LINES: Field<u32> = Field::new(0, 5);
const TYPER_LPIS: Field<u32> = Field::new(17, 1);
const TYPER_ID_BITS: Field<u32> = Field::new(19, 5);
const RCTLR_ENABLE_LPIS: Field<u32> = Field::new(0, 1);
const RTYPER_PLPIS: Field<u64> = Field::new(0, 1);
const RTYPER_PROCESSOR: Field<u64> = Field::new(8, 16);
const BASER_SHAREABILITY: Field<u64> = Field::new(10, 2);
const RTYPER_VLPIS: Field<u64> = Field::new(1, 1);
const RTYPER_LAST: Field<u64> = Field::new(4, 1);
const RTYPER_AFFINITY: Field<u64> = Field::new(32, 32);
const WAKER_SLEEP: Field<u32> = Field::new(1, 1);
const WAKER_ASLEEP: Field<u32> = Field::new(2, 1);

/// A redistributor's frames: control, then SGI and PPI.
const GICR_FRAME: u64 = 0x1_0000;

/// Default priority for every interrupt (lower is more urgent).
pub const PRIORITY: u8 = 0xa0;

/// The first LPI.
pub const FIRST_LPI: u32 = 8192;

/// Attributes of memory the GIC reads tables from: inner shareable,
/// write-back cacheable (outer as inner).
pub const TABLE_ATTRS: u64 = (0b111 << 7) | (0b01 << 10);

/// First and one-past-last spurious interrupt IDs.
const SPURIOUS: core::ops::Range<u32> = 1020..1024;

/// The distributor window (kernel virtual address). Set once at boot.
static GICD: AtomicU64 = AtomicU64::new(0);

/// The distributor, once [`init_distributor`] has recorded it.
fn gicd() -> Mmio<Gicd> {
    // SAFETY: `init_distributor`'s caller mapped the window for good.
    unsafe { Mmio::block(va(GICD.load(Ordering::Relaxed))) }
}

/// The redistributor frames at `gicr`.
///
/// # Safety
///
/// `gicr` maps a redistributor (both frames) for as long as the values
/// are used.
unsafe fn redistributor(gicr: u64) -> (Mmio<GicrRd>, Mmio<GicrSgi>) {
    // SAFETY: per the contract.
    unsafe { (Mmio::block(va(gicr)), Mmio::block(va(gicr + GICR_FRAME))) }
}

/// A kernel virtual address as a pointer-sized integer (lossless on this
/// 64-bit target).
#[allow(clippy::cast_possible_truncation)]
const fn va(a: u64) -> usize {
    a as usize
}

/// Bit `id % 32`, for the 32-bit enable registers.
fn bit(id: u32) -> u32 {
    1 << (id % 32)
}

/// The `ICFGR` value with interrupt `id` set edge- or level-triggered.
fn icfg(cfg: u32, id: u32, edge: bool) -> u32 {
    let shift = (id % 16) * 2;
    cfg.with(Field::new(shift + 1, 1), u32::from(edge))
}

fn index(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
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
    let d = self::gicd();
    d.write(Gicd::CTLR, 0);
    wait_rwp(d);
    let lines = ((d.read(Gicd::TYPER).field(TYPER_LINES) + 1) * 32).min(1020);
    for word in 1..index(lines / 32) {
        d.write(Gicd::IGROUPR.at(word), u32::MAX);
        d.write(Gicd::ICENABLER.at(word), u32::MAX);
    }
    for i in 32..index(lines) {
        d.write(Gicd::IPRIORITYR.at(i), PRIORITY);
        d.write(Gicd::IROUTER.at(i), affinity(boot_mpidr));
    }
    d.write(
        Gicd::CTLR,
        0u32.with(CTLR_ARE_NS, 1).with(CTLR_ENABLE_G1, 1),
    );
    wait_rwp(d);
    lines
}

fn wait_rwp(d: Mmio<Gicd>) {
    while d.read(Gicd::CTLR).has(CTLR_RWP) {
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
    gicd().read(Gicd::IIDR)
}

/// Finds the redistributor for the CPU with `mpidr` in the region at
/// `base` (a kernel virtual address) of `len` bytes.
#[must_use]
pub fn find_redistributor(base: u64, len: u64, mpidr: u64) -> Option<u64> {
    // GICR_TYPER's affinity is Aff3.Aff2.Aff1.Aff0 in 32 bits.
    let want = (((mpidr >> 32) & 0xff) << 24) | (mpidr & 0xff_ffff);
    let mut rd_base = base;
    while rd_base + 2 * GICR_FRAME <= base + len {
        // SAFETY: a mapped redistributor in the region.
        let (r, _) = unsafe { redistributor(rd_base) };
        let typer = r.read(GicrRd::TYPER);
        if typer.field(RTYPER_AFFINITY) == want {
            return Some(rd_base);
        }
        if typer.has(RTYPER_LAST) {
            return None;
        }
        // With virtual LPIs, two more frames follow.
        rd_base += if typer.has(RTYPER_VLPIS) { 4 } else { 2 } * GICR_FRAME;
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
    // SAFETY: per our contract.
    let (r, sgi) = unsafe { redistributor(gicr) };
    r.modify(GicrRd::WAKER, |w| w.with(WAKER_SLEEP, 0));
    while r.read(GicrRd::WAKER).has(WAKER_ASLEEP) {
        core::hint::spin_loop();
    }
    sgi.write(GicrSgi::IGROUPR0, u32::MAX);
    sgi.write(GicrSgi::ICENABLER0, u32::MAX);
    for i in 0..GicrSgi::IPRIORITYR.len() {
        sgi.write(GicrSgi::IPRIORITYR.at(i), PRIORITY);
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

/// The interrupt ID bits the distributor implements, if it supports LPIs.
#[must_use]
pub fn lpi_id_bits() -> Option<u32> {
    let t = gicd().read(Gicd::TYPER);
    t.has(TYPER_LPIS).then(|| t.field(TYPER_ID_BITS) + 1)
}

/// Why a redistributor's LPIs were not enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LpiError {
    /// The redistributor has no physical LPIs.
    Unsupported,
    /// Firmware left LPIs enabled, with tables we cannot replace (the
    /// tables may not change while they are on).
    AlreadyEnabled,
}

/// The redistributor's processor number (an ITS without physical target
/// addresses names redistributors by it).
#[must_use]
pub fn processor_number(gicr: u64) -> u16 {
    // SAFETY: the caller passes a mapped redistributor.
    let (r, _) = unsafe { redistributor(gicr) };
    // A 16-bit field.
    u16::try_from(r.read(GicrRd::TYPER).field(RTYPER_PROCESSOR)).unwrap_or(0)
}

/// Points the redistributor at `gicr` at the LPI configuration table at
/// physical `prop` (for `id_bits` interrupt ID bits) and its pending table
/// at physical `pend` (zeroed, 64 KiB aligned), then enables LPIs. Returns
/// whether the GIC snoops the tables (it kept inner shareability); if not,
/// writes to the configuration table must be cleaned from the caches.
///
/// # Safety
///
/// `gicr` maps a redistributor; the tables are physically contiguous,
/// large enough for `id_bits`, owned by the GIC from now on, and written
/// (the configuration table) before this call.
pub unsafe fn enable_lpis(gicr: u64, prop: u64, id_bits: u32, pend: u64) -> Result<bool, LpiError> {
    // SAFETY: per the contract.
    let (r, _) = unsafe { redistributor(gicr) };
    if !r.read(GicrRd::TYPER).has(RTYPER_PLPIS) {
        return Err(LpiError::Unsupported);
    }
    if r.read(GicrRd::CTLR).has(RCTLR_ENABLE_LPIS) {
        return Err(LpiError::AlreadyEnabled);
    }
    r.write(
        GicrRd::PROPBASER,
        (prop & 0x000f_ffff_ffff_f000) | TABLE_ATTRS | u64::from(id_bits - 1),
    );
    let snooped = r.read(GicrRd::PROPBASER).field(BASER_SHAREABILITY) != 0;
    // PTZ: the pending table is zero.
    r.write(
        GicrRd::PENDBASER,
        (1 << 62) | (pend & 0x000f_ffff_ffff_0000) | TABLE_ATTRS,
    );
    // The tables' contents before the GIC may read them.
    crate::cpu::write_barrier();
    r.modify(GicrRd::CTLR, |c| c.with(RCTLR_ENABLE_LPIS, 1));
    Ok(snooped)
}

/// Enables a private interrupt (SGI or PPI, `id < 32`) on this CPU, whose
/// redistributor is at `gicr`.
pub fn enable_private(gicr: u64, id: u32, edge: bool) {
    // SAFETY: the caller passes this CPU's mapped redistributor.
    let (_, sgi) = unsafe { redistributor(gicr) };
    if (16..32).contains(&id) {
        // SGIs are always edge-triggered; PPIs are configured in ICFGR1.
        sgi.modify(GicrSgi::ICFGR.at(1), |c| icfg(c, id, edge));
    }
    sgi.write(GicrSgi::ISENABLER0, bit(id));
}

/// Sets shared interrupt `id` (32 or above) edge- or level-triggered.
pub fn configure_spi(id: u32, edge: bool) {
    gicd().modify(Gicd::ICFGR.at(index(id / 16)), |c| icfg(c, id, edge));
}

/// Routes shared interrupt `id` to the CPU with `mpidr`.
pub fn route_spi(id: u32, mpidr: u64) {
    gicd().write(Gicd::IROUTER.at(index(id)), affinity(mpidr));
}

/// Enables or disables shared interrupt `id`.
pub fn set_spi_enabled(id: u32, enabled: bool) {
    let d = gicd();
    if enabled {
        d.write(Gicd::ISENABLER.at(index(id / 32)), bit(id));
    } else {
        d.write(Gicd::ICENABLER.at(index(id / 32)), bit(id));
    }
}

/// Enables shared interrupt `id` (32 or above) in the distributor.
pub fn enable_shared(id: u32, edge: bool) {
    let d = gicd();
    d.modify(Gicd::ICFGR.at(index(id / 16)), |c| icfg(c, id, edge));
    d.write(Gicd::ISENABLER.at(index(id / 32)), bit(id));
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
