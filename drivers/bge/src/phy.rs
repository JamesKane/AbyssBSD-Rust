// SPDX-License-Identifier: BSD-2-Clause
//! The BCM57765's internal PHY, as FreeBSD's `miibus` and `brgphy` drive
//! it (`sys/dev/mii`): probe by ID, reset, autonegotiation, and the link's
//! speed and duplex from the auxiliary status register.
//!
//! For this PHY (OUI `BROADCOM3` 0x001be9, model 0x24) `brgphy_reset`
//! returns straight after the BMCR reset, so none of brgphy's bug fixups,
//! jumbo or WireSpeed settings apply (brgphy.c:976-981). A PHY reporting
//! another Broadcom model is driven the same way, without those settings
//! either; that is to be checked once the Mac Pro's PHY ID is captured
//! (docs/MACPRO.md MP0).

/// The registers used (`mii.h`, `brgphyreg.h`).
pub mod reg {
    pub const BMCR: u8 = 0x00;
    pub const BMSR: u8 = 0x01;
    pub const PHYIDR1: u8 = 0x02;
    pub const PHYIDR2: u8 = 0x03;
    pub const ANAR: u8 = 0x04;
    pub const ANLPAR: u8 = 0x05;
    pub const GTCR: u8 = 0x09;
    pub const GTSR: u8 = 0x0a;
    pub const AUXSTS: u8 = 0x19;
    pub const IMR: u8 = 0x1b;
}

pub mod bmcr {
    pub const RESET: u16 = 0x8000;
    pub const AUTOEN: u16 = 0x1000;
    pub const STARTNEG: u16 = 0x0200;
}

pub mod bmsr {
    pub const LINK: u16 = 0x0004;
    pub const ACOMP: u16 = 0x0020;
    pub const EXTSTAT: u16 = 0x0100;
    /// `BMSR_MEDIAMASK`: the 10/100 abilities.
    pub const MEDIAMASK: u16 = 0xf800;
}

/// `ANAR_CSMA`, and the pause bits (`BRGPHY_ANAR_PC`, `_ASP`).
const ANAR_CSMA: u16 = 0x0001;
const ANAR_PC: u16 = 0x0400;
const ANAR_ASP: u16 = 0x0800;
/// `1000CTL`: advertise 1000 full and half duplex.
const GTCR_ADV: u16 = 0x0300;
/// The PHY's interrupt sources 7:0 unmasked (`brgphy_mii_phy_auto`).
const IMR_UNMASKED: u16 = 0xff00;
/// `BRGPHY_AUXSTS_LINK` and `_AN_RES`.
const AUXSTS_LINK: u16 = 0x0004;
const AUXSTS_AN_RES: u16 = 0x0700;
/// `1000STS`'s master bit.
const GTSR_MASTER: u16 = 0x4000;

/// MII register access.
pub trait Mii {
    /// Reads a register; `None` if the access failed.
    fn read(&self, reg: u8) -> Option<u16>;
    /// Writes a register.
    fn write(&self, reg: u8, v: u16) -> Option<()>;
    /// Waits.
    fn delay_us(&self, us: u32);
}

/// The PHY's ID (`mii_oui`, `MII_MODEL`, `MII_REV`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhyId {
    pub oui: u32,
    pub model: u16,
    pub rev: u16,
}

/// `MII_OUI_BROADCOM3` and `MII_MODEL_BROADCOM3_BCM57765`.
pub const OUI_BROADCOM3: u32 = 0x0000_1be9;
pub const MODEL_BCM57765: u16 = 0x24;

impl PhyId {
    /// From `PHYIDR1` and `PHYIDR2` (`mii_oui`: the 22 OUI bits, each
    /// byte bit-reversed).
    #[must_use]
    pub fn of(id1: u16, id2: u16) -> Self {
        let h = u32::from(id1) << 6 | u32::from(id2) >> 10;
        let mut oui = 0u32;
        for i in 0..3 {
            let b = ((h >> (8 * i)) & 0xff) as u8;
            oui |= u32::from(b.reverse_bits()) << (8 * i);
        }
        Self {
            oui,
            model: (id2 & 0x03f0) >> 4,
            rev: id2 & 0xf,
        }
    }
}

/// A link's speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Ten,
    Hundred,
    Thousand,
}

/// What `brgphy_status` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Negotiating, or no link.
    Down,
    /// Up.
    Up {
        speed: Speed,
        full_duplex: bool,
        tx_pause: bool,
        rx_pause: bool,
        master: bool,
    },
}

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyError {
    /// No PHY answers (`mii_attach`: BMSR 0, all ones, or no media).
    Absent,
    /// An access failed.
    Io,
}

/// The PHY: its ID and abilities.
#[derive(Clone, Copy, Debug)]
pub struct Phy {
    pub id: PhyId,
    /// `mii_capabilities`: BMSR as read at attach.
    pub bmsr: u16,
}

impl Phy {
    /// Probes the PHY (`mii_attach`, mii.c:467-492) and resets it
    /// (`brgphy_attach`).
    pub fn attach(m: &impl Mii) -> Result<Self, PhyError> {
        let bmsr = m.read(reg::BMSR).ok_or(PhyError::Io)?;
        if bmsr == 0 || bmsr == 0xffff || bmsr & (bmsr::EXTSTAT | bmsr::MEDIAMASK) == 0 {
            return Err(PhyError::Absent);
        }
        let id1 = m.read(reg::PHYIDR1).ok_or(PhyError::Io)?;
        let id2 = m.read(reg::PHYIDR2).ok_or(PhyError::Io)?;
        let phy = Self {
            id: PhyId::of(id1, id2),
            bmsr,
        };
        phy.reset(m)?;
        Ok(phy)
    }

    /// `brgphy_reset`: BMCR reset, up to 100 ms for it to clear.
    pub fn reset(&self, m: &impl Mii) -> Result<(), PhyError> {
        m.write(reg::BMCR, bmcr::RESET).ok_or(PhyError::Io)?;
        for _ in 0..100 {
            m.delay_us(1000);
            if m.read(reg::BMCR).ok_or(PhyError::Io)? & bmcr::RESET == 0 {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Starts autonegotiation (`brgphy_mii_phy_auto`): the 10/100
    /// abilities, 1000 full and half duplex, pause only if asked (the
    /// default `auto` medium does not), and the PHY's interrupts unmasked.
    pub fn autonegotiate(&self, m: &impl Mii, pause: bool) -> Result<(), PhyError> {
        self.reset(m)?;
        let mut anar = ((self.bmsr & bmsr::MEDIAMASK) >> 6) | ANAR_CSMA;
        if pause {
            anar |= ANAR_PC | ANAR_ASP;
        }
        m.write(reg::ANAR, anar).ok_or(PhyError::Io)?;
        m.write(reg::GTCR, GTCR_ADV).ok_or(PhyError::Io)?;
        let _ = m.read(reg::GTCR);
        m.write(reg::BMCR, bmcr::AUTOEN | bmcr::STARTNEG)
            .ok_or(PhyError::Io)?;
        m.write(reg::IMR, IMR_UNMASKED).ok_or(PhyError::Io)?;
        Ok(())
    }

    /// The link (`brgphy_status`): down while negotiating; else speed and
    /// duplex from `AUXSTS`, read after `1000STS` (reading ANAR, ANLPAR or
    /// 1000STS after AUXSTS wedges some Broadcom PHYs).
    pub fn status(&self, m: &impl Mii) -> Result<Status, PhyError> {
        let _ = m.read(reg::BMSR).ok_or(PhyError::Io)?;
        let bmsr = m.read(reg::BMSR).ok_or(PhyError::Io)?;
        let bmcr = m.read(reg::BMCR).ok_or(PhyError::Io)?;
        if bmcr & bmcr::AUTOEN != 0 && bmsr & bmsr::ACOMP == 0 {
            return Ok(Status::Down);
        }
        let (tx_pause, rx_pause) = flow(
            m.read(reg::ANAR).ok_or(PhyError::Io)?,
            m.read(reg::ANLPAR).ok_or(PhyError::Io)?,
        );
        let gtsr = m.read(reg::GTSR).ok_or(PhyError::Io)?;
        let aux = m.read(reg::AUXSTS).ok_or(PhyError::Io)?;
        if aux & AUXSTS_LINK == 0 {
            return Ok(Status::Down);
        }
        let (speed, full_duplex) = match aux & AUXSTS_AN_RES {
            0x700 => (Speed::Thousand, true),
            0x600 => (Speed::Thousand, false),
            0x500 => (Speed::Hundred, true),
            // 100BASE-T4 (0x400) as 100 half duplex, as `IFM_100_T4` is.
            0x400 | 0x300 => (Speed::Hundred, false),
            0x200 => (Speed::Ten, true),
            0x100 => (Speed::Ten, false),
            _ => return Ok(Status::Down),
        };
        Ok(Status::Up {
            speed,
            full_duplex,
            tx_pause: full_duplex && tx_pause,
            rx_pause: full_duplex && rx_pause,
            master: speed == Speed::Thousand && gtsr & GTSR_MASTER != 0,
        })
    }
}

/// The pause both sides agreed on (`mii_phy_flowstatus`, 802.3 Annex
/// 28B): (we send pause frames, we obey them).
fn flow(anar: u16, anlpar: u16) -> (bool, bool) {
    let (pc, asp) = (ANAR_PC, ANAR_ASP);
    if anar & pc != 0 && anlpar & pc != 0 {
        return (true, true);
    }
    if anar & pc == 0 && anar & asp != 0 && anlpar & pc != 0 && anlpar & asp != 0 {
        return (true, false);
    }
    if anar & pc != 0 && anar & asp != 0 && anlpar & pc == 0 && anlpar & asp != 0 {
        return (false, true);
    }
    (false, false)
}
