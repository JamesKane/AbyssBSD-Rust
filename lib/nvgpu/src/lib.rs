// SPDX-License-Identifier: BSD-2-Clause
//! NVIDIA GPUs from Turing on (the GeForce RTX 4070, `AD104`, is the first
//! target), without hardware: how a chip is identified, where its register
//! and video memory apertures are, how its Video BIOS is read, and the
//! container its firmware files come in.
//!
//! Everything read from the device or from a firmware file is untrusted:
//! offsets and lengths are checked before they are used, and a malformed
//! input is an error, never a panic.
//!
//! The layouts are from the clean-room specification in
//! `~/Projects/OS/chinese-wall/specs/nvidia-nova` (sections 01 and 02). They
//! have been exercised only against images built from that specification,
//! not against a real card's Video BIOS or firmware files.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod chip;
pub mod nvfw;
pub mod vbios;
