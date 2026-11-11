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

    fn wait_timeout(
        &self,
        s: &Sleepable,
        q: &sched::WaitQueue,
        cond: &mut dyn FnMut() -> bool,
        ns: u64,
    ) -> bool {
        crate::time::wait_timeout(s, q, cond, ns)
    }

    fn delay_us(&self, us: u64) {
        let ticks = u128::from(us) * u128::from(crate::time::frequency()) / 1_000_000;
        let start = crate::time::counter();
        while u128::from(crate::time::counter().wrapping_sub(start)) < ticks {
            core::hint::spin_loop();
        }
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
    crate::tests::bus_test::LINE_BUS,
    crate::tests::bus_test::LINE_PROVIDER,
    crate::tests::bus_test::LINE_USER,
    crate::tests::bus_test::COMPANION_BUS,
    crate::tests::bus_test::COMPANION_DEV,
    crate::tests::bus_test::COMPANION_FN,
    crate::tests::bus_test::SHARED_BUS,
    crate::tests::bus_test::SHARED_DEV,
    crate::tests::bus_test::PIN_BUS,
    crate::tests::bus_test::PIN_CONTROLLER,
    crate::tests::bus_test::PIN_USER,
    crate::tests::bus_test::IIC_TEST_BUS,
    crate::tests::bus_test::IIC_TEST_CTL,
    crate::tests::bus_test::IIC_TAKER,
    crate::tests::bus_test::IIC_TEST_DEV,
    crate::tests::bus_test::IIC_STAYS,
    #[cfg(target_arch = "aarch64")]
    crate::tests::sky_i2c::SKY_BUS,
    #[cfg(target_arch = "aarch64")]
    crate::tests::sky_i2c::SKY_CTL,
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

/// The GTDT's non-secure SBSA generic watchdogs. arm64 only, as FreeBSD's
/// `sbsa_gwdt` is (`files.arm64`), and its driver is only in that table.
#[cfg(target_arch = "aarch64")]
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

/// The platform root's rmans, in the order FreeBSD's nexus makes them
/// (x86/x86/nexus.c, arm64/arm64/nexus.c `nexus_attach`), for
/// `hw.bus.rman`: what no bus below has an rman for is listed in these.
fn platform_rmans(m: &bus::Manager) {
    use bus::{RmanKind, RmanSpec};
    #[cfg(target_arch = "x86_64")]
    {
        // The I/O APICs' pins from the start; each MSI line as it is
        // allocated (`nexus_add_irq`). Lines are numbered as `vmstat -i`
        // numbers them, MSIs from `irq::MSI_FIRST`.
        let pins = u64::from(crate::intc::ioapic_pins());
        let top = (1u64 << crate::arch::cpu::phys_addr_bits()) - 1;
        for spec in [
            RmanSpec::new(
                "Interrupt request lines",
                RmanKind::Irq,
                0,
                u64::from(u32::MAX),
            )
            .managing(alloc::vec![(0, pins - 1)])
            .growing(),
            RmanSpec::new("DMA request lines", RmanKind::Drq, 0, 7).managing(alloc::vec![(0, 7)]),
            RmanSpec::new("I/O ports", RmanKind::IoPort, 0, 0xffff)
                .managing(alloc::vec![(0, 0xffff)]),
            RmanSpec::new("I/O memory addresses", RmanKind::Memory, 0, top)
                .managing(alloc::vec![(0, top)]),
        ] {
            m.add_rman(None, spec);
        }
    }
    #[cfg(target_arch = "aarch64")]
    for spec in [
        RmanSpec::new("I/O memory addresses", RmanKind::Memory, 0, u64::MAX)
            .managing(alloc::vec![(0, u64::MAX)]),
        RmanSpec::new("Interrupts", RmanKind::Irq, 0, u64::MAX)
            .managing(alloc::vec![(0, u64::MAX)]),
    ] {
        m.add_rman(None, spec);
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
    let began = crate::time::now_ns();
    let workers = if crate::boot::flag("abyss.bus.serial") {
        1
    } else {
        sched::online_cpus().clamp(1, 4)
    };
    // Verifier mode (DRIVER-MODEL.md §13.1): the checks that are always
    // safe (dropped requests panic, stalled drains are reported).
    // Injected grant failures are their own knob, `abyss.verifier.faults`
    // (one grant in N fails), as tests that need their devices cannot
    // take them.
    let verifier = crate::boot::flag("abyss.verifier");
    let faults = crate::boot::value("abyss.verifier.faults")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    storage::bio::set_verifier(verifier);
    // Drivers' device nodes go to devfs.
    vfs::devices::set_registry(&crate::fs::devfs::Hook);
    crate::iic::init();
    crate::cam::init();
    let _ = uart::TERMINALS.set(&crate::tty::serial::HOST);
    // `vt` takes the framebuffer; keyboards type into its windows.
    crate::vtcons::start();
    input::set_consumer(alloc::sync::Arc::new(crate::tty::ConsoleInput));
    bus::set_bootverbose(crate::console::loader_flag("boot_verbose").unwrap_or(false));
    // `hw.nvme.*` (`TUNABLE_INT_FETCH`), before any controller attaches.
    let tunable = |name: &str| crate::boot::value(name).and_then(|v| v.parse::<i32>().ok());
    nvme::set_tunables(&tunable);
    // `hw.iommu.*` and `hw.smmu.*` (docs/IOMMU.md §4.12): arm64's, where
    // FreeBSD's IOMMU knobs are the SMMU's (x86 DMAR and AMD-Vi are
    // deferred, §9).
    iommu_framework::set_tunables(&tunable);
    iommu_framework::set_env(crate::boot::value);
    #[cfg(target_arch = "aarch64")]
    {
        smmu::set_tunables(&tunable);
        crate::sysctl::with(s, |t| {
            iommu_framework::sysctl_init(t);
            smmu::sysctl_init(t);
        });
    }
    let drivers = drivers();
    let root = bus::init(
        s,
        &PLATFORM,
        drivers,
        bus::Options {
            workers,
            fault_one_in: faults,
            fault_seed: crate::time::counter(),
            // The tunables (`TUNABLE_INT`'s reading: a bad value leaves
            // the default).
            devctl_queue: crate::boot::value("hw.bus.devctl_queue")
                .and_then(|v| v.parse::<i32>().ok())
                .map_or(bus::DEVCTL_DEFAULT_QUEUE, |q| {
                    usize::try_from(q).unwrap_or(0)
                }),
            devctl_nomatch: crate::boot::value("hw.bus.devctl_nomatch_enabled")
                .and_then(|v| v.parse::<i32>().ok())
                .is_none_or(|v| v != 0),
            disabled_hint,
        },
    );
    // The platform root, and the devices the kernel set up itself, as
    // FreeBSD's tree shows them.
    let m = bus::manager();
    crate::ugen::init(s);
    // `hidraw_load="YES"` (loader.conf's, on the command line): loaded
    // before the buses attach.
    if crate::boot::value("hidraw_load").is_some_and(|v| v.eq_ignore_ascii_case("yes")) {
        hidbus::set_hidraw(s, true);
    }
    evdev::init(
        s,
        evdev::Clocks {
            realtime: || crate::shared_page::to_ns(crate::shared_page::realtime()),
            uptime: || crate::shared_page::to_ns(crate::shared_page::uptime()),
        },
    );
    if input::start(s).is_err() {
        kprintln!("input: no keyboard thread");
    }
    let nexus = m.adopt(s, root, "nexus", "", bus::Ident::Platform);
    platform_rmans(m);
    #[cfg(target_arch = "x86_64")]
    {
        let apic = m.adopt(s, nexus, "apic", "APIC resources", bus::Ident::Platform);
        // The local APIC's and I/O APICs' registers, as FreeBSD's apic0
        // holds them (x86/io_apic.c: LAPIC_MEM_REGION, IOAPIC_MEM_REGION).
        let mut windows = alloc::vec![(crate::arch::apic::xapic_phys(), 0x540)];
        windows.extend(
            crate::intc::ioapic_windows()
                .into_iter()
                .map(|pa| (pa, 0x20)),
        );
        for (pa, len) in windows {
            m.hold(apic, apic, bus::RmanKind::Memory, (pa, pa + len - 1), None);
        }
    }
    let _ = NEXUS.set(nexus);
    // The GTDT's non-secure SBSA watchdogs.
    #[cfg(target_arch = "aarch64")]
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
    let took_us = crate::time::now_ns().saturating_sub(began) / 1000;
    // Then what drivers deferred to that point, in the order they
    // deferred it (FreeBSD's interrupt-driven configuration hooks).
    bus::run_config_hooks(s);
    crate::time::set_realtime(s);
    kprintln!(
        "bus: device manager up, {} driver(s), {workers} attach worker(s), attached in {}.{:03} ms{}",
        drivers.len(),
        took_us / 1000,
        took_us % 1000,
        match (verifier, faults) {
            (false, 0) => alloc::string::String::new(),
            (true, 0) => ", verifier on".into(),
            (_, n) => alloc::format!(", verifier on, failing one grant in {n}"),
        }
    );
}

/// A node named `name` under `root`, with its parent.
pub(crate) fn find_named(
    m: &bus::Manager,
    s: &sched::Sleepable,
    root: bus::DeviceId,
    name: &str,
) -> Option<(bus::DeviceId, bus::DeviceId)> {
    let mut todo = alloc::vec![root];
    while let Some(p) = todo.pop() {
        for c in m.children(p, s) {
            if m.nameunit(c, s).as_deref() == Some(name) {
                return Some((p, c));
            }
            todo.push(c);
        }
    }
    None
}

/// `hint.<name>.<unit>.disabled` from the loader's variables, non-zero
/// (FreeBSD's `resource_disabled`, which reads it as an integer).
fn disabled_hint(name: &str, unit: u32) -> bool {
    let key = alloc::format!("hint.{name}.{unit}.disabled");
    crate::boot::value(&key)
        .and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|v| v != 0)
}
