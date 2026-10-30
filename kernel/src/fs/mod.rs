// SPDX-License-Identifier: BSD-2-Clause
//! Files: the namespace mounted at boot, open files, descriptor tables,
//! devfs, and pipes.
//!
//! The root file system is what `vfs.root.mountfrom` names (the loader
//! sets it), mounted read-only as FreeBSD mounts root first: UFS2 on a
//! disk, which may be named by its GPT label or GUID (`/dev/gpt/rootfs0`,
//! `/dev/gptid/…`). Without one, the memory disk is the root, mounted as
//! tarfs (read-only), and without that an empty directory, so devfs and
//! the console still work. devfs goes on `/dev`; for the boot shell
//! (`abyss.shell`), tmpfs on `/tmp` and the EFI System Partition on
//! `/boot/efi` too, which with init `rc` mounts.
//!
//! Behavior reference: sys/kern/vfs_mount.c (`vfs_getnewfsid`),
//! sys/kern/vfs_mountroot.c.

pub(crate) mod block;
pub(crate) mod devfs;
pub(crate) mod eventfd;
pub(crate) mod fdtable;
pub(crate) mod fifo;
pub(crate) mod file;
pub(crate) mod inet;
pub(crate) mod inotify;
pub(crate) mod msdosfs;
pub(crate) mod pipe;
pub(crate) mod shm;
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

/// Whether [`cred`] holds privilege `p` (`priv_check`, `EPERM` if not),
/// taking no reference to it; kernel threads hold every privilege.
pub(crate) fn priv_check(p: crate::cred::Priv) -> Result<(), Errno> {
    crate::proc::Process::with_current(|me| me.priv_check(p)).unwrap_or(Ok(()))
}

/// The credential file operations run with: the current process's, or
/// the superuser's for kernel threads.
pub(crate) fn cred() -> Arc<Ucred> {
    // The process borrowed, not referenced: one count taken, not two.
    crate::proc::Process::with_current(crate::proc::Process::cred)
        .unwrap_or_else(crate::cred::root_shared)
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
    // Not there yet: what the buses are still finding first
    // (`vfs_mountroot_wait_if_neccessary`).
    if storage::find(name).is_none() {
        bus::root_mount_wait(s);
    }
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
    if let Some(line) = crate::console::dual_report() {
        kprintln!("{line}");
    }
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
    crate::devctl::init();
    crate::klog::init(&s);
    let dev = Mount::new(
        MountInfo {
            type_name: "devfs",
            flags: MNT_LOCAL,
            from: b"devfs".to_vec(),
            io_size: 4096,
        },
        Box::new(devfs::DevFs::new(0)),
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
    // For the boot shell (`abyss.shell`), what `rc` would mount: with
    // init, `rc` does, from `fstab` (`mount -a`) and `rc.conf` (`tmpmfs`).
    let shell = crate::boot::flag("abyss.shell");
    // A tmpfs on /tmp, as a FreeBSD install with `tmpmfs` has.
    if shell && let Ok(t) = ns.lookup(&s, &at, b"/tmp") {
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
    if shell && let Ok(dir) = ns.lookup(&s, &at, b"/boot/efi") {
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
        devfs::sysctl_init(t, vfs);
        inotify::sysctl_init(t, vfs);
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

/// `p`'s root directory (`pwd_rdir`): the system's unless it called
/// `chroot` or is in a jail; the system's is borrowed, not counted.
pub(crate) fn root_of(p: &crate::proc::Process) -> Cow<'static, Arc<Vnode>> {
    p.root
        .lock()
        .as_ref()
        .map_or(Cow::Borrowed(namespace().root()), |r| {
            Cow::Owned(Arc::clone(r.vnode()))
        })
}

/// Where a relative path starts: the working directory, or the directory
/// open on a descriptor (the `*at` calls' `fd`), which must hold the
/// rights the operation needs besides `CAP_LOOKUP`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum At {
    Cwd,
    Fd(i32, abi::capsicum::CapRights),
}

impl At {
    /// From an `*at` call's `fd` argument (`AT_FDCWD` for the working
    /// directory), for an operation needing `need` of it.
    pub(crate) const fn new(fd: i32, need: abi::capsicum::CapRights) -> Self {
        if fd == AT_FDCWD {
            Self::Cwd
        } else {
            Self::Fd(fd, need)
        }
    }
}

/// Where a walk starts, and what it inherits.
struct Start {
    dir: Cow<'static, Arc<Vnode>>,
    /// Where absolute paths start, and `..` stops: the process's root.
    root: Cow<'static, Arc<Vnode>>,
    /// The walk must stay beneath the start (`NI_LCF_STRICTREL`): in
    /// capability mode, or from a descriptor with limits.
    strict: bool,
    /// It starts at a descriptor with `UF_RESOLVE_BENEATH`, which what it
    /// opens takes too (`NIRES_BENEATH`).
    beneath: bool,
    /// The starting descriptor's limits, for what a strict walk opens.
    caps: Option<fdtable::FileCaps>,
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
    /// The lookup this walk is, from `start` under `root`, as `cred`.
    fn lookup<'a>(self, start: &'a Start, cred: &'a Cred) -> Lookup<'a> {
        Lookup {
            start: &start.dir,
            root: &start.root,
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

/// A traced lookup's records (`namei`, `namei_setup`,
/// `namei_handle_root`): its path (`KTR_NAMEI`), and what capability mode
/// would refuse, a walk from the working directory or of an absolute path
/// (`CAPFAIL_NAMEI`, once per lookup).
fn trace_lookup(s: &Sleepable, at: At, path: &[u8]) {
    use crate::ktrace::{self, CapFail};
    if ktrace::point(abi::ktrace::KTR_NAMEI) {
        ktrace::namei(s, path);
    }
    if ktrace::cap_tracing() {
        if at == At::Cwd {
            ktrace::capfail(CapFail::Namei(b"AT_FDCWD"));
        } else if path.first() == Some(&b'/') {
            ktrace::capfail(CapFail::Namei(path));
        }
    }
}

/// Where `p`'s walk of `path` starts (`namei_setup`): the root is
/// borrowed, not counted. In capability mode the walk must start at a
/// descriptor (`ECAPMODE` otherwise) and stay beneath it; so must one from
/// a descriptor with Capsicum limits, which must also hold `CAP_LOOKUP`
/// and the operation's rights.
fn start_of(s: &Sleepable, p: &crate::proc::Process, at: At, path: &[u8]) -> Result<Start, Errno> {
    if p.ktrace.flags() != 0 {
        trace_lookup(s, at, path);
    }
    let capmode = p.in_capability_mode();
    if capmode && at == At::Cwd {
        return Err(Errno::ECAPMODE);
    }
    let root = root_of(p);
    if path.first() == Some(&b'/') && !capmode {
        return Ok(Start {
            dir: root.clone(),
            root,
            strict: false,
            beneath: false,
            caps: None,
        });
    }
    match at {
        At::Cwd => Ok(Start {
            dir: Cow::Owned(Arc::clone(p.cwd.lock().vnode())),
            root,
            strict: false,
            beneath: false,
            caps: None,
        }),
        At::Fd(fd, need) => {
            let need = need.with(abi::capsicum::CAP_LOOKUP);
            let start = |slot: &fdtable::Slot| -> Result<Start, Errno> {
                slot.check(&need)?;
                let dir = match slot.file.vnode() {
                    Some(v) if v.is_dir() => Cow::Owned(Arc::clone(v)),
                    _ => return Err(Errno::ENOTDIR),
                };
                let limited = slot.caps.is_some();
                let beneath = slot.flags.resolve_beneath;
                Ok(Start {
                    dir,
                    root: root.clone(),
                    strict: capmode || limited || beneath,
                    beneath,
                    caps: (capmode || limited).then(|| slot.caps()),
                })
            };
            if p.sole_user_of_files() {
                // SAFETY: only the process's threads reach its table, this
                // is the only one, and it does nothing else with the table
                // while `start` runs (as `syscall::io::with_file`): no
                // lock, and no reference to the file (FreeBSD's
                // `fgetvp_lookup_smr`).
                let table = unsafe { p.files.get_unlocked() };
                start(table.slot(fd)?)
            } else {
                start(p.files.lock(s).slot(fd)?)
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
    resolve_caps(s, p, cred, at, path, walk).map(|(f, ..)| f)
}

/// [`resolve_as`], with the limits a file opened by a strict walk
/// inherits from its starting descriptor (`ni_filecaps`), and whether
/// it started at one with `UF_RESOLVE_BENEATH`.
pub(crate) fn resolve_caps(
    s: &Sleepable,
    p: &crate::proc::Process,
    cred: &Cred,
    at: At,
    path: &[u8],
    walk: Walk,
) -> Result<(Found, Option<fdtable::FileCaps>, bool), Errno> {
    let start = start_of(s, p, at, path)?;
    let walk = Walk {
        beneath: walk.beneath || start.strict,
        ..walk
    };
    let found = namespace().resolve(s, &walk.lookup(&start, cred), path)?;
    Ok((found, start.caps, start.beneath))
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
    let walk = Walk {
        beneath: walk.beneath || start.strict,
        ..walk
    };
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
            start: &start.dir,
            root: &start.root,
            follow: false,
            beneath: beneath || start.strict,
            create: false,
            cred: &p.cred(),
        },
        path,
    )
}

/// A directory, and a name in it.
type DirName = (Arc<Vnode>, alloc::vec::Vec<u8>);

/// The directory and name under which `path` (its last component's
/// symbolic links followed) finds a file that is not a directory, for
/// naming it as it was found when it has several links (FreeBSD's
/// `namei` with `WANTPARENT`, for `vn_fullpath_hardlink`). `None` if the
/// last component is a directory, `.`, or `..`.
pub(crate) fn found_under(
    s: &Sleepable,
    p: &crate::proc::Process,
    at: At,
    path: &[u8],
) -> Result<Option<DirName>, Errno> {
    let start = start_of(s, p, at, path)?;
    let cred = p.cred();
    let mut dir = Arc::clone(&start.dir);
    let mut path = path.to_vec();
    for _ in 0..=abi::fcntl::MAXSYMLINKS {
        let parent = namespace().parent_of(
            s,
            &Lookup {
                start: &dir,
                root: &start.root,
                follow: false,
                beneath: start.strict,
                create: false,
                cred: &cred,
            },
            &path,
        )?;
        let name: &[u8] = &parent.name;
        if name.is_empty() || name == b"." || name == b".." {
            return Ok(None);
        }
        let v = Namespace::child(s, &parent.dir, name)?;
        match v.kind() {
            vfs::Kind::Directory => return Ok(None),
            vfs::Kind::Symlink => {
                path = v.readlink(s)?;
                dir = parent.dir;
            }
            _ => return Ok(Some((parent.dir, name.to_vec()))),
        }
    }
    Err(Errno::ELOOP)
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
    let o = match v.object(s) {
        Some(o) => o,
        None => {
            let data = v.resident(s)?;
            let mut objects = OBJECTS.lock();
            Arc::clone(objects.entry(v.id()).or_insert_with(|| {
                vm::object::Object::with_pager(
                    crate::mm::space::page_ops(),
                    Arc::new(ResidentPager { data }),
                )
            }))
        }
    };
    // Its mappings name the file (`kern.proc.vmmap`).
    crate::kinfo::vm::note_file(&o, v);
    Some(o)
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
        && let Ok(vf) = VnodeFile::open(&s, &c, true, false)
    {
        let f = File::new(FileKind::Vnode(vf), FREAD | FWRITE);
        for _ in 0..3 {
            let _ = t.alloc(Arc::clone(&f), FdFlags::default(), 0, 3);
        }
    }
    t
}

/// Opens `/dev/null` on whichever of descriptors 0 to 2 `p` has closed,
/// the same open file on each (FreeBSD's `fdcheckstd`, for set-id
/// programs).
pub(crate) fn check_std(s: &Sleepable, p: &crate::proc::Process) -> Result<(), Errno> {
    let missing: alloc::vec::Vec<i32> = {
        let t = p.files.lock(s);
        (0..3).filter(|&fd| t.slot(fd).is_err()).collect()
    };
    if missing.is_empty() {
        return Ok(());
    }
    let v = namespace().lookup(
        s,
        &Lookup {
            start: namespace().root(),
            root: namespace().root(),
            follow: true,
            beneath: false,
            create: false,
            cred: &cred(),
        },
        b"/dev/null",
    )?;
    let v = v.device(s)?.ops.open()?.unwrap_or(v);
    let vf = VnodeFile::open(s, &v, true, false)?;
    let file = File::new(FileKind::Vnode(vf), FREAD | FWRITE);
    let mut t = p.files.lock(s);
    for fd in missing {
        // Only the process's own thread is in `exec`; nothing else fills
        // these meanwhile but its other threads, which are parked.
        drop(t.install(fd, Arc::clone(&file), FdFlags::default(), usize::MAX)?);
    }
    Ok(())
}
