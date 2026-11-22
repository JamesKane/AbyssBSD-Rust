// SPDX-License-Identifier: BSD-2-Clause
//! A timing wheel (FreeBSD's callout wheel, `kern_timeout.c`): deadlines
//! hashed by their tick into a fixed ring of slots, each a list, so
//! setting or cancelling one is a link or an unlink, whatever the number
//! waiting. A deadline more than a turn of the ring away waits in its slot
//! for the turns between, as a callout's `c_time` past the wheel does.
//!
//! The lists' nodes live in one slab and are linked by index; a freed node
//! is reused, so a steady load allocates nothing. The caller keeps the
//! handle an insert returns, to remove that entry (a socket keeps its
//! timer's, as a callout is part of its `tcpcb`).

use alloc::vec::Vec;

/// A tick: 2^20 ns, about a millisecond (TCP's timers count in ms).
const SHIFT: u32 = 20;
/// Slots on the ring (a power of two): about 4.4 s a turn.
const SLOTS: usize = 4096;
const WORDS: usize = SLOTS / 64;
const NIL: u32 = u32::MAX;

/// No entry (a handle no insert returns).
pub(crate) const NONE: u32 = NIL;

const fn tick(at: u64) -> u64 {
    at >> SHIFT
}

#[allow(clippy::cast_possible_truncation)]
const fn slot(t: u64) -> usize {
    (t as usize) & (SLOTS - 1)
}

#[derive(Debug)]
struct Node<T> {
    prev: u32,
    /// The next in the slot's list, or in the free list.
    next: u32,
    /// The deadline (ns).
    at: u64,
    /// The slot it is listed in.
    slot: u32,
    item: Option<T>,
}

/// The wheel: entries of `T`, each due at a time in ns.
#[derive(Debug)]
pub(crate) struct Wheel<T> {
    /// Each slot's first node (empty until the first insert).
    heads: Vec<u32>,
    /// Slots with entries, a bit each.
    busy: [u64; WORDS],
    nodes: Vec<Node<T>>,
    free: u32,
    len: usize,
    /// Every entry due before this tick has been taken.
    cursor: u64,
}

impl<T> Wheel<T> {
    pub(crate) const fn new() -> Self {
        Self {
            heads: Vec::new(),
            busy: [0; WORDS],
            nodes: Vec::new(),
            free: NIL,
            len: 0,
            cursor: 0,
        }
    }

    /// Entries waiting.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Adds `item`, due at `at` (ns); returns its handle.
    pub(crate) fn insert(&mut self, at: u64, item: T) -> u32 {
        if self.heads.is_empty() {
            self.heads = alloc::vec![NIL; SLOTS];
        }
        // One already due goes where the next run looks first.
        let s = slot(tick(at).max(self.cursor));
        let head = self.heads[s];
        let node = Node {
            prev: NIL,
            next: head,
            at,
            slot: u32::try_from(s).unwrap_or(0),
            item: Some(item),
        };
        let h = if self.free == NIL {
            self.nodes.push(node);
            u32::try_from(self.nodes.len() - 1).unwrap_or(NIL)
        } else {
            let h = self.free;
            let n = &mut self.nodes[h as usize];
            self.free = n.next;
            *n = node;
            h
        };
        if head != NIL {
            self.nodes[head as usize].prev = h;
        }
        self.heads[s] = h;
        self.busy[s / 64] |= 1 << (s % 64);
        self.len += 1;
        h
    }

    /// Takes the entry `h` out, if it is one.
    pub(crate) fn remove(&mut self, h: u32) -> Option<T> {
        let n = self.nodes.get_mut(h as usize)?;
        let item = n.item.take()?;
        let (prev, next, s) = (n.prev, n.next, n.slot as usize);
        n.next = self.free;
        self.free = h;
        if prev == NIL {
            self.heads[s] = next;
            if next == NIL {
                self.busy[s / 64] &= !(1 << (s % 64));
            }
        } else {
            self.nodes[prev as usize].next = next;
        }
        if next != NIL {
            self.nodes[next as usize].prev = prev;
        }
        self.len -= 1;
        Some(item)
    }

    /// The first entry in slot `s` due by `now`, if any.
    fn due_in(&self, s: usize, now: u64) -> Option<u32> {
        let mut h = self.heads[s];
        while h != NIL {
            let n = &self.nodes[h as usize];
            if n.at <= now {
                return Some(h);
            }
            h = n.next;
        }
        None
    }

    /// Takes one entry due by `now` (ns): its deadline and item.
    pub(crate) fn pop_due(&mut self, now: u64) -> Option<(u64, T)> {
        if self.len == 0 {
            self.cursor = self.cursor.max(tick(now));
            return None;
        }
        // `now` never goes back, but an entry put at the cursor is looked at.
        let end = tick(now).max(self.cursor);
        if end.saturating_sub(self.cursor) >= SLOTS as u64 {
            // A turn or more since the last run: every slot, once.
            for s in 0..SLOTS {
                if let Some(h) = self.due_in(s, now) {
                    let at = self.nodes[h as usize].at;
                    return self.remove(h).map(|x| (at, x));
                }
            }
            self.cursor = end;
            return None;
        }
        while self.cursor <= end {
            let s = slot(self.cursor);
            if self.busy[s / 64] & (1 << (s % 64)) != 0
                && let Some(h) = self.due_in(s, now)
            {
                let at = self.nodes[h as usize].at;
                return self.remove(h).map(|x| (at, x));
            }
            if self.cursor == end {
                break;
            }
            // Left here: later turns' entries only.
            self.cursor += 1;
        }
        None
    }

    /// When the next entry may be due (ns): the earliest deadline among
    /// the next busy slot's entries for this turn; the end of the turn if
    /// all wait for later ones; `u64::MAX` if none waits.
    pub(crate) fn next(&self) -> u64 {
        if self.len == 0 {
            return u64::MAX;
        }
        let mut t = self.cursor;
        let last = self.cursor + SLOTS as u64;
        while t < last {
            let s = slot(t);
            let word = self.busy[s / 64] >> (s % 64);
            if word == 0 {
                // The rest of this word is empty.
                t += 64 - (s % 64) as u64;
                continue;
            }
            let skip = u64::from(word.trailing_zeros());
            if skip > 0 {
                t += skip;
                continue;
            }
            let mut best = u64::MAX;
            let mut h = self.heads[s];
            while h != NIL {
                let n = &self.nodes[h as usize];
                if tick(n.at) <= t {
                    best = best.min(n.at);
                }
                h = n.next;
            }
            if best != u64::MAX {
                return best;
            }
            t += 1;
        }
        last << SHIFT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1 << SHIFT;

    #[test]
    fn in_order_of_deadline() {
        let mut w = Wheel::new();
        let base = 1_000_000 * MS;
        w.pop_due(base);
        let a = w.insert(base + 5 * MS, 'a');
        w.insert(base + 2 * MS, 'b');
        w.insert(base + 9 * MS + 3, 'c');
        assert_eq!(w.next(), base + 2 * MS);
        assert_eq!(w.pop_due(base + MS), None);
        assert_eq!(w.pop_due(base + 2 * MS), Some((base + 2 * MS, 'b')));
        assert_eq!(w.remove(a), Some('a'));
        assert_eq!(w.remove(a), None);
        assert_eq!(w.next(), base + 9 * MS + 3);
        assert_eq!(w.pop_due(base + 9 * MS + 2), None);
        assert_eq!(w.pop_due(base + 9 * MS + 3), Some((base + 9 * MS + 3, 'c')));
        assert_eq!(w.len(), 0);
        assert_eq!(w.next(), u64::MAX);
    }

    #[test]
    fn past_and_far_deadlines() {
        let mut w = Wheel::new();
        let base = 50_000 * MS;
        w.pop_due(base);
        // Already due: found at once.
        w.insert(base - 7 * MS, 1);
        assert_eq!(w.next(), base - 7 * MS);
        assert_eq!(w.pop_due(base), Some((base - 7 * MS, 1)));
        // Three turns away: waits out the turns between.
        let far = base + 3 * SLOTS as u64 * MS + 17;
        w.insert(far, 2);
        assert!(w.next() <= far);
        let mut now = base;
        while now < far - MS {
            assert_eq!(w.pop_due(now), None, "at {now}");
            now = w.next().min(far - MS);
        }
        assert_eq!(w.pop_due(far), Some((far, 2)));
    }

    #[test]
    fn long_gap_then_many() {
        let mut w = Wheel::new();
        w.pop_due(0);
        for i in 0..1000u64 {
            w.insert(i * 997, i);
        }
        // Long after: everything comes out, once each.
        let mut got: Vec<u64> =
            core::iter::from_fn(|| w.pop_due(u64::MAX / 4).map(|x| x.1)).collect();
        got.sort_unstable();
        assert_eq!(got, (0..1000).collect::<Vec<_>>());
        // Nodes are reused.
        let n = w.nodes.len();
        for i in 0..1000u64 {
            w.insert(u64::MAX / 4 + i, i);
        }
        assert_eq!(w.nodes.len(), n);
    }
}
