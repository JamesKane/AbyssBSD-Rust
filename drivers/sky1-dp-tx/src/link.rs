// SPDX-License-Identifier: BSD-2-Clause
//! Choosing a link rate and lane count, and training the link
//! (clean-room specification `sky1-dp-tx` 02 §2.5).

use crate::Delay;
use crate::aux::{Aux, AuxError};
use crate::dpcd::{self, Caps, Drive, Rate};
use crate::regs::{self, Words};

/// What the controller needs from the physical layer (sections 03, 04 of
/// the specification): the combo PHY of a USB-C port or the eDP PHY.
pub trait Phy: Send + Sync {
    /// Tells the PHY the link rate and lane count.
    fn set_link(&self, rate: Rate, lanes: u8);
    /// Sets the drive levels of every lane.
    fn set_drive(&self, drive: Drive);
    /// How many lanes the port can give DisplayPort (a combo PHY shared
    /// with USB 3 may give two).
    fn lanes(&self) -> u8;
}

/// What the link is limited to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_rate: Rate,
    pub max_lanes: u8,
}

impl Limits {
    /// The lesser of what the sink, the board and the PHY give.
    #[must_use]
    pub fn of(caps: &Caps, board_rate: Rate, board_lanes: u8, phy_lanes: u8) -> Self {
        Self {
            max_rate: caps.max_rate.min(board_rate),
            max_lanes: caps.max_lanes.min(board_lanes).min(phy_lanes),
        }
    }
}

/// A choice of rate and lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub rate: Rate,
    pub lanes: u8,
}

/// Whether `pixel_khz` at `bpp_x2` (twice the bits per pixel) fits in the
/// link, with `margin_pct` percent to spare.
fn fits(pixel_khz: u32, bpp_x2: u32, l: Link, margin_pct: u64) -> bool {
    // Payload bit rate per lane after 8b/10b: symbol clock times 8.
    let cap = u64::from(l.rate.khz()) * u64::from(l.lanes) * 8 * 2;
    u64::from(pixel_khz) * u64::from(bpp_x2) * 100 <= cap * (100 - margin_pct)
}

/// Rate and lanes for a mode (section 2.5.1 steps 2 and 4): the first
/// rate whose capacity at the most lanes leaves 5 % spare (the maximum
/// rate if none), then the fewest lanes that carry it at that rate.
/// `None` if even the limits do not carry the mode.
#[must_use]
pub fn pick_link(pixel_khz: u32, bpp_x2: u32, lim: Limits) -> Option<Link> {
    let top = Link {
        rate: lim.max_rate,
        lanes: lim.max_lanes,
    };
    if !fits(pixel_khz, bpp_x2, top, 0) {
        return None;
    }
    let rate = Rate::ALL
        .into_iter()
        .filter(|r| *r <= lim.max_rate)
        .find(|r| {
            fits(
                pixel_khz,
                bpp_x2,
                Link {
                    rate: *r,
                    lanes: lim.max_lanes,
                },
                5,
            )
        })
        .unwrap_or(lim.max_rate);
    let lanes = [1u8, 2, 4]
        .into_iter()
        .filter(|n| *n <= lim.max_lanes)
        .find(|n| fits(pixel_khz, bpp_x2, Link { rate, lanes: *n }, 0))
        .unwrap_or(lim.max_lanes);
    Some(Link { rate, lanes })
}

/// The deepest RGB colour (section 2.5.1 step 1) the link can carry: start
/// at what the sink has, within what the user allows, and step down by two
/// to six bits per component.
#[must_use]
pub fn pick_rgb_bpc(pixel_khz: u32, sink_bpc: u32, max_bpc: u32, lim: Limits) -> Option<u32> {
    let top = Link {
        rate: lim.max_rate,
        lanes: lim.max_lanes,
    };
    let mut bpc = sink_bpc.min(max_bpc);
    while bpc >= 6 {
        // 3 components, doubled for the half-bit unit.
        if fits(pixel_khz, 6 * bpc, top, 0) {
            return Some(bpc);
        }
        bpc -= 2;
    }
    None
}

/// Why training failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrainError {
    Aux(AuxError),
    /// The PHY's lanes did not come ready.
    Phy,
    /// The sink never locked the clock.
    ClockRecovery,
    /// The sink never equalised.
    Equalization,
}

impl From<AuxError> for TrainError {
    fn from(e: AuxError) -> Self {
        Self::Aux(e)
    }
}

/// What training reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trained {
    pub link: Link,
    pub drive: Drive,
}

/// The pieces a training run works with.
#[allow(missing_debug_implementations)]
pub struct Trainer<'a> {
    pub w: &'a dyn Words,
    pub d: &'a dyn Delay,
    pub phy: &'a dyn Phy,
    pub caps: Caps,
    /// Lanes the port has: the lane-ready bits to wait for.
    pub port_lanes: u8,
}

const CR_ITERATIONS: u32 = 512;
const EQ_ITERATIONS: u32 = 5;

impl Trainer<'_> {
    fn aux(&self) -> Aux<'_> {
        Aux::new(self.w, self.d)
    }

    /// Configures the link and waits for the PHY (section 2.5.2).
    fn configure(&self, l: Link) -> Result<(), TrainError> {
        let aux = self.aux();
        let enhanced = self.caps.enhanced_framing;
        self.w.write(regs::LANE_COUNT_SET, u32::from(l.lanes));
        self.w
            .write(regs::ENHANCED_FRAMING_ENABLE, u32::from(enhanced));
        let ds = self.caps.downspread;
        self.w.write(regs::DOWNSPREAD_CONTROL, u32::from(ds));
        aux.write(dpcd::DOWNSPREAD_CTRL, &[if ds { 0x10 } else { 0 }])?;
        let lanes = l.lanes | if enhanced { dpcd::ENHANCED_FRAMING } else { 0 };
        aux.write(dpcd::LANE_COUNT_SET, &[lanes])?;
        aux.write(dpcd::MAIN_LINK_CHANNEL_CODING_SET, &[1])?;
        aux.write(dpcd::LINK_BW_SET, &[l.rate.code()])?;
        self.w.write(regs::LINK_BW_SET, u32::from(l.rate.code()));
        self.phy.set_link(l.rate, l.lanes);
        let want = (1u32 << self.port_lanes) - 1;
        for _ in 0..100 {
            if self.w.read(regs::PHY_STATUS) & want == want {
                return Ok(());
            }
            self.d.delay_us(1000);
        }
        Err(TrainError::Phy)
    }

    /// Sets the drive on the PHY and in the sink, for `lanes` lanes.
    fn drive(&self, drive: Drive, lanes: u8) -> Result<(), TrainError> {
        self.phy.set_drive(drive);
        let bytes = [drive.byte(); 4];
        self.aux()
            .write(dpcd::TRAINING_LANE0_SET, &bytes[..usize::from(lanes)])?;
        Ok(())
    }

    /// The sink's status: lane bytes, alignment, and the adjust requests.
    fn status(&self) -> Result<[u8; 6], TrainError> {
        let mut b = [0u8; 6];
        self.aux().read(dpcd::LANE0_1_STATUS, &mut b)?;
        Ok(b)
    }

    /// The highest swing and emphasis any of `lanes` asks for, applied to
    /// all (the controller drives them alike).
    fn requested(adj: &[u8; 2], lanes: u8) -> Drive {
        let mut d = Drive::default();
        for i in 0..usize::from(lanes) {
            let r = dpcd::adjust_request(adj, i);
            d.swing = d.swing.max(r.swing);
            d.emphasis = d.emphasis.max(r.emphasis);
        }
        d
    }

    fn finish(&self) -> Result<(), TrainError> {
        self.aux().write(dpcd::TRAINING_PATTERN_SET, &[0])?;
        self.w.write(regs::TRAINING_PATTERN_SET, 0);
        self.w.write(regs::DISABLE_SCRAMBLING, 0);
        Ok(())
    }

    /// Clock recovery then channel equalisation at `l` (sections 2.5.3 to
    /// 2.5.5). Training ends whether it worked or not.
    ///
    /// # Errors
    ///
    /// [`TrainError`] for the phase that failed.
    pub fn train(&self, l: Link) -> Result<Trained, TrainError> {
        self.configure(l)?;
        let r = self.run(l);
        let end = self.finish();
        let t = r?;
        end?;
        Ok(t)
    }

    fn run(&self, l: Link) -> Result<Trained, TrainError> {
        let n = usize::from(l.lanes);
        let aux = self.aux();
        // Clock recovery with TPS1, scrambling off.
        self.w.write(regs::DISABLE_SCRAMBLING, 1);
        self.w.write(regs::TRAINING_PATTERN_SET, 1);
        aux.write(dpcd::TRAINING_PATTERN_SET, &[0x21])?;
        let mut drive = Drive::default();
        let mut same = 0;
        let mut cr = false;
        for _ in 0..CR_ITERATIONS {
            self.drive(drive, l.lanes)?;
            self.d.delay_us(u64::from(self.caps.cr_wait_us()));
            let s = self.status()?;
            let st = [s[0], s[1]];
            if (0..n).all(|i| dpcd::lane_status(&st, i).cr_done) {
                cr = true;
                break;
            }
            if drive.swing >= 3 {
                break;
            }
            let want = Self::requested(&[s[4], s[5]], l.lanes);
            if want.swing == drive.swing {
                same += 1;
                if same >= 5 {
                    break;
                }
            } else {
                same = 0;
            }
            drive = want;
        }
        if !cr {
            return Err(TrainError::ClockRecovery);
        }
        // Channel equalisation with the best pattern both sides have.
        let (core, sink) = if self.caps.tps4 {
            (4, 0x07)
        } else if self.caps.tps3 {
            (3, 0x23)
        } else {
            (2, 0x22)
        };
        self.w.write(regs::TRAINING_PATTERN_SET, core);
        // TPS4 is scrambled; the others are not.
        self.w.write(regs::DISABLE_SCRAMBLING, u32::from(core != 4));
        aux.write(dpcd::TRAINING_PATTERN_SET, &[sink])?;
        for _ in 0..EQ_ITERATIONS {
            self.drive(drive, l.lanes)?;
            self.d.delay_us(u64::from(self.caps.eq_wait_us()));
            let s = self.status()?;
            let st = [s[0], s[1]];
            let lanes: tally::Lanes = (0..n)
                .map(|i| dpcd::lane_status(&st, i))
                .fold(tally::Lanes::default(), tally::Lanes::add);
            if !lanes.cr {
                return Err(TrainError::ClockRecovery);
            }
            if lanes.eq && lanes.locked && s[2] & 1 != 0 {
                return Ok(Trained { link: l, drive });
            }
            drive = Self::requested(&[s[4], s[5]], l.lanes);
        }
        Err(TrainError::Equalization)
    }

    /// Trains, stepping down the rate when it fails (section 2.5.5): up to
    /// ten attempts. Lanes are recomputed for the new rate.
    ///
    /// # Errors
    ///
    /// The last [`TrainError`].
    pub fn train_with_fallback(
        &self,
        pixel_khz: u32,
        bpp_x2: u32,
        lim: Limits,
        first: Link,
    ) -> Result<Trained, TrainError> {
        let mut l = first;
        let mut last = TrainError::ClockRecovery;
        for _ in 0..10 {
            match self.train(l) {
                Ok(t) => return Ok(t),
                Err(TrainError::Aux(AuxError::NoSink)) => {
                    return Err(TrainError::Aux(AuxError::NoSink));
                }
                Err(e) => last = e,
            }
            let Some(lower) = l.rate.lower() else { break };
            let lim = Limits {
                max_rate: lower,
                ..lim
            };
            match pick_link(pixel_khz, bpp_x2, lim) {
                Some(n) => l = n,
                None => break,
            }
        }
        Err(last)
    }
}

/// The lanes' status folded into whether all are good.
mod tally {
    use crate::dpcd::Lane;

    #[derive(Clone, Copy)]
    pub(super) struct Lanes {
        pub(super) cr: bool,
        pub(super) eq: bool,
        pub(super) locked: bool,
    }

    impl Default for Lanes {
        fn default() -> Self {
            Self {
                cr: true,
                eq: true,
                locked: true,
            }
        }
    }

    impl Lanes {
        pub(super) fn add(self, l: Lane) -> Self {
            Self {
                cr: self.cr && l.cr_done,
                eq: self.eq && l.eq_done,
                locked: self.locked && l.symbol_locked,
            }
        }
    }
}
