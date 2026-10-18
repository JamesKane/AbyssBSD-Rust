// SPDX-License-Identifier: BSD-2-Clause
//! NVM Express controllers (`nvme`), each namespace published as `ndaN`
//! (FreeBSD 15's default, `hw.nvme.use_nvd=0`).
//!
//! - Attach resets the controller, brings up the admin queue, and runs
//!   admin commands synchronously, polling the admin completion queue:
//!   identify, the number of I/O queues, and one completion and
//!   submission queue pair per CPU (as many as the controller and the
//!   MSI-X vectors allow; one pair on INTx).
//! - Requests become one command each: data described by PRPs (the tag
//!   keeps every segment within one page and page-aligned after the first,
//!   bouncing buffers that are not), a per-command PRP list for longer
//!   transfers.
//! - Completions are reaped per queue in its vector's interrupt thread,
//!   tracking the phase tag; then the class resubmits what was waiting.
//! - Stop withdraws the namespaces, shuts the controller down normally,
//!   disables it, and fails what it still held.
//!
//! Everything the controller writes is checked: a completion's command ID
//! must name an outstanding command.
//!
//! Behavior reference: sys/dev/nvme/ (nvme_ctrlr.c, nvme_qpair.c,
//! nvme_pci.c); the NVM Express Base Specification 2.0 for the registers.

#![no_std]
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "command fields pack narrower values; queue and vector counts are small"
)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, StartCx, StopCx,
    StopReason,
};
use devtree::ids::PciId;
use irq::Filter;
use pci::bus::PciChild;
use regs::Bytes;
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable};
use storage::{Bio, BioError, BioOp, Disk, DiskInfo, DiskQueues};
use sync::SpinLock;
use vm::dma::{DeviceOwned, DmaCoherent, DmaConstraints, DmaStream, DmaTag, dir};

/// Controller registers (NVMe 2.0 §3.1).
mod reg {
    pub(crate) const CAP: usize = 0x00;
    pub(crate) const VS: usize = 0x08;
    pub(crate) const INTMS: usize = 0x0c;
    pub(crate) const INTMC: usize = 0x10;
    pub(crate) const CC: usize = 0x14;
    pub(crate) const CSTS: usize = 0x1c;
    pub(crate) const AQA: usize = 0x24;
    pub(crate) const ASQ: usize = 0x28;
    pub(crate) const ACQ: usize = 0x30;
    pub(crate) const DOORBELLS: usize = 0x1000;
}

const CC_EN: u32 = 1;
/// I/O submission and completion entry sizes (2^6, 2^4), 4 KiB pages.
const CC_ENTRIES: u32 = (6 << 16) | (4 << 20);
const CC_SHN_NORMAL: u32 = 1 << 14;
const CSTS_RDY: u32 = 1;
const CSTS_CFS: u32 = 1 << 1;
const CSTS_SHST_DONE: u32 = 2 << 2;

/// Admin opcodes.
const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const ADMIN_SET_FEATURES: u8 = 0x09;
/// I/O opcodes.
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;
/// Identify CNS values.
const CNS_NAMESPACE: u32 = 0;
const CNS_CONTROLLER: u32 = 1;
const CNS_ACTIVE_NAMESPACES: u32 = 2;
/// Set Features: number of queues.
const FEATURE_QUEUES: u32 = 0x07;

const PAGE: usize = 4096;
const SQE: usize = 64;
const CQE: usize = 16;
const ADMIN_ENTRIES: u16 = 32;
/// I/O queue entries wanted (capped by CAP.MQES).
const IO_ENTRIES: u16 = 256;
/// The largest transfer per request (capped by MDTS).
const MAX_TRANSFER: usize = 128 * 1024;
/// PRP list bytes per command: enough entries for the largest transfer.
const PRP_LIST: usize = (MAX_TRANSFER / PAGE + 1) * 8;
/// Admin commands time out after this long (polled every millisecond).
const ADMIN_TIMEOUT_MS: u32 = 5000;

static QUEUE_LOCK: LockClass = LockClass::new("nvme queue", 500);
static ADMIN_LOCK: LockClass = LockClass::new("nvme admin", 490);

/// Known controllers' descriptions, as FreeBSD's table has them (vendor,
/// device, description).
const KNOWN: &[(u16, u16, &str)] = &[
    (0x8086, 0x0111, "NVMe Controller"),
    (0x1c58, 0x0003, "HGST SN100"),
    (0x1c58, 0x0023, "WDC SN200"),
    (0x144d, 0xa821, "Samsung PM1725"),
    (0x144d, 0xa822, "Samsung PM1725a"),
    (0x15ad, 0x07f0, "VMware NVMe Controller"),
    (0x1d0f, 0x8061, "Amazon EBS NVMe Controller"),
];

/// A 64-byte submission queue entry.
#[derive(Clone, Copy, Debug, Default)]
struct Command {
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

/// A completion's status: 0 is success.
type Status = u16;

/// A ring pair's memory and positions.
struct Rings {
    sq: DmaCoherent<u8>,
    cq: DmaCoherent<u8>,
    entries: u16,
    tail: u16,
    head: u16,
    phase: bool,
}

impl Rings {
    fn new(tag: &DmaTag, entries: u16) -> Result<Self, AttachError> {
        let n = usize::from(entries);
        Ok(Self {
            sq: tag
                .coherent::<u8>(n * SQE)
                .map_err(|_| AttachError::NoMemory)?,
            cq: tag
                .coherent::<u8>(n * CQE)
                .map_err(|_| AttachError::NoMemory)?,
            entries,
            tail: 0,
            head: 0,
            phase: true,
        })
    }

    /// Writes `c` with command ID `cid` at the tail and advances it.
    fn push(&mut self, c: &Command, cid: u16) {
        let at = usize::from(self.tail) * SQE;
        let m = &self.sq;
        m.write_at::<u32>(at, u32::from(c.opcode) | u32::from(cid) << 16);
        m.write_at::<u32>(at + 4, c.nsid);
        for off in (8..24).step_by(8) {
            m.write_at::<u64>(at + off, 0);
        }
        m.write_at::<u64>(at + 24, c.prp1);
        m.write_at::<u64>(at + 32, c.prp2);
        m.write_at::<u32>(at + 40, c.cdw10);
        m.write_at::<u32>(at + 44, c.cdw11);
        m.write_at::<u32>(at + 48, c.cdw12);
        for off in (52..64).step_by(4) {
            m.write_at::<u32>(at + off, 0);
        }
        self.tail = (self.tail + 1) % self.entries;
    }

    /// The next completion, if the controller has posted one: (command ID,
    /// status, dword 0).
    fn pop(&mut self) -> Option<(u16, Status, u32)> {
        let at = usize::from(self.head) * CQE;
        let dw3 = self.cq.read_at::<u32>(at + 12);
        if (dw3 >> 16 & 1 != 0) != self.phase {
            return None;
        }
        // The phase bit before the rest of the entry.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let dw0 = self.cq.read_at::<u32>(at);
        let cid = (dw3 & 0xffff) as u16;
        let status = (dw3 >> 17) as u16 & 0x7fff;
        self.head += 1;
        if self.head == self.entries {
            self.head = 0;
            self.phase = !self.phase;
        }
        Some((cid, status, dw0))
    }
}

/// The controller's registers and doorbells.
struct Regs {
    mmio: MmioGrant<Bytes>,
    /// Doorbell stride in bytes.
    stride: usize,
}

impl Regs {
    fn read64(&self, off: usize) -> u64 {
        u64::from(self.mmio.read32(off)) | u64::from(self.mmio.read32(off + 4)) << 32
    }

    fn write64(&self, off: usize, v: u64) {
        self.mmio.write32(off, v as u32);
        self.mmio.write32(off + 4, (v >> 32) as u32);
    }

    fn sq_doorbell(&self, q: u16, tail: u16) {
        self.mmio.write32(
            reg::DOORBELLS + 2 * usize::from(q) * self.stride,
            u32::from(tail),
        );
    }

    fn cq_doorbell(&self, q: u16, head: u16) {
        self.mmio.write32(
            reg::DOORBELLS + (2 * usize::from(q) + 1) * self.stride,
            u32::from(head),
        );
    }
}

/// A request's data, mapped for the controller.
enum Data {
    None,
    Read(DmaStream<Vec<u8>, DeviceOwned<dir::FromDevice>>),
    Write(DmaStream<Vec<u8>, DeviceOwned<dir::ToDevice>>),
}

struct InFlight {
    bio: Bio,
    data: Data,
}

impl InFlight {
    fn finish(self) -> Bio {
        let Self { mut bio, data } = self;
        match data {
            Data::None => {}
            Data::Read(d) => bio.buf = d.unmap().into_inner(),
            Data::Write(d) => bio.buf = d.unmap().into_inner(),
        }
        bio
    }
}

/// One I/O queue pair.
struct IoQueue {
    id: u16,
    rings: Rings,
    /// Per command ID: PRP list space.
    prps: DmaCoherent<u64>,
    slots: Vec<Option<InFlight>>,
    free: Vec<u16>,
}

/// A namespace as a disk.
struct Namespace {
    nsid: u32,
    /// log2 of the block size.
    shift: u32,
    blocks: u64,
}

/// What the disks and interrupt handlers share.
pub struct Controller {
    regs: Regs,
    tag: DmaTag,
    queues: Vec<Mutex<IoQueue>>,
    /// Each queue's MSI-X vector (an index into the lines).
    vectors: Vec<usize>,
    max_transfer: usize,
    disks: SpinLock<Vec<Arc<Disk>>>,
}

impl core::fmt::Debug for Controller {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Controller")
            .field("queues", &self.queues.len())
            .finish_non_exhaustive()
    }
}

/// The disk view of one namespace.
struct NsQueues {
    ctrl: Arc<Controller>,
    nsid: u32,
    shift: u32,
}

impl Controller {
    fn map(&self, bio: &mut Bio) -> Result<Data, ()> {
        let stream = DmaStream::new(&self.tag, core::mem::take(&mut bio.buf));
        match bio.op {
            BioOp::Flush => {
                bio.buf = stream.into_inner();
                Ok(Data::None)
            }
            BioOp::Read => stream
                .map::<dir::FromDevice>()
                .map(Data::Read)
                .map_err(|(s, _)| bio.buf = s.into_inner()),
            BioOp::Write => stream
                .map::<dir::ToDevice>()
                .map(Data::Write)
                .map_err(|(s, _)| bio.buf = s.into_inner()),
        }
    }

    /// Reaps queue `q`'s completions.
    fn reap(&self, s: &Sleepable, q: usize) -> Vec<(Bio, Result<(), BioError>)> {
        let mut out = Vec::new();
        let mut io = self.queues[q].lock(s);
        let mut any = false;
        while let Some((cid, status, _)) = io.rings.pop() {
            any = true;
            let Some(f) = io.slots.get_mut(usize::from(cid)).and_then(Option::take) else {
                continue;
            };
            io.free.push(cid);
            out.push((
                f.finish(),
                if status == 0 {
                    Ok(())
                } else {
                    Err(BioError::Io)
                },
            ));
        }
        if any {
            self.regs.cq_doorbell(io.id, io.rings.head);
        }
        out
    }
}

impl DiskQueues for NsQueues {
    fn queues(&self) -> usize {
        self.ctrl.queues.len()
    }

    fn submit(&self, s: &Sleepable, q: usize, bios: &mut Vec<Bio>) {
        let ctrl = &self.ctrl;
        let mut io = ctrl.queues[q % ctrl.queues.len()].lock(s);
        let mut started = false;
        let mut refused = Vec::new();
        for mut bio in core::mem::take(bios) {
            if !refused.is_empty() {
                refused.push(bio);
                continue;
            }
            let Some(&cid) = io.free.last() else {
                refused.push(bio);
                continue;
            };
            // Before `map` takes the buffer.
            let blocks = (bio.len() >> self.shift) as u32;
            let Ok(data) = ctrl.map(&mut bio) else {
                refused.push(bio);
                continue;
            };
            let segs: &[vm::dma::Segment] = match &data {
                Data::None => &[],
                Data::Read(d) => d.segments(),
                Data::Write(d) => d.segments(),
            };
            // PRP rules: segments after the first start page-aligned, and
            // all but the last end at a page boundary.
            let prp_ok = segs.iter().enumerate().all(|(i, g)| {
                (i == 0 || g.bus % PAGE as u64 == 0)
                    && (i + 1 == segs.len() || (g.bus + g.len) % PAGE as u64 == 0)
            });
            if !prp_ok || segs.len() > PRP_LIST / 8 {
                InFlight { bio, data }
                    .finish()
                    .complete(Err(BioError::Invalid));
                continue;
            }
            let (prp1, prp2) = match segs {
                [] => (0, 0),
                [a] => (a.bus, 0),
                [a, b] => (a.bus, b.bus),
                [a, rest @ ..] => {
                    let base = usize::from(cid) * (PRP_LIST / 8);
                    for (i, g) in rest.iter().enumerate() {
                        io.prps.write(base + i, g.bus);
                    }
                    (a.bus, io.prps.bus_addr() + (base * 8) as u64)
                }
            };
            let lba = bio.offset >> self.shift;
            let opcode = match bio.op {
                BioOp::Read => IO_READ,
                BioOp::Write => IO_WRITE,
                BioOp::Flush => IO_FLUSH,
            };
            let c = Command {
                opcode,
                nsid: self.nsid,
                prp1,
                prp2,
                cdw10: lba as u32,
                cdw11: (lba >> 32) as u32,
                cdw12: blocks.saturating_sub(1),
            };
            io.free.pop();
            io.rings.push(&c, cid);
            io.slots[usize::from(cid)] = Some(InFlight { bio, data });
            started = true;
        }
        if started {
            ctrl.regs.sq_doorbell(io.id, io.rings.tail);
        }
        *bios = refused;
    }
}

/// The admin queue, used during attach, start, and stop.
struct Admin {
    rings: Rings,
    next_cid: u16,
}

/// Sleeps `ms` milliseconds.
fn sleep_ms(s: &Sleepable, ms: u64) {
    bus::manager().sleep_ms(s, ms);
}

impl Admin {
    /// Runs `c` and waits for its completion: dword 0, or the status.
    fn run(&mut self, regs: &Regs, s: &Sleepable, c: &Command) -> Result<u32, Status> {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        self.rings.push(c, cid);
        regs.sq_doorbell(0, self.rings.tail);
        for _ in 0..ADMIN_TIMEOUT_MS {
            if let Some((done, status, dw0)) = self.rings.pop() {
                regs.cq_doorbell(0, self.rings.head);
                if done != cid {
                    continue;
                }
                return if status == 0 { Ok(dw0) } else { Err(status) };
            }
            sleep_ms(s, 1);
        }
        Err(0xffff)
    }
}

/// Waits until `CSTS.RDY` is `ready`, for at most CAP.TO.
fn wait_ready(regs: &Regs, s: &Sleepable, ready: bool, timeout_ms: u64) -> Result<(), AttachError> {
    for _ in 0..timeout_ms.max(1) {
        let csts = regs.mmio.read32(reg::CSTS);
        if csts == u32::MAX || csts & CSTS_CFS != 0 {
            return Err(AttachError::Device("controller fatal status"));
        }
        if (csts & CSTS_RDY != 0) == ready {
            return Ok(());
        }
        sleep_ms(s, 1);
    }
    Err(AttachError::Device("controller ready timeout"))
}

/// An ASCII identify field, without its padding (spaces, or NULs).
fn ascii(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .into()
}

/// An `nvmeN` instance.
pub struct Nvme {
    ctrl: Arc<Controller>,
    /// Kept for its rings while the controller runs.
    _admin: Mutex<Admin>,
    unit: u32,
    namespaces: Vec<Namespace>,
    lines: SpinLock<Vec<IrqLine>>,
    /// MSI-X vector count handed out (0: INTx).
    msix: usize,
    identity: String,
}

impl core::fmt::Debug for Nvme {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Nvme")
            .field("ctrl", &self.ctrl)
            .finish_non_exhaustive()
    }
}

fn filter(_: &Nvme, _: usize, _: &IrqCx) -> Filter {
    Filter::Schedule
}

fn completions(d: &Nvme, vector: usize, s: &Sleepable) {
    let ctrl = &d.ctrl;
    for q in 0..ctrl.queues.len() {
        if ctrl.vectors[q] != vector {
            continue;
        }
        for (bio, r) in ctrl.reap(s, q) {
            bio.complete(r);
        }
        let disks = ctrl.disks.lock().clone();
        for disk in disks {
            disk.resume(s, q);
        }
    }
}

impl Driver for Nvme {
    const NAME: &'static str = "nvme";
    // FreeBSD probes a generic NVMe controller at BUS_PROBE_GENERIC.
    const IDS: Ids = Ids::Pci(&[PciId::class(0x01_08_02)]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(ident: &Ident) -> String {
        if let Ident::Pci(i, _, _) = ident
            && let Some((_, _, d)) = KNOWN
                .iter()
                .find(|(v, dev, _)| *v == i.vendor && *dev == i.device)
        {
            return (*d).into();
        }
        "Generic NVMe Device".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let s = cx.sleepable();
        let mmio = cx.mmio_bytes(0)?;
        child.enable_decoding();
        child.set_bus_master(true);
        let mut regs = Regs { mmio, stride: 4 };
        let cap = regs.read64(reg::CAP);
        if cap == u64::MAX {
            return Err(AttachError::Device("registers read all ones"));
        }
        regs.stride = 4 << ((cap >> 32) & 0xf);
        let mqes = (cap & 0xffff) as u16;
        let timeout_ms = ((cap >> 24) & 0xff).max(1) * 500;
        if (cap >> 37) & 1 == 0 {
            return Err(AttachError::Device("no NVM command set"));
        }
        if (cap >> 48) & 0xf != 0 {
            return Err(AttachError::Device("4 KiB pages not supported"));
        }
        let version = regs.mmio.read32(reg::VS);

        // Reset, then the admin queue.
        regs.mmio.write32(reg::CC, 0);
        wait_ready(&regs, s, false, timeout_ms)?;
        // Queues and PRP lists: contiguous memory. Data: PRP-shaped
        // segments, each within one page, dword-aligned.
        let rings_tag = cx.dma_tag(DmaConstraints::UNLIMITED)?;
        let tag = cx.dma_tag(DmaConstraints {
            addr_bits: 64,
            align: 4,
            boundary: Some(PAGE as u64),
            max_segment: PAGE as u64,
            ..DmaConstraints::UNLIMITED
        })?;
        let admin_entries = ADMIN_ENTRIES.min(mqes.saturating_add(1));
        let rings = Rings::new(&rings_tag, admin_entries)?;
        let n = u32::from(admin_entries) - 1;
        regs.mmio.write32(reg::AQA, n << 16 | n);
        regs.write64(reg::ASQ, rings.sq.bus_addr());
        regs.write64(reg::ACQ, rings.cq.bus_addr());
        // Interrupts stay masked while admin commands are polled.
        regs.mmio.write32(reg::INTMS, u32::MAX);
        regs.mmio.write32(reg::CC, CC_EN | CC_ENTRIES);
        wait_ready(&regs, s, true, timeout_ms)?;
        let mut admin = Admin { rings, next_cid: 0 };
        let fail = |regs: &Regs| regs.mmio.write32(reg::CC, 0);

        let identify = tag
            .coherent::<u8>(PAGE)
            .map_err(|_| AttachError::NoMemory)?;
        let id_cmd = |cns: u32, nsid: u32| Command {
            opcode: ADMIN_IDENTIFY,
            nsid,
            prp1: identify.bus_addr(),
            cdw10: cns,
            ..Command::default()
        };
        if admin.run(&regs, s, &id_cmd(CNS_CONTROLLER, 0)).is_err() {
            fail(&regs);
            return Err(AttachError::Device("identify controller failed"));
        }
        let bytes = |a: usize, b: usize| (a..b).map(|i| identify.read(i)).collect::<Vec<u8>>();
        let model = ascii(&bytes(24, 64));
        let serial = ascii(&bytes(4, 24));
        let firmware = ascii(&bytes(64, 72));
        let mdts = identify.read(77);
        let max_transfer = if mdts == 0 {
            MAX_TRANSFER
        } else {
            MAX_TRANSFER.min(PAGE << mdts)
        };

        // I/O queues: one pair per CPU, within what the controller grants
        // and the MSI-X vectors allow (vector 0 is shared with admin).
        let cpus = sched::online_cpus().max(1);
        let want = u16::try_from(cpus).unwrap_or(u16::MAX);
        let granted = admin
            .run(
                &regs,
                s,
                &Command {
                    opcode: ADMIN_SET_FEATURES,
                    cdw10: FEATURE_QUEUES,
                    cdw11: u32::from(want - 1) << 16 | u32::from(want - 1),
                    ..Command::default()
                },
            )
            .map_or(1, |dw0| ((dw0 & 0xffff).min(dw0 >> 16) + 1) as usize);
        let mut lines = pci::bus::msix_lines(cx, granted.min(cpus) as u16).unwrap_or_default();
        let msix = lines.len();
        if lines.is_empty() {
            lines.push(cx.irq(0)?);
        }
        let nqueues = granted.min(cpus).min(lines.len()).max(1);
        let entries = IO_ENTRIES.min(mqes.saturating_add(1));
        let mut queues = Vec::new();
        let mut vectors = Vec::new();
        for i in 0..nqueues {
            let qid = (i + 1) as u16;
            let vector = if msix > 0 { i % msix } else { 0 };
            let rings = Rings::new(&rings_tag, entries)?;
            let size = u32::from(entries) - 1;
            let cq = Command {
                opcode: ADMIN_CREATE_CQ,
                prp1: rings.cq.bus_addr(),
                cdw10: size << 16 | u32::from(qid),
                // Interrupts enabled, physically contiguous.
                cdw11: (vector as u32) << 16 | 0b11,
                ..Command::default()
            };
            let sq = Command {
                opcode: ADMIN_CREATE_SQ,
                prp1: rings.sq.bus_addr(),
                cdw10: size << 16 | u32::from(qid),
                cdw11: u32::from(qid) << 16 | 1,
                ..Command::default()
            };
            if admin.run(&regs, s, &cq).is_err() || admin.run(&regs, s, &sq).is_err() {
                fail(&regs);
                return Err(AttachError::Device("I/O queue creation failed"));
            }
            let prps = rings_tag
                .coherent::<u64>(usize::from(entries) * (PRP_LIST / 8))
                .map_err(|_| AttachError::NoMemory)?;
            queues.push(Mutex::new(
                &QUEUE_LOCK,
                IoQueue {
                    id: qid,
                    rings,
                    prps,
                    slots: (0..entries).map(|_| None).collect(),
                    // One command ID short of full keeps the ring from
                    // overflowing.
                    free: (0..entries - 1).rev().collect(),
                },
            ));
            vectors.push(vector);
        }

        // Namespaces: the active list, else 1..=NN.
        let nn = u32::from_le_bytes([
            identify.read(516),
            identify.read(517),
            identify.read(518),
            identify.read(519),
        ]);
        let mut nsids: Vec<u32> = Vec::new();
        if admin
            .run(&regs, s, &id_cmd(CNS_ACTIVE_NAMESPACES, 0))
            .is_ok()
        {
            for i in 0..1024 {
                let id = identify.read_at::<u32>(4 * i);
                if id == 0 {
                    break;
                }
                nsids.push(id);
            }
        } else {
            nsids.extend(1..=nn.min(1024));
        }
        let mut namespaces = Vec::new();
        for nsid in nsids {
            if admin.run(&regs, s, &id_cmd(CNS_NAMESPACE, nsid)).is_err() {
                continue;
            }
            let blocks = identify.read_at::<u64>(0);
            let flbas = identify.read(26);
            let format = usize::from(flbas & 0xf | (flbas >> 5 & 0x3) << 4);
            let shift = u32::from(identify.read(128 + 4 * format + 2));
            if blocks == 0 || !(9..=16).contains(&shift) {
                continue;
            }
            namespaces.push(Namespace {
                nsid,
                shift,
                blocks,
            });
        }
        // Unmask (INTx and MSI; MSI-X ignores the masks).
        regs.mmio.write32(reg::INTMC, u32::MAX);
        Ok(Self {
            identity: alloc::format!(
                "{model} (serial {serial}, firmware {firmware}, NVMe {}.{})",
                version >> 16,
                version >> 8 & 0xff
            ),
            ctrl: Arc::new(Controller {
                regs,
                tag,
                queues,
                vectors,
                max_transfer,
                disks: SpinLock::new(Vec::new()),
            }),
            _admin: Mutex::new(&ADMIN_LOCK, admin),
            unit: cx.unit(),
            namespaces,
            lines: SpinLock::new(lines),
            msix,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let lines = core::mem::take(&mut *self.lines.lock());
        for (v, line) in lines.into_iter().enumerate() {
            cx.interrupt(line, Some(filter), Some(completions), v)?;
        }
        bus::manager().log(format_args!(
            "nvme{}: {}, {} I/O queue(s) on {}",
            self.unit,
            self.identity,
            self.ctrl.queues.len(),
            if self.msix > 0 { "MSI-X" } else { "INTx" }
        ));
        let mut disks = Vec::new();
        for ns in &self.namespaces {
            let block = 1u32 << ns.shift;
            let info = DiskInfo {
                name: "nda",
                unit: storage::alloc_unit("nda"),
                sector_size: block,
                sectors: ns.blocks,
                max_transfer: self.ctrl.max_transfer,
                read_only: false,
            };
            let queues = NsQueues {
                ctrl: Arc::clone(&self.ctrl),
                nsid: ns.nsid,
                shift: ns.shift,
            };
            disks.push(storage::publish(cx.sleepable(), info, Arc::new(queues)));
        }
        *self.ctrl.disks.lock() = disks;
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        for disk in core::mem::take(&mut *self.ctrl.disks.lock()) {
            storage::withdraw(s, &disk);
        }
        let regs = &self.ctrl.regs;
        // A normal shutdown, then disabled: no more DMA.
        let cc = regs.mmio.read32(reg::CC);
        regs.mmio.write32(reg::CC, cc | CC_SHN_NORMAL);
        for _ in 0..1000 {
            if regs.mmio.read32(reg::CSTS) & (3 << 2) == CSTS_SHST_DONE {
                break;
            }
            sleep_ms(s, 1);
        }
        regs.mmio.write32(reg::CC, 0);
        for q in &self.ctrl.queues {
            let held: Vec<InFlight> = q
                .lock(s)
                .slots
                .iter_mut()
                .filter_map(Option::take)
                .collect();
            for f in held {
                f.finish().complete(Err(BioError::Gone));
            }
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Nvme>();
