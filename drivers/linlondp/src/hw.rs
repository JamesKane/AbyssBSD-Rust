// SPDX-License-Identifier: BSD-2-Clause
//! Programming the display processor: the data path of one display
//! (read layers, compositor, image processor, timing generator), the
//! operating mode, the flush that latches shadowed registers, and the
//! interrupt (specification `linlon-dpu` 03, 04, 05 §5.1, 06).
//!
//! One pipeline drives one display here; the second pipeline feeding the
//! first (side by side), scalers, rotation, write-back and compressed
//! buffers are not programmed.

use alloc::sync::Arc;

use drm_fourcc::FormatInfo;
use kms_check::Mode;
use sync::SpinLock;

use crate::discover::{Delay, Layout};
use crate::format::HwFormat;
use crate::regs::{self, Words, bs, common, cu, dou, gcu, ips, layer};
use crate::timing::{self, ModeError};

/// How the compositor treats a layer's alpha (section 05 §5.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    /// The format has no alpha, or it is to be ignored.
    Opaque,
    /// The colour is already multiplied by alpha.
    Premultiplied,
    /// Straight alpha.
    Coverage,
}

/// What a layer shows.
#[derive(Clone, Copy, Debug)]
pub struct LayerCfg<'a> {
    pub format: &'a HwFormat,
    pub info: &'a FormatInfo,
    /// Device addresses of the planes' first lines (offset applied).
    pub addr: [u64; 3],
    pub pitch: [u32; 3],
    /// The source rectangle's size, which is also its size on screen
    /// (no scaling), and where it sits on the display.
    pub width: u32,
    pub height: u32,
    pub dst_x: u32,
    pub dst_y: u32,
    /// The addresses are the translation unit's (device-virtual).
    pub translated: bool,
    pub blend: Blend,
    /// Plane alpha, 0 to 255.
    pub alpha: u8,
}

/// What the interrupt reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Events {
    /// Pipelines whose shadow registers were latched.
    pub latched: [bool; 2],
    /// Pipelines whose programmed line passed: a frame.
    pub frame: [bool; 2],
    /// Error events, counted.
    pub errors: u32,
    /// Pipelines whose output ran dry.
    pub underrun: [bool; 2],
}

/// Why programming failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The operating mode did not take.
    Mode,
    /// The latch did not complete in time.
    Latch,
    /// A value does not fit its register.
    Field,
    /// The mode cannot be shown.
    Timing(ModeError),
}

#[derive(Default)]
struct Running {
    on: [bool; 2],
}

/// The display processor.
pub struct Dpu {
    w: Arc<dyn Words>,
    delay: Arc<dyn Delay>,
    layout: Layout,
    running: SpinLock<Running>,
}

impl core::fmt::Debug for Dpu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dpu")
            .field("core_id", &format_args!("{:#010x}", self.layout.core_id))
            .field("pipes", &self.layout.pipes.len())
            .finish_non_exhaustive()
    }
}

/// How many polls a wait gets, a millisecond apart.
const POLLS: u32 = 100;
/// The latch happens at a frame boundary: 3 frames of 16.7 ms and then
/// some (the vendor's allowance, section 03 §3.4).
const LATCH_POLLS: u32 = 60;

impl Dpu {
    /// Wraps a device whose blocks were found.
    pub fn new(w: Arc<dyn Words>, delay: Arc<dyn Delay>, layout: Layout) -> Self {
        Self {
            w,
            delay,
            layout,
            running: SpinLock::new(Running::default()),
        }
    }

    /// What was found.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The register window.
    #[must_use]
    pub fn words(&self) -> &Arc<dyn Words> {
        &self.w
    }

    /// Initial register state (section 03 §3.2 step 6) and the interrupt
    /// masks (§3.5.3).
    pub fn init(&self) {
        for p in &self.layout.pipes {
            for l in &p.layers {
                self.w
                    .write(l.block.offset + layer::PALPHA, layer::PALPHA_DEFAULT);
            }
            for s in &p.scalers {
                self.w.write(s.offset + common::CONTROL, 0);
            }
        }
        self.unmask();
    }

    /// Unmasks the error and completion sources (the frame interrupt of
    /// each timing generator is separate: [`Dpu::frame_interrupt`]).
    pub fn unmask(&self) {
        let w = &*self.w;
        w.write(
            common::IRQ_MASK,
            gcu::irq_cval(0) | gcu::irq_cval(1) | gcu::IRQ_MODE | gcu::IRQ_ERR,
        );
        for p in &self.layout.pipes {
            // Input busy, error, end of write-back.
            w.write(
                p.lpu.offset + common::IRQ_MASK,
                (1 << 10) | (1 << 11) | (1 << 12),
            );
            // Overrun, error.
            w.write(p.cu.offset + common::IRQ_MASK, (1 << 9) | (1 << 11));
            // Underrun, error. The programmed line is left as it is.
            let keep = w.read(p.dou.offset + common::IRQ_MASK) & dou::IRQ_PL0;
            w.write(
                p.dou.offset + common::IRQ_MASK,
                dou::IRQ_UND | dou::IRQ_ERR | keep,
            );
        }
    }

    /// Masks everything.
    pub fn mask(&self) {
        let w = &*self.w;
        w.write(common::IRQ_MASK, 0);
        for p in &self.layout.pipes {
            w.write(p.lpu.offset + common::IRQ_MASK, 0);
            w.write(p.cu.offset + common::IRQ_MASK, 0);
            w.write(p.dou.offset + common::IRQ_MASK, 0);
        }
    }

    /// Turns pipeline `p`'s programmed-line interrupt on or off.
    pub fn frame_interrupt(&self, p: usize, on: bool) {
        let Some(pipe) = self.layout.pipes.get(p) else {
            return;
        };
        let at = pipe.dou.offset + common::IRQ_MASK;
        let v = self.w.read(at);
        self.w.write(
            at,
            if on {
                v | dou::IRQ_PL0
            } else {
                v & !dou::IRQ_PL0
            },
        );
    }

    /// The operating mode that runs the displays in `on` (section 03 §3.3).
    fn mode_for(on: [bool; 2]) -> u32 {
        match on {
            [false, false] => gcu::MODE_INACTIVE,
            [true, false] => gcu::MODE_DISPLAY0,
            [false, true] => gcu::MODE_DISPLAY1,
            [true, true] => gcu::MODE_BOTH,
        }
    }

    fn set_mode(&self, m: u32) -> Result<(), Error> {
        let c = self.w.read(gcu::CONTROL);
        self.w.write(gcu::CONTROL, (c & !gcu::MODE_MASK) | m);
        for _ in 0..POLLS {
            if self.w.read(gcu::CONTROL) & gcu::MODE_MASK == m {
                return Ok(());
            }
            self.delay.delay_us(1000);
        }
        Err(Error::Mode)
    }

    /// Writes the config-valid register of pipeline `p` (section 03 §3.4).
    pub fn flush(&self, p: usize) {
        self.w.write(gcu::config_valid(p), 1);
    }

    /// Whether the last flush of `p` has been latched. On the Linlon-D6
    /// the bit reads 1 until it has (section 03 §3.4, vendor).
    #[must_use]
    pub fn latched(&self, p: usize) -> bool {
        self.w.read(gcu::config_valid(p)) & 1 == 0
    }

    /// Flushes and waits for the latch, a bounded time.
    ///
    /// # Errors
    ///
    /// [`Error::Latch`] if it does not happen.
    pub fn flush_wait(&self, p: usize) -> Result<(), Error> {
        self.flush(p);
        for _ in 0..LATCH_POLLS {
            if self.latched(p) {
                return Ok(());
            }
            self.delay.delay_us(1000);
        }
        Err(Error::Latch)
    }

    /// Whether pipeline `p` is running.
    #[must_use]
    pub fn running(&self, p: usize) -> bool {
        self.running.lock().on.get(p).copied().unwrap_or(false)
    }

    /// Takes over a pipeline firmware left running (section 08 §8.5): its
    /// timing generator is enabled, so it is marked running and nothing is
    /// reset. The timing it runs is returned, in the registers' own
    /// encoding. `None` if the pipeline is not running.
    pub fn adopt(&self, p: usize) -> Option<timing::BsRegs> {
        let pipe = self.layout.pipes.get(p)?;
        let b = pipe.bs.offset;
        let control = self.w.read(b + bs::CONTROL);
        if control & bs::CTRL_EN == 0 {
            return None;
        }
        self.running.lock().on[p] = true;
        Some(timing::BsRegs {
            active: self.w.read(b + bs::ACTIVESIZE),
            hintervals: self.w.read(b + bs::HINTERVALS),
            vintervals: self.w.read(b + bs::VINTERVALS),
            sync: self.w.read(b + bs::SYNC),
            prog_line: self.w.read(b + bs::PROG_LINE),
            prefetch_line: self.w.read(b + bs::PREFETCH_LINE),
            control,
        })
    }

    /// Whether pipeline `p`'s timing generator runs exactly the timing
    /// `mode` asks for, so a display already showing it can be kept (the
    /// pixel clock is the caller's to compare).
    #[must_use]
    pub fn timing_matches(&self, p: usize, mode: &Mode) -> bool {
        let Some(pipe) = self.layout.pipes.get(p) else {
            return false;
        };
        let Ok(want) = timing::bs_regs(mode) else {
            return false;
        };
        let b = pipe.bs.offset;
        self.w.read(b + bs::CONTROL) & bs::CTRL_EN != 0
            && self.w.read(b + bs::ACTIVESIZE) == want.active
            && self.w.read(b + bs::HINTERVALS) == want.hintervals
            && self.w.read(b + bs::VINTERVALS) == want.vintervals
            && self.w.read(b + bs::SYNC) == want.sync
    }

    /// Programs the timing, the compositor's size and the image processor
    /// of pipeline `p` for `mode`, with every compositor input off, and
    /// starts it (section 03 §3.8, from step 2; the clocks are the
    /// caller's).
    ///
    /// # Errors
    ///
    /// [`Error`] for a mode the pipeline cannot show, or hardware that
    /// does not answer.
    pub fn enable(&self, p: usize, mode: &Mode) -> Result<(), Error> {
        let pipe = self.layout.pipes.get(p).ok_or(Error::Field)?;
        let t = timing::bs_regs(mode).map_err(Error::Timing)?;
        let (w, h) = (u32::from(mode.hdisplay), u32::from(mode.vdisplay));
        // The compositor and the layers take 64 pixels at least, and what
        // their line buffers hold at most (section 05 §5.1.4).
        if w < 64 || h < 64 || w > pipe.cu.max_line.max(1) || h > self.layout.max_lines {
            return Err(Error::Field);
        }
        let size = regs::hv_size(w, h).ok_or(Error::Field)?;
        let on = {
            let mut r = self.running.lock();
            r.on[p] = true;
            r.on
        };
        self.set_mode(Self::mode_for(on))?;
        let x = &*self.w;
        // Compositor size. Its inputs are the layers' to set: a commit
        // programs them before the pipeline's, so none is cleared here
        // (see `clear`).
        x.write(pipe.cu.offset + cu::SIZE, size);
        // Image processor: the compositor in, 8 bits, no colour changes.
        x.write(pipe.ips.offset + ips::INPUT_ID0, pipe.cu.input_id(0));
        x.write(pipe.ips.offset + ips::INPUT_ID1, 0);
        x.write(pipe.ips.offset + ips::SIZE, size);
        x.write(pipe.ips.offset + ips::DEPTH, 8);
        x.write(pipe.ips.offset + ips::CONTROL, 0);
        // Timing generator.
        let b = pipe.bs.offset;
        x.write(b + bs::ACTIVESIZE, t.active);
        x.write(b + bs::HINTERVALS, t.hintervals);
        x.write(b + bs::VINTERVALS, t.vintervals);
        x.write(b + bs::SYNC, t.sync);
        x.write(b + bs::PROG_LINE, t.prog_line);
        x.write(b + bs::PREFETCH_LINE, t.prefetch_line);
        x.write(b + bs::CONTROL, t.control);
        Ok(())
    }

    /// Shows layer `l` of pipeline `p` as compositor input `l` (section
    /// 04 §4.4 and 05 §5.1.2). The caller flushes.
    ///
    /// # Errors
    ///
    /// [`Error::Field`] for a value that does not fit.
    pub fn show(&self, p: usize, l: usize, c: &LayerCfg<'_>) -> Result<(), Error> {
        let pipe = self.layout.pipes.get(p).ok_or(Error::Field)?;
        let lay = pipe.layers.get(l).ok_or(Error::Field)?;
        let size = regs::hv_size(c.width, c.height).ok_or(Error::Field)?;
        let at = regs::hv_offset(c.dst_x, c.dst_y).ok_or(Error::Field)?;
        let x = &*self.w;
        let b = lay.block.offset;
        // Planes 2, 1, 0 (section 04 §4.4 step 1).
        let planes = usize::from(c.info.planes);
        for i in (0..planes).rev() {
            let (lo, hi) = regs::addr_words(c.addr[i]);
            let (low, high) = match i {
                0 => (layer::P0_PTR_LOW, layer::P0_PTR_HIGH),
                1 => (layer::P1_PTR_LOW, layer::P1_PTR_HIGH),
                _ => (layer::P2_PTR_LOW, layer::P2_PTR_HIGH),
            };
            x.write(b + low, lo);
            x.write(b + high, hi);
        }
        x.write(b + layer::P0_STRIDE, c.pitch[0]);
        if planes >= 2 {
            x.write(b + layer::P1_STRIDE, c.pitch[1]);
        }
        x.write(b + layer::FORMAT, c.format.id);
        x.write(b + layer::AD_CONTROL, 0);
        x.write(b + layer::IN_SIZE, size);
        let ctl = layer::CTRL_EN | if c.translated { layer::CTRL_TBU_EN } else { 0 };
        let old = x.read(b + layer::CONTROL);
        // Keep the cache attributes, replace the rest we own.
        x.write(b + layer::CONTROL, (old & 0xF000_0000) | ctl);
        // The compositor input.
        let c0 = pipe.cu.offset;
        x.write(c0 + cu::input_size(l), size);
        x.write(c0 + cu::input_offset(l), at);
        let blend = match c.blend {
            Blend::Opaque => cu::IN_PAD,
            Blend::Premultiplied => cu::IN_PMUL,
            Blend::Coverage => 0,
        };
        x.write(
            c0 + cu::input_control(l),
            cu::IN_EN | blend | u32::from(c.alpha) << 8,
        );
        x.write(c0 + cu::INPUT_ID + 4 * l, lay.block.input_id(0));
        Ok(())
    }

    /// Takes every layer of pipeline `p` off its compositor, for a
    /// pipeline the firmware did not leave running. The caller flushes.
    pub fn clear(&self, p: usize) {
        let n = self.layout.pipes.get(p).map_or(0, |pipe| pipe.layers.len());
        for l in 0..n {
            self.hide(p, l);
        }
    }

    /// Takes layer `l` of pipeline `p` off the display (section 03 §3.6).
    /// The caller flushes.
    pub fn hide(&self, p: usize, l: usize) {
        let Some(pipe) = self.layout.pipes.get(p) else {
            return;
        };
        let Some(lay) = pipe.layers.get(l) else {
            return;
        };
        let x = &*self.w;
        let b = lay.block.offset + layer::CONTROL;
        x.write(b, x.read(b) & !layer::CTRL_EN);
        let c0 = pipe.cu.offset;
        x.write(c0 + cu::INPUT_ID + 4 * l, 0);
        x.write(c0 + cu::input_control(l), cu::IN_DISABLED);
    }

    /// Stops pipeline `p` in two flushes, because the hardware does not
    /// latch shadow registers once the timing generator is off (section
    /// 03 §3.6), and returns the operating mode to what remains.
    ///
    /// # Errors
    ///
    /// [`Error`] when a latch or the mode change times out; the pipeline
    /// is marked stopped regardless, so the caller can go on.
    pub fn disable(&self, p: usize) -> Result<(), Error> {
        let pipe = self.layout.pipes.get(p).ok_or(Error::Field)?;
        let x = &*self.w;
        for l in 0..pipe.layers.len() {
            self.hide(p, l);
        }
        x.write(pipe.cu.offset + cu::CONTROL, 0);
        let phase1 = self.flush_wait(p);
        // Phase 2: the image processor and the timing generator.
        x.write(pipe.ips.offset + ips::CONTROL, 0);
        x.write(pipe.ips.offset + ips::INPUT_ID0, 0);
        x.write(pipe.ips.offset + ips::INPUT_ID1, 0);
        let b = pipe.bs.offset + bs::CONTROL;
        x.write(b, x.read(b) & !bs::CTRL_EN);
        let phase2 = self.flush_wait(p);
        self.frame_interrupt(p, false);
        let on = {
            let mut r = self.running.lock();
            r.on[p] = false;
            r.on
        };
        let mode = self.set_mode(Self::mode_for(on));
        phase1.and(phase2).and(mode)
    }

    /// Decodes the interrupt (section 03 §3.5.1): what happened, with
    /// the sources cleared. Empty if the device did not raise it.
    pub fn interrupt(&self) -> Events {
        let x = &*self.w;
        let mut ev = Events::default();
        let top = x.read(gcu::IRQ_STATUS);
        if top == 0 {
            return ev;
        }
        if top & gcu::SUMMARY_GCU != 0 {
            let raw = x.read(common::IRQ_RAW_STATUS);
            for (p, l) in ev.latched.iter_mut().enumerate() {
                *l = raw & gcu::irq_cval(p) != 0;
            }
            if raw & gcu::IRQ_ERR != 0 {
                ev.errors += 1;
                let st = x.read(gcu::STATUS);
                if st & gcu::STATUS_MERR != 0 {
                    x.write(gcu::STATUS, st & !gcu::STATUS_MERR);
                }
            }
            x.write(common::IRQ_CLEAR, raw);
        }
        for (p, pipe) in self.layout.pipes.iter().enumerate() {
            if top & gcu::summary_lpu(p) != 0 {
                let raw = x.read(pipe.lpu.offset + common::IRQ_RAW_STATUS);
                if raw & ((1 << 9) | (1 << 10) | (1 << 11)) != 0 {
                    ev.errors += 1;
                    // The sticky error bits are read and cleared.
                    let st = x.read(pipe.lpu.offset + common::STATUS);
                    x.write(pipe.lpu.offset + common::STATUS, st & 0x8000_0000);
                }
                x.write(pipe.lpu.offset + common::IRQ_CLEAR, raw);
            }
            if top & gcu::summary_cu(p) != 0 {
                let raw = x.read(pipe.cu.offset + common::IRQ_RAW_STATUS);
                if raw & ((1 << 9) | (1 << 11)) != 0 {
                    ev.errors += 1;
                    let st = x.read(pipe.cu.offset + cu::STATUS);
                    // Bit 31 is the block's activity, not an error.
                    x.write(pipe.cu.offset + cu::STATUS, st & 0x8000_0000);
                }
                x.write(pipe.cu.offset + common::IRQ_CLEAR, raw);
            }
            if top & gcu::summary_dou(p) != 0 {
                let raw = x.read(pipe.dou.offset + common::IRQ_RAW_STATUS);
                ev.frame[p] = raw & dou::IRQ_PL0 != 0;
                ev.underrun[p] = raw & dou::IRQ_UND != 0;
                if raw & (dou::IRQ_UND | dou::IRQ_ERR) != 0 {
                    ev.errors += 1;
                    let st = x.read(pipe.dou.offset + common::STATUS);
                    x.write(pipe.dou.offset + common::STATUS, st & 0x8000_0000);
                }
                x.write(pipe.dou.offset + common::IRQ_CLEAR, raw);
            }
        }
        ev
    }
}

/// The address of line `y`, column `x` of plane `i` of a buffer: `base`
/// is the plane's first byte (section 04 §4.4 step 1). `x` and `y` are
/// in the first plane's pixels; the chroma planes are subsampled.
#[must_use]
pub fn plane_addr(info: &FormatInfo, i: usize, base: u64, pitch: u32, x: u32, y: u32) -> u64 {
    let (hs, vs) = if i == 0 {
        (1, 1)
    } else {
        (u32::from(info.hsub), u32::from(info.vsub))
    };
    let bytes = u64::from(info.cpp[i]);
    base + u64::from(y / vs) * u64::from(pitch) + u64::from(x / hs) * bytes
}
