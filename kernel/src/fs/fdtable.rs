// SPDX-License-Identifier: BSD-2-Clause
//! A process's descriptor table (FreeBSD's `filedesc`): numbered slots,
//! each an open file, its descriptor flags (close-on-exec,
//! close-on-fork), and its Capsicum limits, if any (`filecaps`). A new
//! descriptor takes the lowest free number.
//!
//! A descriptor is found with the rights the operation needs
//! ([`FdTable::checked`]): one without them is `ENOTCAPABLE`. Limits only
//! narrow, and travel with `dup`, `fork`, and `SCM_RIGHTS`.
//!
//! Closing takes a file out of the table; the caller drops it after
//! letting go of the table's lock, since the last close of a pipe wakes
//! sleepers and that of a process descriptor may kill. The knotes the
//! table's kqueues have on a descriptor go as it closes (FreeBSD's
//! `knote_fdclose`).
//!
//! Behavior reference: sys/kern/kern_descrip.c.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use event::Kqueue;

use abi::capsicum::{CAP_FCNTL, CAP_FCNTL_ALL, CAP_IOCTL, CapRights};
use abi::errno::Errno;
use alloc::boxed::Box;

use super::file::File;

/// A descriptor's Capsicum limits (FreeBSD's `struct filecaps`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileCaps {
    pub(crate) rights: CapRights,
    /// The `ioctl`s allowed; `None` for every one (`fc_nioctls == -1`).
    pub(crate) ioctls: Option<Vec<u64>>,
    /// The `fcntl` commands allowed (`CAP_FCNTL_*`).
    pub(crate) fcntls: u32,
}

impl FileCaps {
    /// No limit at all.
    pub(crate) const fn all() -> Self {
        Self {
            rights: CapRights::ALL,
            ioctls: None,
            fcntls: CAP_FCNTL_ALL,
        }
    }

    /// Whether these limit nothing.
    pub(crate) fn is_all(&self) -> bool {
        self.rights == CapRights::ALL && self.ioctls.is_none() && self.fcntls == CAP_FCNTL_ALL
    }

    /// `ENOTCAPABLE` unless every right in `need` is held.
    pub(crate) const fn check(&self, need: &CapRights) -> Result<(), Errno> {
        if self.rights.contains(need) {
            Ok(())
        } else {
            Err(Errno::ENOTCAPABLE)
        }
    }

    /// Narrows the rights to `rights` (`kern_cap_rights_limit`): without
    /// `CAP_IOCTL` no ioctl stays allowed, without `CAP_FCNTL` no `fcntl`.
    fn limit(&mut self, rights: CapRights) -> Result<(), Errno> {
        self.check(&rights)?;
        self.rights = rights;
        if !rights.has(CAP_IOCTL) {
            self.ioctls = Some(Vec::new());
        }
        if !rights.has(CAP_FCNTL) {
            self.fcntls = 0;
        }
        Ok(())
    }
}

/// Limits as a slot keeps them: none for no limit, the common case.
fn boxed(c: FileCaps) -> Option<Box<FileCaps>> {
    (!c.is_all()).then(|| Box::new(c))
}

/// A descriptor's flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FdFlags {
    pub(crate) cloexec: bool,
    pub(crate) clofork: bool,
    /// Every lookup from it stays beneath it, and it is no working
    /// directory (`UF_RESOLVE_BENEATH`); once set, it stays.
    pub(crate) resolve_beneath: bool,
}

/// One open descriptor.
#[derive(Clone)]
pub(crate) struct Slot {
    pub(crate) file: Arc<File>,
    pub(crate) flags: FdFlags,
    /// Its Capsicum limits; `None` for none.
    pub(crate) caps: Option<Box<FileCaps>>,
}

impl Slot {
    /// `ENOTCAPABLE` unless it holds every right in `need`.
    pub(crate) fn check(&self, need: &CapRights) -> Result<(), Errno> {
        self.caps.as_ref().map_or(Ok(()), |c| c.check(need))
    }

    /// Its limits, spelled out.
    pub(crate) fn caps(&self) -> FileCaps {
        self.caps.as_deref().cloned().unwrap_or_else(FileCaps::all)
    }
}

/// A `KQUEUE_CPONFORK` kqueue and its copy in a forked child's table.
pub(crate) type KqueueCopy = (Arc<Kqueue>, Arc<Kqueue>);

/// The table.
#[derive(Clone, Default)]
pub(crate) struct FdTable {
    slots: Vec<Option<Slot>>,
    /// The kqueues made here, whose knotes name these descriptors
    /// (`fd_kqlist`).
    kqueues: Vec<Weak<Kqueue>>,
}

fn index(fd: i32) -> Result<usize, Errno> {
    usize::try_from(fd).map_err(|_| Errno::EBADF)
}

impl FdTable {
    pub(crate) const fn new() -> Self {
        Self {
            slots: Vec::new(),
            kqueues: Vec::new(),
        }
    }

    /// Records a kqueue made by this table's process.
    pub(crate) fn add_kqueue(&mut self, kq: &Arc<Kqueue>) {
        self.kqueues.retain(|k| k.strong_count() > 0);
        self.kqueues.push(Arc::downgrade(kq));
    }

    /// Descriptor `fd` is closing: the knotes on it go.
    fn closing(&mut self, fd: usize) {
        if self.kqueues.is_empty() {
            return;
        }
        self.kqueues.retain(|k| k.strong_count() > 0);
        for kq in &self.kqueues {
            if let Some(kq) = kq.upgrade() {
                kq.fd_closed(fd);
            }
        }
    }

    /// One past the highest open descriptor (`fd_nfiles`, which
    /// `select` bounds its sets by).
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// How many descriptors are open (`filedesc_nfiles`).
    pub(crate) fn count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    pub(crate) fn slot(&self, fd: i32) -> Result<&Slot, Errno> {
        self.slots
            .get(index(fd)?)
            .and_then(Option::as_ref)
            .ok_or(Errno::EBADF)
    }

    pub(crate) fn get(&self, fd: i32) -> Result<Arc<File>, Errno> {
        self.slot(fd).map(|s| Arc::clone(&s.file))
    }

    /// Descriptor `fd`, if it holds the rights in `need` (`fget`'s
    /// `cap_check`): `EBADF` if it is not open, `ENOTCAPABLE` if it lacks
    /// one.
    pub(crate) fn checked(&self, fd: i32, need: &CapRights) -> Result<&Slot, Errno> {
        let s = self.slot(fd)?;
        s.check(need)?;
        Ok(s)
    }

    /// [`FdTable::checked`], the file referenced.
    pub(crate) fn get_checked(&self, fd: i32, need: &CapRights) -> Result<Arc<File>, Errno> {
        self.checked(fd, need).map(|s| Arc::clone(&s.file))
    }

    fn slot_mut(&mut self, fd: i32) -> Result<&mut Slot, Errno> {
        self.slots
            .get_mut(index(fd)?)
            .and_then(Option::as_mut)
            .ok_or(Errno::EBADF)
    }

    /// `cap_rights_limit`: `ENOTCAPABLE` to widen them.
    pub(crate) fn limit_rights(&mut self, fd: i32, rights: CapRights) -> Result<(), Errno> {
        let s = self.slot_mut(fd)?;
        let mut c = s.caps();
        c.limit(rights)?;
        s.caps = boxed(c);
        Ok(())
    }

    /// `cap_ioctls_limit`: to `cmds`, each of which must be allowed now.
    pub(crate) fn limit_ioctls(&mut self, fd: i32, cmds: Vec<u64>) -> Result<(), Errno> {
        let s = self.slot_mut(fd)?;
        let mut c = s.caps();
        if let Some(old) = &c.ioctls
            && (old.len() < cmds.len() || cmds.iter().any(|x| !old.contains(x)))
        {
            return Err(Errno::ENOTCAPABLE);
        }
        c.ioctls = Some(cmds);
        s.caps = boxed(c);
        Ok(())
    }

    /// `cap_fcntls_limit`: to `mask`, which must be allowed now.
    pub(crate) fn limit_fcntls(&mut self, fd: i32, mask: u32) -> Result<(), Errno> {
        let s = self.slot_mut(fd)?;
        let mut c = s.caps();
        if mask & !c.fcntls != 0 {
            return Err(Errno::ENOTCAPABLE);
        }
        c.fcntls = mask;
        s.caps = boxed(c);
        Ok(())
    }

    pub(crate) fn set_flags(&mut self, fd: i32, flags: FdFlags) -> Result<(), Errno> {
        let slot = self
            .slots
            .get_mut(index(fd)?)
            .and_then(Option::as_mut)
            .ok_or(Errno::EBADF)?;
        slot.flags = flags;
        Ok(())
    }

    /// The lowest free descriptor at or above `min`, below `limit`
    /// (`RLIMIT_NOFILE`): `EMFILE` if there is none.
    fn free(&self, min: usize, limit: usize) -> Result<usize, Errno> {
        let fd = (min..self.slots.len())
            .find(|&i| self.slots[i].is_none())
            .unwrap_or(self.slots.len().max(min));
        if fd >= limit {
            return Err(Errno::EMFILE);
        }
        Ok(fd)
    }

    /// Puts `slot` at `fd`, growing the table; returns what was there.
    fn put(&mut self, fd: usize, slot: Slot) -> Result<Option<Slot>, Errno> {
        if fd >= self.slots.len() {
            self.slots
                .try_reserve(fd + 1 - self.slots.len())
                .map_err(|_| Errno::ENOMEM)?;
            self.slots.resize(fd + 1, None);
        }
        if self.slots[fd].is_some() {
            self.closing(fd);
        }
        Ok(self.slots[fd].replace(slot))
    }

    /// Installs `file` at the lowest free descriptor at or above `min`.
    pub(crate) fn alloc(
        &mut self,
        file: Arc<File>,
        flags: FdFlags,
        min: usize,
        limit: usize,
    ) -> Result<i32, Errno> {
        self.alloc_with(file, flags, None, min, limit)
    }

    /// [`FdTable::alloc`] with Capsicum limits (a `dup`'s, a passed
    /// descriptor's, or what a lookup through a limited directory gives).
    pub(crate) fn alloc_with(
        &mut self,
        file: Arc<File>,
        flags: FdFlags,
        caps: Option<FileCaps>,
        min: usize,
        limit: usize,
    ) -> Result<i32, Errno> {
        let fd = self.free(min, limit)?;
        let caps = caps.and_then(boxed);
        self.put(fd, Slot { file, flags, caps })?;
        i32::try_from(fd).map_err(|_| Errno::EMFILE)
    }

    /// Installs `file` at `fd` (`dup2`); returns the file it replaces,
    /// for the caller to drop.
    pub(crate) fn install(
        &mut self,
        fd: i32,
        file: Arc<File>,
        flags: FdFlags,
        limit: usize,
    ) -> Result<Option<Slot>, Errno> {
        self.install_with(fd, file, flags, None, limit)
    }

    /// [`FdTable::install`] with Capsicum limits (`dup2`'s).
    pub(crate) fn install_with(
        &mut self,
        fd: i32,
        file: Arc<File>,
        flags: FdFlags,
        caps: Option<FileCaps>,
        limit: usize,
    ) -> Result<Option<Slot>, Errno> {
        let i = index(fd)?;
        if i >= limit {
            return Err(Errno::EBADF);
        }
        let caps = caps.and_then(boxed);
        self.put(i, Slot { file, flags, caps })
    }

    /// Takes `fd` out of the table.
    pub(crate) fn close(&mut self, fd: i32) -> Result<Slot, Errno> {
        let i = index(fd)?;
        let slot = self
            .slots
            .get_mut(i)
            .and_then(Option::take)
            .ok_or(Errno::EBADF)?;
        self.closing(i);
        self.trim();
        Ok(slot)
    }

    /// Takes out every descriptor in `lo..=hi` (or, with `cloexec`, just
    /// marks them close-on-exec; with `clofork`, close-on-fork).
    pub(crate) fn close_range(
        &mut self,
        lo: usize,
        hi: usize,
        cloexec: bool,
        clofork: bool,
    ) -> Vec<Slot> {
        let mut out = Vec::new();
        let end = hi.saturating_add(1).min(self.slots.len());
        for i in lo.min(end)..end {
            if cloexec || clofork {
                if let Some(s) = &mut self.slots[i] {
                    s.flags.cloexec |= cloexec;
                    s.flags.clofork |= clofork;
                }
            } else if let Some(s) = self.slots[i].take() {
                out.push(s);
                self.closing(i);
            }
        }
        self.trim();
        out
    }

    /// Drops trailing empty slots.
    fn trim(&mut self) {
        while matches!(self.slots.last(), Some(None)) {
            self.slots.pop();
        }
    }

    /// The child's table after `fork`: everything but close-on-fork and
    /// kqueues, except that a `KQUEUE_CPONFORK` kqueue becomes a new one
    /// (`kqueue_fork_alloc`); with each such pair, for the caller to copy
    /// the knotes into once the child's table is in place.
    pub(crate) fn fork(&self) -> Result<(Self, Vec<KqueueCopy>), Errno> {
        use super::file::{File, FileKind};
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(self.slots.len())
            .map_err(|_| Errno::ENOMEM)?;
        // Each kqueue file copied once, however many descriptors it has.
        let mut copies: Vec<(*const File, Arc<File>)> = Vec::new();
        let mut pairs = Vec::new();
        let mut kqueues = Vec::new();
        for s in &self.slots {
            let s = s.as_ref().filter(|s| !s.flags.clofork);
            let s = match s.map(|s| (s, s.file.kind())) {
                Some((s, FileKind::Kqueue(k))) => {
                    if !k.cponfork {
                        None
                    } else {
                        let key = Arc::as_ptr(&s.file);
                        let file = match copies.iter().find(|(p, _)| *p == key) {
                            Some((_, f)) => Arc::clone(f),
                            None => {
                                let new = crate::event::KqueueFile::new(true);
                                kqueues.push(Arc::downgrade(&new.kq));
                                pairs.push((Arc::clone(&k.kq), Arc::clone(&new.kq)));
                                let f = File::new(FileKind::Kqueue(new), s.file.flags());
                                copies.push((key, Arc::clone(&f)));
                                f
                            }
                        };
                        Some(Slot {
                            file,
                            flags: s.flags,
                            caps: s.caps.clone(),
                        })
                    }
                }
                _ => s.cloned(),
            };
            slots.push(s);
        }
        let mut t = Self { slots, kqueues };
        t.trim();
        Ok((t, pairs))
    }

    /// Takes out the close-on-exec descriptors (`execve`).
    pub(crate) fn exec(&mut self) -> Vec<Slot> {
        let mut out = Vec::new();
        for i in 0..self.slots.len() {
            if self.slots[i].as_ref().is_some_and(|s| s.flags.cloexec)
                && let Some(s) = self.slots[i].take()
            {
                out.push(s);
                self.closing(i);
            }
        }
        self.trim();
        out
    }

    /// Takes out everything (exit).
    pub(crate) fn take_all(&mut self) -> Vec<Slot> {
        for i in 0..self.slots.len() {
            if self.slots[i].is_some() {
                self.closing(i);
            }
        }
        core::mem::take(&mut self.slots)
            .into_iter()
            .flatten()
            .collect()
    }
}
