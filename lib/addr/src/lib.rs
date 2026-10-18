// SPDX-License-Identifier: BSD-2-Clause
//! Address and size newtypes shared by the loader, the kernel, and the pure
//! libraries that sit below both.
//!
//! [`PhysAddr`] and [`VirtAddr`] never mix: there is no arithmetic between
//! them, and converting one into the other goes through an explicit mapping
//! (the DMAP, a page table walk) owned by whoever knows the mapping.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;
use core::ops::{Add, Sub};

/// The base page size on every supported architecture.
pub const PAGE_SIZE: u64 = 4096;

/// Rounds `v` down to a multiple of `align`, which must be a power of two.
#[must_use]
pub const fn align_down(v: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    v & !(align - 1)
}

/// Rounds `v` up to a multiple of `align`, which must be a power of two.
/// Returns `None` on overflow.
#[must_use]
pub const fn align_up(v: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    match v.checked_add(align - 1) {
        Some(x) => Some(x & !(align - 1)),
        None => None,
    }
}

macro_rules! address {
    ($(#[$m:meta])* $name:ident, $tag:literal) => {
        $(#[$m])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// The zero address.
            pub const ZERO: Self = Self(0);

            /// Wraps a raw address.
            #[must_use]
            pub const fn new(v: u64) -> Self {
                Self(v)
            }

            /// The raw address.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// Rounds down to `align` (a power of two).
            #[must_use]
            pub const fn align_down(self, align: u64) -> Self {
                Self(align_down(self.0, align))
            }

            /// Rounds up to `align` (a power of two), or `None` on overflow.
            #[must_use]
            pub const fn align_up(self, align: u64) -> Option<Self> {
                match align_up(self.0, align) {
                    Some(v) => Some(Self(v)),
                    None => None,
                }
            }

            /// Whether the address is a multiple of `align` (a power of two).
            #[must_use]
            pub const fn is_aligned(self, align: u64) -> bool {
                self.0 & (align - 1) == 0
            }

            /// `self + n`, or `None` on overflow.
            #[must_use]
            pub const fn checked_add(self, n: u64) -> Option<Self> {
                match self.0.checked_add(n) {
                    Some(v) => Some(Self(v)),
                    None => None,
                }
            }

            /// The distance from `base` up to `self`, or `None` if `self` is
            /// below `base`.
            #[must_use]
            pub const fn offset_from(self, base: Self) -> Option<u64> {
                self.0.checked_sub(base.0)
            }
        }

        impl Add<u64> for $name {
            type Output = Self;
            fn add(self, n: u64) -> Self {
                Self(self.0 + n)
            }
        }

        impl Sub<u64> for $name {
            type Output = Self;
            fn sub(self, n: u64) -> Self {
                Self(self.0 - n)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($tag, "({:#x})"), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:#018x}", self.0)
            }
        }

        impl fmt::LowerHex for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::LowerHex::fmt(&self.0, f)
            }
        }
    };
}

address!(
    /// A physical (bus) address.
    PhysAddr,
    "PhysAddr"
);
address!(
    /// A virtual address in some address space.
    VirtAddr,
    "VirtAddr"
);

/// A half-open range of physical addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysRange {
    /// First address in the range.
    pub start: PhysAddr,
    /// One past the last address.
    pub end: PhysAddr,
}

impl PhysRange {
    /// The range `[start, start + len)`, or `None` if it wraps.
    #[must_use]
    pub const fn from_len(start: PhysAddr, len: u64) -> Option<Self> {
        match start.checked_add(len) {
            Some(end) => Some(Self { start, end }),
            None => None,
        }
    }

    /// Length in bytes.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end.get().saturating_sub(self.start.get())
    }

    /// Whether the range is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `a` lies inside the range.
    #[must_use]
    pub const fn contains(&self, a: PhysAddr) -> bool {
        a.get() >= self.start.get() && a.get() < self.end.get()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;

    use super::*;

    #[test]
    fn alignment() {
        assert_eq!(align_down(0x1fff, PAGE_SIZE), 0x1000);
        assert_eq!(align_up(0x1001, PAGE_SIZE), Some(0x2000));
        assert_eq!(align_up(0x2000, PAGE_SIZE), Some(0x2000));
        assert_eq!(align_up(u64::MAX, PAGE_SIZE), None);
        assert!(PhysAddr::new(0x20_0000).is_aligned(0x20_0000));
        assert!(!VirtAddr::new(0x20_1000).is_aligned(0x20_0000));
    }

    #[test]
    fn ranges() {
        let r = PhysRange::from_len(PhysAddr::new(0x1000), 0x2000).unwrap();
        assert_eq!(r.len(), 0x2000);
        assert!(r.contains(PhysAddr::new(0x2fff)));
        assert!(!r.contains(PhysAddr::new(0x3000)));
        assert!(PhysRange::from_len(PhysAddr::new(u64::MAX), 2).is_none());
        assert_eq!(
            VirtAddr::new(0x5000).offset_from(VirtAddr::new(0x1000)),
            Some(0x4000)
        );
        assert_eq!(format!("{:?}", PhysAddr::new(0x10)), "PhysAddr(0x10)");
    }
}
