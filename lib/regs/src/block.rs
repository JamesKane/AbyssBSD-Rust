// SPDX-License-Identifier: BSD-2-Clause
//! Typed register blocks.
//!
//! [`register_block!`](crate::register_block) declares a device's register
//! map: each register's offset, width, and access mode. A
//! [`Mmio<B>`](crate::Mmio) handle then reads and writes registers by
//! name, with the width fixed by the type and writes to read-only
//! registers rejected at compile time. Offsets are checked against the
//! block's size and alignment at compile time, so accesses need no runtime
//! bounds check (array registers check their index).
//!
//! [`Field`] describes a bit field of a register value.
//!
//! ```
//! regs::register_block! {
//!     /// A tiny device.
//!     pub struct Dev: 0x10 {
//!         /// Control.
//!         CTRL @ 0x0 => u32, Rw;
//!         /// Status.
//!         STATUS @ 0x4 => u32, Ro;
//!         /// Data FIFO slots.
//!         DATA @ 0x8 => [u16; 4], Wo;
//!     }
//! }
//! const READY: regs::Field<u32> = regs::Field::new(0, 1);
//! # let _ = (Dev::CTRL, Dev::STATUS, Dev::DATA, READY);
//! ```

use core::marker::PhantomData;

/// A device's register map, declared with
/// [`register_block!`](crate::register_block).
pub trait RegisterBlock {
    /// Bytes the block spans.
    const SIZE: usize;
}

/// An integer register width.
pub trait Width: Copy + sealed::Sealed {
    /// Volatile read.
    ///
    /// # Safety
    ///
    /// `p` is valid and aligned for a volatile read.
    unsafe fn read(p: *const Self) -> Self;
    /// Volatile write.
    ///
    /// # Safety
    ///
    /// `p` is valid and aligned for a volatile write.
    unsafe fn write(p: *mut Self, v: Self);
}

/// Read-only registers.
#[derive(Clone, Copy, Debug)]
pub struct Ro;
/// Write-only registers.
#[derive(Clone, Copy, Debug)]
pub struct Wo;
/// Read-write registers.
#[derive(Clone, Copy, Debug)]
pub struct Rw;

/// Access modes that allow reads.
pub trait Readable: sealed::Sealed {}
/// Access modes that allow writes.
pub trait Writable: sealed::Sealed {}
impl Readable for Ro {}
impl Readable for Rw {}
impl Writable for Wo {}
impl Writable for Rw {}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Ro {}
    impl Sealed for super::Wo {}
    impl Sealed for super::Rw {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

macro_rules! widths {
    ($($t:ty),*) => {$(
        impl Width for $t {
            unsafe fn read(p: *const Self) -> Self {
                // SAFETY: per the caller's contract.
                unsafe { p.read_volatile() }
            }
            unsafe fn write(p: *mut Self, v: Self) {
                // SAFETY: per the caller's contract.
                unsafe { p.write_volatile(v) }
            }
        }
    )*};
}
widths!(u8, u16, u32, u64);

/// Ties a register to its block, width, and access mode without owning any.
type Marker<B, T, A> = PhantomData<fn() -> (B, T, A)>;

/// A register of block `B`: width `T`, access mode `A`.
pub struct Reg<B, T, A> {
    offset: usize,
    _p: Marker<B, T, A>,
}

impl<B, T, A> Reg<B, T, A> {
    /// The register at `offset` (use
    /// [`register_block!`](crate::register_block), which checks it).
    #[doc(hidden)]
    #[must_use]
    pub const fn new(offset: usize) -> Self {
        Self {
            offset,
            _p: PhantomData,
        }
    }

    /// The register's offset in the block.
    #[must_use]
    pub const fn offset(self) -> usize {
        self.offset
    }
}

impl<B, T, A> Clone for Reg<B, T, A> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<B, T, A> Copy for Reg<B, T, A> {}
impl<B, T, A> core::fmt::Debug for Reg<B, T, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Reg({:#x})", self.offset)
    }
}

/// `N` consecutive registers of block `B`, each of width `T`.
pub struct RegArray<B, T, A, const N: usize> {
    offset: usize,
    _p: Marker<B, T, A>,
}

impl<B, T, A, const N: usize> RegArray<B, T, A, N> {
    /// The array at `offset` (use
    /// [`register_block!`](crate::register_block), which checks it).
    #[doc(hidden)]
    #[must_use]
    pub const fn new(offset: usize) -> Self {
        Self {
            offset,
            _p: PhantomData,
        }
    }

    /// Element `i`.
    ///
    /// # Panics
    ///
    /// If `i >= N`: an index past the hardware's array is a driver bug.
    #[must_use]
    pub const fn at(self, i: usize) -> Reg<B, T, A> {
        assert!(i < N, "register index out of range");
        Reg::new(self.offset + i * size_of::<T>())
    }

    /// The number of elements.
    #[must_use]
    pub const fn len(self) -> usize {
        N
    }

    /// Whether the array is empty (it never is).
    #[must_use]
    pub const fn is_empty(self) -> bool {
        N == 0
    }
}

impl<B, T, A, const N: usize> Clone for RegArray<B, T, A, N> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<B, T, A, const N: usize> Copy for RegArray<B, T, A, N> {}
impl<B, T, A, const N: usize> core::fmt::Debug for RegArray<B, T, A, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "RegArray({:#x}; {N})", self.offset)
    }
}

/// Unsigned integers that hold register values and fields.
pub trait Bits:
    Copy
    + PartialEq
    + core::ops::BitAnd<Output = Self>
    + core::ops::BitOr<Output = Self>
    + core::ops::Not<Output = Self>
    + core::ops::Shl<u32, Output = Self>
    + core::ops::Shr<u32, Output = Self>
{
    /// Bits in the type.
    const BITS: u32;
    /// Zero.
    const ZERO: Self;
    /// All ones.
    const ONES: Self;

    /// Field `f` of this value, shifted down.
    #[must_use]
    fn field(self, f: Field<Self>) -> Self {
        (self >> f.shift) & f.mask()
    }

    /// Whether every bit of field `f` is set.
    #[must_use]
    fn has(self, f: Field<Self>) -> bool {
        self.field(f) == f.mask()
    }

    /// This value with field `f` set to `x` (truncated to the field).
    #[must_use]
    fn with(self, f: Field<Self>, x: Self) -> Self {
        let m = f.mask() << f.shift;
        (self & !m) | ((x << f.shift) & m)
    }
}

macro_rules! bits {
    ($($t:ty),*) => {$(
        impl Bits for $t {
            const BITS: u32 = <$t>::BITS;
            const ZERO: Self = 0;
            const ONES: Self = <$t>::MAX;
        }
    )*};
}
bits!(u8, u16, u32, u64);

/// A bit field: `width` bits starting at bit `shift`.
pub struct Field<T> {
    shift: u32,
    width: u32,
    _p: PhantomData<T>,
}

impl<T: Bits> Field<T> {
    /// Bits `shift..shift + width`.
    ///
    /// # Panics
    ///
    /// At compile time (in a `const`) if the field does not fit in `T`.
    #[must_use]
    pub const fn new(shift: u32, width: u32) -> Self {
        assert!(width > 0 && shift + width <= T::BITS, "field out of range");
        Self {
            shift,
            width,
            _p: PhantomData,
        }
    }

    /// The field's mask, shifted down.
    #[must_use]
    pub fn mask(self) -> T {
        if self.width == T::BITS {
            T::ONES
        } else {
            !(T::ONES << self.width)
        }
    }

    /// The field's mask in place.
    #[must_use]
    pub fn bits(self) -> T {
        self.mask() << self.shift
    }
}

impl<T> Clone for Field<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Field<T> {}
impl<T> core::fmt::Debug for Field<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Field({}..{})", self.shift, self.shift + self.width)
    }
}

/// Declares a register block: a unit struct implementing
/// [`RegisterBlock`], with one associated constant per register. See the
/// module documentation for the syntax.
#[macro_export]
macro_rules! register_block {
    (
        $(#[$m:meta])*
        $vis:vis struct $name:ident : $size:literal {
            $($body:tt)*
        }
    ) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug)]
        $vis struct $name;
        impl $crate::RegisterBlock for $name {
            const SIZE: usize = $size;
        }
        $crate::__register_block_regs!($vis $name; $($body)*);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __register_block_regs {
    ($vis:vis $b:ident;) => {};
    (
        $vis:vis $b:ident;
        $(#[$m:meta])* $reg:ident @ $off:expr => [$t:ty; $n:expr], $acc:ident;
        $($rest:tt)*
    ) => {
        // A register map describes the whole device, not only what one
        // driver uses.
        #[allow(dead_code)]
        impl $b {
            $(#[$m])*
            $vis const $reg: $crate::RegArray<$b, $t, $crate::$acc, { $n }> =
                $crate::RegArray::new($off);
        }
        const _: () = {
            assert!(($off) % ::core::mem::size_of::<$t>() == 0, "misaligned register");
            assert!(
                ($off) + ($n) * ::core::mem::size_of::<$t>()
                    <= <$b as $crate::RegisterBlock>::SIZE,
                "register past the end of the block"
            );
        };
        $crate::__register_block_regs!($vis $b; $($rest)*);
    };
    (
        $vis:vis $b:ident;
        $(#[$m:meta])* $reg:ident @ $off:expr => $t:ty, $acc:ident;
        $($rest:tt)*
    ) => {
        // A register map describes the whole device, not only what one
        // driver uses.
        #[allow(dead_code)]
        impl $b {
            $(#[$m])*
            $vis const $reg: $crate::Reg<$b, $t, $crate::$acc> = $crate::Reg::new($off);
        }
        const _: () = {
            assert!(($off) % ::core::mem::size_of::<$t>() == 0, "misaligned register");
            assert!(
                ($off) + ::core::mem::size_of::<$t>() <= <$b as $crate::RegisterBlock>::SIZE,
                "register past the end of the block"
            );
        };
        $crate::__register_block_regs!($vis $b; $($rest)*);
    };
}

/// Writing a read-only register does not compile:
///
/// ```compile_fail,E0277
/// regs::register_block! { pub struct D: 8 { S @ 0 => u32, Ro; } }
/// fn poke(m: regs::Mmio<D>) { m.write(D::S, 1); }
/// ```
///
/// Nor does a register past the end of its block:
///
/// ```compile_fail,E0080
/// regs::register_block! { pub struct D: 4 { S @ 4 => u32, Rw; } }
/// ```
///
/// Nor a misaligned one:
///
/// ```compile_fail,E0080
/// regs::register_block! { pub struct D: 8 { S @ 2 => u32, Rw; } }
/// ```
#[cfg(doc)]
pub mod compile_checks {}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;

    use super::*;
    use crate::Mmio;

    crate::register_block! {
        /// NVMe controller registers (NVM Express Base 2.0, §3.1).
        struct Nvme: 0x2000 {
            CAP @ 0x00 => u64, Ro;
            VS @ 0x08 => u32, Ro;
            INTMS @ 0x0c => u32, Wo;
            INTMC @ 0x10 => u32, Wo;
            CC @ 0x14 => u32, Rw;
            CSTS @ 0x1c => u32, Ro;
            AQA @ 0x24 => u32, Rw;
            ASQ @ 0x28 => u64, Rw;
            ACQ @ 0x30 => u64, Rw;
            /// Doorbells: submission tail `2y`, completion head `2y + 1`,
            /// each shifted left by CAP.DSTRD.
            DOORBELL @ 0x1000 => [u32; 1024], Wo;
        }
    }

    const CAP_MQES: Field<u64> = Field::new(0, 16);
    const CAP_DSTRD: Field<u64> = Field::new(32, 4);
    const CC_EN: Field<u32> = Field::new(0, 1);
    const CC_IOSQES: Field<u32> = Field::new(16, 4);
    const CSTS_RDY: Field<u32> = Field::new(0, 1);

    crate::register_block! {
        /// GICv3 distributor (Arm IHI 0069, §12.9).
        struct Gicd: 0x1_0000 {
            CTLR @ 0x0000 => u32, Rw;
            TYPER @ 0x0004 => u32, Ro;
            IIDR @ 0x0008 => u32, Ro;
            IGROUPR @ 0x0080 => [u32; 32], Rw;
            ISENABLER @ 0x0100 => [u32; 32], Rw;
            ICENABLER @ 0x0180 => [u32; 32], Rw;
            IPRIORITYR @ 0x0400 => [u8; 1020], Rw;
            ICFGR @ 0x0c00 => [u32; 64], Rw;
            IROUTER @ 0x6000 => [u64; 1020], Rw;
        }
    }

    /// A fake register block in ordinary memory.
    fn fake<B: RegisterBlock>(mem: &mut [u64]) -> Mmio<B> {
        assert!(mem.len() * 8 >= B::SIZE);
        // SAFETY: the buffer is ours, aligned, and covers the block; the
        // test is single-threaded.
        unsafe { Mmio::block(mem.as_mut_ptr() as usize) }
    }

    fn byte_at(mem: &[u64], off: usize) -> u8 {
        mem[off / 8].to_le_bytes()[off % 8]
    }

    #[test]
    fn nvme_map() {
        let mut mem = vec![0u64; 0x2000 / 8];
        // CAP: MQES 0x3ff, DSTRD 1.
        mem[0] = 0x3ff | (1 << 32);
        let r: Mmio<Nvme> = fake(&mut mem);
        let cap = r.read(Nvme::CAP);
        assert_eq!(cap.field(CAP_MQES), 0x3ff);
        assert_eq!(cap.field(CAP_DSTRD), 1);
        r.write(Nvme::CC, 0u32.with(CC_IOSQES, 6).with(CC_EN, 1));
        r.modify(Nvme::CC, |v| v.with(CC_EN, 0));
        assert!(!r.read(Nvme::CSTS).has(CSTS_RDY));
        // Queue 1's completion doorbell with DSTRD 1: index (2 + 1) << 1.
        let stride = usize::try_from(cap.field(CAP_DSTRD)).unwrap();
        r.write(Nvme::DOORBELL.at(3 << stride), 0xabcd);
        assert_eq!(mem[0x14 / 8] >> 32, 6 << 16, "CC: IOSQES set, EN cleared");
        assert_eq!(byte_at(&mem, 0x1000 + 24), 0xcd);
        assert_eq!(byte_at(&mem, 0x1000 + 25), 0xab);
    }

    #[test]
    fn gic_arrays() {
        let mut mem = vec![0u64; 0x1_0000 / 8];
        let d: Mmio<Gicd> = fake(&mut mem);
        d.write(Gicd::ISENABLER.at(1), 1 << 5); // interrupt 37
        d.write(Gicd::IPRIORITYR.at(37), 0xa0);
        d.write(Gicd::IROUTER.at(37), 0x0000_0001_0203);
        assert_eq!(d.read(Gicd::ISENABLER.at(1)), 1 << 5);
        assert_eq!(Gicd::IPRIORITYR.len(), 1020);
        assert_eq!(byte_at(&mem, 0x400 + 37), 0xa0);
        assert_eq!(mem[(0x6000 + 37 * 8) / 8], 0x0000_0001_0203);
        assert_eq!(byte_at(&mem, 0x104), 1 << 5);
    }

    #[test]
    #[should_panic(expected = "register index out of range")]
    fn array_index_is_checked() {
        let _ = Gicd::ICFGR.at(64);
    }

    #[test]
    fn fields() {
        let f: Field<u32> = Field::new(4, 4);
        assert_eq!(f.mask(), 0xf);
        assert_eq!(f.bits(), 0xf0);
        assert_eq!(0x1234u32.field(f), 3);
        assert_eq!(0x1234u32.with(f, 0xa), 0x12a4);
        assert_eq!(0x1234u32.with(f, 0x1a), 0x12a4, "truncated to the field");
        let whole: Field<u64> = Field::new(0, 64);
        assert_eq!(whole.mask(), u64::MAX);
        assert!(u64::MAX.has(whole));
    }
}
