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
//! Behavior reference: sys/fs/fifofs/fifo_vnops.c.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};

use abi::errno::Errno;
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
        });
        all.insert(key, Arc::downgrade(&f));
        f
    }
}

/// A new pipe for a side that finds its end closed; the other side's end
/// is replaced too, and dropped at once if that side has no one.
fn new_pipe(st: &mut State) {
    let (rd, wr) = pipe::pair();
    st.rd = (st.readers > 0).then(|| Arc::new(rd));
    st.wr = (st.writers > 0).then(|| Arc::new(wr));
}

/// A FIFO open for reading, writing, or both.
pub(crate) struct FifoFile {
    pub(crate) vnode: VnodeUse,
    fifo: Arc<Fifo>,
    read: bool,
    write: bool,
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
            }
            if write {
                st.writers += 1;
                st.wgen += 1;
            }
            if (read && st.rd.is_none()) || (write && st.wr.is_none()) {
                new_pipe(&mut st);
            }
            *fifo.gens.lock() = (st.rgen, st.wgen);
            (st.rgen, st.wgen)
        };
        fifo.opened.wake_all();
        // From here, dropping the file undoes the counts.
        let file = Self {
            vnode: v.use_ref(),
            fifo,
            read,
            write,
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
        let mut st = self.fifo.state.lock(&s);
        if self.read {
            st.readers -= 1;
            if st.readers == 0 {
                st.rd = None;
            }
        }
        if self.write {
            st.writers -= 1;
            if st.writers == 0 {
                st.wr = None;
            }
        }
    }
}
