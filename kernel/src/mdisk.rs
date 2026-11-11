// SPDX-License-Identifier: BSD-2-Clause
//! md(4) memory disks: `/dev/mdctl` attaches and detaches disks held in
//! kernel memory (`mdconfig -a -t malloc`) or backed by a regular file
//! (`mdconfig -a -t vnode -f FILE`), which `sys/storage` publishes as
//! `mdN` like any other disk, so devfs, `newfs`, and `mount` see them as
//! FreeBSD's.
//!
//! A vnode-backed disk holds its file open, for writing unless read-only
//! (`mdcreate_vnode`), and does its requests through the file's page
//! cache; reading past the file's end reads zeros.
//!
//! A disk's pages are allocated when first written; an unwritten page
//! reads as zeros, as FreeBSD's malloc disks do without `reserve`. Swap
//! disks are memory disks too, there being no swap. Each disk has a
//! thread that does its requests, as FreeBSD's `md` worker does, so
//! completions never run under the disk class's locks.
//!
//! Behavior reference: sys/dev/md/md.c (`kern_mdattach_locked`,
//! `kern_mddetach_locked`, `kern_mdquery_locked`, `g_md_dumpconf`).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::offset_of;

use abi::errno::Errno;
use abi::md::{
    MD_ASYNC, MD_AUTOUNIT, MD_CACHE, MD_COMPRESS, MD_FORCE, MD_MALLOC, MD_MUSTDEALLOC, MD_READONLY,
    MD_SWAP, MD_VERIFY, MD_VNODE, MDIOCATTACH, MDIOCDETACH, MDIOCQUERY, MDIOVERSION, MdIoctl,
};
use sched::{Sleepable, WaitQueue};
use storage::{Bio, BioError, BioOp, Disk, DiskInfo, DiskQueues};
use sync::SpinLock;
use vfs::{CharDevice, Uio};

/// The disk name.
const NAME: &str = "md";
/// The sector size when `mdconfig` names none (`DEV_BSIZE`).
const DEFAULT_SECTOR: u32 = 512;
/// The largest transfer one request may make.
const MAX_TRANSFER: usize = 128 * 1024;
/// Memory is allocated in pages of this size.
const PAGE_BYTES: usize = 4096;
const PAGE: u64 = 4096;

/// Requests waiting for a disk's thread.
struct Queue {
    bios: SpinLock<(Vec<Bio>, bool)>,
    wake: WaitQueue,
}

impl DiskQueues for Queue {
    fn queues(&self) -> usize {
        1
    }

    fn submit(&self, _: &Sleepable, _: usize, bios: &mut Vec<Bio>) {
        let mut q = self.bios.lock();
        if q.1 {
            drop(q);
            for bio in bios.drain(..) {
                bio.complete(Err(BioError::Gone));
            }
            return;
        }
        q.0.append(bios);
        drop(q);
        self.wake.wake_one();
    }
}

/// A disk's contents: the pages written so far.
struct Pages(BTreeMap<u64, Box<[u8]>>);

impl Pages {
    /// Copies `buf.len()` bytes at `offset` out of (`write` false) or into
    /// the disk.
    fn transfer(&mut self, offset: u64, buf: &mut [u8], write: bool) {
        let mut done = 0;
        while done < buf.len() {
            let at = offset + u64::try_from(done).unwrap_or(u64::MAX);
            let page = at / PAGE;
            let within = usize::try_from(at % PAGE).unwrap_or(0);
            let n = (buf.len() - done).min(PAGE_BYTES - within);
            let part = &mut buf[done..done + n];
            if write {
                let p = self
                    .0
                    .entry(page)
                    .or_insert_with(|| alloc::vec![0u8; PAGE_BYTES].into_boxed_slice());
                p[within..within + n].copy_from_slice(part);
            } else {
                match self.0.get(&page) {
                    Some(p) => part.copy_from_slice(&p[within..within + n]),
                    None => part.fill(0),
                }
            }
            done += n;
        }
    }
}

/// A vnode-backed disk's file (`s_vnode`), open while attached.
struct Backing {
    vnode: vfs::VnodeUse,
    /// Counted as a writer (opened `FWRITE`).
    writer: bool,
}

impl Drop for Backing {
    fn drop(&mut self) {
        if self.writer {
            self.vnode.drop_writer();
        }
    }
}

impl Backing {
    /// A request on the file (`mdstart_vnode`).
    fn transfer(
        &self,
        s: &Sleepable,
        offset: u64,
        buf: &mut [u8],
        write: bool,
    ) -> Result<(), BioError> {
        if write {
            let mut w = vfs::WriteBuf::new(buf);
            let n = self
                .vnode
                .write_at(s, offset, &mut w)
                .map_err(|_| BioError::Io)?;
            if n != buf.len() {
                return Err(BioError::Io);
            }
        } else {
            let mut r = vfs::ReadBuf::new(buf);
            self.vnode
                .read_at(s, offset, &mut r)
                .map_err(|_| BioError::Io)?;
            let n = r.filled().len();
            buf[n..].fill(0);
        }
        Ok(())
    }
}

/// A disk's thread: does its requests until the disk is detached.
fn worker(s: &Sleepable, q: &Queue, backing: Option<Backing>) {
    let mut pages = Pages(BTreeMap::new());
    loop {
        q.wake.wait_until(s, || {
            let b = q.bios.lock();
            !b.0.is_empty() || b.1
        });
        let (batch, gone) = {
            let mut b = q.bios.lock();
            (core::mem::take(&mut b.0), b.1)
        };
        for mut bio in batch {
            let r = if gone {
                Err(BioError::Gone)
            } else {
                match (bio.op, &backing) {
                    (BioOp::Read | BioOp::Write, Some(b)) => {
                        let write = bio.op == BioOp::Write;
                        let mut buf = core::mem::take(&mut bio.buf).into_vec();
                        let r = b.transfer(s, bio.offset, &mut buf, write);
                        bio.buf = buf.into();
                        r
                    }
                    (BioOp::Flush, Some(b)) => b.vnode.fsync(s).map_err(|_| BioError::Io),
                    (BioOp::Read, None) => {
                        let mut buf = core::mem::take(&mut bio.buf).into_vec();
                        pages.transfer(bio.offset, &mut buf, false);
                        bio.buf = buf.into();
                        Ok(())
                    }
                    (BioOp::Write, None) => {
                        // Lent pages are copied: memory to memory either way.
                        let mut buf = core::mem::take(&mut bio.buf).into_vec();
                        pages.transfer(bio.offset, &mut buf, true);
                        bio.buf = buf.into();
                        Ok(())
                    }
                    (BioOp::Flush, None) => Ok(()),
                }
            };
            bio.complete(r);
        }
        if gone {
            return;
        }
    }
}

/// An attached disk.
struct Unit {
    disk: Arc<Disk>,
    queue: Arc<Queue>,
    kind: u32,
    options: u32,
    /// Firmware geometry `mdconfig -x` and `-y` gave (0 for none).
    fwsectors: u32,
    fwheads: u32,
    /// References to the disk with nothing using it (the disk lists' and
    /// devfs's); more means a mount holds it.
    idle_refs: usize,
    /// A vnode-backed disk's file: its path as given, and what `ident`
    /// shows (`MD-DEV<fsid>-INO<fileid>`).
    file: Option<(Vec<u8>, alloc::string::String)>,
}

static UNITS: SpinLock<BTreeMap<u32, Unit>> = SpinLock::new(BTreeMap::new());

/// `/dev/mdctl`.
struct Control;

fn get_u32(arg: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&arg[at..at + 4]);
    u32::from_ne_bytes(b)
}

fn get_i64(arg: &[u8], at: usize) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&arg[at..at + 8]);
    i64::from_ne_bytes(b)
}

fn put(arg: &mut [u8], at: usize, v: &[u8]) {
    arg[at..at + v.len()].copy_from_slice(v);
}

/// `MDIOCATTACH`: a new disk; its unit is written back.
fn attach(s: &Sleepable, arg: &mut [u8]) -> Result<(), Errno> {
    let kind = get_u32(arg, offset_of!(MdIoctl, md_type));
    let mut options = get_u32(arg, offset_of!(MdIoctl, md_options));
    if kind != MD_MALLOC && kind != MD_SWAP && kind != MD_VNODE {
        return Err(Errno::EOPNOTSUPP);
    }
    let sector_size = match get_u32(arg, offset_of!(MdIoctl, md_sectorsize)) {
        0 => DEFAULT_SECTOR,
        n => n,
    };
    let size =
        u64::try_from(get_i64(arg, offset_of!(MdIoctl, md_mediasize))).map_err(|_| Errno::EDOM)?;
    let (backing, file) = if kind == MD_VNODE {
        // `kern_mdattach_locked`: no smaller than a sector.
        if size < u64::from(sector_size) {
            return Err(Errno::EINVAL);
        }
        let (b, path, ident) = open_backing(s, arg, options)?;
        if !b.writer {
            options |= MD_READONLY;
        }
        // The options a vnode disk keeps (`sc->flags`).
        options &=
            MD_ASYNC | MD_CACHE | MD_FORCE | MD_VERIFY | MD_MUSTDEALLOC | MD_READONLY | MD_AUTOUNIT;
        (Some(b), Some((path, ident)))
    } else {
        if size == 0 || !size.is_multiple_of(u64::from(sector_size)) {
            return Err(Errno::EDOM);
        }
        (None, None)
    };
    let unit = if options & MD_AUTOUNIT != 0 {
        storage::alloc_unit(NAME)
    } else {
        let u = get_u32(arg, offset_of!(MdIoctl, md_unit));
        if !storage::reserve_unit(NAME, u) {
            return Err(Errno::EBUSY);
        }
        u
    };
    let queue = Arc::new(Queue {
        bios: SpinLock::new((Vec::new(), false)),
        wake: WaitQueue::new(),
    });
    let q = Arc::clone(&queue);
    if sched::spawn("md", sched::Priority::KERNEL, move |s| {
        worker(s, &q, backing)
    })
    .is_err()
    {
        return Err(Errno::ENOMEM);
    }
    let info = DiskInfo {
        name: NAME,
        unit,
        sector_size,
        sectors: size / u64::from(sector_size),
        max_transfer: MAX_TRANSFER,
        read_only: options & MD_READONLY != 0,
    };
    // `int`s: a negative one is none.
    let geometry =
        |at| u32::try_from(i32::from_ne_bytes(get_u32(arg, at).to_ne_bytes())).unwrap_or(0);
    let (fwsectors, fwheads) = (
        geometry(offset_of!(MdIoctl, md_fwsectors)),
        geometry(offset_of!(MdIoctl, md_fwheads)),
    );
    let disk = storage::publish(s, info, Arc::clone(&queue) as Arc<dyn DiskQueues>);
    // One more for the table's own.
    let idle_refs = Arc::strong_count(&disk);
    UNITS.lock().insert(
        unit,
        Unit {
            disk,
            queue,
            kind,
            options: options & !MD_AUTOUNIT,
            fwsectors,
            fwheads,
            idle_refs,
            file,
        },
    );
    put(arg, offset_of!(MdIoctl, md_unit), &unit.to_ne_bytes());
    Ok(())
}

/// Opens the file a vnode-backed disk is attached to (`mdcreate_vnode`):
/// `md_file`'s path, for reading and, unless `MD_READONLY`, writing; a
/// regular file. Returns it, its path, and the disk's `ident`.
fn open_backing(
    s: &Sleepable,
    arg: &[u8],
    options: u32,
) -> Result<(Backing, Vec<u8>, alloc::string::String), Errno> {
    let at = offset_of!(MdIoctl, md_file);
    let ptr = u64::from_ne_bytes(arg[at..at + 8].try_into().map_err(|_| Errno::EINVAL)?);
    let path = crate::uaccess::copy_in_str_vec(ptr, abi::fcntl::PATH_MAX).map_err(|e| match e {
        crate::uaccess::StrError::Fault => Errno::EFAULT,
        crate::uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    })?;
    let p = crate::proc::Process::current().ok_or(Errno::EINVAL)?;
    let v = crate::fs::lookup(s, &p, crate::fs::At::Cwd, &path, crate::fs::Walk::FOLLOW)?;
    let write = options & MD_READONLY == 0;
    let cred = crate::fs::cred();
    let mut want = vfs::ops::VREAD;
    if write {
        if v.is_dir() {
            return Err(Errno::EISDIR);
        }
        if v.mount().read_only() && v.kind() == vfs::Kind::Regular {
            return Err(Errno::EROFS);
        }
        want |= vfs::ops::VWRITE;
    }
    vfs::acl::access(s, &v, &cred, want)?;
    if v.kind() != vfs::Kind::Regular {
        return Err(Errno::EINVAL);
    }
    if write {
        v.add_writer()?;
    }
    let backing = Backing {
        vnode: v.use_ref(),
        writer: write,
    };
    let st = v.stat(s)?;
    let ident = alloc::format!("MD-DEV{}-INO{}", st.st_dev, st.st_ino);
    Ok((backing, path, ident))
}

/// What GEOM's MD class shows of a memory disk (`g_md_dumpconf`).
#[derive(Clone, Debug)]
pub(crate) struct Info {
    pub(crate) unit: u32,
    /// `malloc`, `vnode`, or `swap`.
    pub(crate) kind: &'static str,
    /// A vnode-backed disk's file and `ident`.
    pub(crate) file: Option<(Vec<u8>, alloc::string::String)>,
    pub(crate) compress: bool,
    pub(crate) read_only: bool,
    pub(crate) fwsectors: u32,
    pub(crate) fwheads: u32,
}

/// The memory disk `disk` is, if it is one.
pub(crate) fn info(disk: &Arc<Disk>) -> Option<Info> {
    let units = UNITS.lock();
    let (&unit, u) = units.iter().find(|(_, u)| Arc::ptr_eq(&u.disk, disk))?;
    Some(Info {
        unit,
        kind: match u.kind {
            MD_SWAP => "swap",
            MD_VNODE => "vnode",
            _ => "malloc",
        },
        file: u.file.clone(),
        compress: u.options & MD_COMPRESS != 0,
        read_only: u.options & MD_READONLY != 0,
        fwsectors: u.fwsectors,
        fwheads: u.fwheads,
    })
}

/// `MDIOCDETACH`: `EBUSY` while mounted, unless forced.
fn detach(s: &Sleepable, arg: &[u8]) -> Result<(), Errno> {
    let unit = get_u32(arg, offset_of!(MdIoctl, md_unit));
    let options = get_u32(arg, offset_of!(MdIoctl, md_options));
    let (disk, idle_refs) = {
        let units = UNITS.lock();
        let u = units.get(&unit).ok_or(Errno::ENOENT)?;
        (Arc::clone(&u.disk), u.idle_refs)
    };
    // Open (`opencount`): references beyond the idle ones (this one, and
    // each partition's of its disk, among them), or an open or mount of
    // it or a partition.
    let parts = storage::partitions(&disk).len();
    let busy = Arc::strong_count(&disk) > idle_refs + 1 + parts || crate::geom_kern::in_use(&disk);
    drop(disk);
    if busy && options & MD_FORCE == 0 {
        return Err(Errno::EBUSY);
    }
    let u = UNITS.lock().remove(&unit).ok_or(Errno::ENOENT)?;
    storage::withdraw(s, &u.disk);
    u.queue.bios.lock().1 = true;
    u.queue.wake.wake_one();
    Ok(())
}

/// `MDIOCQUERY`: a disk's type, options, and size.
fn query(arg: &mut [u8]) -> Result<(), Errno> {
    let unit = get_u32(arg, offset_of!(MdIoctl, md_unit));
    let units = UNITS.lock();
    let u = units.get(&unit).ok_or(Errno::ENOENT)?;
    let size = i64::try_from(u.disk.bytes()).unwrap_or(i64::MAX);
    put(arg, offset_of!(MdIoctl, md_type), &u.kind.to_ne_bytes());
    put(
        arg,
        offset_of!(MdIoctl, md_options),
        &u.options.to_ne_bytes(),
    );
    put(arg, offset_of!(MdIoctl, md_mediasize), &size.to_ne_bytes());
    put(
        arg,
        offset_of!(MdIoctl, md_sectorsize),
        &u.disk.sector_size().to_ne_bytes(),
    );
    // A vnode disk's file, to the caller's buffer (`kern_mdquery_locked`).
    if let Some((path, _)) = &u.file {
        let at = offset_of!(MdIoctl, md_file);
        let ptr = u64::from_ne_bytes(arg[at..at + 8].try_into().map_err(|_| Errno::EINVAL)?);
        if ptr != 0 {
            let mut out = path.clone();
            out.push(0);
            crate::uaccess::copy_out(ptr, &out).map_err(|_| Errno::EFAULT)?;
        }
    }
    Ok(())
}

impl CharDevice for Control {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        if !matches!(req, MDIOCATTACH | MDIOCDETACH | MDIOCQUERY) {
            return Err(Errno::ENOTTY);
        }
        if arg.len() != size_of::<MdIoctl>() {
            return Err(Errno::EINVAL);
        }
        if get_u32(arg, offset_of!(MdIoctl, md_version)) != MDIOVERSION {
            return Err(Errno::EINVAL);
        }
        match req {
            MDIOCATTACH => attach(s, arg),
            MDIOCDETACH => detach(s, arg),
            _ => query(arg),
        }
    }
}

/// Adds `/dev/mdctl` (FreeBSD's mode and owner).
pub(crate) fn init() {
    crate::fs::devfs::add(b"mdctl", 0o600, 0, 0, Arc::new(Control));
}
