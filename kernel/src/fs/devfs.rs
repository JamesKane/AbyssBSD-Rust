// SPDX-License-Identifier: BSD-2-Clause
//! devfs (docs/VFS.md §10.1): the device nodes, appearing and going as
//! devices do. The kernel's devices are registered here once (`console`,
//! `null`, `zero`, `random` and its link `urandom`, `tty`, the terminals),
//! disks and their partitions as `sys/storage` publishes and withdraws
//! them, with FreeBSD's names, modes, and owners (disks `crw-r-----
//! root:operator`).
//!
//! Every devfs mount shows the same nodes, each its own way (FreeBSD's
//! per-mount dirents): whether it is hidden, and its mode, owner, and
//! group, which `chmod` and `chown` change on that mount alone; a
//! device's times are the device's. A mount runs its ruleset
//! (`devfs(8)`'s rules, [`vfs::devfsrule`]) on each node as the node
//! first shows in it; the `DEVFSIO_*` requests on any of its directories
//! edit the rulesets and apply them. Removing a node hides it on that
//! mount; symbolic links made in a mount are that mount's alone. A
//! device that goes takes its node with it, and its vnodes are doomed.
//! Each device's number is its inode number, as FreeBSD's devfs numbers
//! them; the numbers follow creation order.
//!
//! `console` is the console's terminal when its port can be typed at;
//! otherwise it only prints, and reads as end of file. A disk node reads
//! and writes whole sectors at sector-aligned offsets (`EINVAL`
//! otherwise), and answers the `DIOCG*` requests `newfs` and `fsck_ffs`
//! use.
//!
//! Behavior reference: sys/fs/devfs/devfs_vnops.c,
//! sys/fs/devfs/devfs_devs.c, sys/fs/devfs/devfs_rule.c,
//! sys/fs/devfs/devfs_vfsops.c, sys/dev/null/null.c,
//! sys/dev/random/randomdev.c, sys/geom/geom_dev.c.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::time::Timespec;
use sched::Sleepable;
use sync::{Once, SpinLock};
use vfs::devfsrule::{Entry, Rules, rid_input, rule_input};
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

/// A raw disk or partition (FreeBSD's `geom_dev`), by its node's name
/// (`md9`, `gpt/rootfs0`): the provider GEOM shows an open of it on.
struct DiskDev(Arc<storage::Disk>, alloc::string::String);

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

    /// Each open is counted against the provider until it closes.
    fn instance_open(
        &self,
        s: &Sleepable,
        _: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        let dev = Arc::new(DiskDev(Arc::clone(&self.0), self.1.clone()));
        Ok(Some(crate::geom_kern::open(
            s, dev, &self.0, &self.1, write,
        )))
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
    /// A device's `d_flags` kind (`D_DISK`, `D_TTY`, ...), for rules.
    dsw: u32,
    /// The mount a symbolic link was made in, the only one showing it.
    owner: Option<u64>,
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
    /// Bumped as nodes come and go, so mounts know to look again.
    generation: u64,
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
                dsw: 0,
                owner: None,
            },
        );
        Registry {
            nodes,
            next: ROOT_INO + 1,
            generation: 1,
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
                            dsw: 0,
                            owner: None,
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
        let (what, dsw) = match what {
            What::Dev(d) => {
                let dsw = d.ops.dtype();
                (
                    What::Dev(Device {
                        rdev: ino,
                        ops: d.ops,
                    }),
                    dsw,
                )
            }
            w => (w, 0),
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
                dsw,
                owner: None,
            },
        );
        self.generation += 1;
        Some(ino)
    }

    /// The inode at `path`.
    fn lookup(&self, path: &[u8]) -> Option<u64> {
        let mut at = ROOT_INO;
        for part in path.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
            let What::Dir(m) = &self.nodes.get(&at)?.what else {
                return None;
            };
            at = *m.get(part)?;
        }
        Some(at)
    }

    /// The path of inode `ino` under `/dev` (`devtoname`).
    fn path(&self, ino: u64) -> Option<Vec<u8>> {
        let mut parts = Vec::new();
        let mut at = ino;
        while at != ROOT_INO {
            let node = self.nodes.get(&at)?;
            let What::Dir(m) = &self.nodes.get(&node.parent)?.what else {
                return None;
            };
            let (name, _) = m.iter().find(|&(_, &i)| i == at)?;
            parts.push(name.clone());
            at = node.parent;
        }
        let mut out = Vec::new();
        for (i, p) in parts.iter().rev().enumerate() {
            if i > 0 {
                out.push(b'/');
            }
            out.extend_from_slice(p);
        }
        Some(out)
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
        self.generation += 1;
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
    let ino = with(|r| {
        r.insert(
            path,
            What::Dev(Device { rdev: 0, ops }),
            attr(mode, uid, gid, 1, t),
        )
    })?;
    forget_names(path);
    Some(ino)
}

/// Drops the name cache's entries along `path`, a node just made: a
/// lookup that failed before must not hide it (FreeBSD's devfs purges
/// negative entries as it populates).
fn forget_names(path: &[u8]) {
    for m in mounts() {
        forget_names_in(&m, path);
    }
}

/// [`forget_names`] on mount `m`.
fn forget_names_in(m: &Mount, path: &[u8]) {
    let mut at = ROOT_INO;
    for part in path.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
        if let Some(dir) = m.cached(at) {
            vfs::cache::remove(&dir, part);
        }
        match with(|r| match &r.nodes.get(&at)?.what {
            What::Dir(d) => d.get(part).copied(),
            _ => None,
        }) {
            Some(next) => at = next,
            None => return,
        }
    }
}

/// Takes the node at `path` away; its vnode, if in core, is doomed.
pub(crate) fn remove(s: &Sleepable, path: &[u8]) {
    let Some(ino) = with(|r| r.remove(path)) else {
        return;
    };
    for m in mounts() {
        if let Some(v) = m.cached(ino) {
            vfs::cache::purge(&v);
            v.reclaim(s);
        }
    }
}

/// Every devfs mount (none before `/dev` is).
fn mounts() -> Vec<Arc<Mount>> {
    // Disks are published from attach threads, maybe while fs::init has
    // mounted /dev but not yet published the namespace.
    let Some(dev) = MOUNT.get() else {
        return Vec::new();
    };
    let Some(ns) = super::NAMESPACE.get() else {
        return alloc::vec![Arc::clone(dev)];
    };
    let mut all = ns.mounts();
    all.retain(|m| m.fs().as_any().is::<DevFs>());
    all
}

/// The device number of the node at `path`.
pub(crate) fn rdev_of(path: &[u8]) -> Option<u64> {
    with(|r| r.lookup(path))
}

/// The attributes of the node at `path` (`si_mode`, `si_uid`, ...).
pub(crate) fn attr_of(path: &[u8]) -> Option<Attr> {
    with(|r| {
        let ino = r.lookup(path)?;
        r.nodes.get(&ino).map(|n| n.attr)
    })
}

/// The node `v` is, if it is devfs's.
fn ino_of(v: &Vnode) -> Option<u64> {
    v.mount().fs().as_any().is::<DevFs>().then(|| v.id().ino)
}

/// The name under `/dev` of the device node `v` is (`devtoname`, for
/// `FIODGNAME`).
pub(crate) fn name_of(v: &Vnode) -> Option<Vec<u8>> {
    let ino = ino_of(v)?;
    with(|r| r.path(ino))
}

/// The name under `/dev` of the device whose number is `rdev`
/// (`kern.devname`, FreeBSD's `devtoname` of a `dev_t`).
pub(crate) fn name_of_rdev(rdev: u64) -> Option<Vec<u8>> {
    with(|r| {
        matches!(r.nodes.get(&rdev)?.what, What::Dev(_))
            .then(|| r.path(rdev))
            .flatten()
    })
}

/// The vnode of the node at `path`, looked up afresh (a revoked node's
/// old vnode is dead).
pub(crate) fn vnode_of(s: &Sleepable, path: &[u8]) -> Option<Arc<Vnode>> {
    let mut at = MOUNT.get()?.root(s).ok()?;
    for part in path.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
        at = vfs::namei::Namespace::child(s, &at, part).ok()?;
    }
    Some(at)
}

/// Revokes the node `ino` (`devfs_revoke`): its device forgets every open
/// of it, and its vnode is doomed, so files open on it are dead; a later
/// open gets a new vnode.
fn revoke_ino(s: &Sleepable, ino: u64) {
    let ops = with(|r| match r.nodes.get(&ino).map(|n| &n.what) {
        Some(What::Dev(d)) => Some(Arc::clone(&d.ops)),
        _ => None,
    });
    let Some(ops) = ops else {
        return;
    };
    for m in mounts() {
        if let Some(v) = m.cached(ino) {
            vfs::cache::purge(&v);
            v.reclaim(s);
        }
    }
    ops.revoke(s);
}

/// Revokes the node at `path`.
pub(crate) fn revoke_path(s: &Sleepable, path: &[u8]) {
    if let Some(ino) = rdev_of(path) {
        revoke_ino(s, ino);
    }
}

/// `revoke(2)` of `v` (`VOP_REVOKE`): only a device node.
pub(crate) fn revoke(s: &Sleepable, v: &Vnode) -> Result<(), Errno> {
    let ino = ino_of(v).ok_or(Errno::EINVAL)?;
    if v.kind() != Kind::CharDevice {
        return Err(Errno::EINVAL);
    }
    revoke_ino(s, ino);
    Ok(())
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

    fn alias(&self, path: &[u8], target: &[u8]) -> bool {
        let t = super::now();
        let made = with(|r| r.insert(path, What::Link(target.to_vec()), attr(0o755, 0, 0, 1, t)))
            .is_some();
        if made {
            forget_names(path);
        }
        made
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
                    Arc::new(DiskDev(Arc::clone(d), name.clone())),
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
    // Nodes drivers made before the clock was set (stamped with the
    // uptime, before the boot's time of day) take its time now, as
    // FreeBSD's devfs stamps nodes when it populates.
    let t = super::now();
    let up = crate::shared_page::to_ns(crate::shared_page::uptime()) / 1_000_000_000;
    let boot = t.tv_sec - i64::try_from(up).unwrap_or(0);
    with(|r| {
        for n in r.nodes.values_mut() {
            if n.attr.birthtime.tv_sec < boot {
                n.attr.atime = t;
                n.attr.mtime = t;
                n.attr.ctime = t;
                n.attr.birthtime = t;
            }
        }
    });
    let console: Arc<dyn CharDevice> = match crate::tty::console() {
        Some(t) => Arc::new(crate::tty::TtyDevice::new(&t, crate::tty::Unit::Console)),
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
    add(b"zero", 0o666, 0, 0, Arc::new(Zero));
    storage::disk::set_listener(disk_event);
    for d in storage::disk::all() {
        for name in names(&d) {
            let _ = add(
                name.as_bytes(),
                0o640,
                0,
                GID_OPERATOR,
                Arc::new(DiskDev(Arc::clone(&d), name.clone())),
            );
        }
    }
}

/// The rulesets (`devfs_rulesets`), every mount's.
static RULES: SpinLock<Rules> = SpinLock::new(Rules::new());
/// `vfs.devfs.rule_depth`: how deep includes go.
static RULE_DEPTH: AtomicU32 = AtomicU32::new(1);
/// Mount ids, for the symbolic links a mount owns.
static NEXT_MOUNT: AtomicU64 = AtomicU64::new(1);

/// A node as one mount shows it (FreeBSD's `devfs_dirent`).
#[derive(Clone, Copy)]
struct De {
    /// `DE_WHITEOUT`.
    hidden: bool,
    /// A directory's `.` and `..` entries hidden (a rule without
    /// conditions hides them too).
    dots: [bool; 2],
    mode: u16,
    uid: u32,
    gid: u32,
    /// Access, modification, change, and birth times, for what is not a
    /// device (a device's are the device's).
    times: [Timespec; 4],
}

impl De {
    fn of(n: &DevNode) -> Self {
        let a = &n.attr;
        Self {
            hidden: false,
            dots: [false; 2],
            mode: a.mode,
            uid: a.uid,
            gid: a.gid,
            times: [a.atime, a.mtime, a.ctime, a.birthtime],
        }
    }
}

/// One mount's nodes (FreeBSD's `devfs_mount`).
struct MountView {
    /// The ruleset in use (`dm_ruleset`).
    ruleset: u16,
    /// The registry generation last looked at.
    generation: u64,
    des: BTreeMap<u64, De>,
}

/// A devfs mount's state, which its vnodes share.
pub(crate) struct DevMount {
    id: u64,
    view: SpinLock<MountView>,
}

impl Drop for DevMount {
    /// `devfs_rules_cleanup`.
    fn drop(&mut self) {
        let rs = self.view.lock().ruleset;
        RULES.lock().release(rs);
    }
}

/// Whether node `n` shows in mount `id` at all.
fn shown(n: &DevNode, id: u64) -> bool {
    n.owner.is_none_or(|o| o == id)
}

/// Runs `f` (a rule, or a ruleset) on node `ino` as `de` shows it.
fn run_on(r: &Registry, ino: u64, n: &DevNode, de: &mut De, f: &dyn Fn(&mut Entry<'_>)) {
    let path = if ino == ROOT_INO { None } else { r.path(ino) };
    let mut e = Entry {
        path: path.as_deref(),
        dswflags: matches!(n.what, What::Dev(_)).then_some(n.dsw),
        hidden: de.hidden,
        uid: de.uid,
        gid: de.gid,
        mode: de.mode,
    };
    f(&mut e);
    de.hidden = e.hidden;
    de.uid = e.uid;
    de.gid = e.gid;
    de.mode = e.mode;
    if matches!(n.what, What::Dir(_)) {
        for d in &mut de.dots {
            let mut e = Entry {
                hidden: *d,
                ..Entry::default()
            };
            f(&mut e);
            *d = e.hidden;
        }
    }
}

/// Gives mount `id` an entry for each node it has not seen, run through
/// its ruleset (`devfs_populate`, `devfs_rules_apply`), and forgets the
/// nodes that went.
fn populate(r: &Registry, rules: &Rules, v: &mut MountView, id: u64) {
    if v.generation == r.generation {
        return;
    }
    let depth = RULE_DEPTH.load(Ordering::Relaxed);
    let rs = v.ruleset;
    v.des.retain(|i, _| r.nodes.contains_key(i));
    for (&ino, n) in &r.nodes {
        if !shown(n, id) || v.des.contains_key(&ino) {
            continue;
        }
        let mut de = De::of(n);
        if rs != 0 {
            run_on(r, ino, n, &mut de, &|e| rules.apply_set(rs, e, depth));
        }
        v.des.insert(ino, de);
    }
    v.generation = r.generation;
}

/// Runs `f` on every node of a mount (`devfs_rule_applydm`); returns the
/// paths of those it hid or showed, whose names the caller forgets.
fn apply_all(r: &Registry, v: &mut MountView, id: u64, f: &dyn Fn(&mut Entry<'_>)) -> Vec<Vec<u8>> {
    let mut changed = Vec::new();
    for (&ino, de) in &mut v.des {
        let Some(n) = r.nodes.get(&ino).filter(|n| shown(n, id)) else {
            continue;
        };
        let was = de.hidden;
        run_on(r, ino, n, de, f);
        if was != de.hidden
            && let Some(p) = r.path(ino)
        {
            changed.push(p);
        }
    }
    changed
}

impl DevMount {
    /// Runs `f` on the registry and this mount's view of it, brought up
    /// to date. Lock order: the registry, a view, the rules.
    fn with<R>(&self, f: impl FnOnce(&mut Registry, &mut MountView) -> R) -> R {
        with(|r| {
            let mut v = self.view.lock();
            {
                let rules = RULES.lock();
                populate(r, &rules, &mut v, self.id);
            }
            f(r, &mut v)
        })
    }

    /// Whether `ino` shows on this mount (not hidden, nor another's).
    fn visible(r: &Registry, v: &MountView, ino: u64, id: u64) -> bool {
        r.nodes.get(&ino).is_some_and(|n| shown(n, id))
            && v.des.get(&ino).is_some_and(|d| !d.hidden)
    }
}

/// devfs, as a file system: one mount of it.
pub(crate) struct DevFs(Arc<DevMount>);

impl DevFs {
    /// A new mount using ruleset `rsnum` (made if it does not exist; 0 is
    /// none), as `mount -o ruleset=` does (`devfs_ruleset_set`).
    pub(crate) fn new(rsnum: u16) -> Self {
        RULES.lock().use_set(0, rsnum);
        Self(Arc::new(DevMount {
            id: NEXT_MOUNT.fetch_add(1, Ordering::Relaxed),
            view: SpinLock::new(MountView {
                ruleset: rsnum,
                generation: 0,
                des: BTreeMap::new(),
            }),
        }))
    }

    /// `mount -u -o ruleset=`: uses ruleset `rsnum` and applies it to the
    /// nodes the mount has (`devfs_ruleset_set`, `devfs_ruleset_apply`).
    pub(crate) fn set_ruleset(&self, rsnum: u16) {
        let dm = &self.0;
        let changed = with(|r| {
            let mut v = dm.view.lock();
            let mut rules = RULES.lock();
            populate(r, &rules, &mut v, dm.id);
            rules.use_set(v.ruleset, rsnum);
            v.ruleset = rsnum;
            if rsnum == 0 || !rules.exists(rsnum) {
                return Vec::new();
            }
            let depth = RULE_DEPTH.load(Ordering::Relaxed);
            apply_all(r, &mut v, dm.id, &|e| rules.apply_set(rsnum, e, depth))
        });
        forget_changed(dm, &changed);
    }
}

/// Forgets the names of nodes a rule hid or showed on mount `dm`.
fn forget_changed(dm: &Arc<DevMount>, paths: &[Vec<u8>]) {
    if paths.is_empty() {
        return;
    }
    for m in mounts() {
        let mine = m
            .fs()
            .as_any()
            .downcast_ref::<DevFs>()
            .is_some_and(|f| Arc::ptr_eq(&f.0, dm));
        if mine {
            for p in paths {
                forget_names_in(&m, p);
            }
        }
    }
}

fn get_u32(arg: &[u8]) -> u32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_ne_bytes)
}

fn get_u16(arg: &[u8]) -> u16 {
    arg.get(..2)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u16::from_ne_bytes)
}

/// A request on a devfs directory (`devfs_rioctl`, `devfs_rules_ioctl`):
/// the `DEVFSIO_*` rule and ruleset requests, which need
/// `PRIV_DEVFS_RULE` whatever they are. `ENOTTY` for anything else, and
/// on any other file system.
pub(crate) fn rule_ioctl(v: &Vnode, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    use abi::devfs::*;
    let Some(fs) = v.mount().fs().as_any().downcast_ref::<DevFs>() else {
        return Err(Errno::ENOTTY);
    };
    crate::fs::priv_check(crate::cred::Priv::DevfsRule)?;
    let dm = &fs.0;
    let rule = |arg: &[u8]| DevfsRule::from_bytes(arg).ok_or(Errno::EINVAL);
    let (result, changed) = with(|r| {
        let mut view = dm.view.lock();
        let mut rules = RULES.lock();
        populate(r, &rules, &mut view, dm.id);
        let cur = view.ruleset;
        let depth = RULE_DEPTH.load(Ordering::Relaxed);
        let mut changed = Vec::new();
        let result = match req {
            DEVFSIO_RADD => rule(arg).and_then(|mut dr| {
                rule_input(&mut dr, cur)?;
                rules.add(&dr)?;
                dr.to_bytes(arg);
                Ok(())
            }),
            DEVFSIO_RAPPLY => rule(arg).and_then(|mut dr| {
                rule_input(&mut dr, cur)?;
                if dr.dr_iacts & DRA_INCSET != 0 && !rules.exists(dr.dr_incset) {
                    return Err(Errno::ESRCH);
                }
                changed = apply_all(r, &mut view, dm.id, &|e| rules.run(&dr, e, depth));
                Ok(())
            }),
            DEVFSIO_RAPPLYID => {
                let rid = rid_input(get_u32(arg), cur);
                match rules.get(rid).copied() {
                    Some(dr) => {
                        changed = apply_all(r, &mut view, dm.id, &|e| rules.run(&dr, e, depth));
                        Ok(())
                    }
                    None => Err(Errno::ENOENT),
                }
            }
            DEVFSIO_RDEL => rules.delete(rid_input(get_u32(arg), cur)),
            DEVFSIO_RGETNEXT => rule(arg).and_then(|mut dr| {
                rule_input(&mut dr, cur)?;
                rules.next_rule(dr.dr_id)?.to_bytes(arg);
                Ok(())
            }),
            DEVFSIO_SUSE => {
                let rs = get_u16(arg);
                rules.use_set(cur, rs);
                view.ruleset = rs;
                Ok(())
            }
            DEVFSIO_SAPPLY => {
                let rs = rid2rsn(rid_input(mkrid(get_u16(arg), 0), cur));
                if rules.exists(rs) {
                    changed = apply_all(r, &mut view, dm.id, &|e| rules.apply_set(rs, e, depth));
                    Ok(())
                } else {
                    Err(Errno::ESRCH)
                }
            }
            DEVFSIO_SGETNEXT => match rules.next_set(get_u16(arg)) {
                Some(n) => {
                    if let Some(a) = arg.get_mut(..2) {
                        a.copy_from_slice(&n.to_ne_bytes());
                    }
                    Ok(())
                }
                None => Err(Errno::ENOENT),
            },
            _ => Err(Errno::ENOTTY),
        };
        (result, changed)
    });
    forget_changed(dm, &changed);
    result.map(|()| 0)
}

/// Adds `vfs.devfs.rule_depth` under `vfs`.
pub(crate) fn sysctl_init(t: &mut sysctl::Tree, vfs: sysctl::NodeId) {
    let Ok(devfs) = t.add_node(vfs, sysctl::Number::Auto, "devfs", "DEVFS filesystem") else {
        return;
    };
    let _ = t.add_writable(
        devfs,
        sysctl::Number::Auto,
        "rule_depth",
        "Max depth of ruleset include",
        0,
        sysctl::Value::Uint(Box::new(|| RULE_DEPTH.load(Ordering::Relaxed))),
        Arc::new(|b: &[u8]| {
            let v = b
                .first_chunk::<4>()
                .map(|c| u32::from_ne_bytes(*c))
                .ok_or(Errno::EINVAL)?;
            RULE_DEPTH.store(v, Ordering::Relaxed);
            Ok(())
        }),
    );
}

/// One node, as one mount's vnode holds it.
struct View {
    m: Arc<DevMount>,
    ino: u64,
}

impl View {
    fn node<R>(&self, f: impl FnOnce(&DevNode) -> R) -> Result<R, Errno> {
        with(|r| r.nodes.get(&self.ino).map(f)).ok_or(Errno::ENOENT)
    }
}

impl Node for View {
    fn attr(&self) -> Attr {
        self.m
            .with(|r, v| {
                let n = r.nodes.get(&self.ino)?;
                let mut a = n.attr;
                if let Some(de) = v.des.get(&self.ino) {
                    a.mode = de.mode;
                    a.uid = de.uid;
                    a.gid = de.gid;
                    if !matches!(n.what, What::Dev(_)) {
                        [a.atime, a.mtime, a.ctime, a.birthtime] = de.times;
                    }
                }
                a.size = match &n.what {
                    What::Dir(_) => 512,
                    What::Link(t) => t.len() as u64,
                    What::Dev(_) => 0,
                };
                Some(a)
            })
            .unwrap_or_default()
    }

    /// `devfs_setattr`: the mode, owner, and group are this mount's; a
    /// device's times are the device's.
    fn setattr(&mut self, _s: &Sleepable, a: &SetAttr) -> Result<(), Errno> {
        if a.flags.is_some() || a.size.is_some() {
            return Err(Errno::EOPNOTSUPP);
        }
        let ino = self.ino;
        self.m.with(|r, v| {
            let n = r.nodes.get_mut(&ino).ok_or(Errno::ENOENT)?;
            let de = v.des.get_mut(&ino).ok_or(Errno::ENOENT)?;
            let t = super::now();
            if let Some(m) = a.mode {
                de.mode = m & 0o7777;
            }
            if let Some(u) = a.uid {
                de.uid = u;
            }
            if let Some(g) = a.gid {
                de.gid = g;
            }
            let times = if matches!(n.what, What::Dev(_)) {
                [&mut n.attr.atime, &mut n.attr.mtime, &mut n.attr.ctime]
            } else {
                let [at, mt, ct, _] = &mut de.times;
                [at, mt, ct]
            };
            let [at, mt, ct] = times;
            if let Some(x) = a.atime {
                *at = x;
            }
            if let Some(x) = a.mtime {
                *mt = x;
            }
            *ct = t;
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
    /// A hidden node, or another mount's link, is not there.
    fn lookup(&self, _s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        let (ino, id) = (self.ino, self.m.id);
        self.m.with(|r, v| {
            let child = match &r.nodes.get(&ino).ok_or(Errno::ENOENT)?.what {
                What::Dir(m) => *m.get(name).ok_or(Errno::ENOENT)?,
                _ => return Err(Errno::ENOTDIR),
            };
            if DevMount::visible(r, v, child, id) {
                Ok(child)
            } else {
                Err(Errno::ENOENT)
            }
        })
    }

    fn parent(&self) -> u64 {
        self.node(|n| n.parent).unwrap_or(ROOT_INO)
    }

    /// `devfs_symlink`: only symbolic links may be made, by the
    /// privileged, and only this mount shows them.
    fn create(
        &mut self,
        _s: &Sleepable,
        name: &[u8],
        node: &NewNode,
        c: &Create,
    ) -> Result<Made, Errno> {
        let NewNode::Symlink(target) = node else {
            return Err(Errno::EOPNOTSUPP);
        };
        crate::fs::priv_check(crate::cred::Priv::DevfsSymlink)?;
        let (dir, id) = (self.ino, self.m.id);
        self.m.with(|r, v| {
            let ino = r.next;
            r.next += 1;
            let DevNode {
                what: What::Dir(m), ..
            } = r.nodes.get_mut(&dir).ok_or(Errno::ENOENT)?
            else {
                return Err(Errno::ENOTDIR);
            };
            m.insert(name.into(), ino);
            if let Some(de) = v.des.get_mut(&dir) {
                de.times[1] = c.time;
                de.times[2] = c.time;
            }
            r.nodes.insert(
                ino,
                DevNode {
                    parent: dir,
                    attr: attr(c.mode, c.uid, c.gid, 1, c.time),
                    what: What::Link(target.clone()),
                    dsw: 0,
                    owner: Some(id),
                },
            );
            r.generation += 1;
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

    /// `devfs_remove`: a link this mount made goes; anything else is
    /// hidden on this mount.
    fn remove(&mut self, _s: &Sleepable, name: &[u8], _child: &mut dyn Node) -> Result<(), Errno> {
        let (dir, id) = (self.ino, self.m.id);
        self.m.with(|r, v| {
            let ino = match &r.nodes.get(&dir).ok_or(Errno::ENOENT)?.what {
                What::Dir(m) => *m.get(name).ok_or(Errno::ENOENT)?,
                _ => return Err(Errno::ENOTDIR),
            };
            if !DevMount::visible(r, v, ino, id) {
                return Err(Errno::ENOENT);
            }
            if r.nodes.get(&ino).is_some_and(|n| n.owner == Some(id)) {
                if let Some(DevNode {
                    what: What::Dir(m), ..
                }) = r.nodes.get_mut(&dir)
                {
                    m.remove(name);
                }
                r.nodes.remove(&ino);
                v.des.remove(&ino);
                r.generation += 1;
            } else if let Some(de) = v.des.get_mut(&ino) {
                de.hidden = true;
            }
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

    /// The nodes this mount shows; `.` and `..` too unless hidden.
    fn readdir(
        &self,
        _s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        let (dir, id) = (self.ino, self.m.id);
        // A snapshot, so nothing is held while the caller copies.
        let (parent, dots, entries) = self.m.with(|r, v| {
            let n = r.nodes.get(&dir).ok_or(Errno::ENOENT)?;
            let What::Dir(m) = &n.what else {
                return Err(Errno::ENOTDIR);
            };
            let entries: Vec<(Box<[u8]>, u64, Kind)> = m
                .iter()
                .filter(|&(_, &i)| DevMount::visible(r, v, i, id))
                .map(|(name, &i)| {
                    (
                        name.clone(),
                        i,
                        r.nodes.get(&i).map_or(Kind::CharDevice, DevNode::kind),
                    )
                })
                .collect();
            let dots = v.des.get(&dir).map_or([false; 2], |d| d.dots);
            Ok((n.parent, dots, entries))
        })?;
        let mut list: Vec<(&[u8], u64, Kind)> = Vec::new();
        if !dots[0] {
            list.push((b".", dir, Kind::Directory));
        }
        if !dots[1] {
            list.push((b"..", parent, Kind::Directory));
        }
        list.extend(entries.iter().map(|(n, i, k)| (&n[..], *i, *k)));
        let skip = usize::try_from(from).unwrap_or(usize::MAX);
        for (i, (name, ino, kind)) in list.into_iter().enumerate().skip(skip) {
            if !f(DirEntry {
                name,
                ino,
                kind,
                next: i as u64 + 1,
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
        let v = Box::new(View {
            m: Arc::clone(&self.0),
            ino,
        });
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

/// Records devfs mount `m`, and the console's vnode, which the boot
/// processes' descriptors need.
pub(crate) fn mounted(s: &Sleepable, m: &Arc<Mount>) {
    let _ = MOUNT.set(Arc::clone(m));
    let Ok(root) = m.root(s) else {
        return;
    };
    if let Ok(c) = vfs::namei::Namespace::child(s, &root, b"console") {
        let _ = CONSOLE.set(c);
    }
}

/// The console's vnode.
pub(crate) fn console() -> Option<Arc<Vnode>> {
    CONSOLE.get().cloned()
}

#[cfg(feature = "ktest")]
mod ktests {
    use alloc::vec;

    use abi::devfs::*;
    use ktest::ktest;
    use vfs::MountInfo;
    use vfs::namei::Namespace;

    use super::*;

    fn cx() -> Sleepable {
        // SAFETY: tests run in threads, holding no spin locks.
        unsafe { Sleepable::assume() }
    }

    /// A second devfs, mounted on a new directory under `/tmp`.
    struct Second {
        m: Arc<Mount>,
    }

    impl Second {
        /// Mounted over a directory of the root that is no mount point
        /// (the test root is read-only) until [`Second::unmount`].
        fn mount(s: &Sleepable, rsnum: u16) -> Self {
            let m = Mount::new(
                MountInfo {
                    type_name: "devfs",
                    flags: abi::mount::MNT_LOCAL,
                    from: b"devfs".to_vec(),
                    io_size: 512,
                },
                Box::new(DevFs::new(rsnum)),
            );
            // The first that is a directory and not covered already.
            let mounted = [&b"/mnt"[..], b"/media", b"/root", b"/etc"]
                .iter()
                .any(|on| {
                    Namespace::child(s, &crate::fs::root(), &on[1..]).is_ok_and(|d| {
                        super::super::namespace()
                            .mount(s, d, Arc::clone(&m), on)
                            .is_ok()
                    })
                });
            assert!(mounted, "no directory to mount on");
            Self { m }
        }

        fn root(&self, s: &Sleepable) -> Arc<Vnode> {
            self.m.root(s).unwrap()
        }

        fn ask(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
            rule_ioctl(&self.root(s), req, arg)
        }

        fn unmount(self, s: &Sleepable) {
            super::super::namespace()
                .unmount(s, &self.m, true)
                .expect("unmount");
        }
    }

    fn names(s: &Sleepable, dir: &Arc<Vnode>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        dir.readdir(s, 0, &mut |e| {
            out.push(e.name.to_vec());
            true
        })
        .unwrap();
        out
    }

    fn rule(rid: u32, path: &[u8], iacts: i32, bacts: i32) -> [u8; DEVFS_RULE_LEN as usize] {
        let mut r = DevfsRule {
            dr_id: rid,
            dr_iacts: iacts,
            dr_bacts: bacts,
            ..DevfsRule::default()
        };
        if !path.is_empty() {
            r.dr_icond = DRC_PATHPTRN;
            r.dr_pathptrn[..path.len()].copy_from_slice(path);
        }
        let mut b = [0u8; DEVFS_RULE_LEN as usize];
        r.to_bytes(&mut b);
        b
    }

    fn include(rid: u32, set: u16) -> [u8; DEVFS_RULE_LEN as usize] {
        let r = DevfsRule {
            dr_id: rid,
            dr_iacts: DRA_INCSET,
            dr_incset: set,
            ..DevfsRule::default()
        };
        let mut b = [0u8; DEVFS_RULE_LEN as usize];
        r.to_bytes(&mut b);
        b
    }

    fn found(s: &Sleepable, dir: &Arc<Vnode>, name: &[u8]) -> bool {
        Namespace::child(s, dir, name).is_ok()
    }

    ktest! {
        fn devfs_rules_hide_and_show_nodes_per_mount() {
            let s = cx();
            let d = Second::mount(&s, 0);
            let root = d.root(&s);
            assert!(found(&s, &root, b"zero"));
            // Ruleset 0 is no one's to change; a rule must be this
            // version's.
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut rule(mkrid(0, 0), b"", DRA_BACTS, DRB_HIDE)), Err(Errno::EIO));
            let mut bad = rule(mkrid(20, 0), b"", 0, 0);
            bad[..4].copy_from_slice(&0u32.to_ne_bytes());
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut bad), Err(Errno::ERPCMISMATCH));
            // Ruleset 20: everything hidden but null and pts (FreeBSD's
            // devfsrules_hide_all and a little of unhide_basic).
            assert_eq!(d.ask(&s, DEVFSIO_SUSE, &mut 20u16.to_ne_bytes()), Ok(0));
            let mut r = rule(mkrid(0, 0), b"", DRA_BACTS, DRB_HIDE);
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut r), Ok(0));
            assert_eq!(rid2rsn(DevfsRule::from_bytes(&r).unwrap().dr_id), 20, "ruleset 0 means the mount's");
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut rule(0, b"null", DRA_BACTS, DRB_UNHIDE)), Ok(0));
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut rule(mkrid(20, 200), b"", 0, 0)), Err(Errno::EEXIST));
            // The rules, by number.
            let mut at = rule(mkrid(20, 0), b"", 0, 0);
            let mut nums = Vec::new();
            while d.ask(&s, DEVFSIO_RGETNEXT, &mut at).is_ok() {
                nums.push(rid2rn(DevfsRule::from_bytes(&at).unwrap().dr_id));
            }
            assert_eq!(nums, [100, 200]);
            let mut n = 19u16.to_ne_bytes();
            assert_eq!(d.ask(&s, DEVFSIO_SGETNEXT, &mut n), Ok(0));
            assert_eq!(u16::from_ne_bytes(n), 20);
            assert_eq!(d.ask(&s, DEVFSIO_SAPPLY, &mut 21u16.to_ne_bytes()), Err(Errno::ESRCH));
            // Applied: only null shows, without `.` and `..` (the first
            // rule hid them too), and /dev is as it was.
            assert_eq!(d.ask(&s, DEVFSIO_SAPPLY, &mut 0u16.to_ne_bytes()), Ok(0));
            assert_eq!(names(&s, &root), vec![b"null".to_vec()]);
            assert!(!found(&s, &root, b"zero"));
            let dev = MOUNT.get().unwrap().root(&s).unwrap();
            assert!(found(&s, &dev, b"zero"));
            assert!(names(&s, &dev).iter().any(|n| n == b"."));
            // A device made now goes through the mount's ruleset.
            add(b"ktest-devfs", 0o600, 0, 0, Arc::new(Null)).unwrap();
            assert!(!found(&s, &root, b"ktest-devfs"));
            assert!(found(&s, &dev, b"ktest-devfs"));
            remove(&s, b"ktest-devfs");
            // One rule, applied by itself.
            assert_eq!(d.ask(&s, DEVFSIO_RAPPLY, &mut rule(0, b"zero", DRA_BACTS, DRB_UNHIDE)), Ok(0));
            assert!(found(&s, &root, b"zero"));
            assert_eq!(d.ask(&s, DEVFSIO_RAPPLYID, &mut mkrid(20, 100).to_ne_bytes()), Ok(0));
            assert!(!found(&s, &root, b"zero"));
            assert_eq!(d.ask(&s, DEVFSIO_RAPPLYID, &mut mkrid(20, 150).to_ne_bytes()), Err(Errno::ENOENT));
            // Emptied and unused, the ruleset goes.
            assert_eq!(d.ask(&s, DEVFSIO_RDEL, &mut mkrid(0, 100).to_ne_bytes()), Ok(0));
            assert_eq!(d.ask(&s, DEVFSIO_RDEL, &mut mkrid(0, 100).to_ne_bytes()), Err(Errno::ENOENT));
            assert_eq!(d.ask(&s, DEVFSIO_RDEL, &mut mkrid(20, 200).to_ne_bytes()), Ok(0));
            assert_eq!(d.ask(&s, DEVFSIO_SUSE, &mut 0u16.to_ne_bytes()), Ok(0));
            assert!(!RULES.lock().exists(20));
            assert_eq!(d.ask(&s, 0x2000_7700, &mut []), Err(Errno::ENOTTY));
            drop((root, dev));
            d.unmount(&s);
        }
    }

    ktest! {
        fn devfs_attributes_are_each_mounts_own() {
            let s = cx();
            let d = Second::mount(&s, 0);
            let root = d.root(&s);
            let dev = MOUNT.get().unwrap().root(&s).unwrap();
            let cred = crate::cred::root_shared();
            let null2 = Namespace::child(&s, &root, b"null").unwrap();
            let a = SetAttr {
                mode: Some(0o600),
                uid: Some(5),
                ..SetAttr::default()
            };
            vfs::ops::setattr(&s, &null2, &a, &cred, false).unwrap();
            let null1 = Namespace::child(&s, &dev, b"null").unwrap();
            assert_eq!((null2.attr(&s).unwrap().mode & 0o7777, null2.attr(&s).unwrap().uid), (0o600, 5));
            assert_eq!((null1.attr(&s).unwrap().mode & 0o7777, null1.attr(&s).unwrap().uid), (0o666, 0));
            // A rule's mode, on this mount alone.
            let mut r = rule(mkrid(21, 0), b"zero", DRA_MODE, 0);
            let mut dr = DevfsRule::from_bytes(&r).unwrap();
            dr.dr_mode = 0o640;
            dr.to_bytes(&mut r);
            assert_eq!(d.ask(&s, DEVFSIO_RAPPLY, &mut r), Ok(0));
            let zero2 = Namespace::child(&s, &root, b"zero").unwrap();
            let zero1 = Namespace::child(&s, &dev, b"zero").unwrap();
            assert_eq!(zero2.attr(&s).unwrap().mode & 0o7777, 0o640);
            assert_eq!(zero1.attr(&s).unwrap().mode & 0o7777, 0o666);
            // Removing hides it here; a link made here shows here only.
            drop(null2);
            vfs::ops::remove(&s, &root, b"null", &cred, false).unwrap();
            assert!(!found(&s, &root, b"null"));
            assert!(found(&s, &dev, b"null"));
            let c = Create {
                mode: 0o755,
                uid: 0,
                gid: 0,
                time: super::super::now(),
            };
            vfs::ops::create(&s, &root, b"ktest-link", &NewNode::Symlink(b"zero".to_vec()), &c, &cred, true)
                .unwrap();
            assert!(found(&s, &root, b"ktest-link"));
            assert!(!found(&s, &dev, b"ktest-link"));
            vfs::ops::remove(&s, &root, b"ktest-link", &cred, false).unwrap();
            assert!(!found(&s, &root, b"ktest-link"));
            drop((root, dev, null1, zero1, zero2));
            d.unmount(&s);
        }
    }

    ktest! {
        fn devfs_ruleset_includes_are_checked_and_bounded() {
            let s = cx();
            let d = Second::mount(&s, 0);
            let root = d.root(&s);
            // An include must name a ruleset that exists.
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut include(mkrid(30, 0), 31)), Err(Errno::ESRCH));
            assert!(!RULES.lock().exists(30));
            let mut r = include(mkrid(30, 0), 31);
            let mut dr = DevfsRule::from_bytes(&r).unwrap();
            dr.dr_iacts |= DRA_INCSET;
            dr.to_bytes(&mut r);
            assert_eq!(d.ask(&s, DEVFSIO_RAPPLY, &mut r), Err(Errno::ESRCH));
            // 31 hides null and includes 30; 30 includes 31: a loop, cut at
            // vfs.devfs.rule_depth.
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut rule(mkrid(31, 0), b"null", DRA_BACTS, DRB_HIDE)), Ok(0));
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut include(mkrid(30, 0), 31)), Ok(0));
            assert_eq!(d.ask(&s, DEVFSIO_RADD, &mut include(mkrid(31, 0), 30)), Ok(0));
            assert_eq!(d.ask(&s, DEVFSIO_SAPPLY, &mut 30u16.to_ne_bytes()), Ok(0));
            assert!(!found(&s, &root, b"null"));
            // An included ruleset stays while included, though empty.
            assert_eq!(d.ask(&s, DEVFSIO_RDEL, &mut mkrid(31, 100).to_ne_bytes()), Ok(0));
            assert_eq!(d.ask(&s, DEVFSIO_RDEL, &mut mkrid(31, 200).to_ne_bytes()), Ok(0));
            assert!(RULES.lock().exists(31));
            assert_eq!(d.ask(&s, DEVFSIO_RDEL, &mut mkrid(30, 100).to_ne_bytes()), Ok(0));
            assert!(!RULES.lock().exists(30) && !RULES.lock().exists(31));
            drop(root);
            d.unmount(&s);
        }
    }
}
