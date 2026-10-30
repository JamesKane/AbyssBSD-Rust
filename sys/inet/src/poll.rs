// SPDX-License-Identifier: BSD-2-Clause
//! `poll` and kqueue filters on internet sockets: the socket layer's
//! generic ones, which TCP and UDP both use.
//!
//! Behavior reference: sys/kern/uipc_socket.c (`sopoll_generic`,
//! `soreadabledata`, `sowriteable`, `filt_soread`, `filt_sowrite`,
//! `filt_soempty`).

use alloc::boxed::Box;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::event::{
    EV_EOF, EVFILT_EMPTY, EVFILT_READ, EVFILT_WRITE, Kevent, NOTE_LOWAT, POLLHUP, POLLIN, POLLOUT,
    POLLPRI, POLLRDBAND, POLLRDHUP, POLLRDNORM, POLLWRNORM,
};
use abi::inet::ss;
use event::{Filter, Knote, Note, Wake};

use crate::{Inner, Proto, Socket};

/// `POLLINIGNEOF`.
const POLLINIGNEOF: i16 = 0x2000;

impl Inner {
    /// `sbavail` of the receive buffer, less control.
    fn avail(&self) -> u32 {
        self.rcv.cc
    }

    /// `soreadabledata`.
    fn readable(&self) -> bool {
        self.avail() >= self.rcv.lowat.max(1) || self.error.is_some()
    }

    /// `sowriteable`.
    fn writeable(&self, proto: Proto) -> bool {
        (self.snd.space() >= self.snd.lowat
            && (self.state & ss::ISCONNECTED != 0 || proto == Proto::Udp))
            || self.snd.cant
            || self.error.is_some()
    }

    fn listen_queue(&self) -> Option<usize> {
        self.tcp
            .as_ref()
            .and_then(|t| t.listen.as_ref())
            .map(|l| l.comp.len())
    }
}

impl Socket {
    /// `sopoll_generic`: the events of `events` that hold, `w`
    /// registered first on what it would wait for.
    pub fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if let Some(w) = w {
            if events & (POLLIN | POLLRDNORM | POLLRDHUP | POLLPRI | POLLRDBAND) != 0 {
                self.rd.record(w);
            }
            if events & (POLLOUT | POLLWRNORM) != 0 {
                self.wr.record(w);
            }
        }
        let i = self.inner.lock();
        if let Some(n) = i.listen_queue() {
            return if events & (POLLIN | POLLRDNORM) == 0 {
                0
            } else if n > 0 {
                events & (POLLIN | POLLRDNORM)
            } else if events & POLLINIGNEOF == 0 && i.error.is_some() {
                (events & (POLLIN | POLLRDNORM)) | POLLHUP
            } else {
                0
            };
        }
        let mut revents = 0;
        // A spliced buffer's data and room are the splice's.
        if events & (POLLIN | POLLRDNORM) != 0 && i.readable() && i.splice.is_none() {
            revents |= events & (POLLIN | POLLRDNORM);
        }
        if events & (POLLOUT | POLLWRNORM) != 0
            && i.writeable(self.proto)
            && i.splice_back.is_none()
        {
            revents |= events & (POLLOUT | POLLWRNORM);
        }
        if events & (POLLPRI | POLLRDBAND) != 0 && crate::tcp::urgent_pending(&i) {
            revents |= events & (POLLPRI | POLLRDBAND);
        }
        if events & POLLINIGNEOF == 0 && i.rcv.cant {
            revents |= events & (POLLIN | POLLRDNORM);
            if i.snd.cant {
                revents |= POLLHUP;
            }
        }
        if i.rcv.cant {
            revents |= events & POLLRDHUP;
        }
        revents
    }
}

/// The filter for a knote on socket `so` (`sokqfilter_generic`).
///
/// # Errors
///
/// `EINVAL` for a filter sockets have not.
pub fn filter(so: &Arc<Socket>, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
    let which = match kev.filter {
        EVFILT_READ => Which::Read,
        EVFILT_WRITE => Which::Write,
        EVFILT_EMPTY => Which::Empty,
        _ => return Err(Errno::EINVAL),
    };
    Ok(Box::new(SockFilter {
        so: Arc::clone(so),
        which,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Read,
    Write,
    Empty,
}

struct SockFilter {
    so: Arc<Socket>,
    which: Which,
}

impl SockFilter {
    fn notify(&self) -> &event::Notify {
        if self.which == Which::Read {
            &self.so.rd
        } else {
            &self.so.wr
        }
    }

    /// `filt_soread`.
    fn read(&self, note: &mut Note) -> bool {
        let i = self.so.inner.lock();
        if let Some(n) = i.listen_queue() {
            note.kev.data = i64::try_from(n).unwrap_or(0);
            if let Some(e) = i.error {
                note.kev.flags |= EV_EOF;
                note.kev.fflags = e.get().cast_unsigned();
                return true;
            }
            return n > 0;
        }
        if i.splice.is_some() {
            return false;
        }
        note.kev.data = i64::from(i.avail());
        if i.rcv.cant {
            note.kev.flags |= EV_EOF;
            note.kev.fflags = i.error.map_or(0, |e| e.get().cast_unsigned());
            return true;
        }
        if i.error.is_some() {
            return true;
        }
        if note.sfflags & NOTE_LOWAT != 0 {
            note.kev.data >= note.sdata
        } else {
            i.avail() >= i.rcv.lowat
        }
    }

    /// `filt_sowrite`.
    fn write(&self, note: &mut Note) -> bool {
        let i = self.so.inner.lock();
        if i.listen_queue().is_some() {
            return false;
        }
        note.kev.data = i64::from(i.snd.space());
        if i.snd.cant {
            note.kev.flags |= EV_EOF;
            note.kev.fflags = i.error.map_or(0, |e| e.get().cast_unsigned());
            return true;
        }
        if i.error.is_some() {
            return true;
        }
        if i.state & ss::ISCONNECTED == 0 && self.so.proto == Proto::Tcp {
            return false;
        }
        if note.sfflags & NOTE_LOWAT != 0 {
            note.kev.data >= note.sdata
        } else {
            note.kev.data >= i64::from(i.snd.lowat)
        }
    }

    /// `filt_soempty`.
    fn empty(&self, note: &mut Note) -> bool {
        let i = self.so.inner.lock();
        if i.listen_queue().is_some() {
            return true;
        }
        note.kev.data = i64::from(i.snd.cc);
        note.kev.data == 0
    }
}

impl Filter for SockFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.notify().add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.notify().remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        match self.which {
            Which::Read => self.read(note),
            Which::Write => self.write(note),
            Which::Empty => self.empty(note),
        }
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self {
            so: Arc::clone(&self.so),
            which: self.which,
        }))
    }
}
