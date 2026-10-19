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

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::fcntl::{AT_FDCWD, FREAD, FWRITE};
use abi::time::Timespec;
use sched::Sleepable;
use sync::{Once, SpinLock};
use vfs::namei::{Found, Lookup, Namespace};
use vfs::tree::Builder;
use vfs::{FileSystem, Meta, NodeId, Vnode};

use self::fdtable::{FdFlags, FdTable};
use self::file::{File, FileKind};

static NAMESPACE: Once<Namespace> = Once::new();

/// Mounts so far, for file system ids.
static MOUNTS: AtomicU64 = AtomicU64::new(0);

/// A new file system id for type `type_num`, as FreeBSD's
/// `vfs_getnewfsid` makes them: `makedev(255, type << 24 | mount)`.
fn new_fsid(type_num: u32) -> u64 {
    let n = MOUNTS.fetch_add(1, Ordering::Relaxed);
    u64::from(type_num & 0xff) << 24 | (n & 0xff00) << 8 | 0xff00 | (n & 0xff)
}

/// The time now, for file times.
pub(crate) fn now() -> Timespec {
    let ns = crate::shared_page::to_ns(crate::shared_page::realtime());
    Timespec {
        tv_sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
        tv_nsec: i64::try_from(ns % 1_000_000_000).unwrap_or(0),
    }
}

/// An empty, read-only root holding only `/dev`.
fn empty_root() -> Arc<Vnode> {
    let fs = Arc::new(FileSystem {
        id: new_fsid(vfs::type_num("tarfs")),
        type_name: "tarfs",
        type_num: vfs::type_num("tarfs"),
        flags: abi::mount::MNT_RDONLY | abi::mount::MNT_LOCAL | abi::mount::MNT_ROOTFS,
        from: b"none".to_vec(),
        io_size: 4096,
    });
    let meta = Meta::root(0o755, now());
    let mut b = Builder::new(fs, meta);
    let _ = b.dir(b"dev", meta);
    b.finish()
}

/// Mounts the root file system and devfs. Runs once, after the memory
/// disk is found and before any process starts.
pub(crate) fn init() {
    let tarfs = vfs::type_num("tarfs");
    let root = match crate::md::archive() {
        Some(a) => {
            let fs = Arc::new(FileSystem {
                id: new_fsid(tarfs),
                type_name: "tarfs",
                type_num: tarfs,
                flags: abi::mount::MNT_RDONLY | abi::mount::MNT_LOCAL | abi::mount::MNT_ROOTFS,
                from: b"/dev/md0".to_vec(),
                io_size: 4096,
            });
            match vfs::tarfs::mount(a, fs) {
                Ok(r) => r,
                Err(e) => {
                    kprintln!("fs: memory disk not mountable ({e:?}); empty root");
                    empty_root()
                }
            }
        }
        None => empty_root(),
    };
    let mut ns = Namespace::new(Arc::clone(&root));
    let dev = devfs::build(new_fsid(vfs::type_num("devfs")));
    let at = vfs::namei::Lookup {
        start: &root,
        root: &root,
        follow: true,
        beneath: false,
    };
    match ns.lookup(&at, b"/dev") {
        Ok(d) => {
            if let Err(e) = ns.mount(d, dev, b"/dev") {
                kprintln!("fs: devfs not mounted on /dev ({e:?})");
            }
        }
        Err(e) => kprintln!("fs: no /dev on the root file system ({e:?}); devfs not mounted"),
    }
    let _ = NAMESPACE.set(ns);
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
    let start = if path.first() == Some(&b'/') {
        Arc::clone(&root)
    } else {
        match at {
            At::Cwd => Arc::clone(&p.cwd.lock()),
            At::Fd(fd) => {
                let f = p.files.lock(s).get(fd)?;
                match f.vnode() {
                    Some(v) if v.is_dir() => Arc::clone(v),
                    _ => return Err(Errno::ENOTDIR),
                }
            }
        }
    };
    namespace().resolve(
        &Lookup {
            start: &start,
            root: &root,
            follow: walk.follow,
            beneath: walk.beneath,
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

/// A file whose pages come from memory the kernel keeps (the memory
/// disk's files).
struct ResidentPager {
    data: &'static [u8],
}

impl vm::object::Pager for ResidentPager {
    fn size(&self) -> u64 {
        (self.data.len() as u64).div_ceil(vm::PAGE_SIZE)
    }

    fn fill(&self, index: u64, page: &mut [u8]) {
        let start = usize::try_from(index * vm::PAGE_SIZE).unwrap_or(usize::MAX);
        let src = self.data.get(start..).unwrap_or(&[]);
        let n = src.len().min(page.len());
        page[..n].copy_from_slice(&src[..n]);
        page[n..].fill(0);
    }
}

/// Each mapped file's memory object, so every mapping of a file shares
/// its cached pages (FreeBSD's `v_object`). The file systems are never
/// unmounted, so entries stay.
static OBJECTS: SpinLock<BTreeMap<NodeId, Arc<vm::object::Object>>> =
    SpinLock::new(BTreeMap::new());

/// `v`'s memory object, if it is a regular file that can be mapped.
pub(crate) fn object(v: &Vnode) -> Option<Arc<vm::object::Object>> {
    let vfs::VnodeKind::Regular(f) = v.kind() else {
        return None;
    };
    let data = f.resident()?;
    let mut objects = OBJECTS.lock();
    Some(Arc::clone(objects.entry(v.id()).or_insert_with(|| {
        vm::object::Object::with_pager(
            crate::mm::space::page_ops(),
            Arc::new(ResidentPager { data }),
        )
    })))
}

/// A table with the console open on descriptors 0, 1, and 2, as init
/// gets it; empty if there is no console device.
pub(crate) fn console_table() -> FdTable {
    let mut t = FdTable::new();
    if let Some(c) = devfs::console() {
        let f = File::new(FileKind::Vnode(c), FREAD | FWRITE);
        for _ in 0..3 {
            let _ = t.alloc(Arc::clone(&f), FdFlags::default(), 0, 3);
        }
    }
    t
}
