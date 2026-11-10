// SPDX-License-Identifier: BSD-2-Clause
//! `sky1_clk`: the CIX Sky1's clocks (`\_SB.CCLK`, `CIXHA010`), the
//! provider nearly every SoC device's clock lines name (the platform's
//! `CLKT` tables, `platform/cix-sky1`).
//!
//! The device is AML only. Its methods speak the SCMI clock protocol to
//! the power-management firmware themselves, through `\_SB.PMMX`'s
//! `SystemMemory` mailbox, serialized by the AML mutex `MBXM`:
//!
//! | Method | SCMI message |
//! |---|---|
//! | `GCLK(id)` | `CLOCK_RATE_GET` |
//! | `SCLK(id, rate low, rate high)` | `CLOCK_RATE_SET`, rounding down |
//! | `CLKD(id, index)` | `CLOCK_DESCRIBE_RATES` |
//! | `CLKC(id, attributes)` | `CLOCK_CONFIG_SET` (bit 0 enables) |
//!
//! Each returns the channel's reply payload ([`scmi`]). So the driver
//! needs no registers and no mailbox of its own: it publishes a
//! [`Clock`] provider whose operations evaluate these methods on the ACPI
//! service thread. Firmware leaves the clocks a booted system needs
//! running, and attach changes nothing.
//!
//! FreeBSD has no driver for it (FreeBSD's consumers rely on firmware's
//! clock state); the name is recorded in docs/compat-deviations.md.

#![no_std]
// The tests make a `Sleepable` for the host; the crate itself has no
// unsafe code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use acpi::AcpiDevice;
use bus::provider::{Clock, Hz, ProviderError};
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::AcpiId;
use sched::Sleepable;

pub mod scmi;
#[cfg(test)]
#[allow(unsafe_code)]
mod tests;

/// The methods a clock operation evaluates.
pub const METHODS: [&str; 4] = ["GCLK", "SCLK", "CLKD", "CLKC"];

/// Describing a clock's rates stops after this many replies (eleven rates
/// each), whatever firmware says remains.
const MAX_DESCRIBE: u32 = 64;

/// Evaluates one of the device's clock methods with integer arguments.
pub trait Methods: Send + Sync {
    /// Runs `method` and returns its reply.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Unsupported`] if the device has no such method,
    /// [`ProviderError::Firmware`] if evaluation failed.
    fn call(
        &self,
        s: &Sleepable,
        method: &'static str,
        args: &[u64],
    ) -> Result<Vec<u8>, ProviderError>;
}

/// Evaluates `method` on `node` with integer `args`; its reply buffer.
///
/// # Errors
///
/// As [`Methods::call`].
pub fn invoke(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    node: aml::NodeId,
    method: &str,
    args: &[u64],
) -> Result<Vec<u8>, ProviderError> {
    let args = args.iter().map(|&a| aml::Value::Integer(a)).collect();
    match interp.evaluate_child(node, method, args, host) {
        Ok(Some(aml::Value::Buffer(b))) => Ok(b),
        Ok(None) => Err(ProviderError::Unsupported),
        Ok(Some(_)) | Err(_) => Err(ProviderError::Firmware(0)),
    }
}

/// The device's methods, through the ACPI service.
#[derive(Debug)]
struct AcpiMethods {
    node: aml::NodeId,
}

impl Methods for AcpiMethods {
    fn call(
        &self,
        s: &Sleepable,
        method: &'static str,
        args: &[u64],
    ) -> Result<Vec<u8>, ProviderError> {
        let svc = acpi::service().ok_or(ProviderError::Revoked)?;
        let (node, args) = (self.node, args.to_vec());
        svc.run(s, move |i, h| invoke(i, h, node, method, &args))
    }
}

/// Firmware clocks: [`Clock`] over the device's methods.
#[derive(Debug)]
pub struct FirmwareClocks<M> {
    methods: M,
}

impl<M: Methods> FirmwareClocks<M> {
    /// Clocks served by `methods`.
    pub fn new(methods: M) -> Self {
        Self { methods }
    }

    /// Every rate clock `id` runs at, asking until firmware says none
    /// remain.
    fn rates(&self, s: &Sleepable, id: u32) -> Result<scmi::Rates, ProviderError> {
        let mut all = Vec::new();
        for _ in 0..MAX_DESCRIBE {
            let index = u64::try_from(all.len()).unwrap_or(u64::MAX);
            let reply = self.methods.call(s, "CLKD", &[id.into(), index])?;
            let d = scmi::described(&reply)?;
            match d.rates {
                range @ scmi::Rates::Range { .. } => return Ok(range),
                scmi::Rates::Discrete(mut some) => {
                    let none = some.is_empty();
                    all.append(&mut some);
                    if d.remaining == 0 || none {
                        break;
                    }
                }
            }
        }
        all.sort_unstable();
        Ok(scmi::Rates::Discrete(all))
    }
}

impl<M: Methods> Clock for FirmwareClocks<M> {
    fn enable(&self, s: &Sleepable, id: u32) -> Result<(), ProviderError> {
        scmi::payload(&self.methods.call(s, "CLKC", &[id.into(), 1])?).map(drop)
    }

    fn disable(&self, s: &Sleepable, id: u32) -> Result<(), ProviderError> {
        scmi::payload(&self.methods.call(s, "CLKC", &[id.into(), 0])?).map(drop)
    }

    fn rate(&self, s: &Sleepable, id: u32) -> Result<Hz, ProviderError> {
        scmi::rate(&self.methods.call(s, "GCLK", &[id.into()])?)
    }

    fn round_rate(&self, s: &Sleepable, id: u32, want: Hz) -> Result<Hz, ProviderError> {
        scmi::round_down(&self.rates(s, id)?, want.0).map(Hz)
    }

    fn set_rate(&self, s: &Sleepable, id: u32, want: Hz) -> Result<(), ProviderError> {
        let (lo, hi) = (want.0 & 0xffff_ffff, want.0 >> 32);
        scmi::payload(&self.methods.call(s, "SCLK", &[id.into(), lo, hi])?).map(drop)
    }
}

/// `sky1_clkN`.
#[derive(Debug)]
pub struct Sky1Clk {
    node: aml::NodeId,
}

impl Driver for Sky1Clk {
    const NAME: &'static str = "sky1_clk";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXHA010")]);

    fn describe(_: &Ident) -> String {
        "CIX Sky1 firmware clocks".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let svc = acpi::service().ok_or(AttachError::Decline)?;
        let node = cx
            .bus_data::<AcpiDevice>()
            .ok_or(AttachError::Decline)?
            .node;
        let complete = svc.run(cx.sleepable(), move |i, _| {
            METHODS.iter().all(|m| i.child(node, m).is_some())
        });
        if !complete {
            return Err(AttachError::Device("clock methods missing"));
        }
        Ok(Self { node })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.provide_clocks(Arc::new(FirmwareClocks::new(AcpiMethods {
            node: self.node,
        })));
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Sky1Clk>();
