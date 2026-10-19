// SPDX-License-Identifier: BSD-2-Clause
//! A process's descriptor table (FreeBSD's `filedesc`): numbered slots,
//! each an open file and its descriptor flags (close-on-exec,
//! close-on-fork). A new descriptor takes the lowest free number.
//!
//! Closing takes a file out of the table; the caller drops it after
//! letting go of the table's lock, since the last close of a pipe wakes
//! sleepers and that of a process descriptor may kill.
//!
//! Behavior reference: sys/kern/kern_descrip.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

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

/// The table.
#[derive(Clone, Default)]
pub(crate) struct FdTable {
    slots: Vec<Option<Slot>>,
}

fn index(fd: i32) -> Result<usize, Errno> {
    usize::try_from(fd).map_err(|_| Errno::EBADF)
}

impl FdTable {
    pub(crate) const fn new() -> Self {
        Self { slots: Vec::new() }
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

    /// The child's table after `fork`: everything but close-on-fork.
    pub(crate) fn fork(&self) -> Result<Self, Errno> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(self.slots.len())
            .map_err(|_| Errno::ENOMEM)?;
        slots.extend(
            self.slots
                .iter()
                .map(|s| s.as_ref().filter(|s| !s.flags.clofork).cloned()),
        );
        let mut t = Self { slots };
        t.trim();
        Ok(t)
    }

    /// Takes out the close-on-exec descriptors (`execve`).
    pub(crate) fn exec(&mut self) -> Vec<Slot> {
        let mut out = Vec::new();
        for s in &mut self.slots {
            if s.as_ref().is_some_and(|s| s.flags.cloexec)
                && let Some(s) = s.take()
            {
                out.push(s);
            }
        }
        self.trim();
        out
    }

    /// Takes out everything (exit).
    pub(crate) fn take_all(&mut self) -> Vec<Slot> {
        core::mem::take(&mut self.slots)
            .into_iter()
            .flatten()
            .collect()
    }
}
