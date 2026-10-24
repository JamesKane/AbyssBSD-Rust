// SPDX-License-Identifier: BSD-2-Clause
//! USB mass storage (`umass`) over the Bulk-Only Transport and USB
//! Attached SCSI, publishing each logical unit as `daN` to the storage
//! class, without CAM (USB.md §8).
//!
//! - Attach finds the LUNs (Get Max LUN over Bulk-Only, REPORT LUNS over
//!   UAS) and probes each as CAM does: INQUIRY, the serial number, TEST
//!   UNIT READY until the medium is ready, READ CAPACITY (10, then 16
//!   past 2 TiB), and MODE SENSE for write protection. LUN 0 must be a
//!   disk; others that are not are skipped.
//! - Worker threads run the requests, one per command the transport takes
//!   at once (one over Bulk-Only; over UAS on USB 3, one per tag).
//! - Errors are retried as FreeBSD's `da` does: after a `UNIT ATTENTION`,
//!   a `NOT READY` that is becoming ready, `BUSY`, and a broken transport
//!   (recovered first), up to four times.
//! - Unplugged, what is queued and what is in flight fail with `ENXIO`.
//!
//! Behavior reference: sys/dev/usb/storage/umass.c, sys/cam/scsi/scsi_da.c
//! (`da_retry_count`, `daerror`), sys/cam/scsi/scsi_all.c (sense actions).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod bbb;
mod uas;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use bus::{
    Announce, AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use devtree::ids::UsbId;
use sched::{Sleepable, Thread, WaitQueue};
use scsi::{Capacity, Cdb, DeviceType, Inquiry, Sense, key, vpd};
use storage::{Bio, BioError, BioOp, Disk, DiskInfo, DiskQueues};
use sync::SpinLock;
use usb::Speed;
use usb::descriptor::{TransferType, class};
use usbus::{UsbDevice, UsbInterface};
use vm::object::IoBuf;

/// SCSI subclass, and the Bulk-Only and UAS protocols.
const SUBCLASS_SCSI: u8 = 0x06;
const PROTO_BBB: u8 = 0x50;
const PROTO_UAS: u8 = 0x62;
/// The largest transfer per command (FreeBSD's `umass` maximum I/O).
const MAX_TRANSFER: usize = 64 * 1024;
/// TEST UNIT READY attempts while the medium comes ready.
const READY_TRIES: u32 = 20;
/// Tries per request (`da_retry_count` is 4 retries).
const TRIES: u32 = 5;
/// REQUEST SENSE's length.
const SENSE_LEN: u8 = 18;
/// How much REPORT LUNS asks for: 63 LUNs.
const REPORT_LUNS_LEN: u32 = 8 + 63 * 8;

/// Why a command failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CmdError {
    /// CHECK CONDITION, with the sense data if the device gave it.
    Check(Option<Sense>),
    /// BUSY or TASK SET FULL: later.
    Busy,
    /// The transport broke (and was recovered).
    Transport,
    /// The device is gone.
    Gone,
}

/// A direction and buffer for a command's data phase.
enum Data {
    None,
    In(IoBuf),
    Out(IoBuf),
}

impl Data {
    /// Its length, whether it is IN, and its buffer.
    fn split(self) -> (usize, bool, IoBuf) {
        match self {
            Self::None => (0, false, IoBuf::Owned(Vec::new())),
            Self::In(b) => (b.len(), true, b),
            Self::Out(b) => (b.len(), false, b),
        }
    }
}

/// How commands reach the device.
enum Transport {
    Bbb(bbb::Bbb),
    Uas(uas::Uas),
}

impl Transport {
    fn dev(&self) -> &Arc<UsbDevice> {
        match self {
            Self::Bbb(b) => &b.dev,
            Self::Uas(u) => &u.dev,
        }
    }

    /// Commands that can run at once.
    fn lanes(&self) -> u16 {
        match self {
            Self::Bbb(_) => 1,
            Self::Uas(u) => u.lanes(),
        }
    }

    /// FreeBSD's name for the wire protocol, in the attach line.
    fn name(&self) -> &'static str {
        match self {
            Self::Bbb(_) => "Bulk-Only",
            Self::Uas(_) => "UAS",
        }
    }

    /// Runs `cdb` on `lun` from lane `lane` (1 to [`Self::lanes`]); the
    /// data buffer back, and the residue.
    fn command(
        &self,
        s: &Sleepable,
        lane: u16,
        lun: u16,
        cdb: &Cdb,
        data: Data,
    ) -> (IoBuf, Result<u32, CmdError>) {
        match self {
            Self::Bbb(b) => b.command(s, u8::try_from(lun).unwrap_or(0), cdb, data),
            Self::Uas(u) => u.command(s, lane, lun, cdb, data),
        }
    }

    /// Runs `cdb` reading `len` bytes, with one retry after a unit
    /// attention: what came back.
    fn read(&self, s: &Sleepable, lun: u16, cdb: &Cdb, len: usize) -> Result<Vec<u8>, CmdError> {
        let mut last = CmdError::Transport;
        for _ in 0..2 {
            let (b, r) = self.command(s, 1, lun, cdb, Data::In(IoBuf::from(vec![0; len])));
            match r {
                Ok(residue) => {
                    let IoBuf::Owned(mut v) = b else {
                        return Err(CmdError::Transport);
                    };
                    v.truncate(len.saturating_sub(usize::try_from(residue).unwrap_or(0)));
                    return Ok(v);
                }
                Err(CmdError::Check(Some(x))) if x.key == key::UNIT_ATTENTION => {
                    last = CmdError::Check(Some(x));
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

/// What probing a LUN found.
struct Probed {
    lun: u16,
    inquiry: Inquiry,
    /// The unit serial number (VPD page 0x80), if it has one.
    serial: Option<String>,
    capacity: Capacity,
    read_only: bool,
}

/// Probes `lun` as CAM's `probe` and `da` do: `None` if nothing is there,
/// it is not a disk, or its medium never comes ready.
fn probe(s: &Sleepable, t: &Transport, lun: u16) -> Result<Probed, &'static str> {
    let inquiry = t
        .read(s, lun, &Cdb::inquiry(36), 36)
        .map_err(|_| "INQUIRY failed")
        .and_then(|b| Inquiry::parse(&b).ok_or("bad INQUIRY data"))?;
    if inquiry.absent {
        return Err("LUN not present");
    }
    if !matches!(
        inquiry.device_type,
        DeviceType::Direct | DeviceType::Simplified
    ) {
        return Err("not a disk");
    }
    // The serial number, if the supported pages list it.
    let serial = t
        .read(
            s,
            lun,
            &Cdb::inquiry_vpd(vpd::SUPPORTED_PAGES, vpd::LEN),
            vpd::LEN.into(),
        )
        .ok()
        .filter(|l| vpd::supports(l, vpd::UNIT_SERIAL))
        .and_then(|_| {
            t.read(
                s,
                lun,
                &Cdb::inquiry_vpd(vpd::UNIT_SERIAL, vpd::LEN),
                vpd::LEN.into(),
            )
            .ok()
        })
        .and_then(|p| vpd::serial(&p).map(|v| String::from_utf8_lossy(v).into_owned()));
    let mut ready = false;
    for _ in 0..READY_TRIES {
        match t.command(s, 1, lun, &Cdb::test_unit_ready(), Data::None).1 {
            Ok(_) => {
                ready = true;
                break;
            }
            Err(CmdError::Gone) => return Err("device gone"),
            Err(CmdError::Check(Some(x))) if x.no_medium() => break,
            _ => bus::manager().sleep_ms(s, 100),
        }
    }
    if !ready {
        return Err("medium not ready");
    }
    let cap = match t.read(s, lun, &Cdb::read_capacity_10(), 8) {
        Ok(b) => match Capacity::parse_10(&b) {
            Some(c) => Some(c),
            None => t
                .read(s, lun, &Cdb::read_capacity_16(), 32)
                .ok()
                .and_then(|b| Capacity::parse_16(&b)),
        },
        Err(_) => None,
    };
    let capacity = cap
        .filter(|c| c.block_size.is_power_of_two() && (512..=65536).contains(&c.block_size))
        .ok_or("READ CAPACITY failed")?;
    // MODE SENSE's header: the WP bit (some devices refuse it).
    let read_only = t
        .read(s, lun, &Cdb::mode_sense(192), 192)
        .ok()
        .and_then(|b| b.get(2).copied())
        .is_some_and(|d| d & 0x80 != 0);
    Ok(Probed {
        lun,
        inquiry,
        serial,
        capacity,
        read_only,
    })
}

/// A published LUN, as the workers see it.
struct Unit {
    lun: u16,
    block: u32,
}

/// What the instance and its workers share.
struct Inner {
    t: Transport,
    units: Vec<Unit>,
    /// Requests, with the index of their unit.
    queue: SpinLock<VecDeque<(usize, Bio)>>,
    wq: WaitQueue,
    stop: AtomicBool,
}

/// The outcome of one try at a request.
enum Try {
    Done(Result<(), BioError>),
    /// Again, after `ms`.
    Again(u32),
}

impl Inner {
    /// One try at `bio` on `unit`.
    fn attempt(&self, s: &Sleepable, lane: u16, unit: &Unit, cdb: &Cdb, bio: &mut Bio) -> Try {
        let buf = core::mem::take(&mut bio.buf);
        let data = match bio.op {
            BioOp::Flush => Data::None,
            BioOp::Read => Data::In(buf),
            BioOp::Write => Data::Out(buf),
        };
        let (b, r) = self.t.command(s, lane, unit.lun, cdb, data);
        if bio.op != BioOp::Flush {
            bio.buf = b;
        }
        let sense = match r {
            Ok(0) => return Try::Done(Ok(())),
            // Short: a medium that shrank, or a broken device.
            Ok(_) => return Try::Done(Err(BioError::Io)),
            Err(CmdError::Gone) => return Try::Done(Err(BioError::Gone)),
            Err(CmdError::Transport) => return Try::Again(0),
            Err(CmdError::Busy) => return Try::Again(100),
            Err(CmdError::Check(None)) => return Try::Again(0),
            Err(CmdError::Check(Some(x))) => x,
        };
        Try::Done(match sense.key {
            key::NO_SENSE | key::RECOVERED_ERROR => Ok(()),
            key::UNIT_ATTENTION => return Try::Again(0),
            // Becoming ready (ASC 0x04, ASCQ 0x01): wait for it.
            key::NOT_READY if sense.asc == 0x04 && sense.ascq == 0x01 => {
                return Try::Again(500);
            }
            key::DATA_PROTECT => Err(BioError::ReadOnly),
            // A device without the cache command (FreeBSD's
            // NO_SYNCHRONIZE_CACHE quirk) has nothing to flush.
            key::ILLEGAL_REQUEST if bio.op == BioOp::Flush => Ok(()),
            _ => Err(BioError::Io),
        })
    }

    /// Runs one block request.
    fn run(&self, s: &Sleepable, lane: u16, unit: &Unit, mut bio: Bio) {
        let block = u64::from(unit.block.max(1));
        let cdb = match bio.op {
            BioOp::Flush => Cdb::synchronize_cache(),
            BioOp::Read | BioOp::Write => {
                let len = u64::try_from(bio.len()).unwrap_or(u64::MAX);
                if !bio.offset.is_multiple_of(block) || !len.is_multiple_of(block) {
                    bio.complete(Err(BioError::Invalid));
                    return;
                }
                let blocks = u32::try_from(len / block).unwrap_or(u32::MAX);
                Cdb::rw(bio.op == BioOp::Write, bio.offset / block, blocks)
            }
        };
        let mut result = Err(BioError::Io);
        for _ in 0..TRIES {
            match self.attempt(s, lane, unit, &cdb, &mut bio) {
                Try::Done(r) => {
                    result = r;
                    break;
                }
                Try::Again(ms) => {
                    if self.t.dev().is_gone() {
                        result = Err(BioError::Gone);
                        break;
                    }
                    if ms > 0 {
                        bus::manager().sleep_ms(s, u64::from(ms));
                    }
                }
            }
        }
        bio.complete(result);
    }

    /// A worker: runs requests on lane `lane` until stopped.
    fn worker(self: Arc<Self>, s: &Sleepable, lane: u16) {
        loop {
            self.wq.wait_until(s, || {
                self.stop.load(Ordering::Acquire) || !self.queue.lock().is_empty()
            });
            if self.stop.load(Ordering::Acquire) {
                let left: Vec<(usize, Bio)> = self.queue.lock().drain(..).collect();
                for (_, b) in left {
                    b.complete(Err(BioError::Gone));
                }
                return;
            }
            let next = self.queue.lock().pop_front();
            let Some((i, bio)) = next else { continue };
            match self.units.get(i) {
                Some(u) if !self.t.dev().is_gone() => self.run(s, lane, u, bio),
                _ => bio.complete(Err(BioError::Gone)),
            }
        }
    }
}

/// One disk's view of the instance.
struct Queues {
    inner: Arc<Inner>,
    unit: usize,
}

impl DiskQueues for Queues {
    fn queues(&self) -> usize {
        1
    }

    fn submit(&self, _: &Sleepable, _: usize, bios: &mut Vec<Bio>) {
        let u = self.unit;
        self.inner
            .queue
            .lock()
            .extend(bios.drain(..).map(|b| (u, b)));
        self.inner.wq.wake_all();
    }
}

/// The `umass` driver.
pub struct Umass {
    inner: Arc<Inner>,
    unit: u32,
    luns: Vec<Probed>,
    /// Its CAM bus number, while started.
    scbus: SpinLock<Option<u32>>,
    disks: SpinLock<Vec<Arc<Disk>>>,
    threads: SpinLock<Vec<Arc<Thread>>>,
}

impl core::fmt::Debug for Umass {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Umass")
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

/// The transport for `iface`, Bulk-Only or UAS.
fn transport(s: &Sleepable, iface: &UsbInterface) -> Result<Transport, AttachError> {
    let dev = Arc::clone(&iface.device);
    if iface.desc.protocol == PROTO_UAS {
        return uas::Uas::new(s, iface)
            .map(Transport::Uas)
            .map_err(AttachError::Device);
    }
    let bulk = |is_in: bool| {
        iface
            .desc
            .endpoints
            .iter()
            .find(|e| e.is_in() == is_in && e.transfer_type() == TransferType::Bulk)
            .map(|e| e.address)
    };
    let (Some(bulk_in), Some(bulk_out)) = (bulk(true), bulk(false)) else {
        return Err(AttachError::Device("no bulk endpoints"));
    };
    Ok(Transport::Bbb(bbb::Bbb::new(
        dev,
        iface.desc.number,
        bulk_in,
        bulk_out,
    )))
}

impl Driver for Umass {
    const NAME: &'static str = "umass";
    const IDS: Ids = Ids::Usb(&[
        UsbId::interface(class::MASS_STORAGE)
            .subclass(SUBCLASS_SCSI)
            .protocol(PROTO_BBB),
        UsbId::interface(class::MASS_STORAGE)
            .subclass(SUBCLASS_SCSI)
            .protocol(PROTO_UAS),
    ]);
    const ANNOUNCE: Announce = Announce::Bare;

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Usb(_, p) => p.desc.clone(),
            _ => String::new(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let iface = cx
            .bus_data_arc::<UsbInterface>()
            .ok_or(AttachError::Decline)?;
        let s = cx.sleepable();
        let t = transport(s, &iface)?;
        // The LUNs: asked as FreeBSD asks over Bulk-Only (some devices
        // expect Get Max LUN before any command); REPORT LUNS over UAS.
        let luns: Vec<u16> = match &t {
            Transport::Bbb(b) => (0..=u16::from(b.max_lun(s))).collect(),
            Transport::Uas(_) => {
                let mut l: Vec<u16> = t
                    .read(s, 0, &Cdb::report_luns(REPORT_LUNS_LEN), 512)
                    .map(|b| scsi::report_luns(&b).collect())
                    .unwrap_or_default();
                l.sort_unstable();
                l.dedup();
                if l.first() != Some(&0) {
                    l.insert(0, 0);
                }
                l
            }
        };
        let mut found = Vec::new();
        for lun in luns {
            match probe(s, &t, lun) {
                Ok(p) => found.push(p),
                Err(why) if lun == 0 => return Err(AttachError::Device(why)),
                Err(_) => {}
            }
        }
        let units = found
            .iter()
            .map(|p| Unit {
                lun: p.lun,
                block: p.capacity.block_size,
            })
            .collect();
        Ok(Self {
            inner: Arc::new(Inner {
                t,
                units,
                queue: SpinLock::new(VecDeque::new()),
                wq: WaitQueue::new(),
                stop: AtomicBool::new(false),
            }),
            unit: cx.unit(),
            luns: found,
            scbus: SpinLock::new(None),
            disks: SpinLock::new(Vec::new()),
            threads: SpinLock::new(Vec::new()),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        for lane in 1..=self.inner.t.lanes() {
            let inner = Arc::clone(&self.inner);
            let t = sched::Builder::new("umass")
                .priority(sched::Priority::KERNEL)
                .spawn(move |s| inner.worker(s, lane))
                .map_err(|_| AttachError::NoMemory)?;
            self.threads.lock().push(t);
        }
        let u = self.unit;
        let dev = self.inner.t.dev();
        dev.announce(Self::NAME, u);
        let mgr = bus::manager();
        mgr.log(format_args!(
            "umass{u}:  SCSI over {}; quirks = 0x0",
            self.inner.t.name()
        ));
        let scbus = storage::alloc_bus();
        *self.scbus.lock() = Some(scbus);
        // FreeBSD prints this with USB_DEBUG, which GENERIC has.
        mgr.log(format_args!(
            "umass{u}:{scbus}:{u}: Attached to scbus{scbus}"
        ));
        for (i, p) in self.luns.iter().enumerate() {
            let disk = self.publish(cx.sleepable(), scbus, i, p, dev.speed);
            self.disks.lock().push(disk);
        }
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        let disks: Vec<Arc<Disk>> = self.disks.lock().drain(..).collect();
        for d in disks {
            storage::withdraw(s, &d);
        }
        self.inner.stop.store(true, Ordering::Release);
        self.inner.wq.wake_all();
        let threads: Vec<Arc<Thread>> = self.threads.lock().drain(..).collect();
        for t in threads {
            t.join(s);
        }
        if let Some(b) = self.scbus.lock().take() {
            storage::free_bus(b);
        }
    }
}

impl Umass {
    /// Publishes LUN `p` (unit `i`) as `daN`, with CAM's lines.
    fn publish(&self, s: &Sleepable, scbus: u32, i: usize, p: &Probed, speed: Speed) -> Arc<Disk> {
        let mgr = bus::manager();
        let (u, inq) = (self.unit, &p.inquiry);
        let unit = storage::alloc_unit("da");
        mgr.log(format_args!(
            "da{unit} at umass-sim{u} bus 0 scbus{scbus} target 0 lun {}",
            p.lun
        ));
        let spc = match inq.version {
            3 => String::from("SPC"),
            v @ 4..=7 => alloc::format!("SPC-{}", v - 2),
            v => alloc::format!("SCSI-{v}"),
        };
        mgr.log(format_args!(
            "da{unit}: <{} {} {}> {} Direct Access {spc} SCSI device",
            scsi::ident(&inq.vendor),
            scsi::ident(&inq.product),
            scsi::ident(&inq.revision),
            if inq.removable { "Removable" } else { "Fixed" }
        ));
        if let Some(sn) = &p.serial {
            // CAM prints at most 60 characters.
            let sn: String = sn.chars().take(60).collect();
            mgr.log(format_args!("da{unit}: Serial Number {sn}"));
        }
        // The SIM's base transfer speed, in KB/s.
        let kbps: u32 = match speed {
            Speed::Super | Speed::SuperPlus => 400_000,
            Speed::High => 40_000,
            Speed::Low | Speed::Full => 1_000,
        };
        mgr.log(format_args!(
            "da{unit}: {}.{:03}MB/s transfers",
            kbps / 1000,
            kbps % 1000
        ));
        let info = DiskInfo {
            name: "da",
            unit,
            sector_size: p.capacity.block_size,
            sectors: p.capacity.blocks,
            max_transfer: MAX_TRANSFER,
            read_only: p.read_only,
        };
        let disk = storage::publish(
            s,
            info,
            Arc::new(Queues {
                inner: Arc::clone(&self.inner),
                unit: i,
            }),
        );
        // umass's SIM says PIM_NO_6_BYTE, which da keeps as a quirk.
        mgr.log(format_args!("da{unit}: quirks=0x2<NO_6_BYTE>"));
        disk
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Umass>();
