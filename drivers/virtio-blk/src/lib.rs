// SPDX-License-Identifier: BSD-2-Clause
//! The virtio block device (`vtblk`), published as `vtbdN`.
//!
//! One request queue. A request is a chain: a 16-byte header the device
//! reads (type, sector), the data, and a status byte the device writes.
//! Headers and status bytes live in one DMA block indexed by the chain's
//! head; data is mapped from the request's own buffer for the transfer.
//! Completions are popped in the interrupt thread, which unmaps the data,
//! completes the requests, and lets the class resubmit what was waiting.
//!
//! Detach withdraws the disk first (no new requests, deferred ones fail),
//! then resets the device and fails what it still held.
//!
//! Behavior reference: sys/dev/virtio/block/virtio_blk.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, StartCx, StopCx, StopReason,
};
use devtree::ids::VirtioId;
use irq::Filter;
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable};
use storage::{Bio, BioError, BioOp, Disk, DiskInfo, DiskQueues};
use sync::SpinLock;
use virtio::{VirtioDevice, Virtqueue, device_type};
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag, Mapped, dir};

/// Feature bits (virtio 1.2 §5.2.3).
const F_SIZE_MAX: u64 = 1 << 1;
const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;

/// Configuration fields.
const CFG_CAPACITY: u16 = 0;
const CFG_SIZE_MAX: u16 = 8;
const CFG_SEG_MAX: u16 = 12;
const CFG_BLK_SIZE: u16 = 20;

/// Request types and status values (§5.2.6).
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const S_OK: u8 = 0;

/// Capacity is in 512-byte sectors whatever the block size.
const SECTOR: u64 = 512;
/// Entries wanted in the request queue.
const QUEUE: u16 = 256;
/// Bytes per head in the header block: the header, then the status byte.
const SLOT: usize = 32;
/// The largest transfer per request.
const MAX_TRANSFER: usize = 128 * 1024;

static QUEUE_LOCK: LockClass = LockClass::new("vtblk queue", 500);

/// A chain's device-readable or device-writable buffers.
type Buffers = Vec<(u64, u32)>;

/// A request's data, mapped for the device.
enum Data {
    None,
    Read(Mapped<Vec<u8>, dir::FromDevice>),
    Write(Mapped<Vec<u8>, dir::ToDevice>),
}

/// A request on the device.
struct InFlight {
    bio: Bio,
    data: Data,
}

impl InFlight {
    /// The request, with its buffer back.
    fn finish(self) -> Bio {
        let Self { mut bio, data } = self;
        match data {
            Data::None => {}
            Data::Read(d) => bio.buf = d.unmap(),
            Data::Write(d) => bio.buf = d.unmap(),
        }
        bio
    }
}

struct Queue {
    vq: Virtqueue,
    headers: DmaCoherent<u8>,
    slots: Vec<Option<InFlight>>,
}

/// The queue state the disk calls into.
pub struct Shared {
    queue: Mutex<Queue>,
    tag: DmaTag,
    flush: bool,
    /// Segments one request's data may use.
    seg_max: usize,
    disk: SpinLock<Option<Arc<Disk>>>,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared {
    /// Maps `bio`'s data, or gives the bio back if it cannot be mapped.
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

    /// Pops completions; returns the finished requests and their status.
    fn reap(&self, s: &Sleepable) -> Vec<(Bio, Result<(), BioError>)> {
        let mut out = Vec::new();
        let mut q = self.queue.lock(s);
        // No interrupts while draining (the filter has already turned them
        // off, unless the drain did not come from an interrupt).
        q.vq.set_interrupts(false);
        loop {
            let used = match q.vq.pop() {
                Ok(Some(u)) => u,
                Ok(None) if q.vq.rearm() => break,
                Ok(None) => {
                    q.vq.set_interrupts(false);
                    continue;
                }
                Err(e) => {
                    // The device broke the ring's rules; what it still
                    // holds is failed when the instance stops.
                    bus::manager()
                        .log(format_args!("vtblk: bad completion from the device: {e:?}"));
                    q.vq.set_interrupts(true);
                    break;
                }
            };
            let head = usize::from(used.head);
            let status = q.headers.read(head * SLOT + 16);
            if let Some(f) = q.slots.get_mut(head).and_then(Option::take) {
                let r = if status == S_OK {
                    Ok(())
                } else {
                    Err(BioError::Io)
                };
                out.push((f.finish(), r));
            }
        }
        out
    }
}

impl DiskQueues for Shared {
    fn queues(&self) -> usize {
        1
    }

    fn submit(&self, s: &Sleepable, _: usize, bios: &mut Vec<Bio>) {
        let mut q = self.queue.lock(s);
        let mut started = 0;
        let mut refused = Vec::new();
        for mut bio in core::mem::take(bios) {
            if !refused.is_empty() {
                refused.push(bio);
                continue;
            }
            if bio.op == BioOp::Flush && !self.flush {
                // Nothing to make durable that a cacheless device holds.
                bio.complete(Ok(()));
                continue;
            }
            let Ok(data) = self.map(&mut bio) else {
                refused.push(bio);
                continue;
            };
            let segs: &[vm::dma::Segment] = match &data {
                Data::None => &[],
                Data::Read(d) => d.segments(),
                Data::Write(d) => d.segments(),
            };
            let Some(head) = q.vq.ring().next_head() else {
                refused.push(InFlight { bio, data }.finish());
                continue;
            };
            if segs.len() > self.seg_max || usize::from(q.vq.ring().free()) < segs.len() + 2 {
                refused.push(InFlight { bio, data }.finish());
                continue;
            }
            let at = usize::from(head) * SLOT;
            let kind = match bio.op {
                BioOp::Read => T_IN,
                BioOp::Write => T_OUT,
                BioOp::Flush => T_FLUSH,
            };
            q.headers.write_at::<u32>(at, kind);
            q.headers.write_at::<u32>(at + 4, 0);
            q.headers.write_at::<u64>(at + 8, bio.offset / SECTOR);
            q.headers.write(at + 16, 0xff);
            let header = (q.headers.bus_addr() + at as u64, 16u32);
            let status = (q.headers.bus_addr() + at as u64 + 16, 1u32);
            // Segments are at most the transfer size, far below 4 GiB.
            let data_segs = segs
                .iter()
                .map(|g| (g.bus, u32::try_from(g.len).unwrap_or(u32::MAX)));
            let (out, inp): (Buffers, Buffers) = match bio.op {
                BioOp::Write => (
                    core::iter::once(header).chain(data_segs).collect(),
                    alloc::vec![status],
                ),
                _ => (
                    alloc::vec![header],
                    data_segs.chain(core::iter::once(status)).collect(),
                ),
            };
            match q.vq.push(&out, &inp) {
                Ok(h) => {
                    q.slots[usize::from(h)] = Some(InFlight { bio, data });
                    started += 1;
                }
                Err(_) => refused.push(InFlight { bio, data }.finish()),
            }
        }
        if started > 0 {
            q.vq.kick();
        }
        *bios = refused;
    }
}

/// A `vtblkN` instance.
#[derive(Debug)]
pub struct VtBlk {
    dev: Arc<VirtioDevice>,
    shared: Arc<Shared>,
    info: DiskInfo,
    line: SpinLock<Option<IrqLine>>,
    /// The request queue's interrupts, for the filter.
    intr: virtio::InterruptMask,
}

fn filter(d: &VtBlk, _: usize, _: &IrqCx) -> Filter {
    if d.dev.shared_line() && d.dev.isr() == 0 {
        return Filter::Stray;
    }
    // Off until the drain catches up (as Linux's virtio callbacks do):
    // under load the device would interrupt for each completion it posts
    // before the drain runs.
    d.intr.suppress();
    Filter::Schedule
}

fn completions(d: &VtBlk, _: usize, s: &Sleepable) {
    for (bio, r) in d.shared.reap(s) {
        bio.complete(r);
    }
    let disk = d.shared.disk.lock().clone();
    if let Some(disk) = disk {
        disk.resume(s, 0);
    }
}

impl Driver for VtBlk {
    const NAME: &'static str = "vtblk";
    const IDS: Ids = Ids::Virtio(&[VirtioId::new(device_type::BLOCK)]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(_: &Ident) -> String {
        "VirtIO Block Adapter".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let dev = cx
            .bus_data_arc::<VirtioDevice>()
            .ok_or(AttachError::Decline)?;
        let features = dev
            .negotiate(
                F_SIZE_MAX
                    | F_SEG_MAX
                    | F_RO
                    | F_BLK_SIZE
                    | F_FLUSH
                    // Notifications and interrupts only when the other side
                    // asks (as FreeBSD's vtblk does): far fewer VM exits.
                    | virtio::feature::RING_EVENT_IDX,
            )
            .map_err(|_| AttachError::Device("feature negotiation failed"))?;
        let built = (|| {
            let sectors = dev.config64(CFG_CAPACITY);
            let block = if features & F_BLK_SIZE != 0 {
                dev.config32(CFG_BLK_SIZE)
            } else {
                512
            };
            if !block.is_power_of_two() || !(512..=65536).contains(&block) {
                return Err(AttachError::Device("bad block size"));
            }
            let size_max = if features & F_SIZE_MAX != 0 {
                u64::from(dev.config32(CFG_SIZE_MAX)).max(4096)
            } else {
                u64::MAX
            };
            let tag = cx.dma_tag(DmaConstraints {
                addr_bits: if dev.is_modern() { 64 } else { 44 },
                max_segment: size_max,
                ..DmaConstraints::UNLIMITED
            })?;
            let vq = dev
                .queue(&tag, 0, QUEUE)
                .map_err(|_| AttachError::Device("request queue"))?;
            let size = usize::from(vq.ring().size());
            let intr = vq.interrupt_mask();
            // Without SEG_MAX the device takes chains as long as the ring.
            let seg_max = if features & F_SEG_MAX != 0 {
                (dev.config32(CFG_SEG_MAX) as usize).clamp(1, size.saturating_sub(2).max(1))
            } else {
                size.saturating_sub(2).max(1)
            };
            let headers = tag
                .coherent::<u8>(size * SLOT)
                .map_err(|_| AttachError::NoMemory)?;
            let line = dev.queue_irq(0).map(|i| cx.irq(i)).transpose()?;
            let info = DiskInfo {
                name: "vtbd",
                unit: cx.unit(),
                sector_size: block,
                sectors: sectors * SECTOR / u64::from(block),
                max_transfer: MAX_TRANSFER,
                read_only: features & F_RO != 0,
            };
            let shared = Shared {
                queue: Mutex::new(
                    &QUEUE_LOCK,
                    Queue {
                        vq,
                        headers,
                        slots: (0..size).map(|_| None).collect(),
                    },
                ),
                tag,
                flush: features & F_FLUSH != 0,
                seg_max,
                disk: SpinLock::new(None),
            };
            Ok((shared, info, line, intr))
        })();
        let (shared, info, line, intr) = built.inspect_err(|_| dev.reset())?;
        if line.is_none() {
            dev.reset();
            return Err(AttachError::Device("no interrupt"));
        }
        Ok(Self {
            dev,
            shared: Arc::new(shared),
            info,
            line: SpinLock::new(line),
            intr,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        // Taken in its own statement: installing may sleep.
        let line = self.line.lock().take();
        if let Some(line) = line {
            cx.interrupt(line, Some(filter), Some(completions), 0)?;
        }
        self.dev.ready();
        let disk = storage::publish(cx.sleepable(), self.info, Arc::clone(&self.shared) as _);
        *self.shared.disk.lock() = Some(disk);
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        // Taken in its own statement: no spinlock held while withdrawing.
        let disk = self.shared.disk.lock().take();
        if let Some(disk) = disk {
            storage::withdraw(s, &disk);
        }
        self.dev.reset();
        // The device let go of everything: fail what it held.
        let held: Vec<InFlight> = self
            .shared
            .queue
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

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<VtBlk>();
