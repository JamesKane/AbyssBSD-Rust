// SPDX-License-Identifier: BSD-2-Clause
//! The interface as CAM's SIM (`umass_cam_action`): its path inquiry
//! and transfer settings, and `XPT_SCSI_IO` from a LUN's pass device.
//!
//! A pass-through command is queued with the disks' requests and run by
//! a worker, so it never overlaps the transport's other commands where
//! the transport allows only one (Bulk-Only). A failed one comes back
//! with its sense data as the CCB asked (`sense_len` bytes requested
//! over Bulk-Only; what came with the status over UAS).
//!
//! Behavior reference: sys/dev/usb/storage/umass.c (`umass_cam_action`,
//! `umass_cam_cb`, `umass_cam_sense_cb`), sys/cam/scsi/scsi_xpt.c
//! (`probedone`, `scsi_devise_transport`).

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::cam::{proto, status, xport};
use cam::ccb::{PathInq, TranSettings};
use cam::io::{Dir, ScsiIo};
use sched::{Sleepable, WaitQueue};
use scsi::Cdb;
use sync::SpinLock;
use usb::Speed;
use vm::object::IoBuf;

use crate::{CmdError, Data, Inner, MAX_TRANSFER, Probed, Transport, Work};

/// SCSI statuses.
const CHECK_CONDITION: u8 = 0x02;
const BUSY: u8 = 0x08;

/// A command, before a worker takes it.
struct Request {
    cdb: Cdb,
    dir: Dir,
    data: Vec<u8>,
    sense_len: u8,
}

/// How it ended.
struct Ended {
    result: Result<u32, CmdError>,
    sense: Vec<u8>,
    data: Vec<u8>,
}

/// A pass-through command on the worker queue.
pub(crate) struct Job {
    req: SpinLock<Option<Request>>,
    done: SpinLock<Option<Ended>>,
    wq: WaitQueue,
}

impl Job {
    fn finish(&self, e: Ended) {
        *self.done.lock() = Some(e);
        self.wq.wake_all();
    }

    /// Runs it on lane `lane`, LUN `lun` (a worker).
    pub(crate) fn run(&self, s: &Sleepable, t: &Transport, lane: u16, lun: u16) {
        let Some(r) = self.req.lock().take() else {
            return;
        };
        let data = match r.dir {
            Dir::None => Data::None,
            _ if r.data.is_empty() => Data::None,
            Dir::In => Data::In(IoBuf::Owned(r.data)),
            Dir::Out | Dir::Both => Data::Out(IoBuf::Owned(r.data)),
        };
        let mut sense = Vec::new();
        let (buf, result) = t.command_sense(s, lane, lun, &r.cdb, data, r.sense_len, &mut sense);
        self.finish(Ended {
            result,
            sense,
            data: buf.into_vec(),
        });
    }

    /// The device went before a worker ran it.
    pub(crate) fn gone(&self) {
        let data = self.req.lock().take().map(|r| r.data).unwrap_or_default();
        self.finish(Ended {
            result: Err(CmdError::Gone),
            sense: Vec::new(),
            data,
        });
    }
}

/// The interface's SIM. CAM holds it until the bus goes; the instance
/// (and with it the USB device) it holds weakly.
pub(crate) struct UmassSim {
    pub(crate) inner: Weak<Inner>,
    pub(crate) unit: u32,
    pub(crate) speed: Speed,
    pub(crate) max_lun: u32,
}

impl cam::Sim for UmassSim {
    fn path_inq(&self, _: u64) -> PathInq {
        PathInq {
            version_num: 1,
            hba_misc: abi::cam::pim::NO_6_BYTE,
            // One target (`UMASS_SCSIID_MAX`), the host after it.
            max_target: 1,
            initiator_id: 1,
            sim_vid: "FreeBSD",
            hba_vid: "USB SCSI",
            dev_name: "umass-sim",
            unit_number: self.unit,
            bus_id: self.unit,
            protocol: proto::SCSI,
            protocol_version: 2,
            transport: xport::USB,
            transport_version: 0,
            base_transfer_speed: match self.speed {
                Speed::Super | Speed::SuperPlus => 400_000,
                Speed::High => 40_000,
                Speed::Low | Speed::Full => 1_000,
            },
            max_lun: self.max_lun,
            maxio: u32::try_from(MAX_TRANSFER).unwrap_or(u32::MAX),
            ..PathInq::default()
        }
    }

    fn trans_settings(&self, _: &Sleepable, _: u32, _: bool) -> TranSettings {
        TranSettings {
            protocol: proto::SCSI,
            protocol_version: 2,
            transport: xport::USB,
            transport_version: 0,
            ..TranSettings::default()
        }
    }

    fn scsi_io(&self, s: &Sleepable, target: u32, lun: u64, io: &mut ScsiIo) -> u32 {
        let Some(inner) = self.inner.upgrade() else {
            return status::DEV_NOT_THERE;
        };
        let Some(i) = inner
            .units
            .iter()
            .position(|u| target == 0 && u64::from(u.lun) == lun)
        else {
            return status::SEL_TIMEOUT;
        };
        let Some(cdb) = Cdb::from_bytes(&io.cdb) else {
            return status::REQ_INVALID;
        };
        let job = Arc::new(Job {
            req: SpinLock::new(Some(Request {
                cdb,
                dir: io.dir,
                data: core::mem::take(&mut io.data),
                sense_len: io.sense_len,
            })),
            done: SpinLock::new(None),
            wq: WaitQueue::new(),
        });
        inner
            .queue
            .lock()
            .push_back(Work::Pass(i, Arc::clone(&job)));
        inner.wq.wake_all();
        // The workers hold the instance while they run it.
        drop(inner);
        job.wq.wait_until(s, || job.done.lock().is_some());
        let Some(e) = job.done.lock().take() else {
            return status::REQ_CMP_ERR;
        };
        io.data = e.data;
        match e.result {
            Ok(residue) => {
                io.scsi_status = 0;
                io.resid = residue;
                status::REQ_CMP
            }
            Err(CmdError::Check(_)) => {
                io.scsi_status = CHECK_CONDITION;
                if !e.sense.is_empty() {
                    io.sense = e.sense;
                    status::SCSI_STATUS_ERROR | status::AUTOSNS_VALID
                } else if io.sense_len > 0 {
                    status::AUTOSENSE_FAIL
                } else {
                    status::SCSI_STATUS_ERROR
                }
            }
            Err(CmdError::Busy) => {
                io.scsi_status = BUSY;
                status::SCSI_STATUS_ERROR
            }
            Err(CmdError::Transport) => status::REQ_CMP_ERR,
            Err(CmdError::Gone) => status::DEV_NOT_THERE,
        }
    }
}

/// CAM's view of LUN `p`, `da` unit `unit` (`probedone`).
pub(crate) fn device_info(p: &Probed, unit: u32) -> cam::DeviceInfo {
    let mut i = cam::DeviceInfo::new(0, u64::from(p.lun), proto::SCSI, ("da", unit));
    i.protocol_version = u32::from(p.inquiry.version);
    i.transport = xport::USB;
    i.transport_version = 0;
    let mut inq = Box::new([0u8; 256]);
    let n = p.inq_raw.len().min(256);
    inq[..n].copy_from_slice(&p.inq_raw[..n]);
    // What the SIM cannot do is not claimed: tagged queueing,
    // synchronous and wide transfers (`scsi_set_transfer_settings`).
    i.inq_flags = inq.get(7).copied().unwrap_or(0) & !(0x02 | 0x10 | 0x20 | 0x40);
    i.inq_data = inq;
    if let Some(sn) = &p.serial {
        i.serial = sn.trim_matches(' ').as_bytes().to_vec();
    }
    i
}
