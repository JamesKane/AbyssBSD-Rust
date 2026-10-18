// SPDX-License-Identifier: BSD-2-Clause
//! Debug Port Table 2 (Microsoft DBG2 specification).

use crate::{Error, Gas, HEADER_LEN, Sdt, SerialPort, UartKind, u16_at, u32_at};

const PORT_TYPE_SERIAL: u16 = 0x8000;
const DEVICE_MIN_LEN: usize = 22;

/// The serial ports a DBG2 table lists, using each device's first address.
#[must_use]
pub fn serial_ports<'a>(t: &Sdt<'a>) -> SerialPorts<'a> {
    let b = t.bytes;
    let state = if t.signature() == *b"DBG2" {
        match (u32_at(b, HEADER_LEN), u32_at(b, HEADER_LEN + 4)) {
            (Some(off), Some(left)) => match usize::try_from(off) {
                Ok(off) => State::Walking { off, left },
                Err(_) => State::Failed(Some(Error::BadLength(*b"DBG2"))),
            },
            _ => State::Failed(Some(Error::BadLength(*b"DBG2"))),
        }
    } else {
        State::Failed(Some(Error::BadSignature))
    };
    SerialPorts { bytes: b, state }
}

/// Iterator returned by [`serial_ports`]. A malformed entry yields one error
/// and ends the walk.
#[derive(Clone, Debug)]
pub struct SerialPorts<'a> {
    bytes: &'a [u8],
    state: State,
}

#[derive(Clone, Debug)]
enum State {
    Walking { off: usize, left: u32 },
    Failed(Option<Error>),
}

impl Iterator for SerialPorts<'_> {
    type Item = Result<SerialPort, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.state {
                State::Failed(e) => return e.take().map(Err),
                State::Walking { left: 0, .. } => return None,
                State::Walking { off, left } => {
                    *left -= 1;
                    match device(self.bytes, *off) {
                        Ok((len, port)) => {
                            *off += len;
                            if let Some(port) = port {
                                return Some(Ok(port));
                            }
                        }
                        Err(e) => self.state = State::Failed(Some(e)),
                    }
                }
            }
        }
    }
}

/// Parses the device information structure at `off`: its length, and the
/// serial port it describes if it is one.
fn device(b: &[u8], off: usize) -> Result<(usize, Option<SerialPort>), Error> {
    let bad = Error::Malformed("DBG2 device entry");
    let len = usize::from(u16_at(b, off + 1).ok_or(bad)?);
    if len < DEVICE_MIN_LEN || off.checked_add(len).is_none_or(|end| end > b.len()) {
        return Err(bad);
    }
    let entry = &b[off..off + len];
    let gas_count = entry[3];
    let port_type = u16_at(entry, 12).ok_or(bad)?;
    let subtype = u16_at(entry, 14).ok_or(bad)?;
    let gas_off = usize::from(u16_at(entry, 18).ok_or(bad)?);
    if port_type != PORT_TYPE_SERIAL || gas_count == 0 {
        return Ok((len, None));
    }
    let base = Gas::parse(entry, gas_off)?;
    Ok((
        len,
        Some(SerialPort {
            kind: UartKind::from_subtype(subtype),
            base,
        }),
    ))
}
