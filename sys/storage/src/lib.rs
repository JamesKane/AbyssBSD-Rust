// SPDX-License-Identifier: BSD-2-Clause
//! The storage class (docs/DRIVER-MODEL.md §9.1): block requests, the
//! queue-shaped trait disk drivers implement, and the disks they publish.
//!
//! - [`Bio`]: an owned request, completed exactly once. Completing
//!   consumes it; one dropped uncompleted is completed with an error and
//!   counted, so a driver bug cannot strand a waiter.
//! - [`DiskQueues`]: what a driver implements: start a batch of requests
//!   on one hardware queue, keeping what does not fit.
//! - [`Disk`]: a published disk or partition (GEOM-lite), with FreeBSD's
//!   names (`vtbd0`, `nda0`, `ada0`, and `p1`… for GPT partitions). Calls
//!   into the driver go through the disk's [`bus::Gate`]; requests that
//!   do not fit wait on a per-queue list until the driver reports room.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod bio;
pub mod disk;

pub use bio::{Bio, BioError, BioOp};
pub use disk::{Disk, DiskInfo, DiskQueues, disks, find, publish, withdraw};
