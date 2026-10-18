// SPDX-License-Identifier: BSD-2-Clause
//! Serial Port Console Redirection table (Microsoft SPCR specification,
//! revisions 1–4).

use crate::{Error, Gas, Sdt, SerialPort, UartKind, gas::GAS_LEN};

/// A parsed SPCR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spcr {
    /// The console port.
    pub port: SerialPort,
    /// Configured baud rate, if the table states one.
    pub baud: Option<u32>,
}

const INTERFACE_TYPE: usize = 36;
const BASE_ADDRESS: usize = 40;
const BAUD_RATE: usize = 58;
/// The fields up to and including the terminal type.
const MIN_LEN: usize = 63;

/// Parses an SPCR table.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table, [`Error::BadLength`] if it is
/// too short, [`Error::Malformed`] for a bad address.
pub fn parse(t: &Sdt<'_>) -> Result<Spcr, Error> {
    if t.signature() != *b"SPCR" {
        return Err(Error::BadSignature);
    }
    let b = t.bytes;
    if b.len() < MIN_LEN || b.len() < BASE_ADDRESS + GAS_LEN {
        return Err(Error::BadLength(*b"SPCR"));
    }
    // Revision 1 used the DBGP interface types, 0 (16550) and 1 (16450),
    // which coincide with the DBG2 subtypes.
    let kind = UartKind::from_subtype(u16::from(b[INTERFACE_TYPE]));
    let base = Gas::parse(b, BASE_ADDRESS)?;
    let baud = match b[BAUD_RATE] {
        3 => Some(9600),
        4 => Some(19_200),
        6 => Some(57_600),
        7 => Some(115_200),
        _ => None,
    };
    Ok(Spcr {
        port: SerialPort { kind, base },
        baud,
    })
}
