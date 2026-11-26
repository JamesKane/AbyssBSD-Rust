// SPDX-License-Identifier: BSD-2-Clause
//! SCMI's power-domain protocol over a shared-memory channel and a firmware call, without
//! hardware. The message layout and the power-domain protocol (`0x11`) are Arm's public System
//! Control and Management Interface specification (DEN0056); the Sky1's transport, an SMC to
//! Trusted Firmware with function `0xC2000001`, is the clean-room Sky1 SoC specification 01 §1.3.

/// The channel's shared memory (SCMI shared-memory transport layout).
pub trait Shmem {
    fn read32(&self, off: usize) -> u32;
    fn write32(&self, off: usize, v: u32);
}

/// Rings the firmware: the call returns once it has answered (`x0` = 0 when delivered).
pub trait Doorbell {
    fn ring(&self) -> i64;
}

// The shared-memory layout.
const CHANNEL_STATUS: usize = 0x04;
const FLAGS: usize = 0x10;
const LENGTH: usize = 0x14;
const HEADER: usize = 0x18;
const PAYLOAD: usize = 0x1C;
/// Channel status bit 0: the channel is free (the platform has answered).
const FREE: u32 = 1;
/// Channel status bit 1: the platform saw an error on the channel.
const CHANNEL_ERROR: u32 = 1 << 1;

/// The power-domain management protocol.
pub const PROTOCOL_POWER: u8 = 0x11;
const PROTOCOL_VERSION: u8 = 0x0;
const PROTOCOL_ATTRIBUTES: u8 = 0x1;
const POWER_STATE_SET: u8 = 0x4;
const POWER_STATE_GET: u8 = 0x5;

/// A power state: on (state 0, context preserved).
pub const STATE_ON: u32 = 0;
/// Off, context lost.
pub const STATE_OFF: u32 = 0x4000_0000;

/// What a call can fail with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The channel was not free before the call.
    Busy,
    /// The firmware call itself failed (its `x0`).
    Call(i64),
    /// The channel reports an error, or the answer is not one.
    Channel,
    /// The platform's SCMI status (negative: not supported, invalid parameters, denied, ...).
    Status(i32),
    /// The domain did not reach the state asked for (the state read back).
    NotReached(u32),
}

/// One channel: its memory, its doorbell, and the next token.
pub struct Channel<S, D> {
    shmem: S,
    doorbell: D,
    token: u16,
}

impl<S, D> core::fmt::Debug for Channel<S, D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Channel")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

/// The message header (DEN0056 §4.1.1): message ID, type 0 (command), protocol, token.
fn header(protocol: u8, msg: u8, token: u16) -> u32 {
    u32::from(msg) | u32::from(protocol) << 10 | u32::from(token & 0x3FF) << 18
}

impl<S: Shmem, D: Doorbell> Channel<S, D> {
    pub fn new(shmem: S, doorbell: D) -> Self {
        Self {
            shmem,
            doorbell,
            token: 0,
        }
    }

    /// The token the next message carries.
    pub fn token(&self) -> u16 {
        self.token
    }

    /// Continues another channel's token sequence.
    pub fn set_token(&mut self, token: u16) {
        self.token = token & 0x3FF;
    }

    /// Sends `msg` with `args`, and returns the answer's words after the status, up to `N`.
    fn call<const N: usize>(&mut self, msg: u8, args: &[u32]) -> Result<[u32; N], Error> {
        let m = &self.shmem;
        if m.read32(CHANNEL_STATUS) & FREE == 0 {
            return Err(Error::Busy);
        }
        let token = self.token;
        self.token = (self.token + 1) & 0x3FF;
        m.write32(CHANNEL_STATUS, 0);
        // Polled: no completion interrupt.
        m.write32(FLAGS, 0);
        let hdr = header(PROTOCOL_POWER, msg, token);
        m.write32(HEADER, hdr);
        for (k, a) in args.iter().enumerate() {
            m.write32(PAYLOAD + 4 * k, *a);
        }
        m.write32(
            LENGTH,
            u32::try_from(4 + 4 * args.len()).map_err(|_| Error::Channel)?,
        );
        let x0 = self.doorbell.ring();
        if x0 != 0 {
            return Err(Error::Call(x0));
        }
        let status = m.read32(CHANNEL_STATUS);
        if status & FREE == 0 || status & CHANNEL_ERROR != 0 || m.read32(HEADER) != hdr {
            return Err(Error::Channel);
        }
        let len = m.read32(LENGTH) as usize;
        // Header and status at least, and every word asked for.
        if len < 8 + 4 * N {
            return Err(Error::Channel);
        }
        let code = m.read32(PAYLOAD).cast_signed();
        if code != 0 {
            return Err(Error::Status(code));
        }
        Ok(core::array::from_fn(|k| m.read32(PAYLOAD + 4 + 4 * k)))
    }

    /// The protocol's version (major in bits 31:16, minor in 15:0).
    pub fn version(&mut self) -> Result<u32, Error> {
        self.call::<1>(PROTOCOL_VERSION, &[]).map(|[v]| v)
    }

    /// The number of power domains.
    pub fn domains(&mut self) -> Result<u32, Error> {
        self.call::<1>(PROTOCOL_ATTRIBUTES, &[])
            .map(|[a]| a & 0xFFFF)
    }

    /// Domain `id`'s state.
    pub fn state(&mut self, id: u32) -> Result<u32, Error> {
        self.call::<1>(POWER_STATE_GET, &[id]).map(|[s]| s)
    }

    /// Sets domain `id` to `state` synchronously, then reads it back: a different state is an
    /// error (Sky1 SoC specification 01 §1.3.3).
    pub fn set_state(&mut self, id: u32, state: u32) -> Result<(), Error> {
        self.call::<0>(POWER_STATE_SET, &[0, id, state])?;
        match self.state(id)? {
            s if s == state => Ok(()),
            s => Err(Error::NotReached(s)),
        }
    }
}
