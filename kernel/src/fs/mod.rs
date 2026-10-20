// SPDX-License-Identifier: BSD-2-Clause
//! Files: the namespace mounted at boot, open files, descriptor tables,
//! devfs, and pipes.
//!
//! The root file system is the memory disk, mounted as tarfs (read-only);
//! devfs goes on its `/dev`. Without a memory disk the root is an empty
//! directory, so devfs and the console still work.
//!
//! Behavior reference: sys/kern/vfs_mount.c (`vfs_getnewfsid`),
//! sys/kern/vfs_mountroot.c.

pub(crate) mod devfs;
pub(crate) mod fdtable;
pub(crate) mod file;
pub(crate) mod pipe;
pub(crate) mod uio;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::fcntl::{AT_FDCWD, FREAD, FWRITE};
use abi::mount::{MNT_LOCAL, MNT_RDONLY, MNT_ROOTFS};
use abi::time::Timespec;
use sched::Sleepable;
use sync::{Once, SpinLock};
use vfs::namei::{Found, Lookup, Namespace, Parent};
use vfs::tree::Builder;
use vfs::{Cred, Meta, Mount, MountInfo, NodeId, Vnode};

use self::fdtable::{FdFlags, FdTable};
use self::file::{File, FileKind, VnodeFile};

static NAMESPACE: Once<Namespace> = Once::new();

/// The time now, for file times.
pub(crate) fn now() -> Timespec {
    let ns = crate::shared_page::to_ns(crate::shared_page::realtime());
    Timespec {
        tv_sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from(ns % 1_000_000_000).unwrap_or(0),
    }
}

/// The credential file operations run with: every process is the
/// superuser until Phase 6 brings credentials.
pub(crate) fn cred() -> &'static Cred {
    static ROOT: Cred = Cred::root();
    &ROOT
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

/// Mounts the root file system and devfs. Runs once, after the memory
/// disk is found and before any process starts.
pub(crate) fn init() {
    vfs::set_clock(now);
    // SAFETY: boot runs in the first kernel thread, which may sleep.
    let s = unsafe { Sleepable::assume() };
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
        cred: cred(),
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
    let _ = NAMESPACE.set(ns);
    if sched::Builder::new("vnlru")
        .priority(sched::Priority::KERNEL)
        .spawn(vnlru)
        .is_err()
    {
        kprintln!("fs: no vnode reclaimer (out of memory)");
    }
}

/// The most vnodes kept in core (`kern.maxvnodes`), sized as FreeBSD sizes
/// `desiredvnodes`: one per 32 pages of memory, plus room for processes.
fn max_vnodes() -> usize {
    let pages = crate::mm::phys_stats().managed;
    usize::try_from(pages / 32).unwrap_or(usize::MAX).max(4096) + 1024
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
}

impl Walk {
    pub(crate) const FOLLOW: Self = Self {
        follow: true,
        beneath: false,
    };
    pub(crate) const NOFOLLOW: Self = Self {
        follow: false,
        beneath: false,
    };
}

/// Where `p`'s walk of `path` starts.
fn start_of(
    s: &Sleepable,
    p: &crate::proc::Process,
    at: At,
    path: &[u8],
) -> Result<Arc<Vnode>, Errno> {
    if path.first() == Some(&b'/') {
        return Ok(root());
    }
    match at {
        At::Cwd => Ok(Arc::clone(p.cwd.lock().vnode())),
        At::Fd(fd) => {
            let f = p.files.lock(s).get(fd)?;
            match f.vnode() {
                Some(v) if v.is_dir() => Ok(Arc::clone(v)),
                _ => Err(Errno::ENOTDIR),
            }
        }
    }
}

/// Looks `path` up for process `p`; a missing last component is not an
/// error.
pub(crate) fn resolve(
    s: &Sleepable,
    p: &crate::proc::Process,
    at: At,
    path: &[u8],
    walk: Walk,
) -> Result<Found, Errno> {
    let root = root();
    let start = start_of(s, p, at, path)?;
    namespace().resolve(
        s,
        &Lookup {
            start: &start,
            root: &root,
            follow: walk.follow,
            beneath: walk.beneath,
            cred: cred(),
        },
        path,
    )
}

/// The vnode at `path` for process `p`.
pub(crate) fn lookup(
    s: &Sleepable,
    p: &crate::proc::Process,
    at: At,
    path: &[u8],
    walk: Walk,
) -> Result<Arc<Vnode>, Errno> {
    match resolve(s, p, at, path, walk)? {
        Found::Vnode(v) => Ok(v),
        Found::Missing { .. } => Err(Errno::ENOENT),
    }
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
    let root = root();
    let start = start_of(s, p, at, path)?;
    namespace().parent_of(
        s,
        &Lookup {
            start: &start,
            root: &root,
            follow: false,
            beneath,
            cred: cred(),
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
static OBJECTS: SpinLock<BTreeMap<NodeId, Arc<vm::object::Object>>> =
    SpinLock::new(BTreeMap::new());

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
        && let Ok(vf) = VnodeFile::open(&s, &c)
    {
        let f = File::new(FileKind::Vnode(vf), FREAD | FWRITE);
        for _ in 0..3 {
            let _ = t.alloc(Arc::clone(&f), FdFlags::default(), 0, 3);
        }
    }
    t
}
