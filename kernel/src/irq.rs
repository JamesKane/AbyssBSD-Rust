// SPDX-License-Identifier: BSD-2-Clause
//! Interrupt controllers and dispatch.
//!
//! x86_64: the local APIC (x2APIC when available) on every CPU, I/O APICs
//! masked until drivers claim inputs, legacy PICs masked. The TSC is
//! calibrated against the ACPI PM timer.
//!
//! aarch64: GICv3, with the distributor routing to the boot CPU and one
//! redistributor per CPU; the virtual timer's PPI comes from the GTDT.
//!
//! Every interrupt ends with a preemption check, so a thread woken by an
//! interrupt (or a slice that ran out) gets the CPU on the way out.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use acpi_tables::madt::{self, MadtEntry};
use addr::PhysAddr;
use hal::{CpuId, MAX_CPUS};

use crate::arch;

/// Each CPU's interrupt-controller ID (APIC ID, or MPIDR affinity).
static HW_ID: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];
/// Interrupts nobody handled, for diagnostics.
static STRAY: AtomicU32 = AtomicU32::new(0);

/// The interrupt-controller ID of `cpu`.
pub(crate) fn hw_id(cpu: CpuId) -> Option<u64> {
    let v = HW_ID.get(cpu.index())?.load(Ordering::Acquire);
    (v != u64::MAX).then_some(v)
}

fn madt() -> Option<madt::Madt<'static>> {
    let t = crate::acpi::find(*b"APIC")?;
    madt::parse(&t).ok()
}

/// Records `cpu`'s interrupt-controller ID (before starting it).
pub(crate) fn set_hw_id(cpu: CpuId, id: u64) {
    HW_ID[cpu.index()].store(id, Ordering::Release);
}

/// Sends the cross-call IPI (TLB shootdowns, pings) to `cpu`.
pub(crate) fn send_call(cpu: CpuId) {
    let Some(id) = hw_id(cpu) else {
        return;
    };
    #[cfg(target_arch = "x86_64")]
    arch::apic::send_ipi(u32::try_from(id).unwrap_or(0), CALL_VECTOR);
    #[cfg(target_arch = "aarch64")]
    arch::gic::send_sgi(id, CALL_SGI);
}

/// Sends the cross-call IPI to every CPU but this one, with one write
/// (x86's destination shorthand). Every CPU that can take it must be
/// online: see `smp::BROADCAST`.
#[cfg(target_arch = "x86_64")]
pub(crate) fn send_call_all_but_self() {
    arch::apic::send_ipi_all_but_self(CALL_VECTOR);
}

/// Asks `cpu` to reschedule.
pub(crate) fn send_resched(cpu: CpuId) {
    let Some(id) = hw_id(cpu) else {
        return;
    };
    #[cfg(target_arch = "x86_64")]
    arch::apic::send_ipi(u32::try_from(id).unwrap_or(0), RESCHED_VECTOR);
    #[cfg(target_arch = "aarch64")]
    arch::gic::send_sgi(id, RESCHED_SGI);
}

/// Handles one interrupt (from the arch trap code, interrupts masked),
/// then switches threads if the handler asked for it.
pub(crate) fn dispatch(irq: arch::Interrupt) {
    hal::interrupt::enter();
    handle(irq);
    hal::interrupt::exit();
    sched::preempt_point();
}

fn handle(irq: arch::Interrupt) {
    // SAFETY: we are the interrupt dispatcher, interrupts masked.
    let cx = unsafe { sched::IrqCx::assume() };
    #[cfg(target_arch = "x86_64")]
    {
        match irq.vector {
            arch::apic::SPURIOUS_VECTOR => return,
            arch::apic::TIMER_VECTOR => crate::time::on_timer(&cx),
            // The waker set `need_resched` if the newcomer should preempt;
            // either way, re-arm for the changed run queue.
            RESCHED_VECTOR => crate::time::rearm(),
            CALL_VECTOR => crate::smp::on_call_ipi(),
            arch::apic::ERROR_VECTOR => kprintln!("apic: error interrupt"),
            v => stray(u32::from(v)),
        }
        arch::apic::eoi();
    }
    #[cfg(target_arch = "aarch64")]
    {
        let _ = irq.from_user;
        let Some(id) = arch::gic::ack() else {
            return;
        };
        if id == u32::from(RESCHED_SGI) {
            crate::time::rearm();
        } else if id == u32::from(CALL_SGI) {
            crate::smp::on_call_ipi();
        } else if id == TIMER_PPI.load(Ordering::Relaxed) {
            crate::time::on_timer(&cx);
        } else {
            stray(id);
        }
        arch::gic::eoi(id);
    }
}

fn stray(id: u32) {
    if STRAY.fetch_add(1, Ordering::Relaxed) < 8 {
        kprintln!("irq: unexpected interrupt {id}");
    }
}

#[cfg(target_arch = "x86_64")]
const RESCHED_VECTOR: u8 = arch::apic::IPI_BASE;
#[cfg(target_arch = "x86_64")]
const CALL_VECTOR: u8 = arch::apic::IPI_BASE + 1;

/// Sets up this (non-boot) CPU's local APIC.
#[cfg(target_arch = "x86_64")]
pub(crate) fn init_ap(_cpu: CpuId) {
    // SAFETY: once per CPU, interrupts masked, after the boot CPU chose
    // the register interface.
    unsafe { arch::apic::init_cpu() };
    arch::apic::configure_timer();
}

#[cfg(target_arch = "x86_64")]
static IOAPICS: sync::SpinLock<alloc::vec::Vec<arch::apic::IoApic>> =
    sync::SpinLock::new(alloc::vec::Vec::new());

/// Sets up interrupt controllers and the clock on the boot CPU.
#[cfg(target_arch = "x86_64")]
pub(crate) fn init_boot_cpu() {
    use arch::apic;
    let madt = madt();
    if madt.as_ref().is_none_or(madt::Madt::pcat_compat) {
        apic::mask_legacy_pics();
    }
    let xapic_va = (!apic::x2apic_supported()).then(|| {
        crate::mm::map_device(PhysAddr::new(apic::xapic_phys()), 4096)
            .expect("xAPIC mapping")
            .get()
    });
    // SAFETY: boot CPU, interrupts masked, the xAPIC window (if any) is
    // mapped uncached for good.
    let mode = unsafe {
        let mode = apic::init(xapic_va);
        apic::init_cpu();
        mode
    };
    HW_ID[0].store(u64::from(apic::id()), Ordering::Release);

    let mut ioapics = 0;
    for e in madt
        .iter()
        .flat_map(madt::Madt::entries)
        .filter_map(Result::ok)
    {
        if let MadtEntry::IoApic(io) = e {
            let Ok(va) = crate::mm::map_device(PhysAddr::new(u64::from(io.address)), 4096) else {
                kprintln!("irq: cannot map I/O APIC {}", io.id);
                continue;
            };
            // SAFETY: the window was just mapped for this I/O APIC alone.
            let io = unsafe { apic::IoApic::new(va.get(), io.gsi_base) };
            io.mask_all();
            ioapics += 1;
            IOAPICS.lock().push(io);
        }
    }

    let freq = calibrate_tsc();
    crate::time::init_clock(freq);
    // SAFETY: boot CPU, after `init_cpu`, interrupts masked.
    unsafe { apic::calibrate_timer(|| pm_wait_tsc(10_000_000).unwrap_or(freq / 100)) };
    apic::configure_timer();
    kprintln!(
        "irq: local APIC {mode:?} id {}, {ioapics} I/O APIC(s), timer {}",
        apic::id(),
        if apic::uses_tsc_deadline() {
            "TSC-deadline"
        } else {
            "one-shot"
        }
    );
}

/// The TSC frequency, measured against the ACPI PM timer.
#[cfg(target_arch = "x86_64")]
fn calibrate_tsc() -> u64 {
    // Ten 10 ms samples; keep the median to shrug off an interrupted one.
    let mut samples = [0u64; 5];
    for s in &mut samples {
        match pm_wait_tsc(10_000_000) {
            Some(t) => *s = t * 100,
            None => {
                kprintln!("time: no ACPI PM timer; assuming a 1 GHz TSC");
                return 1_000_000_000;
            }
        }
    }
    samples.sort_unstable();
    samples[2]
}

/// Busy-waits `ns` on the ACPI PM timer and returns the TSC ticks taken.
#[cfg(target_arch = "x86_64")]
fn pm_wait_tsc(ns: u64) -> Option<u64> {
    const PM_HZ: u64 = 3_579_545;
    let fadt = crate::acpi::find(*b"FACP")?;
    let pm = acpi_tables::fadt::parse(&fadt).ok()?.pm_timer()?;
    if pm.block.space != acpi_tables::AddressSpace::Io {
        return None;
    }
    let port = u16::try_from(pm.block.address).ok()?;
    let mask = if pm.bits == 32 { u32::MAX } else { 0xff_ffff };
    // SAFETY: the FADT names this port as the PM timer; reads are
    // side-effect free.
    let read = || unsafe { arch::cpu::inl(port) } & mask;
    let want = u32::try_from(PM_HZ * ns / crate::time::NANOS).ok()?;
    let start = read();
    let tsc0 = arch::cpu::timestamp();
    while read().wrapping_sub(start) & mask < want {
        core::hint::spin_loop();
    }
    Some(arch::cpu::timestamp() - tsc0)
}

#[cfg(target_arch = "aarch64")]
const RESCHED_SGI: u8 = 0;
#[cfg(target_arch = "aarch64")]
const CALL_SGI: u8 = 1;

/// The timer's interrupt ID.
#[cfg(target_arch = "aarch64")]
static TIMER_PPI: AtomicU32 = AtomicU32::new(27);
/// Each CPU's redistributor (kernel virtual address).
#[cfg(target_arch = "aarch64")]
static GICR: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// The redistributor regions, mapped: (virtual base, length).
#[cfg(target_arch = "aarch64")]
static GICR_REGIONS: sync::SpinLock<alloc::vec::Vec<(u64, u64)>> =
    sync::SpinLock::new(alloc::vec::Vec::new());

/// Sets up interrupt controllers and the clock on the boot CPU.
#[cfg(target_arch = "aarch64")]
pub(crate) fn init_boot_cpu() {
    use arch::gic;
    let madt = madt().expect("no MADT: cannot find the GIC");
    let gicd = madt
        .gic_distributor()
        .expect("no GIC distributor in the MADT");
    let gicd_va =
        crate::mm::map_device(PhysAddr::new(gicd.physical_base), 0x1_0000).expect("GICD mapping");
    let mut regions = GICR_REGIONS.lock();
    for e in madt.entries().filter_map(Result::ok) {
        match e {
            MadtEntry::GicRedistributor(r) => {
                let len = u64::from(r.length);
                match crate::mm::map_device(PhysAddr::new(r.base), len) {
                    Ok(va) => regions.push((va.get(), len)),
                    Err(e) => kprintln!("irq: cannot map GICR at {:#x}: {e:?}", r.base),
                }
            }
            // Without GICR structures, each GICC names its own frame.
            MadtEntry::GicCpuInterface(c) if c.gicr_base.is_some_and(|b| b != 0) => {
                let base = c.gicr_base.unwrap_or(0);
                if let Ok(va) = crate::mm::map_device(PhysAddr::new(base), 0x2_0000) {
                    regions.push((va.get(), 0x2_0000));
                }
            }
            _ => {}
        }
    }
    drop(regions);
    let mpidr = arch::cpu::mpidr();
    // SAFETY: boot CPU, interrupts masked, the window is mapped for good.
    let lines = unsafe { gic::init_distributor(gicd_va.get(), mpidr) };
    HW_ID[0].store(mpidr, Ordering::Release);
    init_this_cpu(CpuId::BOOT);

    crate::time::init_clock(arch::timer::frequency());
    kprintln!(
        "irq: GICv{} distributor (IIDR {:#010x}), {lines} interrupt lines, timer PPI {}",
        gicd.version,
        gic::distributor_iidr(),
        TIMER_PPI.load(Ordering::Relaxed)
    );
}

/// Sets up this (non-boot) CPU's redistributor and timer interrupt.
#[cfg(target_arch = "aarch64")]
pub(crate) fn init_ap(cpu: CpuId) {
    init_this_cpu(cpu);
}

/// Wakes this CPU's redistributor and enables its timer interrupt.
#[cfg(target_arch = "aarch64")]
fn init_this_cpu(cpu: CpuId) {
    use arch::gic;
    let mpidr = arch::cpu::mpidr();
    let gicr = GICR_REGIONS
        .lock()
        .iter()
        .find_map(|&(base, len)| gic::find_redistributor(base, len, mpidr))
        .expect("no redistributor for this CPU");
    GICR[cpu.index()].store(gicr, Ordering::Relaxed);
    // SAFETY: the redistributor is mapped for good; interrupts are masked.
    unsafe { gic::init_cpu(gicr) };
    let gtdt = crate::acpi::find(*b"GTDT").and_then(|t| acpi_tables::gtdt::parse(&t).ok());
    let timer = gtdt.and_then(|g| g.virtual_el1());
    if let Some(t) = timer {
        TIMER_PPI.store(t.gsiv, Ordering::Relaxed);
    }
    let edge = timer.is_some_and(|t| t.flags.edge_triggered());
    gic::enable_private(gicr, TIMER_PPI.load(Ordering::Relaxed), edge);
    gic::enable_private(gicr, u32::from(RESCHED_SGI), true);
    gic::enable_private(gicr, u32::from(CALL_SGI), true);
}
