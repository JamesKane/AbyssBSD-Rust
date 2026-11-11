// SPDX-License-Identifier: BSD-2-Clause
//! CAM's user interfaces (docs/DRIVER-MODEL.md §9.1.2): the existing
//! device table of the buses, devices, and peripherals the storage
//! drivers register, `/dev/xpt0`, and a `/dev/passN` for each device.
//!
//! AbyssBSD names its disks as CAM does (`da`, `ada`, `nda`) without
//! putting a CAM layer in their I/O path: requests to a disk go from the
//! storage class straight to its driver. CAM here is only what userland
//! sees of it, `camcontrol(8)`, libcam, and smartmontools:
//!
//! - [`Bus::register`]: a SIM's bus (`ahcich0`, `nvme0`, `umass-sim0`),
//!   numbered `scbusN` as FreeBSD numbers them, with the [`Sim`] that
//!   answers its path inquiries and runs its pass-through commands.
//! - [`Bus::add_device`]: a probed device and its peripherals (the disk
//!   driver's, and `pass`).
//! - `XPT_DEV_MATCH` ([`matching`]), `CAMGETPASSTHRU`, and each
//!   pass device's CCBs ([`xpt`]), marshalled in place ([`ccb`]).
//!
//! The kernel installs its user-memory access ([`set_host`]) and makes
//! `/dev/xpt0` ([`init`]) before any storage driver attaches.
//!
//! Left out: CAM's peripheral drivers other than `pass` (`cd`, `sa`,
//! `ses`, `ch`, `targ`), its queued interface (`CAMIOQUEUE`,
//! `CAMIOGET`), rescans that find new devices, and target mode.
//!
//! Behavior reference: sys/cam/cam_xpt.c, sys/cam/scsi/scsi_pass.c,
//! sys/cam/cam_periph.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod ccb;
mod edt;
pub mod io;
pub mod matching;
mod xpt;

#[cfg(test)]
mod tests;

pub use edt::{Bus, Device, DeviceInfo, NvmeData, Periph, Sim, SimName};
pub use xpt::{Host, host, set_host};

/// FreeBSD's `GID_OPERATOR`, which owns `xpt0` and the pass devices.
pub const GID_OPERATOR: u32 = 5;

/// Makes `/dev/xpt0`. Called once, after devfs is up.
pub fn init() {
    // The table starts with the transport layer's bus.
    edt::with(|_| ());
    xpt::make_xpt0();
}
