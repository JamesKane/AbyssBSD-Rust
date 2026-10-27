// SPDX-License-Identifier: BSD-2-Clause
//! Named pipes (FIFOs): a FIFO node in any file system, opened, is a pipe
//! its readers and writers share (FreeBSD's `fifofs`).
//!
//! Opening for reading waits for a writer, and opening for writing waits
//! for a reader, unless the open is non-blocking: then a reader goes
//! ahead (and reads end of file until a writer comes), and a writer with
//! no reader gets `ENXIO`. Opening for both never waits. Data goes through
//! a pipe ([`super::pipe`]) whose reading end the readers share and whose
//! writing end the writers share. When the last reader closes, writers get
//! `EPIPE`; when the last writer closes, readers read what is left, then
//! end of file. A side that comes back to a half-closed FIFO starts a new
//! pipe.
//!
//! Polling and kqueues follow FreeBSD's named pipe: the FIFO's end of
//! file is set when either side's last closes and cleared when that side
//! comes back, and a reader that opened with no writer sees no hang-up
//! until a writer has come and gone (`f_pipegen` against `pipe_wgen`).
//!
//! Behavior reference: sys/fs/fifofs/fifo_vnops.c.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::event::{
    EV_EOF, EVFILT_READ, EVFILT_WRITE, Kevent, POLLHUP, POLLIN, POLLINIGNEOF, POLLOUT, POLLPRI,
    POLLRDBAND, POLLRDNORM, POLLWRNORM,
};
use abi::fcntl::{FREAD, FWRITE, PIPE_BUF};
use event::{Filter, Knote, Note, Notify, Wake};
use sched::lock::{LockClass, Mutex};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vfs::{Uio, Vnode, VnodeUse};

use super::pipe::{self, End};

/// Above the vnode locks (a FIFO is opened with none held), below the
/// pipe's buffer locks.
static FIFO_LOCK: LockClass = LockClass::new("fifo", 360);

/// Who has a FIFO open, and its pipe's ends.
struct State {
    readers: u32,
    writers: u32,
    /// The end the readers read: `None` once the last reader closed.
    rd: Option<Arc<End>>,
    /// The end the writers write: `None` once the last writer closed.
    wr: Option<Arc<End>>,
    /// Bumped when a reader or a writer opens, which waiting opens of
    /// the other side wait for (FreeBSD's `fi_rgen`, `fi_wgen`).
    rgen: u64,
    wgen: u64,
}

/// A FIFO in use.
pub(crate) struct Fifo {
    state: Mutex<State>,
    /// Mirrors of the generations, for the wait conditions (which run
    /// under the wait queue's spin lock).
    gens: SpinLock<(u64, u64)>,
    opened: WaitQueue,
    /// Its pollers and knotes, which its pipes notify.
    notify: Arc<Notify>,
    /// The pipe in use, for the filters.
    pipe: SpinLock<Option<Arc<pipe::Pipe>>>,
    /// `PIPE_EOF`.
    eof: AtomicBool,
    /// `pipe_wgen`: bumped when the last writer closes.
    wgen: AtomicU64,
}

/// The FIFOs in use, by mount and inode.
static FIFOS: SpinLock<BTreeMap<(u64, u64), Weak<Fifo>>> = SpinLock::new(BTreeMap::new());

impl Fifo {
    /// The FIFO behind node `v`, made on its first open.
    fn of(v: &Vnode) -> Arc<Self> {
        let key = (v.mount().id, v.ino());
        let mut all = FIFOS.lock();
        if let Some(f) = all.get(&key).and_then(Weak::upgrade) {
            return f;
        }
        all.retain(|_, w| w.strong_count() > 0);
        let f = Arc::new(Self {
            state: Mutex::new(
                &FIFO_LOCK,
                State {
                    readers: 0,
                    writers: 0,
                    rd: None,
                    wr: None,
                    rgen: 0,
                    wgen: 0,
                },
            ),
            gens: SpinLock::new((0, 0)),
            opened: WaitQueue::new(),
            notify: Arc::new(Notify::new()),
            pipe: SpinLock::new(None),
            eof: AtomicBool::new(false),
            wgen: AtomicU64::new(0),
        });
        all.insert(key, Arc::downgrade(&f));
        f
    }
}

/// A new pipe for a side that finds its end closed; the other side's end
/// is replaced too, and dropped at once if that side has no one.
fn new_pipe(fifo: &Fifo, st: &mut State) {
    let (rd, wr) = pipe::named(&fifo.notify);
    *fifo.pipe.lock() = Some(Arc::clone(rd.pipe()));
    st.rd = (st.readers > 0).then(|| Arc::new(rd));
    st.wr = (st.writers > 0).then(|| Arc::new(wr));
}

/// A FIFO open for reading, writing, or both.
pub(crate) struct FifoFile {
    pub(crate) vnode: VnodeUse,
    fifo: Arc<Fifo>,
    read: bool,
    write: bool,
    /// `f_pipegen`: the writer generation a reader opened in.
    pipegen: u64,
}

impl FifoFile {
    /// Opens the FIFO node `v` (FreeBSD's `fifo_open`).
    pub(crate) fn open(
        s: &Sleepable,
        v: &Arc<Vnode>,
        read: bool,
        write: bool,
        nonblock: bool,
    ) -> Result<Self, Errno> {
        let fifo = Fifo::of(v);
        let (rgen, wgen) = {
            let mut st = fifo.state.lock(s);
            if write && !read && nonblock && st.readers == 0 {
                return Err(Errno::ENXIO);
            }
            if read {
                st.readers += 1;
                st.rgen += 1;
                if st.readers == 1 {
                    fifo.eof.store(false, Ordering::Release);
                }
            }
            if write {
                st.writers += 1;
                st.wgen += 1;
                if st.writers == 1 {
                    fifo.eof.store(false, Ordering::Release);
                }
            }
            if (read && st.rd.is_none()) || (write && st.wr.is_none()) {
                new_pipe(&fifo, &mut st);
            }
            *fifo.gens.lock() = (st.rgen, st.wgen);
            (st.rgen, st.wgen)
        };
        fifo.opened.wake_all();
        fifo.notify.notify(0);
        // As FreeBSD counts it when the read side opens, before the write
        // side of the same open.
        let pipegen = {
            let writers = u64::from(fifo.state.lock(s).writers) - u64::from(write);
            fifo.wgen.load(Ordering::Acquire).wrapping_sub(writers)
        };
        // From here, dropping the file undoes the counts.
        let file = Self {
            vnode: v.use_ref(),
            fifo,
            read,
            write,
            pipegen,
        };
        if read && !write && !nonblock {
            let has_writer = {
                let st = file.fifo.state.lock(s);
                st.writers > 0
            };
            if !has_writer {
                let f = &file.fifo;
                crate::signal::sleep(s, &f.opened, || f.gens.lock().1 != wgen, None)?;
            }
        }
        if write && !read {
            let has_reader = {
                let st = file.fifo.state.lock(s);
                st.readers > 0
            };
            if !has_reader {
                let f = &file.fifo;
                crate::signal::sleep(s, &f.opened, || f.gens.lock().0 != rgen, None)?;
            }
        }
        Ok(file)
    }

    pub(crate) fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let rd = self.fifo.state.lock(s).rd.clone();
        match rd {
            Some(end) => end.read(s, uio, nonblock),
            None => Ok(0),
        }
    }

    pub(crate) fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let wr = self.fifo.state.lock(s).wr.clone();
        match wr {
            Some(end) => end.write(s, uio, nonblock),
            None => Err(Errno::EPIPE),
        }
    }

    /// Whether a reader sees the end of file: not before a writer has
    /// come and gone since it opened.
    fn eof_counts(&self) -> bool {
        self.pipegen != self.fifo.wgen.load(Ordering::Acquire)
    }

    /// `pipe_poll` for a named pipe open with `fflags`.
    pub(crate) fn poll(
        &self,
        _s: &Sleepable,
        fflags: u32,
        mut events: i16,
        w: Option<&Arc<dyn Wake>>,
    ) -> i16 {
        if let Some(w) = w {
            self.fifo.notify.record(w);
        }
        let f = &self.fifo;
        let (len, room) = f
            .pipe
            .lock()
            .as_ref()
            .map_or((0, PIPE_BUF), |p| (p.len(0), p.room(0)));
        let eof = f.eof.load(Ordering::Acquire);
        let mut revents = 0;
        if fflags & FREAD != 0 && events & (POLLIN | POLLRDNORM) != 0 && len > 0 {
            revents |= events & (POLLIN | POLLRDNORM);
        }
        if fflags & FWRITE != 0 && events & (POLLOUT | POLLWRNORM) != 0 && (eof || room >= PIPE_BUF)
        {
            revents |= events & (POLLOUT | POLLWRNORM);
        }
        let levents = events & (POLLIN | POLLINIGNEOF | POLLPRI | POLLRDNORM | POLLRDBAND);
        if fflags & FREAD != 0 && levents != 0 && !self.eof_counts() {
            events |= POLLINIGNEOF;
        }
        if events & POLLINIGNEOF == 0 && eof {
            if fflags & FREAD != 0 {
                revents |= events & (POLLIN | POLLRDNORM);
            }
            revents |= POLLHUP;
        }
        revents
    }

    /// The named pipe's filters (`pipe_kqfilter`), or `None` for one the
    /// vnode serves.
    pub(crate) fn kqfilter(
        &self,
        fflags: u32,
        kev: &Kevent,
    ) -> Option<Result<Box<dyn Filter>, Errno>> {
        let mk = |write| -> Box<dyn Filter> {
            Box::new(FifoFilter {
                fifo: Arc::clone(&self.fifo),
                write,
                pipegen: self.pipegen,
            })
        };
        Some(Ok(match kev.filter {
            EVFILT_READ if fflags & FREAD == 0 => Box::new(crate::event::Never),
            EVFILT_WRITE if fflags & FWRITE == 0 => Box::new(crate::event::Never),
            EVFILT_READ => mk(false),
            EVFILT_WRITE => mk(true),
            _ => return None,
        }))
    }

    /// Bytes waiting to be read (`FIONREAD`).
    pub(crate) fn readable(&self, s: &Sleepable) -> usize {
        self.fifo
            .state
            .lock(s)
            .rd
            .as_ref()
            .map_or(0, |e| e.readable())
    }
}

impl Drop for FifoFile {
    fn drop(&mut self) {
        // SAFETY: open files are dropped in thread context (closing a
        // descriptor, or a process's exit).
        let s = unsafe { Sleepable::assume() };
        let f = &self.fifo;
        let mut st = f.state.lock(&s);
        let mut changed = false;
        if self.read {
            st.readers -= 1;
            if st.readers == 0 {
                st.rd = None;
                f.eof.store(true, Ordering::Release);
                changed = true;
            }
        }
        if self.write {
            st.writers -= 1;
            if st.writers == 0 {
                st.wr = None;
                f.eof.store(true, Ordering::Release);
                f.wgen.fetch_add(1, Ordering::AcqRel);
                changed = true;
            }
        }
        drop(st);
        if changed {
            f.notify.notify(0);
        }
    }
}

/// A named pipe's `EVFILT_READ` (`filt_piperead`) and `EVFILT_WRITE`
/// (`filt_pipewrite`).
struct FifoFilter {
    fifo: Arc<Fifo>,
    write: bool,
    pipegen: u64,
}

impl Filter for FifoFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.fifo.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.fifo.notify.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let f = &self.fifo;
        let (len, room) = f
            .pipe
            .lock()
            .as_ref()
            .map_or((0, PIPE_BUF), |p| (p.len(0), p.room(0)));
        let eof = f.eof.load(Ordering::Acquire);
        if self.write {
            note.kev.data = i64::try_from(room).unwrap_or(i64::MAX);
            if eof {
                note.kev.flags |= EV_EOF;
                return true;
            }
            note.kev.flags &= !EV_EOF;
            return room >= PIPE_BUF;
        }
        note.kev.data = i64::try_from(len).unwrap_or(i64::MAX);
        if eof && self.pipegen != f.wgen.load(Ordering::Acquire) {
            note.kev.flags |= EV_EOF;
            return true;
        }
        note.kev.flags &= !EV_EOF;
        len > 0
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self {
            fifo: Arc::clone(&self.fifo),
            write: self.write,
            pipegen: self.pipegen,
        }))
    }
}
