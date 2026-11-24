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
//! `~/Projects/OS/chinese-wall/specs/nvidia-nova` (sections 01 and 02). The
//! unit tests use images built from that specification. The Video BIOS
//! reader has also read the RTX 4070's own ROM on the X870E (2026-11-24: four
//! images, and a version 3 descriptor for the GPU system processor with two
//! signatures). The firmware container has not met a real firmware file.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod chip;
pub mod fuse;
pub mod mem;
pub mod nvfw;
pub mod vbios;
