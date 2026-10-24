// SPDX-License-Identifier: BSD-2-Clause
//! The Bulk-Only Transport (BBB 1.0): one command at a time, as a CBW,
//! its data, and a CSW. A failed command's sense data is asked for at
//! once, as FreeBSD's `umass` does; a stall, a bad CSW, or a phase error
//! gets the reset recovery (§5.3.4).
//!
//! Behavior reference: sys/dev/usb/storage/umass.c (`umass_t_bbb_*`).

use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicU32, Ordering};

use sched::Sleepable;
use scsi::bbb::{self, Csw, Status};
use scsi::{Cdb, Sense};
use usb::Setup;
use usb::request::rt;
use usbus::transfer::Waiter;
use usbus::{UsbDevice, UsbError};
use vm::object::IoBuf;

use crate::{CmdError, Data, SENSE_LEN};

/// Timeouts: the wrappers, and a data phase.
const WRAPPER_MS: u32 = 5000;
const DATA_MS: u32 = 20_000;

/// A Bulk-Only interface.
pub(crate) struct Bbb {
    pub(crate) dev: Arc<UsbDevice>,
    pub(crate) iface: u8,
    pub(crate) bulk_in: u8,
    pub(crate) bulk_out: u8,
    tag: AtomicU32,
}

/// How a command ended, before its sense data is asked for.
enum Raw {
    Failed,
    Transport,
    Gone,
}

impl Bbb {
    pub(crate) fn new(dev: Arc<UsbDevice>, iface: u8, bulk_in: u8, bulk_out: u8) -> Self {
        Self {
            dev,
            iface,
            bulk_in,
            bulk_out,
            tag: AtomicU32::new(1),
        }
    }

    /// Get Max LUN: the highest LUN, 0 if the device refuses (as FreeBSD
    /// takes a stall).
    pub(crate) fn max_lun(&self, s: &Sleepable) -> u8 {
        let setup = Setup {
            request_type: rt::IN | rt::CLASS | rt::INTERFACE,
            request: bbb::GET_MAX_LUN,
            value: 0,
            index: u16::from(self.iface),
            length: 1,
        };
        self.dev
            .control(s, setup, None)
            .ok()
            .and_then(|v| v.first().copied())
            .filter(|&n| n <= 15)
            .unwrap_or(0)
    }

    /// One transfer, waited for.
    fn transfer(
        &self,
        s: &Sleepable,
        ep: u8,
        buf: IoBuf,
        ms: u32,
    ) -> (IoBuf, Result<usize, UsbError>) {
        let w = Waiter::new();
        if let Err(r) = self.dev.submit(ep, w.transfer(buf, ms)) {
            let mut t = r.transfer;
            return (core::mem::take(&mut t.buf), Err(r.error));
        }
        w.wait(s)
    }

    /// The bulk-only reset recovery: the class reset, then both halts
    /// cleared.
    fn reset_recovery(&self, s: &Sleepable) {
        let setup = Setup {
            request_type: rt::CLASS | rt::INTERFACE,
            request: bbb::RESET,
            value: 0,
            index: u16::from(self.iface),
            length: 0,
        };
        let _ = self.dev.control(s, setup, None);
        let _ = self.dev.clear_halt(s, self.bulk_in);
        let _ = self.dev.clear_halt(s, self.bulk_out);
    }

    /// Runs `cdb` on `lun`; sense data is asked for after a failure.
    pub(crate) fn command(
        &self,
        s: &Sleepable,
        lun: u8,
        cdb: &Cdb,
        data: Data,
    ) -> (IoBuf, Result<u32, CmdError>) {
        let (buf, r) = self.raw(s, lun, cdb, data);
        let e = match r {
            Ok(residue) => return (buf, Ok(residue)),
            Err(Raw::Gone) => CmdError::Gone,
            Err(Raw::Transport) => CmdError::Transport,
            Err(Raw::Failed) => {
                let (b, r) = self.raw(
                    s,
                    lun,
                    &Cdb::request_sense(SENSE_LEN),
                    Data::In(IoBuf::from(vec![0; SENSE_LEN.into()])),
                );
                match (b, r) {
                    (IoBuf::Owned(v), Ok(_)) => CmdError::Check(Sense::parse(&v)),
                    (_, Err(Raw::Gone)) => CmdError::Gone,
                    _ => CmdError::Check(None),
                }
            }
        };
        (buf, Err(e))
    }

    /// CBW, data, CSW.
    fn raw(&self, s: &Sleepable, lun: u8, cdb: &Cdb, data: Data) -> (IoBuf, Result<u32, Raw>) {
        let (len, is_in, buf) = data.split();
        let gone = |b| (b, Err(Raw::Gone));
        if self.dev.is_gone() {
            return gone(buf);
        }
        let tag = self.tag.fetch_add(1, Ordering::Relaxed);
        let n = u32::try_from(len).unwrap_or(u32::MAX);
        let cbw = bbb::cbw(tag, n, is_in, lun, cdb);
        let (_, r) = self.transfer(s, self.bulk_out, IoBuf::from(cbw.to_vec()), WRAPPER_MS);
        match r {
            Ok(_) => {}
            Err(UsbError::Gone) => return gone(buf),
            Err(_) => {
                self.reset_recovery(s);
                return (buf, Err(Raw::Transport));
            }
        }
        let mut buf = buf;
        if len > 0 {
            let ep = if is_in { self.bulk_in } else { self.bulk_out };
            let (b, r) = self.transfer(s, ep, buf, DATA_MS);
            buf = b;
            match r {
                Ok(_) => {}
                // A stalled data phase: clear it and read the status.
                Err(UsbError::Stalled) => {
                    let _ = self.dev.clear_halt(s, ep);
                }
                Err(UsbError::Gone) => return gone(buf),
                Err(_) => {
                    self.reset_recovery(s);
                    return (buf, Err(Raw::Transport));
                }
            }
        }
        let mut csw = None;
        // A stalled CSW is cleared and read once more (§6.7.2).
        for _ in 0..2 {
            let (b, r) = self.transfer(
                s,
                self.bulk_in,
                IoBuf::from(vec![0; bbb::CSW_LEN]),
                WRAPPER_MS,
            );
            match r {
                Ok(n) => {
                    if let IoBuf::Owned(v) = b {
                        csw = Csw::parse(&v[..n.min(v.len())]);
                    }
                    break;
                }
                Err(UsbError::Stalled) => {
                    let _ = self.dev.clear_halt(s, self.bulk_in);
                }
                Err(UsbError::Gone) => return gone(buf),
                Err(_) => break,
            }
        }
        match csw {
            Some(c) if c.tag == tag && c.status == Status::Passed => (buf, Ok(c.residue.min(n))),
            Some(c) if c.tag == tag && c.status == Status::Failed => (buf, Err(Raw::Failed)),
            _ => {
                self.reset_recovery(s);
                (buf, Err(Raw::Transport))
            }
        }
    }
}
