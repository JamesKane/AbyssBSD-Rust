// SPDX-License-Identifier: BSD-2-Clause
//! USB mass storage (`umass`) over the Bulk-Only Transport, publishing
//! each disk as `daN` to the storage class, without CAM (USB.md §8).
//!
//! - Attach asks for the highest LUN, then probes LUN 0: INQUIRY, TEST
//!   UNIT READY until the medium is ready, READ CAPACITY (10, then 16 past
//!   2 TiB), and MODE SENSE for write protection.
//! - A thread runs the requests one at a time, as the transport allows:
//!   CBW, data, CSW, with REQUEST SENSE after a failure and one retry
//!   after a `UNIT ATTENTION`. A stall, a bad CSW, or a phase error gets
//!   the bulk-only reset recovery (BBB 1.0 §5.3.4).
//! - Unplugged, what is queued fails with `ENXIO`.
//!
//! Behavior reference: sys/dev/usb/storage/umass.c, sys/cam/scsi/scsi_da.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use bus::{
    Announce, AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use devtree::ids::UsbId;
use sched::{Sleepable, Thread, WaitQueue};
use scsi::bbb::{self, Csw, Status};
use scsi::{Capacity, Cdb, Inquiry, Sense, key, vpd};
use storage::{Bio, BioError, BioOp, Disk, DiskInfo, DiskQueues};
use sync::SpinLock;
use usb::descriptor::{TransferType, class};
use usb::request::rt;
use usb::{Setup, Speed};
use usbus::transfer::Waiter;
use usbus::{UsbDevice, UsbError, UsbInterface};
use vm::object::IoBuf;

/// SCSI subclass, and the Bulk-Only protocol.
const SUBCLASS_SCSI: u8 = 0x06;
const PROTO_BBB: u8 = 0x50;
/// Timeouts: the wrappers, and a data phase.
const WRAPPER_MS: u32 = 5000;
const DATA_MS: u32 = 20_000;
/// The largest transfer per command (FreeBSD's `umass` maximum I/O).
const MAX_TRANSFER: usize = 64 * 1024;
/// TEST UNIT READY attempts while the medium comes ready.
const READY_TRIES: u32 = 20;

/// Why a command failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CmdError {
    /// The device reported a failure: sense data says why.
    Check,
    /// The transport broke (recovered by reset).
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

/// What the instance and its thread share.
struct Inner {
    dev: Arc<UsbDevice>,
    iface: u8,
    bulk_in: u8,
    bulk_out: u8,
    lun: u8,
    tag: AtomicU32,
    block: u32,
    queue: SpinLock<VecDeque<Bio>>,
    wq: WaitQueue,
    stop: AtomicBool,
}

impl Inner {
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
            return (IoBuf::Owned(Vec::new()), Err(r.error));
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

    /// Runs `cdb` with its data phase; returns the data buffer and the
    /// residue.
    fn command(&self, s: &Sleepable, cdb: &Cdb, data: Data) -> (IoBuf, Result<u32, CmdError>) {
        let (len, is_in, buf) = match data {
            Data::None => (0, false, IoBuf::Owned(Vec::new())),
            Data::In(b) => (b.len(), true, b),
            Data::Out(b) => (b.len(), false, b),
        };
        let gone = |b| (b, Err(CmdError::Gone));
        if self.dev.is_gone() {
            return gone(buf);
        }
        let tag = self.tag.fetch_add(1, Ordering::Relaxed);
        let n = u32::try_from(len).unwrap_or(u32::MAX);
        let cbw = bbb::cbw(tag, n, is_in, self.lun, cdb);
        let (_, r) = self.transfer(s, self.bulk_out, IoBuf::from(cbw.to_vec()), WRAPPER_MS);
        match r {
            Ok(_) => {}
            Err(UsbError::Gone) => return gone(buf),
            Err(_) => {
                self.reset_recovery(s);
                return (buf, Err(CmdError::Transport));
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
                    return (buf, Err(CmdError::Transport));
                }
            }
        }
        let mut csw = None;
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
            Some(c) if c.tag == tag && c.status == Status::Failed => (buf, Err(CmdError::Check)),
            _ => {
                self.reset_recovery(s);
                (buf, Err(CmdError::Transport))
            }
        }
    }

    /// REQUEST SENSE after a failed command.
    fn sense(&self, s: &Sleepable) -> Option<Sense> {
        let (b, r) = self.command(
            s,
            &Cdb::request_sense(18),
            Data::In(IoBuf::from(vec![0; 18])),
        );
        r.ok()?;
        match b {
            IoBuf::Owned(v) => Sense::parse(&v),
            _ => None,
        }
    }

    /// Runs `cdb` reading `len` bytes, with one retry after a unit
    /// attention.
    fn read_cmd(&self, s: &Sleepable, cdb: &Cdb, len: usize) -> Result<Vec<u8>, CmdError> {
        for _ in 0..2 {
            let (b, r) = self.command(s, cdb, Data::In(IoBuf::from(vec![0; len])));
            match r {
                Ok(residue) => {
                    let IoBuf::Owned(mut v) = b else {
                        return Err(CmdError::Transport);
                    };
                    v.truncate(len.saturating_sub(usize::try_from(residue).unwrap_or(0)));
                    return Ok(v);
                }
                Err(CmdError::Check) => {
                    if self.sense(s).is_some_and(|x| x.key == key::UNIT_ATTENTION) {
                        continue;
                    }
                    return Err(CmdError::Check);
                }
                Err(e) => return Err(e),
            }
        }
        Err(CmdError::Check)
    }

    /// Runs one block request.
    fn run(&self, s: &Sleepable, mut bio: Bio) {
        let block = u64::from(self.block.max(1));
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
        for _ in 0..2 {
            let buf = core::mem::take(&mut bio.buf);
            let data = match bio.op {
                BioOp::Flush => Data::None,
                BioOp::Read => Data::In(buf),
                BioOp::Write => Data::Out(buf),
            };
            let (b, r) = self.command(s, &cdb, data);
            if bio.op != BioOp::Flush {
                bio.buf = b;
            }
            result = match r {
                Ok(0) => Ok(()),
                Ok(_) => Err(BioError::Io),
                Err(CmdError::Gone) => Err(BioError::Gone),
                Err(CmdError::Transport) => Err(BioError::Io),
                Err(CmdError::Check) => match self.sense(s) {
                    Some(x) if x.key == key::UNIT_ATTENTION => continue,
                    Some(x) if x.key == key::RECOVERED_ERROR => Ok(()),
                    Some(x) if x.key == key::DATA_PROTECT => Err(BioError::ReadOnly),
                    // A device without the cache command (FreeBSD's
                    // NO_SYNCHRONIZE_CACHE quirk) has nothing to flush.
                    Some(x) if x.key == key::ILLEGAL_REQUEST && bio.op == BioOp::Flush => Ok(()),
                    _ => Err(BioError::Io),
                },
            };
            break;
        }
        bio.complete(result);
    }

    fn thread(self: Arc<Self>, s: &Sleepable) {
        loop {
            self.wq.wait_until(s, || {
                self.stop.load(Ordering::Acquire) || !self.queue.lock().is_empty()
            });
            if self.stop.load(Ordering::Acquire) {
                let left: Vec<Bio> = self.queue.lock().drain(..).collect();
                for b in left {
                    b.complete(Err(BioError::Gone));
                }
                return;
            }
            let next = self.queue.lock().pop_front();
            if let Some(bio) = next {
                if self.dev.is_gone() {
                    bio.complete(Err(BioError::Gone));
                } else {
                    self.run(s, bio);
                }
            }
        }
    }
}

/// The disk's view of the instance.
struct Queues(Arc<Inner>);

impl DiskQueues for Queues {
    fn queues(&self) -> usize {
        1
    }

    fn submit(&self, _: &Sleepable, _: usize, bios: &mut Vec<Bio>) {
        self.0.queue.lock().extend(bios.drain(..));
        self.0.wq.wake_all();
    }
}

/// The `umass` driver.
pub struct Umass {
    inner: Arc<Inner>,
    unit: u32,
    inquiry: Inquiry,
    /// The unit serial number (VPD page 0x80), if it has one.
    serial: Option<String>,
    capacity: Capacity,
    read_only: bool,
    /// Its CAM bus number, while started.
    scbus: SpinLock<Option<u32>>,
    disk: SpinLock<Option<Arc<Disk>>>,
    thread: SpinLock<Option<Arc<Thread>>>,
}

impl core::fmt::Debug for Umass {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Umass")
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

impl Driver for Umass {
    const NAME: &'static str = "umass";
    const IDS: Ids = Ids::Usb(&[UsbId::interface(class::MASS_STORAGE)
        .subclass(SUBCLASS_SCSI)
        .protocol(PROTO_BBB)]);
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
        let dev = Arc::clone(&iface.device);
        // Asked as FreeBSD asks (some devices expect it before any
        // command); only LUN 0 is published so far.
        let _max_lun = dev
            .control(
                s,
                Setup {
                    request_type: rt::IN | rt::CLASS | rt::INTERFACE,
                    request: bbb::GET_MAX_LUN,
                    value: 0,
                    index: u16::from(iface.desc.number),
                    length: 1,
                },
                None,
            )
            .ok()
            .and_then(|v| v.first().copied())
            .unwrap_or(0);
        let inner = Inner {
            dev,
            iface: iface.desc.number,
            bulk_in,
            bulk_out,
            lun: 0,
            tag: AtomicU32::new(1),
            block: 512,
            queue: SpinLock::new(VecDeque::new()),
            wq: WaitQueue::new(),
            stop: AtomicBool::new(false),
        };
        let dead = |_| AttachError::Device("INQUIRY failed");
        let inquiry = inner
            .read_cmd(s, &Cdb::inquiry(36), 36)
            .map_err(dead)
            .and_then(|b| Inquiry::parse(&b).ok_or(AttachError::Device("bad INQUIRY data")))?;
        if inquiry.absent {
            return Err(AttachError::Device("LUN 0 not present"));
        }
        // As CAM's probe: the serial number, if the supported pages
        // list it.
        let serial = inner
            .read_cmd(
                s,
                &Cdb::inquiry_vpd(vpd::SUPPORTED_PAGES, vpd::LEN),
                vpd::LEN.into(),
            )
            .ok()
            .filter(|l| vpd::supports(l, vpd::UNIT_SERIAL))
            .and_then(|_| {
                inner
                    .read_cmd(
                        s,
                        &Cdb::inquiry_vpd(vpd::UNIT_SERIAL, vpd::LEN),
                        vpd::LEN.into(),
                    )
                    .ok()
            })
            .and_then(|p| vpd::serial(&p).map(|v| String::from_utf8_lossy(v).into_owned()));
        let mut ready = false;
        for _ in 0..READY_TRIES {
            match inner.command(s, &Cdb::test_unit_ready(), Data::None).1 {
                Ok(_) => {
                    ready = true;
                    break;
                }
                Err(CmdError::Gone) => return Err(AttachError::Device("device gone")),
                _ => {
                    let sense = inner.sense(s);
                    if sense.is_some_and(|x| x.no_medium()) {
                        break;
                    }
                    bus::manager().sleep_ms(s, 100);
                }
            }
        }
        if !ready {
            return Err(AttachError::Device("medium not ready"));
        }
        let cap = match inner.read_cmd(s, &Cdb::read_capacity_10(), 8) {
            Ok(b) => match Capacity::parse_10(&b) {
                Some(c) => Some(c),
                None => inner
                    .read_cmd(s, &Cdb::read_capacity_16(), 32)
                    .ok()
                    .and_then(|b| Capacity::parse_16(&b)),
            },
            Err(_) => None,
        };
        let capacity = cap
            .filter(|c| c.block_size.is_power_of_two() && (512..=65536).contains(&c.block_size))
            .ok_or(AttachError::Device("READ CAPACITY failed"))?;
        // MODE SENSE's header: the WP bit (some devices refuse it).
        let read_only = inner
            .read_cmd(s, &Cdb::mode_sense(192), 192)
            .ok()
            .and_then(|b| b.get(2).copied())
            .is_some_and(|d| d & 0x80 != 0);
        Ok(Self {
            inner: Arc::new(Inner {
                block: capacity.block_size,
                ..inner
            }),
            unit: cx.unit(),
            inquiry,
            serial,
            capacity,
            read_only,
            scbus: SpinLock::new(None),
            disk: SpinLock::new(None),
            thread: SpinLock::new(None),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let inner = Arc::clone(&self.inner);
        let t = sched::Builder::new("umass")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| inner.thread(s))
            .map_err(|_| AttachError::NoMemory)?;
        *self.thread.lock() = Some(t);
        let (u, i) = (self.unit, &self.inquiry);
        self.inner.dev.announce(Self::NAME, u);
        let mgr = bus::manager();
        mgr.log(format_args!("umass{u}:  SCSI over Bulk-Only; quirks = 0x0"));
        let scbus = storage::alloc_bus();
        *self.scbus.lock() = Some(scbus);
        // FreeBSD prints this with USB_DEBUG, which GENERIC has.
        mgr.log(format_args!(
            "umass{u}:{scbus}:{u}: Attached to scbus{scbus}"
        ));
        let unit = storage::alloc_unit("da");
        mgr.log(format_args!(
            "da{unit} at umass-sim{u} bus 0 scbus{scbus} target 0 lun 0"
        ));
        let spc = match i.version {
            3 => String::from("SPC"),
            v @ 4..=7 => alloc::format!("SPC-{}", v - 2),
            v => alloc::format!("SCSI-{v}"),
        };
        mgr.log(format_args!(
            "da{unit}: <{} {} {}> {} Direct Access {spc} SCSI device",
            scsi::ident(&i.vendor),
            scsi::ident(&i.product),
            scsi::ident(&i.revision),
            if i.removable { "Removable" } else { "Fixed" }
        ));
        if let Some(sn) = &self.serial {
            // CAM prints at most 60 characters.
            let sn: String = sn.chars().take(60).collect();
            mgr.log(format_args!("da{unit}: Serial Number {sn}"));
        }
        // The SIM's base transfer speed, in KB/s.
        let kbps: u32 = match self.inner.dev.speed {
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
            sector_size: self.capacity.block_size,
            sectors: self.capacity.blocks,
            max_transfer: MAX_TRANSFER,
            read_only: self.read_only,
        };
        let disk = storage::publish(
            cx.sleepable(),
            info,
            Arc::new(Queues(Arc::clone(&self.inner))),
        );
        *self.disk.lock() = Some(disk);
        // umass's SIM says PIM_NO_6_BYTE, which da keeps as a quirk.
        mgr.log(format_args!("da{unit}: quirks=0x2<NO_6_BYTE>"));
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        let disk = self.disk.lock().take();
        if let Some(d) = disk {
            storage::withdraw(s, &d);
        }
        self.inner.stop.store(true, Ordering::Release);
        self.inner.wq.wake_all();
        let t = self.thread.lock().take();
        if let Some(t) = t {
            t.join(s);
        }
        if let Some(b) = self.scbus.lock().take() {
            storage::free_bus(b);
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Umass>();
