// SPDX-License-Identifier: BSD-2-Clause
//! A chain of mbufs (`m_next`), owned, with its byte count and what a
//! socket buffer charges for it kept as it changes.
//!
//! Behavior reference: sys/kern/uipc_mbuf.c (`m_split`, `m_copym`,
//! `m_copydata`, `m_adj`, `m_pullup`, `m_prepend`, `m_uiotombuf`),
//! sys/kern/kern_mbuf.c (`mc_get`).

use core::ptr::{self, NonNull};

use crate::mbuf::{Cluster, Mbuf, Node, free_node};
use crate::{MCLBYTES, MINCLSIZE, MJUMPAGESIZE, MLEN, NoBufs};

/// An owned chain of mbufs.
pub struct Chain {
    head: *mut Node,
    tail: *mut Node,
    len: usize,
    /// `MSIZE` and external storage of each mbuf (`sb_mbcnt`'s share).
    cost: u32,
    count: u32,
}

// SAFETY: a chain owns its nodes, which are `Send` as `Mbuf`s are.
unsafe impl Send for Chain {}
// SAFETY: `&Chain` only reads.
unsafe impl Sync for Chain {}

impl core::fmt::Debug for Chain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Chain")
            .field("len", &self.len)
            .field("mbufs", &self.count)
            .field("cost", &self.cost)
            .finish()
    }
}

impl Default for Chain {
    fn default() -> Self {
        Self::new()
    }
}

/// `mc_get`'s choice for `left` bytes still to place: the room and
/// cluster of the next mbuf.
#[must_use]
pub fn carve_next(left: usize) -> (usize, Option<Cluster>) {
    if left > MCLBYTES as usize {
        (MJUMPAGESIZE as usize, Some(Cluster::Page))
    } else if left >= MINCLSIZE as usize {
        (MCLBYTES as usize, Some(Cluster::Std))
    } else {
        (MLEN as usize, None)
    }
}

/// What `mc_get` charges for `len` bytes: `MSIZE` and the cluster of each
/// mbuf it would use.
#[must_use]
pub fn carve_cost(len: usize) -> u32 {
    let mut cost = 0u32;
    let mut left = len;
    loop {
        let (room, c) = carve_next(left);
        cost += crate::MSIZE + c.map_or(0, Cluster::size);
        left = left.saturating_sub(room);
        if left == 0 {
            return cost;
        }
    }
}

impl Chain {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            len: 0,
            cost: 0,
            count: 0,
        }
    }

    /// Bytes of data.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    /// What a socket buffer charges for it.
    #[must_use]
    pub fn cost(&self) -> u32 {
        self.cost
    }

    /// Its mbufs.
    #[must_use]
    pub fn mbufs(&self) -> u32 {
        self.count
    }

    /// Whether its last mbuf ends a record.
    #[must_use]
    pub fn eor(&self) -> bool {
        // SAFETY: a non-null tail is a live node of ours.
        !self.tail.is_null() && unsafe { (*self.tail).eor() }
    }

    /// Marks (or unmarks) its last mbuf as ending a record.
    pub fn set_eor(&mut self, on: bool) {
        if let Some(t) = NonNull::new(self.tail) {
            // SAFETY: ours; borrowed only for the call, never dropped.
            let mut m = core::mem::ManuallyDrop::new(unsafe { Mbuf::from_raw(t) });
            m.set_eor(on);
        }
    }

    /// Appends one mbuf.
    pub fn push_back(&mut self, m: Mbuf) {
        let (len, cost) = (m.len(), m.cost());
        let n = m.into_raw().as_ptr();
        if self.tail.is_null() {
            self.head = n;
        } else {
            // SAFETY: the tail is a live node of ours.
            unsafe { (*self.tail).next = n };
        }
        self.tail = n;
        self.len += len;
        self.cost += cost;
        self.count += 1;
    }

    /// Puts one mbuf first.
    pub fn push_front(&mut self, m: Mbuf) {
        let (len, cost) = (m.len(), m.cost());
        let n = m.into_raw().as_ptr();
        // SAFETY: `n` is ours, with no successor.
        unsafe { (*n).next = self.head };
        if self.tail.is_null() {
            self.tail = n;
        }
        self.head = n;
        self.len += len;
        self.cost += cost;
        self.count += 1;
    }

    /// Takes the first mbuf.
    pub fn pop_front(&mut self) -> Option<Mbuf> {
        let n = NonNull::new(self.head)?;
        // SAFETY: a live node of ours; unlinked before it is handed out.
        unsafe {
            self.head = (*n.as_ptr()).next;
            (*n.as_ptr()).next = ptr::null_mut();
            if self.head.is_null() {
                self.tail = ptr::null_mut();
            }
            let m = Mbuf::from_raw(n);
            self.len -= m.len();
            self.cost -= m.cost();
            self.count -= 1;
            Some(m)
        }
    }

    /// Appends `other` (`m_cat`, without compacting).
    pub fn append(&mut self, mut other: Self) {
        if other.is_empty() {
            return;
        }
        if self.tail.is_null() {
            self.head = other.head;
        } else {
            // SAFETY: the tail is a live node of ours.
            unsafe { (*self.tail).next = other.head };
        }
        self.tail = other.tail;
        self.len += other.len;
        self.cost += other.cost;
        self.count += other.count;
        other.head = ptr::null_mut();
        other.tail = ptr::null_mut();
    }

    /// Appends `b`, into the last mbuf's room if it has enough, else as
    /// [`Chain::from_slice`] does (`sbcompress`'s copy into the tail).
    ///
    /// # Errors
    ///
    /// `NoBufs` at a zone's limit; nothing is appended then.
    pub fn extend_from_slice(&mut self, b: &[u8]) -> Result<(), NoBufs> {
        if let Some(t) = NonNull::new(self.tail) {
            let t = t.as_ptr();
            // SAFETY: the tail is a live node of ours, not borrowed.
            if unsafe { !(*t).eor() && Node::try_append(t, b) } {
                self.len += b.len();
                return Ok(());
            }
        }
        let c = Self::from_slice(b)?;
        self.append(c);
        Ok(())
    }

    /// Its segments' data, in order.
    pub fn segments(&self) -> Segments<'_> {
        Segments {
            n: self.head,
            _c: core::marker::PhantomData,
        }
    }

    /// `b` in mbufs as `mc_uiotomc` carves them (`mc_get`'s sizes).
    ///
    /// # Errors
    ///
    /// `NoBufs` at a zone's limit.
    pub fn from_slice(b: &[u8]) -> Result<Self, NoBufs> {
        Self::fill(b.len(), |dst, at| {
            dst.copy_from_slice(&b[at..at + dst.len()]);
            Ok::<_, NoBufs>(dst.len())
        })
    }

    /// `len` bytes in mbufs as `mc_uiotomc` carves them, `f` copying each
    /// piece straight in (given the piece and its offset) and returning
    /// how much it copied. A short copy ends the chain there.
    ///
    /// # Errors
    ///
    /// What `f` returns, `NoBufs` (converted) at a zone's limit.
    pub fn fill<E: From<NoBufs>>(
        len: usize,
        mut f: impl FnMut(&mut [u8], usize) -> Result<usize, E>,
    ) -> Result<Self, E> {
        let mut out = Self::new();
        let mut at = 0;
        while at < len {
            let (room, c) = carve_next(len - at);
            let mut m = match c {
                Some(c) => Mbuf::with_cluster(c)?,
                None => Mbuf::new()?,
            };
            let want = room.min(len - at);
            let got = m.fill(want, |b| f(b, at))?;
            at += got;
            if got > 0 {
                out.push_back(m);
            }
            if got < want {
                break;
            }
        }
        Ok(out)
    }

    /// Copies `dst.len()` bytes from `off` into `dst` (`m_copydata`);
    /// returns how many there were.
    pub fn copy_out(&self, mut off: usize, dst: &mut [u8]) -> usize {
        let mut at = 0;
        for s in self.segments() {
            if at == dst.len() {
                break;
            }
            if off >= s.len() {
                off -= s.len();
                continue;
            }
            let n = (s.len() - off).min(dst.len() - at);
            dst[at..at + n].copy_from_slice(&s[off..off + n]);
            at += n;
            off = 0;
        }
        at
    }

    /// `len` bytes from `off` by reference (`m_copym`): clusters and
    /// pages shared, inline data copied.
    ///
    /// # Errors
    ///
    /// `NoBufs` at the mbuf zone's limit.
    pub fn copy(&self, mut off: usize, mut len: usize) -> Result<Self, NoBufs> {
        let mut out = Self::new();
        let mut n = self.head;
        while !n.is_null() && len > 0 {
            // SAFETY: a live node of ours, borrowed for the copy.
            let m =
                core::mem::ManuallyDrop::new(unsafe { Mbuf::from_raw(NonNull::new_unchecked(n)) });
            if off >= m.len() {
                off -= m.len();
            } else {
                let take = (m.len() - off).min(len);
                out.push_back(m.share(off, take)?);
                len -= take;
                off = 0;
            }
            // SAFETY: as above.
            n = unsafe { (*n).next };
        }
        Ok(out)
    }

    /// Splits it after `at` bytes (`m_split`), returning the rest. An mbuf
    /// cut in two keeps its cluster in both halves (both charged); a plain
    /// one's tail is copied to a new mbuf.
    ///
    /// # Errors
    ///
    /// `NoBufs` at the mbuf zone's limit; the chain is unchanged then.
    pub fn split_off(&mut self, at: usize) -> Result<Self, NoBufs> {
        if at >= self.len {
            return Ok(Self::new());
        }
        let mut head = Self::new();
        let mut have = 0;
        while let Some(m) = self.pop_front() {
            if have + m.len() <= at {
                have += m.len();
                head.push_back(m);
                continue;
            }
            let cut = at - have;
            if cut == 0 {
                self.push_front(m);
                break;
            }
            match m.share(cut, m.len() - cut) {
                Ok(mut tail) => {
                    let mut m = m;
                    m.trim_back(m.len() - cut);
                    // A record's end stays at its end.
                    tail.set_eor(m.eor());
                    m.set_eor(false);
                    self.push_front(tail);
                    head.push_back(m);
                }
                Err(e) => {
                    self.push_front(m);
                    head.append(core::mem::take(self));
                    *self = head;
                    return Err(e);
                }
            }
            break;
        }
        Ok(core::mem::replace(self, head))
    }

    /// Takes the first `n` bytes off (the rest of a cut mbuf staying, as
    /// `m_adj` leaves it).
    ///
    /// # Errors
    ///
    /// As [`Chain::split_off`].
    pub fn take_front(&mut self, n: usize) -> Result<Self, NoBufs> {
        let rest = self.split_off(n)?;
        Ok(core::mem::replace(self, rest))
    }

    /// Drops `n` bytes from the front (`m_adj` with `n > 0`); emptied
    /// mbufs are freed.
    pub fn trim_front(&mut self, mut n: usize) {
        while n > 0 {
            let Some(mut m) = self.pop_front() else {
                return;
            };
            if m.len() > n {
                m.trim_front(n);
                self.push_front(m);
                return;
            }
            n -= m.len();
        }
    }

    /// Makes the first `n` bytes contiguous in the first mbuf and returns
    /// them (`m_pullup`): moved into the first mbuf if it is writable with
    /// room, else into a new one put before the rest. `n` is at most
    /// `MLEN`.
    ///
    /// # Errors
    ///
    /// `NoBufs` if the chain is shorter than `n`, `n` exceeds `MLEN`, or
    /// at the mbuf zone's limit. The chain is unchanged then.
    pub fn pullup(&mut self, n: usize) -> Result<&[u8], NoBufs> {
        if n > self.len || n > MLEN as usize {
            return Err(NoBufs);
        }
        let mut h = self.pop_front().ok_or(NoBufs)?;
        if h.len() < n {
            let mut buf = [0u8; MLEN as usize];
            if h.len() + h.trailing_space() >= n {
                let need = n - h.len();
                self.copy_out(0, &mut buf[..need]);
                self.trim_front(need);
                h.append(&buf[..need]);
            } else {
                let mut f = match Mbuf::new() {
                    Ok(f) => f,
                    Err(e) => {
                        self.push_front(h);
                        return Err(e);
                    }
                };
                self.push_front(h);
                self.copy_out(0, &mut buf[..n]);
                self.trim_front(n);
                f.append(&buf[..n]);
                h = f;
            }
        }
        self.push_front(h);
        Ok(&self.first().unwrap_or_default()[..n])
    }

    /// Makes room for `n` bytes before the data and returns them
    /// (`M_PREPEND`): in the first mbuf's leading space if it has it, else
    /// a new mbuf with the bytes at its end.
    ///
    /// # Errors
    ///
    /// `NoBufs` at the mbuf zone's limit, or `n` over `MLEN`.
    pub fn prepend(&mut self, n: usize) -> Result<&mut [u8], NoBufs> {
        if n > MLEN as usize {
            return Err(NoBufs);
        }
        let mut h = match self.pop_front() {
            Some(h) if h.leading_space() >= n => h,
            other => {
                if let Some(h) = other {
                    self.push_front(h);
                }
                let mut m = Mbuf::new()?;
                m.reserve(MLEN as usize - n);
                m
            }
        };
        let p = h.prepend(n).ok_or(NoBufs)?.as_mut_ptr();
        self.push_front(h);
        // SAFETY: the head's first `n` bytes, writable (it had leading
        // space), valid while `self` is borrowed mutably.
        Ok(unsafe { core::slice::from_raw_parts_mut(p, n) })
    }

    /// The first mbuf's data, if any.
    #[must_use]
    pub fn first(&self) -> Option<&[u8]> {
        // SAFETY: a non-null head is a live node of ours.
        NonNull::new(self.head).map(|h| unsafe { (*h.as_ptr()).bytes() })
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        let mut n = self.head;
        while !n.is_null() {
            // SAFETY: each node is ours; its successor is read before it
            // is freed.
            unsafe {
                let next = (*n).next;
                free_node(n);
                n = next;
            }
        }
    }
}

impl From<Mbuf> for Chain {
    fn from(m: Mbuf) -> Self {
        let mut c = Self::new();
        c.push_back(m);
        c
    }
}

/// The data of each mbuf of a chain.
#[derive(Debug)]
pub struct Segments<'a> {
    n: *mut Node,
    _c: core::marker::PhantomData<&'a Chain>,
}

impl<'a> Iterator for Segments<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let n = NonNull::new(self.n)?;
        // SAFETY: a live node of the borrowed chain.
        unsafe {
            self.n = (*n.as_ptr()).next;
            Some((*n.as_ptr()).bytes())
        }
    }
}
