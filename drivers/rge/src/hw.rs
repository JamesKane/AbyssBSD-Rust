// SPDX-License-Identifier: BSD-2-Clause
//! Bringing the chip up: register access through the OCP, CSI, and EPHY
//! windows, reset, the MAC and PHY microcode, and the PHY configuration.
//!
//! Only the RTL8126's two revisions are programmed for now (the Orange Pi
//! 6 Plus's NICs are `RTL8126_2`); the other revisions are identified so
//! attach can say what it found.
//!
//! Behavior reference: sys/dev/rge/if_rge_hw.c.

use crate::generated;
use crate::program::{self, PhyTarget};
use crate::regs::{self, mii};

/// The chip revision, from `TXCFG`'s hardware revision field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacType {
    /// RTL8125.
    R25,
    /// RTL8125B.
    R25B,
    /// RTL8125D, first revision.
    R25D1,
    /// RTL8125D, second revision.
    R25D2,
    /// RTL8126, first revision.
    R26_1,
    /// RTL8126, second revision.
    R26_2,
    /// RTL8127.
    R27,
}

impl MacType {
    /// The revision `TXCFG`'s hardware revision field names.
    #[must_use]
    pub const fn of(hwrev: u32) -> Option<Self> {
        Some(match hwrev & regs::TXCFG_HWREV {
            0x6090_0000 => Self::R25,
            0x6410_0000 => Self::R25B,
            0x6490_0000 => Self::R26_1,
            0x64a0_0000 => Self::R26_2,
            0x6880_0000 => Self::R25D1,
            0x6890_0000 => Self::R25D2,
            0x6c90_0000 => Self::R27,
            _ => return None,
        })
    }

    /// FreeBSD's name for it, as `rge` prints it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::R25 => "RTL8125",
            Self::R25B => "RTL8125B",
            Self::R25D1 => "RTL8125D_1",
            Self::R25D2 => "RTL8125D_2",
            Self::R26_1 => "RTL8126_1",
            Self::R26_2 => "RTL8126_2",
            Self::R27 => "RTL8127",
        }
    }

    /// Whether the driver can program it yet.
    #[must_use]
    pub const fn supported(self) -> bool {
        matches!(self, Self::R26_1 | Self::R26_2)
    }
}

/// The controller's registers, and a busy-wait.
pub trait Regs {
    /// Reads 8 bits.
    fn read8(&self, off: usize) -> u8;
    /// Reads 16 bits.
    fn read16(&self, off: usize) -> u16;
    /// Reads 32 bits.
    fn read32(&self, off: usize) -> u32;
    /// Writes 8 bits.
    fn write8(&self, off: usize, v: u8);
    /// Writes 16 bits.
    fn write16(&self, off: usize, v: u16);
    /// Writes 32 bits.
    fn write32(&self, off: usize, v: u32);
    /// Busy-waits (FreeBSD's `DELAY`).
    fn delay_us(&self, us: u32);
}

impl<R: Regs + ?Sized> Regs for &R {
    fn read8(&self, off: usize) -> u8 {
        (**self).read8(off)
    }
    fn read16(&self, off: usize) -> u16 {
        (**self).read16(off)
    }
    fn read32(&self, off: usize) -> u32 {
        (**self).read32(off)
    }
    fn write8(&self, off: usize, v: u8) {
        (**self).write8(off, v);
    }
    fn write16(&self, off: usize, v: u16) {
        (**self).write16(off, v);
    }
    fn write32(&self, off: usize, v: u32) {
        (**self).write32(off, v);
    }
    fn delay_us(&self, us: u32) {
        (**self).delay_us(us);
    }
}

/// Why bringing the chip up failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwError {
    /// The soft reset never completed.
    ResetTimeout,
    /// The PHY never left reset.
    PhyResetTimeout,
}

/// A controller being programmed.
#[derive(Debug)]
pub struct Chip<R> {
    /// Its registers.
    pub regs: R,
    /// Its revision.
    pub mac: MacType,
    /// The MAC microcode version it reported before init.
    pub mcode_version: u64,
    /// The PHY microcode version it reported before configuration.
    pub rcode_version: u16,
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "OCP and EPHY data registers are 16 bits wide"
)]
impl<R: Regs> Chip<R> {
    /// A chip of revision `mac`.
    pub fn new(regs: R, mac: MacType) -> Self {
        Self {
            regs,
            mac,
            mcode_version: 0,
            rcode_version: 0,
        }
    }

    fn set8(&self, off: usize, b: u8) {
        self.regs.write8(off, self.regs.read8(off) | b);
    }

    fn clr8(&self, off: usize, b: u8) {
        self.regs.write8(off, self.regs.read8(off) & !b);
    }

    fn clr16(&self, off: usize, b: u16) {
        self.regs.write16(off, self.regs.read16(off) & !b);
    }

    fn clr32(&self, off: usize, b: u32) {
        self.regs.write32(off, self.regs.read32(off) & !b);
    }

    // The OCP, CSI, and EPHY windows. A write sets the busy flag and the
    // controller clears it when done; a read clears it and the
    // controller sets it once the data is there.

    /// Writes a MAC OCP register.
    pub fn write_mac_ocp(&self, reg: u16, val: u16) {
        let cmd = (u32::from(reg) >> 1) << regs::OCP_ADDR_SHIFT | u32::from(val) | regs::OCP_BUSY;
        self.regs.write32(regs::MACOCP, cmd);
    }

    /// Reads a MAC OCP register.
    pub fn read_mac_ocp(&self, reg: u16) -> u16 {
        self.regs
            .write32(regs::MACOCP, (u32::from(reg) >> 1) << regs::OCP_ADDR_SHIFT);
        (self.regs.read32(regs::MACOCP) & regs::OCP_DATA_MASK) as u16
    }

    fn mac_set(&self, reg: u16, b: u16) {
        self.write_mac_ocp(reg, self.read_mac_ocp(reg) | b);
    }

    fn mac_clr(&self, reg: u16, b: u16) {
        self.write_mac_ocp(reg, self.read_mac_ocp(reg) & !b);
    }

    fn mac_modify(&self, reg: u16, mask: u16, b: u16) {
        self.write_mac_ocp(reg, self.read_mac_ocp(reg) & !mask | b);
    }

    /// Writes a PHY OCP register.
    pub fn write_phy_ocp(&self, reg: u16, val: u16) {
        let cmd = (u32::from(reg) >> 1) << regs::OCP_ADDR_SHIFT | regs::OCP_BUSY | u32::from(val);
        self.regs.write32(regs::PHYOCP, cmd);
        for _ in 0..20_000 {
            self.regs.delay_us(1);
            if self.regs.read32(regs::PHYOCP) & regs::OCP_BUSY == 0 {
                break;
            }
        }
    }

    /// Reads a PHY OCP register.
    pub fn read_phy_ocp(&self, reg: u16) -> u16 {
        self.regs
            .write32(regs::PHYOCP, (u32::from(reg) >> 1) << regs::OCP_ADDR_SHIFT);
        let mut v = 0;
        for _ in 0..20_000 {
            self.regs.delay_us(1);
            v = self.regs.read32(regs::PHYOCP);
            if v & regs::OCP_BUSY != 0 {
                break;
            }
        }
        (v & regs::OCP_DATA_MASK) as u16
    }

    fn phy_set(&self, reg: u16, b: u16) {
        self.write_phy_ocp(reg, self.read_phy_ocp(reg) | b);
    }

    fn phy_clr(&self, reg: u16, b: u16) {
        self.write_phy_ocp(reg, self.read_phy_ocp(reg) & !b);
    }

    /// The PHY OCP address of MII register `reg` (`rge_write_phy` with
    /// address 0: the standard registers, eight to a page from 0xa40).
    const fn mii_ocp(reg: u16) -> u16 {
        let page = regs::PHYBASE + reg / 8;
        (page << 4) + ((0x10 + reg % 8 - 16) << 1)
    }

    /// Writes MII register `reg`.
    pub fn write_mii(&self, reg: u16, val: u16) {
        self.write_phy_ocp(Self::mii_ocp(reg), val);
    }

    /// Reads MII register `reg`.
    pub fn read_mii(&self, reg: u16) -> u16 {
        self.read_phy_ocp(Self::mii_ocp(reg))
    }

    /// Writes a PCIe configuration register through the CSI window.
    pub fn write_csi(&self, reg: u32, val: u32) {
        self.regs.write32(regs::CSIDR, val);
        self.regs.write32(
            regs::CSIAR,
            reg & regs::CSIAR_ADDR_MASK
                | regs::CSIAR_BYTE_EN << regs::CSIAR_BYTE_EN_SHIFT
                | regs::CSIAR_BUSY,
        );
        for _ in 0..20_000 {
            self.regs.delay_us(1);
            if self.regs.read32(regs::CSIAR) & regs::CSIAR_BUSY == 0 {
                break;
            }
        }
        self.regs.delay_us(20);
    }

    /// Reads a PCIe configuration register through the CSI window.
    pub fn read_csi(&self, reg: u32) -> u32 {
        self.regs.write32(
            regs::CSIAR,
            reg & regs::CSIAR_ADDR_MASK | regs::CSIAR_BYTE_EN << regs::CSIAR_BYTE_EN_SHIFT,
        );
        for _ in 0..20_000 {
            self.regs.delay_us(1);
            if self.regs.read32(regs::CSIAR) & regs::CSIAR_BUSY != 0 {
                break;
            }
        }
        self.regs.delay_us(20);
        self.regs.read32(regs::CSIDR)
    }

    fn switch_mcu_ram_page(&self, page: u16) {
        self.mac_modify(0xe446, 0x0003, page);
    }

    /// Stops the receiver and transmitter and soft-resets the MAC
    /// (`rge_reset`).
    ///
    /// # Errors
    ///
    /// [`HwError::ResetTimeout`] if the reset bit never clears.
    pub fn reset(&self) -> Result<(), HwError> {
        self.clr32(regs::RXCFG, regs::RXCFG_FILTERS);
        // Enable the RXDV gate.
        self.set8(regs::PPSW, 0x08);
        self.set8(regs::CMD, regs::CMD_STOPREQ);
        self.regs.delay_us(200);
        let empty = regs::MCUCMD_RXFIFO_EMPTY | regs::MCUCMD_TXFIFO_EMPTY;
        for _ in 0..3000 {
            self.regs.delay_us(50);
            if self.regs.read8(regs::MCUCMD) & empty == empty {
                break;
            }
        }
        if self.mac != MacType::R25 {
            for _ in 0..3000 {
                self.regs.delay_us(50);
                if self.regs.read16(regs::IM) & 0x0103 == 0x0103 {
                    break;
                }
            }
        }
        let cmd = self.regs.read8(regs::CMD);
        self.regs
            .write8(regs::CMD, cmd & (regs::CMD_TXENB | regs::CMD_RXENB));
        self.regs.write8(regs::CMD, regs::CMD_RESET);
        for _ in 0..regs::TIMEOUT {
            self.regs.delay_us(100);
            if self.regs.read8(regs::CMD) & regs::CMD_RESET == 0 {
                return Ok(());
            }
        }
        Err(HwError::ResetTimeout)
    }

    /// Takes the chip from firmware's out-of-band management
    /// (`rge_exit_oob`).
    fn exit_oob(&self) -> Result<(), HwError> {
        // Disable RealWoW.
        self.write_mac_ocp(0xc0bc, 0x00ff);
        self.reset()?;
        // Disable out-of-band management.
        self.clr8(regs::MCUCMD, regs::MCUCMD_IS_OOB);
        self.mac_clr(0xe8de, 0x4000);
        let twi = |this: &Self| {
            for _ in 0..10 {
                this.regs.delay_us(100);
                if this.regs.read16(regs::TWICMD) & 0x0200 != 0 {
                    break;
                }
            }
        };
        twi(self);
        self.write_mac_ocp(0xc0aa, 0x07d0);
        self.write_mac_ocp(0xc0a6, 0x01b5);
        self.write_mac_ocp(0xc01e, 0x5555);
        twi(self);
        if self.read_mac_ocp(0xd42c) & 0x0100 != 0 {
            for _ in 0..regs::TIMEOUT {
                if self.read_phy_ocp(0xa420) & 0x0007 == 2 {
                    break;
                }
                self.regs.delay_us(1000);
            }
            self.mac_clr(0xd42c, 0x0100);
            if self.mac != MacType::R25 {
                self.phy_clr(0xa466, 0x0001);
            }
            self.phy_clr(0xa468, 0x000a);
        }
        Ok(())
    }

    /// Powers the PHY up or down (`rge_set_phy_power`).
    pub fn set_phy_power(&self, on: bool) {
        if on {
            self.set8(regs::PMCH, 0xc0);
            self.write_mii(mii::BMCR, mii::BMCR_AUTOEN);
            for _ in 0..regs::TIMEOUT {
                if self.read_phy_ocp(0xa420) & 0x0007 == 3 {
                    break;
                }
                self.regs.delay_us(1000);
            }
        } else {
            self.write_mii(mii::BMCR, mii::BMCR_AUTOEN | mii::BMCR_PDOWN);
            self.clr8(regs::PMCH, 0x80);
            self.clr8(regs::PPSW, 0x40);
        }
    }

    /// Turns off ASPM and clock requests (`rge_disable_aspm_clkreq`).
    pub fn disable_aspm_clkreq(&self) {
        let unlock = self.regs.read8(regs::EECMD) & regs::EECMD_WRITECFG != regs::EECMD_WRITECFG;
        if unlock {
            self.set8(regs::EECMD, regs::EECMD_WRITECFG);
        }
        // The RTL8126's clock-request enable is in INT_CFG0.
        self.clr8(regs::INT_CFG0, 0x08);
        self.clr8(regs::CFG5, regs::CFG5_PME_STS);
        if unlock {
            self.clr8(regs::EECMD, regs::EECMD_WRITECFG);
        }
    }

    /// The MAC microcode a revision loads.
    fn mac_bps(&self) -> &'static [(u16, u16)] {
        match self.mac {
            MacType::R26_1 => generated::R26_1_MAC_BPS,
            _ => generated::R26_2_MAC_BPS,
        }
    }

    /// The version a microcode table carries: its last four values.
    #[must_use]
    pub fn bin_version(bps: &[(u16, u16)]) -> u64 {
        bps[bps.len().saturating_sub(4)..]
            .iter()
            .fold(0, |v, &(_, x)| v << 16 | u64::from(x))
    }

    /// Loads the MAC microcode unless the chip already runs it, then arms
    /// its breakpoints (`rge_mac_config_ext_mcu`, RTL8126 and RTL8127).
    fn mac_config_ext_mcu(&mut self) {
        self.switch_mcu_ram_page(2);
        self.mcode_version = 0;
        for i in (0..8).step_by(2) {
            self.mcode_version =
                self.mcode_version << 16 | u64::from(self.read_mac_ocp(0xf9f8 + i));
        }
        self.switch_mcu_ram_page(0);
        let bps = self.mac_bps();
        let version = Self::bin_version(bps);
        if self.mcode_version != version {
            self.switch_mcu_ram_page(0);
            match self.mac {
                MacType::R26_1 => {
                    for &(r, v) in bps {
                        self.write_mac_ocp(r, v);
                    }
                }
                _ => {
                    // 256 words to page 0, the rest to page 1.
                    let (first, rest) = bps.split_at(bps.len().min(256));
                    for &(r, v) in first {
                        self.write_mac_ocp(r, v);
                    }
                    self.switch_mcu_ram_page(1);
                    for &(r, v) in rest {
                        self.write_mac_ocp(r, v);
                    }
                }
            }
        }
        let breakpoints: &[(u16, u16)] = match self.mac {
            MacType::R26_1 => &[
                (0xfc26, 0x8000),
                (0xfc2c, 0x2360),
                (0xfc2e, 0x14a4),
                (0xfc30, 0x415e),
                (0xfc32, 0x41e4),
                (0xfc34, 0x4280),
                (0xfc36, 0x234a),
                (0xfc48, 0x00fc),
            ],
            _ => &[
                (0xfc26, 0x8000),
                (0xfc2c, 0x14a4),
                (0xfc2e, 0x4176),
                (0xfc30, 0x41fc),
                (0xfc32, 0x4298),
                (0xfc3a, 0x234a),
                (0xfc48, 0x023c),
            ],
        };
        for &(r, v) in breakpoints {
            self.write_mac_ocp(r, v);
        }
        // Record the version, most significant word last.
        self.switch_mcu_ram_page(2);
        let mut v = version;
        for i in (0..8).step_by(2) {
            self.write_mac_ocp(0xf9f8 + 6 - i, v as u16);
            v >>= 16;
        }
        self.switch_mcu_ram_page(0);
    }

    /// MAC initialisation (`rge_hw_init`).
    fn hw_init(&mut self) {
        self.disable_aspm_clkreq();
        self.clr8(0xf1, 0x80);
        // Disable UPS.
        self.mac_clr(0xd40a, 0x0010);
        // Disable the MAC MCU.
        self.disable_aspm_clkreq();
        self.write_mac_ocp(0xfc48, 0);
        for reg in (0xfc28..0xfc48).step_by(2) {
            self.write_mac_ocp(reg, 0);
        }
        self.regs.delay_us(3000);
        self.write_mac_ocp(0xfc26, 0);
        self.mac_config_ext_mcu();
        // Set PCIe uncorrectable error status.
        let v = self.read_csi(0x108);
        self.write_csi(0x108, v | 0x0010_0000);
    }

    /// Masks interrupts and resets (`rge_hw_reset`).
    pub fn hw_reset(&self) -> Result<(), HwError> {
        self.regs.write32(regs::IMR, 0);
        let isr = self.regs.read32(regs::ISR);
        self.regs.write32(regs::ISR, isr);
        for t in [
            regs::TIMERINT0,
            regs::TIMERINT1,
            regs::TIMERINT2,
            regs::TIMERINT3,
        ] {
            self.regs.write32(t, 0);
        }
        self.reset()
    }

    /// Power-on and MAC setup (`rge_chipinit`).
    ///
    /// # Errors
    ///
    /// A reset that never completes.
    pub fn chip_init(&mut self) -> Result<(), HwError> {
        self.exit_oob()?;
        self.set_phy_power(true);
        self.hw_init();
        self.hw_reset()
    }

    /// Resets the PHY, loads its microcode and configuration, and turns
    /// off EEE (`rge_phy_config`; the RTL8126 has no EPHY setup).
    ///
    /// # Errors
    ///
    /// [`HwError::PhyResetTimeout`] if the PHY never leaves reset.
    pub fn phy_config(&mut self) -> Result<(), HwError> {
        let anar = self.read_mii(mii::ANAR);
        self.write_mii(
            mii::ANAR,
            anar & !(mii::ANAR_TX_FD | mii::ANAR_TX | mii::ANAR_10_FD | mii::ANAR_10),
        );
        let gtcr = self.read_mii(mii::GTCR);
        self.write_mii(
            mii::GTCR,
            gtcr & !(mii::GTCR_ADV_1000TFDX | mii::GTCR_ADV_1000THDX),
        );
        self.phy_clr(0xa5d4, regs::ADV_5000TFDX | regs::ADV_2500TFDX);
        self.write_mii(
            mii::BMCR,
            mii::BMCR_RESET | mii::BMCR_AUTOEN | mii::BMCR_STARTNEG,
        );
        let mut reset = false;
        for _ in 0..2500 {
            if self.read_mii(mii::BMCR) & mii::BMCR_RESET == 0 {
                reset = true;
                break;
            }
            self.regs.delay_us(1000);
        }
        if !reset {
            return Err(HwError::PhyResetTimeout);
        }
        // The PHY microcode version it runs.
        self.write_phy_ocp(0xa436, 0x801e);
        self.rcode_version = self.read_phy_ocp(0xa438);
        let config = match self.mac {
            MacType::R26_1 => generated::R26_1_PHY_CONFIG,
            _ => generated::R26_2_PHY_CONFIG,
        };
        program::run(config, &mut &*self);
        self.phy_clr(0xa5b4, 0x8000);
        // Disable EEE.
        self.mac_clr(0xe040, 0x0003);
        self.phy_clr(0xa5d0, 0x0006);
        self.phy_clr(0xa6d4, 0x0001);
        self.phy_clr(0xa6d4, 0x0002);
        self.phy_clr(0xa6d8, 0x0010);
        self.phy_clr(0xa428, 0x0080);
        self.phy_clr(0xa4a2, 0x0200);
        // Disable advanced EEE.
        self.mac_clr(0xe052, 0x0001);
        self.phy_clr(0xa442, 0x3000);
        self.phy_clr(0xa430, 0x8000);
        Ok(())
    }

    /// Starts or ends patching the PHY's microcontroller
    /// (`rge_patch_phy_mcu`); `false` if it never acknowledged.
    pub fn patch_phy_mcu(&self, on: bool) -> bool {
        if on {
            self.phy_set(0xb820, 0x0010);
        } else {
            self.phy_clr(0xb820, 0x0010);
        }
        for _ in 0..1000 {
            if (self.read_phy_ocp(0xb800) & 0x0040 != 0) == on {
                return true;
            }
            self.regs.delay_us(100);
        }
        false
    }

    /// Loads the PHY microcode unless the PHY runs `version`, and records
    /// it (`rge_phy_config_mcu`).
    fn load_phy_mcu(&self, version: u16) {
        if self.rcode_version == version {
            return;
        }
        self.patch_phy_mcu(true);
        match self.mac {
            MacType::R26_1 => {
                // In two parts, the patch window reopened between them.
                let (first, rest) = generated::R26_1_PHY_MCU.split_at(6989);
                for &(r, v) in first {
                    self.write_phy_ocp(r, v);
                }
                self.patch_phy_mcu(false);
                self.patch_phy_mcu(true);
                for &(r, v) in rest {
                    self.write_phy_ocp(r, v);
                }
            }
            _ => {
                for &(r, v) in generated::R26_2_PHY_MCU {
                    self.write_phy_ocp(r, v);
                }
            }
        }
        self.patch_phy_mcu(false);
        self.write_phy_ocp(0xa436, 0x801e);
        self.write_phy_ocp(0xa438, version);
    }

    /// Programs the station address (`rge_set_macaddr`).
    pub fn set_macaddr(&self, a: [u8; 6]) {
        self.set8(regs::EECMD, regs::EECMD_WRITECFG);
        self.regs
            .write32(regs::MAC0, u32::from_le_bytes([a[0], a[1], a[2], a[3]]));
        self.regs
            .write32(regs::MAC4, u32::from(u16::from_le_bytes([a[4], a[5]])));
        self.clr8(regs::EECMD, regs::EECMD_WRITECFG);
    }

    /// The factory station address (`rge_get_macaddr`: the backup address
    /// registers, which firmware cannot change), programmed back.
    pub fn get_macaddr(&self) -> [u8; 6] {
        let lo = self.regs.read32(regs::ADDR0).to_le_bytes();
        let hi = self.regs.read16(regs::ADDR1).to_le_bytes();
        let a = [lo[0], lo[1], lo[2], lo[3], hi[0], hi[1]];
        self.set_macaddr(a);
        a
    }

    /// Whether the link is up.
    pub fn link_up(&self) -> bool {
        self.regs.read16(regs::PHYSTAT) & regs::PHYSTAT_LINK != 0
    }

    /// The PHY's link status register.
    pub fn phystat(&self) -> u16 {
        self.regs.read16(regs::PHYSTAT)
    }

    /// Stops reception and transmission, masks interrupts, and resets
    /// (the hardware part of `rge_stop_locked`).
    ///
    /// # Errors
    ///
    /// A reset that never completes.
    pub fn stop(&self) -> Result<(), HwError> {
        self.clr32(regs::RXCFG, regs::RXCFG_FILTERS);
        let r = self.hw_reset();
        self.mac_clr(0xc0ac, 0x1f80);
        r
    }

    /// Programs the MAC for operation with the rings at `rx` and `tx`
    /// (bus addresses), after [`Chip::chip_init`] and
    /// [`Chip::phy_config`]: the middle of `rge_init_locked`, up to
    /// enabling transmit and receive.
    pub fn configure(&self, rx: u64, tx: u64, rx_csum: bool) {
        self.set8(regs::EECMD, regs::EECMD_WRITECFG);
        self.clr8(0xf1, 0x80);
        self.disable_aspm_clkreq();
        self.regs
            .write16(regs::EEE_TXIDLE_TIMER, regs::JUMBO_MTU + 14 + 32);
        self.regs.write32(regs::RXDESC_ADDR_LO, rx as u32);
        self.regs.write32(regs::RXDESC_ADDR_HI, (rx >> 32) as u32);
        self.regs.write32(regs::TXDESC_ADDR_LO, tx as u32);
        self.regs.write32(regs::TXDESC_ADDR_HI, (tx >> 32) as u32);
        self.regs.write32(regs::RXCFG, regs::RXCFG_CONFIG_8126);
        self.regs.write32(regs::TXCFG, regs::TXCFG_CONFIG);
        let v = self.read_csi(0x70c) & !0x3f00_0000;
        self.write_csi(0x70c, v | 0x2700_0000);
        // Disable the L1 timeout.
        let v = self.read_csi(0x890) & !0x0000_0001;
        self.write_csi(0x890, v);
        self.regs.write8(regs::RSS_CTRL, 0);
        // One receive queue.
        let v = self.regs.read16(regs::RXQUEUE_CTRL) & !0x001c;
        self.regs.write16(regs::RXQUEUE_CTRL, v);
        self.clr8(regs::CFG1, regs::CFG1_SPEED_DOWN);
        self.write_mac_ocp(0xc140, 0xffff);
        self.write_mac_ocp(0xc142, 0xffff);
        self.mac_set(0xeb58, 0x0001);
        self.clr8(0xd8, 0x02);
        self.mac_modify(0xe614, 0x0700, 0x0300);
        // One queue: (fls(1) - 1) in bits 11:10.
        self.mac_modify(0xe63e, 0x0c00, 0);
        self.mac_modify(0xe63e, 0x0030, 0x0020);
        self.mac_clr(0xc0b4, 0x0001);
        self.mac_set(0xc0b4, 0x0001);
        self.mac_set(0xc0b4, 0x000c);
        self.mac_modify(0xeb6a, 0x00ff, 0x0033);
        self.mac_modify(0xeb50, 0x03e0, 0x0040);
        self.mac_clr(0xe056, 0x00f0);
        self.regs.write8(regs::TDFNR, 0x10);
        self.mac_clr(0xe040, 0x1000);
        self.mac_modify(0xea1c, 0x0003, 0x0001);
        self.write_mac_ocp(0xe0c0, 0x4000);
        self.mac_set(0xe052, 0x0060);
        self.mac_clr(0xe052, 0x0088);
        self.mac_modify(0xd430, 0x0fff, 0x045f);
        self.set8(regs::DLLPR, regs::DLLPR_PFM_EN | regs::DLLPR_TX_10M_PS_EN);
        // Disable EEE plus.
        self.mac_clr(0xe080, 0x0002);
        self.mac_clr(0xea1c, 0x0304);
        // Clear the TCAM entries.
        self.mac_set(0xeb54, 0x0001);
        self.regs.delay_us(1);
        self.mac_clr(0xeb54, 0x0001);
        self.clr16(0x1880, 0x0030);
        // Interrupt type.
        self.clr8(regs::INT_CFG0, regs::INT_CFG0_EN);
        for t in [
            regs::TIMERINT0,
            regs::TIMERINT1,
            regs::TIMERINT2,
            regs::TIMERINT3,
        ] {
            self.regs.write32(t, 0);
        }
        // Clear the interrupt moderation timers (32 on the RTL8126).
        for i in 0..32 {
            self.regs.write32(regs::INTMITI + 4 * i, 0);
        }
        self.clr8(
            regs::INT_CFG0,
            regs::INT_CFG0_TIMEOUT_BYPASS
                | regs::INT_CFG0_RDU_BYPASS_8126
                | regs::INT_CFG0_MITIGATION_BYPASS,
        );
        self.regs.write16(regs::INT_CFG1, 0);
        self.mac_set(0xc0ac, 0x1f80);
        self.write_mac_ocp(0xe098, 0xc302);
        self.mac_clr(0xe032, 0x0003);
        let v = self.read_csi(0x98) & !0x0000_ff00;
        self.write_csi(0x98, v);
        self.mac_clr(0xe092, 0x00ff);
        // No hardware VLAN tagging yet.
        self.clr32(regs::RXCFG, regs::RXCFG_VLANSTRIP);
        let c = self.regs.read16(regs::CPLUSCMD);
        let c = if rx_csum {
            c | regs::CPLUSCMD_RXCSUM
        } else {
            c & !regs::CPLUSCMD_RXCSUM
        };
        self.regs.write16(regs::CPLUSCMD, c);
        let _ = self.regs.read16(regs::CPLUSCMD);
        self.regs.write16(regs::RXMAXSIZE, regs::JUMBO_FRAMELEN);
        // Disable the RXDV gate.
        self.clr8(regs::PPSW, 0x08);
        self.regs.delay_us(2000);
    }

    /// Sets the receive filter (`rge_iff_locked`): unicast to us,
    /// broadcast, and multicast through `hash` always (all ones for
    /// all-multicast or promiscuous reception), every unicast address too
    /// if `promisc`.
    pub fn set_filter(&self, promisc: bool, hash: [u32; 2]) {
        let mut rx = self.regs.read32(regs::RXCFG) & !(regs::RXCFG_ALLPHYS | regs::RXCFG_MULTI);
        rx |= regs::RXCFG_INDIV | regs::RXCFG_BROAD | regs::RXCFG_MULTI;
        if promisc {
            rx |= regs::RXCFG_ALLPHYS;
        }
        self.regs.write32(regs::RXCFG, rx);
        // The hash words go in byte-swapped and in reverse order.
        self.regs.write32(regs::MAR0, hash[1].swap_bytes());
        self.regs.write32(regs::MAR4, hash[0].swap_bytes());
    }

    /// Advertises every speed and pause, and restarts autonegotiation
    /// (`rge_ifmedia_upd` for `IFM_AUTO`).
    pub fn autoneg(&self) {
        // Disable Gigabit Lite.
        self.phy_clr(0xa428, 0x0200);
        self.phy_clr(0xa5ea, 0x0001);
        self.phy_clr(0xa5ea, 0x0007);
        let adv = self.read_phy_ocp(0xa5d4) & !(regs::ADV_5000TFDX | regs::ADV_2500TFDX)
            | regs::ADV_2500TFDX
            | regs::ADV_5000TFDX;
        let anar = mii::ANAR_TX_FD | mii::ANAR_TX | mii::ANAR_10_FD | mii::ANAR_10;
        self.write_mii(mii::ANAR, anar | mii::ANAR_PAUSE_ASYM | mii::ANAR_PAUSE_SYM);
        self.write_mii(mii::GTCR, mii::GTCR_ADV_1000TFDX | mii::GTCR_ADV_1000THDX);
        self.write_phy_ocp(0xa5d4, adv);
        self.write_mii(
            mii::BMCR,
            mii::BMCR_RESET | mii::BMCR_AUTOEN | mii::BMCR_STARTNEG,
        );
    }

    /// Finishes `rge_init_locked`: ends configuration writes, starts
    /// autonegotiation, starts the receiver and transmitter, and unmasks
    /// every completion and link interrupt, unmoderated
    /// (`RGE_IMTYPE_NONE`).
    pub fn start(&self) {
        self.disable_aspm_clkreq();
        self.clr8(regs::EECMD, regs::EECMD_WRITECFG);
        self.regs.delay_us(10);
        self.autoneg();
        self.regs
            .write8(regs::CMD, regs::CMD_TXENB | regs::CMD_RXENB);
        self.regs.write16(regs::IM, 0);
        self.regs.write32(regs::TIMERINT0, 0);
        self.regs.write32(regs::IMR, regs::INTRS);
    }

    /// `rge_init_locked` up to starting the chip, from any state: stops
    /// it, programs the station address, has `rings` reset the descriptor
    /// rings (the controller is stopped while it runs), powers up and
    /// initializes the MAC and PHY again, and configures the MAC for the
    /// rings at `rx` and `tx`. [`Chip::set_filter`] and [`Chip::start`]
    /// follow.
    ///
    /// The MAC and PHY microcode versions are read afresh, so microcode
    /// the chip still runs is not loaded again.
    ///
    /// # Errors
    ///
    /// A reset that never completes, or a PHY that never leaves reset.
    pub fn init_locked(
        &self,
        lladdr: [u8; 6],
        rx: u64,
        tx: u64,
        rx_csum: bool,
        rings: impl FnOnce(),
    ) -> Result<(), HwError>
    where
        R: Clone,
    {
        self.stop()?;
        self.set_macaddr(lladdr);
        rings();
        let mut c = Self::new(self.regs.clone(), self.mac);
        c.chip_init()?;
        c.phy_config()?;
        self.configure(rx, tx, rx_csum);
        Ok(())
    }

    /// Whether the device still answers: `TXCFG` holds the revision, never
    /// all ones.
    pub fn responding(&self) -> bool {
        crate::health::responding(self.regs.read32(regs::TXCFG))
    }

    /// Masks every interrupt (the filter, before the thread runs).
    pub fn mask(&self) {
        self.regs.write32(regs::IMR, 0);
    }

    /// Unmasks the interrupts [`Chip::start`] enabled.
    pub fn unmask(&self) {
        self.regs.write32(regs::IMR, regs::INTRS);
    }

    /// Reads and acknowledges what is pending.
    pub fn ack(&self) -> u32 {
        let st = self.regs.read32(regs::ISR);
        if st != 0 {
            self.regs.write32(regs::ISR, st);
        }
        st
    }

    /// Tells the transmitter new descriptors are ready (after
    /// [`hal::dma::doorbell_barrier`]).
    pub fn kick_tx(&self) {
        hal::dma::doorbell_barrier();
        self.regs.write16(regs::TXSTART, regs::TXSTART_START);
    }
}

impl<R: Regs> PhyTarget for &Chip<R> {
    fn read_phy_ocp(&mut self, reg: u16) -> u16 {
        Chip::read_phy_ocp(self, reg)
    }
    fn write_phy_ocp(&mut self, reg: u16, val: u16) {
        Chip::write_phy_ocp(self, reg, val);
    }
    fn patch_phy_mcu(&mut self, on: bool) {
        Chip::patch_phy_mcu(self, on);
    }
    fn load_phy_mcu(&mut self, version: u16) {
        Chip::load_phy_mcu(self, version);
    }
}
