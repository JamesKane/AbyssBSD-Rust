// SPDX-License-Identifier: BSD-2-Clause
//! GEOM's userland view of the disks: `kern.geom.confxml` (which libgeom
//! reads for `gpart`, `geom`, and `glabel`), `kern.geom.conftxt`,
//! `kern.geom.confdot`, and `kern.disks`, made from the disks and GPT
//! partitions `sys/storage` publishes, the memory disks, the mounts, and
//! the opens of disk nodes.
//!
//! The mesh ([`geom::Mesh`]) is made afresh for each read, in the order
//! FreeBSD would have made it: each disk as it was published (its DISK or
//! MD geom, its partition table when it was tasted, and its DEV geom),
//! each partition's LABEL geoms (`gpt/`, then `gptid/`) and DEV geom, the
//! labels' DEV geoms, and the mounts' VFS geoms. Ids are synthetic,
//! unique within a read and stable while the objects last.
//!
//! Opens of disk nodes are counted here, as GEOM's DEV class counts them
//! (`g_dev_open`), and passed down the mesh. As `g_access` does, the
//! first write open of a whole disk spoils its partition table (unless a
//! partition is in use) and the last write close has it tasted again,
//! on an event thread (`g_event`); the sysctls wait for the events
//! posted before them (`g_waitfor_event`), so what a writer leaves is
//! what the next read shows. A partition opened or mounted for writing
//! hides its other names (the LABEL geoms GEOM withers then). As
//! `g_access` refuses it, a node is not opened for writing while
//! something else holds its provider exclusively (`EPERM`): a mounted
//! file system, a partition of the disk in use, uncommitted changes to
//! its table; `kern.geom.debugflags` 0x10 (`G_F_FOOTSHOOTING`) lets a
//! whole disk (rank 1) be written anyway.
//!
//! Control requests (`/dev/geom.ctl`, [`crate::geom_ctl`]) run on the
//! event thread too ([`run`]). Partitions a request makes appear in the
//! mesh when it made them, after what came before: their providers,
//! then their LABEL and DEV geoms, as the tastes of new providers make
//! them; names tasted again after a commit (`g_media_changed`) are LABEL
//! geoms made then.
//!
//! Behavior reference: sys/geom/geom_kern.c, geom_dump.c, geom_disk.c
//! (`g_disk_dumpconf`, `sysctl_disks`), geom_dev.c (`g_dev_open`),
//! geom_vfs.c (`g_vfs_open`), geom_slice.c (`g_slice_dumpconf`),
//! geom_subr.c (`g_access`, `g_spoil`), part/g_part.c
//! (`g_part_dumpconf`, `g_part_new_provider`), part/g_part_gpt.c
//! (`g_part_gpt_dumpconf`), label/g_label.c (`g_label_taste`),
//! sys/dev/md/md.c (`g_md_dumpconf`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use abi::errno::Errno;
use geom::table::Scheme;
use geom::{Access, Field, GeomRef, Kind, Media, Mesh, ProviderRef};
use sched::{Sleepable, WaitQueue};
use storage::{Disk, Rotation};
use sync::SpinLock;
use vfs::{CharDevice, Uio};

/// The classes FreeBSD 15.1's GENERIC kernel has (with no modules
/// loaded), in the order it loads them, so `confxml` lists them newest
/// first as FreeBSD's does. Those AbyssBSD has no geoms of stay empty.
#[cfg(target_arch = "x86_64")]
const CLASSES: &[&str] = &[
    "MD", "DEV", "DISK", "Flashmap", "VFS", "LABEL", "PART", "SWAP", "RAID", "FD",
];
/// The classes FreeBSD 15.1's GENERIC kernel has (with no modules
/// loaded), in the order it loads them, so `confxml` lists them newest
/// first as FreeBSD's does. Those AbyssBSD has no geoms of stay empty.
#[cfg(not(target_arch = "x86_64"))]
const CLASSES: &[&str] = &[
    "MD", "DEV", "DISK", "Flashmap", "VFS", "LABEL", "PART", "SWAP", "RAID",
];

/// The class named `name`, if the kernel has it.
pub(crate) fn class_named(name: &[u8]) -> Option<&'static str> {
    CLASSES.iter().copied().find(|c| c.as_bytes() == name)
}

/// Where the synthetic ids start: classes, then everything else.
const CLASS_IDS: u64 = 0xffff_ffff_8190_0000;
const OBJECT_IDS: u64 = 0xffff_f800_0000_0000;
const MOUNT_IDS: u64 = 0xffff_fc00_0000_0000;

/// The objects of one disk or partition, each an id slot: its own geom
/// and provider, its PART geom and consumer, its DEV geom (and consumer,
/// the next slot), and from [`LABEL`] its LABEL geoms', five slots each
/// (`gpt` first): geom, consumer, provider, DEV geom, DEV consumer.
const GEOM: u64 = 0;
const PROVIDER: u64 = 1;
const PART_GEOM: u64 = 2;
const PART_CONSUMER: u64 = 3;
const DEV: u64 = 4;
const LABEL: u64 = 6;

/// The id of slot `slot` of disk or partition `serial`.
fn id(serial: u64, slot: u64) -> u64 {
    OBJECT_IDS | (serial << 8) | (slot << 3)
}

/// The id of the provider of the disk or partition with serial `serial`,
/// which its statistics carry (`devstat`'s `id`), so that `gstat` finds
/// it in `confxml`.
pub(crate) fn provider_id(serial: u64) -> u64 {
    id(serial, PROVIDER)
}

// --- The event thread ---------------------------------------------------

pub(crate) type Event = Box<dyn FnOnce(&Sleepable) + Send>;

struct Events {
    queue: VecDeque<Event>,
    posted: u64,
    done: u64,
}

static EVENTS: SpinLock<Events> = SpinLock::new(Events {
    queue: VecDeque::new(),
    posted: 0,
    done: 0,
});
static WORK: WaitQueue = WaitQueue::new();
static DONE: WaitQueue = WaitQueue::new();
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Runs `e` on the event thread, after what was posted before.
fn post(e: Event) {
    // Acquire: pairs with the release once the thread is up.
    if !RUNNING.load(Ordering::Acquire) {
        kprintln!("geom: no event thread; event dropped");
        return;
    }
    {
        let mut q = EVENTS.lock();
        q.queue.push_back(e);
        q.posted += 1;
    }
    WORK.wake_one();
}

/// Runs `e` on the event thread and waits for it (`g_waitfor_event`).
pub(crate) fn run(s: &Sleepable, e: Event) {
    post(e);
    drain(s);
}

/// `kern.geom.debugflags` (`g_debugflags`).
static DEBUGFLAGS: AtomicI32 = AtomicI32::new(0);

/// `kern.geom.debugflags`.
pub(crate) fn debugflags() -> u32 {
    // A setting; nothing is ordered after it.
    u32::from_ne_bytes(DEBUGFLAGS.load(Ordering::Relaxed).to_ne_bytes())
}

/// Whether opens and resizes that would shoot one's foot are allowed
/// (`G_F_FOOTSHOOTING`).
fn footshooting() -> bool {
    debugflags() & geom::table::G_F_FOOTSHOOTING != 0
}

/// Waits until the events posted so far have run.
fn drain(s: &Sleepable) {
    let want = EVENTS.lock().posted;
    DONE.wait_until(s, || EVENTS.lock().done >= want);
}

fn run_events(s: &Sleepable) {
    loop {
        WORK.wait_until(s, || !EVENTS.lock().queue.is_empty());
        let next = EVENTS.lock().queue.pop_front();
        if let Some(e) = next {
            e(s);
            EVENTS.lock().done += 1;
            DONE.wake_all();
        }
    }
}

// --- Opens of disk nodes ------------------------------------------------

/// An open of a disk node: its disk's serial and the node's name.
struct Open {
    id: u64,
    serial: u64,
    name: String,
    write: bool,
}

static OPENS: SpinLock<Vec<Open>> = SpinLock::new(Vec::new());
static NEXT_OPEN: AtomicU64 = AtomicU64::new(1);

/// The mounts of disks: the file system's GEOM name (`ffs`), what it was
/// mounted from (a provider's name), whether for writing, and its id.
fn mounts() -> Vec<(&'static str, String, bool, u64)> {
    crate::fs::namespace()
        .mounts()
        .iter()
        .filter_map(|m| {
            let fs = match m.type_name() {
                // `g_vfs_open(devvp, &cp, "ffs", ...)`.
                "ufs" => "ffs",
                "msdosfs" => "msdosfs",
                _ => return None,
            };
            let from = m.info().from.strip_prefix(b"/dev/")?;
            let from = core::str::from_utf8(from).ok()?;
            Some((fs, String::from(from), !m.read_only(), m.id))
        })
        .collect()
}

/// Whether `disk` or a partition of it is open or mounted, or its table
/// has changes to commit (md's `opencount`).
pub(crate) fn in_use(disk: &Arc<Disk>) -> bool {
    busy(disk)
}

/// `disk` and the partitions under it, however deep (a BSD label's in
/// an MBR slice).
fn subtree(disk: &Arc<Disk>) -> Vec<Arc<Disk>> {
    let mut out = vec![Arc::clone(disk)];
    let mut i = 0;
    while i < out.len() {
        let next = storage::partitions(&out[i]);
        out.extend(next);
        i += 1;
    }
    out
}

/// Whether `disk` or anything under it is open or mounted, under any of
/// its names, or holds a table with changes to commit (its provider's
/// access counts are not all zero).
pub(crate) fn busy(disk: &Arc<Disk>) -> bool {
    let tree = subtree(disk);
    let serials: Vec<u64> = tree.iter().map(|d| d.serial()).collect();
    tree.iter()
        .any(|d| d.table().is_some_and(|t| t.part.opened))
        || OPENS.lock().iter().any(|o| serials.contains(&o.serial))
        || mounts().iter().any(|(_, from, _, _)| {
            storage::find(from).is_some_and(|d| serials.contains(&d.serial()))
        })
}

/// `g_access(cp, 1, 1, 1)` of a table's consumer on whole disk `disk`:
/// refused (`EPERM`) while another writes the disk (an open of its node
/// for writing, a read-write mount) or holds it exclusively (a mount),
/// unless the foot is to be shot (a disk is of rank 1).
pub(crate) fn table_access(disk: &Arc<Disk>) -> Result<(), Errno> {
    if footshooting() {
        return Ok(());
    }
    let serial = disk.serial();
    let written = OPENS.lock().iter().any(|o| o.serial == serial && o.write);
    let mounted = mounts()
        .iter()
        .any(|(_, from, _, _)| storage::find(from).is_some_and(|d| d.serial() == serial));
    if written || mounted {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// Whether a partition of `disk` (or one under it) is in use.
fn partitions_in_use(disk: &Arc<Disk>) -> bool {
    storage::partitions(disk).iter().any(busy)
}

/// An open disk node: its I/O goes to the node's device; closing it
/// ends the open's count.
struct Opened {
    dev: Arc<dyn CharDevice>,
    id: u64,
    disk: Weak<Disk>,
    serial: u64,
    /// A write open of a whole disk, which retastes it when the last
    /// one closes.
    rewrites: bool,
    /// A write open of a partition, whose names are tasted again when
    /// the last one closes.
    relabels: bool,
}

/// Counts an open of disk node `name` (of `disk`), for writing if
/// `write`; the returned device serves the open file's I/O through `dev`
/// until it closes. The first write open of a whole disk spoils its
/// partition table, unless a partition is in use. `EPERM` for writing a
/// provider another holds exclusively (`g_access`), but a whole disk with
/// `kern.geom.debugflags` 0x10.
pub(crate) fn open(
    s: &Sleepable,
    dev: Arc<dyn CharDevice>,
    disk: &Arc<Disk>,
    name: &str,
    write: bool,
) -> Result<Arc<dyn CharDevice>, Errno> {
    let whole = disk.partition_index().is_none();
    if write {
        let node = String::from(name);
        let held = with_mesh(s, move |m| {
            m.provider(&node).is_some_and(|p| {
                m.provider_access(p).e > 0 && !(footshooting() && m.provider_rank(p) == 1)
            })
        });
        if held {
            return Err(Errno::EPERM);
        }
    }
    let id = NEXT_OPEN.fetch_add(1, Ordering::Relaxed);
    let first_writer = {
        let mut opens = OPENS.lock();
        let first = write && !opens.iter().any(|o| o.serial == disk.serial() && o.write);
        opens.push(Open {
            id,
            serial: disk.serial(),
            name: String::from(name),
            write,
        });
        first
    };
    // A partition's own table (a label in a slice) goes too.
    if first_writer && disk.table().is_some() && !partitions_in_use(disk) {
        storage::spoil(s, disk);
    }
    Ok(Arc::new(Opened {
        dev,
        id,
        disk: Arc::downgrade(disk),
        serial: disk.serial(),
        rewrites: write,
        relabels: write && !whole,
    }))
}

impl Drop for Opened {
    fn drop(&mut self) {
        let last_writer = {
            let mut opens = OPENS.lock();
            opens.retain(|o| o.id != self.id);
            !opens.iter().any(|o| o.serial == self.serial && o.write)
        };
        if self.rewrites && last_writer {
            let disk = Weak::clone(&self.disk);
            let relabels = self.relabels;
            post(Box::new(move |s| {
                if let Some(d) = disk.upgrade() {
                    storage::retaste(s, &d);
                    if relabels {
                        storage::relabel(s, &d);
                    }
                }
            }));
        }
    }
}

impl CharDevice for Opened {
    fn dtype(&self) -> u32 {
        self.dev.dtype()
    }

    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        offset: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        self.dev.read(s, uio, offset, nonblock)
    }

    fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        offset: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        self.dev.write(s, uio, offset, nonblock)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        self.dev.ioctl(s, req, arg)
    }

    fn ioctl_value(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
        self.dev.ioctl_value(s, req, arg)
    }

    fn ioctl_file(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &mut [u8],
        fflag: u32,
    ) -> Result<usize, Errno> {
        self.dev.ioctl_file(s, req, arg, fflag)
    }
}

// --- The mesh -----------------------------------------------------------

/// What builds the mesh reads: the opens by node, and the mounts.
struct Holders {
    opens: Vec<(String, bool)>,
    mounts: Vec<(&'static str, String, bool, u64)>,
}

impl Holders {
    /// The DEV class's counts for node `name`: a reader per open, a
    /// writer per open for writing.
    fn dev(&self, name: &str) -> Access {
        self.opens
            .iter()
            .filter(|(n, _)| n == name)
            .fold(Access::NONE, |a, &(_, w)| {
                Access::new(a.r + 1, a.w + u32::from(w), a.e)
            })
    }

    /// Whether provider `name` is open or mounted for writing.
    fn writes(&self, name: &str) -> bool {
        self.opens.iter().any(|(n, w)| *w && n == name)
            || self.mounts.iter().any(|(_, n, w, _)| *w && n == name)
    }
}

/// `d_rotation_rate` as `g_disk_dumpconf` shows it.
fn rotation(r: Rotation) -> String {
    match r {
        Rotation::Unknown => String::from("unknown"),
        Rotation::NonRotating => String::from("0"),
        Rotation::Rpm(n) if (0x401..=0xfffe).contains(&n) => format!("{n}"),
        Rotation::Rpm(_) => String::from("invalid"),
    }
}

struct Builder<'a> {
    m: Mesh,
    held: &'a Holders,
    /// The PART geoms made, by their disk's serial.
    tables: Vec<(u64, GeomRef)>,
    /// Providers made whose tastes are still to come, oldest first, as
    /// GEOM's event queue holds them (`g_new_provider_event`): drained
    /// in a loop, so no nesting of tables deepens the stack.
    tastes: VecDeque<Vec<(Arc<Disk>, ProviderRef, Media)>>,
}

/// A label provider made, for its DEV geom: its partition's serial, its
/// first id slot, its name, and its provider.
type Made = (u64, u64, String, ProviderRef);

/// A disk's firmware geometry (a memory disk's own): heads, sectors.
fn firmware(d: &Arc<Disk>) -> (u32, u32) {
    crate::mdisk::info(d).map_or_else(
        || {
            let desc = d.description();
            (desc.fwheads, desc.fwsectors)
        },
        |md| (md.fwheads, md.fwsectors),
    )
}

/// What a disk's or partition's provider answers for `GEOM::fwheads`
/// and `GEOM::fwsectors`: a disk's firmware geometry, a partition's
/// table's (`g_part_start`), which is the disk's when it reports one.
pub(crate) fn fw_geometry(d: &Arc<Disk>) -> (u32, u32) {
    let Some((parent, _)) = d.partition_index() else {
        return firmware(d);
    };
    let fw = fw_geometry(parent);
    match parent.table() {
        Some(t) if fw.0 == 0 || fw.1 == 0 => (t.part.heads, t.part.sectors),
        _ => fw,
    }
}

/// A partition's provider's size and alignment.
fn partition_media(part: &Arc<Disk>, e: &storage::PartitionEntry) -> Media {
    let desc = part.description();
    let offset = e.first_lba * u64::from(part.sector_size());
    let mut stripeoffset = desc.stripeoffset + offset;
    if desc.stripesize > 0 {
        stripeoffset %= desc.stripesize;
    }
    Media {
        mediasize: part.bytes(),
        sectorsize: part.sector_size(),
        stripesize: desc.stripesize,
        stripeoffset,
    }
}

impl Builder<'_> {
    fn class(&self, name: &str) -> geom::ClassRef {
        // Every class used is in `CLASSES`.
        self.m
            .class(name)
            .unwrap_or_else(|| unreachable!("class {name} is loaded"))
    }

    /// The DEV geom of provider `p` (named `name`), with its opens.
    fn dev(&mut self, serial: u64, slot: u64, name: &str, p: ProviderRef) {
        let c = self.class("DEV");
        let g = self.m.add_geom(c, id(serial, slot), name, Kind::Dev);
        let cp = self.m.attach(g, id(serial, slot + 1), p);
        let a = self.held.dev(name);
        self.m.open(cp, a);
    }

    /// A disk as it is published: its DISK or MD geom, its table if it
    /// was found then, and its DEV geom.
    fn disk(&mut self, d: &Arc<Disk>) {
        let desc = d.description();
        let media = Media {
            mediasize: d.bytes(),
            sectorsize: d.sector_size(),
            stripesize: desc.stripesize,
            stripeoffset: desc.stripeoffset,
        };
        let serial = d.serial();
        let (p, fw) = if let Some(md) = crate::mdisk::info(d) {
            let c = self.class("MD");
            let g = self.m.add_geom(c, id(serial, GEOM), d.name(), Kind::Md);
            let p = self
                .m
                .add_provider(g, id(serial, PROVIDER), d.name(), media);
            let access = if md.read_only {
                "read-only"
            } else {
                "read-write"
            };
            let mut config = vec![
                Field::new("unit", md.unit),
                Field::new("sectorsize", d.sector_size()),
                Field::new("fwheads", md.fwheads),
                Field::new("fwsectors", md.fwsectors),
            ];
            // A vnode-backed disk's file and `ident` (`g_md_dumpconf`).
            let file = md
                .file
                .as_ref()
                .map(|(f, ident)| (String::from_utf8_lossy(f).into_owned(), ident.clone()));
            if let Some((_, ident)) = &file {
                config.push(Field::new("ident", ident));
            }
            config.extend([
                Field::new("length", d.bytes()),
                Field::new("compression", if md.compress { "on" } else { "off" }),
                Field::new("access", access),
                Field::new("type", md.kind),
            ]);
            if let Some((f, _)) = &file {
                config.push(Field::new("file", f));
                config.push(Field::new("cache", "off"));
            }
            config.push(Field::new("label", ""));
            let txt = format!(
                " u {} s {} f {} fs {} l {} t {}{} label ",
                md.unit,
                d.sector_size(),
                md.fwheads,
                md.fwsectors,
                d.bytes(),
                md.kind,
                file.as_ref()
                    .map_or_else(String::new, |(f, _)| format!(" file {f}")),
            );
            self.m.set_provider_config(p, config, txt);
            (p, (md.fwheads, md.fwsectors))
        } else {
            let c = self.class("DISK");
            let g = self.m.add_geom(c, id(serial, GEOM), d.name(), Kind::Disk);
            let p = self
                .m
                .add_provider(g, id(serial, PROVIDER), d.name(), media);
            let mut config = vec![
                Field::new("fwheads", desc.fwheads),
                Field::new("fwsectors", desc.fwsectors),
                Field::new("rotationrate", rotation(desc.rotation)),
                Field::new("ident", &desc.ident),
            ];
            if let Some(lunid) = &desc.lunid {
                config.push(Field::new("lunid", lunid));
            }
            config.push(Field::new("descr", &desc.descr));
            let txt = format!(" hd {} sc {}", desc.fwheads, desc.fwsectors);
            self.m.set_provider_config(p, config, txt);
            (p, (desc.fwheads, desc.fwsectors))
        };
        if d.table().is_some_and(|t| t.at_publication) {
            self.table(d, p, fw);
        }
        self.dev(serial, DEV, d.name(), p);
    }

    /// Disk `d`'s partition table (on its provider `dp`, with firmware
    /// geometry `fw`): the PART geom, its consumer (held open while the
    /// table has changes to commit), and the partitions found with it.
    fn table(&mut self, d: &Arc<Disk>, dp: ProviderRef, fw: (u32, u32)) {
        let Some(t) = d.table() else { return };
        let serial = d.serial();
        let c = self.class("PART");
        let g = self
            .m
            .add_geom(c, id(serial, PART_GEOM), d.name(), Kind::Part);
        let part = &t.part;
        // A destroyed table's geometry is none (`g_part_null_scheme`).
        let (heads, sectors) = if part.scheme == Scheme::Null {
            (0, 0)
        } else if fw.0 != 0 && fw.1 != 0 {
            fw
        } else {
            (part.heads, part.sectors)
        };
        self.m.set_geom_config(
            g,
            vec![
                Field::new("scheme", part.scheme.name()),
                Field::new("entries", part.entries),
                Field::new("first", part.first),
                Field::new("last", part.last),
                Field::new("fwsectors", sectors),
                Field::new("fwheads", heads),
                Field::new("state", if part.corrupt { "CORRUPT" } else { "OK" }),
                Field::new("modified", if part.opened { "true" } else { "false" }),
            ],
        );
        let cp = self.m.attach(g, id(serial, PART_CONSUMER), dp);
        if part.opened {
            self.m.open(cp, Access::new(1, 1, 1));
        }
        self.tables.push((serial, g));
        self.batch(d, t.born);
    }

    /// Runs the tastes queued, and those they queue.
    fn settle(&mut self) {
        while let Some(made) = self.tastes.pop_front() {
            self.taste(&made);
        }
    }

    /// The partitions of disk `d` published together in `batch`: their
    /// providers now, as the PART class makes them, and their tastes
    /// queued ([`Self::taste`]).
    fn batch(&mut self, d: &Arc<Disk>, batch: u64) {
        let Some(&(_, g)) = self.tables.iter().find(|(s, _)| *s == d.serial()) else {
            return;
        };
        let mut parts: Vec<Arc<Disk>> = storage::partitions(d)
            .into_iter()
            .filter(|p| p.partition_times().is_some_and(|(b, _)| b == batch))
            .collect();
        parts.sort_unstable_by_key(|p| p.serial());
        let Some(table) = d.table() else { return };
        let mut made = Vec::new();
        for part in &parts {
            let Some((_, e)) = part.partition() else {
                continue;
            };
            let media = partition_media(part, &e);
            let p = self
                .m
                .add_provider(g, id(part.serial(), PROVIDER), part.name(), media);
            let (config, txt) =
                e.raw
                    .provider_config(&table.part, part.sector_size(), part.bytes());
            self.m.set_provider_config(p, config, txt);
            made.push((Arc::clone(part), p, media));
        }
        self.tastes.push_back(made);
    }

    /// The tastes of a batch of new partitions' providers `made`: each
    /// one's own table (whose providers are made then, their tastes
    /// queued), LABEL geoms, and DEV geom, then the labels' DEV geoms.
    fn taste(&mut self, made: &[(Arc<Disk>, ProviderRef, Media)]) {
        let mut labels = Vec::new();
        for (part, p, media) in made {
            // The PART class tastes a new provider first: a table in it.
            if part.table().is_some_and(|t| t.at_publication) {
                self.table(part, *p, firmware(part));
            }
            if part.partition_times().is_some_and(|(b, l)| l == b) {
                labels.extend(self.labels(part, *p, *media));
            }
            self.dev(part.serial(), DEV, part.name(), *p);
        }
        for (s, base, name, lp) in labels {
            self.dev(s, base + 3, &name, lp);
        }
    }

    /// Partition `part`'s LABEL geoms (on its provider `p`), the names
    /// its writers leave: the label providers made.
    fn labels(&mut self, part: &Arc<Disk>, p: ProviderRef, media: Media) -> Vec<Made> {
        let names = part.label_names();
        // Written through its own name: GEOM withered its labels;
        // through one of them: the others.
        let keep: Vec<bool> = if self.held.writes(part.name()) {
            vec![false; names.len()]
        } else if let Some(w) = names.iter().position(|n| self.held.writes(n)) {
            (0..names.len()).map(|i| i == w).collect()
        } else {
            vec![true; names.len()]
        };
        let mut made = Vec::new();
        for (n, (name, keep)) in (0u64..).zip(names.iter().zip(keep)) {
            // A name another provider has already is not made again.
            if !keep || self.m.provider(name).is_some() {
                continue;
            }
            let s = part.serial();
            let c = self.class("LABEL");
            let base = LABEL + 5 * n;
            let lg = self.m.add_geom(c, id(s, base), part.name(), Kind::Label);
            self.m.attach(lg, id(s, base + 1), p);
            let lp = self.m.add_provider(lg, id(s, base + 2), name, media);
            let len = media.mediasize;
            self.m.set_provider_config(
                lp,
                vec![
                    Field::new("index", 0),
                    Field::new("length", len),
                    Field::new("seclength", len / 512),
                    Field::new("offset", 0),
                    Field::new("secoffset", 0),
                ],
                String::from(" i 0 o 0"),
            );
            made.push((s, base, name.clone(), lp));
        }
        made
    }

    /// Partition `part`'s names, tasted again after its provider was
    /// made (`g_media_changed`, or its last writer's close): its LABEL
    /// geoms, then their DEV geoms.
    fn relabel(&mut self, part: &Arc<Disk>) {
        let (Some(p), Some((_, e))) = (self.m.provider(part.name()), part.partition()) else {
            return;
        };
        let media = partition_media(part, &e);
        for (s, base, name, lp) in self.labels(part, p, media) {
            self.dev(s, base + 3, &name, lp);
        }
    }

    /// The mounts' VFS geoms, in mount order.
    fn mounts(&mut self) {
        for (fs, from, write, mount) in &self.held.mounts {
            let Some(p) = self.m.provider(from) else {
                continue;
            };
            let c = self.class("VFS");
            let name = format!("{fs}.{from}");
            let g = self
                .m
                .add_geom(c, MOUNT_IDS | (mount << 8), &name, Kind::Vfs);
            let cp = self.m.attach(g, MOUNT_IDS | (mount << 8) | 8, p);
            // `g_vfs_open`: r1, and w1e1 for writing.
            let w = u32::from(*write);
            self.m.open(cp, Access::new(1, w, w));
        }
    }
}

/// What made part of the mesh, in the order it happened.
enum Happened<'a> {
    /// A disk was published.
    Disk(&'a Arc<Disk>),
    /// A table was found or made on it after its publication.
    Table(&'a Arc<Disk>),
    /// Partitions of it were published (a control request).
    Batch(&'a Arc<Disk>, u64),
    /// A partition's names were tasted again.
    Relabel(Arc<Disk>),
}

/// `f` of the mesh now, once the events posted so far have run. The mesh
/// is built on the event thread, whose stack is GEOM's (as FreeBSD's
/// topology work is `g_event`'s), not on the caller's system call stack;
/// inline only if there is no event thread.
fn with_mesh<T: Send + 'static>(s: &Sleepable, f: impl FnOnce(&Mesh) -> T + Send + 'static) -> T {
    // Acquire: pairs with the release once the thread is up.
    if !RUNNING.load(Ordering::Acquire) {
        return f(&build());
    }
    let slot: Arc<SpinLock<Option<T>>> = Arc::new(SpinLock::new(None));
    let out = Arc::clone(&slot);
    run(
        s,
        Box::new(move |_| {
            let r = f(&build());
            *out.lock() = Some(r);
        }),
    );
    let r = slot.lock().take();
    r.unwrap_or_else(|| unreachable!("the event ran before the drain returned"))
}

/// The event thread's stack: what building the mesh and running control
/// requests take, with room to spare.
const EVENT_STACK: usize = 64 << 10;

/// The mesh as it is (on the event thread, which must not wait for
/// itself).
pub(crate) fn build() -> Mesh {
    let held = Holders {
        opens: OPENS
            .lock()
            .iter()
            .map(|o| (o.name.clone(), o.write))
            .collect(),
        mounts: mounts(),
    };
    let mut b = Builder {
        m: Mesh::new(),
        held: &held,
        tables: Vec::new(),
        tastes: VecDeque::new(),
    };
    for (i, name) in (0u64..).zip(CLASSES) {
        b.m.add_class(CLASS_IDS + (i << 8), name);
    }
    // Disks as they were published; the tables found or made later, and
    // the partitions and names that came after their tables, when they
    // did.
    let all = storage::disk::all();
    let mut events: Vec<(u64, Happened<'_>)> = Vec::new();
    for d in &all {
        if d.partition_index().is_none() {
            events.push((d.serial(), Happened::Disk(d)));
        }
        let Some(t) = d.table() else { continue };
        if !t.at_publication {
            events.push((t.born, Happened::Table(d)));
        }
        let mut batches = Vec::new();
        for p in storage::partitions(d) {
            let Some((batch, labeled)) = p.partition_times() else {
                continue;
            };
            if batch != t.born && !batches.contains(&batch) {
                batches.push(batch);
            }
            if labeled != batch {
                events.push((labeled, Happened::Relabel(p)));
            }
        }
        events.extend(
            batches
                .into_iter()
                .map(|batch| (batch, Happened::Batch(d, batch))),
        );
    }
    // Serials are unique: no two events tie.
    events.sort_unstable_by_key(|(when, _)| *when);
    for (_, e) in events {
        match e {
            Happened::Disk(d) => b.disk(d),
            Happened::Table(d) => {
                if let Some(p) = b.m.provider(d.name()) {
                    b.table(d, p, firmware(d));
                }
            }
            Happened::Batch(d, batch) => b.batch(d, batch),
            Happened::Relabel(p) => b.relabel(&p),
        }
        b.settle();
    }
    b.mounts();
    b.m
}

/// A tunable's value from the kernel environment (`TUNABLE_INT`), else
/// `default`.
fn tunable(name: &str, default: i32) -> i32 {
    crate::boot::value(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// One of the mesh's text forms.
type View = fn(&Mesh) -> String;

/// Adds `kern.geom` and its `confxml`, `confdot`, and `conftxt`, and
/// `kern.disks`, and starts the event thread. Each is a read-only string
/// made when read, after the tree's lock is dropped (making one waits for
/// the event thread).
pub(crate) fn init(s: &Sleepable) {
    use abi::sysctl::{ctlflag, ctltype};
    use sysctl::Number;
    crate::geom_ctl::init();
    match sched::Builder::new("g_event")
        .priority(sched::Priority::KERNEL)
        .stack(EVENT_STACK)
        .spawn(run_events)
    {
        // Release: pairs with `post`'s acquire.
        Ok(_) => RUNNING.store(true, Ordering::Release),
        Err(_) => kprintln!("geom: no event thread"),
    }
    let text = |f: View| -> sysctl::QueryFn {
        Arc::new(move |new: Option<&[u8]>, out: &mut Vec<u8>| {
            if new.is_some() {
                return Err(Errno::EPERM);
            }
            let s = crate::kinfo::sleepable();
            out.extend_from_slice(with_mesh(&s, f).as_bytes());
            out.push(0);
            Ok(())
        })
    };
    let kind = ctltype::STRING | ctlflag::RD | ctlflag::MPSAFE;
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        let g = t
            .add_node(kern, Number::Auto, "geom", "GEOMetry management")
            .expect("kern.geom");
        let nodes: [(&str, &str, View); 3] = [
            ("confxml", "Dump the GEOM config in XML", Mesh::confxml),
            ("confdot", "Dump the GEOM config in dot", Mesh::confdot),
            ("conftxt", "Dump the GEOM config in txt", Mesh::conftxt),
        ];
        for (name, descr, f) in nodes {
            t.add_query(g, Number::Auto, name, descr, kind, "", text(f))
                .expect("kern.geom.conf*");
        }
        DEBUGFLAGS.store(tunable("kern.geom.debugflags", 0), Ordering::Relaxed);
        t.add_writable(
            g,
            Number::Auto,
            "debugflags",
            "Set various trace levels for GEOM debugging",
            ctlflag::TUN,
            sysctl::Value::Int(Box::new(|| DEBUGFLAGS.load(Ordering::Relaxed))),
            sysctl::int_writer(|n| {
                DEBUGFLAGS.store(n, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.geom.debugflags");
        let part = t
            .add_node(g, Number::Auto, "part", "GEOM_PART stuff")
            .expect("kern.geom.part");
        let check = tunable("kern.geom.part.check_integrity", 1) != 0;
        storage::CHECK_INTEGRITY.store(check, Ordering::Relaxed);
        t.add_writable(
            part,
            Number::Auto,
            "check_integrity",
            "Enable integrity checking",
            ctlflag::TUN,
            sysctl::Value::Uint(Box::new(|| {
                u32::from(storage::CHECK_INTEGRITY.load(Ordering::Relaxed))
            })),
            sysctl::int_writer(|n| {
                storage::CHECK_INTEGRITY.store(n != 0, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.geom.part.check_integrity");
        t.add_query(
            kern,
            Number::Auto,
            "disks",
            "names of available disks",
            kind,
            "A",
            text(|m| m.geom_names("DISK")),
        )
        .expect("kern.disks");
    });
}
