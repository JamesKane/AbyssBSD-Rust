// SPDX-License-Identifier: BSD-2-Clause
//! The kernel services behind the ACPI bus (`sys/acpi`): physical memory
//! and ports for AML operation regions, configuration space, time, and
//! interrupt routing.

#![allow(
    clippy::cast_possible_truncation,
    reason = "region accesses narrow values to the declared access width"
)]

use alloc::string::String;
use alloc::vec::Vec;

use addr::PhysAddr;
use sync::SpinLock;

/// Device pages AML has touched, mapped once and kept, as FreeBSD's
/// `AcpiOsMapMemory` mappings are for the regions AML declares.
static MAPPED: SpinLock<Vec<(u64, u64)>> = SpinLock::new(Vec::new());

/// The virtual address of `pa`, through the DMAP for RAM or a cached
/// uncached mapping for anything else. The access must not cross a page.
fn va_of(pa: u64, bytes: u8) -> Option<u64> {
    let end = pa.checked_add(u64::from(bytes))?;
    if pa / 4096 != (end - 1) / 4096 {
        return None;
    }
    let in_ram = crate::boot::memory_map().iter().any(|r| {
        let range = r.range();
        r.kind.is_ram() && range.start.get() <= pa && end <= range.end.get()
    });
    if in_ram {
        return bootproto::layout::dmap(PhysAddr::new(pa)).map(|v| v.get());
    }
    let page = pa & !4095;
    let mut m = MAPPED.lock();
    let base = match m.iter().find(|(p, _)| *p == page) {
        Some(&(_, va)) => va,
        None => {
            let va = crate::mm::map_device(PhysAddr::new(page), 4096).ok()?.get();
            m.push((page, va));
            va
        }
    };
    Some(base + (pa - page))
}

struct KernelAcpi;

impl acpi::Ops for KernelAcpi {
    fn mem_read(&self, pa: u64, bytes: u8) -> Option<u64> {
        let va = va_of(pa, bytes)?;
        let p = core::ptr::with_exposed_provenance::<u8>(usize::try_from(va).ok()?);
        if bytes > 1 && pa.is_multiple_of(u64::from(bytes)) {
            // One access of the declared width: registers can have read
            // side effects.
            // SAFETY: `va_of` gave a mapping covering the access (RAM in
            // the DMAP, or a device page mapped for AML), aligned to the
            // width.
            return Some(unsafe {
                match bytes {
                    2 => u64::from(p.cast::<u16>().read_volatile()),
                    4 => u64::from(p.cast::<u32>().read_volatile()),
                    _ => p.cast::<u64>().read_volatile(),
                }
            });
        }
        let mut v = 0u64;
        for i in 0..bytes {
            // SAFETY: as above; byte accesses avoid misaligned wider ones.
            v |= u64::from(unsafe { p.add(usize::from(i)).read_volatile() }) << (8 * i);
        }
        Some(v)
    }

    fn mem_write(&self, pa: u64, bytes: u8, v: u64) -> Option<()> {
        let va = va_of(pa, bytes)?;
        let p = core::ptr::with_exposed_provenance_mut::<u8>(usize::try_from(va).ok()?);
        if bytes > 1 && pa.is_multiple_of(u64::from(bytes)) {
            // SAFETY: as in `mem_read`, aligned to the width.
            unsafe {
                match bytes {
                    2 => p.cast::<u16>().write_volatile(v as u16),
                    4 => p.cast::<u32>().write_volatile(v as u32),
                    _ => p.cast::<u64>().write_volatile(v),
                }
            }
        } else {
            for i in 0..bytes {
                // SAFETY: as in `mem_read`.
                unsafe { p.add(usize::from(i)).write_volatile((v >> (8 * i)) as u8) };
            }
        }
        Some(())
    }

    #[cfg(target_arch = "x86_64")]
    fn io_read(&self, port: u16, bytes: u8) -> Option<u32> {
        // SAFETY: AML declared this port in an operation region; FreeBSD
        // lets firmware's AML access its own ports the same way.
        Some(unsafe {
            match bytes {
                1 => u32::from(crate::arch::cpu::inb(port)),
                2 => u32::from(crate::arch::cpu::inw(port)),
                _ => crate::arch::cpu::inl(port),
            }
        })
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn io_read(&self, _port: u16, _bytes: u8) -> Option<u32> {
        None
    }

    #[cfg(target_arch = "x86_64")]
    fn io_write(&self, port: u16, bytes: u8, v: u32) -> Option<()> {
        // SAFETY: as in `io_read`.
        unsafe {
            match bytes {
                1 => crate::arch::cpu::outb(port, v as u8),
                2 => crate::arch::cpu::outw(port, v as u16),
                _ => crate::arch::cpu::outl(port, v),
            }
        }
        Some(())
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn io_write(&self, _port: u16, _bytes: u8, _v: u32) -> Option<()> {
        None
    }

    fn pci_config(&self, a: pci::PciAddress) -> Option<&'static (dyn pci::ConfigSpace + Sync)> {
        crate::pci::config_for(a)
    }

    fn sleep_ms(&self, s: &sched::Sleepable, ms: u64) {
        crate::time::sleep_ns(s, ms.saturating_mul(1_000_000));
    }

    fn stall_us(&self, us: u64) {
        let ticks = u128::from(us) * u128::from(crate::time::frequency()) / 1_000_000;
        let start = crate::time::counter();
        while u128::from(crate::time::counter().wrapping_sub(start)) < ticks {
            core::hint::spin_loop();
        }
    }

    fn timer_100ns(&self) -> u64 {
        let f = u128::from(crate::time::frequency().max(1));
        (u128::from(crate::time::counter()) * 10_000_000 / f) as u64
    }

    fn gsi(&self, gsi: u32) -> Option<(&'static dyn irq::IrqDomain, u32)> {
        Some((crate::intc::gsi_domain(), gsi))
    }

    #[cfg(target_arch = "x86_64")]
    fn isa_irq(&self, irq: u8) -> (u32, irq::Trigger, irq::Polarity) {
        crate::intc::isa_irq(irq)
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn isa_irq(&self, irq: u8) -> (u32, irq::Trigger, irq::Polarity) {
        (u32::from(irq), irq::Trigger::Edge, irq::Polarity::High)
    }

    fn definition_blocks(&self) -> Vec<&'static [u8]> {
        let mut out = Vec::new();
        let Some(t) = crate::acpi::tables() else {
            return out;
        };
        let dsdt = crate::acpi::find(*b"FACP")
            .and_then(|f| acpi_tables::fadt::parse(&f).ok())
            .and_then(|f| f.dsdt())
            .and_then(|pa| t.read(pa).ok());
        out.extend(dsdt.map(|d| d.bytes()));
        out.extend(
            t.iter()
                .filter_map(Result::ok)
                .filter(|s| s.signature() == *b"SSDT")
                .map(|s| s.bytes()),
        );
        out
    }

    fn oem_description(&self) -> String {
        let Some(t) = crate::acpi::tables() else {
            return String::new();
        };
        let x = t.xsdt();
        // As FreeBSD builds it: each field whole, trailing whitespace
        // trimmed (a NUL stops the trim), joined by a space, and the
        // result read as a C string. bhyve's "BVFACP \0" keeps its space.
        let mut b: Vec<u8> = Vec::new();
        let trim = |b: &mut Vec<u8>| {
            while b.last().is_some_and(u8::is_ascii_whitespace) {
                b.pop();
            }
        };
        b.extend_from_slice(x.oem_id());
        trim(&mut b);
        b.push(b' ');
        b.extend_from_slice(x.oem_table_id());
        trim(&mut b);
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into()
    }

    fn log(&self, args: core::fmt::Arguments<'_>) {
        kprintln!("{args}");
    }
}

static OPS: KernelAcpi = KernelAcpi;

/// Loads the AML namespace on its own thread. Returns whether ACPI is
/// usable.
pub(crate) fn init(s: &sched::Sleepable) -> bool {
    match acpi::init(&OPS, s) {
        Some(l) => {
            kprintln!(
                "acpi: {} definition block(s), {} namespace nodes{}{}{}",
                l.tables,
                l.nodes,
                if l.failed > 0 {
                    alloc::format!(", {} failed", l.failed)
                } else {
                    String::new()
                },
                if l.warnings > 0 {
                    alloc::format!(", {} load warning(s)", l.warnings)
                } else {
                    String::new()
                },
                if l.init_errors > 0 {
                    alloc::format!(", {} _STA/_INI error(s)", l.init_errors)
                } else {
                    String::new()
                },
            );
            true
        }
        None => false,
    }
}
