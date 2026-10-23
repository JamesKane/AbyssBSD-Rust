// SPDX-License-Identifier: BSD-2-Clause
//! What a file system implements for its files (docs/VFS.md §7): the
//! in-core node behind a vnode, with operations by kind.
//!
//! The node lives inside its vnode while the vnode exists: a file system
//! hands it over in [`FileSystem::load`](crate::FileSystem::load) and gets
//! it back in [`Node::reclaim`]. Operations that only look take `&self`
//! and run with the vnode locked shared; operations that change take
//! `&mut self`, under the exclusive lock. The generic layer has already
//! checked permissions, kinds, and names when it calls them.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::dirent::{DT_BLK, DT_CHR, DT_DIR, DT_FIFO, DT_LNK, DT_REG, DT_SOCK};
use abi::errno::Errno;
use abi::stat::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, S_IFSOCK};
use abi::time::Timespec;
use sched::Sleepable;

use crate::Uio;

/// What a vnode is. Fixed for its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    /// A block device node: FreeBSD's file systems keep them, though no
    /// device is opened through one.
    BlockDevice,
    Fifo,
    Socket,
}

impl Kind {
    /// The `S_IFMT` bits.
    #[must_use]
    pub const fn ifmt(self) -> u16 {
        match self {
            Self::Regular => S_IFREG,
            Self::Directory => S_IFDIR,
            Self::Symlink => S_IFLNK,
            Self::CharDevice => S_IFCHR,
            Self::BlockDevice => S_IFBLK,
            Self::Fifo => S_IFIFO,
            Self::Socket => S_IFSOCK,
        }
    }

    /// A directory entry's `d_type`.
    #[must_use]
    pub const fn dtype(self) -> u8 {
        match self {
            Self::Regular => DT_REG,
            Self::Directory => DT_DIR,
            Self::Symlink => DT_LNK,
            Self::CharDevice => DT_CHR,
            Self::BlockDevice => DT_BLK,
            Self::Fifo => DT_FIFO,
            Self::Socket => DT_SOCK,
        }
    }
}

/// A node's attributes (FreeBSD's `struct vattr`): what `stat` reports,
/// apart from the identity the vnode supplies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    /// Permission and set-id bits (`07777`).
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
    pub size: u64,
    /// Bytes of storage the file uses (`st_blocks` is this over 512).
    pub bytes: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub birthtime: Timespec,
    /// `chflags` flags.
    pub flags: u32,
    pub generation: u64,
    /// The device number, for device nodes.
    pub rdev: u64,
    /// The preferred I/O size; 0 for the mount's.
    pub blksize: u32,
    /// The number `stat` reports, when it is not the vnode's (0): a file
    /// system whose numbers can repeat, as msdosfs's FAT32 root shares
    /// FreeBSD's number with the first entry in it.
    pub fileid: u64,
}

/// Changes to a node's attributes, each optional (FreeBSD's `VOP_SETATTR`
/// with `VNOVAL` for the rest). The file system updates `ctime` itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetAttr {
    pub mode: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Truncate or extend (regular files only).
    pub size: Option<u64>,
    pub atime: Option<Timespec>,
    pub mtime: Option<Timespec>,
    pub birthtime: Option<Timespec>,
    pub flags: Option<u32>,
}

/// What a new directory entry names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewNode {
    Regular,
    Directory,
    Symlink(Vec<u8>),
    /// `mknod` of a character device: the number only (devfs serves the
    /// devices themselves).
    CharDevice(u64),
    /// `mknod` of a block device: the number only.
    BlockDevice(u64),
    Fifo,
    Socket,
}

impl NewNode {
    #[must_use]
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Regular => Kind::Regular,
            Self::Directory => Kind::Directory,
            Self::Symlink(_) => Kind::Symlink,
            Self::CharDevice(_) => Kind::CharDevice,
            Self::BlockDevice(_) => Kind::BlockDevice,
            Self::Fifo => Kind::Fifo,
            Self::Socket => Kind::Socket,
        }
    }
}

/// Who creates a node, and with which permission bits (already masked by
/// the umask).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Create {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub time: Timespec,
}

/// One directory entry, as [`DirectoryNode::readdir`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirEntry<'a> {
    pub name: &'a [u8],
    pub ino: u64,
    pub kind: Kind,
    /// Where reading resumes after this entry (its `d_off`).
    pub next: u64,
}

/// Operations every node has.
pub trait Node: Send + Sync + Any {
    fn attr(&self) -> Attr;

    /// Applies `a`; the generic layer checked permissions and that a size
    /// change is for a regular file.
    fn setattr(&mut self, s: &Sleepable, a: &SetAttr) -> Result<(), Errno>;

    /// Writes the node's own metadata (its inode) and whatever else the
    /// file system keeps dirty for it; the file's pages went first.
    fn fsync(&mut self, _s: &Sleepable) -> Result<(), Errno> {
        Ok(())
    }

    /// Whether the last use going leaves [`Self::inactive`] work (FreeBSD's
    /// `VOP_NEED_INACTIVE`): asked with the vnode only shared, so most
    /// last closes take no exclusive lock.
    fn need_inactive(&self, _s: &Sleepable) -> bool {
        true
    }

    /// The last use of the vnode went: free the node's storage if it is
    /// unlinked (it is reclaimed next).
    fn inactive(&mut self, _s: &Sleepable) {}

    /// The vnode goes: hand the node back to the file system (or free it,
    /// if inactive freed it).
    fn reclaim(self: Box<Self>, s: &Sleepable);

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// A regular file's contents.
pub trait RegularNode: Node {
    /// Reads from `offset` into `uio`; returns the bytes read (0 at or
    /// past the end).
    fn read(&self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno>;

    /// Writes `uio` at `offset`, extending the file; returns the bytes
    /// written.
    fn write(&mut self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno>;

    /// The whole contents, if they sit in memory for the kernel's
    /// lifetime (the memory disk's files): mapping them needs no I/O.
    fn resident(&self) -> Option<&'static [u8]> {
        None
    }

    /// The VM object holding the file's pages, which mappings share
    /// (FreeBSD's `v_object`), if the file system keeps one.
    fn object(&self) -> Option<Arc<vm::object::Object>> {
        None
    }
}

/// A node [`DirectoryNode::create`] made.
pub struct Made {
    pub ino: u64,
    /// The node, for its vnode (FreeBSD's `VOP_CREATE` returns the vnode).
    pub ops: Option<NodeOps>,
}

impl core::fmt::Debug for Made {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Made({}, node {})", self.ino, self.ops.is_some())
    }
}

impl From<u64> for Made {
    fn from(ino: u64) -> Self {
        Self { ino, ops: None }
    }
}

/// A directory.
pub trait DirectoryNode: Node {
    /// The inode named `name` (never `.` or `..`), or `ENOENT`.
    fn lookup(&self, s: &Sleepable, name: &[u8]) -> Result<u64, Errno>;

    /// The inode of `..` (this directory's own at the file system's root).
    fn parent(&self) -> u64;

    /// Makes a node and links it as `name`, which does not exist; returns
    /// its inode, and the node itself if the file system has it at hand
    /// (then it is not loaded again).
    fn create(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        node: &NewNode,
        c: &Create,
    ) -> Result<Made, Errno>;

    /// Links the existing non-directory `target` (inode `ino`) as `name`,
    /// which does not exist.
    fn link(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        ino: u64,
        target: &mut dyn Node,
    ) -> Result<(), Errno>;

    /// Removes the entry `name` for the non-directory `child`.
    fn remove(&mut self, s: &Sleepable, name: &[u8], child: &mut dyn Node) -> Result<(), Errno>;

    /// Removes the entry `name` for the directory `child`: `ENOTEMPTY`
    /// unless it holds only `.` and `..`.
    fn rmdir(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        child: &mut dyn DirectoryNode,
    ) -> Result<(), Errno>;

    /// Calls `f` with each entry from cookie `from` on (0 is the start,
    /// with `.` and `..` first), in a stable order, until it returns
    /// `false` or the entries run out.
    fn readdir(
        &self,
        s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno>;
}

/// A symbolic link.
pub trait SymlinkNode: Node {
    fn target(&self, s: &Sleepable) -> Result<Vec<u8>, Errno>;
}

/// `FIODTYPE`'s device kinds (FreeBSD's `D_DISK`, `D_TTY`).
pub const D_DISK: u32 = 0x0002;
pub const D_TTY: u32 = 0x0004;

/// A character device's operations, as devfs serves them.
pub trait CharDevice: Send + Sync {
    /// Called when the device is opened: `Some` opens that vnode instead
    /// (`/dev/tty` opens the caller's controlling terminal).
    fn open(&self) -> Result<Option<Arc<crate::Vnode>>, Errno> {
        Ok(None)
    }
    /// Reads into `uio` from `offset` (the file's offset, which disks use
    /// and terminals ignore); `nonblock` asks for `EAGAIN` instead of
    /// waiting.
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        offset: u64,
        nonblock: bool,
    ) -> Result<usize, Errno>;
    /// Writes from `uio` at `offset`.
    fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        offset: u64,
        nonblock: bool,
    ) -> Result<usize, Errno>;
    /// Whether mapping it gives zero-filled anonymous memory, as
    /// `/dev/zero` does; other devices cannot be mapped yet.
    fn maps_as_zeros(&self) -> bool {
        false
    }
    /// Its kind, as `FIODTYPE` reports it (FreeBSD's `d_flags &
    /// D_TYPEMASK`): [`D_DISK`], [`D_TTY`], or 0.
    fn dtype(&self) -> u32 {
        0
    }
    /// Device-specific control: `arg` is the request's argument, already
    /// copied in and copied back out afterwards.
    fn ioctl(&self, _s: &Sleepable, _req: u32, _arg: &mut [u8]) -> Result<(), Errno> {
        Err(Errno::ENOTTY)
    }
}

/// A device: its number and the driver behind it.
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

/// A device node: devfs's, whose opens reach the device.
pub trait DeviceNode: Node {
    fn device(&self) -> Device;
}

/// A node, by kind.
pub enum NodeOps {
    Regular(Box<dyn RegularNode>),
    Directory(Box<dyn DirectoryNode>),
    Symlink(Box<dyn SymlinkNode>),
    /// A character device served by devfs.
    Device(Box<dyn DeviceNode>),
    /// A node with no operations of its own: a FIFO, a socket, or a device
    /// node outside devfs (which, as on FreeBSD, cannot be opened).
    Other(Kind, Box<dyn Node>),
}

impl NodeOps {
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Self::Regular(_) => Kind::Regular,
            Self::Directory(_) => Kind::Directory,
            Self::Symlink(_) => Kind::Symlink,
            Self::Device(_) => Kind::CharDevice,
            Self::Other(k, _) => *k,
        }
    }

    /// The operations every node has.
    #[must_use]
    pub fn node(&self) -> &dyn Node {
        match self {
            Self::Regular(n) => &**n,
            Self::Directory(n) => &**n,
            Self::Symlink(n) => &**n,
            Self::Device(n) => &**n,
            Self::Other(_, n) => &**n,
        }
    }

    pub fn node_mut(&mut self) -> &mut dyn Node {
        match self {
            Self::Regular(n) => &mut **n,
            Self::Directory(n) => &mut **n,
            Self::Symlink(n) => &mut **n,
            Self::Device(n) => &mut **n,
            Self::Other(_, n) => &mut **n,
        }
    }

    /// Hands the node back to its file system.
    pub fn reclaim(self, s: &Sleepable) {
        match self {
            Self::Regular(n) => n.reclaim(s),
            Self::Directory(n) => n.reclaim(s),
            Self::Symlink(n) => n.reclaim(s),
            Self::Device(n) => n.reclaim(s),
            Self::Other(_, n) => n.reclaim(s),
        }
    }
}

impl core::fmt::Debug for NodeOps {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "NodeOps({:?})", self.kind())
    }
}
