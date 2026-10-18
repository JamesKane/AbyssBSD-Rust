// SPDX-License-Identifier: BSD-2-Clause
//! Parses FreeBSD's `sys/kern/syscalls.master` and renders the Rust syscall
//! tables in `lib/abi`.
//!
//! `syscalls.master` is the single source of truth for syscall numbers;
//! generating from it means numbers can never drift by hand. Run through
//! `cargo xtask syscallgen`.

#![forbid(unsafe_code)]

mod parse;
mod render;

pub use parse::{Arg, CompatLevel, Decl, Entry, Flags, Kind, ParseError, parse};
pub use render::{RenderError, render};
