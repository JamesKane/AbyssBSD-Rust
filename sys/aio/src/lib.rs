// SPDX-License-Identifier: BSD-2-Clause
//! POSIX asynchronous I/O's bookkeeping: one process's requests, their
//! states, `lio_listio` groups, and the order `aio_fsync` keeps, without
//! the I/O itself (the kernel's `aio` module does that, with its worker
//! threads and the sockets' own queues).
//!
//! A request (FreeBSD's `kaiocb`) is queued while the kernel is still
//! handing it to a backend (`KAIOCB_QUEUEING`), then pending in a
//! backend's queue, where it can be cancelled, or running, where it
//! cannot, then finished, waiting for `aio_return` or
//! `aio_waitcomplete`, which frees it. A backend's queue is cancellable
//! through a hook the backend sets while the request waits there and
//! clears when it takes the request to run it (`aio_set_cancel_function`,
//! `aio_clear_cancel_function`); a cancel in progress
//! (`KAIOCB_CANCELLING`) holds back the request's completion until it is
//! over, so the request cannot be freed under it.
//!
//! An `aio_fsync` waits for every write and read queued on the same file
//! before it (`KAIOCB_CHECKSYNC`, `pending`), and runs once the last of
//! them finishes.
//!
//! A `lio_listio` group counts its requests and those finished; while it
//! is being submitted it holds one count more, so neither its completion
//! nor its freeing happens before every request is in.
//!
//! Behavior reference: sys/kern/vfs_aio.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use abi::errno::Errno;

/// A request's number: the order requests were made in, system-wide
/// (`seqno`).
pub type JobId = u64;
/// A `lio_listio` group's number.
pub type LioId = u64;

/// `jobflags`.
mod flag {
    pub(super) const QUEUEING: u8 = 0x01;
    pub(super) const CANCELLED: u8 = 0x02;
    pub(super) const CANCELLING: u8 = 0x04;
    pub(super) const CHECKSYNC: u8 = 0x08;
    pub(super) const FINISHED: u8 = 0x20;
}

/// Where a cancellable request waits, which its cancel hook takes it out
/// of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// The worker threads' queue (`aio_cancel_daemon_job`).
    Daemon,
    /// Its file's earlier requests, as an `aio_fsync` (`aio_cancel_sync`).
    Sync,
    /// A socket buffer's queue (`soo_aio_cancel`).
    Socket,
}

/// One request's state.
#[derive(Clone, Debug)]
pub struct Job {
    pub id: JobId,
    /// The `aiocb`'s user address, which names it to the calls.
    pub ujob: u64,
    /// `aio_fildes`.
    pub fd: i32,
    /// The open file it is on, by identity.
    pub file: usize,
    /// An `aio_fsync`.
    pub sync: bool,
    /// Its `lio_listio` group.
    pub lio: Option<LioId>,
    /// `_aiocb_private.status` once finished.
    pub status: i64,
    /// `_aiocb_private.error` once finished: `None` for success.
    pub error: Option<Errno>,
    flags: u8,
    cancel: Option<Backend>,
    /// Earlier requests an `aio_fsync` waits for.
    pending: u32,
    /// In the process's list (`kaio_all`): queueing is over.
    listed: bool,
}

impl Job {
    /// Whether it has finished (`KAIOCB_FINISHED`).
    #[must_use]
    pub const fn finished(&self) -> bool {
        self.flags & flag::FINISHED != 0
    }

    /// Whether a cancel was asked for (`KAIOCB_CANCELLED`).
    #[must_use]
    pub const fn cancelled(&self) -> bool {
        self.flags & flag::CANCELLED != 0
    }
}

/// A `lio_listio` group (`aioliojob`).
#[derive(Clone, Debug, Default)]
pub struct Lio {
    count: u32,
    finished: u32,
    /// It signals when done (`LIOJ_SIGNAL`).
    pub signal: bool,
    /// The signal went (`LIOJ_SIGNAL_POSTED`).
    pub signal_posted: bool,
    /// The kevent went (`LIOJ_KEVENT_POSTED`).
    pub kevent_posted: bool,
}

/// What a finished request asks of the kernel (`aio_bio_done_notify`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub id: JobId,
    /// Signal and post kevents: the process is not being run down.
    pub notify: bool,
    /// Its group finished with it.
    pub lio_done: Option<LioId>,
    /// `aio_fsync`s that waited for it and may run now.
    pub fsync_ready: Vec<JobId>,
}

/// What [`Kaio::cancel_begin`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelStep {
    /// Already finished or already being cancelled: not cancelled.
    Skip,
    /// Running, with no hook: marked, but not cancelled.
    Marked,
    /// Waiting in this backend's queue: call its hook, then
    /// [`Kaio::cancel_end`].
    Call(Backend),
}

/// What `aio_suspend` should do for its list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suspend {
    /// One of them has finished.
    Ready,
    /// None of them is a request of this process's.
    NoJobs,
    /// Sleep until a request finishes.
    Wait,
}

/// A process's requests (`kaioinfo`).
#[derive(Debug, Default)]
pub struct Kaio {
    jobs: BTreeMap<JobId, Job>,
    /// Requests not finished (`kaio_jobqueue`), in order.
    queue: BTreeSet<JobId>,
    /// Finished requests, in the order they finished (`kaio_done`).
    done: VecDeque<JobId>,
    /// `aio_fsync`s waiting for earlier requests (`kaio_syncqueue`).
    syncq: Vec<JobId>,
    lios: BTreeMap<LioId, Lio>,
    next_lio: LioId,
    /// Requests running on a worker (`kaio_active_count`).
    active: u32,
    /// Requests listed (`kaio_count`).
    count: u32,
    /// Being run down at exit or exec (`KAIO_RUNDOWN`).
    rundown: bool,
}

impl Kaio {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests listed (`kaio_count`).
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// Requests on a worker now (`kaio_active_count`).
    #[must_use]
    pub const fn active(&self) -> u32 {
        self.active
    }

    /// Whether another request fits under the per-process limit.
    #[must_use]
    pub const fn admits(&self, max_queue_per_proc: u32) -> bool {
        self.count < max_queue_per_proc
    }

    /// A new request, being queued (`KAIOCB_QUEUEING`): invisible to the
    /// calls that look requests up until [`Kaio::queued`].
    pub fn begin(
        &mut self,
        id: JobId,
        ujob: u64,
        fd: i32,
        file: usize,
        sync: bool,
        lio: Option<LioId>,
    ) {
        self.jobs.insert(
            id,
            Job {
                id,
                ujob,
                fd,
                file,
                sync,
                lio,
                status: -1,
                error: Some(Errno::EINPROGRESS),
                flags: flag::QUEUEING,
                cancel: None,
                pending: 0,
                listed: false,
            },
        );
    }

    /// A request its backend refused: it goes, uncounted.
    pub fn abandon(&mut self, id: JobId) {
        if self.jobs.get(&id).is_some_and(|j| !j.listed) {
            self.jobs.remove(&id);
            self.syncq.retain(|&j| j != id);
        }
    }

    /// Queueing is over: the request is listed and counted, in its group
    /// too. If its backend finished it already, the completion deferred
    /// until now happens.
    pub fn queued(&mut self, id: JobId) -> Option<Done> {
        let j = self.jobs.get_mut(&id)?;
        j.flags &= !flag::QUEUEING;
        j.listed = true;
        let (lio, finished) = (j.lio, j.finished());
        self.count += 1;
        if let Some(l) = lio.and_then(|l| self.lios.get_mut(&l)) {
            l.count += 1;
        }
        if finished {
            Some(self.notify(id))
        } else {
            self.queue.insert(id);
            None
        }
    }

    /// Installs a backend's cancel hook (`aio_set_cancel_function`):
    /// false if a cancel was asked for already, and the backend must
    /// cancel the request itself.
    pub fn set_cancel(&mut self, id: JobId, b: Backend) -> bool {
        let Some(j) = self.jobs.get_mut(&id) else {
            return false;
        };
        if j.cancelled() {
            return false;
        }
        j.cancel = Some(b);
        true
    }

    /// Takes the hook away as the backend takes the request to run it
    /// (`aio_clear_cancel_function`): false while a cancel is running
    /// the hook, which then finishes the request.
    pub fn clear_cancel(&mut self, id: JobId) -> bool {
        let Some(j) = self.jobs.get_mut(&id) else {
            return false;
        };
        if j.flags & flag::CANCELLING != 0 {
            return false;
        }
        j.cancel = None;
        true
    }

    /// The request finished with `status` and `error` (`aio_complete`).
    /// Its completion waits while it is still being queued or cancelled.
    pub fn complete(&mut self, id: JobId, status: i64, error: Option<Errno>) -> Option<Done> {
        let j = self.jobs.get_mut(&id)?;
        if j.finished() {
            return None;
        }
        j.status = status;
        j.error = error;
        j.flags |= flag::FINISHED;
        if j.flags & (flag::QUEUEING | flag::CANCELLING) != 0 {
            return None;
        }
        self.queue.remove(&id);
        Some(self.notify(id))
    }

    /// `aio_bio_done_notify`'s bookkeeping: into the finished list; its
    /// group's count; the `aio_fsync`s waiting for it.
    fn notify(&mut self, id: JobId) -> Done {
        let mut out = Done {
            id,
            notify: !self.rundown,
            lio_done: None,
            fsync_ready: Vec::new(),
        };
        self.done.push_back(id);
        let Some(j) = self.jobs.get(&id) else {
            return out;
        };
        let (lio, file, checksync) = (j.lio, j.file, j.flags & flag::CHECKSYNC != 0);
        if let Some(l) = lio.and_then(|l| self.lios.get_mut(&l).map(|g| (l, g))) {
            l.1.finished += 1;
            if l.1.count == l.1.finished {
                out.lio_done = Some(l.0);
            }
        }
        if checksync {
            let mut ready = Vec::new();
            for &s in &self.syncq {
                let Some(sj) = self.jobs.get_mut(&s) else {
                    continue;
                };
                if sj.file != file || id >= s {
                    continue;
                }
                sj.pending = sj.pending.saturating_sub(1);
                if sj.pending == 0 {
                    ready.push(s);
                }
            }
            self.syncq.retain(|s| !ready.contains(s));
            for s in ready {
                if self.clear_cancel(s) {
                    out.fsync_ready.push(s);
                }
            }
        }
        out
    }

    /// An `aio_fsync` being queued: the requests on the same file queued
    /// before it that it must wait for are marked to tell it when they
    /// finish. Returns how many; with any, it waits in the sync queue,
    /// where [`Kaio::complete`] of the last one releases it.
    pub fn sync_deps(&mut self, id: JobId) -> u32 {
        let Some(file) = self.jobs.get(&id).map(|j| j.file) else {
            return 0;
        };
        let mut n = 0;
        for &q in &self.queue {
            if let Some(j) = self.jobs.get_mut(&q)
                && j.file == file
                && !j.sync
                && q < id
            {
                j.flags |= flag::CHECKSYNC;
                n += 1;
            }
        }
        if let Some(j) = self.jobs.get_mut(&id) {
            j.pending = n;
        }
        if n > 0 {
            self.syncq.push(id);
        }
        n
    }

    /// Takes an `aio_fsync` out of the sync queue (its cancel hook).
    pub fn unsync(&mut self, id: JobId) {
        self.syncq.retain(|&j| j != id);
    }

    /// Starts cancelling a request (`aio_cancel_job`, up to calling the
    /// hook).
    pub fn cancel_begin(&mut self, id: JobId) -> CancelStep {
        let Some(j) = self.jobs.get_mut(&id) else {
            return CancelStep::Skip;
        };
        if j.flags & (flag::CANCELLED | flag::FINISHED) != 0 {
            return CancelStep::Skip;
        }
        j.flags |= flag::CANCELLED;
        match j.cancel {
            None => CancelStep::Marked,
            Some(b) => {
                j.flags |= flag::CANCELLING;
                CancelStep::Call(b)
            }
        }
    }

    /// The hook has run: whether the request counts as cancelled (it
    /// finished with `ECANCELED`), and its deferred completion.
    pub fn cancel_end(&mut self, id: JobId) -> (bool, Option<Done>) {
        let Some(j) = self.jobs.get_mut(&id) else {
            return (false, None);
        };
        j.flags &= !flag::CANCELLING;
        if !j.finished() {
            return (false, None);
        }
        let cancelled = j.error == Some(Errno::ECANCELED);
        if j.flags & flag::QUEUEING != 0 {
            return (cancelled, None);
        }
        self.queue.remove(&id);
        (cancelled, Some(self.notify(id)))
    }

    /// A listed request by its `aiocb` (`kaio_all`).
    #[must_use]
    pub fn find(&self, ujob: u64) -> Option<&Job> {
        self.jobs.values().find(|j| j.listed && j.ujob == ujob)
    }

    /// A request by number.
    #[must_use]
    pub fn get(&self, id: JobId) -> Option<&Job> {
        self.jobs.get(&id)
    }

    /// `aio_error`: the error of a finished request, `EINPROGRESS` for
    /// one not finished, `None` for none.
    #[must_use]
    pub fn error_of(&self, ujob: u64) -> Option<i32> {
        self.find(ujob).map(|j| {
            if j.finished() {
                j.error.map_or(0, Errno::get)
            } else {
                Errno::EINPROGRESS.get()
            }
        })
    }

    /// `aio_return` (`Some(ujob)`) or `aio_waitcomplete` (`None`, the
    /// first to finish): takes a finished request and frees it
    /// (`aio_free_entry`), and its group if it was the last.
    pub fn take_done(&mut self, ujob: Option<u64>) -> Option<(Job, Option<LioId>)> {
        let at = match ujob {
            Some(u) => self
                .done
                .iter()
                .position(|id| self.jobs.get(id).is_some_and(|j| j.ujob == u))?,
            None if self.done.is_empty() => return None,
            None => 0,
        };
        let id = self.done.remove(at)?;
        self.free(id)
    }

    fn free(&mut self, id: JobId) -> Option<(Job, Option<LioId>)> {
        let j = self.jobs.remove(&id)?;
        self.count = self.count.saturating_sub(1);
        let mut freed = None;
        if let Some(l) = j.lio
            && let Some(g) = self.lios.get_mut(&l)
        {
            g.count = g.count.saturating_sub(1);
            g.finished = g.finished.saturating_sub(1);
            if g.count == 0 {
                self.lios.remove(&l);
                freed = Some(l);
            }
        }
        Some((j, freed))
    }

    /// `aio_suspend`'s test over the `aiocb`s in `list`.
    #[must_use]
    pub fn suspend(&self, list: &[u64]) -> Suspend {
        let mut any = false;
        for j in self.jobs.values().filter(|j| j.listed) {
            if list.contains(&j.ujob) {
                if j.finished() {
                    return Suspend::Ready;
                }
                any = true;
            }
        }
        if any { Suspend::Wait } else { Suspend::NoJobs }
    }

    /// The unfinished requests on descriptor `fd` (and `aiocb` `ujob` if
    /// given) that `aio_cancel` tries, in order.
    #[must_use]
    pub fn cancel_targets(&self, fd: i32, ujob: Option<u64>) -> Vec<JobId> {
        self.queue
            .iter()
            .filter(|id| {
                self.jobs
                    .get(id)
                    .is_some_and(|j| j.fd == fd && ujob.is_none_or(|u| u == j.ujob))
            })
            .copied()
            .collect()
    }

    /// The process is being run down (exit, exec): nothing more is
    /// notified. Returns the requests to cancel.
    pub fn begin_rundown(&mut self) -> Vec<JobId> {
        self.rundown = true;
        self.queue.iter().copied().collect()
    }

    /// Whether a finished request waits to be taken.
    #[must_use]
    pub fn has_done(&self) -> bool {
        !self.done.is_empty()
    }

    /// No request is unfinished or running.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.queue.is_empty() && self.active == 0
    }

    /// Frees every finished request, as a run down does.
    pub fn drain_done(&mut self) -> Vec<(Job, Option<LioId>)> {
        let mut out = Vec::new();
        while let Some(id) = self.done.pop_front() {
            out.extend(self.free(id));
        }
        out
    }

    /// Whether a worker may take another of this process's requests
    /// (`aio_selectjob`'s `max_aio_per_proc`).
    #[must_use]
    pub const fn can_start(&self, max_per_proc: u32) -> bool {
        self.active < max_per_proc
    }

    /// A worker took one.
    pub fn started(&mut self) {
        self.active += 1;
    }

    /// A worker is done with one.
    pub fn stopped(&mut self) {
        self.active = self.active.saturating_sub(1);
    }

    /// A new `lio_listio` group, holding its submission's extra count.
    pub fn lio_new(&mut self, signal: bool) -> LioId {
        self.next_lio += 1;
        let id = self.next_lio;
        self.lios.insert(
            id,
            Lio {
                count: 1,
                signal,
                ..Lio::default()
            },
        );
        id
    }

    /// The group, to read or post its notifications.
    pub fn lio_mut(&mut self, id: LioId) -> Option<&mut Lio> {
        self.lios.get_mut(&id)
    }

    /// Whether every request of the group submitted so far has finished
    /// (`lioj_count - 1 == lioj_finished_count`, the extra count held).
    #[must_use]
    pub fn lio_all_done(&self, id: LioId) -> bool {
        self.lios
            .get(&id)
            .is_none_or(|l| l.count.saturating_sub(1) == l.finished)
    }

    /// The submission lets go of its extra count; true if that freed the
    /// group.
    pub fn lio_release(&mut self, id: LioId) -> bool {
        let Some(l) = self.lios.get_mut(&id) else {
            return false;
        };
        l.count = l.count.saturating_sub(1);
        if l.count == 0 {
            self.lios.remove(&id);
            true
        } else {
            false
        }
    }

    /// Groups left (a run down must have none with requests).
    #[must_use]
    pub fn lios(&self) -> usize {
        self.lios.len()
    }
}

#[cfg(test)]
mod tests;
