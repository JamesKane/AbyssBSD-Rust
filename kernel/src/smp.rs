// SPDX-License-Identifier: BSD-2-Clause
//! Bringing up the other CPUs, and cross-CPU TLB shootdowns.
//!
//! CPUs come from the MADT (enabled local APIC / x2APIC / GICC entries).
//! Each gets a logical number in start-up order, a kernel stack, and its
//! arch per-CPU block; it then initializes its interrupt controller and
//! becomes an idle thread in the scheduler.
//!
//! Behavior reference: sys/x86/x86/mp_x86.c, sys/arm64/arm64/mp_machdep.c,
//! sys/x86/x86/mp_x86.c (smp_targeted_tlb_shootdown).

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use acpi_tables::madt::HwId;
use addr::VirtAddr;
use hal::{CpuId, MAX_CPUS};

use crate::arch;
use crate::mm::kstack::KernelStack;

/// How long to wait for a started CPU to check in.
const START_TIMEOUT_NS: u64 = 1_000_000_000;

/// Set by each started CPU once it is running the scheduler.
static ONLINE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// A zeroed `T` on the heap, leaked: for per-CPU blocks too large to
/// build on a kernel stack, where all-zero is a valid value.
fn leak_zeroed<T>() -> &'static mut T {
    let layout = Layout::new::<T>();
    // SAFETY: `T` is not zero-sized (both per-CPU block types are large).
    let p = unsafe { alloc_zeroed(layout) }.cast::<T>();
    assert!(!p.is_null(), "out of memory for a per-CPU block");
    // SAFETY: freshly allocated, zeroed (valid for the callers' types),
    // and never freed.
    unsafe { &mut *p }
}

/// Starts every other enabled CPU. Returns how many CPUs are online.
pub(crate) fn start_cpus() -> usize {
    // The boot CPU takes part in shootdowns like the rest.
    ONLINE[CpuId::BOOT.index()].store(true, Ordering::Release);
    let madt = crate::acpi::find(*b"APIC").and_then(|t| acpi_tables::madt::parse(&t).ok());
    let Some(madt) = madt else {
        kprintln!("smp: no MADT; running on one CPU");
        return 1;
    };
    let boot = crate::intc::hw_id(CpuId::BOOT);
    let mut next = 1u32;
    let mut failed = 0;
    for cpu in madt.cpus().filter(|c| c.enabled) {
        let hw = match cpu.hw_id {
            HwId::Apic(id) => u64::from(id),
            HwId::Mpidr(m) => m,
            HwId::GicCpuInterface(_) => continue,
        };
        if Some(hw) == boot {
            continue;
        }
        if next as usize >= MAX_CPUS {
            kprintln!("smp: more than {MAX_CPUS} CPUs; ignoring the rest");
            break;
        }
        if start(CpuId(next), hw) {
            next += 1;
        } else {
            failed += 1;
        }
    }
    let online = sched::online_cpus();
    // A CPU that failed to check in might still come up and take a
    // broadcast it is not counted for.
    BROADCAST.store(failed == 0, Ordering::Release);
    kprintln!(
        "smp: {online} CPUs online{}",
        if failed > 0 {
            " (some failed to start)"
        } else {
            ""
        }
    );
    online
}

/// Starts one CPU as logical `cpu`; waits until it is up.
fn start(cpu: CpuId, hw: u64) -> bool {
    let stack = KernelStack::new(crate::threads::STACK_PAGES).expect("AP stack");
    let top = stack.top().get();
    // The stack becomes the CPU's idle thread stack for good.
    core::mem::forget(stack);
    crate::intc::set_hw_id(cpu, hw);
    if !arch_start(cpu, hw, top) {
        kprintln!("smp: cpu{} (id {hw:#x}) did not start", cpu.0);
        return false;
    }
    let deadline = crate::time::now_ns() + START_TIMEOUT_NS;
    // Checked in, then joined the scheduler (which publishes it).
    while !ONLINE[cpu.index()].load(Ordering::Acquire) || sched::online_cpus() <= cpu.index() {
        if crate::time::now_ns() > deadline {
            kprintln!("smp: cpu{} (id {hw:#x}) did not check in", cpu.0);
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// The first kernel code on a started CPU (from the arch AP entry).
pub(crate) fn ap_main(_arg: usize) -> ! {
    let cpu = hal::cpu_id();
    crate::intc::init_ap(cpu);
    crate::time::init_cpu();
    crate::threads::init_fpu(false);
    arch::user::init_cpu();
    crate::topology::record_this_cpu(cpu);
    ONLINE[cpu.index()].store(true, Ordering::Release);
    sched::run_idle()
}

#[cfg(target_arch = "x86_64")]
fn arch_start(cpu: CpuId, hw: u64, stack_top: u64) -> bool {
    use arch::smp;

    /// The trampoline page and four table pages, below 1 MiB.
    static LOW: sync::Once<addr::PhysAddr> = sync::Once::new();
    if LOW.get().is_none() {
        let Some(low) = crate::mm::low_pages(5) else {
            kprintln!("smp: no free memory below 1 MiB for the trampoline");
            return false;
        };
        prepare_trampoline(low);
        let _ = LOW.set(low);
    }
    let low = *LOW.get().expect("set above");
    let page = bootproto::layout::dmap(low).expect("low memory in the DMAP");
    let block = leak_zeroed::<arch::CpuBlock>();
    let start = Box::leak(Box::new(smp::ApStart {
        cpu: cpu.0,
        block,
        arg: 0,
    }));
    let put = |off: usize, v: u64| {
        // SAFETY: the trampoline page is ours (reserved low memory, in the
        // DMAP); `off` is within its data area.
        unsafe { ((page.get() + off as u64) as *mut u64).write_volatile(v) };
    };
    put(smp::DATA_STACK, stack_top);
    put(smp::DATA_ARG, core::ptr::from_mut(start) as u64);

    let apic = u32::try_from(hw).unwrap_or(u32::MAX);
    let vector = u8::try_from(low.get() >> 12).unwrap_or(0);
    arch::apic::send_init(apic);
    crate::time::spin_ns(10_000_000);
    arch::apic::send_startup(apic, vector);
    crate::time::spin_ns(200_000);
    arch::apic::send_startup(apic, vector);
    true
}

/// Copies the trampoline to `low` and builds its temporary page table
/// (the trampoline page identity mapped, plus the kernel's upper half) in
/// the four pages after it.
#[cfg(target_arch = "x86_64")]
fn prepare_trampoline(low: addr::PhysAddr) {
    use arch::smp;
    let dmap = |pa: addr::PhysAddr| bootproto::layout::dmap(pa).expect("low memory").get();
    let page = dmap(low);
    let code = smp::trampoline();
    assert!(code.len() <= smp::DATA_GDT, "trampoline overlaps its data");
    // SAFETY: the five low pages are reserved for us and mapped in the
    // DMAP; we are the only CPU touching them until the SIPI.
    unsafe {
        core::ptr::write_bytes(page as *mut u8, 0, 5 * 4096);
        core::ptr::copy_nonoverlapping(code.as_ptr(), page as *mut u8, code.len());
        let w64 = |off: usize, v: u64| ((page + off as u64) as *mut u64).write_volatile(v);
        let w32 = |off: usize, v: u32| ((page + off as u64) as *mut u32).write_unaligned(v);
        for (i, d) in smp::TRAMPOLINE_GDT.iter().enumerate() {
            w64(smp::DATA_GDT + 8 * i, *d);
        }
        let base = u32::try_from(low.get()).expect("low memory");
        // GDT pointer: 16-bit limit, 32-bit base.
        ((page + smp::DATA_GDT_PTR as u64) as *mut u16).write_volatile(31);
        w32(
            smp::DATA_GDT_PTR + 2,
            base + u32::try_from(smp::DATA_GDT).expect("small offset"),
        );
        for (operand, target) in smp::jump_patches() {
            w32(operand, base + u32::try_from(target).expect("small offset"));
        }

        // Tables: PML4 at +1 page, then PDPT, PD, PT.
        let table = |i: u64| low + i * 4096;
        let pml4 = dmap(table(1)) as *mut u64;
        let kernel = dmap(crate::mm::kernel_root()) as *const u64;
        for i in 256..512 {
            pml4.add(i).write(kernel.add(i).read());
        }
        // Present | writable for the identity path; the trampoline page
        // itself is a 4 KiB leaf.
        let link = |pa: addr::PhysAddr| pa.get() | 0b11;
        let idx = |shift: u32| usize::try_from((low.get() >> shift) & 0x1ff).expect("index");
        pml4.add(idx(39)).write(link(table(2)));
        (dmap(table(2)) as *mut u64)
            .add(idx(30))
            .write(link(table(3)));
        (dmap(table(3)) as *mut u64)
            .add(idx(21))
            .write(link(table(4)));
        (dmap(table(4)) as *mut u64).add(idx(12)).write(link(low));

        w64(smp::DATA_TEMP_CR3, table(1).get());
        w64(smp::DATA_KERNEL_CR3, crate::mm::kernel_root().get());
        w64(smp::DATA_ENTRY, smp::abyss_ap_entry64 as *const () as u64);
    }
}

#[cfg(target_arch = "aarch64")]
fn arch_start(cpu: CpuId, hw: u64, stack_top: u64) -> bool {
    use arch::smp;
    static IDENTITY: sync::Once<addr::PhysAddr> = sync::Once::new();
    let info = crate::boot::info();
    if IDENTITY.get().is_none() {
        let _ = IDENTITY.set(crate::mm::identity_table(
            info.kernel.phys,
            info.kernel.size,
        ));
    }
    let block = leak_zeroed::<arch::cpu::CpuBlock>();
    let start = Box::leak(Box::new(smp::ApStart {
        cpu: cpu.0,
        block,
        arg: 0,
    }));
    let boot = Box::leak(Box::new(smp::ApBoot {
        kernel_root: crate::mm::kernel_root().get(),
        identity_root: IDENTITY.get().expect("set above").get(),
        stack_top,
        arg: core::ptr::from_mut(start) as u64,
    }));
    let boot_va = core::ptr::from_mut(boot) as usize;
    // The CPU reads this with its caches off.
    arch::cache::clean_to_poc(boot_va, size_of::<smp::ApBoot>());
    let boot_pa =
        bootproto::layout::dmap_to_phys(VirtAddr::new(boot_va as u64)).expect("heap in the DMAP");
    let entry = info.kernel.phys.get() + (smp::entry_point() - info.kernel.virt.get());
    let r = smp::psci_cpu_on(psci_conduit(), hw, entry, boot_pa.get());
    if r != 0 {
        kprintln!("smp: PSCI CPU_ON for {hw:#x} failed: {r}");
    }
    r == 0
}

/// The pending cross-call: a TLB range to flush (empty for a ping).
static CALL_BUSY: AtomicBool = AtomicBool::new(false);
static CALL_VA: AtomicU64 = AtomicU64::new(0);
static CALL_LEN: AtomicU64 = AtomicU64::new(0);
static CALL_PENDING: AtomicU32 = AtomicU32::new(0);

/// Whether a cross-call to every other CPU may use one broadcast IPI: true
/// once every CPU the firmware listed (and we could take) is online.
static BROADCAST: AtomicBool = AtomicBool::new(false);

/// Which CPUs a cross-call interrupts.
#[derive(Clone, Copy)]
enum Targets {
    AllOthers,
    One(CpuId),
    /// A set of CPUs, one bit each, never this one.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))] // TLBI broadcasts there
    Mask(u64),
}

/// Removes stale translations for `[va, va + len)` from every other CPU
/// and waits until they have. Call with interrupts enabled and no spin
/// locks held: other initiators must be able to interrupt the wait.
///
/// On aarch64 the TLBI instructions broadcast, so there is nothing to do.
pub(crate) fn shootdown(va: VirtAddr, len: u64) {
    if cfg!(target_arch = "aarch64") || sched::online_cpus() <= 1 {
        return;
    }
    cross_call(va, len, Targets::AllOthers);
}

/// As [`shootdown`], for the CPUs in `mask` only (one bit per CPU; this
/// CPU's is ignored): those a user address space is loaded on.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))] // TLBI broadcasts there
pub(crate) fn shootdown_cpus(va: VirtAddr, len: u64, mask: u64) {
    let mask = mask & !(1 << hal::cpu_id().0);
    if cfg!(target_arch = "aarch64") || mask == 0 {
        return;
    }
    cross_call(va, len, Targets::Mask(mask));
}

/// Interrupts `cpu` and waits until it has acknowledged (an IPI round
/// trip). Same calling rules as [`shootdown`].
#[cfg_attr(feature = "ktest", allow(dead_code))]
pub(crate) fn ping(cpu: CpuId) {
    if cpu != hal::cpu_id() {
        cross_call(VirtAddr::new(0), 0, Targets::One(cpu));
    }
}

/// Sends the cross-call IPI to `targets`, asking each to flush
/// `[va, va + len)`, and waits for all to acknowledge.
fn cross_call(va: VirtAddr, len: u64, targets: Targets) {
    // With interrupts masked this CPU cannot answer another initiator's
    // call while it waits for CALL_BUSY: both stop forever.
    if !arch::cpu::interrupts_enabled() {
        sched::lock::report(format_args!("smp: cross-call with interrupts masked"));
    }
    while CALL_BUSY
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    CALL_VA.store(va.get(), Ordering::Relaxed);
    CALL_LEN.store(len, Ordering::Relaxed);
    let me = hal::cpu_id();
    let selected = |c: CpuId| match targets {
        Targets::AllOthers => c != me,
        Targets::One(t) => c == t,
        Targets::Mask(m) => c != me && m & (1 << c.0) != 0,
    };
    let online = || {
        (0..sched::online_cpus())
            .filter_map(|i| u32::try_from(i).ok().map(CpuId))
            .filter(|&c| selected(c) && ONLINE[c.index()].load(Ordering::Acquire))
    };
    let n = u32::try_from(online().count()).unwrap_or(u32::MAX);
    // Release: the range is visible to targets before they see the count.
    CALL_PENDING.store(n, Ordering::Release);
    #[cfg(target_arch = "x86_64")]
    if matches!(targets, Targets::AllOthers) && BROADCAST.load(Ordering::Acquire) {
        crate::intc::send_call_all_but_self();
    } else {
        online().for_each(crate::intc::send_call);
    }
    #[cfg(target_arch = "aarch64")]
    online().for_each(crate::intc::send_call);
    while CALL_PENDING.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
    }
    CALL_BUSY.store(false, Ordering::Release);
}

/// The cross-call IPI arrived: flush the range and acknowledge.
pub(crate) fn on_call_ipi() {
    let va = VirtAddr::new(CALL_VA.load(Ordering::Acquire));
    let len = CALL_LEN.load(Ordering::Acquire);
    if len > 64 * 4096 {
        arch::mmu::flush_all();
    } else if len > 0 {
        crate::mm::flush_range(va, len);
    }
    CALL_PENDING.fetch_sub(1, Ordering::AcqRel);
}

/// How PSCI calls reach firmware: the FADT's `PSCI_USE_HVC`, else SMC.
#[cfg(target_arch = "aarch64")]
pub(crate) fn psci_conduit() -> arch::smp::Conduit {
    crate::acpi::find(*b"FACP")
        .and_then(|t| acpi_tables::fadt::parse(&t).ok())
        .and_then(|f| f.arm_boot_arch())
        .map_or(arch::smp::Conduit::Smc, |a| {
            if a.psci_use_hvc() {
                arch::smp::Conduit::Hvc
            } else {
                arch::smp::Conduit::Smc
            }
        })
}
