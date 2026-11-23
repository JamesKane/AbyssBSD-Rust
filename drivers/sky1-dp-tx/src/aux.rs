// SPDX-License-Identifier: BSD-2-Clause
//! The auxiliary channel: native DPCD reads and writes, and I2C over AUX
//! (clean-room specification `sky1-dp-tx` 02 §2.3).

use crate::Delay;
use crate::regs::{self, Words};

/// Why a transfer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuxError {
    /// No sink: the hot-plug line is low.
    NoSink,
    /// The channel was busy, or the reply never came.
    Timeout,
    /// The reply carried the wrong number of bytes.
    Io,
    /// The same, with what was seen: the request code, the count in the
    /// reply, the count asked for, and the reply code.
    Count {
        cmd: u8,
        got: u8,
        want: u8,
        code: u8,
    },
    /// The sink refused (NACK).
    Nack,
    /// The sink kept deferring.
    Defer,
}

/// What a reply code said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    Ack,
    Nack,
    Defer,
}

/// Most bytes one transaction carries.
const CHUNK: usize = 16;
/// A native request is retried this often on a defer (the standard says at
/// least seven) and this often on a timeout.
const DEFER_RETRIES: u32 = 7;
const TIMEOUT_RETRIES: u32 = 3;
/// Polls of the status register, a millisecond apart (the vendor gives up
/// after the third).
const POLLS: u32 = 3;

/// The channel.
#[allow(missing_debug_implementations)]
pub struct Aux<'a> {
    w: &'a dyn Words,
    d: &'a dyn Delay,
}

impl<'a> Aux<'a> {
    #[must_use]
    pub fn new(w: &'a dyn Words, d: &'a dyn Delay) -> Self {
        Self { w, d }
    }

    /// The hot-plug line is high.
    #[must_use]
    pub fn hpd(&self) -> bool {
        self.w.read(regs::HPD_INPUT_STATE) & 1 != 0
    }

    /// One transaction of at most 16 bytes. For a read, `read` is filled;
    /// `write` is the data of a write. `native` picks the reply field.
    fn once(
        &self,
        cmd: u32,
        addr: u32,
        write: &[u8],
        read: &mut [u8],
        address_only: bool,
        native: bool,
    ) -> Result<(Reply, usize), AuxError> {
        if !self.hpd() {
            return Err(AuxError::NoSink);
        }
        let busy = regs::AUX_REQUEST_IN_PROGRESS | regs::AUX_REPLY_IN_PROGRESS;
        if self.w.read(regs::AUX_STATUS) & busy != 0 {
            return Err(AuxError::Timeout);
        }
        self.w.write(regs::AUX_ADDRESS, addr & 0xF_FFFF);
        for &b in write {
            self.w.write(regs::AUX_WRITE_FIFO, u32::from(b));
        }
        let len = write.len().max(read.len());
        let mut c = cmd << 8;
        if address_only {
            c |= regs::AUX_ADDRESS_ONLY;
        } else {
            c |= u32::try_from(len.saturating_sub(1)).unwrap_or(0) & 0xF;
        }
        self.w.write(regs::AUX_COMMAND, c);
        let mut got = false;
        for _ in 0..POLLS {
            let s = self.w.read(regs::AUX_STATUS);
            if s & regs::AUX_REPLY_ERROR != 0 {
                return Err(AuxError::Timeout);
            }
            if s & regs::AUX_REPLY_RECEIVED != 0 {
                got = true;
                break;
            }
            self.d.delay_us(1000);
        }
        if !got {
            return Err(AuxError::Timeout);
        }
        let code = self.w.read(regs::AUX_REPLY_CODE);
        let field = if native { code & 3 } else { (code >> 2) & 3 };
        let reply = match field {
            0 => Reply::Ack,
            1 => Reply::Nack,
            _ => Reply::Defer,
        };
        let mut received = 0;
        if reply == Reply::Ack && !read.is_empty() {
            let n = (self.w.read(regs::AUX_REPLY_DATA_COUNT) & 0xFF) as usize;
            // A native read returns what was asked for. An I2C-over-AUX
            // read may be answered with fewer bytes, and the source asks
            // again for the rest (the standard's I2C-over-AUX framing).
            if n == 0 || n > read.len() || (native && n != read.len()) {
                return Err(AuxError::Count {
                    cmd: u8::try_from(cmd & 0xFF).unwrap_or(0xFF),
                    got: u8::try_from(n).unwrap_or(0xFF),
                    want: u8::try_from(read.len()).unwrap_or(0xFF),
                    code: u8::try_from(code & 0xFF).unwrap_or(0xFF),
                });
            }
            for b in read.iter_mut().take(n) {
                *b = u8::try_from(self.w.read(regs::AUX_REPLY_DATA) & 0xFF).unwrap_or(0);
            }
            received = n;
        }
        Ok((reply, received))
    }

    /// One transaction with the retries the standard asks of a source.
    fn retried(
        &self,
        cmd: u32,
        addr: u32,
        write: &[u8],
        read: &mut [u8],
        address_only: bool,
        native: bool,
    ) -> Result<(), AuxError> {
        self.retried_n(cmd, addr, write, read, address_only, native)
            .map(|_| ())
    }

    /// [`Aux::retried`], with the number of bytes a read received.
    fn retried_n(
        &self,
        cmd: u32,
        addr: u32,
        write: &[u8],
        read: &mut [u8],
        address_only: bool,
        native: bool,
    ) -> Result<usize, AuxError> {
        let (mut defers, mut timeouts) = (0, 0);
        loop {
            match self.once(cmd, addr, write, read, address_only, native) {
                Ok((Reply::Ack, n)) => return Ok(n),
                Ok((Reply::Nack, _)) => return Err(AuxError::Nack),
                Ok((Reply::Defer, _)) => {
                    defers += 1;
                    if defers > DEFER_RETRIES {
                        return Err(AuxError::Defer);
                    }
                    self.d.delay_us(400);
                }
                Err(AuxError::Timeout) => {
                    timeouts += 1;
                    if timeouts >= TIMEOUT_RETRIES {
                        return Err(AuxError::Timeout);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Reads `buf.len()` bytes at DPCD address `addr`.
    ///
    /// # Errors
    ///
    /// [`AuxError`]; bytes before the failing chunk are in `buf`.
    pub fn read(&self, addr: u32, buf: &mut [u8]) -> Result<(), AuxError> {
        let mut at = addr;
        for chunk in buf.chunks_mut(CHUNK) {
            self.retried(regs::AUX_NATIVE_READ, at, &[], chunk, false, true)?;
            at += u32::try_from(chunk.len()).unwrap_or(0);
        }
        Ok(())
    }

    /// Writes `data` at DPCD address `addr`.
    ///
    /// # Errors
    ///
    /// [`AuxError`].
    pub fn write(&self, addr: u32, data: &[u8]) -> Result<(), AuxError> {
        let mut at = addr;
        for chunk in data.chunks(CHUNK) {
            self.retried(regs::AUX_NATIVE_WRITE, at, chunk, &mut [], false, true)?;
            at += u32::try_from(chunk.len()).unwrap_or(0);
        }
        Ok(())
    }

    /// One byte.
    ///
    /// # Errors
    ///
    /// [`AuxError`].
    pub fn read_byte(&self, addr: u32) -> Result<u8, AuxError> {
        let mut b = [0];
        self.read(addr, &mut b)?;
        Ok(b[0])
    }

    /// Reads from the I2C device `slave` (7-bit) over AUX: writes the
    /// register `offset` bytes, then reads `buf`, and stops (the standard
    /// I2C-over-AUX framing: the middle-of-transaction flag on every
    /// request but the last).
    ///
    /// # Errors
    ///
    /// [`AuxError`].
    pub fn i2c_read(&self, slave: u8, offset: &[u8], buf: &mut [u8]) -> Result<(), AuxError> {
        let a = u32::from(slave);
        let mot = regs::AUX_I2C_MOT;
        // Start: an address-only write, then the offset.
        self.retried(regs::AUX_I2C_WRITE | mot, a, &[], &mut [], true, false)?;
        if !offset.is_empty() {
            self.retried(regs::AUX_I2C_WRITE | mot, a, offset, &mut [], false, false)?;
        }
        let chunks = buf.len().div_ceil(CHUNK);
        for (i, chunk) in buf.chunks_mut(CHUNK).enumerate() {
            let last = i + 1 == chunks;
            let cmd = regs::AUX_I2C_READ | if last { 0 } else { mot };
            // A short reply is not the end: ask for the rest.
            let mut done = 0;
            while done < chunk.len() {
                done += self.retried_n(cmd, a, &[], &mut chunk[done..], false, false)?;
            }
        }
        // Stop.
        self.retried(regs::AUX_I2C_READ, a, &[], &mut [], true, false)
    }

    /// Reads EDID block `block` (128 bytes): slave `0x50`, with the
    /// segment pointer (`0x30`) for blocks past the second.
    ///
    /// # Errors
    ///
    /// [`AuxError`].
    pub fn read_edid_block(&self, block: u8) -> Result<[u8; 128], AuxError> {
        if block >= 2 {
            // Segment pointer: written without a stop.
            let a = 0x30;
            self.retried(
                regs::AUX_I2C_WRITE | regs::AUX_I2C_MOT,
                a,
                &[block / 2],
                &mut [],
                false,
                false,
            )?;
        }
        let mut out = [0u8; 128];
        self.i2c_read(0x50, &[(block % 2) * 128], &mut out)?;
        Ok(out)
    }
}
