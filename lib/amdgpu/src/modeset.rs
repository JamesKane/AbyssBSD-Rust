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
/// `HUBPREQ0_BLANK_OFFSET_0`: h-blank end in reference cycles in 12:0, v-blank
/// end in lines in 30:16.
pub const BLANK_OFFSET_0: usize = 0xec10;
/// `HUBPREQ0_BLANK_OFFSET_1`: the minimum next start line in quarter lines.
pub const BLANK_OFFSET_1: usize = 0xec14;
/// `HUBPREQ0_REF_FREQ_TO_PIX_FREQ`: the reference clock over the pixel clock.
pub const REF_FREQ_TO_PIX_FREQ: usize = 0xec74;
/// `OTG0_OTG_VSTARTUP_PARAM`: the start-up line in 9:0.
pub const VSTARTUP_PARAM: usize = 0x1411c;
/// `HUBP0_DCSURF_PRI_VIEWPORT_DIMENSION`: width in 13:0, height in 29:16.
pub const VIEWPORT_DIMENSION: usize = 0xeaa8;
/// `DSCL0_RECOUT_SIZE`: width in 13:0, height in 29:16.
pub const RECOUT_SIZE: usize = 0x10748;
/// `DSCL0_MPC_SIZE`: width in 13:0, height in 29:16.
pub const MPC_SIZE: usize = 0x1074c;
/// `DSCL0_OTG_H_BLANK`: the timing generator's horizontal blank start and end.
pub const DSCL_H_BLANK: usize = 0x1073c;
/// `DSCL0_OTG_V_BLANK`.
pub const DSCL_V_BLANK: usize = 0x10740;
/// `DPG0_DPG_DIMENSIONS`: height in 15:0, width in 31:16.
pub const DPG_DIMENSIONS: usize = 0x13458;
/// The display hub's reference clock, in kHz (24 MHz).
pub const REFERENCE_KHZ: u64 = 24_000;

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
    pub writes: [Option<(&'static str, usize, u32, u32)>; 24],
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
    let mut writes = [None; 24];
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

/// The plan for a mode at another pixel clock and size (08 section 3.7.2 and
/// 3.7.3): the timing registers that differ; the closed-form request fields
/// by the reference's rules (with r = 24 MHz / pixel clock): the reference
/// to pixel frequency ratio r x 2^19, the reference cycles per line
/// r x htotal x 2^8, the horizontal blank end in reference cycles and the
/// vertical blank end in lines, the minimum next start line (vertical blank
/// start x 4); the vertical update offset (a quarter of the total); the start-up line
/// kept if the new vertical blanking allows it; and every register that holds
/// the active size. The model-only fields (prefetch, the vertical blank and
/// nominal parameters, the per-line delivery, the time-to-urgency values) are
/// left as the firmware set them.
///
/// The caller changes the pixel clock itself; `mode.clock_khz` is the clock
/// the plan is for.
///
/// # Errors
///
/// [`Error::Timing`] for a mode with no timing.
pub fn plan_retimed<W: Window + ?Sized>(w: &W, mode: &Mode) -> Result<Plan, Error> {
    let new = Timing::from_mode(mode).map_err(Error::Timing)?;
    let old = Timing::read(w);
    let (hactive, vactive, htotal, _) = new.size();
    let mut writes = [None; 24];
    let mut n = 0;
    let mut put = |name: &'static str, at: usize, old: u32, new: u32| {
        if old != new && n < writes.len() {
            writes[n] = Some((name, at, old, new));
            n += 1;
        }
    };
    for (i, (name, at)) in timing::REGISTERS.iter().enumerate() {
        put(name, *at, old.0[i], new.0[i]);
    }
    let clock = u64::from(mode.clock_khz.max(1));
    let (h_blank_end, v_blank_end) = ((new.0[1] >> 16) & 0x7fff, (new.0[5] >> 16) & 0x7fff);
    let v_blank_start = new.0[5] & 0x7fff;
    let field = |old: u32, mask: u32, shift: u32, value: u64| -> u32 {
        let value = u32::try_from(value).unwrap_or(0) & (mask >> shift);
        (old & !mask) | (value << shift)
    };
    let reg = |at: usize| w.read32(at);
    let (r_ref, r_ln) = (
        REFERENCE_KHZ * (1 << 19) / clock,
        REFERENCE_KHZ * u64::from(htotal) * 256 / clock,
    );
    put(
        "HUBPREQ_REF_FREQ_TO_PIX_FREQ",
        REF_FREQ_TO_PIX_FREQ,
        reg(REF_FREQ_TO_PIX_FREQ),
        field(reg(REF_FREQ_TO_PIX_FREQ), 0x1f_ffff, 0, r_ref),
    );
    put(
        "HUBPREQ_DST_DIMENSIONS",
        DST_DIMENSIONS,
        reg(DST_DIMENSIONS),
        field(reg(DST_DIMENSIONS), 0x1f_ffff, 0, r_ln),
    );
    let blank0 = field(
        field(
            reg(BLANK_OFFSET_0),
            0x1fff,
            0,
            REFERENCE_KHZ * u64::from(h_blank_end) / clock,
        ),
        0x7fff_0000,
        16,
        u64::from(v_blank_end),
    );
    put(
        "HUBPREQ_BLANK_OFFSET_0",
        BLANK_OFFSET_0,
        reg(BLANK_OFFSET_0),
        blank0,
    );
    put(
        "HUBPREQ_BLANK_OFFSET_1",
        BLANK_OFFSET_1,
        reg(BLANK_OFFSET_1),
        field(
            reg(BLANK_OFFSET_1),
            0x3_ffff,
            0,
            u64::from(v_blank_start) * 4,
        ),
    );
    let vupdate = reg(VUPDATE_PARAM);
    put(
        "OTG_VUPDATE_PARAM",
        VUPDATE_PARAM,
        vupdate,
        field(vupdate, 0xffff, 0, u64::from(htotal.div_ceil(4))),
    );
    // The start-up line may not pass the vertical blanking less one.
    let startup = reg(VSTARTUP_PARAM);
    let bound = u32::from(mode.vtotal - mode.vdisplay).saturating_sub(1);
    if startup & 0x3ff > bound {
        put(
            "OTG_VSTARTUP_PARAM",
            VSTARTUP_PARAM,
            startup,
            (startup & !0x3ff) | bound,
        );
    }
    // Everything that holds the active size.
    let size = hactive | (vactive << 16);
    for (name, at) in [
        ("HUBP_PRI_VIEWPORT_DIMENSION", VIEWPORT_DIMENSION),
        ("DSCL_RECOUT_SIZE", RECOUT_SIZE),
        ("DSCL_MPC_SIZE", MPC_SIZE),
    ] {
        put(name, at, reg(at), size);
    }
    put(
        "DSCL_OTG_H_BLANK",
        DSCL_H_BLANK,
        reg(DSCL_H_BLANK),
        new.0[1],
    );
    put(
        "DSCL_OTG_V_BLANK",
        DSCL_V_BLANK,
        reg(DSCL_V_BLANK),
        new.0[5],
    );
    put(
        "DPG_DIMENSIONS",
        DPG_DIMENSIONS,
        reg(DPG_DIMENSIONS),
        vactive | (hactive << 16),
    );
    Ok(Plan { writes })
}

/// The registers a plan may write, with their names: what [`snapshot`] keeps
/// so the firmware's own values can be put back exactly.
pub const BASELINE: [(&str, usize); 22] = [
    ("OTG_H_TOTAL", timing::H_TOTAL),
    ("OTG_H_BLANK_START_END", timing::H_BLANK_START_END),
    ("OTG_H_SYNC_A", timing::H_SYNC_A),
    ("OTG_H_SYNC_A_CNTL", timing::H_SYNC_A_CNTL),
    ("OTG_V_TOTAL", timing::V_TOTAL),
    ("OTG_V_BLANK_START_END", timing::V_BLANK_START_END),
    ("OTG_V_SYNC_A", timing::V_SYNC_A),
    ("OTG_V_SYNC_A_CNTL", timing::V_SYNC_A_CNTL),
    ("HUBPREQ_REF_FREQ_TO_PIX_FREQ", REF_FREQ_TO_PIX_FREQ),
    ("HUBPREQ_DST_DIMENSIONS", DST_DIMENSIONS),
    ("HUBPREQ_BLANK_OFFSET_0", BLANK_OFFSET_0),
    ("HUBPREQ_BLANK_OFFSET_1", BLANK_OFFSET_1),
    ("OTG_VUPDATE_PARAM", VUPDATE_PARAM),
    ("OTG_VSTARTUP_PARAM", VSTARTUP_PARAM),
    ("HUBP_PRI_VIEWPORT_DIMENSION", VIEWPORT_DIMENSION),
    ("DSCL_RECOUT_SIZE", RECOUT_SIZE),
    ("DSCL_MPC_SIZE", MPC_SIZE),
    ("DSCL_OTG_H_BLANK", DSCL_H_BLANK),
    ("DSCL_OTG_V_BLANK", DSCL_V_BLANK),
    ("DPG_DIMENSIONS", DPG_DIMENSIONS),
    ("PIXEL_RATE_CNTL", 0x500),
    ("PHYPLLA_PIXCLK_RESYNC_CNTL", 0x400),
];

/// The values of [`BASELINE`], as the firmware left them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Baseline(pub [u32; 22]);

/// Reads the registers a plan may write.
pub fn snapshot<W: Window + ?Sized>(w: &W) -> Baseline {
    Baseline(BASELINE.map(|(_, at)| w.read32(at)))
}

/// The plan that puts the registers a plan may write back to `baseline`.
/// The last two ([`BASELINE`]: the pixel rate and the pixel clock's resync
/// control) are only read: a plan never writes them.
#[must_use]
pub fn plan_restore<W: Window + ?Sized>(w: &W, baseline: &Baseline) -> Plan {
    let mut writes = [None; 24];
    let mut n = 0;
    for (i, (name, at)) in BASELINE.iter().take(20).enumerate() {
        let now = w.read32(*at);
        if now != baseline.0[i] {
            writes[n] = Some((*name, *at, now, baseline.0[i]));
            n += 1;
        }
    }
    Plan { writes }
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
                (REF_FREQ_TO_PIX_FREQ, 0x14e5e),
                (BLANK_OFFSET_0, 0x0029_0040),
                (BLANK_OFFSET_1, 0x1136),
                (VSTARTUP_PARAM, 0xd),
                (VIEWPORT_DIMENSION, 0x0438_0780),
                (RECOUT_SIZE, 0x0438_0780),
                (MPC_SIZE, 0x0438_0780),
                (DSCL_H_BLANK, 0x00c0_0840),
                (DSCL_V_BLANK, 0x0029_0461),
                (DPG_DIMENSIONS, 0x0780_0438),
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

    fn cta(code: u8) -> Mode {
        edid::mode_for_vic(code).unwrap()
    }

    /// The reference's rule values for each mode (08 section 3.7.2's table).
    #[test]
    fn the_formula_fields_are_the_specifications_for_every_mode() {
        let p = Pipe::new();
        // (code, REF_FREQ, DST_DIMENSIONS, BLANK_OFFSET_0, BLANK_OFFSET_1, VUPDATE offset)
        for (vic, rf, dst, b0, b1, vu) in [
            (
                16u8,
                0x14afdu32,
                0x1638eu32,
                0x0029_001fu32,
                0x1184u32,
                550u32,
            ),
            (31, 0x14afd, 0x1aaaa, 0x0029_001f, 0x1184, 660),
            (4, 0x295fa, 0x21555, 0x0019_0054, 0xba4, 413),
            (19, 0x295fa, 0x28000, 0x0019_0054, 0xba4, 495),
            (3, 0x71c71, 0x2faaa, 0x0024_006c, 0x810, 215),
            (1, 0x7a069, 0x2faa9, 0x0023_0089, 0x80c, 200),
            (18, 0x71c71, 0x30000, 0x002c_0075, 0x9b0, 216),
        ] {
            let mut m = cta(vic);
            if vic == 1 {
                // The specification's clock is 25.175 MHz (the shared table
                // rounds this mode to 25.2).
                m.clock_khz = 25_175;
            }
            let pl = plan_retimed(&p, &m).unwrap();
            let get = |at: usize| {
                pl.writes
                    .iter()
                    .flatten()
                    .find(|c| c.1 == at)
                    .map_or_else(|| p.get(at), |c| c.3)
            };
            assert_eq!(get(REF_FREQ_TO_PIX_FREQ), rf, "VIC {vic}");
            assert_eq!(get(DST_DIMENSIONS) & 0x1f_ffff, dst, "VIC {vic}");
            assert_eq!(get(BLANK_OFFSET_0), b0, "VIC {vic}");
            assert_eq!(get(BLANK_OFFSET_1), b1, "VIC {vic}");
            assert_eq!(get(VUPDATE_PARAM) & 0xffff, vu, "VIC {vic}");
        }
    }

    #[test]
    fn a_720p_plan_changes_every_size_holder() {
        let p = Pipe::new();
        let pl = plan_retimed(&p, &cta(4)).unwrap();
        let get = |at: usize| pl.writes.iter().flatten().find(|c| c.1 == at).map(|c| c.3);
        let size = 1280 | (720 << 16);
        assert_eq!(get(VIEWPORT_DIMENSION), Some(size));
        assert_eq!(get(RECOUT_SIZE), Some(size));
        assert_eq!(get(MPC_SIZE), Some(size));
        assert_eq!(get(DPG_DIMENSIONS), Some(720 | (1280 << 16)));
        // The blank registers the scaler mirrors are the generator's.
        assert_eq!(get(DSCL_H_BLANK), get(timing::H_BLANK_START_END));
        assert_eq!(get(DSCL_V_BLANK), get(timing::V_BLANK_START_END));
        // 720p: 750 total, 720 active: the firmware's start-up line (13)
        // is under the bound (29), so it stays.
        assert_eq!(get(VSTARTUP_PARAM), None);
        // Timing as the specification's table.
        assert_eq!(get(timing::H_TOTAL), Some(0x671));
        assert_eq!(get(timing::H_BLANK_START_END), Some(0x0104_0604));
    }

    #[test]
    fn restoring_the_baseline_puts_every_register_back_after_a_retimed_mode() {
        let p = Pipe::new();
        let base = snapshot(&p);
        let go = plan_retimed(&p, &cta(4)).unwrap();
        apply(&p, &go);
        assert_ne!(snapshot(&p), base);
        let back = plan_restore(&p, &base);
        assert!(back.changes().count() > 10);
        apply(&p, &back);
        assert_eq!(snapshot(&p), base);
        // And a restore with nothing changed writes nothing.
        assert_eq!(plan_restore(&p, &base).changes().count(), 0);
    }

    #[test]
    fn a_start_up_line_past_the_blanking_is_pulled_in() {
        let p = Pipe::new();
        p.regs.borrow_mut().insert(VSTARTUP_PARAM, 40);
        // 480p: 525 total, 480 active: bound 44; 576p: 625-576-1 = 48; use
        // a made-up short blanking instead.
        let mut m = cta(4);
        m.vdisplay = 740;
        m.vsync_start = 743;
        m.vsync_end = 746;
        let pl = plan_retimed(&p, &m).unwrap();
        let c = pl.writes.iter().flatten().find(|c| c.1 == VSTARTUP_PARAM);
        assert_eq!(c.map(|c| c.3), Some(9));
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
