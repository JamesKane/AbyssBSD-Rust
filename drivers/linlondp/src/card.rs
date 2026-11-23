// SPDX-License-Identifier: BSD-2-Clause
//! The display processor as a graphics card: one display controller per
//! pipeline, one plane per read layer, over [`Dpu`] (specification
//! `linlon-dpu` 07 §7.3, §7.4).
//!
//! What the card shows: linear buffers, one to one (no scaling,
//! rotation, compressed buffers or colour management), a layer's
//! position in the compositor fixed by its number. Buffers are system
//! pages the display reads through [`Platform::map`], and, because the
//! unit does not snoop the processor's caches (section 08 §8.2.3), are
//! cleaned before a commit shows them.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::errno::Errno;
use drm::gem::{Gem, Placement};
use drm::kms::{
    ConnectorHw, CrtcHw, CrtcUpdate, EncoderHw, Fb, Kms, KmsLimits, PlaneHw, PlaneKind, PlaneUpdate,
};
use drm::{DriverInfo, DrmDriver, DumbLimits, KmsBuilder};
use drm_fourcc::{Fourcc, MOD_LINEAR, lookup};
use sched::Sleepable;
use sync::SpinLock;

use crate::format::{self, LayerKind};
use crate::hw::{Blend, Dpu, LayerCfg, plane_addr};
use crate::timing;

/// A buffer made readable by the display processor.
#[derive(Debug)]
pub struct Mapped {
    /// The device address of the buffer's first byte.
    pub addr: u64,
    /// The addresses go through the unit's own translation (section 01
    /// §1.5); only when it has one.
    pub translated: bool,
    /// Holds the mapping; dropping it unmaps.
    pub keep: Box<dyn DebugAny>,
}

/// Whatever holds a mapping.
pub trait DebugAny: Any + Send + Sync + core::fmt::Debug {}
impl<T: Any + Send + Sync + core::fmt::Debug> DebugAny for T {}

/// What the card needs from the machine.
pub trait Platform: Send + Sync {
    /// Makes `gem` readable by the display processor.
    ///
    /// # Errors
    ///
    /// `ENOMEM`, or `EINVAL` for a buffer it cannot map.
    fn map(&self, gem: &Arc<Gem>) -> Result<Mapped, Errno>;
    /// Cleans the processor's caches for bytes `range` of `gem`.
    fn clean(&self, gem: &Gem, range: core::ops::Range<u64>);
    /// Sets pipeline `p`'s pixel clock to `khz` and runs it.
    ///
    /// # Errors
    ///
    /// `EINVAL` when the clock cannot be made.
    fn pixel_clock_on(&self, s: &Sleepable, p: usize, khz: u32) -> Result<(), Errno>;
    /// Stops pipeline `p`'s pixel clock.
    fn pixel_clock_off(&self, s: &Sleepable, p: usize);
    /// Joins the pixel clock firmware left running for pipeline `p`,
    /// unchanged; its rate in kHz.
    fn pixel_clock_adopt(&self, s: &Sleepable, p: usize) -> Option<u32>;
    /// The main clock's rate in kHz.
    fn aclk_khz(&self) -> u64;
    /// The time, in nanoseconds, for frame stamps.
    fn now_ns(&self) -> u64;
}

/// What a pipeline's output is: the encoder and connector the
/// aggregate attached.
#[allow(missing_debug_implementations)]
pub struct Output {
    pub pipe: usize,
    pub encoder: Arc<dyn EncoderHw>,
    pub connector: Arc<dyn ConnectorHw>,
}

struct Inner {
    dpu: Dpu,
    plat: Arc<dyn Platform>,
    /// What each layer shows now, to show again after the pipeline is
    /// restarted for a mode change.
    shown: SpinLock<[[Option<LayerCfg<'static>>; 4]; 2]>,
    /// The pixel clock (kHz) of a pipeline taken over from firmware, until
    /// the first mode set decides to keep it or take it down.
    adopted: SpinLock<[Option<u32>; 2]>,
}

struct Crtc {
    inner: Arc<Inner>,
    pipe: usize,
}

impl CrtcHw for Crtc {
    fn vblank_enable(&self, on: bool) {
        self.inner.dpu.frame_interrupt(self.pipe, on);
    }

    fn vblank_interrupt(&self) -> bool {
        true
    }

    fn check(&self, u: &CrtcUpdate<'_>) -> Result<(), Errno> {
        let (true, Some(m)) = (u.new.active, u.new.mode.as_ref()) else {
            return Ok(());
        };
        timing::bs_regs(m).map_err(|_| Errno::EINVAL)?;
        // The main clock must keep up with the pixel clock.
        if timing::min_aclk_khz(m, false) > self.inner.plat.aclk_khz() {
            return Err(Errno::EINVAL);
        }
        let pipe = &self.inner.dpu.layout().pipes[self.pipe];
        if u32::from(m.hdisplay) > pipe.cu.max_line {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    fn commit(&self, u: &CrtcUpdate<'_>) {
        let (d, p) = (&self.inner.dpu, self.pipe);
        // A mode change needs the requesting thread, for the clocks.
        if u.modeset && u.s.is_none() {
            return;
        }
        // A display firmware left running, asked for in the mode it runs:
        // keep it, changing only what the layers show (section 08 §8.5).
        let adopted = self.inner.adopted.lock()[p].take();
        if let (true, Some(khz), true, Some(m)) =
            (u.modeset, adopted, u.new.active, u.new.mode.as_ref())
            && d.timing_matches(p, m)
            && m.clock_khz.abs_diff(khz) <= khz / 100
        {
            let shown = *self.inner.shown.lock();
            // With nothing to show (no layer's buffer could be mapped for
            // the display) the firmware's picture stays, rather than a
            // blank one replacing it.
            if shown[p].iter().all(Option::is_none) {
                return;
            }
            for (l, cfg) in shown[p].iter().enumerate() {
                if cfg.is_none() {
                    d.hide(p, l);
                }
            }
            let _ = d.flush_wait(p);
            return;
        }
        if let (true, true, Some(s)) = (u.modeset, d.running(p), u.s) {
            // Changing mode, or turning off: stop first.
            if d.disable(p).is_err() {
                // The log is the platform's; the state is marked stopped.
            }
            self.inner.plat.pixel_clock_off(s, p);
        }
        match (u.new.active, u.new.mode.as_ref()) {
            (true, Some(m)) if u.modeset => {
                let Some(s) = u.s else { return };
                if self.inner.plat.pixel_clock_on(s, p, m.clock_khz).is_err()
                    || d.enable(p, m).is_err()
                {
                    return;
                }
                // The layers were programmed before the pipeline was
                // restarted (a stop hides them): show them again.
                let shown = *self.inner.shown.lock();
                for (l, cfg) in shown[p].iter().enumerate() {
                    if let Some(c) = cfg {
                        let _ = d.show(p, l, c);
                    }
                }
                // The first latch starts the output; wait for it, so a
                // flip that follows finds the pipeline running.
                let _ = d.flush_wait(p);
            }
            (true, _) if d.running(p) => d.flush(p),
            _ => {}
        }
    }
}

struct Held {
    fb: Arc<Fb>,
    /// One mapping per memory plane; kept as long as the layer shows it.
    maps: Vec<Mapped>,
}

/// Moves what is held one commit back, and lets go of what was held three
/// commits ago.
fn retire(h: &mut [Option<Held>; 3]) {
    h[2] = h[1].take();
    h[1] = h[0].take();
}

struct Layer {
    inner: Arc<Inner>,
    pipe: usize,
    layer: usize,
    kind: LayerKind,
    formats: Vec<Fourcc>,
    /// What is shown, and what was shown before it, twice over. A flip
    /// takes effect at the next latch, which can be a frame after the
    /// commit's event was sent, so the display may still read what the
    /// previous commit replaced when the next one arrives: a buffer goes
    /// only when three commits have replaced it (observed on the board:
    /// the unit read a buffer one commit after it was released).
    held: SpinLock<[Option<Held>; 3]>,
}

impl PlaneHw for Layer {
    fn formats(&self) -> &[Fourcc] {
        &self.formats
    }

    fn check(&self, u: &PlaneUpdate<'_>) -> Result<(), Errno> {
        let (Some(fb), true) = (u.fb, u.visible.shown) else {
            return Ok(());
        };
        let info = lookup(fb.format).ok_or(Errno::EINVAL)?;
        let hf = format::lookup(fb.format)
            .filter(|h| h.takes(self.kind))
            .ok_or(Errno::EINVAL)?;
        let _ = hf;
        if fb.modifier != MOD_LINEAR {
            return Err(Errno::EINVAL);
        }
        let planes = usize::from(info.planes);
        // Pitches are whole bus words (section 04 §4.6); the chroma planes
        // of a three-plane format share one stride (§4.4 step 2).
        if fb.pitches[..planes].iter().any(|p| p % 16 != 0) {
            return Err(Errno::EINVAL);
        }
        if planes == 3 && fb.pitches[1] != fb.pitches[2] {
            return Err(Errno::EINVAL);
        }
        let v = u.visible;
        let (x, y, w, h) = (v.src.x >> 16, v.src.y >> 16, v.dst.w, v.dst.h);
        let (hs, vs) = (u32::from(info.hsub), u32::from(info.vsub));
        // Sizes and positions fall on the chroma samples (§4.6). The
        // fractional part of the source is dropped, and the size must be
        // exactly the destination's: no scaling.
        if x % hs != 0 || y % vs != 0 || w % hs != 0 || h % vs != 0 {
            return Err(Errno::EINVAL);
        }
        if v.src.w >> 16 != w
            || v.src.h >> 16 != h
            || v.src.w & 0xFFFF != 0
            || v.src.h & 0xFFFF != 0
        {
            return Err(Errno::EINVAL);
        }
        let lay = &self.inner.dpu.layout().pipes[self.pipe].layers[self.layer];
        let max_w = if info.is_yuv {
            lay.max_line_yuv
        } else {
            lay.block.max_line
        };
        if w < 4 || h < 4 || w > max_w || h > self.inner.dpu.layout().max_lines {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    fn commit(&self, u: &PlaneUpdate<'_>) {
        let (p, l) = (self.pipe, self.layer);
        let inner = &self.inner;
        let (Some(fb), true, Some(_)) = (u.fb, u.visible.shown, u.crtc) else {
            inner.shown.lock()[p][l] = None;
            inner.dpu.hide(p, l);
            // What it showed may still be read until this commit
            // completes: it goes at the next one.
            let mut h = self.held.lock();
            retire(&mut h);
            return;
        };
        let (Some(info), Some(hf)) = (lookup(fb.format), format::lookup(fb.format)) else {
            return;
        };
        // Map each plane's buffer, once for a framebuffer already shown.
        let reuse = {
            let h = self.held.lock();
            h[0].as_ref().is_some_and(|c| Arc::ptr_eq(&c.fb, fb))
        };
        let mut fresh = Vec::new();
        if !reuse {
            for i in 0..usize::from(info.planes) {
                let Some(gem) = fb.gems[i].as_ref() else {
                    return;
                };
                if !gem.placement().coherent {
                    inner.plat.clean(gem, 0..gem.size());
                }
                match inner.plat.map(gem) {
                    Ok(m) => fresh.push(m),
                    Err(_) => return,
                }
            }
        }
        let v = u.visible;
        let (x, y) = (v.src.x >> 16, v.src.y >> 16);
        let mut addr = [0u64; 3];
        let mut translated = false;
        {
            let mut h = self.held.lock();
            if !reuse {
                // What was shown three commits ago is no longer read.
                retire(&mut h);
                h[0] = Some(Held {
                    fb: Arc::clone(fb),
                    maps: fresh,
                });
            }
            let Some(cur) = h[0].as_ref() else { return };
            for (i, m) in cur.maps.iter().enumerate().take(3) {
                let base = m.addr + u64::from(fb.offsets[i]);
                addr[i] = plane_addr(info, i, base, fb.pitches[i], x, y);
                translated |= m.translated;
            }
        }
        let blend = if info.has_alpha {
            Blend::Premultiplied
        } else {
            Blend::Opaque
        };
        let cfg = LayerCfg {
            format: hf,
            info,
            addr,
            pitch: [fb.pitches[0], fb.pitches[1], fb.pitches[2]],
            width: v.dst.w,
            height: v.dst.h,
            dst_x: u32::try_from(v.dst.x).unwrap_or(0),
            dst_y: u32::try_from(v.dst.y).unwrap_or(0),
            translated,
            blend,
            alpha: 0xFF,
        };
        if inner.dpu.show(p, l, &cfg).is_ok() {
            inner.shown.lock()[p][l] = Some(cfg);
        }
    }
}

/// The card.
pub struct Card {
    kms: Kms,
    inner: Arc<Inner>,
}

impl core::fmt::Debug for Card {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Card")
            .field("dpu", &self.inner.dpu)
            .finish()
    }
}

/// Makes the card over `dpu`, with the outputs the aggregate has.
///
/// # Errors
///
/// What the object model refuses.
pub fn create(dpu: Dpu, plat: Arc<dyn Platform>, outputs: Vec<Output>) -> Result<Arc<Card>, Errno> {
    let lay = dpu.layout().clone();
    let inner = Arc::new(Inner {
        dpu,
        plat,
        shown: SpinLock::new([[None; 4]; 2]),
        adopted: SpinLock::new([None; 2]),
    });
    let mut b = KmsBuilder::new(KmsLimits {
        min_width: 4,
        max_width: lay.max_line,
        min_height: 4,
        max_height: lay.max_lines,
    });
    for (p, pipe) in lay.pipes.iter().enumerate() {
        let c = b.crtc(Arc::new(Crtc {
            inner: Arc::clone(&inner),
            pipe: p,
        }));
        for (l, layer) in pipe.layers.iter().enumerate() {
            let kind = if layer.rich {
                LayerKind::Rich
            } else {
                LayerKind::Simple
            };
            let plane = Layer {
                inner: Arc::clone(&inner),
                pipe: p,
                layer: l,
                kind,
                formats: format::offered(kind),
                held: SpinLock::new([None, None, None]),
            };
            let pk = if l == 0 {
                PlaneKind::Primary
            } else {
                PlaneKind::Overlay
            };
            b.plane(Arc::new(plane), pk, 1 << c);
        }
        if let Some(o) = outputs.iter().find(|o| o.pipe == p) {
            let e = b.encoder(Arc::clone(&o.encoder), 1 << c, 0);
            b.connector(Arc::clone(&o.connector), &[e]);
        }
    }
    let kms = b.build()?;
    Ok(Arc::new(Card { kms, inner }))
}

impl Card {
    /// Handles the device's interrupt: frames complete commits and wake
    /// waiters. Whether the device raised it.
    pub fn interrupt(&self) -> bool {
        let ev = self.inner.dpu.interrupt();
        let now = self.inner.plat.now_ns();
        for p in 0..2 {
            if ev.frame[p] {
                self.kms.vblank(p, now);
            }
        }
        ev != crate::hw::Events::default()
    }

    /// Takes over every pipeline firmware left running, with the pixel
    /// clock it runs on: the first mode set keeps the display if it asks
    /// for the mode on screen, and takes it down otherwise. Whether any
    /// was running.
    pub fn adopt(&self, s: &Sleepable) -> bool {
        let mut any = false;
        for p in 0..self.inner.dpu.layout().pipes.len() {
            if self.inner.dpu.adopt(p).is_some() {
                let khz = self.inner.plat.pixel_clock_adopt(s, p);
                self.inner.adopted.lock()[p] = khz;
                any = true;
            }
        }
        any
    }

    /// The display processor.
    #[must_use]
    pub fn dpu(&self) -> &Dpu {
        &self.inner.dpu
    }
}

impl DrmDriver for Card {
    fn info(&self) -> DriverInfo {
        // Mesa pairs a render-only GPU with the display by this name
        // (section 07 §7.1). Not yet checked against the Mesa in the 15.1
        // packages.
        DriverInfo {
            name: "komeda",
            desc: "Arm China Linlon display processor",
            date: "20261123",
            major: 0,
            minor: 1,
            patch: 0,
        }
    }

    fn kms(&self) -> Option<&Kms> {
        Some(&self.kms)
    }

    fn dumb(&self) -> Option<DumbLimits> {
        // Pitches in whole 16 byte bus words (section 07 §7.2).
        Some(DumbLimits {
            pitch_align: 16,
            ..DumbLimits::default()
        })
    }

    fn dumb_placement(&self) -> Placement {
        // The unit does not snoop the processor's caches.
        Placement::SYSTEM_NONCOHERENT
    }

    fn sync_for_device(&self, gem: &Gem, range: core::ops::Range<u64>) {
        self.inner.plat.clean(gem, range);
    }
}

#[cfg(test)]
#[path = "card_tests.rs"]
mod tests;
