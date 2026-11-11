// SPDX-License-Identifier: BSD-2-Clause
//! Pass-through commands (`nvme_ctrlr_passthrough_cmd`, `nvme_sim`'s
//! `XPT_NVME_IO` and `XPT_NVME_ADMIN`): a command from userland, its
//! data in a kernel buffer, run to completion.
//!
//! - Admin commands go on the admin queue, polled under its lock, as the
//!   driver's own do.
//! - I/O commands take a command ID on the caller's CPU's I/O queue,
//!   beside the disks' requests (waiting while the queue is full or
//!   being reset), and complete through the same interrupt path: retried
//!   as theirs are, aborted with them by a reset that fails.
//! - The completion's status comes back without its phase bit; on a
//!   failed controller a command ends aborted (do not retry).
//!
//! Behavior reference: sys/dev/nvme/nvme_ctrlr.c
//! (`nvme_ctrlr_passthrough_cmd`), sys/dev/nvme/nvme_sim.c
//! (`nvme_sim_nvmeio`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::nvme::NvmeCommand;
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vm::object::IoBuf;

use crate::status::{self, Status};
use crate::timeout::Deadline;
use crate::{
    ADMIN_FAILED, Command, Controller, Data, InFlight, PRP_LIST, QueueStats, Recovery, Req,
    prp_pair, prp_shaped, sleep_ms,
};

/// How a pass-through command ended.
#[derive(Debug)]
pub(crate) struct Done {
    /// The completion's status, without its phase bit.
    pub(crate) status: Status,
    /// Dword 0.
    pub(crate) dw0: u32,
    /// The submission queue it ran on, and its command ID.
    pub(crate) sqid: u16,
    pub(crate) cid: u16,
    /// Its data buffer.
    pub(crate) buf: IoBuf,
}

/// Where a pass-through command's sender waits.
pub(crate) struct Waiter {
    done: SpinLock<Option<Done>>,
    wq: WaitQueue,
}

impl Waiter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: SpinLock::new(None),
            wq: WaitQueue::new(),
        })
    }

    /// Ends the command (from the completion path, any context).
    pub(crate) fn finish(&self, d: Done) {
        *self.done.lock() = Some(d);
        self.wq.wake_all();
    }

    fn wait(&self, s: &Sleepable) -> Done {
        loop {
            self.wq.wait_until(s, || self.done.lock().is_some());
            if let Some(d) = self.done.lock().take() {
                return d;
            }
        }
    }
}

/// What a pass-through command came back with.
#[derive(Debug)]
pub(crate) struct Outcome {
    /// The completion's status, without its phase bit.
    pub(crate) status: Status,
    /// Dword 0.
    pub(crate) dw0: u32,
    /// The queue and command ID.
    pub(crate) sqid: u16,
    pub(crate) cid: u16,
    /// The data: what the controller wrote, or what was sent.
    pub(crate) data: Vec<u8>,
}

/// A command aborted without being sent: what a failed controller's
/// queues answer (`nvme_qpair_manual_complete_request`, do not retry).
fn aborted(data: Vec<u8>) -> Outcome {
    Outcome {
        status: status::make(0, status::SC_ABORTED_BY_REQUEST, true),
        dw0: 0,
        sqid: 0,
        cid: 0,
        data,
    }
}

/// The command userland gave, as the driver sends it: everything but
/// the command ID, metadata pointer, and data pointers (which are the
/// driver's).
fn command(c: &NvmeCommand) -> Command {
    Command {
        opcode: c.opc,
        fuse: c.fuse,
        nsid: c.nsid,
        cdw2: c.rsvd2,
        cdw3: c.rsvd3,
        prp1: 0,
        prp2: 0,
        cdw10: c.cdw10,
        cdw11: c.cdw11,
        cdw12: c.cdw12,
        cdw13: c.cdw13,
        cdw14: c.cdw14,
        cdw15: c.cdw15,
    }
}

impl Controller {
    /// Maps `data` for the controller: written to it unless `read`.
    fn map_pass(&self, data: Vec<u8>, read: bool) -> Result<Data, Errno> {
        if data.is_empty() {
            return Ok(Data::None);
        }
        let buf = IoBuf::Owned(data);
        if read {
            self.tag.map(buf).map(Data::Read).map_err(|_| Errno::ENOMEM)
        } else {
            self.tag
                .map(buf)
                .map(Data::Write)
                .map_err(|_| Errno::ENOMEM)
        }
    }

    /// Runs `c` (an admin command if `admin`, else an I/O command) with
    /// `data` (read into if `read`, else sent), and waits for it.
    ///
    /// # Errors
    ///
    /// `ENOMEM` if the buffer cannot be mapped, `EINVAL` if it cannot be
    /// described by PRPs.
    pub(crate) fn passthrough(
        &self,
        s: &Sleepable,
        admin: bool,
        c: &NvmeCommand,
        read: bool,
        data: Vec<u8>,
    ) -> Result<Outcome, Errno> {
        if self.failed.load(Ordering::Acquire) {
            return Ok(aborted(data));
        }
        let data = self.map_pass(data, read)?;
        let shaped = match &data {
            Data::None => true,
            Data::Read(d) => prp_shaped(d.segments(), PRP_LIST / 8),
            Data::Write(d) => prp_shaped(d.segments(), PRP_LIST / 8),
        };
        if !shaped {
            drop(data.unmap());
            return Err(Errno::EINVAL);
        }
        if admin {
            Ok(self.admin_pass(s, c, data))
        } else {
            Ok(self.io_pass(s, c, data))
        }
    }

    /// An admin command, polled.
    fn admin_pass(&self, s: &Sleepable, c: &NvmeCommand, data: Data) -> Outcome {
        let mut cmd = command(c);
        let mut admin = self.admin.lock(s);
        let segs: &[vm::dma::Segment] = match &data {
            Data::None => &[],
            Data::Read(d) => d.segments(),
            Data::Write(d) => d.segments(),
        };
        (cmd.prp1, cmd.prp2) = prp_pair(segs, &self.admin_prp, 0);
        let r = admin.run(&self.regs, s, &cmd, self.admin_timeout_ms());
        drop(admin);
        let (status, dw0) = match r {
            Ok(dw0) => (0, dw0),
            // Never completed: as the reset that follows a timeout
            // would end it.
            Err(ADMIN_FAILED) => (status::make(0, status::SC_ABORTED_BY_REQUEST, true), 0),
            Err(st) => (st, 0),
        };
        Outcome {
            status,
            dw0,
            sqid: 0,
            cid: 0,
            data: data.unmap().into_vec(),
        }
    }

    /// An I/O command, on this CPU's queue.
    fn io_pass(&self, s: &Sleepable, c: &NvmeCommand, data: Data) -> Outcome {
        let qi = sched::this_cpu().id().index() % self.queues.len();
        let w = Waiter::new();
        let mut f = Some(InFlight {
            req: Req::Pass(Arc::clone(&w)),
            data,
            cmd: command(c),
            retries: 0,
            deadline: Deadline::Unset,
        });
        let wake = loop {
            if self.failed.load(Ordering::Acquire) {
                let data = f
                    .take()
                    .map_or(IoBuf::Owned(Vec::new()), |f| f.data.unmap());
                return aborted(data.into_vec());
            }
            let mut io = self.queues[qi].lock(s);
            if io.recovery == Recovery::None
                && let Some(&cid) = io.free.last()
                && let Some(mut inflight) = f.take()
            {
                let segs: &[vm::dma::Segment] = match &inflight.data {
                    Data::None => &[],
                    Data::Read(d) => d.segments(),
                    Data::Write(d) => d.segments(),
                };
                let base = usize::from(cid) * (PRP_LIST / 8);
                (inflight.cmd.prp1, inflight.cmd.prp2) = prp_pair(segs, &io.prps, base);
                io.free.pop();
                io.rings.push(&inflight.cmd, cid);
                io.slots[usize::from(cid)] = Some(inflight);
                QueueStats::add(&self.knobs.stats[qi].cmds, 1);
                self.regs.sq_doorbell(io.id, io.rings.tail);
                break self.arm();
            }
            drop(io);
            sleep_ms(s, 1);
        };
        if wake {
            self.watch.queue.wake_all();
        }
        let d = w.wait(s);
        Outcome {
            status: d.status,
            dw0: d.dw0,
            sqid: d.sqid,
            cid: d.cid,
            data: d.buf.into_vec(),
        }
    }
}
