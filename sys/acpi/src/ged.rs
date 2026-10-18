// SPDX-License-Identifier: BSD-2-Clause
//! `acpi_ged`: the Generic Event Device (ACPI0013), how hardware-reduced
//! platforms (arm64) signal ACPI events: each interrupt in its `_CRS`
//! runs `_EVT` with the interrupt's number, which typically `Notify`s a
//! device (the power button on QEMU's `virt`).
//!
//! `_EVT` is AML, so the handlers are threaded only: a level-triggered
//! line stays masked until the method has run.
//!
//! Behavior reference: sys/dev/acpica/acpi_ged.c.

use alloc::string::String;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, Resource, StartCx, StopCx,
    StopReason,
};
use devtree::ids::AcpiId;
use sched::Sleepable;

use crate::bus::AcpiDevice;
use crate::service::service;

/// `acpi_gedN`.
#[derive(Debug)]
pub struct AcpiGed {
    node: aml::NodeId,
    /// Each interrupt's number, for `_EVT`.
    events: Vec<u32>,
    lines: sync::SpinLock<Vec<IrqLine>>,
}

fn event(d: &AcpiGed, i: usize, s: &Sleepable) {
    let (Some(svc), Some(&n)) = (service(), d.events.get(i)) else {
        return;
    };
    let node = d.node;
    svc.run(s, move |interp, host| {
        let _ = interp.evaluate_child(
            node,
            "_EVT",
            alloc::vec![aml::Value::Integer(u64::from(n))],
            host,
        );
    });
}

impl Driver for AcpiGed {
    const NAME: &'static str = "acpi_ged";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("ACPI0013")]);

    fn describe(_: &Ident) -> String {
        "Generic Event Device".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        service().ok_or(AttachError::Decline)?;
        let node = cx
            .bus_data::<AcpiDevice>()
            .ok_or(AttachError::Decline)?
            .node;
        let events: Vec<u32> = cx
            .resources()
            .iter()
            .filter_map(|r| match r {
                Resource::Irq { hw, .. } => Some(*hw),
                _ => None,
            })
            .collect();
        let mut lines = Vec::new();
        for i in 0..events.len() {
            lines.push(cx.irq(i)?);
        }
        Ok(Self {
            node,
            events,
            lines: sync::SpinLock::new(lines),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let lines = core::mem::take(&mut *self.lines.lock());
        for (i, line) in lines.into_iter().enumerate() {
            cx.interrupt(line, None, Some(event), i)?;
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const GED: DriverDecl = DriverDecl::of::<AcpiGed>();
