// SPDX-License-Identifier: BSD-2-Clause
//! The RTL8125/8126 register map: the offsets and bits the driver uses,
//! as FreeBSD's `if_rgereg.h` names them.

#![allow(dead_code, reason = "the register map is listed whole")]

pub const MAC0: usize = 0x0000;
pub const MAC4: usize = 0x0004;
pub const MAR0: usize = 0x0008;
pub const MAR4: usize = 0x000c;
pub const TXDESC_ADDR_LO: usize = 0x0020;
pub const TXDESC_ADDR_HI: usize = 0x0024;
pub const INT_CFG0: usize = 0x0034;
pub const CMD: usize = 0x0037;
pub const IMR: usize = 0x0038;
pub const ISR: usize = 0x003c;
pub const TXCFG: usize = 0x0040;
pub const RXCFG: usize = 0x0044;
pub const TIMERCNT: usize = 0x0048;
pub const EECMD: usize = 0x0050;
pub const CFG1: usize = 0x0052;
pub const CFG2: usize = 0x0053;
pub const CFG3: usize = 0x0054;
pub const CFG5: usize = 0x0056;
pub const TDFNR: usize = 0x0057;
pub const TIMERINT0: usize = 0x0058;
pub const TIMERINT1: usize = 0x005c;
pub const CSIDR: usize = 0x0064;
pub const CSIAR: usize = 0x0068;
pub const PHYSTAT: usize = 0x006c;
pub const PMCH: usize = 0x006f;
pub const INT_CFG1: usize = 0x007a;
pub const EPHYAR: usize = 0x0080;
pub const TIMERINT2: usize = 0x008c;
pub const TXSTART: usize = 0x0090;
pub const MACOCP: usize = 0x00b0;
pub const PHYOCP: usize = 0x00b8;
pub const DLLPR: usize = 0x00d0;
pub const TWICMD: usize = 0x00d2;
pub const MCUCMD: usize = 0x00d3;
pub const RXMAXSIZE: usize = 0x00da;
pub const CPLUSCMD: usize = 0x00e0;
pub const IM: usize = 0x00e2;
pub const RXDESC_ADDR_LO: usize = 0x00e4;
pub const RXDESC_ADDR_HI: usize = 0x00e8;
pub const PPSW: usize = 0x00f2;
pub const TIMERINT3: usize = 0x00f4;
/// The interrupt moderation timers, 4 bytes each.
pub const INTMITI: usize = 0x0a00;
pub const PHYBASE: u16 = 0x0a40;
pub const ADDR0: usize = 0x19e0;
pub const ADDR1: usize = 0x19e4;
pub const RSS_CTRL: usize = 0x4500;
pub const RXQUEUE_CTRL: usize = 0x4800;
pub const EEE_TXIDLE_TIMER: usize = 0x6048;

pub const INT_CFG0_EN: u8 = 0x01;
pub const INT_CFG0_TIMEOUT_BYPASS: u8 = 0x02;
pub const INT_CFG0_MITIGATION_BYPASS: u8 = 0x04;
pub const INT_CFG0_RDU_BYPASS_8126: u8 = 0x10;

pub const CMD_TXENB: u8 = 0x04;
pub const CMD_RXENB: u8 = 0x08;
pub const CMD_RESET: u8 = 0x10;
pub const CMD_STOPREQ: u8 = 0x80;

pub const ISR_RX_OK: u32 = 0x0000_0001;
pub const ISR_RX_ERR: u32 = 0x0000_0002;
pub const ISR_TX_OK: u32 = 0x0000_0004;
pub const ISR_TX_ERR: u32 = 0x0000_0008;
pub const ISR_RX_DESC_UNAVAIL: u32 = 0x0000_0010;
pub const ISR_LINKCHG: u32 = 0x0000_0020;
pub const ISR_RX_FIFO_OFLOW: u32 = 0x0000_0040;
pub const ISR_TX_DESC_UNAVAIL: u32 = 0x0000_0080;
pub const ISR_SWI: u32 = 0x0000_0100;
pub const ISR_PCS_TIMEOUT: u32 = 0x0000_4000;
pub const ISR_SYSTEM_ERR: u32 = 0x0000_8000;
/// The interrupts the driver takes without moderation (`RGE_INTRS`).
pub const INTRS: u32 = ISR_RX_OK
    | ISR_RX_ERR
    | ISR_TX_OK
    | ISR_TX_ERR
    | ISR_LINKCHG
    | ISR_TX_DESC_UNAVAIL
    | ISR_PCS_TIMEOUT
    | ISR_SYSTEM_ERR;
/// The interrupts the driver takes with simulated moderation: completions
/// come from the timer (`ISR_PCS_TIMEOUT`).
pub const INTRS_TIMER: u32 = ISR_RX_ERR | ISR_TX_ERR | ISR_PCS_TIMEOUT | ISR_SYSTEM_ERR;

pub const TXCFG_HWREV: u32 = 0x7cf0_0000;
pub const TXCFG_CONFIG: u32 = 0x0300_0700;

pub const RXCFG_ALLPHYS: u32 = 0x01;
pub const RXCFG_INDIV: u32 = 0x02;
pub const RXCFG_MULTI: u32 = 0x04;
pub const RXCFG_BROAD: u32 = 0x08;
pub const RXCFG_RUNT: u32 = 0x10;
pub const RXCFG_ERRPKT: u32 = 0x20;
pub const RXCFG_VLANSTRIP: u32 = 0x00c0_0000;
pub const RXCFG_CONFIG_8126: u32 = 0x4120_0d00;
/// Every receive filter bit.
pub const RXCFG_FILTERS: u32 =
    RXCFG_ALLPHYS | RXCFG_INDIV | RXCFG_MULTI | RXCFG_BROAD | RXCFG_RUNT | RXCFG_ERRPKT;

pub const EECMD_WRITECFG: u8 = 0xc0;
pub const CFG1_SPEED_DOWN: u8 = 0x10;
pub const CFG5_PME_STS: u8 = 0x01;

pub const CSIAR_BYTE_EN: u32 = 0x0000_000f;
pub const CSIAR_BYTE_EN_SHIFT: u32 = 12;
pub const CSIAR_ADDR_MASK: u32 = 0x0000_0fff;
pub const CSIAR_BUSY: u32 = 0x8000_0000;

pub const PHYSTAT_FDX: u16 = 0x0001;
pub const PHYSTAT_LINK: u16 = 0x0002;
pub const PHYSTAT_10MBPS: u16 = 0x0004;
pub const PHYSTAT_100MBPS: u16 = 0x0008;
pub const PHYSTAT_1000MBPS: u16 = 0x0010;
pub const PHYSTAT_2500MBPS: u16 = 0x0400;
pub const PHYSTAT_5000MBPS: u16 = 0x1000;

pub const EPHYAR_DATA_MASK: u32 = 0x0000_ffff;
pub const EPHYAR_BUSY: u32 = 0x8000_0000;
pub const EPHYAR_ADDR_MASK: u32 = 0x0000_007f;
pub const EPHYAR_ADDR_SHIFT: u32 = 16;

pub const TXSTART_START: u16 = 0x0001;

pub const OCP_DATA_MASK: u32 = 0x0000_ffff;
pub const OCP_BUSY: u32 = 0x8000_0000;
pub const OCP_ADDR_SHIFT: u32 = 16;

pub const DLLPR_PFM_EN: u8 = 0x40;
pub const DLLPR_TX_10M_PS_EN: u8 = 0x80;

pub const MCUCMD_RXFIFO_EMPTY: u8 = 0x10;
pub const MCUCMD_TXFIFO_EMPTY: u8 = 0x20;
pub const MCUCMD_IS_OOB: u8 = 0x80;

pub const CPLUSCMD_RXCSUM: u16 = 0x0020;

/// The largest frame the controller takes.
pub const JUMBO_FRAMELEN: u16 = 9216;
/// The MTU of a jumbo frame: less the Ethernet header, CRC, and a VLAN
/// tag.
pub const JUMBO_MTU: u16 = JUMBO_FRAMELEN - 14 - 4 - 4;

/// Bounded polls: reset completion and PHY states (FreeBSD's
/// `RGE_TIMEOUT`).
pub const TIMEOUT: u32 = 100;

/// The 5 Gb/s advertisement bit of PHY register 0xa5d4.
pub const ADV_5000TFDX: u16 = 0x0100;
/// The 2.5 Gb/s advertisement bit.
pub const ADV_2500TFDX: u16 = 0x0080;

/// MII registers (IEEE 802.3 clause 22), through the PHY's OCP window.
pub mod mii {
    pub const BMCR: u16 = 0x00;
    pub const BMSR: u16 = 0x01;
    pub const ANAR: u16 = 0x04;
    pub const GTCR: u16 = 0x09;

    pub const BMCR_RESET: u16 = 0x8000;
    pub const BMCR_AUTOEN: u16 = 0x1000;
    pub const BMCR_PDOWN: u16 = 0x0800;
    pub const BMCR_STARTNEG: u16 = 0x0200;

    pub const ANAR_10: u16 = 0x0020;
    pub const ANAR_10_FD: u16 = 0x0040;
    pub const ANAR_TX: u16 = 0x0080;
    pub const ANAR_TX_FD: u16 = 0x0100;
    pub const ANAR_PAUSE_SYM: u16 = 0x0400;
    pub const ANAR_PAUSE_ASYM: u16 = 0x0800;

    pub const GTCR_ADV_1000THDX: u16 = 0x0100;
    pub const GTCR_ADV_1000TFDX: u16 = 0x0200;
}
