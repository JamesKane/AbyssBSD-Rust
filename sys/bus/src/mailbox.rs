// SPDX-License-Identifier: BSD-2-Clause
//! Mailbox providers (DRIVER-MODEL.md §9.3): a doorbell to another
//! processor and the message registers beside it.
//!
//! A mailbox controller's driver implements [`Mailbox`] and publishes it
//! with `StartCx::provide_mailbox`. A client (an SCMI agent) asks for the
//! channels its firmware names (ACPI `_DSD` `mboxes`: a mailbox and a
//! channel number, in order) with `AttachCx::mailbox` and gets a
//! [`MailboxChannel`]. FreeBSD has no mailbox framework; its CIX driver
//! (`cix_mbox`) offers the same calls to its one client directly.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::provider::ProviderError;

/// One direction of a mailbox: message registers and a doorbell.
pub trait Mailbox: Send + Sync {
    /// The number of 32-bit message registers.
    fn words(&self) -> usize;
    /// Message register `word` (below [`Self::words`]).
    fn read(&self, word: usize) -> u32;
    /// Writes message register `word` (below [`Self::words`]).
    fn write(&self, word: usize, value: u32);
    /// Rings the doorbell of `channel`, after the message registers'
    /// writes. [`ProviderError::Unsupported`] for a channel or a direction
    /// that has no doorbell to ring.
    fn ring(&self, channel: u32) -> Result<(), ProviderError>;
}

/// A published mailbox.
pub struct MailboxSource {
    imp: Arc<dyn Mailbox>,
    revoked: AtomicBool,
}

impl core::fmt::Debug for MailboxSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MailboxSource")
            .field("words", &self.imp.words())
            .finish_non_exhaustive()
    }
}

impl MailboxSource {
    /// Wraps a provider.
    #[must_use]
    pub fn new(imp: Arc<dyn Mailbox>) -> Arc<Self> {
        Arc::new(Self {
            imp,
            revoked: AtomicBool::new(false),
        })
    }

    /// Channel `channel` of the mailbox.
    #[must_use]
    pub fn channel(self: &Arc<Self>, channel: u32) -> MailboxChannel {
        MailboxChannel {
            source: Arc::clone(self),
            channel,
        }
    }

    /// Stops every access: the driver is detaching.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    fn live(&self) -> Result<&dyn Mailbox, ProviderError> {
        if self.revoked.load(Ordering::Acquire) {
            Err(ProviderError::Revoked)
        } else {
            Ok(&*self.imp)
        }
    }
}

/// A client's channel of a mailbox.
#[derive(Debug, Clone)]
pub struct MailboxChannel {
    source: Arc<MailboxSource>,
    channel: u32,
}

impl MailboxChannel {
    /// The channel number.
    #[must_use]
    pub fn number(&self) -> u32 {
        self.channel
    }

    /// The number of 32-bit message registers.
    #[must_use]
    pub fn words(&self) -> usize {
        self.source.imp.words()
    }

    /// Message register `word`. [`ProviderError::OutOfRange`] past the
    /// last.
    pub fn read(&self, word: usize) -> Result<u32, ProviderError> {
        let m = self.source.live()?;
        if word >= m.words() {
            return Err(ProviderError::OutOfRange);
        }
        Ok(m.read(word))
    }

    /// Writes message register `word`. [`ProviderError::OutOfRange`] past
    /// the last.
    pub fn write(&self, word: usize, value: u32) -> Result<(), ProviderError> {
        let m = self.source.live()?;
        if word >= m.words() {
            return Err(ProviderError::OutOfRange);
        }
        m.write(word, value);
        Ok(())
    }

    /// Rings the channel's doorbell.
    pub fn ring(&self) -> Result<(), ProviderError> {
        self.source.live()?.ring(self.channel)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::sync::Mutex;
    use std::vec::Vec;

    use super::*;

    #[derive(Default)]
    struct Fake {
        regs: Mutex<[u32; 4]>,
        rung: Mutex<Vec<u32>>,
    }

    impl Mailbox for Fake {
        fn words(&self) -> usize {
            4
        }
        fn read(&self, word: usize) -> u32 {
            self.regs.lock().unwrap()[word]
        }
        fn write(&self, word: usize, value: u32) {
            self.regs.lock().unwrap()[word] = value;
        }
        fn ring(&self, channel: u32) -> Result<(), ProviderError> {
            self.rung.lock().unwrap().push(channel);
            Ok(())
        }
    }

    #[test]
    fn a_channel_reaches_its_mailbox_until_revoked() {
        let fake = Arc::new(Fake::default());
        let src = MailboxSource::new(fake.clone());
        let ch = src.channel(8);
        ch.write(3, 0xabcd).unwrap();
        assert_eq!(ch.read(3), Ok(0xabcd));
        assert_eq!(ch.read(4), Err(ProviderError::OutOfRange));
        assert_eq!(ch.write(4, 1), Err(ProviderError::OutOfRange));
        ch.ring().unwrap();
        assert_eq!(*fake.rung.lock().unwrap(), [8]);
        src.revoke();
        assert_eq!(ch.read(0), Err(ProviderError::Revoked));
        assert_eq!(ch.ring(), Err(ProviderError::Revoked));
    }
}
