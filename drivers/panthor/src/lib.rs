// SPDX-License-Identifier: BSD-2-Clause
//! `panthor`: the Arm Mali GPU with the Command Stream Frontend on the CIX Sky1 (`CIXH5000`, an
//! Immortalis-G720), as the board's FreeBSD names it (`panthor0: <Arm Mali GPU (CIX Sky1)>`).
//!
//! Brought up in stages (docs/GRAPHICS.md §17). This one powers the GPU's side and reads the
//! RCSU only: the power-domain vote (domain 21, `sky1_pd`), the clocks, and the RCSU's clock
//! gating and core-fuse words (panthor specification 11 §11.5, §11.6). The reset pulse and the
//! first GPU register read are the next stage.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use bus::provider::{ClockHandle, PerfLevel, PowerVote, ProviderError};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, ResourceError, StartCx,
    StopCx, StopReason,
};
use devtree::ids::AcpiId;
use regs::Bytes;

/// The firmware's clock names (the DSDT's `CLKT`): the core clock is required; the others are
/// enabled with it when present (11 §11.4.1).
const CORE_CLOCK: &str = "gpu_clk_core";
const OTHER_CLOCKS: [&str; 3] = ["gpu_clk_stacks", "gpu_clk_200M", "gpu_clk_400M"];

fn provider(e: ProviderError) -> AttachError {
    AttachError::from(ResourceError::Provider(e))
}

/// A `panthorN` instance.
pub struct Panthor {
    _domain: PowerVote,
    _clocks: Vec<ClockHandle>,
    _rcsu: MmioGrant<Bytes>,
}

impl core::fmt::Debug for Panthor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Panthor").finish_non_exhaustive()
    }
}

impl Driver for Panthor {
    const NAME: &'static str = "panthor";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXH5000")]);

    fn describe(_: &Ident) -> String {
        "Arm Mali GPU (CIX Sky1)".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let s = cx.sleepable();
        let unit = cx.unit();
        let log = |args: core::fmt::Arguments<'_>| {
            bus::manager().log(format_args!("panthor{unit}: {args}"));
        };
        // Power before any register of the subsystem (11 §11.5 step 1).
        let domain = cx.power_domain(Some("gpu"))?;
        domain.set(s, PerfLevel::ON).map_err(provider)?;
        log(format_args!("power domain on"));
        let mut clocks = Vec::new();
        let mut core = cx.clock(Some(CORE_CLOCK))?;
        core.enable(s).map_err(provider)?;
        let rate = core.rate(s).map_err(provider)?.0;
        clocks.push(core);
        for name in OTHER_CLOCKS {
            if let Ok(mut c) = cx.clock(Some(name)) {
                c.enable(s).map_err(provider)?;
                clocks.push(c);
            }
        }
        log(format_args!("{} clocks on, core at {rate} Hz", clocks.len()));
        // Resource 0 is the RCSU, resource 1 the GPU (11 §11.4). The RCSU is shared with
        // firmware: `GPUP`'s power resource reads and writes `PGCTRL` and runs the memory repair
        // at the window's start, only when it changes power state.
        let rcsu = cx.mmio_bytes_shared(0)?;
        if rcsu.len() < 0x308 {
            return Err(AttachError::Device("RCSU window too small"));
        }
        let pgctrl = rcsu.read32(panthor::sky1::RCSU_PGCTRL);
        let fuses = rcsu.read32(panthor::sky1::RCSU_FUSES);
        log(format_args!(
            "RCSU PGCTRL {pgctrl:#x}, fuses {fuses:#x} (cores {:#x}); GPU registers not yet read",
            panthor::sky1::harvested(panthor::sky1::FULL_SHADER_PRESENT, fuses)
        ));
        Ok(Self {
            _domain: domain,
            _clocks: clocks,
            _rcsu: rcsu,
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Panthor>();

/// `sky1_gpupwrN`: the GPU's power resource holder (`CIXH5001`, `\_SB.GPUP`), which the GPU's
/// `_DSD` names as its `power-supply`. The device has only a power resource (`_PR0`, whose `_ON`
/// sets the RCSU's power gates and runs the SRAM memory repair); having a driver is what makes
/// the framework bring it to D0 (DRIVER-MODEL.md §8.5), before the GPU, which waits on it.
#[derive(Debug)]
pub struct GpuPower;

impl Driver for GpuPower {
    const NAME: &'static str = "sky1_gpupwr";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXH5001")]);

    fn describe(_: &Ident) -> String {
        "CIX Sky1 GPU power resource".into()
    }

    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The power resource holder's declaration.
pub const POWER_DRIVER: DriverDecl = DriverDecl::of::<GpuPower>();
