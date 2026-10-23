// SPDX-License-Identifier: BSD-2-Clause
//! The eXtensible Host Controller Interface (xHCI 1.2), as data: TRBs and
//! their builders, the producer and event rings, device and input
//! contexts, and register fields. No register or DMA access happens here;
//! `drivers/xhci` lays these over the controller ([USB.md §4]).
//!
//! Everything read back from the controller (events, output contexts) is
//! untrusted and decodes without panicking.
//!
//! [USB.md §4]: ../../../docs/USB.md

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod context;
pub mod event;
pub mod regs;
pub mod ring;
pub mod trb;

#[cfg(test)]
mod tests;
