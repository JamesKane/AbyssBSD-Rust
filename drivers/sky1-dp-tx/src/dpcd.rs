// SPDX-License-Identifier: BSD-2-Clause
//! DisplayPort configuration data (DPCD) addresses and what a sink's
//! capability block says (VESA DisplayPort 1.4, as the specification
//! `sky1-dp-tx` 02 §2.4 uses it).

/// Addresses.
pub const REV: u32 = 0x000;
pub const MAX_LINK_RATE: u32 = 0x001;
pub const MAX_LANE_COUNT: u32 = 0x002;
pub const MAX_DOWNSPREAD: u32 = 0x003;
pub const TRAINING_AUX_RD_INTERVAL: u32 = 0x00E;
pub const LINK_BW_SET: u32 = 0x100;
pub const LANE_COUNT_SET: u32 = 0x101;
pub const TRAINING_PATTERN_SET: u32 = 0x102;
pub const TRAINING_LANE0_SET: u32 = 0x103;
pub const DOWNSPREAD_CTRL: u32 = 0x107;
pub const MAIN_LINK_CHANNEL_CODING_SET: u32 = 0x108;
pub const SINK_COUNT: u32 = 0x200;
pub const LANE0_1_STATUS: u32 = 0x202;
pub const LANE_ALIGN_STATUS: u32 = 0x204;
pub const ADJUST_REQUEST_LANE0_1: u32 = 0x206;
pub const SET_POWER: u32 = 0x600;

/// `LANE_COUNT_SET` bit: enhanced framing.
pub const ENHANCED_FRAMING: u8 = 1 << 7;

/// Link rates, as the code the sink and the core use (`LINK_BW_SET`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rate {
    Rbr,
    Hbr,
    Hbr2,
    Hbr3,
}

impl Rate {
    pub const ALL: [Self; 4] = [Self::Rbr, Self::Hbr, Self::Hbr2, Self::Hbr3];

    /// The bandwidth code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Rbr => 0x06,
            Self::Hbr => 0x0A,
            Self::Hbr2 => 0x14,
            Self::Hbr3 => 0x1E,
        }
    }

    #[must_use]
    pub const fn from_code(c: u8) -> Option<Self> {
        match c {
            0x06 => Some(Self::Rbr),
            0x0A => Some(Self::Hbr),
            0x14 => Some(Self::Hbr2),
            0x1E => Some(Self::Hbr3),
            _ => None,
        }
    }

    /// The link symbol clock, in kHz.
    #[must_use]
    pub fn khz(self) -> u32 {
        u32::from(self.code()) * 27_000
    }

    /// The next slower rate.
    #[must_use]
    pub const fn lower(self) -> Option<Self> {
        match self {
            Self::Rbr => None,
            Self::Hbr => Some(Self::Rbr),
            Self::Hbr2 => Some(Self::Hbr),
            Self::Hbr3 => Some(Self::Hbr2),
        }
    }
}

/// What the sink can do (DPCD `0x000` to `0x00F`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub revision: u8,
    pub max_rate: Rate,
    pub max_lanes: u8,
    pub enhanced_framing: bool,
    pub tps3: bool,
    pub tps4: bool,
    pub downspread: bool,
    pub fast_train: bool,
    /// The training interval the sink asks for between steps, in
    /// microseconds (`0x00E`).
    pub aux_interval_us: u32,
}

/// Why a capability block is not usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapsError {
    /// A rate code the standard does not define (or is above HBR3).
    Rate(u8),
    /// No lanes, or more than four.
    Lanes(u8),
}

impl Caps {
    /// Parses the 16 bytes at DPCD `0x000`.
    ///
    /// # Errors
    ///
    /// [`CapsError`] for a block no source can use.
    pub fn parse(b: &[u8; 16]) -> Result<Self, CapsError> {
        let max_rate = Rate::from_code(b[1]).ok_or(CapsError::Rate(b[1]))?;
        let lanes = b[2] & 0x1F;
        if lanes == 0 || lanes > 4 {
            return Err(CapsError::Lanes(lanes));
        }
        // 0x00E: 0 is 100 microseconds (clock recovery) and 400
        // (equalisation); 1 to 4 is 4 ms times the value.
        let aux_interval_us = match b[14] & 0x7F {
            0 => 0,
            n => u32::from(n) * 4000,
        };
        Ok(Self {
            revision: b[0],
            max_rate,
            max_lanes: lanes,
            enhanced_framing: b[2] & 0x80 != 0,
            tps3: b[2] & 0x40 != 0,
            tps4: b[3] & 0x80 != 0,
            downspread: b[3] & 1 != 0,
            fast_train: b[3] & 0x40 != 0,
            aux_interval_us,
        })
    }

    /// How long to wait before reading clock-recovery status.
    #[must_use]
    pub fn cr_wait_us(&self) -> u32 {
        if self.aux_interval_us == 0 {
            100
        } else {
            self.aux_interval_us
        }
    }

    /// How long to wait before reading equalisation status.
    #[must_use]
    pub fn eq_wait_us(&self) -> u32 {
        if self.aux_interval_us == 0 {
            400
        } else {
            self.aux_interval_us
        }
    }
}

/// The status of lane `i` from the bytes at `0x202`, `0x203`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lane {
    pub cr_done: bool,
    pub eq_done: bool,
    pub symbol_locked: bool,
}

/// Decodes lane `i` (0 to 3) from the two status bytes.
#[must_use]
pub fn lane_status(status: &[u8; 2], i: usize) -> Lane {
    let nib = (status[i / 2] >> (4 * (i % 2))) & 0xF;
    Lane {
        cr_done: nib & 1 != 0,
        eq_done: nib & 2 != 0,
        symbol_locked: nib & 4 != 0,
    }
}

/// Per-lane drive settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drive {
    /// Voltage swing, 0 to 3.
    pub swing: u8,
    /// Pre-emphasis, 0 to 3.
    pub emphasis: u8,
}

impl Drive {
    /// The byte written to `TRAINING_LANEx_SET`: swing, pre-emphasis,
    /// and the flags for having reached the maximum of each.
    #[must_use]
    pub fn byte(self) -> u8 {
        let mut b = self.swing | self.emphasis << 3;
        if self.swing >= 3 {
            b |= 1 << 2;
        }
        if self.emphasis >= 2 {
            b |= 1 << 5;
        }
        b
    }
}

/// The sink's requests from the two adjust bytes (`0x206`, `0x207`) for
/// lane `i`.
#[must_use]
pub fn adjust_request(adj: &[u8; 2], i: usize) -> Drive {
    let v = adj[i / 2] >> (4 * (i % 2));
    Drive {
        swing: v & 3,
        emphasis: (v >> 2) & 3,
    }
}
