// SPDX-License-Identifier: BSD-2-Clause
//! The display processor's registers: offsets within a block and the
//! encodings of the values written to them (clean-room specification
//! `linlon-dpu`, section 02).
//!
//! Every block is `0x200` bytes. Most registers are shadowed: a write
//! collects in a shadow copy and takes effect when the pipeline's
//! config-valid register is written (section 00 §0.4).

/// A block's size and the stride of the block list.
pub const BLOCK: usize = 0x200;

/// Access to the device's 32-bit registers, by byte offset from the start
/// of its window. The kernel's implementation is an uncached mapping; the
/// tests' is a simulated device.
pub trait Words: Send + Sync {
    /// Reads the word at `offset`.
    fn read(&self, offset: usize) -> u32;
    /// Writes the word at `offset`.
    fn write(&self, offset: usize, v: u32);
}

/// Registers every block has (section 02 §2.2).
pub mod common {
    pub const BLOCK_INFO: usize = 0x000;
    pub const PIPELINE_INFO: usize = 0x004;
    pub const MAX_LINE_SIZE: usize = 0x008;
    pub const VALID_INPUT_ID: usize = 0x020;
    pub const OUTPUT_ID: usize = 0x060;
    pub const INPUT_ID: usize = 0x080;
    pub const IRQ_RAW_STATUS: usize = 0x0A0;
    /// Write only.
    pub const IRQ_CLEAR: usize = 0x0A4;
    pub const IRQ_MASK: usize = 0x0A8;
    pub const IRQ_STATUS: usize = 0x0AC;
    pub const STATUS: usize = 0x0B0;
    pub const INFO: usize = 0x0C0;
    pub const CONTROL: usize = 0x0D0;
    pub const SIZE: usize = 0x0D4;
    pub const IN_SIZE: usize = 0x0E0;
}

/// Block types, `BLOCK_INFO[15:8]` (section 02 §2.2.1).
pub mod kind {
    pub const GCU: u8 = 0x00;
    pub const LPU: u8 = 0x01;
    pub const CU: u8 = 0x02;
    pub const DOU: u8 = 0x03;
    pub const PERIPH: u8 = 0x08;
    pub const LAYER: u8 = 0x10;
    pub const WRITEBACK: u8 = 0x11;
    pub const SPLITTER: u8 = 0x20;
    pub const SCALER: u8 = 0x21;
    pub const MERGER: u8 = 0x22;
    pub const IPS: u8 = 0x30;
    pub const BS: u8 = 0x31;
    pub const FT_TABLE: u8 = 0x32;
    /// A slot with nothing in it.
    pub const RESERVED: u8 = 0xFF;
}

/// The global control unit (block 0; section 02 §2.3).
pub mod gcu {
    pub const CORE_ID: usize = 0x004;
    pub const CORE_INFO: usize = 0x008;
    pub const IRQ_STATUS: usize = 0x010;
    pub const STATUS: usize = 0x0B0;
    pub const CONTROL: usize = 0x0D0;
    /// Config-valid of pipeline `p`.
    pub const fn config_valid(p: usize) -> usize {
        0x0D4 + 4 * p
    }
    pub const CONFIG_ID0: usize = 0x100;
    pub const CONFIG_ID1: usize = 0x104;

    /// `GLB_CORE_ID[31:16]`: Linlon-D6.
    pub const PRODUCT_D6: u32 = 0x0060;
    /// Other products of the register model (Mali-D71, Mali-D32).
    pub const PRODUCT_D71: u32 = 0x0071;
    pub const PRODUCT_D32: u32 = 0x0032;

    /// `GCU_CONTROL.SRST`: soft reset, self clearing.
    pub const SRST: u32 = 1 << 16;
    /// Operating modes, `GCU_CONTROL[2:0]`.
    pub const MODE_INACTIVE: u32 = 0;
    pub const MODE_TBU_CONNECT: u32 = 1;
    pub const MODE_TBU_DISCONNECT: u32 = 2;
    pub const MODE_DISPLAY0: u32 = 3;
    pub const MODE_DISPLAY1: u32 = 4;
    pub const MODE_BOTH: u32 = 5;
    pub const MODE_MASK: u32 = 7;

    /// The summary bits of `GLB_IRQ_STATUS`.
    pub const SUMMARY_GCU: u32 = 1 << 0;
    pub const fn summary_lpu(p: usize) -> u32 {
        1 << (8 + p)
    }
    pub const fn summary_cu(p: usize) -> u32 {
        1 << (16 + p)
    }
    pub const fn summary_dou(p: usize) -> u32 {
        1 << (24 + p)
    }

    /// `GCU` interrupt bits.
    pub const fn irq_cval(p: usize) -> u32 {
        1 << p
    }
    pub const IRQ_MODE: u32 = 1 << 4;
    pub const IRQ_ERR: u32 = 1 << 11;
    /// `GCU_STATUS.MERR`: a mode error, sticky.
    pub const STATUS_MERR: u32 = 1 << 4;
    /// `GCU_STATUS.TCS0`: translation unit 0 connected.
    pub const STATUS_TCS0: u32 = 1 << 8;
}

/// The display output unit's interrupt bits (section 02 §2.12).
pub mod dou {
    pub const IRQ_UND: u32 = 1 << 8;
    pub const IRQ_ERR: u32 = 1 << 11;
    /// The programmed line, which the driver uses as the frame.
    pub const IRQ_PL0: u32 = 1 << 13;
}

/// A read layer (section 02 §2.6).
pub mod layer {
    pub const LAYER_INFO: usize = 0x0C0;
    pub const CONTROL: usize = 0x0D0;
    pub const RICH_CONTROL: usize = 0x0D4;
    pub const FORMAT: usize = 0x0D8;
    pub const IN_SIZE: usize = 0x0E0;
    pub const PALPHA: usize = 0x0E4;
    pub const P0_PTR_LOW: usize = 0x100;
    pub const P0_PTR_HIGH: usize = 0x104;
    pub const P0_STRIDE: usize = 0x108;
    pub const P1_PTR_LOW: usize = 0x110;
    pub const P1_PTR_HIGH: usize = 0x114;
    pub const P1_STRIDE: usize = 0x118;
    pub const P2_PTR_LOW: usize = 0x120;
    pub const P2_PTR_HIGH: usize = 0x124;
    pub const YUV_RGB_COEFF: usize = 0x130;
    pub const AD_CONTROL: usize = 0x160;

    /// `LAYER_INFO[0]`: a rich layer (takes YUV).
    pub const INFO_RICH: u32 = 1;
    /// `L_CONTROL` fields.
    pub const CTRL_EN: u32 = 1;
    pub const CTRL_ROT_SHIFT: u32 = 8;
    pub const CTRL_HFLIP: u32 = 1 << 10;
    pub const CTRL_VFLIP: u32 = 1 << 11;
    pub const CTRL_TBU_EN: u32 = 1 << 16;
    /// The reset value of the alpha expansion map: codes 0..3 give alpha
    /// 0x00, 0x55, 0xAA, 0xFF.
    pub const PALPHA_DEFAULT: u32 = 0xFFAA_5500;
}

/// The compositor (section 02 §2.8).
pub mod cu {
    pub const INPUT_ID: usize = 0x080;
    pub const STATUS: usize = 0x0B0;
    pub const CONTROL: usize = 0x0D0;
    pub const SIZE: usize = 0x0D4;
    /// Registers of input `n`.
    pub const fn input_size(n: usize) -> usize {
        0x0E0 + 0x10 * n
    }
    pub const fn input_offset(n: usize) -> usize {
        0x0E4 + 0x10 * n
    }
    pub const fn input_control(n: usize) -> usize {
        0x0E8 + 0x10 * n
    }
    /// `CU_INPUT<n>_CONTROL`.
    pub const IN_EN: u32 = 1;
    /// Ignore the pixel's alpha.
    pub const IN_PAD: u32 = 1 << 1;
    /// The pixel's colour is premultiplied.
    pub const IN_PMUL: u32 = 1 << 2;
    /// A disabled input: alpha field 0xFF, nothing else.
    pub const IN_DISABLED: u32 = 0x0000_FF00;
    /// Inputs a compositor has.
    pub const INPUTS: usize = 5;
}

/// The image processor (section 02 §2.13).
pub mod ips {
    pub const INPUT_ID0: usize = 0x080;
    pub const INPUT_ID1: usize = 0x084;
    pub const INFO: usize = 0x0C0;
    pub const CONTROL: usize = 0x0D0;
    pub const SIZE: usize = 0x0D4;
    pub const DEPTH: usize = 0x0D8;
    pub const CTRL_RGB: u32 = 1;
    pub const CTRL_YUV: u32 = 1 << 8;
    pub const CTRL_CHD422: u32 = 1 << 9;
    pub const CTRL_CHD420: u32 = 1 << 10;
}

/// The timing generator (section 02 §2.15).
pub mod bs {
    pub const CONTROL: usize = 0x0D0;
    pub const PROG_LINE: usize = 0x0D4;
    pub const PREFETCH_LINE: usize = 0x0D8;
    pub const ACTIVESIZE: usize = 0x0E0;
    pub const HINTERVALS: usize = 0x0E4;
    pub const VINTERVALS: usize = 0x0E8;
    pub const SYNC: usize = 0x0EC;
    pub const DRIFT_TO: usize = 0x100;
    pub const CTRL_EN: u32 = 1;
    /// Video mode: continuous timing. Set at reset.
    pub const CTRL_VM: u32 = 1 << 1;
    /// Two pixels per clock (`[22:21]` = 1).
    pub const CTRL_PPC2: u32 = 1 << 21;
    pub const SYNC_HSP: u32 = 1 << 12;
    pub const SYNC_VSP: u32 = 1 << 28;
}

/// The size encodings (section 02 §2.2.3): a horizontal and a vertical
/// count. `None` where a count does not fit its field.
#[must_use]
pub fn hv_size(h: u32, v: u32) -> Option<u32> {
    (h <= 0x1FFF && v <= 0x1FFF).then_some(h | v << 16)
}

/// A position: up to 4095 each way.
#[must_use]
pub fn hv_offset(h: u32, v: u32) -> Option<u32> {
    (h <= 0xFFF && v <= 0xFFF).then_some(h | v << 16)
}

/// The input ID that names a block: its `BLOCK_INFO` without the count of
/// sub-blocks, plus the output port (section 02 §2.2.2).
#[must_use]
pub fn input_id(block_info: u32, port: u32) -> u32 {
    (block_info & 0xFFF0) + (port & 0xF)
}

/// The low and high words of a 40-bit device address.
#[must_use]
pub fn addr_words(addr: u64) -> (u32, u32) {
    (
        u32::try_from(addr & 0xFFFF_FFFF).unwrap_or(0),
        u32::try_from(addr >> 32).unwrap_or(u32::MAX),
    )
}
