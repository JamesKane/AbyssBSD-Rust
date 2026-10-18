// SPDX-License-Identifier: BSD-2-Clause
//! The AbyssBSD kernel.
//!
//! Phase 1: take over from the loader, bring up the consoles, ingest the
//! memory map, switch to the kernel's own W^X page tables, and power off.
//! Built with the `ktest` feature it runs the in-kernel tests instead,
//! ending with a deliberate fatal page fault to prove the trap reporting.
//!
//! This crate is glue and init order; subsystems live in their own crates
//! (docs/ARCHITECTURE.md §2).

#![no_std]
#![no_main]

extern crate alloc;

#[cfg(target_arch = "aarch64")]
use arch_aarch64 as arch;
#[cfg(target_arch = "x86_64")]
use arch_x86_64 as arch;

#[macro_use]
mod console;
mod acpi;
mod acpi_bus;
#[cfg(not(feature = "ktest"))]
mod bench;
mod boot;
mod devices;
mod efirtc;
mod errata;
mod firmware;
mod intc;
#[cfg(target_arch = "aarch64")]
mod its;
mod mm;
mod panic;
mod pci;
mod qemu;
mod random;
mod smp;
#[cfg(not(feature = "ktest"))]
mod stress;
mod symbols;
mod sysctl;
#[cfg(feature = "ktest")]
mod tests;
mod threads;
mod time;
mod topology;

use addr::PhysAddr;

arch::kernel_hooks!(
    main = kmain,
    ap_main = smp::ap_main,
    fatal_trap = panic::fatal_trap,
    interrupt = intc::dispatch,
);

fn kmain(boot_info: PhysAddr) -> ! {
    let Ok(info) = boot::take(boot_info) else {
        // Without a valid handoff there is no console to complain on.
        arch::cpu::halt_forever();
    };
    console::init(info);
    kprintln!(
        "\nAbyssBSD {} ({}) kernel at {:#x}, slide {:#x}",
        env!("CARGO_PKG_VERSION"),
        arch::NAME,
        info.kernel.virt,
        info.kernel.slide
    );
    #[cfg(target_arch = "aarch64")]
    if arch::booted_at_el2() {
        kprintln!("entered at EL2; EL2 stub installed, running at EL1");
    }
    kprintln!("command line: {:?}", boot::cmdline());
    symbols::init(info);

    let tables = mm::init(info);
    firmware::init(info, tables.firmware_root);
    kprintln!("kernel page tables live (W^X, DMAP, no identity map)");
    intc::init_boot_cpu();
    intc::init_dispatch();
    threads::init_fpu(true);
    threads::init();
    time::init_cpu();
    sysctl::init();
    topology::record_this_cpu(hal::CpuId::BOOT);
    hal::irq::enable();
    // SAFETY: kmain is now a thread (`threads::init`) with interrupts
    // enabled, holding no locks: it may sleep.
    let s = unsafe { sched::Sleepable::assume() };
    random::init();
    smp::start_cpus();
    topology::summarize();
    pci::init();
    devices::init(&s);
    random::start_harvester();

    #[cfg(feature = "ktest")]
    tests::run(&s);

    #[cfg(not(feature = "ktest"))]
    {
        let (phys, heap) = (mm::phys_stats(), mm::heap_stats());
        kprintln!(
            "memory: {} MiB free of {} MiB; heap {} objects in {} slabs, {} large pages",
            (phys.free * 4096) >> 20,
            (phys.managed * 4096) >> 20,
            heap.live_objects,
            heap.slabs,
            heap.large_pages
        );
        if let Some(secs) = boot::value("abyss.stress").and_then(|v| v.parse().ok()) {
            let passed = stress::run(&s, secs);
            qemu::exit(passed);
            power_off(&s)
        }
        if boot::flag("abyss.acpidump") {
            acpi::dump();
            power_off(&s)
        }
        if let Some(suite) = boot::value("abyss.bench") {
            let ran = bench::run(&s, suite);
            qemu::exit(ran);
            power_off(&s)
        }
        if boot::flag("abyss.halt") {
            kprintln!("Phase 1 kernel: nothing more to do yet. Halted (abyss.halt).");
            arch::cpu::halt_forever();
        }
        kprintln!("Phase 1 kernel: nothing more to do yet. Powering off.");
        power_off(&s)
    }
}

/// Stops the devices, then powers off through firmware.
pub(crate) fn power_off(s: &sched::Sleepable) -> ! {
    devices::shutdown(s);
    firmware::reset(efi::ResetKind::Shutdown)
}
