// SPDX-License-Identifier: BSD-2-Clause
//! A function group's widget graph, read through a [`Codec`].
//!
//! The walk asks for each widget's capabilities, its connection list, and
//! (for a pin) its default configuration, as FreeBSD's `hdaa` does at the
//! start of its attach. Association parsing and the devices built from the
//! graph come after.

use alloc::vec::Vec;

use crate::verb::{self, INVALID};

/// How a codec is asked: one verb, one response.
pub trait Codec {
    /// Sends `verb` (with the codec address in place) and returns its
    /// response, or `None` if none came.
    fn command(&mut self, verb: u32) -> Option<u32>;
}

/// Why a graph could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// The codec stopped answering, or answered all ones.
    NoResponse {
        /// The verb that went unanswered.
        verb: u32,
    },
    /// The function group has more subordinate nodes than node numbers.
    TooManyWidgets,
}

/// What a widget is (audio widget capabilities, bits 23:20).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetKind {
    /// An audio output converter.
    Output,
    /// An audio input converter.
    Input,
    /// A mixer.
    Mixer,
    /// A selector.
    Selector,
    /// A pin complex.
    Pin,
    /// A power widget.
    Power,
    /// A volume knob.
    VolumeKnob,
    /// A beep generator.
    BeepGenerator,
    /// A vendor-defined widget.
    Vendor,
    /// A type the specification reserves.
    Reserved(u8),
}

impl WidgetKind {
    /// The kind widget capabilities `caps` name.
    #[must_use]
    pub const fn of(caps: u32) -> Self {
        match (caps >> 20) as u8 & 0xf {
            0 => Self::Output,
            1 => Self::Input,
            2 => Self::Mixer,
            3 => Self::Selector,
            4 => Self::Pin,
            5 => Self::Power,
            6 => Self::VolumeKnob,
            7 => Self::BeepGenerator,
            0xf => Self::Vendor,
            n => Self::Reserved(n),
        }
    }
}

/// One widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Widget {
    /// Its node number.
    pub nid: u8,
    /// What it is.
    pub kind: WidgetKind,
    /// Its capabilities, as the codec reported them.
    pub caps: u32,
    /// The nodes it can take its input from.
    pub connections: Vec<u8>,
    /// A pin's default configuration (zero for any other widget).
    pub config_default: u32,
}

/// An audio function group and its widgets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    /// Its node number.
    pub nid: u8,
    /// Its type (`FCT_GRP_AUDIO`, say).
    pub kind: u8,
    /// Its subsystem ID.
    pub subsystem: u32,
    /// Its widgets, by node number.
    pub widgets: Vec<Widget>,
}

impl Function {
    /// The widget with node number `nid`.
    #[must_use]
    pub fn widget(&self, nid: u8) -> Option<&Widget> {
        self.widgets.iter().find(|w| w.nid == nid)
    }
}

fn ask(codec: &mut dyn Codec, verb: u32) -> Result<u32, GraphError> {
    match codec.command(verb) {
        Some(r) if r != INVALID => Ok(r),
        _ => Err(GraphError::NoResponse { verb }),
    }
}

/// The most entries a connection list is taken to have: its length field
/// is seven bits, and a range entry could otherwise expand without bound.
const CONN_MAX: usize = 127;

/// A widget's connection list (`hdaa_widget_parse`'s `CONN_LIST_LEN` and
/// `GET_CONN_LIST_ENTRY` loop): entries are node numbers, and an entry
/// with its top bit set stands for every node from the one before it, plus
/// one, up to its own number.
fn connections(codec: &mut dyn Codec, cad: u8, nid: u8) -> Result<Vec<u8>, GraphError> {
    let len = ask(
        codec,
        verb::get_parameter(cad, nid, verb::PARAM_CONN_LIST_LEN),
    )?;
    let long = len & 0x80 != 0;
    let count = (len & 0x7f) as usize;
    let (per_response, bits) = if long { (2, 16) } else { (4, 8) };
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < count {
        let r = ask(codec, verb::get_conn_list_entry(cad, nid, i as u8))?;
        for k in 0..per_response {
            if i + k >= count {
                break;
            }
            let e = r >> (bits * k) & ((1 << bits) - 1);
            let (range, n) = if long {
                (e & 0x8000 != 0, (e & 0x7fff) as u16)
            } else {
                (e & 0x80 != 0, (e & 0x7f) as u16)
            };
            // Node numbers are eight bits here; a larger one names nothing.
            let Ok(n) = u8::try_from(n) else { continue };
            match (range, out.last().copied()) {
                (true, Some(prev)) if n > prev => {
                    for m in prev + 1..=n {
                        if out.len() >= CONN_MAX {
                            break;
                        }
                        out.push(m);
                    }
                }
                _ if out.len() < CONN_MAX => out.push(n),
                _ => {}
            }
        }
        i += per_response;
    }
    Ok(out)
}

/// Reads function group `nid` of codec `cad`: its type and subsystem ID,
/// then every widget under it.
///
/// # Errors
///
/// [`GraphError::NoResponse`] if the codec does not answer a question the
/// walk needs.
pub fn read_function(codec: &mut dyn Codec, cad: u8, nid: u8) -> Result<Function, GraphError> {
    let kind = verb::fct_grp_type(ask(
        codec,
        verb::get_parameter(cad, nid, verb::PARAM_FCT_GRP_TYPE),
    )?);
    // Not every codec answers the subsystem ID; FreeBSD takes what comes.
    let subsystem = codec
        .command(verb::get_subsystem_id(cad, nid))
        .unwrap_or(INVALID);
    let sub = ask(
        codec,
        verb::get_parameter(cad, nid, verb::PARAM_SUB_NODE_COUNT),
    )?;
    let (start, total) = (
        u16::from(verb::sub_node_start(sub)),
        u16::from(verb::sub_node_total(sub)),
    );
    if start + total > 256 {
        return Err(GraphError::TooManyWidgets);
    }
    let mut widgets = Vec::with_capacity(total as usize);
    for n in start..start + total {
        let n = n as u8;
        let caps = ask(
            codec,
            verb::get_parameter(cad, n, verb::PARAM_AUDIO_WIDGET_CAP),
        )?;
        let kind = WidgetKind::of(caps);
        // Bit 8: it has a connection list.
        let connections = if caps & 0x100 != 0 {
            connections(codec, cad, n)?
        } else {
            Vec::new()
        };
        let config_default = if kind == WidgetKind::Pin {
            ask(codec, verb::get_config_default(cad, n))?
        } else {
            0
        };
        widgets.push(Widget {
            nid: n,
            kind,
            caps,
            connections,
            config_default,
        });
    }
    Ok(Function {
        nid,
        kind,
        subsystem,
        widgets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A codec in a table: a function group at node 1 with an output
    /// converter (2), a pin (3) fed by it, and a mixer (4) fed by a range.
    struct Fake {
        asked: Vec<u32>,
        silent_after: usize,
    }

    impl Codec for Fake {
        fn command(&mut self, v: u32) -> Option<u32> {
            self.asked.push(v);
            if self.asked.len() > self.silent_after {
                return None;
            }
            let (nid, verb, payload) = ((v >> 20) as u8, (v >> 8) & 0xfff, v as u8);
            Some(match (nid, verb, payload) {
                (1, 0xf00, 0x05) => 0x0000_0101,
                (1, 0xf00, 0x04) => 0x0002_0003,
                (1, 0xf20, _) => 0x1af4_0022,
                (2, 0xf00, 0x09) => 0x0000_0001,
                (3, 0xf00, 0x09) => 0x0040_0101,
                (3, 0xf00, 0x0e) => 0x01,
                (3, 0xf02, 0) => 0x0000_0002,
                (3, 0xf1c, _) => 0x0221_4010,
                (4, 0xf00, 0x09) => 0x0020_0101,
                // The short form, four entries.
                (4, 0xf00, 0x0e) => 0x04,
                (4, 0xf02, 0) => 0x7f84_0302,
                _ => return None,
            })
        }
    }

    #[test]
    fn reads_a_small_function_group() {
        let mut c = Fake {
            asked: vec![],
            silent_after: usize::MAX,
        };
        let f = read_function(&mut c, 0, 1).unwrap();
        assert_eq!((f.nid, f.kind, f.subsystem), (1, 1, 0x1af4_0022));
        assert_eq!(f.widgets.len(), 3);
        assert_eq!(f.widget(2).unwrap().kind, WidgetKind::Output);
        let pin = f.widget(3).unwrap();
        assert_eq!(pin.kind, WidgetKind::Pin);
        assert_eq!(pin.connections, vec![2]);
        assert_eq!(pin.config_default, 0x0221_4010);
        let mixer = f.widget(4).unwrap();
        assert_eq!(mixer.kind, WidgetKind::Mixer);
        // Entries 02, 03, 84 (a range from the one before, 3, up to 4:
        // just 4), and 7f.
        assert_eq!(mixer.connections, vec![2, 3, 4, 0x7f]);
    }

    #[test]
    fn a_silent_codec_is_an_error() {
        let mut c = Fake {
            asked: vec![],
            silent_after: 3,
        };
        assert!(matches!(
            read_function(&mut c, 0, 1),
            Err(GraphError::NoResponse { .. })
        ));
    }

    #[test]
    fn ranges_expand_from_the_entry_before() {
        struct R;
        impl Codec for R {
            fn command(&mut self, v: u32) -> Option<u32> {
                Some(match (v >> 8) & 0xfff {
                    0xf00 => 0x02,
                    // 5, then a range up to 9.
                    0xf02 => 0x0000_8905,
                    _ => return None,
                })
            }
        }
        assert_eq!(connections(&mut R, 0, 7).unwrap(), vec![5, 6, 7, 8, 9]);
    }

    #[test]
    fn a_hostile_range_is_bounded() {
        struct H;
        impl Codec for H {
            fn command(&mut self, v: u32) -> Option<u32> {
                Some(match (v >> 8) & 0xfff {
                    // Long form, 127 entries.
                    0xf00 => 0xff,
                    _ => 0xffff_ffff,
                })
            }
        }
        // All-ones responses are `INVALID`, so this is an error, not a
        // flood of nodes.
        assert!(connections(&mut H, 0, 7).is_err());
    }
}
