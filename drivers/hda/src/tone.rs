// SPDX-License-Identifier: BSD-2-Clause
//! A test tone through a codec: find an output pin and the converter behind
//! it, point the converter at an output stream, run the stream on a looped
//! 1 kHz sine at 48 kHz, 16 bits, stereo, and log what the hardware shows
//! (the link position moving, the pin's presence, the stream status).
//!
//! This is the stream half of docs/AUDIO.md stage A2, with no sound engine:
//! it proves a codec's path from memory to the connector (for the Radeon's
//! HDMI codec, to the monitor) and is the driver a `PcmHw` later wraps.
//!
//! Behavior reference: sys/dev/sound/pci/hda/hdaa.c (15.1), `hdaa_audio_setup`
//! and `hdaa_audio_ctl_amp_set`.

use alloc::vec::Vec;

use hda::stream;
use hda::verb;
use hda::{Function, Widget, WidgetKind};
use sched::Sleepable;

use crate::controller::Link;

/// What a path is: the pin, the nodes between (pin first, converter last).
struct Path {
    nodes: Vec<u8>,
}

/// Pin capabilities: HDMI (bit 7) or DisplayPort (bit 24).
fn is_digital_pin(cap: u32) -> bool {
    cap & (1 << 7) != 0 || cap & (1 << 24) != 0
}

/// Widget capabilities: digital (bit 9), output amp present (bit 2), input
/// amp present (bit 1), amp parameters override (bit 3).
const CAP_DIGITAL: u32 = 1 << 9;
const CAP_OUT_AMP: u32 = 1 << 2;
const CAP_IN_AMP: u32 = 1 << 1;
const CAP_AMP_OVERRIDE: u32 = 1 << 3;

/// Finds the converter behind `pin` by following the first connection of
/// each node (a selector or mixer between them is set to that input), at
/// most four steps. Returns the nodes from the pin to the converter.
fn path_from(f: &Function, pin: &Widget) -> Option<Path> {
    let mut nodes = alloc::vec![pin.nid];
    let mut at = pin;
    for _ in 0..4 {
        let next = f.widget(*at.connections.first()?)?;
        nodes.push(next.nid);
        if next.kind == WidgetKind::Output {
            return Some(Path { nodes });
        }
        at = next;
    }
    None
}

/// The amplifier's 0 dB step: the offset field of the amplifier
/// capabilities (parameter `0x12` output, `0x0d` input), asked of the widget
/// or, if it defers, of the function group.
fn amp_offset(link: &Link, s: &Sleepable, w: &Widget, afg: u8, param: u8) -> u8 {
    let from = if w.caps & CAP_AMP_OVERRIDE != 0 {
        w.nid
    } else {
        afg
    };
    link.command(s, verb::get_parameter(link.cad, from, param))
        .map_or(0, |c| (c & 0x7f) as u8)
}

/// What a tone run saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The link position moved between two reads 100 ms apart.
    pub moved: bool,
    /// How many times the position wrapped to the start of the buffer.
    pub wraps: u32,
}

/// Plays the tone for `seconds` through the best output pin of `function`,
/// logging each step with `log`.
pub(crate) fn play(
    s: &Sleepable,
    link: &Link,
    function: &Function,
    seconds: u32,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> Report {
    let cad = link.cad;
    let Some(ctlr) = link.controller() else {
        log(format_args!("tone: the controller is gone"));
        return Report::default();
    };
    // The pins, asked again now: a display endpoint that the display driver
    // enabled after attach has new capabilities and a new presence.
    let mut best: Option<(u8, bool, bool)> = None; // (nid, digital, present)
    for w in function
        .widgets
        .iter()
        .filter(|w| w.kind == WidgetKind::Pin)
    {
        let cap = link
            .command(s, verb::get_parameter(cad, w.nid, stream::PARAM_PIN_CAP))
            .unwrap_or(0);
        let sense = link
            .command(s, stream::get_pin_sense(cad, w.nid))
            .unwrap_or(0);
        let digital = is_digital_pin(cap);
        let present = sense & stream::SENSE_PRESENCE != 0;
        let output = cap & (1 << 4) != 0;
        log(format_args!(
            "tone: pin {}: capabilities {cap:#010x} ({}{}), sense {sense:#010x} (display {}, ELD {}), configuration {:#010x}",
            w.nid,
            if digital { "digital" } else { "analog" },
            if output { ", output" } else { "" },
            if present { "present" } else { "absent" },
            if sense & stream::SENSE_ELD_VALID != 0 {
                "valid"
            } else {
                "invalid"
            },
            w.config_default
        ));
        if !output || path_from(function, w).is_none() {
            continue;
        }
        // A present digital pin first, then any present pin, then a digital
        // one, then the first.
        let rank = |d: bool, p: bool| u8::from(d) + 2 * u8::from(p);
        if best.is_none_or(|(_, bd, bp)| rank(digital, present) > rank(bd, bp)) {
            best = Some((w.nid, digital, present));
        }
    }
    let Some((pin_nid, digital, present)) = best else {
        log(format_args!(
            "tone: no output pin with a converter behind it"
        ));
        return Report::default();
    };
    let pin = function.widget(pin_nid).expect("chosen from the function");
    let Some(path) = path_from(function, pin) else {
        return Report::default();
    };
    let conv_nid = *path.nodes.last().expect("a path has nodes");
    let conv = function.widget(conv_nid).expect("on the path");
    log(format_args!(
        "tone: pin {pin_nid} ({}, display {}) through {:?} to converter {conv_nid}",
        if digital { "digital" } else { "analog" },
        if present { "present" } else { "absent" },
        path.nodes
    ));
    // The converter's supported rates and formats.
    let rates = link.command(
        s,
        verb::get_parameter(cad, conv_nid, stream::PARAM_PCM_SIZE_RATE),
    );
    let formats = link.command(
        s,
        verb::get_parameter(cad, conv_nid, stream::PARAM_STREAM_FORMATS),
    );
    log(format_args!(
        "tone: converter {conv_nid}: PCM sizes and rates {rates:#x?}, stream formats {formats:#x?}"
    ));
    let Some(format) = stream::format(48_000, 16, 2) else {
        return Report::default();
    };
    // The nodes on the path: power up, amplifiers open, selectors set.
    for (i, &nid) in path.nodes.iter().enumerate() {
        let Some(w) = function.widget(nid) else {
            continue;
        };
        let _ = link.command(s, verb::set_power_state(cad, nid, verb::POWER_D0));
        if w.caps & CAP_OUT_AMP != 0 {
            let g = amp_offset(link, s, w, function.nid, 0x12);
            let _ = link.command(s, stream::set_output_amp(cad, nid, g));
        }
        if w.caps & CAP_IN_AMP != 0 && w.kind != WidgetKind::Output {
            // The input the path comes in on: the first connection.
            let g = amp_offset(link, s, w, function.nid, 0x0d);
            let _ = link.command(s, stream::set_input_amp(cad, nid, 0, g));
        }
        if i + 1 < path.nodes.len() && w.connections.len() > 1 {
            let _ = link.command(s, stream::set_connection_select(cad, nid, 0));
        }
    }
    // The pin: output on; an analog pin also drives its amplifier (EAPD).
    let _ = link.command(
        s,
        stream::set_pin_control(cad, pin_nid, stream::PIN_OUT_ENABLE),
    );
    let _ = link.command(s, stream::set_eapd(cad, pin_nid, 2));
    // The buffer: a whole number of tone periods.
    let data = stream::tone_buffer(160, 2);
    let play = match ctlr.play(&data, format) {
        Ok(p) => p,
        Err(e) => {
            log(format_args!("tone: the stream did not start: {e}"));
            return Report::default();
        }
    };
    // The converter listens to the stream, in the format, channel 0.
    let _ = link.command(s, stream::set_converter_format(cad, conv_nid, format));
    if conv.caps & CAP_DIGITAL != 0 {
        let _ = link.command(
            s,
            stream::set_digital_converter(cad, conv_nid, stream::DIGEN),
        );
    }
    let _ = link.command(s, stream::set_stream_channel(cad, conv_nid, play.tag, 0));
    // The HDMI pin: the channel slots, then the audio infoframe, as hdaa does.
    if digital {
        for ch in 0..2u8 {
            let _ = link.command(s, stream::set_hdmi_channel_slot(cad, pin_nid, ch, ch));
        }
        let _ = link.command(s, stream::set_hdmi_dip_index(cad, pin_nid, 0));
        let _ = link.command(s, stream::set_hdmi_dip_xmit(cad, pin_nid, 0));
        let _ = link.command(s, stream::set_hdmi_dip_index(cad, pin_nid, 0));
        for _ in 0..32 {
            let _ = link.command(s, stream::set_hdmi_dip_data(cad, pin_nid, 0));
        }
        let _ = link.command(s, stream::set_hdmi_dip_index(cad, pin_nid, 0));
        for b in stream::audio_infoframe(2) {
            let _ = link.command(s, stream::set_hdmi_dip_data(cad, pin_nid, b));
        }
        let _ = link.command(s, stream::set_hdmi_dip_index(cad, pin_nid, 0));
        let _ = link.command(s, stream::set_hdmi_dip_xmit(cad, pin_nid, 0xc0));
    }
    log(format_args!(
        "tone: stream {} running: control {:#04x}, status {:#04x}, {} bytes, format {format:#06x}",
        play.tag,
        play.control(),
        play.status(),
        play.bytes
    ));
    // What the codec says it was told.
    let ask = |v: u32| link.command(s, v);
    log(format_args!(
        "tone: readback: converter {conv_nid}: format {:#x?}, stream/channel {:#x?}, digital {:#x?}, power {:#x?}; pin {pin_nid}: control {:#x?}, power {:#x?}",
        ask(stream::get_converter_format(cad, conv_nid)),
        ask(stream::get_stream_channel(cad, conv_nid)),
        ask(stream::get_digital_converter(cad, conv_nid)),
        ask(stream::get_power_state(cad, conv_nid)),
        ask(stream::get_pin_control(cad, pin_nid)),
        ask(stream::get_power_state(cad, pin_nid)),
    ));
    // The ladder: does the link position advance, and does it wrap?
    let queue = sched::WaitQueue::new();
    let wait = |ms: u64| {
        let _ = bus::manager().wait_timeout(s, &queue, &mut || false, ms * 1_000_000);
    };
    let first = play.position();
    wait(100);
    let second = play.position();
    log(format_args!(
        "tone: link position {first} then {second} after 100 ms (a 48 kHz stereo stream moves 19,200 bytes in 100 ms)"
    ));
    let moved = second != first;
    let mut wraps = 0u32;
    let mut last = second;
    let polls = u64::from(seconds.clamp(1, 20)) * 10;
    for _ in 0..polls {
        wait(100);
        let now = play.position();
        if now < last {
            wraps += 1;
        }
        last = now;
    }
    // The pin's sense once more, with the stream running.
    let sense = link
        .command(s, stream::get_pin_sense(cad, pin_nid))
        .unwrap_or(0);
    log(format_args!(
        "tone: after {seconds} s: position {last}, {wraps} wraps of the buffer, status {:#04x}, pin sense {sense:#010x}",
        play.status()
    ));
    drop(play);
    // Detach the converter and silence the pin.
    let _ = link.command(s, stream::set_stream_channel(cad, conv_nid, 0, 0));
    let _ = link.command(s, stream::set_pin_control(cad, pin_nid, 0));
    log(format_args!("tone: stopped"));
    Report { moved, wraps }
}
