// SPDX-License-Identifier: BSD-2-Clause
//! The registers, SRAM addresses and bits the BCM57765 family's path uses
//! (`if_bgereg.h`). Offsets are into BAR0 unless they say configuration
//! space (`CFG_*`) or NIC SRAM (`SRAM_*`).

// Configuration space (BAR0 0x00-0xff mirrors it).

/// `BGE_PCI_MISC_CTL`: the legacy chip ID in bits 31:16.
pub const CFG_MISC_CTL: u16 = 0x68;
/// `BGE_PCI_DMA_RW_CTL`.
pub const CFG_DMA_RW_CTL: u16 = 0x6c;
/// `BGE_PCI_PCISTATE`.
pub const CFG_PCISTATE: u16 = 0x70;
/// `BGE_PCI_MEMWIN_BASEADDR`: the SRAM window's base.
pub const CFG_MEMWIN_BASE: u16 = 0x7c;
/// `BGE_PCI_MEMWIN_DATA`: an indirect SRAM word.
pub const CFG_MEMWIN_DATA: u16 = 0x84;
/// `BGE_PCI_GEN15_PRODID_ASICREV`: the 57765 family's chip ID.
pub const CFG_GEN15_PRODID_ASICREV: u16 = 0xfc;

/// `BGE_PCI_MISC_CTL` bits.
pub mod misc_ctl {
    pub const CLEAR_INTA: u32 = 0x1;
    pub const MASK_PCI_INTR: u32 = 0x2;
    pub const ENDIAN_WORDSWAP: u32 = 0x8;
    pub const PCISTATE_RW: u32 = 0x10;
    pub const INDIRECT_ACCESS: u32 = 0x80;
    pub const TAGGED_STATUS: u32 = 0x200;
    /// Where the legacy chip ID starts.
    pub const ASICREV_SHIFT: u32 = 16;
    /// `bge_reset`'s value: indirect access, interrupts masked, word swap,
    /// state writable (0x9a).
    pub const RESET: u32 = INDIRECT_ACCESS | MASK_PCI_INTR | ENDIAN_WORDSWAP | PCISTATE_RW;
    /// `BGE_INIT` (0x8b).
    pub const INIT: u32 = ENDIAN_WORDSWAP | CLEAR_INTA | MASK_PCI_INTR | INDIRECT_ACCESS;
}

/// `BGE_PCI_PCISTATE` after reset: `ROM_ENABLE | ROM_RETRY_ENABLE`.
pub const PCISTATE_ROM: u32 = 0x60;

/// `BGE_PCI_DMA_RW_CTL` for PCIe: read command 6, write command 7, and the
/// write watermark by the maximum payload (`bge_chipinit`).
pub const DMA_RW_CTL_MPS256: u32 = 0x7638_0000;
pub const DMA_RW_CTL_MPS128: u32 = 0x7618_0000;

/// The ASIC revision of the 57765 (`BGE_ASICREV_BCM57765`).
pub const ASICREV_57765: u32 = 0x57785;
/// The 57766's (`BGE_ASICREV_BCM57766`).
pub const ASICREV_57766: u32 = 0x57766;
/// The 57765's A-stepping chip revision (`BGE_CHIPREV_57765_AX`).
pub const CHIPREV_57765_AX: u32 = 0x57_7850;
/// The 57765 A0 (`BGE_CHIPID_BCM57765_A0`).
pub const CHIPID_57765_A0: u32 = 0x5778_5000;
/// `BGE_ASICREV_USE_PRODID_REG`: the legacy ID says to read a product ID
/// register.
pub const ASICREV_USE_PRODID_REG: u32 = 0x0f;

// NIC SRAM.

/// `BGE_SRAM_FW_MB`: the firmware handshake mailbox.
pub const SRAM_FW_MB: u32 = 0xb50;
/// `BGE_SRAM_FW_MB_MAGIC` ("KevT"); the bootcode answers with its
/// complement.
pub const FW_MB_MAGIC: u32 = 0x4b65_7654;
/// `BGE_SRAM_DATA_SIG`: "KevT" when the bootcode left its configuration.
pub const SRAM_DATA_SIG: u32 = 0xb54;
/// `BGE_SRAM_DATA_CFG`: the hardware configuration word.
pub const SRAM_DATA_CFG: u32 = 0xb58;
/// `BGE_SRAM_MAC_ADDR_HIGH_MB`: "HK" and the address's first two bytes.
pub const SRAM_MAC_HIGH: u32 = 0xc14;
/// `BGE_SRAM_MAC_ADDR_LOW_MB`: its last four.
pub const SRAM_MAC_LOW: u32 = 0xc18;
/// The "HK" signature in `SRAM_MAC_HIGH`'s upper half.
pub const MAC_SIG: u32 = 0x484b;
/// The statistics and NIC-side status blocks `bge_chipinit` zeroes:
/// 0x300 up to 0xb50.
pub const SRAM_CLEAR_START: u32 = 0x300;
pub const SRAM_CLEAR_END: u32 = 0xb50;
/// The send and return rings' control blocks, through the window.
pub const SRAM_SEND_RCB: u32 = 0x100;
pub const SRAM_RETURN_RCB: u32 = 0x200;
/// Where the chip keeps the standard and jumbo producer rings, and the
/// first send ring (`BGE_STD_RX_RINGS`, `BGE_JUMBO_RX_RINGS`,
/// `BGE_SEND_RING_1_TO_4`).
pub const NIC_STD_RX: u32 = 0x6000;
pub const NIC_JUMBO_RX: u32 = 0x7000;
pub const NIC_SEND_RING: u32 = 0x4000;
/// The SRAM window in BAR0 (`BGE_MEMWIN_START`).
pub const MEMWIN: usize = 0x8000;

/// The hardware configuration word's bits (`BGE_HWCFG_*`).
pub mod hwcfg {
    pub const ASF: u32 = 0x80;
    pub const MEDIA: u32 = 0x30;
    pub const MEDIA_FIBER: u32 = 0x20;
}

// Mailboxes (only the low halves are written).

pub const MBX_IRQ0: usize = 0x204;
pub const MBX_RX_STD_PROD: usize = 0x26c;
pub const MBX_RX_JUMBO_PROD: usize = 0x274;
/// Return ring `i`'s consumer: `MBX_RX_CONS0 + 8 * i`.
pub const MBX_RX_CONS0: usize = 0x284;
pub const MBX_TX_HOST_PROD0: usize = 0x304;
pub const MBX_TX_NIC_PROD0: usize = 0x384;

// The MAC.

pub const MAC_MODE: usize = 0x400;
pub const MAC_STS: usize = 0x404;
pub const MAC_EVT_ENB: usize = 0x408;
pub const MAC_ADDR1_LO: usize = 0x410;
pub const MAC_ADDR1_HI: usize = 0x414;
pub const TX_RANDOM_BACKOFF: usize = 0x438;
pub const RX_MTU: usize = 0x43c;
pub const MI_COMM: usize = 0x44c;
pub const MI_STS: usize = 0x450;
pub const TX_MODE: usize = 0x45c;
pub const TX_LENGTHS: usize = 0x464;
pub const RX_MODE: usize = 0x468;
pub const MAR0: usize = 0x470;
pub const RX_RULES_CFG: usize = 0x500;
pub const MAX_RX_FRAME_LOWAT: usize = 0x504;

/// `BGE_MAC_MODE` bits.
pub mod mac_mode {
    pub const HALF_DUPLEX: u32 = 0x2;
    pub const PORTMODE: u32 = 0xc;
    pub const PORTMODE_MII: u32 = 0x4;
    pub const PORTMODE_GMII: u32 = 0x8;
    /// `bge_blockinit`'s: DMA, statistics, frame headers, MII port (0x00e0d804).
    pub const INIT: u32 = 0x00e0_d804;
}

/// `BGE_MAC_STS` bits, write-one-to-clear: sync, configuration, MI
/// complete, link changed.
pub const MAC_STS_ACK: u32 = 0x0040_1018;
/// `BGE_EVTENB_LINK_CHANGED`.
pub const EVT_LINK_CHANGED: u32 = 0x1000;

/// `BGE_MI_COMM` fields.
pub mod mi {
    pub const WRITE: u32 = 0x0400_0000;
    pub const READ: u32 = 0x0800_0000;
    pub const READFAIL: u32 = 0x1000_0000;
    pub const BUSY: u32 = 0x2000_0000;
    pub const fn reg(r: u8) -> u32 {
        ((r as u32) & 0x1f) << 16
    }
    pub const fn phy(p: u8) -> u32 {
        ((p as u32) & 0x1f) << 21
    }
}

/// `BGE_TX_MODE` and `BGE_RX_MODE` bits.
pub mod txrx {
    pub const ENABLE: u32 = 0x2;
    pub const TX_FLOWCTL: u32 = 0x10;
    pub const TX_MBUF_LOCKUP_FIX: u32 = 0x100;
    pub const RX_FLOWCTL: u32 = 0x4;
    pub const RX_PROMISC: u32 = 0x100;
    pub const RX_KEEP_VLAN_DIAG: u32 = 0x400;
    pub const RX_IPV6_ENABLE: u32 = 0x0100_0000;
}

// The blocks' mode registers, each enabled by bit 1 (`*_ENABLE` = 0x2).

pub const ENABLE: u32 = 0x2;
pub const SDI_MODE: usize = 0x0c00;
pub const SDI_STATS_CTL: usize = 0x0c08;
pub const SDI_STATS_ENABLE_MASK: usize = 0x0c0c;
pub const SDC_MODE: usize = 0x1000;
pub const SRS_MODE: usize = 0x1400;
pub const SBDI_MODE: usize = 0x1800;
pub const SBDC_MODE: usize = 0x1c00;
pub const RXLP_MODE: usize = 0x2000;
pub const RXLP_CFG: usize = 0x2010;
pub const RXLP_STATS_CTL: usize = 0x2014;
pub const RXLP_STATS_ENABLE_MASK: usize = 0x2018;
pub const RDBDI_MODE: usize = 0x2400;
pub const RX_JUMBO_RCB: usize = 0x2440;
pub const RX_STD_RCB: usize = 0x2450;
pub const RDC_MODE: usize = 0x2800;
pub const RBDI_MODE: usize = 0x2c00;
pub const RBDI_STD_REPL_THRESH: usize = 0x2c18;
pub const RBDI_JUMBO_REPL_THRESH: usize = 0x2c1c;
pub const STD_REPLENISH_LWM: usize = 0x2d00;
pub const JMB_REPLENISH_LWM: usize = 0x2d04;
pub const RBDC_MODE: usize = 0x3000;
pub const CPMU_PADRNG_CTL: usize = 0x3668;
pub const CPMU_PADRNG_RDIV2: u32 = 0x0004_0000;
pub const HCC_MODE: usize = 0x3c00;
pub const HCC_RX_COAL_TICKS: usize = 0x3c08;
pub const HCC_TX_COAL_TICKS: usize = 0x3c0c;
pub const HCC_RX_MAX_COAL_BDS: usize = 0x3c10;
pub const HCC_TX_MAX_COAL_BDS: usize = 0x3c14;
pub const HCC_RX_MAX_COAL_BDS_INT: usize = 0x3c20;
pub const HCC_TX_MAX_COAL_BDS_INT: usize = 0x3c24;
pub const HCC_STATUSBLK_ADDR: usize = 0x3c38;
/// `BGE_HCCMODE_COAL_NOW`: update the status block and interrupt now.
pub const HCC_COAL_NOW: u32 = 0x8;
/// `BGE_STATBLKSZ_32BYTE`.
pub const HCC_STATBLK_32: u32 = 0x100;
pub const MARB_MODE: usize = 0x4000;
pub const BMAN_MODE: usize = 0x4400;
pub const BMAN_READDMA_LOWAT: usize = 0x4410;
pub const BMAN_MACRX_LOWAT: usize = 0x4414;
pub const BMAN_HIWAT: usize = 0x4418;
pub const BMAN_DESC_LOWAT: usize = 0x4434;
pub const BMAN_DESC_HIWAT: usize = 0x4438;
/// `BGE_BMANMODE_ENABLE | BGE_BMANMODE_LOMBUF_ATTN`.
pub const BMAN_INIT: u32 = 0x12;
pub const RDMA_MODE: usize = 0x4800;
/// `ENABLE | ALL_ATTNS | FIFO_LONG_BURST`. TSO4/TSO6 (0x18000000) are
/// left out until the driver offers TSO.
pub const RDMA_INIT: u32 = 0x0003_03fe;
pub const RDMA_RSRVCTRL: usize = 0x4900;
pub const RDMA_FIFO_OFLW_FIX: u32 = 0x4;
pub const WDMA_MODE: usize = 0x4c00;
/// `ENABLE | ALL_ATTNS | STATUS_TAG_FIX`.
pub const WDMA_INIT: u32 = 0x2000_03fe;
pub const FTQ_RESET: usize = 0x5c00;
pub const MSI_MODE: usize = 0x6000;
/// `BGE_MSIMODE_ONE_SHOT_DISABLE`.
pub const MSI_ONE_SHOT_DISABLE: u32 = 0x20;
pub const MODE_CTL: usize = 0x6800;
/// `bge_dma_swap_options` on a little-endian host.
pub const MODE_CTL_SWAP: u32 = 0x34;
/// `BGE_MODECTL_MAC_ATTN_INTR | HOST_SEND_BDS | TX_NO_PHDR_CSUM`.
pub const MODE_CTL_INIT: u32 = 0x0412_0000;
pub const MODE_CTL_STACKUP: u32 = 0x1_0000;
pub const MISC_CFG: usize = 0x6804;
/// `BGE_32BITTIME_66MHZ`: the timer prescaler.
pub const MISC_CFG_66MHZ: u32 = 0x82;
/// `BGE_MISCCFG_RESET_CORE_CLOCKS`, and bit 29: no PCIe link training
/// during the reset.
pub const MISC_CFG_RESET: u32 = 0x2000_0083;
pub const MISC_CFG_NO_LINK_TRAINING: u32 = 0x2000_0000;
pub const MISC_LOCAL_CTL: usize = 0x6808;
/// `BGE_MLC_INTR_ONATTN`.
pub const MLC_INTR_ONATTN: u32 = 0x8;
/// `BGE_MLC_AUTO_EEPROM`.
pub const MLC_AUTO_EEPROM: u32 = 0x0100_0000;
pub const EE_ADDR: usize = 0x6838;
pub const EE_DATA: usize = 0x683c;
pub const FASTBOOT_PC: usize = 0x6894;
pub const NVRAM_SWARB: usize = 0x7020;
pub const NVRAM_SWARB_SET1: u32 = 0x2;
pub const NVRAM_SWARB_GNT1: u32 = 0x200;

/// `BGE_EE_*` (the legacy EEPROM auto-access interface).
pub mod ee {
    /// `BGE_EEADDR_RESET | BGE_EEHALFCLK(0x60)`.
    pub const RESET: u32 = 0x2060_0000;
    /// `BGE_EE_READCMD`: half clock 0x60, start, read, done.
    pub const READCMD: u32 = 0xc260_0000;
    pub const DONE: u32 = 0x4000_0000;
    /// Where the MAC address is (`BGE_EE_MAC_OFFSET` + 2).
    pub const MAC: u32 = 0x7e;
    /// Where the hardware configuration word is (`BGE_EE_HWCFG_OFFSET`).
    pub const HWCFG: u32 = 0xc8;
}

/// The statistics registers `bge_stats_update_regs` reads (read to clear).
pub mod stats {
    pub const TX_OCTETS: usize = 0x800;
    pub const TX_COLLS: usize = 0x808;
    pub const TX_ERRORS: usize = 0x818;
    pub const TX_UCAST: usize = 0x86c;
    pub const TX_MCAST: usize = 0x870;
    pub const TX_BCAST: usize = 0x874;
    pub const RX_OCTETS: usize = 0x880;
    pub const RX_UCAST: usize = 0x88c;
    pub const RX_MCAST: usize = 0x890;
    pub const RX_BCAST: usize = 0x894;
    pub const RX_FCS: usize = 0x898;
    pub const RX_ALIGN: usize = 0x89c;
    pub const RX_TOO_LONG: usize = 0x8b0;
    pub const RX_UNDERSIZE: usize = 0x8b8;
    pub const RXLP_OUT_OF_BDS: usize = 0x224c;
    pub const RXLP_IFIN_DROPS: usize = 0x2250;
    pub const RXLP_IFIN_ERRORS: usize = 0x2254;
    /// Every MAC statistics register, read once to clear
    /// (`bge_stats_clear_regs`): 0x800 to 0x8bc.
    pub const MAC_FIRST: usize = 0x800;
    pub const MAC_LAST: usize = 0x8bc;
}

/// A ring control block's `maxlen_flags`: entries (or the longest frame)
/// in bits 31:16, flags below (`BGE_RCB_MAXLEN_FLAGS`).
#[must_use]
pub const fn rcb_maxlen_flags(maxlen: u32, flags: u32) -> u32 {
    maxlen << 16 | flags
}
/// `BGE_RCB_FLAG_RING_DISABLED`.
pub const RCB_DISABLED: u32 = 0x2;
/// `BGE_RCB_FLAG_USE_EXT_RX_BD`.
pub const RCB_EXT_RX_BD: u32 = 0x1;

/// The longest frame (`BGE_MAX_FRAMELEN`).
pub const MAX_FRAMELEN: u32 = 1536;
