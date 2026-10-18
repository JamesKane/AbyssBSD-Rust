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
    /// The global system interrupt (I/O APIC, GIC, …), if the table
    /// states one.
    pub gsiv: Option<u32>,
    /// The PC-AT (8259) IRQ, if the table states one.
    pub pc_irq: Option<u8>,
}

const INTERFACE_TYPE: usize = 36;
const BASE_ADDRESS: usize = 40;
const INTERRUPT_TYPE: usize = 52;
const PC_IRQ: usize = 53;
const GSIV: usize = 54;
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
    // Interrupt type: bit 0 PC-AT 8259, bits 1–4 an interrupt controller
    // numbered by GSI (I/O APIC, I/O SAPIC, GIC, RISC-V PLIC).
    let itype = b[INTERRUPT_TYPE];
    let gsiv = (itype & 0x1e != 0)
        .then(|| u32::from_le_bytes([b[GSIV], b[GSIV + 1], b[GSIV + 2], b[GSIV + 3]]));
    let pc_irq = (itype & 1 != 0).then_some(b[PC_IRQ]);
    Ok(Spcr {
        port: SerialPort { kind, base },
        baud,
        gsiv,
        pc_irq,
    })
}
