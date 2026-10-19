// SPDX-License-Identifier: BSD-2-Clause
//! The device manager's kernel side: the platform services `sys/bus`
//! needs, the build's driver table, and start-up.

use addr::{PhysAddr, VirtAddr};
use bus::{DriverDecl, Platform};
use sched::Sleepable;
use sync::SpinLock;
use sysctl::Tree;
use vm::dma::DmaMemory;

/// Ranges leaf drivers may not map: interrupt controllers, IOMMUs, ECAM,
/// and the consoles. Registered by the code that owns them.
static SENSITIVE: SpinLock<alloc::vec::Vec<(u64, u64)>> = SpinLock::new(alloc::vec::Vec::new());

/// Maps registers the kernel itself owns (interrupt controllers, and
/// later IOMMUs and ECAM), marking them off limits to drivers.
pub(crate) fn map_reserved(pa: PhysAddr, len: u64) -> Result<VirtAddr, crate::mm::DeviceMapError> {
    SENSITIVE.lock().push((pa.get(), len));
    crate::mm::map_device(pa, len)
}

struct KernelPlatform;

impl Platform for KernelPlatform {
    fn map_device(&self, pa: u64, len: u64) -> Option<usize> {
        crate::mm::map_device(PhysAddr::new(pa), len)
            .ok()
            .and_then(|va| usize::try_from(va.get()).ok())
    }

    fn unmap_device(&self, va: usize, len: u64) {
        crate::mm::unmap_device(VirtAddr::new(va as u64), len);
    }

    fn dma_memory(&self) -> &'static dyn DmaMemory {
        &crate::mm::dma::KERNEL_DMA
    }

    fn top_of_ram(&self) -> u64 {
        crate::mm::dma::top_of_ram()
    }

    fn sensitive(&self, pa: u64, len: u64) -> bool {
        SENSITIVE
            .lock()
            .iter()
            .any(|&(b, l)| pa < b + l && b < pa + len)
    }

    fn sysctl(&self, s: &Sleepable, f: &mut dyn FnMut(&mut Tree)) {
        crate::sysctl::with(s, |t| f(t));
    }

    #[cfg(target_arch = "x86_64")]
    fn io_read(&self, port: u16, width: u8) -> u32 {
        // SAFETY: the port is in a range the manager granted a driver from
        // a bus-described resource (a BAR), claimed by that driver alone.
        unsafe {
            match width {
                1 => u32::from(crate::arch::cpu::inb(port)),
                2 => u32::from(crate::arch::cpu::inw(port)),
                _ => crate::arch::cpu::inl(port),
            }
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn io_read(&self, _port: u16, _width: u8) -> u32 {
        u32::MAX
    }

    #[cfg(target_arch = "x86_64")]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the value is `width` bytes wide"
    )]
    fn io_write(&self, port: u16, width: u8, v: u32) {
        // SAFETY: as in `io_read`.
        unsafe {
            match width {
                1 => crate::arch::cpu::outb(port, v as u8),
                2 => crate::arch::cpu::outw(port, v as u16),
                _ => crate::arch::cpu::outl(port, v),
            }
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn io_write(&self, _port: u16, _width: u8, _v: u32) {}

    fn sleep_ns(&self, s: &Sleepable, ns: u64) {
        crate::time::sleep_ns(s, ns);
    }

    fn log(&self, args: core::fmt::Arguments<'_>) {
        kprintln!("{args}");
    }
}

static PLATFORM: KernelPlatform = KernelPlatform;

/// `nexus0`, the platform root firmware buses attach under.
static NEXUS: sync::Once<bus::DeviceId> = sync::Once::new();

/// `nexus0`.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn nexus() -> bus::DeviceId {
    *NEXUS.get().expect("devices::init")
}

/// The manager's test drivers, which ktest builds add to the table.
#[cfg(feature = "ktest")]
static TEST_DRIVERS: &[DriverDecl] = &[
    crate::tests::bus_test::TEST_BUS,
    crate::tests::bus_test::TEST_DEV,
    crate::tests::bus_test::PICKY,
    crate::tests::bus_test::FALLBACK,
    crate::tests::bus_test::BROKEN,
    crate::tests::bus_test::SLOW,
    crate::tests::bus_test::NEEDY,
    crate::tests::bus_test::DEP_BUS,
    crate::tests::bus_test::ORDERED,
];

/// Drivers that call into the kernel itself (firmware services).
static KERNEL_DRIVERS: &[DriverDecl] = &[crate::efirtc::DRIVER];

/// The build's drivers (`driver-table`, checked on the host for ties),
/// the kernel's own, and the test drivers in ktest builds.
fn drivers() -> &'static [DriverDecl] {
    let mut all = alloc::vec::Vec::from(driver_table::DRIVERS);
    all.extend_from_slice(KERNEL_DRIVERS);
    #[cfg(feature = "ktest")]
    all.extend_from_slice(TEST_DRIVERS);
    alloc::vec::Vec::leak(all)
}

/// Keeps drivers off the devices the console took over from the loader;
/// their real drivers will adopt them explicitly (P3.8).
fn reserve_boot_devices() {
    let info = crate::boot::info();
    let mut r = SENSITIVE.lock();
    if let Some(bootproto::Serial::Ns16550Mmio(m) | bootproto::Serial::Pl011(m)) = info.serial.get()
    {
        r.push((m.phys.get(), m.size.max(4096)));
    }
    if let Some(f) = info.framebuffer.get() {
        r.push((f.phys.get(), f.size));
    }
}

/// The GTDT's non-secure SBSA generic watchdogs.
fn sbsa_watchdogs() -> alloc::vec::Vec<acpi_tables::gtdt::Watchdog> {
    let Some(g) = crate::acpi::find(*b"GTDT").and_then(|t| acpi_tables::gtdt::parse(&t).ok())
    else {
        return alloc::vec::Vec::new();
    };
    g.platform_timers()
        .flatten()
        .filter_map(|t| match t {
            acpi_tables::gtdt::PlatformTimer::Watchdog(w) if !w.secure => Some(w),
            _ => None,
        })
        .collect()
}

/// Detaches every device, consumers before suppliers, so none is still
/// doing DMA when firmware takes over.
pub(crate) fn shutdown(s: &Sleepable) {
    bus::manager().shutdown(s);
}

/// Starts the device manager. Call once, after the other CPUs are up.
pub(crate) fn init(s: &Sleepable) {
    reserve_boot_devices();
    let workers = if crate::boot::flag("abyss.bus.serial") {
        1
    } else {
        sched::online_cpus().clamp(1, 4)
    };
    let verifier = crate::boot::flag("abyss.verifier");
    let drivers = drivers();
    let root = bus::init(
        s,
        &PLATFORM,
        drivers,
        bus::Options {
            workers,
            fault_one_in: if verifier { 64 } else { 0 },
            fault_seed: crate::time::counter(),
        },
    );
    // The platform root, and the devices the kernel set up itself, as
    // FreeBSD's tree shows them.
    let m = bus::manager();
    let nexus = m.adopt(s, root, "nexus", "", bus::Ident::Platform);
    #[cfg(target_arch = "x86_64")]
    m.adopt(s, nexus, "apic", "APIC resources", bus::Ident::Platform);
    let _ = NEXUS.set(nexus);
    // The GTDT's non-secure SBSA watchdogs.
    for w in sbsa_watchdogs() {
        let res = alloc::vec![
            bus::Resource::Memory {
                base: w.refresh_frame,
                len: 0x1000,
                prefetchable: false,
            },
            bus::Resource::Memory {
                base: w.control_frame,
                len: 0x1000,
                prefetchable: false,
            },
        ];
        let timebase = sbsa_gwdt::Timebase {
            frequency: crate::time::frequency(),
        };
        m.add_child(
            s,
            nexus,
            bus::Ident::Nexus("sbsa_gwdt"),
            Some(alloc::sync::Arc::new(timebase)),
            res,
            &[],
        );
    }
    // The firmware clock, where runtime services answer (FreeBSD's
    // efirtc identify).
    if crate::firmware::time().is_some() {
        m.add_child(
            s,
            nexus,
            bus::Ident::Nexus("efirtc"),
            None,
            alloc::vec![],
            &[],
        );
    }
    if crate::acpi_bus::init(s) {
        crate::acpi_bus::apply_cppc(s);
        crate::power::init(s);
        m.add_child(
            s,
            nexus,
            bus::Ident::Nexus("acpi"),
            None,
            alloc::vec![],
            &[],
        );
    }
    // Boot goes on once the devices found so far have attached, as
    // FreeBSD finishes autoconfiguration before mounting root.
    bus::manager().wait_idle(s);
    crate::time::set_realtime();
    kprintln!(
        "bus: device manager up, {} driver(s), {workers} attach worker(s){}",
        drivers.len(),
        if verifier { ", verifier on" } else { "" }
    );
}
