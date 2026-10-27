// SPDX-License-Identifier: BSD-2-Clause
//! A process's descriptor table (FreeBSD's `filedesc`): numbered slots,
//! each an open file and its descriptor flags (close-on-exec,
//! close-on-fork). A new descriptor takes the lowest free number.
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

use abi::errno::Errno;

use super::file::File;

/// A descriptor's flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FdFlags {
    pub(crate) cloexec: bool,
    pub(crate) clofork: bool,
}

/// One open descriptor.
#[derive(Clone)]
pub(crate) struct Slot {
    pub(crate) file: Arc<File>,
    pub(crate) flags: FdFlags,
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

    pub(crate) fn slot(&self, fd: i32) -> Result<&Slot, Errno> {
        self.slots
            .get(index(fd)?)
            .and_then(Option::as_ref)
            .ok_or(Errno::EBADF)
    }

    pub(crate) fn get(&self, fd: i32) -> Result<Arc<File>, Errno> {
        self.slot(fd).map(|s| Arc::clone(&s.file))
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
        let fd = self.free(min, limit)?;
        self.put(fd, Slot { file, flags })?;
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
        let i = index(fd)?;
        if i >= limit {
            return Err(Errno::EBADF);
        }
        self.put(i, Slot { file, flags })
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
