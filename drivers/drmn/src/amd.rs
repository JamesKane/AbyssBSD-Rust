// SPDX-License-Identifier: MIT
// Copyright 2018 Advanced Micro Devices, Inc.
// Copyright 2018-2024 Advanced Micro Devices, Inc. All rights reserved.
// Copyright (C) 2018  Advanced Micro Devices, Inc.
// Copyright 2008 Advanced Micro Devices, Inc.
// Copyright 2008 Red Hat Inc.
// Copyright 2009 Jerome Glisse.
// Copyright 2025 Advanced Micro Devices, Inc.
// Copyright (C) 2020  Advanced Micro Devices, Inc.
// Copyright (C) 2022  Advanced Micro Devices, Inc.
// Copyright (c) 2026 The AbyssBSD Project
//! The AMD side of `drmn` (the graphics device node of FreeBSD's `drm-kmod`:
//! see the crate's module comment): what an AMD GPU's registers say at attach (plain
//! reads of two fixed registers), and, on request through sysctls, its IP
//! discovery table and the display state the firmware left.
//!
//! Nothing here writes to the GPU at attach. Reading the discovery table
//! writes the two VRAM index registers (and puts them back), so it is a
//! sysctl trigger, not attach work, and it runs under one lock.
//!
//! Layouts: the clean-room specification `amd-rdna2` (sections 01 and 08).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

mod debug;
mod reset;

use amdgpu::discovery::{Discovery, hw};
use amdgpu::{display, regs};
use sync::SpinLock;
use vgapci::Aperture;

/// The GPU's register window, for `amdgpu`'s sequences.
pub(crate) struct Bar5<'a>(pub(crate) &'a Aperture);

impl regs::Window for Bar5<'_> {
    fn read32(&self, offset: usize) -> u32 {
        self.0.read32(offset).unwrap_or(u32::MAX)
    }
    fn write32(&self, offset: usize, value: u32) {
        self.0.write32(offset, value);
    }
}

/// What attach read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) device: u16,
    pub(crate) revision: u8,
    /// VRAM in MiB, as the memory size register says.
    pub(crate) vram_mib: u32,
    /// Whether the platform security processor has finished its boot work.
    pub(crate) psp_ready: bool,
}

/// An AMD GPU being driven from its sysctls.
pub(crate) struct Amd {
    name: String,
    gpu: Weak<Aperture>,
    /// Held across every index/data sequence and the table cache.
    table: SpinLock<Option<Discovery>>,
    /// Whether the host-data-path flush has been set up.
    hdp_ready: core::sync::atomic::AtomicBool,
    /// The graphics block's start-up on first use of the render node: 0 not
    /// tried, 1 running, 2 done, 3 failed (a reboot clears it).
    gfx_state: core::sync::atomic::AtomicU8,
    /// The unit of the graphics card made for it, or -1.
    dri_unit: core::sync::atomic::AtomicI32,
    /// The graphics card, once registered.
    kms: SpinLock<Option<(Arc<amd_kms::AmdKms>, Arc<drm::Card>)>>,
    /// The interrupts.
    irq: Arc<crate::interrupt::Irq>,
    /// The page the hubs send faults and aperture misses to, while the
    /// hubs are set up.
    dummy: SpinLock<Option<vm::dma::DmaCoherent<u8>>>,
    /// The security processor's ring, once made.
    psp: SpinLock<Option<crate::psp::Brought>>,
    /// Whether the audio side of HDMI is prepared (stage R3.4).
    hdmi_prepared: core::sync::atomic::AtomicBool,
    /// The pixel clock now running, in kHz (the firmware's, until a mode
    /// change that moves it).
    clock_khz: core::sync::atomic::AtomicU32,
    /// The firmware's own values of the registers a mode change writes,
    /// taken before the first change, to put back.
    baseline: SpinLock<Option<amdgpu::modeset::Baseline>>,
    /// Whether the display firmware was loaded (stage R3.3).
    dmcub_up: core::sync::atomic::AtomicBool,
    /// The encoder's front end and back end registers as the firmware had
    /// them before a clock change (`DIG_FE_CNTL`, `DIG_BE_CNTL`), for the
    /// reconnection.
    link_saved: SpinLock<Option<(u32, u32)>>,
    /// Itself, for the card's display to reach the driver.
    me: Weak<Amd>,
    /// A mode set is running (one at a time).
    modeset_busy: core::sync::atomic::AtomicBool,
    /// The graphics ring, once started (stage R4).
    gfx: SpinLock<Option<crate::gcring::Gfx>>,
    /// System DMA engine 0, once started.
    sdma: SpinLock<Option<crate::gcring::Sdma>>,
    /// A chip reset is running.
    reset_busy: core::sync::atomic::AtomicBool,
    /// The render interface, once the card has one (a reset completes its
    /// jobs).
    render: SpinLock<Option<Weak<crate::render::Render>>>,
    /// What the monitor's EDID said, if it was read (stage R3.5).
    sink: SpinLock<Option<edid::Edid>>,
}

/// Where the GART's page table is in video memory (BAR0 offset): past the
/// interrupt handler's ring, well below the top 2 MiB.
pub(crate) const GART_AT: usize = 144 << 20;
/// The table's size: 8 bytes for each 4 KiB page of a 1 GiB GART.
pub(crate) const GART_BYTES: usize = (1 << 30) / 4096 * 8;

impl core::fmt::Debug for Amd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Amd").finish_non_exhaustive()
    }
}

/// Reads the identity (two plain reads); `None` if the device does not
/// answer.
pub(crate) fn identify(gpu: &Aperture) -> Option<Identity> {
    let w = Bar5(gpu);
    let memsize = regs::Window::read32(&w, regs::RCC_CONFIG_MEMSIZE);
    if memsize == u32::MAX {
        return None;
    }
    let ident = gpu.pci().function.header.ident;
    Some(Identity {
        device: ident.device,
        revision: ident.revision,
        vram_mib: memsize,
        psp_ready: regs::Window::read32(&w, regs::MP0_C2PMSG_33) >> 31 & 1 != 0,
    })
}

impl Amd {
    pub(crate) fn new(name: String, gpu: Weak<Aperture>, me: Weak<Amd>) -> Self {
        Self {
            name,
            table: SpinLock::new(None),
            hdp_ready: core::sync::atomic::AtomicBool::new(false),
            gfx_state: core::sync::atomic::AtomicU8::new(0),
            dri_unit: core::sync::atomic::AtomicI32::new(-1),
            irq: Arc::new(crate::interrupt::Irq::new(gpu.clone())),
            gpu,
            kms: SpinLock::new(None),
            dummy: SpinLock::new(None),
            psp: SpinLock::new(None),
            hdmi_prepared: core::sync::atomic::AtomicBool::new(false),
            clock_khz: core::sync::atomic::AtomicU32::new(amdgpu::modeset::FIRMWARE_CLOCK_KHZ),
            baseline: SpinLock::new(None),
            dmcub_up: core::sync::atomic::AtomicBool::new(false),
            link_saved: SpinLock::new(None),
            me,
            modeset_busy: core::sync::atomic::AtomicBool::new(false),
            gfx: SpinLock::new(None),
            sdma: SpinLock::new(None),
            sink: SpinLock::new(None),
            reset_busy: core::sync::atomic::AtomicBool::new(false),
            render: SpinLock::new(None),
        }
    }

    pub(crate) fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("{}: {args}", self.name));
    }

    fn gpu(&self) -> Result<Arc<Aperture>, abi::errno::Errno> {
        self.gpu.upgrade().ok_or(abi::errno::Errno::ENXIO)
    }

    /// Reads the discovery table through the index registers and logs what
    /// is in it.
    pub(crate) fn discover(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        self.log(format_args!("reading the IP discovery table"));
        let found = {
            // One lock across the whole index/data sequence.
            let mut cache = self.table.lock();
            let r = regs::read_discovery(&Bar5(&gpu));
            if let Ok(d) = &r {
                *cache = Some(d.clone());
            }
            r
        };
        let d = match found {
            Ok(d) => d,
            Err(e) => {
                self.log(format_args!("discovery table not read: {e:?}"));
                return Err(Errno::EIO);
            }
        };
        self.log(format_args!(
            "discovery table version {}.{}, {} die(s), {} block(s), {} harvested",
            d.version.0,
            d.version.1,
            d.dies.len(),
            d.dies.first().map_or(0, |x| x.ips.len()),
            d.harvest.len(),
        ));
        if let Some(die) = d.dies.first() {
            for ip in &die.ips {
                self.log(format_args!(
                    "  block {} instance {}: version {}.{}.{}, {} segment(s): {:x?}",
                    ip.hw_id,
                    ip.instance,
                    ip.major,
                    ip.minor,
                    ip.revision,
                    ip.bases.len(),
                    ip.bases,
                ));
            }
        }
        if let Some(gc) = &d.gc {
            self.log(format_args!(
                "  GC info {}.{}: {} shader engine(s), {} shader array(s) each, {} compute \
                 unit(s) per array",
                gc.version.0,
                gc.version.1,
                gc.shader_engines,
                gc.shader_arrays_per_se,
                gc.cus_per_sa(),
            ));
        }
        Ok(())
    }

    /// Makes the display the firmware left a graphics card, and takes over
    /// from the firmware framebuffer's card, if this is the display the
    /// console shows (docs/GRAPHICS.md §16.3). Logs why not otherwise.
    pub(crate) fn register_kms(&self, s: &sched::Sleepable) {
        if self.kms.lock().is_some() {
            return;
        }
        let Ok(gpu) = self.gpu() else {
            return;
        };
        if self.table.lock().is_none() && self.discover().is_err() {
            return;
        }
        {
            let cache = self.table.lock();
            let Some(d) = cache.as_ref() else {
                return;
            };
            if let Err(skip) = display::check(d) {
                self.log(format_args!("no graphics card: {skip:?}"));
                return;
            }
        }
        // The monitor first: the card's modes follow its EDID.
        self.read_sink();
        let card = match crate::kms::card(&gpu, Arc::clone(&self.irq), self.me.clone()) {
            Ok(c) => c,
            Err(e) => {
                self.log(format_args!("no graphics card: {e:?}"));
                return;
            }
        };
        let render = crate::render::Render::new(self.me.clone(), 512 << 20);
        // A fence's interrupt wakes whoever waits for a job.
        *self.render.lock() = Some(Arc::downgrade(&render));
        let queue = render.queue();
        let _ = self.irq.set_on_eop(Box::new(move || {
            queue.wake_all();
        }));
        card.set_render(render);
        let node = match drm::register(card.clone()) {
            Ok(n) => n,
            Err(e) => {
                self.log(format_args!("graphics card not registered: {e:?}"));
                return;
            }
        };
        self.dri_unit.store(
            i32::try_from(node.unit()).unwrap_or(-1),
            core::sync::atomic::Ordering::Release,
        );
        self.log(format_args!(
            "graphics card dri/card{} over the firmware's display",
            node.unit()
        ));
        let gone = drm::retire_firmware_cards(s);
        if gone > 0 {
            self.log(format_args!("took over from the firmware framebuffer card"));
        }
        // Frames come from the display's interrupt when the function has a
        // vector; without one the core times them.
        let frames = Arc::clone(&card);
        self.irq.set_on_frame(alloc::boxed::Box::new(move |now| {
            use drm::DrmDriver;
            if let Some(kms) = frames.kms() {
                kms.vblank(0, now);
            }
        }));
        if self.irq.start().is_ok() {
            self.log(format_args!(
                "vertical blanking from the display's interrupt"
            ));
        }
        *self.kms.lock() = Some((card, node));
    }

    /// Takes the card away. The firmware's picture is shown again unless
    /// the machine is shutting down or the device is being reset: then the
    /// last frame stays, as it does across a reboot, rather than flashing
    /// the console.
    pub(crate) fn unregister_kms(&self, s: &sched::Sleepable, restore: bool) {
        let held = self.kms.lock().take();
        if let Some((card, node)) = held {
            if restore {
                // The console is the firmware's picture: its mode first.
                let _ = self.set_mode_for(Some(s), None);
                card.restore();
            }
            node.unregister(s);
        }
    }

    /// Makes the interrupt handler's ring and turns the frame-start interrupt
    /// on (stage R2.1).
    pub(crate) fn start_interrupts(&self) -> Result<(), abi::errno::Errno> {
        let r = self.irq.start();
        match &r {
            Ok(()) => self.log(format_args!("interrupts on")),
            Err(e) => self.log(format_args!("interrupts not started: {e:?}")),
        }
        r
    }

    /// Turns the interrupts off.
    pub(crate) fn stop_interrupts(&self) {
        self.irq.stop();
    }

    /// Sets a translation hub up (stages R2.2 and R2.3, docs/GRAPHICS.md
    /// §16.6): the GART's flat page table in video memory (every entry
    /// invalid; the two hubs share it), a dummy system page for faults, the
    /// hub's registers in the specification's order, and an invalidation of
    /// VMID 0 on the processor's engine. The graphics hub serves the
    /// engines and is idle; the memory hub serves the display and the
    /// interrupt handler, so the frame counter is read before and after, to
    /// show the display kept running.
    pub(crate) fn gmc(&self, hub: amdgpu::gmc::Hub) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        use amdgpu::flip;
        use amdgpu::gmc::{self, Hub, Reg};
        use amdgpu::regs::Window;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let (len, tag) = (gpu.vram_len(), gpu.dma_tag());
        let (Some(len), Some(tag)) = (len, tag) else {
            self.log(format_args!(
                "translation hub: no video memory window or no DMA tag"
            ));
            return Err(Errno::ENXIO);
        };
        if len < GART_AT + GART_BYTES {
            return Err(Errno::ENXIO);
        }
        // The first hub makes the table and the page; the second shares them.
        let existing = self
            .dummy
            .lock()
            .as_ref()
            .map(vm::dma::DmaCoherent::bus_addr);
        let (dummy_page, fresh) = if let Some(addr) = existing {
            (addr, None)
        } else {
            for at in (0..GART_BYTES).step_by(4) {
                gpu.vram_write32(GART_AT + at, 0);
            }
            let page = tag.coherent::<u8>(4096).map_err(|_| Errno::ENOMEM)?;
            (page.bus_addr(), Some(page))
        };
        let setup = gmc::Setup {
            gart_table: (u64::from(w.read32(hub.reg(Reg::FbOffset))) << 24) + GART_AT as u64,
            gart_start: 0,
            gart_end: (1 << 30) - 1,
            dummy_page,
            retry_interrupt: hub == Hub::Gfx,
        };
        let before = flip::frame_count(&w);
        // The table is in video memory: make the writes visible first.
        let _ = gpu.vram_read32(GART_AT);
        if !self
            .hdp_ready
            .swap(true, core::sync::atomic::Ordering::AcqRel)
        {
            flip::hdp_init(&w);
        }
        flip::hdp_flush(&w);
        if let Err(e) = gmc::init(&w, hub, &setup) {
            self.log(format_args!("{hub:?} hub: not set up: {e:?}"));
            return Err(Errno::EINVAL);
        }
        if let Some(page) = fresh {
            *self.dummy.lock() = Some(page);
        }
        flip::hdp_flush(&w);
        let done = gmc::invalidate(&w, hub, 0, 1000, &|| bus::manager().delay_us(10));
        let after = flip::frame_count(&w);
        self.log(format_args!(
            "{hub:?} hub: GART {:#x}..{:#x} (table at {:#x}), context 0 {:#010x}, L2 {:#010x}, L1 {:#010x}; \
             invalidation {done:?}; display frames {before} to {after}",
            setup.gart_start,
            setup.gart_end,
            setup.gart_table,
            w.read32(hub.context_cntl(0)),
            w.read32(hub.reg(Reg::L2Cntl)),
            w.read32(hub.reg(Reg::L1Tlb)),
        ));
        done.map_err(|_| Errno::EIO)
    }

    /// Brings the security processor up as far as the trusted memory zone
    /// (stages R3.1 and R3.2), on the calling kernel thread, which reads the
    /// firmware file. The translation hubs are set up first if they are not
    /// (the specification: the PSP's addresses go through them).
    pub(crate) fn psp_bring_up(&self, s: &sched::Sleepable) {
        use amdgpu::flip;
        use amdgpu::gmc::Hub;
        let Ok(gpu) = self.gpu() else {
            return;
        };
        if self.psp.lock().is_some() {
            self.log(format_args!("PSP: already up"));
            return;
        }
        let Some(fw) = crate::firmware() else {
            self.log(format_args!("PSP: no firmware reader"));
            return;
        };
        let toc = match fw.read(s, crate::psp::TOC_PATH) {
            Ok(t) => t,
            Err(e) => {
                self.log(format_args!(
                    "PSP: {} not read: {e:?}",
                    crate::psp::TOC_PATH
                ));
                return;
            }
        };
        if self.dummy.lock().is_none() {
            let _ = self.gmc(Hub::Gfx);
            let _ = self.gmc(Hub::Mm);
        }
        // The flush registers are set up for the table's write.
        if !self
            .hdp_ready
            .swap(true, core::sync::atomic::Ordering::AcqRel)
        {
            flip::hdp_init(&Bar5(&gpu));
        }
        let log = |args: core::fmt::Arguments<'_>| self.log(args);
        match crate::psp::bring_up(&gpu, &toc, &log) {
            Ok(b) => {
                self.log(format_args!(
                    "PSP: up; trusted memory zone {:#x} bytes",
                    b.tmr_size
                ));
                *self.psp.lock() = Some(b);
            }
            Err(e) => self.log(format_args!("PSP: stopped: {e:?}")),
        }
    }

    /// The device's video BIOS: the image its `VFCT` table holds (01 section
    /// 5.2). A chip reset's re-initialisation runs the image's command
    /// tables. `None` without the table or a block for this device.
    pub(crate) fn vbios_image(&self) -> Option<Vec<u8>> {
        let gpu = self.gpu().ok()?;
        let vfct = bus::manager().acpi_table(*b"VFCT")?;
        let a = gpu.pci().function.addr;
        let ident = gpu.pci().function.header.ident;
        amdgpu::vbios::vfct_image(
            &vfct,
            ident.vendor,
            ident.device,
            u32::from(a.slot),
            u32::from(a.function),
        )
        .ok()
    }

    /// The aperture, for the render interface.
    pub(crate) fn gpu_arc(&self) -> Result<Arc<Aperture>, abi::errno::Errno> {
        self.gpu()
    }

    /// The video memory's physical offset (the carve-out's address), from
    /// the graphics hub.
    pub(crate) fn vram_phys(&self) -> Result<u64, abi::errno::Errno> {
        use amdgpu::gmc::{Hub, Reg};
        use amdgpu::regs::Window;
        let gpu = self.gpu()?;
        Ok(u64::from(Bar5(&gpu).read32(Hub::Gfx.reg(Reg::FbOffset))) << 24)
    }

    /// The started graphics block, locked.
    pub(crate) fn gfx_lock(&self) -> sync::SpinGuard<'_, Option<crate::gcring::Gfx>> {
        self.gfx.lock()
    }

    /// Starts the graphics block for a client of the render node, once: the
    /// security processor, the engines' firmware, the ring (stages R3 and
    /// R4.1 to R4.4). Another caller waits for the first.
    ///
    /// # Errors
    ///
    /// `ENXIO` if a step failed (the log says which).
    pub(crate) fn ensure_gfx(&self, s: &sched::Sleepable) -> Result<(), abi::errno::Errno> {
        use core::sync::atomic::Ordering::{AcqRel, Acquire, Release};
        if self.gfx_ready() {
            return Ok(());
        }
        if self
            .gfx_state
            .compare_exchange(0, 1, AcqRel, Acquire)
            .is_ok()
        {
            let ok = self.start_gfx(s);
            self.gfx_state.store(if ok { 2 } else { 3 }, Release);
        }
        while self.gfx_state.load(Acquire) == 1 {
            sched::yield_now();
        }
        if self.gfx_ready() {
            Ok(())
        } else {
            Err(abi::errno::Errno::ENXIO)
        }
    }

    fn start_gfx(&self, s: &sched::Sleepable) -> bool {
        let Ok(gpu) = self.gpu() else {
            return false;
        };
        let log = |args: core::fmt::Arguments<'_>| self.log(args);
        use amdgpu::regs::Window;
        if Bar5(&gpu).read32(crate::gcload::BOOTLOAD_STATUS) >> 31 == 0 {
            self.gc_load(s);
        }
        match crate::gcring::start(&gpu, &log) {
            Ok(g) => {
                *self.gfx.lock() = Some(g);
                // The system DMA engine too; the graphics ring works without.
                match crate::gcring::start_sdma(&gpu, &log) {
                    Ok(d) => *self.sdma.lock() = Some(d),
                    Err(e) => self.log(format_args!("SDMA: not started: {e:?}")),
                }
                true
            }
            Err(e) => {
                self.log(format_args!("GC: ring not started: {e:?}"));
                false
            }
        }
    }

    /// How many kernel compute rings answered their start-up test.
    pub(crate) fn compute_rings(&self) -> usize {
        self.gfx
            .lock()
            .as_ref()
            .map_or(0, crate::gcring::Gfx::compute_rings)
    }

    /// Whether system DMA engine 0 has been started.
    pub(crate) fn sdma_ready(&self) -> bool {
        self.sdma.lock().is_some()
    }

    /// The started system DMA engine, locked.
    pub(crate) fn sdma_lock(&self) -> sync::SpinGuard<'_, Option<crate::gcring::Sdma>> {
        self.sdma.lock()
    }

    /// Whether the graphics ring has been started.
    pub(crate) fn gfx_ready(&self) -> bool {
        self.gfx.lock().is_some()
    }

    /// Hands the graphics, compute and DMA engines' firmware to the security
    /// processor and starts the RLC autoload (stage R4.1), on the calling
    /// kernel thread (which reads the files). The processor is brought up
    /// first if it is not.
    pub(crate) fn gc_load(&self, s: &sched::Sleepable) {
        let Ok(gpu) = self.gpu() else {
            return;
        };
        if self.psp.lock().is_none() {
            self.psp_bring_up(s);
        }
        let Some(mut brought) = self.psp.lock().take() else {
            self.log(format_args!("GC: no security processor"));
            return;
        };
        let log = |args: core::fmt::Arguments<'_>| self.log(args);
        let Some(fw) = crate::firmware() else {
            self.log(format_args!("GC: no firmware reader"));
            *self.psp.lock() = Some(brought);
            return;
        };
        let mut data = alloc::vec::Vec::new();
        for name in crate::gcload::FILES {
            let path = alloc::format!("{}{name}", crate::gcload::DIR);
            match fw.read(s, &path) {
                Ok(d) => data.push(d),
                Err(e) => {
                    self.log(format_args!("GC: {path} not read: {e:?}"));
                    *self.psp.lock() = Some(brought);
                    return;
                }
            }
        }
        let files = amdgpu::gcfw::Files {
            sdma: &data[0],
            ce: &data[1],
            pfp: &data[2],
            me: &data[3],
            mec: &data[4],
            mec2: &data[5],
            rlc: &data[6],
        };
        crate::gcload::snapshot(&gpu, "before", &log);
        match crate::gcload::load(&gpu, &mut brought.psp, &files, &log) {
            Ok(()) => {
                let (cp, boot, done) = crate::gcload::wait_autoload(&gpu, 2000);
                self.log(format_args!(
                    "GC: autoload {}: CP_STAT {cp:#x}, RLC_RLCS_BOOTLOAD_STATUS {boot:#010x}",
                    if done {
                        "finished"
                    } else {
                        "did not finish in 2 s"
                    }
                ));
                crate::gcload::snapshot(&gpu, "after the autoload", &log);
            }
            Err(e) => self.log(format_args!("GC: stopped: {e:?}")),
        }
        *self.psp.lock() = Some(brought);
    }

    /// Loads the display microcontroller's firmware (stage R3.3), on the
    /// calling kernel thread: the security processor first if it is not up.
    pub(crate) fn dmcub_load(&self, s: &sched::Sleepable) {
        let Ok(gpu) = self.gpu() else {
            return;
        };
        if self.psp.lock().is_none() {
            self.psp_bring_up(s);
        }
        let Some(mut brought) = self.psp.lock().take() else {
            self.log(format_args!("DMCUB: no security processor"));
            return;
        };
        let Some(fw) = crate::firmware() else {
            self.log(format_args!("DMCUB: no firmware reader"));
            *self.psp.lock() = Some(brought);
            return;
        };
        match fw.read(s, crate::dmcub::PATH) {
            Ok(file) => {
                let log = |args: core::fmt::Arguments<'_>| self.log(args);
                match crate::dmcub::load(&gpu, &mut brought.psp, &file, &log) {
                    Ok(()) => {
                        self.dmcub_up
                            .store(true, core::sync::atomic::Ordering::Release);
                        self.log(format_args!("DMCUB: loaded"));
                    }
                    Err(e) => self.log(format_args!("DMCUB: stopped: {e:?}")),
                }
            }
            Err(e) => self.log(format_args!(
                "DMCUB: {} not read: {e:?}",
                crate::dmcub::PATH
            )),
        }
        *self.psp.lock() = Some(brought);
    }

    /// Logs the HDMI and audio registers worth watching, with the frame
    /// count, under `what`.
    fn hdmi_watch(&self, w: &Bar5<'_>, what: &str) {
        use amdgpu::regs::Window;
        use amdgpu::{flip, hdmi};
        self.log(format_args!(
            "HDMI: {what} (frames {}):",
            flip::frame_count(w)
        ));
        for (name, at) in hdmi::WATCH {
            self.log(format_args!(
                "  {name} @{at:#07x} = {:#010x}",
                w.read32(*at)
            ));
        }
        let ep = |i: u32| hdmi::endpoint_read(w, i);
        self.log(format_args!(
            "  endpoint 0: speakers {:#x}, descriptor 1 {:#x}, hot-plug {:#x}",
            ep(hdmi::EP_CHANNEL_SPEAKER),
            ep(hdmi::EP_AUDIO_DESCRIPTOR + 1),
            ep(hdmi::EP_HOTPLUG)
        ));
    }

    /// Changes the mode to CTA video identification code `vic` on the running
    /// pipe (stage R3.4, 08 section 3.7). The firmware's own mode (code 16)
    /// puts every register back to the firmware's values; a mode at the
    /// running pixel clock and size changes the few registers that differ;
    /// any other needs the display firmware loaded (`dmcub`) to set the pixel
    /// clock. A plan is checked and logged before anything is written; the
    /// picture goes while the generator is stopped.
    pub(crate) fn set_mode(&self, vic: i32) -> Result<(), abi::errno::Errno> {
        self.set_mode_by_code(vic, true)
    }

    /// Reads the monitor's EDID over the DDC engine and keeps what it says:
    /// the modes the card offers follow it (stage R3.5). A monitor that does
    /// not answer, or answers with a bad block, leaves the whole table.
    pub(crate) fn read_sink(&self) {
        use amdgpu::ddc;
        use amdgpu::regs::Window;
        let Ok(gpu) = self.gpu() else {
            return;
        };
        let w = Bar5(&gpu);
        let pause = |us: u32| bus::manager().delay_us(u64::from(us));
        // The specification's prescale for 100 kHz first, then the platform's
        // own (the time base differs from the one the specification's
        // prescale is for).
        let platform = w.read32(ddc::SPEED) >> 16;
        let raw = [ddc::PRESCALE, platform]
            .into_iter()
            .find_map(|prescale| ddc::read_edid(&w, 0, 4, prescale, &pause).ok());
        let Some(raw) = raw else {
            self.log(format_args!("sink: no EDID answered; every mode offered"));
            return;
        };
        match edid::parse(&raw) {
            Ok(e) => {
                let modes = crate::sink::offered(Some(&e));
                self.log(format_args!(
                    "sink: {:?}, {} timings in the EDID, {} offered of the card's {}",
                    e.name,
                    e.modes.len(),
                    modes.len(),
                    crate::sink::SETTABLE.len()
                ));
                *self.sink.lock() = Some(e);
            }
            Err(e) => self.log(format_args!(
                "sink: EDID not valid ({e:?}); every mode offered"
            )),
        }
    }

    /// What the monitor is and which modes it is offered, for the `sink`
    /// sysctl.
    pub(crate) fn sink_summary(&self) -> String {
        let modes = self.offered_modes();
        let list: Vec<String> = modes
            .iter()
            .map(|(vic, m)| {
                let star = if m.kind & abi::drm::DRM_MODE_TYPE_PREFERRED != 0 {
                    "*"
                } else {
                    ""
                };
                format!("{vic}{star}")
            })
            .collect();
        match self.sink.lock().as_ref() {
            Some(e) => format!(
                "{:?}: modes {} (of {} timings; * preferred)",
                e.name.as_deref().unwrap_or("unnamed"),
                list.join(" "),
                e.modes.len()
            ),
            None => format!("no EDID: modes {} (* preferred)", list.join(" ")),
        }
    }

    /// The modes the card offers: those of the table it can set that the
    /// monitor's EDID lists, or the whole table before the EDID is read.
    pub(crate) fn offered_modes(&self) -> alloc::vec::Vec<(u8, kms_check::Mode)> {
        crate::sink::offered(self.sink.lock().as_ref())
    }

    /// Sets the mode `mode` (one of the modes the card can set, [`crate::sink::settable`]), or the
    /// firmware's if `None`; the card's display calls it on a mode set. A
    /// pixel clock change needs the display firmware, which is loaded here on
    /// the first need (this may sleep on `s`, the requesting thread).
    pub(crate) fn set_mode_for(
        &self,
        s: Option<&sched::Sleepable>,
        mode: Option<&kms_check::Mode>,
    ) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        use core::sync::atomic::Ordering;
        let vic = match mode {
            None => 16,
            Some(m) => crate::sink::settable()
                .into_iter()
                .find(|(_, w)| {
                    (w.hdisplay, w.vdisplay, w.htotal, w.vtotal, w.clock_khz)
                        == (m.hdisplay, m.vdisplay, m.htotal, m.vtotal, m.clock_khz)
                })
                .map(|(v, _)| v)
                .ok_or(Errno::EINVAL)?,
        };
        if self.modeset_busy.swap(true, Ordering::AcqRel) {
            return Err(Errno::EBUSY);
        }
        let mut result = Ok(());
        // The display firmware is needed for any clock other than the one
        // running; the platform's own copy of the video BIOS is read then.
        let wanted = crate::sink::settable()
            .into_iter()
            .find(|(v, _)| *v == vic)
            .map_or(0, |(_, m)| m.clock_khz);
        if wanted != self.clock_khz.load(Ordering::Acquire)
            && !self.dmcub_up.load(Ordering::Acquire)
        {
            match s {
                Some(s) => self.dmcub_load(s),
                None => result = Err(Errno::ENXIO),
            }
        }
        if result.is_ok() {
            result = self.set_mode_by_code(i32::from(vic), false);
        }
        self.modeset_busy.store(false, Ordering::Release);
        result
    }

    fn set_mode_by_code(&self, vic: i32, measure: bool) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        use amdgpu::regs::Window;
        use amdgpu::{dmcub, flip, hdmi, modeset};
        use core::sync::atomic::Ordering;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let pause = |us: u32| bus::manager().delay_us(u64::from(us));
        let code = u8::try_from(vic).unwrap_or(0);
        let Some(mut mode) = edid::mode_for_vic(code) else {
            self.log(format_args!("mode: no timing for code {vic}"));
            return Err(Errno::EINVAL);
        };
        if code == 1 {
            // The specification's clock for 640x480 (the shared table rounds it).
            mode.clock_khz = 25_175;
        }
        if let Err(e) = modeset::preflight(&w) {
            self.log(format_args!("mode: the pipe is not as expected: {e:?}"));
            return Err(Errno::EINVAL);
        }
        // The firmware's own register values, before the first change.
        let baseline = *self
            .baseline
            .lock()
            .get_or_insert_with(|| modeset::snapshot(&w));
        let clock = self.clock_khz.load(Ordering::Acquire);
        let firmware_mode = code == 16;
        let target_clock = if firmware_mode {
            modeset::FIRMWARE_CLOCK_KHZ
        } else {
            mode.clock_khz
        };
        let plan = if firmware_mode {
            Ok(modeset::plan_restore(&w, &baseline))
        } else {
            match modeset::plan(&w, &mode, clock) {
                Err(modeset::Error::ClockChange | modeset::Error::SizeChange) => {
                    modeset::plan_retimed(&w, &mode)
                }
                other => other,
            }
        };
        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                self.log(format_args!("mode: code {vic} not set: {e:?}"));
                return Err(Errno::ENOTSUP);
            }
        };
        let clock_change = target_clock != clock;
        self.log(format_args!(
            "mode: code {vic}: {}x{} total {}x{} at {} kHz (running {} kHz); the writes:",
            mode.hdisplay, mode.vdisplay, mode.htotal, mode.vtotal, target_clock, clock
        ));
        let mut any = false;
        for (name, at, old, new) in plan.changes() {
            any = true;
            self.log(format_args!(
                "  {name} @{at:#07x}: {old:#010x} -> {new:#010x}"
            ));
        }
        if !any && !clock_change {
            self.log(format_args!("mode: already running"));
            return Ok(());
        }
        let bios_6 = w.read32(0x148);
        if clock_change {
            self.log(format_args!(
                "mode: the pixel clock {clock} -> {target_clock} kHz through the display firmware; BIOS scratch 6 reads {bios_6:#x}"
            ));
            if !self.dmcub_up.load(Ordering::Acquire) || !dmcub::running(&w) {
                self.log(format_args!(
                    "mode: a clock change needs the display firmware: write 1 to `dmcub` first"
                ));
                return Err(Errno::ENXIO);
            }
        }
        let wide = u32::from(mode.hdisplay) * 9 == u32::from(mode.vdisplay) * 16;
        let before = flip::frame_count(&w);
        let vram_base = u64::from(w.read32(flip::FB_LOCATION_BASE) & 0xffff) << 24;
        let inbox = dmcub::Inbox::new(vram_base + crate::dmcub::MAILBOXES_AT as u64);
        let mem = crate::psp::vram_memory(&gpu, vram_base);
        // One firmware command, waited for; logs what it did.
        let command = |what: &str, bytes: &[u8]| -> bool {
            match inbox.submit(&w, &mem, bytes, 200, 1000, &pause) {
                Ok(()) => {
                    self.log(format_args!(
                        "mode: {what}: taken (DIG_FE_CNTL {:#010x}, DIG_BE_CNTL {:#010x}, FIFO {:#010x})",
                        w.read32(hdmi::DIG_FE_CNTL),
                        w.read32(hdmi::DIG_BE_CNTL),
                        w.read32(0x15544),
                    ));
                    true
                }
                Err(e) => {
                    self.log(format_args!("mode: {what}: {e:?}"));
                    false
                }
            }
        };
        // The front end's connection as the firmware left it (reconnected
        // after the way on, step 12 of the reference).
        let front_end = w.read32(hdmi::DIG_FE_CNTL);
        if clock_change {
            *self.link_saved.lock() = Some((front_end, w.read32(hdmi::DIG_BE_CNTL)));
            // The way off (08 section 3.7.7): the packets stopped, the plane
            // blanked, the transmitter off, then the generator stopped.
            self.link_state(&w, "before");
            hdmi::stop_packets(&w);
            modeset::blank(&w);
            if !command("transmitter off", &dmcub::transmitter_disable()) {
                modeset::unblank(&w);
                self.log(format_args!(
                    "mode: nothing else was changed; the plane is unblanked again"
                ));
                return Err(Errno::EIO);
            }
        }
        if let Err(e) = modeset::stop(&w, &pause) {
            self.log(format_args!("mode: stop: {e:?}"));
            return Err(Errno::EIO);
        }
        self.log(format_args!("mode: generator stopped"));
        let mut apply = true;
        let mut running_clock = clock;
        if clock_change {
            if command(
                "set pixel clock",
                &dmcub::set_pixel_clock(target_clock * 10, 0),
            ) {
                running_clock = target_clock;
                self.clock_khz.store(target_clock, Ordering::Release);
            } else {
                self.log(format_args!(
                    "mode: the clock did not change: restarting with the old timing at the old clock"
                ));
                apply = false;
            }
        }
        if apply {
            modeset::apply(&w, &plan);
        }
        let started = modeset::start_blanked(&w, &pause);
        self.irq.rearm_frame_start();
        if clock_change {
            // The way on: the encoder in HDMI mode and its stream set up, the
            // packets, the transmitter on with the new symbol clock.
            let symbol = running_clock / 10;
            hdmi::set_dig_mode(&w, 3);
            command("encoder stream setup", &dmcub::encoder_stream_setup(symbol));
            hdmi::stream_controls(&w);
            // Step 12: the front end connected again, its source the timing
            // generator, as the firmware had it.
            self.link_reconnect_now(&w, "before the transmitter is on");
        }
        let avi_code = if apply { code } else { 16 };
        let avi_wide = if apply { wide } else { true };
        let clear = hdmi::write_packet(&w, 0, &hdmi::avi_for(avi_code, avi_wide), &pause);
        if clock_change {
            hdmi::packet_controls(&w);
            command(
                "transmitter on",
                &dmcub::transmitter_enable(running_clock / 10),
            );
            // The transmitter-on command clears the front end again.
            self.link_reconnect_now(&w, "after the transmitter is on");
        }
        if let Err(e) = started {
            self.log(format_args!("mode: start: {e:?} (the plane stays blanked)"));
            return Err(Errno::EIO);
        }
        modeset::unblank(&w);
        self.log(format_args!(
            "mode: generator running, AVI info frame for code {avi_code} written (no conflict: {clear}); frames {before} -> {}",
            flip::frame_count(&w)
        ));
        if clock_change {
            bus::manager().delay_us(100_000);
            self.link_state(&w, "after");
        }
        if measure {
            let _ = self.refresh();
        }
        if !apply {
            return Err(Errno::EIO);
        }
        Ok(())
    }

    /// Puts the encoder's front end back as it was before the last clock
    /// change (the front end register, and the back end's front-end select
    /// bit), if either differs; logs what it did.
    fn link_reconnect_now(&self, w: &Bar5<'_>, when: &str) {
        use amdgpu::hdmi;
        use amdgpu::regs::Window;
        let Some((fe, be)) = *self.link_saved.lock() else {
            self.log(format_args!("LINK: nothing saved to reconnect"));
            return;
        };
        let (fe_now, be_now) = (w.read32(hdmi::DIG_FE_CNTL), w.read32(hdmi::DIG_BE_CNTL));
        if fe_now == fe && be_now & 0x100 == be & 0x100 {
            self.log(format_args!("LINK: front end already connected {when}"));
            return;
        }
        hdmi::update(w, hdmi::DIG_BE_CNTL, 0x100, be);
        w.write32(hdmi::DIG_FE_CNTL, fe);
        self.log(format_args!(
            "LINK: reconnected {when}: DIG_FE_CNTL {fe_now:#010x} -> {:#010x}, DIG_BE_CNTL {be_now:#010x} -> {:#010x}",
            w.read32(hdmi::DIG_FE_CNTL),
            w.read32(hdmi::DIG_BE_CNTL)
        ));
    }

    /// Logs the link's read-only diagnostics (08 section 3.7.7) under `what`.
    fn link_state(&self, w: &Bar5<'_>, what: &str) {
        use amdgpu::hdmi;
        use amdgpu::regs::Window;
        self.log(format_args!("LINK: {what}:"));
        for (name, at) in hdmi::LINK_STATE {
            self.log(format_args!("  {name} @{at:#07x} = {:#010x}", w.read32(at)));
        }
    }

    /// A one-line summary of the cached table, for a sysctl.
    pub(crate) fn summary(&self) -> String {
        match &*self.table.lock() {
            None => "not read".into(),
            Some(d) => {
                let v = |id| {
                    d.ip(id, 0).map_or_else(
                        || "-".into(),
                        |i| format!("{}.{}.{}", i.major, i.minor, i.revision),
                    )
                };
                format!(
                    "GC {} SDMA {} DMU {} PSP {} SMU {} IH {}",
                    v(hw::GC),
                    v(hw::SDMA0),
                    v(hw::DMU),
                    v(hw::MP0),
                    v(hw::MP1),
                    v(hw::OSSSYS),
                )
            }
        }
    }
}

/// `hw.dri.<unit>.busid` and `.modesetting`: where the graphics library finds
/// which device a node belongs to (it reads the device's identity through the
/// bus address, `pci:<domain>:<bus>:<device>.<function>`).
fn publish_dri(t: &mut sysctl::Tree, a: &Arc<Amd>) {
    let unit = a.dri_unit.load(core::sync::atomic::Ordering::Acquire);
    if unit < 0 {
        return;
    }
    let Ok(gpu) = a.gpu() else {
        return;
    };
    let addr = gpu.pci().function.addr;
    let busid = alloc::format!(
        "pci:{:04x}:{:02x}:{:02x}.{}",
        addr.domain,
        addr.bus,
        addr.slot,
        addr.function
    );
    let root = t.root();
    let hw = match t.child(root, "hw") {
        Some(h) => h,
        None => return,
    };
    let dri = match t.child(hw, "dri") {
        Some(d) => d,
        None => match t.add_node(hw, sysctl::Number::Auto, "dri", "Direct rendering devices") {
            Ok(d) => d,
            Err(_) => return,
        },
    };
    let Ok(n) = t.add_node(
        dri,
        sysctl::Number::Auto,
        &alloc::format!("{unit}"),
        "A graphics card",
    ) else {
        return;
    };
    let _ = t.add_value(
        n,
        sysctl::Number::Auto,
        "busid",
        "The device's bus address",
        sysctl::Value::Str(Box::new(move |out| out.push_str(&busid))),
    );
    let _ = t.add_value(
        n,
        sysctl::Number::Auto,
        "modesetting",
        "Whether the card sets display modes",
        sysctl::Value::Int(Box::new(|| 1)),
    );
}

/// The sysctls for `a`, under `node`: what the card is (the IP blocks it
/// has, what its interrupts did) and, unless `hw.drmn.debug` is 0, the
/// bring-up and probe triggers of [`debug`].
pub(crate) fn publish(t: &mut sysctl::Tree, node: sysctl::NodeId, a: &Arc<Amd>) {
    publish_dri(t, a);
    let r = Arc::clone(a);
    let _ = t.add_value(
        node,
        sysctl::Number::Auto,
        "sink",
        "The monitor's name and the video modes offered for it",
        sysctl::Value::Str(Box::new(move |out| out.push_str(&r.sink_summary()))),
    );
    let r = Arc::clone(a);
    let _ = t.add_value(
        node,
        sysctl::Number::Auto,
        "discovered",
        "The IP block versions, once `discover` has run",
        sysctl::Value::Str(Box::new(move |out| out.push_str(&r.summary()))),
    );
    let r = Arc::clone(a);
    let _ = t.add_value(
        node,
        sysctl::Number::Auto,
        "irq",
        "What the interrupts have done",
        sysctl::Value::Str(Box::new(move |out| out.push_str(&r.irq.summary()))),
    );
    if crate::debug_enabled() {
        debug::publish(t, node, a);
    }
}
