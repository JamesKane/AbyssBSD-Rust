// SPDX-License-Identifier: BSD-2-Clause
//! The device manager (docs/DRIVER-MODEL.md).
//!
//! - [`types`]: identities, states, resources, and errors.
//! - [`driver`]: the [`Driver`] trait, bindings, grants, and the contexts
//!   drivers run in.
//! - [`manager`]: the node table, matching, the dependency graph, attach
//!   workers, and detach.
//! - [`gate`]: revocable references from long-lived objects to bindings.
//! - [`platform`]: what the kernel provides.
//! - [`provider`]: clock, reset, and power-domain providers, and the
//!   counted handles consumers hold.
//! - [`gpio`], [`i2c`]: GPIO and I2C controllers as providers, the pins
//!   and bus connections consumers hold.
//! - [`space`]: granted memory and I/O ranges, and the ranges firmware
//!   or another device shares.
//!
//! The pure logic it builds on (ranking, the graph, units, FreeBSD's
//! strings) is in `lib/devtree`.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod driver;
pub mod gate;
pub mod gpio;
pub mod i2c;
pub mod mailbox;
pub mod manager;
pub mod platform;
pub mod provider;
mod roothold;
pub mod space;
mod sysctl;
pub mod types;

pub use driver::{
    AttachCx, DetachReport, Driver, DriverDecl, Hint, IoGrant, IrqLine, Ledger, MmioGrant, StartCx,
    StopCx,
};
pub use gate::{Gate, GateGuard, Revoked};
pub use manager::{
    AttachRequestError, DEVCTL_DEFAULT_QUEUE, DetachError, Manager, Options, init, manager,
};
pub use platform::Platform;

/// `bootverbose`: the loader's `boot_verbose` (`boot -v`), and
/// `debug.bootverbose`. Drivers print their diagnostics only with it.
static BOOTVERBOSE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Whether drivers print what only bring-up needs (`bootverbose`).
#[must_use]
pub fn bootverbose() -> bool {
    BOOTVERBOSE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Sets [`bootverbose`].
pub fn set_bootverbose(on: bool) {
    BOOTVERBOSE.store(on, core::sync::atomic::Ordering::Relaxed);
}
pub use roothold::{RootHold, root_mount_hold, root_mount_wait};
pub use space::{AddressSpace, Range, Sharing};
pub use sysctl::device_node;
pub use types::{
    Announce, AttachError, Busy, Concurrency, DetachReason, DeviceId, Ident, Ids, Line, LineDetail,
    LineKind, Need, NodeState, Resource, ResourceError, StopReason, TestId, TestIdent, WaitReason,
};
