// SPDX-License-Identifier: BSD-2-Clause
//! The virtual file system: vnodes, the file systems they belong to, and
//! path lookup across mounts.
//!
//! A [`Vnode`] is one file, directory, symbolic link, or device. What it
//! can do depends on its [`VnodeKind`]: only a directory can be looked in,
//! only a regular file read at an offset, so code cannot ask a file for
//! directory entries. File systems implement the kind's trait
//! ([`Directory`], [`RegularFile`], [`CharDevice`]).
//!
//! Data moves through a [`Uio`], FreeBSD's `struct uio`: the caller's
//! buffers, which the kernel backs with user memory and tests with slices.
//!
//! This first VFS serves file systems built once and never changed: the
//! memory disk ([`tarfs`]) and devfs, both on [`tree`]. Writable file
//! systems, the name cache, and mounting at run time come with Phase 5.
//!
//! Behavior reference: sys/kern/vfs_lookup.c, sys/kern/vfs_vnops.c,
//! sys/kern/vfs_cache.c (`vn_fullpath`).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod namei;
pub mod tarfs;
pub mod tree;

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::dirent::{DT_CHR, DT_DIR, DT_LNK, DT_REG};
use abi::errno::Errno;
use abi::stat::{S_IFCHR, S_IFDIR, S_IFLNK, S_IFREG, Stat};
use abi::time::Timespec;
use sched::Sleepable;

/// A mounted file system.
#[derive(Debug)]
pub struct FileSystem {
    /// `st_dev` of its files, and its `fsid`.
    pub id: u64,
    /// The type's name (`f_fstypename`).
    pub type_name: &'static str,
    /// The type's number (`f_type`).
    pub type_num: u32,
    /// `MNT_*` flags.
    pub flags: u64,
    /// What it was mounted from (`f_mntfromname`).
    pub from: Vec<u8>,
    /// The preferred I/O size (`st_blksize`, `f_iosize`).
    pub io_size: u32,
}

impl FileSystem {
    #[must_use]
    pub const fn read_only(&self) -> bool {
        self.flags & abi::mount::MNT_RDONLY != 0
    }
}

/// A file system type's number (`vfc_typenum`): FreeBSD hashes the name
/// (32-bit FNV-1 from its own seed, low 8 bits, 0 made 1). FreeBSD then moves a colliding
/// type to the next free number; the types here do not collide.
#[must_use]
pub const fn type_num(name: &str) -> u32 {
    let b = name.as_bytes();
    let mut h: u32 = 33_554_467; // FreeBSD's `FNV1_32_INIT`
    let mut i = 0;
    while i < b.len() {
        h = h.wrapping_mul(0x0100_0193) ^ b[i] as u32;
        i += 1;
    }
    match h & 0xff {
        0 => 1,
        n => n,
    }
}

/// A vnode's identity: its file system and inode number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeId {
    pub fs: u64,
    pub ino: u64,
}

/// Ownership, permissions, and the modification time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Permission bits (`ALLPERMS`).
    pub perm: u16,
    pub uid: u32,
    pub gid: u32,
    pub mtime: Timespec,
}

impl Meta {
    /// Owned by root with permissions `perm`, modified at `mtime`.
    #[must_use]
    pub const fn root(perm: u16, mtime: Timespec) -> Self {
        Self {
            perm,
            uid: 0,
            gid: 0,
            mtime,
        }
    }
}

/// The caller's side of a transfer (FreeBSD's `struct uio`).
pub trait Uio {
    /// Bytes still to move.
    fn resid(&self) -> usize;
    /// Moves the start of `data` to the caller (a read); returns how much
    /// went, at most [`resid`](Self::resid).
    fn copy_out(&mut self, data: &[u8]) -> Result<usize, Errno>;
    /// Fills the start of `buf` from the caller (a write); returns how
    /// much came, at most [`resid`](Self::resid).
    fn copy_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno>;
}

/// A [`Uio`] over a kernel buffer, for reads (`copy_out` fills it).
#[derive(Debug)]
pub struct ReadBuf<'a> {
    buf: &'a mut [u8],
    done: usize,
}

impl<'a> ReadBuf<'a> {
    #[must_use]
    pub const fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, done: 0 }
    }

    /// The bytes read so far.
    #[must_use]
    pub fn filled(&self) -> &[u8] {
        &self.buf[..self.done]
    }
}

impl Uio for ReadBuf<'_> {
    fn resid(&self) -> usize {
        self.buf.len() - self.done
    }

    fn copy_out(&mut self, data: &[u8]) -> Result<usize, Errno> {
        let n = data.len().min(self.resid());
        self.buf[self.done..self.done + n].copy_from_slice(&data[..n]);
        self.done += n;
        Ok(n)
    }

    fn copy_in(&mut self, _: &mut [u8]) -> Result<usize, Errno> {
        Err(Errno::EINVAL)
    }
}

/// A regular file's contents.
pub trait RegularFile: Send + Sync {
    fn size(&self) -> u64;
    /// Reads from `offset` into `uio`; returns the bytes read (0 at or
    /// past the end).
    fn read(&self, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno>;
    /// The whole contents, if they sit in memory for the kernel's
    /// lifetime (the memory disk's files): mapping them needs no I/O.
    fn resident(&self) -> Option<&'static [u8]> {
        None
    }
}

/// One entry of a directory, as [`Directory::entries`] reports it.
#[derive(Debug)]
pub struct DirEntry<'a> {
    pub name: &'a [u8],
    pub vnode: &'a Arc<Vnode>,
}

/// A directory's contents.
pub trait Directory: Send + Sync {
    /// The entry called `name` (never `.` or `..`, which lookup handles).
    fn lookup(&self, name: &[u8]) -> Result<Arc<Vnode>, Errno>;
    /// The directory holding this one; `None` at its file system's root.
    fn parent(&self) -> Option<Arc<Vnode>>;
    /// Calls `f` with each entry from the `from`th on, in a stable order,
    /// until it returns `false` or the entries run out.
    fn entries(&self, from: u64, f: &mut dyn FnMut(DirEntry<'_>) -> bool);
}

/// A character device's operations.
pub trait CharDevice: Send + Sync {
    /// Called when the device is opened: `Some` opens that vnode instead
    /// (`/dev/tty` opens the caller's controlling terminal).
    fn open(&self) -> Result<Option<Arc<Vnode>>, Errno> {
        Ok(None)
    }
    /// Reads into `uio`; `nonblock` asks for `EAGAIN` instead of waiting.
    fn read(&self, s: &Sleepable, uio: &mut dyn Uio, nonblock: bool) -> Result<usize, Errno>;
    /// Writes from `uio`.
    fn write(&self, s: &Sleepable, uio: &mut dyn Uio, nonblock: bool) -> Result<usize, Errno>;
    /// Whether mapping it gives zero-filled anonymous memory, as
    /// `/dev/zero` does; other devices cannot be mapped yet.
    fn maps_as_zeros(&self) -> bool {
        false
    }
    /// Device-specific control: `arg` is the request's argument, already
    /// copied in and copied back out afterwards.
    fn ioctl(&self, _s: &Sleepable, _req: u32, _arg: &mut [u8]) -> Result<(), Errno> {
        Err(Errno::ENOTTY)
    }
}

/// A device node: the device number and the driver behind it.
#[derive(Clone)]
pub struct Device {
    pub rdev: u64,
    pub ops: Arc<dyn CharDevice>,
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Device({:#x})", self.rdev)
    }
}

/// What a vnode is.
#[derive(Clone)]
pub enum VnodeKind {
    Regular(Arc<dyn RegularFile>),
    Directory(Arc<dyn Directory>),
    Symlink(Arc<[u8]>),
    CharDevice(Device),
}

impl core::fmt::Debug for VnodeKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Regular(r) => write!(f, "Regular({} bytes)", r.size()),
            Self::Directory(_) => f.write_str("Directory"),
            Self::Symlink(t) => write!(f, "Symlink({t:?})"),
            Self::CharDevice(d) => d.fmt(f),
        }
    }
}

/// A file system object.
pub struct Vnode {
    fs: Arc<FileSystem>,
    ino: u64,
    meta: Meta,
    nlink: u64,
    kind: VnodeKind,
}

impl core::fmt::Debug for Vnode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Vnode({}:{})", self.fs.id, self.ino)
    }
}

impl Vnode {
    #[must_use]
    pub const fn new(
        fs: Arc<FileSystem>,
        ino: u64,
        meta: Meta,
        nlink: u64,
        kind: VnodeKind,
    ) -> Self {
        Self {
            fs,
            ino,
            meta,
            nlink,
            kind,
        }
    }

    #[must_use]
    pub fn id(&self) -> NodeId {
        NodeId {
            fs: self.fs.id,
            ino: self.ino,
        }
    }

    #[must_use]
    pub const fn ino(&self) -> u64 {
        self.ino
    }

    #[must_use]
    pub const fn kind(&self) -> &VnodeKind {
        &self.kind
    }

    #[must_use]
    pub fn fs(&self) -> &Arc<FileSystem> {
        &self.fs
    }

    #[must_use]
    pub const fn meta(&self) -> &Meta {
        &self.meta
    }

    /// The directory, if this is one.
    #[must_use]
    pub fn as_dir(&self) -> Option<&Arc<dyn Directory>> {
        match &self.kind {
            VnodeKind::Directory(d) => Some(d),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_dir(&self) -> bool {
        matches!(self.kind, VnodeKind::Directory(_))
    }

    /// The `d_type` of a directory entry for this vnode.
    #[must_use]
    pub const fn dtype(&self) -> u8 {
        match self.kind {
            VnodeKind::Regular(_) => DT_REG,
            VnodeKind::Directory(_) => DT_DIR,
            VnodeKind::Symlink(_) => DT_LNK,
            VnodeKind::CharDevice(_) => DT_CHR,
        }
    }

    /// The size `stat` reports.
    #[must_use]
    pub fn size(&self) -> u64 {
        match &self.kind {
            VnodeKind::Regular(f) => f.size(),
            // A nominal size, as UFS's first directory block.
            VnodeKind::Directory(_) => 512,
            VnodeKind::Symlink(t) => t.len() as u64,
            VnodeKind::CharDevice(_) => 0,
        }
    }

    /// `struct stat` for this vnode.
    #[must_use]
    pub fn stat(&self) -> Stat {
        let (ty, rdev) = match &self.kind {
            VnodeKind::Regular(_) => (S_IFREG, 0),
            VnodeKind::Directory(_) => (S_IFDIR, 0),
            VnodeKind::Symlink(_) => (S_IFLNK, 0),
            VnodeKind::CharDevice(d) => (S_IFCHR, d.rdev),
        };
        let size = self.size();
        let t = self.meta.mtime;
        Stat {
            st_dev: self.fs.id,
            st_ino: self.ino,
            st_nlink: self.nlink,
            st_mode: ty | (self.meta.perm & abi::stat::ALLPERMS),
            st_uid: self.meta.uid,
            st_gid: self.meta.gid,
            st_rdev: rdev,
            st_atim: t,
            st_mtim: t,
            st_ctim: t,
            st_birthtim: t,
            st_size: i64::try_from(size).unwrap_or(i64::MAX),
            st_blocks: i64::try_from(size.div_ceil(512)).unwrap_or(i64::MAX),
            st_blksize: i32::try_from(self.fs.io_size).unwrap_or(i32::MAX),
            ..Stat::default()
        }
    }
}

#[cfg(test)]
mod tests;
