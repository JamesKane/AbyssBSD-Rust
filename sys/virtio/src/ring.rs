// SPDX-License-Identifier: BSD-2-Clause
//! Split virtqueues (virtio 1.2 §2.7).
//!
//! A [`Ring`] is the descriptor table, the available ring the driver
//! fills, and the used ring the device fills, in one block laid out as
//! the legacy interface requires (the used ring on the next page), which
//! the modern interface accepts too.
//!
//! The driver keeps its own copy of everything it needs to free a chain
//! (the next links and each head's length), so a device that rewrites
//! descriptors or reports a bogus head can confuse only itself.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};

use vm::dma::{DmaCoherent, DmaError, DmaTag};

use crate::transport::VirtioDevice;

/// Descriptor flag: the chain continues at `next`.
const DESC_NEXT: u16 = 1;
/// Descriptor flag: the device writes this buffer.
const DESC_WRITE: u16 = 2;
/// Available-ring flag: the driver does not want interrupts.
const AVAIL_NO_INTERRUPT: u16 = 1;
/// Used-ring flag: the device does not want notifications.
const USED_NO_NOTIFY: u16 = 1;
/// The legacy interface's ring alignment.
const ALIGN: usize = 4096;
/// The end of a free list.
const NONE: u16 = u16::MAX;

/// The ring has fewer free descriptors than the chain needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

/// The device broke the ring's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// A used entry named a descriptor that is not an outstanding head.
    BadHead(u32),
    /// The used index moved further than buffers outstanding.
    BadIndex(u16),
}

/// A completed chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    /// The chain's head descriptor, as [`Ring::push`] returned it.
    pub head: u16,
    /// Bytes the device wrote, at most the chain's writable length.
    pub len: u32,
}

/// A split ring in DMA memory.
#[derive(Debug)]
pub struct Ring {
    mem: DmaCoherent<u8>,
    size: u16,
    avail: usize,
    used: usize,
    free_head: u16,
    free: u16,
    /// Next links, the driver's copy.
    next: Vec<u16>,
    /// Per head: descriptors in its chain (0: not outstanding).
    chain: Vec<u16>,
    /// Per head: the writable bytes in its chain.
    writable: Vec<u32>,
    avail_idx: u16,
    last_used: u16,
}

impl Ring {
    /// Bytes a ring of `size` entries takes.
    #[must_use]
    pub fn bytes(size: u16) -> usize {
        let n = usize::from(size);
        (16 * n + 6 + 2 * n).next_multiple_of(ALIGN) + 6 + 8 * n
    }

    /// A ring of `size` entries (a power of two, at most 32768), zeroed.
    pub fn new(tag: &DmaTag, size: u16) -> Result<Self, DmaError> {
        if !size.is_power_of_two() || size > 32768 {
            return Err(DmaError::Unsatisfiable);
        }
        let n = usize::from(size);
        let mem = tag.coherent::<u8>(Self::bytes(size))?;
        let avail = 16 * n;
        let mut next: Vec<u16> = (1..=size).collect();
        next[n - 1] = NONE;
        Ok(Self {
            mem,
            size,
            avail,
            used: (avail + 6 + 2 * n).next_multiple_of(ALIGN),
            free_head: 0,
            free: size,
            next,
            chain: vec![0; n],
            writable: vec![0; n],
            avail_idx: 0,
            last_used: 0,
        })
    }

    /// Entries in the ring.
    #[must_use]
    pub fn size(&self) -> u16 {
        self.size
    }

    /// Free descriptors.
    #[must_use]
    pub fn free(&self) -> u16 {
        self.free
    }

    /// The descriptor table's bus address.
    #[must_use]
    pub fn desc_addr(&self) -> u64 {
        self.mem.bus_addr()
    }

    /// The available ring's bus address.
    #[must_use]
    pub fn avail_addr(&self) -> u64 {
        self.mem.bus_addr() + self.avail as u64
    }

    /// The used ring's bus address.
    #[must_use]
    pub fn used_addr(&self) -> u64 {
        self.mem.bus_addr() + self.used as u64
    }

    fn slot(&self, idx: u16) -> usize {
        usize::from(idx & (self.size - 1))
    }

    /// Makes a chain of buffers available: `out` the device reads, then
    /// `inp` it writes, each a bus address and length. Returns the head.
    pub fn push(&mut self, out: &[(u64, u32)], inp: &[(u64, u32)]) -> Result<u16, Full> {
        let n = u16::try_from(out.len() + inp.len()).map_err(|_| Full)?;
        if n == 0 || n > self.free {
            return Err(Full);
        }
        let head = self.free_head;
        let mut d = head;
        let mut writable = 0u32;
        for (i, &(addr, len)) in out.iter().chain(inp).enumerate() {
            let write = i >= out.len();
            if write {
                writable = writable.saturating_add(len);
            }
            let last = i + 1 == usize::from(n);
            let off = 16 * usize::from(d);
            let next = self.next[usize::from(d)];
            self.mem.write_at::<u64>(off, addr);
            self.mem.write_at::<u32>(off + 8, len);
            let flags = if last { 0 } else { DESC_NEXT } | if write { DESC_WRITE } else { 0 };
            self.mem.write_at::<u16>(off + 12, flags);
            self.mem
                .write_at::<u16>(off + 14, if last { 0 } else { next });
            if last {
                self.free_head = next;
            } else {
                d = next;
            }
        }
        self.free -= n;
        self.chain[usize::from(head)] = n;
        self.writable[usize::from(head)] = writable;
        let at = self.avail + 4 + 2 * self.slot(self.avail_idx);
        self.mem.write_at::<u16>(at, head);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // The entry and its descriptors before the index that publishes
        // them.
        fence(Ordering::Release);
        self.mem.write_at::<u16>(self.avail + 2, self.avail_idx);
        Ok(head)
    }

    /// Whether the device wants to be told about new buffers.
    #[must_use]
    pub fn needs_notify(&self) -> bool {
        // The index store before the flag load (the device does the
        // reverse).
        fence(Ordering::SeqCst);
        self.mem.read_at::<u16>(self.used) & USED_NO_NOTIFY == 0
    }

    /// Whether completions are waiting.
    #[must_use]
    pub fn has_used(&self) -> bool {
        self.mem.read_at::<u16>(self.used + 2) != self.last_used
    }

    /// Takes the next completion, if any, and frees its chain.
    pub fn pop(&mut self) -> Result<Option<Used>, RingError> {
        let idx = self.mem.read_at::<u16>(self.used + 2);
        if idx == self.last_used {
            return Ok(None);
        }
        let outstanding = self.size - self.free;
        let ahead = idx.wrapping_sub(self.last_used);
        if ahead > outstanding {
            return Err(RingError::BadIndex(idx));
        }
        // The index load before the entry loads.
        fence(Ordering::Acquire);
        let at = self.used + 4 + 8 * self.slot(self.last_used);
        let id = self.mem.read_at::<u32>(at);
        let len = self.mem.read_at::<u32>(at + 4);
        self.last_used = self.last_used.wrapping_add(1);
        let head = u16::try_from(id)
            .ok()
            .filter(|&h| h < self.size && self.chain[usize::from(h)] != 0)
            .ok_or(RingError::BadHead(id))?;
        let n = core::mem::take(&mut self.chain[usize::from(head)]);
        // Return the chain to the free list, following our own links.
        let mut tail = head;
        for _ in 1..n {
            tail = self.next[usize::from(tail)];
        }
        self.next[usize::from(tail)] = self.free_head;
        self.free_head = head;
        self.free += n;
        Ok(Some(Used {
            head,
            len: len.min(self.writable[usize::from(head)]),
        }))
    }

    /// Asks the device for interrupts on completion, or not (a hint).
    pub fn set_interrupts(&self, on: bool) {
        let flags = if on { 0 } else { AVAIL_NO_INTERRUPT };
        self.mem.write_at::<u16>(self.avail, flags);
    }

    /// The ring's memory, for tests that play the device.
    #[cfg(test)]
    pub(crate) fn memory(&self) -> &DmaCoherent<u8> {
        &self.mem
    }
}

/// A ring set up on one of a device's queues.
#[derive(Debug)]
pub struct Virtqueue {
    ring: Ring,
    dev: Arc<VirtioDevice>,
    index: u16,
}

impl Virtqueue {
    pub(crate) fn new(ring: Ring, dev: Arc<VirtioDevice>, index: u16) -> Self {
        Self { ring, dev, index }
    }

    /// The queue's number on its device.
    #[must_use]
    pub fn index(&self) -> u16 {
        self.index
    }

    /// The ring.
    #[must_use]
    pub fn ring(&self) -> &Ring {
        &self.ring
    }

    /// See [`Ring::push`].
    pub fn push(&mut self, out: &[(u64, u32)], inp: &[(u64, u32)]) -> Result<u16, Full> {
        self.ring.push(out, inp)
    }

    /// Tells the device about buffers pushed, unless it said not to.
    pub fn kick(&self) {
        if self.ring.needs_notify() {
            self.dev.notify(self.index);
        }
    }

    /// See [`Ring::has_used`].
    #[must_use]
    pub fn has_used(&self) -> bool {
        self.ring.has_used()
    }

    /// See [`Ring::pop`].
    pub fn pop(&mut self) -> Result<Option<Used>, RingError> {
        self.ring.pop()
    }

    /// See [`Ring::set_interrupts`].
    pub fn set_interrupts(&self, on: bool) {
        self.ring.set_interrupts(on);
    }
}
