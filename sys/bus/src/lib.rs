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
//!
//! The pure logic it builds on (ranking, the graph, units, FreeBSD's
//! strings) is in `lib/devtree`.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod driver;
pub mod gate;
pub mod manager;
pub mod platform;
mod sysctl;
pub mod types;

pub use driver::{
    AttachCx, DetachReport, Driver, DriverDecl, IoGrant, IrqLine, Ledger, MmioGrant, StartCx,
    StopCx,
};
pub use gate::{Gate, GateGuard, Revoked};
pub use manager::{AttachRequestError, DetachError, Manager, Options, init, manager};
pub use platform::Platform;
pub use types::{
    AttachError, Busy, Concurrency, DetachReason, DeviceId, Ident, Ids, Need, NodeState, Resource,
    ResourceError, StopReason, TestId, TestIdent, WaitReason,
};
