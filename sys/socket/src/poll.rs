// SPDX-License-Identifier: BSD-2-Clause
//! `poll` and kqueue filters on sockets.
//!
//! Behavior reference: sys/kern/uipc_usrreq.c
//! (`uipc_sopoll_stream_or_seqpacket`, `uipc_filt_sowrite`,
//! `uipc_filt_soempty`), sys/kern/uipc_socket.c (`sopoll_generic`,
//! `filt_soread`, `filt_sowrite`, `filt_soempty`).

use alloc::boxed::Box;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::event::{
    EV_EOF, EVFILT_EMPTY, EVFILT_READ, EVFILT_WRITE, Kevent, NOTE_LOWAT, POLLHUP, POLLIN, POLLOUT,
    POLLRDHUP, POLLRDNORM, POLLWRNORM,
};
use event::{Filter, Knote, Note, Wake};

use crate::{Socket, Type, state};

impl Socket {
    /// `pr_sopoll`: the events of `events` that hold, `w` registered
    /// first on what it would wait for.
    pub fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if let Some(w) = w {
            if events & (POLLIN | POLLRDNORM | POLLRDHUP) != 0 {
                self.rd.record(w);
            }
            if events & (POLLOUT | POLLWRNORM) != 0 {
                self.wr.record(w);
            }
        }
        let i = self.inner.lock();
        if let Some(l) = &i.listen {
            return if events & (POLLIN | POLLRDNORM) == 0 {
                0
            } else if !l.comp.is_empty() {
                events & (POLLIN | POLLRDNORM)
            } else if i.error.is_some() {
                (events & (POLLIN | POLLRDNORM)) | POLLHUP
            } else {
                0
            };
        }
        let mut revents = 0;
        if self.ty == Type::Dgram {
            if events & (POLLIN | POLLRDNORM) != 0 && (i.rcv.cc >= i.rcv.lowat || i.error.is_some())
            {
                revents |= events & (POLLIN | POLLRDNORM);
            }
            // The send buffer holds nothing: always room, as FreeBSD's.
            if events & (POLLOUT | POLLWRNORM) != 0
                && (i.snd.hiwat >= i.snd.lowat || i.snd.cant || i.error.is_some())
            {
                revents |= events & (POLLOUT | POLLWRNORM);
            }
            if i.rcv.cant {
                revents |= events & (POLLIN | POLLRDNORM | POLLRDHUP);
                if i.snd.cant {
                    revents |= POLLHUP;
                }
            }
            return revents;
        }
        if i.state & state::ISDISCONNECTED != 0 {
            revents = POLLHUP;
        }
        if events & (POLLIN | POLLRDNORM | POLLRDHUP) != 0 {
            if i.rcv.cc >= i.rcv.lowat || i.error.is_some() {
                revents |= events & (POLLIN | POLLRDNORM);
            }
            if i.rcv.cant {
                revents |= events & (POLLIN | POLLRDNORM | POLLRDHUP);
            }
        }
        let peer = i.peer.clone();
        drop(i);
        if events & (POLLOUT | POLLWRNORM) != 0
            && let Some(p) = peer
        {
            let p = p.inner.lock();
            if p.rcv.stream_space() >= p.rcv.lowat {
                revents |= events & (POLLOUT | POLLWRNORM);
            }
            if p.rcv.cant {
                revents |= POLLHUP;
            }
        }
        revents
    }
}

/// The filter for a knote on socket `so` (`pr_kqfilter`).
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
        if let Some(l) = &i.listen {
            note.kev.data = i64::try_from(l.comp.len()).unwrap_or(0);
            if let Some(e) = i.error {
                note.kev.flags |= EV_EOF;
                note.kev.fflags = e.get().cast_unsigned();
                return true;
            }
            return !l.comp.is_empty();
        }
        let avail = i64::from(i.rcv.cc);
        note.kev.data = avail - i64::from(i.rcv.ctl);
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
            avail >= i64::from(i.rcv.lowat)
        }
    }

    /// `uipc_filt_sowrite` (stream, seqpacket) and `filt_sowrite`
    /// (datagram).
    fn write(&self, note: &mut Note) -> bool {
        let i = self.so.inner.lock();
        if i.listen.is_some() {
            return false;
        }
        if self.so.ty == Type::Dgram {
            note.kev.data = i64::from(i.snd.hiwat);
            if i.snd.cant {
                note.kev.flags |= EV_EOF;
                note.kev.fflags = i.error.map_or(0, |e| e.get().cast_unsigned());
                return true;
            }
            if i.error.is_some() {
                return true;
            }
            return if note.sfflags & NOTE_LOWAT != 0 {
                note.kev.data >= note.sdata
            } else {
                note.kev.data >= i64::from(i.snd.lowat)
            };
        }
        let Some(peer) = i.peer.clone() else {
            if i.state & state::ISDISCONNECTED != 0 {
                note.kev.flags |= EV_EOF;
                note.kev.fflags = i.error.map_or(0, |e| e.get().cast_unsigned());
                return true;
            }
            return false;
        };
        drop(i);
        let p = peer.inner.lock();
        note.kev.data = i64::from(p.rcv.stream_space());
        if p.rcv.cant {
            note.kev.flags |= EV_EOF;
            return true;
        }
        if note.sfflags & NOTE_LOWAT != 0 {
            note.kev.data >= note.sdata
        } else {
            note.kev.data >= i64::from(p.rcv.lowat)
        }
    }

    /// `uipc_filt_soempty` and `filt_soempty`.
    fn empty(&self, note: &mut Note) -> bool {
        let i = self.so.inner.lock();
        if i.listen.is_some() || self.so.ty == Type::Dgram {
            note.kev.data = 0;
            return true;
        }
        let Some(peer) = i.peer.clone() else {
            return true;
        };
        drop(i);
        note.kev.data = i64::from(peer.inner.lock().rcv.stream_space());
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
