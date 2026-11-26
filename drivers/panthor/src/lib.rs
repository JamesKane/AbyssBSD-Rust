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

mod dri;
mod mcu;
mod mem;
mod render;
mod uapi;
mod work;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use abi::errno::Errno;
use bus::provider::{ClockHandle, Micros, PerfLevel, PowerVote, ProviderError, ResetHandle};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, ResourceError,
    StartCx, StopCx, StopReason,
};
use devtree::ids::AcpiId;
use irq::Filter;
use panthor::id::Info;
use panthor::{Delay, Regs, control, id, sky1};
use regs::Bytes;
use sched::IrqCx;
use sched::lock::{LockClass, Mutex};
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

/// What the attached GPU, its thread, its interrupts and its card share.
pub(crate) struct Dev {
    unit: u32,
    pub(crate) regs: Window,
    pub(crate) tag: DmaTag,
    pub(crate) info: Info,
    core_hz: u64,
    /// The system counter's rate, which the GPU's timestamp counts at (measured at attach).
    pub(crate) timer_hz: u64,
    /// A boot asked for, not yet run; files closed, not yet cleaned up; the thread's exit.
    requested: AtomicBool,
    closed: SpinLock<Vec<u64>>,
    exit: AtomicBool,
    queue: WaitQueue,
    /// The firmware and the GPU's work.
    pub(crate) core: Mutex<work::Core>,
    /// Why the firmware did not boot, once it failed; what `dev.panthor.N.mcu` says.
    boot_failed: SpinLock<Option<&'static str>>,
    status: SpinLock<String>,
    /// Woken when jobs may have finished; and the waiters that asked to be told.
    pub(crate) done: WaitQueue,
    pub(crate) wakers: SpinLock<Vec<Arc<dyn vfs::event::Wake>>>,
    /// The MMU fault bits the interrupt thread unmasks when it is done.
    pub(crate) mmu_mask: AtomicU32,
    /// The page userspace maps for the flush ID.
    pub(crate) flush_page: Once<Arc<vm::object::Object>>,
}

static CORE_LOCK: LockClass = LockClass::new("panthor core", 410);

/// The GPU-control events the interrupt takes (03 §3.8.4, without the cache-flush completion,
/// which the flushes poll for).
const GPU_EVENTS: u32 = panthor::regs::INT_FAULT | panthor::regs::INT_PROTM_FAULT;

impl Dev {
    pub(crate) fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("panthor{}: {args}", self.unit));
    }

    fn status(&self) -> String {
        if let Some(why) = *self.boot_failed.lock() {
            return alloc::format!("MCU boot failed: {why}");
        }
        self.status.lock().clone()
    }

    /// Boots the firmware if it is not running yet (on the caller's thread: the image is read
    /// from the root file system).
    pub(crate) fn ensure_booted(&self, s: &Sleepable) -> Result<(), Errno> {
        if self.boot_failed.lock().is_some() {
            return Err(Errno::ENODEV);
        }
        let mut core = self.core.lock(s);
        if core.fw.is_some() {
            return Ok(());
        }
        match mcu::boot(s, &self.regs, &self.tag, &self.info, self.core_hz, &|a| {
            self.log(a)
        }) {
            Ok(b) => {
                *self.status.lock() = alloc::format!(
                    "MCU running, interface {}, {} groups, firmware {}",
                    b.iface.version,
                    b.iface.groups,
                    b.git_sha
                );
                core.fw = Some(b);
                // The firmware's events from here on come through the JOB interrupt.
                control::unmask_block(&self.regs, panthor::regs::JOB_INT, u32::MAX);
                Ok(())
            }
            Err(why) => {
                self.log(format_args!("MCU boot failed: {why}"));
                *self.boot_failed.lock() = Some(why);
                Err(Errno::ENODEV)
            }
        }
    }

    /// A file closed: its objects go on the driver's thread.
    pub(crate) fn close_file(&self, file: u64) {
        self.closed.lock().push(file);
        self.queue.wake_all();
    }

    /// The firmware's liveness (05 §5.8): a ping, answered within 100 ms.
    fn ping(&self, s: &Sleepable) {
        let core = self.core.lock(s);
        let Some(fw) = core.fw.as_ref() else {
            return;
        };
        let sh = mcu::SharedMem(&fw.sections[fw.shared]);
        panthor::csf::ping(&sh, &fw.iface);
        mcu::ring(&self.regs);
        for _ in 0..10_000 {
            if panthor::csf::acked(&sh, &fw.iface, panthor::csf::REQ_PING) {
                return;
            }
            bus::manager().delay_us(10);
        }
        self.log(format_args!(
            "the firmware did not answer a ping (MCU_STATUS {})",
            self.regs.read32(panthor::regs::MCU_STATUS)
        ));
    }
}

/// The driver's thread: a requested boot, closed files, and the ping every 12 seconds.
fn worker(d: &Dev, s: &Sleepable) {
    let mut last_ping = sched::now_ns();
    loop {
        let mut stop = || {
            d.requested.load(Ordering::Acquire)
                || d.exit.load(Ordering::Acquire)
                || !d.closed.lock().is_empty()
        };
        bus::manager().wait_timeout(s, &d.queue, &mut stop, 12_000_000_000);
        if d.exit.load(Ordering::Acquire) {
            return;
        }
        if d.requested.swap(false, Ordering::AcqRel) {
            let _ = d.ensure_booted(s);
        }
        let closed: Vec<u64> = core::mem::take(&mut *d.closed.lock());
        if !closed.is_empty() {
            let mut core = d.core.lock(s);
            for f in closed {
                d.file_closed(&mut core, f);
            }
        }
        if sched::now_ns() - last_ping >= 12_000_000_000 {
            d.ping(s);
            last_ping = sched::now_ns();
        }
    }
}

/// The interrupt blocks, by line: `_CRS` interrupt 0 is JOB, 1 MMU, 2 GPU (11 §11.4).
const BLOCKS: [usize; 3] = [
    panthor::regs::JOB_INT,
    panthor::regs::MMU_INT,
    panthor::regs::GPU_INT,
];

/// The filter (03 §3.8.2): not ours if the block has nothing pending; else the block is masked
/// so the level line drops, and the thread runs.
fn irq_filter(p: &Panthor, line: usize, _: &IrqCx) -> Filter {
    let block = BLOCKS[line];
    if p.dev.regs.read32(block + panthor::regs::INT_STAT) == 0 {
        return Filter::Stray;
    }
    p.dev.regs.write32(block + panthor::regs::INT_MASK, 0);
    Filter::Schedule
}

/// The thread: each pending event is cleared before it is handled, then the block unmasked.
fn irq_thread(p: &Panthor, line: usize, s: &Sleepable) {
    let d = &p.dev;
    let block = BLOCKS[line];
    let enabled = || match line {
        0 => u32::MAX,
        1 => d.mmu_mask.load(Ordering::Acquire),
        _ => GPU_EVENTS,
    };
    loop {
        let bits = d.regs.read32(block + panthor::regs::INT_RAWSTAT) & enabled();
        if bits == 0 {
            break;
        }
        d.regs.write32(block + panthor::regs::INT_CLEAR, bits);
        if line == 1 {
            // The faulting slots are stopped first, without the core lock: a request holding it
            // may be waiting for the firmware, which the fault keeps from answering (04 §4.9.1).
            d.mmu_mask.fetch_and(!(bits & 0xFFFF), Ordering::AcqRel);
            d.stop_faulted(bits);
        }
        let mut core = d.core.lock(s);
        match line {
            0 => d.job_events(&mut core, bits),
            1 => d.mmu_events(&mut core, bits),
            _ => d.log(format_args!(
                "GPU fault: status {:#x} at {:#x}",
                d.regs.read32(panthor::regs::GPU_FAULT_STATUS),
                d.regs.read64(panthor::regs::GPU_FAULT_ADDR)
            )),
        }
    }
    d.regs.write32(block + panthor::regs::INT_MASK, enabled());
}

/// A `panthorN` instance.
pub struct Panthor {
    dev: Arc<Dev>,
    thread: SpinLock<Option<Arc<Thread>>>,
    lines: SpinLock<Vec<IrqLine>>,
    card: SpinLock<Option<Arc<drm::card::Card>>>,
    _domain: PowerVote,
    _clocks: Vec<ClockHandle>,
    _reset: Option<ResetHandle>,
    _rcsu: MmioGrant<Bytes>,
}

impl core::fmt::Debug for Panthor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Panthor")
            .field("gpu_id", &self.dev.info.gpu_id)
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
        // The interrupts, by position: JOB, MMU, GPU (11 §11.4).
        let mut lines = Vec::new();
        for k in 0..3 {
            lines.push(cx.irq(k)?);
        }
        // The system counter's rate: the GPU timestamp's ticks over 10 ms.
        let (t0, n0) = (gpu.read_counter(panthor::regs::TIMESTAMP), sched::now_ns());
        bus::manager().delay_us(10_000);
        let (t1, n1) = (gpu.read_counter(panthor::regs::TIMESTAMP), sched::now_ns());
        let hz = (t1.wrapping_sub(t0)).saturating_mul(1_000_000_000) / (n1 - n0).max(1);
        // To the nearest MHz: the counter runs at a round rate.
        let timer_hz = (hz + 500_000) / 1_000_000 * 1_000_000;
        log(format_args!("timestamp counter at {timer_hz} Hz"));
        Ok(Self {
            dev: Arc::new(Dev {
                unit,
                regs: gpu,
                tag,
                info,
                core_hz: rate,
                timer_hz,
                requested: AtomicBool::new(false),
                closed: SpinLock::new(Vec::new()),
                exit: AtomicBool::new(false),
                queue: WaitQueue::new(),
                core: Mutex::new(&CORE_LOCK, work::Core::new(info.as_present)),
                boot_failed: SpinLock::new(None),
                status: SpinLock::new("MCU not started".into()),
                done: WaitQueue::new(),
                wakers: SpinLock::new(Vec::new()),
                mmu_mask: AtomicU32::new(1),
                flush_page: Once::new(),
            }),
            thread: SpinLock::new(None),
            lines: SpinLock::new(lines),
            card: SpinLock::new(None),
            _domain: domain,
            _clocks: clocks,
            _reset: reset,
            _rcsu: rcsu,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let d = Arc::clone(&self.dev);
        let th = sched::Builder::new("panthor")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| worker(&d, s))
            .map_err(|_| AttachError::NoMemory)?;
        *self.thread.lock() = Some(th);
        // Every block masked until there is something to take (03 §3.8.3).
        for b in BLOCKS {
            control::mask_block(&self.dev.regs, b);
        }
        let lines: Vec<IrqLine> = core::mem::take(&mut *self.lines.lock());
        for (k, line) in lines.into_iter().enumerate() {
            cx.interrupt(line, Some(irq_filter), Some(irq_thread), k)?;
        }
        control::unmask_block(&self.dev.regs, panthor::regs::GPU_INT, GPU_EVENTS);
        // The render node.
        match drm::register(Arc::new(render::Render(Arc::clone(&self.dev)))) {
            Ok(card) => {
                dri::publish(cx.sleepable(), card.unit(), true, "platform:panthor", false);
                *self.card.lock() = Some(card);
            }
            Err(e) => self.dev.log(format_args!("no render node: {e:?}")),
        }
        let unit = self.dev.unit;
        let (boot, report) = (Arc::clone(&self.dev), Arc::clone(&self.dev));
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
                sysctl::Value::Str(Box::new(move |out| out.push_str(&r.status()))),
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

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        // The render node first: no new requests.
        if let Some(card) = self.card.lock().take() {
            card.unregister(s);
        }
        // The thread: a boot or a cleanup in progress finishes before anything it uses goes.
        self.dev.exit.store(true, Ordering::Release);
        self.dev.queue.wake_all();
        let t = self.thread.lock().take();
        if let Some(t) = t {
            t.join(s);
        }
        // Then a running MCU is stopped and its memory freed (05 §5.7.4), before the clocks,
        // the domain vote and the register grants are released with the instance.
        let booted = self.dev.core.lock(s).fw.take();
        if let Some(b) = booted {
            b.shut_down(&self.dev.regs, &|a| self.dev.log(a));
        }
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
