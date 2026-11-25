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
        }
    }

    fn log(&self, args: core::fmt::Arguments<'_>) {
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

    /// Reads the display registers the firmware left, if the table says it
    /// is the chip the list is for.
    pub(crate) fn display(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        let d = {
            let cache = self.table.lock();
            cache.clone()
        };
        let Some(d) = d else {
            self.log(format_args!(
                "display registers: read the discovery table first (dev.drmn sysctl `discover`)"
            ));
            return Err(Errno::EINVAL);
        };
        match display::read_all(&Bar5(&gpu), &d) {
            Ok(readings) => {
                self.log(format_args!(
                    "display registers as the firmware left them ({} reads):",
                    readings.len()
                ));
                for r in &readings {
                    self.log(format_args!(
                        "  {} @{:#07x} = {:#010x}",
                        r.name, r.offset, r.value
                    ));
                }
                Ok(())
            }
            Err(skip) => {
                self.log(format_args!("display registers not read: {skip:?}"));
                Err(Errno::ENOTSUP)
            }
        }
    }

    /// Reads the memory, interrupt and doorbell registers the firmware left
    /// (stage R2.0, docs/GRAPHICS.md §16.6) and logs them: plain reads.
    pub(crate) fn memory(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        let d = {
            let cache = self.table.lock();
            cache.clone()
        };
        let Some(d) = d else {
            self.log(format_args!(
                "memory registers: read the discovery table first (dev.drmn sysctl `discover`)"
            ));
            return Err(Errno::EINVAL);
        };
        match amdgpu::memory::read_all(&Bar5(&gpu), &d) {
            Ok(readings) => {
                self.log(format_args!(
                    "memory registers as the firmware left them ({} reads):",
                    readings.len()
                ));
                for r in &readings {
                    self.log(format_args!(
                        "  {} @{:#07x} = {:#010x}",
                        r.name, r.offset, r.value
                    ));
                }
                Ok(())
            }
            Err(skip) => {
                self.log(format_args!("memory registers not read: {skip:?}"));
                Err(Errno::ENOTSUP)
            }
        }
    }

    /// Tests whether BAR0 is the same memory as the video memory the display
    /// and the index registers see: words at the firmware's framebuffer are
    /// compared between the two paths, and then two words in unused memory
    /// (64 MiB and just below the aperture's end) are written through BAR0
    /// and read back through the index registers.
    pub(crate) fn vram_probe(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        let Some(len) = gpu.vram_len() else {
            self.log(format_args!("VRAM aperture (BAR0) is not mapped"));
            return Err(Errno::ENXIO);
        };
        self.log(format_args!("VRAM aperture (BAR0): {len:#x} bytes"));
        let mut same = true;
        for at in [0usize, 0x7800, 0x7e_8ff8, 0x80_0000] {
            let bar0 = gpu.vram_read32(at).unwrap_or(u32::MAX);
            let index = {
                let _guard = self.table.lock();
                regs::read_vram(&Bar5(&gpu), at as u64)
            };
            same &= bar0 == index;
            self.log(format_args!(
                "  offset {at:#x}: BAR0 {bar0:#010x}, index registers {index:#010x}"
            ));
        }
        for (at, value) in [(64usize << 20, 0xa5a5_1234u32), (len - 4, 0x5a5a_4321)] {
            let before = gpu.vram_read32(at).unwrap_or(0);
            gpu.vram_write32(at, value);
            let index = {
                let _guard = self.table.lock();
                regs::read_vram(&Bar5(&gpu), at as u64)
            };
            gpu.vram_write32(at, before);
            same &= index == value;
            self.log(format_args!(
                "  offset {at:#x}: wrote {value:#010x} through BAR0, the index registers read {index:#010x}"
            ));
        }
        self.log(format_args!(
            "BAR0 {} the video memory the index registers reach",
            if same { "is" } else { "is NOT" }
        ));
        Ok(())
    }

    /// Stage R1's first board test: shows a test pattern from video memory
    /// outside the firmware's framebuffer by flipping the display's surface
    /// to it, holds it three seconds, and flips back. The display's own
    /// state (timing, format, pitch) is never written.
    pub(crate) fn flip_test(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        use amdgpu::flip;
        use core::sync::atomic::Ordering;
        /// Where the test surface starts in video memory (past the firmware's
        /// 8 MiB framebuffer, and well below the top 2 MiB).
        const AT: usize = 16 << 20;
        const WIDTH: usize = 1920;
        const HEIGHT: usize = 1080;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let Some(len) = gpu.vram_len() else {
            self.log(format_args!("flip: VRAM aperture (BAR0) is not mapped"));
            return Err(Errno::ENXIO);
        };
        let bytes = WIDTH * HEIGHT * 4;
        if AT + bytes > len {
            return Err(Errno::ENOSPC);
        }
        // Only the firmware's own display, as the discovery table says it is
        // here, and as the registers say it is running.
        if self.table.lock().is_none() {
            self.discover()?;
        }
        {
            let cache = self.table.lock();
            let d = cache.as_ref().ok_or(Errno::EIO)?;
            if let Err(skip) = display::check(d) {
                self.log(format_args!("flip: this display is not adopted: {skip:?}"));
                return Err(Errno::ENODEV);
            }
        }
        let adopted = match flip::adopt(&w) {
            Ok(a) if a.pitch_pixels as usize == WIDTH => a,
            Ok(a) => {
                self.log(format_args!(
                    "flip: the pitch is {} pixels, not {WIDTH}",
                    a.pitch_pixels
                ));
                return Err(Errno::ENODEV);
            }
            Err(e) => {
                self.log(format_args!(
                    "flip: the display is not the firmware's: {e:?}"
                ));
                return Err(Errno::ENODEV);
            }
        };
        self.log(format_args!(
            "flip: the surface is at {:#x}; painting the test pattern at video memory {AT:#x}",
            flip::surface(&w)
        ));
        // Eight bars, a white frame, and a black row at every 100th line.
        const BARS: [u32; 8] = [
            0xffff_ffff,
            0xffff_ff00,
            0xff00_ffff,
            0xff00_ff00,
            0xffff_00ff,
            0xffff_0000,
            0xff00_00ff,
            0xff00_0000,
        ];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let frame = x < 8 || y < 8 || x >= WIDTH - 8 || y >= HEIGHT - 8;
                let px = if frame {
                    0xffff_ffff
                } else if y % 100 == 99 {
                    0xff00_0000
                } else {
                    BARS[x * 8 / WIDTH]
                };
                gpu.vram_write32(AT + 4 * (y * WIDTH + x), px);
            }
        }
        if !self.hdp_ready.swap(true, Ordering::AcqRel) {
            flip::hdp_init(&w);
        }
        // The last word back through BAR0 completes the posted writes.
        let _ = gpu.vram_read32(AT + bytes - 4);
        flip::hdp_flush(&w);
        let shown = adopted.vram_base + AT as u64;
        for (what, to) in [
            ("to the pattern", shown),
            ("back to the firmware's", adopted.vram_base),
        ] {
            flip::flip(&w, to);
            let mut waited_us = 0u32;
            while flip::pending(&w) && waited_us < 200_000 {
                bus::manager().delay_us(100);
                waited_us += 100;
            }
            self.log(format_args!(
                "flip {what}: {}; surface now {:#x}; waited {waited_us} us",
                if flip::pending(&w) {
                    "STILL PENDING"
                } else {
                    "taken"
                },
                flip::surface(&w)
            ));
            if what == "to the pattern" {
                for _ in 0..3000 {
                    bus::manager().delay_us(1000);
                }
            }
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
        let card = match crate::kms::card(&gpu, Arc::clone(&self.irq), self.me.clone()) {
            Ok(c) => c,
            Err(e) => {
                self.log(format_args!("no graphics card: {e:?}"));
                return;
            }
        };
        let node = match drm::register(card.clone()) {
            Ok(n) => n,
            Err(e) => {
                self.log(format_args!("graphics card not registered: {e:?}"));
                return;
            }
        };
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

    /// Counts the timing generator's frames over three seconds and logs the
    /// refresh rate they make, to compare with the mode's (the core's frame
    /// timer runs at the mode's rate, since the card has no vertical
    /// blanking interrupt yet).
    pub(crate) fn refresh(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::flip;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let first = flip::frame_count(&w);
        let started = sched::now_ns();
        for _ in 0..3000 {
            bus::manager().delay_us(1000);
        }
        let second = flip::frame_count(&w);
        let micros = sched::now_ns().saturating_sub(started) / 1000;
        let frames = flip::frames_between(first, second);
        match flip::refresh_millihz(frames, micros) {
            Some(mhz) => self.log(format_args!(
                "refresh: {frames} frames in {micros} us is {}.{:03} Hz",
                mhz / 1000,
                mhz % 1000
            )),
            None => self.log(format_args!("refresh: no time passed")),
        }
        Ok(())
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

    /// Moves the interrupt handler's read pointer to the doorbell and checks
    /// the register shows what was written (stage R2.4).
    pub(crate) fn doorbells(&self) -> Result<(), abi::errno::Errno> {
        match self.irq.use_doorbell() {
            Ok((held, shown)) => {
                self.log(format_args!(
                    "doorbells: the interrupt handler's read pointer {held:#x}, its register {shown:#x} ({})",
                    if held == shown { "agree" } else { "DIFFER" }
                ));
                Ok(())
            }
            Err(e) => {
                self.log(format_args!("doorbells not started: {e:?}"));
                Err(e)
            }
        }
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

    /// Starts graphics ring 0 and runs the ring test (stages R4.2 to R4.4);
    /// the engines' firmware must be loaded (`gc_fw`). Run again, it only
    /// repeats the test.
    pub(crate) fn gc_ring(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        let log = |args: core::fmt::Arguments<'_>| self.log(args);
        let mut slot = self.gfx.lock().take();
        if slot.is_none() {
            match crate::gcring::start(&gpu, &log) {
                Ok(g) => slot = Some(g),
                Err(e) => {
                    self.log(format_args!("GC: ring not started: {e:?}"));
                    return Err(Errno::EIO);
                }
            }
        }
        let ok = slot
            .as_mut()
            .is_some_and(|g| crate::gcring::test(&gpu, g, &log));
        *self.gfx.lock() = slot;
        if ok { Ok(()) } else { Err(Errno::EIO) }
    }

    /// Logs the graphics block's state (reads only; see `gcload::snapshot`).
    pub(crate) fn gc_probe(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        let log = |args: core::fmt::Arguments<'_>| self.log(args);
        crate::gcload::snapshot(&gpu, "probe", &log);
        Ok(())
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

    /// The audio side of HDMI (stage R3.4, steps 1 to 3 of 08 section 4.6):
    /// the encoder's audio setup, the endpoint, the audio clock. The picture
    /// is not touched.
    pub(crate) fn hdmi_prepare(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::hdmi;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        self.hdmi_watch(&w, "before the audio preparation");
        hdmi::audio_encoder(&w, 0);
        self.log(format_args!("HDMI: encoder audio setup done"));
        hdmi::endpoint_configure(&w);
        self.log(format_args!("HDMI: endpoint 0 configured"));
        hdmi::audio_clock(&w);
        self.log(format_args!("HDMI: audio clock set (148.5 MHz)"));
        self.hdmi_prepared
            .store(true, core::sync::atomic::Ordering::Release);
        self.hdmi_watch(&w, "after the audio preparation");
        Ok(())
    }

    /// The video switch to HDMI mode, the AVI info frame, the packet
    /// controls, the endpoint's enable and the unmute (steps 4 to 7). This
    /// disturbs the stream while the encoder's FIFO resets.
    pub(crate) fn hdmi_enable(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::{flip, hdmi};
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        if !self
            .hdmi_prepared
            .load(core::sync::atomic::Ordering::Acquire)
        {
            self.hdmi_prepare()?;
        }
        let pause = |us: u32| bus::manager().delay_us(u64::from(us));
        self.hdmi_watch(&w, "before the switch to HDMI mode");
        hdmi::video_mode(&w, &pause);
        self.log(format_args!(
            "HDMI: encoder in HDMI mode (frames {})",
            flip::frame_count(&w)
        ));
        let clear = hdmi::avi_info_frame(&w, &pause);
        self.log(format_args!(
            "HDMI: AVI info frame written (no conflict: {clear})"
        ));
        hdmi::packet_controls(&w);
        self.log(format_args!("HDMI: packet controls set"));
        bus::manager().delay_us(50_000);
        self.log(format_args!(
            "HDMI: 50 ms later: frames {}",
            flip::frame_count(&w)
        ));
        hdmi::endpoint_enable(&w);
        self.log(format_args!("HDMI: endpoint 0 audio enabled"));
        hdmi::unmute(&w);
        bus::manager().delay_us(50_000);
        self.hdmi_watch(&w, "after the switch, the enable and the unmute");
        Ok(())
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

    /// The modes the card offers (CTA video identification code, timing),
    /// the firmware's first and preferred. 640x480 has the specification's
    /// 25.175 MHz clock (the shared table rounds it to 25.2).
    pub(crate) fn supported_modes() -> alloc::vec::Vec<(u8, kms_check::Mode)> {
        use abi::drm::DRM_MODE_TYPE_PREFERRED;
        [16u8, 31, 4, 19, 3, 18, 1]
            .into_iter()
            .filter_map(|vic| {
                let mut m = edid::mode_for_vic(vic)?;
                if vic == 1 {
                    m.clock_khz = 25_175;
                }
                if vic == 16 {
                    m.kind |= DRM_MODE_TYPE_PREFERRED;
                }
                Some((vic, m))
            })
            .collect()
    }

    /// Sets the mode `mode` (one of [`Amd::supported_modes`]), or the
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
            Some(m) => Self::supported_modes()
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
        let wanted = Self::supported_modes()
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

    /// The trigger: [`Amd::link_reconnect_now`] on demand.
    pub(crate) fn link_reconnect(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        self.link_reconnect_now(&Bar5(&gpu), "on request");
        Ok(())
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

    /// Reads what a mode change at another pixel clock or size depends on
    /// (08 section 3.7), writing nothing: the size holders, the locks, the
    /// generator's state, the pixel-clock registers, and the first words of
    /// the platform's copy of the video BIOS (in video memory, through the
    /// register window).
    pub(crate) fn mode_probe(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::regs::{self, Window};
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        for (name, at) in [
            ("HUBP0_DCHUBP_CNTL", 0xeacc),
            ("HUBP0_DCSURF_PRI_VIEWPORT_START", 0xeaa4),
            ("HUBP0_DCSURF_PRI_VIEWPORT_DIMENSION", 0xeaa8),
            ("DSCL0_SCL_MODE", 0x10700),
            ("DSCL0_SCL_HORZ_FILTER_SCALE_RATIO", 0x10704),
            ("DSCL0 0x10708", 0x10708),
            ("DSCL0 0x1070c", 0x1070c),
            ("DSCL0 0x10710", 0x10710),
            ("DSCL0_OTG_H_BLANK", 0x1073c),
            ("DSCL0_OTG_V_BLANK", 0x10740),
            ("DSCL0_RECOUT_START", 0x10744),
            ("DSCL0_RECOUT_SIZE", 0x10748),
            ("DSCL0_MPC_SIZE", 0x1074c),
            ("DPG0_DPG_CONTROL", 0x13450),
            ("DPG0_DPG_DIMENSIONS", 0x13458),
            ("OPPBUF0_OPPBUF_CONTROL", 0x13510),
            ("ODM0_OPTC_DATA_SOURCE_SELECT", 0x13e2c),
            ("ODM0_OPTC_WIDTH_CONTROL", 0x13e38),
            ("ODM0_OPTC_MEMORY_CONFIG", 0x13e40),
            ("OTG0_OTG_CONTROL", 0x14004),
            ("OTG0_OTG_DOUBLE_BUFFER_CONTROL", 0x1406c),
            ("OTG0_OTG_CLOCK_CONTROL", 0x14118),
            ("OTG0_OTG_MASTER_UPDATE_LOCK", 0x1412c),
            ("OTG0_OTG_GLOBAL_CONTROL0", 0x14140),
            ("VTG0_CONTROL", 0xe7b4),
            ("PHYPLLA_PIXCLK_RESYNC_CNTL", 0x00400),
            ("OTG0_PIXEL_RATE_CNTL", 0x00500),
            ("DIG0_DIG_BE_CNTL", 0x155c0),
            ("BIOS_SCRATCH_6", 0x148),
            ("DSCL0_SCL_MODE (0x106ec)", 0x106ec),
            ("DSCL0_SCL_TAP_CONTROL (0x106f0)", 0x106f0),
        ] {
            self.log(format_args!(
                "MODE: {name} @{at:#07x} = {:#010x}",
                w.read32(at)
            ));
        }
        // The platform's windows 3 to 6 (08 section 9.0.1) are at the top of
        // video memory: read the first words of window 3, the video BIOS copy.
        let base = 0x1fe2_0000u64;
        let mut line = alloc::string::String::new();
        for i in 0..8u64 {
            use core::fmt::Write as _;
            let _ = write!(line, "{:08x} ", regs::read_vram(&w, base + 4 * i));
        }
        self.log(format_args!(
            "MODE: video memory {base:#x} (the platform's video BIOS copy): {line}"
        ));
        Ok(())
    }

    /// Reads the DDC engine's and pins' registers, writing nothing    /// Reads the DDC engine's and pins' registers, writing nothing (stage
    /// R3.4: the EDID over the HDMI connector's DDC line).
    pub(crate) fn ddc_probe(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::ddc;
        use amdgpu::regs::Window;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let g = ddc::gpio(0);
        for (name, at) in [
            ("MICROSECOND_TIME_BASE_DIV", ddc::TIME_BASE_DIV),
            ("DC_I2C_CONTROL", ddc::CONTROL),
            ("DC_I2C_ARBITRATION", ddc::ARBITRATION),
            ("DC_I2C_SW_STATUS", ddc::SW_STATUS),
            ("DDC1_HW_STATUS", ddc::HW_STATUS),
            ("DDC1_SPEED", ddc::SPEED),
            ("DDC1_SETUP", ddc::SETUP),
            ("DC_GPIO_DDC1_MASK", g),
            ("DC_GPIO_DDC1_A", g + 4),
            ("DC_GPIO_DDC1_EN", g + 8),
            ("DC_GPIO_DDC1_Y", g + 12),
            ("DDC_PAD1_I2CMODE", ddc::PAD_I2C_MODE),
            ("AUX1_PAD_RXSEL", ddc::PAD_RXSEL),
            ("memory light sleep 0", ddc::LIGHT_SLEEP[0]),
            ("memory light sleep 1", ddc::LIGHT_SLEEP[1]),
        ] {
            self.log(format_args!(
                "DDC: {name} @{at:#07x} = {:#010x}",
                w.read32(at)
            ));
        }
        Ok(())
    }

    /// Reads the HDMI monitor's EDID over DDC line 0 with the hardware I2C
    /// engine, logs it and what it says, and gives the audio endpoint its
    /// audio capabilities.
    pub(crate) fn edid_read(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::regs::Window;
        use amdgpu::{ddc, hdmi};
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let pause = |us: u32| bus::manager().delay_us(u64::from(us));
        // The spec's prescale for 100 kHz first, then the platform's own
        // (the time base differs from the one the spec's prescale is for).
        let platform = w.read32(ddc::SPEED) >> 16;
        self.log(format_args!(
            "EDID: time base {:#x}, platform prescale {platform}",
            w.read32(ddc::TIME_BASE_DIV)
        ));
        let mut result = Err(ddc::Error::Busy);
        for prescale in [ddc::PRESCALE, platform] {
            result = ddc::read_edid(&w, 0, 4, prescale, &pause);
            match &result {
                Ok(_) => {
                    self.log(format_args!("EDID: read with prescale {prescale}"));
                    break;
                }
                Err(e) => self.log(format_args!("EDID: prescale {prescale}: {e:?}")),
            }
        }
        let Ok(raw) = result else {
            return Err(abi::errno::Errno::EIO);
        };
        self.log(format_args!("EDID: {} bytes:", raw.len()));
        for (i, line) in raw.chunks(32).enumerate() {
            let mut text = alloc::string::String::new();
            for b in line {
                use core::fmt::Write as _;
                let _ = write!(text, "{b:02x}");
            }
            self.log(format_args!("EDID: {:03x}: {text}", i * 32));
        }
        match edid::parse(&raw) {
            Ok(e) => {
                self.log(format_args!(
                    "EDID: name {:?}, manufacturer {:#x}, product {:#x}, {} extension block(s), {} mode(s), speakers {:#x}, basic audio {}",
                    e.name,
                    e.manufacturer,
                    e.product,
                    e.extensions,
                    e.modes.len(),
                    e.audio.speakers,
                    e.audio.basic
                ));
                for (format, d) in e.audio.descriptors.iter().enumerate().skip(1) {
                    if *d != [0; 3] {
                        self.log(format_args!("EDID: audio format {format}: {d:02x?}"));
                    }
                }
                hdmi::endpoint_configure_for(&w, &e);
                self.log(format_args!("EDID: endpoint 0 configured from it"));
                Ok(())
            }
            Err(e) => {
                self.log(format_args!("EDID: not valid: {e:?}"));
                Err(abi::errno::Errno::EINVAL)
            }
        }
    }

    /// The reverse of the audio enable: mute, then clear the endpoint.
    pub(crate) fn hdmi_off(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        amdgpu::hdmi::mute_and_disable(&Bar5(&gpu));
        self.log(format_args!("HDMI: audio muted and the endpoint cleared"));
        Ok(())
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

/// The sysctls for `a`, under `node`.
pub(crate) fn publish(t: &mut sysctl::Tree, node: sysctl::NodeId, a: &Arc<Amd>) {
    let r = Arc::clone(a);
    let _ = t.add_value(
        node,
        sysctl::Number::Auto,
        "discovered",
        "The IP block versions, once `discover` has run",
        sysctl::Value::Str(Box::new(move |out| out.push_str(&r.summary()))),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "discover",
        "Write 1 to read the IP discovery table and log it",
        move || r.discover(),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "vram",
        "Write 1 to read a few words of VRAM through BAR0 and log them (read-only)",
        move || r.vram_probe(),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "refresh",
        "Write 1 to count frames for three seconds and log the refresh rate they make (read-only)",
        move || r.refresh(),
    );
    crate::control::trigger(
        t,
        node,
        "panic_restore",
        "Write 1 to run the kernel's panic hook for graphics cards (shows the console's surface), to test it",
        || {
            drm::panic_restore();
            Ok(())
        },
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "flip",
        "Write 1 to show a test pattern from video memory for three seconds, then restore the firmware's picture",
        move || r.flip_test(),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "gmc",
        "Write 1 to set the graphics hub up: the GART's table in video memory, the apertures, context 0, an invalidation",
        move || r.gmc(amdgpu::gmc::Hub::Gfx),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "gmc_mm",
        "Write 1 to set the memory hub up (the display's and the interrupt handler's): as `gmc`",
        move || r.gmc(amdgpu::gmc::Hub::Mm),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "psp",
        "Write 1 to bring the security processor up: the ring, the table of contents, the trusted memory zone (on a kernel thread; see the log)",
        move || {
            let r = Arc::clone(&r);
            sched::spawn("amd-psp", sched::Priority::KERNEL, move |s| {
                r.psp_bring_up(s)
            })
            .map(|_| ())
            .map_err(|_| abi::errno::Errno::EAGAIN)
        },
    );
    let r = Arc::clone(a);
    crate::control::trigger_with(
        t,
        node,
        "mode",
        "Write a CTA video identification code (16 = 1080p60, 31 = 1080p50) to change the mode at the firmware's pixel clock; the picture goes while the generator restarts",
        move |vic| r.set_mode(vic),
    );
    for (name, help, f) in [
        (
            "hdmi_audio",
            "Write 1 to prepare the HDMI audio side (encoder, endpoint, clock); the picture is not touched",
            Amd::hdmi_prepare as fn(&Amd) -> Result<(), abi::errno::Errno>,
        ),
        (
            "hdmi",
            "Write 1 to switch the encoder to HDMI mode with the AVI info frame and enable audio; disturbs the picture briefly",
            Amd::hdmi_enable,
        ),
        (
            "link_reconnect",
            "Write 1 to put the encoder's front end back as before the last clock change (an experiment on a lost signal)",
            Amd::link_reconnect,
        ),
        (
            "gc_ring",
            "Write 1 to start graphics ring 0 and run the ring test (after gc_fw); again, only the test",
            Amd::gc_ring,
        ),
        (
            "gc_probe",
            "Write 1 to log the graphics block's power state and, if it is on, its registers (reads only)",
            Amd::gc_probe,
        ),
        (
            "mode_probe",
            "Write 1 to log the registers a mode change at another clock or size depends on (reads only)",
            Amd::mode_probe,
        ),
        (
            "ddc_probe",
            "Write 1 to log the DDC engine's and pins' registers (reads only)",
            Amd::ddc_probe,
        ),
        (
            "edid",
            "Write 1 to read the HDMI monitor's EDID over DDC and give the audio endpoint its capabilities",
            Amd::edid_read,
        ),
        (
            "hdmi_off",
            "Write 1 to mute the HDMI audio and clear the endpoint",
            Amd::hdmi_off,
        ),
    ] {
        let r = Arc::clone(a);
        crate::control::trigger(t, node, name, help, move || f(&r));
    }
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "gc_fw",
        "Write 1 to hand the graphics, compute and DMA engines' firmware to the security processor and start the RLC autoload (on a kernel thread; see the log)",
        move || {
            let r = Arc::clone(&r);
            sched::spawn("amd-gc", sched::Priority::KERNEL, move |s| r.gc_load(s))
                .map(|_| ())
                .map_err(|_| abi::errno::Errno::EAGAIN)
        },
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "dmcub",
        "Write 1 to load the display microcontroller's firmware through the security processor (on a kernel thread; see the log)",
        move || {
            let r = Arc::clone(&r);
            sched::spawn("amd-dmcub", sched::Priority::KERNEL, move |s| {
                r.dmcub_load(s)
            })
            .map(|_| ())
            .map_err(|_| abi::errno::Errno::EAGAIN)
        },
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "doorbells",
        "Write 1 to enable the doorbell aperture and publish the interrupt handler's read pointer through its doorbell",
        move || r.doorbells(),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "interrupts",
        "Write 1 to make the interrupt handler's ring and turn the frame-start interrupt on",
        move || r.start_interrupts(),
    );
    let r = Arc::clone(a);
    let _ = t.add_value(
        node,
        sysctl::Number::Auto,
        "irq",
        "What the interrupts have done",
        sysctl::Value::Str(Box::new(move |out| out.push_str(&r.irq.summary()))),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "memory",
        "Write 1 to read the memory, interrupt and doorbell registers and log them (read-only)",
        move || r.memory(),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "display",
        "Write 1 to read the display registers and log them (read-only)",
        move || r.display(),
    );
}
