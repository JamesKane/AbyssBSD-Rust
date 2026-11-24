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
}

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
    pub(crate) fn new(name: String, gpu: Weak<Aperture>) -> Self {
        Self {
            name,
            gpu,
            table: SpinLock::new(None),
            hdp_ready: core::sync::atomic::AtomicBool::new(false),
            kms: SpinLock::new(None),
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
        let card = match crate::kms::card(&gpu) {
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
        *self.kms.lock() = Some((card, node));
    }

    /// Takes the card away and shows the firmware's picture again.
    pub(crate) fn unregister_kms(&self, s: &sched::Sleepable) {
        let held = self.kms.lock().take();
        if let Some((card, node)) = held {
            card.restore();
            node.unregister(s);
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
        "display",
        "Write 1 to read the display registers and log them (read-only)",
        move || r.display(),
    );
}
