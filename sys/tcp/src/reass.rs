// SPDX-License-Identifier: BSD-2-Clause
//! The reassembly queue (`tcp_reass.c`): data that arrived ahead of
//! `rcv_nxt`, held in sequence order until the gap before it fills.
//!
//! Where a segment overlaps what is queued, the data queued before it is
//! kept and it replaces what it overlaps after it, as FreeBSD's prepend,
//! replace, and forward merge do. The queue is limited to the receive
//! buffer's mbuf budget and to `min(hiwat / maxseg + 1, 100)` entries.
//!
//! Behavior reference: sys/netinet/tcp_reass.c.

use alloc::vec::Vec;

use mbuf::Chain;

use crate::hdr::th;
use crate::seq::{self, Seq};
use crate::tcb::{Tcpcb, tf};
use crate::{Ctx, Sock, tcps};

/// `tcp_reass_maxqueuelen`.
const MAXQUEUELEN: u32 = 100;

/// One entry (`struct tseg_qent`).
#[derive(Debug)]
struct Ent {
    start: Seq,
    len: u32,
    /// `TH_FIN` if the data ends the stream.
    flags: u16,
    data: Chain,
}

impl Ent {
    fn end(&self) -> Seq {
        self.start.wrapping_add(self.len)
    }
}

/// `t_segq`, `t_segqlen`, and `t_segqmbuflen`.
#[derive(Debug, Default)]
pub struct Reass {
    q: Vec<Ent>,
    /// What its mbufs cost the receive buffer's budget.
    mbuflen: u32,
}

impl Reass {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    /// `t_segqlen`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.q.len()
    }

    /// `tcp_reass_flush`.
    pub fn flush(&mut self) {
        self.q.clear();
        self.mbuflen = 0;
    }

    /// The `[start, end)` ranges held, for SACK and tests.
    pub fn ranges(&self) -> impl Iterator<Item = (Seq, Seq)> + '_ {
        self.q.iter().map(|e| (e.start, e.end()))
    }
}

/// Drops `n` bytes from the front of `c`.
fn trim_front(c: &mut Chain, n: u32) {
    c.trim_front(n as usize);
}

/// Keeps the first `n` bytes of `c`.
fn trim_back(c: &mut Chain, n: u32) {
    if (c.len() as u32) > n {
        let _ = c.split_off(n as usize);
    }
}

impl Tcpcb {
    /// `tcp_reass`: queues the segment of `tlen` bytes at `seq` (flags
    /// `thflags`), or with `None` only delivers what the queue holds at
    /// `rcv_nxt`. Returns `TH_FIN` if what was delivered ends with one,
    /// and what `*tlenp` becomes: 0 if dropped, the length of the entry
    /// the data joined (or duplicated), or the segment's own.
    pub(crate) fn reass<S: Sock>(
        &mut self,
        so: &mut S,
        cx: &Ctx,
        seg: Option<(Seq, u16, u32, Chain)>,
    ) -> (u16, u32) {
        let mut out_len = 0;
        if let Some((mut sq, thflags, mut tlen, mut m)) = seg {
            if tlen == 0 && (thflags & th::FIN == 0 || !self.state.have_established()) {
                return (0, 0);
            }
            let cost = m.cost();
            if (sq != self.rcv_nxt || !self.state.have_established())
                && u64::from(so.rcv_mbcnt()) + u64::from(self.segq.mbuflen) + u64::from(cost)
                    > u64::from(so.rcv_mbmax())
            {
                cx.stats.inc(tcps::RCVREASSFULL);
                return (0, 0);
            }
            if let Some(last) = self.segq.q.last() {
                let lend = last.end();
                if thflags & th::FIN != 0 && seq::lt(sq.wrapping_add(tlen), lend) {
                    // A FIN inside what is queued is bogus.
                    return (0, 0);
                }
            }
            // Where it goes: after every entry starting at or before it.
            let pos = self
                .segq
                .q
                .iter()
                .position(|e| seq::gt(e.start, sq))
                .unwrap_or(self.segq.q.len());
            // The entry before keeps its data: trim the front.
            if pos > 0 {
                let p = &self.segq.q[pos - 1];
                if p.flags & th::FIN != 0 && seq::geq(sq, p.end()) {
                    return (0, 0);
                }
                let pend = p.end();
                if seq::leq(sq, pend) {
                    let i = pend.wrapping_sub(sq);
                    if i >= tlen {
                        cx.stats.inc(tcps::RCVDUPPACK);
                        cx.stats.add(tcps::RCVDUPBYTE, u64::from(tlen));
                        return (0, p.len);
                    }
                    if i > 0 {
                        trim_front(&mut m, i);
                        sq = sq.wrapping_add(i);
                        tlen -= i;
                    }
                }
            }
            let end = sq.wrapping_add(tlen);
            // In order with nothing queued before it: straight to the
            // socket.
            if sq == self.rcv_nxt
                && self.state.have_established()
                && (pos == 0 && self.segq.q.first().is_none_or(|f| seq::gt(f.start, end)))
            {
                self.rcv_nxt = self.rcv_nxt.wrapping_add(tlen);
                cx.stats.inc(tcps::RCVOOPACK);
                cx.stats.add(tcps::RCVOOBYTE, u64::from(tlen));
                if !so.rcv_closed() {
                    so.rcv_append(m);
                }
                self.flags |= tf::WAKESOR;
                return (thflags & th::FIN, tlen);
            }
            let mergeable = pos > 0 && self.segq.q[pos - 1].end() == sq;
            // Touching the next entry joins it too (`tcp_reass_prepend`).
            let overlaps_next = self.segq.q.get(pos).is_some_and(|n| seq::geq(end, n.start));
            if !mergeable && !overlaps_next {
                // A new entry, within the queue's limit.
                let limit = (so.rcv_hiwat() / self.t_maxseg.max(1) + 1).min(MAXQUEUELEN);
                if self.segq.q.len() as u32 >= limit {
                    cx.stats.inc(tcps::RCVREASSFULL);
                    return (0, 0);
                }
                self.rcvoopack += 1;
                cx.stats.inc(tcps::RCVOOPACK);
                cx.stats.add(tcps::RCVOOBYTE, u64::from(tlen));
                self.segq.mbuflen += cost;
                self.segq.q.insert(
                    pos,
                    Ent {
                        start: sq,
                        len: tlen,
                        flags: thflags & th::FIN,
                        data: m,
                    },
                );
                out_len = tlen;
            } else {
                // Joined to its neighbors: the data after it is replaced
                // where they overlap.
                self.segq.mbuflen += cost;
                let at = if mergeable {
                    let p = &mut self.segq.q[pos - 1];
                    p.data.append(m);
                    p.len += tlen;
                    p.flags |= thflags & th::FIN;
                    pos - 1
                } else {
                    self.segq.q.insert(
                        pos,
                        Ent {
                            start: sq,
                            len: tlen,
                            flags: thflags & th::FIN,
                            data: m,
                        },
                    );
                    pos
                };
                self.reass_merge_forward(at);
                out_len = self.segq.q[at].len;
            }
        }
        // `present`: what now starts at `rcv_nxt` goes to the socket.
        if !self.state.have_established() {
            return (0, out_len);
        }
        let mut flags = 0;
        while self.segq.q.first().is_some_and(|q| q.start == self.rcv_nxt) {
            let q = self.segq.q.remove(0);
            self.rcv_nxt = self.rcv_nxt.wrapping_add(q.len);
            flags = q.flags & th::FIN;
            self.segq.mbuflen = self.segq.mbuflen.saturating_sub(q.data.cost());
            if !so.rcv_closed() {
                so.rcv_append(q.data);
            }
            self.flags |= tf::WAKESOR;
        }
        if self.segq.q.is_empty() {
            self.segq.mbuflen = 0;
        }
        (flags, out_len)
    }

    /// `tcp_reass_merge_forward`: entry `at` swallows the entries after
    /// it that it overlaps or touches.
    fn reass_merge_forward(&mut self, at: usize) {
        while at + 1 < self.segq.q.len() {
            let end = self.segq.q[at].end();
            let next_start = self.segq.q[at + 1].start;
            if seq::lt(end, next_start) {
                break;
            }
            let mut n = self.segq.q.remove(at + 1);
            let over = end.wrapping_sub(n.start);
            let ncost = n.data.cost();
            if over >= n.len {
                // Wholly covered.
                self.segq.mbuflen = self.segq.mbuflen.saturating_sub(ncost);
                let cur = &mut self.segq.q[at];
                cur.flags |= n.flags;
                continue;
            }
            trim_front(&mut n.data, over);
            let cur = &mut self.segq.q[at];
            cur.data.append(n.data);
            cur.len += n.len - over;
            cur.flags |= n.flags;
        }
        // An entry never runs past its own FIN.
        let cur = &mut self.segq.q[at];
        if cur.flags & th::FIN != 0 {
            trim_back(&mut cur.data, cur.len);
        }
    }
}
