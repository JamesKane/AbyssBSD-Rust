// SPDX-License-Identifier: BSD-2-Clause
//! The timing generator's (OTG, "output timing generator") registers for a
//! mode, from the formulas of specification 08 section 2.2, and the reverse:
//! the mode a timing generator is running. Progressive modes only.
//!
//! This builds and reads the register values; it does not start or stop the
//! generator, which is the caller's sequence.

use kms_check::Mode;

use crate::regs::Window;

/// `OTG0_OTG_H_TOTAL`: horizontal total minus 1.
pub const H_TOTAL: usize = 0x13fa8;
/// `OTG0_OTG_H_BLANK_START_END`: start in 14:0, end in 30:16.
pub const H_BLANK_START_END: usize = 0x13fac;
/// `OTG0_OTG_H_SYNC_A`: start in 14:0, end in 30:16.
pub const H_SYNC_A: usize = 0x13fb0;
/// `OTG0_OTG_H_SYNC_A_CNTL`: bit 0 = 1 for negative polarity.
pub const H_SYNC_A_CNTL: usize = 0x13fb4;
/// `OTG0_OTG_V_TOTAL`: vertical total minus 1.
pub const V_TOTAL: usize = 0x13fbc;
/// `OTG0_OTG_V_BLANK_START_END`.
pub const V_BLANK_START_END: usize = 0x13fd8;
/// `OTG0_OTG_V_SYNC_A`.
pub const V_SYNC_A: usize = 0x13fdc;
/// `OTG0_OTG_V_SYNC_A_CNTL`.
pub const V_SYNC_A_CNTL: usize = 0x13fe0;
/// `OTG0_OTG_INTERLACE_CONTROL`: bit 0 interlaced.
pub const INTERLACE_CONTROL: usize = 0x14010;

/// The eight registers that set a progressive timing, in the order the
/// specification lists them, as (name, byte offset).
pub const REGISTERS: [(&str, usize); 8] = [
    ("OTG_H_TOTAL", H_TOTAL),
    ("OTG_H_BLANK_START_END", H_BLANK_START_END),
    ("OTG_H_SYNC_A", H_SYNC_A),
    ("OTG_H_SYNC_A_CNTL", H_SYNC_A_CNTL),
    ("OTG_V_TOTAL", V_TOTAL),
    ("OTG_V_BLANK_START_END", V_BLANK_START_END),
    ("OTG_V_SYNC_A", V_SYNC_A),
    ("OTG_V_SYNC_A_CNTL", V_SYNC_A_CNTL),
];

/// Why a mode has no timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Interlaced or double-scan, which the generator is not set up for here.
    Unsupported,
    /// The sync and total fields are not in increasing order, or a field is
    /// wider than the registers' 15 bits.
    Order,
}

/// The values of the eight timing registers, in the order of [`REGISTERS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing(pub [u32; 8]);

fn field(start: u32, end: u32) -> u32 {
    start | (end << 16)
}

impl Timing {
    /// The registers for `m` (08 section 2.2): the totals less one; sync from
    /// 0 to the sync width; blanking starting at the total less the front
    /// porch and ending one active width earlier; polarity 1 for negative.
    ///
    /// # Errors
    ///
    /// [`Error`] for an interlaced or double-scan mode, an out-of-order mode,
    /// or one too big for the 15-bit fields.
    pub fn from_mode(m: &Mode) -> Result<Self, Error> {
        use abi::drm::{
            DRM_MODE_FLAG_DBLSCAN, DRM_MODE_FLAG_INTERLACE, DRM_MODE_FLAG_NHSYNC,
            DRM_MODE_FLAG_NVSYNC,
        };
        if m.flags & (DRM_MODE_FLAG_INTERLACE | DRM_MODE_FLAG_DBLSCAN) != 0 || m.vscan > 1 {
            return Err(Error::Unsupported);
        }
        let ok = m.hdisplay > 0
            && m.hdisplay < m.hsync_start
            && m.hsync_start < m.hsync_end
            && m.hsync_end <= m.htotal
            && m.vdisplay > 0
            && m.vdisplay < m.vsync_start
            && m.vsync_start < m.vsync_end
            && m.vsync_end <= m.vtotal
            && m.htotal <= 0x7fff
            && m.vtotal <= 0x7fff;
        if !ok {
            return Err(Error::Order);
        }
        let (htotal, hactive) = (u32::from(m.htotal), u32::from(m.hdisplay));
        let hfront = u32::from(m.hsync_start - m.hdisplay);
        let hsync = u32::from(m.hsync_end - m.hsync_start);
        let (vtotal, vactive) = (u32::from(m.vtotal), u32::from(m.vdisplay));
        let vfront = u32::from(m.vsync_start - m.vdisplay);
        let vsync = u32::from(m.vsync_end - m.vsync_start);
        let hblank_start = htotal - hfront;
        let vblank_start = vtotal - vfront;
        Ok(Self([
            htotal - 1,
            field(hblank_start, hblank_start - hactive),
            field(0, hsync),
            u32::from(m.flags & DRM_MODE_FLAG_NHSYNC != 0),
            vtotal - 1,
            field(vblank_start, vblank_start - vactive),
            field(0, vsync),
            u32::from(m.flags & DRM_MODE_FLAG_NVSYNC != 0),
        ]))
    }

    /// The timing the generator has now.
    pub fn read<W: Window + ?Sized>(w: &W) -> Self {
        Self(REGISTERS.map(|(_, at)| w.read32(at)))
    }

    /// Writes the eight registers (no start, no stop, no locking).
    pub fn write<W: Window + ?Sized>(&self, w: &W) {
        for ((_, at), v) in REGISTERS.iter().zip(self.0) {
            w.write32(*at, v);
        }
    }

    /// The registers whose values differ from `other`: (name, byte offset,
    /// this value, the other's).
    #[must_use]
    pub fn differences(&self, other: &Self) -> [Option<(&'static str, usize, u32, u32)>; 8] {
        let mut out = [None; 8];
        for (i, (name, at)) in REGISTERS.iter().enumerate() {
            if self.0[i] != other.0[i] {
                out[i] = Some((*name, *at, self.0[i], other.0[i]));
            }
        }
        out
    }

    /// The mode's active size and totals, read back (08 section 2.2): the
    /// horizontal and vertical active sizes and totals. The pixel clock is
    /// not in these registers.
    #[must_use]
    pub fn size(&self) -> (u32, u32, u32, u32) {
        let hactive = (self.0[1] & 0x7fff) - ((self.0[1] >> 16) & 0x7fff);
        let vactive = (self.0[5] & 0x7fff) - ((self.0[5] >> 16) & 0x7fff);
        (hactive, vactive, self.0[0] + 1, self.0[4] + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::drm::{
        DRM_MODE_FLAG_INTERLACE, DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_NVSYNC, DRM_MODE_FLAG_PHSYNC,
        DRM_MODE_FLAG_PVSYNC,
    };

    fn m1080(clock_khz: u32, htotal: u16) -> Mode {
        Mode {
            clock_khz,
            hdisplay: 1920,
            hsync_start: 1920 + if htotal == 2200 { 88 } else { 528 },
            hsync_end: 1920 + if htotal == 2200 { 88 } else { 528 } + 44,
            htotal,
            vdisplay: 1080,
            vsync_start: 1084,
            vsync_end: 1089,
            vtotal: 1125,
            flags: DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC,
            ..Mode::default()
        }
    }

    /// The registers the GOP left for 1080p60 (tests/corpus/x870e/abyssbsd/
    /// raphael-drmn1.txt).
    const GOP: [u32; 8] = [
        0x0000_0897,
        0x00c0_0840,
        0x002c_0000,
        0,
        0x0000_0464,
        0x0029_0461,
        0x0005_0000,
        0,
    ];

    #[test]
    fn the_formulas_reproduce_the_gops_1080p60_registers() {
        assert_eq!(
            Timing::from_mode(&m1080(148_500, 2200)).unwrap(),
            Timing(GOP)
        );
    }

    #[test]
    fn the_registers_read_back_as_the_mode_size() {
        assert_eq!(Timing(GOP).size(), (1920, 1080, 2200, 1125));
    }

    #[test]
    fn p50_changes_only_the_horizontal_total() {
        // Same clock, 2640 total: the blank start (total less front porch)
        // and end are the same as 1080p60's; only H_TOTAL moves.
        let p50 = Timing::from_mode(&m1080(148_500, 2640)).unwrap();
        let d = p50.differences(&Timing(GOP));
        let changed: std::vec::Vec<_> = d.iter().flatten().collect();
        assert_eq!(changed, [&("OTG_H_TOTAL", H_TOTAL, 2639, 2199)]);
        assert_eq!(p50.size(), (1920, 1080, 2640, 1125));
    }

    #[test]
    fn negative_sync_sets_the_polarity_bits() {
        let mut m = m1080(148_500, 2200);
        m.flags = DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC;
        let t = Timing::from_mode(&m).unwrap();
        assert_eq!((t.0[3], t.0[7]), (1, 1));
    }

    #[test]
    fn a_720p60_timing_has_the_right_blanking() {
        // CTA 720p60: 1280 active, 110 front, 40 sync, 1650 total; 720
        // active, 5 front, 5 sync, 750 total.
        let m = Mode {
            clock_khz: 74_250,
            hdisplay: 1280,
            hsync_start: 1390,
            hsync_end: 1430,
            htotal: 1650,
            vdisplay: 720,
            vsync_start: 725,
            vsync_end: 730,
            vtotal: 750,
            flags: DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC,
            ..Mode::default()
        };
        let t = Timing::from_mode(&m).unwrap();
        assert_eq!(t.0[0], 1649);
        // Blank start 1650 - 110 = 1540, end 260 (= 40 sync + 220 back porch).
        assert_eq!(t.0[1], (260 << 16) | 1540);
        assert_eq!(t.0[2], 40 << 16);
        assert_eq!(t.0[4], 749);
        assert_eq!(t.0[5], (25 << 16) | 745);
        assert_eq!(t.0[6], 5 << 16);
        assert_eq!(t.size(), (1280, 720, 1650, 750));
    }

    #[test]
    fn interlaced_and_out_of_order_modes_are_refused() {
        let mut m = m1080(148_500, 2200);
        m.flags |= DRM_MODE_FLAG_INTERLACE;
        assert_eq!(Timing::from_mode(&m), Err(Error::Unsupported));
        let mut m = m1080(148_500, 2200);
        m.hsync_end = 3000;
        assert_eq!(Timing::from_mode(&m), Err(Error::Order));
        let mut m = m1080(148_500, 2200);
        m.vtotal = 0x9000;
        assert_eq!(Timing::from_mode(&m), Err(Error::Order));
    }
}
