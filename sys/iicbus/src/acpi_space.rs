// SPDX-License-Identifier: BSD-2-Clause
//! The `GenericSerialBus` address-space handler an ACPI-hinted `iicbus`
//! installs on its controller (FreeBSD's `acpi_iicbus_space_handler`):
//! a field access in a region under the controller becomes an I2C
//! transfer to the address its `Connection` (`I2cSerialBusV2`) names.
//!
//! The field's buffer is ACPI's: a status byte, a length byte, then the
//! data, sized by the access attribute as ACPICA sizes it (one byte for
//! `AttribSendReceive` and `AttribByte`, two for `AttribWord` and
//! `AttribProcessCall`, 255 for the rest). The status is FreeBSD's: the
//! `iicbus` error code (`IIC_NOERR`, `IIC_ENOACK`, ...), 0 on success.
//!
//! - `AttribSendReceive`, `AttribByte`, `AttribWord`, `AttribBlock`, and
//!   `AttribBytes(n)` are FreeBSD's messages, a register write joined to
//!   the read by a repeated START.
//! - FreeBSD refuses the rest; they are done here as ACPI 6.5 §19.6.7
//!   and the SMBus protocols define them: `AttribQuick` (the address
//!   alone), `AttribProcessCall` and `AttribBlockProcessCall` (a write,
//!   then a read after a repeated START), `AttribRawBytes(n)` (`n` bytes
//!   with no command), and `AttribRawProcessBytes(n)` (`n` written, then
//!   `n` read).
//! - A block read cannot learn its length from its first byte here, as
//!   FreeBSD's continued message (`IIC_M_NOSTART`) does: it reads the
//!   SMBus limit of 32 bytes after the count, and keeps the count's
//!   (docs/compat-deviations.md).
//! - A read sets the length byte to the bytes read; FreeBSD sets it for a
//!   block read alone.
//!
//! Behavior reference: sys/dev/iicbus/acpi_iicbus.c,
//! sys/contrib/dev/acpica/components/executer/exserial.c, exfield.c.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use aml::host::attrib;
use aml::resource::{Resource, SerialBusKind};
use aml::{BufferTransfer, HostError, RegionSpace};
use bus::i2c::{I2cSource, Msg, Op};
use bus::provider::ProviderError;
use sched::Sleepable;

use crate::iic::IicStatus;

/// The SMBus block limit (SMBus 2.0 §5.5.7).
const BLOCK_MAX: usize = 32;

/// `ACPI_SERIAL_HEADER_SIZE`: the status and length bytes.
const HEADER: usize = 2;

/// `EBADMSG` and `EINVAL` as `errno2iic` makes them, cut to the status
/// byte as `gsb->status = val` cuts them on FreeBSD.
const STATUS_EBADMSG: u8 = 89;
const STATUS_EINVAL: u8 = 22;

/// The data bytes a field's buffer holds for its attribute
/// (`AcpiProtocolLengths`), or `None` for an attribute ACPI does not
/// define.
#[must_use]
pub fn data_len(attr: u8) -> Option<usize> {
    match attr {
        attrib::QUICK => Some(0),
        attrib::SEND_RECEIVE | attrib::BYTE => Some(1),
        attrib::WORD | attrib::PROCESS_CALL => Some(2),
        attrib::BLOCK
        | attrib::BYTES
        | attrib::BLOCK_PROCESS_CALL
        | attrib::RAW_BYTES
        | attrib::RAW_PROCESS_BYTES => Some(255),
        _ => None,
    }
}

/// The status byte for a failed transfer: FreeBSD's `iicbus` code.
#[must_use]
pub fn status_of(e: ProviderError) -> u8 {
    let s = match e {
        ProviderError::NoAck => IicStatus::NoAck,
        ProviderError::ArbitrationLost | ProviderError::BusError => IicStatus::BusErr,
        ProviderError::BusBusy => IicStatus::BusBusy,
        ProviderError::Timeout => IicStatus::Timeout,
        ProviderError::Unsupported => IicStatus::NotSupported,
        ProviderError::OutOfRange => IicStatus::Overflow,
        ProviderError::Revoked => IicStatus::NoAddr,
        _ => IicStatus::Status,
    };
    u8::try_from(s as i32).unwrap_or(0xff)
}

/// The handler on one controller.
pub struct SpaceHandler {
    source: Arc<I2cSource>,
}

impl core::fmt::Debug for SpaceHandler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpaceHandler").finish_non_exhaustive()
    }
}

impl SpaceHandler {
    /// The handler for the controller `source`.
    #[must_use]
    pub fn new(source: Arc<I2cSource>) -> Self {
        Self { source }
    }

    /// Runs `ops` to `addr` as one transfer.
    fn run(&self, s: &Sleepable, addr: u16, ops: &mut [Op<'_>]) -> Result<(), ProviderError> {
        let mut msgs: Vec<Msg<'_>> = ops
            .iter_mut()
            .map(|op| Msg {
                address: addr,
                ten_bit: false,
                op: match op {
                    Op::Read(b) => Op::Read(b),
                    Op::Write(b) => Op::Write(b),
                },
            })
            .collect();
        self.source.transfer(s, &mut msgs)
    }

    /// Does the field access `req` for the device at `addr`, filling
    /// `buf` (status, length, data).
    fn access(
        &self,
        s: &Sleepable,
        addr: u16,
        req: &BufferTransfer<'_>,
        buf: &mut [u8],
    ) -> Result<(), HostError> {
        let write = req.write.is_some();
        // FreeBSD's `char cmd`: the field's byte offset, cut to a byte.
        let cmd = req.command.to_le_bytes()[0];
        let n = usize::from(req.access_len).min(buf.len() - HEADER);
        let (head, data) = buf.split_at_mut(HEADER);
        let mut status = 0u8;
        let mut got: Option<usize> = None;
        let mut fail = |e: ProviderError| status = status_of(e);
        match (req.attrib, write) {
            (attrib::QUICK, true) => {
                self.run(s, addr, &mut [Op::Write(&[])])
                    .unwrap_or_else(&mut fail);
            }
            (attrib::QUICK, false) => {
                self.run(s, addr, &mut [Op::Read(&mut [])])
                    .unwrap_or_else(&mut fail);
                got = Some(0);
            }
            (attrib::SEND_RECEIVE, true) => {
                self.run(s, addr, &mut [Op::Write(&data[..1])])
                    .unwrap_or_else(&mut fail);
            }
            (attrib::SEND_RECEIVE, false) => {
                self.run(s, addr, &mut [Op::Read(&mut data[..1])])
                    .unwrap_or_else(&mut fail);
                got = Some(1);
            }
            (attrib::BYTE | attrib::WORD | attrib::BYTES, true) => {
                let k = match req.attrib {
                    attrib::BYTE => 1,
                    attrib::WORD => 2,
                    _ => n,
                };
                let mut out = vec![cmd];
                out.extend_from_slice(&data[..k]);
                self.run(s, addr, &mut [Op::Write(&out)])
                    .unwrap_or_else(&mut fail);
            }
            (attrib::BYTE | attrib::WORD | attrib::BYTES, false) => {
                let k = match req.attrib {
                    attrib::BYTE => 1,
                    attrib::WORD => 2,
                    _ => n,
                };
                self.run(s, addr, &mut [Op::Write(&[cmd]), Op::Read(&mut data[..k])])
                    .unwrap_or_else(&mut fail);
                got = Some(k);
            }
            (attrib::BLOCK, true) => {
                // `acpi_iicbus_bwrite`: the count is the buffer's length
                // byte.
                let count = usize::from(head[1]).min(data.len());
                if count == 0 {
                    status = STATUS_EINVAL;
                } else {
                    let mut out = vec![cmd, head[1]];
                    out.extend_from_slice(&data[..count]);
                    self.run(s, addr, &mut [Op::Write(&out)])
                        .unwrap_or_else(&mut fail);
                }
            }
            (attrib::BLOCK, false) => {
                status = self.block_read(s, addr, &[cmd], head, data);
            }
            (attrib::PROCESS_CALL, true) => {
                let out = [cmd, data[0], data[1]];
                self.run(s, addr, &mut [Op::Write(&out), Op::Read(&mut data[..2])])
                    .unwrap_or_else(&mut fail);
                got = Some(2);
            }
            (attrib::BLOCK_PROCESS_CALL, true) => {
                let count = usize::from(head[1]).min(BLOCK_MAX);
                let mut out = vec![cmd, u8::try_from(count).unwrap_or(0)];
                out.extend_from_slice(&data[..count]);
                status = self.block_read(s, addr, &out, head, data);
            }
            (attrib::RAW_BYTES, true) => {
                self.run(s, addr, &mut [Op::Write(&data[..n])])
                    .unwrap_or_else(&mut fail);
            }
            (attrib::RAW_BYTES, false) => {
                self.run(s, addr, &mut [Op::Read(&mut data[..n])])
                    .unwrap_or_else(&mut fail);
                got = Some(n);
            }
            (attrib::RAW_PROCESS_BYTES, true) => {
                let out = data[..n].to_vec();
                self.run(s, addr, &mut [Op::Write(&out), Op::Read(&mut data[..n])])
                    .unwrap_or_else(&mut fail);
                got = Some(n);
            }
            // A read of a process call has nothing to send (ACPICA
            // refuses it for raw process bytes before calling here).
            _ => return Err(HostError::Failed),
        }
        head[0] = status;
        if status == 0
            && let Some(k) = got
        {
            head[1] = u8::try_from(k).unwrap_or(u8::MAX);
        }
        Ok(())
    }

    /// `acpi_iicbus_bread`: `out` (the command, and a process call's
    /// block), then the count and up to [`BLOCK_MAX`] bytes. Returns the
    /// status; the length byte is the count.
    fn block_read(
        &self,
        s: &Sleepable,
        addr: u16,
        out: &[u8],
        head: &mut [u8],
        data: &mut [u8],
    ) -> u8 {
        let mut raw = [0u8; 1 + BLOCK_MAX];
        if let Err(e) = self.run(s, addr, &mut [Op::Write(out), Op::Read(&mut raw)]) {
            return status_of(e);
        }
        let count = usize::from(raw[0]);
        if count == 0 {
            return STATUS_EBADMSG;
        }
        let k = count.min(BLOCK_MAX).min(data.len());
        data[..k].copy_from_slice(&raw[1..=k]);
        head[1] = raw[0];
        0
    }
}

impl acpi::opregion::RegionHandler for SpaceHandler {
    fn transfer(&self, s: &Sleepable, req: &BufferTransfer<'_>) -> Result<Vec<u8>, HostError> {
        if req.space != RegionSpace::GenericSerialBus {
            return Err(HostError::Unsupported);
        }
        let conn = req.connection.ok_or(HostError::Failed)?;
        let Ok(Resource::SerialBus(sb)) = aml::resource::connection(conn) else {
            return Err(HostError::Failed);
        };
        // 10-bit addresses are refused, as FreeBSD's handler refuses them.
        let SerialBusKind::I2c {
            ten_bit: false,
            address,
            ..
        } = sb.bus
        else {
            return Err(HostError::Failed);
        };
        let len = data_len(req.attrib).ok_or(HostError::Failed)? + HEADER;
        let mut buf = vec![0u8; len];
        if let Some(w) = req.write {
            let k = w.len().min(len);
            buf[..k].copy_from_slice(&w[..k]);
        }
        self.access(s, address, req, &mut buf)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests;
