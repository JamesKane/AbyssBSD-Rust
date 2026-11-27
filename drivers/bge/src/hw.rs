// SPDX-License-Identifier: BSD-2-Clause
//! Bringing a BCM57765-family chip up and down, over [`Regs`], as
//! FreeBSD's `bge` does on that path (`if_bge.c`; the lines cited are
//! 16-CURRENT's): identification (`bge_chipid`), the core reset and the
//! bootcode handshake (`bge_reset`), `bge_chipinit`, `bge_blockinit`, the
//! station address (`bge_get_eaddr`), MII access, the receive filter, the
//! link's MAC settings (`bge_miibus_statchg`) and `bge_stop`.
//!
//! Left out, each for a reason:
//! - ASF/IPMI firmware: detected and reported, never handshaken with.
//! - The jumbo ring is programmed disabled, as FreeBSD's is below a jumbo
//!   MTU, and never filled: the driver does not offer jumbo frames yet.
//! - TSO: the read DMA's TSO bits stay clear until the driver offers it.
//! - `PCIM_CMD_MWIEN` is not cleared (`bge_chipinit` 1900): configuration
//!   space below 0x40 is the bus's, and PCI Express ignores the bit.

use crate::regs as r;

/// The chip's registers and configuration space.
pub trait Regs {
    /// Reads a 32-bit BAR0 register.
    fn read32(&self, off: usize) -> u32;
    /// Writes a 32-bit BAR0 register.
    fn write32(&self, off: usize, v: u32);
    /// Reads a configuration register.
    fn cfg_read32(&self, off: u16) -> u32;
    /// Writes a configuration register (0x40 and up).
    fn cfg_write32(&self, off: u16, v: u32);
    /// Reads a 16-bit configuration register.
    fn cfg_read16(&self, off: u16) -> u16;
    /// Writes a 16-bit configuration register (0x40 and up).
    fn cfg_write16(&self, off: u16, v: u16);
    /// Saves the configuration header and the MSI capability, which the
    /// core reset clears.
    fn save_config(&self);
    /// Puts them back.
    fn restore_config(&self);
    /// Busy-waits (`DELAY`).
    fn delay_us(&self, us: u32);
}

impl<T: Regs + ?Sized> Regs for &T {
    fn read32(&self, off: usize) -> u32 {
        (**self).read32(off)
    }
    fn write32(&self, off: usize, v: u32) {
        (**self).write32(off, v);
    }
    fn cfg_read32(&self, off: u16) -> u32 {
        (**self).cfg_read32(off)
    }
    fn cfg_write32(&self, off: u16, v: u32) {
        (**self).cfg_write32(off, v);
    }
    fn cfg_read16(&self, off: u16) -> u16 {
        (**self).cfg_read16(off)
    }
    fn cfg_write16(&self, off: u16, v: u16) {
        (**self).cfg_write16(off, v);
    }
    fn save_config(&self) {
        (**self).save_config();
    }
    fn restore_config(&self) {
        (**self).restore_config();
    }
    fn delay_us(&self, us: u32) {
        (**self).delay_us(us);
    }
}

/// `BGE_TIMEOUT`: polls before a wait gives up.
const TIMEOUT: u32 = 100_000;

/// What failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwError {
    /// The chip is not a 57765 or 57766.
    NotSupported(u32),
    /// The buffer manager did not start (`bge_blockinit`).
    BufferManager,
    /// The flow-through queues did not leave reset.
    FtqReset,
    /// Host coalescing did not stop.
    HostCoalescing,
    /// No station address anywhere.
    NoAddress,
    /// The hardware configuration word could not be read, on a 57765
    /// (`bge_attach` 3773: fatal there, tolerated on a 57766).
    NoHwConfig,
    /// The chip is wired to fiber, which the driver does not do.
    Fiber,
    /// An MII access did not finish.
    MiiTimeout,
}

/// The chip's identity (`bge_chipid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChipId(pub u32);

impl ChipId {
    /// `bge_asicrev`: `chipid >> 12`.
    #[must_use]
    pub const fn asicrev(self) -> u32 {
        self.0 >> 12
    }
    /// `bge_chiprev`: `chipid >> 8`.
    #[must_use]
    pub const fn chiprev(self) -> u32 {
        self.0 >> 8
    }
    /// The name `bge_probe` prints.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self.0 {
            0x5778_5000 => "BCM57765 A0",
            0x5778_5100 => "BCM57765 B0",
            _ if self.asicrev() == r::ASICREV_57766 => "BCM57766",
            _ => "unknown BCM57765",
        }
    }
}

/// What the bootcode left: the hardware configuration and whether ASF
/// firmware runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwConfig {
    pub word: u32,
}

impl HwConfig {
    #[must_use]
    pub const fn asf(self) -> bool {
        self.word & r::hwcfg::ASF != 0
    }
    #[must_use]
    pub const fn fiber(self) -> bool {
        self.word & r::hwcfg::MEDIA == r::hwcfg::MEDIA_FIBER
    }
}

/// The ring and block addresses `bge_blockinit` gives the chip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rings {
    /// The standard receive producer ring (512 descriptors).
    pub std_rx: u64,
    /// The jumbo producer ring (programmed disabled).
    pub jumbo_rx: u64,
    /// The receive return ring (1024 descriptors).
    pub rx_return: u64,
    /// The send ring (512 descriptors).
    pub tx: u64,
    /// The status block (32 bytes).
    pub status: u64,
}

/// A link's MAC settings (`bge_miibus_statchg`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkMode {
    pub gigabit: bool,
    pub full_duplex: bool,
    pub tx_pause: bool,
    pub rx_pause: bool,
}

/// The chip.
#[derive(Debug)]
pub struct Chip<R> {
    pub regs: R,
    pub id: ChipId,
    /// PCI Express Device Capabilities' maximum payload, bytes.
    pub mps: u32,
    /// The PCI Express capability's offset.
    pub expcap: u16,
    /// The PHY's MII address (`bge_phy_addr`).
    pub phy: u8,
}

/// Device Control and Device Status, from the PCI Express capability.
const PCIE_DEVCTL: u16 = 8;
const PCIE_DEVSTA: u16 = 10;
const PCIE_DEVCAP: u16 = 4;
/// Device Control: relaxed ordering, no snoop, the read request size.
const DEVCTL_RELAXED: u16 = 0x0010;
const DEVCTL_NOSNOOP: u16 = 0x0800;
const DEVCTL_MRRS: u16 = 0x7000;
/// 4096-byte read requests (`pci_set_max_read_req(dev, 4096)`).
const DEVCTL_MRRS_4096: u16 = 5 << 12;

/// The chip ID (`bge_chipid`): the legacy ID in `MISC_CTL`, or, on the
/// 57765 family, `GEN15_PRODID_ASICREV`.
pub fn chip_id(regs: &impl Regs) -> ChipId {
    let legacy = regs.cfg_read32(r::CFG_MISC_CTL) >> r::misc_ctl::ASICREV_SHIFT;
    if legacy >> 12 == r::ASICREV_USE_PRODID_REG {
        ChipId(regs.cfg_read32(r::CFG_GEN15_PRODID_ASICREV))
    } else {
        ChipId(legacy)
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "register fields and SRAM offsets: MII data is 16 bits, EEPROM bytes 8, SRAM below 64 KiB"
)]
impl<R: Regs> Chip<R> {
    /// The chip, if it is the 57765 family's (`bge_attach`'s checks).
    pub fn new(regs: R, expcap: u16) -> Result<Self, HwError> {
        let id = chip_id(&regs);
        if !matches!(id.asicrev(), r::ASICREV_57765 | r::ASICREV_57766) {
            return Err(HwError::NotSupported(id.0));
        }
        let devcap = regs.cfg_read32(expcap + PCIE_DEVCAP);
        Ok(Self {
            regs,
            id,
            mps: 128 << (devcap & 0x7),
            expcap,
            phy: 1,
        })
    }

    fn wait(&self, us: u32, polls: u32, mut done: impl FnMut(&R) -> bool) -> bool {
        for _ in 0..polls {
            if done(&self.regs) {
                return true;
            }
            self.regs.delay_us(us);
        }
        done(&self.regs)
    }

    fn set(&self, off: usize, bits: u32) {
        self.regs.write32(off, self.regs.read32(off) | bits);
    }

    fn clear(&self, off: usize, bits: u32) {
        self.regs.write32(off, self.regs.read32(off) & !bits);
    }

    /// Reads an SRAM word indirectly (`bge_readmem_ind`).
    pub fn sram_read(&self, off: u32) -> u32 {
        self.regs.cfg_write32(r::CFG_MEMWIN_BASE, off);
        let v = self.regs.cfg_read32(r::CFG_MEMWIN_DATA);
        self.regs.cfg_write32(r::CFG_MEMWIN_BASE, 0);
        v
    }

    /// Writes an SRAM word indirectly (`bge_writemem_ind`).
    pub fn sram_write(&self, off: u32, v: u32) {
        self.regs.cfg_write32(r::CFG_MEMWIN_BASE, off);
        self.regs.cfg_write32(r::CFG_MEMWIN_DATA, v);
        self.regs.cfg_write32(r::CFG_MEMWIN_BASE, 0);
    }

    /// Writes an SRAM word through the window, whose base is 0 below 64 KiB.
    fn window_write(&self, off: u32, v: u32) {
        self.regs.write32(r::MEMWIN + off as usize, v);
    }

    /// `bge_reset` (4009-4260) on this chip's path. A firmware handshake
    /// that times out is reported and not fatal, as FreeBSD's.
    pub fn reset(&self) -> Result<(), &'static str> {
        let regs = &self.regs;
        let mac_mode =
            regs.read32(r::MAC_MODE) & (r::mac_mode::HALF_DUPLEX | r::mac_mode::PORTMODE);
        // Take the NVRAM arbitration (the reset drops it).
        regs.write32(r::NVRAM_SWARB, r::NVRAM_SWARB_SET1);
        let _ = self.wait(20, 8000, |g| {
            g.read32(r::NVRAM_SWARB) & r::NVRAM_SWARB_GNT1 != 0
        });
        regs.save_config();
        regs.cfg_write32(r::CFG_MISC_CTL, r::misc_ctl::RESET);
        regs.write32(r::FASTBOOT_PC, 0);
        // Ask the bootcode to answer once it has run.
        self.sram_write(r::SRAM_FW_MB, r::FW_MB_MAGIC);
        regs.write32(r::MISC_CFG, r::MISC_CFG_NO_LINK_TRAINING);
        regs.write32(r::MISC_CFG, r::MISC_CFG_RESET);
        regs.delay_us(100_000);
        // PCI Express: no relaxed ordering or snoop-free requests, 4096-byte
        // reads, errors cleared.
        let ctl = regs.cfg_read16(self.expcap + PCIE_DEVCTL);
        let ctl = (ctl & !(DEVCTL_RELAXED | DEVCTL_NOSNOOP | DEVCTL_MRRS)) | DEVCTL_MRRS_4096;
        regs.cfg_write16(self.expcap + PCIE_DEVCTL, ctl);
        regs.cfg_write16(self.expcap + PCIE_DEVSTA, 0x000f);
        regs.cfg_write32(r::CFG_MISC_CTL, r::misc_ctl::RESET);
        regs.cfg_write32(r::CFG_PCISTATE, r::PCISTATE_ROM);
        regs.restore_config();
        regs.write32(r::MARB_MODE, r::ENABLE);
        regs.write32(r::MODE_CTL, r::MODE_CTL_SWAP);
        let m = regs.read32(r::MAC_MODE) & !(r::mac_mode::HALF_DUPLEX | r::mac_mode::PORTMODE);
        regs.write32(r::MAC_MODE, m | mac_mode);
        regs.delay_us(40);
        // The bootcode's answer: the magic's complement.
        let answered = self.wait(10, TIMEOUT, |_| {
            self.sram_read(r::SRAM_FW_MB) == !r::FW_MB_MAGIC
        });
        if self.id.0 == r::CHIPID_57765_A0 {
            regs.delay_us(10_000);
        }
        if answered {
            Ok(())
        } else {
            Err("firmware handshake timed out")
        }
    }

    /// `bge_chipinit` (1732-1916).
    pub fn chip_init(&self, tagged: bool) {
        let regs = &self.regs;
        let misc = r::misc_ctl::INIT
            | if tagged {
                r::misc_ctl::TAGGED_STATUS
            } else {
                0
            };
        regs.cfg_write32(r::CFG_MISC_CTL, misc);
        let mut a = r::SRAM_CLEAR_START;
        while a < r::SRAM_CLEAR_END {
            self.window_write(a, 0);
            a += 4;
        }
        if self.id.chiprev() != r::CHIPREV_57765_AX {
            // The PCIe fast training sequence, against TX hangs (1767-1779).
            self.set(r::CPMU_PADRNG_CTL, r::CPMU_PADRNG_RDIV2);
        }
        let dma = if self.mps >= 256 {
            r::DMA_RW_CTL_MPS256
        } else {
            r::DMA_RW_CTL_MPS128
        };
        regs.cfg_write32(r::CFG_DMA_RW_CTL, dma);
        regs.write32(r::MODE_CTL, r::MODE_CTL_SWAP | r::MODE_CTL_INIT);
        regs.write32(r::MISC_CFG, r::MISC_CFG_66MHZ);
    }

    /// The hardware configuration word (`bge_attach` 3763-3782): the
    /// bootcode's copy in SRAM, else the EEPROM's.
    pub fn hw_config(&self) -> Result<HwConfig, HwError> {
        if self.sram_read(r::SRAM_DATA_SIG) == r::FW_MB_MAGIC {
            return Ok(HwConfig {
                word: self.sram_read(r::SRAM_DATA_CFG),
            });
        }
        match self.eeprom_read(r::ee::HWCFG, 4) {
            Some(b) => Ok(HwConfig {
                word: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            }),
            None if self.id.asicrev() == r::ASICREV_57766 => Ok(HwConfig { word: 0 }),
            None => Err(HwError::NoHwConfig),
        }
    }

    /// One EEPROM byte (`bge_eeprom_getbyte`).
    fn eeprom_byte(&self, addr: u32) -> Option<u8> {
        let regs = &self.regs;
        self.set(r::MISC_LOCAL_CTL, r::MLC_AUTO_EEPROM);
        regs.write32(r::EE_ADDR, r::ee::RESET);
        regs.delay_us(20);
        regs.write32(r::EE_ADDR, r::ee::READCMD | addr);
        // FreeBSD polls up to 1,000,000 times 10 µs; a tenth is ample.
        if !self.wait(10, TIMEOUT, |g| g.read32(r::EE_ADDR) & r::ee::DONE != 0) {
            return None;
        }
        let data = regs.read32(r::EE_DATA);
        Some((data >> ((addr % 4) * 8)) as u8)
    }

    fn eeprom_read(&self, addr: u32, n: usize) -> Option<[u8; 6]> {
        let mut out = [0u8; 6];
        for (i, b) in out.iter_mut().take(n).enumerate() {
            *b = self.eeprom_byte(addr + i as u32)?;
        }
        Some(out)
    }

    /// The station address (`bge_get_eaddr`): the bootcode's in SRAM
    /// ("HK"), else the EEPROM's. (FreeBSD's NVRAM path refuses every chip
    /// but the 5906; its loader hint comes later.)
    pub fn station_address(&self) -> Result<[u8; 6], HwError> {
        let hi = self.sram_read(r::SRAM_MAC_HIGH);
        if hi >> 16 == r::MAC_SIG {
            let lo = self.sram_read(r::SRAM_MAC_LOW).to_be_bytes();
            let h = (hi as u16).to_be_bytes();
            return Ok([h[0], h[1], lo[0], lo[1], lo[2], lo[3]]);
        }
        self.eeprom_read(r::ee::MAC, 6)
            .filter(|a| *a != [0; 6] && *a != [0xff; 6])
            .ok_or(HwError::NoAddress)
    }

    /// Reads PHY register `reg` (`bge_miibus_readreg`): 0 on a failed or
    /// timed-out read, as FreeBSD's.
    pub fn mii_read(&self, reg: u8) -> Result<u16, HwError> {
        let regs = &self.regs;
        regs.write32(
            r::MI_COMM,
            r::mi::READ | r::mi::BUSY | r::mi::phy(self.phy) | r::mi::reg(reg),
        );
        if !self.wait(10, TIMEOUT, |g| g.read32(r::MI_COMM) & r::mi::BUSY == 0) {
            return Err(HwError::MiiTimeout);
        }
        regs.delay_us(5);
        let v = regs.read32(r::MI_COMM);
        Ok(if v & r::mi::READFAIL != 0 {
            0
        } else {
            v as u16
        })
    }

    /// Writes PHY register `reg` (`bge_miibus_writereg`).
    pub fn mii_write(&self, reg: u8, v: u16) -> Result<(), HwError> {
        let regs = &self.regs;
        regs.write32(
            r::MI_COMM,
            r::mi::WRITE | r::mi::BUSY | r::mi::phy(self.phy) | r::mi::reg(reg) | u32::from(v),
        );
        if !self.wait(10, TIMEOUT, |g| g.read32(r::MI_COMM) & r::mi::BUSY == 0) {
            return Err(HwError::MiiTimeout);
        }
        regs.delay_us(5);
        let _ = regs.read32(r::MI_COMM);
        Ok(())
    }

    /// `bge_blockinit` (1919-2607): the rings, the status block, host
    /// coalescing and every block's mode, for a stopped, freshly reset chip.
    pub fn block_init(&self, rings: &Rings, lladdr: [u8; 6]) -> Result<(), HwError> {
        let regs = &self.regs;
        let hi = |a: u64| (a >> 32) as u32;
        let lo = |a: u64| a as u32;
        regs.cfg_write32(r::CFG_MEMWIN_BASE, 0);
        regs.write32(r::BMAN_READDMA_LOWAT, 0);
        regs.write32(r::BMAN_MACRX_LOWAT, 0x2a);
        regs.write32(r::BMAN_HIWAT, 0xa0);
        regs.write32(r::BMAN_DESC_LOWAT, 5);
        regs.write32(r::BMAN_DESC_HIWAT, 10);
        regs.write32(r::BMAN_MODE, r::BMAN_INIT);
        if !self.wait(10, TIMEOUT, |g| g.read32(r::BMAN_MODE) & r::ENABLE != 0) {
            return Err(HwError::BufferManager);
        }
        regs.write32(r::FTQ_RESET, 0xffff_ffff);
        regs.write32(r::FTQ_RESET, 0);
        if !self.wait(10, TIMEOUT, |g| g.read32(r::FTQ_RESET) == 0) {
            return Err(HwError::FtqReset);
        }
        // The standard producer ring, in registers.
        regs.write32(r::RX_STD_RCB, hi(rings.std_rx));
        regs.write32(r::RX_STD_RCB + 4, lo(rings.std_rx));
        regs.write32(
            r::RX_STD_RCB + 8,
            r::rcb_maxlen_flags(crate::ring::STD_RX as u32, r::MAX_FRAMELEN << 2),
        );
        regs.write32(r::RX_STD_RCB + 12, r::NIC_STD_RX);
        regs.write32(r::MBX_RX_STD_PROD, 0);
        // The jumbo ring, disabled.
        regs.write32(r::RX_JUMBO_RCB, hi(rings.jumbo_rx));
        regs.write32(r::RX_JUMBO_RCB + 4, lo(rings.jumbo_rx));
        regs.write32(r::RX_JUMBO_RCB + 8, r::RCB_EXT_RX_BD | r::RCB_DISABLED);
        regs.write32(r::RX_JUMBO_RCB + 12, r::NIC_JUMBO_RX);
        regs.write32(r::MBX_RX_JUMBO_PROD, 0);
        regs.write32(r::RBDI_STD_REPL_THRESH, 8);
        regs.write32(r::RBDI_JUMBO_REPL_THRESH, 256 / 8);
        regs.write32(r::STD_REPLENISH_LWM, 32);
        regs.write32(r::JMB_REPLENISH_LWM, 16);
        // Two send ring control blocks; ring 0 enabled.
        for i in 0..2u32 {
            let rcb = r::SRAM_SEND_RCB + 16 * i;
            self.window_write(rcb + 8, r::RCB_DISABLED);
            self.window_write(rcb + 12, 0);
        }
        self.window_write(r::SRAM_SEND_RCB, hi(rings.tx));
        self.window_write(r::SRAM_SEND_RCB + 4, lo(rings.tx));
        self.window_write(r::SRAM_SEND_RCB + 12, r::NIC_SEND_RING);
        self.window_write(
            r::SRAM_SEND_RCB + 8,
            r::rcb_maxlen_flags(crate::ring::TX as u32, 0),
        );
        // Four return ring control blocks; ring 0 enabled.
        for i in 0..4u32 {
            let rcb = r::SRAM_RETURN_RCB + 16 * i;
            self.window_write(rcb, 0);
            self.window_write(rcb + 4, 0);
            self.window_write(rcb + 8, r::RCB_DISABLED);
            self.window_write(rcb + 12, 0);
            regs.write32(r::MBX_RX_CONS0 + 8 * i as usize, 0);
        }
        self.window_write(r::SRAM_RETURN_RCB, hi(rings.rx_return));
        self.window_write(r::SRAM_RETURN_RCB + 4, lo(rings.rx_return));
        self.window_write(r::SRAM_RETURN_RCB + 12, 0);
        self.window_write(
            r::SRAM_RETURN_RCB + 8,
            r::rcb_maxlen_flags(crate::ring::RETURN as u32, 0),
        );
        let backoff = lladdr.iter().map(|&b| u32::from(b)).sum::<u32>() & 0x3ff;
        regs.write32(r::TX_RANDOM_BACKOFF, backoff);
        regs.write32(r::TX_LENGTHS, 0x2620);
        regs.write32(r::RX_RULES_CFG, 0x08);
        regs.write32(r::RXLP_CFG, 0x181);
        regs.write32(r::RXLP_STATS_ENABLE_MASK, 0x007f_ffff);
        regs.write32(r::RXLP_STATS_CTL, 0x1);
        regs.write32(r::HCC_MODE, 0);
        if !self.wait(10, TIMEOUT, |g| g.read32(r::HCC_MODE) & r::ENABLE == 0) {
            return Err(HwError::HostCoalescing);
        }
        regs.write32(r::HCC_RX_COAL_TICKS, 150);
        regs.write32(r::HCC_TX_COAL_TICKS, 150);
        regs.write32(r::HCC_RX_MAX_COAL_BDS, 10);
        regs.write32(r::HCC_TX_MAX_COAL_BDS, 10);
        regs.write32(r::HCC_RX_MAX_COAL_BDS_INT, 1);
        regs.write32(r::HCC_TX_MAX_COAL_BDS_INT, 1);
        regs.write32(r::HCC_STATUSBLK_ADDR, hi(rings.status));
        regs.write32(r::HCC_STATUSBLK_ADDR + 4, lo(rings.status));
        regs.write32(r::HCC_MODE, r::HCC_STATBLK_32 | r::ENABLE);
        regs.write32(r::RBDC_MODE, 0x6);
        regs.write32(r::RXLP_MODE, r::ENABLE);
        regs.write32(r::MAC_MODE, r::mac_mode::INIT);
        regs.delay_us(40);
        self.set(r::MISC_LOCAL_CTL, r::MLC_INTR_ONATTN);
        regs.write32(r::WDMA_MODE, r::WDMA_INIT);
        regs.delay_us(40);
        self.set(r::RDMA_RSRVCTRL, r::RDMA_FIFO_OFLW_FIX);
        regs.write32(r::RDMA_MODE, r::RDMA_INIT);
        regs.delay_us(40);
        for m in [
            r::RDC_MODE,
            r::RBDI_MODE,
            r::RDBDI_MODE,
            r::SBDC_MODE,
            r::SDC_MODE,
        ] {
            regs.write32(m, r::ENABLE);
        }
        // Without TSO, no `HW_LSO_PRE_DMA`.
        regs.write32(r::SDI_MODE, r::ENABLE);
        regs.write32(r::SBDI_MODE, r::ENABLE);
        regs.write32(r::SRS_MODE, r::ENABLE);
        regs.write32(r::SDI_STATS_ENABLE_MASK, 0x007f_ffff);
        regs.write32(r::SDI_STATS_CTL, 0x3);
        regs.write32(r::MAC_STS, r::MAC_STS_ACK);
        regs.write32(r::MI_STS, 0);
        regs.write32(r::MAC_STS, r::MAC_STS_ACK);
        self.set(r::MAC_EVT_ENB, r::EVT_LINK_CHANGED);
        Ok(())
    }

    /// The MAC's own settings from `bge_init_locked` (5447-5561), after
    /// the rings are filled: MTU, station address, transmitter, receiver,
    /// statistics cleared, stack-up mode.
    pub fn start_mac(&self, lladdr: [u8; 6], mtu: u32) {
        let regs = &self.regs;
        // The MTU with Ethernet header, CRC and a VLAN tag.
        regs.write32(r::RX_MTU, mtu + 14 + 4 + 4);
        let a = lladdr.map(u32::from);
        regs.write32(r::MAC_ADDR1_LO, a[0] << 8 | a[1]);
        regs.write32(r::MAC_ADDR1_HI, a[2] << 24 | a[3] << 16 | a[4] << 8 | a[5]);
        // Tags stay in the frame: the driver does not take them in the BD.
        self.set(r::RX_MODE, r::txrx::RX_KEEP_VLAN_DIAG);
        self.set(r::TX_MODE, r::txrx::TX_MBUF_LOCKUP_FIX | r::txrx::ENABLE);
        regs.delay_us(100);
        self.set(r::RX_MODE, r::txrx::RX_IPV6_ENABLE | r::txrx::ENABLE);
        regs.delay_us(10);
        regs.write32(r::MAX_RX_FRAME_LOWAT, 1);
        let mut s = r::stats::MAC_FIRST;
        while s <= r::stats::MAC_LAST {
            let _ = regs.read32(s);
            s += 4;
        }
        self.set(r::MODE_CTL, r::MODE_CTL_STACKUP);
        // One-shot MSI (attach's step, 3900-3904), again after each reset.
        self.clear(r::MSI_MODE, r::MSI_ONE_SHOT_DISABLE);
    }

    /// The receive filter (`bge_setpromisc`, `bge_setmulti`): promiscuous,
    /// or the multicast hash (all ones for all-multicast).
    pub fn set_filter(&self, promisc: bool, hash: [u32; 4]) {
        if promisc {
            self.set(r::RX_MODE, r::txrx::RX_PROMISC);
        } else {
            self.clear(r::RX_MODE, r::txrx::RX_PROMISC);
        }
        for (i, h) in hash.iter().enumerate() {
            self.regs.write32(r::MAR0 + 4 * i, *h);
        }
    }

    /// Lets the chip interrupt (`bge_init_locked` 5573-5577).
    pub fn enable_interrupts(&self) {
        let v = self.regs.read32(r::CFG_MISC_CTL as usize);
        self.regs.write32(
            r::CFG_MISC_CTL as usize,
            (v | r::misc_ctl::CLEAR_INTA) & !r::misc_ctl::MASK_PCI_INTR,
        );
        self.regs.write32(r::MBX_IRQ0, 0);
    }

    /// Masks the chip's interrupt (`bge_stop` 5975-5976).
    pub fn disable_interrupts(&self) {
        self.set(r::CFG_MISC_CTL as usize, r::misc_ctl::MASK_PCI_INTR);
        self.regs.write32(r::MBX_IRQ0, 1);
    }

    /// Acknowledges the status block whose tag was `tag` and re-enables
    /// the interrupt (`bge_intr_task` 4633).
    pub fn ack(&self, tag: u8) {
        self.regs.write32(r::MBX_IRQ0, u32::from(tag) << 24);
    }

    /// Forces a status block update and an interrupt (`COAL_NOW`).
    pub fn kick(&self) {
        self.set(r::HCC_MODE, r::HCC_COAL_NOW);
    }

    /// The MAC for a link that came up (`bge_miibus_statchg`).
    pub fn set_link(&self, l: LinkMode) {
        let regs = &self.regs;
        let mut mac =
            regs.read32(r::MAC_MODE) & !(r::mac_mode::PORTMODE | r::mac_mode::HALF_DUPLEX);
        mac |= if l.gigabit {
            r::mac_mode::PORTMODE_GMII
        } else {
            r::mac_mode::PORTMODE_MII
        };
        let mut tx = regs.read32(r::TX_MODE) & !r::txrx::TX_FLOWCTL;
        let mut rx = regs.read32(r::RX_MODE) & !r::txrx::RX_FLOWCTL;
        if l.full_duplex {
            if l.tx_pause {
                tx |= r::txrx::TX_FLOWCTL;
            }
            if l.rx_pause {
                rx |= r::txrx::RX_FLOWCTL;
            }
        } else {
            mac |= r::mac_mode::HALF_DUPLEX;
        }
        regs.write32(r::MAC_MODE, mac);
        regs.delay_us(40);
        regs.write32(r::TX_MODE, tx);
        regs.write32(r::RX_MODE, rx);
    }

    /// Acknowledges the MAC's link-change attention.
    pub fn ack_link(&self) {
        self.regs.write32(r::MAC_STS, r::MAC_STS_ACK);
    }

    /// Clears a block's enable bit and waits for it to stop
    /// (`bge_stop_block`): FreeBSD waits up to 10 s per block; a tenth.
    fn stop_block(&self, reg: usize) {
        self.clear(reg, r::ENABLE);
        let _ = self.wait(100, TIMEOUT / 10, |g| g.read32(reg) & r::ENABLE == 0);
    }

    /// `bge_stop` (5964-6058): interrupts off, every block stopped, the
    /// queues reset, then the chip.
    pub fn stop(&self) {
        self.disable_interrupts();
        for b in [
            r::RX_MODE,
            r::RBDI_MODE,
            r::RXLP_MODE,
            r::RDBDI_MODE,
            r::RDC_MODE,
            r::RBDC_MODE,
            r::SRS_MODE,
            r::SBDI_MODE,
            r::SDI_MODE,
            r::RDMA_MODE,
            r::SDC_MODE,
            r::SBDC_MODE,
            r::HCC_MODE,
            r::WDMA_MODE,
        ] {
            self.stop_block(b);
        }
        self.regs.write32(r::FTQ_RESET, 0xffff_ffff);
        self.regs.write32(r::FTQ_RESET, 0);
        let _ = self.reset();
        self.clear(r::MODE_CTL, r::MODE_CTL_STACKUP);
    }
}

/// The multicast hash of `addrs` (`bge_setmulti`): bit `h & 0x1f` of
/// register `(h & 0x60) >> 5`, `h` the low 7 bits of the little-endian CRC.
#[must_use]
pub fn multicast_hash(addrs: &[[u8; 6]]) -> [u32; 4] {
    let mut h = [0u32; 4];
    for a in addrs {
        let crc = ether_crc32_le(a) & 0x7f;
        h[((crc & 0x60) >> 5) as usize] |= 1 << (crc & 0x1f);
    }
    h
}

/// `ether_crc32_le`.
pub(crate) fn ether_crc32_le(buf: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in buf {
        let mut x = u32::from(b);
        for _ in 0..8 {
            let carry = (crc ^ x) & 1;
            crc >>= 1;
            x >>= 1;
            if carry != 0 {
                crc ^= 0xedb8_8320;
            }
        }
    }
    crc
}
