// SPDX-License-Identifier: BSD-2-Clause
//! devfs (docs/VFS.md §10.1): the device nodes, appearing and going as
//! devices do. The kernel's devices are registered here once (`console`,
//! `null`, `zero`, `random` and its link `urandom`, `tty`, the terminals),
//! disks and their partitions as `sys/storage` publishes and withdraws
//! them, with FreeBSD's names, modes, and owners (disks `crw-r-----
//! root:operator`).
//!
//! Nodes can be given other modes, owners, and times (`chmod`, `chown`,
//! `touch`), and symbolic links can be made, as in FreeBSD's devfs. A
//! device that goes takes its node with it, and its vnode is doomed.
//! Each device's number is its inode number, as FreeBSD's devfs numbers
//! them; the numbers follow creation order.
//!
//! `console` is the console's terminal when its port can be typed at;
//! otherwise it only prints, and reads as end of file. A disk node reads
//! and writes whole sectors at sector-aligned offsets (`EINVAL`
//! otherwise), and answers the `DIOCG*` requests `newfs` and `fsck_ffs`
//! use.
//!
//! Behavior reference: sys/fs/devfs/devfs_vnops.c, sys/dev/null/null.c,
//! sys/dev/random/randomdev.c, sys/geom/geom_dev.c.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::errno::Errno;
use abi::time::Timespec;
use sched::Sleepable;
use sync::{Once, SpinLock};
use vfs::{
    Attr, CharDevice, Create, Device, DeviceNode, DirEntry, DirectoryNode, FileSystem, FsStats,
    Kind, Made, Mount, NewNode, Node, NodeOps, Rename, SetAttr, SymlinkNode, Uio, Vnode,
};

/// `/dev/null`: reads end at once, writes vanish.
struct Null;

impl CharDevice for Null {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Ok(uio.resid())
    }
}

/// `/dev/zero`: zeros forever; writes vanish.
struct Zero;

static ZEROS: [u8; 4096] = [0; 4096];

impl CharDevice for Zero {
    fn maps_as_zeros(&self) -> bool {
        true
    }

    fn read(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        let mut done = 0;
        while uio.resid() > 0 {
            done += uio.copy_out(&ZEROS)?;
        }
        Ok(done)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Ok(uio.resid())
    }
}

/// `/dev/random`: the kernel's generator. Writes are accepted and
/// ignored (FreeBSD mixes them in, root only).
struct Random;

impl CharDevice for Random {
    fn read(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        let mut chunk = [0u8; 256];
        let mut done = 0;
        while uio.resid() > 0 {
            let n = uio.resid().min(chunk.len());
            crate::random::fill(&mut chunk[..n]).map_err(|_| Errno::EAGAIN)?;
            done += uio.copy_out(&chunk[..n])?;
        }
        Ok(done)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Ok(uio.resid())
    }
}

/// `/dev/console`: output to the kernel console.
struct Console;

impl CharDevice for Console {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        let mut chunk = [0u8; 256];
        let mut done = 0;
        while uio.resid() > 0 {
            let n = uio.copy_in(&mut chunk)?;
            crate::console::write_bytes(&chunk[..n]);
            done += n;
        }
        Ok(done)
    }
}

/// A raw disk or partition (FreeBSD's `geom_dev`).
struct DiskDev(Arc<storage::Disk>);

fn bio_errno(e: storage::BioError) -> Errno {
    match e {
        storage::BioError::Io => Errno::EIO,
        storage::BioError::Gone => Errno::ENXIO,
        storage::BioError::Invalid => Errno::EINVAL,
        storage::BioError::ReadOnly => Errno::EROFS,
    }
}

impl DiskDev {
    /// The byte range a transfer of `len` at `offset` covers within the
    /// disk: whole sectors only.
    fn range(&self, offset: u64, len: usize) -> Result<(u64, usize), Errno> {
        let ss = u64::from(self.0.sector_size());
        if !offset.is_multiple_of(ss) || !(len as u64).is_multiple_of(ss) {
            return Err(Errno::EINVAL);
        }
        let size = self.0.bytes();
        let n = usize::try_from(size.saturating_sub(offset)).unwrap_or(usize::MAX);
        Ok((offset, len.min(n)))
    }
}

/// Transfers at most this much per disk request.
const CHUNK: usize = 128 * 1024;

impl CharDevice for DiskDev {
    fn dtype(&self) -> u32 {
        vfs::D_DISK
    }

    fn read(&self, s: &Sleepable, uio: &mut dyn Uio, offset: u64, _: bool) -> Result<usize, Errno> {
        let (at, len) = self.range(offset, uio.resid())?;
        let mut done = 0;
        while done < len {
            let n = CHUNK.min(len - done);
            let data = self.0.read(s, at + done as u64, n).map_err(bio_errno)?;
            let moved = uio.copy_out(&data)?;
            done += moved;
            if moved < n {
                break;
            }
        }
        Ok(done)
    }

    fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        offset: u64,
        _: bool,
    ) -> Result<usize, Errno> {
        let (at, len) = self.range(offset, uio.resid())?;
        if len == 0 && uio.resid() > 0 {
            return Err(Errno::ENOSPC);
        }
        let mut buf = alloc::vec![0u8; CHUNK.min(len)];
        let mut done = 0;
        while done < len {
            let n = CHUNK.min(len - done);
            let got = uio.copy_in(&mut buf[..n])?;
            self.0
                .write(s, at + done as u64, &buf[..got])
                .map_err(bio_errno)?;
            done += got;
            if got < n {
                break;
            }
        }
        Ok(done)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        use abi::ioctl::*;
        let put = |arg: &mut [u8], v: &[u8]| {
            arg[..v.len()].copy_from_slice(v);
        };
        match req {
            DIOCGSECTORSIZE => put(arg, &self.0.sector_size().to_ne_bytes()),
            DIOCGMEDIASIZE => put(
                arg,
                &i64::try_from(self.0.bytes())
                    .unwrap_or(i64::MAX)
                    .to_ne_bytes(),
            ),
            // No firmware geometry, as for NVMe disks on FreeBSD.
            DIOCGFWSECTORS | DIOCGFWHEADS => put(arg, &0u32.to_ne_bytes()),
            DIOCGSTRIPESIZE | DIOCGSTRIPEOFFSET => put(arg, &0i64.to_ne_bytes()),
            DIOCGFLUSH => self.0.flush(s).map_err(bio_errno)?,
            // No serial number is known.
            DIOCGIDENT => arg.fill(0),
            DIOCGPROVIDERNAME => {
                arg.fill(0);
                let name = self.0.name().as_bytes();
                put(arg, &name[..name.len().min(arg.len() - 1)]);
            }
            _ => return Err(Errno::ENOTTY),
        }
        Ok(())
    }
}

/// The root directory's inode.
const ROOT_INO: u64 = 2;
/// FreeBSD's `GID_OPERATOR`, which owns disks.
const GID_OPERATOR: u32 = 5;

enum What {
    Dir(BTreeMap<Box<[u8]>, u64>),
    Dev(Device),
    Link(Vec<u8>),
}

struct DevNode {
    parent: u64,
    attr: Attr,
    what: What,
}

impl DevNode {
    fn kind(&self) -> Kind {
        match self.what {
            What::Dir(_) => Kind::Directory,
            What::Dev(_) => Kind::CharDevice,
            What::Link(_) => Kind::Symlink,
        }
    }
}

/// The device nodes.
struct Registry {
    nodes: BTreeMap<u64, DevNode>,
    next: u64,
}

static REGISTRY: SpinLock<Option<Registry>> = SpinLock::new(None);
/// The devfs mount, for dooming a gone device's vnode.
static MOUNT: Once<Arc<Mount>> = Once::new();
static CONSOLE: Once<Arc<Vnode>> = Once::new();

fn with<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut r = REGISTRY.lock();
    let reg = r.get_or_insert_with(|| {
        let t = super::now();
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT_INO,
            DevNode {
                parent: ROOT_INO,
                attr: attr(0o555, 0, 0, 2, t),
                what: What::Dir(BTreeMap::new()),
            },
        );
        Registry {
            nodes,
            next: ROOT_INO + 1,
        }
    });
    f(reg)
}

fn attr(mode: u16, uid: u32, gid: u32, nlink: u64, t: Timespec) -> Attr {
    Attr {
        mode,
        uid,
        gid,
        nlink,
        atime: t,
        mtime: t,
        ctime: t,
        birthtime: t,
        ..Attr::default()
    }
}

impl Registry {
    /// The directory holding `path`'s last component (made, `0555`, if
    /// missing), and that component.
    fn parent<'p>(&mut self, path: &'p [u8]) -> Option<(u64, &'p [u8])> {
        let parts: Vec<&[u8]> = path
            .split(|&b| b == b'/')
            .filter(|p| !p.is_empty())
            .collect();
        let (last, dirs) = parts.split_last()?;
        let mut at = ROOT_INO;
        for d in dirs {
            let existing = match &self.nodes.get(&at)?.what {
                What::Dir(m) => m.get(*d).copied(),
                _ => return None,
            };
            at = match existing {
                Some(i) => i,
                None => {
                    let ino = self.next;
                    self.next += 1;
                    let t = super::now();
                    self.nodes.insert(
                        ino,
                        DevNode {
                            parent: at,
                            attr: attr(0o555, 0, 0, 2, t),
                            what: What::Dir(BTreeMap::new()),
                        },
                    );
                    if let Some(DevNode {
                        what: What::Dir(m),
                        attr,
                        ..
                    }) = self.nodes.get_mut(&at)
                    {
                        m.insert((*d).into(), ino);
                        attr.nlink += 1;
                    }
                    ino
                }
            };
        }
        Some((at, last))
    }

    fn insert(&mut self, path: &[u8], what: What, a: Attr) -> Option<u64> {
        let (dir, name) = self.parent(path)?;
        let ino = self.next;
        self.next += 1;
        let mut a = a;
        if matches!(what, What::Dev(_)) {
            a.rdev = ino;
        }
        let what = match what {
            What::Dev(d) => What::Dev(Device {
                rdev: ino,
                ops: d.ops,
            }),
            w => w,
        };
        let DevNode {
            what: What::Dir(m), ..
        } = self.nodes.get_mut(&dir)?
        else {
            return None;
        };
        if m.contains_key(name) {
            return None;
        }
        m.insert(name.into(), ino);
        self.nodes.insert(
            ino,
            DevNode {
                parent: dir,
                attr: a,
                what,
            },
        );
        Some(ino)
    }

    /// Takes `path` out; returns its inode.
    fn remove(&mut self, path: &[u8]) -> Option<u64> {
        let mut at = ROOT_INO;
        for part in path.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
            let What::Dir(m) = &self.nodes.get(&at)?.what else {
                return None;
            };
            at = *m.get(part)?;
        }
        let node = self.nodes.remove(&at)?;
        if let Some(DevNode {
            what: What::Dir(m), ..
        }) = self.nodes.get_mut(&node.parent)
        {
            m.retain(|_, &mut i| i != at);
        }
        Some(at)
    }
}

/// Adds the device `ops` at `path` (under `/dev`), with permissions
/// `mode`, owned by `uid` and `gid`.
pub(crate) fn add(
    path: &[u8],
    mode: u16,
    uid: u32,
    gid: u32,
    ops: Arc<dyn CharDevice>,
) -> Option<u64> {
    let t = super::now();
    with(|r| {
        r.insert(
            path,
            What::Dev(Device { rdev: 0, ops }),
            attr(mode, uid, gid, 1, t),
        )
    })
}

/// Takes the node at `path` away; its vnode, if in core, is doomed.
pub(crate) fn remove(s: &Sleepable, path: &[u8]) {
    let Some(ino) = with(|r| r.remove(path)) else {
        return;
    };
    if let Some(m) = MOUNT.get()
        && let Some(v) = m.cached(ino)
    {
        vfs::cache::purge(&v);
        v.reclaim(s);
    }
}

/// devfs as [`vfs::devices`]' registry, for drivers and classes.
pub(crate) struct Hook;

impl vfs::devices::Registry for Hook {
    fn make(
        &self,
        path: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        ops: Arc<dyn CharDevice>,
    ) -> Option<u64> {
        add(path, mode, uid, gid, ops)
    }

    fn destroy(&self, s: &Sleepable, path: &[u8]) {
        remove(s, path);
    }
}

/// `sys/storage`'s news: a disk's node appears or goes.
fn disk_event(s: &Sleepable, e: storage::disk::Event<'_>) {
    match e {
        storage::disk::Event::Published(d) => {
            for name in names(d) {
                let _ = add(
                    name.as_bytes(),
                    0o640,
                    0,
                    GID_OPERATOR,
                    Arc::new(DiskDev(Arc::clone(d))),
                );
            }
        }
        storage::disk::Event::Withdrawn(d) => {
            for name in names(d) {
                remove(s, name.as_bytes());
            }
        }
    }
}

/// A disk's nodes: its name, and a partition's `gpt/` and `gptid/`
/// aliases (FreeBSD's `glabel`, which also hides the aliases while the
/// partition is open by its own name; these stay).
fn names(d: &storage::Disk) -> Vec<alloc::string::String> {
    let mut out = alloc::vec![d.name().into()];
    if let Some((_, e)) = d.partition() {
        out.extend(e.aliases());
    }
    out
}

/// Registers the kernel's own devices and the disks published so far, and
/// listens for the rest. Runs once, before devfs is mounted.
pub(crate) fn init() {
    // Nodes drivers made before the clock was set take its time now, as
    // FreeBSD's show the boot's time.
    let t = super::now();
    with(|r| {
        for n in r.nodes.values_mut() {
            if n.attr.birthtime == Timespec::default() {
                n.attr.atime = t;
                n.attr.mtime = t;
                n.attr.ctime = t;
                n.attr.birthtime = t;
            }
        }
    });
    let console: Arc<dyn CharDevice> = match crate::tty::console() {
        Some(t) => Arc::new(crate::tty::TtyDevice(t)),
        None => Arc::new(Console),
    };
    add(b"console", 0o600, 0, 0, console);
    add(b"null", 0o666, 0, 0, Arc::new(Null));
    add(b"random", 0o644, 0, 0, Arc::new(Random));
    let t = super::now();
    with(|r| {
        r.insert(
            b"urandom",
            What::Link(b"random".to_vec()),
            attr(0o755, 0, 0, 1, t),
        )
    });
    add(b"tty", 0o666, 0, 0, Arc::new(crate::tty::ControllingTty));
    for tty in &crate::tty::all() {
        add(
            tty.name().as_bytes(),
            0o600,
            0,
            0,
            Arc::new(crate::tty::TtyDevice(Arc::clone(tty))),
        );
    }
    add(b"zero", 0o666, 0, 0, Arc::new(Zero));
    storage::disk::set_listener(disk_event);
    for d in storage::disk::all() {
        for name in names(&d) {
            let _ = add(
                name.as_bytes(),
                0o640,
                0,
                GID_OPERATOR,
                Arc::new(DiskDev(Arc::clone(&d))),
            );
        }
    }
}

/// devfs, as a file system.
pub(crate) struct DevFs;

/// One node, as its vnode holds it.
struct View(u64);

impl View {
    fn node<R>(&self, f: impl FnOnce(&DevNode) -> R) -> Result<R, Errno> {
        with(|r| r.nodes.get(&self.0).map(f)).ok_or(Errno::ENOENT)
    }
}

impl Node for View {
    fn attr(&self) -> Attr {
        self.node(|n| {
            let mut a = n.attr;
            a.size = match &n.what {
                What::Dir(_) => 512,
                What::Link(t) => t.len() as u64,
                What::Dev(_) => 0,
            };
            a
        })
        .unwrap_or_default()
    }

    fn setattr(&mut self, _s: &Sleepable, a: &SetAttr) -> Result<(), Errno> {
        if a.flags.is_some() || a.size.is_some() {
            return Err(Errno::EOPNOTSUPP);
        }
        with(|r| {
            let n = r.nodes.get_mut(&self.0).ok_or(Errno::ENOENT)?;
            let t = super::now();
            if let Some(m) = a.mode {
                n.attr.mode = m & 0o7777;
            }
            if let Some(u) = a.uid {
                n.attr.uid = u;
            }
            if let Some(g) = a.gid {
                n.attr.gid = g;
            }
            if let Some(x) = a.atime {
                n.attr.atime = x;
            }
            if let Some(x) = a.mtime {
                n.attr.mtime = x;
            }
            n.attr.ctime = t;
            Ok(())
        })
    }

    fn reclaim(self: Box<Self>, _s: &Sleepable) {}

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl DeviceNode for View {
    fn device(&self) -> Device {
        self.node(|n| match &n.what {
            What::Dev(d) => Some(d.clone()),
            _ => None,
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| Device {
            rdev: 0,
            ops: Arc::new(Gone),
        })
    }
}

/// What a vnode of a device that went reaches.
struct Gone;

impl CharDevice for Gone {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }
}

impl SymlinkNode for View {
    fn target(&self, _s: &Sleepable) -> Result<Vec<u8>, Errno> {
        self.node(|n| match &n.what {
            What::Link(t) => Ok(t.clone()),
            _ => Err(Errno::EINVAL),
        })?
    }
}

impl DirectoryNode for View {
    fn lookup(&self, _s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        self.node(|n| match &n.what {
            What::Dir(m) => m.get(name).copied().ok_or(Errno::ENOENT),
            _ => Err(Errno::ENOTDIR),
        })?
    }

    fn parent(&self) -> u64 {
        self.node(|n| n.parent).unwrap_or(ROOT_INO)
    }

    fn create(
        &mut self,
        _s: &Sleepable,
        name: &[u8],
        node: &NewNode,
        c: &Create,
    ) -> Result<Made, Errno> {
        // Only symbolic links may be made, as in FreeBSD's devfs.
        let NewNode::Symlink(target) = node else {
            return Err(Errno::EOPNOTSUPP);
        };
        with(|r| {
            let ino = r.next;
            r.next += 1;
            let DevNode {
                what: What::Dir(m),
                attr: da,
                ..
            } = r.nodes.get_mut(&self.0).ok_or(Errno::ENOENT)?
            else {
                return Err(Errno::ENOTDIR);
            };
            m.insert(name.into(), ino);
            da.mtime = c.time;
            da.ctime = c.time;
            r.nodes.insert(
                ino,
                DevNode {
                    parent: self.0,
                    attr: attr(c.mode, c.uid, c.gid, 1, c.time),
                    what: What::Link(target.clone()),
                },
            );
            Ok(ino.into())
        })
    }

    fn link(
        &mut self,
        _s: &Sleepable,
        _name: &[u8],
        _ino: u64,
        _target: &mut dyn Node,
    ) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    fn remove(&mut self, _s: &Sleepable, name: &[u8], _child: &mut dyn Node) -> Result<(), Errno> {
        with(|r| {
            let ino = match &r.nodes.get(&self.0).ok_or(Errno::ENOENT)?.what {
                What::Dir(m) => *m.get(name).ok_or(Errno::ENOENT)?,
                _ => return Err(Errno::ENOTDIR),
            };
            if let Some(DevNode {
                what: What::Dir(m), ..
            }) = r.nodes.get_mut(&self.0)
            {
                m.remove(name);
            }
            r.nodes.remove(&ino);
            Ok(())
        })
    }

    fn rmdir(
        &mut self,
        _s: &Sleepable,
        _name: &[u8],
        _child: &mut dyn DirectoryNode,
    ) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    fn readdir(
        &self,
        _s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        // A snapshot, so the registry is not held while the caller copies.
        let (parent, entries) = with(|r| {
            let n = r.nodes.get(&self.0).ok_or(Errno::ENOENT)?;
            let What::Dir(m) = &n.what else {
                return Err(Errno::ENOTDIR);
            };
            let entries: Vec<(Box<[u8]>, u64, Kind)> = m
                .iter()
                .map(|(name, &i)| {
                    (
                        name.clone(),
                        i,
                        r.nodes.get(&i).map_or(Kind::CharDevice, DevNode::kind),
                    )
                })
                .collect();
            Ok((n.parent, entries))
        })?;
        if from == 0
            && !f(DirEntry {
                name: b".",
                ino: self.0,
                kind: Kind::Directory,
                next: 1,
            })
        {
            return Ok(());
        }
        if from <= 1
            && !f(DirEntry {
                name: b"..",
                ino: parent,
                kind: Kind::Directory,
                next: 2,
            })
        {
            return Ok(());
        }
        let skip = usize::try_from(from.max(2) - 2).unwrap_or(usize::MAX);
        for (i, (name, ino, kind)) in entries.iter().enumerate().skip(skip) {
            if !f(DirEntry {
                name,
                ino: *ino,
                kind: *kind,
                next: i as u64 + 3,
            }) {
                break;
            }
        }
        Ok(())
    }
}

impl FileSystem for DevFs {
    fn root(&self) -> u64 {
        ROOT_INO
    }

    fn load(&self, _s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        let kind = with(|r| r.nodes.get(&ino).map(DevNode::kind)).ok_or(Errno::ENOENT)?;
        let v = Box::new(View(ino));
        Ok(match kind {
            Kind::Directory => NodeOps::Directory(v),
            Kind::Symlink => NodeOps::Symlink(v),
            _ => NodeOps::Device(v),
        })
    }

    fn statfs(&self, _s: &Sleepable) -> Result<FsStats, Errno> {
        // FreeBSD's devfs reports nothing but its block size.
        Ok(FsStats {
            bsize: 512,
            iosize: 512,
            ..FsStats::default()
        })
    }

    fn rename(&self, _s: &Sleepable, _r: Rename<'_>) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    fn pathconf(&self, kind: Kind, name: u32) -> Result<i64, Errno> {
        use abi::fcntl::*;
        match name {
            _PC_LINK_MAX => Ok(32767),
            _PC_SYMLINK_MAX => Ok(PATH_MAX as i64),
            _PC_NAME_MAX => Ok(NAME_MAX as i64),
            _PC_PIPE_BUF if matches!(kind, Kind::Directory) => Ok(512),
            _PC_CHOWN_RESTRICTED | _PC_NO_TRUNC => Ok(1),
            _ => vfs::mount::std_pathconf(name),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Records devfs mount `m`, and the vnodes of its terminals and console,
/// which opening `/dev/tty` and the boot processes' descriptors need.
pub(crate) fn mounted(s: &Sleepable, m: &Arc<Mount>) {
    let _ = MOUNT.set(Arc::clone(m));
    let Ok(root) = m.root(s) else {
        return;
    };
    for t in &crate::tty::all() {
        if let Ok(v) = vfs::namei::Namespace::child(s, &root, t.name().as_bytes()) {
            t.set_vnode(&v);
        }
    }
    if let Ok(c) = vfs::namei::Namespace::child(s, &root, b"console") {
        let _ = CONSOLE.set(c);
    }
}

/// The console's vnode.
pub(crate) fn console() -> Option<Arc<Vnode>> {
    CONSOLE.get().cloned()
}
