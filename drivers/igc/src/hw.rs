// SPDX-License-Identifier: BSD-2-Clause
//! The I225/I226 controller over its registers: reset, the station
//! address, the internal PHY's management interface and its
//! auto-negotiation, the link, the receive filter, the queues, and the
//! interrupt mask.
//!
//! [`Regs`] is the controller's register window and a microsecond delay:
//! the driver gives it the memory BAR, and the tests a model of the
//! register protocol, since no emulator has these chips.
//!
//! Behavior reference: sys/dev/igc (`igc_reset_hw_i225`,
//! `igc_init_hw_base`, `igc_setup_copper_link_i225`, `igc_phy.c`,
//! `igc_if_init`). Deviations: flow control stays off (FreeBSD's default is
//! full, with watermarks from the packet buffer size), and the latency
//! tolerance reporting (`igc_set_ltr_i225`) is not programmed.

use nicwatch::responding;

use crate::regs as r;

/// The controller's register window.
pub trait Regs {
    fn read32(&self, off: usize) -> u32;
    fn write32(&self, off: usize, v: u32);
    fn delay_us(&self, us: u32);
}

/// Why a hardware operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwError {
    /// Registers read all ones: the device left the bus.
    NotResponding,
    /// The software/firmware semaphore was not got in time.
    Semaphore,
    /// A management interface access did not finish, or reported an error.
    Phy,
}

/// What the link is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    M10,
    M100,
    M1000,
    M2500,
}

/// The link's state, from the status register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkState {
    pub up: bool,
    pub speed: Speed,
    pub full_duplex: bool,
}

impl LinkState {
    /// The state a status register word says.
    #[must_use]
    pub fn of(status: u32) -> Self {
        let speed = if status & r::STATUS_SPEED_1000 != 0 {
            // The I225 says 1 Gb/s in both modes; one more bit tells 2.5.
            if status & r::STATUS_SPEED_2500 != 0 {
                Speed::M2500
            } else {
                Speed::M1000
            }
        } else if status & r::STATUS_SPEED_100 != 0 {
            Speed::M100
        } else {
            Speed::M10
        };
        Self {
            up: status & r::STATUS_LU != 0,
            speed,
            full_duplex: status & r::STATUS_FD != 0,
        }
    }
}

/// The controller.
#[derive(Debug)]
pub struct Chip<R: Regs> {
    regs: R,
}

impl<R: Regs> Chip<R> {
    #[must_use]
    pub const fn new(regs: R) -> Self {
        Self { regs }
    }

    fn rd(&self, off: usize) -> u32 {
        self.regs.read32(off)
    }

    fn wr(&self, off: usize, v: u32) {
        self.regs.write32(off, v);
    }

    /// `IGC_WRITE_FLUSH`: a read to push the writes out.
    fn flush(&self) {
        let _ = self.rd(r::STATUS);
    }

    fn delay_us(&self, us: u32) {
        self.regs.delay_us(us);
    }

    /// Whether the device answers: one that has left the bus completes
    /// reads with all ones.
    #[must_use]
    pub fn responding(&self) -> bool {
        responding(self.rd(r::STATUS))
    }

    /// `igc_reset_hw_i225`: stops the bus master, masks every interrupt,
    /// stops the receiver and transmitter, resets the device, and waits for
    /// the NVM's automatic read of its settings. FreeBSD carries on when
    /// the master disable or the automatic read does not finish, and so
    /// does this.
    ///
    /// # Errors
    ///
    /// [`HwError::NotResponding`] if the registers read all ones.
    pub fn reset(&self) -> Result<(), HwError> {
        if !self.responding() {
            return Err(HwError::NotResponding);
        }
        // `igc_disable_pcie_master_generic`.
        self.wr(r::CTRL, self.rd(r::CTRL) | r::CTRL_GIO_MASTER_DISABLE);
        for _ in 0..800 {
            if self.rd(r::STATUS) & r::STATUS_GIO_MASTER_ENABLE == 0 {
                break;
            }
            self.delay_us(100);
        }
        self.wr(r::IMC, u32::MAX);
        self.wr(r::RCTL, 0);
        self.wr(r::TCTL, r::TCTL_PSP);
        self.flush();
        self.delay_us(10_000);
        self.wr(r::CTRL, self.rd(r::CTRL) | r::CTRL_DEV_RST);
        // `igc_get_auto_rd_done_generic`: up to ten milliseconds.
        for _ in 0..10 {
            if self.rd(r::EECD) & r::EECD_AUTO_RD != 0 {
                break;
            }
            self.delay_us(1000);
        }
        self.wr(r::IMC, u32::MAX);
        let _ = self.rd(r::ICR);
        Ok(())
    }

    /// The station address the controller loaded from its NVM into
    /// receive address register 0 (`igc_read_mac_addr`).
    #[must_use]
    pub fn mac_address(&self) -> [u8; 6] {
        let lo = self.rd(r::ral(0)).to_le_bytes();
        let hi = self.rd(r::rah(0)).to_le_bytes();
        [lo[0], lo[1], lo[2], lo[3], hi[0], hi[1]]
    }

    /// `igc_rar_set_generic`.
    fn rar_set(&self, addr: [u8; 6], index: usize) {
        let lo = u32::from_le_bytes([addr[0], addr[1], addr[2], addr[3]]);
        let mut hi = u32::from(addr[4]) | u32::from(addr[5]) << 8;
        if lo != 0 || hi != 0 {
            hi |= r::RAH_AV;
        }
        // Some bridges combine consecutive writes into a burst, which
        // malfunctions on some parts: a flush between them.
        self.wr(r::ral(index), lo);
        self.flush();
        self.wr(r::rah(index), hi);
        self.flush();
    }

    /// `igc_init_hw_base`: the station address into receive address 0, the
    /// others and the multicast table cleared, and the link set up.
    ///
    /// # Errors
    ///
    /// What setting up the link fails with.
    pub fn init_hw(&self, mac: [u8; 6]) -> Result<(), HwError> {
        self.rar_set(mac, 0);
        for i in 1..r::RAR_COUNT {
            self.rar_set([0; 6], i);
        }
        for i in 0..r::MTA_COUNT {
            self.wr(r::MTA + i * 4, 0);
        }
        self.flush();
        self.setup_link()
    }

    // ---- The software/firmware semaphores -----------------------------------

    /// `igc_get_hw_semaphore_i225`: the software semaphore bit, then the
    /// firmware's.
    fn get_hw_semaphore(&self) -> Result<(), HwError> {
        // The NVM's word count plus one: a few thousand tries of 50 us.
        const TRIES: u32 = 2048;
        let mut got = false;
        for _ in 0..TRIES {
            if self.rd(r::SWSM) & r::SWSM_SMBI == 0 {
                got = true;
                break;
            }
            self.delay_us(50);
        }
        if !got {
            return Err(HwError::Semaphore);
        }
        for _ in 0..TRIES {
            let swsm = self.rd(r::SWSM);
            self.wr(r::SWSM, swsm | r::SWSM_SWESMBI);
            // Acquired if the bit latched.
            if self.rd(r::SWSM) & r::SWSM_SWESMBI != 0 {
                return Ok(());
            }
            self.delay_us(50);
        }
        self.put_hw_semaphore();
        Err(HwError::Semaphore)
    }

    /// `igc_put_hw_semaphore_generic`.
    fn put_hw_semaphore(&self) {
        let swsm = self.rd(r::SWSM) & !(r::SWSM_SMBI | r::SWSM_SWESMBI);
        self.wr(r::SWSM, swsm);
    }

    /// `igc_acquire_swfw_sync_i225` for the PHY.
    fn acquire_phy(&self) -> Result<(), HwError> {
        let mask = r::SWFW_PHY0_SM;
        for _ in 0..200 {
            self.get_hw_semaphore()?;
            let sync = self.rd(r::SW_FW_SYNC);
            // The firmware's bits are the software's shifted up 16.
            if sync & (mask | mask << 16) == 0 {
                self.wr(r::SW_FW_SYNC, sync | mask);
                self.put_hw_semaphore();
                return Ok(());
            }
            self.put_hw_semaphore();
            self.delay_us(5000);
        }
        Err(HwError::Semaphore)
    }

    /// `igc_release_swfw_sync_i225`.
    fn release_phy(&self) {
        while self.get_hw_semaphore().is_err() {
            self.delay_us(50);
        }
        let sync = self.rd(r::SW_FW_SYNC) & !r::SWFW_PHY0_SM;
        self.wr(r::SW_FW_SYNC, sync);
        self.put_hw_semaphore();
    }

    // ---- The PHY's management interface -----------------------------------------

    /// One access through `MDIC` (`igc_read_phy_reg_mdic`,
    /// `igc_write_phy_reg_mdic`), the PHY's semaphore held.
    fn mdic(&self, op: u32, reg: u32, data: u16) -> Result<u16, HwError> {
        if reg > r::MAX_PHY_REG {
            return Err(HwError::Phy);
        }
        let cmd =
            u32::from(data) | reg << r::MDIC_REG_SHIFT | r::PHY_ADDR << r::MDIC_PHY_SHIFT | op;
        self.wr(r::MDIC, cmd);
        let mut v = 0;
        for _ in 0..r::MDIC_POLLS {
            self.delay_us(50);
            v = self.rd(r::MDIC);
            if v & r::MDIC_READY != 0 {
                break;
            }
        }
        if v & r::MDIC_READY == 0 || v & r::MDIC_ERROR != 0 {
            return Err(HwError::Phy);
        }
        // The register echoed back is the one asked for.
        if (v & r::MDIC_REG_MASK) >> r::MDIC_REG_SHIFT != reg {
            return Err(HwError::Phy);
        }
        Ok((v & r::MDIC_DATA_MASK) as u16)
    }

    /// A clause 22 register, with the PHY's semaphore (always let go,
    /// where FreeBSD's `igc_read_phy_reg_gpy` leaks it on an error).
    fn phy_plain(&self, op: u32, reg: u32, data: u16) -> Result<u16, HwError> {
        self.acquire_phy()?;
        let v = self.mdic(op, reg, data);
        self.release_phy();
        v
    }

    /// `igc_read_phy_reg_gpy`: `reg` is a register, or `mmd << 16 | reg`
    /// for one of an MMD.
    ///
    /// # Errors
    ///
    /// [`HwError::Semaphore`] or [`HwError::Phy`].
    pub fn phy_read(&self, reg: u32) -> Result<u16, HwError> {
        let mmd = reg >> r::MMD_DEVADDR_SHIFT;
        let off = reg & 0xffff;
        if mmd == 0 {
            return self.phy_plain(r::MDIC_OP_READ, off, 0);
        }
        self.xmdio(off, mmd, None)
    }

    /// `igc_write_phy_reg_gpy`.
    ///
    /// # Errors
    ///
    /// As [`Chip::phy_read`].
    pub fn phy_write(&self, reg: u32, data: u16) -> Result<(), HwError> {
        let mmd = reg >> r::MMD_DEVADDR_SHIFT;
        let off = reg & 0xffff;
        if mmd == 0 {
            return self.phy_plain(r::MDIC_OP_WRITE, off, data).map(|_| ());
        }
        self.xmdio(off, mmd, Some(data)).map(|_| ())
    }

    /// `__igc_access_xmdio_reg`: an MMD register through the two access
    /// registers.
    fn xmdio(&self, addr: u32, mmd: u32, write: Option<u16>) -> Result<u16, HwError> {
        let dev = u16::try_from(mmd).map_err(|_| HwError::Phy)?;
        let addr = u16::try_from(addr).map_err(|_| HwError::Phy)?;
        self.phy_write(r::MMDAC, dev)?;
        self.phy_write(r::MMDAAD, addr)?;
        self.phy_write(r::MMDAC, r::MMDAC_FUNC_DATA | dev)?;
        let v = match write {
            Some(d) => {
                self.phy_write(r::MMDAAD, d)?;
                0
            }
            None => self.phy_read(r::MMDAAD)?,
        };
        self.phy_write(r::MMDAC, 0)?;
        Ok(v)
    }

    // ---- The link -----------------------------------------------------------------

    /// `igc_phy_hw_reset_generic`.
    fn phy_reset(&self) -> Result<(), HwError> {
        self.acquire_phy()?;
        let ctrl = self.rd(r::CTRL);
        self.wr(r::CTRL, ctrl | r::CTRL_PHY_RST);
        self.flush();
        self.delay_us(100);
        self.wr(r::CTRL, ctrl);
        self.flush();
        self.delay_us(150);
        for _ in 0..10_000 {
            if self.rd(r::PHPM) & r::PHPM_RST_COMPL != 0 {
                break;
            }
            self.delay_us(1);
        }
        self.release_phy();
        Ok(())
    }

    /// `igc_setup_copper_link_i225` with `igc_copper_link_autoneg` and
    /// `igc_phy_setup_autoneg`: link up forced on in the MAC, the PHY
    /// powered up, and everything it can do advertised (10 and 100 half and
    /// full, 1 Gb/s and 2.5 Gb/s full, no pause), then auto-negotiation
    /// restarted. The flow control registers get FreeBSD's constants.
    ///
    /// # Errors
    ///
    /// What the PHY accesses fail with.
    pub fn setup_link(&self) -> Result<(), HwError> {
        let ctrl = self.rd(r::CTRL) | r::CTRL_SLU;
        self.wr(
            r::CTRL,
            ctrl & !(r::CTRL_FRCSPD | r::CTRL_FRCDPX | r::CTRL_RFCE | r::CTRL_TFCE),
        );
        self.wr(r::PHPM, self.rd(r::PHPM) & !r::PHPM_GO_LINKD);
        self.phy_reset()?;
        // `igc_power_up_phy_copper`.
        let ctl = self.phy_read(r::PHY_CONTROL)?;
        self.phy_write(r::PHY_CONTROL, ctl & !r::MII_CR_POWER_DOWN)?;
        self.delay_us(300);
        // What to advertise.
        let adv = self.phy_read(r::PHY_AUTONEG_ADV)?;
        let adv = (adv
            & !(r::NWAY_AR_10T_HD
                | r::NWAY_AR_10T_FD
                | r::NWAY_AR_100TX_HD
                | r::NWAY_AR_100TX_FD
                | r::NWAY_AR_PAUSE
                | r::NWAY_AR_ASM_DIR))
            | r::NWAY_AR_10T_HD
            | r::NWAY_AR_10T_FD
            | r::NWAY_AR_100TX_HD
            | r::NWAY_AR_100TX_FD;
        let gig = self.phy_read(r::PHY_1000T_CTRL)?;
        let gig = (gig & !(r::CR_1000T_HD | r::CR_1000T_FD)) | r::CR_1000T_FD;
        let multi_reg = r::STANDARD_AN_REG_MASK << r::MMD_DEVADDR_SHIFT | r::ANEG_MULTIGBT_AN_CTRL;
        let multi = self.phy_read(multi_reg)? | r::CR_2500T_FD;
        self.phy_write(r::PHY_AUTONEG_ADV, adv)?;
        self.phy_write(r::PHY_1000T_CTRL, gig)?;
        self.phy_write(multi_reg, multi)?;
        // Restart: auto-negotiation enabled, and again.
        let ctl = self.phy_read(r::PHY_CONTROL)?;
        self.phy_write(
            r::PHY_CONTROL,
            ctl | r::MII_CR_AUTO_NEG_EN | r::MII_CR_RESTART_AUTO_NEG,
        )?;
        // `igc_setup_link_generic`'s flow control constants, no thresholds.
        self.wr(r::FCT, 0x8808);
        self.wr(r::FCAH, 0x0000_0100);
        self.wr(r::FCAL, 0x00c2_8001);
        self.wr(r::FCTTV, 0x0680);
        self.wr(r::FCRTL, 0);
        self.wr(r::FCRTH, 0);
        Ok(())
    }

    /// The link now (`IGC_STATUS`).
    #[must_use]
    pub fn link(&self) -> LinkState {
        LinkState::of(self.rd(r::STATUS))
    }

    /// `igc_config_collision_dist_generic`, which FreeBSD does at each link
    /// up.
    pub fn link_up_config(&self) {
        let tctl = self.rd(r::TCTL) & !r::TCTL_COLD;
        self.wr(r::TCTL, tctl | r::COLLISION_DISTANCE << r::COLD_SHIFT);
        self.flush();
    }

    /// Whether the PHY reports link (`igc_phy_has_link_generic`: the status
    /// register's latched bit read twice).
    ///
    /// # Errors
    ///
    /// What the access fails with.
    pub fn phy_has_link(&self) -> Result<bool, HwError> {
        let _ = self.phy_read(r::PHY_STATUS)?;
        Ok(self.phy_read(r::PHY_STATUS)? & r::MII_SR_LINK_STATUS != 0)
    }

    // ---- The receive filter -------------------------------------------------------

    /// The multicast table's words for `groups` (`igc_update_mc_addr_list`
    /// with filter type 0: twelve bits of the last two address bytes pick
    /// one of 4096).
    #[must_use]
    pub fn multicast_table(groups: &[[u8; 6]]) -> [u32; r::MTA_COUNT] {
        let mut t = [0u32; r::MTA_COUNT];
        for g in groups {
            // `igc_hash_mc_addr_generic`: the bit shift is 4 for 128 registers.
            let h = (u32::from(g[4]) >> 4 | u32::from(g[5]) << 4) & 0xfff;
            t[(h >> 5) as usize & (r::MTA_COUNT - 1)] |= 1 << (h & 0x1f);
        }
        t
    }

    /// The receive filter (`igc_if_multi_set`): promiscuous (unicast and
    /// multicast), all multicast, or the table for `groups`.
    pub fn set_filter(&self, promisc: bool, allmulti: bool, groups: &[[u8; 6]]) {
        let mut rctl = self.rd(r::RCTL);
        if promisc {
            rctl |= r::RCTL_UPE | r::RCTL_MPE;
        } else if allmulti || groups.len() >= 128 {
            rctl |= r::RCTL_MPE;
            rctl &= !r::RCTL_UPE;
        } else {
            rctl &= !(r::RCTL_UPE | r::RCTL_MPE);
        }
        if groups.len() < 128 {
            let t = Self::multicast_table(groups);
            for (i, w) in t.iter().enumerate().rev() {
                self.wr(r::MTA + i * 4, *w);
            }
            self.flush();
        }
        self.wr(r::RCTL, rctl);
    }

    // ---- The queues ---------------------------------------------------------------

    /// `igc_initialize_transmit_unit` for queue 0: the ring's base and length,
    /// head and tail at 0, the queue enabled, and the transmitter on.
    pub fn tx_init(&self, bus: u64, bytes: u32) {
        self.wr(r::TDLEN, bytes);
        self.wr(r::TDBAH, r::hi(bus));
        self.wr(r::TDBAL, r::lo(bus));
        self.wr(r::TDT, 0);
        self.wr(r::TDH, 0);
        self.wr(
            r::TXDCTL,
            r::TX_PTHRESH | r::TX_HTHRESH << 8 | r::TXDCTL_QUEUE_ENABLE,
        );
        let tctl = self.rd(r::TCTL) & !r::TCTL_CT;
        self.wr(
            r::TCTL,
            tctl | r::TCTL_PSP | r::TCTL_RTLC | r::TCTL_EN | r::COLLISION_THRESHOLD << r::CT_SHIFT,
        );
    }

    /// `igc_initialize_receive_unit` for queue 0: the receiver off while the
    /// ring is set up, 2 KiB buffers in one-buffer advanced descriptors,
    /// the checksum offload, the queue enabled, and the receiver on. The
    /// caller then posts buffers and writes the tail ([`Chip::rx_tail`]).
    pub fn rx_init(&self, bus: u64, bytes: u32) {
        let rctl = self.rd(r::RCTL);
        self.wr(r::RCTL, rctl & !r::RCTL_EN);
        let mut rctl = rctl;
        rctl |= r::RCTL_EN | r::RCTL_BAM | r::RCTL_SZ_2048;
        // No bad packets, no long packets, strip the CRC, no VLAN filter.
        rctl &= !(r::RCTL_SBP | r::RCTL_LPE | r::RCTL_VFE);
        rctl |= r::RCTL_SECRC;
        self.wr(
            r::RXCSUM,
            self.rd(r::RXCSUM) | r::RXCSUM_CRCOFL | r::RXCSUM_IPPCSE,
        );
        self.wr(r::RDLEN, bytes);
        self.wr(r::RDBAH, r::hi(bus));
        self.wr(r::RDBAL, r::lo(bus));
        self.wr(
            r::SRRCTL,
            (2048 >> r::SRRCTL_BSIZEPKT_SHIFT) | r::SRRCTL_DESCTYPE_ADV_ONEBUF,
        );
        self.wr(r::RDH, 0);
        self.wr(r::RDT, 0);
        let rxdctl = self.rd(r::RXDCTL) & !(0x1f | 0x1f << 8 | 0x1f << 16);
        self.wr(
            r::RXDCTL,
            rxdctl
                | r::RX_PTHRESH
                | r::RX_HTHRESH << 8
                | r::RX_WTHRESH << 16
                | r::RXDCTL_QUEUE_ENABLE,
        );
        self.wr(r::RCTL, rctl);
    }

    /// The receive tail: the controller may fill up to, not including, it.
    pub fn rx_tail(&self, tail: u32) {
        self.wr(r::RDT, tail);
    }

    /// The transmit tail: the controller sends up to, not including, it.
    pub fn tx_tail(&self, tail: u32) {
        self.wr(r::TDT, tail);
    }

    /// Stops the receiver and transmitter and masks interrupts, as the
    /// start of `igc_reset_hw_i225` does.
    pub fn stop(&self) -> Result<(), HwError> {
        self.reset()
    }

    // ---- Interrupts ---------------------------------------------------------------

    /// Masks every cause (`IGC_IMC`).
    pub fn mask(&self) {
        self.wr(r::IMC, u32::MAX);
    }

    /// Unmasks the causes the driver takes (`igc_if_enable_intr`).
    pub fn unmask(&self) {
        self.wr(r::IMS, r::IMS_ENABLE);
    }

    /// Reads (and so clears) the causes pending.
    #[must_use]
    pub fn ack(&self) -> u32 {
        self.rd(r::ICR)
    }
}
