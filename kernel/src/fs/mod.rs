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
    let dev = Mount::new(
        MountInfo {
            type_name: "devfs",
            flags: MNT_LOCAL,
            from: b"devfs".to_vec(),
            io_size: 4096,
        },
        Box::new(devfs::build()),
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

/// `v`'s memory object, if it is a regular file that can be mapped (so
/// far, the memory disk's files; the page cache brings the rest in P5.3).
pub(crate) fn object(s: &Sleepable, v: &Vnode) -> Option<Arc<vm::object::Object>> {
    let data = v.resident(s)?;
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
