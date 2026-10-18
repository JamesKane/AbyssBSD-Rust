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

/// The drivers this build includes (DRIVER-MODEL.md §7.1): reviewable code
/// rather than linker sections. Real buses and drivers arrive in P3.6 and
/// P3.7; ktest builds add the manager's test drivers.
static DRIVERS: &[DriverDecl] = &[
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
];

/// Starts the device manager. Call once, after the other CPUs are up.
pub(crate) fn init() {
    let workers = if crate::boot::flag("abyss.bus.serial") {
        1
    } else {
        sched::online_cpus().clamp(1, 4)
    };
    let verifier = crate::boot::flag("abyss.verifier");
    bus::init(
        &PLATFORM,
        DRIVERS,
        bus::Options {
            workers,
            fault_one_in: if verifier { 64 } else { 0 },
            fault_seed: crate::time::counter(),
        },
    );
    kprintln!(
        "bus: device manager up, {} driver(s), {workers} attach worker(s){}",
        DRIVERS.len(),
        if verifier { ", verifier on" } else { "" }
    );
}
