// SPDX-License-Identifier: BSD-2-Clause
//! UFS2 mounted read-only (P5.4, docs/VFS.md §10.3): `lib/ufs` over a
//! disk.
//!
//! Metadata (the superblock, inodes, indirect and directory blocks) goes
//! through the disk's buffer cache. File data goes through the page
//! cache: each regular file's VM object has a pager that maps file blocks
//! with `bmap_run` and reads each contiguous run straight from the disk in
//! one transfer, as FreeBSD's vnode pager does (`VOP_GETPAGES` over
//! `VOP_BMAP`), so data is never cached twice.
//!
//! Writing comes with P5.5: until then a mount must ask for read-only.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::errno::Errno;
use abi::time::Timespec;
use bufcache::BufCache;
use sched::Sleepable;
use vfs::{
    Attr, Create, DirEntry, DirectoryNode, FileSystem, FsStats, Kind, NewNode, Node, NodeOps,
    Rename, SetAttr, SymlinkNode, Uio,
};
use vm::object::{Object, PageIndex, Pager, PagerError};

/// Metadata blocks through the buffer cache.
struct CacheIo(BufCache);

impl ufs::BlockIo for CacheIo {
    type Cx = Sleepable;

    fn read(
        &self,
        s: &Sleepable,
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> Result<(), ufs::Error> {
        self.0
            .read(s, offset, len, |b| f(b))
            .map_err(|_| ufs::Error::Io)
    }
}

/// What every node of one mount shares.
struct Shared {
    ufs: ufs::Ufs<CacheIo>,
    disk: Arc<storage::Disk>,
}

/// A mounted UFS2 file system.
pub(crate) struct UfsFs {
    shared: Arc<Shared>,
}

impl core::fmt::Debug for UfsFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "UfsFs({})", self.shared.disk.name())
    }
}

fn errno(e: ufs::Error) -> Errno {
    match e {
        ufs::Error::Io => Errno::EIO,
        ufs::Error::NotUfs2 => Errno::EINVAL,
        ufs::Error::Corrupt(what) => {
            kprintln!("ufs: corrupt: {what}");
            Errno::EIO
        }
        ufs::Error::Integrity(what) => {
            kprintln!("ufs: {what} failed");
            Errno::EINTEGRITY
        }
    }
}

impl UfsFs {
    /// Mounts the UFS2 file system on `disk`.
    pub(crate) fn mount(s: &Sleepable, disk: Arc<storage::Disk>) -> Result<Self, Errno> {
        let sector = disk.sector_size();
        let cache = crate::fs::block::cache(Arc::clone(&disk), bufcache::Limits::default());
        let ufs = ufs::Ufs::open(s, CacheIo(cache), sector).map_err(errno)?;
        Ok(Self {
            shared: Arc::new(Shared { ufs, disk }),
        })
    }

    /// The block size, for the mount's I/O size.
    pub(crate) fn block_size(&self) -> u32 {
        self.shared.ufs.superblock().bsize
    }
}

impl FileSystem for UfsFs {
    fn root(&self) -> u64 {
        u64::from(ufs::ROOT_INO)
    }

    fn load(&self, s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        let ino32 = u32::try_from(ino).map_err(|_| Errno::ENOENT)?;
        let ip = self.shared.ufs.inode(s, ino32).map_err(errno)?;
        if ip.mode == 0 {
            kprintln!("ufs: {}: inode {ino} is free", self.shared.disk.name());
            return Err(Errno::EIO);
        }
        let mut node = Box::new(UfsNode {
            parent: u64::from(ino32),
            object: None,
            fs: Arc::clone(&self.shared),
            ip,
        });
        Ok(match node.ip.kind() {
            ufs::Kind::Regular => {
                let pager = Arc::new(FilePager {
                    fs: Arc::clone(&self.shared),
                    ip: node.ip.clone(),
                });
                node.object = Some(Object::with_pager(crate::mm::space::page_ops(), pager));
                NodeOps::Regular(node)
            }
            ufs::Kind::Directory => {
                if ino32 != ufs::ROOT_INO {
                    node.parent = self
                        .shared
                        .ufs
                        .lookup(s, &node.ip, b"..")
                        .map_err(errno)?
                        .map_or(u64::from(ino32), u64::from);
                }
                NodeOps::Directory(node)
            }
            ufs::Kind::Symlink => NodeOps::Symlink(node),
            ufs::Kind::CharDevice | ufs::Kind::BlockDevice => {
                NodeOps::Other(Kind::CharDevice, node)
            }
            ufs::Kind::Fifo => NodeOps::Other(Kind::Fifo, node),
            ufs::Kind::Socket => NodeOps::Other(Kind::Socket, node),
            ufs::Kind::Whiteout | ufs::Kind::None => {
                kprintln!("ufs: inode {ino} has mode {:o}", node.ip.mode);
                return Err(Errno::EIO);
            }
        })
    }

    fn statfs(&self, _s: &Sleepable) -> Result<FsStats, Errno> {
        let st = self.shared.ufs.stats();
        Ok(FsStats {
            bsize: st.fsize,
            iosize: st.iosize,
            blocks: st.blocks,
            bfree: st.bfree,
            bavail: st.bavail,
            files: st.files,
            ffree: st.ffree,
        })
    }

    fn rename(&self, _s: &Sleepable, _r: Rename<'_>) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn unmount(&self, s: &Sleepable) -> Result<(), Errno> {
        // Read-only: nothing is dirty, but let the buffers go.
        let cache = &self.shared.ufs.io().0;
        cache.sync(s).map_err(|_| Errno::EIO)?;
        cache.invalidate();
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// One file's in-core node.
struct UfsNode {
    ip: ufs::Inode,
    /// A directory's `..`.
    parent: u64,
    /// A regular file's pages.
    object: Option<Arc<Object>>,
    fs: Arc<Shared>,
}

fn time(t: ufs::Timestamp) -> Timespec {
    Timespec {
        tv_sec: t.sec,
        tv_nsec: i64::from(t.nsec),
    }
}

/// A directory entry's type as a node kind (`None` for a whiteout).
fn kind_of(dtype: u8) -> Option<Kind> {
    use abi::dirent::{DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK};
    Some(match dtype {
        DT_REG => Kind::Regular,
        DT_DIR => Kind::Directory,
        DT_LNK => Kind::Symlink,
        DT_CHR | DT_BLK => Kind::CharDevice,
        DT_FIFO => Kind::Fifo,
        DT_SOCK => Kind::Socket,
        _ => return None,
    })
}

impl Node for UfsNode {
    fn attr(&self) -> Attr {
        let ip = &self.ip;
        Attr {
            mode: ip.perm(),
            uid: ip.uid,
            gid: ip.gid,
            nlink: u64::from(ip.nlink),
            size: ip.size,
            bytes: ip.blocks * 512,
            atime: time(ip.atime),
            mtime: time(ip.mtime),
            ctime: time(ip.ctime),
            birthtime: time(ip.birthtime),
            flags: ip.flags,
            generation: u64::from(ip.generation),
            rdev: match ip.kind() {
                ufs::Kind::CharDevice | ufs::Kind::BlockDevice => ip.rdev(),
                _ => 0,
            },
            blksize: self.fs.ufs.superblock().bsize,
        }
    }

    fn setattr(&mut self, _s: &Sleepable, _a: &SetAttr) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn reclaim(self: Box<Self>, _s: &Sleepable) {
        // Mappings may outlive the vnode: the object stays theirs.
        if let Some(o) = &self.object {
            o.disown();
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl vfs::RegularNode for UfsNode {
    fn read(&self, _s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let object = self.object.as_ref().ok_or(Errno::EINVAL)?;
        vfs::pagecache::read(object, self.ip.size, offset, uio)
    }

    fn write(&mut self, _s: &Sleepable, _offset: u64, _uio: &mut dyn Uio) -> Result<usize, Errno> {
        Err(Errno::EROFS)
    }

    fn object(&self) -> Option<Arc<Object>> {
        self.object.clone()
    }
}

impl SymlinkNode for UfsNode {
    fn target(&self, s: &Sleepable) -> Result<Vec<u8>, Errno> {
        self.fs.ufs.readlink(s, &self.ip).map_err(errno)
    }
}

impl DirectoryNode for UfsNode {
    fn lookup(&self, s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        self.fs
            .ufs
            .lookup(s, &self.ip, name)
            .map_err(errno)?
            .map(u64::from)
            .ok_or(Errno::ENOENT)
    }

    fn parent(&self) -> u64 {
        self.parent
    }

    fn create(&mut self, _: &Sleepable, _: &[u8], _: &NewNode, _: &Create) -> Result<u64, Errno> {
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
        let ufs = &self.fs.ufs;
        let mut failed = None;
        ufs.read_dir(s, &self.ip, from, &mut |e| {
            let kind = match kind_of(e.kind) {
                Some(k) => k,
                // A whiteout names nothing.
                None if e.kind == abi::dirent::DT_WHT => return true,
                // An entry without a type: ask its inode.
                None => match ufs.inode(s, e.ino) {
                    Ok(ip) => match ip.kind() {
                        ufs::Kind::Directory => Kind::Directory,
                        ufs::Kind::Symlink => Kind::Symlink,
                        ufs::Kind::CharDevice | ufs::Kind::BlockDevice => Kind::CharDevice,
                        ufs::Kind::Fifo => Kind::Fifo,
                        ufs::Kind::Socket => Kind::Socket,
                        _ => Kind::Regular,
                    },
                    Err(err) => {
                        failed = Some(err);
                        return false;
                    }
                },
            };
            f(DirEntry {
                name: e.name,
                ino: u64::from(e.ino),
                kind,
                next: e.next,
            })
        })
        .map_err(errno)?;
        failed.map_or(Ok(()), |e| Err(errno(e)))
    }
}

/// The most pages one fill reads: read-ahead grows to this.
const MAX_RUN: PageIndex = 64;
const PAGE: u64 = 4096;

/// A regular file's pages, read from its blocks.
struct FilePager {
    fs: Arc<Shared>,
    ip: ufs::Inode,
}

impl Pager for FilePager {
    fn size(&self) -> PageIndex {
        self.ip.size.div_ceil(PAGE)
    }

    fn max_run(&self) -> PageIndex {
        MAX_RUN
    }

    fn fill(&self, index: PageIndex, page: &mut [u8]) -> Result<(), PagerError> {
        self.fill_run(index, page)
    }

    fn fill_run(&self, index: PageIndex, buf: &mut [u8]) -> Result<(), PagerError> {
        // SAFETY: the VM calls pagers only where they may sleep (a fault
        // fills with no map lock held; the page cache in thread context),
        // as `Pager::fill`'s contract says.
        let s = unsafe { Sleepable::assume() };
        let ufs = &self.fs.ufs;
        let sb = ufs.superblock();
        let bsize = u64::from(sb.bsize);
        let fsize = u64::from(sb.fsize);
        let size = self.ip.size;
        let start = index * PAGE;
        let end = start + buf.len() as u64;
        let mut at = start;
        while at < end {
            let out = usize::try_from(at - start).map_err(|_| PagerError)?;
            if at >= size {
                buf[out..].fill(0);
                break;
            }
            let lbn = at / bsize;
            let within = at % bsize;
            let blocks = (end - at).div_ceil(bsize).max(1);
            let (addr, run) = ufs
                .bmap_run(&s, &self.ip, lbn, blocks)
                .map_err(|_| PagerError)?;
            // The bytes the run holds from `at`: whole blocks, but only
            // the allocated fragments of the file's last one.
            let last = lbn + run - 1;
            let held = (run - 1) * bsize + u64::from(sb.block_size(size, last)) - within;
            let n = held.min(end - at);
            let len = usize::try_from(n).map_err(|_| PagerError)?;
            if addr == 0 {
                buf[out..out + len].fill(0);
            } else {
                let data = self
                    .fs
                    .disk
                    .read(&s, addr * fsize + within, len)
                    .map_err(|_| PagerError)?;
                buf[out..out + len].copy_from_slice(&data);
            }
            at += n;
        }
        // Past the end of the file within its last page reads as zeros,
        // whatever the fragment held there.
        if let Ok(past) = usize::try_from(size.saturating_sub(start))
            && past < buf.len()
        {
            buf[past..].fill(0);
        }
        Ok(())
    }
}
