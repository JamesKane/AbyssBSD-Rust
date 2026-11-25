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

mod atomic;
mod build;
pub mod card;
mod commit;
pub mod dmabuf;
pub mod driver;
pub mod gem;
mod ioctl;
pub mod kms;
mod kms_ioctl;
pub mod sync_file;
pub mod syncobj;
#[cfg(test)]
mod tests;
mod vblank;

pub use build::{KmsBuilder, in_formats_blob};
pub use card::{Card, Open, panic_restore, register, retire_firmware_cards};
pub use dmabuf::DmaBuf;
pub use driver::{DriverInfo, DrmDriver, DumbLimits, IoctlCx};
pub use sync_file::SyncFile;
pub use syncobj::{Done, Fence};

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

/// A one-shot timer whose handler runs in thread context (the kernel's
/// softclock): the core times frames with it when a display controller
/// has no vertical blanking interrupt.
pub trait Ticker: Send + Sync {
    /// Runs `f(arg)` in `delay_ns` nanoseconds, replacing any arming not
    /// yet run.
    fn arm(&self, delay_ns: u64, f: fn(&sched::Sleepable, usize), arg: usize);
    /// Disarms it.
    fn cancel(&self);
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
    /// Nanoseconds of uptime.
    pub now_ns: fn() -> u64,
    /// A new timer.
    pub ticker: fn() -> alloc::boxed::Box<dyn Ticker>,
    /// Gives the calling process a descriptor for a dma-buf: close on
    /// exec if the flag is set, open for writing if the other is.
    pub export_dmabuf:
        fn(&sched::Sleepable, alloc::sync::Arc<DmaBuf>, bool, bool) -> Result<i32, Errno>,
    /// The dma-buf the calling process's descriptor names: `EBADF` for
    /// no such descriptor, `EINVAL` for one that is not a dma-buf.
    pub import_dmabuf: fn(&sched::Sleepable, i32) -> Result<alloc::sync::Arc<DmaBuf>, Errno>,
    /// Gives the calling process a descriptor for a sync file: close on
    /// exec if the flag is set.
    pub export_syncfile:
        fn(&sched::Sleepable, alloc::sync::Arc<SyncFile>, bool) -> Result<i32, Errno>,
    /// The sync file the calling process's descriptor names: `EBADF` for
    /// no such descriptor, `EINVAL` for one that is not a sync file.
    pub import_syncfile: fn(&sched::Sleepable, i32) -> Result<alloc::sync::Arc<SyncFile>, Errno>,
}

static ENV: Once<Env> = Once::new();

/// Installs the environment, before any card registers.
pub fn init(env: Env) {
    let _ = ENV.set(env);
}

pub(crate) fn env() -> Option<&'static Env> {
    ENV.get()
}
