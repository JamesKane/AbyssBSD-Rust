// SPDX-License-Identifier: BSD-2-Clause
//! The core of the Direct Rendering Manager: `/dev/dri/card<N>`,
//! its open files, handles to graphics execution manager (GEM) buffers,
//! dumb buffers, events, and the ioctl dispatcher.
//!
//! The user interface is the Direct Rendering Manager's, as FreeBSD's
//! ports see it (`docs/GRAPHICS.md` §2): FreeBSD's request encoding,
//! the Direct Rendering Manager's structure rules, Linux driver names. A driver implements [`DrmDriver`] and calls
//! [`register`]; the core does the rest. Display objects (the kernel
//! mode setting core) arrive in `kms`; fences, syncobj and dma-buf with M-G.2.
//!
//! Behavior reference: `sys/dev/drm2` and `drm-kmod`'s device interface,
//! as userland (libdrm) uses them; the MIT Direct Rendering Manager uapi headers.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod build;
pub mod card;
pub mod driver;
pub mod gem;
mod ioctl;
pub mod kms;
mod kms_ioctl;
#[cfg(test)]
mod tests;

pub use build::{KmsBuilder, in_formats_blob};
pub use card::{Card, Open, register};
pub use driver::{DriverInfo, DrmDriver, DumbLimits};

use abi::errno::Errno;
use sync::Once;
use vm::object::PageOps;

/// The caller's memory, as the kernel's `copyin` and `copyout` reach it,
/// for the requests that carry pointers (`VERSION`).
pub trait UserMemory: Sync {
    /// Copies `dst.len()` bytes from user address `src`.
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno>;
    /// Copies `src` to user address `dst`.
    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno>;
}

/// What the kernel gives the core, once.
#[allow(
    missing_debug_implementations,
    reason = "it holds trait objects and a fn"
)]
pub struct Env {
    pub user: &'static dyn UserMemory,
    /// Where buffer pages come from.
    pub pages: &'static dyn PageOps,
    /// Where the core says what it refused (`kprintln!`).
    pub log: fn(core::fmt::Arguments<'_>),
}

static ENV: Once<Env> = Once::new();

/// Installs the environment, before any card registers.
pub fn init(env: Env) {
    let _ = ENV.set(env);
}

pub(crate) fn env() -> Option<&'static Env> {
    ENV.get()
}
