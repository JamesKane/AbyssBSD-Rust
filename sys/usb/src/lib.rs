// SPDX-License-Identifier: BSD-2-Clause
//! The USB bus ([USB.md §5]): what sits between a host controller driver
//! and the drivers of the devices on it.
//!
//! - [`HostController`] is what a controller driver (`xhci`) implements:
//!   root ports, device slots, endpoints, and transfers.
//! - [`Transfer`] is an owned request, completed exactly once.
//! - `usbus` (a child of the controller) runs the bus's explore thread,
//!   which resets ports, enumerates devices, and adds one child per
//!   interface to the hub the device is on, with [`Ident::Usb`] and a
//!   [`UsbInterface`] as bus data. `uhub` drives the root hub and external
//!   hubs through one [`hub::Ports`] interface.
//!
//! Names and strings are FreeBSD's: `usbus0`, `uhub0`, `ugen0.2`, and
//! `uhub_child_pnpinfo`/`uhub_child_location`.
//!
//! Behavior reference: sys/dev/usb/usb_hub.c, usb_device.c,
//! usb_request.c, usb_transfer.c.
//!
//! [USB.md §5]: ../../../docs/USB.md
//! [`Ident::Usb`]: bus::Ident::Usb

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod bus;
pub mod device;
pub mod hc;
pub mod hub;
pub mod transfer;

pub use bus::USBUS;
pub use device::{UsbDevice, UsbInterface};
pub use hc::{HostController, NewDevice, PortState, SlotId};
pub use hub::UHUB;
pub use transfer::{Rejected, Transfer};

/// Why a transfer or request failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbError {
    /// The endpoint stalled (a `STALL` handshake, or a request the device
    /// refused).
    Stalled,
    /// No completion within the transfer's timeout.
    TimedOut,
    /// Cancelled by its owner.
    Cancelled,
    /// The device was unplugged.
    Gone,
    /// The device sent more than the transfer could hold.
    Babble,
    /// A transaction error (CRC, timeout on the wire, bad PID).
    Transaction,
    /// The controller has no room for the request now.
    Busy,
    /// Out of memory for buffers or contexts.
    NoMemory,
    /// The request is malformed (an endpoint the device lacks).
    Invalid,
    /// The device broke the protocol (a short descriptor).
    Protocol(&'static str),
    /// The controller reported something else (its completion code).
    Controller(u8),
}
