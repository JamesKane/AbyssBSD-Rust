// SPDX-License-Identifier: BSD-2-Clause
//! Changing the mode on the pipe the firmware left running (08 section 3.7):
//! one output (HDMI on DIG0), one full-screen linear plane, the pixel clock
//! unchanged for now.
//!
//! The steps are separate so the caller logs between them: [`preflight`]
//! (checks the live registers are as the specification says, writing
//! nothing), [`plan`] (the register writes for the mode), [`stop`], the
//! plan's writes, [`start`].

use kms_check::Mode;

use crate::regs::Window;
use crate::timing::{self, Timing};

/// `HUBP0_DCHUBP_CNTL`: bit 0 `HUBP_BLANK_EN`.
pub const DCHUBP_CNTL: usize = 0xeacc;
/// `ODM0_OPTC_DATA_SOURCE_SELECT`: the four segments' sources, four bits
/// each from bit 16 (segment 0 in 19:16), 0xf for none.
pub const ODM_DATA_SOURCE: usize = 0x13e2c;
/// `ODM0_OPTC_MEMORY_CONFIG`.
pub const ODM_MEMORY_CONFIG: usize = 0x13e40;
/// `OTG0_OTG_CONTROL`: bit 0 master enable, bits 9:8 disable point.
pub const OTG_CONTROL: usize = 0x14004;
/// `VTG0_CONTROL`: bit 31 enable.
pub const VTG_CONTROL: usize = 0xe7b4;
/// `OTG0_OTG_CLOCK_CONTROL`: bit 16 `OTG_BUSY`.
pub const OTG_CLOCK_CONTROL: usize = 0x14118;
/// `OTG0_OTG_VUPDATE_PARAM`: offset in 15:0, width in 25:16.
pub const VUPDATE_PARAM: usize = 0x14120;
/// `HUBPREQ0_DST_DIMENSIONS`: `REFCYC_PER_HTOTAL` in 20:0.
pub const DST_DIMENSIONS: usize = 0xec18;

/// The pixel clock the firmware's mode runs at, in kHz (08 section 9.0).
pub const FIRMWARE_CLOCK_KHZ: u32 = 148_500;
/// Segment 0 fed by pipe 0, the others unused (the firmware's value).
const ODM_RUNNING: u32 = 0xfff0_0000;
/// All four segments unused.
const ODM_STOPPED: u32 = 0xffff_0000;

/// Why the mode was not changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The live registers are not what the specification says; the register
    /// and its value.
    Unexpected(&'static str, u32),
    /// The mode has no timing (see [`timing::Error`]).
    Timing(timing::Error),
    /// The mode needs a different pixel clock, which this does not change yet.
    ClockChange,
    /// The mode's active size is not the running one.
    SizeChange,
    /// The generator did not stop in time; the last `OTG_CLOCK_CONTROL`.
    StopTimeout(u32),
    /// The generator did not show busy after the start.
    StartTimeout(u32),
}

/// The registers the specification says the steps depend on, read, with what
/// they must be: (name, byte, mask, wanted).
const EXPECTED: [(&str, usize, u32, u32); 4] = [
    ("OTG_CONTROL master enable", OTG_CONTROL, 1, 1),
    ("VTG_CONTROL enable", VTG_CONTROL, 1 << 31, 1 << 31),
    (
        "OTG_CLOCK_CONTROL busy",
        OTG_CLOCK_CONTROL,
        1 << 16,
        1 << 16,
    ),
    ("ODM data source", ODM_DATA_SOURCE, 0xffff_ffff, ODM_RUNNING),
];

/// Checks the pipe is running as 08 section 3.7 describes, writing nothing.
///
/// # Errors
///
/// [`Error::Unexpected`] naming the first register that is not.
pub fn preflight<W: Window + ?Sized>(w: &W) -> Result<(), Error> {
    for (name, at, mask, want) in EXPECTED {
        let v = w.read32(at);
        if v & mask != want {
            return Err(Error::Unexpected(name, v));
        }
    }
    Ok(())
}

/// The register writes that take the running mode to `mode`, in order, with
/// the old value of each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// (name, byte, old value, new value).
    pub writes: [Option<(&'static str, usize, u32, u32)>; 10],
}

impl Plan {
    /// The writes that change something.
    pub fn changes(&self) -> impl Iterator<Item = &(&'static str, usize, u32, u32)> {
        self.writes.iter().flatten().filter(|c| c.2 != c.3)
    }
}

/// The plan for `mode` against the running registers: the timing registers
/// that differ; the update offset a quarter of the total; the delivery
/// field scaled by the line-time ratio (08 section 3.7.2, the policy for a
/// same-clock, same-size mode). `clock_khz` is the pixel clock now running.
///
/// # Errors
///
/// [`Error`] for a mode with no timing, a different pixel clock, or a
/// different active size.
pub fn plan<W: Window + ?Sized>(w: &W, mode: &Mode, clock_khz: u32) -> Result<Plan, Error> {
    let new = Timing::from_mode(mode).map_err(Error::Timing)?;
    if mode.clock_khz != clock_khz {
        return Err(Error::ClockChange);
    }
    let old = Timing::read(w);
    let (oh, ov, ot, _) = old.size();
    let (nh, nv, nt, _) = new.size();
    if (oh, ov) != (nh, nv) {
        return Err(Error::SizeChange);
    }
    let mut writes = [None; 10];
    let mut n = 0;
    for (i, (name, at)) in timing::REGISTERS.iter().enumerate() {
        if new.0[i] != old.0[i] {
            writes[n] = Some((*name, *at, old.0[i], new.0[i]));
            n += 1;
        }
    }
    // The vertical update starts a quarter of a line in.
    let vupdate = w.read32(VUPDATE_PARAM);
    let offset = nt.div_ceil(4);
    if vupdate & 0xffff != offset {
        writes[n] = Some((
            "OTG_VUPDATE_PARAM",
            VUPDATE_PARAM,
            vupdate,
            (vupdate & !0xffff) | offset,
        ));
        n += 1;
    }
    // Reference cycles per line scale with the line time (same clock).
    let dst = w.read32(DST_DIMENSIONS);
    let field = u64::from(dst & 0x1f_ffff);
    let scaled = (field * u64::from(nt) + u64::from(ot) / 2) / u64::from(ot);
    let scaled = u32::try_from(scaled).unwrap_or(0) & 0x1f_ffff;
    if scaled != dst & 0x1f_ffff {
        writes[n] = Some((
            "HUBPREQ_DST_DIMENSIONS",
            DST_DIMENSIONS,
            dst,
            (dst & !0x1f_ffff) | scaled,
        ));
    }
    Ok(Plan { writes })
}

/// Stops the pipe (08 section 3.7.4, step 2): the plane blanked, the output
/// data source off, the memory configuration cleared, the generator and the
/// timing generator off; waits for the generator to go idle (`pause_us` is
/// called with 1000).
///
/// # Errors
///
/// [`Error::StopTimeout`] if it does not stop within 100 ms; the pipe is
/// left stopped but not idle.
pub fn stop<W: Window + ?Sized>(w: &W, pause_us: &dyn Fn(u32)) -> Result<(), Error> {
    w.write32(DCHUBP_CNTL, w.read32(DCHUBP_CNTL) | 1);
    w.write32(ODM_DATA_SOURCE, ODM_STOPPED);
    w.write32(ODM_MEMORY_CONFIG, 0);
    w.write32(OTG_CONTROL, w.read32(OTG_CONTROL) & !1);
    w.write32(VTG_CONTROL, w.read32(VTG_CONTROL) & !(1 << 31));
    let mut last = 0;
    for _ in 0..100 {
        last = w.read32(OTG_CLOCK_CONTROL);
        if last & (1 << 16) == 0 {
            return Ok(());
        }
        pause_us(1000);
    }
    Err(Error::StopTimeout(last))
}

/// Writes a plan's changes.
pub fn apply<W: Window + ?Sized>(w: &W, plan: &Plan) {
    for (_, at, _, new) in plan.changes() {
        w.write32(*at, *new);
    }
}

/// Starts the pipe (08 section 3.7.4, step 5): the data source back on
/// segment 0, the timing generator on (before the generator, as the reference
/// does), the generator on with disable point 2; waits for it to show busy;
/// then unblanks the plane.
///
/// # Errors
///
/// [`Error::StartTimeout`] if the generator is not busy after 100 ms; the
/// plane stays blanked.
pub fn start<W: Window + ?Sized>(w: &W, pause_us: &dyn Fn(u32)) -> Result<(), Error> {
    w.write32(ODM_DATA_SOURCE, ODM_RUNNING);
    w.write32(VTG_CONTROL, w.read32(VTG_CONTROL) | (1 << 31));
    w.write32(OTG_CONTROL, (w.read32(OTG_CONTROL) & !0x300) | (2 << 8) | 1);
    let mut last = 0;
    for _ in 0..100 {
        last = w.read32(OTG_CLOCK_CONTROL);
        if last & (1 << 16) != 0 {
            w.write32(DCHUBP_CNTL, w.read32(DCHUBP_CNTL) & !1);
            return Ok(());
        }
        pause_us(1000);
    }
    Err(Error::StartTimeout(last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::drm::{DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PVSYNC};
    use core::cell::RefCell;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    /// The firmware's registers (tests/corpus/x870e/abyssbsd/raphael-drmn1.txt)
    /// with a generator that goes idle when disabled and busy when enabled.
    struct Pipe {
        regs: RefCell<BTreeMap<usize, u32>>,
        log: RefCell<Vec<(usize, u32)>>,
        /// Whether the generator ever goes idle / busy.
        responds: bool,
    }

    impl Pipe {
        fn new() -> Self {
            let p = Self {
                regs: RefCell::new(BTreeMap::new()),
                log: RefCell::new(Vec::new()),
                responds: true,
            };
            for (at, v) in [
                (timing::H_TOTAL, 0x897),
                (timing::H_BLANK_START_END, 0x00c0_0840),
                (timing::H_SYNC_A, 0x002c_0000),
                (timing::H_SYNC_A_CNTL, 0),
                (timing::V_TOTAL, 0x464),
                (timing::V_BLANK_START_END, 0x0029_0461),
                (timing::V_SYNC_A, 0x0005_0000),
                (timing::V_SYNC_A_CNTL, 0),
                (OTG_CONTROL, 0x0001_0301),
                (VTG_CONTROL, 0x8000_0000),
                (OTG_CLOCK_CONTROL, 0x0001_0000),
                (ODM_DATA_SOURCE, 0xfff0_0000),
                (VUPDATE_PARAM, 0x0140_0226),
                (DST_DIMENSIONS, 0x0001_e641),
                (DCHUBP_CNTL, 0x000f_1002),
            ] {
                p.regs.borrow_mut().insert(at, v);
            }
            p
        }
        fn get(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
    }

    impl Window for Pipe {
        fn read32(&self, o: usize) -> u32 {
            self.get(o)
        }
        fn write32(&self, o: usize, v: u32) {
            self.log.borrow_mut().push((o, v));
            self.regs.borrow_mut().insert(o, v);
            if o == OTG_CONTROL && self.responds {
                let busy = if v & 1 != 0 { 1 << 16 } else { 0 };
                self.regs.borrow_mut().insert(OTG_CLOCK_CONTROL, busy);
            }
        }
    }

    fn mode_1080(htotal: u16, front: u16) -> Mode {
        Mode {
            clock_khz: 148_500,
            hdisplay: 1920,
            hsync_start: 1920 + front,
            hsync_end: 1920 + front + 44,
            htotal,
            vdisplay: 1080,
            vsync_start: 1084,
            vsync_end: 1089,
            vtotal: 1125,
            flags: DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC,
            ..Mode::default()
        }
    }

    #[test]
    fn the_preflight_accepts_the_firmwares_state_and_names_what_is_off() {
        let p = Pipe::new();
        assert_eq!(preflight(&p), Ok(()));
        p.regs.borrow_mut().insert(VTG_CONTROL, 0);
        assert_eq!(
            preflight(&p),
            Err(Error::Unexpected("VTG_CONTROL enable", 0))
        );
        assert!(p.log.borrow().is_empty());
    }

    #[test]
    fn the_same_mode_needs_no_writes() {
        let p = Pipe::new();
        let pl = plan(&p, &mode_1080(2200, 88), FIRMWARE_CLOCK_KHZ).unwrap();
        assert_eq!(pl.changes().count(), 0);
    }

    #[test]
    fn p50_changes_three_registers() {
        let p = Pipe::new();
        let pl = plan(&p, &mode_1080(2640, 528), FIRMWARE_CLOCK_KHZ).unwrap();
        let c: Vec<_> = pl.changes().map(|c| (c.0, c.3)).collect();
        assert_eq!(
            c,
            [
                ("OTG_H_TOTAL", 2639),
                ("OTG_VUPDATE_PARAM", 0x0140_0294),
                // The GOP's value scaled by 2640/2200 (the specification's 0x24781).
                ("HUBPREQ_DST_DIMENSIONS", 0x0002_4781),
            ]
        );
    }

    #[test]
    fn going_back_restores_the_gops_values() {
        let p = Pipe::new();
        let go = plan(&p, &mode_1080(2640, 528), FIRMWARE_CLOCK_KHZ).unwrap();
        apply(&p, &go);
        let back = plan(&p, &mode_1080(2200, 88), FIRMWARE_CLOCK_KHZ).unwrap();
        apply(&p, &back);
        assert_eq!(p.get(timing::H_TOTAL), 0x897);
        assert_eq!(p.get(VUPDATE_PARAM), 0x0140_0226);
        assert_eq!(p.get(DST_DIMENSIONS), 0x0001_e641);
    }

    #[test]
    fn a_different_clock_or_size_is_refused_before_anything_is_written() {
        let p = Pipe::new();
        let mut m = mode_1080(2640, 528);
        m.clock_khz = 74_250;
        assert_eq!(plan(&p, &m, FIRMWARE_CLOCK_KHZ), Err(Error::ClockChange));
        let mut m = mode_1080(2200, 88);
        m.hdisplay = 1280;
        m.hsync_start = 1368;
        m.hsync_end = 1412;
        assert_eq!(plan(&p, &m, FIRMWARE_CLOCK_KHZ), Err(Error::SizeChange));
        assert!(p.log.borrow().is_empty());
    }

    #[test]
    fn stop_blanks_first_and_waits_for_idle_and_start_unblanks_last() {
        let p = Pipe::new();
        stop(&p, &|_| {}).unwrap();
        let order: Vec<usize> = p.log.borrow().iter().map(|(r, _)| *r).collect();
        assert_eq!(
            order,
            [
                DCHUBP_CNTL,
                ODM_DATA_SOURCE,
                ODM_MEMORY_CONFIG,
                OTG_CONTROL,
                VTG_CONTROL
            ]
        );
        assert_eq!(p.get(DCHUBP_CNTL) & 1, 1);
        assert_eq!(p.get(OTG_CONTROL) & 1, 0);
        assert_eq!(p.get(VTG_CONTROL) >> 31, 0);
        p.log.borrow_mut().clear();
        start(&p, &|_| {}).unwrap();
        let order: Vec<usize> = p.log.borrow().iter().map(|(r, _)| *r).collect();
        assert_eq!(
            order,
            [ODM_DATA_SOURCE, VTG_CONTROL, OTG_CONTROL, DCHUBP_CNTL]
        );
        assert_eq!(p.get(ODM_DATA_SOURCE), 0xfff0_0000);
        // Disable point 2, master enable.
        assert_eq!(p.get(OTG_CONTROL) & 0x301, 0x201);
        assert_eq!(p.get(DCHUBP_CNTL) & 1, 0);
        assert_eq!(p.get(DCHUBP_CNTL), 0x000f_1002);
    }

    #[test]
    fn a_generator_that_does_not_idle_or_start_is_an_error_and_the_plane_stays_blanked() {
        let mut p = Pipe::new();
        p.responds = false;
        assert_eq!(stop(&p, &|_| {}), Err(Error::StopTimeout(0x0001_0000)));
        p.regs.borrow_mut().insert(OTG_CLOCK_CONTROL, 0);
        assert_eq!(start(&p, &|_| {}), Err(Error::StartTimeout(0)));
        assert_eq!(p.get(DCHUBP_CNTL) & 1, 1);
    }
}
