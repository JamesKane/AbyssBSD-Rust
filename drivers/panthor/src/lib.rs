// SPDX-License-Identifier: BSD-2-Clause
//! `panthor`: the Arm Mali GPU with the Command Stream Frontend on the CIX Sky1 (`CIXH5000`, an
//! Immortalis-G720), as the board's FreeBSD names it (`panthor0: <Arm Mali GPU (CIX Sky1)>`).
//!
//! Brought up in stages (docs/GRAPHICS.md §17). Attach runs the Sky1's power-up sequence
//! (panthor specification 11 §11.5; Sky1 SoC specification 02 §2.4.1): the power-domain vote
//! (domain 21, `sky1_pd`), the clocks, the `gpu_reset` pulse, the RCSU's Q-channel clock
//! gating; then identifies the GPU (03 §3.1) with the RCSU's core fuses applied (11 §11.6), and
//! soft-resets it (03 §3.3).
//!
//! The microcontroller's boot ([`mcu`]) runs on the driver's thread when `dev.panthor.N.mcu_boot`
//! is set to 1, never at attach (as `drmn`'s stages: one that stops the machine cannot keep the
//! installed kernel from booting). `dev.panthor.N.mcu` reports where it is.
//!
//! Each step is logged before it runs, so a boot that stops names the step.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod mcu;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use abi::errno::Errno;
use bus::provider::{ClockHandle, Micros, PerfLevel, PowerVote, ProviderError, ResetHandle};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, ResourceError, StartCx,
    StopCx, StopReason,
};
use devtree::ids::AcpiId;
use panthor::id::Info;
use panthor::{Delay, Regs, control, id, sky1};
use regs::Bytes;
use sched::{Sleepable, Thread, WaitQueue};
use sync::{Once, SpinLock};
use vm::dma::{Coherency, DmaConstraints, DmaTag};

/// The firmware's clock names (the DSDT's `CLKT`): the core clock is required; the others are
/// enabled with it when present (11 §11.4.1).
const CORE_CLOCK: &str = "gpu_clk_core";
const OTHER_CLOCKS: [&str; 3] = ["gpu_clk_stacks", "gpu_clk_200M", "gpu_clk_400M"];
/// The reset line the driver pulses (`RSTL`; `gpu_rcsu_reset` is never toggled, 11 §11.4.2).
const RESET: &str = "gpu_reset";
/// The GPU window must hold the MMU's slots and doorbell 0 (02 §2.2).
const GPU_WINDOW_MIN: usize = 0x9_0000;

/// Reads a firmware file, whole, from the root file system.
pub trait Firmware: Sync {
    fn read(&self, s: &Sleepable, path: &str) -> Result<Vec<u8>, Errno>;
}

static FIRMWARE: Once<&'static dyn Firmware> = Once::new();

/// Gives the driver the kernel's firmware reader, once, before drivers start.
pub fn set_firmware(f: &'static dyn Firmware) {
    let _ = FIRMWARE.set(f);
}

fn firmware() -> Option<&'static dyn Firmware> {
    FIRMWARE.get().copied()
}

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

/// Busy-waits (these are microseconds).
struct Spin;

impl Delay for Spin {
    fn delay_us(&mut self, us: u32) {
        bus::manager().delay_us(u64::from(us));
    }
}

/// What the attached GPU and its thread share.
struct Gpu {
    unit: u32,
    regs: Window,
    tag: DmaTag,
    info: Info,
    core_hz: u64,
    /// A boot asked for, not yet run.
    requested: AtomicBool,
    exit: AtomicBool,
    queue: WaitQueue,
    /// The booted firmware, or why the last boot failed.
    state: SpinLock<Result<Option<mcu::Booted>, &'static str>>,
}

impl Gpu {
    fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("panthor{}: {args}", self.unit));
    }

    fn report(&self) -> String {
        match &*self.state.lock() {
            Ok(None) => "MCU not started".into(),
            Ok(Some(b)) => alloc::format!(
                "MCU running, interface {}, {} groups, firmware {}",
                b.iface.version,
                b.iface.groups,
                b.git_sha
            ),
            Err(why) => alloc::format!("MCU boot failed: {why}"),
        }
    }
}

/// The driver's thread: runs a requested boot.
fn worker(g: &Gpu, s: &Sleepable) {
    loop {
        g.queue.wait_until(s, || {
            g.requested.load(Ordering::Acquire) || g.exit.load(Ordering::Acquire)
        });
        if g.exit.load(Ordering::Acquire) {
            return;
        }
        g.requested.store(false, Ordering::Release);
        if !matches!(*g.state.lock(), Ok(None)) {
            g.log(format_args!(
                "the MCU was already started (or failed): reboot to try again"
            ));
            continue;
        }
        let r = mcu::boot(s, &g.regs, &g.tag, &g.info, g.core_hz, &|a| g.log(a));
        if let Err(why) = &r {
            g.log(format_args!("MCU boot failed: {why}"));
        }
        *g.state.lock() = r.map(Some);
    }
}

/// A `panthorN` instance.
pub struct Panthor {
    gpu: Arc<Gpu>,
    thread: SpinLock<Option<Arc<Thread>>>,
    _domain: PowerVote,
    _clocks: Vec<ClockHandle>,
    _reset: Option<ResetHandle>,
    _rcsu: MmioGrant<Bytes>,
}

impl core::fmt::Debug for Panthor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Panthor")
            .field("gpu_id", &self.gpu.info.gpu_id)
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
        // Memory the GPU walks and runs from: physical addresses within its reach (no IOMMU,
        // 04 §4.6.3), and non-coherent for the CPU whatever `_CCA` says (11 §11.7).
        let tag = cx.dma_tag(DmaConstraints {
            addr_bits: info.pa_bits(),
            align: 0x1000,
            coherency: Coherency::NonCoherent,
            ..DmaConstraints::UNLIMITED
        })?;
        Ok(Self {
            gpu: Arc::new(Gpu {
                unit,
                regs: gpu,
                tag,
                info,
                core_hz: rate,
                requested: AtomicBool::new(false),
                exit: AtomicBool::new(false),
                queue: WaitQueue::new(),
                state: SpinLock::new(Ok(None)),
            }),
            thread: SpinLock::new(None),
            _domain: domain,
            _clocks: clocks,
            _reset: reset,
            _rcsu: rcsu,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let g = Arc::clone(&self.gpu);
        let t = sched::Builder::new("panthor")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| worker(&g, s))
            .map_err(|_| AttachError::NoMemory)?;
        *self.thread.lock() = Some(t);
        let unit = self.gpu.unit;
        let (boot, report) = (Arc::clone(&self.gpu), Arc::clone(&self.gpu));
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            let Some(node) = bus::device_node(t, Self::NAME, unit) else {
                return;
            };
            let r = Arc::clone(&report);
            let _ = t.add_value(
                node,
                sysctl::Number::Auto,
                "mcu",
                "Where the GPU's microcontroller is",
                sysctl::Value::Str(Box::new(move |out| out.push_str(&r.report()))),
            );
            let b = Arc::clone(&boot);
            let _ = t.add_writable(
                node,
                sysctl::Number::Auto,
                "mcu_boot",
                "Boot the GPU's microcontroller (write 1)",
                0,
                sysctl::Value::Int(Box::new(|| 0)),
                sysctl::int_writer(move |v| {
                    if v == 1 {
                        b.requested.store(true, Ordering::Release);
                        b.queue.wake_all();
                    }
                    Ok(())
                }),
            );
        });
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        self.gpu.exit.store(true, Ordering::Release);
        self.gpu.queue.wake_all();
        // The MCU is stopped and the interrupt blocks masked (05 §5.7.2).
        self.gpu
            .regs
            .write32(panthor::regs::JOB_INT + panthor::regs::INT_MASK, 0);
        self.gpu.regs.write32(panthor::regs::MCU_CONTROL, 0);
    }
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
