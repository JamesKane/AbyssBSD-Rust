// SPDX-License-Identifier: BSD-2-Clause
//! `cix_mbox`: the CIX Sky1's mailboxes (ACPI `CIXHA001`), as the
//! clean-room `sky1-soc` specification describes them (§1.1).
//!
//! Each mailbox is one direction between the application processors and
//! another processor (`_DSD` `cix,mbox_dir`: 0 sends from here, 1
//! receives). Its first 128 bytes are 32 message registers, which SCMI
//! uses as its shared memory on the link to the power-management
//! processor; writing bit 0 of `DB_ACK` rings the peer's doorbell on a
//! sending mailbox. Only the doorbell channel (8) is served, the one SCMI
//! uses, and it is polled: no interrupts are enabled. The FIFO, register
//! and fast channels (the security engine's and the audio DSP's) are not.
//!
//! The device is FreeBSD's `cix_mbox` in the AbyssBSD Project's FreeBSD
//! port for the board; it is not in FreeBSD itself (docs/compat-deviations.md).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;

use bus::mailbox::Mailbox;
use bus::provider::ProviderError;
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, StartCx, StopCx, StopReason,
};
use devtree::ids::AcpiId;
use regs::Bytes;

/// Message registers.
pub const WORDS: usize = 32;
/// `DB_ACK`: bit 0 rings the peer's doorbell.
pub const DB_ACK: usize = 0x80;
const DB_RING: u32 = 1 << 0;
/// The doorbell channel.
pub const DOORBELL: u32 = 8;
/// The registers the driver touches end here.
const SPAN: usize = DB_ACK + 4;

/// Which way a mailbox carries messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the application processors to the peer.
    Send,
    /// From the peer.
    Receive,
}

impl Direction {
    /// `cix,mbox_dir`'s value.
    #[must_use]
    pub fn from_property(v: u64) -> Option<Self> {
        match v {
            0 => Some(Self::Send),
            1 => Some(Self::Receive),
            _ => None,
        }
    }
}

/// A mailbox's 32-bit registers.
pub trait Words: Send + Sync {
    /// Reads the word at `offset`.
    fn read32(&self, offset: usize) -> u32;
    /// Writes the word at `offset`.
    fn write32(&self, offset: usize, v: u32);
}

impl Words for MmioGrant<Bytes> {
    fn read32(&self, offset: usize) -> u32 {
        regs::Mmio::read32(self, offset)
    }
    fn write32(&self, offset: usize, v: u32) {
        regs::Mmio::write32(self, offset, v);
    }
}

/// One mailbox: [`Mailbox`] over its registers.
#[derive(Debug)]
pub struct CixMailbox<W> {
    regs: W,
    dir: Direction,
}

impl<W: Words> CixMailbox<W> {
    /// The mailbox at `regs`, carrying messages `dir`.
    pub fn new(regs: W, dir: Direction) -> Self {
        Self { regs, dir }
    }
}

impl<W: Words> Mailbox for CixMailbox<W> {
    fn words(&self) -> usize {
        WORDS
    }

    fn read(&self, word: usize) -> u32 {
        self.regs.read32(4 * word)
    }

    fn write(&self, word: usize, value: u32) {
        self.regs.write32(4 * word, value);
    }

    fn ring(&self, channel: u32) -> Result<(), ProviderError> {
        if channel != DOORBELL || self.dir != Direction::Send {
            return Err(ProviderError::Unsupported);
        }
        // Device memory: the message registers' writes reach the mailbox
        // before this one.
        self.regs.write32(DB_ACK, DB_RING);
        Ok(())
    }
}

/// `cix_mboxN`.
#[derive(Debug)]
pub struct CixMbox {
    mailbox: Arc<CixMailbox<MmioGrant<Bytes>>>,
}

impl Driver for CixMbox {
    const NAME: &'static str = "cix_mbox";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXHA001")]);

    fn describe(_: &Ident) -> String {
        "CIX mailbox".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let dir = acpi::properties::integer(cx, "cix,mbox_dir")
            .and_then(Direction::from_property)
            .ok_or(AttachError::Device("no direction (cix,mbox_dir)"))?;
        // The SCMI shared memory devices describe the first 128 bytes of
        // the same window (platform/cix-sky1 declares the overlap).
        let regs = cx.mmio_bytes(0)?;
        if regs.len() < SPAN {
            return Err(AttachError::Device("register window too small"));
        }
        Ok(Self {
            mailbox: Arc::new(CixMailbox::new(regs, dir)),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.provide_mailbox(Arc::clone(&self.mailbox) as Arc<dyn Mailbox>);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<CixMbox>();

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::vec::Vec;

    use super::*;

    #[derive(Default)]
    struct Fake(Mutex<BTreeMap<usize, u32>>, Mutex<Vec<(usize, u32)>>);

    impl Words for Fake {
        fn read32(&self, off: usize) -> u32 {
            self.0.lock().unwrap().get(&off).copied().unwrap_or(0)
        }
        fn write32(&self, off: usize, v: u32) {
            self.0.lock().unwrap().insert(off, v);
            self.1.lock().unwrap().push((off, v));
        }
    }

    #[test]
    fn messages_then_the_doorbell() {
        let m = CixMailbox::new(Fake::default(), Direction::Send);
        m.write(7, 0x5007);
        assert_eq!(m.read(7), 0x5007);
        m.ring(DOORBELL).unwrap();
        assert_eq!(*m.regs.1.lock().unwrap(), [(0x1c, 0x5007), (DB_ACK, 1)]);
        // Only the doorbell channel, and only from a sending mailbox.
        assert_eq!(m.ring(9), Err(ProviderError::Unsupported));
        let rx = CixMailbox::new(Fake::default(), Direction::Receive);
        assert_eq!(rx.ring(DOORBELL), Err(ProviderError::Unsupported));
        assert_eq!(Direction::from_property(2), None);
    }
}
