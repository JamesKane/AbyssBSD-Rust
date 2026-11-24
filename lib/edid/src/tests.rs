// SPDX-License-Identifier: BSD-2-Clause
use std::vec::Vec;

use super::*;

/// 1920x1080 at 60 hertz, 509 by 286 mm, digital separate sync, both
/// polarities positive.
const DTD_1080P: [u8; 18] = [
    0x02, 0x3A, 0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C, 0x45, 0x00, 0xFD, 0x1E, 0x11, 0x00,
    0x00, 0x1E,
];

/// 1280x720 at 60 hertz (74.25 MHz).
const DTD_720P: [u8; 18] = [
    0x01, 0x1D, 0x00, 0x72, 0x51, 0xD0, 0x1E, 0x20, 0x6E, 0x28, 0x55, 0x00, 0xC4, 0x8E, 0x21, 0x00,
    0x00, 0x1E,
];

fn name_descriptor(name: &str) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[3] = 0xFC;
    let mut text = [0x20u8; 13];
    for (i, c) in name.bytes().enumerate() {
        text[i] = c;
    }
    if name.len() < 13 {
        text[name.len()] = b'\n';
    }
    d[5..].copy_from_slice(&text);
    d
}

fn block(descriptors: &[[u8; 18]]) -> Vec<u8> {
    let mut b = std::vec![0u8; BLOCK];
    b[..8].copy_from_slice(&HEADER);
    // EDID 1.4, digital 8 bits a component, 51 by 29 cm.
    b[18] = 1;
    b[19] = 4;
    b[20] = 0x80 | 2 << 4;
    b[21] = 51;
    b[22] = 29;
    for (i, d) in descriptors.iter().enumerate() {
        b[54 + 18 * i..72 + 18 * i].copy_from_slice(d);
    }
    let sum = b[..127].iter().fold(0u8, |a, x| a.wrapping_add(*x));
    b[127] = 0u8.wrapping_sub(sum);
    b
}

#[test]
fn the_detailed_timing_becomes_a_mode() {
    let e = parse(&block(&[DTD_1080P])).unwrap();
    assert_eq!(e.modes.len(), 1);
    let m = e.modes[0];
    assert_eq!(m.clock_khz, 148_500);
    assert_eq!(
        (m.hdisplay, m.hsync_start, m.hsync_end, m.htotal),
        (1920, 2008, 2052, 2200)
    );
    assert_eq!(
        (m.vdisplay, m.vsync_start, m.vsync_end, m.vtotal),
        (1080, 1084, 1089, 1125)
    );
    assert_eq!(m.flags, DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC);
    assert_ne!(m.kind & DRM_MODE_TYPE_PREFERRED, 0);
    assert_eq!(e.size_mm, (510, 290));
    assert_eq!(e.bpc, Some(8));
}

#[test]
fn the_first_descriptor_is_preferred_and_the_name_is_read() {
    let e = parse(&block(&[
        DTD_720P,
        DTD_1080P,
        name_descriptor("Test Monitor"),
    ]))
    .unwrap();
    assert_eq!(e.modes.len(), 2);
    assert_eq!(e.modes[0].hdisplay, 1280);
    assert_ne!(e.modes[0].kind & DRM_MODE_TYPE_PREFERRED, 0);
    assert_eq!(e.modes[1].kind & DRM_MODE_TYPE_PREFERRED, 0);
    assert_eq!(e.name.as_deref(), Some("Test Monitor"));
    assert_eq!(e.modes[0].clock_khz, 74_250);
    assert_eq!(e.modes[0].hsync_start, 1390);
}

#[test]
fn a_bad_block_is_an_error_not_a_panic() {
    assert_eq!(parse(&[0; 10]), Err(Error::Short));
    let mut b = block(&[DTD_1080P]);
    b[0] = 1;
    assert_eq!(parse(&b), Err(Error::Header));
    let mut b = block(&[DTD_1080P]);
    b[60] ^= 0xFF;
    assert_eq!(parse(&b), Err(Error::Checksum));
}

#[test]
fn nonsense_timings_are_dropped() {
    // Front porch and sync wider than the blanking.
    let mut d = DTD_1080P;
    d[3] = 0x10;
    d[4] = 0x70;
    let e = parse(&block(&[d, DTD_720P])).unwrap();
    assert_eq!(e.modes.len(), 1);
    assert_eq!(e.modes[0].hdisplay, 1280);
    // No active area.
    let mut z = DTD_1080P;
    z[2] = 0;
    z[4] = 0x01;
    z[5] = 0;
    z[7] = 0;
    assert!(parse(&block(&[z])).unwrap().modes.is_empty());
}

#[test]
fn sync_that_is_not_separate_digital_is_negative() {
    let mut d = DTD_1080P;
    // Analog composite.
    d[17] = 0x00;
    let m = parse(&block(&[d])).unwrap().modes[0];
    assert_eq!(m.flags, DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC);
    // Interlaced is marked.
    let mut i = DTD_1080P;
    i[17] |= 0x80;
    assert_ne!(
        parse(&block(&[i])).unwrap().modes[0].flags & abi::drm::DRM_MODE_FLAG_INTERLACE,
        0
    );
}

#[test]
fn extensions_and_ratio_sizes() {
    let mut b = block(&[DTD_1080P]);
    b[126] = 1;
    b[21] = 16;
    b[22] = 0;
    let sum = b[..127].iter().fold(0u8, |a, x| a.wrapping_add(*x));
    b[127] = 0u8.wrapping_sub(sum);
    let e = parse(&b).unwrap();
    assert_eq!(e.extensions, 1);
    assert_eq!(e.size_mm, (0, 0));
}

#[test]
fn the_fallback_prefers_1080p() {
    let m = fallback_modes();
    assert_eq!(m[0].hdisplay, 1920);
    assert_ne!(m[0].kind & DRM_MODE_TYPE_PREFERRED, 0);
    assert!(
        m.iter()
            .all(|x| x.htotal > x.hsync_end && x.vtotal > x.vsync_end)
    );
}

/// A CTA-861 extension block (revision 3): a video data block of `codes`,
/// then the detailed timings.
fn cta_block(codes: &[u8], dtds: &[[u8; 18]]) -> Vec<u8> {
    let mut b = std::vec![0u8; BLOCK];
    b[0] = 0x02;
    b[1] = 3;
    let mut at = 4;
    if !codes.is_empty() {
        b[at] = 2 << 5 | u8::try_from(codes.len()).unwrap();
        b[at + 1..at + 1 + codes.len()].copy_from_slice(codes);
        at += 1 + codes.len();
    }
    b[2] = u8::try_from(at).unwrap();
    for d in dtds {
        b[at..at + 18].copy_from_slice(d);
        at += 18;
    }
    let sum = b[..127].iter().fold(0u8, |a, x| a.wrapping_add(*x));
    b[127] = 0u8.wrapping_sub(sum);
    b
}

fn resum(b: &mut [u8]) {
    let sum = b[..127].iter().fold(0u8, |a, x| a.wrapping_add(*x));
    b[127] = 0u8.wrapping_sub(sum);
}

fn with_extension(base: &[[u8; 18]], ext: &[u8]) -> Vec<u8> {
    let mut b = block(base);
    b[126] = 1;
    let sum = b[..127].iter().fold(0u8, |a, x| a.wrapping_add(*x));
    b[127] = 0u8.wrapping_sub(sum);
    b.extend_from_slice(ext);
    b
}

#[test]
fn video_codes_in_the_extension_add_modes_after_the_base_blocks() {
    // Native 1080p60 (the base block's timing: not added twice), 720p60,
    // 2160p60, 1080p50, and a code this does not read.
    let ext = cta_block(&[0x90, 4, 97, 31, 200], &[]);
    let e = parse(&with_extension(&[DTD_1080P], &ext)).unwrap();
    let got: Vec<(u16, u16, u32)> = e
        .modes
        .iter()
        .map(|m| (m.hdisplay, m.vdisplay, m.clock_khz))
        .collect();
    assert_eq!(
        got,
        [
            (1920, 1080, 148_500),
            (1280, 720, 74_250),
            (3840, 2160, 594_000),
            (1920, 1080, 148_500)
        ]
    );
    // The 1080p50 differs from the base block's 1080p60 by rate: it stays,
    // and only the base block's timing is preferred.
    assert!(e.modes[0].kind & DRM_MODE_TYPE_PREFERRED != 0);
    assert!(
        e.modes[1..]
            .iter()
            .all(|m| m.kind & DRM_MODE_TYPE_PREFERRED == 0)
    );
    assert_eq!(e.modes[3].htotal, 2640);
}

#[test]
fn the_extensions_detailed_timings_are_read_too() {
    let ext = cta_block(&[], &[DTD_720P]);
    let e = parse(&with_extension(&[DTD_1080P], &ext)).unwrap();
    assert_eq!(e.modes.len(), 2);
    assert_eq!((e.modes[1].hdisplay, e.modes[1].vdisplay), (1280, 720));
}

#[test]
fn a_malformed_extension_gives_no_modes_and_no_panic() {
    let good = cta_block(&[4, 16], &[]);
    // Bad checksum.
    let mut bad = good.clone();
    bad[127] = bad[127].wrapping_add(1);
    // Not CTA-861.
    let mut other = good.clone();
    other[0] = 0x70;
    resum(&mut other);
    // A data block longer than the space before the detailed timings.
    let mut long = good.clone();
    long[4] = 2 << 5 | 31;
    resum(&mut long);
    // Detailed timings offset out of range.
    let mut off = good.clone();
    off[2] = 200;
    resum(&mut off);
    let mut low = good.clone();
    low[2] = 2;
    resum(&mut low);
    for ext in [bad, other, long, off, low] {
        // Each is refused for its own reason (the checksum is redone for
        // all but the one meant to fail it).
        let e = parse(&with_extension(&[DTD_1080P], &ext)).unwrap();
        assert_eq!(e.modes.len(), 1);
    }
    // A short extension (fewer than 128 bytes) is left out.
    let mut short = with_extension(&[DTD_1080P], &good);
    short.truncate(BLOCK + 60);
    assert_eq!(parse(&short).unwrap().modes.len(), 1);
    // And the good one gives its two modes.
    assert_eq!(
        parse(&with_extension(&[DTD_1080P], &good))
            .unwrap()
            .modes
            .len(),
        2
    );
}

/// A CTA-861 extension with two audio descriptors (linear PCM stereo at 32,
/// 44.1 and 48 kHz with 16, 20 and 24 bits; AC-3 on six channels) and front
/// left and right speakers.
fn audio_block() -> Vec<u8> {
    let mut x = std::vec![0u8; BLOCK];
    x[0] = 0x02;
    x[1] = 3;
    x[3] = 0x40;
    x[4] = (1 << 5) | 6;
    x[5..8].copy_from_slice(&[(1 << 3) | 1, 0x07, 0x07]);
    x[8..11].copy_from_slice(&[(2 << 3) | 5, 0x07, 0x50]);
    x[11] = (4 << 5) | 3;
    x[12] = 0x01;
    x[2] = 15;
    let sum = x[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    x[127] = 0u8.wrapping_sub(sum);
    x
}

#[test]
fn the_audio_descriptors_and_speakers_are_read_by_format() {
    let mut e = block(&[DTD_1080P, name_descriptor("TESTMON")]);
    e[8] = 0x10;
    e[9] = 0xac;
    e[10] = 0x34;
    e[11] = 0x12;
    e[126] = 1;
    let sum = e[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    e[127] = 0u8.wrapping_sub(sum);
    e.extend(audio_block());
    let d = parse(&e).unwrap();
    assert_eq!(d.manufacturer, 0xac10);
    assert_eq!(d.product, 0x1234);
    assert_eq!(d.audio.descriptors[1], [9, 0x07, 0x07]);
    assert_eq!(d.audio.descriptors[2], [(2 << 3) | 5, 0x07, 0x50]);
    assert_eq!(d.audio.descriptors[3], [0; 3]);
    assert_eq!(d.audio.speakers, 1);
    assert!(d.audio.basic);
}

#[test]
fn no_extension_or_a_bad_one_means_no_audio() {
    let e = block(&[DTD_1080P]);
    assert_eq!(parse(&e).unwrap().audio, Audio::default());
    let mut e2 = e.clone();
    e2[126] = 1;
    let sum = e2[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    e2[127] = 0u8.wrapping_sub(sum);
    let mut x = audio_block();
    x[20] ^= 1;
    e2.extend(x);
    assert_eq!(parse(&e2).unwrap().audio, Audio::default());
}

#[test]
fn an_audio_block_running_off_the_extension_stops_the_walk() {
    let mut x = audio_block();
    x[4..13].fill(0);
    for at in [4, 36, 68] {
        x[at] = (7 << 5) | 31;
    }
    x[100] = (1 << 5) | 31;
    x[2] = 127;
    x[127] = 0;
    let sum = x[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    x[127] = 0u8.wrapping_sub(sum);
    assert_eq!(cta::audio(&x).unwrap().descriptors[1], [0; 3]);
}

/// The EDID of the X870E's HDMI monitor (a DELL ST2421L), read off the
/// connector by the Radeon driver (tests/corpus/x870e/abyssbsd/edid-hdmi-a-1.txt).
const DELL_ST2421L: [&str; 8] = [
    "00ffffffffffff0010ac70a0554646312f15010380351e78ea9265a655559f28",
    "0d5054a54b00714f8180d1c001010101010101010101023a801871382d40582c",
    "4500132b2100001e000000ff005450445450314245314646550a000000fc0044",
    "454c4c205354323432314c0a000000fd00384c1e5311000a2020202020200140",
    "02031ff14c9005040302071601141f12132309070765030c0010008301000002",
    "3a801871382d40582c4500132b2100001e011d8018711c1620582c2500132b21",
    "00009e011d007251d01e206e285500132b2100001e8c0ad08a20e02d10103e96",
    "00132b210000180000000000000000000000000000000000000000000000001b",
];

fn dell() -> Vec<u8> {
    let hex: String = DELL_ST2421L.concat();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn a_real_monitors_edid_gives_its_name_modes_and_audio() {
    let d = parse(&dell()).unwrap();
    assert_eq!(d.name.as_deref(), Some("DELL ST2421L"));
    assert_eq!(d.manufacturer, 0xac10);
    assert_eq!(d.product, 0xa070);
    assert_eq!(d.extensions, 1);
    assert!(
        d.modes
            .iter()
            .any(|m| m.hdisplay == 1920 && m.vdisplay == 1080)
    );
    // Stereo linear PCM at 32, 44.1 and 48 kHz, 16, 20 and 24 bits; no
    // other format; front left and right speakers; basic audio.
    assert_eq!(d.audio.descriptors[1], [0x09, 0x07, 0x07]);
    assert!(d.audio.descriptors[2..].iter().all(|x| *x == [0; 3]));
    assert_eq!(d.audio.speakers, 1);
    assert!(d.audio.basic);
}
