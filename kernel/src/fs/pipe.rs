// SPDX-License-Identifier: BSD-2-Clause
//! Pipes. As on FreeBSD, both ends of a pipe read and write: each end has
//! a buffer the other end writes into.
//!
//! The buffer the first end reads starts at `PIPE_SIZE` (16 KiB), the
//! other at `SMALL_PIPE_SIZE` (a page); each is allocated on its first
//! write. A write grows its buffer, doubling, until what is there and what
//! comes fit, up to `BIG_PIPE_SIZE` (64 KiB). A write of at most
//! `PIPE_BUF` bytes goes in whole or waits; a larger one goes in pieces as
//! room appears. (FreeBSD also shrinks buffers when pipe memory runs low.) Reading returns what is
//! there, or waits for something; once the writing end is closed it
//! returns 0 (end of file). Writing with the reading end closed fails with
//! `EPIPE`; the caller sends `SIGPIPE`.
//!
//! The buffer's lock is held while user memory is copied (a fault may
//! sleep). What sleepers wait for (bytes there, room free, an end closed)
//! is mirrored in atomics, since a wait condition runs under the wait
//! queue's spin lock.
//!
//! Each direction has a [`Notify`] its readers and writers poll and
//! watch (`pipe_sel`); every read, write, and close notifies it.
//!
//! FreeBSD's direct-write path (mapping a writer's pages for the reader)
//! is not implemented.
//!
//! Behavior reference: sys/kern/sys_pipe.c.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::event::{
    EV_EOF, EVFILT_READ, EVFILT_WRITE, Kevent, POLLHUP, POLLIN, POLLINIGNEOF, POLLOUT, POLLRDNORM,
    POLLWRNORM,
};
use abi::fcntl::{FREAD, FWRITE, PIPE_BUF};
use abi::stat::{S_IFIFO, Stat};
use abi::time::Timespec;
use event::{Filter, Knote, Note, Notify, Wake};
use sched::lock::{LockClass, Mutex};
use sched::{Sleepable, WaitQueue};
use vfs::Uio;

/// `SMALL_PIPE_SIZE`, `PIPE_SIZE`, and `BIG_PIPE_SIZE`.
const SMALL_PIPE_SIZE: usize = 4096;
const PIPE_SIZE: usize = 16 * 1024;
const BIG_PIPE_SIZE: usize = 64 * 1024;

static PIPE_LOCK: LockClass = LockClass::new("pipe", 400);

/// Pipes' inode numbers (`pipe_ino`).
static NEXT_INO: AtomicU64 = AtomicU64::new(1);
/// `st_dev` of every pipe (FreeBSD's `pipedev_ino`).
const PIPE_DEV: u64 = 0xffff_fffe;

/// A circular buffer.
struct Ring {
    data: Vec<u8>,
    head: usize,
    len: usize,
    mtime: Timespec,
    atime: Timespec,
}

impl Ring {
    const fn new() -> Self {
        Self {
            data: Vec::new(),
            head: 0,
            len: 0,
            mtime: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            atime: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        }
    }

    /// Makes the buffer `size` bytes (at least its contents), keeping
    /// what is in it.
    fn resize(&mut self, size: usize) -> Result<(), Errno> {
        let mut v = Vec::new();
        v.try_reserve_exact(size).map_err(|_| Errno::ENOMEM)?;
        let first = self.len.min(self.data.len() - self.head);
        v.extend_from_slice(&self.data[self.head..self.head + first]);
        v.extend_from_slice(&self.data[..self.len - first]);
        v.resize(size, 0);
        self.data = v;
        self.head = 0;
        Ok(())
    }

    /// Moves up to `uio.resid()` bytes out.
    fn read(&mut self, uio: &mut dyn Uio) -> Result<usize, Errno> {
        let mut done = 0;
        while self.len > 0 && uio.resid() > 0 {
            let run = self.len.min(self.data.len() - self.head);
            let n = uio.copy_out(&self.data[self.head..self.head + run])?;
            self.head = (self.head + n) % self.data.len();
            self.len -= n;
            done += n;
            if n < run {
                break;
            }
        }
        if self.len == 0 {
            self.head = 0;
        }
        Ok(done)
    }

    /// Moves up to `max` bytes in.
    fn write(&mut self, uio: &mut dyn Uio, max: usize) -> Result<usize, Errno> {
        let cap = self.data.len();
        let mut done = 0;
        while done < max && self.len < cap {
            let tail = (self.head + self.len) % cap;
            let run = (cap - self.len).min(cap - tail).min(max - done);
            let n = uio.copy_in(&mut self.data[tail..tail + run])?;
            self.len += n;
            done += n;
            if n < run {
                break;
            }
        }
        Ok(done)
    }
}

/// One direction: the buffer one end reads and the other writes.
struct Channel {
    ring: Mutex<Ring>,
    /// The buffer's size when first written.
    initial: usize,
    /// Mirrors of `ring.len` and `ring.data.len()` for wait conditions.
    len: AtomicUsize,
    cap: AtomicUsize,
    reader_gone: AtomicBool,
    writer_gone: AtomicBool,
    /// Readers waiting for bytes, writers waiting for room.
    readable: WaitQueue,
    writable: WaitQueue,
    /// Its pollers and knotes; a FIFO's pipes share the FIFO's.
    notify: Arc<Notify>,
}

impl Channel {
    fn new(initial: usize, notify: Arc<Notify>) -> Self {
        Self {
            ring: Mutex::new(&PIPE_LOCK, Ring::new()),
            initial,
            len: AtomicUsize::new(0),
            cap: AtomicUsize::new(initial),
            reader_gone: AtomicBool::new(false),
            writer_gone: AtomicBool::new(false),
            readable: WaitQueue::new(),
            writable: WaitQueue::new(),
            notify,
        }
    }

    fn room(&self) -> usize {
        // Acquire: pairs with the release stores made under the lock.
        self.cap
            .load(Ordering::Acquire)
            .saturating_sub(self.len.load(Ordering::Acquire))
    }
}

/// A pipe: two ends, `0` and `1`; end `i` reads channel `i` and writes
/// channel `1 - i`.
pub(crate) struct Pipe {
    channels: [Channel; 2],
    ino: [u64; 2],
    ctime: Timespec,
    /// A FIFO's (`PIPE_TYPE_NAMED`): its `Notify` outlives the pipe.
    named: bool,
}

/// One end of a pipe; dropping it closes it.
pub(crate) struct End {
    pipe: Arc<Pipe>,
    side: usize,
}

/// A new pipe's two ends.
pub(crate) fn pair() -> (End, End) {
    pair_with(None)
}

/// A FIFO's pipe, notifying the FIFO's `notify`.
pub(crate) fn named(notify: &Arc<Notify>) -> (End, End) {
    pair_with(Some(notify))
}

fn pair_with(notify: Option<&Arc<Notify>>) -> (End, End) {
    let ino = NEXT_INO.fetch_add(2, Ordering::Relaxed);
    let n = || notify.map_or_else(|| Arc::new(Notify::new()), Arc::clone);
    let pipe = Arc::new(Pipe {
        channels: [
            Channel::new(PIPE_SIZE, n()),
            Channel::new(SMALL_PIPE_SIZE, n()),
        ],
        ino: [ino, ino + 1],
        ctime: super::now(),
        named: notify.is_some(),
    });
    (
        End {
            pipe: Arc::clone(&pipe),
            side: 0,
        },
        End { pipe, side: 1 },
    )
}

impl End {
    fn rx(&self) -> &Channel {
        &self.pipe.channels[self.side]
    }

    fn tx(&self) -> &Channel {
        &self.pipe.channels[1 - self.side]
    }

    /// Which of the pipe's two ends it is.
    pub(crate) fn side(&self) -> usize {
        self.side
    }

    /// Bytes waiting to be read (`FIONREAD`).
    pub(crate) fn readable(&self) -> usize {
        self.rx().len.load(Ordering::Acquire)
    }

    pub(crate) fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let ch = self.rx();
        if uio.resid() == 0 {
            return Ok(0);
        }
        loop {
            {
                let mut r = ch.ring.lock(s);
                if r.len > 0 {
                    let n = r.read(uio)?;
                    r.atime = super::now();
                    ch.len.store(r.len, Ordering::Release);
                    drop(r);
                    ch.writable.wake_all();
                    ch.notify.notify(0);
                    return Ok(n);
                }
            }
            if ch.writer_gone.load(Ordering::Acquire) {
                return Ok(0);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _w = sched::wait_message("piperd");
            crate::signal::sleep(
                s,
                &ch.readable,
                || ch.len.load(Ordering::Acquire) > 0 || ch.writer_gone.load(Ordering::Acquire),
                None,
            )?;
        }
    }

    /// Writes `uio`; returns what went in, which is everything unless the
    /// end is non-blocking or a signal came after some went in.
    pub(crate) fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let ch = self.tx();
        let total = uio.resid();
        let mut done = 0;
        loop {
            if ch.reader_gone.load(Ordering::Acquire) {
                return Err(Errno::EPIPE);
            }
            // Small writes are atomic: they wait for room for all of it.
            let need = if total <= PIPE_BUF { uio.resid() } else { 1 };
            {
                let mut r = ch.ring.lock(s);
                // Room for what is there and what comes, if it can be had.
                let mut size = if r.data.is_empty() {
                    ch.initial
                } else {
                    r.data.len()
                };
                size = size.max(SMALL_PIPE_SIZE);
                while size < r.len + uio.resid() && size < BIG_PIPE_SIZE {
                    size *= 2;
                }
                if size != r.data.len() {
                    r.resize(size)?;
                    ch.cap.store(size, Ordering::Release);
                }
                if r.data.len() - r.len >= need {
                    let max = uio.resid();
                    let n = r.write(uio, max)?;
                    r.mtime = super::now();
                    ch.len.store(r.len, Ordering::Release);
                    drop(r);
                    done += n;
                    ch.readable.wake_all();
                    ch.notify.notify(0);
                }
            }
            if uio.resid() == 0 {
                return Ok(done);
            }
            if nonblock {
                return if done > 0 {
                    Ok(done)
                } else {
                    Err(Errno::EAGAIN)
                };
            }
            let _w = sched::wait_message("pipewr");
            let woke = crate::signal::sleep(
                s,
                &ch.writable,
                || ch.room() >= need || ch.reader_gone.load(Ordering::Acquire),
                None,
            );
            if let Err(e) = woke {
                return if done > 0 { Ok(done) } else { Err(e) };
            }
        }
    }

    /// `fstat` of this end: what waits in its buffer, and when the
    /// buffer was last read and written (its creation, before either).
    pub(crate) fn stat(&self, s: &Sleepable) -> Stat {
        let ch = self.rx();
        let (size, atime, mtime) = {
            let r = ch.ring.lock(s);
            (r.len, r.atime, r.mtime)
        };
        let ctime = self.pipe.ctime;
        let or_ctime = |t: Timespec| if t == Timespec::default() { ctime } else { t };
        let page = usize::try_from(vm::PAGE_SIZE).unwrap_or(usize::MAX);
        Stat {
            st_dev: PIPE_DEV,
            st_ino: self.pipe.ino[self.side],
            st_mode: S_IFIFO,
            st_blksize: i32::try_from(page).unwrap_or(i32::MAX),
            st_size: i64::try_from(size).unwrap_or(i64::MAX),
            st_blocks: i64::try_from(size.div_ceil(page)).unwrap_or(i64::MAX),
            st_atim: or_ctime(atime),
            st_mtim: or_ctime(mtime),
            st_ctim: ctime,
            ..Stat::default()
        }
    }
}

impl Pipe {
    /// Bytes waiting in channel `side`.
    pub(crate) fn len(&self, side: usize) -> usize {
        self.channels[side].len.load(Ordering::Acquire)
    }

    /// Room in channel `side`.
    pub(crate) fn room(&self, side: usize) -> usize {
        self.channels[side].room()
    }
}

impl End {
    /// Its pipe, which a FIFO's filters read.
    pub(crate) fn pipe(&self) -> &Arc<Pipe> {
        &self.pipe
    }

    /// `pipe_poll` for a file open with `fflags`.
    pub(crate) fn poll(&self, fflags: u32, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        let (rx, tx) = (self.rx(), self.tx());
        // Registered whatever is asked: `POLLHUP` comes unasked.
        if let Some(w) = w {
            if fflags & FREAD != 0 {
                rx.notify.record(w);
            }
            if fflags & FWRITE != 0 && !tx.reader_gone.load(Ordering::Acquire) {
                tx.notify.record(w);
            }
        }
        let mut revents = 0;
        if fflags & FREAD != 0
            && events & (POLLIN | POLLRDNORM) != 0
            && rx.len.load(Ordering::Acquire) > 0
        {
            revents |= events & (POLLIN | POLLRDNORM);
        }
        let peer_gone = tx.reader_gone.load(Ordering::Acquire);
        if fflags & FWRITE != 0
            && events & (POLLOUT | POLLWRNORM) != 0
            && (peer_gone || tx.room() >= PIPE_BUF)
        {
            revents |= events & (POLLOUT | POLLWRNORM);
        }
        if events & POLLINIGNEOF == 0 && rx.writer_gone.load(Ordering::Acquire) {
            if fflags & FREAD != 0 {
                revents |= events & (POLLIN | POLLRDNORM);
            }
            if peer_gone {
                revents |= POLLHUP;
            }
        }
        revents
    }

    /// `pipe_kqfilter`: a filter the end is not open for never fires.
    pub(crate) fn kqfilter(&self, fflags: u32, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        match kev.filter {
            EVFILT_READ if fflags & FREAD == 0 => Ok(Box::new(crate::event::Never)),
            EVFILT_WRITE if fflags & FWRITE == 0 => Ok(Box::new(crate::event::Never)),
            EVFILT_READ => Ok(Box::new(PipeRead {
                pipe: Arc::clone(&self.pipe),
                side: self.side,
            })),
            EVFILT_WRITE => {
                if self.tx().reader_gone.load(Ordering::Acquire) {
                    return Err(Errno::EPIPE);
                }
                Ok(Box::new(PipeWrite {
                    pipe: Arc::clone(&self.pipe),
                    side: 1 - self.side,
                }))
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

/// `pipe_rfiltops`: `data` the bytes waiting; `EV_EOF` once the writer
/// has gone.
pub(crate) struct PipeRead {
    pub(crate) pipe: Arc<Pipe>,
    /// The channel read.
    pub(crate) side: usize,
}

impl PipeRead {
    fn ch(&self) -> &Channel {
        &self.pipe.channels[self.side]
    }

    /// `filt_piperead`; `eof_counts` is false for a FIFO reader no writer
    /// has come to since it opened.
    pub(crate) fn check(&self, note: &mut Note, eof_counts: bool) -> bool {
        let ch = self.ch();
        note.kev.data = i64::try_from(ch.len.load(Ordering::Acquire)).unwrap_or(i64::MAX);
        if ch.writer_gone.load(Ordering::Acquire) && eof_counts {
            note.kev.flags |= EV_EOF;
            return true;
        }
        note.kev.flags &= !EV_EOF;
        note.kev.data > 0
    }
}

impl Filter for PipeRead {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.ch().notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.ch().notify.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        self.check(note, true)
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self {
            pipe: Arc::clone(&self.pipe),
            side: self.side,
        }))
    }
}

/// `pipe_wfiltops`: `data` the room left; `EV_EOF` once the reader has
/// gone.
pub(crate) struct PipeWrite {
    pub(crate) pipe: Arc<Pipe>,
    /// The channel written.
    pub(crate) side: usize,
}

impl Filter for PipeWrite {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.pipe.channels[self.side].notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.pipe.channels[self.side].notify.remove(kn);
    }

    /// `filt_pipewrite`.
    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let ch = &self.pipe.channels[self.side];
        let gone = ch.reader_gone.load(Ordering::Acquire);
        if !gone || self.pipe.named {
            note.kev.data = i64::try_from(ch.room()).unwrap_or(i64::MAX);
        }
        if gone {
            note.kev.flags |= EV_EOF;
            return true;
        }
        note.kev.flags &= !EV_EOF;
        note.kev.data >= i64::try_from(PIPE_BUF).unwrap_or(i64::MAX)
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self {
            pipe: Arc::clone(&self.pipe),
            side: self.side,
        }))
    }
}

impl Drop for End {
    fn drop(&mut self) {
        let (rx, tx) = (self.rx(), self.tx());
        // `pipeclose`: this end's watchers look once more while it is still
        // there (a writer's knote keeps the room it last saw).
        rx.notify.notify(0);
        rx.reader_gone.store(true, Ordering::Release);
        tx.writer_gone.store(true, Ordering::Release);
        // Writers into this end get `EPIPE`; readers of the other, the end.
        rx.writable.wake_all();
        tx.readable.wake_all();
        // Those of this end's are done with it (a FIFO's watchers stay,
        // for the FIFO's next pipe); the other end's see the end.
        if self.pipe.named {
            rx.notify.notify(0);
        } else {
            rx.notify.clear();
        }
        tx.notify.notify(0);
    }
}
