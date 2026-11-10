// SPDX-License-Identifier: BSD-2-Clause
//! The CIX Sky1's clocks: the SCMI clock IDs the platform's `CLKT` tables
//! give nearly every SoC device (`platform/cix-sky1`), and the two ACPI
//! devices around them.
//!
//! - `sky1_scmi` (`\_SB.SCMI`, `CIXHA006`) is the OS's SCMI agent to the
//!   power-management processor ([`agent`]), over the mailbox channels its
//!   `_DSD` names. It serves the clock lines: `CLKT` IDs are SCMI clock
//!   IDs (the clean-room `sky1-soc` specification, §1.2.4, §1.4).
//! - `scmi_shmem` (`CIXHA004`, `CIXHA005`) binds the agent's shared
//!   memory, as FreeBSD's `scmi_shmem` does.
//! - `sky1_clk` (`\_SB.CCLK`, `CIXHA010`) stands for "the ACPI clock
//!   provider" so devices can name it in `_DEP`; it is bound and serves
//!   nothing. Its AML methods (`GCLK`, `SCLK`, `CLKD`, `CLKC`) send SCMI
//!   clock messages through `\_SB.PMMX` on another agent's channel, which
//!   answers `NOT_FOUND` for every clock (seen on the board, and in the
//!   AbyssBSD Project's FreeBSD port); [`invoke`] and the tests keep them
//!   as a description of the messages.
//!
//! | Operation | SCMI message (protocol `0x14`) | AML method |
//! |---|---|---|
//! | rate | `CLOCK_RATE_GET` | `GCLK(id)` |
//! | set rate | `CLOCK_RATE_SET`, rounding down | `SCLK(id, rate low, rate high)` |
//! | rates | `CLOCK_DESCRIBE_RATES` | `CLKD(id, index)` |
//! | enable, disable | `CLOCK_CONFIG_SET` (bit 0 enables) | `CLKC(id, attributes)` |
//!
//! Firmware leaves the clocks a booted system needs running, and attach
//! changes nothing.
//!
//! FreeBSD has neither driver; the names are those of the AbyssBSD
//! Project's FreeBSD port for the board (docs/compat-deviations.md).

#![no_std]
// The tests make a `Sleepable` for the host; the crate itself has no
// unsafe code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::provider::{Clock, Hz, ProviderError};
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::AcpiId;
use sched::Sleepable;

pub mod agent;
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

/// `sky1_clkN`: bound so `_DEP`s on the clock provider are met.
#[derive(Debug)]
pub struct Sky1Clk;

impl Driver for Sky1Clk {
    const NAME: &'static str = "sky1_clk";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXHA010")]);

    fn describe(_: &Ident) -> String {
        "CIX Sky1 ACPI clock provider".into()
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// `sky1_scmiN`: the OS's SCMI agent, serving the clock lines.
#[derive(Debug)]
pub struct Sky1Scmi {
    clocks: Arc<FirmwareClocks<agent::Agent>>,
}

impl Driver for Sky1Scmi {
    const NAME: &'static str = "sky1_scmi";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXHA006")]);

    fn describe(_: &Ident) -> String {
        "CIX Sky1 SCMI".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        // `mboxes`: the sending channel first, then the receiving one
        // (unused: replies are polled for).
        let tx = cx.mailbox(0)?;
        Ok(Self {
            clocks: Arc::new(FirmwareClocks::new(agent::Agent::new(tx))),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.provide_clocks(Arc::clone(&self.clocks) as Arc<dyn Clock>);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// `scmi_shmemN`: the SCMI agent's shared memory (`\_SB.SHM0`, `SHM1`;
/// `_DSD` `compatible` `arm,scmi-shmem`), FreeBSD's `scmi_shmem`. Bound so
/// the agent's `shmem` references are met; it maps nothing, because the
/// memory is the mailbox's message registers, which `cix_mbox` serves.
#[derive(Debug)]
pub struct ScmiShmem;

impl Driver for ScmiShmem {
    const NAME: &'static str = "scmi_shmem";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXHA004"), AcpiId::new("CIXHA005")]);

    fn describe(_: &Ident) -> String {
        "ARM SCMI Shared Memory driver".into()
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declarations for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Sky1Clk>();
/// The SCMI agent's.
pub const SCMI_DRIVER: DriverDecl = DriverDecl::of::<Sky1Scmi>();
/// Its shared memory's.
pub const SHMEM_DRIVER: DriverDecl = DriverDecl::of::<ScmiShmem>();
