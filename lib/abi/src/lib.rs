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

pub mod acl;
pub mod bpf;
pub mod bus;
pub mod capsicum;
pub mod consio;
pub mod devfs;
pub mod dirent;
pub mod errno;
pub mod evdev;
pub mod event;
pub mod exec;
pub mod fbio;
pub mod fcntl;
pub mod inet;
pub mod inotify;
pub mod ioctl;
pub mod ipc;
pub mod jail;
pub mod kbio;
pub mod kinfo;
pub mod ktrace;
pub mod layout;
pub mod linker;
pub mod md;
pub mod mman;
pub mod mount;
pub mod mouse;
pub mod net;
pub mod netlink;
pub mod procctl;
pub mod procdesc;
pub mod ptrace;
pub mod resource;
pub mod signal;
pub mod socket;
pub mod stat;
pub mod sysarch;
pub mod syscall;
pub mod sysctl;
pub mod syslog;
pub mod termios;
pub mod thr;
pub mod time;
pub mod ucontext;
pub mod ucred;
pub mod uio;
pub mod umtx;
pub mod vdso;
