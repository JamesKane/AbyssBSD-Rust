// SPDX-License-Identifier: BSD-2-Clause
//! Host tests of the display processor's programming, against the
//! simulated device.

use std::prelude::rust_2024::*;
use std::sync::Arc;

use abi::drm::{DRM_MODE_FLAG_INTERLACE, DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_PHSYNC};
use drm_fourcc::{format, lookup};
use kms_check::Mode;

use crate::discover::{self, Error, Layout};
use crate::format::{LayerKind, lookup as hw_format, offered};
use crate::hw::{Blend, Dpu, LayerCfg, plane_addr};
use crate::regs::{BLOCK, Words, bs, common, cu, gcu, ips, layer};
use crate::sim::{Faults, Sim};
use crate::timing::{self, ModeError};

/// 1920x1080 at 60 hertz (CEA mode 16).
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
        flags: DRM_MODE_FLAG_PHSYNC | abi::drm::DRM_MODE_FLAG_PVSYNC,
        ..Mode::default()
    }
}

fn dpu() -> (Arc<Sim>, Dpu) {
    let sim = Arc::new(Sim::d6());
    let layout = discover::walk(&*sim, 0x2_0000).unwrap();
    let d = Dpu::new(sim.clone(), sim.clone(), layout);
    d.init();
    (sim, d)
}

#[test]
fn the_walk_finds_the_blocks_where_the_vendor_has_them() {
    let sim = Sim::d6();
    let l = discover::walk(&sim, 0x2_0000).unwrap();
    assert_eq!(l.product(), gcu::PRODUCT_D6);
    assert_eq!(l.pipes.len(), 2);
    let p = &l.pipes[0];
    // Specification 02 §2.17.
    assert_eq!(p.layers[0].block.offset, 0x400);
    assert_eq!(p.layers[1].block.offset, 0x600);
    assert_eq!(p.layers[3].block.offset, 0xA00);
    assert_eq!(p.cu.offset, 0xE00);
    assert_eq!(p.splitter.unwrap().offset, 0x1000);
    assert_eq!(p.scalers[0].offset, 0x1200);
    assert_eq!(p.scalers[1].offset, 0x1400);
    assert_eq!(p.merger.unwrap().offset, 0x1600);
    assert_eq!(p.ips.offset, 0x1A00);
    assert_eq!(p.bs.offset, 0x1E00);
    assert_eq!(p.layers.len(), 4);
    assert!(p.layers[0].rich && p.layers[1].rich);
    assert!(!p.layers[2].rich && !p.layers[3].rich);
    assert_eq!(p.layers[0].max_line_yuv, 2048);
    // The second pipeline follows with its own instance numbers.
    assert_eq!(l.pipes[1].layers[0].block.offset, 0x2400);
    assert_eq!(l.pipes[1].layers[0].block.info >> 4 & 0xF, 4);
    assert_eq!((l.max_line, l.max_lines), (4096, 4096));
    assert!(l.dual_link && !l.integrated_tbu);
}

#[test]
fn input_ids_name_a_block_and_its_port() {
    let sim = Sim::d6();
    let l = discover::walk(&sim, 0x2_0000).unwrap();
    // Layer 1 of pipeline 0: type 0x10, instance 1.
    assert_eq!(l.pipes[0].layers[1].block.input_id(0), 0x1010);
    // The splitter's right output.
    assert_eq!(l.pipes[0].splitter.unwrap().input_id(1), 0x2001);
}

#[test]
fn an_unknown_product_and_unknown_blocks() {
    let sim = Sim::d6();
    sim.poke(gcu::CORE_ID, 0x0099_0000);
    assert_eq!(
        discover::walk(&sim, 0x2_0000).unwrap_err(),
        Error::Product(0x99)
    );
    // A block type nobody described (an AFBC encoder) is skipped, not an
    // error.
    let sim = Sim::d6();
    sim.poke(16 * BLOCK + common::BLOCK_INFO, 0x04 << 8);
    assert!(discover::walk(&sim, 0x2_0000).is_ok());
    // A window too small for the announced blocks.
    assert_eq!(
        discover::walk(&Sim::d6(), 0x4000).unwrap_err(),
        Error::Window
    );
}

#[test]
fn a_layer_with_a_hole_is_refused() {
    let sim = Sim::d6();
    // Pipeline 0's layer 2 goes missing.
    sim.poke(4 * BLOCK + common::BLOCK_INFO, 0xFF << 8);
    assert_eq!(discover::walk(&sim, 0x2_0000).unwrap_err(), Error::Layout);
}

#[test]
fn soft_reset_finishes_or_times_out() {
    let sim = Arc::new(Sim::d6());
    discover::soft_reset(&*sim, &*sim).unwrap();
    sim.fault(Faults {
        stuck_reset: true,
        ..Faults::default()
    });
    assert_eq!(
        discover::soft_reset(&*sim, &*sim).unwrap_err(),
        Error::ResetTimeout
    );
}

#[test]
fn a_full_hd_mode_becomes_timing_registers() {
    let t = timing::bs_regs(&hd()).unwrap();
    assert_eq!(t.active, 1920 | 1080 << 16);
    // Front porch 88, back porch 148; 4 and 36.
    assert_eq!(t.hintervals, 88 | 148 << 16);
    assert_eq!(t.vintervals, 4 | 36 << 16);
    // Sync widths 44 and 5, both active high.
    assert_eq!(t.sync, 44 | 5 << 16 | bs::SYNC_HSP | bs::SYNC_VSP);
    assert_eq!(t.control, bs::CTRL_EN | bs::CTRL_VM);
    let mut neg = hd();
    neg.flags = DRM_MODE_FLAG_NHSYNC | abi::drm::DRM_MODE_FLAG_NVSYNC;
    assert_eq!(timing::bs_regs(&neg).unwrap().sync, 44 | 5 << 16);
}

#[test]
fn modes_the_hardware_cannot_show_are_refused_not_truncated() {
    let mut m = hd();
    m.flags |= DRM_MODE_FLAG_INTERLACE;
    assert_eq!(timing::bs_regs(&m).unwrap_err(), ModeError::Interlaced);
    // A back porch past 1023 would wrap silently in the field.
    let mut m = hd();
    m.htotal = m.hsync_end + 1024;
    assert_eq!(timing::bs_regs(&m).unwrap_err(), ModeError::Field);
    let mut m = hd();
    m.vtotal = m.vsync_end + 256;
    assert_eq!(timing::bs_regs(&m).unwrap_err(), ModeError::Field);
    // More than one pixel per clock can carry.
    let mut m = hd();
    m.clock_khz = 600_001;
    assert_eq!(timing::bs_regs(&m).unwrap_err(), ModeError::Clock);
    let mut m = hd();
    m.clock_khz = 0;
    assert_eq!(timing::bs_regs(&m).unwrap_err(), ModeError::Clock);
    // Out of order.
    let mut m = hd();
    m.hsync_end = m.hsync_start - 1;
    assert_eq!(timing::bs_regs(&m).unwrap_err(), ModeError::Field);
}

#[test]
fn the_main_clock_and_the_downscale_rule() {
    let m = hd();
    assert_eq!(timing::min_aclk_khz(&m, false), 148_500);
    assert_eq!(timing::min_aclk_khz(&m, true), 297_000);
    // The Sky1 pins 800 MHz: a 2:1 horizontal downscale of 1920 fits.
    assert!(timing::downscale_fits(&m, 800_000, 3840, 1080, 1080));
    // At the pixel clock itself it does not.
    assert!(!timing::downscale_fits(&m, 148_500, 3840, 1080, 1080));
}

#[test]
fn enabling_a_pipeline_writes_shadows_that_go_live_at_the_flush() {
    let (sim, d) = dpu();
    let l = d.layout().clone();
    let p = &l.pipes[0];
    d.enable(0, &hd()).unwrap();
    // Software's copy has it; the hardware's does not yet.
    assert_eq!(sim.shadow(p.bs.offset + bs::ACTIVESIZE), 1920 | 1080 << 16);
    assert_eq!(sim.live(p.bs.offset + bs::ACTIVESIZE), 0);
    assert_eq!(sim.live(gcu::CONTROL) & gcu::MODE_MASK, gcu::MODE_DISPLAY0);
    assert!(d.running(0));
    d.flush_wait(0).unwrap();
    assert_eq!(sim.live(p.bs.offset + bs::ACTIVESIZE), 1920 | 1080 << 16);
    assert_eq!(sim.live(p.bs.offset + bs::CONTROL) & bs::CTRL_EN, 1);
    assert_eq!(sim.live(p.cu.offset + cu::SIZE), 1920 | 1080 << 16);
    assert_eq!(sim.live(p.ips.offset + ips::INPUT_ID0), p.cu.input_id(0));
    assert_eq!(sim.live(p.ips.offset + ips::DEPTH), 8);
    // Pipeline 1 was never touched.
    assert_eq!(sim.live(l.pipes[1].bs.offset + bs::ACTIVESIZE), 0);
}

#[test]
fn a_pipeline_too_small_or_too_large_for_the_compositor_is_refused() {
    let (_, d) = dpu();
    let mut m = hd();
    m.hdisplay = 32;
    m.hsync_start = 40;
    m.hsync_end = 48;
    m.htotal = 64;
    assert_eq!(d.enable(0, &m).unwrap_err(), crate::hw::Error::Field);
    assert!(!d.running(0));
    assert_eq!(d.enable(5, &hd()).unwrap_err(), crate::hw::Error::Field);
}

fn argb_cfg<'a>(addr: u64, w: u32, h: u32) -> LayerCfg<'a> {
    LayerCfg {
        format: hw_format(format::ARGB8888).unwrap(),
        info: lookup(format::ARGB8888).unwrap(),
        addr: [addr, 0, 0],
        pitch: [w * 4, 0, 0],
        width: w,
        height: h,
        dst_x: 0,
        dst_y: 0,
        translated: false,
        blend: Blend::Premultiplied,
        alpha: 0xFF,
    }
}

#[test]
fn a_layer_shows_on_the_compositor_and_hides_again() {
    let (sim, d) = dpu();
    let l = d.layout().clone();
    let p = &l.pipes[0];
    d.enable(0, &hd()).unwrap();
    // A 40-bit address across both pointer words.
    let addr = 0xAB_8000_1000_u64;
    d.show(0, 0, &argb_cfg(addr, 1920, 1080)).unwrap();
    d.flush_wait(0).unwrap();
    let b = p.layers[0].block.offset;
    assert_eq!(sim.live(b + layer::P0_PTR_LOW), 0x8000_1000);
    assert_eq!(sim.live(b + layer::P0_PTR_HIGH), 0xAB);
    assert_eq!(sim.live(b + layer::P0_STRIDE), 1920 * 4);
    assert_eq!(sim.live(b + layer::FORMAT), 0x08);
    assert_eq!(sim.live(b + layer::IN_SIZE), 1920 | 1080 << 16);
    assert_eq!(sim.live(b + layer::CONTROL) & layer::CTRL_EN, 1);
    assert_eq!(sim.live(b + layer::CONTROL) & layer::CTRL_TBU_EN, 0);
    assert_eq!(sim.live(b + layer::AD_CONTROL), 0);
    assert_eq!(
        sim.live(p.cu.offset + cu::INPUT_ID),
        p.layers[0].block.input_id(0)
    );
    assert_eq!(
        sim.live(p.cu.offset + cu::input_control(0)),
        cu::IN_EN | cu::IN_PMUL | 0xFF00
    );
    assert_eq!(sim.live(p.cu.offset + cu::input_size(0)), 1920 | 1080 << 16);
    d.hide(0, 0);
    d.flush_wait(0).unwrap();
    assert_eq!(sim.live(b + layer::CONTROL) & layer::CTRL_EN, 0);
    assert_eq!(sim.live(p.cu.offset + cu::INPUT_ID), 0);
    assert_eq!(
        sim.live(p.cu.offset + cu::input_control(0)),
        cu::IN_DISABLED
    );
}

#[test]
fn a_second_layer_goes_on_top_at_its_place() {
    let (sim, d) = dpu();
    let p = d.layout().pipes[0].clone();
    d.enable(0, &hd()).unwrap();
    d.show(0, 0, &argb_cfg(0x9000_0000, 1920, 1080)).unwrap();
    let mut c = argb_cfg(0x9100_0000, 64, 64);
    c.dst_x = 100;
    c.dst_y = 200;
    c.blend = Blend::Opaque;
    c.alpha = 0x80;
    d.show(0, 1, &c).unwrap();
    d.flush_wait(0).unwrap();
    assert_eq!(sim.live(p.cu.offset + cu::input_offset(1)), 100 | 200 << 16);
    assert_eq!(
        sim.live(p.cu.offset + cu::input_control(1)),
        cu::IN_EN | cu::IN_PAD | 0x8000
    );
    // Input 1 is blended over input 0: the higher index is on top.
    assert_eq!(
        sim.live(p.cu.offset + cu::INPUT_ID + 4),
        p.layers[1].block.input_id(0)
    );
}

#[test]
fn values_that_do_not_fit_are_refused() {
    let (_, d) = dpu();
    d.enable(0, &hd()).unwrap();
    let mut c = argb_cfg(0x9000_0000, 9000, 1080);
    assert_eq!(d.show(0, 0, &c).unwrap_err(), crate::hw::Error::Field);
    c = argb_cfg(0x9000_0000, 64, 64);
    c.dst_x = 5000;
    assert_eq!(d.show(0, 0, &c).unwrap_err(), crate::hw::Error::Field);
    assert_eq!(d.show(0, 4, &c).unwrap_err(), crate::hw::Error::Field);
}

#[test]
fn stopping_takes_two_latches_and_ends_with_the_mode_off() {
    let (sim, d) = dpu();
    let p = d.layout().pipes[0].clone();
    d.enable(0, &hd()).unwrap();
    d.show(0, 0, &argb_cfg(0x9000_0000, 1920, 1080)).unwrap();
    d.flush_wait(0).unwrap();
    let before = sim.frames();
    d.disable(0).unwrap();
    // Two flushes, each waits for a frame.
    assert!(sim.frames() >= before + 2);
    assert!(!d.running(0));
    assert_eq!(sim.live(p.bs.offset + bs::CONTROL) & bs::CTRL_EN, 0);
    assert_eq!(sim.live(gcu::CONTROL) & gcu::MODE_MASK, gcu::MODE_INACTIVE);
    assert_eq!(
        sim.live(p.layers[0].block.offset + layer::CONTROL) & layer::CTRL_EN,
        0
    );
    // With the output off, a further flush no longer latches.
    d.show(0, 0, &argb_cfg(0x9000_0000, 64, 64)).unwrap();
    assert!(d.flush_wait(0).is_err());
}

#[test]
fn the_two_pipelines_choose_the_operating_mode_together() {
    let (sim, d) = dpu();
    d.enable(0, &hd()).unwrap();
    d.flush_wait(0).unwrap();
    d.enable(1, &hd()).unwrap();
    d.flush_wait(1).unwrap();
    assert_eq!(sim.live(gcu::CONTROL) & gcu::MODE_MASK, gcu::MODE_BOTH);
    d.disable(0).unwrap();
    assert_eq!(sim.live(gcu::CONTROL) & gcu::MODE_MASK, gcu::MODE_DISPLAY1);
    d.disable(1).unwrap();
    assert_eq!(sim.live(gcu::CONTROL) & gcu::MODE_MASK, gcu::MODE_INACTIVE);
}

#[test]
fn a_mode_that_does_not_take_or_a_latch_that_never_comes_is_an_error() {
    let (sim, d) = dpu();
    sim.fault(Faults {
        stuck_mode: true,
        ..Faults::default()
    });
    assert_eq!(d.enable(0, &hd()).unwrap_err(), crate::hw::Error::Mode);
    let (sim, d) = dpu();
    d.enable(0, &hd()).unwrap();
    sim.fault(Faults {
        never_latch: true,
        ..Faults::default()
    });
    assert_eq!(d.flush_wait(0).unwrap_err(), crate::hw::Error::Latch);
}

#[test]
fn the_interrupt_reports_latches_and_frames_and_clears_them() {
    let (sim, d) = dpu();
    d.enable(0, &hd()).unwrap();
    // The latch interrupt is unmasked from the start; frames only when
    // asked for.
    d.flush(0);
    sim.frame();
    assert!(sim.irq_pending());
    let ev = d.interrupt();
    assert_eq!(ev.latched, [true, false]);
    assert_eq!(ev.frame, [false, false]);
    assert!(!sim.irq_pending());
    assert_eq!(d.interrupt(), crate::hw::Events::default());
    d.frame_interrupt(0, true);
    sim.frame();
    let ev = d.interrupt();
    assert_eq!(ev.frame, [true, false]);
    assert!(!ev.latched[0]);
    d.frame_interrupt(0, false);
    sim.frame();
    assert!(!sim.irq_pending());
}

#[test]
fn plane_addresses_follow_the_chroma_subsampling() {
    let nv12 = lookup(format::NV12).unwrap();
    // Luma: one byte per pixel; chroma: a pair per 2x2 pixels.
    assert_eq!(
        plane_addr(nv12, 0, 0x1000, 1920, 10, 4),
        0x1000 + 4 * 1920 + 10
    );
    assert_eq!(
        plane_addr(nv12, 1, 0x20_0000, 1920, 10, 4),
        0x20_0000 + 2 * 1920 + 5 * 2
    );
    let argb = lookup(format::ARGB8888).unwrap();
    assert_eq!(plane_addr(argb, 0, 0, 7680, 3, 2), 2 * 7680 + 12);
}

#[test]
fn the_format_table_is_consistent() {
    // No two formats share an identifier or a FourCC.
    for (i, a) in crate::format::FORMATS.iter().enumerate() {
        for b in &crate::format::FORMATS[i + 1..] {
            assert_ne!(a.id, b.id);
            assert_ne!(a.fourcc, b.fourcc);
        }
        assert!(
            lookup(a.fourcc).is_some(),
            "{:?} is a format the core knows",
            a.fourcc
        );
    }
    let rich = offered(LayerKind::Rich);
    let simple = offered(LayerKind::Simple);
    assert!(rich.contains(&format::NV12) && !simple.contains(&format::NV12));
    assert!(simple.contains(&format::R8) && !rich.contains(&format::R8));
    assert!(offered(LayerKind::WriteBack).contains(&format::XRGB8888));
    // The identifiers the specification lists.
    assert_eq!(hw_format(format::XRGB8888).unwrap().id, 0x10);
    assert_eq!(hw_format(format::RGB565).unwrap().id, 0x22);
}

#[test]
fn the_sticky_errors_are_read_and_cleared() {
    let (sim, d) = dpu();
    let p = d.layout().pipes[0].clone();
    // An underrun is raised on the display output.
    sim.poke(p.dou.offset + common::IRQ_RAW_STATUS, 1 << 8);
    sim.poke(p.dou.offset + common::STATUS, 0x8000_0003);
    let ev = d.interrupt();
    assert!(ev.underrun[0] && ev.errors == 1);
    assert_eq!(sim.live(p.dou.offset + common::STATUS), 0x8000_0000);
    assert_eq!(sim.live(p.dou.offset + common::IRQ_RAW_STATUS), 0);
    let l: &Layout = d.layout();
    assert_eq!(l.pipes.len(), 2);
    let _ = (Words::read(&*sim, gcu::CORE_ID), BLOCK);
}
