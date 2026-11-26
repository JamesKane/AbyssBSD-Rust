// SPDX-License-Identifier: BSD-2-Clause
//! The card's planes and controllers, driven directly with a buffer in
//! host memory and a platform that records what it is asked.

use std::collections::HashMap;
use std::prelude::rust_2024::*;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::{boxed::Box, vec};

use abi::errno::Errno;
use drm::gem::{Gem, Placement};
use drm::kms::{CrtcSt, CrtcUpdate, Fb, PlaneSt, PlaneUpdate};
use drm_fourcc::{Fourcc, MOD_LINEAR, Modifier, format};
use kms_check::{DstRect, Mode, ObjId, SrcRect, Visible};

use super::*;
use crate::discover;
use crate::regs::{bs, cu, gcu, layer};
use crate::sim::Sim;

#[derive(Default)]
struct HeapPages {
    pages: Mutex<HashMap<u64, Box<[u8; 4096]>>>,
    next: AtomicU64,
}

impl vm::object::PageOps for HeapPages {
    fn alloc_zeroed(&self) -> Option<vm::Pfn> {
        let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.pages.lock().unwrap().insert(n, Box::new([0; 4096]));
        Some(vm::Pfn(n))
    }
    fn alloc_copy(&self, from: vm::Pfn) -> Option<vm::Pfn> {
        let c = Box::new(*self.pages.lock().unwrap()[&from.0]);
        let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.pages.lock().unwrap().insert(n, c);
        Some(vm::Pfn(n))
    }
    fn alloc_filled(&self, fill: &mut dyn FnMut(&mut [u8])) -> Option<vm::Pfn> {
        let mut p = Box::new([0; 4096]);
        fill(&mut p[..]);
        let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.pages.lock().unwrap().insert(n, p);
        Some(vm::Pfn(n))
    }
    fn free(&self, pfn: vm::Pfn) {
        self.pages.lock().unwrap().remove(&pfn.0);
    }
    fn access(&self, pfn: vm::Pfn, f: &mut dyn FnMut(&mut [u8])) {
        f(&mut self.pages.lock().unwrap().get_mut(&pfn.0).unwrap()[..]);
    }
}

/// A live mapping: counts itself while it exists.
#[derive(Debug)]
struct Token(Arc<AtomicU32>);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct TestPlatform {
    live_maps: Arc<AtomicU32>,
    /// Buffers cannot be mapped (a display that sees only contiguous
    /// memory, given scattered pages).
    refuse: std::sync::atomic::AtomicBool,
    next_addr: AtomicU64,
    cleaned: AtomicU32,
    pixel: Mutex<Vec<(usize, Option<u32>)>>,
    /// The console framebuffers handed to the kernel's console.
    consoles: Mutex<Vec<ConsoleBuffer>>,
}

impl Platform for TestPlatform {
    fn map(&self, _: &Arc<Gem>) -> Result<Mapped, Errno> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(Errno::EINVAL);
        }
        self.live_maps.fetch_add(1, Ordering::SeqCst);
        Ok(Mapped {
            addr: self.next_addr.fetch_add(0x100_0000, Ordering::SeqCst),
            translated: false,
            keep: Box::new(Token(Arc::clone(&self.live_maps))),
        })
    }
    fn clean(&self, _: &Gem, _: core::ops::Range<u64>) {
        self.cleaned.fetch_add(1, Ordering::SeqCst);
    }
    fn pixel_clock_on(&self, _: &Sleepable, p: usize, khz: u32) -> Result<(), Errno> {
        self.pixel.lock().unwrap().push((p, Some(khz)));
        Ok(())
    }
    fn pixel_clock_off(&self, _: &Sleepable, p: usize) {
        self.pixel.lock().unwrap().push((p, None));
    }
    fn pixel_clock_adopt(&self, _: &Sleepable, _: usize) -> Option<u32> {
        Some(148_500)
    }
    fn aclk_khz(&self) -> u64 {
        800_000
    }
    fn now_ns(&self) -> u64 {
        1_000
    }
    fn console_buffer(&self, width: u32, height: u32) -> Result<ConsoleBuffer, Errno> {
        Ok(ConsoleBuffer {
            addr: 0x4_0000_0000,
            translated: false,
            phys: 0x9_0000_0000,
            width,
            height,
        })
    }
    fn console_retarget(&self, _: &Sleepable, b: &ConsoleBuffer) -> bool {
        self.consoles.lock().unwrap().push(*b);
        true
    }
}

struct Rig {
    sim: Arc<Sim>,
    plat: Arc<TestPlatform>,
    card: Arc<Card>,
    pages: &'static HeapPages,
    crtc: Crtc,
    layers: Vec<Layer>,
}

fn rig() -> Rig {
    let sim = Arc::new(Sim::d6());
    let layout = discover::walk(&*sim, 0x2_0000).unwrap();
    let dpu = Dpu::new(sim.clone(), sim.clone(), layout);
    dpu.init();
    let plat = Arc::new(TestPlatform {
        live_maps: Arc::new(AtomicU32::new(0)),
        refuse: std::sync::atomic::AtomicBool::new(false),
        next_addr: AtomicU64::new(0x8000_0000),
        cleaned: AtomicU32::new(0),
        pixel: Mutex::new(Vec::new()),
        consoles: Mutex::new(Vec::new()),
    });
    let card = create(dpu, plat.clone(), Vec::new()).unwrap();
    let inner = Arc::clone(&card.inner);
    let layers = (0..4)
        .map(|l| {
            let rich = inner.dpu.layout().pipes[0].layers[l].rich;
            let kind = if rich {
                LayerKind::Rich
            } else {
                LayerKind::Simple
            };
            Layer {
                inner: Arc::clone(&inner),
                pipe: 0,
                layer: l,
                kind,
                formats: crate::format::offered(kind),
                held: SpinLock::new([None, None, None]),
            }
        })
        .collect();
    let crtc = Crtc { inner, pipe: 0 };
    Rig {
        sim,
        plat,
        card,
        pages: Box::leak(Box::default()),
        crtc,
        layers,
    }
}

fn hd() -> Mode {
    Mode {
        clock_khz: 148_500,
        hdisplay: 1920,
        hsync_start: 2008,
        hsync_end: 2052,
        htotal: 2200,
        vdisplay: 1080,
        vsync_start: 1084,
        vsync_end: 1089,
        vtotal: 1125,
        ..Mode::default()
    }
}

fn fb(r: &Rig, f: Fourcc, w: u32, h: u32, pitch: u32) -> Arc<Fb> {
    let size = u64::from(pitch) * u64::from(h) * 2;
    let gem = Gem::new(r.pages, size, 0x10_0000, Placement::SYSTEM_NONCOHERENT).unwrap();
    Arc::new(Fb {
        id: ObjId(100),
        width: w,
        height: h,
        format: f,
        modifier: MOD_LINEAR,
        flags: 0,
        gems: [Some(gem.clone()), Some(gem.clone()), Some(gem), None],
        pitches: [pitch, pitch, pitch, 0],
        offsets: [0; 4],
        owner: 1,
    })
}

fn visible(w: u32, h: u32, x: i32, y: i32) -> Visible {
    Visible {
        shown: true,
        src: SrcRect {
            x: 0,
            y: 0,
            w: w << 16,
            h: h << 16,
        },
        dst: DstRect { x, y, w, h },
    }
}

fn update<'a>(
    fb: Option<&'a Arc<Fb>>,
    v: Visible,
    old: &'a PlaneSt,
    new: &'a PlaneSt,
) -> PlaneUpdate<'a> {
    PlaneUpdate {
        index: 0,
        crtc: Some(0),
        fb,
        visible: v,
        old,
        new,
    }
}

fn turn_on(r: &Rig) {
    let (old, new) = (
        CrtcSt::default(),
        CrtcSt {
            active: true,
            mode: Some(hd()),
            ..CrtcSt::default()
        },
    );
    let u = CrtcUpdate {
        index: 0,
        old: &old,
        new: &new,
        modeset: true,
        s: Some(&sl()),
    };
    r.crtc.check(&u).unwrap();
    r.crtc.commit(&u);
}

#[test]
fn what_a_plane_will_take_and_what_it_refuses() {
    let r = rig();
    let (o, n) = (PlaneSt::default(), PlaneSt::default());
    let ok = fb(&r, format::ARGB8888, 640, 480, 2560);
    let chk = |l: usize, f: &Arc<Fb>, v: Visible| r.layers[l].check(&update(Some(f), v, &o, &n));
    assert!(chk(0, &ok, visible(640, 480, 0, 0)).is_ok());
    // No scaling.
    let mut v = visible(640, 480, 0, 0);
    v.dst.w = 320;
    assert_eq!(chk(0, &ok, v), Err(Errno::EINVAL));
    // A pitch that is not whole bus words.
    let odd = fb(&r, format::ARGB8888, 640, 480, 2556);
    assert_eq!(chk(0, &odd, visible(640, 480, 0, 0)), Err(Errno::EINVAL));
    // Too small to fetch.
    assert_eq!(chk(0, &ok, visible(2, 480, 0, 0)), Err(Errno::EINVAL));
    // A modifier other than linear.
    let mut m = fb(&r, format::ARGB8888, 640, 480, 2560);
    Arc::get_mut(&mut m).unwrap().modifier = Modifier(1);
    assert_eq!(chk(0, &m, visible(640, 480, 0, 0)), Err(Errno::EINVAL));
    // YUV on a layer that is not rich: layer 2 is simple, layer 0 is rich.
    let nv = fb(&r, format::NV12, 640, 480, 640);
    assert!(chk(0, &nv, visible(640, 480, 0, 0)).is_ok());
    assert_eq!(chk(2, &nv, visible(640, 480, 0, 0)), Err(Errno::EINVAL));
    // Chroma samples: an odd width or position.
    assert_eq!(chk(0, &nv, visible(639, 480, 0, 0)), Err(Errno::EINVAL));
    // Wider than the layer fetches.
    let wide = fb(&r, format::ARGB8888, 4200, 100, 16800);
    assert_eq!(chk(0, &wide, visible(4200, 100, 0, 0)), Err(Errno::EINVAL));
}

#[test]
fn a_pipeline_turns_on_with_its_clock_and_off_again() {
    let r = rig();
    turn_on(&r);
    assert!(r.card.dpu().running(0));
    assert_eq!(
        r.plat.pixel.lock().unwrap().as_slice(),
        &[(0, Some(148_500))]
    );
    assert_eq!(
        r.sim.live(gcu::CONTROL) & gcu::MODE_MASK,
        gcu::MODE_DISPLAY0
    );
    let p = &r.card.dpu().layout().pipes[0];
    assert_eq!(r.sim.live(p.bs.offset + bs::CONTROL) & bs::CTRL_EN, 1);
    let (old, new) = (
        CrtcSt {
            active: true,
            mode: Some(hd()),
            ..CrtcSt::default()
        },
        CrtcSt::default(),
    );
    r.crtc.commit(&CrtcUpdate {
        index: 0,
        old: &old,
        new: &new,
        modeset: true,
        s: Some(&sl()),
    });
    assert!(!r.card.dpu().running(0));
    assert_eq!(r.plat.pixel.lock().unwrap().last(), Some(&(0, None)));
}

#[test]
fn a_mode_the_clocks_cannot_serve_is_refused() {
    let r = rig();
    let mut m = hd();
    // Faster than the 800 MHz main clock.
    m.clock_khz = 900_000;
    let (old, new) = (
        CrtcSt::default(),
        CrtcSt {
            active: true,
            mode: Some(m),
            ..CrtcSt::default()
        },
    );
    let u = CrtcUpdate {
        index: 0,
        old: &old,
        new: &new,
        modeset: true,
        s: Some(&sl()),
    };
    assert_eq!(r.crtc.check(&u), Err(Errno::EINVAL));
}

#[test]
fn a_plane_shows_a_buffer_at_its_mapped_address_and_hides() {
    let r = rig();
    turn_on(&r);
    let (o, n) = (PlaneSt::default(), PlaneSt::default());
    let f = fb(&r, format::ARGB8888, 1920, 1080, 7680);
    // Showing a window: a 640x480 rectangle at (100, 50) on screen, from
    // (16, 8) of the buffer.
    let mut v = visible(640, 480, 100, 50);
    v.src.x = 16 << 16;
    v.src.y = 8 << 16;
    r.layers[0].commit(&update(Some(&f), v, &o, &n));
    r.crtc.commit(&CrtcUpdate {
        index: 0,
        old: &CrtcSt {
            active: true,
            mode: Some(hd()),
            ..CrtcSt::default()
        },
        new: &CrtcSt {
            active: true,
            mode: Some(hd()),
            ..CrtcSt::default()
        },
        modeset: false,
        s: None,
    });
    r.sim.frame();
    let p = &r.card.dpu().layout().pipes[0];
    let b = p.layers[0].block.offset;
    assert_eq!(
        u64::from(r.sim.live(b + layer::P0_PTR_LOW)),
        0x8000_0000 + 8 * 7680 + 16 * 4
    );
    assert_eq!(r.sim.live(b + layer::P0_STRIDE), 7680);
    assert_eq!(
        r.sim.live(p.cu.offset + cu::input_offset(0)),
        100 | 50 << 16
    );
    assert_eq!(r.sim.live(b + layer::CONTROL) & layer::CTRL_EN, 1);
    // The buffer was cleaned for a display that does not snoop.
    assert_eq!(r.plat.cleaned.load(Ordering::SeqCst), 1);
    // Hiding.
    r.layers[0].commit(&update(
        None,
        Visible {
            shown: false,
            src: SrcRect::default(),
            dst: DstRect::default(),
        },
        &o,
        &n,
    ));
    r.card.dpu().flush(0);
    r.sim.frame();
    assert_eq!(r.sim.live(b + layer::CONTROL) & layer::CTRL_EN, 0);
}

#[test]
fn a_buffer_is_unmapped_when_three_commits_have_replaced_it() {
    // A flip can take effect a frame after its event, so the display may
    // still read what the previous commit replaced (the board's translation
    // faults, 2026-11-23): a buffer goes when the third newer one arrives.
    let r = rig();
    turn_on(&r);
    let (o, n) = (PlaneSt::default(), PlaneSt::default());
    let live = &r.plat.live_maps;
    let a = fb(&r, format::ARGB8888, 256, 256, 1024);
    let b = fb(&r, format::ARGB8888, 256, 256, 1024);
    let c = fb(&r, format::ARGB8888, 256, 256, 1024);
    let d = fb(&r, format::ARGB8888, 256, 256, 1024);
    let v = visible(256, 256, 0, 0);
    r.layers[0].commit(&update(Some(&a), v, &o, &n));
    assert_eq!(live.load(Ordering::SeqCst), 1);
    // The same buffer again maps nothing.
    r.layers[0].commit(&update(Some(&a), v, &o, &n));
    assert_eq!(live.load(Ordering::SeqCst), 1);
    // New ones: the old are kept, as the display may still read them.
    r.layers[0].commit(&update(Some(&b), v, &o, &n));
    assert_eq!(live.load(Ordering::SeqCst), 2);
    r.layers[0].commit(&update(Some(&c), v, &o, &n));
    assert_eq!(live.load(Ordering::SeqCst), 3);
    // The fourth replaces the first for the third time: it goes.
    r.layers[0].commit(&update(Some(&d), v, &o, &n));
    assert_eq!(live.load(Ordering::SeqCst), 3);
}

#[test]
fn a_mode_change_shows_the_layers_again() {
    let r = rig();
    turn_on(&r);
    let (o, n) = (PlaneSt::default(), PlaneSt::default());
    let f = fb(&r, format::XRGB8888, 1920, 1080, 7680);
    r.layers[0].commit(&update(Some(&f), visible(1920, 1080, 0, 0), &o, &n));
    r.card.dpu().flush_wait(0).unwrap();
    let mut m = hd();
    m.clock_khz = 148_000;
    let (old, new) = (
        CrtcSt {
            active: true,
            mode: Some(hd()),
            ..CrtcSt::default()
        },
        CrtcSt {
            active: true,
            mode: Some(m),
            ..CrtcSt::default()
        },
    );
    r.crtc.commit(&CrtcUpdate {
        index: 0,
        old: &old,
        new: &new,
        modeset: true,
        s: Some(&sl()),
    });
    let p = &r.card.dpu().layout().pipes[0];
    assert!(r.card.dpu().running(0));
    // The restart took the layer off, and showing it again put it back.
    assert_eq!(
        r.sim.live(p.layers[0].block.offset + layer::CONTROL) & layer::CTRL_EN,
        1
    );
    assert_eq!(
        r.plat.pixel.lock().unwrap().as_slice(),
        &[(0, Some(148_500)), (0, None), (0, Some(148_000))]
    );
}

#[test]
fn the_interrupt_is_for_the_device_or_not() {
    let r = rig();
    assert!(!r.card.interrupt());
    turn_on(&r);
    r.crtc.vblank_enable(true);
    r.sim.frame();
    assert!(r.card.interrupt());
    assert!(!r.card.interrupt());
    let _ = vec![0u8];
}

#[test]
fn the_card_reads_as_the_specification_describes_it() {
    let r = rig();
    let k = r.card.kms().unwrap();
    // Two display controllers, eight planes.
    assert_eq!(k.crtc_ids().len(), 2);
    assert_eq!(k.plane_ids().len(), 8);
    let info = r.card.info();
    assert_eq!(info.name, "komeda");
    let d = r.card.dumb().unwrap();
    assert_eq!(d.pitch_align, 16);
    assert!(!r.card.dumb_placement().coherent);
}

#[allow(unsafe_code)]
fn sl() -> Sleepable {
    // SAFETY: host tests run on threads that may block.
    unsafe { Sleepable::assume() }
}

/// What firmware leaves: pipeline 0 scanning out `m`, layer 2 showing.
fn lit_by_firmware(r: &Rig, m: &Mode) {
    let p = r.card.dpu().layout().pipes[0].clone();
    let t = crate::timing::bs_regs(m).unwrap();
    r.sim.poke(p.bs.offset + bs::ACTIVESIZE, t.active);
    r.sim.poke(p.bs.offset + bs::HINTERVALS, t.hintervals);
    r.sim.poke(p.bs.offset + bs::VINTERVALS, t.vintervals);
    r.sim.poke(p.bs.offset + bs::SYNC, t.sync);
    r.sim.poke(p.bs.offset + bs::CONTROL, t.control);
    r.sim
        .poke(p.layers[2].block.offset + layer::CONTROL, layer::CTRL_EN);
    r.sim.poke(
        p.cu.offset + cu::INPUT_ID + 8,
        p.layers[2].block.input_id(0),
    );
    r.sim.poke(p.cu.offset + cu::input_control(2), cu::IN_EN);
}

fn modeset(r: &Rig, m: &Mode) {
    let (old, new) = (
        CrtcSt::default(),
        CrtcSt {
            active: true,
            mode: Some(*m),
            ..CrtcSt::default()
        },
    );
    let u = CrtcUpdate {
        index: 0,
        old: &old,
        new: &new,
        modeset: true,
        s: Some(&sl()),
    };
    r.crtc.check(&u).unwrap();
    r.crtc.commit(&u);
}

#[test]
fn a_display_firmware_left_running_is_kept_when_its_mode_is_asked_for() {
    let r = rig();
    lit_by_firmware(&r, &hd());
    assert!(r.card.adopt(&sl()));
    assert!(r.card.dpu().running(0));
    // Adopting touched no clock and changed no operating mode.
    assert!(r.plat.pixel.lock().unwrap().is_empty());
    assert_eq!(r.sim.live(gcu::CONTROL) & gcu::MODE_MASK, 0);
    let (o, n) = (PlaneSt::default(), PlaneSt::default());
    let f = fb(&r, format::XRGB8888, 1920, 1080, 7680);
    r.layers[0].commit(&update(Some(&f), visible(1920, 1080, 0, 0), &o, &n));
    modeset(&r, &hd());
    let p = r.card.dpu().layout().pipes[0].clone();
    // No stop, no restart, no clock change: the pipeline never went down.
    assert!(r.plat.pixel.lock().unwrap().is_empty());
    assert_eq!(r.sim.live(p.bs.offset + bs::CONTROL) & bs::CTRL_EN, 1);
    // Our layer shows, and the firmware's other one is gone.
    assert_eq!(
        r.sim.live(p.layers[0].block.offset + layer::CONTROL) & layer::CTRL_EN,
        1
    );
    assert_eq!(
        r.sim.live(p.layers[2].block.offset + layer::CONTROL) & layer::CTRL_EN,
        0
    );
    assert_eq!(r.sim.live(p.cu.offset + cu::INPUT_ID + 8), 0);
}

#[test]
fn the_console_gets_a_framebuffer_at_the_mode_and_comes_back_at_the_last_close() {
    let r = rig();
    lit_by_firmware(&r, &hd());
    assert!(r.card.adopt(&sl()));
    let p = r.card.dpu().layout().pipes[0].clone();
    let l0 = p.layers[0].block.offset;
    // The console's own framebuffer, one to one at the mode, alone.
    r.card.console(&sl());
    let given = r.plat.consoles.lock().unwrap().clone();
    assert_eq!(given.len(), 1);
    assert_eq!((given[0].width, given[0].height), (1920, 1080));
    assert_eq!(r.sim.live(l0 + layer::P0_PTR_LOW), 0);
    assert_eq!(r.sim.live(l0 + layer::P0_PTR_HIGH), 4);
    assert_eq!(r.sim.live(l0 + layer::P0_STRIDE), 1920 * 4);
    assert_eq!(r.sim.live(l0 + layer::IN_SIZE), 1920 | 1080 << 16);
    assert_eq!(
        r.sim.live(p.layers[2].block.offset + layer::CONTROL) & layer::CTRL_EN,
        0
    );
    assert_eq!(
        r.sim.live(p.cu.offset + cu::INPUT_ID),
        p.layers[0].block.input_id(0)
    );
    // A client's buffers on layers 0 and 1.
    modeset(&r, &hd());
    let (o, n) = (PlaneSt::default(), PlaneSt::default());
    let f = fb(&r, format::XRGB8888, 1920, 1080, 7680);
    r.layers[0].commit(&update(Some(&f), visible(1920, 1080, 0, 0), &o, &n));
    let g = fb(&r, format::ARGB8888, 64, 64, 256);
    r.layers[1].commit(&update(Some(&g), visible(64, 64, 10, 10), &o, &n));
    r.card.dpu().flush_wait(0).unwrap();
    assert_ne!(r.sim.live(l0 + layer::P0_PTR_HIGH), 4);
    // It quits: the console again, the client's other layer gone.
    r.card.last_close();
    assert_eq!(r.sim.live(l0 + layer::P0_PTR_LOW), 0);
    assert_eq!(r.sim.live(l0 + layer::P0_PTR_HIGH), 4);
    assert_eq!(
        r.sim.live(p.layers[1].block.offset + layer::CONTROL) & layer::CTRL_EN,
        0
    );
    // A display left in a mode of another size keeps what it shows.
    r.layers[0].commit(&update(Some(&f), visible(1920, 1080, 0, 0), &o, &n));
    r.card.dpu().flush_wait(0).unwrap();
    r.sim.poke(p.bs.offset + bs::ACTIVESIZE, 1280 | 720 << 16);
    r.card.last_close();
    assert_ne!(r.sim.live(l0 + layer::P0_PTR_HIGH), 4);
}

#[test]
fn a_different_mode_or_clock_takes_the_firmwares_display_down_first() {
    // A different size.
    let r = rig();
    lit_by_firmware(&r, &hd());
    r.card.adopt(&sl());
    let mut small = hd();
    small.hdisplay = 1280;
    small.hsync_start = 1390;
    small.hsync_end = 1430;
    small.htotal = 1650;
    small.vdisplay = 720;
    small.vsync_start = 725;
    small.vsync_end = 730;
    small.vtotal = 750;
    small.clock_khz = 74_250;
    modeset(&r, &small);
    assert_eq!(
        r.plat.pixel.lock().unwrap().as_slice(),
        &[(0, None), (0, Some(74_250))]
    );
    // The same timing at a clock firmware is not running.
    let r = rig();
    lit_by_firmware(&r, &hd());
    r.card.adopt(&sl());
    let mut fast = hd();
    fast.clock_khz = 160_000;
    modeset(&r, &fast);
    assert_eq!(
        r.plat.pixel.lock().unwrap().as_slice(),
        &[(0, None), (0, Some(160_000))]
    );
}

#[test]
fn only_the_first_mode_set_keeps_the_adopted_display() {
    let r = rig();
    lit_by_firmware(&r, &hd());
    r.card.adopt(&sl());
    modeset(&r, &hd());
    assert!(r.plat.pixel.lock().unwrap().is_empty());
    // A second mode set is an ordinary one: stop and start.
    modeset(&r, &hd());
    assert_eq!(r.plat.pixel.lock().unwrap().len(), 2);
}

#[test]
fn a_pipeline_that_is_not_running_is_not_adopted() {
    let r = rig();
    assert!(!r.card.adopt(&sl()));
    assert!(!r.card.dpu().running(0));
}

#[test]
fn a_buffer_that_cannot_be_mapped_leaves_the_firmwares_picture_alone() {
    let r = rig();
    lit_by_firmware(&r, &hd());
    r.card.adopt(&sl());
    r.plat.refuse.store(true, Ordering::SeqCst);
    let (o, n) = (PlaneSt::default(), PlaneSt::default());
    let f = fb(&r, format::XRGB8888, 1920, 1080, 7680);
    r.layers[0].commit(&update(Some(&f), visible(1920, 1080, 0, 0), &o, &n));
    modeset(&r, &hd());
    let p = r.card.dpu().layout().pipes[0].clone();
    // Firmware's layer is still showing, and the timing generator runs.
    assert_eq!(
        r.sim.live(p.layers[2].block.offset + layer::CONTROL) & layer::CTRL_EN,
        1
    );
    assert_eq!(r.sim.live(p.bs.offset + bs::CONTROL) & bs::CTRL_EN, 1);
    assert!(r.plat.pixel.lock().unwrap().is_empty());
}
