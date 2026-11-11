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
//!   A command failed with a retriable status is sent again, up to
//!   `hw.nvme.retry_count` times; a failure is printed as FreeBSD prints
//!   it (the command, then its completion).
//! - A watchdog thread checks the outstanding commands every half second
//!   while there are any (`nvme_qpair_timeout`): a controller reading all
//!   ones or with `CSTS.CFS` set is reset; a command past its deadline
//!   (`dev.nvme.N.timeout_period`, 30 s) first has its queue polled, in
//!   case its interrupt was missed, and if it is still outstanding the
//!   controller is reset. A reset disables the queues (interrupts and new
//!   requests wait), resets and re-enables the controller, identifies it,
//!   creates the I/O queues again, and sends what was outstanding again,
//!   counting a retry (`nvme_io_qpair_enable`); a request out of retries
//!   fails with `EIO`. A reset that fails fails the controller: what it
//!   held fails with `EIO`, and its namespaces are withdrawn (`ENXIO`).
//! - Stop stops the watchdog, withdraws the namespaces, shuts the
//!   controller down normally, disables it, and fails what it still held.
//!
//! Everything the controller writes is checked: a completion's command ID
//! must name an outstanding command.
//!
//! Behavior reference: sys/dev/nvme/ (nvme_ctrlr.c, nvme_qpair.c,
//! nvme_pci.c, nvme_sysctl.c); the NVM Express Base Specification 2.0 for
//! the registers.

#![no_std]
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "command fields pack narrower values; queue and vector counts are small"
)]

extern crate alloc;

pub mod fault;
mod status;
mod timeout;

pub use timeout::set_tunables;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, StartCx, StopCx,
    StopReason,
};
use devtree::ids::PciId;
use irq::Filter;
use pci::bus::PciChild;
use regs::Bytes;
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable, Thread, WaitQueue};
use status::{Outcome, Status};
use storage::{Bio, BioError, BioOp, Disk, DiskInfo, DiskQueues};
use sync::SpinLock;
use sysctl::{Number, Value};
use timeout::{Deadline, Scan};
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag, Mapped, dir};
use vm::object::IoBuf;

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
/// What a register reads once the device is gone from the bus
/// (`NVME_GONE`).
const GONE: u32 = u32::MAX;

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
/// The largest transfer per request (capped by MDTS): FreeBSD's
/// `MAXPHYS`, so a file system's cluster is one command and one doorbell
/// (each an exit to the hypervisor in a VM).
const MAX_TRANSFER: usize = 1024 * 1024;
/// PRP list bytes per command: one entry per page after the first, so
/// the largest transfer (a page more when it starts mid-page) needs
/// `MAX_TRANSFER / PAGE` entries. A list must not cross a page: the
/// controller takes a page's last entry as a pointer to the next list
/// page (the base specification's PRP list rules), so a crossing list
/// sends the data wherever the following entry points. FreeBSD keeps
/// each tracker's list within a page (`nvme_qpair_construct`); here the
/// lists sit at multiples of their size in page-aligned memory, and the
/// size divides a page.
const PRP_LIST: usize = MAX_TRANSFER / PAGE * 8;
const _: () = assert!(PAGE.is_multiple_of(PRP_LIST));
/// A polled admin command that never completes, or a controller gone.
const ADMIN_FAILED: Status = 0xffff;

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

/// A completion queue entry.
#[derive(Clone, Copy, Debug)]
struct Cpl {
    cid: u16,
    status: Status,
    dw0: u32,
    /// The phase tag it was posted with.
    phase: bool,
}

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

    /// Back to the state the controller expects of a new queue
    /// (`nvme_qpair_reset`): both rings empty and zeroed, the phase tag
    /// looked for 1. Only while the controller does not use it.
    fn reset(&mut self) {
        let n = usize::from(self.entries);
        for off in (0..n * SQE).step_by(8) {
            self.sq.write_at::<u64>(off, 0);
        }
        for off in (0..n * CQE).step_by(8) {
            self.cq.write_at::<u64>(off, 0);
        }
        self.tail = 0;
        self.head = 0;
        self.phase = true;
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

    /// The next completion, if the controller has posted one.
    fn pop(&mut self) -> Option<Cpl> {
        let at = usize::from(self.head) * CQE;
        let dw3 = self.cq.read_at::<u32>(at + 12);
        if (dw3 >> 16 & 1 != 0) != self.phase {
            return None;
        }
        // The phase bit before the rest of the entry.
        fence(Ordering::Acquire);
        let cpl = Cpl {
            cid: (dw3 & 0xffff) as u16,
            status: (dw3 >> 17) as u16 & 0x7fff,
            dw0: self.cq.read_at::<u32>(at),
            phase: self.phase,
        };
        self.head += 1;
        if self.head == self.entries {
            self.head = 0;
            self.phase = !self.phase;
        }
        Some(cpl)
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

    fn csts(&self) -> u32 {
        self.mmio.read32(reg::CSTS)
    }

    fn sq_doorbell(&self, q: u16, tail: u16) {
        // The new entries reach memory before the controller is told.
        hal::dma::doorbell_barrier();
        self.mmio.write32(
            reg::DOORBELLS + 2 * usize::from(q) * self.stride,
            u32::from(tail),
        );
    }

    fn cq_doorbell(&self, q: u16, head: u16) {
        hal::dma::doorbell_barrier();
        self.mmio.write32(
            reg::DOORBELLS + (2 * usize::from(q) + 1) * self.stride,
            u32::from(head),
        );
    }

    /// Points the controller at the admin rings (it must be disabled).
    fn set_admin(&self, rings: &Rings) {
        let n = u32::from(rings.entries) - 1;
        self.mmio.write32(reg::AQA, n << 16 | n);
        self.write64(reg::ASQ, rings.sq.bus_addr());
        self.write64(reg::ACQ, rings.cq.bus_addr());
    }
}

/// A request's data, mapped for the controller.
enum Data {
    None,
    Read(Mapped<IoBuf, dir::FromDevice>),
    Write(Mapped<IoBuf, dir::ToDevice>),
}

/// An outstanding command (FreeBSD's tracker and request).
struct InFlight {
    bio: Bio,
    data: Data,
    /// The command as sent, to send again.
    cmd: Command,
    /// Times sent again.
    retries: u32,
    deadline: Deadline,
}

impl InFlight {
    fn finish(self) -> Bio {
        let Self { mut bio, data, .. } = self;
        match data {
            Data::None => {}
            Data::Read(d) => bio.buf = d.unmap(),
            Data::Write(d) => bio.buf = d.unmap(),
        }
        bio
    }
}

/// A queue's recovery state (`recovery_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    /// Normal operation.
    None,
    /// Disabled while the controller resets: interrupts are ignored and
    /// new requests wait.
    Waiting,
}

/// One I/O queue pair.
struct IoQueue {
    id: u16,
    rings: Rings,
    /// Per command ID: PRP list space.
    prps: DmaCoherent<u64>,
    slots: Vec<Option<InFlight>>,
    free: Vec<u16>,
    recovery: Recovery,
}

impl IoQueue {
    fn outstanding(&mut self) -> impl Iterator<Item = &mut InFlight> {
        self.slots.iter_mut().flatten()
    }
}

/// A queue's counters (`num_*`), readable without its lock. Each is
/// written only under the queue's lock.
struct QueueStats {
    entries: u32,
    cmds: AtomicU64,
    intr_handler_calls: AtomicU64,
    retries: AtomicU64,
    failures: AtomicU64,
    ignored: AtomicU64,
    recovery_nolock: AtomicU64,
    /// The recovery state as `dev.nvme.N.ioq.M.recovery` shows it.
    recovery: AtomicU32,
}

impl QueueStats {
    fn new(entries: u16) -> Self {
        Self {
            entries: u32::from(entries),
            cmds: AtomicU64::new(0),
            intr_handler_calls: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            ignored: AtomicU64::new(0),
            recovery_nolock: AtomicU64::new(0),
            recovery: AtomicU32::new(0),
        }
    }

    /// Adds `n` (the queue's lock is held: no other writer).
    fn add(a: &AtomicU64, n: u64) {
        // Relaxed: a statistic, one writer.
        a.store(a.load(Ordering::Relaxed).wrapping_add(n), Ordering::Relaxed);
    }

    fn set_recovery(&self, r: Recovery) {
        let v = match r {
            Recovery::None => 0,
            Recovery::Waiting => 1,
        };
        self.recovery.store(v, Ordering::Relaxed);
    }
}

/// A namespace as a disk.
struct Namespace {
    nsid: u32,
    /// log2 of the block size.
    shift: u32,
    blocks: u64,
    /// Its world-wide name, from its NGUID or EUI-64.
    lunid: Option<String>,
}

/// The watchdog's state (the qpair callout's `timer_armed`).
struct Watch {
    queue: WaitQueue,
    /// Whether it looks every half second; set by a submission, cleared
    /// when it finds nothing outstanding.
    armed: AtomicBool,
    stopping: AtomicBool,
}

/// What the sysctls read and set.
struct Knobs {
    /// Per I/O queue.
    stats: Vec<QueueStats>,
    /// `dev.nvme.N.timeout_period`, seconds.
    timeout_period: AtomicU32,
    /// `dev.nvme.N.admin_timeout_period`, seconds.
    admin_timeout_period: AtomicU32,
}

impl Knobs {
    /// `admin_timeout_period`, or `timeout_period`.
    fn period(&self, admin: bool) -> &AtomicU32 {
        if admin {
            &self.admin_timeout_period
        } else {
            &self.timeout_period
        }
    }
}

/// What the disks, interrupt handlers, and watchdog share.
pub struct Controller {
    regs: Regs,
    tag: DmaTag,
    unit: u32,
    queues: Vec<Mutex<IoQueue>>,
    /// The counters and settings the sysctls show (shared with them
    /// apart from the registers, which go with the instance).
    knobs: Arc<Knobs>,
    /// Each queue's MSI-X vector (an index into the lines).
    vectors: Vec<usize>,
    max_transfer: usize,
    disks: SpinLock<Vec<Arc<Disk>>>,
    admin: Mutex<Admin>,
    /// Identify data, for the admin commands.
    identify: DmaCoherent<u8>,
    /// CAP.TO: how long CSTS.RDY may take to change.
    ready_timeout_ms: u64,
    /// `is_failed`: every request fails.
    failed: AtomicBool,
    /// `isr_warned`.
    isr_warned: AtomicBool,
    watch: Watch,
}

impl core::fmt::Debug for Controller {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Controller")
            .field("unit", &self.unit)
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

/// What reaping a queue produced, delivered once its lock is dropped.
#[derive(Default)]
struct Reaped {
    done: Vec<(Bio, Result<(), BioError>)>,
    /// Commands that completed (or were sent again).
    found: usize,
    /// Lines to print.
    notes: Vec<String>,
}

impl Controller {
    fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("nvme{}: {args}", self.unit));
    }

    /// `nvme_ctrlr_devctl_log`: a console line, and a devctl event.
    fn devctl_log(&self, kind: &str, msg: &str) {
        self.log(format_args!("{msg}"));
        let data = format!("name=\"nvme{}\" {msg}", self.unit);
        bus::manager().notify("nvme", "controller", kind, Some(&data));
    }

    fn admin_timeout_ms(&self) -> u64 {
        u64::from(self.knobs.admin_timeout_period.load(Ordering::Relaxed)) * 1000
    }

    /// The I/O timeout in nanoseconds.
    fn period_ns(&self) -> u64 {
        fault::timeout_override_ns(self.unit).unwrap_or_else(|| {
            u64::from(self.knobs.timeout_period.load(Ordering::Relaxed)) * 1_000_000_000
        })
    }

    fn map(&self, bio: &mut Bio) -> Result<Data, ()> {
        match bio.op {
            BioOp::Flush => Ok(Data::None),
            BioOp::Read => self
                .tag
                .map(core::mem::take(&mut bio.buf))
                .map(Data::Read)
                .map_err(|(buf, _)| bio.buf = buf),
            BioOp::Write => self
                .tag
                .map(core::mem::take(&mut bio.buf))
                .map(Data::Write)
                .map_err(|(buf, _)| bio.buf = buf),
        }
    }

    /// The two lines `nvme_qpair_print_command` and
    /// `nvme_qpair_print_completion` print for a failed command.
    fn note_failure(notes: &mut Vec<String>, sqid: u16, cid: u16, f: &InFlight, cpl: &Cpl) {
        let c = &f.cmd;
        notes.push(format!(
            "{}",
            status::IoCommand {
                opcode: c.opcode,
                sqid,
                cid,
                nsid: c.nsid,
                cdw10: c.cdw10,
                cdw11: c.cdw11,
                cdw12: c.cdw12,
            }
        ));
        notes.push(format!(
            "{}",
            status::Completion {
                status: cpl.status,
                phase: cpl.phase,
                sqid,
                cid,
                cdw0: cpl.dw0,
            }
        ));
    }

    /// Reaps queue `q`'s completions (`nvme_qpair_process_completions`),
    /// from its interrupt handler (`isr`) or the watchdog.
    fn reap(&self, s: &Sleepable, q: usize, isr: bool) -> Reaped {
        let mut out = Reaped::default();
        let stats = &self.knobs.stats[q];
        let mut io = self.queues[q].lock(s);
        if io.recovery != Recovery::None {
            // The queue is disabled for a reset: what completed is sent
            // again once it is enabled.
            QueueStats::add(
                if isr {
                    &stats.recovery_nolock
                } else {
                    &stats.ignored
                },
                1,
            );
            return out;
        }
        if isr {
            QueueStats::add(&stats.intr_handler_calls, 1);
        }
        let limit = timeout::retry_count();
        let (mut any, mut resent) = (false, false);
        while let Some(cpl) = io.rings.pop() {
            any = true;
            let cid = cpl.cid;
            let i = usize::from(cid);
            if io.slots.get(i).is_none_or(Option::is_none) || fault::lose(self.unit) {
                continue;
            }
            let Some(mut f) = io.slots[i].take() else {
                continue;
            };
            out.found += 1;
            let outcome = status::outcome(cpl.status, f.retries, limit);
            if outcome != Outcome::Done {
                // ERROR_PRINT_ALL.
                Self::note_failure(&mut out.notes, io.id, cid, &f, &cpl);
            }
            match outcome {
                Outcome::Done => {
                    io.free.push(cid);
                    out.done.push((f.finish(), Ok(())));
                }
                Outcome::Retry => {
                    QueueStats::add(&stats.retries, 1);
                    f.retries += 1;
                    f.deadline = Deadline::Unset;
                    io.rings.push(&f.cmd, cid);
                    io.slots[i] = Some(f);
                    resent = true;
                }
                Outcome::Fail { exhausted } => {
                    if exhausted {
                        QueueStats::add(&stats.failures, 1);
                    }
                    io.free.push(cid);
                    out.done.push((f.finish(), Err(BioError::Io)));
                }
            }
        }
        if any {
            self.regs.cq_doorbell(io.id, io.rings.head);
        }
        if resent {
            QueueStats::add(&stats.cmds, 1);
            self.regs.sq_doorbell(io.id, io.rings.tail);
        }
        out
    }

    /// Prints what a reap noted and completes its requests.
    fn deliver(&self, r: Reaped) {
        for n in &r.notes {
            self.log(format_args!("{n}"));
        }
        for (bio, res) in r.done {
            bio.complete(res);
        }
    }

    /// Has the disks resubmit what waits on queue `q`.
    fn resume(&self, s: &Sleepable, q: usize) {
        let disks = self.disks.lock().clone();
        for disk in disks {
            disk.resume(s, q);
        }
    }

    /// Starts the watchdog if it is idle; called with a queue's lock held
    /// after starting commands on it.
    fn arm(&self) -> bool {
        // Relaxed: the queue's lock orders this against the watchdog's
        // look at the queue (see `timeout`); the swap makes one caller
        // the waker.
        !self.watch.armed.load(Ordering::Relaxed) && !self.watch.armed.swap(true, Ordering::Relaxed)
    }

    /// One look at the outstanding commands (`nvme_qpair_timeout`, for
    /// every queue at once; aborts are not used, as `hw.nvme.enable_aborts`
    /// is off by default).
    fn timeout(&self, s: &Sleepable) {
        let w = &self.watch;
        if self.failed.load(Ordering::Acquire) {
            self.log(format_args!(
                "Failed controller, stopping watchdog timeout."
            ));
            w.armed.store(false, Ordering::Relaxed);
            return;
        }
        // Disarmed before looking: a submission the look misses sees it
        // disarmed (the queue's lock orders the two) and wakes the
        // watchdog again.
        w.armed.store(false, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        let now = sched::now_ns();
        let period = self.period_ns();
        let mut outstanding = 0;
        let mut expired = Vec::new();
        for (q, m) in self.queues.iter().enumerate() {
            let mut io = m.lock(s);
            let scan = Scan::of(io.outstanding().map(|f| &mut f.deadline), now, period);
            outstanding += scan.outstanding;
            if scan.expired > 0 {
                expired.push(q);
            }
        }
        if outstanding == 0 {
            return;
        }
        w.armed.store(true, Ordering::Relaxed);
        // A controller gone, or failed: reset at once (letting the reset
        // find out whether it is gone for good).
        let csts = self.regs.csts();
        if csts == GONE || csts & CSTS_CFS != 0 {
            self.log(format_args!(
                "Resetting controller due to a timeout{}.",
                if csts == GONE {
                    " and possible hot unplug"
                } else {
                    " and fatal error status"
                }
            ));
            self.reset(s);
            return;
        }
        let mut reset = false;
        for q in expired {
            // A last try, in case the interrupt was missed.
            let r = self.reap(s, q, false);
            let found = r.found;
            self.deliver(r);
            self.resume(s, q);
            if found > 0 && !self.isr_warned.swap(true, Ordering::Relaxed) {
                self.log(format_args!("System interrupt issues?"));
            }
            let mut io = self.queues[q].lock(s);
            if io.outstanding().any(|f| f.deadline.passed(now)) {
                reset = true;
            }
        }
        if reset {
            self.log(format_args!("Resetting controller due to a timeout."));
            self.reset(s);
        }
    }

    /// Waits until CSTS.RDY is `ready`, for at most CAP.TO
    /// (`nvme_ctrlr_wait_for_ready`).
    fn wait_ready(&self, s: &Sleepable, ready: bool) -> Result<(), ()> {
        for _ in 0..self.ready_timeout_ms.max(1) {
            let csts = self.regs.csts();
            if csts == GONE {
                return Err(());
            }
            if (csts & CSTS_RDY != 0) == ready {
                return Ok(());
            }
            sleep_ms(s, 1);
        }
        self.log(format_args!(
            "controller ready did not become {} within {} ms",
            u8::from(ready),
            self.ready_timeout_ms
        ));
        Err(())
    }

    /// Disables and enables the controller, with the admin queue reset
    /// (`nvme_ctrlr_hw_reset`). Fails if it is gone or never ready.
    fn hw_reset(&self, s: &Sleepable, admin: &mut Admin) -> Result<(), ()> {
        let regs = &self.regs;
        // `nvme_ctrlr_disable`: EN and RDY must agree before EN changes.
        let cc = regs.mmio.read32(reg::CC);
        let csts = regs.csts();
        if csts == GONE {
            return Err(());
        }
        if cc & CC_EN != 0 {
            if csts & CSTS_RDY == 0 {
                self.wait_ready(s, true)?;
            }
            regs.mmio.write32(reg::CC, cc & !CC_EN);
        }
        self.wait_ready(s, false)?;
        // `nvme_ctrlr_enable`, with the admin queue as new.
        admin.rings.reset();
        regs.set_admin(&admin.rings);
        // Masked while admin commands are polled (INTx, MSI).
        regs.mmio.write32(reg::INTMS, u32::MAX);
        regs.mmio.write32(reg::CC, CC_EN | CC_ENTRIES);
        self.wait_ready(s, true)?;
        regs.mmio.write32(reg::INTMS, u32::MAX);
        Ok(())
    }

    /// Identifies the controller again and creates the I/O queues again
    /// (`nvme_ctrlr_start` when resetting), empty.
    fn restart(&self, s: &Sleepable, admin: &mut Admin) -> Result<(), &'static str> {
        let regs = &self.regs;
        let t = self.admin_timeout_ms();
        let id = Command {
            opcode: ADMIN_IDENTIFY,
            prp1: self.identify.bus_addr(),
            cdw10: CNS_CONTROLLER,
            ..Command::default()
        };
        admin
            .run(regs, s, &id, t)
            .map_err(|_| "nvme_identify_controller failed!")?;
        let n = self.queues.len() as u32;
        let dw0 = admin
            .run(
                regs,
                s,
                &Command {
                    opcode: ADMIN_SET_FEATURES,
                    cdw10: FEATURE_QUEUES,
                    cdw11: (n - 1) << 16 | (n - 1),
                    ..Command::default()
                },
                t,
            )
            .map_err(|_| "nvme_ctrlr_set_num_qpairs failed!")?;
        if (dw0 & 0xffff).min(dw0 >> 16) + 1 < n {
            return Err("number of I/O queues changed across the reset");
        }
        for (i, m) in self.queues.iter().enumerate() {
            let (qid, cq, sq, entries) = {
                let mut io = m.lock(s);
                io.rings.reset();
                (
                    io.id,
                    io.rings.cq.bus_addr(),
                    io.rings.sq.bus_addr(),
                    io.rings.entries,
                )
            };
            let size = u32::from(entries) - 1;
            let (create_cq, create_sq) = queue_commands(qid, self.vectors[i], size, cq, sq);
            admin
                .run(regs, s, &create_cq, t)
                .map_err(|_| "nvme_create_io_cq failed!")?;
            admin
                .run(regs, s, &create_sq, t)
                .map_err(|_| "nvme_create_io_sq failed!")?;
        }
        regs.mmio.write32(reg::INTMC, u32::MAX);
        Ok(())
    }

    /// Disables every I/O queue for a reset (`nvme_io_qpair_disable`):
    /// interrupts are ignored, new requests wait, and nothing times out.
    fn disable_queues(&self, s: &Sleepable) {
        for (q, m) in self.queues.iter().enumerate() {
            let mut io = m.lock(s);
            io.recovery = Recovery::Waiting;
            self.knobs.stats[q].set_recovery(Recovery::Waiting);
            for f in io.outstanding() {
                f.deadline = Deadline::Unset;
            }
        }
    }

    /// Enables queue `q` after a reset (`nvme_io_qpair_enable`): what was
    /// outstanding is aborted, which sends it again unless its retries
    /// are spent; then what waited is resubmitted.
    fn enable_queue(&self, s: &Sleepable, q: usize) {
        let stats = &self.knobs.stats[q];
        let limit = timeout::retry_count();
        let mut r = Reaped::default();
        {
            let mut io = self.queues[q].lock(s);
            let sqid = io.id;
            let held: Vec<u16> = (0..io.slots.len())
                .filter(|&i| io.slots[i].is_some())
                .map(|i| i as u16)
                .collect();
            if !held.is_empty() {
                r.notes.push("aborting outstanding i/o".into());
            }
            let aborted = Cpl {
                cid: 0,
                status: status::make(0, status::SC_ABORTED_BY_REQUEST, false),
                dw0: 0,
                phase: false,
            };
            let mut resent = 0;
            for &cid in &held {
                let i = usize::from(cid);
                let Some(mut f) = io.slots[i].take() else {
                    continue;
                };
                match status::outcome(aborted.status, f.retries, limit) {
                    Outcome::Retry => {
                        QueueStats::add(&stats.retries, 1);
                        f.retries += 1;
                        f.deadline = Deadline::Unset;
                        io.rings.push(&f.cmd, cid);
                        io.slots[i] = Some(f);
                        resent += 1;
                    }
                    other => {
                        // ERROR_PRINT_NO_RETRY.
                        if other == (Outcome::Fail { exhausted: true }) {
                            QueueStats::add(&stats.failures, 1);
                        }
                        Self::note_failure(&mut r.notes, sqid, cid, &f, &Cpl { cid, ..aborted });
                        io.free.push(cid);
                        r.done.push((f.finish(), Err(BioError::Io)));
                    }
                }
            }
            if !held.is_empty() {
                r.notes.push("done aborting outstanding i/o".into());
            }
            io.recovery = Recovery::None;
            stats.set_recovery(Recovery::None);
            if resent > 0 {
                QueueStats::add(&stats.cmds, resent);
                self.regs.sq_doorbell(io.id, io.rings.tail);
            }
        }
        self.deliver(r);
        self.resume(s, q);
    }

    /// Fails the controller (`nvme_ctrlr_fail`): what it holds fails with
    /// `EIO`, and its namespaces are withdrawn.
    fn fail(&self, s: &Sleepable) {
        self.failed.store(true, Ordering::Release);
        let dead = Cpl {
            cid: 0,
            status: status::make(0, status::SC_ABORTED_BY_REQUEST, true),
            dw0: 0,
            phase: false,
        };
        for q in 0..self.queues.len() {
            let mut r = Reaped::default();
            {
                let mut io = self.queues[q].lock(s);
                let sqid = io.id;
                for i in 0..io.slots.len() {
                    let Some(f) = io.slots[i].take() else {
                        continue;
                    };
                    if r.done.is_empty() {
                        r.notes.push("failing outstanding i/o".into());
                    }
                    let cid = i as u16;
                    Self::note_failure(&mut r.notes, sqid, cid, &f, &Cpl { cid, ..dead });
                    io.free.push(cid);
                    r.done.push((f.finish(), Err(BioError::Io)));
                }
            }
            self.deliver(r);
        }
        // The namespaces go (`nvme_sim_fail_all_ns`): what waits on them
        // fails, and so does anything sent to them later. Taken in its own
        // statement: no spinlock held while withdrawing.
        let disks = core::mem::take(&mut *self.disks.lock());
        for disk in disks {
            storage::withdraw(s, &disk);
        }
    }

    /// Resets the controller and sends what it held again
    /// (`nvme_ctrlr_reset_task`), or fails it.
    fn reset(&self, s: &Sleepable) {
        let mut admin = self.admin.lock(s);
        self.devctl_log("RESET", "event=\"start\"");
        self.disable_queues(s);
        let ok = match self.hw_reset(s, &mut admin) {
            Ok(()) => {
                self.devctl_log("RESET", "event=\"success\"");
                match self.restart(s, &mut admin) {
                    Ok(()) => true,
                    Err(why) => {
                        self.log(format_args!("{why}"));
                        false
                    }
                }
            }
            Err(()) => {
                self.devctl_log("RESET", "event=\"timed_out\"");
                false
            }
        };
        // The disks' locks come before the admin lock.
        drop(admin);
        if ok {
            for q in 0..self.queues.len() {
                self.enable_queue(s, q);
            }
        } else {
            self.fail(s);
        }
    }
}

/// The admin commands creating I/O queue `qid`'s completion and
/// submission queues of `size + 1` entries, interrupting on `vector`.
fn queue_commands(qid: u16, vector: usize, size: u32, cq: u64, sq: u64) -> (Command, Command) {
    (
        Command {
            opcode: ADMIN_CREATE_CQ,
            prp1: cq,
            cdw10: size << 16 | u32::from(qid),
            // Interrupts enabled, physically contiguous.
            cdw11: (vector as u32) << 16 | 0b11,
            ..Command::default()
        },
        Command {
            opcode: ADMIN_CREATE_SQ,
            prp1: sq,
            cdw10: size << 16 | u32::from(qid),
            cdw11: u32::from(qid) << 16 | 1,
            ..Command::default()
        },
    )
}

impl DiskQueues for NsQueues {
    fn queues(&self) -> usize {
        self.ctrl.queues.len()
    }

    fn submit(&self, s: &Sleepable, q: usize, bios: &mut Vec<Bio>) {
        let ctrl = &self.ctrl;
        let qi = q % ctrl.queues.len();
        if ctrl.failed.load(Ordering::Acquire) {
            // `_nvme_qpair_submit_request` on a failed controller.
            for bio in core::mem::take(bios) {
                bio.complete(Err(BioError::Io));
            }
            return;
        }
        let mut io = ctrl.queues[qi].lock(s);
        if io.recovery != Recovery::None {
            // Resubmitted when the reset enables the queue.
            return;
        }
        let mut started = 0u64;
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
            if !prp_ok || segs.len() > PRP_LIST / 8 + 1 {
                InFlight {
                    bio,
                    data,
                    cmd: Command::default(),
                    retries: 0,
                    deadline: Deadline::Unset,
                }
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
            io.slots[usize::from(cid)] = Some(InFlight {
                bio,
                data,
                cmd: c,
                retries: 0,
                deadline: Deadline::Unset,
            });
            started += 1;
        }
        let mut wake = false;
        if started > 0 {
            QueueStats::add(&ctrl.knobs.stats[qi].cmds, started);
            ctrl.regs.sq_doorbell(io.id, io.rings.tail);
            wake = ctrl.arm();
        }
        drop(io);
        if wake {
            ctrl.watch.queue.wake_all();
        }
        *bios = refused;
    }
}

/// The admin queue: polled, one command at a time.
struct Admin {
    rings: Rings,
    next_cid: u16,
}

/// Sleeps `ms` milliseconds.
fn sleep_ms(s: &Sleepable, ms: u64) {
    bus::manager().sleep_ms(s, ms);
}

impl Admin {
    /// Runs `c` and waits for its completion, at most `timeout_ms`: dword
    /// 0, or the status ([`ADMIN_FAILED`] if it never completed or the
    /// controller is gone).
    fn run(
        &mut self,
        regs: &Regs,
        s: &Sleepable,
        c: &Command,
        timeout_ms: u64,
    ) -> Result<u32, Status> {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        self.rings.push(c, cid);
        regs.sq_doorbell(0, self.rings.tail);
        for _ in 0..timeout_ms.max(1) {
            if let Some(cpl) = self.rings.pop() {
                regs.cq_doorbell(0, self.rings.head);
                if cpl.cid != cid {
                    continue;
                }
                return if cpl.status == 0 {
                    Ok(cpl.dw0)
                } else {
                    Err(cpl.status)
                };
            }
            if regs.csts() == GONE {
                break;
            }
            sleep_ms(s, 1);
        }
        Err(ADMIN_FAILED)
    }
}

/// Waits until `CSTS.RDY` is `ready`, for at most CAP.TO (attach).
fn wait_ready(regs: &Regs, s: &Sleepable, ready: bool, timeout_ms: u64) -> Result<(), AttachError> {
    for _ in 0..timeout_ms.max(1) {
        let csts = regs.csts();
        if csts == GONE {
            return Err(AttachError::Device("controller gone"));
        }
        if csts & CSTS_CFS != 0 {
            return Err(AttachError::Device("controller fatal status"));
        }
        if (csts & CSTS_RDY != 0) == ready {
            return Ok(());
        }
        sleep_ms(s, 1);
    }
    Err(AttachError::Device("controller ready timeout"))
}

/// An identify field as CAM shows it (`cam_strvis_flag` with
/// `CAM_STRVIS_FLAG_NONASCII_SPC`): leading spaces and trailing spaces
/// and NULs trimmed, anything not printable ASCII a space.
fn strvis(b: &[u8]) -> String {
    let start = b.iter().position(|&c| c != b' ').unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|&c| c != b' ' && c != 0)
        .map_or(start, |i| i + 1)
        .max(start);
    b[start..end]
        .iter()
        .map(|&c| {
            if (0x20..0x80).contains(&c) {
                char::from(c)
            } else {
                ' '
            }
        })
        .collect()
}

/// A namespace's `GEOM::lunid` (`nvme_xpt`'s device id, as `xpt_getattr`
/// prints it): its NGUID in hex if it has one, else its EUI-64.
fn lunid(nguid: &[u8], eui64: &[u8]) -> Option<String> {
    use core::fmt::Write;
    let id = [nguid, eui64]
        .into_iter()
        .find(|b| b.iter().any(|&c| c != 0))?;
    let mut s = String::new();
    for b in id {
        let _ = write!(s, "{b:02x}");
    }
    Some(s)
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
    unit: u32,
    namespaces: Vec<Namespace>,
    lines: SpinLock<Vec<IrqLine>>,
    /// MSI-X vector count handed out (0: INTx).
    msix: usize,
    /// The MSI-X vectors, given back when the instance goes.
    _messages: Option<pci::bus::Messages>,
    identity: String,
    /// The model and serial number, as `nda` shows them (`descr`,
    /// `ident`).
    model: String,
    serial: String,
    watchdog: SpinLock<Option<Arc<Thread>>>,
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
    if fault::ignoring(ctrl.unit) {
        return;
    }
    for q in 0..ctrl.queues.len() {
        if ctrl.vectors[q] != vector {
            continue;
        }
        let r = ctrl.reap(s, q, true);
        ctrl.deliver(r);
        ctrl.resume(s, q);
    }
}

/// The watchdog thread: every half second while commands are
/// outstanding, [`Controller::timeout`]; asleep otherwise.
fn watchdog(ctrl: &Controller, s: &Sleepable) {
    let w = &ctrl.watch;
    // Acquire: pairs with stop's release.
    let stopping = || w.stopping.load(Ordering::Acquire);
    loop {
        w.queue
            .wait_until(s, || w.armed.load(Ordering::Relaxed) || stopping());
        if stopping() {
            return;
        }
        let mut stop = stopping;
        bus::manager().wait_timeout(s, &w.queue, &mut stop, timeout::CHECK_NS);
        if stopping() {
            return;
        }
        ctrl.timeout(s);
    }
}

impl Nvme {
    /// `nvme_sysctl_initialize_ctrlr`: the timeouts, and the queues'
    /// counters, summed and each.
    fn add_sysctls(&self, s: &Sleepable) {
        type Stat = fn(&QueueStats) -> &AtomicU64;
        const STATS: [(&str, &str, Stat); 6] = [
            ("num_cmds", "Number of commands submitted", |q| &q.cmds),
            (
                "num_intr_handler_calls",
                "Number of times interrupt handler was invoked (will typically be less than number of actual interrupts generated due to coalescing)",
                |q| &q.intr_handler_calls,
            ),
            ("num_retries", "Number of commands retried", |q| &q.retries),
            (
                "num_failures",
                "Number of commands ending in failure after all retries",
                |q| &q.failures,
            ),
            (
                "num_ignored",
                "Number of interrupts posted, but were administratively ignored",
                |q| &q.ignored,
            ),
            (
                "num_recovery_nolock",
                "Number of times that we failed to lock recovery in the ISR",
                |q| &q.recovery_nolock,
            ),
        ];
        let read = |a: &AtomicU64| a.load(Ordering::Relaxed).cast_signed();
        let unit = self.unit;
        bus::manager().sysctl(s, &mut |t| {
            let Some(node) = bus::device_node(t, Nvme::NAME, unit) else {
                return;
            };
            let n = self.ctrl.queues.len() as u32;
            let _ = t.add_value(
                node,
                Number::Auto,
                "num_io_queues",
                "Number of I/O queue pairs",
                Value::Uint(Box::new(move || n)),
            );
            for (name, descr, admin) in [
                (
                    "admin_timeout_period",
                    "Timeout period for Admin queue (in seconds)",
                    true,
                ),
                (
                    "timeout_period",
                    "Timeout period for I/O queues (in seconds)",
                    false,
                ),
            ] {
                let (r, w) = (Arc::clone(&self.ctrl.knobs), Arc::clone(&self.ctrl.knobs));
                let _ = t.add_writable(
                    node,
                    Number::Auto,
                    name,
                    descr,
                    ctlflag::MPSAFE,
                    // Relaxed: a setting, read when a command is next
                    // stamped.
                    Value::Uint(Box::new(move || r.period(admin).load(Ordering::Relaxed))),
                    sysctl::int_writer(move |v| {
                        let p = timeout::check_period(v).ok_or(Errno::EINVAL)?;
                        w.period(admin).store(p, Ordering::Relaxed);
                        Ok(())
                    }),
                );
            }
            for (name, descr, field) in STATS {
                let c = Arc::clone(&self.ctrl.knobs);
                let _ = t.add_formatted(
                    node,
                    Number::Auto,
                    name,
                    descr,
                    ctltype::S64,
                    "IU",
                    Value::S64(Box::new(move || {
                        c.stats.iter().map(|q| read(field(q))).sum()
                    })),
                );
            }
            let Ok(ioq) = t.add_node(node, Number::Auto, "ioq", "I/O Queues") else {
                return;
            };
            for i in 0..self.ctrl.queues.len() {
                let Ok(qn) = t.add_node(ioq, Number::Auto, &format!("{i}"), "IO Queue") else {
                    continue;
                };
                let entries = self.ctrl.knobs.stats[i].entries;
                let _ = t.add_value(
                    qn,
                    Number::Auto,
                    "num_entries",
                    "Number of entries in hardware queue",
                    Value::Uint(Box::new(move || entries)),
                );
                let _ = t.add_value(
                    qn,
                    Number::Auto,
                    "num_trackers",
                    "Number of trackers pre-allocated for this queue pair",
                    Value::Uint(Box::new(move || entries - 1)),
                );
                for (name, descr, field) in STATS {
                    let c = Arc::clone(&self.ctrl.knobs);
                    let _ = t.add_value(
                        qn,
                        Number::Auto,
                        name,
                        descr,
                        Value::S64(Box::new(move || read(field(&c.stats[i])))),
                    );
                }
                let c = Arc::clone(&self.ctrl.knobs);
                let _ = t.add_value(
                    qn,
                    Number::Auto,
                    "recovery",
                    "Current recovery state of the queue",
                    Value::Uint(Box::new(move || {
                        c.stats[i].recovery.load(Ordering::Relaxed)
                    })),
                );
            }
        });
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
        let (io_period, admin_period) = timeout::periods();
        let admin_ms = u64::from(admin_period) * 1000;

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
        regs.set_admin(&rings);
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
        if admin
            .run(&regs, s, &id_cmd(CNS_CONTROLLER, 0), admin_ms)
            .is_err()
        {
            fail(&regs);
            return Err(AttachError::Device("identify controller failed"));
        }
        let bytes = |a: usize, b: usize| (a..b).map(|i| identify.read(i)).collect::<Vec<u8>>();
        let model = ascii(&bytes(24, 64));
        let serial = ascii(&bytes(4, 24));
        // As `nda` shows them; read now, before the namespaces' identify
        // data replaces the controller's in the buffer.
        let (nda_model, nda_serial) = (strvis(&bytes(24, 64)), strvis(&bytes(4, 24)));
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
                admin_ms,
            )
            .map_or(1, |dw0| ((dw0 & 0xffff).min(dw0 >> 16) + 1) as usize);
        let (mut lines, messages) = match pci::bus::msix_lines(cx, granted.min(cpus) as u16) {
            Ok((l, m)) => (l, Some(m)),
            Err(_) => (Vec::new(), None),
        };
        let msix = lines.len();
        if lines.is_empty() {
            lines.push(cx.irq(0)?);
        }
        let nqueues = granted.min(cpus).min(lines.len()).max(1);
        let entries = IO_ENTRIES.min(mqes.saturating_add(1));
        let mut queues = Vec::new();
        let mut stats = Vec::new();
        let mut vectors = Vec::new();
        for i in 0..nqueues {
            let qid = (i + 1) as u16;
            let vector = if msix > 0 { i % msix } else { 0 };
            let rings = Rings::new(&rings_tag, entries)?;
            let size = u32::from(entries) - 1;
            let (cq, sq) =
                queue_commands(qid, vector, size, rings.cq.bus_addr(), rings.sq.bus_addr());
            if admin.run(&regs, s, &cq, admin_ms).is_err()
                || admin.run(&regs, s, &sq, admin_ms).is_err()
            {
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
                    recovery: Recovery::None,
                },
            ));
            stats.push(QueueStats::new(entries));
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
            .run(&regs, s, &id_cmd(CNS_ACTIVE_NAMESPACES, 0), admin_ms)
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
            if admin
                .run(&regs, s, &id_cmd(CNS_NAMESPACE, nsid), admin_ms)
                .is_err()
            {
                continue;
            }
            let blocks = identify.read_at::<u64>(0);
            let flbas = identify.read(26);
            let format = usize::from(flbas & 0xf | (flbas >> 5 & 0x3) << 4);
            let shift = u32::from(identify.read(128 + 4 * format + 2));
            if blocks == 0 || !(9..=16).contains(&shift) {
                continue;
            }
            let field = |a: usize, b: usize| (a..b).map(|i| identify.read(i)).collect::<Vec<u8>>();
            namespaces.push(Namespace {
                nsid,
                shift,
                blocks,
                lunid: lunid(&field(104, 120), &field(120, 128)),
            });
        }
        // Unmask (INTx and MSI; MSI-X ignores the masks).
        regs.mmio.write32(reg::INTMC, u32::MAX);
        let unit = cx.unit();
        Ok(Self {
            model: nda_model,
            serial: nda_serial,
            identity: alloc::format!(
                "{model} (serial {serial}, firmware {firmware}, NVMe {}.{})",
                version >> 16,
                version >> 8 & 0xff
            ),
            ctrl: Arc::new(Controller {
                regs,
                tag,
                unit,
                queues,
                knobs: Arc::new(Knobs {
                    stats,
                    timeout_period: AtomicU32::new(io_period),
                    admin_timeout_period: AtomicU32::new(admin_period),
                }),
                vectors,
                max_transfer,
                disks: SpinLock::new(Vec::new()),
                admin: Mutex::new(&ADMIN_LOCK, admin),
                identify,
                ready_timeout_ms: timeout_ms,
                failed: AtomicBool::new(false),
                isr_warned: AtomicBool::new(false),
                watch: Watch {
                    queue: WaitQueue::new(),
                    armed: AtomicBool::new(false),
                    stopping: AtomicBool::new(false),
                },
            }),
            unit,
            namespaces,
            lines: SpinLock::new(lines),
            msix,
            _messages: messages,
            watchdog: SpinLock::new(None),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let lines = core::mem::take(&mut *self.lines.lock());
        for (v, line) in lines.into_iter().enumerate() {
            cx.interrupt(line, Some(filter), Some(completions), v)?;
        }
        let ctrl = Arc::clone(&self.ctrl);
        let t = sched::Builder::new("nvme watchdog")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| watchdog(&ctrl, s))
            .map_err(|_| AttachError::NoMemory)?;
        *self.watchdog.lock() = Some(t);
        bus::manager().log(format_args!(
            "nvme{}: {}, {} I/O queue(s) on {}",
            self.unit,
            self.identity,
            self.ctrl.queues.len(),
            if self.msix > 0 { "MSI-X" } else { "INTx" }
        ));
        self.add_sysctls(cx.sleepable());
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
            let disk = storage::publish(cx.sleepable(), info, Arc::new(queues));
            // What `nda` gives GEOM (`ndaregister`): the model and serial
            // number, a solid-state disk, and CAM's devstat entry.
            disk.describe(storage::Description {
                descr: self.model.clone(),
                ident: self.serial.clone(),
                lunid: ns.lunid.clone(),
                rotation: storage::Rotation::NonRotating,
                ..storage::Description::default()
            });
            disk.set_devstat(storage::devstat::Kind::NVME);
            disks.push(disk);
        }
        *self.ctrl.disks.lock() = disks;
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        // The watchdog first: no reset may race the shutdown.
        let w = &self.ctrl.watch;
        // Release: pairs with the watchdog's acquire.
        w.stopping.store(true, Ordering::Release);
        w.queue.wake_all();
        let t = self.watchdog.lock().take();
        if let Some(t) = t {
            t.join(s);
        }
        // Taken in its own statement: no spinlock held while withdrawing.
        let disks = core::mem::take(&mut *self.ctrl.disks.lock());
        for disk in disks {
            storage::withdraw(s, &disk);
        }
        // Withdrawing stops new I/O; commands already sent finish first
        // (the handlers are still installed), for at most a second.
        let in_flight = || {
            self.ctrl
                .queues
                .iter()
                .map(|q| q.lock(s).slots.iter().filter(|f| f.is_some()).count())
                .sum::<usize>()
        };
        for _ in 0..1000 {
            if in_flight() == 0 {
                break;
            }
            sleep_ms(s, 1);
        }
        let regs = &self.ctrl.regs;
        // A normal shutdown, then disabled: no more DMA.
        let cc = regs.mmio.read32(reg::CC);
        regs.mmio.write32(reg::CC, cc | CC_SHN_NORMAL);
        for _ in 0..1000 {
            if regs.csts() & (3 << 2) == CSTS_SHST_DONE {
                break;
            }
            sleep_ms(s, 1);
        }
        // Masked first: the controller must not leave its INTx input
        // asserted once our handler is gone.
        regs.mmio.write32(reg::INTMS, u32::MAX);
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
