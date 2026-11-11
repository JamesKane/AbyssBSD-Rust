// SPDX-License-Identifier: BSD-2-Clause
//! nullfs (docs/VFS.md §10.5): a second view of part of another mount, as
//! FreeBSD's loopback file system gives it. Each vnode stacks on the
//! vnode below it (FreeBSD's `null_node`), holding a use of it, and every
//! operation goes to that one:
//!
//! - Its file data is the lower vnode's: reads and writes are the lower
//!   vnode's, and its VM object is the lower one's, so a mapping through
//!   either maps the same pages (`null_open` sharing `v_object`).
//! - Advisory locks, the writer and text counts, FIFOs, and (with
//!   `unixbypass`, the default) bound sockets are the lower vnode's
//!   ([`crate::Node::lower`]).
//! - Names are not cached at this layer (`null_lookup` enters nothing):
//!   each lookup asks the lower directory, whose name cache answers.
//! - Changes to directories are made below through the generic layer,
//!   which checks and keeps that mount's caches; a rename is made below
//!   whole ([`crate::ops::rename`]). The generic layer has checked this
//!   layer's rules first: its read-only flag, and that both ends are on
//!   this mount (`EXDEV` otherwise).
//! - `stat` reports the lower file's attributes with this mount's `st_dev`
//!   (`null_getattr`); `statfs` the lower mount's counts and type number
//!   (`nullfs_statfs`).
//! - A vnode nothing uses is kept while the mount caches (`cache`, the
//!   default), and goes with its last use otherwise (`nocache`,
//!   `null_want_recycle`); removing a file below lets idle vnodes of it
//!   go either way, so it is freed (`nullfs_unlink_lowervp`).
//!
//! Behavior reference: sys/fs/nullfs (`null_vfsops.c`, `null_vnops.c`,
//! `null_subr.c`).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use sched::Sleepable;

use crate::Uio;
use crate::mount::{FileSystem, FsStats, Mount, Rename};
use crate::namei::Namespace;
use crate::node::{
    Attr, Create, Device, DeviceNode, DirEntry, DirectoryNode, Kind, Made, NewNode, Node, NodeOps,
    RegularNode, SetAttr, SymlinkNode,
};
use crate::ops::Cred;
use crate::vnode::{Vnode, VnodeUse};

/// How a nullfs mount behaves (its mount options).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Keep unused vnodes (`cache`; `nocache` clears it).
    pub cache: bool,
    /// A socket bound through this mount is the lower vnode's
    /// (`unixbypass`, the default; `nounixbypass` clears it).
    pub unp_bypass: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cache: true,
            unp_bypass: true,
        }
    }
}

/// A mounted nullfs: the lower directory (or file) it shows, held in use
/// while mounted (`nullm_lowerrootvp`).
pub struct NullFs {
    lower_root: VnodeUse,
    opts: Options,
    /// The mount this file system is, once [`NullFs::attach`] recorded it
    /// with the mount below.
    upper: AtomicU64,
}

impl core::fmt::Debug for NullFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "NullFs({:?})", self.lower_root.vnode())
    }
}

impl NullFs {
    /// A nullfs over `lower`, a directory or a regular file.
    ///
    /// # Errors
    ///
    /// `EINVAL` for anything else (`nullfs_mount`).
    pub fn new(lower: &Arc<Vnode>, opts: Options) -> Result<Self, Errno> {
        if !matches!(lower.kind(), Kind::Directory | Kind::Regular) {
            return Err(Errno::EINVAL);
        }
        Ok(Self {
            lower_root: lower.use_ref(),
            opts,
            upper: AtomicU64::new(0),
        })
    }

    /// The mount below (`nullm_vfs`).
    #[must_use]
    pub fn lower_mount(&self) -> &Arc<Mount> {
        self.lower_root.mount()
    }

    /// The vnode it shows.
    #[must_use]
    pub fn lower_root(&self) -> &Arc<Vnode> {
        self.lower_root.vnode()
    }

    #[must_use]
    pub fn options(&self) -> Options {
        self.opts
    }

    /// Records `upper`, the mount of this file system, with the mount
    /// below, which then stays mounted until it goes
    /// (`vfs_register_upper_from_vp`).
    ///
    /// # Errors
    ///
    /// `ENOENT` while the mount below is being unmounted.
    pub fn attach(&self, upper: &Arc<Mount>) -> Result<(), Errno> {
        self.lower_mount().add_upper(upper)?;
        self.upper.store(upper.id, Ordering::Release);
        Ok(())
    }

    /// Undoes [`Self::attach`] (an unmount, or a mount that failed after).
    pub fn detach(&self) {
        let id = self.upper.swap(0, Ordering::AcqRel);
        if id != 0 {
            self.lower_mount().remove_upper(id);
        }
    }

    fn node(&self, lower: Arc<Vnode>) -> NodeOps {
        let kind = lower.kind();
        let n = Box::new(NullNode {
            lower: lower.use_ref(),
            opts: self.opts,
        });
        match kind {
            Kind::Regular => NodeOps::Regular(n),
            Kind::Directory => NodeOps::Directory(n),
            Kind::Symlink => NodeOps::Symlink(n),
            Kind::CharDevice => NodeOps::Device(n),
            other => NodeOps::Other(other, n),
        }
    }
}

impl FileSystem for NullFs {
    fn root(&self) -> u64 {
        self.lower_root.ino()
    }

    fn load(&self, s: &Sleepable, ino: u64) -> Result<NodeOps, Errno> {
        // `nullfs_vget`: the lower file system's vnode, layered.
        let lower = if ino == self.lower_root.ino() {
            Arc::clone(self.lower_root.vnode())
        } else {
            self.lower_mount().vget(s, ino)?
        };
        Ok(self.node(lower))
    }

    fn statfs(&self, s: &Sleepable) -> Result<FsStats, Errno> {
        let lower = self.lower_mount();
        let st = lower.statfs(s)?;
        Ok(FsStats {
            type_num: Some(st.type_num.unwrap_or(lower.type_num)),
            flags_mask: None,
            ..st
        })
    }

    fn rename(&self, _s: &Sleepable, _r: Rename<'_>) -> Result<(), Errno> {
        // The generic layer renames below instead (`null_rename`).
        Err(Errno::EXDEV)
    }

    fn pathconf(&self, kind: Kind, name: u32) -> Result<i64, Errno> {
        self.lower_mount().fs().pathconf(kind, name)
    }

    fn unmount(&self, _s: &Sleepable) -> Result<(), Errno> {
        self.detach();
        Ok(())
    }

    fn caches_created(&self) -> bool {
        false
    }

    fn name_cache(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A nullfs vnode's node: the vnode below, held in use (`null_lowervp`,
/// `VREF`ed once).
struct NullNode {
    lower: VnodeUse,
    opts: Options,
}

/// The lower vnode of a nullfs node the generic layer hands over.
fn lower_of(n: &dyn Node) -> Result<&Arc<Vnode>, Errno> {
    n.as_any()
        .downcast_ref::<NullNode>()
        .map(|n| n.lower.vnode())
        .ok_or(Errno::EXDEV)
}

impl Node for NullNode {
    fn attr(&self, s: &Sleepable) -> Attr {
        // `st_dev` is this mount's, which the vnode supplies.
        self.lower.attr(s).unwrap_or_default()
    }

    fn setattr(&mut self, s: &Sleepable, a: &SetAttr) -> Result<(), Errno> {
        // Checked here already with the caller's credential; made below.
        crate::ops::setattr(s, &self.lower, a, &Cred::root(), false)
    }

    fn fsync(&mut self, s: &Sleepable) -> Result<(), Errno> {
        self.lower.fsync(s)
    }

    fn need_inactive(&self, s: &Sleepable) -> bool {
        // To go with the last use, or to let an unlinked file below go.
        !self.opts.cache || self.lower.attr(s).is_ok_and(|a| a.nlink == 0)
    }

    fn reclaim(self: Box<Self>, _s: &Sleepable) {
        // Dropping the use of the vnode below may make it inactive.
        drop(self);
    }

    fn get_extattr(&self, s: &Sleepable, ns: u8, name: &[u8]) -> Result<Vec<u8>, Errno> {
        self.lower.read(s).live()?.node().get_extattr(s, ns, name)
    }

    fn set_extattr(
        &mut self,
        s: &Sleepable,
        ns: u8,
        name: &[u8],
        value: &[u8],
    ) -> Result<(), Errno> {
        self.lower
            .write(s)
            .live_mut()?
            .node_mut()
            .set_extattr(s, ns, name, value)
    }

    fn delete_extattr(&mut self, s: &Sleepable, ns: u8, name: &[u8]) -> Result<(), Errno> {
        self.lower
            .write(s)
            .live_mut()?
            .node_mut()
            .delete_extattr(s, ns, name)
    }

    fn list_extattr(&self, s: &Sleepable, ns: u8) -> Result<Vec<Vec<u8>>, Errno> {
        self.lower.read(s).live()?.node().list_extattr(s, ns)
    }

    fn vptocnp(&self, s: &Sleepable) -> Option<(u64, Vec<u8>)> {
        // `null_vptocnp`: the lower vnode's name, which its cache keeps.
        let l = self.lower.vnode();
        if let Some((p, name)) = l.named()
            && p.mount().id == l.mount().id
        {
            return Some((p.ino(), name));
        }
        // A lower layer's own answer (nullfs over nullfs).
        l.read(s).live().ok().and_then(|n| n.node().vptocnp(s))
    }

    fn lower(&self) -> Option<Arc<Vnode>> {
        Some(Arc::clone(self.lower.vnode()))
    }

    fn unp_bypass(&self) -> bool {
        self.opts.unp_bypass
    }

    fn read_link(&self, s: &Sleepable) -> Result<Vec<u8>, Errno> {
        self.lower.readlink(s)
    }

    fn recycle_when_unused(&self) -> bool {
        !self.opts.cache
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl RegularNode for NullNode {
    fn read(&self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        self.lower.read_at(s, offset, uio)
    }

    fn write(&mut self, s: &Sleepable, offset: u64, uio: &mut dyn Uio) -> Result<usize, Errno> {
        self.lower.write_at(s, offset, uio)
    }

    fn resident(&self, s: &Sleepable) -> Option<&'static [u8]> {
        self.lower.resident(s)
    }

    fn object(&self, s: &Sleepable) -> Option<Arc<vm::object::Object>> {
        self.lower.object(s)
    }
}

impl SymlinkNode for NullNode {
    fn target(&self, s: &Sleepable) -> Result<Vec<u8>, Errno> {
        self.lower.readlink(s)
    }
}

impl DeviceNode for NullNode {
    fn device(&self, s: &Sleepable) -> Result<Device, Errno> {
        self.lower.device(s)
    }
}

impl DirectoryNode for NullNode {
    fn lookup(&self, s: &Sleepable, name: &[u8]) -> Result<u64, Errno> {
        // `null_lookup`: the lower directory's lookup, never crossing a
        // mount below (as `VOP_LOOKUP` does not).
        let v = Namespace::child(s, &self.lower, name)?;
        if v.mount().id != self.lower.mount().id {
            return Err(Errno::ENOENT);
        }
        Ok(v.ino())
    }

    fn parent(&self, s: &Sleepable) -> u64 {
        match self.lower.read(s).live() {
            Ok(NodeOps::Directory(d)) => d.parent(s),
            _ => self.lower.ino(),
        }
    }

    fn create(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        node: &NewNode,
        c: &Create,
    ) -> Result<Made, Errno> {
        let (v, _) = crate::ops::create(s, &self.lower, name, node, c, &Cred::root(), true)?;
        Ok(Made::from(v.ino()))
    }

    fn link(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        _ino: u64,
        target: &mut dyn Node,
    ) -> Result<(), Errno> {
        let t = Arc::clone(lower_of(target)?);
        crate::ops::link(s, &t, &self.lower, name, &Cred::root())
    }

    fn remove(&mut self, s: &Sleepable, name: &[u8], _child: &mut dyn Node) -> Result<(), Errno> {
        crate::ops::remove(s, &self.lower, name, &Cred::root(), false)
    }

    fn rmdir(
        &mut self,
        s: &Sleepable,
        name: &[u8],
        _child: &mut dyn DirectoryNode,
    ) -> Result<(), Errno> {
        crate::ops::remove(s, &self.lower, name, &Cred::root(), true)
    }

    fn readdir(
        &self,
        s: &Sleepable,
        from: u64,
        f: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        self.lower.readdir(s, from, f)
    }
}
