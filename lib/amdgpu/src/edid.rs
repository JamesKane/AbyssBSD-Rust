// SPDX-License-Identifier: BSD-2-Clause
//! The sink's EDID (extended display identification data), read for what the
//! audio endpoint needs (08 section 4.6): the manufacturer and product, the
//! monitor's name, the short audio descriptors and the speaker allocation.
//!
//! The layouts are the public EDID 1.4 and CTA-861 ones; nothing here knows
//! how the bytes were fetched.

use crate::regs::Window;

/// Bytes in one EDID block.
pub const BLOCK: usize = 128;
/// The header every base block starts with.
const HEADER: [u8; 8] = [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0];
/// The CTA-861 extension's tag.
const CTA_TAG: u8 = 0x02;
/// Short audio descriptors are indexed by the audio format code (1 = linear
/// PCM ... 14), and there are 14 endpoint slots.
pub const FORMATS: usize = 15;

/// What the endpoint is told about the sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    /// The manufacturer id as the two EDID bytes (8 and 9) read little-endian.
    pub manufacturer: u16,
    /// The product code (bytes 10 and 11, little-endian).
    pub product: u16,
    /// The monitor's name (descriptor tag `0xfc`), up to 13 characters.
    pub name: [u8; 18],
    /// Its length.
    pub name_len: usize,
    /// The short audio descriptors by format code: the three bytes, or zero
    /// if the sink has none of that format.
    pub audio: [[u8; 3]; FORMATS],
    /// The speaker allocation block's first byte (0 if there is none).
    pub speakers: u8,
    /// Whether the sink's CTA block says it is an HDMI sink with basic audio
    /// (bit 6 of byte 3 of the extension).
    pub basic_audio: bool,
}

/// Why a block did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Fewer than 128 bytes.
    Short,
    /// The 8-byte header is wrong.
    Header,
    /// The block's bytes do not sum to 0 modulo 256.
    Checksum,
}

/// Whether `block` (128 bytes) sums to 0 modulo 256.
#[must_use]
pub fn checksum_ok(block: &[u8]) -> bool {
    block.len() >= BLOCK && block[..BLOCK].iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
}

/// Parses an EDID: the base block, and the first CTA-861 extension if the
/// bytes include one.
///
/// # Errors
///
/// [`Error`] if the base block is short, has the wrong header, or fails its
/// checksum.
pub fn parse(edid: &[u8]) -> Result<Sink, Error> {
    if edid.len() < BLOCK {
        return Err(Error::Short);
    }
    if edid[..8] != HEADER {
        return Err(Error::Header);
    }
    if !checksum_ok(edid) {
        return Err(Error::Checksum);
    }
    let mut sink = Sink {
        manufacturer: u16::from_le_bytes([edid[8], edid[9]]),
        product: u16::from_le_bytes([edid[10], edid[11]]),
        name: [0; 18],
        name_len: 0,
        audio: [[0; 3]; FORMATS],
        speakers: 0,
        basic_audio: false,
    };
    // The four 18-byte descriptors at 54, 72, 90, 108: a monitor name has
    // zeros in the first three bytes, then tag 0xfc.
    for d in (54..126).step_by(18) {
        if edid[d..d + 3] == [0, 0, 0] && edid[d + 3] == 0xfc {
            let text = &edid[d + 5..d + 18];
            let n = text.iter().position(|c| *c == b'\n').unwrap_or(13);
            sink.name[..n].copy_from_slice(&text[..n]);
            sink.name_len = n;
        }
    }
    let extensions = usize::from(edid[126]);
    for e in 1..=extensions {
        let Some(ext) = edid.get(e * BLOCK..(e + 1) * BLOCK) else {
            break;
        };
        if ext[0] == CTA_TAG && checksum_ok(ext) {
            cta(ext, &mut sink);
            break;
        }
    }
    Ok(sink)
}

/// Reads the CTA-861 extension's data blocks: audio (tag 1) and speaker
/// allocation (tag 4).
fn cta(ext: &[u8], sink: &mut Sink) {
    sink.basic_audio = ext[3] & 0x40 != 0;
    let end = usize::from(ext[2]).clamp(4, BLOCK);
    let mut at = 4;
    while at < end {
        let tag = ext[at] >> 5;
        let len = usize::from(ext[at] & 0x1f);
        let body = at + 1;
        if body + len > BLOCK {
            return;
        }
        match tag {
            1 => {
                for s in ext[body..body + len].chunks_exact(3) {
                    let format = usize::from((s[0] >> 3) & 0xf);
                    if format != 0 && format < FORMATS {
                        sink.audio[format] = [s[0], s[1], s[2]];
                    }
                }
            }
            4 if len >= 3 => sink.speakers = ext[body],
            _ => {}
        }
        at = body + len;
    }
}

/// The endpoint's descriptor word for the descriptor bytes: maximum
/// channels minus one in 2:0, supported rates in 15:8, the third byte
/// (bit depths for linear PCM) in 23:16. The stereo-only rates (31:24) are
/// not derived from the EDID.
#[must_use]
pub const fn descriptor_word(d: [u8; 3]) -> u32 {
    (d[0] as u32 & 7) | (d[1] as u32) << 8 | (d[2] as u32) << 16
}

/// Writes the sink into the endpoint's registers (08 section 4.6): speakers
/// with the HDMI connection bit, the audio descriptors, the manufacturer and
/// product, the name and its length. The caller brackets it with the
/// clock-gating disable.
pub fn write_endpoint<W: Window + ?Sized>(w: &W, sink: &Sink) {
    use crate::hdmi::{endpoint_write, EP_AUDIO_DESCRIPTOR, EP_CHANNEL_SPEAKER};
    endpoint_write(w, EP_CHANNEL_SPEAKER, u32::from(sink.speakers) | (1 << 16));
    for (format, d) in sink.audio.iter().enumerate().skip(1) {
        endpoint_write(w, EP_AUDIO_DESCRIPTOR + format as u32, descriptor_word(*d));
    }
    endpoint_write(
        w,
        0x3a,
        u32::from(sink.manufacturer) | u32::from(sink.product) << 16,
    );
    endpoint_write(w, 0x3b, sink.name_len as u32);
    for (i, chunk) in sink.name.chunks(4).take(5).enumerate() {
        let mut b = [0u8; 4];
        b[..chunk.len()].copy_from_slice(chunk);
        endpoint_write(w, 0x3e + i as u32, u32::from_le_bytes(b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn fix(block: &mut [u8]) {
        block[127] = 0;
        let sum = block[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        block[127] = 0u8.wrapping_sub(sum);
    }

    /// A base block plus a CTA extension: "TESTMON", stereo LPCM 32/44.1/48
    /// kHz with 16/20/24 bits, an AC-3 descriptor, front left and right.
    fn sample() -> Vec<u8> {
        let mut e = vec![0u8; 256];
        e[..8].copy_from_slice(&HEADER);
        e[8] = 0x10;
        e[9] = 0xac;
        e[10] = 0x34;
        e[11] = 0x12;
        e[126] = 1;
        let d = 72;
        e[d + 3] = 0xfc;
        e[d + 5..d + 5 + 8].copy_from_slice(b"TESTMON\n");
        for b in &mut e[d + 13..d + 18] {
            *b = b' ';
        }
        fix(&mut e[..128]);
        let x = &mut e[128..];
        x[0] = CTA_TAG;
        x[1] = 3;
        x[3] = 0x40;
        // Audio block: two descriptors (LPCM, AC-3), 6 bytes.
        x[4] = (1 << 5) | 6;
        x[5..8].copy_from_slice(&[(1 << 3) | 1, 0x07, 0x07]);
        x[8..11].copy_from_slice(&[(2 << 3) | 5, 0x07, 0x50]);
        // Speaker allocation: front left and right.
        x[11] = (4 << 5) | 3;
        x[12] = 0x01;
        x[2] = 15;
        fix(x);
        e
    }

    #[test]
    fn the_sample_parses_into_the_sinks_audio_capabilities() {
        let s = parse(&sample()).unwrap();
        assert_eq!(s.manufacturer, 0xac10);
        assert_eq!(s.product, 0x1234);
        assert_eq!(&s.name[..s.name_len], b"TESTMON");
        assert_eq!(s.audio[1], [9, 0x07, 0x07]);
        assert_eq!(s.audio[2], [(2 << 3) | 5, 0x07, 0x50]);
        assert_eq!(s.audio[3], [0; 3]);
        assert_eq!(s.speakers, 0x01);
        assert!(s.basic_audio);
        // The descriptor word: 2 channels, three rates, three depths.
        assert_eq!(descriptor_word(s.audio[1]), 0x0007_0701);
        assert_eq!(descriptor_word(s.audio[2]), 0x0050_0705);
    }

    #[test]
    fn a_bad_block_is_refused_not_guessed() {
        assert_eq!(parse(&[0; 64]), Err(Error::Short));
        let mut e = sample();
        e[0] = 1;
        assert_eq!(parse(&e), Err(Error::Header));
        let mut e = sample();
        e[20] ^= 1;
        assert_eq!(parse(&e), Err(Error::Checksum));
        // A base block alone has no audio, and a bad extension is ignored.
        let s = parse(&sample()[..128]).unwrap();
        assert_eq!(s.audio, [[0; 3]; FORMATS]);
        let mut e = sample();
        e[200] ^= 1;
        assert!(!parse(&e).unwrap().basic_audio);
    }

    #[test]
    fn a_data_block_running_off_the_extension_stops_the_walk() {
        let mut e = sample();
        let x = &mut e[128..];
        x[4..12].fill(0);
        x[11] = 0;
        x[12] = 0;
        // Three filler blocks (tag 7, 31 bytes) to offset 100, then an
        // audio block whose length runs past the end of the extension.
        for at in [4, 36, 68] {
            x[at] = (7 << 5) | 31;
        }
        x[100] = (1 << 5) | 31;
        x[2] = 127;
        fix(x);
        let s = parse(&e).unwrap();
        assert_eq!(s.audio[1], [0; 3]);
    }

    #[test]
    fn the_endpoint_registers_get_the_sink() {
        use core::cell::RefCell;
        use std::collections::BTreeMap;
        struct Ep {
            index: RefCell<u32>,
            regs: RefCell<BTreeMap<u32, u32>>,
        }
        impl Window for Ep {
            fn read32(&self, _: usize) -> u32 {
                0
            }
            fn write32(&self, o: usize, v: u32) {
                if o == crate::hdmi::ENDPOINT_INDEX {
                    *self.index.borrow_mut() = v;
                } else {
                    self.regs.borrow_mut().insert(*self.index.borrow(), v);
                }
            }
        }
        let w = Ep {
            index: RefCell::new(0),
            regs: RefCell::new(BTreeMap::new()),
        };
        write_endpoint(&w, &parse(&sample()).unwrap());
        let r = w.regs.borrow();
        assert_eq!(r[&0x25], 0x0001_0001);
        assert_eq!(r[&0x29], 0x0007_0701);
        assert_eq!(r[&(0x28 + 2)], 0x0050_0705);
        assert_eq!(r[&0x3a], 0x1234_ac10);
        assert_eq!(r[&0x3b], 7);
        assert_eq!(r[&0x3e], u32::from_le_bytes(*b"TEST"));
        assert_eq!(r[&0x3f], u32::from_le_bytes([b'M', b'O', b'N', 0]));
    }
}
