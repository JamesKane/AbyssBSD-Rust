// SPDX-License-Identifier: BSD-2-Clause
//! The OS's SCMI agent (`\_SB.SCMI`, `CIXHA006`): SCMI messages in the
//! shared memory of the sending mailbox (MBX6, channel 8), its doorbell
//! rung, and the reply polled for. The platform marks the channel free
//! when the reply is in place (SCMI 3.x §5.1.2). Interrupts are not used,
//! as in the AbyssBSD Project's FreeBSD port (`sky1_scmi`).
//!
//! The firmware's AML (`\_SB.PMMX`) speaks SCMI too, on another agent's
//! channel, which answers `NOT_FOUND` for every clock: clocks are this
//! agent's.

use alloc::vec::Vec;

use bus::mailbox::MailboxChannel;
use bus::provider::ProviderError;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};

use crate::Methods;

static CHANNEL: LockClass = LockClass::new("sky1 scmi channel", 262);

/// Shared-memory words (SCMI 3.x §5.1.2), in message registers.
mod shmem {
    /// Channel status, at byte 0x04.
    pub(super) const STATUS: usize = 1;
    pub(super) const FREE: u32 = 1 << 0;
    pub(super) const ERROR: u32 = 1 << 1;
    pub(super) const FLAGS: usize = 0x10 / 4;
    pub(super) const LENGTH: usize = 0x14 / 4;
    pub(super) const HEADER: usize = 0x18 / 4;
    pub(super) const PAYLOAD: usize = 0x1c / 4;
}

/// The clock protocol.
const CLOCK: u32 = 0x14;
/// How long the platform has to answer (as CIX's Linux allows).
const REPLY_US: u64 = 300_000;
/// Polls before the wait sleeps between them.
const SPIN_POLLS: u32 = 100;

/// A message header: message, protocol, token (SCMI 3.x §4.1.1).
#[must_use]
pub fn header(protocol: u32, message: u32, token: u32) -> u32 {
    message & 0xff | (protocol & 0xff) << 10 | (token & 0x3ff) << 18
}

/// How the agent waits between polls: a short spin first, then a sleep.
pub type Pause = fn(&Sleepable, u32);

fn kernel_pause(s: &Sleepable, poll: u32) {
    if poll < SPIN_POLLS {
        bus::manager().delay_us(10);
    } else {
        bus::manager().sleep_ms(s, 1);
    }
}

/// The agent's channel to the platform.
pub struct Agent {
    tx: MailboxChannel,
    /// The next token; the channel is one transaction at a time.
    token: Mutex<u32>,
    pause: Pause,
}

impl core::fmt::Debug for Agent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Agent")
            .field("channel", &self.tx.number())
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// The agent sending on `tx`.
    #[must_use]
    pub fn new(tx: MailboxChannel) -> Self {
        Self::with_pause(tx, kernel_pause)
    }

    /// As [`Self::new`], waiting with `pause`.
    #[must_use]
    pub fn with_pause(tx: MailboxChannel, pause: Pause) -> Self {
        Self {
            tx,
            token: Mutex::new(&CHANNEL, 0),
            pause,
        }
    }

    /// Sends `payload` as `message` of `protocol` and returns the reply's
    /// payload as bytes: its status word, then its values.
    ///
    /// # Errors
    ///
    /// [`ProviderError::BusBusy`] if the channel is not free,
    /// [`ProviderError::Timeout`] if no reply comes, and
    /// [`ProviderError::Firmware`] for a channel error or a payload that
    /// does not fit.
    pub fn request(
        &self,
        s: &Sleepable,
        protocol: u32,
        message: u32,
        payload: &[u32],
    ) -> Result<Vec<u8>, ProviderError> {
        let room = self.tx.words().saturating_sub(shmem::PAYLOAD);
        if payload.len() > room {
            return Err(ProviderError::Firmware(0));
        }
        let mut token = self.token.lock(s);
        let tx = &self.tx;
        if tx.read(shmem::STATUS)? & shmem::FREE == 0 {
            return Err(ProviderError::BusBusy);
        }
        *token = token.wrapping_add(1) & 0x3ff;
        tx.write(shmem::FLAGS, 0)?; // polled
        tx.write(shmem::HEADER, header(protocol, message, *token))?;
        for (i, &w) in payload.iter().enumerate() {
            tx.write(shmem::PAYLOAD + i, w)?;
        }
        let len = u32::try_from(4 + 4 * payload.len()).unwrap_or(u32::MAX);
        tx.write(shmem::LENGTH, len)?;
        // The channel is the platform's until it hands it back.
        tx.write(shmem::STATUS, 0)?;
        tx.ring()?;
        let mut status = 0;
        let mut waited = 0u64;
        for poll in 0.. {
            status = tx.read(shmem::STATUS)?;
            if status & shmem::FREE != 0 || waited >= REPLY_US {
                break;
            }
            (self.pause)(s, poll);
            waited += if poll < SPIN_POLLS { 10 } else { 1000 };
        }
        if status & shmem::FREE == 0 {
            return Err(ProviderError::Timeout);
        }
        if status & shmem::ERROR != 0 {
            return Err(ProviderError::Firmware(0));
        }
        // The reply's length counts its header.
        let words = usize::try_from(tx.read(shmem::LENGTH)?.saturating_sub(4) / 4)
            .unwrap_or(0)
            .clamp(1, room);
        let mut out = Vec::with_capacity(4 * words);
        for i in 0..words {
            out.extend_from_slice(&tx.read(shmem::PAYLOAD + i)?.to_le_bytes());
        }
        Ok(out)
    }
}

/// The clock operations as SCMI clock messages, in the shape of the AML
/// methods' replies [`crate::FirmwareClocks`] reads.
impl Methods for Agent {
    fn call(
        &self,
        s: &Sleepable,
        method: &'static str,
        args: &[u64],
    ) -> Result<Vec<u8>, ProviderError> {
        let w = |i: usize| {
            args.get(i)
                .and_then(|&a| u32::try_from(a).ok())
                .ok_or(ProviderError::OutOfRange)
        };
        let (message, payload) = match method {
            // CLOCK_DESCRIBE_RATES: id, first rate index.
            "CLKD" => (4, alloc::vec![w(0)?, w(1)?]),
            // CLOCK_RATE_SET: flags (synchronous, round down), id, rate.
            "SCLK" => (5, alloc::vec![0, w(0)?, w(1)?, w(2)?]),
            // CLOCK_RATE_GET: id.
            "GCLK" => (6, alloc::vec![w(0)?]),
            // CLOCK_CONFIG_SET: id, attributes (bit 0 enables).
            "CLKC" => (7, alloc::vec![w(0)?, w(1)?]),
            _ => return Err(ProviderError::Unsupported),
        };
        self.request(s, CLOCK, message, &payload)
    }
}
