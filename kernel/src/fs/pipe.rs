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
//! FreeBSD's direct-write path (mapping a writer's pages for the reader)
//! is not implemented.
//!
//! Behavior reference: sys/kern/sys_pipe.c.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::fcntl::PIPE_BUF;
use abi::stat::{S_IFIFO, Stat};
use abi::time::Timespec;
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
}

impl Channel {
    const fn new(initial: usize) -> Self {
        Self {
            ring: Mutex::new(&PIPE_LOCK, Ring::new()),
            initial,
            len: AtomicUsize::new(0),
            cap: AtomicUsize::new(initial),
            reader_gone: AtomicBool::new(false),
            writer_gone: AtomicBool::new(false),
            readable: WaitQueue::new(),
            writable: WaitQueue::new(),
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
}

/// One end of a pipe; dropping it closes it.
pub(crate) struct End {
    pipe: Arc<Pipe>,
    side: usize,
}

/// A new pipe's two ends.
pub(crate) fn pair() -> (End, End) {
    let ino = NEXT_INO.fetch_add(2, Ordering::Relaxed);
    let pipe = Arc::new(Pipe {
        channels: [Channel::new(PIPE_SIZE), Channel::new(SMALL_PIPE_SIZE)],
        ino: [ino, ino + 1],
        ctime: super::now(),
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
                    return Ok(n);
                }
            }
            if ch.writer_gone.load(Ordering::Acquire) {
                return Ok(0);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
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

impl Drop for End {
    fn drop(&mut self) {
        let (rx, tx) = (self.rx(), self.tx());
        rx.reader_gone.store(true, Ordering::Release);
        tx.writer_gone.store(true, Ordering::Release);
        // Writers into this end get `EPIPE`; readers of the other, the end.
        rx.writable.wake_all();
        tx.readable.wake_all();
    }
}
