// SPDX-License-Identifier: BSD-2-Clause
//! `panthor`: the Arm Mali GPU with the Command Stream Frontend on the CIX Sky1 (`CIXH5000`, an
//! Immortalis-G720), as the board's FreeBSD names it (`panthor0: <Arm Mali GPU (CIX Sky1)>`).
//!
//! Brought up in stages (docs/GRAPHICS.md §17). Attach now runs the Sky1's power-up sequence
//! (panthor specification 11 §11.5; Sky1 SoC specification 02 §2.4.1): the power-domain vote
//! (domain 21, `sky1_pd`), the clocks, the `gpu_reset` pulse, the RCSU's Q-channel clock
//! gating; then identifies the GPU (03 §3.1) with the RCSU's core fuses applied (11 §11.6), and
//! soft-resets it (03 §3.3). The L2, the MMU and the firmware are the next stages.
//!
//! Each step is logged before it runs, so a boot that stops names the step.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use bus::provider::{ClockHandle, Micros, PerfLevel, PowerVote, ProviderError, ResetHandle};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, ResourceError, StartCx,
    StopCx, StopReason,
};
use devtree::ids::AcpiId;
use panthor::id::Info;
use panthor::{Delay, Regs, control, id, sky1};
use regs::Bytes;

/// The firmware's clock names (the DSDT's `CLKT`): the core clock is required; the others are
/// enabled with it when present (11 §11.4.1).
const CORE_CLOCK: &str = "gpu_clk_core";
const OTHER_CLOCKS: [&str; 3] = ["gpu_clk_stacks", "gpu_clk_200M", "gpu_clk_400M"];
/// The reset line the driver pulses (`RSTL`; `gpu_rcsu_reset` is never toggled, 11 §11.4.2).
const RESET: &str = "gpu_reset";
/// The GPU window must hold at least the MMU's address-space slots (02 §2.2).
const GPU_WINDOW_MIN: usize = 0x2800;

fn provider(e: ProviderError) -> AttachError {
    AttachError::from(ResourceError::Provider(e))
}

/// The GPU's register window.
struct Window(MmioGrant<Bytes>);

impl Regs for Window {
    fn read32(&self, off: usize) -> u32 {
        self.0.read32(off)
    }
    fn write32(&self, off: usize, v: u32) {
        self.0.write32(off, v);
    }
}

/// Busy-waits (attach may spin: these are microseconds).
struct Spin;

impl Delay for Spin {
    fn delay_us(&mut self, us: u32) {
        bus::manager().delay_us(u64::from(us));
    }
}

/// A `panthorN` instance.
pub struct Panthor {
    _domain: PowerVote,
    _clocks: Vec<ClockHandle>,
    _reset: Option<ResetHandle>,
    _rcsu: MmioGrant<Bytes>,
    _gpu: Window,
    info: Info,
}

impl core::fmt::Debug for Panthor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Panthor")
            .field("gpu_id", &self.info.gpu_id)
            .finish_non_exhaustive()
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
        // 1. Power, before any register of the subsystem.
        let domain = cx.power_domain(Some("gpu"))?;
        domain.set(s, PerfLevel::ON).map_err(provider)?;
        // 2. Clocks.
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
        log(format_args!(
            "power domain on, {} clocks on, clock rate = {rate}",
            clocks.len()
        ));
        // Resource 0 is the RCSU, resource 1 the GPU (11 §11.4). The RCSU is shared with
        // firmware: `GPUP`'s power resource reads and writes `PGCTRL` and runs the memory repair
        // at the window's start, only when it changes power state.
        let rcsu = cx.mmio_bytes_shared(0)?;
        if rcsu.len() < 0x308 {
            return Err(AttachError::Device("RCSU window too small"));
        }
        let gpu = cx.mmio_bytes(1)?;
        if gpu.len() < GPU_WINDOW_MIN {
            return Err(AttachError::Device("GPU window too small"));
        }
        // 3. The reset pulse (optional under ACPI, 11 §11.4.2), 20 microseconds.
        let mut reset = match cx.reset(Some(RESET)) {
            Ok(r) => Some(r),
            Err(ResourceError::NoSuchLine) => None,
            Err(e) => return Err(e.into()),
        };
        log(format_args!("pulsing {RESET}"));
        if let Some(r) = &mut reset {
            r.pulse(s, Micros(20)).map_err(provider)?;
        }
        // 4. Q-channel clock gating on.
        let pgctrl = rcsu.read32(sky1::RCSU_PGCTRL);
        rcsu.write32(sky1::RCSU_PGCTRL, pgctrl | 1);
        let fuses = rcsu.read32(sky1::RCSU_FUSES);
        log(format_args!(
            "RCSU PGCTRL {pgctrl:#x} -> {:#x}, fuses {fuses:#x}; reading GPU_ID",
            rcsu.read32(sky1::RCSU_PGCTRL)
        ));
        // 5. Identify.
        let gpu = Window(gpu);
        let mut info = id::probe(&gpu).map_err(|e| {
            log(format_args!("identification failed: {e:?}"));
            AttachError::Device("GPU identification failed")
        })?;
        info.shader_present = sky1::harvested(info.shader_present, fuses);
        let g = info.gpu_id;
        log(format_args!(
            "{} id {:#x} major {:#x} minor {:#x} status {:#x}",
            info.product(),
            g.0 >> 16,
            g.version_major(),
            g.version_minor(),
            g.version_status()
        ));
        log(format_args!(
            "Features: L2:{:#x} Tiler:{:#x} Mem:{:#x} MMU:{:#x} AS:{:#x}",
            info.l2_features,
            info.tiler_features,
            info.mem_features,
            info.mmu_features,
            info.as_present
        ));
        log(format_args!(
            "shader_present={:#x} l2_present={:#x} tiler_present={:#x}",
            info.shader_present, info.l2_present, info.tiler_present
        ));
        // 6. Soft reset, every interrupt block masked after it.
        log(format_args!("soft reset"));
        control::soft_reset(&gpu, &mut Spin).map_err(|e| {
            log(format_args!("soft reset failed: {e:?}"));
            AttachError::Device("GPU soft reset failed")
        })?;
        log(format_args!(
            "reset done, GPU_STATUS {:#x}",
            gpu.read32(panthor::regs::GPU_STATUS)
        ));
        Ok(Self {
            _domain: domain,
            _clocks: clocks,
            _reset: reset,
            _rcsu: rcsu,
            _gpu: gpu,
            info,
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
