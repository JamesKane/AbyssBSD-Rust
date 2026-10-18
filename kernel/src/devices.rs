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

/// The drivers this build includes (DRIVER-MODEL.md §7.1): reviewable code
/// rather than linker sections. Real buses and drivers arrive in P3.6 and
/// P3.7; ktest builds add the manager's test drivers.
static DRIVERS: &[DriverDecl] = &[
    acpi::DRIVER,
    acpi::PCIB,
    pci::bus::DRIVER,
    #[cfg(feature = "ktest")]
    crate::tests::bus_test::TEST_BUS,
    #[cfg(feature = "ktest")]
    crate::tests::bus_test::TEST_DEV,
    #[cfg(feature = "ktest")]
    crate::tests::bus_test::PICKY,
    #[cfg(feature = "ktest")]
    crate::tests::bus_test::FALLBACK,
    #[cfg(feature = "ktest")]
    crate::tests::bus_test::BROKEN,
    #[cfg(feature = "ktest")]
    crate::tests::bus_test::SLOW,
    #[cfg(feature = "ktest")]
    crate::tests::bus_test::NEEDY,
];

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
    let root = bus::init(
        s,
        &PLATFORM,
        DRIVERS,
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
    if crate::acpi_bus::init(s) {
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
    kprintln!(
        "bus: device manager up, {} driver(s), {workers} attach worker(s){}",
        DRIVERS.len(),
        if verifier { ", verifier on" } else { "" }
    );
}
