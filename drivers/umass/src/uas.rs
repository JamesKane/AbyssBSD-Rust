// SPDX-License-Identifier: BSD-2-Clause
//! USB Attached SCSI (T10 UAS-2): a command pipe, a status pipe, and a
//! data pipe each way, named by their pipe usage descriptors.
//!
//! - **USB 3:** the status and data pipes have bulk streams, and a
//!   command's tag is its stream, so tagged commands run at once: the
//!   status and data transfers are queued on the tag's stream, then the
//!   command IU is sent.
//! - **USB 2:** no streams. One command at a time: the device says on the
//!   status pipe when it is ready for the data (`READ READY`, `WRITE
//!   READY`), then sends the sense IU.
//!
//! Sense data comes with the status (autosense). A command whose
//! transport broke (a timeout, a stall, a tag out of place) is aborted
//! with `ABORT TASK`, then `LOGICAL UNIT RESET` if that fails; its
//! streams' transfers are taken back without touching the other tags'.
//!
//! FreeBSD has no UAS driver: the reference is the specification, with
//! the behavior of FreeBSD's `umass` for what the transports share.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use sched::Sleepable;
use scsi::uas::{self, Function, Pipe, Response, Status};
use scsi::{Cdb, status};
use usb::descriptor::Endpoint;
use usbus::transfer::Waiter;
use usbus::{UsbDevice, UsbError, UsbInterface};
use vm::object::IoBuf;

use crate::{CmdError, Data};

/// Timeouts: an IU on the command pipe, a command's status (it waits for
/// the whole command), its data, and a task management function.
const IU_MS: u32 = 5000;
const STATUS_MS: u32 = 30_000;
const DATA_MS: u32 = 20_000;
const TASK_MS: u32 = 5000;
/// Commands at once, at most (each also needs a tag for its task
/// management IUs).
const MAX_LANES: u16 = 8;
/// Streams asked for: FreeBSD's xHCI gives each endpoint 16.
const STREAMS: u16 = 15;

/// A UAS interface.
pub(crate) struct Uas {
    pub(crate) dev: Arc<UsbDevice>,
    cmd: u8,
    status: u8,
    data_in: u8,
    data_out: u8,
    /// Tags are streams; without, one command at a time.
    streams: bool,
    /// Commands at once: tags 1 to `lanes`, and `lane + lanes` for each
    /// one's task management.
    lanes: u16,
}

/// The four pipes' endpoints, by their pipe usage descriptors.
fn pipes(iface: &UsbInterface) -> Option<[&Endpoint; 4]> {
    let find = |want: Pipe| {
        iface.desc.endpoints.iter().find(|e| {
            iface.endpoint_descriptors(e).any(|(t, b)| {
                t == uas::PIPE_USAGE && b.get(2..).and_then(Pipe::parse) == Some(want)
            })
        })
    };
    Some([
        find(Pipe::Command)?,
        find(Pipe::Status)?,
        find(Pipe::DataIn)?,
        find(Pipe::DataOut)?,
    ])
}

impl Uas {
    /// Sets up `iface`'s pipes: streams on the status and data pipes on
    /// USB 3.
    pub(crate) fn new(s: &Sleepable, iface: &UsbInterface) -> Result<Self, &'static str> {
        let [cmd, st, din, dout] = pipes(iface).ok_or("no UAS pipes")?;
        let dev = Arc::clone(&iface.device);
        let (streams, lanes) = if dev.speed.is_super() {
            let eps = [st.clone(), din.clone(), dout.clone()];
            let n = dev
                .open_streams(s, &eps, STREAMS)
                .map_err(|_| "no streams")?;
            (true, (n / 2).clamp(1, MAX_LANES))
        } else {
            (false, 1)
        };
        Ok(Self {
            dev,
            cmd: cmd.address,
            status: st.address,
            data_in: din.address,
            data_out: dout.address,
            streams,
            lanes,
        })
    }

    /// Commands that can run at once.
    pub(crate) fn lanes(&self) -> u16 {
        self.lanes
    }

    /// Queues a transfer on `ep` (on stream `tag` with streams).
    fn post(
        &self,
        ep: u8,
        buf: IoBuf,
        ms: u32,
        tag: u16,
    ) -> Result<Arc<Waiter>, (IoBuf, UsbError)> {
        let w = Waiter::new();
        let mut t = w.transfer(buf, ms);
        if self.streams && ep != self.cmd {
            t = t.on_stream(tag);
        }
        match self.dev.submit(ep, t) {
            Ok(()) => Ok(w),
            Err(r) => {
                let mut t = r.transfer;
                Err((core::mem::take(&mut t.buf), r.error))
            }
        }
    }

    /// Sends an IU on the command pipe and waits for it.
    fn send(&self, s: &Sleepable, iu: Vec<u8>) -> Result<(), UsbError> {
        let w = self
            .post(self.cmd, IoBuf::from(iu), IU_MS, 0)
            .map_err(|(_, e)| e)?;
        let r = w.wait(s).1;
        if r == Err(UsbError::Stalled) {
            let _ = self.dev.clear_halt(s, self.cmd);
        }
        r.map(|_| ())
    }

    /// Takes back what is queued on `ep` for `tag`.
    fn take_back(&self, s: &Sleepable, ep: u8, tag: u16) {
        if self.streams {
            self.dev.abort_stream(s, ep, tag);
        } else {
            self.dev.abort(s, ep);
        }
    }

    /// A status pipe read, waited for and parsed.
    fn read_status(w: &Waiter, s: &Sleepable) -> Result<Status, UsbError> {
        let (b, r) = w.wait(s);
        let n = r?;
        match b {
            IoBuf::Owned(v) => {
                Status::parse(&v[..n.min(v.len())]).ok_or(UsbError::Protocol("bad UAS IU"))
            }
            _ => Err(UsbError::Protocol("bad UAS IU")),
        }
    }

    /// Runs `cdb` on `lun` as tag `lane`.
    pub(crate) fn command(
        &self,
        s: &Sleepable,
        lane: u16,
        lun: u16,
        cdb: &Cdb,
        data: Data,
    ) -> (IoBuf, Result<u32, CmdError>) {
        let (len, is_in, buf) = data.split();
        if self.dev.is_gone() {
            return (buf, Err(CmdError::Gone));
        }
        let tag = lane;
        let data_ep = if is_in { self.data_in } else { self.data_out };
        let status_buf = || IoBuf::from(vec![0; uas::STATUS_LEN]);
        let st = match self.post(self.status, status_buf(), STATUS_MS, tag) {
            Ok(w) => w,
            Err((_, e)) => return (buf, Err(self.broken(s, tag, lun, e))),
        };
        let mut held = buf;
        let mut data = None;
        // With streams the data waits on its stream from the start.
        if self.streams && len > 0 {
            match self.post(data_ep, core::mem::take(&mut held), DATA_MS, tag) {
                Ok(w) => data = Some(w),
                Err((b, e)) => {
                    self.take_back(s, self.status, tag);
                    let _ = st.wait(s);
                    return (b, Err(self.broken(s, tag, lun, e)));
                }
            }
        }
        if let Err(e) = self.send(s, uas::command(tag, lun, cdb).to_vec()) {
            self.take_back(s, self.status, tag);
            let _ = st.wait(s);
            if let Some(w) = data {
                self.take_back(s, data_ep, tag);
                held = w.wait(s).0;
            }
            return (held, Err(self.broken(s, tag, lun, e)));
        }
        let mut moved = 0;
        let mut status = Self::read_status(&st, s);
        if !self.streams {
            // The device asks for the data phase, then sends the status.
            let ready = match status {
                Ok(Status::ReadReady(t)) => t == tag && is_in && len > 0,
                Ok(Status::WriteReady(t)) => t == tag && !is_in && len > 0,
                _ => false,
            };
            if ready {
                let r = self
                    .post(data_ep, core::mem::take(&mut held), DATA_MS, tag)
                    .map(|w| w.wait(s));
                let r = match r {
                    Ok((b, r)) => {
                        held = b;
                        r
                    }
                    Err((b, e)) => {
                        held = b;
                        Err(e)
                    }
                };
                status = match r {
                    Ok(n) => {
                        moved = n;
                        match self.post(self.status, status_buf(), STATUS_MS, tag) {
                            Ok(w) => Self::read_status(&w, s),
                            Err((_, e)) => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };
            }
        } else if let Some(w) = data {
            // A command that failed, or moved less than asked without a
            // short packet, leaves its data transfer queued.
            if !w.is_done() {
                self.take_back(s, data_ep, tag);
            }
            let (b, r) = w.wait(s);
            held = b;
            match r {
                Ok(n) => moved = n,
                Err(UsbError::Cancelled) if status.is_ok() => {}
                Err(e) => {
                    if status.is_ok() {
                        status = Err(e);
                    }
                }
            }
        }
        let residue = u32::try_from(len.saturating_sub(moved)).unwrap_or(u32::MAX);
        let r = match status {
            Ok(Status::Sense {
                tag: t,
                status: st,
                sense,
            }) if t == tag => match st {
                status::GOOD => Ok(residue),
                status::CHECK_CONDITION => Err(CmdError::Check(sense)),
                status::BUSY | status::TASK_SET_FULL => Err(CmdError::Busy),
                _ => Err(CmdError::Check(None)),
            },
            // The device refused the IU: nothing is left running.
            Ok(Status::Response { tag: t, .. }) if t == tag => Err(CmdError::Transport),
            Ok(_) => Err(self.broken(s, tag, lun, UsbError::Protocol("UAS tag"))),
            Err(e) => Err(self.broken(s, tag, lun, e)),
        };
        (held, r)
    }

    /// A command's transport broke: aborts it so its tag can be used
    /// again.
    fn broken(&self, s: &Sleepable, tag: u16, lun: u16, e: UsbError) -> CmdError {
        if e == UsbError::Gone || self.dev.is_gone() {
            return CmdError::Gone;
        }
        if e == UsbError::Stalled {
            let _ = self.dev.clear_halt(s, self.status);
        }
        if !self.task(s, tag, lun, Function::AbortTask)
            && !self.task(s, tag, lun, Function::LogicalUnitReset)
        {
            for ep in [self.cmd, self.status, self.data_in, self.data_out] {
                let _ = self.dev.clear_halt(s, ep);
            }
        }
        CmdError::Transport
    }

    /// Runs task management function `f` about task `tag`; whether it
    /// completed.
    fn task(&self, s: &Sleepable, tag: u16, lun: u16, f: Function) -> bool {
        let t = tag + self.lanes;
        let Ok(w) = self.post(
            self.status,
            IoBuf::from(vec![0; uas::STATUS_LEN]),
            TASK_MS,
            t,
        ) else {
            return false;
        };
        if self.send(s, uas::task(t, lun, f, tag).to_vec()).is_err() {
            self.take_back(s, self.status, t);
            let _ = w.wait(s);
            return false;
        }
        matches!(
            Self::read_status(&w, s),
            Ok(Status::Response {
                tag: rt,
                code: Response::Complete | Response::Succeeded,
            }) if rt == t
        )
    }
}
