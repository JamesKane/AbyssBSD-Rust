// SPDX-License-Identifier: BSD-2-Clause
//! What is written to the combo PHY's registers: the initialisation
//! tables for each mode and plug orientation, the values for each link
//! rate, and the drive levels (clean-room specification `sky1-dp-tx` 03
//! §3.5, §3.7 to §3.9). Registers are 16 bits wide, named by index.

use alloc::vec::Vec;

use sky1_dp_tx::dpcd::Rate;

/// What the PHY carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// USB 3 only: no DisplayPort lanes.
    UsbOnly,
    /// DisplayPort on four lanes.
    DpOnly,
    /// USB 3 and DisplayPort on two lanes.
    UsbDp,
}

impl Mode {
    /// The lanes DisplayPort may use.
    #[must_use]
    pub fn dp_lanes(self) -> u8 {
        match self {
            Self::UsbOnly => 0,
            Self::DpOnly => 4,
            Self::UsbDp => 2,
        }
    }

    /// Whether the initialisation table is the DP-only one. USB only and
    /// USB plus DP share theirs (section 3.5.1).
    #[must_use]
    pub fn dp_table(self) -> bool {
        self == Self::DpOnly
    }
}

/// How the cable is plugged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Normal,
    Flipped,
}

// PMA common registers (indices).
pub const PLL0_BASE: u32 = 0;
const PLL1_OFFSET: u32 = 0x40;
const PDIAG_PLL1_OFFSET: u32 = 0x20;

pub const CMN_CTRL2: u32 = 0xE001;
pub const PLL_CTRL: u32 = 0xE003;
/// `PHY_PMA_CMN_CTRL2` bits.
pub const PLL1_LOCKED: u16 = 1 << 7;
/// `PHY_PMA_PLL_CTRL` bit: PLL1 runs.
pub const PLL1_RUN: u16 = 1 << 1;

pub const PLL1_INTDIV: u32 = 0x00D0;
pub const PLL1_FRACDIVL: u32 = 0x00D1;
pub const PLL1_HIGH_THR: u32 = 0x00D3;
pub const PLL1_SS_CTRL2: u32 = 0x00D9;
pub const PLL1_SS_CTRL3: u32 = 0x00DA;
pub const PLL1_SS_CTRL4: u32 = 0x00DB;
pub const PDIAG_PLL1_CLK_SEL: u32 = 0x01C1;
pub const PDIAG_PLL1_ITRIM: u32 = 0x01C3;

// Per-lane registers (lane 0; add `LANE_STRIDE` times the lane).
pub const LANE_STRIDE: u32 = 0x200;
pub const TX_TXCC_CTRL: u32 = 0x4040;
pub const TX_TXCC_CPOST_MULT_00: u32 = 0x404C;
pub const TX_TXCC_MGNFS_MULT_000: u32 = 0x4050;
pub const DRV_DIAG_TX_DRV: u32 = 0x40C6;
pub const XCVR_DIAG_HSCLK_DIV: u32 = 0x40E7;
pub const TX_DIAG_ACYA: u32 = 0x41E7;

/// The register of lane `l` for lane-0 index `r`.
#[must_use]
pub fn lane(r: u32, l: u32) -> u32 {
    r + LANE_STRIDE * l
}

/// A register write.
pub type Write = (u32, u16);

/// The role of a PMA lane in a mode (section 3.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Dp,
    UsbTx,
    UsbRx,
}

fn roles(mode: Mode, o: Orientation) -> [Role; 4] {
    use Role::{Dp, UsbRx, UsbTx};
    match (mode.dp_table(), o) {
        (true, _) => [Dp; 4],
        (false, Orientation::Normal) => [UsbTx, UsbRx, Dp, Dp],
        (false, Orientation::Flipped) => [Dp, Dp, UsbRx, UsbTx],
    }
}

/// The PMA lanes DisplayPort uses for `n` lanes of its link (section
/// 3.8.3 step 4): one lane is DP lane 0; two lanes are the DP pair; four
/// are all.
#[must_use]
pub fn dp_pma_lanes(mode: Mode, o: Orientation, n: u8) -> Vec<u32> {
    let all: &[u32] = match (mode.dp_table(), o) {
        (true, _) => &[0, 1, 2, 3],
        (false, Orientation::Normal) => &[2, 3],
        (false, Orientation::Flipped) => &[0, 1],
    };
    match n {
        0 => Vec::new(),
        1 => {
            // DP lane 0.
            let first = if o == Orientation::Normal { 2 } else { 1 };
            alloc::vec![first]
        }
        2 => match (mode.dp_table(), o) {
            (true, Orientation::Normal) | (false, Orientation::Normal) => alloc::vec![2, 3],
            _ => alloc::vec![0, 1],
        },
        _ => all.to_vec(),
    }
}

/// The full initialisation, in the order the specification lists it, with
/// `CMN_DIAG_GPANA_0` last.
#[must_use]
pub fn init_table(mode: Mode, o: Orientation) -> Vec<Write> {
    let mut t: Vec<Write> = Vec::new();
    let mut w = |r: u32, v: u16| t.push((r, v));
    // Common (§3.7.1).
    for (r, v) in [
        (0x0022, 0x0018),
        (0x002A, 0x0030),
        (0x002C, 0x00F0),
        (0x0032, 0x0030),
        (0x0034, 0x00F0),
        (0x0064, 0x0078),
        (0x0065, 0x0078),
        (0x0074, 0x0018),
        (0x0104, 0x001D),
        (0x010C, 0x001D),
        (0x0114, 0x02D0),
        (0x0128, 0x0137),
    ] {
        w(r, v);
    }
    for l in 0..4 {
        for (r, v) in [
            (0x8044, 0x0018),
            (0x8045, 0x0078),
            (0x804C, 0x0018),
            (0x804D, 0x0078),
            (0x4123, 0x0960),
        ] {
            w(lane(r, l), v);
        }
    }
    // PLLs. PLL1 sits 0x40 above PLL0 (0x20 for the diagnostic block).
    let pll = |w: &mut dyn FnMut(u32, u16), n: u32, vals: PllVals| {
        let o = PLL0_BASE + n * PLL1_OFFSET;
        let p = n * PDIAG_PLL1_OFFSET;
        w(0x01A1 + p, vals.clk_sel);
        w(0x0094 + o, vals.dsm);
        if let Some(t) = vals.itrim {
            w(0x01A3 + p, t);
        }
        w(0x01A4 + p, 0x0B17);
        w(0x01A5 + p, 0x0E01);
        w(0x01A6 + p, 0x0D05);
        w(0x0090 + o, vals.intdiv);
        w(0x0091 + o, vals.fracl);
        w(0x0092 + o, 0x0002);
        w(0x0093 + o, vals.thr);
        w(0x01A0 + p, 0x1002);
        for (i, v) in vals.ss.into_iter().enumerate() {
            w(0x0098 + o + u32::try_from(i).unwrap_or(0), v);
        }
        for (r, v) in [
            (0x0084, 0x00F0),
            (0x0085, 0x0004),
            (0x0086, 0x02F8),
            (0x0088, 0x02F6),
            (0x0082, 0x0003),
            (0x009C, 0x00BF),
            (0x009E, 0x00BF),
            (0x009F, 0x0005),
        ] {
            w(r + o, v);
        }
    };
    pll(
        &mut w,
        0,
        PllVals {
            clk_sel: 0x8600,
            dsm: 0x0004,
            itrim: None,
            intdiv: 0x01A0,
            fracl: 0xAAAB,
            thr: 0x0116,
            ss: [0x0001, 0x0416, 0x007C, 0x0003],
        },
    );
    pll(
        &mut w,
        1,
        PllVals {
            clk_sel: if mode.dp_table() { 0x0701 } else { 0x0601 },
            dsm: 0x8004,
            itrim: Some(0x003F),
            intdiv: 0x01C2,
            fracl: 0x0000,
            thr: 0x012C,
            ss: [0x0001, 0x044F, 0x007F, 0x0003],
        },
    );
    // PHY level.
    let map = if mode.dp_table() { 0x51D9 } else { 0x5100 };
    for (r, v) in [
        (0xC010, map),
        (0xC011, 0x0100),
        (0xC018, 0x0A0A),
        (0xC01A, 0x1000),
        (0xC01B, 0x0010),
        (0x0041, 0x8200),
        (0x0047, 0x8200),
        (0xE005, 0x000B),
        (0xE006, 0x2224),
    ] {
        w(r, v);
    }
    // Lanes by role (§3.7.2, §3.7.3).
    for (l, role) in (0u32..).zip(roles(mode, o)) {
        match role {
            Role::Dp => {
                for (r, v) in [
                    (0x40E6, 0x0001),
                    (0x40E7, if mode.dp_table() { 0x0001 } else { 0x0000 }),
                    (0x40E5, 0x0019),
                    (0x4100, 0x00FB),
                    (0x4102, 0x04AA),
                    (0x4103, 0x04AA),
                    (0x8000, 0),
                    (0x8002, 0),
                    (0x8003, 0),
                    (0x8006, 0),
                    (0x40EA, 0x000F),
                    (0x8108, 0),
                    (0x8110, 0),
                    (0x8118, 0),
                ] {
                    w(lane(r, l), v);
                }
            }
            Role::UsbTx => {
                for (r, v) in [
                    (0x40E5, 0x0041),
                    (0x4100, 0x02FF),
                    (0x4101, 0x06AF),
                    (0x4102, 0x06AE),
                    (0x4103, 0x06AE),
                    (0x4040, 0x2A82),
                    (0x404D, 0x0014),
                    (0x4050, 0x0007),
                    (0x81FF, 0x0002),
                    (0x40EA, 0x000F),
                ] {
                    w(lane(r, l), v);
                }
            }
            Role::UsbRx => {
                for (r, v) in [
                    (0x40E5, 0x0041),
                    (0x40EA, 0x00F0),
                    (0x8000, 0x0D1D),
                    (0x8001, 0x0D1D),
                    (0x8002, 0x0D00),
                    (0x8003, 0x0500),
                    (0x8090, 0x0013),
                    (0x8108, 0),
                    (0x8149, 0x0C02),
                    (0x8177, 0x0330),
                    (0x8178, 0x0300),
                    (0x40EB, 0x0003),
                    (0x81E8, 0x1004),
                    (0x81E5, 0x00F9),
                    (0x81E2, 0x0C01),
                    (0x81E3, 0x0002),
                    (0x81F5, 0x0000),
                    (0x81F4, 0x0031),
                    (0x8080, 0x018C),
                    (0x8082, 0x0003),
                ] {
                    w(lane(r, l), v);
                }
            }
        }
    }
    w(0x01F0, 0x0100);
    t
}

struct PllVals {
    clk_sel: u16,
    dsm: u16,
    itrim: Option<u16>,
    intdiv: u16,
    fracl: u16,
    thr: u16,
    ss: [u16; 4],
}

/// The values of one link rate on PLL1 and the DP lanes (§3.8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateVals {
    pub hsclk_div: u16,
    pub clk_sel: u16,
    pub intdiv: u16,
    pub fracl: u16,
    pub thr: u16,
}

#[must_use]
pub fn rate_vals(r: Rate) -> RateVals {
    match r {
        Rate::Rbr => RateVals {
            hsclk_div: 2,
            clk_sel: 0x0F01,
            intdiv: 0x0195,
            fracl: 0,
            thr: 0x010E,
        },
        Rate::Hbr => RateVals {
            hsclk_div: 1,
            clk_sel: 0x0701,
            intdiv: 0x01C2,
            fracl: 0,
            thr: 0x012C,
        },
        Rate::Hbr2 => RateVals {
            hsclk_div: 0,
            clk_sel: 0x0301,
            intdiv: 0x01C2,
            fracl: 0,
            thr: 0x012C,
        },
        Rate::Hbr3 => RateVals {
            hsclk_div: 0,
            clk_sel: 0x0200,
            intdiv: 0x0151,
            fracl: 0x8000,
            thr: 0x00E2,
        },
    }
}

/// The extras written before the rate's values (§3.8.2): current trim and
/// spread-spectrum controls. At 8.1 Gb/s the vendor writes control 3 twice
/// (`0x0062`, then `0x0004`) and never control 4; the second value is
/// taken to be meant for control 4 (an open question for the board).
#[must_use]
pub fn rate_extras(r: Rate) -> Vec<Write> {
    if r == Rate::Hbr3 {
        alloc::vec![
            (PDIAG_PLL1_ITRIM, 0x007F),
            (PLL1_SS_CTRL2, 0x0430),
            (PLL1_SS_CTRL3, 0x0062),
            (PLL1_SS_CTRL4, 0x0004),
        ]
    } else {
        alloc::vec![
            (PDIAG_PLL1_ITRIM, 0x003F),
            (PLL1_SS_CTRL2, 0x044F),
            (PLL1_SS_CTRL3, 0x007F),
            (PLL1_SS_CTRL4, 0x0003),
        ]
    }
}

/// The transmitter's margin and post-cursor values for a drive level
/// (section 3.9). The standard allows no swing plus emphasis above 3; the
/// table's filler cells stand in: (1,3) as (1,2), (2,2) and (2,3) as
/// (2,1), and swing 3 as (3,0). The levels actually used are returned.
#[must_use]
pub fn drive_vals(swing: u8, emphasis: u8) -> ((u8, u8), (u16, u16)) {
    let (s, e) = match (swing.min(3), emphasis.min(3)) {
        (1, 3) => (1, 2),
        (2, 2 | 3) => (2, 1),
        (3, _) => (3, 0),
        other => other,
    };
    let v = match (s, e) {
        (0, 0) => (0x0028, 0x0000),
        (0, 1) => (0x001F, 0x0014),
        (0, 2) => (0x0012, 0x0020),
        (0, 3) => (0x0000, 0x0027),
        (1, 0) => (0x001F, 0x0000),
        (1, 1) => (0x0013, 0x0012),
        (1, 2) => (0x0000, 0x001F),
        (2, 0) => (0x0013, 0x0000),
        (2, 1) => (0x0000, 0x0013),
        _ => (0x0000, 0x0000),
    };
    ((s, e), v)
}
