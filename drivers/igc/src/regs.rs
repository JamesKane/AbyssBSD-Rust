// SPDX-License-Identifier: BSD-2-Clause
//! The I225 and I226's registers and the bits of them the driver uses.
//! The offsets and bit values are the controller's (facts of the
//! hardware), as FreeBSD's `sys/dev/igc/igc_regs.h` and `igc_defines.h`
//! give them; only what this driver touches is here.

// Device control and status.
pub const CTRL: usize = 0x0000;
pub const STATUS: usize = 0x0008;
pub const EECD: usize = 0x0010;
pub const MDIC: usize = 0x0020;
pub const FCAL: usize = 0x0028;
pub const FCAH: usize = 0x002c;
pub const FCT: usize = 0x0030;
pub const FCTTV: usize = 0x0170;
pub const FCRTL: usize = 0x2160;
pub const FCRTH: usize = 0x2168;

pub const CTRL_FD: u32 = 0x0000_0001;
pub const CTRL_GIO_MASTER_DISABLE: u32 = 0x0000_0004;
pub const CTRL_SLU: u32 = 0x0000_0040;
pub const CTRL_FRCSPD: u32 = 0x0000_0800;
pub const CTRL_FRCDPX: u32 = 0x0000_1000;
pub const CTRL_DEV_RST: u32 = 0x2000_0000;
pub const CTRL_RFCE: u32 = 0x0800_0000;
pub const CTRL_TFCE: u32 = 0x1000_0000;
pub const CTRL_PHY_RST: u32 = 0x8000_0000;

pub const STATUS_FD: u32 = 0x0000_0001;
pub const STATUS_LU: u32 = 0x0000_0002;
pub const STATUS_SPEED_100: u32 = 0x0000_0040;
pub const STATUS_SPEED_1000: u32 = 0x0000_0080;
/// With `STATUS_SPEED_1000`: 2.5 Gb/s, not 1 Gb/s.
pub const STATUS_SPEED_2500: u32 = 0x0040_0000;
pub const STATUS_GIO_MASTER_ENABLE: u32 = 0x0008_0000;

/// The NVM's automatic read of the device's settings is done.
pub const EECD_AUTO_RD: u32 = 0x0000_0200;

// The PHY's management interface.
pub const MDIC_DATA_MASK: u32 = 0x0000_ffff;
pub const MDIC_REG_MASK: u32 = 0x001f_0000;
pub const MDIC_REG_SHIFT: u32 = 16;
pub const MDIC_PHY_SHIFT: u32 = 21;
pub const MDIC_OP_WRITE: u32 = 0x0400_0000;
pub const MDIC_OP_READ: u32 = 0x0800_0000;
pub const MDIC_READY: u32 = 0x1000_0000;
pub const MDIC_ERROR: u32 = 0x4000_0000;
/// The register address field is five bits.
pub const MAX_PHY_REG: u32 = 0x1f;
/// The internal PHY's address on the management interface.
pub const PHY_ADDR: u32 = 0;
/// Polls of `MDIC` (each 50 microseconds): `IGC_GEN_POLL_TIMEOUT` times 3.
pub const MDIC_POLLS: u32 = 640 * 3;

/// The PHY's power management (the I225's `IGC_I225_PHPM`).
pub const PHPM: usize = 0x0e14;
pub const PHPM_GO_LINKD: u32 = 0x0020;
pub const PHPM_RST_COMPL: u32 = 0x0100;

// The PHY's registers (clause 22), and those reached through the MMD
// access registers (clause 45): `mmd << 16 | register`.
pub const PHY_CONTROL: u32 = 0x00;
pub const PHY_STATUS: u32 = 0x01;
pub const PHY_AUTONEG_ADV: u32 = 0x04;
pub const PHY_1000T_CTRL: u32 = 0x09;
pub const MMDAC: u32 = 13;
pub const MMDAAD: u32 = 14;
pub const MMDAC_FUNC_DATA: u16 = 0x4000;
pub const MMD_DEVADDR_SHIFT: u32 = 16;
/// The auto-negotiation device and its multi-gigabit control register.
pub const STANDARD_AN_REG_MASK: u32 = 0x0007;
pub const ANEG_MULTIGBT_AN_CTRL: u32 = 0x0020;

pub const MII_CR_RESTART_AUTO_NEG: u16 = 0x0200;
pub const MII_CR_POWER_DOWN: u16 = 0x0800;
pub const MII_CR_AUTO_NEG_EN: u16 = 0x1000;
/// The status register's link bit and auto-negotiation complete bit.
pub const MII_SR_LINK_STATUS: u16 = 0x0004;
pub const MII_SR_AUTONEG_COMPLETE: u16 = 0x0020;
pub const NWAY_AR_10T_HD: u16 = 0x0020;
pub const NWAY_AR_10T_FD: u16 = 0x0040;
pub const NWAY_AR_100TX_HD: u16 = 0x0080;
pub const NWAY_AR_100TX_FD: u16 = 0x0100;
pub const NWAY_AR_PAUSE: u16 = 0x0400;
pub const NWAY_AR_ASM_DIR: u16 = 0x0800;
pub const CR_1000T_HD: u16 = 0x0100;
pub const CR_1000T_FD: u16 = 0x0200;
pub const CR_2500T_FD: u16 = 0x0080;

// The software/firmware semaphores around the PHY.
pub const SWSM: usize = 0x5b50;
pub const SW_FW_SYNC: usize = 0x5b5c;
pub const SWSM_SMBI: u32 = 0x0000_0001;
pub const SWSM_SWESMBI: u32 = 0x0000_0002;
pub const SWFW_PHY0_SM: u32 = 0x02;

// Interrupts.
pub const ICR: usize = 0x1500;
pub const IMS: usize = 0x1508;
pub const IMC: usize = 0x150c;
pub const ICR_TXDW: u32 = 0x0000_0001;
pub const ICR_LSC: u32 = 0x0000_0004;
pub const ICR_RXSEQ: u32 = 0x0000_0008;
pub const ICR_RXDMT0: u32 = 0x0000_0010;
pub const ICR_RXO: u32 = 0x0000_0040;
pub const ICR_RXT0: u32 = 0x0000_0080;
/// `IMS_ENABLE_MASK`: what the driver takes interrupts for.
pub const IMS_ENABLE: u32 = ICR_RXT0 | ICR_TXDW | ICR_RXDMT0 | ICR_RXSEQ | ICR_LSC;
/// The device's own reset, and a fatal error: reinitialize.
pub const ICR_DRSTA: u32 = 0x4000_0000;
pub const ICR_FER: u32 = 0x0080_0000;

// Receive.
pub const RCTL: usize = 0x0100;
pub const RCTL_EN: u32 = 0x0000_0002;
pub const RCTL_SBP: u32 = 0x0000_0004;
pub const RCTL_UPE: u32 = 0x0000_0008;
pub const RCTL_MPE: u32 = 0x0000_0010;
pub const RCTL_LPE: u32 = 0x0000_0020;
pub const RCTL_BAM: u32 = 0x0000_8000;
pub const RCTL_SZ_2048: u32 = 0x0000_0000;
pub const RCTL_VFE: u32 = 0x0004_0000;
pub const RCTL_SECRC: u32 = 0x0400_0000;
pub const RXCSUM: usize = 0x5000;
pub const RXCSUM_CRCOFL: u32 = 0x0000_0800;
pub const RXCSUM_IPPCSE: u32 = 0x0000_1000;
pub const MTA: usize = 0x5200;
pub const MTA_COUNT: usize = 128;
/// Receive address registers (the first 16).
pub const RAR_COUNT: usize = 16;
pub const RAH_AV: u32 = 0x8000_0000;

/// Queue 0's receive registers.
pub const RDBAL: usize = 0x2800;
pub const RDBAH: usize = 0x2804;
pub const RDLEN: usize = 0x2808;
pub const SRRCTL: usize = 0x280c;
pub const RDH: usize = 0x2810;
pub const RDT: usize = 0x2818;
pub const RXDCTL: usize = 0x2828;
pub const SRRCTL_BSIZEPKT_SHIFT: u32 = 10;
pub const SRRCTL_DESCTYPE_ADV_ONEBUF: u32 = 0x0200_0000;
pub const RXDCTL_QUEUE_ENABLE: u32 = 0x0200_0000;
pub const RX_PTHRESH: u32 = 8;
pub const RX_HTHRESH: u32 = 8;
pub const RX_WTHRESH: u32 = 4;

/// The low and high halves of a 64-bit bus address.
#[must_use]
pub fn lo(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[must_use]
pub fn hi(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[4], b[5], b[6], b[7]])
}

/// Receive address register `i`'s low and high words.
#[must_use]
pub const fn ral(i: usize) -> usize {
    0x5400 + i * 8
}

#[must_use]
pub const fn rah(i: usize) -> usize {
    0x5404 + i * 8
}

// Transmit.
pub const TCTL: usize = 0x0400;
pub const TCTL_EN: u32 = 0x0000_0002;
pub const TCTL_PSP: u32 = 0x0000_0008;
pub const TCTL_CT: u32 = 0x0000_0ff0;
pub const TCTL_COLD: u32 = 0x003f_f000;
pub const TCTL_RTLC: u32 = 0x0100_0000;
pub const CT_SHIFT: u32 = 4;
pub const COLD_SHIFT: u32 = 12;
pub const COLLISION_THRESHOLD: u32 = 15;
pub const COLLISION_DISTANCE: u32 = 63;
pub const TDBAL: usize = 0x3800;
pub const TDBAH: usize = 0x3804;
pub const TDLEN: usize = 0x3808;
pub const TDH: usize = 0x3810;
pub const TDT: usize = 0x3818;
pub const TXDCTL: usize = 0x3828;
pub const TXDCTL_QUEUE_ENABLE: u32 = 0x0200_0000;
pub const TX_PTHRESH: u32 = 8;
pub const TX_HTHRESH: u32 = 1;

// The advanced descriptors' bits.
/// A receive descriptor's status (word 2): done, end of packet, and the
/// checksums the controller computed.
pub const RXD_STAT_DD: u32 = 0x01;
pub const RXD_STAT_EOP: u32 = 0x02;
pub const RXD_STAT_IXSM: u32 = 0x04;
pub const RXD_STAT_UDPCS: u32 = 0x10;
pub const RXD_STAT_TCPCS: u32 = 0x20;
pub const RXD_STAT_IPCS: u32 = 0x40;
/// Its errors (the top byte of word 2): a layer 4 checksum, a layer 3
/// checksum, and a bad frame.
pub const RXD_ERR_L4E: u32 = 0x2000_0000;
pub const RXD_ERR_IPE: u32 = 0x4000_0000;
pub const RXD_ERR_RXE: u32 = 0x8000_0000;
/// A transmit descriptor's command word: the advanced data type, the
/// extension bit, the end of the frame, the CRC, and a status report.
pub const ADVTXD_DTYP_DATA: u32 = 0x0030_0000;
pub const ADVTXD_DCMD_EOP: u32 = 0x0100_0000;
pub const ADVTXD_DCMD_IFCS: u32 = 0x0200_0000;
pub const ADVTXD_DCMD_RS: u32 = 0x0800_0000;
pub const ADVTXD_DCMD_DEXT: u32 = 0x2000_0000;
pub const ADVTXD_PAYLEN_SHIFT: u32 = 14;
/// The status the controller writes back into the last descriptor.
pub const TXD_STAT_DD: u32 = 0x0000_0001;
