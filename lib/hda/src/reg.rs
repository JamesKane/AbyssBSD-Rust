// SPDX-License-Identifier: BSD-2-Clause
//! The controller's registers (specification section 3.3): byte offsets
//! into the memory BAR, and the bit fields the driver uses.

/// Global capabilities (16 bits).
pub const GCAP: usize = 0x00;
/// Global control (32 bits).
pub const GCTL: usize = 0x08;
/// Wake enable (16 bits).
pub const WAKEEN: usize = 0x0c;
/// State change status (16 bits): one bit per codec address that came out
/// of reset.
pub const STATESTS: usize = 0x0e;
/// Interrupt control (32 bits).
pub const INTCTL: usize = 0x20;
/// Interrupt status (32 bits).
pub const INTSTS: usize = 0x24;
/// Command output ring buffer, lower base (32 bits).
pub const CORBLBASE: usize = 0x40;
/// Command output ring buffer, upper base (32 bits).
pub const CORBUBASE: usize = 0x44;
/// Command output ring buffer, write pointer (16 bits).
pub const CORBWP: usize = 0x48;
/// Command output ring buffer, read pointer (16 bits).
pub const CORBRP: usize = 0x4a;
/// Command output ring buffer, control (8 bits).
pub const CORBCTL: usize = 0x4c;
/// Command output ring buffer, size (8 bits).
pub const CORBSIZE: usize = 0x4e;
/// Response input ring buffer, lower base (32 bits).
pub const RIRBLBASE: usize = 0x50;
/// Response input ring buffer, upper base (32 bits).
pub const RIRBUBASE: usize = 0x54;
/// Response input ring buffer, write pointer (16 bits).
pub const RIRBWP: usize = 0x58;
/// Response interrupt count (16 bits).
pub const RINTCNT: usize = 0x5a;
/// Response input ring buffer, control (8 bits).
pub const RIRBCTL: usize = 0x5c;
/// Response input ring buffer, size (8 bits).
pub const RIRBSIZE: usize = 0x5e;
/// DMA position buffer, lower base (32 bits).
pub const DPLBASE: usize = 0x70;
/// DMA position buffer, upper base (32 bits).
pub const DPUBASE: usize = 0x74;
/// The first stream descriptor.
pub const SD_BASE: usize = 0x80;
/// The distance between stream descriptors.
pub const SD_STEP: usize = 0x20;

/// `GCTL`: controller reset is released (the controller is running).
pub const GCTL_CRST: u32 = 1 << 0;
/// `GCTL`: accept unsolicited responses.
pub const GCTL_UNSOL: u32 = 1 << 8;

/// `CORBRP`: reset the read pointer.
pub const CORBRP_RST: u16 = 1 << 15;
/// `CORBCTL`: the DMA engine runs.
pub const CORBCTL_RUN: u8 = 1 << 1;
/// `RIRBWP`: reset the write pointer.
pub const RIRBWP_RST: u16 = 1 << 15;
/// `RIRBCTL`: the DMA engine runs.
pub const RIRBCTL_DMAEN: u8 = 1 << 1;

/// What `GCAP` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Output streams.
    pub output_streams: u8,
    /// Input streams.
    pub input_streams: u8,
    /// Bidirectional streams.
    pub bidirectional_streams: u8,
    /// Serial data output signals, as the field's power of two.
    pub serial_data_out: u8,
    /// Whether 64-bit addresses are supported.
    pub addr64: bool,
}

impl Caps {
    /// Decodes `GCAP`.
    #[must_use]
    pub const fn of(gcap: u16) -> Self {
        Self {
            output_streams: (gcap >> 12) as u8 & 0xf,
            input_streams: (gcap >> 8) as u8 & 0xf,
            bidirectional_streams: (gcap >> 3) as u8 & 0x1f,
            serial_data_out: (gcap >> 1) as u8 & 0x3,
            addr64: gcap & 1 != 0,
        }
    }

    /// The streams in all.
    #[must_use]
    pub const fn streams(self) -> usize {
        self.output_streams as usize
            + self.input_streams as usize
            + self.bidirectional_streams as usize
    }
}

/// A ring's size, from its `SIZE` register: the largest the capability
/// bits (7:4) offer. `None` if it offers none, which FreeBSD treats as
/// 256 entries and writes back.
#[must_use]
pub const fn ring_entries(size_reg: u8) -> Option<u16> {
    let cap = size_reg >> 4 & 0xf;
    if cap & 0b100 != 0 {
        Some(256)
    } else if cap & 0b010 != 0 {
        Some(16)
    } else if cap & 0b001 != 0 {
        Some(2)
    } else {
        None
    }
}

/// The `SIZE` register's size-selection field (bits 1:0) for a ring of
/// `entries` (2, 16 or 256).
#[must_use]
pub const fn ring_size_select(entries: u16) -> u8 {
    match entries {
        256 => 2,
        16 => 1,
        _ => 0,
    }
}

/// Whether codec address `cad` came out of reset, by `STATESTS`.
#[must_use]
pub const fn codec_present(statests: u16, cad: u8) -> bool {
    cad < 15 && statests & (1 << cad) != 0
}

/// The most codec addresses (`HDAC_CODEC_MAX`).
pub const CODEC_MAX: usize = 15;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_of_a_controller_with_four_streams_each_way() {
        let c = Caps::of(0x4401);
        assert_eq!(c.output_streams, 4);
        assert_eq!(c.input_streams, 4);
        assert_eq!(c.bidirectional_streams, 0);
        assert!(c.addr64);
        assert_eq!(c.streams(), 8);
    }

    #[test]
    fn ring_sizes() {
        assert_eq!(ring_entries(0x40), Some(256));
        assert_eq!(ring_entries(0x70), Some(256));
        assert_eq!(ring_entries(0x20), Some(16));
        assert_eq!(ring_entries(0x10), Some(2));
        assert_eq!(ring_entries(0x00), None);
        assert_eq!(ring_size_select(256), 2);
        assert_eq!(ring_size_select(16), 1);
        assert_eq!(ring_size_select(2), 0);
    }

    #[test]
    fn codecs_by_state_change_bit() {
        assert!(codec_present(0b101, 0));
        assert!(!codec_present(0b101, 1));
        assert!(codec_present(0b101, 2));
        // Address 15 does not exist.
        assert!(!codec_present(0xffff, 15));
    }
}
