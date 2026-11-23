// SPDX-License-Identifier: BSD-2-Clause
//! One transmitter: host initialisation, the hot-plug interrupt, and
//! bringing a link and a stream up and down (clean-room specification
//! `sky1-dp-tx` 02 §2.1, §2.2, §2.5.9, §2.11.2).

use alloc::sync::Arc;
use alloc::vec::Vec;

use kms_check::Mode;

use crate::Delay;
use crate::aux::{Aux, AuxError};
use crate::dpcd::{self, Caps, CapsError, Rate};
use crate::link::{Limits, Link, Phy, TrainError, Trained, Trainer, pick_link, pick_rgb_bpc};
use crate::regs::{self, Words};
use crate::stream;

/// What the board and the user allow.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// `cix,dp-max-rate`: the Orion O6 and Orange Pi 6 Plus say HBR2.
    pub max_rate: Rate,
    pub max_lanes: u8,
    /// The most bits per colour component to use (8 to 10).
    pub max_bpc: u32,
    /// Whether the sync polarities are inverted for the core (section
    /// 2.6.1; confirm on a sink that honours them).
    pub invert_sync: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_rate: Rate::Hbr2,
            max_lanes: 4,
            max_bpc: 8,
            invert_sync: true,
        }
    }
}

/// Why a link or stream could not be brought up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Aux(AuxError),
    Caps(CapsError),
    Train(TrainError),
    /// No sink is connected.
    NoSink,
    /// The mode does not fit the link at any depth.
    Bandwidth,
}

impl From<AuxError> for Error {
    fn from(e: AuxError) -> Self {
        Self::Aux(e)
    }
}

/// What a hot-plug interrupt reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Events {
    /// The line changed: plug or unplug.
    pub hpd: bool,
    /// The sink asks for attention (a short pulse).
    pub attention: bool,
}

/// A link that is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Up {
    pub trained: Trained,
    pub bpc: u32,
}

/// The transmitter.
pub struct Dptx {
    w: Arc<dyn Words>,
    d: Arc<dyn Delay>,
    phy: Arc<dyn Phy>,
    cfg: Config,
    caps: Option<Caps>,
    up: Option<Up>,
}

impl core::fmt::Debug for Dptx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dptx")
            .field("caps", &self.caps)
            .field("up", &self.up)
            .finish_non_exhaustive()
    }
}

impl Dptx {
    #[must_use]
    pub fn new(w: Arc<dyn Words>, d: Arc<dyn Delay>, phy: Arc<dyn Phy>, cfg: Config) -> Self {
        Self {
            w,
            d,
            phy,
            cfg,
            caps: None,
            up: None,
        }
    }

    fn aux(&self) -> Aux<'_> {
        Aux::new(&*self.w, &*self.d)
    }

    /// Host initialisation (section 2.1 steps 6 to 8): the transmitter on,
    /// the interrupt mask, and the AUX time base from the register
    /// clock's rate in MHz (25 if unknown; 10 to 400).
    pub fn host_init(&mut self, apb_mhz: u32) {
        self.w.write(regs::TRANSMITTER_ENABLE, 1);
        self.w.write(regs::INTERRUPT_MASK, regs::IRQ_MASK_OPERATING);
        let mhz = if apb_mhz == 0 { 25 } else { apb_mhz };
        self.w.write(regs::AUX_CLOCK_DIVIDER, mhz.clamp(10, 400));
    }

    /// Host shutdown: the transmitter off, everything masked.
    pub fn host_exit(&mut self) {
        self.link_down();
        self.w.write(regs::TRANSMITTER_ENABLE, 0);
        self.w.write(regs::INTERRUPT_MASK, regs::IRQ_MASK_ALL);
        self.caps = None;
    }

    /// The hot-plug line is high.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.aux().hpd()
    }

    /// Reads the interrupt (once: the cause is cleared by reading it) and
    /// says what it was; `None` if it was not this core's (section 2.2.1).
    pub fn interrupt(&mut self) -> Option<Events> {
        let cause = self.w.read(regs::INTERRUPT_CAUSE);
        let mask = self.w.read(regs::INTERRUPT_MASK);
        let live = cause & !mask;
        if live == 0 {
            return None;
        }
        Some(Events {
            hpd: live & regs::IRQ_HPD_EVENT != 0,
            attention: live & regs::IRQ_HPD_IRQ != 0,
        })
    }

    /// Reads the sink's capabilities (section 2.4), five times at most
    /// for a sink that wakes slowly.
    ///
    /// # Errors
    ///
    /// [`Error`] when no usable block is read.
    pub fn probe(&mut self) -> Result<Caps, Error> {
        if !self.connected() {
            self.caps = None;
            return Err(Error::NoSink);
        }
        let mut last = Error::NoSink;
        for _ in 0..5 {
            let mut b = [0u8; 16];
            match self.aux().read(dpcd::REV, &mut b) {
                Ok(()) => {
                    let c = Caps::parse(&b).map_err(Error::Caps)?;
                    self.caps = Some(c);
                    return Ok(c);
                }
                Err(AuxError::NoSink) => return Err(Error::NoSink),
                Err(e) => last = Error::Aux(e),
            }
            self.d.delay_us(100_000);
        }
        Err(last)
    }

    /// The sink's EDID, as many blocks as it says (at most 4).
    ///
    /// # Errors
    ///
    /// [`Error::Aux`].
    pub fn edid(&self) -> Result<Vec<u8>, Error> {
        let first = self.aux().read_edid_block(0)?;
        let blocks = usize::from(first[126]).min(3) + 1;
        let mut out = first.to_vec();
        for i in 1..blocks {
            out.extend_from_slice(&self.aux().read_edid_block(u8::try_from(i).unwrap_or(0))?);
        }
        Ok(out)
    }

    /// Brings the link up for `mode` (already adjusted): the sink to D0,
    /// a rate and lane count, training, and the sources enabled (section
    /// 2.5.9). RGB.
    ///
    /// # Errors
    ///
    /// [`Error`].
    pub fn link_up(&mut self, mode: &Mode, sink_bpc: u32) -> Result<Up, Error> {
        let caps = match self.caps {
            Some(c) => c,
            None => self.probe()?,
        };
        let lim = Limits::of(
            &caps,
            self.cfg.max_rate,
            self.cfg.max_lanes,
            self.phy.lanes(),
        );
        let bpc = pick_rgb_bpc(mode.clock_khz, sink_bpc, self.cfg.max_bpc, lim)
            .ok_or(Error::Bandwidth)?;
        let bpp_x2 = 6 * bpc;
        let first = pick_link(mode.clock_khz, bpp_x2, lim).ok_or(Error::Bandwidth)?;
        // Video off, the sink awake.
        self.w.write(regs::source(0) + regs::VIDEO_STREAM_ENABLE, 0);
        self.sink_power(1)?;
        let t = Trainer {
            w: &*self.w,
            d: &*self.d,
            phy: &*self.phy,
            caps,
            port_lanes: self.phy.lanes().min(4),
        };
        let trained = t
            .train_with_fallback(mode.clock_khz, bpp_x2, lim, first)
            .map_err(Error::Train)?;
        stream::enable_sources(&*self.w, 1);
        let up = Up { trained, bpc };
        self.up = Some(up);
        Ok(up)
    }

    /// Sets the sink's power state: 1 awake, 2 asleep (section 2.5.8).
    fn sink_power(&self, state: u8) -> Result<(), Error> {
        let aux = self.aux();
        let v = aux.read_byte(dpcd::SET_POWER)?;
        aux.write(dpcd::SET_POWER, &[(v & !7) | state])?;
        // The sink wakes in a few milliseconds.
        self.d.delay_us(if state == 1 { 4000 } else { 200 });
        Ok(())
    }

    /// Starts the stream of source 0 on the link that is up.
    ///
    /// # Errors
    ///
    /// [`Error::NoSink`] without a link.
    pub fn stream_on(&mut self, mode: &Mode) -> Result<(), Error> {
        let up = self.up.ok_or(Error::NoSink)?;
        let m = if self.cfg.invert_sync {
            stream::with_inverted_sync(mode)
        } else {
            *mode
        };
        let ppc = stream::pixels_per_clock(m.clock_khz);
        let r = stream::stream_regs(&m, up.bpc, up.trained.link, ppc);
        let pol = r
            .values
            .iter()
            .find(|v| v.0 == regs::MAIN_STREAM_POLARITY)
            .map_or(0, |v| v.1);
        stream::start(&*self.w, 0, &r, pol);
        Ok(())
    }

    /// Stops the stream.
    pub fn stream_off(&mut self) {
        stream::stop(&*self.w, 0);
    }

    /// Takes the link down (section 2.11.2): the stream, the sink to D3 if
    /// it is still there, the sources.
    pub fn link_down(&mut self) {
        if self.up.take().is_none() {
            return;
        }
        stream::stop(&*self.w, 0);
        if self.connected() {
            let _ = self.sink_power(2);
        }
        stream::disable_sources(&*self.w);
    }

    /// The link now, if it is up.
    #[must_use]
    pub fn link(&self) -> Option<Link> {
        self.up.map(|u| u.trained.link)
    }

    /// What was last read of the sink.
    #[must_use]
    pub fn caps(&self) -> Option<Caps> {
        self.caps
    }
}
