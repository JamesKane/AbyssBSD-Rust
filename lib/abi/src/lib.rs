// SPDX-License-Identifier: BSD-2-Clause
//! The FreeBSD userland ABI: syscall numbers, and the layouts of structures
//! that cross the user/kernel boundary.
//!
//! Every structure here mirrors a FreeBSD C definition exactly. Each one is
//! listed in [`layout::LAYOUTS`], and each ABI constant in
//! [`layout::CONSTANTS`], which `cargo xtask abi-check` turns into C static
//! assertions and compiles against real FreeBSD headers, so a mismatch fails
//! CI instead of corrupting user memory.
//!
//! See docs/COMPATIBILITY.md.

#![no_std]
#![forbid(unsafe_code)]

pub mod bus;
pub mod errno;
pub mod exec;
pub mod layout;
pub mod mman;
pub mod resource;
pub mod signal;
pub mod stat;
pub mod sysarch;
pub mod syscall;
pub mod sysctl;
pub mod thr;
pub mod time;
pub mod uio;
pub mod vdso;
