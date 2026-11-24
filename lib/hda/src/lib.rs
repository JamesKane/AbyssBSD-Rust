// SPDX-License-Identifier: BSD-2-Clause
//! High Definition Audio, without hardware: the controller's registers,
//! the verbs a codec is asked, and the widget graph read out of a codec.
//!
//! Everything a codec answers is device data and untrusted: counts are
//! bounded, node numbers are checked, and a codec that stops answering is
//! an error, never a panic.
//!
//! Layouts follow Intel's High Definition Audio specification 1.0a; the
//! behavior (what FreeBSD asks, and in what order) follows 15.1's
//! `sys/dev/sound/pci/hda`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod graph;
pub mod reg;
pub mod verb;

pub use graph::{Codec, Function, GraphError, Widget, WidgetKind, read_function};
