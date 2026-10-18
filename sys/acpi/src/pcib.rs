// SPDX-License-Identifier: BSD-2-Clause
//! `pcib`: the ACPI host-PCI bridge (FreeBSD's `acpi_pcib`) and the ACPI
//! PCI-PCI bridge (`acpi_pcib_pci`), each with its `pci` bus as its child.
//!
//! - The host bridge's `_OSC` is told what the OS supports and asked for
//!   no control at attach, as FreeBSD does; drivers that take over native
//!   hotplug or AER will ask for those.
//! - The host bridge's segment is `_SEG` and its root bus `_BBN`, else
//!   the start of the `_CRS` bus-number range, else 0.
//! - `_PRT` routes INTx: a GSI directly, or through a link device's
//!   current `_CRS` interrupt. A PCI-PCI bridge without one swizzles.
//! - The `_ADR` devices below a bridge give PCI children their ACPI
//!   handles.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use aml::resource::{AddressKind, Resource as AmlResource};
use aml::{PrtSource, Value};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::ids::{AcpiId, PciId};
use devtree::matching::Rank;
use pci::bus::{PciBusData, PciChild};

use crate::bus::AcpiDevice;
use crate::service;

/// The PCI host bridge `_OSC` UUID, 33DB4D5B-1FF7-401C-9657-7441C03DD766,
/// in its byte order.
const PCI_HOST_BRIDGE_UUID: [u8; 16] = [
    0x5b, 0x4d, 0xdb, 0x33, 0xf7, 0x1f, 0x1c, 0x40, 0x96, 0x57, 0x74, 0x41, 0xc0, 0x3d, 0xd7, 0x66,
];

/// `_OSC` support bits (PCI Firmware Specification 3.3 §4.5.1).
pub mod osc_support {
    /// Extended (4 KiB) configuration space.
    pub const EXT_CONFIG: u32 = 0x01;
    /// Active state power management.
    pub const ASPM: u32 = 0x02;
    /// Clock power management.
    pub const CLOCK_PM: u32 = 0x04;
    /// PCI segment groups.
    pub const SEGMENTS: u32 = 0x08;
    /// MSI.
    pub const MSI: u32 = 0x10;
}

/// `_OSC` control bits (PCI Firmware Specification 3.3 §4.5.1).
pub mod osc_control {
    /// PCIe native hotplug.
    pub const PCIE_HOTPLUG: u32 = 0x01;
    /// Standard hotplug controller native hotplug.
    pub const SHPC_HOTPLUG: u32 = 0x02;
    /// PCIe native power management events.
    pub const PCIE_PME: u32 = 0x04;
    /// PCIe advanced error reporting.
    pub const PCIE_AER: u32 = 0x08;
    /// The PCIe capability structure.
    pub const PCIE_CAPABILITY: u32 = 0x10;
}

/// What the OS supports, as FreeBSD declares it: ASPM and clock PM only
/// on x86, since some arm64 firmware powers devices off when the OS
/// claims them.
#[must_use]
pub fn osc_supported() -> u32 {
    use osc_support::{ASPM, CLOCK_PM, EXT_CONFIG, MSI, SEGMENTS};
    let base = EXT_CONFIG | SEGMENTS | MSI;
    if cfg!(target_arch = "x86_64") {
        base | ASPM | CLOCK_PM
    } else {
        base
    }
}

/// A host bridge's answer to `_OSC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Osc {
    /// The status word: bit 1 failure, bit 2 unrecognized UUID, bit 3
    /// unrecognized revision, bit 4 capabilities masked.
    pub status: u32,
    /// The controls granted.
    pub control: u32,
}

impl Osc {
    /// Whether firmware reported an error (masking a request is not one).
    #[must_use]
    pub fn failed(&self) -> bool {
        self.status & 0b1110 != 0
    }
}

/// Evaluates `_OSC` under `node` with `support` and asking for `control`.
/// `None` if there is no `_OSC` or its answer is not three dwords.
pub fn osc(
    interp: &mut aml::Interpreter,
    node: aml::NodeId,
    host: &mut dyn aml::Host,
    support: u32,
    control: u32,
) -> Option<Osc> {
    let mut caps = Vec::with_capacity(12);
    for d in [0, support, control] {
        caps.extend_from_slice(&d.to_le_bytes());
    }
    let args = alloc::vec![
        Value::Buffer(PCI_HOST_BRIDGE_UUID.to_vec()),
        Value::Integer(1),
        Value::Integer(3),
        Value::Buffer(caps),
    ];
    let Ok(Some(Value::Buffer(b))) = interp.evaluate_child(node, "_OSC", args, host) else {
        return None;
    };
    let dword = |i: usize| {
        Some(u32::from_le_bytes(
            b.get(4 * i..4 * i + 4)?.try_into().ok()?,
        ))
    };
    if b.len() != 12 {
        return None;
    }
    Some(Osc {
        status: dword(0)?,
        control: dword(2)?,
    })
}

/// A route `_PRT` gives, before the interrupt is looked up.
enum Route {
    Gsi(u32, irq::Trigger, irq::Polarity),
    Isa(u8),
}

/// What a bridge's namespace node says about the bus behind it.
struct Below {
    adr_paths: Vec<(u32, String)>,
    /// `_PRT`, if the node has one.
    routes: Option<Vec<(u8, u8, Route)>>,
}

struct Found {
    osc: Option<Osc>,
    segment: u32,
    bus: u8,
    below: Below,
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
        let osc = osc(interp, node, host, osc_supported(), 0);
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
        let below = below(interp, node, host);
        Found {
            osc,
            segment,
            bus,
            below,
        }
    }
}

fn below(interp: &mut aml::Interpreter, node: aml::NodeId, host: &mut dyn aml::Host) -> Below {
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
    let prt = interp.prt(node, host).ok().flatten();
    let has_prt = prt.is_some();
    let mut routes = Vec::new();
    for e in prt.unwrap_or_default() {
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
    Below {
        adr_paths,
        routes: has_prt.then_some(routes),
    }
}

/// Looks the routes' interrupts up.
fn resolve(ops: &dyn crate::Ops, routes: Vec<(u8, u8, Route)>) -> Vec<(u8, u8, Resource)> {
    routes
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
        .collect()
}

/// `pcibN` (ACPI host bridge).
#[derive(Debug)]
pub struct AcpiPcib {
    data: Arc<PciBusData>,
    /// The `_OSC` answer at attach, if the bridge has one.
    pub osc: Option<Osc>,
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
        if let Some(o) = found.osc.filter(Osc::failed) {
            ops.log(format_args!(
                "pcib{}: _OSC failed: status {:#x}",
                cx.unit(),
                o.status
            ));
        }
        let cfg = ops
            .pci_config(pci::PciAddress {
                domain: found.segment,
                bus: found.bus,
                slot: 0,
                function: 0,
            })
            .ok_or(AttachError::Device("no configuration space for the bridge"))?;
        let intx = resolve(ops, found.below.routes.unwrap_or_default());
        Ok(Self {
            osc: found.osc,
            data: Arc::new(PciBusData {
                cfg,
                adr_paths: found.below.adr_paths,
                intx,
                depth: 0,
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

/// `pcibN` (ACPI PCI-PCI bridge): a bridge with a namespace node, whose
/// `_PRT` (if any) routes the interrupts behind it.
#[derive(Debug)]
pub struct AcpiPciBridge {
    data: Arc<PciBusData>,
    domain: u32,
    bus: u8,
}

impl Driver for AcpiPciBridge {
    const NAME: &'static str = "pcib";
    // FreeBSD's acpi_pcib_pci probes at -1000, above the plain bridge.
    const IDS: Ids = Ids::Pci(&[PciId::subclass(0x06, 0x04).rank(Rank(-1000))]);

    fn describe(_: &Ident) -> String {
        "ACPI PCI-PCI bridge".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let Ident::Pci(_, _, Some(path)) = cx.ident() else {
            return Err(AttachError::Decline);
        };
        let path = aml::AmlPath::parse(path).map_err(|_| AttachError::Decline)?;
        let svc = service().ok_or(AttachError::Decline)?;
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let found = svc.run(cx.sleepable(), move |interp, host| {
            interp.lookup(&path).map(|node| below(interp, node, host))
        });
        let found = found.ok_or(AttachError::Decline)?;
        let intx = found.routes.map(|r| resolve(svc.ops(), r));
        let (bus, data) = pci::bridge::secondary(&child, found.adr_paths, intx)?;
        Ok(Self {
            data: Arc::new(data),
            domain: child.function.addr.domain,
            bus,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.add_child(
            Ident::PciBus {
                domain: self.domain,
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
pub const PCI_PCI_BRIDGE: DriverDecl = DriverDecl::of::<AcpiPciBridge>();
