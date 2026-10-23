// SPDX-License-Identifier: BSD-2-Clause
//! Rings (xHCI 1.2 §4.9): the command and transfer rings software
//! produces into, and the event ring the controller produces into.
//!
//! Ownership of a slot is its cycle bit: a producer ring's slot is the
//! controller's once its cycle bit matches the producer's cycle state, an
//! event ring's slot is software's once its cycle bit matches the
//! consumer's. [`ProducerRing::push`] writes a TD's first TRB's control
//! word last, after a release fence, so the controller never runs into a
//! half-written chain; [`EventRing::pop`] reads an event's other words
//! only after an acquire fence on its control word.
//!
//! Rings are one segment closed by a link TRB. The memory is behind
//! [`TrbMemory`], so the kernel lays them over DMA memory and host tests
//! over a vector.

use alloc::collections::VecDeque;
use core::sync::atomic::{Ordering, fence};

use crate::trb::{self, Trb, flag};

/// Size of a TRB in bytes.
pub const TRB_BYTES: u64 = 16;

/// Memory holding a ring's TRBs: physically contiguous, 64-byte aligned,
/// and not crossing a 64 KiB boundary (xHCI 1.2 §6, Table 6-1).
pub trait TrbMemory {
    /// The number of TRB slots.
    fn slots(&self) -> usize;
    /// The bus address of slot 0.
    fn bus_addr(&self) -> u64;
    /// Reads slot `i`. `i < slots()`.
    fn read(&self, i: usize) -> Trb;
    /// Writes the first three words of slot `i` (not the control word).
    fn write_body(&self, i: usize, body: [u32; 3]);
    /// Writes the control word of slot `i`.
    fn write_control(&self, i: usize, control: u32);
}

/// The ring cannot take the chain now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    /// Not enough free slots; retry after completions.
    Full,
    /// The chain is empty, or longer than the ring could ever hold.
    BadLength,
}

/// Where a pushed chain sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pushed {
    /// The bus address of its first TRB.
    pub first: u64,
    /// The bus address of its last TRB.
    pub last: u64,
}

/// A completion matched to a pushed chain.
#[derive(Debug, PartialEq, Eq)]
pub struct Matched<T> {
    /// The chain's tag.
    pub tag: T,
    /// Which of the chain's TRBs the event named (0-based, link TRBs not
    /// counted).
    pub trb: usize,
    /// The chain's TRB count.
    pub len: usize,
}

#[derive(Debug)]
struct Chain<T> {
    /// Index of its first slot.
    start: usize,
    /// Its TRBs (the link TRB it may cross is not one of them).
    len: usize,
    /// `None` once completed, while chains ahead of it are outstanding.
    tag: Option<T>,
}

/// A command or transfer ring: software produces, the controller
/// consumes. `T` identifies what a pushed chain is for.
#[derive(Debug)]
pub struct ProducerRing<M, T> {
    mem: M,
    /// Usable slots: all but the closing link TRB.
    usable: usize,
    enqueue: usize,
    cycle: bool,
    /// Slots held by outstanding chains.
    used: usize,
    chains: VecDeque<Chain<T>>,
}

impl<M: TrbMemory, T> ProducerRing<M, T> {
    /// A ring over `mem`, cleared, with its closing link TRB. The
    /// producer cycle state starts at 1, as the controller's consumer
    /// state does once the ring's dequeue pointer is programmed with
    /// [`ProducerRing::dequeue`].
    ///
    /// # Panics
    ///
    /// If `mem` has fewer than two slots.
    pub fn new(mem: M) -> Self {
        let n = mem.slots();
        assert!(n >= 2, "a ring needs a slot and its link");
        for i in 0..n {
            mem.write_body(i, [0; 3]);
            mem.write_control(i, 0);
        }
        let l = trb::link(mem.bus_addr(), true);
        mem.write_body(n - 1, [l.0[0], l.0[1], l.0[2]]);
        // The link's cycle bit stays clear (the controller's) until the
        // producer reaches it.
        mem.write_control(n - 1, l.0[3]);
        Self {
            mem,
            usable: n - 1,
            enqueue: 0,
            cycle: true,
            used: 0,
            chains: VecDeque::new(),
        }
    }

    /// The memory.
    pub fn memory(&self) -> &M {
        &self.mem
    }

    /// Free slots.
    #[must_use]
    pub fn free(&self) -> usize {
        self.usable - self.used
    }

    /// The dequeue pointer and cycle state to give the controller for an
    /// empty ring: where the next chain will go (`CRCR`, an endpoint
    /// context's TR dequeue pointer, or Set TR Dequeue Pointer).
    #[must_use]
    pub fn dequeue(&self) -> (u64, bool) {
        (self.addr(self.enqueue), self.cycle)
    }

    fn addr(&self, i: usize) -> u64 {
        // `i < slots()`, and a ring is far smaller than 2^64 bytes.
        self.mem.bus_addr() + u64::try_from(i).unwrap_or(0) * TRB_BYTES
    }

    /// Writes `trbs` as one chain tagged `tag`. Callers set the chain
    /// bit on all but the last, and IOC where they want events; the ring
    /// sets the cycle bits, and the chain bit on a link TRB the chain
    /// crosses.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] if the free slots cannot hold the chain,
    /// [`PushError::BadLength`] if it is empty or could never fit.
    pub fn push(&mut self, trbs: &[Trb], tag: T) -> Result<Pushed, PushError> {
        if trbs.is_empty() || trbs.len() > self.usable {
            return Err(PushError::BadLength);
        }
        if trbs.len() > self.free() {
            return Err(PushError::Full);
        }
        let start = self.enqueue;
        let first_cycle = self.cycle;
        let mut last = 0;
        for (k, t) in trbs.iter().enumerate() {
            let i = self.enqueue;
            last = i;
            self.mem.write_body(i, [t.0[0], t.0[1], t.0[2]]);
            if k > 0 {
                self.mem.write_control(i, with_cycle(t.0[3], self.cycle));
            }
            self.enqueue += 1;
            if self.enqueue == self.usable {
                self.cross_link(k + 1 < trbs.len());
            }
        }
        fence(Ordering::Release);
        self.mem
            .write_control(start, with_cycle(trbs[0].0[3], first_cycle));
        self.used += trbs.len();
        self.chains.push_back(Chain {
            start,
            len: trbs.len(),
            tag: Some(tag),
        });
        Ok(Pushed {
            first: self.addr(start),
            last: self.addr(last),
        })
    }

    /// Hands the link TRB to the controller (chained if the chain goes on
    /// past it) and wraps.
    fn cross_link(&mut self, chained: bool) {
        let link = self.usable;
        let mut c = self.mem.read(link).0[3] & !(flag::CHAIN | flag::CYCLE);
        if chained {
            c |= flag::CHAIN;
        }
        self.mem.write_control(link, with_cycle(c, self.cycle));
        self.enqueue = 0;
        self.cycle = !self.cycle;
    }

    /// The chain a TRB address belongs to, and the TRB's place in it.
    fn locate(&self, addr: u64) -> Option<(usize, usize)> {
        let base = self.mem.bus_addr();
        let off = addr.checked_sub(base)?;
        if !off.is_multiple_of(TRB_BYTES) {
            return None;
        }
        let i = usize::try_from(off / TRB_BYTES).ok()?;
        if i >= self.usable {
            return None;
        }
        self.chains.iter().enumerate().find_map(|(c, ch)| {
            // Offset of slot `i` from the chain's start, around the ring.
            let rel = (i + self.usable - ch.start) % self.usable;
            (rel < ch.len).then_some((c, rel))
        })
    }

    /// The outstanding chain holding the TRB at `addr`, without
    /// completing it: its tag, the TRB's place in it, and its length. For
    /// an event that does not end a TD (a control transfer's short data
    /// stage, before its status stage completes).
    pub fn find_mut(&mut self, addr: u64) -> Option<(&mut T, usize, usize)> {
        let (c, trb) = self.locate(addr)?;
        let ch = &mut self.chains[c];
        let len = ch.len;
        ch.tag.as_mut().map(|t| (t, trb, len))
    }

    /// Mutable access to every outstanding chain's tag, oldest first.
    pub fn outstanding_mut(&mut self) -> impl Iterator<Item = &mut T> + '_ {
        self.chains.iter_mut().filter_map(|c| c.tag.as_mut())
    }

    /// Completes the chain holding the TRB at `addr`, named by an event,
    /// and frees the slots of every completed chain at the front. `None`
    /// if no outstanding chain holds it (a second event for a chain
    /// already completed, or a bogus pointer).
    pub fn complete(&mut self, addr: u64) -> Option<Matched<T>> {
        let (c, trb) = self.locate(addr)?;
        let ch = &mut self.chains[c];
        let tag = ch.tag.take()?;
        let len = ch.len;
        while self.chains.front().is_some_and(|f| f.tag.is_none()) {
            if let Some(f) = self.chains.pop_front() {
                self.used -= f.len;
            }
        }
        Some(Matched { tag, trb, len })
    }

    /// Forgets every outstanding chain, for after the controller stopped
    /// the ring and was given a new dequeue pointer ([`ProducerRing::dequeue`]),
    /// or after a reset. Returns their tags, oldest first.
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.used = 0;
        self.chains.drain(..).filter_map(|c| c.tag)
    }

    /// Whether any chain is outstanding.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.chains.is_empty()
    }
}

fn with_cycle(control: u32, cycle: bool) -> u32 {
    if cycle {
        control | flag::CYCLE
    } else {
        control & !flag::CYCLE
    }
}

/// An event ring segment table entry (xHCI 1.2 §6.5): one segment of
/// `slots` TRBs at `base`.
#[must_use]
pub fn erst_entry(base: u64, slots: u16) -> [u32; 4] {
    let b = base.to_le_bytes();
    [
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        u32::from(slots),
        0,
    ]
}

/// The event ring: the controller produces, software consumes.
#[derive(Debug)]
pub struct EventRing<M> {
    mem: M,
    dequeue: usize,
    cycle: bool,
}

impl<M: TrbMemory> EventRing<M> {
    /// A ring over `mem`, cleared. The consumer cycle state starts at 1.
    pub fn new(mem: M) -> Self {
        for i in 0..mem.slots() {
            mem.write_body(i, [0; 3]);
            mem.write_control(i, 0);
        }
        Self {
            mem,
            dequeue: 0,
            cycle: true,
        }
    }

    /// The memory.
    pub fn memory(&self) -> &M {
        &self.mem
    }

    /// The next event, if the controller has written one.
    pub fn pop(&mut self) -> Option<Trb> {
        let control = self.mem.read(self.dequeue).0[3];
        if (control & flag::CYCLE != 0) != self.cycle {
            return None;
        }
        fence(Ordering::Acquire);
        let t = self.mem.read(self.dequeue);
        self.dequeue += 1;
        if self.dequeue == self.mem.slots() {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Some(t)
    }

    /// The dequeue pointer to write to `ERDP`: the next slot to read.
    #[must_use]
    pub fn erdp(&self) -> u64 {
        self.mem.bus_addr() + u64::try_from(self.dequeue).unwrap_or(0) * TRB_BYTES
    }
}
