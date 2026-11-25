// SPDX-License-Identifier: BSD-2-Clause
//! The full chip reset (specification 10 sections 3, 5 and 8): for a chip
//! whose rings cannot be reset one by one. The power management firmware is
//! asked for a "mode 2" reset of the graphics block; the configuration space
//! the reset clears is put back; the video BIOS's initialisation table runs;
//! then everything the driver built on the chip is built again in the
//! order the specification gives: the translation hubs and the interrupt
//! handler, the security processor, the engines' firmware, the graphics ring
//! and the DMA engine. The jobs that were in flight are completed (their
//! fences are written) and every context reads that a reset happened.

use core::sync::atomic::Ordering;

use abi::errno::Errno;
use amdgpu::regs::Window;
use amdgpu::vbios::Image;
use amdgpu::{atom, smu};

use super::{Amd, Bar5};

/// `BIOS_SCRATCH_3` (BAR5 byte `0x13c`): bit 29 says the graphics engine is
/// hung while a reset runs (10 section 3.1).
const SCRATCH_3: usize = 0x13c;
const GUI_HUNG: u32 = 1 << 29;
/// `RCC_CONFIG_MEMSIZE` (dword `0xde3`): reads all ones while the chip is in
/// reset (10 section 3.2).
const CONFIG_MEMSIZE: usize = 0xde3 * 4;
/// How long the chip may take to answer again.
const RESET_WAIT_US: u32 = 5_000_000;
/// The register window's size in bytes: registers beyond it are reached
/// through the index/data pair.
const WINDOW_BYTES: u64 = 0x8_0000;

/// The register access the interpreter uses: direct inside the window,
/// through the pair beyond it.
struct AtomIo<'a>(&'a Bar5<'a>);

impl atom::Io for AtomIo<'_> {
    fn read(&self, index: u32) -> u32 {
        let byte = u64::from(index) * 4;
        if byte < WINDOW_BYTES {
            self.0.read32(usize::try_from(byte).unwrap_or(0))
        } else {
            smu::read(self.0, u32::try_from(byte).unwrap_or(0))
        }
    }

    fn write(&self, index: u32, value: u32) {
        let byte = u64::from(index) * 4;
        if byte < WINDOW_BYTES {
            self.0.write32(usize::try_from(byte).unwrap_or(0), value);
        } else {
            smu::write(self.0, u32::try_from(byte).unwrap_or(0), value);
        }
    }

    fn delay_us(&self, us: u64) {
        bus::manager().delay_us(us);
    }
}

impl Amd {
    /// Runs the video BIOS's ASIC-init table (12 section 7): the boot clocks
    /// of the firmware-info table as its parameters. Skipped (and `Ok`) if
    /// the clocks or the table are missing, as the reference does.
    ///
    /// # Errors
    ///
    /// `ENXIO` without an image; `EIO` if the table fails.
    pub(crate) fn asic_init(&self) -> Result<(), Errno> {
        let gpu = self.gpu()?;
        let image =
            Image::parse(self.vbios_image().ok_or(Errno::ENXIO)?).map_err(|_| Errno::ENXIO)?;
        let Some((engine, memory)) = image.boot_clocks() else {
            self.log(format_args!("ASIC init: no firmware-info table; skipped"));
            return Ok(());
        };
        if engine == 0 || memory == 0 || image.command_table(0).is_none() {
            self.log(format_args!(
                "ASIC init: skipped (clocks {engine}/{memory})"
            ));
            return Ok(());
        }
        let w = Bar5(&gpu);
        let io = AtomIo(&w);
        let mut ps = [0u32; 16];
        ps[0] = engine;
        ps[1] = memory;
        match atom::Interpreter::new(&image, &io).run(0, &mut ps) {
            Ok(()) => {
                self.log(format_args!(
                    "ASIC init: ran (clocks {engine}/{memory}); BIOS scratch 7 {:#x}",
                    w.read32(0x53 * 4)
                ));
                Ok(())
            }
            Err(e) => {
                self.log(format_args!("ASIC init: stopped: {e:?}"));
                Err(Errno::EIO)
            }
        }
    }

    /// Resets the chip and builds everything on it again (see the module
    /// comment). Slow (seconds), and a hazard for the display: the card's
    /// picture may go while the chip is in reset.
    ///
    /// # Errors
    ///
    /// `EBUSY` if a reset is running; `ENXIO` without the video BIOS or the
    /// power management firmware; `EIO` if the chip does not come back.
    pub(crate) fn chip_reset(&self, s: &sched::Sleepable) -> Result<(), Errno> {
        if self.reset_busy.swap(true, Ordering::AcqRel) {
            return Err(Errno::EBUSY);
        }
        let r = self.chip_reset_run(s);
        self.reset_busy.store(false, Ordering::Release);
        r
    }

    fn chip_reset_run(&self, s: &sched::Sleepable) -> Result<(), Errno> {
        let gpu = self.gpu()?;
        let w = Bar5(&gpu);
        let pause = |us: u32| bus::manager().delay_us(u64::from(us));
        // Before anything is touched: the pieces the reset needs are there.
        if self
            .vbios_image()
            .and_then(|v| Image::parse(v).ok())
            .is_none()
        {
            self.log(format_args!("chip reset: no video BIOS; not resetting"));
            return Err(Errno::ENXIO);
        }
        if !smu::running(&w) {
            self.log(format_args!(
                "chip reset: the power firmware is not running"
            ));
            return Err(Errno::ENXIO);
        }
        self.log(format_args!("chip reset: starting"));
        // New work waits until the graphics block is built again.
        self.gfx_state
            .store(1, core::sync::atomic::Ordering::Release);
        // 1. The engines' state goes: nothing the driver holds of them is
        //    valid after the reset (the rings' memory is, but the queues are
        //    not mapped).
        *self.gfx.lock() = None;
        *self.sdma.lock() = None;
        // 2. The flag, the configuration space saved, bus mastering off.
        w.write32(SCRATCH_3, w.read32(SCRATCH_3) | GUI_HUNG);
        let saved = gpu.pci().save_state();
        gpu.pci().set_bus_master(false);
        // 3. The reset: message 10, mode 2. The answer may not come (the
        //    chip is going); the wait afterwards is for the chip.
        let sent = smu::send(&w, smu::message::GFX_DEVICE_RESET, 2, 2_000_000, &pause);
        self.log(format_args!("chip reset: the reset message: {sent:?}"));
        // 4. The configuration space put back, and the wait for the chip.
        gpu.pci().restore_state(&saved);
        let mut back = false;
        for _ in 0..RESET_WAIT_US / 100 {
            let v = w.read32(CONFIG_MEMSIZE);
            if v != 0xffff_ffff && v != 0 {
                back = true;
                break;
            }
            pause(100);
        }
        w.write32(SCRATCH_3, w.read32(SCRATCH_3) & !GUI_HUNG);
        self.log(format_args!(
            "chip reset: the chip {} (memory size register {:#x})",
            if back {
                "answers again"
            } else {
                "does NOT answer"
            },
            w.read32(CONFIG_MEMSIZE)
        ));
        if !back {
            self.gfx_state
                .store(3, core::sync::atomic::Ordering::Release);
            return Err(Errno::EIO);
        }
        // 5. The video BIOS's initialisation.
        let _ = self.asic_init();
        // 6. The translation hubs, the host data path and the interrupt
        //    handler, in the order of 10 section 8.
        self.hdp_ready
            .store(false, core::sync::atomic::Ordering::Release);
        if self.gmc(amdgpu::gmc::Hub::Gfx).is_err() || self.gmc(amdgpu::gmc::Hub::Mm).is_err() {
            self.log(format_args!(
                "chip reset: the translation hubs did not come up"
            ));
        }
        if !self
            .hdp_ready
            .swap(true, core::sync::atomic::Ordering::AcqRel)
        {
            amdgpu::flip::hdp_init(&w);
        }
        match self.irq.restart() {
            Ok(()) => self.log(format_args!("chip reset: interrupts on again")),
            Err(e) => self.log(format_args!("chip reset: interrupts: {e:?}")),
        }
        // 7. The security processor, then every engine's firmware.
        *self.psp.lock() = None;
        self.psp_bring_up(s);
        if self.psp.lock().is_none() {
            self.log(format_args!(
                "chip reset: the security processor did not come up"
            ));
            self.gfx_state
                .store(3, core::sync::atomic::Ordering::Release);
            return Err(Errno::EIO);
        }
        self.gc_load(s);
        // 8. The graphics ring, the compute queues and the DMA engine.
        self.gfx_state
            .store(0, core::sync::atomic::Ordering::Release);
        let up = self.ensure_gfx(s).is_ok();
        self.log(format_args!(
            "chip reset: the graphics block is {}",
            if up { "up" } else { "NOT up" }
        ));
        // 9. The jobs in flight are completed; every context sees the reset.
        if let Some(r) = self
            .render
            .lock()
            .as_ref()
            .and_then(alloc::sync::Weak::upgrade)
        {
            r.after_reset(self);
        }
        // 10. The display, last: do the frames still arrive?
        let before = self.irq.frames();
        pause(200_000);
        let after = self.irq.frames();
        self.log(format_args!(
            "chip reset: display frames {} in 200 ms ({})",
            after.wrapping_sub(before),
            if after != before {
                "the display runs"
            } else {
                "the display is STOPPED"
            }
        ));
        if up { Ok(()) } else { Err(Errno::EIO) }
    }
}
