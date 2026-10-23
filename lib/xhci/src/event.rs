// SPDX-License-Identifier: BSD-2-Clause
//! Event TRBs (xHCI 1.2 §6.4.2), decoded. Everything here comes from the
//! device: any field may be garbage, and decoding never fails, it only
//! sorts.

use crate::trb::{CompletionCode, Trb, TrbType, byte, flag};

/// An event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A transfer TRB (or event data) completed.
    Transfer {
        /// The TRB's address, or the event data if `event_data`.
        pointer: u64,
        /// The bytes not transferred of the TRB's length (or, with event
        /// data, the bytes transferred in the TD so far).
        residue: u32,
        /// The completion code.
        code: CompletionCode,
        /// `pointer` is event data.
        event_data: bool,
        /// The slot.
        slot: u8,
        /// The endpoint's device context index.
        endpoint: u8,
    },
    /// A command completed.
    Command {
        /// The command TRB's address.
        pointer: u64,
        /// The completion code.
        code: CompletionCode,
        /// The slot (Enable Slot's result).
        slot: u8,
        /// The completion parameter.
        parameter: u32,
    },
    /// A root hub port's status changed.
    PortStatusChange {
        /// The port number, from 1.
        port: u8,
    },
    /// A host controller event (event ring full, for one).
    HostController {
        /// The completion code.
        code: CompletionCode,
    },
    /// The microframe index wrapped.
    MfindexWrap,
    /// Anything else: bandwidth requests, doorbells, device
    /// notifications, and types this crate does not know.
    Other {
        /// The raw type.
        trb_type: u8,
    },
}

impl Event {
    /// Decodes `t`.
    #[must_use]
    pub fn decode(t: &Trb) -> Self {
        match t.trb_type() {
            Some(TrbType::TransferEvent) => Self::Transfer {
                pointer: t.param(),
                residue: t.status() & 0x00ff_ffff,
                code: t.code(),
                event_data: t.control() & flag::EVENT_DATA != 0,
                slot: t.slot(),
                endpoint: t.endpoint(),
            },
            Some(TrbType::CommandCompletion) => Self::Command {
                pointer: t.param(),
                code: t.code(),
                slot: t.slot(),
                parameter: t.status() & 0x00ff_ffff,
            },
            Some(TrbType::PortStatusChange) => Self::PortStatusChange {
                port: byte(t.0[0] >> 24),
            },
            Some(TrbType::HostController) => Self::HostController { code: t.code() },
            Some(TrbType::MfindexWrap) => Self::MfindexWrap,
            _ => Self::Other {
                trb_type: t.raw_type(),
            },
        }
    }
}
