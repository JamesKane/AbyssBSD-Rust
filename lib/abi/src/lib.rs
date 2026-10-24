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
pub mod dirent;
pub mod errno;
pub mod evdev;
pub mod exec;
pub mod fcntl;
pub mod ioctl;
pub mod kbio;
pub mod kinfo;
pub mod layout;
pub mod md;
pub mod mman;
pub mod mount;
pub mod procdesc;
pub mod resource;
pub mod signal;
pub mod socket;
pub mod stat;
pub mod sysarch;
pub mod syscall;
pub mod sysctl;
pub mod termios;
pub mod thr;
pub mod time;
pub mod ucontext;
pub mod uio;
pub mod vdso;
