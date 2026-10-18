// SPDX-License-Identifier: BSD-2-Clause
//! Kernel memory management: the physical page allocator, the kernel heap,
//! and the kernel virtual address arena.
//!
//! - [`phys`]: a buddy allocator over per-domain free lists, with page
//!   metadata in a [`phys::VmPage`] array (`vm_phys`, `vm_page`).
//! - [`slab`]: size-class slab allocation backed by whole pages from a
//!   [`slab::PageSource`]; the kernel's `GlobalAlloc` (`uma`, `malloc(9)`).
//! - [`vmem`]: a boundary-tag arena for kernel virtual address ranges
//!   (`vmem(9)`), which kernel stacks and later `kmem` mappings come from.
//!
//! Everything here is independent of the architecture and of how physical
//! memory is reached, so it runs in host tests; the kernel crate supplies
//! the page tables and the direct map.
//!
//! Behavior reference: sys/vm/vm_phys.c, sys/vm/uma_core.c,
//! sys/kern/subr_vmem.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod phys;
pub mod slab;
pub mod vmem;

use core::fmt;

pub use addr::{PAGE_SIZE, PhysAddr, VirtAddr};

/// A physical page frame number: the physical address divided by
/// [`PAGE_SIZE`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pfn(pub u64);

impl Pfn {
    /// The frame containing `pa`.
    #[must_use]
    pub const fn containing(pa: PhysAddr) -> Self {
        Self(pa.get() / PAGE_SIZE)
    }

    /// The frame's first byte.
    #[must_use]
    pub const fn addr(self) -> PhysAddr {
        PhysAddr::new(self.0 * PAGE_SIZE)
    }

    /// The frame `n` frames on.
    #[must_use]
    pub const fn add(self, n: u64) -> Self {
        Self(self.0 + n)
    }
}

impl fmt::Debug for Pfn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pfn({:#x})", self.0)
    }
}

/// A buddy order: a block of `2^order` contiguous, naturally aligned
/// pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Order(u8);

impl Order {
    /// The largest order: 16 MiB blocks with 4 KiB pages.
    pub const MAX: Self = Self(12);
    /// One page.
    pub const ZERO: Self = Self(0);

    /// `n`, if it is a valid order.
    #[must_use]
    pub const fn new(n: u8) -> Option<Self> {
        if n <= Self::MAX.0 {
            Some(Self(n))
        } else {
            None
        }
    }

    /// The smallest order holding `bytes`, if there is one.
    #[must_use]
    pub fn for_bytes(bytes: u64) -> Option<Self> {
        let pages = bytes.div_ceil(PAGE_SIZE).max(1);
        let n = u8::try_from(pages.next_power_of_two().trailing_zeros()).ok()?;
        Self::new(n)
    }

    /// The order as a number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Pages in a block of this order.
    #[must_use]
    pub const fn pages(self) -> u64 {
        1 << self.0
    }

    /// Bytes in a block of this order.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        PAGE_SIZE << self.0
    }

    /// The next order up, for merging.
    const fn higher(self) -> Option<Self> {
        Self::new(self.0 + 1)
    }

    const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Number of buddy orders.
pub const ORDERS: usize = Order::MAX.index() + 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders() {
        assert_eq!(Order::for_bytes(1), Some(Order::ZERO));
        assert_eq!(Order::for_bytes(4096), Some(Order::ZERO));
        assert_eq!(Order::for_bytes(4097).map(Order::get), Some(1));
        assert_eq!(Order::for_bytes(3 * 4096).map(Order::get), Some(2));
        assert_eq!(Order::for_bytes(16 << 20), Some(Order::MAX));
        assert_eq!(Order::for_bytes((16 << 20) + 1), None);
        assert_eq!(Order::MAX.bytes(), 16 << 20);
    }
}
