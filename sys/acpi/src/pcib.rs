// SPDX-License-Identifier: BSD-2-Clause
//! `pcib`: the ACPI host-PCI bridge (FreeBSD's `acpi_pcib`), with its
//! `pci` bus as its child.
//!
//! - The segment is `_SEG` and the root bus `_BBN`, else the start of the
//!   `_CRS` bus-number range, else 0.
//! - `_PRT` routes INTx: a GSI directly, or through a link device's
//!   current `_CRS` interrupt.
//! - The `_ADR` devices below the bridge give PCI children their ACPI
//!   handles.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use aml::resource::{AddressKind, Resource as AmlResource};
use aml::{PrtSource, Value};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::ids::AcpiId;
use pci::bus::PciBusData;

use crate::bus::AcpiDevice;
use crate::service;

/// A route `_PRT` gives, before the interrupt is looked up.
enum Route {
    Gsi(u32, irq::Trigger, irq::Polarity),
    Isa(u8),
}

struct Found {
    segment: u32,
    bus: u8,
    adr_paths: Vec<(u32, String)>,
    routes: Vec<(u8, u8, Route)>,
}

fn int(v: Option<Value>) -> Option<u64> {
    match v? {
        Value::Integer(n) => Some(n),
        _ => None,
    }
}

/// The interrupt a link device's current settings give, by index.
fn link_route(crs: &[AmlResource], index: u32) -> Option<Route> {
    let irqs: Vec<Route> = crs
        .iter()
        .filter_map(|r| match r {
            AmlResource::Irq(i) if i.mask != 0 => {
                u8::try_from(i.mask.trailing_zeros()).ok().map(Route::Isa)
            }
            AmlResource::ExtendedIrq(e) => e.interrupts.first().map(|&g| {
                let t = match e.trigger {
                    aml::resource::Trigger::Edge => irq::Trigger::Edge,
                    aml::resource::Trigger::Level => irq::Trigger::Level,
                };
                let p = match e.polarity {
                    aml::resource::Polarity::ActiveLow => irq::Polarity::Low,
                    _ => irq::Polarity::High,
                };
                Route::Gsi(g, t, p)
            }),
            _ => None,
        })
        .collect();
    irqs.into_iter().nth(index as usize)
}

fn probe(
    node: aml::NodeId,
    crs: &[AmlResource],
) -> impl FnOnce(&mut aml::Interpreter, &mut dyn aml::Host) -> Found + Send + 'static {
    let bus_from_crs = crs.iter().find_map(|r| match r {
        AmlResource::Address(a) if matches!(a.kind, AddressKind::BusNumber) => {
            u8::try_from(a.min).ok()
        }
        _ => None,
    });
    move |interp, host| {
        let segment = int(interp
            .evaluate_child(node, "_SEG", Vec::new(), host)
            .ok()
            .flatten())
        .and_then(|s| u32::try_from(s & 0xffff).ok())
        .unwrap_or(0);
        let bus = int(interp
            .evaluate_child(node, "_BBN", Vec::new(), host)
            .ok()
            .flatten())
        .and_then(|b| u8::try_from(b & 0xff).ok())
        .or(bus_from_crs)
        .unwrap_or(0);
        let mut adr_paths = Vec::new();
        for &c in interp.children(node).to_vec().iter() {
            if interp.object_type(c) != Some(aml::ObjectType::Device) {
                continue;
            }
            if let Ok(Some(adr)) = interp.adr(c, host)
                && let Ok(adr) = u32::try_from(adr)
            {
                adr_paths.push((adr, alloc::format!("{}", interp.path(c))));
            }
        }
        let mut routes = Vec::new();
        for e in interp.prt(node, host).ok().flatten().unwrap_or_default() {
            let Ok(slot) = u8::try_from(e.device) else {
                continue;
            };
            let route = match e.source {
                // A hard-wired GSI is level, active low (PCI INTx).
                PrtSource::Gsi(g) => Some(Route::Gsi(g, irq::Trigger::Level, irq::Polarity::Low)),
                PrtSource::Link { node: link, index } => interp
                    .crs(link, host)
                    .ok()
                    .flatten()
                    .and_then(|crs| link_route(&crs, index)),
                _ => None,
            };
            if let Some(r) = route {
                routes.push((slot, e.pin, r));
            }
        }
        Found {
            segment,
            bus,
            adr_paths,
            routes,
        }
    }
}

/// `pcibN` (ACPI host bridge).
#[derive(Debug)]
pub struct AcpiPcib {
    data: Arc<PciBusData>,
    segment: u32,
    bus: u8,
}

impl Driver for AcpiPcib {
    const NAME: &'static str = "pcib";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("PNP0A08"), AcpiId::new("PNP0A03")]);

    fn describe(_: &Ident) -> String {
        "ACPI Host-PCI bridge".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let svc = service().ok_or(AttachError::Device("no ACPI namespace"))?;
        let dev = cx
            .bus_data::<AcpiDevice>()
            .ok_or(AttachError::Device("not an ACPI device"))?;
        let found = svc.run(cx.sleepable(), probe(dev.node, &dev.crs));
        let ops = svc.ops();
        let cfg = ops
            .pci_config(pci::PciAddress {
                domain: found.segment,
                bus: found.bus,
                slot: 0,
                function: 0,
            })
            .ok_or(AttachError::Device("no configuration space for the bridge"))?;
        let intx = found
            .routes
            .into_iter()
            .filter_map(|(slot, pin, r)| {
                let (gsi, trigger, polarity) = match r {
                    Route::Gsi(g, t, p) => (g, t, p),
                    Route::Isa(i) => ops.isa_irq(i),
                };
                let (domain, hw) = ops.gsi(gsi)?;
                Some((
                    slot,
                    pin,
                    Resource::Irq {
                        domain,
                        hw,
                        trigger,
                        polarity,
                    },
                ))
            })
            .collect();
        Ok(Self {
            data: Arc::new(PciBusData {
                cfg,
                adr_paths: found.adr_paths,
                intx,
            }),
            segment: found.segment,
            bus: found.bus,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.add_child(
            Ident::PciBus {
                domain: self.segment,
                bus: self.bus,
                acpi: true,
            },
            Some(Arc::clone(&self.data) as Arc<dyn core::any::Any + Send + Sync>),
            Vec::new(),
        );
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const PCIB: DriverDecl = DriverDecl::of::<AcpiPcib>();
