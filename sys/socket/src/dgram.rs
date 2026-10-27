// SPDX-License-Identifier: BSD-2-Clause
//! A datagram socket's receive queues: one for unconnected senders, and
//! one per connected sender, which that sender's own send buffer limits
//! (FreeBSD's `uxdg_mb`, `uxdg_conns`, and the sender's `so_snd`).
//!
//! Behavior reference: sys/kern/uipc_usrreq.c (`uipc_sosend_dgram`,
//! `uipc_soreceive_dgram`, `unp_disconnect`).

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::buf::{self, MSIZE, Sockbuf};
use crate::control::{Control, InFlight};

/// One datagram: who sent it, its control messages, and its data, with
/// what it is counted as.
#[derive(Debug)]
pub(crate) struct Dgram {
    pub(crate) from: Vec<u8>,
    pub(crate) control: Vec<Control>,
    pub(crate) data: Vec<u8>,
    /// Data plus control bytes, control bytes, memory.
    cc: u32,
    ctl: u32,
    mbcnt: u32,
}

impl Dgram {
    pub(crate) fn new(from: Vec<u8>, control: Vec<Control>, data: &[u8]) -> Self {
        let ctl: u32 =
            u32::try_from(from.len()).unwrap_or(0) + control.iter().map(Control::len).sum::<u32>();
        // The name's mbuf, the control's, and the data's (as
        // `m_uiotombuf` carves it).
        let n = u32::try_from(data.len()).unwrap_or(u32::MAX);
        let mbcnt = MSIZE
            + control.iter().map(Control::cost).sum::<u32>()
            + buf::carve(n)
                .iter()
                .map(|&(_, ext)| MSIZE + ext)
                .sum::<u32>();
        Self {
            from,
            control,
            data: data.to_vec(),
            cc: n + ctl,
            ctl,
            mbcnt,
        }
    }
}

/// A queue and its counts.
#[derive(Debug, Default)]
struct Queue {
    q: VecDeque<Dgram>,
    cc: u32,
    ctl: u32,
    mbcnt: u32,
}

impl Queue {
    fn push(&mut self, d: Dgram) {
        self.cc += d.cc;
        self.ctl += d.ctl;
        self.mbcnt += d.mbcnt;
        self.q.push_back(d);
    }

    fn pop(&mut self) -> Option<Dgram> {
        let d = self.q.pop_front()?;
        self.cc -= d.cc;
        self.ctl -= d.ctl;
        self.mbcnt -= d.mbcnt;
        Some(d)
    }
}

/// `uipc_dgram_sbspace`.
fn fits(hiwat: u32, mbmax: u32, q: &Queue, d: &Dgram) -> bool {
    hiwat >= q.cc && mbmax >= q.mbcnt && hiwat - q.cc >= d.cc && mbmax - q.mbcnt >= d.mbcnt
}

/// The receive side's queues.
#[derive(Debug, Default)]
pub(crate) struct Dgrams {
    /// Unconnected senders'.
    main: Queue,
    /// Connected senders', by sender, each with its send buffer's limits;
    /// only those with datagrams waiting, oldest first.
    conns: VecDeque<(usize, (u32, u32), Queue)>,
    /// A datagram a peek took out, given first next.
    peeked: Option<Dgram>,
}

/// A datagram [`Dgrams::next`] found: peeked (left in place) or taken.
pub(crate) enum Next<'a> {
    Peeked(&'a Dgram),
    Taken(Dgram),
}

impl core::ops::Deref for Next<'_> {
    type Target = Dgram;
    fn deref(&self) -> &Dgram {
        match self {
            Self::Peeked(d) => d,
            Self::Taken(d) => d,
        }
    }
}

impl Next<'_> {
    /// The datagram, taken.
    pub(crate) fn take(self) -> Option<Dgram> {
        match self {
            Self::Peeked(_) => None,
            Self::Taken(d) => Some(d),
        }
    }
}

impl Dgrams {
    /// Queues `d`: from connected sender `conn` (with its send buffer's
    /// limits) on its queue, else on the main one, limited by `rcv`.
    /// Returns it if there is no room (`ENOBUFS`).
    pub(crate) fn push(
        &mut self,
        d: Dgram,
        conn: Option<(usize, (u32, u32))>,
        rcv: &mut Sockbuf,
    ) -> Result<(), Dgram> {
        if rcv.cant {
            return Err(d);
        }
        let (cc, ctl, mbcnt) = (d.cc, d.ctl, d.mbcnt);
        match conn {
            Some((id, (hiwat, mbmax))) => {
                if let Some((_, _, q)) = self.conns.iter_mut().find(|c| c.0 == id) {
                    if !fits(hiwat, mbmax, q, &d) {
                        return Err(d);
                    }
                    q.push(d);
                } else {
                    let mut q = Queue::default();
                    if !fits(hiwat, mbmax, &q, &d) {
                        return Err(d);
                    }
                    q.push(d);
                    // `TAILQ_INSERT_HEAD`.
                    self.conns.push_front((id, (hiwat, mbmax), q));
                }
            }
            None => {
                if !fits(rcv.hiwat, rcv.mbmax, &self.main, &d) {
                    return Err(d);
                }
                self.main.push(d);
            }
        }
        rcv.cc += cc;
        rcv.ctl += ctl;
        rcv.mbcnt += mbcnt;
        Ok(())
    }

    /// The next datagram: the peeked one, a connected sender's, then the
    /// main queue's. With `peek` it stays queued (moved aside, as
    /// `uxdg_peeked`).
    pub(crate) fn next(&mut self, peek: bool, rcv: &mut Sockbuf) -> Option<Next<'_>> {
        let d = match self.peeked.take() {
            Some(d) => d,
            None => {
                if let Some((_, _, q)) = self.conns.front_mut() {
                    let d = q.pop()?;
                    if q.q.is_empty() {
                        self.conns.pop_front();
                    }
                    d
                } else {
                    self.main.pop()?
                }
            }
        };
        if peek {
            return Some(Next::Peeked(self.peeked.insert(d)));
        }
        rcv.cc -= d.cc;
        rcv.ctl -= d.ctl;
        rcv.mbcnt -= d.mbcnt;
        Some(Next::Taken(d))
    }

    /// Sender `id` disconnected: its queue moves to the main one if that
    /// is empty and the socket still receives, else it is dropped (and
    /// returned, to be freed unlocked).
    pub(crate) fn sender_gone(&mut self, id: usize, rcv: &mut Sockbuf) -> Vec<Dgram> {
        let Some(at) = self.conns.iter().position(|c| c.0 == id) else {
            return Vec::new();
        };
        let Some((_, _, mut q)) = self.conns.remove(at) else {
            return Vec::new();
        };
        if !rcv.cant && self.main.q.is_empty() {
            while let Some(d) = q.pop() {
                self.main.push(d);
            }
            return Vec::new();
        }
        rcv.cc -= q.cc;
        rcv.ctl -= q.ctl;
        rcv.mbcnt -= q.mbcnt;
        q.q.into_iter().collect()
    }

    /// Everything queued, removed (`unp_dispose`).
    pub(crate) fn take_all(&mut self) -> Vec<Dgram> {
        let mut out: Vec<Dgram> = self.peeked.take().into_iter().collect();
        for (_, _, q) in core::mem::take(&mut self.conns) {
            out.extend(q.q);
        }
        out.extend(core::mem::take(&mut self.main).q);
        out
    }

    /// Calls `f` on every descriptor queued.
    pub(crate) fn scan(&self, f: &mut dyn FnMut(&InFlight)) {
        let all = self
            .peeked
            .iter()
            .chain(self.conns.iter().flat_map(|c| c.2.q.iter()))
            .chain(self.main.q.iter());
        for d in all {
            for c in &d.control {
                crate::control::rights_of(c).iter().for_each(&mut *f);
            }
        }
    }
}
