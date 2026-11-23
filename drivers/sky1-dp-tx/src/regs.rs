// SPDX-License-Identifier: BSD-2-Clause
//! The DisplayPort transmitter core's registers (clean-room specification
//! `sky1-dp-tx` 01). Offsets are bytes from the start of the `dp` window.

/// Access to the core's 32-bit registers by byte offset.
pub trait Words: Send + Sync {
    fn read(&self, offset: usize) -> u32;
    fn write(&self, offset: usize, v: u32);
}

/// Link configuration (§1.2).
pub const LINK_BW_SET: usize = 0x000;
pub const LANE_COUNT_SET: usize = 0x004;
pub const ENHANCED_FRAMING_ENABLE: usize = 0x008;
pub const TRAINING_PATTERN_SET: usize = 0x00C;
pub const DISABLE_SCRAMBLING: usize = 0x014;
pub const DOWNSPREAD_CONTROL: usize = 0x018;

/// Enables (§1.3).
pub const TRANSMITTER_ENABLE: usize = 0x080;
pub const SOFT_RESET: usize = 0x090;
pub const SOURCE_ENABLE: usize = 0x094;
pub const FORCE_SCRAMBLER_RESET: usize = 0x0C0;
pub const CORE_REVISION: usize = 0x0FC;

/// The auxiliary channel, hot-plug detect and interrupts (§1.4).
pub const AUX_COMMAND: usize = 0x100;
pub const AUX_WRITE_FIFO: usize = 0x104;
pub const AUX_ADDRESS: usize = 0x108;
pub const AUX_CLOCK_DIVIDER: usize = 0x10C;
pub const HPD_INPUT_STATE: usize = 0x128;
pub const AUX_REPLY_DATA: usize = 0x134;
pub const AUX_REPLY_CODE: usize = 0x138;
pub const INTERRUPT_CAUSE: usize = 0x140;
pub const INTERRUPT_MASK: usize = 0x144;
pub const AUX_REPLY_DATA_COUNT: usize = 0x148;
pub const AUX_STATUS: usize = 0x14C;

/// `AUX_STATUS` bits.
pub const AUX_REPLY_RECEIVED: u32 = 1 << 0;
pub const AUX_REPLY_IN_PROGRESS: u32 = 1 << 1;
pub const AUX_REQUEST_IN_PROGRESS: u32 = 1 << 2;
pub const AUX_REPLY_ERROR: u32 = 1 << 3;

/// Interrupt causes (§1.4.1).
pub const IRQ_HPD_EVENT: u32 = 1 << 0;
pub const IRQ_HPD_IRQ: u32 = 1 << 1;
pub const IRQ_HDCP_TIMER: u32 = 1 << 5;
/// The mask in operation: HPD event, HPD IRQ and the HDCP timer on. A set
/// bit masks.
pub const IRQ_MASK_OPERATING: u32 = 0x1C;
/// Everything masked.
pub const IRQ_MASK_ALL: u32 = 0x3F;

/// AUX request commands (§1.4.2).
pub const AUX_NATIVE_WRITE: u32 = 0x8;
pub const AUX_NATIVE_READ: u32 = 0x9;
pub const AUX_I2C_WRITE: u32 = 0x0;
pub const AUX_I2C_READ: u32 = 0x1;
pub const AUX_I2C_STATUS: u32 = 0x2;
/// Middle of an I2C transaction.
pub const AUX_I2C_MOT: u32 = 0x4;
/// Zero-length transaction.
pub const AUX_ADDRESS_ONLY: u32 = 1 << 12;

/// The PHY's lane-ready bits (§1.6).
pub const PHY_STATUS: usize = 0x280;

/// Source `s`'s main-stream block (§1.5).
pub const fn source(s: usize) -> usize {
    0x800 + 0x200 * s
}
pub const VIDEO_STREAM_ENABLE: usize = 0x00;
pub const SECONDARY_STREAM_ENABLE: usize = 0x04;
pub const SECONDARY_DATA_WINDOW: usize = 0x08;
pub const DATA_CONTROL: usize = 0x10;
pub const MAIN_STREAM_OVERRIDE: usize = 0x14;
pub const MAIN_STREAM_HTOTAL: usize = 0x20;
pub const MAIN_STREAM_VTOTAL: usize = 0x24;
pub const MAIN_STREAM_POLARITY: usize = 0x28;
pub const MAIN_STREAM_HSWIDTH: usize = 0x2C;
pub const MAIN_STREAM_VSWIDTH: usize = 0x30;
pub const MAIN_STREAM_HRES: usize = 0x34;
pub const MAIN_STREAM_VRES: usize = 0x38;
pub const MAIN_STREAM_HSTART: usize = 0x3C;
pub const MAIN_STREAM_VSTART: usize = 0x40;
pub const MAIN_STREAM_MISC0: usize = 0x44;
pub const MAIN_STREAM_MISC1: usize = 0x48;
pub const TU_CONFIG: usize = 0x50;
pub const USER_PIXEL_COUNT: usize = 0x58;
pub const USER_DATA_COUNT: usize = 0x5C;
pub const USER_SYNC_POLARITY: usize = 0x64;

/// The video adapter in the `dp_rcsu` window (§1.10): pipeline `p`'s
/// register on units 0, 1, 3 and 4.
pub const fn rcsu_adapter(p: usize) -> usize {
    0x300 + 4 * p
}
