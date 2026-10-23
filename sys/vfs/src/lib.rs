// SPDX-License-Identifier: BSD-2-Clause
//! The virtual file system (docs/VFS.md): vnodes, the mounts they belong
//! to, the name cache, path lookup across mounts, and the generic layer's
//! operations, plus the file systems that need nothing from the kernel
//! (tmpfs, and the fixed trees of tarfs and the boot devfs).
//!
//! - [`Vnode`]: one in-core file, its node behind a shared/exclusive lock
//!   ([`vnode`]); [`VnodeUse`] marks it in use.
//! - [`Mount`] and [`FileSystem`]: a mounted instance and its type's
//!   operations, with the table of in-core vnodes ([`mount`]).
//! - [`node`]: what a file system implements, by kind.
//! - [`cache`]: the name cache; [`namei`]: path lookup.
//! - [`ops`]: create, remove, link, rename, and setattr with FreeBSD's
//!   rules, and `getdirentries` packing.
//!
//! Data moves through a [`Uio`], FreeBSD's `struct uio`: the caller's
//! buffers, which the kernel backs with user memory and tests with slices.
//!
//! Behavior reference: sys/kern/vfs_*.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unsafe_code)]

extern crate alloc;

pub mod cache;
pub mod lockf;
pub mod mount;
pub mod namei;
pub mod node;
pub mod ops;
pub mod pagecache;
pub mod tarfs;
pub mod tmpfs;
pub mod tree;
pub mod vnode;

use abi::errno::Errno;
use abi::time::Timespec;

pub use mount::{FileSystem, FsStats, Mount, MountInfo, Rename, type_num};
pub use node::{
    Attr, CharDevice, Create, D_DISK, D_TTY, Device, DeviceNode, DirEntry, DirectoryNode, Kind,
    Made, NewNode, Node, NodeOps, RegularNode, SetAttr, SymlinkNode,
};
pub use ops::Cred;
pub use vnode::{State, Vnode, VnodeUse};

/// A vnode's identity: its file system and inode number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId {
    pub fs: u64,
    pub ino: u64,
}

/// Ownership, permissions, and the modification time, for building fixed
/// trees ([`tree`]).
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

/// The clock file systems stamp times with; the kernel sets it at boot
/// (host tests leave it: times are 0).
static CLOCK: sync::Once<fn() -> Timespec> = sync::Once::new();

/// Sets the clock [`now`] reads (once).
pub fn set_clock(f: fn() -> Timespec) {
    let _ = CLOCK.set(f);
}

/// The time now, for file times (FreeBSD's `vfs_timestamp`).
#[must_use]
pub fn now() -> Timespec {
    CLOCK.get().map_or(Timespec::default(), |f| f())
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

/// A [`Uio`] over a kernel buffer, for writes (`copy_in` drains it).
#[derive(Debug)]
pub struct WriteBuf<'a> {
    buf: &'a [u8],
    done: usize,
}

impl<'a> WriteBuf<'a> {
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, done: 0 }
    }
}

impl Uio for WriteBuf<'_> {
    fn resid(&self) -> usize {
        self.buf.len() - self.done
    }

    fn copy_out(&mut self, _: &[u8]) -> Result<usize, Errno> {
        Err(Errno::EINVAL)
    }

    fn copy_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let n = buf.len().min(self.resid());
        buf[..n].copy_from_slice(&self.buf[self.done..self.done + n]);
        self.done += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests;
