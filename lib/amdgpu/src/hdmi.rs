// SPDX-License-Identifier: BSD-2-Clause
//! HDMI on the first digital encoder (DIG0) of Raphael, with audio: the
//! switch from the firmware's DVI mode, the AVI info frame, and the audio
//! path of specification 08 sections 4.5 and 4.6 (stage R3.4).
//!
//! Split into steps the driver runs and logs one at a time:
//!
//! - [`audio_encoder`], [`endpoint_configure`], [`audio_clock`]: the audio
//!   side's preparation; none of it touches the picture.
//! - [`video_mode`], [`avi_info_frame`], [`packet_controls`]: the encoder
//!   into HDMI mode and the packets; this one disturbs the stream.
//! - [`endpoint_enable`], [`unmute`]: the last two steps.
//!
//! Stereo at 48 kHz on 1920x1080 at 60 Hz (a 148.5 MHz pixel clock) is the
//! one case the specification has the values for; the sink's EDID is not
//! read (no DDC engine yet), so the endpoint is told "two channels of linear
//! PCM at 32, 44.1 and 48 kHz" and nothing about the sink's name.

use crate::regs::Window;

/// The pixel clock the audio clock is for, in units of 100 Hz (148.5 MHz).
pub const PIXEL_CLOCK_100HZ: u32 = 1_485_000;
/// The audio reference, 24 MHz in units of 100 Hz.
pub const AUDIO_REFERENCE_100HZ: u32 = 240_000;

// DIG0 registers (Raphael bytes). DIG n is 0x400 further on.
/// `DIG0_DIG_FE_CNTL`: bit 10 `DIG_START`.
pub const DIG_FE_CNTL: usize = 0x1552c;
/// `DIG0_DIG_CLOCK_PATTERN`.
pub const DIG_CLOCK_PATTERN: usize = 0x15538;
/// `DIG0_DIG_BE_CNTL`: bits 18:16 the signal mode (2 DVI, 3 HDMI).
pub const DIG_BE_CNTL: usize = 0x155c0;
/// `DIG0_HDMI_CONTROL`.
pub const HDMI_CONTROL: usize = 0x1554c;
/// `HDMI_GENERIC_PACKET_CONTROL0`: packet n send is bit 4n, continue 4n+1.
pub const GENERIC_PACKET_CONTROL0: usize = 0x15568;
/// `HDMI_GENERIC_PACKET_CONTROL1`: packet 0's line in bits 15:0.
pub const GENERIC_PACKET_CONTROL1: usize = 0x15578;
/// `HDMI_DB_CONTROL`: bit 12 double-buffer disable.
pub const DB_CONTROL: usize = 0x15598;
/// `HDMI_VBI_PACKET_CONTROL`.
pub const VBI_PACKET_CONTROL: usize = 0x1555c;
/// `HDMI_INFOFRAME_CONTROL0`: bit 4 audio info frame send.
pub const INFOFRAME_CONTROL0: usize = 0x15560;
/// `HDMI_INFOFRAME_CONTROL1`: bits 13:8 audio info frame line.
pub const INFOFRAME_CONTROL1: usize = 0x15564;
/// `HDMI_GC`: bit 0 AV-mute.
pub const HDMI_GC: usize = 0x15574;
/// `AFMT_CNTL`: bit 0 audio clock enable.
pub const AFMT_CNTL: usize = 0x155bc;
/// `HDMI_AUDIO_PACKET_CONTROL`: bits 5:4 audio delay enable.
pub const AUDIO_PACKET_CONTROL: usize = 0x15554;
/// `HDMI_ACR_PACKET_CONTROL`: bit 12 auto-send, bit 8 source, bit 31 priority.
pub const ACR_PACKET_CONTROL: usize = 0x15558;
/// The three clock-regeneration pairs: (CTS register, N register).
pub const ACR_32: (usize, usize) = (0x1559c, 0x155a0);
/// 44.1 kHz.
pub const ACR_44_1: (usize, usize) = (0x155a4, 0x155a8);
/// 48 kHz.
pub const ACR_48: (usize, usize) = (0x155ac, 0x155b0);
/// `AFMT_AUDIO_SRC_CONTROL`: bits 2:0 the endpoint feeding this encoder.
pub const AFMT_AUDIO_SRC_CONTROL: usize = 0x15514;
/// `AFMT_AUDIO_PACKET_CONTROL2`: bits 15:8 channel enable.
pub const AFMT_AUDIO_PACKET_CONTROL2: usize = 0x154d4;
/// `AFMT_AUDIO_PACKET_CONTROL`: bit 26 update channel status, bit 0 sample send.
pub const AFMT_AUDIO_PACKET_CONTROL: usize = 0x15508;
/// `AFMT_60958_0`: left channel number in bits 23:20, accuracy 29:28.
pub const AFMT_60958_0: usize = 0x154e0;
/// `AFMT_60958_1`: right channel number in bits 23:20.
pub const AFMT_60958_1: usize = 0x154e4;
/// `AFMT_MEM_PWR`: bits 5:4 force (0 = powered).
pub const AFMT_MEM_PWR: usize = 0x1551c;
/// `AFMT_STATUS`: bit 4 audio enabled (read-only).
pub const AFMT_STATUS: usize = 0x15504;

// The video packet generator (VPG0).
/// `VPG0_VPG_GENERIC_PACKET_ACCESS_CTRL`: the dword index.
pub const VPG_ACCESS: usize = 0x154a0;
/// `VPG0_VPG_GENERIC_PACKET_DATA`: the auto-incrementing write port.
pub const VPG_DATA: usize = 0x154a4;
/// `VPG0_VPG_GSP_IMMEDIATE_UPDATE_CTRL`: bit n takes packet n now.
pub const VPG_IMMEDIATE: usize = 0x154ac;
/// `VPG0_VPG_GENERIC_STATUS`: bit 1 conflict; write bit 4 to clear.
pub const VPG_STATUS: usize = 0x154b0;
/// Dwords per packet in the packet memory.
pub const PACKET_DWORDS: u32 = 9;

// The audio clock (the display clock generator).
/// `DCCG_AUDIO_DTO_SOURCE`: bits 2:0 the timing generator, bits 6:4 the DTO.
pub const AUDIO_DTO_SOURCE: usize = 0x5ac;
/// `DCCG_AUDIO_DTO0_PHASE`.
pub const AUDIO_DTO0_PHASE: usize = 0x5b0;
/// `DCCG_AUDIO_DTO0_MODULE`.
pub const AUDIO_DTO0_MODULE: usize = 0x5b4;

// The codec's endpoints (the indirect pair, endpoint 0).
/// `AZF0ENDPOINT0_AZALIA_F0_CODEC_ENDPOINT_INDEX`.
pub const ENDPOINT_INDEX: usize = 0xe118;
/// `AZF0ENDPOINT0_AZALIA_F0_CODEC_ENDPOINT_DATA`.
pub const ENDPOINT_DATA: usize = 0xe11c;
/// The endpoint's channel and speaker register.
pub const EP_CHANNEL_SPEAKER: u32 = 0x25;
/// The first audio descriptor (descriptor i is `+ i`).
pub const EP_AUDIO_DESCRIPTOR: u32 = 0x28;
/// The hot-plug control: bit 0 clock-gating disable, bit 31 audio enabled.
pub const EP_HOTPLUG: u32 = 0x54;
/// The first sink-info register (manufacturer and product; then the name's
/// length, two port words, and the name from `+ 4`, four characters each).
pub const EP_SINK_INFO: u32 = 0x3a;
/// The codec's supported-rates register and its power-state register.
pub const CODEC_RATES: usize = 0xe32c;
/// The codec's power-state register.
pub const CODEC_POWER: usize = 0xe334;

/// Registers worth a line in the log, before and after: (name, byte).
pub const WATCH: &[(&str, usize)] = &[
    ("DIG_FE_CNTL", DIG_FE_CNTL),
    ("DIG_CLOCK_PATTERN", DIG_CLOCK_PATTERN),
    ("DIG_BE_CNTL", DIG_BE_CNTL),
    ("HDMI_CONTROL", HDMI_CONTROL),
    ("HDMI_VBI_PACKET_CONTROL", VBI_PACKET_CONTROL),
    ("HDMI_GENERIC_PACKET_CONTROL0", GENERIC_PACKET_CONTROL0),
    ("HDMI_DB_CONTROL", DB_CONTROL),
    ("AFMT_CNTL", AFMT_CNTL),
    ("AFMT_AUDIO_SRC_CONTROL", AFMT_AUDIO_SRC_CONTROL),
    ("AFMT_AUDIO_PACKET_CONTROL", AFMT_AUDIO_PACKET_CONTROL),
    ("AFMT_STATUS", AFMT_STATUS),
    ("HDMI_ACR_PACKET_CONTROL", ACR_PACKET_CONTROL),
    ("DCCG_AUDIO_DTO_SOURCE", AUDIO_DTO_SOURCE),
    ("DCCG_AUDIO_DTO0_PHASE", AUDIO_DTO0_PHASE),
    ("DCCG_AUDIO_DTO0_MODULE", AUDIO_DTO0_MODULE),
    ("CODEC_RATES", CODEC_RATES),
    ("CODEC_POWER", CODEC_POWER),
];

/// Sets the bits of `mask` to `value` (read, change, write).
pub fn update<W: Window + ?Sized>(w: &W, at: usize, mask: u32, value: u32) {
    w.write32(at, (w.read32(at) & !mask) | (value & mask));
}

/// A word of the clock-regeneration pair for an audio rate: the CTS goes in
/// bits 31:12 of its register.
#[must_use]
pub const fn acr_cts(cts: u32) -> u32 {
    cts << 12
}

/// Step 1 of the audio side (08 section 4.6): the encoder's audio setup for
/// stereo from endpoint `endpoint`: the audio clock on, the formatter's
/// memory powered, channel numbers, the audio delay, the three clock
/// regeneration pairs with auto-send, the routing, and the channel mask.
pub fn audio_encoder<W: Window + ?Sized>(w: &W, endpoint: u32) {
    update(w, AFMT_CNTL, 1, 1);
    update(w, AFMT_MEM_PWR, 0x30, 0);
    update(w, AFMT_60958_0, 0x00f0_0000 | 0x3000_0000, 1 << 20);
    update(w, AFMT_60958_1, 0x00f0_0000, 2 << 20);
    update(w, AUDIO_PACKET_CONTROL, 0x30, 1 << 4);
    // 148.5 MHz: 32 kHz N 4096 / CTS 148500, 44.1 kHz N 6272 / CTS 165000,
    // 48 kHz N 6144 / CTS 148500 (the HDMI specification's table).
    for (pair, cts, n) in [
        (ACR_32, 148_500, 4096),
        (ACR_44_1, 165_000, 6272),
        (ACR_48, 148_500, 6144),
    ] {
        w.write32(pair.0, acr_cts(cts));
        update(w, pair.1, 0x000f_ffff, n);
    }
    // Auto-send on, hardware-computed CTS, normal priority.
    update(
        w,
        ACR_PACKET_CONTROL,
        (1 << 12) | (1 << 8) | (1 << 31),
        1 << 12,
    );
    update(w, AFMT_AUDIO_SRC_CONTROL, 7, endpoint);
    // Two channels; the layout and OSF overrides off.
    update(
        w,
        AFMT_AUDIO_PACKET_CONTROL2,
        0xff00 | 1 | (1 << 28),
        0x03 << 8,
    );
}

/// Writes one endpoint register through the index and data pair.
pub fn endpoint_write<W: Window + ?Sized>(w: &W, index: u32, value: u32) {
    w.write32(ENDPOINT_INDEX, index & 0x3fff);
    w.write32(ENDPOINT_DATA, value);
}

/// Reads one endpoint register.
pub fn endpoint_read<W: Window + ?Sized>(w: &W, index: u32) -> u32 {
    w.write32(ENDPOINT_INDEX, index & 0x3fff);
    w.read32(ENDPOINT_DATA)
}

/// The channel and speaker word for stereo (front left and right) with the
/// HDMI connection bit.
#[must_use]
pub const fn stereo_speakers() -> u32 {
    0x01 | (1 << 16)
}

/// The audio descriptor word for linear PCM: two channels (max channels
/// minus one in bits 2:0), 32, 44.1 and 48 kHz (bits 15:8), 16, 20 and 24
/// bits (bits 23:16).
#[must_use]
pub const fn lpcm_descriptor() -> u32 {
    1 | (0x07 << 8) | (0x07 << 16)
}

/// Step 2 (08 section 4.6): the endpoint's configuration, bracketed by the
/// clock-gating disable. With no EDID the descriptor is the stereo LPCM one;
/// descriptor index 1 is linear PCM (the CTA-861 coding type; the index
/// mapping is the specification's "descriptor i", assumed to follow the
/// coding type). The codec's supported rates (32, 44.1, 48 kHz) are set too.
pub fn endpoint_configure<W: Window + ?Sized>(w: &W) {
    update(w, CODEC_RATES, 0xfff, 0x70);
    let hotplug = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, hotplug | 1);
    endpoint_write(w, EP_CHANNEL_SPEAKER, stereo_speakers());
    endpoint_write(w, EP_AUDIO_DESCRIPTOR + 1, lpcm_descriptor());
    let hotplug = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, hotplug & !1);
}

/// The endpoint's descriptor word for a short audio descriptor: maximum
/// channels minus one in 2:0, supported rates in 15:8, the third byte (bit
/// depths for linear PCM) in 23:16. The stereo-only rates (31:24) are not
/// derived from the monitor's data.
#[must_use]
pub const fn descriptor_word(d: [u8; 3]) -> u32 {
    (d[0] as u32 & 7) | (d[1] as u32) << 8 | (d[2] as u32) << 16
}

/// Step 2 with the sink's EDID: the same bracket and registers as
/// [`endpoint_configure`], but the speakers, the audio descriptors (by
/// format code), the manufacturer and product, and the monitor's name come
/// from `sink`.
pub fn endpoint_configure_for<W: Window + ?Sized>(w: &W, sink: &edid::Edid) {
    update(w, CODEC_RATES, 0xfff, 0x70);
    let hotplug = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, hotplug | 1);
    endpoint_write(
        w,
        EP_CHANNEL_SPEAKER,
        u32::from(sink.audio.speakers) | (1 << 16),
    );
    for (format, d) in (1u32..).zip(sink.audio.descriptors.iter().skip(1)) {
        endpoint_write(w, EP_AUDIO_DESCRIPTOR + format, descriptor_word(*d));
    }
    endpoint_write(
        w,
        EP_SINK_INFO,
        u32::from(sink.manufacturer) | u32::from(sink.product) << 16,
    );
    let name = sink.name.as_deref().unwrap_or("").as_bytes();
    let name = &name[..name.len().min(18)];
    endpoint_write(w, EP_SINK_INFO + 1, u32::try_from(name.len()).unwrap_or(0));
    for (i, chunk) in (0u32..).zip(name.chunks(4)) {
        let mut b = [0u8; 4];
        b[..chunk.len()].copy_from_slice(chunk);
        endpoint_write(w, EP_SINK_INFO + 4 + i, u32::from_le_bytes(b));
    }
    let hotplug = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, hotplug & !1);
}

/// Step 3: the audio clock for 148.5 MHz, in the order the specification
/// insists on: the source first, then the module, then the phase.
pub fn audio_clock<W: Window + ?Sized>(w: &W) {
    update(w, AUDIO_DTO_SOURCE, 0x77, 0);
    w.write32(AUDIO_DTO0_MODULE, PIXEL_CLOCK_100HZ);
    w.write32(AUDIO_DTO0_PHASE, AUDIO_REFERENCE_100HZ);
}

/// Step 4a: the encoder into HDMI mode (08 section 4.5): the back-end mode,
/// the clock pattern and a pulse of `DIG_START` (which resets the front-end
/// FIFO), then the stream controls: packet generator version 1, keep-out
/// mode, no extra null packets, no deep colour, no scrambling.
///
/// This disturbs the stream while the FIFO resets.
pub fn video_mode<W: Window + ?Sized>(w: &W, pause_us: &dyn Fn(u32)) {
    update(w, DIG_BE_CNTL, 7 << 16, 3 << 16);
    w.write32(DIG_CLOCK_PATTERN, 0x1f);
    update(w, DIG_FE_CNTL, 1 << 10, 1 << 10);
    pause_us(1);
    update(w, DIG_FE_CNTL, 1 << 10, 0);
    pause_us(1);
    update(
        w,
        HDMI_CONTROL,
        (1 << 24) | (3 << 28) | (1 << 4) | (1 << 3) | (1 << 2) | (1 << 1) | 1,
        (1 << 4) | (1 << 3) | 1,
    );
}

/// The AVI info frame for 1920x1080p60, RGB, 16:9, VIC 16 (08 section 4.5):
/// the header dword and the eight body dwords.
pub const AVI_1080P60: [u32; 9] = [0x000d_0282, 0x0028_1027, 0x0000_0010, 0, 0, 0, 0, 0, 0];

/// The AVI info frame for video identification code `vic`, RGB, with the
/// aspect (08 section 4.5 and 3.7.6): header `0x000d0282`; PB1 `0x10`, PB2
/// the aspect (`0x28` for 16:9, `0x18` for 4:3), PB3 0, PB4 the code; PB0 the
/// checksum that makes the header and body sum to 0 modulo 256.
#[must_use]
pub fn avi_for(vic: u8, wide: bool) -> [u32; 9] {
    let (pb1, pb2, pb3, pb4) = (0x10u8, if wide { 0x28u8 } else { 0x18 }, 0u8, vic);
    let sum = 0x82u8
        .wrapping_add(0x02)
        .wrapping_add(0x0d)
        .wrapping_add(pb1)
        .wrapping_add(pb2)
        .wrapping_add(pb3)
        .wrapping_add(pb4);
    let pb0 = 0u8.wrapping_sub(sum);
    [
        0x000d_0282,
        u32::from_le_bytes([pb0, pb1, pb2, pb3]),
        u32::from(pb4),
        0,
        0,
        0,
        0,
        0,
        0,
    ]
}

/// Writes the packet at index `n` (the dwords at `n * 9`), as the reference
/// does: wait for no conflict, clear it, set the index, write, ask for an
/// immediate update. Returns whether the conflict flag cleared in time.
pub fn write_packet<W: Window + ?Sized>(
    w: &W,
    n: u32,
    dwords: &[u32; 9],
    pause_us: &dyn Fn(u32),
) -> bool {
    let mut clear = false;
    for _ in 0..50 {
        if w.read32(VPG_STATUS) & 2 == 0 {
            clear = true;
            break;
        }
        pause_us(10);
    }
    w.write32(VPG_STATUS, 1 << 4);
    w.write32(VPG_ACCESS, n * PACKET_DWORDS);
    for d in dwords {
        w.write32(VPG_DATA, *d);
    }
    update(w, VPG_IMMEDIATE, 1 << n, 1 << n);
    clear
}

/// Step 4b: the AVI info frame, packet 0.
pub fn avi_info_frame<W: Window + ?Sized>(w: &W, pause_us: &dyn Fn(u32)) -> bool {
    write_packet(w, 0, &AVI_1080P60, pause_us)
}

/// Step 4c: the packet controls: the double buffer off, the general-control
/// and null packets, the audio info frame on line 2, AV-mute off, packet 0
/// (the AVI) sent every frame on line 2.
pub fn packet_controls<W: Window + ?Sized>(w: &W) {
    update(w, DB_CONTROL, 1 << 12, 1 << 12);
    update(
        w,
        VBI_PACKET_CONTROL,
        (1 << 4) | (1 << 5) | 1 | (1 << 12),
        (1 << 4) | (1 << 5) | 1,
    );
    update(w, INFOFRAME_CONTROL0, 1 << 4, 1 << 4);
    update(w, INFOFRAME_CONTROL1, 0x3f00, 2 << 8);
    update(w, HDMI_GC, 1, 0);
    update(w, GENERIC_PACKET_CONTROL1, 0xffff, 2);
    update(w, GENERIC_PACKET_CONTROL0, 3, 3);
}

/// Step 6: the endpoint's "audio enabled": hot-plug control bits 0 and 31
/// set, then bit 0 cleared (08 section 4.6).
pub fn endpoint_enable<W: Window + ?Sized>(w: &W) {
    let h = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, h | 1 | (1 << 31));
    let h = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, h & !1);
}

/// Step 7: unmute: update the channel status, send audio samples.
pub fn unmute<W: Window + ?Sized>(w: &W) {
    update(w, AFMT_AUDIO_PACKET_CONTROL, (1 << 26) | 1, (1 << 26) | 1);
}

/// The reverse of [`unmute`] and [`endpoint_enable`] (08 section 4.6).
pub fn mute_and_disable<W: Window + ?Sized>(w: &W) {
    update(w, AFMT_AUDIO_PACKET_CONTROL, 1, 0);
    let h = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, h | 1);
    let h = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, h & !(1 << 31));
    let h = endpoint_read(w, EP_HOTPLUG);
    endpoint_write(w, EP_HOTPLUG, h & !1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::vec;
    use std::vec::Vec;

    /// Registers, endpoint registers behind the index/data pair, the packet
    /// memory behind the access/data pair, and a log of writes.
    struct Model {
        regs: RefCell<BTreeMap<usize, u32>>,
        endpoint: RefCell<BTreeMap<u32, u32>>,
        packets: RefCell<Vec<u32>>,
        cursor: RefCell<usize>,
        log: RefCell<Vec<(usize, u32)>>,
    }

    impl Model {
        fn new() -> Self {
            Self {
                regs: RefCell::new(BTreeMap::new()),
                endpoint: RefCell::new(BTreeMap::new()),
                packets: RefCell::new(vec![0; 15 * 9]),
                cursor: RefCell::new(0),
                log: RefCell::new(Vec::new()),
            }
        }
        fn get(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
        fn set(&self, o: usize, v: u32) {
            self.regs.borrow_mut().insert(o, v);
        }
        fn ep(&self, i: u32) -> u32 {
            *self.endpoint.borrow().get(&i).unwrap_or(&0)
        }
        fn wrote(&self, o: usize) -> Vec<u32> {
            self.log
                .borrow()
                .iter()
                .filter(|(r, _)| *r == o)
                .map(|(_, v)| *v)
                .collect()
        }
    }

    impl Window for Model {
        fn read32(&self, o: usize) -> u32 {
            if o == ENDPOINT_DATA {
                return self.ep(self.get(ENDPOINT_INDEX));
            }
            self.get(o)
        }
        fn write32(&self, o: usize, v: u32) {
            self.log.borrow_mut().push((o, v));
            match o {
                ENDPOINT_DATA => {
                    let i = self.get(ENDPOINT_INDEX);
                    self.endpoint.borrow_mut().insert(i, v);
                }
                VPG_ACCESS => *self.cursor.borrow_mut() = v as usize,
                VPG_DATA => {
                    let c = *self.cursor.borrow();
                    self.packets.borrow_mut()[c] = v;
                    *self.cursor.borrow_mut() += 1;
                    self.set(o, v);
                }
                _ => self.set(o, v),
            }
        }
    }

    #[test]
    fn the_acr_values_are_the_hdmi_tables() {
        assert_eq!(acr_cts(148_500), 0x2441_4000);
        assert_eq!(acr_cts(165_000), 0x2848_8000);
        let m = Model::new();
        audio_encoder(&m, 0);
        assert_eq!(m.get(ACR_48.0), 0x2441_4000);
        assert_eq!(m.get(ACR_48.1) & 0xfffff, 0x1800);
        assert_eq!(m.get(ACR_32.1) & 0xfffff, 4096);
        assert_eq!(m.get(ACR_44_1.1) & 0xfffff, 6272);
        assert_eq!(
            m.get(ACR_PACKET_CONTROL) & ((1 << 12) | (1 << 8) | (1 << 31)),
            1 << 12
        );
    }

    #[test]
    fn the_audio_encoder_keeps_other_bits_and_routes_the_endpoint() {
        let m = Model::new();
        m.set(AFMT_AUDIO_SRC_CONTROL, 0xf0);
        m.set(AFMT_MEM_PWR, 0x30);
        m.set(AFMT_AUDIO_PACKET_CONTROL2, 0x1000_0001);
        audio_encoder(&m, 3);
        assert_eq!(m.get(AFMT_AUDIO_SRC_CONTROL), 0xf3);
        assert_eq!(m.get(AFMT_MEM_PWR), 0);
        assert_eq!(m.get(AFMT_AUDIO_PACKET_CONTROL2), 0x0300);
        assert_eq!(m.get(AFMT_CNTL) & 1, 1);
        assert_eq!(m.get(AFMT_60958_0) >> 20 & 0xf, 1);
        assert_eq!(m.get(AFMT_60958_1) >> 20 & 0xf, 2);
    }

    #[test]
    fn the_audio_clock_is_written_source_first() {
        let m = Model::new();
        m.set(AUDIO_DTO_SOURCE, 0x77);
        audio_clock(&m);
        let order: Vec<usize> = m.log.borrow().iter().map(|(r, _)| *r).collect();
        assert_eq!(
            order,
            [AUDIO_DTO_SOURCE, AUDIO_DTO0_MODULE, AUDIO_DTO0_PHASE]
        );
        assert_eq!(m.get(AUDIO_DTO_SOURCE), 0);
        assert_eq!(m.get(AUDIO_DTO0_MODULE), 0x16_a8c8);
        assert_eq!(m.get(AUDIO_DTO0_PHASE), 0x3_a980);
    }

    #[test]
    fn the_endpoint_is_configured_inside_the_clock_gating_bracket() {
        let m = Model::new();
        m.endpoint.borrow_mut().insert(EP_HOTPLUG, 0x8000_0000);
        endpoint_configure(&m);
        assert_eq!(m.ep(EP_CHANNEL_SPEAKER), 0x0001_0001);
        assert_eq!(m.ep(EP_AUDIO_DESCRIPTOR + 1), 0x0007_0701);
        // The bracket: bit 0 set before the writes, cleared after, bit 31 kept.
        assert_eq!(m.ep(EP_HOTPLUG), 0x8000_0000);
        let hot: Vec<u32> = m
            .log
            .borrow()
            .windows(2)
            .filter(|p| p[0] == (ENDPOINT_INDEX, EP_HOTPLUG) && p[1].0 == ENDPOINT_DATA)
            .map(|p| p[1].1)
            .collect();
        assert_eq!(hot, [0x8000_0001, 0x8000_0000]);
        assert_eq!(m.get(CODEC_RATES) & 0xfff, 0x70);
    }

    #[test]
    fn the_endpoint_gets_the_sinks_edid_data() {
        let m = Model::new();
        let mut sink = edid_sample();
        endpoint_configure_for(&m, &sink);
        assert_eq!(m.ep(EP_CHANNEL_SPEAKER), 0x0001_0001);
        assert_eq!(m.ep(EP_AUDIO_DESCRIPTOR + 1), 0x0007_0701);
        assert_eq!(m.ep(EP_AUDIO_DESCRIPTOR + 2), 0x0050_0705);
        assert_eq!(m.ep(EP_AUDIO_DESCRIPTOR + 3), 0);
        assert_eq!(m.ep(EP_SINK_INFO), 0x1234_ac10);
        assert_eq!(m.ep(EP_SINK_INFO + 1), 7);
        assert_eq!(m.ep(EP_SINK_INFO + 4), u32::from_le_bytes(*b"TEST"));
        assert_eq!(
            m.ep(EP_SINK_INFO + 5),
            u32::from_le_bytes([b'M', b'O', b'N', 0])
        );
        // The bracket closes with the clock gating as it was.
        assert_eq!(m.ep(EP_HOTPLUG), 0);
        // A name longer than the endpoint holds is cut to 18 characters.
        sink.name = Some("ABCDEFGHIJKLMNOPQRSTUVWXYZ".into());
        endpoint_configure_for(&m, &sink);
        assert_eq!(m.ep(EP_SINK_INFO + 1), 18);
    }

    fn edid_sample() -> edid::Edid {
        let mut audio = edid::Audio::default();
        audio.descriptors[1] = [9, 0x07, 0x07];
        audio.descriptors[2] = [(2 << 3) | 5, 0x07, 0x50];
        audio.speakers = 1;
        edid::Edid {
            name: Some("TESTMON".into()),
            size_mm: (0, 0),
            modes: Vec::new(),
            extensions: 1,
            bpc: None,
            manufacturer: 0xac10,
            product: 0x1234,
            audio,
        }
    }

    #[test]
    fn the_endpoint_enable_sets_bits_zero_and_thirty_one_then_clears_zero() {
        let m = Model::new();
        endpoint_enable(&m);
        assert_eq!(m.ep(EP_HOTPLUG), 0x8000_0000);
        let data: Vec<u32> = m.wrote(ENDPOINT_DATA);
        assert_eq!(data, [0x8000_0001, 0x8000_0000]);
        mute_and_disable(&m);
        assert_eq!(m.ep(EP_HOTPLUG), 0);
    }

    #[test]
    fn the_video_mode_switch_changes_only_its_bits_and_pulses_dig_start() {
        let m = Model::new();
        // As the firmware left it.
        m.set(DIG_BE_CNTL, 0x0002_0100);
        m.set(DIG_FE_CNTL, 0x0100_0000);
        m.set(HDMI_CONTROL, 0x0001_0011);
        video_mode(&m, &|_| {});
        assert_eq!(m.get(DIG_BE_CNTL), 0x0003_0100);
        assert_eq!(m.get(DIG_CLOCK_PATTERN), 0x1f);
        assert_eq!(m.wrote(DIG_FE_CNTL), [0x0100_0400, 0x0100_0000]);
        assert_eq!(m.get(HDMI_CONTROL), 0x0001_0019);
    }

    #[test]
    fn the_avi_packet_goes_to_index_zero_with_the_specifications_bytes() {
        let m = Model::new();
        assert!(avi_info_frame(&m, &|_| {}));
        assert_eq!(m.wrote(VPG_ACCESS), [0]);
        let p = m.packets.borrow();
        assert_eq!(p[0], 0x000d_0282);
        assert_eq!(p[1], 0x0028_1027);
        assert_eq!(p[2], 0x10);
        assert!(p[3..9].iter().all(|d| *d == 0));
        drop(p);
        // The checksum makes the packet's bytes (header included) sum to 0.
        let sum: u32 = [0x82u32, 0x02, 0x0d, 0x27, 0x10, 0x28, 0x00, 0x10]
            .iter()
            .sum();
        assert_eq!(sum & 0xff, 0);
        assert_eq!(m.get(VPG_IMMEDIATE) & 1, 1);
        // The packet at index 1 lands at dword 9.
        write_packet(&m, 1, &[7; 9], &|_| {});
        assert_eq!(m.wrote(VPG_ACCESS), [0, 9]);
        assert_eq!(m.packets.borrow()[9], 7);
    }

    #[test]
    fn the_avi_frames_for_the_modes_are_the_specifications() {
        // (code, 16:9, checksum, body dword 0, body dword 1)
        for (vic, wide, sum, d0, d1) in [
            (16u8, true, 0x27u32, 0x0028_1027u32, 0x10u32),
            (31, true, 0x18, 0x0028_1018, 0x1f),
            (4, true, 0x33, 0x0028_1033, 0x04),
            (19, true, 0x24, 0x0028_1024, 0x13),
            (3, true, 0x34, 0x0028_1034, 0x03),
            (2, false, 0x45, 0x0018_1045, 0x02),
            (1, false, 0x46, 0x0018_1046, 0x01),
            (18, true, 0x25, 0x0028_1025, 0x12),
            (17, false, 0x36, 0x0018_1036, 0x11),
        ] {
            let f = avi_for(vic, wide);
            assert_eq!(f[0], 0x000d_0282);
            assert_eq!(f[1] & 0xff, sum, "VIC {vic}");
            assert_eq!(f[1], d0, "VIC {vic}");
            assert_eq!(f[2], d1, "VIC {vic}");
            assert!(f[3..].iter().all(|d| *d == 0));
        }
        assert_eq!(avi_for(16, true), AVI_1080P60);
    }

    #[test]
    fn a_packet_write_waits_for_the_conflict_flag() {
        let m = Model::new();
        m.set(VPG_STATUS, 2);
        let waited = RefCell::new(0u32);
        let ok = write_packet(&m, 0, &AVI_1080P60, &|us| *waited.borrow_mut() += us);
        assert!(!ok);
        assert_eq!(*waited.borrow(), 500);
        // The conflict flag is cleared by writing bit 4 either way.
        assert_eq!(m.wrote(VPG_STATUS), [1 << 4]);
    }

    #[test]
    fn the_packet_controls_send_the_avi_every_frame_on_line_two() {
        let m = Model::new();
        packet_controls(&m);
        assert_eq!(m.get(GENERIC_PACKET_CONTROL0) & 3, 3);
        assert_eq!(m.get(GENERIC_PACKET_CONTROL1) & 0xffff, 2);
        assert_eq!(m.get(DB_CONTROL) >> 12 & 1, 1);
        assert_eq!(m.get(VBI_PACKET_CONTROL), 0x31);
        assert_eq!(m.get(INFOFRAME_CONTROL1) >> 8 & 0x3f, 2);
        assert_eq!(m.get(HDMI_GC) & 1, 0);
    }

    #[test]
    fn unmute_sets_the_status_update_and_sample_send() {
        let m = Model::new();
        unmute(&m);
        assert_eq!(m.get(AFMT_AUDIO_PACKET_CONTROL), (1 << 26) | 1);
    }
}
