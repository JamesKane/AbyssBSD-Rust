// SPDX-License-Identifier: BSD-2-Clause
//! High Definition Audio, as FreeBSD's `snd_hda` lays it out in the device
//! tree: `hdac` (the controller, on PCI), `hdacc` (a codec on its link), and
//! `hdaa` (the codec's audio function group).
//!
//! This is the controller and the codec's graph. Streams, the mixer and the
//! `pcm` devices wait for the sound engine (docs/AUDIO.md, stage A2).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod codec;
mod controller;
mod names;

pub use codec::{HDAA, HDACC};
pub use controller::HDAC;
