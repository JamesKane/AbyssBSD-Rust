// SPDX-License-Identifier: BSD-2-Clause
//! The `acpi` bus driver: `acpi0` on `nexus0`, with a child for every
//! namespace device that has a `_HID` or `_CID`, as FreeBSD's
//! `acpi_probe_child` adds them.
//!
//! - Children are added in namespace walk order, which is the order
//!   `devinfo` lists them in; attach order between suppliers and
//!   consumers comes from the dependency graph, not from FreeBSD's probe
//!   orders.
//! - A device whose `_STA` says it is absent is left out, except PCI link
//!   devices and the RTC, which FreeBSD always keeps.
//! - Children get the resources their `_CRS` consumes; ranges a bridge
//!   produces stay with the bridge's driver.
//! - `_DEP` becomes a dependency edge on the named device when that device
//!   was added first.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use aml::resource::{AddressKind, Resource as AmlResource};
use aml::{DepRef, Uid};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::graph::EdgeKind;
use devtree::ids::AcpiIdent;

use crate::{Ops, service};

/// What a child of `acpi0` carries for its driver (its bus data).
#[derive(Clone, Debug)]
pub struct AcpiDevice {
    /// The namespace node.
    pub node: aml::NodeId,
    /// Its path (`\_SB_.PCI0`).
    pub path: String,
    /// Every `_CRS` descriptor, including produced ranges (a host bridge's
    /// windows).
    pub crs: Vec<AmlResource>,
}

/// One device found in the namespace.
struct Found {
    device: AcpiDevice,
    ident: AcpiIdent,
    deps: Vec<String>,
}

/// IDs FreeBSD keeps even when `_STA` says absent.
fn always_present(ident: &AcpiIdent) -> bool {
    ident
        .hid
        .as_deref()
        .is_some_and(|h| h == "PNP0C0F" || h == "PNP0B00")
}

fn walk(interp: &mut aml::Interpreter, host: &mut dyn aml::Host) -> Vec<Found> {
    let mut out = Vec::new();
    for node in interp.devices() {
        let path = alloc::format!("{}", interp.path(node));
        if path == "\\_TZ_" {
            continue;
        }
        let hid = interp.hid(node, host).ok().flatten();
        let cids = interp.cids(node, host).unwrap_or_default();
        if hid.is_none() && cids.is_empty() {
            continue;
        }
        let uid = interp.uid(node, host).ok().flatten().map(|u| match u {
            Uid::Integer(n) => alloc::format!("{n}"),
            Uid::String(s) => s,
        });
        let ident = AcpiIdent { hid, cids, uid };
        let present = interp.sta(node, host).map_or(true, |sta| sta & 1 != 0);
        if !present && !always_present(&ident) {
            continue;
        }
        let crs = interp.crs(node, host).ok().flatten().unwrap_or_default();
        let deps = interp
            .dep(node, host)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|d| match d {
                DepRef::Resolved(n) => Some(alloc::format!("{}", interp.path(n))),
                DepRef::Unresolved(_) => None,
            })
            .collect();
        out.push(Found {
            device: AcpiDevice { node, path, crs },
            ident,
            deps,
        });
    }
    out
}

fn trigger(t: aml::resource::Trigger) -> irq::Trigger {
    match t {
        aml::resource::Trigger::Edge => irq::Trigger::Edge,
        aml::resource::Trigger::Level => irq::Trigger::Level,
    }
}

fn polarity(p: aml::resource::Polarity) -> irq::Polarity {
    match p {
        aml::resource::Polarity::ActiveLow => irq::Polarity::Low,
        _ => irq::Polarity::High,
    }
}

/// The resources a device consumes, in `_CRS` order.
fn resources(ops: &dyn Ops, crs: &[AmlResource]) -> Vec<Resource> {
    let mut out = Vec::new();
    let irq_of = |gsi: u32, t: irq::Trigger, p: irq::Polarity, out: &mut Vec<Resource>| {
        if let Some((domain, hw)) = ops.gsi(gsi) {
            out.push(Resource::Irq {
                domain,
                hw,
                trigger: t,
                polarity: p,
            });
        }
    };
    for r in crs {
        match r {
            AmlResource::Irq(i) => {
                for bit in (0..16u8).filter(|b| i.mask & (1 << b) != 0) {
                    let (gsi, t, p) = ops.isa_irq(bit);
                    irq_of(gsi, t, p, &mut out);
                }
            }
            AmlResource::ExtendedIrq(e) if e.consumer && e.source.is_none() => {
                for &gsi in &e.interrupts {
                    irq_of(gsi, trigger(e.trigger), polarity(e.polarity), &mut out);
                }
            }
            AmlResource::Io(io) if io.length > 0 => out.push(Resource::IoPort {
                base: io.min,
                len: u16::from(io.length),
            }),
            AmlResource::FixedIo { base, length } if *length > 0 => out.push(Resource::IoPort {
                base: *base,
                len: u16::from(*length),
            }),
            AmlResource::Memory24(m) | AmlResource::Memory32(m) | AmlResource::FixedMemory32(m)
                if m.length > 0 =>
            {
                out.push(Resource::Memory {
                    base: u64::from(m.min),
                    len: u64::from(m.length),
                    prefetchable: false,
                });
            }
            AmlResource::Address(a) if a.consumer && a.length > 0 => match a.kind {
                AddressKind::Memory { .. } => out.push(Resource::Memory {
                    base: a.min,
                    len: a.length,
                    prefetchable: false,
                }),
                AddressKind::Io { .. } => {
                    if let (Ok(base), Ok(len)) = (u16::try_from(a.min), u16::try_from(a.length)) {
                        out.push(Resource::IoPort { base, len });
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    out
}

/// `acpi0`.
#[derive(Debug)]
pub struct AcpiBus;

impl Driver for AcpiBus {
    const NAME: &'static str = "acpi";
    const IDS: Ids = Ids::Nexus(&["acpi"]);

    fn describe(_: &Ident) -> String {
        service()
            .map(|s| s.ops().oem_description())
            .unwrap_or_default()
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        if service().is_none() {
            return Err(AttachError::Device("no ACPI namespace"));
        }
        Ok(Self)
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let svc = service().ok_or(AttachError::Device("no ACPI namespace"))?;
        let found = svc.run(cx.sleepable(), walk);
        let mut added: Vec<(String, bus::DeviceId)> = Vec::new();
        for f in found {
            let res = resources(svc.ops(), &f.device.crs);
            let deps: Vec<(bus::DeviceId, EdgeKind)> = f
                .deps
                .iter()
                .filter_map(|p| {
                    added
                        .iter()
                        .find(|(q, _)| q == p)
                        .map(|(_, id)| (*id, EdgeKind::Required))
                })
                .collect();
            let path = f.device.path.clone();
            let id = cx.add_child_with_deps(
                Ident::Acpi(f.ident, path.clone()),
                Some(Arc::new(f.device)),
                res,
                &deps,
            );
            added.push((path, id));
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<AcpiBus>();
