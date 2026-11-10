// SPDX-License-Identifier: BSD-2-Clause
//! `cpu`: the processors, `ACPI0007` devices and `Processor` objects. A
//! processor's unit is its CPU's ID, found through its UID in the MADT, so
//! `dev.cpu.N` is CPU N. Under the first processor of each frequency domain whose
//! `_CPC` controls frequency it adds `acpi_cppc` ([`crate::cppc`]), as
//! that driver's identify routine does under FreeBSD's `cpu` devices.
//!
//! Behavior reference: sys/dev/acpica/acpi_cpu.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    Announce, AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use devtree::ids::AcpiKind;

use crate::bus::AcpiDevice;
use crate::cppc::CppcChild;
use crate::service::service;

/// `cpuN`.
#[derive(Debug)]
pub struct AcpiCpu {
    cpu: u32,
    node: aml::NodeId,
}

/// The CPU of a processor, from its UID.
fn cpu_of(bus_data: Option<&(dyn core::any::Any + Send + Sync)>) -> Option<u32> {
    let uid = bus_data?.downcast_ref::<AcpiDevice>()?.processor?;
    service()?.ops().cpu_of_uid(uid)
}

impl Driver for AcpiCpu {
    const NAME: &'static str = "cpu";
    const IDS: Ids = Ids::AcpiKind(AcpiKind::Processor);
    const ANNOUNCE: Announce = Announce::FirstUnit;

    fn describe(_: &Ident) -> String {
        "ACPI CPU".into()
    }

    fn unit(_: &Ident, bus_data: Option<&(dyn core::any::Any + Send + Sync)>) -> Option<u32> {
        cpu_of(bus_data)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let dev = cx.bus_data::<AcpiDevice>().ok_or(AttachError::Decline)?;
        // A processor no CPU runs (disabled in the MADT), or one whose
        // unit another took: `acpi_cpu_probe` refuses it.
        let cpu = cpu_of(Some(dev)).ok_or(AttachError::Decline)?;
        if cx.unit() != cpu {
            return Err(AttachError::Decline);
        }
        Ok(Self {
            cpu,
            node: dev.node,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let svc = service().ok_or(AttachError::Decline)?;
        let ops = svc.ops();
        let node = self.node;
        let cpus = svc.run(cx.sleepable(), move |i, h| {
            crate::cppc::domain_cpus(i, h, ops, node)
        });
        // The domain's first processor takes its frequency driver.
        if cpus.first().is_some_and(|&(c, _)| c == self.cpu) {
            let child = CppcChild {
                cpu_device: cx.node(),
                cpus,
            };
            cx.add_child(
                Ident::Child {
                    class: "acpi_cppc",
                    desc: "ACPI CPPC CPU frequency control",
                },
                Some(Arc::new(child)),
                Vec::new(),
            );
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const CPU: DriverDecl = DriverDecl::of::<AcpiCpu>();
