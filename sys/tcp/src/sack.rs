// SPDX-License-Identifier: BSD-2-Clause
//! Selective acknowledgement (`tcp_sack.c`): the blocks a receiver
//! reports (with D-SACK), and the sender's scoreboard of holes, with
//! RFC 6675's loss accounting and lost-retransmission detection.
//!
//! Behavior reference: sys/netinet/tcp_sack.c, sys/netinet/tcp_input.c
//! (`tcp_record_dsack`).

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::cc::Signal;
use crate::hdr::{MAX_SACK, MAX_SACK_BLKS, Opts, SackBlk, tof};
use crate::seq::{self, Seq};
use crate::tcb::{Tcpcb, tf};
use crate::timer::Tt;
use crate::{Ctx, Fate, Sock, get, tcps};

/// `tcprexmtthresh`: duplicate acknowledgements before fast retransmit.
pub const REXMTTHRESH: u32 = 3;
/// `MAX_TCPOPTLEN`.
const MAX_TCPOPTLEN: u32 = 40;

/// A hole (`struct sackhole`): `[start, end)` not acknowledged, with
/// `rxmit` the next byte of it to retransmit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hole {
    pub start: Seq,
    pub end: Seq,
    pub rxmit: Seq,
}

/// The scoreboard (`snd_holes`), in sequence order.
#[derive(Clone, Debug, Default)]
pub struct Holes(pub Vec<Hole>);

impl Holes {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// `struct sackhint`; `nexthole` is an index into the scoreboard.
#[derive(Clone, Copy, Debug, Default)]
pub struct SackHint {
    pub nexthole: Option<usize>,
    pub sack_bytes_rexmit: i32,
    pub last_sack_ack: Seq,
    pub delivered_data: i32,
    pub sacked_bytes: i32,
    pub recover_fs: u32,
    pub prr_delivered: u32,
    pub prr_out: u32,
    pub hole_bytes: i32,
    pub lost_bytes: i32,
}

/// What an acknowledgement did to the scoreboard (`sackstatus_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SackStatus {
    NoChange,
    Change,
    NewLoss,
}

impl Tcpcb {
    /// `tcp_dsack_block_exists`.
    #[must_use]
    pub fn dsack_block_exists(&self) -> bool {
        self.rcv_numsacks > 0 && seq::leq(self.sackblks[0].end, self.rcv_nxt)
    }

    /// `tcp_update_dsack_list`: `[rcv_start, rcv_end)` arrived again.
    pub fn update_dsack_list(&mut self, rcv_start: Seq, rcv_end: Seq) {
        let mut saved = [SackBlk::default(); MAX_SACK_BLKS];
        if seq::lt(rcv_end, self.rcv_nxt)
            || (rcv_end == self.rcv_nxt
                && self.rcv_numsacks > 0
                && self.sackblks[0].end == self.rcv_nxt)
        {
            saved[0] = SackBlk {
                start: rcv_start,
                end: rcv_end,
            };
        }
        let mut head = SackBlk::default();
        let mut mid = SackBlk {
            start: rcv_start,
            end: rcv_end,
        };
        let mut identical = false;
        for i in 0..self.rcv_numsacks {
            let SackBlk { start, end } = self.sackblks[i];
            if seq::lt(rcv_end, start) || seq::gt(rcv_start, end) {
                continue;
            }
            if seq::gt(self.rcv_nxt, end) {
                if seq::max(rcv_start, start) != seq::min(rcv_end, end)
                    && (seq::gt(head.start, seq::max(rcv_start, start)) || head.start == head.end)
                {
                    head = SackBlk {
                        start: seq::max(rcv_start, start),
                        end: seq::min(rcv_end, end),
                    };
                }
                continue;
            }
            if (head.start == head.end || seq::lt(start, head.start))
                && seq::gt(end, rcv_start)
                && seq::leq(start, rcv_end)
            {
                head = SackBlk { start, end };
            }
            mid.start = seq::min(mid.start, start);
            mid.end = seq::max(mid.end, end);
            if mid.start == start && mid.end == end {
                identical = true;
            }
        }
        if seq::lt(head.start, head.end) {
            saved[0] = SackBlk {
                start: seq::max(rcv_start, head.start),
                end: seq::min(rcv_end, head.end),
            };
        }
        let mut n = 1;
        if (seq::lt(self.rcv_nxt, mid.end) && mid != saved[0]) || identical {
            saved[n] = mid;
            n += 1;
        }
        for j in 0..self.rcv_numsacks {
            if n >= MAX_SACK_BLKS {
                break;
            }
            let b = self.sackblks[j];
            if (seq::lt(b.end, mid.start) || seq::gt(b.start, mid.end))
                && seq::gt(b.start, self.rcv_nxt)
            {
                saved[n] = b;
                n += 1;
            }
        }
        let mut j = 0;
        for b in &saved[..n] {
            if seq::lt(b.start, b.end) {
                self.sackblks[j] = *b;
                j += 1;
            }
        }
        self.rcv_numsacks = j;
    }

    /// `tcp_update_sack_list`: `[rcv_start, rcv_end)` arrived out of
    /// order; it heads the list, merged with what it touches.
    pub fn update_sack_list(&mut self, rcv_start: Seq, rcv_end: Seq) {
        let mut head =
            if rcv_start == rcv_end && self.rcv_numsacks >= 1 && rcv_end == self.sackblks[0].end {
                self.sackblks[0]
            } else {
                SackBlk {
                    start: rcv_start,
                    end: rcv_end,
                }
            };
        let mut saved = [SackBlk::default(); MAX_SACK_BLKS];
        let mut num_saved = 0;
        let save = |saved: &mut [SackBlk; MAX_SACK_BLKS], n: &mut usize, b: SackBlk| {
            if *n >= 1
                && seq::geq(saved[*n - 1].start, b.start)
                && seq::leq(saved[*n - 1].end, b.end)
            {
                *n -= 1;
            }
            if *n < MAX_SACK_BLKS {
                saved[*n] = b;
                *n += 1;
            }
        };
        for i in 0..self.rcv_numsacks {
            let SackBlk { mut start, mut end } = self.sackblks[i];
            if seq::geq(start, end) || seq::leq(start, self.rcv_nxt) {
                // Empty, or now below the left edge: dropped.
            } else if seq::leq(head.start, end) && seq::geq(head.end, start) {
                if head.start == end {
                    head.start = start;
                } else if head.end == start {
                    head.end = end;
                } else {
                    if seq::lt(head.start, start) {
                        core::mem::swap(&mut head.start, &mut start);
                    }
                    if seq::gt(head.end, end) {
                        core::mem::swap(&mut head.end, &mut end);
                    }
                    if head.start != start || head.end != end {
                        save(&mut saved, &mut num_saved, SackBlk { start, end });
                    }
                }
            } else {
                save(&mut saved, &mut num_saved, SackBlk { start, end });
            }
        }
        let mut num_head = 0;
        if seq::lt(rcv_start, rcv_end) {
            self.sackblks[0] = head;
            num_head = 1;
            if num_saved >= MAX_SACK_BLKS {
                num_saved -= 1;
            }
        }
        if rcv_start == rcv_end && rcv_start == self.sackblks[0].end {
            num_head = 1;
        }
        self.sackblks[num_head..num_head + num_saved].copy_from_slice(&saved[..num_saved]);
        self.rcv_numsacks = num_head + num_saved;
    }

    /// `tcp_clean_dsack_blocks`: drops blocks at or below `rcv_nxt`.
    pub fn clean_dsack_blocks(&mut self) {
        let mut saved = [SackBlk::default(); MAX_SACK_BLKS];
        let mut n = 0;
        for b in &self.sackblks[..self.rcv_numsacks] {
            if seq::geq(b.start, b.end) || seq::leq(b.start, self.rcv_nxt) {
                continue;
            }
            saved[n] = *b;
            n += 1;
        }
        self.sackblks[..n].copy_from_slice(&saved[..n]);
        self.rcv_numsacks = n;
    }

    /// `tcp_clean_sackreport`.
    pub fn clean_sackreport(&mut self) {
        self.rcv_numsacks = 0;
        self.sackblks = [SackBlk::default(); MAX_SACK_BLKS];
    }

    /// The blocks a segment carries, at most `MAX_SACK`.
    pub(crate) fn sack_opts(&self, to: &mut Opts) {
        let n = self.rcv_numsacks.min(MAX_SACK);
        to.nsacks = n as u8;
        to.sacks[..n].copy_from_slice(&self.sackblks[..n]);
        to.flags |= tof::SACK;
    }

    /// `tcp_record_dsack`.
    pub(crate) fn record_dsack(&mut self, cx: &Ctx, start: Seq, end: Seq) {
        cx.stats.inc(tcps::DSACK_COUNT);
        self.dsack_pack += 1;
        let n = if seq::gt(end, start) {
            end.wrapping_sub(start)
        } else {
            start.wrapping_sub(end)
        };
        self.dsack_bytes += n;
        cx.stats.add(tcps::DSACK_BYTES, u64::from(n));
    }

    /// `tcp_sackhole_insert`: a hole after index `after` (or at the end);
    /// `None` past the per-connection or system limit.
    fn hole_insert(
        &mut self,
        cx: &Ctx,
        start: Seq,
        end: Seq,
        after: Option<usize>,
    ) -> Option<usize> {
        if self.snd_holes.len() >= get(&cx.conf.sack_maxholes) as usize
            || cx.stats.sack_globalholes.load(Ordering::Relaxed)
                >= get(&cx.conf.sack_globalmaxholes)
        {
            cx.stats.inc(tcps::SACK_SBOVERFLOW);
            return None;
        }
        // Relaxed: a count.
        cx.stats.sack_globalholes.fetch_add(1, Ordering::Relaxed);
        let at = after.map_or(self.snd_holes.len(), |i| i + 1);
        self.snd_holes.0.insert(
            at,
            Hole {
                start,
                end,
                rxmit: start,
            },
        );
        match self.sackhint.nexthole {
            None => self.sackhint.nexthole = Some(at),
            Some(n) if n >= at => self.sackhint.nexthole = Some(n + 1),
            Some(_) => {}
        }
        Some(at)
    }

    /// `tcp_sackhole_remove`.
    fn hole_remove(&mut self, globalholes: &core::sync::atomic::AtomicI32, i: usize) {
        self.snd_holes.0.remove(i);
        // Relaxed: a count.
        globalholes.fetch_sub(1, Ordering::Relaxed);
        self.sackhint.nexthole = match self.sackhint.nexthole {
            Some(n) if n > i => Some(n - 1),
            // The removed one's successor now has its index.
            Some(n) if n == i && i < self.snd_holes.len() => Some(i),
            Some(n) if n == i => None,
            other => other,
        };
    }

    /// `tcp_sack_doack`: folds the SACK blocks of an acknowledgement of
    /// `th_ack` into the scoreboard.
    pub(crate) fn sack_doack(&mut self, cx: &Ctx, to: &Opts, th_ack: Seq) -> SackStatus {
        let mut blocks = [SackBlk::default(); MAX_SACK + 1];
        let mut nblk = 0;
        let mut changed = SackStatus::NoChange;
        let mut delivered: i32 = 0;
        let mut left_edge_delta: i32 = 0;
        let maxseg = self.t_maxseg.saturating_sub(MAX_TCPOPTLEN);
        let mut loss_hiack: Seq;
        let mut loss_thresh: u32 = 0;
        let mut loss_sblks: u32 = 0;
        let mut notlost: i32 = 0;
        let thresh_bytes = (REXMTTHRESH - 1) * self.t_maxseg;

        if seq::lt(self.snd_una, th_ack) && !self.snd_holes.is_empty() {
            left_edge_delta = th_ack.wrapping_sub(self.snd_una) as i32;
            delivered += left_edge_delta;
            blocks[nblk] = SackBlk {
                start: self.snd_una,
                end: th_ack,
            };
            nblk += 1;
            if seq::lt(self.snd_fack, th_ack) {
                self.snd_fack = th_ack;
                changed = SackStatus::Change;
            }
        }
        if to.has(tof::SACK) {
            for s in to.sacks() {
                if seq::gt(s.end, s.start)
                    && seq::gt(s.start, self.snd_una)
                    && seq::gt(s.start, th_ack)
                    && seq::lt(s.start, self.snd_max)
                    && seq::gt(s.end, self.snd_una)
                    && seq::leq(s.end, self.snd_max)
                    && (s.end.wrapping_sub(s.start) >= maxseg || seq::geq(s.end, self.snd_max))
                {
                    blocks[nblk] = *s;
                    nblk += 1;
                } else if seq::leq(s.start, th_ack) && seq::leq(s.end, th_ack) {
                    self.record_dsack(cx, s.start, s.end);
                }
            }
        }
        if nblk == 0 {
            return changed;
        }
        // Sort by the end, ascending.
        blocks[..nblk].sort_unstable_by(|a, b| {
            if seq::gt(a.end, b.end) {
                core::cmp::Ordering::Greater
            } else if a.end == b.end {
                core::cmp::Ordering::Equal
            } else {
                core::cmp::Ordering::Less
            }
        });
        if self.snd_holes.is_empty() {
            self.snd_fack = seq::max(self.snd_una, th_ack);
        }
        // `sblkp` as an index that may go below zero.
        let mut sb: isize = nblk as isize - 1;
        let last = blocks[nblk - 1];
        self.sackhint.last_sack_ack = last.end;
        if seq::lt(self.snd_fack, last.start) {
            let tail = self.snd_holes.len().checked_sub(1);
            if let Some(t) = tail.filter(|&t| seq::leq(self.snd_fack, self.snd_holes.0[t].end)) {
                // The last hole grows to the new block.
                let h = &mut self.snd_holes.0[t];
                self.sackhint.hole_bytes -= h.end.wrapping_sub(h.start) as i32;
                h.start = seq::max(self.snd_fack, seq::max(self.snd_una, th_ack));
                h.end = last.start;
                h.rxmit = h.start;
                delivered += last.end.wrapping_sub(last.start) as i32;
                self.sackhint.hole_bytes += h.end.wrapping_sub(h.start) as i32;
                self.snd_fack = last.end;
                sb -= 1;
                changed = SackStatus::NewLoss;
            } else if let Some(i) = self.hole_insert(cx, self.snd_fack, last.start, None) {
                let h = self.snd_holes.0[i];
                delivered += last.end.wrapping_sub(last.start) as i32;
                self.sackhint.hole_bytes += h.end.wrapping_sub(h.start) as i32;
                self.snd_fack = last.end;
                sb -= 1;
                changed = SackStatus::Change;
            } else {
                // No hole could be had: skip the blocks it would need.
                while sb >= 0 && seq::lt(self.snd_fack, blocks[sb as usize].start) {
                    sb -= 1;
                }
                if sb >= 0 && seq::lt(self.snd_fack, blocks[sb as usize].end) {
                    delivered += blocks[sb as usize].end.wrapping_sub(self.snd_fack) as i32;
                    self.snd_fack = blocks[sb as usize].end;
                    changed = SackStatus::NewLoss;
                }
            }
        } else if seq::lt(self.snd_fack, last.end) {
            delivered += last.end.wrapping_sub(self.snd_fack) as i32;
            self.snd_fack = last.end;
            changed = SackStatus::Change;
        }

        // Walk the holes from the last, and the blocks from the last.
        let mut cur: isize = self.snd_holes.len() as isize - 1;
        loss_hiack = self.snd_fack;
        let lost = |sblks: u32, thresh: u32| sblks >= REXMTTHRESH || thresh > thresh_bytes;
        while cur >= 0 {
            let ci = cur as usize;
            if sb < 0 {
                if lost(loss_sblks, loss_thresh) {
                    break;
                }
                let h = self.snd_holes.0[ci];
                loss_thresh += loss_hiack.wrapping_sub(h.end);
                loss_hiack = h.start;
                loss_sblks += 1;
                if lost(loss_sblks, loss_thresh) {
                    break;
                }
                notlost += h.end.wrapping_sub(h.start) as i32;
                cur -= 1;
                continue;
            }
            let b = blocks[sb as usize];
            let h = self.snd_holes.0[ci];
            if seq::geq(b.start, h.end) {
                // The block is past this hole.
                sb -= 1;
                continue;
            }
            if seq::leq(b.end, h.start) {
                // The block is before this hole.
                loss_thresh += loss_hiack.wrapping_sub(h.end);
                loss_hiack = h.start;
                loss_sblks += 1;
                if !lost(loss_sblks, loss_thresh) {
                    notlost += h.end.wrapping_sub(h.start) as i32;
                }
                cur -= 1;
                continue;
            }
            self.sackhint.sack_bytes_rexmit -=
                seq::min(h.rxmit, h.end).wrapping_sub(h.start) as i32;
            changed = changed.max(SackStatus::Change);
            if seq::leq(b.start, h.start) {
                if seq::geq(b.end, h.end) {
                    // The whole hole is covered: it goes.
                    delivered += h.end.wrapping_sub(h.start) as i32;
                    self.sackhint.hole_bytes -= h.end.wrapping_sub(h.start) as i32;
                    self.hole_remove(&cx.stats.sack_globalholes, ci);
                    cur -= 1;
                    continue;
                }
                // Its front is covered.
                let hm = &mut self.snd_holes.0[ci];
                delivered += b.end.wrapping_sub(hm.start) as i32;
                self.sackhint.hole_bytes -= b.end.wrapping_sub(hm.start) as i32;
                hm.start = b.end;
                hm.rxmit = seq::max(hm.rxmit, hm.start);
            } else if seq::geq(b.end, h.end) {
                // Its back is covered.
                let lrd = self.flags & tf::LRD != 0;
                let recover = self.snd_recover;
                let hm = &mut self.snd_holes.0[ci];
                delivered += hm.end.wrapping_sub(b.start) as i32;
                self.sackhint.hole_bytes -= hm.end.wrapping_sub(b.start) as i32;
                hm.end = b.start;
                hm.rxmit = seq::min(hm.rxmit, hm.end);
                if lrd && seq::geq(hm.rxmit, hm.end) {
                    hm.rxmit = seq::max(hm.rxmit, recover);
                }
            } else {
                // The block splits the hole in two.
                changed = SackStatus::NewLoss;
                if let Some(ti) = self.hole_insert(cx, b.end, h.end, Some(ci)) {
                    let lrd = self.flags & tf::LRD != 0;
                    let recover = self.snd_recover;
                    let crx = self.snd_holes.0[ci].rxmit;
                    let t = &mut self.snd_holes.0[ti];
                    if seq::gt(crx, t.rxmit) {
                        t.rxmit = crx;
                        self.sackhint.sack_bytes_rexmit +=
                            seq::min(t.rxmit, t.end).wrapping_sub(t.start) as i32;
                    }
                    let t = *t;
                    self.sackhint.hole_bytes -= b.end.wrapping_sub(b.start) as i32;
                    loss_thresh += loss_hiack.wrapping_sub(t.end);
                    loss_hiack = t.start;
                    loss_sblks += 1;
                    if !lost(loss_sblks, loss_thresh) {
                        notlost += t.end.wrapping_sub(t.start) as i32;
                    }
                    let hm = &mut self.snd_holes.0[ci];
                    hm.end = b.start;
                    hm.rxmit = seq::min(hm.rxmit, hm.end);
                    if lrd && seq::geq(hm.rxmit, hm.end) {
                        hm.rxmit = seq::max(hm.rxmit, recover);
                    }
                    delivered += b.end.wrapping_sub(b.start) as i32;
                }
            }
            let h = self.snd_holes.0[ci];
            self.sackhint.sack_bytes_rexmit +=
                seq::min(h.rxmit, h.end).wrapping_sub(h.start) as i32;
            if seq::leq(b.start, h.start) {
                loss_thresh += loss_hiack.wrapping_sub(h.end);
                loss_hiack = h.start;
                loss_sblks += 1;
                if !lost(loss_sblks, loss_thresh) {
                    notlost += h.end.wrapping_sub(h.start) as i32;
                }
                cur -= 1;
            } else {
                sb -= 1;
            }
        }
        if self.snd_holes.is_empty() {
            self.sackhint.sack_bytes_rexmit = 0;
            self.sackhint.sacked_bytes = 0;
            self.sackhint.lost_bytes = 0;
        } else {
            self.sackhint.delivered_data = delivered;
            self.sackhint.sacked_bytes += delivered - left_edge_delta;
            self.sackhint.lost_bytes = self.sackhint.hole_bytes - notlost;
        }
        if !to.has(tof::SACK) {
            changed = SackStatus::NoChange;
        }
        changed
    }

    /// `tcp_free_sackholes`.
    pub fn free_sackholes(&mut self, stats: &crate::Stats) {
        let n = self.snd_holes.len() as i32;
        if n > 0 {
            // Relaxed: a count.
            stats.sack_globalholes.fetch_sub(n, Ordering::Relaxed);
        }
        self.snd_holes.0.clear();
        self.sackhint.nexthole = None;
        self.sackhint.sack_bytes_rexmit = 0;
        self.sackhint.delivered_data = 0;
        self.sackhint.sacked_bytes = 0;
        self.sackhint.hole_bytes = 0;
        self.sackhint.lost_bytes = 0;
    }

    /// `tcp_resend_sackholes`: every hole is to be sent again.
    pub(crate) fn resend_sackholes(&mut self) {
        for h in &mut self.snd_holes.0 {
            h.rxmit = h.start;
        }
        self.sackhint.nexthole = (!self.snd_holes.is_empty()).then_some(0);
        self.sackhint.sack_bytes_rexmit = 0;
    }

    /// `tcp_sack_partialack`: an acknowledgement inside fast recovery
    /// that does not reach `snd_recover`.
    pub(crate) fn sack_partialack<S: Sock>(
        &mut self,
        so: &mut S,
        cx: &mut Ctx,
        th_ack: Seq,
        maxseg: u32,
    ) -> Fate {
        let maxseg = if maxseg == 0 { self.maxseg() } else { maxseg };
        self.timer_activate(Tt::Rexmt, 0, cx.now);
        self.rtttime = 0;
        let num_segs = if th_ack.wrapping_sub(self.snd_una) / maxseg.max(1) >= 2 {
            2
        } else {
            1
        };
        if self.snd_nxt == self.snd_max {
            self.snd_cwnd = (self.sackhint.sack_bytes_rexmit.max(0) as u32)
                .wrapping_add(self.snd_nxt.wrapping_sub(self.snd_recover))
                .wrapping_add(num_segs * maxseg);
        }
        if self.snd_cwnd > self.snd_ssthresh {
            self.snd_cwnd = self.snd_ssthresh;
        }
        self.flags |= tf::ACKNOW;
        if seq::lt(th_ack, self.snd_recover)
            && self.snd_holes.is_empty()
            && self.sackhint.delivered_data > 0
        {
            // RFC 6675's rescue retransmission: the last segment below
            // the recovery point.
            let mut highdata = self.snd_max;
            if self.flags & tf::SENTFIN != 0 {
                highdata = highdata.wrapping_sub(1);
            }
            highdata = seq::min(highdata, self.snd_recover);
            if seq::lt(th_ack, highdata) {
                self.snd_fack = seq::max(th_ack, self.snd_fack);
                let start = seq::max(th_ack, highdata.wrapping_sub(maxseg));
                if let Some(i) = self.hole_insert(cx, start, highdata, None) {
                    let h = self.snd_holes.0[i];
                    self.sackhint.hole_bytes += h.end.wrapping_sub(h.start) as i32;
                }
            }
        }
        self.output(so, cx)
    }

    /// `tcp_sack_output`: the hole to retransmit from, if any.
    pub(crate) fn sack_output(&mut self) -> Option<usize> {
        let mut i = self.sackhint.nexthole?;
        let h = *self.snd_holes.0.get(i)?;
        if seq::geq(h.rxmit, h.end) {
            loop {
                i += 1;
                let h = self.snd_holes.0.get(i)?;
                if seq::lt(h.rxmit, h.end) {
                    self.sackhint.nexthole = Some(i);
                    break;
                }
            }
        }
        Some(i)
    }

    /// `tcp_sack_adjust`: moves `snd_nxt` past what was SACKed; returns
    /// the bytes SACKed below it.
    pub(crate) fn sack_adjust(&mut self) -> i32 {
        let Some(&first) = self.snd_holes.0.first() else {
            return 0;
        };
        if seq::geq(self.snd_nxt, self.snd_fack) {
            return self.sackhint.sacked_bytes;
        }
        let mut cur = first;
        let mut sacked = 0i32;
        for p in self.snd_holes.0.iter().skip(1) {
            if seq::lt(self.snd_nxt, cur.end) {
                return sacked;
            }
            sacked += p.start.wrapping_sub(cur.end) as i32;
            if seq::geq(self.snd_nxt, p.start) {
                cur = *p;
            } else {
                self.snd_nxt = p.start;
                return sacked;
            }
        }
        if seq::lt(self.snd_nxt, cur.end) {
            return sacked;
        }
        self.snd_nxt = self.snd_fack;
        self.sackhint.sacked_bytes
    }

    /// `tcp_sack_lost_retransmission`: a retransmission was lost too
    /// (SACKs reached past it): recovery starts over.
    pub(crate) fn sack_lost_retransmission(&mut self, cx: &Ctx, th_ack: Seq) {
        let Some(&first) = self.snd_holes.0.first() else {
            return;
        };
        if self.in_recovery()
            && seq::gt(self.snd_fack, self.snd_recover)
            && seq::geq(first.rxmit, first.end)
            && seq::geq(self.snd_fack, first.rxmit)
        {
            cx.stats.inc(tcps::SACK_LOSTREXMT);
            self.sackhint.nexthole = Some(0);
            let fack = self.snd_fack;
            for h in &mut self.snd_holes.0 {
                if seq::geq(fack, h.rxmit) && seq::geq(h.rxmit, h.end) {
                    h.rxmit = h.start;
                }
            }
            self.snd_cwnd = self.snd_ssthresh;
            self.flags &= !(tf::FASTRECOVERY | tf::CONGRECOVERY);
            self.cc_cong_signal(Some(th_ack), Signal::NDupAck, cx.stats);
            self.snd_cwnd = self.maxseg();
            self.sackhint.recover_fs = self
                .snd_max
                .wrapping_sub(self.snd_una)
                .wrapping_sub(self.sackhint.recover_fs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cc::Algo;
    use crate::tcb::Inc;
    use crate::{Conf, Stats};

    fn tcb() -> Tcpcb {
        let mut t = Tcpcb::new(Inc::default(), Algo::NewReno, &Conf::new(), 0);
        t.t_maxseg = 1000;
        t
    }

    #[test]
    fn out_of_order_blocks_merge_most_recent_first() {
        let mut t = tcb();
        t.rcv_nxt = 1000;
        t.update_sack_list(3000, 4000);
        t.update_sack_list(5000, 6000);
        assert_eq!(
            t.sack_blocks(),
            [
                SackBlk {
                    start: 5000,
                    end: 6000
                },
                SackBlk {
                    start: 3000,
                    end: 4000
                }
            ]
        );
        // The gap between them filled: one block.
        t.update_sack_list(4000, 5000);
        assert_eq!(
            t.sack_blocks(),
            [SackBlk {
                start: 3000,
                end: 6000
            }]
        );
        // In-order data reached it: none left.
        t.rcv_nxt = 6000;
        t.clean_dsack_blocks();
        assert!(t.sack_blocks().is_empty());
    }

    #[test]
    fn a_duplicate_is_reported_first() {
        let mut t = tcb();
        t.rcv_nxt = 5000;
        t.update_dsack_list(1000, 2000);
        assert!(t.dsack_block_exists());
        assert_eq!(
            t.sackblks[0],
            SackBlk {
                start: 1000,
                end: 2000
            }
        );
    }

    #[test]
    fn the_scoreboard_tracks_holes() {
        let conf = Conf::new();
        let stats = Stats::new();
        let mut out = Vec::new();
        let cx = Ctx {
            now: 0,
            conf: &conf,
            stats: &stats,
            random: || 4,
            out: &mut out,
            hc: &crate::hostcache::HostCache::new(),
            path: &|_| crate::tcb::Path::default(),
            secrets: &crate::isn::Secrets::default(),
        };
        let mut t = tcb();
        t.snd_una = 0;
        t.snd_max = 10_000;
        t.snd_fack = 0;
        let mut to = Opts {
            flags: tof::SACK,
            nsacks: 1,
            ..Opts::default()
        };
        // Segments 2 and 3 arrived; 0..2000 is a hole.
        to.sacks[0] = SackBlk {
            start: 2000,
            end: 4000,
        };
        assert_eq!(t.sack_doack(&cx, &to, 0), SackStatus::Change);
        assert_eq!(
            t.snd_holes.0,
            [Hole {
                start: 0,
                end: 2000,
                rxmit: 0
            }]
        );
        assert_eq!(t.snd_fack, 4000);
        assert_eq!(t.sackhint.sacked_bytes, 2000);
        // 6000..7000 too: a second hole, 4000..6000.
        to.sacks[0] = SackBlk {
            start: 6000,
            end: 7000,
        };
        t.sack_doack(&cx, &to, 0);
        assert_eq!(t.snd_holes.len(), 2);
        assert_eq!(t.snd_holes.0[1].start, 4000);
        // The cumulative ACK reaches 2000: the first hole closes.
        to.flags = 0;
        t.sack_doack(&cx, &to, 2000);
        t.snd_una = 2000;
        assert_eq!(t.snd_holes.len(), 1);
        assert_eq!(t.sack_output(), Some(0));
        t.free_sackholes(&stats);
        assert_eq!(stats.sack_globalholes.load(Ordering::Relaxed), 0);
    }
}
