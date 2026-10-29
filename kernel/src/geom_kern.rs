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
//! hides its other names (the LABEL geoms GEOM withers then).
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
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use abi::errno::Errno;
use geom::{Access, Field, Kind, Media, Mesh, ProviderRef};
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

// --- The event thread ---------------------------------------------------

type Event = Box<dyn FnOnce(&Sleepable) + Send>;

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

/// Whether `disk` or a partition of it is open or mounted (md's
/// `opencount`).
pub(crate) fn in_use(disk: &Arc<Disk>) -> bool {
    let serial = disk.serial();
    OPENS.lock().iter().any(|o| o.serial == serial)
        || mounts()
            .iter()
            .any(|(_, from, _, _)| storage::find(from).is_some_and(|d| d.serial() == serial))
        || partitions_in_use(disk)
}

/// Whether a partition of `disk` is open or mounted.
fn partitions_in_use(disk: &Arc<Disk>) -> bool {
    let parts: Vec<u64> = storage::partitions(disk)
        .iter()
        .map(|p| p.serial())
        .collect();
    if OPENS.lock().iter().any(|o| parts.contains(&o.serial)) {
        return true;
    }
    mounts()
        .iter()
        .any(|(_, from, _, _)| storage::find(from).is_some_and(|d| parts.contains(&d.serial())))
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
}

/// Counts an open of disk node `name` (of `disk`), for writing if
/// `write`; the returned device serves the open file's I/O through `dev`
/// until it closes. The first write open of a whole disk spoils its
/// partition table, unless a partition is in use.
pub(crate) fn open(
    s: &Sleepable,
    dev: Arc<dyn CharDevice>,
    disk: &Arc<Disk>,
    name: &str,
    write: bool,
) -> Arc<dyn CharDevice> {
    let whole = disk.partition().is_none();
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
    if first_writer && whole && disk.table().is_some() && !partitions_in_use(disk) {
        storage::spoil(s, disk);
    }
    Arc::new(Opened {
        dev,
        id,
        disk: Arc::downgrade(disk),
        serial: disk.serial(),
        rewrites: write && whole,
    })
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
            post(Box::new(move |s| {
                if let Some(d) = disk.upgrade() {
                    storage::retaste(s, &d);
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
            let config = vec![
                Field::new("unit", md.unit),
                Field::new("sectorsize", d.sector_size()),
                Field::new("fwheads", md.fwheads),
                Field::new("fwsectors", md.fwsectors),
                Field::new("length", d.bytes()),
                Field::new("compression", if md.compress { "on" } else { "off" }),
                Field::new("access", access),
                Field::new("type", md.kind),
                Field::new("label", ""),
            ];
            let txt = format!(
                " u {} s {} f {} fs {} l {} t {} label ",
                md.unit,
                d.sector_size(),
                md.fwheads,
                md.fwsectors,
                d.bytes(),
                md.kind
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
        if d.table().is_some_and(|t| !t.retasted) {
            self.table(d, p, fw);
        }
        self.dev(serial, DEV, d.name(), p);
    }

    /// Disk `d`'s partition table (on its provider `dp`, with firmware
    /// geometry `fw`): the PART geom and its providers, then each
    /// partition's LABEL and DEV geoms, then the labels' DEV geoms, as
    /// the tastes of each new provider make them.
    fn table(&mut self, d: &Arc<Disk>, dp: ProviderRef, fw: (u32, u32)) {
        let Some(t) = d.table() else { return };
        let serial = d.serial();
        let c = self.class("PART");
        let g = self
            .m
            .add_geom(c, id(serial, PART_GEOM), d.name(), Kind::Part);
        let ss = u64::from(d.sector_size());
        let (heads, sectors) = geom::part::geometry(d.sectors(), Some(fw));
        self.m.set_geom_config(
            g,
            vec![
                Field::new("scheme", "GPT"),
                Field::new("entries", t.entries),
                Field::new("first", t.first),
                Field::new("last", t.last),
                Field::new("fwsectors", sectors),
                Field::new("fwheads", heads),
                Field::new("state", if t.corrupt { "CORRUPT" } else { "OK" }),
                Field::new("modified", "false"),
            ],
        );
        self.m.attach(g, id(serial, PART_CONSUMER), dp);
        let desc = d.description();
        let parts = storage::partitions(d);
        let mut made = Vec::new();
        for part in &parts {
            let Some((_, e)) = part.partition() else {
                continue;
            };
            let offset = e.first_lba * ss;
            let mut stripeoffset = desc.stripeoffset + offset;
            if desc.stripesize > 0 {
                stripeoffset %= desc.stripesize;
            }
            let media = Media {
                mediasize: part.bytes(),
                sectorsize: part.sector_size(),
                stripesize: desc.stripesize,
                stripeoffset,
            };
            let p = self
                .m
                .add_provider(g, id(part.serial(), PROVIDER), part.name(), media);
            let (index, ty, raw_type, raw_uuid) =
                (e.index + 1, e.type_name(), e.raw_type(), e.raw_uuid());
            let mut config = vec![
                Field::new("start", e.first_lba),
                Field::new("end", e.last_lba),
                Field::new("index", index),
                Field::new("type", &ty),
                Field::new("offset", offset),
                Field::new("length", part.bytes()),
                Field::new("label", geom::label::utf16_name(&e.name)),
            ];
            for (bit, name) in geom::part::GPT_ATTRIBUTES {
                if e.attributes & bit != 0 {
                    config.push(Field::new("attrib", name));
                }
            }
            config.push(Field::new("rawtype", &raw_type));
            config.push(Field::new("rawuuid", &raw_uuid));
            config.push(Field::new(
                "efimedia",
                geom::part::gpt_efimedia(index, &raw_uuid, e.first_lba, e.last_lba),
            ));
            let txt = format!(" i {index} o {offset} ty {ty} xs GPT xt {raw_type}");
            self.m.set_provider_config(p, config, txt);
            made.push((Arc::clone(part), p, media));
        }
        let mut labels = Vec::new();
        for (part, p, media) in &made {
            let Some((_, e)) = part.partition() else {
                continue;
            };
            let names = e.label_names();
            // Written through its own name: GEOM withered its labels;
            // through one of them: the others.
            let keep: Vec<bool> = if self.held.writes(part.name()) {
                vec![false; names.len()]
            } else if let Some(w) = names.iter().position(|n| self.held.writes(n)) {
                (0..names.len()).map(|i| i == w).collect()
            } else {
                vec![true; names.len()]
            };
            for (n, (name, keep)) in (0u64..).zip(names.iter().zip(keep)) {
                // A name another provider has already is not made again.
                if !keep || self.m.provider(name).is_some() {
                    continue;
                }
                let s = part.serial();
                let c = self.class("LABEL");
                let base = LABEL + 5 * n;
                let lg = self.m.add_geom(c, id(s, base), part.name(), Kind::Label);
                self.m.attach(lg, id(s, base + 1), *p);
                let lp = self.m.add_provider(lg, id(s, base + 2), name, *media);
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
                labels.push((s, base, name.clone(), lp));
            }
            self.dev(part.serial(), DEV, part.name(), *p);
        }
        for (s, base, name, lp) in labels {
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

/// The mesh now, once the events posted so far have run.
fn mesh(s: &Sleepable) -> Mesh {
    drain(s);
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
    };
    for (i, name) in (0u64..).zip(CLASSES) {
        b.m.add_class(CLASS_IDS + (i << 8), name);
    }
    // Disks as they were published, and the tables found later when
    // those were found (their first partition's serial).
    let all = storage::disk::all();
    let mut events: Vec<(u64, &Arc<Disk>, bool)> = Vec::new();
    for d in all.iter().filter(|d| d.partition().is_none()) {
        events.push((d.serial(), d, false));
        if d.table().is_some_and(|t| t.retasted) {
            let when = storage::partitions(d)
                .iter()
                .map(|p| p.serial())
                .min()
                .unwrap_or(u64::MAX);
            events.push((when, d, true));
        }
    }
    events.sort_by_key(|&(when, _, _)| when);
    for (_, d, table) in events {
        if table {
            if let Some(p) = b.m.provider(d.name()) {
                let fw = crate::mdisk::info(d).map_or_else(
                    || {
                        let desc = d.description();
                        (desc.fwheads, desc.fwsectors)
                    },
                    |md| (md.fwheads, md.fwsectors),
                );
                b.table(d, p, fw);
            }
        } else {
            b.disk(d);
        }
    }
    b.mounts();
    b.m
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
    match sched::spawn("g_event", sched::Priority::KERNEL, run_events) {
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
            out.extend_from_slice(f(&mesh(&s)).as_bytes());
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
