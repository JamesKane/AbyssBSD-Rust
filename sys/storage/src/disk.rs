// SPDX-License-Identifier: BSD-2-Clause
//! Disks: what drivers publish, and the GPT partitions found on them.
//!
//! A driver publishes a [`DiskInfo`] and its [`DiskQueues`]. Requests to
//! the disk are checked against its size and sector alignment, then
//! started on the hardware queue for the submitting CPU; whatever does
//! not fit waits on that queue's deferred list, in order, until the
//! driver reports room ([`Disk::resume`]). Withdrawing the disk revokes
//! its gate (waiting for calls inside the driver), fails what is still
//! deferred with [`BioError::Gone`], and removes the disk and its
//! partitions.
//!
//! On publication the disk is tasted for a GPT; each partition becomes a
//! disk of its own (`vtbd0p1`), forwarding to its parent at its offset.
//!
//! Behavior reference: sys/geom/geom_disk.c, sys/geom/part/g_part_gpt.c.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::Gate;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;

use crate::bio::{Bio, BioError, BioOp, Waiter};

/// What a disk driver implements: hardware queues that take batches.
pub trait DiskQueues: Send + Sync {
    /// Hardware queues; a CPU submits on queue `cpu % queues`.
    fn queues(&self) -> usize;

    /// Starts as many of `bios` as fit on hardware queue `q`, in order,
    /// removing them from the vector. The rest stay, and the class
    /// submits them again when the driver calls [`Disk::resume`].
    fn submit(&self, s: &Sleepable, q: usize, bios: &mut Vec<Bio>);
}

/// A disk as its driver describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskInfo {
    /// FreeBSD's disk name: `vtbd`, `nda`, `ada`.
    pub name: &'static str,
    /// Its unit number.
    pub unit: u32,
    /// Logical sector size in bytes.
    pub sector_size: u32,
    /// Size in sectors.
    pub sectors: u64,
    /// The largest transfer one request may make, in bytes.
    pub max_transfer: usize,
    /// Writes are refused.
    pub read_only: bool,
}

/// Above the binding serializer, below drivers' own queue locks.
static DEFERRED: LockClass = LockClass::new("disk deferred", 450);

enum Target {
    Device {
        /// The driver's queues, dropped at withdrawal (once the gate has
        /// drained) so a disk that outlives its driver holds none of the
        /// driver's grants.
        queues: SpinLock<Option<Arc<dyn DiskQueues>>>,
        gate: alloc::boxed::Box<Gate>,
        deferred: Vec<Mutex<Vec<Bio>>>,
    },
    Partition {
        parent: Arc<Disk>,
        /// Byte offset on the parent.
        first: u64,
        entry: PartitionEntry,
    },
}

/// What a partition's GPT entry says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionEntry {
    /// Its index in the table (`p1` is index 0).
    pub index: u32,
    /// Its type.
    pub type_guid: gpt::Guid,
    /// Its first sector on the parent.
    pub first_lba: u64,
    /// Its name.
    pub label: String,
}

impl PartitionEntry {
    /// The type's `gpart` name (`efi`, `freebsd-ufs`, …), or its GUID.
    #[must_use]
    pub fn type_name(&self) -> String {
        match self.type_guid {
            g if g == gpt::types::EFI_SYSTEM => "efi".into(),
            g if g == gpt::types::FREEBSD_UFS => "freebsd-ufs".into(),
            g if g == gpt::types::FREEBSD_SWAP => "freebsd-swap".into(),
            g => format!("{g}"),
        }
    }
}

/// A published disk or partition.
pub struct Disk {
    name: String,
    sector_size: u32,
    sectors: u64,
    max_transfer: usize,
    read_only: bool,
    target: Target,
}

impl core::fmt::Debug for Disk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Disk")
            .field("name", &self.name)
            .field("sectors", &self.sectors)
            .finish_non_exhaustive()
    }
}

/// The published disks and partitions.
static DISKS: SpinLock<Vec<Arc<Disk>>> = SpinLock::new(Vec::new());

/// Units handed out by [`alloc_unit`]: (name, unit).
static UNITS: SpinLock<Vec<(&'static str, u32)>> = SpinLock::new(Vec::new());

/// The lowest free unit for disks named `name` (`nda`, `ada`), for disks
/// whose unit is not their driver's (as CAM numbers its peripherals).
/// Freed when the disk is withdrawn.
#[must_use]
pub fn alloc_unit(name: &'static str) -> u32 {
    let mut units = UNITS.lock();
    let unit = (0..)
        .find(|u| !units.iter().any(|&(n, v)| n == name && v == *u))
        .unwrap_or(0);
    units.push((name, unit));
    unit
}

impl Disk {
    /// Its name (`vtbd0`, `vtbd0p1`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Logical sector size in bytes.
    #[must_use]
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    /// Size in sectors.
    #[must_use]
    pub fn sectors(&self) -> u64 {
        self.sectors
    }

    /// Size in bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.sectors * u64::from(self.sector_size)
    }

    /// The largest transfer one request may make, in bytes.
    #[must_use]
    pub fn max_transfer(&self) -> usize {
        self.max_transfer
    }

    /// For a partition: its parent and GPT entry.
    #[must_use]
    pub fn partition(&self) -> Option<(&Arc<Disk>, &PartitionEntry)> {
        match &self.target {
            Target::Partition { parent, entry, .. } => Some((parent, entry)),
            Target::Device { .. } => None,
        }
    }

    fn check(&self, bio: &Bio) -> Result<(), BioError> {
        if bio.op == BioOp::Flush {
            return Ok(());
        }
        if bio.op == BioOp::Write && self.read_only {
            return Err(BioError::ReadOnly);
        }
        let ss = u64::from(self.sector_size);
        let len = bio.len() as u64;
        let fits = bio
            .offset
            .checked_add(len)
            .is_some_and(|end| end <= self.bytes());
        if !fits || !bio.offset.is_multiple_of(ss) || !len.is_multiple_of(ss) || len == 0 {
            return Err(BioError::Invalid);
        }
        if bio.len() > self.max_transfer {
            return Err(BioError::Invalid);
        }
        Ok(())
    }

    /// Sends `bio` to the disk (FreeBSD's `g_io_request`).
    pub fn strategy(&self, s: &Sleepable, mut bio: Bio) {
        if let Err(e) = self.check(&bio) {
            bio.complete(Err(e));
            return;
        }
        match &self.target {
            Target::Partition { parent, first, .. } => {
                bio.offset += first;
                parent.strategy(s, bio);
            }
            Target::Device {
                queues,
                gate,
                deferred,
            } => {
                let Ok(_inside) = gate.enter() else {
                    bio.complete(Err(BioError::Gone));
                    return;
                };
                // Inside the gate the queues are there: withdrawal drops
                // them only after draining it. (Cloned in its own
                // statement: no spinlock across `submit`.)
                let queues = queues.lock().clone();
                let Some(queues) = queues else {
                    bio.complete(Err(BioError::Gone));
                    return;
                };
                let q = sched::this_cpu().id().index() % deferred.len();
                let mut waiting = deferred[q].lock(s);
                // Behind anything already waiting, to keep order.
                waiting.push(bio);
                if waiting.len() == 1 {
                    queues.submit(s, q, &mut waiting);
                }
            }
        }
    }

    /// Resubmits what waits on queue `q`. The driver calls it when a
    /// completion has made room, with none of its own locks held.
    pub fn resume(&self, s: &Sleepable, q: usize) {
        let Target::Device {
            queues,
            gate,
            deferred,
        } = &self.target
        else {
            return;
        };
        let Ok(_inside) = gate.enter() else { return };
        let Some(list) = deferred.get(q) else { return };
        // Cloned in its own statement: the spinlock must not be held
        // across `submit`, which sleeps on the driver's lock.
        let queues = queues.lock().clone();
        let Some(queues) = queues else { return };
        let mut waiting = list.lock(s);
        if !waiting.is_empty() {
            queues.submit(s, q, &mut waiting);
        }
    }

    fn sync(
        &self,
        s: &Sleepable,
        op: BioOp,
        offset: u64,
        buf: Vec<u8>,
    ) -> (Vec<u8>, Result<(), BioError>) {
        let (bio, waiter) = Waiter::bio(op, offset, buf);
        self.strategy(s, bio);
        waiter.wait(s)
    }

    /// Reads `len` bytes at `offset`, in requests of at most the disk's
    /// largest transfer.
    pub fn read(&self, s: &Sleepable, offset: u64, len: usize) -> Result<Vec<u8>, BioError> {
        let mut out = Vec::with_capacity(len);
        let chunk = self.max_transfer.max(self.sector_size as usize);
        while out.len() < len {
            let n = chunk.min(len - out.len());
            let (buf, r) = self.sync(s, BioOp::Read, offset + out.len() as u64, alloc::vec![0; n]);
            r?;
            out.extend_from_slice(&buf);
        }
        Ok(out)
    }

    /// Writes `data` at `offset`, in requests of at most the disk's largest
    /// transfer.
    pub fn write(&self, s: &Sleepable, offset: u64, data: &[u8]) -> Result<(), BioError> {
        let chunk = self.max_transfer.max(self.sector_size as usize);
        for (i, piece) in data.chunks(chunk).enumerate() {
            let at = offset + (i * chunk) as u64;
            self.sync(s, BioOp::Write, at, piece.to_vec()).1?;
        }
        Ok(())
    }

    /// Makes earlier writes durable.
    pub fn flush(&self, s: &Sleepable) -> Result<(), BioError> {
        self.sync(s, BioOp::Flush, 0, Vec::new()).1
    }
}

/// FreeBSD's announcement size: MB under 10 GB, else GB.
fn size_text(bytes: u64) -> String {
    let mb = bytes >> 20;
    if mb >= 10 * 1024 {
        format!("{}GB", mb >> 10)
    } else {
        format!("{mb}MB")
    }
}

/// The GPT partitions of `disk`, as disks of their own.
fn taste(s: &Sleepable, disk: &Arc<Disk>) -> Vec<Arc<Disk>> {
    if disk.sector_size as usize != gpt::SECTOR {
        return Vec::new();
    }
    let ss = u64::from(disk.sector_size);
    let Ok(sector) = disk.read(s, ss, gpt::SECTOR) else {
        return Vec::new();
    };
    let Ok(h) = gpt::PrimaryHeader::read(&sector) else {
        return Vec::new();
    };
    let len = h.entries_len.next_multiple_of(gpt::SECTOR);
    let Some(at) = h.entries_lba.checked_mul(ss) else {
        return Vec::new();
    };
    let Ok(entries) = disk.read(s, at, len) else {
        return Vec::new();
    };
    let Ok(table) = gpt::Table::from_parts(&h, &entries) else {
        return Vec::new();
    };
    table
        .partitions()
        .filter(|p| p.first_lba <= p.last_lba && p.last_lba < disk.sectors)
        .map(|p| {
            let mut label = String::new();
            let _ = p.write_name(&mut label);
            Arc::new(Disk {
                name: format!("{}p{}", disk.name, p.index + 1),
                sector_size: disk.sector_size,
                sectors: p.sectors(),
                max_transfer: disk.max_transfer,
                read_only: disk.read_only,
                target: Target::Partition {
                    parent: Arc::clone(disk),
                    first: p.first_lba * ss,
                    entry: PartitionEntry {
                        index: p.index,
                        type_guid: p.type_guid,
                        first_lba: p.first_lba,
                        label,
                    },
                },
            })
        })
        .collect()
}

/// What happened to a disk or partition, for whoever names them in `/dev`.
#[derive(Debug)]
pub enum Event<'a> {
    Published(&'a Arc<Disk>),
    Withdrawn(&'a Arc<Disk>),
}

static LISTENER: sync::Once<fn(&Sleepable, Event<'_>)> = sync::Once::new();

/// Sets the function told of every disk and partition published or
/// withdrawn from now on (devfs's). Disks published before are in
/// [`all`].
pub fn set_listener(f: fn(&Sleepable, Event<'_>)) {
    let _ = LISTENER.set(f);
}

fn tell(s: &Sleepable, e: Event<'_>) {
    if let Some(f) = LISTENER.get() {
        f(s, e);
    }
}

/// Every published disk and partition.
#[must_use]
pub fn all() -> Vec<Arc<Disk>> {
    DISKS.lock().clone()
}

/// Publishes a disk, announces it as FreeBSD does, and publishes its GPT
/// partitions. Call from the driver's `start`, ready for I/O.
pub fn publish(s: &Sleepable, info: DiskInfo, queues: Arc<dyn DiskQueues>) -> Arc<Disk> {
    let n = queues.queues().max(1);
    let queues = SpinLock::new(Some(queues));
    let disk = Arc::new(Disk {
        name: format!("{}{}", info.name, info.unit),
        sector_size: info.sector_size,
        sectors: info.sectors,
        max_transfer: info.max_transfer,
        read_only: info.read_only,
        target: Target::Device {
            queues,
            gate: alloc::boxed::Box::new(Gate::new()),
            deferred: (0..n).map(|_| Mutex::new(&DEFERRED, Vec::new())).collect(),
        },
    });
    bus::manager().log(format_args!(
        "{}: {} ({} {} byte sectors)",
        disk.name,
        size_text(disk.bytes()),
        disk.sectors,
        disk.sector_size
    ));
    DISKS.lock().push(Arc::clone(&disk));
    tell(s, Event::Published(&disk));
    let parts = taste(s, &disk);
    DISKS.lock().extend(parts.iter().cloned());
    for p in &parts {
        tell(s, Event::Published(p));
    }
    disk
}

/// Withdraws a disk and its partitions: new requests fail with
/// [`BioError::Gone`], calls inside the driver finish first, and deferred
/// requests are failed. Call from the driver's `stop`, before it resets
/// the hardware (which must then fail what the hardware still holds).
pub fn withdraw(s: &Sleepable, disk: &Arc<Disk>) {
    if let Some(i) = disk.name.find(|c: char| c.is_ascii_digit())
        && let Ok(unit) = disk.name[i..].parse::<u32>()
    {
        let base = &disk.name[..i];
        UNITS.lock().retain(|&(n, u)| !(n == base && u == unit));
    }
    let mut gone = Vec::new();
    DISKS.lock().retain(|d| {
        let keep = !Arc::ptr_eq(d, disk)
            && !matches!(&d.target, Target::Partition { parent, .. } if Arc::ptr_eq(parent, disk));
        if !keep {
            gone.push(Arc::clone(d));
        }
        keep
    });
    for d in &gone {
        tell(s, Event::Withdrawn(d));
    }
    if let Target::Device {
        gate,
        deferred,
        queues,
    } = &disk.target
    {
        gate.revoke(s);
        for list in deferred {
            for bio in core::mem::take(&mut *list.lock(s)) {
                bio.complete(Err(BioError::Gone));
            }
        }
        // Nothing can reach the driver now; let go of its state.
        queues.lock().take();
    }
}

/// The partitions found on `disk`, in table order.
#[must_use]
pub fn partitions(disk: &Arc<Disk>) -> Vec<Arc<Disk>> {
    let mut out: Vec<Arc<Disk>> = DISKS
        .lock()
        .iter()
        .filter(|d| d.partition().is_some_and(|(p, _)| Arc::ptr_eq(p, disk)))
        .cloned()
        .collect();
    out.sort_by_key(|d| d.partition().map(|(_, e)| e.index));
    out
}

/// The disk or partition named `name`.
#[must_use]
pub fn find(name: &str) -> Option<Arc<Disk>> {
    DISKS.lock().iter().find(|d| d.name == name).cloned()
}

/// Every published disk and partition, by name.
#[must_use]
pub fn disks() -> Vec<String> {
    DISKS.lock().iter().map(|d| d.name.clone()).collect()
}
