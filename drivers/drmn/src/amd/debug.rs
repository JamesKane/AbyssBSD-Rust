// SPDX-License-Identifier: BSD-2-Clause
//! The AMD card's bring-up and probe triggers: one-shot sysctls (`discover`,
//! `psp`, `gc_fw`, `gc_ring`, `gc_probe`, `mode`, ...) that start a stage of
//! the device by hand or log its registers, and the code behind them. The
//! production path (attach, the graphics card, the render node) does not
//! need them; they are published unless the tunable `hw.drmn.debug` is 0.
//! Their names are stable: board scripts use them.

use alloc::sync::Arc;

use core::sync::atomic::Ordering;

use super::{Amd, Bar5};
use amdgpu::{display, regs};

impl Amd {
    /// Resets the whole chip and builds everything again (see `reset.rs`),
    /// on a kernel thread: the log says how it went. A hazard for the
    /// display.
    pub(crate) fn chip_reset_trigger(&self) -> Result<(), abi::errno::Errno> {
        let me = self.me.upgrade().ok_or(abi::errno::Errno::ENXIO)?;
        sched::spawn("amd-reset", sched::Priority::KERNEL, move |s| {
            let r = me.chip_reset(s);
            me.log(format_args!("chip reset: finished: {r:?}"));
        })
        .map(|_| ())
        .map_err(|_| abi::errno::Errno::EAGAIN)
    }

    /// [`Amd::chip_reset_trigger`] without the video BIOS's initialisation
    /// (to see what that changes).
    pub(crate) fn chip_reset_no_init_trigger(&self) -> Result<(), abi::errno::Errno> {
        self.skip_asic_init.store(true, Ordering::Release);
        self.chip_reset_trigger()
    }

    /// Runs the video BIOS's ASIC-init table on the running chip (the table
    /// is for a chip that was just reset): left as a way to test the
    /// interpreter on hardware; it writes a few registers.
    pub(crate) fn asic_init_trigger(&self) -> Result<(), abi::errno::Errno> {
        self.asic_init()
    }

    /// Reads the audio path's registers (the encoder's audio formatter, the
    /// packet and clock-regeneration controls, the audio clock, endpoint 0)
    /// and logs them: reads only. Run it while a tone plays.
    pub(crate) fn afmt_probe(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::hdmi;
        use amdgpu::regs::Window as _;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        for (name, at) in hdmi::WATCH {
            self.log(format_args!(
                "AFMT: {name} @{at:#07x} = {:#010x}",
                w.read32(*at)
            ));
        }
        for (name, at) in [
            (
                "AFMT_AUDIO_PACKET_CONTROL2",
                hdmi::AFMT_AUDIO_PACKET_CONTROL2,
            ),
            ("AFMT_60958_0", hdmi::AFMT_60958_0),
            ("AFMT_60958_1", hdmi::AFMT_60958_1),
            ("AFMT_AUDIO_INFO0", 0x154d8),
            ("AFMT_AUDIO_INFO1", 0x154dc),
            ("AFMT_INFOFRAME_CONTROL0", 0x1550c),
            ("AFMT_MEM_PWR", hdmi::AFMT_MEM_PWR),
            ("HDMI_AUDIO_PACKET_CONTROL", hdmi::AUDIO_PACKET_CONTROL),
            ("HDMI_INFOFRAME_CONTROL0", hdmi::INFOFRAME_CONTROL0),
            ("HDMI_INFOFRAME_CONTROL1", hdmi::INFOFRAME_CONTROL1),
            ("HDMI_ACR_32_0", hdmi::ACR_32.0),
            ("HDMI_ACR_32_1", hdmi::ACR_32.1),
            ("HDMI_ACR_44_0", hdmi::ACR_44_1.0),
            ("HDMI_ACR_44_1", hdmi::ACR_44_1.1),
            ("HDMI_ACR_48_0", hdmi::ACR_48.0),
            ("HDMI_ACR_48_1", hdmi::ACR_48.1),
        ] {
            self.log(format_args!(
                "AFMT: {name} @{at:#07x} = {:#010x}",
                w.read32(at)
            ));
        }
        for (name, index) in [
            ("endpoint channel/speaker", hdmi::EP_CHANNEL_SPEAKER),
            ("endpoint hot-plug", hdmi::EP_HOTPLUG),
            ("endpoint multichannel", 0x36),
            ("endpoint response configuration", 0x56),
        ] {
            self.log(format_args!(
                "AFMT: {name} (index {index:#x}) = {:#010x}",
                hdmi::endpoint_read(&w, index)
            ));
        }
        Ok(())
    }

    /// Reads the firmware's metrics table (04 section 3.3: clocks, activity,
    /// power, the limits that hold it down) into a page of video memory and
    /// logs it: harmless, the way to see what the chip is running at.
    pub(crate) fn smu_metrics(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::psp::Memory as _;
        use amdgpu::regs::Window as _;
        use amdgpu::smu;
        /// Where the table goes: a video memory offset nothing else uses.
        const AT: usize = 200 << 20;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let pause = |us: u32| bus::manager().delay_us(u64::from(us));
        let base = u64::from(w.read32(amdgpu::flip::FB_LOCATION_BASE) & 0xffff) << 24;
        let address = base + AT as u64;
        let send = |m: u32, a: u32| smu::send(&w, m, a, 2_000_000, &pause);
        let mut r = send(
            smu::message::SET_DRIVER_TABLE_ADDRESS_HIGH,
            u32::try_from(address >> 32).unwrap_or(0),
        );
        if r.is_ok() {
            r = send(
                smu::message::SET_DRIVER_TABLE_ADDRESS_LOW,
                u32::try_from(address & 0xffff_ffff).unwrap_or(0),
            );
        }
        if r.is_ok() {
            r = send(smu::message::TRANSFER_TABLE_TO_DRAM, smu::METRICS_TABLE);
        }
        if let Err(e) = r {
            self.log(format_args!("SMU: metrics: {e:?}"));
            return Err(abi::errno::Errno::EIO);
        }
        let mem = crate::psp::vram_memory(&gpu, base);
        mem.invalidate();
        let mut b = [0u8; smu::METRICS_BYTES];
        for (i, c) in b.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            c.copy_from_slice(&mem.read32(address + 4 * i as u64).to_le_bytes());
        }
        match smu::Metrics::decode(&b) {
            Some(m) => self.log(format_args!(
                "SMU: metrics: graphics {} MHz, SOC {} MHz, memory {} MHz, activity {}.{:02}%, VDD/SOC {}/{} mV, power {}/{} mW, socket {} mW, throttler {:#x}",
                m.graphics_mhz, m.soc_mhz, m.memory_mhz,
                m.graphics_activity / 100, m.graphics_activity % 100,
                m.voltage_mv[0], m.voltage_mv[1], m.power_mw[0], m.power_mw[1],
                m.socket_power_mw, m.throttler
            )),
            None => self.log(format_args!("SMU: metrics: short")),
        }
        Ok(())
    }

    /// Sends the power firmware's "disallow GFXOFF entry" (23) or "allow"
    /// (22) message (04 section 4): whether the graphics block may power
    /// itself off when idle. A test of what a reset leaves it at.
    pub(crate) fn smu_gfxoff(&self, allow: bool) -> Result<(), abi::errno::Errno> {
        use amdgpu::smu;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let pause = |us: u32| bus::manager().delay_us(u64::from(us));
        let m = if allow {
            smu::message::ALLOW_GFXOFF_ENTRY
        } else {
            smu::message::DISALLOW_GFXOFF_ENTRY
        };
        let r = smu::send(&w, m, 0, 2_000_000, &pause);
        self.log(format_args!(
            "SMU: GFXOFF {}: {r:?}",
            if allow { "allowed" } else { "disallowed" }
        ));
        r.map(|_| ()).map_err(|_| abi::errno::Errno::EIO)
    }

    /// Asks the power management firmware two harmless questions (its
    /// version and its driver-interface version), and logs whether it is
    /// running: the check of the mailbox's address (04 section 1).
    pub(crate) fn smu_probe(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::smu;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let pause = |us: u32| bus::manager().delay_us(u64::from(us));
        self.log(format_args!(
            "SMU: firmware flags {:#x} (running: {})",
            smu::read(&w, smu::FIRMWARE_FLAGS),
            smu::running(&w)
        ));
        let mut ok = true;
        for (name, m) in [
            ("version", smu::message::GET_VERSION),
            (
                "driver interface",
                smu::message::GET_DRIVER_INTERFACE_VERSION,
            ),
            ("graphics clock (MHz)", smu::message::GET_GFX_CLOCK),
        ] {
            match smu::send(&w, m, 0, 2_000_000, &pause) {
                Ok(v) => {
                    let (program, major, minor, debug) = smu::version_fields(v);
                    self.log(format_args!(
                        "SMU: {name} {v:#x} (as a version: {program}.{major}.{minor}.{debug})"
                    ));
                }
                Err(e) => {
                    ok = false;
                    self.log(format_args!("SMU: {name}: {e:?}"));
                }
            }
        }
        // The features the firmware runs (04 section 3.4): a 64-bit mask in two
        // answers, low then high. 8, 9, 10, 31, 36, 41, 44, 45 and 46 are the
        // clock scaling ones.
        let low = smu::send(&w, smu::message::GET_ENABLED_FEATURES, 0, 2_000_000, &pause);
        let high = smu::send(&w, smu::message::GET_ENABLED_FEATURES, 1, 2_000_000, &pause);
        if let (Ok(l), Ok(h)) = (low, high) {
            self.log(format_args!(
                "SMU: features {:#018x}",
                (u64::from(h) << 32) | u64::from(l)
            ));
        }
        if ok {
            Ok(())
        } else {
            Err(abi::errno::Errno::EIO)
        }
    }

    /// Logs the platform's video BIOS in hex, 64 bytes a line (to keep a
    /// copy for the interpreter's tests; reads only).
    pub(crate) fn vbios_dump(&self) -> Result<(), abi::errno::Errno> {
        let Some(image) = self.vbios_image() else {
            self.log(format_args!("VBIOS: no image"));
            return Err(abi::errno::Errno::ENXIO);
        };
        self.log(format_args!("VBIOS: {} bytes", image.len()));
        for (i, line) in image.chunks(64).enumerate() {
            let mut text = alloc::string::String::new();
            for b in line {
                use core::fmt::Write as _;
                let _ = write!(text, "{b:02x}");
            }
            self.log(format_args!("VBIOS: {:05x}: {text}", i * 64));
        }
        Ok(())
    }

    /// Preempts the DMA engine's queue and lets it run again (07 section 8),
    /// and logs whether its fence appeared.
    pub(crate) fn sdma_preempt(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        let mut g = self.sdma_lock();
        let sdma = g.as_mut().ok_or(abi::errno::Errno::ENXIO)?;
        let ok = sdma.preempt(&gpu, 0x7e57_d11a);
        self.log(format_args!(
            "SDMA: preempt: the fence was {}",
            if ok { "written" } else { "NOT written" }
        ));
        if ok {
            Ok(())
        } else {
            Err(abi::errno::Errno::EIO)
        }
    }

    /// Preempts each compute queue without unmapping it (10 section 4.3,
    /// action 3) and logs whether the processor wrote the queue's fence
    /// word: the preemption primitive, run on idle queues.
    pub(crate) fn gc_preempt(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        let mut g = self.gfx_lock();
        let gfx = g.as_mut().ok_or(abi::errno::Errno::ENXIO)?;
        let mut all = true;
        for n in 0..gfx.compute_rings() {
            let value = 0x7e57_0000 | u32::try_from(n).unwrap_or(0);
            match gfx.preempt_compute(&gpu, n, value) {
                Ok(true) => self.log(format_args!(
                    "GC: compute queue {n} preempted (fence {value:#x})"
                )),
                Ok(false) => {
                    all = false;
                    self.log(format_args!(
                        "GC: compute queue {n}: the preempt fence was NOT written"
                    ));
                }
                Err(e) => {
                    all = false;
                    self.log(format_args!(
                        "GC: compute queue {n}: preempt stopped: {e:?}"
                    ));
                }
            }
            // A preempted queue does not run again by itself: reset it.
            let log = |args: core::fmt::Arguments<'_>| self.log(args);
            if let Err(e) = gfx.reset_compute(&gpu, n, &log) {
                self.log(format_args!(
                    "GC: compute queue {n}: reset after it stopped: {e:?}"
                ));
            }
        }
        if all {
            Ok(())
        } else {
            Err(abi::errno::Errno::EIO)
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

    /// Logs the translation hubs' fault status and what they were set to
    /// (reads only), to find why the engine did not read an address.
    pub(crate) fn gc_fault(&self) -> Result<(), abi::errno::Errno> {
        use amdgpu::gmc::{Hub, Reg};
        use amdgpu::regs::Window;
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        for (name, hub, addr_at) in [
            ("GFXHUB", Hub::Gfx, 0xa0a4usize),
            ("MMHUB", Hub::Mm, 0x69c34),
        ] {
            let st = w.read32(hub.reg(Reg::FaultStatus));
            self.log(format_args!(
                "FAULT: {name}: status {st:#010x} (client {}, vmid {}, write {}, mapping error {}, permission {:#x}, walker {}), address {:#x}{:08x}",
                (st >> 9) & 0x1ff,
                (st >> 20) & 0xf,
                (st >> 18) & 1,
                (st >> 8) & 1,
                (st >> 4) & 0xf,
                (st >> 1) & 7,
                w.read32(addr_at + 4) & 0xf,
                w.read32(addr_at)
            ));
            for (n, r) in [
                ("FB_LOCATION_BASE", Reg::FbLocationBase),
                ("FB_LOCATION_TOP", Reg::FbLocationTop),
                ("FB_OFFSET", Reg::FbOffset),
                ("APERTURE_LOW", Reg::ApertureLow),
                ("APERTURE_HIGH", Reg::ApertureHigh),
                ("L1_TLB_CNTL", Reg::L1Tlb),
                ("L2_CNTL", Reg::L2Cntl),
                ("FAULT_CNTL", Reg::FaultCntl),
            ] {
                self.log(format_args!(
                    "  {name} {n} @{:#07x} = {:#010x}",
                    hub.reg(r),
                    w.read32(hub.reg(r))
                ));
            }
            self.log(format_args!(
                "  {name} CONTEXT0_CNTL @{:#07x} = {:#010x}",
                hub.context_cntl(0),
                w.read32(hub.context_cntl(0))
            ));
            for (a, b) in hub.context_table(0) {
                self.log(format_args!(
                    "  {name} context 0 registers @{a:#07x}/{b:#07x} = {:#010x}/{:#010x}",
                    w.read32(a),
                    w.read32(b)
                ));
            }
        }
        Ok(())
    }

    /// Runs an indirect buffer from a virtual address in VMID 1 on the
    /// started graphics ring (stage R4.3); `gc_ring` first.
    pub(crate) fn gc_vm(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        let log = |args: core::fmt::Arguments<'_>| self.log(args);
        let mut g = self.gfx.lock().take().ok_or(Errno::ENXIO)?;
        let ok = crate::gcring::vm_test(&gpu, &mut g, &log);
        *self.gfx.lock() = Some(g);
        if ok { Ok(()) } else { Err(Errno::EIO) }
    }

    /// Logs the graphics ring's position and the packets around it.
    pub(crate) fn gc_hang(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        let log = |args: core::fmt::Arguments<'_>| self.log(args);
        let g = self.gfx.lock();
        g.as_ref()
            .ok_or(abi::errno::Errno::ENXIO)?
            .hang_report(&gpu, &log);
        if let Some(d) = self.sdma.lock().as_ref() {
            d.report(&gpu, &log);
        }
        Ok(())
    }

    /// Logs the graphics block's state (reads only; see `gcload::snapshot`).
    pub(crate) fn gc_probe(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        let log = |args: core::fmt::Arguments<'_>| self.log(args);
        crate::gcload::snapshot(&gpu, "probe", &log);
        Ok(())
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

    /// The trigger: [`Amd::link_reconnect_now`] on demand.
    pub(crate) fn link_reconnect(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        self.link_reconnect_now(&Bar5(&gpu), "on request");
        Ok(())
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
}

/// The triggers.
pub(super) fn publish(t: &mut sysctl::Tree, node: sysctl::NodeId, a: &Arc<Amd>) {
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
            "sdma_preempt",
            "Write 1 to preempt the DMA engine's queue and let it run again, and log whether its fence was written",
            Amd::sdma_preempt,
        ),
        (
            "chip_reset",
            "Write 1 to reset the whole chip through the power firmware and build everything again (on a kernel thread; see the log). The display may go",
            Amd::chip_reset_trigger,
        ),
        (
            "chip_reset_no_init",
            "Write 1 to reset the chip as chip_reset does, without the video BIOS's initialisation table (a test)",
            Amd::chip_reset_no_init_trigger,
        ),
        (
            "asic_init",
            "Write 1 to run the video BIOS's initialisation table on the running chip (it writes a few registers)",
            Amd::asic_init_trigger,
        ),
        (
            "afmt_probe",
            "Write 1 to log the audio path's registers (formatter, packets, clock, endpoint 0): reads only; run it while a tone plays",
            Amd::afmt_probe,
        ),
        (
            "smu_metrics",
            "Write 1 to read the power firmware's metrics (clocks, activity, power, limits) and log them (harmless)",
            Amd::smu_metrics,
        ),
        (
            "smu_gfxoff_disallow",
            "Write 1 to tell the power firmware the graphics block may not power itself off when idle",
            |a: &Amd| a.smu_gfxoff(false),
        ),
        (
            "smu_gfxoff_allow",
            "Write 1 to tell the power firmware the graphics block may power itself off when idle",
            |a: &Amd| a.smu_gfxoff(true),
        ),
        (
            "smu_probe",
            "Write 1 to ask the power management firmware for its version and log it (harmless messages; checks the mailbox's address)",
            Amd::smu_probe,
        ),
        (
            "vbios_dump",
            "Write 1 to log the platform's video BIOS in hex (reads only)",
            Amd::vbios_dump,
        ),
        (
            "gc_preempt",
            "Write 1 to preempt each compute queue without unmapping it, log whether the fence was written, then reset the queue (a preempted queue does not run again by itself)",
            Amd::gc_preempt,
        ),
        (
            "gc_hang",
            "Write 1 to log the graphics ring's pointers and the packets around its read pointer (reads only)",
            Amd::gc_hang,
        ),
        (
            "gc_fault",
            "Write 1 to log the translation hubs' fault status and set-up (reads only)",
            Amd::gc_fault,
        ),
        (
            "gc_vm",
            "Write 1 to run an indirect buffer at a virtual address in VMID 1 on the graphics ring (after gc_ring)",
            Amd::gc_vm,
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
