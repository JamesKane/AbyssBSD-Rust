// SPDX-License-Identifier: BSD-2-Clause
//! CTA-861 extension blocks (extension tag `0x02`): the video data block's
//! standard video codes, and the detailed timings after the data blocks.
//! A monitor's 1920 by 1080 and its lower and higher rates are listed here,
//! not in the base block, which holds only its preferred timing.
//!
//! The block is untrusted input from the monitor: every index is checked.

use alloc::vec::Vec;

use abi::drm::{
    DRM_MODE_FLAG_NHSYNC, DRM_MODE_FLAG_NVSYNC, DRM_MODE_FLAG_PHSYNC, DRM_MODE_FLAG_PVSYNC,
    DRM_MODE_TYPE_DRIVER,
};
use kms_check::Mode;

use crate::{BLOCK, detailed};

/// The extension tag of a CTA-861 block.
pub(crate) const TAG: u8 = 0x02;

/// One standard video timing (the progressive ones that matter here).
struct Vic {
    code: u8,
    /// Active, front porch, sync width, total: horizontal then vertical.
    h: [u16; 4],
    v: [u16; 4],
    clock_khz: u32,
    /// The refresh rate the timings are meant to give, in hertz (the
    /// 59.94-style rates are given as their integer rate and clock). Only
    /// the table's test reads it: it checks the numbers written out.
    #[allow(dead_code)]
    hz: u32,
    /// Both syncs positive (the high-definition ones) or both negative.
    positive: bool,
}

const fn vic(code: u8, h: [u16; 4], v: [u16; 4], clock_khz: u32, hz: u32, positive: bool) -> Vic {
    Vic {
        code,
        h,
        v,
        clock_khz,
        hz,
        positive,
    }
}

/// The video identification codes this reads: 480p, 576p, 720p, 1080p and
/// 2160p, in the rates monitors list them.
#[rustfmt::skip]
const VICS: &[Vic] = &[
    vic(1,  [640, 16, 96, 800],    [480, 10, 2, 525],   25_200, 60, false),
    vic(2,  [720, 16, 62, 858],    [480, 9, 6, 525],    27_000, 60, false),
    vic(3,  [720, 16, 62, 858],    [480, 9, 6, 525],    27_000, 60, false),
    vic(4,  [1280, 110, 40, 1650], [720, 5, 5, 750],    74_250, 60, true),
    vic(16, [1920, 88, 44, 2200],  [1080, 4, 5, 1125],  148_500, 60, true),
    vic(17, [720, 12, 64, 864],    [576, 5, 5, 625],    27_000, 50, false),
    vic(18, [720, 12, 64, 864],    [576, 5, 5, 625],    27_000, 50, false),
    vic(19, [1280, 440, 40, 1980], [720, 5, 5, 750],    74_250, 50, true),
    vic(31, [1920, 528, 44, 2640], [1080, 4, 5, 1125],  148_500, 50, true),
    vic(32, [1920, 638, 44, 2750], [1080, 4, 5, 1125],  74_250, 24, true),
    vic(33, [1920, 528, 44, 2640], [1080, 4, 5, 1125],  74_250, 25, true),
    vic(34, [1920, 88, 44, 2200],  [1080, 4, 5, 1125],  74_250, 30, true),
    vic(41, [1280, 440, 40, 1980], [720, 5, 5, 750],    148_500, 100, true),
    vic(47, [1280, 110, 40, 1650], [720, 5, 5, 750],    148_500, 120, true),
    vic(60, [1280, 1760, 40, 3300], [720, 5, 5, 750],   59_400, 24, true),
    vic(61, [1280, 2420, 40, 3960], [720, 5, 5, 750],   74_250, 25, true),
    vic(62, [1280, 1760, 40, 3300], [720, 5, 5, 750],   74_250, 30, true),
    vic(63, [1920, 88, 44, 2200],  [1080, 4, 5, 1125],  297_000, 120, true),
    vic(64, [1920, 528, 44, 2640], [1080, 4, 5, 1125],  297_000, 100, true),
    vic(93, [3840, 1276, 88, 5500], [2160, 8, 10, 2250], 297_000, 24, true),
    vic(94, [3840, 1056, 88, 5280], [2160, 8, 10, 2250], 297_000, 25, true),
    vic(95, [3840, 176, 88, 4400], [2160, 8, 10, 2250], 297_000, 30, true),
    vic(96, [3840, 1056, 88, 5280], [2160, 8, 10, 2250], 594_000, 50, true),
    vic(97, [3840, 176, 88, 4400], [2160, 8, 10, 2250], 594_000, 60, true),
];

/// The mode of video identification code `code`, if this reads it.
fn mode_of(code: u8) -> Option<Mode> {
    let t = VICS.iter().find(|t| t.code == code)?;
    let sync = if t.positive {
        DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC
    } else {
        DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC
    };
    let [ha, hfp, hsw, ht] = t.h;
    let [va, vfp, vsw, vt] = t.v;
    Some(Mode {
        clock_khz: t.clock_khz,
        hdisplay: ha,
        hsync_start: ha + hfp,
        hsync_end: ha + hfp + hsw,
        htotal: ht,
        vdisplay: va,
        vsync_start: va + vfp,
        vsync_end: va + vfp + vsw,
        vtotal: vt,
        flags: sync,
        kind: DRM_MODE_TYPE_DRIVER,
        ..Mode::default()
    })
}

/// The modes one CTA-861 block gives: the video data block's codes, then
/// the detailed timings after the data blocks. Empty for a block that is not
/// CTA-861, does not sum to zero, or is malformed.
pub(crate) fn modes(b: &[u8]) -> Vec<Mode> {
    let mut out = Vec::new();
    let Some(b) = b.get(..BLOCK) else {
        return out;
    };
    if b[0] != TAG || b.iter().fold(0u8, |a, x| a.wrapping_add(*x)) != 0 {
        return out;
    }
    let (revision, dtd) = (b[1], usize::from(b[2]));
    // Offset 0: no detailed timings and no data blocks. Otherwise the
    // detailed timings start at `dtd`, after the data blocks (revision 3).
    if !(4..BLOCK).contains(&dtd) {
        return out;
    }
    if revision >= 3 {
        let mut i = 4;
        while i < dtd {
            let (tag, len) = (b[i] >> 5, usize::from(b[i] & 0x1F));
            let Some(payload) = b.get(i + 1..i + 1 + len).filter(|_| i + 1 + len <= dtd) else {
                break;
            };
            // Tag 2 is the video data block: one code a byte, the high
            // bit marking a native one for codes 1 to 64.
            if tag == 2 {
                for &s in payload {
                    let code = if (129..=192).contains(&s) {
                        s & 0x7F
                    } else {
                        s
                    };
                    if let Some(m) = mode_of(code) {
                        out.push(m);
                    }
                }
            }
            i += 1 + len;
        }
    }
    // The detailed timings, 18 bytes each, up to the checksum byte.
    let mut at = dtd;
    while at + 18 < BLOCK {
        let d = &b[at..at + 18];
        let clock = u32::from(u16::from_le_bytes([d[0], d[1]]));
        if clock == 0 {
            break;
        }
        if let Some(m) = detailed(d, clock) {
            out.push(m);
        }
        at += 18;
    }
    out
}

/// Audio format codes run 1 (linear PCM) to 14, so descriptors are indexed
/// by the code, with 0 unused.
pub const FORMATS: usize = 15;

/// What a CTA-861 block says about the sink's audio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Audio {
    /// The short audio descriptors (three bytes each) by format code; all
    /// zero for a format the sink does not list.
    pub descriptors: [[u8; 3]; FORMATS],
    /// The speaker allocation data block's first byte (0 if there is none).
    pub speakers: u8,
    /// Whether the block says the sink supports basic audio (bit 6 of its
    /// byte 3).
    pub basic: bool,
}

/// The audio data of one CTA-861 block: its audio data blocks (tag 1) and
/// speaker allocation block (tag 4). `None` for a block that is not
/// CTA-861 or does not sum to zero; blocks running off the end stop the
/// walk.
pub(crate) fn audio(b: &[u8]) -> Option<Audio> {
    let b = b.get(..BLOCK)?;
    if b[0] != TAG || b.iter().fold(0u8, |a, x| a.wrapping_add(*x)) != 0 {
        return None;
    }
    let mut out = Audio {
        basic: b[3] & 0x40 != 0,
        ..Audio::default()
    };
    let dtd = usize::from(b[2]);
    if b[1] < 3 || !(4..BLOCK).contains(&dtd) {
        return Some(out);
    }
    let mut i = 4;
    while i < dtd {
        let (tag, len) = (b[i] >> 5, usize::from(b[i] & 0x1F));
        let Some(payload) = b.get(i + 1..i + 1 + len).filter(|_| i + 1 + len <= dtd) else {
            break;
        };
        match tag {
            1 => {
                for s in payload.as_chunks::<3>().0 {
                    let format = usize::from((s[0] >> 3) & 0xF);
                    if format != 0 && format < FORMATS {
                        out.descriptors[format] = [s[0], s[1], s[2]];
                    }
                }
            }
            4 if len >= 3 => out.speakers = payload[0],
            _ => {}
        }
        i += 1 + len;
    }
    Some(out)
}

/// The refresh rate of `m` in hertz, rounded.
pub(crate) fn refresh_hz(m: &Mode) -> u32 {
    let total = u64::from(m.htotal) * u64::from(m.vtotal);
    if total == 0 {
        return 0;
    }
    u32::try_from((u64::from(m.clock_khz) * 1000 + total / 2) / total).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers of each table entry give the rate it is meant for: a
    /// check on the table, which was written out by hand.
    #[test]
    fn every_timing_gives_its_refresh_rate() {
        for t in VICS {
            let m = mode_of(t.code).unwrap();
            let exact =
                f64::from(m.clock_khz) * 1000.0 / (f64::from(m.htotal) * f64::from(m.vtotal));
            let want = f64::from(t.hz);
            assert!(
                (exact - want).abs() / want < 0.001,
                "VIC {}: {exact} Hz, wanted {want}",
                t.code
            );
            // Syncs inside the blanking, in order.
            assert!(m.hdisplay < m.hsync_start && m.hsync_start < m.hsync_end);
            assert!(m.hsync_end <= m.htotal);
            assert!(m.vdisplay < m.vsync_start && m.vsync_start < m.vsync_end);
            assert!(m.vsync_end <= m.vtotal);
        }
        // No code twice.
        for (i, a) in VICS.iter().enumerate() {
            assert!(VICS[i + 1..].iter().all(|b| b.code != a.code));
        }
    }
}
