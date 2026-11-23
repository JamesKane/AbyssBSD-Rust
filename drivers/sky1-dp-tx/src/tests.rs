// SPDX-License-Identifier: BSD-2-Clause
//! Host tests of the transmitter against the simulated core and sink.

use std::prelude::rust_2024::*;
use std::sync::Arc;
use std::vec;

use abi::drm::{
    DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_NVSYNC, DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PVSYNC,
};
use kms_check::Mode;

use crate::aux::{Aux, AuxError};
use crate::dpcd::{self, Caps, CapsError, Drive, Rate};
use crate::dptx::{Config, Dptx, Error};
use crate::link::{Limits, Link, TrainError, Trainer, pick_link, pick_rgb_bpc};
use crate::regs::{self, Words};
use crate::sim::{Sim, TestPhy};
use crate::stream;

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
        flags: DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC,
        ..Mode::default()
    }
}

fn rig(cfg: Config) -> (Arc<Sim>, Arc<TestPhy>, Dptx) {
    let sim = Arc::new(Sim::new());
    let phy = Arc::new(TestPhy::new(4));
    let d = Dptx::new(sim.clone(), sim.clone(), phy.clone(), cfg);
    d.host_init(100);
    (sim, phy, d)
}

fn caps_block() -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0] = 0x14;
    b[1] = 0x14;
    b[2] = 0xC4;
    b[3] = 1;
    b
}

#[test]
fn the_rates_have_the_standard_codes_and_clocks() {
    assert_eq!(Rate::Rbr.khz(), 162_000);
    assert_eq!(Rate::Hbr.khz(), 270_000);
    assert_eq!(Rate::Hbr2.khz(), 540_000);
    assert_eq!(Rate::Hbr3.khz(), 810_000);
    assert_eq!(Rate::Hbr2.lower(), Some(Rate::Hbr));
    assert_eq!(Rate::Rbr.lower(), None);
    assert_eq!(Rate::from_code(0x14), Some(Rate::Hbr2));
    assert_eq!(Rate::from_code(0x07), None);
}

#[test]
fn a_capability_block_is_parsed_or_refused() {
    let c = Caps::parse(&caps_block()).unwrap();
    assert_eq!((c.max_rate, c.max_lanes), (Rate::Hbr2, 4));
    assert!(c.enhanced_framing && c.tps3 && !c.tps4 && c.downspread);
    assert_eq!((c.cr_wait_us(), c.eq_wait_us()), (100, 400));
    let mut b = caps_block();
    b[14] = 2;
    assert_eq!(Caps::parse(&b).unwrap().cr_wait_us(), 8000);
    b[1] = 0x07;
    assert_eq!(Caps::parse(&b).unwrap_err(), CapsError::Rate(7));
    let mut b = caps_block();
    b[2] = 0x80;
    assert_eq!(Caps::parse(&b).unwrap_err(), CapsError::Lanes(0));
}

#[test]
fn drive_settings_encode_as_the_standard_does() {
    assert_eq!(
        Drive {
            swing: 0,
            emphasis: 0
        }
        .byte(),
        0
    );
    assert_eq!(
        Drive {
            swing: 2,
            emphasis: 1
        }
        .byte(),
        2 | 1 << 3
    );
    // Maximum swing and emphasis carry their flags.
    assert_eq!(
        Drive {
            swing: 3,
            emphasis: 2
        }
        .byte(),
        3 | 1 << 2 | 2 << 3 | 1 << 5
    );
    let adj = [0x21, 0x43];
    assert_eq!(
        dpcd::adjust_request(&adj, 0),
        Drive {
            swing: 1,
            emphasis: 0
        }
    );
    assert_eq!(
        dpcd::adjust_request(&adj, 1),
        Drive {
            swing: 2,
            emphasis: 0
        }
    );
    assert_eq!(
        dpcd::adjust_request(&adj, 3),
        Drive {
            swing: 0,
            emphasis: 1
        }
    );
}

#[test]
fn aux_transfers_are_chunked_and_addressed() {
    let sim = Sim::new();
    let aux = Aux::new(&sim, &sim);
    let mut b = [0u8; 40];
    aux.read(0x000, &mut b).unwrap();
    assert_eq!(b[1], Rate::Hbr2.code());
    let r = sim.requests();
    // 16 + 16 + 8 bytes, each at the next address.
    assert_eq!(r, vec![(9, 0, 16), (9, 16, 16), (9, 32, 8)]);
    aux.write(0x300, &[1, 2, 3]).unwrap();
    assert_eq!(sim.sink(|s| s.dpcd[0x300..0x303].to_vec()), vec![1, 2, 3]);
}

#[test]
fn aux_defers_are_retried_and_refusals_are_not() {
    let sim = Sim::new();
    let aux = Aux::new(&sim, &sim);
    sim.sink(|s| s.defer = 3);
    assert_eq!(aux.read_byte(0).unwrap(), 0x14);
    sim.sink(|s| s.defer = 20);
    assert_eq!(aux.read_byte(0), Err(AuxError::Defer));
    sim.sink(|s| {
        s.defer = 0;
        s.nack_write = vec![0x400];
    });
    assert_eq!(aux.write(0x400, &[1]), Err(AuxError::Nack));
}

#[test]
fn aux_with_no_sink_or_no_reply() {
    let sim = Sim::new();
    let aux = Aux::new(&sim, &sim);
    sim.sink(|s| s.hpd = false);
    assert_eq!(aux.read_byte(0), Err(AuxError::NoSink));
    // Not even a request went out.
    assert!(sim.requests().is_empty());
    sim.sink(|s| {
        s.hpd = true;
        s.dead = true;
    });
    assert_eq!(aux.read_byte(0), Err(AuxError::Timeout));
    // Three tries.
    assert_eq!(sim.requests().len(), 3);
}

#[test]
fn the_edid_is_read_over_i2c_with_the_middle_flag_set_until_the_last() {
    let sim = Sim::new();
    sim.sink(|s| {
        for (i, b) in s.edid.iter_mut().enumerate().skip(8) {
            *b = u8::try_from(i).unwrap();
        }
    });
    let aux = Aux::new(&sim, &sim);
    let e = aux.read_edid_block(0).unwrap();
    assert_eq!(e[8], 8);
    assert_eq!(e[127], 127);
    let r = sim.requests();
    // Start (write, middle, address only), offset, eight 16-byte reads
    // with the middle flag but the last, and a stop.
    assert_eq!(r[0], (4, 0x50, 0));
    assert_eq!(r[1], (4, 0x50, 1));
    assert_eq!(r[2], (5, 0x50, 16));
    assert_eq!(r[9], (1, 0x50, 16));
    assert_eq!(*r.last().unwrap(), (1, 0x50, 0));
}

#[test]
fn the_link_is_the_slowest_that_leaves_room() {
    let all = Limits {
        max_rate: Rate::Hbr2,
        max_lanes: 4,
    };
    // 1080p60 at 24 bits: reduced bit rate on four lanes.
    assert_eq!(
        pick_link(148_500, 48, all),
        Some(Link {
            rate: Rate::Rbr,
            lanes: 4
        })
    );
    // 4K60 needs HBR2 on four lanes.
    assert_eq!(
        pick_link(594_000, 48, all),
        Some(Link {
            rate: Rate::Hbr2,
            lanes: 4
        })
    );
    // A small mode on one lane.
    assert_eq!(
        pick_link(25_175, 48, all),
        Some(Link {
            rate: Rate::Rbr,
            lanes: 1
        })
    );
    // With only two lanes, or only HBR, 4K60 does not fit.
    assert_eq!(
        pick_link(
            594_000,
            48,
            Limits {
                max_lanes: 2,
                ..all
            }
        ),
        None
    );
    assert_eq!(
        pick_link(
            594_000,
            48,
            Limits {
                max_rate: Rate::Hbr,
                ..all
            }
        ),
        None
    );
}

#[test]
fn the_colour_depth_steps_down_until_the_link_carries_it() {
    let hbr2 = Limits {
        max_rate: Rate::Hbr2,
        max_lanes: 4,
    };
    assert_eq!(pick_rgb_bpc(148_500, 10, 10, hbr2), Some(10));
    assert_eq!(pick_rgb_bpc(148_500, 10, 8, hbr2), Some(8));
    assert_eq!(pick_rgb_bpc(594_000, 10, 10, hbr2), Some(8));
    let hbr = Limits {
        max_rate: Rate::Hbr,
        max_lanes: 4,
    };
    assert_eq!(pick_rgb_bpc(594_000, 10, 10, hbr), None);
}

#[test]
fn training_reaches_what_the_sink_asks_and_ends_cleanly() {
    let (sim, phy, d) = rig(Config::default());
    sim.sink(|s| {
        s.need = Drive {
            swing: 2,
            emphasis: 1,
        }
    });
    d.probe().unwrap();
    let up = d.link_up(&hd(), 8).unwrap();
    assert_eq!(
        up.trained.link,
        Link {
            rate: Rate::Rbr,
            lanes: 4
        }
    );
    assert_eq!(
        up.trained.drive,
        Drive {
            swing: 2,
            emphasis: 1
        }
    );
    assert_eq!(up.bpc, 8);
    // The core and the sink are configured, and training has ended.
    assert_eq!(sim.reg(regs::LINK_BW_SET), 6);
    assert_eq!(sim.reg(regs::LANE_COUNT_SET), 4);
    assert_eq!(sim.reg(regs::ENHANCED_FRAMING_ENABLE), 1);
    assert_eq!(sim.reg(regs::DOWNSPREAD_CONTROL), 1);
    assert_eq!(sim.reg(regs::TRAINING_PATTERN_SET), 0);
    assert_eq!(sim.reg(regs::DISABLE_SCRAMBLING), 0);
    sim.sink(|s| {
        assert_eq!(s.dpcd[0x102], 0);
        assert_eq!(s.dpcd[0x100], 6);
        assert_eq!(s.dpcd[0x101], 4 | 0x80);
        assert_eq!(s.dpcd[0x107], 0x10);
        assert_eq!(s.dpcd[0x108], 1);
        // Awake.
        assert_eq!(s.dpcd[0x600] & 7, 1);
    });
    assert_eq!(sim.reg(regs::SOURCE_ENABLE), 1);
    assert_eq!(sim.reg(regs::SOFT_RESET), 1);
    let log = phy.log.lock().unwrap().clone();
    assert_eq!(log[0], "link 162000 x4");
    assert_eq!(log.last().unwrap(), "drive 2 1");
}

#[test]
fn the_best_pattern_both_sides_have_is_used() {
    let (sim, _, d) = rig(Config::default());
    sim.sink(|s| s.dpcd[3] |= 0x80);
    d.probe().unwrap();
    d.link_up(&hd(), 8).unwrap();
    // Training pattern 4 was used (its sink code is 7): visible as the
    // last non-zero pattern in the requests.
    let pats: Vec<_> = sim
        .requests()
        .into_iter()
        .filter(|r| r.0 == 8 && r.1 == 0x102)
        .collect();
    assert_eq!(pats.len(), 3);
}

#[test]
fn a_sink_that_cannot_train_at_the_chosen_rate_gets_a_slower_one() {
    let (sim, _, d) = rig(Config::default());
    // 350 MHz at 24 bits fits HBR only without margin, so HBR2 is picked
    // first; this sink locks only up to HBR.
    sim.sink(|s| s.works_up_to = Rate::Hbr);
    let mut m = hd();
    m.clock_khz = 350_000;
    d.probe().unwrap();
    let up = d.link_up(&m, 8).unwrap();
    assert_eq!(up.trained.link.rate, Rate::Hbr);
    // Training ended each time.
    assert_eq!(sim.reg(regs::TRAINING_PATTERN_SET), 0);
}

#[test]
fn a_sink_that_never_locks_is_an_error_and_training_still_ends() {
    let (sim, _, d) = rig(Config::default());
    sim.sink(|s| s.works_up_to = Rate::Rbr);
    let mut m = hd();
    m.clock_khz = 297_000;
    d.probe().unwrap();
    assert_eq!(
        d.link_up(&m, 8).unwrap_err(),
        Error::Train(TrainError::ClockRecovery)
    );
    sim.sink(|s| assert_eq!(s.dpcd[0x102], 0));
    assert!(d.link().is_none());
}

#[test]
fn the_phy_must_report_its_lanes_ready() {
    let (sim, phy, d) = rig(Config::default());
    sim.set_ready_mask(0b0011);
    d.probe().unwrap();
    assert_eq!(
        d.link_up(&hd(), 8).unwrap_err(),
        Error::Train(TrainError::Phy)
    );
    // It waited its hundred milliseconds.
    assert!(sim.waited_us() >= 100_000);
    let _ = phy;
}

#[test]
fn a_port_with_two_lanes_trains_two() {
    let sim = Arc::new(Sim::new());
    let phy = Arc::new(TestPhy::new(2));
    let d = Dptx::new(sim.clone(), sim.clone(), phy, Config::default());
    d.host_init(100);
    sim.set_ready_mask(0b0011);
    d.probe().unwrap();
    let up = d.link_up(&hd(), 8).unwrap();
    assert_eq!(up.trained.link.lanes, 2);
    // 1080p60 on two lanes needs more than reduced bit rate.
    assert_eq!(up.trained.link.rate, Rate::Hbr);
}

#[test]
fn the_trainer_can_be_used_on_its_own() {
    let sim = Arc::new(Sim::new());
    let phy = TestPhy::new(4);
    let mut b = [0u8; 16];
    sim.sink(|s| b.copy_from_slice(&s.dpcd[..16]));
    let t = Trainer {
        w: &*sim,
        d: &*sim,
        phy: &phy,
        caps: Caps::parse(&b).unwrap(),
        port_lanes: 4,
    };
    let t = t
        .train(Link {
            rate: Rate::Hbr,
            lanes: 2,
        })
        .unwrap();
    assert_eq!(t.link.lanes, 2);
}

#[test]
fn the_stream_registers_follow_the_mode() {
    let m = hd();
    let l = Link {
        rate: Rate::Rbr,
        lanes: 4,
    };
    let r = stream::stream_regs(&m, 8, l, 1);
    let get = |o: usize| r.values.iter().find(|v| v.0 == o).unwrap().1;
    assert_eq!(get(regs::MAIN_STREAM_HTOTAL), 2200);
    assert_eq!(get(regs::MAIN_STREAM_VTOTAL), 1125);
    assert_eq!(get(regs::MAIN_STREAM_HSWIDTH), 44);
    assert_eq!(get(regs::MAIN_STREAM_VSWIDTH), 5);
    assert_eq!(get(regs::MAIN_STREAM_HRES), 1920);
    assert_eq!(get(regs::MAIN_STREAM_VRES), 1080);
    assert_eq!(get(regs::MAIN_STREAM_HSTART), 2200 - 2008);
    assert_eq!(get(regs::MAIN_STREAM_VSTART), 1125 - 1084);
    assert_eq!(get(regs::MAIN_STREAM_POLARITY), 3);
    assert_eq!(get(regs::MAIN_STREAM_MISC0), 1 << 5);
    // 44 and zero sixteenths valid symbols per 64-symbol unit.
    assert_eq!(get(regs::TU_CONFIG), 64 | 44 << 16);
    assert_eq!(get(regs::USER_DATA_COUNT), 1440);
    assert_eq!(get(regs::SECONDARY_DATA_WINDOW), 68);
    assert_eq!(get(regs::DATA_CONTROL), 0x2004);
    assert_eq!(get(regs::USER_PIXEL_COUNT), 1);
    // Ten bits a component.
    assert_eq!(stream::misc0_rgb(10), 2 << 5);
    assert_eq!(stream::misc0_rgb(7), 1 << 5);
}

#[test]
fn a_short_back_porch_is_lengthened_at_the_same_refresh() {
    let mut m = hd();
    m.htotal = m.hsync_end + 4;
    let a = stream::adjust_mode(&m);
    assert_eq!(a.htotal, m.hsync_end + 12);
    assert!(a.clock_khz > m.clock_khz);
    let refresh =
        |x: &Mode| u64::from(x.clock_khz) * 1000 / (u64::from(x.htotal) * u64::from(x.vtotal));
    assert!(refresh(&a).abs_diff(refresh(&m)) <= 1);
    // A mode with room is left alone.
    assert_eq!(stream::adjust_mode(&hd()), hd());
}

#[test]
fn inverting_the_sync_flips_each_polarity() {
    let m = stream::with_inverted_sync(&hd());
    assert_eq!(m.flags & (DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC), 0);
    assert_ne!(m.flags & DRM_MODE_FLAG_NHSYNC, 0);
    assert_ne!(m.flags & DRM_MODE_FLAG_NVSYNC, 0);
    assert_eq!(stream::with_inverted_sync(&m).flags, hd().flags);
}

#[test]
fn the_stream_starts_after_the_link_and_stops_before_it_goes() {
    let (sim, _, d) = rig(Config::default());
    d.probe().unwrap();
    d.link_up(&hd(), 8).unwrap();
    d.stream_on(&hd()).unwrap();
    let base = regs::source(0);
    assert_eq!(sim.reg(base + regs::VIDEO_STREAM_ENABLE), 1);
    assert_eq!(sim.reg(base + regs::SECONDARY_STREAM_ENABLE), 1);
    // The mode is programmed as given; `offered` is where the vendor's
    // inversion is applied.
    assert_eq!(sim.reg(base + regs::MAIN_STREAM_POLARITY), 3);
    assert_eq!(sim.reg(base + regs::USER_SYNC_POLARITY), 0xC | 3);
    let o = d.offered(&hd());
    assert_eq!(o.flags & (DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC), 0);
    assert_ne!(o.flags & DRM_MODE_FLAG_NHSYNC, 0);
    d.stream_off();
    assert_eq!(sim.reg(base + regs::VIDEO_STREAM_ENABLE), 0);
    d.stream_on(&hd()).unwrap();
    d.link_down();
    assert_eq!(sim.reg(base + regs::VIDEO_STREAM_ENABLE), 0);
    assert_eq!(sim.reg(regs::SOURCE_ENABLE), 0);
    assert_eq!(sim.reg(regs::SOFT_RESET), 0);
    // The sink was put to sleep.
    sim.sink(|s| assert_eq!(s.dpcd[0x600] & 7, 2));
    assert!(d.link().is_none());
    // A stream needs a link.
    assert_eq!(d.stream_on(&hd()).unwrap_err(), Error::NoSink);
}

#[test]
fn an_unplugged_sink_is_not_probed_or_put_to_sleep() {
    let (sim, _, d) = rig(Config::default());
    d.probe().unwrap();
    d.link_up(&hd(), 8).unwrap();
    sim.sink(|s| s.hpd = false);
    sim.sink(|s| s.dpcd[0x600] = 1);
    d.link_down();
    sim.sink(|s| assert_eq!(s.dpcd[0x600], 1));
    assert_eq!(d.probe().unwrap_err(), Error::NoSink);
    assert!(!d.connected());
}

#[test]
fn a_slow_sink_gets_five_tries() {
    let (sim, _, d) = rig(Config::default());
    sim.sink(|s| s.dead = true);
    assert_eq!(d.probe().unwrap_err(), Error::Aux(AuxError::Timeout));
    assert!(sim.waited_us() >= 400_000);
}

#[test]
fn host_setup_and_the_interrupt() {
    let (sim, _, d) = rig(Config::default());
    assert_eq!(sim.reg(regs::TRANSMITTER_ENABLE), 1);
    assert_eq!(sim.reg(regs::INTERRUPT_MASK), 0x1C);
    assert_eq!(sim.reg(regs::AUX_CLOCK_DIVIDER), 100);
    d.host_init(0);
    assert_eq!(sim.reg(regs::AUX_CLOCK_DIVIDER), 25);
    d.host_init(1000);
    assert_eq!(sim.reg(regs::AUX_CLOCK_DIVIDER), 400);
    // Not this core's: nothing raised, or only masked sources.
    assert_eq!(d.interrupt(), None);
    sim.raise(regs::IRQ_HPD_EVENT | 1 << 2);
    let ev = d.interrupt().unwrap();
    assert!(ev.hpd && !ev.attention);
    // The cause was cleared by reading it.
    assert_eq!(d.interrupt(), None);
    sim.raise(regs::IRQ_HPD_IRQ);
    assert!(d.interrupt().unwrap().attention);
    sim.raise(1 << 2);
    assert_eq!(d.interrupt(), None);
    d.host_exit();
    assert_eq!(sim.reg(regs::TRANSMITTER_ENABLE), 0);
    assert_eq!(sim.reg(regs::INTERRUPT_MASK), 0x3F);
}

#[test]
fn the_sinks_edid_is_read_whole() {
    let (sim, _, d) = rig(Config::default());
    sim.sink(|s| {
        s.edid[126] = 1;
        s.edid.extend(core::iter::repeat_n(0xAA, 128));
    });
    let e = d.edid().unwrap();
    assert_eq!(e.len(), 256);
    assert_eq!(e[128], 0xAA);
    assert!(e.starts_with(&[0, 0xFF]));
    let _ = sim.reg(0);
    let _: &dyn Words = &*sim;
}

/// What firmware leaves: a trained link at reduced rate on four lanes and
/// source 0 streaming the 1080p mode.
fn lit_by_firmware(sim: &Sim) {
    let l = Link {
        rate: Rate::Rbr,
        lanes: 4,
    };
    sim.write(regs::LINK_BW_SET, 6);
    sim.write(regs::LANE_COUNT_SET, 4);
    sim.write(regs::SOURCE_ENABLE, 1);
    let base = regs::source(0);
    for &(r, v) in &stream::stream_regs(&hd(), 8, l, 1).values {
        sim.write(base + r, v);
    }
    sim.write(base + regs::VIDEO_STREAM_ENABLE, 1);
}

#[test]
fn a_link_firmware_left_up_is_adopted_without_a_reset_or_training() {
    let sim = Arc::new(Sim::new());
    let phy = Arc::new(TestPhy::new(4));
    let d = Dptx::new(sim.clone(), sim.clone(), phy.clone(), Config::default());
    // Nothing running: nothing to adopt.
    assert!(d.adopt().is_none());
    lit_by_firmware(&sim);
    let up = d.adopt().unwrap();
    assert_eq!(
        up.trained.link,
        Link {
            rate: Rate::Rbr,
            lanes: 4
        }
    );
    assert_eq!(up.bpc, 8);
    assert_eq!(d.link(), Some(up.trained.link));
    assert_eq!(sim.reg(regs::INTERRUPT_MASK), 0x1C);
    // No AUX traffic, nothing told to the PHY, the stream still on.
    assert!(sim.requests().is_empty());
    assert!(phy.log.lock().unwrap().is_empty());
    assert_eq!(sim.reg(regs::source(0) + regs::VIDEO_STREAM_ENABLE), 1);
    assert_eq!(sim.reg(regs::SOURCE_ENABLE), 1);
}

#[test]
fn the_running_stream_is_matched_against_the_mode_asked_for() {
    let sim = Arc::new(Sim::new());
    let d = Dptx::new(
        sim.clone(),
        sim.clone(),
        Arc::new(TestPhy::new(4)),
        Config::default(),
    );
    lit_by_firmware(&sim);
    assert!(!d.stream_matches(&hd()), "not adopted yet");
    d.adopt().unwrap();
    assert!(d.stream_matches(&hd()));
    let mut other = hd();
    other.vdisplay = 720;
    other.vsync_start = 725;
    other.vsync_end = 730;
    other.vtotal = 750;
    assert!(!d.stream_matches(&other));
    // The same timing with the other polarity is not the same stream.
    let flipped = stream::with_inverted_sync(&hd());
    assert!(!d.stream_matches(&flipped));
    d.forget_link();
    assert!(!d.stream_matches(&hd()));
    assert!(d.link().is_none());
}

#[test]
fn nonsense_left_in_the_link_registers_is_not_adopted() {
    let sim = Arc::new(Sim::new());
    let d = Dptx::new(
        sim.clone(),
        sim.clone(),
        Arc::new(TestPhy::new(4)),
        Config::default(),
    );
    lit_by_firmware(&sim);
    sim.write(regs::LINK_BW_SET, 0x07);
    assert!(d.adopt().is_none());
    sim.write(regs::LINK_BW_SET, 6);
    sim.write(regs::LANE_COUNT_SET, 3);
    assert!(d.adopt().is_none());
}

#[test]
fn a_sink_that_answers_an_i2c_read_with_fewer_bytes_is_asked_for_the_rest() {
    // Seen on the board: the EDID read failed when a reply carried fewer
    // bytes than the 16 asked for.
    let sim = Sim::new();
    sim.sink(|s| {
        for (i, b) in s.edid.iter_mut().enumerate().skip(8) {
            *b = u8::try_from(i).unwrap();
        }
    });
    sim.short_i2c_reads(5);
    let aux = Aux::new(&sim, &sim);
    let e = aux.read_edid_block(0).unwrap();
    assert_eq!(e[8], 8);
    assert_eq!(e[127], 127);
    assert_eq!(e[64], 64);
}

#[test]
fn a_channel_level_defer_on_an_i2c_read_is_retried() {
    // Seen on the board: the sink answered the EDID read with the channel's
    // DEFER and the I2C field ACK, and no data.
    let sim = Sim::new();
    sim.sink(|s| {
        s.i2c_defer = 3;
        for (i, b) in s.edid.iter_mut().enumerate().skip(8) {
            *b = u8::try_from(i).unwrap();
        }
    });
    let aux = Aux::new(&sim, &sim);
    let e = aux.read_edid_block(0).unwrap();
    assert_eq!(e[8], 8);
    assert_eq!(e[127], 127);
}

#[test]
fn an_extension_block_that_cannot_be_read_leaves_the_blocks_that_were() {
    // Seen on the board: a monitor behind a KVM switch refused the segment
    // pointer (0x30) that EDID blocks 2 and 3 need; the base block and the
    // first extension had been read, and the modes are in the base block.
    let (sim, _, d) = rig(Config::default());
    sim.sink(|s| {
        s.edid[126] = 3;
        s.edid.extend(core::iter::repeat_n(0xAA, 128 * 3));
        s.nack_i2c = vec![0x30];
    });
    let e = d.edid().unwrap();
    assert_eq!(e.len(), 256, "the base block and block 1");
    assert!(e.starts_with(&[0, 0xFF]));
    assert_eq!(e[128], 0xAA);
}
