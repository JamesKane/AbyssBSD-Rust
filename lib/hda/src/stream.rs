// SPDX-License-Identifier: BSD-2-Clause
//! Streams: the controller's stream descriptor registers, the buffer
//! descriptor list, the stream format word, and the verbs that point a
//! converter at a stream and an HDMI pin at a converter.
//!
//! Layouts are the High Definition Audio 1.0a specification's (sections 3.3
//! and 7), as FreeBSD 15.1's `hdac.c` and `hdaa.c` use them
//! (`hdac_stream_start`, `hdaa_audio_setup`).

#![allow(
    clippy::cast_possible_truncation,
    reason = "each cast takes one field of a register or a verb"
)]

use alloc::vec::Vec;

use crate::reg::{Caps, SD_BASE, SD_STEP};

// Offsets inside one stream descriptor.

/// Control, low byte: reset, run and the interrupt enables.
pub const SD_CTL0: usize = 0x00;
/// Control, third byte: stripe, direction, stream number in bits 7:4.
pub const SD_CTL2: usize = 0x02;
/// Status (8 bits).
pub const SD_STS: usize = 0x03;
/// Link position in buffer (32 bits): bytes the DMA engine has taken.
pub const SD_LPIB: usize = 0x04;
/// Cyclic buffer length (32 bits).
pub const SD_CBL: usize = 0x08;
/// Last valid index (16 bits): the buffer descriptor list's last entry.
pub const SD_LVI: usize = 0x0c;
/// FIFO size (16 bits).
pub const SD_FIFOS: usize = 0x10;
/// Stream format (16 bits).
pub const SD_FMT: usize = 0x12;
/// Buffer descriptor list base, lower half (32 bits).
pub const SD_BDPL: usize = 0x18;
/// Buffer descriptor list base, upper half (32 bits).
pub const SD_BDPU: usize = 0x1c;

/// `SD_CTL0`: stream reset.
pub const CTL_SRST: u8 = 1 << 0;
/// `SD_CTL0`: the DMA engine runs.
pub const CTL_RUN: u8 = 1 << 1;
/// `SD_CTL0`: interrupt on buffer completion.
pub const CTL_IOCE: u8 = 1 << 2;
/// `SD_STS`: a buffer with the interrupt flag completed.
pub const STS_BCIS: u8 = 1 << 2;
/// `SD_STS`: FIFO error.
pub const STS_FIFOE: u8 = 1 << 3;
/// `SD_STS`: descriptor error.
pub const STS_DESE: u8 = 1 << 4;
/// `SD_STS`: the FIFO is ready.
pub const STS_FIFORDY: u8 = 1 << 5;
/// `SD_CTL2`: the stream number's shift.
pub const CTL2_STREAM_SHIFT: u8 = 4;

/// The byte offset of stream descriptor `index` (all streams counted
/// from zero: inputs first, then outputs, then bidirectional ones).
#[must_use]
pub const fn descriptor(index: usize) -> usize {
    SD_BASE + index * SD_STEP
}

/// The index of output stream `n`: output descriptors follow the input
/// ones. `None` if the controller has no such output stream.
#[must_use]
pub const fn output_index(caps: Caps, n: usize) -> Option<usize> {
    if n < caps.output_streams as usize {
        Some(caps.input_streams as usize + n)
    } else {
        None
    }
}

/// A buffer descriptor list entry: 16 bytes (address, length, and the
/// interrupt-on-completion flag in bit 0 of the last word).
#[must_use]
pub const fn bdle(addr: u64, len: u32, ioc: bool) -> [u32; 4] {
    [addr as u32, (addr >> 32) as u32, len, ioc as u32]
}

/// The stream format word for linear PCM at `rate` Hz, `bits` per sample
/// (8, 16, 20, 24 or 32) and `channels` (1 to 16), `None` for a rate the
/// base and multiplier fields cannot say.
///
/// Bit 14 selects the 44.1 kHz base, bits 13:11 the multiplier minus one,
/// bits 10:8 the divisor minus one, bits 6:4 the sample size, bits 3:0 the
/// channels minus one.
#[must_use]
pub const fn format(rate: u32, bits: u8, channels: u8) -> Option<u16> {
    let size = match bits {
        8 => 0,
        16 => 1,
        20 => 2,
        24 => 3,
        32 => 4,
        _ => return None,
    };
    if channels == 0 || channels > 16 {
        return None;
    }
    // (base, multiplier, divisor).
    let (base, mult, div): (u16, u16, u16) = match rate {
        8_000 => (0, 1, 6),
        11_025 => (1, 1, 4),
        16_000 => (0, 1, 3),
        22_050 => (1, 1, 2),
        32_000 => (0, 2, 3),
        44_100 => (1, 1, 1),
        48_000 => (0, 1, 1),
        88_200 => (1, 2, 1),
        96_000 => (0, 2, 1),
        176_400 => (1, 4, 1),
        192_000 => (0, 4, 1),
        _ => return None,
    };
    Some(base << 14 | (mult - 1) << 11 | (div - 1) << 8 | size << 4 | (channels as u16 - 1))
}

// Verbs (specification section 7.3.3; FreeBSD's `hda_reg.h`).

/// Set Converter Format (a 4-bit verb, 16-bit payload).
#[must_use]
pub const fn set_converter_format(cad: u8, nid: u8, fmt: u16) -> u32 {
    crate::verb::verb4(cad, nid, 0x2, fmt)
}

/// Set Converter Stream, Channel: the stream number in bits 7:4, the first
/// channel in bits 3:0 (stream 0 detaches the converter).
#[must_use]
pub const fn set_stream_channel(cad: u8, nid: u8, stream: u8, channel: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x706, stream << 4 | (channel & 0xf))
}

/// Set Digital Converter Control 1: bit 0 enables the digital output
/// (`DIGEN`), bit 5 marks non-audio data.
#[must_use]
pub const fn set_digital_converter(cad: u8, nid: u8, bits: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x70d, bits)
}

/// `DIGEN`, for [`set_digital_converter`].
pub const DIGEN: u8 = 1 << 0;

/// Set Pin Widget Control: bit 6 enables the output, bit 5 the input.
#[must_use]
pub const fn set_pin_control(cad: u8, nid: u8, control: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x707, control)
}

/// Pin control: the output is enabled.
pub const PIN_OUT_ENABLE: u8 = 1 << 6;

/// Get Pin Sense.
#[must_use]
pub const fn get_pin_sense(cad: u8, nid: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0xf09, 0)
}

/// Pin sense: a display is connected.
pub const SENSE_PRESENCE: u32 = 1 << 31;
/// Pin sense: the display's identification (ELD) is valid.
pub const SENSE_ELD_VALID: u32 = 1 << 30;

/// Get Parameter: the converter's supported PCM sizes and rates.
pub const PARAM_PCM_SIZE_RATE: u8 = 0x0a;
/// Get Parameter: the converter's supported stream formats.
pub const PARAM_STREAM_FORMATS: u8 = 0x0b;
/// Get Parameter: a pin's capabilities.
pub const PARAM_PIN_CAP: u8 = 0x0c;

/// Set Amplifier Gain/Mute, output, both channels, gain `gain`, not muted
/// (index 0).
#[must_use]
pub const fn set_output_amp(cad: u8, nid: u8, gain: u8) -> u32 {
    crate::verb::verb4(
        cad,
        nid,
        0x3,
        0x8000 | 0x2000 | 0x1000 | (gain as u16 & 0x7f),
    )
}

/// Set Amplifier Gain/Mute, input amplifier `index`, both channels, gain
/// `gain`, not muted.
#[must_use]
pub const fn set_input_amp(cad: u8, nid: u8, index: u8, gain: u8) -> u32 {
    crate::verb::verb4(
        cad,
        nid,
        0x3,
        0x4000 | 0x2000 | 0x1000 | (index as u16 & 0xf) << 8 | (gain as u16 & 0x7f),
    )
}

/// Set Connection Select: the input a selector or pin takes (`index` into
/// its connection list).
#[must_use]
pub const fn set_connection_select(cad: u8, nid: u8, index: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x701, index)
}

/// Set EAPD/BTL Enable: bit 1 turns on the external amplifier.
#[must_use]
pub const fn set_eapd(cad: u8, nid: u8, bits: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x70c, bits)
}

/// Set the HDMI data island packet buffer's write index (the packet type
/// in bits 7:3 of the index, the byte in bits 3:0: `index` 0 is the audio
/// infoframe's first byte).
#[must_use]
pub const fn set_hdmi_dip_index(cad: u8, nid: u8, index: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x730, index)
}

/// Write one byte of the data island packet buffer, advancing the index.
#[must_use]
pub const fn set_hdmi_dip_data(cad: u8, nid: u8, byte: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x731, byte)
}

/// Set the data island packet transmit control: `0xc0` sends the packet
/// every frame, 0 stops it.
#[must_use]
pub const fn set_hdmi_dip_xmit(cad: u8, nid: u8, control: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x732, control)
}

/// Set Channel Count on a converter (`count` is the channels minus one).
#[must_use]
pub const fn set_channel_count(cad: u8, nid: u8, count: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x72d, count)
}

/// Set HDMI channel slot: pins map stream channel `channel` to slot
/// `slot` (the speaker position the sink hears it at).
#[must_use]
pub const fn set_hdmi_channel_slot(cad: u8, nid: u8, channel: u8, slot: u8) -> u32 {
    crate::verb::verb12(cad, nid, 0x734, slot << 4 | (channel & 0xf))
}

/// The audio infoframe's bytes for `channels` of stereo-or-more linear PCM
/// (CTA-861: type 4, version 1, length 10; the checksum, channel count
/// minus one, four reserved bytes and the channel allocation), as
/// `hdaa_audio_setup` writes them to the pin's data island packet buffer.
/// `channels` is 1 to 8; the allocation is FreeBSD's for the layout.
#[must_use]
pub fn audio_infoframe(channels: u8) -> Vec<u8> {
    // Channel allocation by channel count (x.0 layouts).
    const ALLOCATION: [u8; 8] = [0x02, 0x00, 0x04, 0x08, 0x0a, 0x0e, 0x12, 0x12];
    let n = channels.clamp(1, 8);
    let ca = ALLOCATION[usize::from(n) - 1];
    let mut sum = 0u8;
    for b in [0x84u8, 0x01, 0x0a, n - 1, ca] {
        sum = sum.wrapping_sub(b);
    }
    let mut v = Vec::new();
    v.extend_from_slice(&[0x84, 0x01, 0x0a, sum, n - 1, 0, 0, ca]);
    v
}

/// One period of a 1 kHz sine at 48 kHz, 48 samples, peak 8,000 of 32,767
/// (about -12 dBFS).
pub const TONE_1KHZ_48K: [i16; 48] = [
    0, 1044, 2071, 3061, 4000, 4870, 5657, 6347, 6928, 7391, 7727, 7932, 8000, 7932, 7727, 7391,
    6928, 6347, 5657, 4870, 4000, 3061, 2071, 1044, 0, -1044, -2071, -3061, -4000, -4870, -5657,
    -6347, -6928, -7391, -7727, -7932, -8000, -7932, -7727, -7391, -6928, -6347, -5657, -4870,
    -4000, -3061, -2071, -1044,
];

/// `periods` periods of [`TONE_1KHZ_48K`] as interleaved 16-bit samples
/// for `channels` channels (every channel the same), little-endian bytes.
/// A whole number of periods, so the buffer loops without a click.
#[must_use]
pub fn tone_buffer(periods: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(periods * TONE_1KHZ_48K.len() * channels * 2);
    for _ in 0..periods {
        for s in TONE_1KHZ_48K {
            for _ in 0..channels {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_format_word() {
        // 48 kHz, 16 bits, stereo: the format every HDA codec takes.
        assert_eq!(format(48_000, 16, 2), Some(0x0011));
        // 44.1 kHz base, 24 bits, two channels.
        assert_eq!(format(44_100, 24, 2), Some(0x4031));
        // 96 kHz is 48 kHz times two.
        assert_eq!(format(96_000, 16, 2), Some(0x0811));
        // 32 kHz is 48 kHz times two over three.
        assert_eq!(format(32_000, 16, 2), Some(0x0a11));
        assert_eq!(format(12_345, 16, 2), None);
        assert_eq!(format(48_000, 12, 2), None);
        assert_eq!(format(48_000, 16, 0), None);
    }

    #[test]
    fn descriptors_follow_the_input_streams() {
        let caps = Caps::of(0x4401); // four output streams, four input streams
        assert_eq!(output_index(caps, 0), Some(4));
        assert_eq!(output_index(caps, 3), Some(7));
        assert_eq!(output_index(caps, 4), None);
        assert_eq!(descriptor(4), 0x80 + 4 * 0x20);
    }

    #[test]
    fn a_buffer_descriptor_entry() {
        assert_eq!(
            bdle(0x1_2345_6000, 0x1000, true),
            [0x2345_6000, 0x1, 0x1000, 1]
        );
    }

    #[test]
    fn verbs_encode_as_hdaa_sends_them() {
        assert_eq!(set_converter_format(0, 2, 0x0011), 0x0022_0011);
        assert_eq!(set_stream_channel(0, 2, 1, 0), 0x0027_0610);
        assert_eq!(set_digital_converter(0, 2, DIGEN), 0x0027_0d01);
        assert_eq!(set_pin_control(0, 3, PIN_OUT_ENABLE), 0x0037_0740);
        assert_eq!(get_pin_sense(0, 3), 0x003f_0900);
        assert_eq!(set_connection_select(0, 4, 1), 0x0047_0101);
        assert_eq!(set_hdmi_dip_xmit(0, 3, 0xc0), 0x0037_32c0);
        assert_eq!(set_hdmi_dip_data(0, 3, 0x84), 0x0037_3184);
        assert_eq!(set_input_amp(0, 2, 1, 0x20), 0x0023_7120);
    }

    #[test]
    fn the_stereo_audio_infoframe_matches_hdaa() {
        // hdaa: header 0x84 0x01 0x0a, checksum, channels-1, 0, 0, allocation.
        let f = audio_infoframe(2);
        assert_eq!(f, [0x84, 0x01, 0x0a, 0x70, 0x01, 0, 0, 0x00]);
        // The bytes sum to zero modulo 256 once the checksum is in place.
        let sum = f.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn the_tone_is_a_whole_number_of_periods_with_both_channels_equal() {
        let b = tone_buffer(2, 2);
        assert_eq!(b.len(), 2 * 48 * 2 * 2);
        // Sample 12 (the peak), left and right.
        let at = 12 * 4;
        assert_eq!(&b[at..at + 2], &8000i16.to_le_bytes());
        assert_eq!(&b[at + 2..at + 4], &8000i16.to_le_bytes());
        // The second period repeats the first.
        let half = b.len() / 2;
        assert_eq!(b[..half], b[half..]);
    }
}
