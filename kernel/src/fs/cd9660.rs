// SPDX-License-Identifier: BSD-2-Clause
//! cd9660 (docs/VFS.md §10.7): ISO 9660 volumes with Rock Ridge and
//! Joliet, read-only, over `lib/cd9660`'s engine, as FreeBSD 15.1's cd9660
//! presents them.
//!
//! Directory records go through the disk's buffer cache; file data through
//! the page cache, with a pager reading a file's one extent. A file's
//! number is where its record lies, a directory's where its data starts,
//! and its attributes come from the record its lookup found (FreeBSD's
//! `cd9660_vget_internal` takes the entry): a lookup leaves that record
//! for the load that follows. `stat` reports what `cd9660_getattr` does:
//! the masks and the mount's owner applied, `st_blocks` the size in whole
//! 512-byte blocks, no birth time, generation 1; `readdir` reports no
//! types (`DT_UNKNOWN`).
//!
//! `kiconv` (`mount_cd9660 -C`) is not supported: FreeBSD without its
//! `cd9660_iconv` module behaves the same.
//!
//! Behavior reference: sys/fs/cd9660 (FreeBSD 15.1).

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;

use abi::errno::Errno;
use abi::time::Timespec;
use cd9660::{Error as IsoError, Found, Inode, Options, Volume};
use sched::Sleepable;
use sync::{Once, SpinLock};
use vfs::{
    Attr, Create, DirEntry, DirectoryNode, FileSystem, FsStats, Kind, Made, Mount, NewNode, Node,
    NodeOps, Rename, SetAttr, SymlinkNode, Uio,
};
use vm::object::{Object, PageIndex, Pager, PagerError};

use crate::fs::block::DiskIo;

/// Lookups' records kept for the loads that follow them.
const HINTS: usize = 32;

/// What every node of one mount shares.
struct Shared {
    vol: Volume,
    io: DiskIo,
    /// Records lookups found, newest last.
    hints: SpinLock<Vec<Found>>,
    /// The mount, for a link to the volume's root (`VOLROOT`).
    mount: Once<Weak<Mount>>,
}

/// A mounted ISO 9660 volume.
pub(crate) struct Cd9660Fs {
    shared: Arc<Shared>,
}

impl core::fmt::Debug for Cd9660Fs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Cd9660Fs({})", self.shared.io.disk.name())
    }
}

fn errno(e: IsoError) -> Errno {
    match e {
        IsoError::Io => Errno::EIO,
        IsoError::Invalid => Errno::EINVAL,
        IsoError::Stale => Errno::ESTALE,
        IsoError::NotFound => Errno::ENOENT,
    }
}

impl cd9660::BlockIo for DiskIo {
    type Cx = Sleepable;

    fn read(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), IsoError> {
        self.cache
            .read(s, offset, len, |b| f(b))
            .map_err(|_| IsoError::Io)
    }

    fn hostname(&self, _s: &Sleepable) -> Vec<u8> {
        crate::prison::current().settings().hostname
    }
}

/// A sleepable context for the pager, which is not given one.
fn sleepable() -> Sleepable {
    // SAFETY: pagers fill in thread context (the VM fills with no map
    // lock held, from a fault or a read).
    unsafe { Sleepable::assume() }
}

impl Cd9660Fs {
    /// Mounts the volume on `disk` (FreeBSD's `iso_mountfs`).
    pub(crate) fn mount(
        s: &Sleepable,
        disk: Arc<storage::Disk>,
        opts: &Options,
    ) -> Result<Self, Errno> {
        let sector = disk.sector_size();
        let io = DiskIo::new(disk);
        let vol = Volume::open(&io, s, sector, opts).map_err(errno)?;
        Ok(Self {
            shared: Arc::new(Shared {
                vol,
                io,
                hints: SpinLock::new(Vec::new()),
                mount: Once::new(),
            }),
        })
    }

    /// Records the mount it is, for `VOLROOT` links.
    pub(crate) fn set_mount(&self, m: &Arc<Mount>) {
        let _ = self.shared.mount.set(Arc::downgrade(m));
    }

    /// The disk it is on.
    pub(crate) fn disk(&self) -> &Arc<storage::Disk> {
        &self.shared.io.disk
    }

    /// The logical block size, the mount's I/O size.
    pub(crate) fn block_size(&self) -> u32 {
        self.shared.vol.logical_block_size()
    }
}

impl Shared {
    /// Where the volume is mounted, for `VOLROOT`.
    fn mounted_on(&self) -> Vec<u8> {
        self.mount
            .get()
            .and_then(Weak::upgrade)
            .map(|m| m.on())
            .unwrap_or_default()
    }

    /// The record a lookup found for `ino`, if one did lately.
    fn hint(&self, ino: u64) -> Option<Found> {
        let mut h = self.hints.lock();
        let i = h.iter().rposition(|f| f.ino == ino)?;
        Some(h.remove(i))
    }

    fn remember(&self, f: Found) {
        let mut h = self.hints.lock();
        h.retain(|x| x.ino != f.ino);
        if h.len() >= HINTS {
            h.remove(0);
        }
        h.push(f);
    }
}

impl FileSystem for Cd9660Fs {
    fn root(&self) -> u64 {
        self.shared.vol.root_ino()
    }

    fn load(&self, s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        let sh = &self.shared;
        let hint = sh.hint(ino);
        let ip = sh.vol.load(&sh.io, s, ino, hint.as_ref()).map_err(errno)?;
        // A link recorded with no size has its target's length
        // (`cd9660_getattr`).
        let mut size = ip.size;
        if ip.kind() == cd9660::S_IFLNK && size == 0 {
            size = sh
                .vol
                .readlink(&sh.io, s, &ip, &sh.mounted_on())
                .map_or(0, |t| t.len() as u64);
        }
        let kind = match ip.kind() {
            cd9660::S_IFDIR => Kind::Directory,
            cd9660::S_IFREG => Kind::Regular,
            cd9660::S_IFLNK => Kind::Symlink,
            0o010_000 => Kind::Fifo,
            0o140_000 => Kind::Socket,
            0o020_000 => Kind::CharDevice,
            0o060_000 => Kind::BlockDevice,
            // A type FreeBSD's `IFTOVT` makes `VNON`, which `stat` refuses.
            _ => return Err(Errno::EBADF),
        };
        let object = (kind == Kind::Regular).then(|| {
            Object::with_pager(
                crate::mm::space::page_ops(),
                Arc::new(FilePager {
                    fs: Arc::clone(sh),
                    start: sh.vol.data_offset(&ip, 0),
                    size: ip.size,
                }),
            )
        });
        let node = Box::new(IsoNode {
            fs: Arc::clone(sh),
            ip,
            size,
            object,
        });
        Ok(match kind {
            Kind::Directory => NodeOps::Directory(node),
            Kind::Regular => NodeOps::Regular(node),
            Kind::Symlink => NodeOps::Symlink(node),
            other => NodeOps::Other(other, node),
        })
    }

    fn statfs(&self, _s: &Sleepable) -> Result<FsStats, Errno> {
        // `cd9660_statfs`: the volume's blocks, nothing free.
        let v = &self.shared.vol;
        Ok(FsStats {
            bsize: u64::from(v.block_size),
            iosize: u64::from(v.block_size),
            blocks: v.volume_space_size,
            ..FsStats::default()
        })
    }

    fn rename(&self, _s: &Sleepable, _r: Rename<'_>) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn pathconf(&self, _kind: Kind, name: u32) -> Result<i64, Errno> {
        use abi::fcntl::{
            _PC_FILESIZEBITS, _PC_LINK_MAX, _PC_NAME_MAX, _PC_NO_TRUNC, _PC_SYMLINK_MAX, NAME_MAX,
            PATH_MAX,
        };
        let rrip = self.shared.vol.ftype == cd9660::FType::Rrip;
        match name {
            _PC_FILESIZEBITS => Ok(32),
            _PC_LINK_MAX => Ok(1),
            _PC_NAME_MAX if rrip => Ok(i64::try_from(NAME_MAX).unwrap_or(255)),
            _PC_NAME_MAX => Ok(37),
            _PC_SYMLINK_MAX if rrip => Ok(i64::try_from(PATH_MAX).unwrap_or(1024)),
            _PC_SYMLINK_MAX => Err(Errno::EINVAL),
            _PC_NO_TRUNC => Ok(1),
            _ => vfs::mount::std_pathconf(name),
        }
    }

    fn reports_dtype(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A file of the volume.
struct IsoNode {
    fs: Arc<Shared>,
    ip: Inode,
    /// The size `stat` reports (a link's target's length when unrecorded).
    size: u64,
    object: Option<Arc<Object>>,
}

fn time(t: cd9660::Time) -> Timespec {
    Timespec {
        tv_sec: t.sec,
        tv_nsec: t.nsec,
    }
}

impl Node for IsoNode {
    fn attr(&self, _s: &Sleepable) -> Attr {
        let v = &self.fs.vol;
        let dev = matches!(self.ip.kind(), 0o020_000 | 0o060_000);
        Attr {
            mode: v.mode_of(&self.ip),
            uid: v.uid_of(&self.ip),
            gid: v.gid_of(&self.ip),
            nlink: u64::from_ne_bytes(i64::from(self.ip.links).to_ne_bytes()),
            size: self.size,
            // `vop_stdstat`'s `va_bytes / S_BLKSIZE`, rounded down.
            bytes: self.ip.size / 512 * 512,
            atime: time(self.ip.atime),
            mtime: time(self.ip.mtime),
            ctime: time(self.ip.ctime),
            birthtime: Timespec {
                tv_sec: -1,
                tv_nsec: -1,
            },
            flags: 0,
            generation: 1,
            rdev: if dev { self.ip.rdev } else { u64::MAX },
            // The page size, which `vop_stdstat` rounds the 2048-byte
            // logical block up to.
            blksize: v.block_size.max(4096),
            fileid: 0,
            kind: None,
        }
    }

    fn setattr(&mut self, _s: &Sleepable, _a: &SetAttr) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn need_inactive(&self, _s: &Sleepable) -> bool {
        false
    }

    fn reclaim(self: Box<Self>, _s: &Sleepable) {}

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl vfs::RegularNode for IsoNode {
    fn read(&self, _s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let object = self.object.as_ref().ok_or(Errno::EINVAL)?;
        vfs::pagecache::read(object, self.ip.size, offset, uio)
    }

    fn write(&mut self, _s: &Sleepable, _offset: u64, _uio: &mut dyn Uio) -> Result<usize, Errno> {
        Err(Errno::EROFS)
    }

    fn object(&self, _s: &Sleepable) -> Option<Arc<Object>> {
        self.object.clone()
    }
}

impl SymlinkNode for IsoNode {
    fn target(&self, s: &Sleepable) -> Result<Vec<u8>, Errno> {
        let sh = &self.fs;
        sh.vol
            .readlink(&sh.io, s, &self.ip, &sh.mounted_on())
            .map_err(errno)
    }
}

impl DirectoryNode for IsoNode {
    fn lookup(&self, s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        let sh = &self.fs;
        let f = sh.vol.lookup(&sh.io, s, &self.ip, name).map_err(errno)?;
        let ino = f.ino;
        sh.remember(f);
        Ok(ino)
    }

    fn parent(&self, _s: &Sleepable) -> u64 {
        self.ip.parent
    }

    fn create(&mut self, _: &Sleepable, _: &[u8], _: &NewNode, _: &Create) -> Result<Made, Errno> {
        Err(Errno::EROFS)
    }

    fn link(&mut self, _: &Sleepable, _: &[u8], _: u64, _: &mut dyn Node) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn remove(&mut self, _: &Sleepable, _: &[u8], _: &mut dyn Node) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn rmdir(&mut self, _: &Sleepable, _: &[u8], _: &mut dyn DirectoryNode) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn readdir(
        &self,
        s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        let sh = &self.fs;
        sh.vol
            .readdir(&sh.io, s, &self.ip, from, &mut |e| {
                f(DirEntry {
                    name: e.name,
                    ino: e.ino,
                    // Not reported (`DT_UNKNOWN`).
                    kind: Kind::Regular,
                    next: e.next,
                })
            })
            .map_err(errno)
    }
}

/// A file's data: one extent from `start`.
struct FilePager {
    fs: Arc<Shared>,
    start: u64,
    size: u64,
}

const PAGE: u64 = 4096;

impl Pager for FilePager {
    fn size(&self) -> PageIndex {
        self.size.div_ceil(PAGE)
    }

    fn fill(&self, index: PageIndex, page: &mut [u8]) -> Result<(), PagerError> {
        let s = sleepable();
        page.fill(0);
        let from = index * PAGE;
        if from >= self.size {
            return Ok(());
        }
        let n = usize::try_from((self.size - from).min(PAGE)).map_err(|_| PagerError)?;
        let disk = &self.fs.io.disk;
        let ss = usize::try_from(disk.sector_size()).unwrap_or(512);
        let data = disk
            .read(&s, self.start + from, n.next_multiple_of(ss))
            .map_err(|_| PagerError)?;
        page[..n].copy_from_slice(data.get(..n).ok_or(PagerError)?);
        Ok(())
    }

    fn writes_back(&self) -> bool {
        false
    }

    fn write_back(&self, _index: PageIndex, _page: &[u8]) -> Result<(), PagerError> {
        Err(PagerError)
    }
}
