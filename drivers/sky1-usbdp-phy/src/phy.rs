// SPDX-License-Identifier: BSD-2-Clause
//! The combo PHY of a USB-C port, as DisplayPort's physical layer:
//! powering the shared core, the controller-side interface window, the
//! rate change and the drive levels (clean-room specification
//! `sky1-dp-tx` 03 §3.6, §3.8 to §3.10).

use alloc::vec::Vec;

use sky1_dp_tx::dpcd::{Drive, Rate};
use sky1_dp_tx::link::Phy;
use sync::SpinLock;

use crate::tables::{self, Mode, Orientation};

/// The PHY's 16-bit registers, by register index (the byte address is
/// four times it).
pub trait Regs16: Send + Sync {
    fn read(&self, index: u32) -> u16;
    fn write(&self, index: u32, v: u16);
}

/// The controller's PHY-interface window: 32-bit registers by byte offset.
pub trait Words32: Send + Sync {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, v: u32);
}

/// What the PHY needs from the machine.
pub trait Platform: Send + Sync {
    /// Sets the plug orientation bit in the system controller (section
    /// 3.5.3); a platform that cannot reach it does nothing.
    fn set_orientation(&self, flipped: bool);
    /// Asserts the core and register-interface resets.
    fn assert_resets(&self);
    fn release_apb_reset(&self);
    fn release_core_reset(&self);
    /// Enables the register clock; whether it came on.
    fn pclk_enable(&self) -> bool;
    fn pclk_disable(&self);
    fn delay_us(&self, us: u64);
}

/// Why the PHY did not come up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The register clock would not start.
    Clock,
    /// The interface never reported the PHY ready.
    NotReady,
    /// The mode has no DisplayPort lanes.
    NoDisplayPort,
}

// The interface window (section 3.10.2).
const PHY_CONFIG_RSTN: usize = 0x40;
const PHY_CMN_READY: usize = 0x44;
const PHY_LANE_EN: usize = 0x48;
const PHY_LANE_RSTN: usize = 0x4C;
const PHY_DATA_ENABLE: usize = 0x50;
const AUXHPD_ENABLE: usize = 0x54;
const TX_ELEC_IDLE: usize = 0x58;
const DP_RESET: usize = 0x5C;
const LANE_PWR_STATE_REQ: usize = 0x60;
const LANE_PWR_STATE_ACK: usize = 0x64;
const LANE_PLLCLK_ENABLE: usize = 0x68;
const LANE_PLLCLK_EN_ACK: usize = 0x6C;
const ID_REV: usize = 0xFC;

/// `ID_REV[31:16]` for the combo PHY's interface.
pub const INTERFACE_COMBO: u32 = 0x54;

/// Lane power states (section 3.10.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    A0 = 0x1,
    A2 = 0x4,
    A3 = 0x8,
}

#[derive(Debug)]
struct CoreState {
    mode: Mode,
    orient: Orientation,
    /// The core is out of reset with its table written.
    powered: bool,
    /// The table in place: DP-only or the shared one, and its
    /// orientation.
    table: Option<(bool, Orientation)>,
    /// Counts the core's initialisations: an interface that saw an
    /// earlier one has lost its state.
    epoch: u32,
}

/// The shared PHY core: the table, the resets and the clock, which the
/// USB side and the DisplayPort side both use (section 3.6). The
/// controller's interface window is the DisplayPort side's own
/// ([`UsbDpPhy`]).
#[allow(missing_debug_implementations)]
pub struct Core {
    regs: alloc::sync::Arc<dyn Regs16>,
    plat: alloc::sync::Arc<dyn Platform>,
    s: SpinLock<CoreState>,
}

impl Core {
    #[must_use]
    pub fn new(
        regs: alloc::sync::Arc<dyn Regs16>,
        plat: alloc::sync::Arc<dyn Platform>,
        mode: Mode,
    ) -> Self {
        Self {
            regs,
            plat,
            s: SpinLock::new(CoreState {
                mode,
                orient: Orientation::Normal,
                powered: false,
                table: None,
                epoch: 0,
            }),
        }
    }

    fn power(&self, st: &mut CoreState) -> Result<(), Error> {
        let key = (st.mode.dp_table(), st.orient);
        if st.powered && st.table == Some(key) {
            return Ok(());
        }
        if st.powered {
            self.plat.assert_resets();
            self.plat.pclk_disable();
        }
        self.plat.set_orientation(st.orient == Orientation::Flipped);
        self.plat.delay_us(10);
        self.plat.assert_resets();
        let mut on = false;
        for _ in 0..10 {
            if self.plat.pclk_enable() {
                on = true;
                break;
            }
            self.plat.delay_us(5000);
        }
        if !on {
            return Err(Error::Clock);
        }
        self.plat.release_apb_reset();
        for (r, v) in tables::init_table(st.mode, st.orient) {
            self.regs.write(r, v);
        }
        self.plat.release_core_reset();
        st.powered = true;
        st.table = Some(key);
        st.epoch += 1;
        Ok(())
    }

    /// Brings the core up with the table for the mode and orientation, if
    /// it is not up with it already. The table is rewritten whenever it
    /// would change (which the vendor does not do for DP-only to USB plus
    /// DP; an open question).
    ///
    /// # Errors
    ///
    /// [`Error::Clock`] when the register clock does not start.
    pub fn power_up(&self) -> Result<(), Error> {
        let mut st = self.s.lock();
        self.power(&mut st)
    }

    /// Records a new mode or orientation (from the Type-C controller). A
    /// powered core is re-initialised at once if its table changes.
    ///
    /// # Errors
    ///
    /// As [`Core::power_up`].
    pub fn update(&self, mode: Mode, orient: Orientation) -> Result<(), Error> {
        let mut st = self.s.lock();
        st.mode = mode;
        st.orient = orient;
        if st.powered && st.table != Some((mode.dp_table(), orient)) {
            self.power(&mut st)?;
        }
        Ok(())
    }

    /// Powers the core down, for when no user is left.
    pub fn power_down(&self) {
        let mut st = self.s.lock();
        if st.powered {
            self.plat.assert_resets();
            self.plat.pclk_disable();
        }
        st.powered = false;
        st.table = None;
    }

    fn snapshot(&self) -> (Mode, Orientation, u32) {
        let st = self.s.lock();
        (st.mode, st.orient, st.epoch)
    }

    /// The mode now.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.s.lock().mode
    }
}

/// What the DisplayPort side keeps: its interface and link.
#[derive(Debug)]
struct State {
    /// The interface is up (section 3.10.4 "init" done), as of core
    /// initialisation number `epoch`.
    iface_up: bool,
    epoch: u32,
    link: Option<(Rate, u8)>,
    locked: bool,
}

/// The combo PHY as one DisplayPort controller sees it.
#[allow(missing_debug_implementations)]
pub struct UsbDpPhy {
    core: alloc::sync::Arc<Core>,
    iface: alloc::sync::Arc<dyn Words32>,
    plat: alloc::sync::Arc<dyn Platform>,
    regs: alloc::sync::Arc<dyn Regs16>,
    s: SpinLock<State>,
}

impl UsbDpPhy {
    /// A PHY with its own core (the interface window and the core one
    /// device's: tests, and a machine with no sharing).
    #[must_use]
    pub fn new(
        regs: alloc::sync::Arc<dyn Regs16>,
        iface: alloc::sync::Arc<dyn Words32>,
        plat: alloc::sync::Arc<dyn Platform>,
        mode: Mode,
    ) -> Self {
        let core = alloc::sync::Arc::new(Core::new(regs.clone(), plat.clone(), mode));
        Self::with_core(core, regs, iface, plat)
    }

    /// The DisplayPort side of `core`, through the controller's interface
    /// window.
    #[must_use]
    pub fn with_core(
        core: alloc::sync::Arc<Core>,
        regs: alloc::sync::Arc<dyn Regs16>,
        iface: alloc::sync::Arc<dyn Words32>,
        plat: alloc::sync::Arc<dyn Platform>,
    ) -> Self {
        Self {
            core,
            iface,
            plat,
            regs,
            s: SpinLock::new(State {
                iface_up: false,
                epoch: 0,
                link: None,
                locked: false,
            }),
        }
    }

    /// The core this uses.
    #[must_use]
    pub fn core(&self) -> &alloc::sync::Arc<Core> {
        &self.core
    }

    /// Whether the controller's PHY-interface window is the combo PHY's
    /// (section 3.10.1).
    #[must_use]
    pub fn is_combo(iface: &dyn Words32) -> bool {
        iface.read(ID_REV) >> 16 == INTERFACE_COMBO
    }

    fn poll(&self, off: usize, mask: u32, want: u32, tries: u32) -> bool {
        for _ in 0..tries {
            if self.iface.read(off) & mask == want {
                return true;
            }
            self.plat.delay_us(1);
        }
        false
    }

    /// Records a new mode or orientation (see [`Core::update`]).
    ///
    /// # Errors
    ///
    /// As [`Core::power_up`].
    pub fn update(&self, mode: Mode, orient: Orientation) -> Result<(), Error> {
        self.core.update(mode, orient)
    }

    /// Powers the core (see [`Core::power_up`]).
    ///
    /// # Errors
    ///
    /// [`Error::Clock`].
    pub fn power_up(&self) -> Result<(), Error> {
        self.core.power_up()
    }

    /// Powers the core down (see [`Core::power_down`]).
    pub fn power_down(&self) {
        self.core.power_down();
        let mut st = self.s.lock();
        st.iface_up = false;
        st.link = None;
    }

    /// The interface "prepare" step, needed before any AUX transaction
    /// (section 3.10.4).
    pub fn prepare(&self) {
        let i = &*self.iface;
        i.write(AUXHPD_ENABLE, 0);
        i.write(TX_ELEC_IDLE, 0xF);
        i.write(PHY_DATA_ENABLE, 0);
        i.write(PHY_LANE_RSTN, 0);
        i.write(PHY_LANE_EN, 0);
        i.write(PHY_CONFIG_RSTN, 0);
        i.write(DP_RESET, 1);
        self.plat.delay_us(1);
        for (v, wait) in [(0x1, 1), (0x3, 1), (0x7, 5)] {
            i.write(AUXHPD_ENABLE, v);
            self.plat.delay_us(wait);
        }
        i.write(AUXHPD_ENABLE, 0xF);
        i.write(DP_RESET, 0);
    }

    /// A power-state handshake (section 3.10.3): request, wait for the
    /// acknowledge, clear the request. A timeout is not an error, as the
    /// vendor treats it.
    fn power_state(&self, p: Power) -> bool {
        let i = &*self.iface;
        i.write(LANE_PWR_STATE_REQ, p as u32);
        let ok = self.poll(LANE_PWR_STATE_ACK, 0x1F, p as u32, 200);
        i.write(LANE_PWR_STATE_REQ, 0);
        self.plat.delay_us(1);
        ok
    }

    /// The interface "init" step: lanes out of reset, wait for the PHY,
    /// the lane clocks, A2 then A0, data on (section 3.10.4).
    fn iface_init(&self, st: &mut State) -> Result<(), Error> {
        if self.core.mode().dp_lanes() == 0 {
            return Err(Error::NoDisplayPort);
        }
        self.core.power_up()?;
        st.epoch = self.core.snapshot().2;
        let i = &*self.iface;
        i.write(PHY_LANE_RSTN, 0xF);
        i.write(PHY_LANE_EN, 0xF);
        i.write(TX_ELEC_IDLE, 0);
        i.write(PHY_CONFIG_RSTN, 1);
        if !self.poll(PHY_CMN_READY, 1, 1, 500) {
            return Err(Error::NotReady);
        }
        i.write(LANE_PLLCLK_ENABLE, 1);
        if !self.poll(LANE_PLLCLK_EN_ACK, 1, 1, 500) {
            return Err(Error::NotReady);
        }
        self.power_state(Power::A2);
        self.power_state(Power::A0);
        i.write(PHY_DATA_ENABLE, 1);
        st.iface_up = true;
        Ok(())
    }

    /// Changes the link rate (section 3.8.3), bringing the interface up
    /// first if it is not.
    ///
    /// # Errors
    ///
    /// [`Error`] when the PHY or its interface does not come up.
    pub fn set_rate(&self, rate: Rate, lanes: u8) -> Result<(), Error> {
        let mut st = self.s.lock();
        // A core initialised since the interface came up has lost it.
        if st.iface_up && st.epoch != self.core.snapshot().2 {
            st.iface_up = false;
            st.link = None;
        }
        if !st.iface_up {
            self.iface_init(&mut st)?;
        }
        let (mode, orient, _) = self.core.snapshot();
        let i = &*self.iface;
        // Bracket: data off, A3, lane clocks off.
        i.write(PHY_DATA_ENABLE, 0);
        self.power_state(Power::A3);
        i.write(LANE_PLLCLK_ENABLE, 0);
        self.poll(LANE_PLLCLK_EN_ACK, 1, 0, 100);
        let run = self.regs.read(tables::PLL_CTRL);
        self.regs.write(tables::PLL_CTRL, run & !tables::PLL1_RUN);
        self.plat.delay_us(10);
        for (r, v) in tables::rate_extras(rate) {
            self.regs.write(r, v);
        }
        let v = tables::rate_vals(rate);
        for l in tables::dp_pma_lanes(mode, orient, lanes) {
            self.regs
                .write(tables::lane(tables::XCVR_DIAG_HSCLK_DIV, l), v.hsclk_div);
        }
        self.regs.write(tables::PLL1_INTDIV, v.intdiv);
        self.regs.write(tables::PLL1_FRACDIVL, v.fracl);
        self.regs.write(tables::PLL1_HIGH_THR, v.thr);
        self.regs.write(tables::PDIAG_PLL1_CLK_SEL, v.clk_sel);
        let run = self.regs.read(tables::PLL_CTRL);
        self.regs.write(tables::PLL_CTRL, run | tables::PLL1_RUN);
        self.plat.delay_us(10);
        // The vendor does not look; the status is kept for the log.
        st.locked = self.regs.read(tables::CMN_CTRL2) & tables::PLL1_LOCKED != 0;
        i.write(LANE_PLLCLK_ENABLE, 1);
        let acked = self.poll(LANE_PLLCLK_EN_ACK, 1, 1, 100);
        self.power_state(Power::A2);
        self.power_state(Power::A0);
        i.write(PHY_DATA_ENABLE, 1);
        st.link = Some((rate, lanes));
        if acked { Ok(()) } else { Err(Error::NotReady) }
    }

    /// Sets the drive levels of the lanes in use (section 3.9). The
    /// levels actually applied are returned (the table's filler cells
    /// stand in for combinations the standard does not allow).
    pub fn set_levels(&self, d: Drive) -> Drive {
        let st = self.s.lock();
        let (mode, orient, _) = self.core.snapshot();
        let lanes = st.link.map_or(mode.dp_lanes(), |l| l.1);
        let ((s, e), (mgn, cpost)) = tables::drive_vals(d.swing, d.emphasis);
        for l in tables::dp_pma_lanes(mode, orient, lanes) {
            let acya = tables::lane(tables::TX_DIAG_ACYA, l);
            let a = self.regs.read(acya);
            self.regs.write(acya, a | 1);
            self.regs
                .write(tables::lane(tables::TX_TXCC_CTRL, l), 0x08A4);
            self.regs
                .write(tables::lane(tables::DRV_DIAG_TX_DRV, l), 0x0003);
            self.regs
                .write(tables::lane(tables::TX_TXCC_MGNFS_MULT_000, l), mgn);
            self.regs
                .write(tables::lane(tables::TX_TXCC_CPOST_MULT_00, l), cpost);
            self.regs.write(acya, a & !1);
        }
        Drive {
            swing: s,
            emphasis: e,
        }
    }

    /// Link off: A3, the interface reset (section 3.10.5).
    pub fn link_off(&self) {
        let mut st = self.s.lock();
        if st.iface_up {
            self.power_state(Power::A3);
            let i = &*self.iface;
            i.write(PHY_DATA_ENABLE, 0);
            i.write(PHY_LANE_RSTN, 0);
            i.write(PHY_LANE_EN, 0);
            i.write(LANE_PWR_STATE_REQ, 0);
            i.write(LANE_PLLCLK_ENABLE, 0);
            i.write(DP_RESET, 1);
        }
        st.iface_up = false;
        st.link = None;
    }

    /// Whether PLL1 was seen locked after the last rate change.
    #[must_use]
    pub fn pll_locked(&self) -> bool {
        self.s.lock().locked
    }

    /// The PMA lanes the link uses now.
    #[must_use]
    pub fn active_lanes(&self) -> Vec<u32> {
        let st = self.s.lock();
        let (mode, orient, _) = self.core.snapshot();
        st.link
            .map_or_else(Vec::new, |l| tables::dp_pma_lanes(mode, orient, l.1))
    }
}

impl Phy for UsbDpPhy {
    fn set_link(&self, rate: Rate, lanes: u8) {
        // A failure shows as lanes that never become ready, which the
        // controller reports.
        let _ = self.set_rate(rate, lanes);
    }

    fn set_drive(&self, drive: Drive) {
        let _ = self.set_levels(drive);
    }

    fn lanes(&self) -> u8 {
        self.core.mode().dp_lanes()
    }
}
