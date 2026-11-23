// SPDX-License-Identifier: BSD-2-Clause
//! Host tests of the combo PHY against a simulated register file and
//! interface window.

use std::collections::HashMap;
use std::format;
use std::prelude::rust_2024::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::vec;

use sky1_dp_tx::dpcd::{Drive, Rate};
use sky1_dp_tx::link::Phy;

use crate::phy::{Error, Platform, Regs16, UsbDpPhy, Words32};
use crate::tables::{self, Mode, Orientation, lane};

/// One ordered log of everything the PHY did.
#[derive(Default)]
struct World {
    log: Mutex<Vec<String>>,
    regs: Mutex<HashMap<u32, u16>>,
    iface: Mutex<HashMap<usize, u32>>,
    pclk_failures: AtomicU32,
    pclk_calls: AtomicU32,
}

impl World {
    fn note(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }
    fn reg(&self, i: u32) -> u16 {
        *self.regs.lock().unwrap().get(&i).unwrap_or(&0)
    }
    fn count(&self, prefix: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(prefix))
            .count()
    }
    fn position(&self, what: &str) -> Option<usize> {
        self.log.lock().unwrap().iter().position(|l| l == what)
    }
}

struct R(Arc<World>);
impl Regs16 for R {
    fn read(&self, i: u32) -> u16 {
        match i {
            // PLL1 locked whenever it is released to run.
            tables::CMN_CTRL2 => {
                if self.0.reg(tables::PLL_CTRL) & tables::PLL1_RUN != 0 {
                    tables::PLL1_LOCKED
                } else {
                    0
                }
            }
            _ => self.0.reg(i),
        }
    }
    fn write(&self, i: u32, v: u16) {
        self.0.regs.lock().unwrap().insert(i, v);
        if i == 0xE003 {
            self.0.note(format!("pll_ctrl {v:#x}"));
        } else if i == 0x01F0 {
            self.0.note("table done".into());
        }
    }
}

struct I(Arc<World>);
impl Words32 for I {
    fn read(&self, off: usize) -> u32 {
        let m = self.0.iface.lock().unwrap();
        let g = |o: usize| *m.get(&o).unwrap_or(&0);
        match off {
            0xFC => 0x0054_0000,
            // Common ready once the interface is released.
            0x44 => g(0x40) & 1,
            // The acknowledges follow their requests.
            0x64 => g(0x60),
            0x6C => g(0x68) & 1,
            o => g(o),
        }
    }
    fn write(&self, off: usize, v: u32) {
        self.0.iface.lock().unwrap().insert(off, v);
        match off {
            0x60 if v != 0 => self.0.note(format!("state {v:#x}")),
            0x50 => self.0.note(format!("data {v}")),
            0x68 => self.0.note(format!("pllclk {v}")),
            _ => {}
        }
    }
}

struct P(Arc<World>);
impl Platform for P {
    fn set_orientation(&self, flipped: bool) {
        self.0.note(format!("orientation {}", u8::from(flipped)));
    }
    fn assert_resets(&self) {
        self.0.note("assert".into());
    }
    fn release_apb_reset(&self) {
        self.0.note("apb".into());
    }
    fn release_core_reset(&self) {
        self.0.note("core".into());
    }
    fn pclk_enable(&self) -> bool {
        self.0.pclk_calls.fetch_add(1, Ordering::SeqCst);
        if self.0.pclk_failures.load(Ordering::SeqCst) > 0 {
            self.0.pclk_failures.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        self.0.note("pclk".into());
        true
    }
    fn pclk_disable(&self) {
        self.0.note("pclk off".into());
    }
    fn delay_us(&self, _: u64) {}
}

fn rig(mode: Mode) -> (Arc<World>, UsbDpPhy) {
    let w = Arc::new(World::default());
    let phy = UsbDpPhy::new(
        Arc::new(R(w.clone())),
        Arc::new(I(w.clone())),
        Arc::new(P(w.clone())),
        mode,
    );
    (w, phy)
}

fn writes_to(t: &[(u32, u16)], r: u32) -> Vec<u16> {
    t.iter().filter(|w| w.0 == r).map(|w| w.1).collect()
}

#[test]
fn the_dp_only_table_has_four_dp_lanes_and_the_dp_map() {
    let t = tables::init_table(Mode::DpOnly, Orientation::Normal);
    assert_eq!(*t.last().unwrap(), (0x01F0, 0x0100));
    assert_eq!(writes_to(&t, 0xC010), vec![0x51D9]);
    assert_eq!(writes_to(&t, 0x01C1), vec![0x0701]);
    for l in 0..4 {
        assert_eq!(writes_to(&t, lane(0x40E6, l)), vec![1]);
        assert_eq!(writes_to(&t, lane(0x40E7, l)), vec![1]);
        assert_eq!(writes_to(&t, lane(0x4100, l)), vec![0x00FB]);
    }
    // Both orientations use one table.
    assert_eq!(t, tables::init_table(Mode::DpOnly, Orientation::Flipped));
}

#[test]
fn the_shared_table_puts_usb_on_one_side_and_dp_on_the_other() {
    let n = tables::init_table(Mode::UsbDp, Orientation::Normal);
    assert_eq!(writes_to(&n, 0xC010), vec![0x5100]);
    assert_eq!(writes_to(&n, 0x01C1), vec![0x0601]);
    // USB transmit on PMA 0, receive on 1; DP on 2 and 3.
    assert_eq!(writes_to(&n, lane(0x4100, 0)), vec![0x02FF]);
    assert_eq!(writes_to(&n, lane(0x8000, 1)), vec![0x0D1D]);
    assert_eq!(writes_to(&n, lane(0x4100, 2)), vec![0x00FB]);
    assert_eq!(writes_to(&n, lane(0x40E7, 3)), vec![0]);
    assert!(writes_to(&n, lane(0x40E6, 0)).is_empty());
    let f = tables::init_table(Mode::UsbDp, Orientation::Flipped);
    assert_eq!(writes_to(&f, 0xC010), vec![0x5100]);
    assert_eq!(writes_to(&f, lane(0x4100, 3)), vec![0x02FF]);
    assert_eq!(writes_to(&f, lane(0x8000, 2)), vec![0x0D1D]);
    assert_eq!(writes_to(&f, lane(0x4100, 0)), vec![0x00FB]);
    // USB only is the shared table.
    assert_eq!(
        tables::init_table(Mode::UsbOnly, Orientation::Normal),
        tables::init_table(Mode::UsbDp, Orientation::Normal)
    );
}

#[test]
fn pll_one_sits_forty_above_pll_zero() {
    let t = tables::init_table(Mode::DpOnly, Orientation::Normal);
    assert_eq!(writes_to(&t, 0x0090), vec![0x01A0]);
    assert_eq!(writes_to(&t, 0x00D0), vec![0x01C2]);
    assert_eq!(writes_to(&t, 0x0098), vec![1]);
    assert_eq!(writes_to(&t, 0x00D9), vec![0x044F]);
    assert_eq!(writes_to(&t, 0x01A0), vec![0x1002]);
    assert_eq!(writes_to(&t, 0x01C0), vec![0x1002]);
    assert_eq!(writes_to(&t, 0x01C3), vec![0x003F]);
    assert!(writes_to(&t, 0x01A3).is_empty());
}

#[test]
fn lanes_for_a_link_follow_mode_and_orientation() {
    use Orientation::{Flipped, Normal};
    assert_eq!(
        tables::dp_pma_lanes(Mode::DpOnly, Normal, 4),
        vec![0, 1, 2, 3]
    );
    assert_eq!(tables::dp_pma_lanes(Mode::DpOnly, Normal, 1), vec![2]);
    assert_eq!(tables::dp_pma_lanes(Mode::DpOnly, Flipped, 1), vec![1]);
    assert_eq!(tables::dp_pma_lanes(Mode::UsbDp, Normal, 2), vec![2, 3]);
    assert_eq!(tables::dp_pma_lanes(Mode::UsbDp, Flipped, 2), vec![0, 1]);
    assert!(tables::dp_pma_lanes(Mode::UsbOnly, Normal, 0).is_empty());
}

#[test]
fn drive_levels_use_the_filler_cells_for_what_the_standard_forbids() {
    assert_eq!(tables::drive_vals(0, 0), ((0, 0), (0x28, 0)));
    assert_eq!(tables::drive_vals(1, 3), ((1, 2), (0, 0x1F)));
    assert_eq!(tables::drive_vals(2, 3), ((2, 1), (0, 0x13)));
    assert_eq!(tables::drive_vals(3, 2), ((3, 0), (0, 0)));
    assert_eq!(tables::drive_vals(9, 9).0, (3, 0));
}

#[test]
fn power_up_orders_orientation_resets_clock_table_core() {
    let (w, phy) = rig(Mode::DpOnly);
    phy.update(Mode::DpOnly, Orientation::Flipped).unwrap();
    phy.power_up().unwrap();
    let pos = |s: &str| w.position(s).expect("a step the sequence needs");
    assert!(pos("orientation 1") < pos("assert"));
    assert!(pos("assert") < pos("pclk"));
    assert!(pos("pclk") < pos("apb"));
    assert!(pos("apb") < pos("table done"));
    assert!(pos("table done") < pos("core"));
    // Powering again changes nothing.
    phy.power_up().unwrap();
    assert_eq!(w.count("core"), 1);
}

#[test]
fn the_register_clock_is_retried_and_may_never_start() {
    let (w, phy) = rig(Mode::DpOnly);
    w.pclk_failures.store(3, Ordering::SeqCst);
    phy.power_up().unwrap();
    assert_eq!(w.pclk_calls.load(Ordering::SeqCst), 4);
    let (w, phy) = rig(Mode::DpOnly);
    w.pclk_failures.store(100, Ordering::SeqCst);
    assert_eq!(phy.power_up(), Err(Error::Clock));
    assert_eq!(w.pclk_calls.load(Ordering::SeqCst), 10);
}

#[test]
fn a_rate_change_brackets_the_pll_and_sets_the_values() {
    let (w, phy) = rig(Mode::DpOnly);
    phy.set_rate(Rate::Hbr2, 4).unwrap();
    assert_eq!(w.reg(tables::PLL1_INTDIV), 0x01C2);
    assert_eq!(w.reg(tables::PDIAG_PLL1_CLK_SEL), 0x0301);
    assert_eq!(w.reg(tables::PLL1_HIGH_THR), 0x012C);
    assert_eq!(w.reg(tables::PDIAG_PLL1_ITRIM), 0x003F);
    for l in 0..4 {
        assert_eq!(w.reg(lane(tables::XCVR_DIAG_HSCLK_DIV, l)), 0);
    }
    // The PLL was stopped and started again; data went off and on; the
    // lanes went through A3, then A2 and A0.
    let log = w.log.lock().unwrap().clone();
    let plls: Vec<_> = log.iter().filter(|l| l.starts_with("pll_ctrl")).collect();
    assert_eq!(plls.len(), 2);
    assert_eq!(w.reg(tables::PLL_CTRL) & tables::PLL1_RUN, tables::PLL1_RUN);
    assert!(phy.pll_locked());
    let a3 = log.iter().position(|l| l == "state 0x8").unwrap();
    let off = log.iter().rposition(|l| l == "data 0").unwrap();
    let on = log.iter().rposition(|l| l == "data 1").unwrap();
    assert!(off < a3 && a3 < on);
    assert_eq!(log[on - 1], "state 0x1");
    assert_eq!(phy.active_lanes(), vec![0, 1, 2, 3]);
}

#[test]
fn each_rate_has_its_own_values_and_the_fast_one_its_extras() {
    let (w, phy) = rig(Mode::DpOnly);
    phy.set_rate(Rate::Rbr, 4).unwrap();
    assert_eq!(w.reg(tables::PLL1_INTDIV), 0x0195);
    assert_eq!(w.reg(tables::PDIAG_PLL1_CLK_SEL), 0x0F01);
    assert_eq!(w.reg(lane(tables::XCVR_DIAG_HSCLK_DIV, 2)), 2);
    phy.set_rate(Rate::Hbr3, 4).unwrap();
    assert_eq!(w.reg(tables::PLL1_INTDIV), 0x0151);
    assert_eq!(w.reg(tables::PLL1_FRACDIVL), 0x8000);
    assert_eq!(w.reg(tables::PDIAG_PLL1_ITRIM), 0x007F);
    assert_eq!(w.reg(tables::PLL1_SS_CTRL2), 0x0430);
    assert_eq!(w.reg(tables::PLL1_SS_CTRL3), 0x0062);
    assert_eq!(w.reg(tables::PLL1_SS_CTRL4), 0x0004);
    // Back to a slower rate restores the usual extras.
    phy.set_rate(Rate::Hbr, 4).unwrap();
    assert_eq!(w.reg(tables::PLL1_SS_CTRL3), 0x007F);
    assert_eq!(w.reg(tables::PLL1_SS_CTRL4), 0x0003);
}

#[test]
fn a_one_lane_link_on_a_flipped_cable_programs_only_its_lane() {
    let (w, phy) = rig(Mode::UsbDp);
    phy.update(Mode::UsbDp, Orientation::Flipped).unwrap();
    phy.set_rate(Rate::Hbr, 1).unwrap();
    assert_eq!(w.reg(lane(tables::XCVR_DIAG_HSCLK_DIV, 1)), 1);
    // Lane 0 of the pair keeps the table's value (0), and so do the USB
    // lanes: only PMA 1 was written by the rate change.
    assert_eq!(phy.active_lanes(), vec![1]);
}

#[test]
fn drive_levels_are_written_on_the_lanes_in_use_with_the_hold_bit() {
    let (w, phy) = rig(Mode::DpOnly);
    phy.set_rate(Rate::Hbr2, 2).unwrap();
    let got = phy.set_levels(Drive {
        swing: 2,
        emphasis: 3,
    });
    assert_eq!(
        got,
        Drive {
            swing: 2,
            emphasis: 1
        }
    );
    for l in [2, 3] {
        assert_eq!(w.reg(lane(tables::TX_TXCC_CTRL, l)), 0x08A4);
        assert_eq!(w.reg(lane(tables::DRV_DIAG_TX_DRV, l)), 3);
        assert_eq!(w.reg(lane(tables::TX_TXCC_MGNFS_MULT_000, l)), 0);
        assert_eq!(w.reg(lane(tables::TX_TXCC_CPOST_MULT_00, l)), 0x13);
        // The hold bit is back to clear.
        assert_eq!(w.reg(lane(tables::TX_DIAG_ACYA, l)) & 1, 0);
    }
    // Lanes 0 and 1 were not touched.
    assert_eq!(w.reg(lane(tables::TX_TXCC_CTRL, 0)), 0);
}

#[test]
fn a_mode_without_displayport_cannot_be_used_for_it() {
    let (_, phy) = rig(Mode::UsbOnly);
    assert_eq!(phy.set_rate(Rate::Hbr, 2), Err(Error::NoDisplayPort));
    assert_eq!(phy.lanes(), 0);
}

#[test]
fn the_phy_reports_its_lane_count_to_the_controller() {
    assert_eq!(rig(Mode::DpOnly).1.lanes(), 4);
    assert_eq!(rig(Mode::UsbDp).1.lanes(), 2);
}

#[test]
fn a_new_orientation_reinitialises_a_powered_phy_but_not_an_unchanged_one() {
    let (w, phy) = rig(Mode::UsbDp);
    phy.power_up().unwrap();
    assert_eq!(w.count("core"), 1);
    phy.update(Mode::UsbDp, Orientation::Normal).unwrap();
    assert_eq!(w.count("core"), 1);
    phy.update(Mode::UsbDp, Orientation::Flipped).unwrap();
    assert_eq!(w.count("core"), 2);
    assert!(w.position("orientation 1").is_some());
    // Going on to DP only changes the table too.
    phy.update(Mode::DpOnly, Orientation::Flipped).unwrap();
    assert_eq!(w.count("core"), 3);
    // The link must be set again afterwards.
    assert!(phy.active_lanes().is_empty());
}

#[test]
fn the_interface_prepare_sequence() {
    let (w, phy) = rig(Mode::DpOnly);
    phy.prepare();
    {
        let i = w.iface.lock().unwrap();
        // It ends with the AUX path fully enabled and out of reset.
        assert_eq!(i[&0x54], 0xF);
        assert_eq!(i[&0x5C], 0);
        assert_eq!(i[&0x58], 0xF);
        assert_eq!(i[&0x40], 0);
    }
    assert!(UsbDpPhy::is_combo(&I(w.clone())));
}

#[test]
fn link_off_and_power_down() {
    let (w, phy) = rig(Mode::DpOnly);
    phy.set_rate(Rate::Hbr, 4).unwrap();
    phy.link_off();
    {
        let i = w.iface.lock().unwrap();
        assert_eq!((i[&0x50], i[&0x4C], i[&0x68], i[&0x5C]), (0, 0, 0, 1));
    }
    assert!(phy.active_lanes().is_empty());
    phy.power_down();
    assert!(w.position("pclk off").is_some());
    // Up again from nothing.
    phy.power_up().unwrap();
    assert_eq!(w.count("core"), 2);
}
