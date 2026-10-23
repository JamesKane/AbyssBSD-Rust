// SPDX-License-Identifier: BSD-2-Clause
//! Files: the namespace mounted at boot, open files, descriptor tables,
//! devfs, and pipes.
//!
//! The root file system is what `vfs.root.mountfrom` names (the loader
//! sets it), mounted read-only as FreeBSD mounts root first: UFS2 on a
//! disk, which may be named by its GPT label or GUID (`/dev/gpt/rootfs0`,
//! `/dev/gptid/…`). Without one, the memory disk is the root, mounted as
//! tarfs (read-only), and without that an empty directory, so devfs and
//! the console still work. devfs goes on `/dev`, tmpfs on `/tmp`, and the
//! EFI System Partition on `/boot/efi`.
//!
//! Behavior reference: sys/kern/vfs_mount.c (`vfs_getnewfsid`),
//! sys/kern/vfs_mountroot.c.

pub(crate) mod block;
pub(crate) mod devfs;
pub(crate) mod fdtable;
pub(crate) mod fifo;
pub(crate) mod file;
pub(crate) mod msdosfs;
pub(crate) mod pipe;
pub(crate) mod socket;
pub(crate) mod ufs;
pub(crate) mod uio;

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::fcntl::{AT_FDCWD, FREAD, FWRITE};
use abi::mount::{MNT_LOCAL, MNT_RDONLY, MNT_ROOTFS};
use abi::time::Timespec;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::{Once, PreemptLock};
use vfs::namei::{Found, Lookup, Namespace, Parent};
use vfs::tree::Builder;
use vfs::{Cred, Meta, Mount, MountInfo, NodeId, Vnode};

use self::fdtable::{FdFlags, FdTable};
use self::file::{File, FileKind, VnodeFile};
use crate::cred::Ucred;

static NAMESPACE: Once<Namespace> = Once::new();

/// Held while [`shutdown`] unmounts, before any file system lock.
static SHUTDOWN_LOCK: LockClass = LockClass::new("fs shutdown", 50);
static SHUTDOWN: Mutex<bool> = Mutex::new(&SHUTDOWN_LOCK, false);

/// The time now, for file times.
pub(crate) fn now() -> Timespec {
    let ns = crate::shared_page::to_ns(crate::shared_page::realtime());
    Timespec {
        tv_sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from(ns % 1_000_000_000).unwrap_or(0),
    }
}

/// The credential file operations run with: the current process's, or
/// the superuser's for kernel threads.
pub(crate) fn cred() -> Arc<Ucred> {
    crate::proc::Process::current().map_or_else(Ucred::root_shared, |p| p.cred())
}

fn root_info(from: &[u8]) -> MountInfo {
    MountInfo {
        type_name: "tarfs",
        flags: MNT_RDONLY | MNT_LOCAL | MNT_ROOTFS,
        from: from.to_vec(),
        io_size: 4096,
    }
}

/// An empty, read-only root holding only `/dev`.
fn empty_root() -> Arc<Mount> {
    let meta = Meta::root(0o755, now());
    let mut b = Builder::new(meta);
    let _ = b.dir(b"dev", meta);
    Mount::new(root_info(b"none"), Box::new(b.finish()))
}

/// How long the root's disk may take to appear (FreeBSD's
/// `vfs.mountroot.timeout`).
const ROOT_WAIT_NS: u64 = 3_000_000_000;

/// The root file system `spec` (`ufs:/dev/gpt/rootfs0`) names, read-only,
/// and its disk; the disk may take [`ROOT_WAIT_NS`] to appear (FreeBSD's
/// `parse_mount`).
fn mount_root_from(s: &Sleepable, spec: &str) -> Result<(Arc<Mount>, Arc<storage::Disk>), Errno> {
    let (fstype, dev) = spec.split_once(':').ok_or(Errno::EINVAL)?;
    kprintln!("Trying to mount root from {fstype}:{dev} []...");
    if fstype != "ufs" {
        return Err(Errno::ENOENT);
    }
    let name = dev.strip_prefix("/dev/").unwrap_or(dev);
    let start = crate::time::now_ns();
    let disk = loop {
        if let Some(d) = storage::find(name) {
            break d;
        }
        if crate::time::now_ns().saturating_sub(start) >= ROOT_WAIT_NS {
            return Err(Errno::ENXIO);
        }
        crate::time::sleep_ns(s, 100_000_000);
    };
    let fs = ufs::UfsFs::mount(s, Arc::clone(&disk), true, false)?;
    let io_size = fs.block_size();
    let m = Mount::new(
        MountInfo {
            type_name: "ufs",
            flags: MNT_RDONLY | MNT_LOCAL | MNT_ROOTFS,
            from: dev.as_bytes().to_vec(),
            io_size,
        },
        Box::new(fs),
    );
    Ok((m, disk))
}

/// The root file system (see the module comment), and the disk it is on
/// if one.
fn mount_root(s: &Sleepable) -> (Arc<Mount>, Option<Arc<storage::Disk>>) {
    if let Some(spec) = crate::boot::value("vfs.root.mountfrom") {
        match mount_root_from(s, spec) {
            Ok((m, disk)) => return (m, Some(disk)),
            Err(e) => kprintln!("Mounting from {spec} failed with error {}.", e.get()),
        }
    }
    let root = match crate::md::archive() {
        Some(a) => match vfs::tarfs::mount(a) {
            Ok(t) => Mount::new(root_info(b"/dev/md0"), Box::new(t)),
            Err(e) => {
                kprintln!("fs: memory disk not mountable ({e:?}); empty root");
                empty_root()
            }
        },
        None => empty_root(),
    };
    (root, None)
}

/// Mounts the root file system and devfs. Runs once, after the memory
/// disk is found and before any process starts.
pub(crate) fn init() {
    vfs::set_clock(now);
    // SAFETY: boot runs in the first kernel thread, which may sleep.
    let s = unsafe { Sleepable::assume() };
    let (root, root_disk) = mount_root(&s);
    let ns = match Namespace::new(&s, root) {
        Ok(ns) => ns,
        Err(e) => {
            kprintln!("fs: root not mountable ({e:?}); empty root");
            match Namespace::new(&s, empty_root()) {
                Ok(ns) => ns,
                Err(_) => return,
            }
        }
    };
    devfs::init();
    crate::mdisk::init();
    let dev = Mount::new(
        MountInfo {
            type_name: "devfs",
            flags: MNT_LOCAL,
            from: b"devfs".to_vec(),
            io_size: 4096,
        },
        Box::new(devfs::DevFs),
    );
    let root = Arc::clone(ns.root());
    let at = Lookup {
        start: &root,
        root: &root,
        follow: true,
        beneath: false,
        create: false,
        cred: &Cred::root(),
    };
    match ns.lookup(&s, &at, b"/dev") {
        Ok(d) => match ns.mount(&s, d, Arc::clone(&dev), b"/dev") {
            Ok(()) => devfs::mounted(&s, &dev),
            Err(e) => kprintln!("fs: devfs not mounted on /dev ({e:?})"),
        },
        Err(e) => kprintln!("fs: no /dev on the root file system ({e:?}); devfs not mounted"),
    }
    // A tmpfs on /tmp, as a FreeBSD install with `tmpmfs` has.
    if let Ok(t) = ns.lookup(&s, &at, b"/tmp") {
        let tmp = Mount::new(
            MountInfo {
                type_name: "tmpfs",
                flags: MNT_LOCAL,
                from: b"tmpfs".to_vec(),
                io_size: 4096,
            },
            Box::new(vfs::tmpfs::TmpFs::new(
                0o1777,
                0,
                0,
                crate::mm::space::page_ops(),
            )),
        );
        if let Err(e) = ns.mount(&s, t, tmp, b"/tmp") {
            kprintln!("fs: tmpfs not mounted on /tmp ({e:?})");
        }
    }
    // The EFI System Partition on /boot/efi, read-write, as FreeBSD's
    // installer mounts it: the root disk's, or else the first disk's (the
    // boot disk's in the test machines).
    if let Ok(dir) = ns.lookup(&s, &at, b"/boot/efi") {
        let root_parent = root_disk
            .as_ref()
            .and_then(|d| d.partition().map(|(p, _)| Arc::clone(p)));
        let esps: alloc::vec::Vec<_> = storage::disk::all()
            .into_iter()
            .filter(|d| {
                d.partition()
                    .is_some_and(|(_, p)| p.type_guid == gpt::types::EFI_SYSTEM)
            })
            .collect();
        let on_root = esps.iter().find(|d| {
            d.partition()
                .is_some_and(|(p, _)| root_parent.as_ref().is_some_and(|r| Arc::ptr_eq(p, r)))
        });
        let esp = on_root.or(esps.first()).cloned();
        if let Some(esp) = esp {
            let name = alloc::format!("/dev/{}", esp.name());
            match msdosfs::MsdosFs::mount(&s, esp, false, msdosfs::Opts::default()) {
                Ok(fs) => {
                    let io_size = fs.cluster_size();
                    let m = Mount::new(
                        MountInfo {
                            type_name: "msdosfs",
                            flags: MNT_LOCAL,
                            from: name.clone().into_bytes(),
                            io_size,
                        },
                        Box::new(fs),
                    );
                    match ns.mount(&s, dir, m, b"/boot/efi") {
                        Ok(()) => kprintln!("fs: {name} on /boot/efi"),
                        Err(e) => kprintln!("fs: {name} not mounted on /boot/efi ({e:?})"),
                    }
                }
                Err(e) => kprintln!("fs: {name}: not mountable as msdosfs ({e:?})"),
            }
        }
    }
    let _ = NAMESPACE.set(ns);
    if sched::Builder::new("vnlru")
        .priority(sched::Priority::KERNEL)
        .spawn(vnlru)
        .is_err()
    {
        kprintln!("fs: no vnode reclaimer (out of memory)");
    }
    // Dirty file pages past which writers write their own back: a tenth
    // of memory (FreeBSD's `hidirtybuffers` is a share of buffer space).
    vfs::pagecache::set_dirty_limit(
        usize::try_from(crate::mm::phys_stats().managed / 10).unwrap_or(usize::MAX),
    );
    if sched::Builder::new("syncer")
        .priority(sched::Priority::KERNEL)
        .spawn(syncer)
        .is_err()
    {
        kprintln!("fs: no syncer (out of memory)");
    }
}

/// How long dirty data waits before the syncer writes it (FreeBSD's
/// `kern.metadelay` to `kern.filedelay`: 28 to 30 seconds; one delay for
/// all here).
const SYNC_DELAY_NS: u64 = 30_000_000_000;

/// The syncer (FreeBSD's `sched_sync`): once a second, each mount writes
/// what has been dirty longer than [`SYNC_DELAY_NS`].
fn syncer(s: &Sleepable) {
    loop {
        crate::time::sleep_ns(s, 1_000_000_000);
        let Some(ns) = NAMESPACE.get() else {
            continue;
        };
        let before = crate::time::now_ns().saturating_sub(SYNC_DELAY_NS);
        for m in ns.mounts() {
            if !m.read_only() {
                m.fs().sync_aged(s, before);
            }
        }
    }
}

/// The most vnodes kept in core (`kern.maxvnodes`), sized as FreeBSD sizes
/// `desiredvnodes`: one per 32 KiB of memory, one more per 32 KiB of the
/// first 1.5 GiB, plus room for processes.
pub(crate) fn max_vnodes() -> usize {
    let kib = crate::mm::phys_stats().managed * (addr::PAGE_SIZE / 1024);
    let n = kib / 32 + kib.min(98_304 * 16) / 32;
    usize::try_from(n).unwrap_or(usize::MAX).max(4096) + 1024
}

/// The vnode reclaimer (FreeBSD's `vnlru`): once a second, if more vnodes
/// are in core than [`max_vnodes`], each mount gives up its longest unused
/// idle ones, in proportion, down to nine tenths of the limit.
fn vnlru(s: &Sleepable) {
    loop {
        crate::time::sleep_ns(s, 1_000_000_000);
        let Some(ns) = NAMESPACE.get() else {
            continue;
        };
        let max = max_vnodes();
        let mounts = ns.mounts();
        let total: usize = mounts.iter().map(|m| m.vnodes()).sum();
        if total <= max {
            continue;
        }
        let excess = total - max * 9 / 10;
        for m in &mounts {
            let share = (excess * m.vnodes()).div_ceil(total);
            m.prune(s, share);
        }
    }
}

/// The file system types the kernel has, with FreeBSD's `VFS_SET` flags.
const TYPES: &[(&str, i32)] = &[
    ("devfs", abi::mount::VFCF_SYNTHETIC | abi::mount::VFCF_JAIL),
    ("tmpfs", abi::mount::VFCF_JAIL),
    ("tarfs", abi::mount::VFCF_JAIL | abi::mount::VFCF_READONLY),
    ("ufs", abi::mount::VFCF_FILEREVINC),
    ("msdosfs", 0),
];

/// Adds `vfs.conflist` (what `getvfsbyname` and `lsvfs` read) to the
/// sysctl tree.
pub(crate) fn sysctl_init(s: &Sleepable) {
    crate::sysctl::with(s, |t| {
        let root = t.root();
        let vfs = match t.child(root, "vfs") {
            Some(v) => v,
            None => t
                .add_node(
                    root,
                    sysctl::Number::Fixed(abi::sysctl::ctl::VFS),
                    "vfs",
                    "File system",
                )
                .expect("vfs node"),
        };
        t.add_handler(
            vfs,
            sysctl::Number::Auto,
            "conflist",
            "List of all configured filesystems",
            Arc::new(|rest: &[i32], out: &mut alloc::vec::Vec<u8>| {
                if !rest.is_empty() {
                    return Err(sysctl::ReadError::Invalid);
                }
                let mounts = NAMESPACE.get().map(Namespace::mounts).unwrap_or_default();
                for &(name, flags) in TYPES {
                    let mut x = abi::mount::Xvfsconf {
                        vfc_typenum: i32::try_from(vfs::type_num(name)).unwrap_or(0),
                        vfc_refcount: i32::try_from(
                            mounts.iter().filter(|m| m.type_name() == name).count(),
                        )
                        .unwrap_or(i32::MAX),
                        vfc_flags: flags,
                        ..abi::mount::Xvfsconf::default()
                    };
                    x.vfc_name[..name.len()].copy_from_slice(name.as_bytes());
                    out.extend_from_slice(crate::uaccess::bytes_of(&x));
                }
                Ok(())
            }),
        )
        .expect("vfs.conflist");
    });
}

/// The namespace. [`init`] runs before any process, and processes are
/// the only callers.
pub(crate) fn namespace() -> &'static Namespace {
    NAMESPACE
        .get()
        .unwrap_or_else(|| unreachable!("fs::init runs before processes start"))
}

/// Unmounts every file system, the root last, so each is left clean
/// (FreeBSD's `vfs_unmountall` at shutdown). Nothing may use files after.
pub(crate) fn shutdown(s: &Sleepable) {
    // Once: the power button's thread and the boot thread may both get
    // here, and the second waits for the first.
    let mut done = SHUTDOWN.lock(s);
    if core::mem::replace(&mut *done, true) {
        return;
    }
    let Some(ns) = NAMESPACE.get() else {
        return;
    };
    for (on, e) in ns.unmount_all(s) {
        kprintln!(
            "fs: unmounting {} failed ({e:?})",
            core::str::from_utf8(&on).unwrap_or("?")
        );
    }
}

/// The root directory.
pub(crate) fn root() -> Arc<Vnode> {
    Arc::clone(namespace().root())
}

/// Where a relative path starts: the working directory, or the directory
/// open on a descriptor (the `*at` calls' `fd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum At {
    Cwd,
    Fd(i32),
}

impl At {
    /// From an `*at` call's `fd` argument (`AT_FDCWD` for the working
    /// directory).
    pub(crate) const fn new(fd: i32) -> Self {
        if fd == AT_FDCWD {
            Self::Cwd
        } else {
            Self::Fd(fd)
        }
    }
}

/// How a path is walked.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Walk {
    /// Follow a symbolic link as the last component.
    pub(crate) follow: bool,
    /// Stay beneath the starting directory (`O_RESOLVE_BENEATH`).
    pub(crate) beneath: bool,
    /// A missing last component is about to be created (`O_CREAT`).
    pub(crate) create: bool,
}

impl Walk {
    /// The lookup this walk is, from `start`, as `cred`.
    fn lookup<'a>(self, start: &'a Arc<Vnode>, cred: &'a Cred) -> Lookup<'a> {
        Lookup {
            start,
            root: namespace().root(),
            follow: self.follow,
            beneath: self.beneath,
            create: self.create,
            cred,
        }
    }

    pub(crate) const FOLLOW: Self = Self {
        follow: true,
        beneath: false,
        create: false,
    };
    pub(crate) const NOFOLLOW: Self = Self {
        follow: false,
        beneath: false,
        create: false,
    };
}

/// Where `p`'s walk of `path` starts: the root is borrowed, not counted.
fn start_of(
    s: &Sleepable,
    p: &crate::proc::Process,
    at: At,
    path: &[u8],
) -> Result<Cow<'static, Arc<Vnode>>, Errno> {
    if path.first() == Some(&b'/') {
        return Ok(Cow::Borrowed(namespace().root()));
    }
    match at {
        At::Cwd => Ok(Cow::Owned(Arc::clone(p.cwd.lock().vnode()))),
        At::Fd(fd) => {
            let f = p.files.lock(s).get(fd)?;
            match f.vnode() {
                Some(v) if v.is_dir() => Ok(Cow::Owned(Arc::clone(v))),
                _ => Err(Errno::ENOTDIR),
            }
        }
    }
}

/// Looks `path` up for process `p` as `cred` (`access` checks with the
/// real ids); a missing last component is not an error.
pub(crate) fn resolve_as(
    s: &Sleepable,
    p: &crate::proc::Process,
    cred: &Cred,
    at: At,
    path: &[u8],
    walk: Walk,
) -> Result<Found, Errno> {
    let start = start_of(s, p, at, path)?;
    namespace().resolve(s, &walk.lookup(&start, cred), path)
}

/// The vnode at `path` for process `p`.
pub(crate) fn lookup(
    s: &Sleepable,
    p: &crate::proc::Process,
    at: At,
    path: &[u8],
    walk: Walk,
) -> Result<Arc<Vnode>, Errno> {
    let start = start_of(s, p, at, path)?;
    namespace().lookup(s, &walk.lookup(&start, &p.cred()), path)
}

/// The directory `path`'s last component is in, and that component, for
/// the calls that create or remove it.
pub(crate) fn parent_of(
    s: &Sleepable,
    p: &crate::proc::Process,
    at: At,
    path: &[u8],
    beneath: bool,
) -> Result<Parent, Errno> {
    let start = start_of(s, p, at, path)?;
    namespace().parent_of(
        s,
        &Lookup {
            start: &start,
            root: namespace().root(),
            follow: false,
            beneath,
            create: false,
            cred: &p.cred(),
        },
        path,
    )
}

/// A file whose pages come from memory the kernel keeps (the memory
/// disk's files).
struct ResidentPager {
    data: &'static [u8],
}

impl vm::object::Pager for ResidentPager {
    fn size(&self) -> u64 {
        (self.data.len() as u64).div_ceil(vm::PAGE_SIZE)
    }

    fn fill(&self, index: u64, page: &mut [u8]) -> Result<(), vm::object::PagerError> {
        let start = usize::try_from(index * vm::PAGE_SIZE).unwrap_or(usize::MAX);
        let src = self.data.get(start..).unwrap_or(&[]);
        let n = src.len().min(page.len());
        page[..n].copy_from_slice(&src[..n]);
        page[n..].fill(0);
        Ok(())
    }
}

/// Each mapped file's memory object, so every mapping of a file shares
/// its cached pages (FreeBSD's `v_object`). The file systems are never
/// unmounted, so entries stay.
static OBJECTS: PreemptLock<BTreeMap<NodeId, Arc<vm::object::Object>>> =
    PreemptLock::new(BTreeMap::new());

/// `v`'s memory object, if it is a regular file that can be mapped: its
/// file system's (tmpfs's), or for a memory-disk file one over its
/// resident contents, made at the first mapping and kept.
pub(crate) fn object(s: &Sleepable, v: &Vnode) -> Option<Arc<vm::object::Object>> {
    if let Some(o) = v.object(s) {
        return Some(o);
    }
    let data = v.resident(s)?;
    let mut objects = OBJECTS.lock();
    Some(Arc::clone(objects.entry(v.id()).or_insert_with(|| {
        vm::object::Object::with_pager(
            crate::mm::space::page_ops(),
            Arc::new(ResidentPager { data }),
        )
    })))
}

/// Descriptors of process `pid` holding `slots` were closed: its POSIX
/// locks on each file go, as closing any descriptor for a file releases
/// them (FreeBSD's `closef`).
pub(crate) fn closed<'a>(pid: i32, slots: impl IntoIterator<Item = &'a fdtable::Slot>) {
    for slot in slots {
        if let Some(v) = slot.file.vnode() {
            v.lockf().release(vfs::lockf::Owner::Process(pid));
        }
    }
}

/// A table with the console open on descriptors 0, 1, and 2, as init
/// gets it; empty if there is no console device.
pub(crate) fn console_table() -> FdTable {
    let mut t = FdTable::new();
    // SAFETY: tables are made in thread context (process creation).
    let s = unsafe { Sleepable::assume() };
    if let Some(c) = devfs::console()
        && let Ok(vf) = VnodeFile::open(&s, &c, true)
    {
        let f = File::new(FileKind::Vnode(vf), FREAD | FWRITE);
        for _ in 0..3 {
            let _ = t.alloc(Arc::clone(&f), FdFlags::default(), 0, 3);
        }
    }
    t
}
