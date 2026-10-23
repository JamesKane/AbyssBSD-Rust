// SPDX-License-Identifier: BSD-2-Clause
//! eXtensible Host Controller Interface controllers (`xhci`), on PCI
//! (class `0c0330`) and on ACPI (`PNP0D10`, as on the Sky1), each with a
//! `usbus` child (USB.md §4).
//!
//! - Attach takes the controller from firmware, allocates the device
//!   context array, scratchpad buffers, the command ring, and one event
//!   ring, and picks MSI-X, else MSI, else INTx. Start installs the
//!   handler and only then resets and starts the controller, in FreeBSD's
//!   order (`xhci_pci_attach`).
//! - Commands are polled to completion from thread context, so they work
//!   whether or not interrupts do; transfers complete in the interrupt
//!   thread. A watchdog thread times transfers out.
//! - Stop halts the controller and fails what it still held.
//!
//! Everything the controller writes is checked: an event names a slot, an
//! endpoint, and a TRB of an outstanding transfer, or it is ignored.
//!
//! Behavior reference: sys/dev/usb/controller/xhci.c, xhci_pci.c; the
//! xHCI 1.2 specification for the registers.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod hc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, StartCx, StopCx, StopReason,
};
use devtree::ids::{AcpiId, PciId};
use irq::Filter;
use pci::bus::{Messages, PciChild};
use sched::{IrqCx, Sleepable, Thread};
use sync::SpinLock;
use usbus::HostController;
use usbus::bus::UsbusData;

use crate::hc::{Hc, TICK_MS};

/// PCI vendors FreeBSD names (`xhci_pci_attach`'s `sc_vendor`).
const VENDORS: &[(u16, &str)] = &[
    (0x1022, "AMD"),
    (0x8086, "Intel"),
    (0x15ad, "VMware"),
    (0x1d17, "Zhaoxin"),
];

/// Controllers FreeBSD names (`xhci_pci_match`), those of the reference
/// machines and common emulators; the rest are generic.
const KNOWN: &[(u16, u16, &str)] = &[
    (
        0x1022,
        0x15b6,
        "AMD Raphael/Granite Ridge USB 3.1 controller",
    ),
    (
        0x1022,
        0x15b7,
        "AMD Raphael/Granite Ridge USB 3.1 controller",
    ),
    (
        0x1022,
        0x15b8,
        "AMD Raphael/Granite Ridge USB 2.0 controller",
    ),
    (0x1022, 0x43f7, "AMD 600 Series USB 3.2 controller"),
    (0x1022, 0x7814, "AMD FCH USB 3.0 controller"),
    (0x15ad, 0x0778, "VMware USB 3.0 controller"),
    (0x15ad, 0x0779, "VMware USB 3.0 controller"),
    (0x1b21, 0x3242, "ASMedia ASM3242 USB 3.2 controller"),
];

/// What the instance shares with its handlers and watchdog.
struct Shared {
    hc: Arc<Hc>,
    lines: SpinLock<Vec<IrqLine>>,
    watchdog: SpinLock<Option<Arc<Thread>>>,
    unit: u32,
    kind: &'static str,
}

impl Shared {
    fn start<D: Driver>(
        &self,
        cx: &mut StartCx<'_, D>,
        filter: fn(&D, usize, &IrqCx) -> Filter,
        thread: fn(&D, usize, &Sleepable),
    ) -> Result<(), AttachError> {
        self.hc.announce(self.unit);
        let lines = core::mem::take(&mut *self.lines.lock());
        for (v, line) in lines.into_iter().enumerate() {
            cx.interrupt(line, Some(filter), Some(thread), v)?;
        }
        // As FreeBSD's xhci_pci_attach: the handler first, then the reset
        // and start.
        self.hc.start_controller(cx.sleepable())?;
        let hc = Arc::clone(&self.hc);
        let t = sched::Builder::new("xhci watchdog")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| {
                while !hc.is_stopping() {
                    bus::manager().sleep_ms(s, u64::from(TICK_MS));
                    hc.process_events();
                    hc.tick(s);
                }
            })
            .map_err(|_| AttachError::NoMemory)?;
        *self.watchdog.lock() = Some(t);
        let hc: Arc<dyn HostController> = self.hc.clone();
        cx.add_child(
            Ident::Nexus("usbus"),
            Some(Arc::new(UsbusData { hc })),
            Vec::new(),
        );
        Ok(())
    }

    fn stop(&self, s: &Sleepable) {
        self.hc.shutdown(s);
        let t = self.watchdog.lock().take();
        if let Some(t) = t {
            t.join(s);
        }
    }
}

/// `xhci` on PCI.
pub struct XhciPci {
    shared: Shared,
    _messages: Option<Messages>,
}

impl core::fmt::Debug for XhciPci {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XhciPci")
            .field("unit", &self.shared.unit)
            .finish_non_exhaustive()
    }
}

fn pci_filter(d: &XhciPci, _: usize, _: &IrqCx) -> Filter {
    d.shared.hc.filter(d.shared.kind != "INTx")
}

fn pci_events(d: &XhciPci, _: usize, _: &Sleepable) {
    d.shared.hc.process_events();
}

impl Driver for XhciPci {
    const NAME: &'static str = "xhci";
    const IDS: Ids = Ids::Pci(&[PciId::class(0x0c_03_30)]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(ident: &Ident) -> String {
        if let Ident::Pci(i, _, _) = ident
            && let Some((_, _, d)) = KNOWN
                .iter()
                .find(|(v, dev, _)| *v == i.vendor && *dev == i.device)
        {
            return (*d).into();
        }
        "XHCI (generic) USB 3.0 controller".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let vendor_id = match cx.ident() {
            Ident::Pci(i, _, _) => i.vendor,
            _ => 0,
        };
        let vendor = VENDORS.iter().find(|(v, _)| *v == vendor_id).map_or_else(
            || alloc::format!("(0x{vendor_id:04x})"),
            |(_, n)| String::from(*n),
        );
        let mmio = cx.mmio_bytes(0)?;
        child.enable_decoding();
        child.set_bus_master(true);
        let unit = cx.unit();
        let (tag, data_tag) = hc::tags(cx, &mmio)?;
        let hc = Hc::new(
            cx.sleepable(),
            alloc::format!("xhci{unit}"),
            vendor,
            mmio,
            tag,
            data_tag,
        )?;
        let (lines, messages, kind) = match pci::bus::msix_lines(cx, 1) {
            Ok((l, m)) if !l.is_empty() => (l, Some(m), "MSI-X"),
            _ => match pci::bus::msi_line(cx) {
                Ok((l, m)) => (alloc::vec![l], Some(m), "MSI"),
                Err(_) => (alloc::vec![cx.irq(0)?], None, "INTx"),
            },
        };
        Ok(Self {
            shared: Shared {
                hc: Arc::new(hc),
                lines: SpinLock::new(lines),
                watchdog: SpinLock::new(None),
                unit,
                kind,
            },
            _messages: messages,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        self.shared.start(cx, pci_filter, pci_events)
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.shared.stop(cx.sleepable());
    }
}

/// `xhci` on ACPI (`PNP0D10`).
pub struct XhciAcpi {
    shared: Shared,
}

impl core::fmt::Debug for XhciAcpi {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XhciAcpi")
            .field("unit", &self.shared.unit)
            .finish_non_exhaustive()
    }
}

fn acpi_filter(d: &XhciAcpi, _: usize, _: &IrqCx) -> Filter {
    d.shared.hc.filter(false)
}

fn acpi_events(d: &XhciAcpi, _: usize, _: &Sleepable) {
    d.shared.hc.process_events();
}

impl Driver for XhciAcpi {
    const NAME: &'static str = "xhci";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("PNP0D10")]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(_: &Ident) -> String {
        "Generic USB 3.0 controller".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let mmio = cx.mmio_bytes(0)?;
        let line = cx.irq(0)?;
        let unit = cx.unit();
        let (tag, data_tag) = hc::tags(cx, &mmio)?;
        let hc = Hc::new(
            cx.sleepable(),
            alloc::format!("xhci{unit}"),
            String::from("Generic"),
            mmio,
            tag,
            data_tag,
        )?;
        Ok(Self {
            shared: Shared {
                hc: Arc::new(hc),
                lines: SpinLock::new(alloc::vec![line]),
                watchdog: SpinLock::new(None),
                unit,
                kind: "INTx",
            },
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        self.shared.start(cx, acpi_filter, acpi_events)
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.shared.stop(cx.sleepable());
    }
}

/// The PCI declaration for the build's driver table.
pub const PCI: DriverDecl = DriverDecl::of::<XhciPci>();
/// The ACPI declaration.
pub const ACPI: DriverDecl = DriverDecl::of::<XhciAcpi>();
