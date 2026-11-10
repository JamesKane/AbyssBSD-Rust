// SPDX-License-Identifier: BSD-2-Clause
//! Tables captured from QEMU 10.2 with edk2 firmware: `q35`, and `virt`
//! with `GICv3`, both `-smp 8,sockets=2,cores=2,threads=2` with two NUMA nodes
//! (CPUs 0–3 and 4–7, 256 MiB each, distance 21). Captured by dumping guest
//! RAM from the QEMU monitor once firmware reached its shell.
//!
//! And the reference machines' firmware: the Orange Pi 6 Plus's (CIX P1 /
//! Sky1, `tests/corpus/acpi/opi6p`) and the X870E's (AMI, `.../x870e`),
//! checked against what Linux (and FreeBSD) reported on them
//! (`tests/corpus/opi6p`, `tests/corpus/x870e`).

extern crate std;
use std::vec::Vec;

use crate::testutil::{Mem, exercise};
use crate::{AddressSpace, Sdt, dbg2, fadt, gtdt, hpet, madt, mcfg, pptt, slit, spcr, srat};

macro_rules! corpus {
    ($path:literal) => {
        Mem(include_bytes!(concat!("../../tests/corpus/", $path)).to_vec())
    };
}

/// The Orange Pi 6 Plus's tables.
macro_rules! opi6p {
    ($name:literal) => {
        Mem(include_bytes!(concat!("../../../../tests/corpus/acpi/opi6p/", $name)).to_vec())
    };
}

/// The X870E's tables.
macro_rules! x870e {
    ($name:literal) => {
        Mem(include_bytes!(concat!("../../../../tests/corpus/acpi/x870e/", $name)).to_vec())
    };
}

fn sdt(m: &Mem) -> Sdt<'_> {
    Sdt::read(&m, 0).unwrap()
}

#[test]
fn every_table_survives_the_exerciser() {
    for m in [
        corpus!("qemu-x86_64/apic.dat"),
        corpus!("qemu-x86_64/facp.dat"),
        corpus!("qemu-x86_64/hpet.dat"),
        corpus!("qemu-x86_64/mcfg.dat"),
        corpus!("qemu-x86_64/slit.dat"),
        corpus!("qemu-x86_64/srat.dat"),
        corpus!("qemu-aarch64/apic.dat"),
        corpus!("qemu-aarch64/dbg2.dat"),
        corpus!("qemu-aarch64/facp.dat"),
        corpus!("qemu-aarch64/gtdt.dat"),
        corpus!("qemu-aarch64/mcfg.dat"),
        corpus!("qemu-aarch64/pptt.dat"),
        corpus!("qemu-aarch64/slit.dat"),
        corpus!("qemu-aarch64/spcr.dat"),
        corpus!("qemu-aarch64/srat.dat"),
    ] {
        exercise(&sdt(&m));
    }
}

#[test]
fn x86_madt() {
    let m = corpus!("qemu-x86_64/apic.dat");
    let t = madt::parse(&sdt(&m)).unwrap();
    assert_eq!(t.local_apic_address(), 0xfee0_0000);
    assert!(t.pcat_compat());
    assert!(t.entries().all(|e| e.is_ok()));
    assert_eq!(t.enabled_cpu_count(), 8);
    let ids: Vec<_> = t.cpus().map(|c| c.hw_id).collect();
    assert_eq!(ids, (0..8).map(madt::HwId::Apic).collect::<Vec<_>>());
    let ioapic = t.entries().find_map(|e| match e {
        Ok(madt::MadtEntry::IoApic(a)) => Some(a),
        _ => None,
    });
    assert_eq!(
        ioapic.map(|a| (a.address, a.gsi_base)),
        Some((0xfec0_0000, 0))
    );
    // IRQ 0 is wired to GSI 2; the PCI link IRQs are level, active high.
    let isos: Vec<_> = t
        .entries()
        .filter_map(|e| match e {
            Ok(madt::MadtEntry::InterruptSourceOverride(o)) => Some(o),
            _ => None,
        })
        .collect();
    assert_eq!((isos[0].source, isos[0].gsi), (0, 2));
    assert_eq!(isos[2].source, 9);
    assert_eq!(isos[2].flags.trigger(), madt::Trigger::Level);
    assert_eq!(isos[2].flags.polarity(), madt::Polarity::ActiveHigh);
}

#[test]
fn aarch64_madt() {
    let m = corpus!("qemu-aarch64/apic.dat");
    let t = madt::parse(&sdt(&m)).unwrap();
    assert!(!t.pcat_compat());
    assert!(t.entries().all(|e| e.is_ok()));
    assert_eq!(t.enabled_cpu_count(), 8);
    let d = t.gic_distributor().unwrap();
    assert_eq!((d.physical_base, d.version), (0x0800_0000, 3));
    let cpus: Vec<_> = t.cpus().collect();
    assert_eq!(cpus.len(), 8);
    assert_eq!(cpus[5].uid, 5);
    assert_eq!(cpus[5].hw_id, madt::HwId::Mpidr(5));
    let gicc = t.entries().find_map(|e| match e {
        Ok(madt::MadtEntry::GicCpuInterface(g)) => Some(g),
        _ => None,
    });
    let gicc = gicc.unwrap();
    assert_eq!(gicc.performance_interrupt, 23);
    assert_eq!(gicc.efficiency_class, Some(0));
    assert_eq!(gicc.trbe_interrupt, None);
    assert!(t.entries().any(|e| e
        == Ok(madt::MadtEntry::GicRedistributor(madt::GicRedistributor {
            base: 0x080a_0000,
            length: 0xf6_0000,
        }))));
    assert!(t.entries().any(|e| e
        == Ok(madt::MadtEntry::GicIts(madt::GicIts {
            id: 0,
            physical_base: 0x0808_0000,
        }))));
}

#[test]
fn x86_fadt_revision_3() {
    let m = corpus!("qemu-x86_64/facp.dat");
    let f = fadt::parse(&sdt(&m)).unwrap();
    assert_eq!(f.major_version(), 3);
    assert_eq!(f.dsdt(), Some(0x1fe8_d000));
    assert_eq!(f.facs(), Some(0x1fef_0000));
    assert_eq!(f.sci_interrupt(), Some(9));
    assert_eq!(f.smi_command_port(), Some(0xb2));
    let tmr = f.pm_timer().unwrap();
    assert_eq!(
        (tmr.block.space, tmr.block.address, tmr.bits),
        (AddressSpace::Io, 0x608, 24)
    );
    assert_eq!(f.pm1a_event_block().map(|g| g.address), Some(0x600));
    assert_eq!(f.pm1a_control_block().map(|g| g.address), Some(0x604));
    assert_eq!(f.gpe0_block().map(|g| g.address), Some(0x620));
    assert_eq!(f.gpe0_len(), Some(16));
    assert!(!f.flags().unwrap().hw_reduced_acpi());
    assert!(f.iapc_boot_arch().unwrap().has_8042());
    assert_eq!(
        f.reset_register().map(|r| (r.register.address, r.value)),
        Some((0xcf9, 0xf))
    );
    assert_eq!(f.century(), Some(0x32));
    // 244 bytes: no sleep registers or hypervisor identity.
    assert_eq!(f.sleep_control_register(), None);
    assert_eq!(f.hypervisor_vendor(), None);
}

#[test]
fn aarch64_fadt_hardware_reduced() {
    let m = corpus!("qemu-aarch64/facp.dat");
    let f = fadt::parse(&sdt(&m)).unwrap();
    assert_eq!((f.major_version(), f.minor_version()), (6, Some(3)));
    assert!(f.flags().unwrap().hw_reduced_acpi());
    let arm = f.arm_boot_arch().unwrap();
    assert!(arm.psci_compliant() && arm.psci_use_hvc());
    assert_eq!(f.dsdt(), Some(0x47ff_1018));
    assert_eq!(f.pm_timer(), None);
    assert_eq!(f.pm1a_event_block(), None);
    assert_eq!(
        f.hypervisor_vendor(),
        Some(u64::from(u32::from_le_bytes(*b"QEMU")))
    );
}

#[test]
fn x86_hpet() {
    let m = corpus!("qemu-x86_64/hpet.dat");
    let h = hpet::parse(&sdt(&m)).unwrap();
    assert_eq!(h.base.address, 0xfed0_0000);
    assert_eq!(h.block_id.vendor_id(), 0x8086);
    assert_eq!(h.block_id.comparators(), 3);
    assert!(h.block_id.counter_64bit() && h.block_id.legacy_replacement());
}

#[test]
fn mcfg_windows() {
    for (m, base) in [
        (corpus!("qemu-x86_64/mcfg.dat"), 0xe000_0000),
        (corpus!("qemu-aarch64/mcfg.dat"), 0x40_1000_0000),
    ] {
        let t = mcfg::parse(&sdt(&m)).unwrap();
        let r: Vec<_> = t.regions().map(Result::unwrap).collect();
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].base, r[0].segment, r[0].start_bus, r[0].end_bus),
            (base, 0, 0, 0xff)
        );
    }
}

#[test]
fn numa_tables() {
    for m in [
        corpus!("qemu-x86_64/slit.dat"),
        corpus!("qemu-aarch64/slit.dat"),
    ] {
        let s = slit::parse(&sdt(&m)).unwrap();
        assert_eq!(s.locality_count(), 2);
        assert_eq!(
            (s.distance(0, 0), s.distance(0, 1), s.distance(1, 0)),
            (Some(10), Some(21), Some(21))
        );
    }
    let m = corpus!("qemu-x86_64/srat.dat");
    let s = srat::parse(&sdt(&m)).unwrap();
    let cpus: Vec<_> = s
        .entries()
        .filter_map(|e| match e.unwrap() {
            srat::SratEntry::ProcessorLocalApic(p) => Some((p.apic_id, p.domain)),
            _ => None,
        })
        .collect();
    assert_eq!(
        cpus,
        [
            (0, 0),
            (1, 0),
            (2, 0),
            (3, 0),
            (4, 1),
            (5, 1),
            (6, 1),
            (7, 1)
        ]
    );
    let mem: Vec<_> = s
        .entries()
        .filter_map(|e| match e.unwrap() {
            srat::SratEntry::MemoryAffinity(a) if a.flags.enabled() => {
                Some((a.domain, a.base, a.length))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        mem,
        [
            (0, 0, 0xa_0000),
            (0, 0x10_0000, 0xff0_0000),
            (1, 0x1000_0000, 0x1000_0000)
        ]
    );

    let m = corpus!("qemu-aarch64/srat.dat");
    let s = srat::parse(&sdt(&m)).unwrap();
    let cpus: Vec<_> = s
        .entries()
        .filter_map(|e| match e.unwrap() {
            srat::SratEntry::GiccAffinity(g) if g.enabled => Some((g.processor_uid, g.domain)),
            _ => None,
        })
        .collect();
    assert_eq!(
        cpus,
        [
            (0, 0),
            (1, 0),
            (2, 0),
            (3, 0),
            (4, 1),
            (5, 1),
            (6, 1),
            (7, 1)
        ]
    );
}

#[test]
fn aarch64_gtdt() {
    let m = corpus!("qemu-aarch64/gtdt.dat");
    let g = gtdt::parse(&sdt(&m)).unwrap();
    assert_eq!(g.cnt_control_base(), None);
    assert_eq!(g.cnt_read_base(), None);
    assert_eq!(g.secure_el1().map(|t| t.gsiv), Some(29));
    let ns = g.nonsecure_el1().unwrap();
    assert_eq!(ns.gsiv, 30);
    assert!(ns.flags.always_on() && !ns.flags.edge_triggered() && !ns.flags.active_low());
    assert_eq!(g.virtual_el1().map(|t| t.gsiv), Some(27));
    assert_eq!(g.nonsecure_el2().map(|t| t.gsiv), Some(26));
    assert_eq!(g.virtual_el2(), None);
    assert_eq!(g.platform_timers().count(), 0);
}

#[test]
fn aarch64_pptt() {
    let m = corpus!("qemu-aarch64/pptt.dat");
    let p = pptt::parse(&sdt(&m)).unwrap();
    assert_eq!(p.nodes().count(), 17);
    assert!(p.nodes().all(|n| n.is_ok()));
    let mut packages = Vec::new();
    let mut cores = Vec::new();
    for uid in 0..8 {
        let t = p.topology(uid).unwrap();
        assert!(t.is_thread());
        assert!(t.cluster.is_some());
        packages.push(t.package.unwrap().offset);
        cores.push(t.core.offset);
    }
    // Two sockets of two cores of two threads.
    assert_eq!(packages, [56, 56, 56, 56, 216, 216, 216, 216]);
    assert_eq!(cores, [96, 96, 156, 156, 256, 256, 316, 316]);
    assert_eq!(p.topology(8), None);
}

#[test]
fn aarch64_console() {
    let m = corpus!("qemu-aarch64/spcr.dat");
    let s = spcr::parse(&sdt(&m)).unwrap();
    assert_eq!(s.port.base.address, 0x0900_0000);
    // QEMU virt's PL011 is SPI 1, INTID 33.
    assert_eq!(s.gsiv, Some(33));
    assert_eq!(s.pc_irq, None);
    let m = corpus!("qemu-aarch64/dbg2.dat");
    let ports: Vec<_> = dbg2::serial_ports(&sdt(&m)).collect();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].map(|p| p.base.address), Ok(0x0900_0000));
}

#[test]
fn opi6p_every_table_survives_the_exerciser() {
    for m in [
        opi6p!("apic.dat"),
        opi6p!("csrt.dat"),
        opi6p!("dbg2.dat"),
        opi6p!("facp.dat"),
        opi6p!("gtdt.dat"),
        opi6p!("iort.dat"),
        opi6p!("mcfg.dat"),
        opi6p!("pcct.dat"),
        opi6p!("pptt.dat"),
        opi6p!("sdei.dat"),
    ] {
        exercise(&sdt(&m));
    }
}

#[test]
fn opi6p_madt() {
    let m = opi6p!("apic.dat");
    let t = madt::parse(&sdt(&m)).unwrap();
    assert!(t.entries().all(|e| e.is_ok()));
    assert_eq!(t.enabled_cpu_count(), 12);
    // GICv4.1: Linux reports "Using GICv4.1 mode".
    let d = t.gic_distributor().unwrap();
    assert_eq!((d.physical_base, d.version), (0x0e01_0000, 4));
    // The boot CPU is the first of the cluster at MPIDR 0xA00 (Linux: "CPU0:
    // found redistributor a00"); the A520s are 0x000-0x300.
    let mpidrs: Vec<_> = t.cpus().map(|c| c.hw_id).collect();
    let want: Vec<_> = [
        0xa00, 0xb00, 0, 0x100, 0x200, 0x300, 0x400, 0x500, 0x600, 0x700, 0x800, 0x900,
    ]
    .into_iter()
    .map(madt::HwId::Mpidr)
    .collect();
    assert_eq!(mpidrs, want);
    // Every efficiency class is 0, though Linux finds five capacities
    // (1024, 279, 905, 866, 984): the core classes come from _CPC.
    for e in t.entries() {
        if let Ok(madt::MadtEntry::GicCpuInterface(g)) = e {
            assert_eq!(g.efficiency_class, Some(0));
            assert_eq!(g.performance_interrupt, 23);
        }
    }
    // One redistributor region of 4 x 64 KiB frames per CPU: CPU0 (core 10)
    // at 0x0E310000, where Linux found it.
    let gicr = t.entries().find_map(|e| match e {
        Ok(madt::MadtEntry::GicRedistributor(r)) => Some(r),
        _ => None,
    });
    let gicr = gicr.unwrap();
    assert_eq!((gicr.base, gicr.length), (0x0e09_0000, 12 * 0x4_0000));
    assert_eq!(gicr.base + 10 * 0x4_0000, 0x0e31_0000);
    // Linux: "ITS [mem 0x0e050000-0x0e06ffff]".
    let its: Vec<_> = t
        .entries()
        .filter_map(|e| match e {
            Ok(madt::MadtEntry::GicIts(i)) => Some(i.physical_base),
            _ => None,
        })
        .collect();
    assert_eq!(its, [0x0e05_0000]);
}

#[test]
fn opi6p_fadt_psci_over_smc() {
    let m = opi6p!("facp.dat");
    let f = fadt::parse(&sdt(&m)).unwrap();
    assert_eq!((f.major_version(), f.minor_version()), (6, Some(3)));
    assert!(f.flags().unwrap().hw_reduced_acpi());
    let arm = f.arm_boot_arch().unwrap();
    assert!(arm.psci_compliant() && !arm.psci_use_hvc());
    assert_eq!(f.dsdt(), Some(0x7_f986_0018));
    assert_eq!(f.facs(), None);
}

/// No memory-mapped timer (HARDWARE.md §4), and the SBSA watchdog.
#[test]
fn opi6p_gtdt() {
    let m = opi6p!("gtdt.dat");
    let g = gtdt::parse(&sdt(&m)).unwrap();
    assert_eq!(g.cnt_control_base(), None);
    assert_eq!(g.cnt_read_base(), None);
    let gsivs = [
        g.secure_el1(),
        g.nonsecure_el1(),
        g.virtual_el1(),
        g.nonsecure_el2(),
        g.virtual_el2(),
    ]
    .map(|t| t.map(|t| t.gsiv));
    assert_eq!(gsivs, [Some(29), Some(30), Some(27), Some(26), Some(28)]);
    let timers: Vec<_> = g.platform_timers().map(Result::unwrap).collect();
    let [gtdt::PlatformTimer::Watchdog(w)] = timers.as_slice() else {
        panic!("{timers:?}")
    };
    assert_eq!(
        (w.refresh_frame, w.control_frame, w.gsiv),
        (0x1600_8000, 0x1600_3000, 408)
    );
    assert!(!w.edge_triggered && !w.active_low && !w.secure);
}

/// Four root complexes share one ECAM base in segment 0, each decoding
/// its own buses. Each window starts where Linux's `sky1-pcie-cix` found
/// it ("ECAM at [mem 0x2c000000-0x2fffffff] for [bus c0-ff]", ...), though
/// that driver gives the last three 48 buses where the MCFG gives 32.
#[test]
fn opi6p_mcfg_windows() {
    let m = opi6p!("mcfg.dat");
    let t = mcfg::parse(&sdt(&m)).unwrap();
    let r: Vec<_> = t.regions().map(Result::unwrap).collect();
    let got: Vec<_> = r
        .iter()
        .map(|r| {
            (
                r.segment,
                r.start_bus,
                r.end_bus,
                r.function_address(r.start_bus, 0, 0),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            (0, 0xc0, 0xff, Some(0x2c00_0000)),
            (0, 0x90, 0xaf, Some(0x2900_0000)),
            (0, 0x60, 0x7f, Some(0x2600_0000)),
            (0, 0x30, 0x4f, Some(0x2300_0000)),
        ]
    );
    assert!(r.iter().all(|r| r.base == 0x2000_0000));
}

/// The bring-up console (HARDWARE.md §4): no SPCR, so DBG2's PL011.
#[test]
fn opi6p_dbg2_console() {
    let m = opi6p!("dbg2.dat");
    let ports: Vec<_> = dbg2::serial_ports(&sdt(&m)).collect();
    assert_eq!(ports.len(), 1);
    let p = ports[0].unwrap();
    assert_eq!(p.base.address, 0x040d_0000);
    assert_eq!(p.kind, crate::UartKind::Pl011);
}

/// One package of five clusters, as Linux's `topology/cluster_id`: 1 (CPUs
/// 0-1), 2 (2-5, the A520s), 3 (6-7), 4 (8-9), 5 (10-11).
#[test]
fn opi6p_pptt_clusters() {
    let m = opi6p!("pptt.dat");
    let p = pptt::parse(&sdt(&m)).unwrap();
    assert!(p.nodes().all(|n| n.is_ok()));
    let mut clusters = Vec::new();
    for uid in 0..12 {
        let t = p.topology(uid).unwrap();
        assert!(!t.is_thread());
        assert_eq!(t.package.unwrap().offset, 36);
        clusters.push(t.cluster.unwrap().acpi_processor_id);
    }
    assert_eq!(clusters, [1, 1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 5]);
    assert_eq!(p.topology(12), None);
}

#[test]
fn x870e_every_table_survives_the_exerciser() {
    for m in [
        x870e!("apic.dat"),
        x870e!("bgrt.dat"),
        x870e!("facp.dat"),
        x870e!("fidt.dat"),
        x870e!("fpdt.dat"),
        x870e!("hpet.dat"),
        x870e!("hwin.dat"),
        x870e!("ivrs.dat"),
        x870e!("mcfg.dat"),
        x870e!("tpm2.dat"),
        x870e!("wdrt.dat"),
        x870e!("wpbt.dat"),
        x870e!("wsmt.dat"),
    ] {
        exercise(&sdt(&m));
    }
}

/// Linux: "IOAPIC[0]: apic_id 32, ... address 0xfec00000, GSI 0-23",
/// "IOAPIC[1]: apic_id 33, ... address 0xfec01000, GSI 24-55", and the
/// CPUs' APIC IDs in `/proc/cpuinfo` order: the first threads of the two
/// dies' cores (0-11, 16-27), then their siblings.
#[test]
fn x870e_madt() {
    let m = x870e!("apic.dat");
    let t = madt::parse(&sdt(&m)).unwrap();
    assert!(t.entries().all(|e| e.is_ok()));
    assert_eq!(t.local_apic_address(), 0xfee0_0000);
    assert!(t.pcat_compat());
    let ioapics: Vec<_> = t
        .entries()
        .filter_map(|e| match e {
            Ok(madt::MadtEntry::IoApic(a)) => Some((a.id, a.address, a.gsi_base)),
            _ => None,
        })
        .collect();
    assert_eq!(ioapics, [(32, 0xfec0_0000, 0), (33, 0xfec0_1000, 24)]);
    let enabled: Vec<_> = t.cpus().filter(|c| c.enabled).map(|c| c.hw_id).collect();
    let want: Vec<_> = [0, 2, 4, 6, 8, 10, 16, 18, 20, 22, 24, 26]
        .into_iter()
        .chain([1, 3, 5, 7, 9, 11, 17, 19, 21, 23, 25, 27])
        .map(madt::HwId::Apic)
        .collect();
    assert_eq!(enabled, want);
    assert_eq!(t.enabled_cpu_count(), 24);
    // Linux: "INT_SRC_OVR (bus 0 bus_irq 9 global_irq 9 low level)".
    let isos: Vec<_> = t
        .entries()
        .filter_map(|e| match e {
            Ok(madt::MadtEntry::InterruptSourceOverride(o)) => Some(o),
            _ => None,
        })
        .collect();
    assert_eq!((isos[0].source, isos[0].gsi), (0, 2));
    assert_eq!((isos[1].source, isos[1].gsi), (9, 9));
    assert_eq!(isos[1].flags.trigger(), madt::Trigger::Level);
    assert_eq!(isos[1].flags.polarity(), madt::Polarity::ActiveLow);
}

/// Linux: "PM-Timer IO Port: 0x808", "Reserving FACS table memory at
/// [mem 0x90462000-...]". The timer is 32 bits wide (`TMR_VAL_EXT`), though
/// Linux's `acpi_pm` always counts it as 24.
#[test]
fn x870e_fadt() {
    let m = x870e!("facp.dat");
    let f = fadt::parse(&sdt(&m)).unwrap();
    assert_eq!((f.major_version(), f.minor_version()), (6, Some(5)));
    assert!(!f.flags().unwrap().hw_reduced_acpi());
    assert_eq!(f.facs(), Some(0x9046_2000));
    assert_eq!(f.sci_interrupt(), Some(9));
    assert_eq!(f.smi_command_port(), Some(0xb2));
    let tmr = f.pm_timer().unwrap();
    assert_eq!(
        (tmr.block.space, tmr.block.address, tmr.bits),
        (AddressSpace::Io, 0x808, 32)
    );
    assert_eq!(f.pm1a_event_block().map(|g| g.address), Some(0x800));
    assert_eq!(f.pm1a_control_block().map(|g| g.address), Some(0x804));
    assert_eq!(f.gpe0_block().map(|g| g.address), Some(0x820));
    assert_eq!(f.gpe0_len(), Some(8));
    assert_eq!(
        f.reset_register().map(|r| (r.register.address, r.value)),
        Some((0xb2, 0xbe))
    );
    assert_eq!(f.century(), Some(0x32));
    // No 8042 declared (USB keyboards only).
    assert!(!f.iapc_boot_arch().unwrap().has_8042());
}

/// Linux: "ACPI: HPET id: 0x10228201 base: 0xfed00000".
#[test]
fn x870e_hpet() {
    let m = x870e!("hpet.dat");
    let h = hpet::parse(&sdt(&m)).unwrap();
    assert_eq!(h.base.address, 0xfed0_0000);
    assert_eq!(h.block_id.vendor_id(), 0x1022);
    assert_eq!(h.block_id.comparators(), 3);
    assert!(!h.block_id.counter_64bit() && h.block_id.legacy_replacement());
}

/// Linux: "PCI: ECAM [mem 0xe0000000-0xefffffff] ... [bus 00-ff]".
#[test]
fn x870e_mcfg() {
    let m = x870e!("mcfg.dat");
    let t = mcfg::parse(&sdt(&m)).unwrap();
    let r: Vec<_> = t.regions().map(Result::unwrap).collect();
    assert_eq!(r.len(), 1);
    assert_eq!(
        (r[0].base, r[0].segment, r[0].start_bus, r[0].end_bus),
        (0xe000_0000, 0, 0, 0xff)
    );
}
