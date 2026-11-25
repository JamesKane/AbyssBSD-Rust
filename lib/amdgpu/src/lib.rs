// SPDX-License-Identifier: MIT
// Copyright 2018 Advanced Micro Devices, Inc.
// Copyright 2018-2024 Advanced Micro Devices, Inc. All rights reserved.
// Copyright (C) 2018  Advanced Micro Devices, Inc.
// Copyright (c) 2026 The AbyssBSD Project
//! AMD GPUs of the RDNA2 generation, without hardware: how a chip names
//! itself in its IP discovery table, the segment bases of its blocks, and
//! what is read from it before any driver programs it.
//!
//! The layouts come from the clean-room specification
//! `~/Projects/OS/chinese-wall/specs/amd-rdna2` (section 01), whose sources
//! are MIT-licensed files of AMD's driver, so these files are MIT too with
//! AMD's notice kept.
//!
//! Everything read from the device is untrusted: every offset and count is
//! checked against the table's own size, and a malformed table is an error,
//! never a panic.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod atom;
pub mod ddc;
pub mod discovery;
pub mod display;
mod display_regs;
pub mod dmcub;
pub mod doorbell;
pub mod flip;
pub mod gc;
mod gc_tables;
pub mod gcfw;
pub mod gmc;
pub mod hdmi;
pub mod hpd;
pub mod ih;
pub mod info;
pub mod kiq;
pub mod memory;
mod memory_regs;
pub mod modeset;
pub mod mqd;
pub mod pm4;
pub mod psp;
pub mod regs;
pub mod scanout;
pub mod sdma;
pub mod smu;
pub mod timing;
pub mod uapi;
pub mod vbios;
pub mod vm;
