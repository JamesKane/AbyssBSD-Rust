// SPDX-License-Identifier: BSD-2-Clause
//! Device register access.
//!
//! [`Mmio`] is a register block at a fixed virtual address, read and written
//! with volatile accesses of an explicit width. [`RegisterIo`] abstracts a
//! byte-register device over MMIO and x86 port I/O so one driver serves
//! both (a 16550 can sit at an I/O port or in MMIO with a register stride).
//!
//! This crate is one of the few allowed to touch raw device memory
//! (docs/ENGINEERING.md §4).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

/// A block of memory-mapped device registers.
#[derive(Clone, Copy, Debug)]
pub struct Mmio {
    base: usize,
    len: usize,
}

impl Mmio {
    /// Wraps `len` bytes of registers at virtual address `base`.
    ///
    /// # Safety
    ///
    /// `[base, base + len)` must be mapped as device memory for as long as
    /// the value (and its copies) are used, and accesses through it must be
    /// acceptable to the device.
    pub const unsafe fn new(base: usize, len: usize) -> Self {
        Self { base, len }
    }

    fn ptr<T>(&self, offset: usize) -> *mut T {
        assert!(
            offset.is_multiple_of(size_of::<T>()) && offset + size_of::<T>() <= self.len,
            "register offset out of range"
        );
        (self.base + offset) as *mut T
    }

    /// Reads the 8-bit register at `offset`.
    pub fn read8(&self, offset: usize) -> u8 {
        // SAFETY: in bounds and aligned (checked by `ptr`); mapped per the
        // constructor's contract.
        unsafe { self.ptr::<u8>(offset).read_volatile() }
    }

    /// Writes the 8-bit register at `offset`.
    pub fn write8(&self, offset: usize, v: u8) {
        // SAFETY: as for `read8`.
        unsafe { self.ptr::<u8>(offset).write_volatile(v) }
    }

    /// Reads the 16-bit register at `offset`.
    pub fn read16(&self, offset: usize) -> u16 {
        // SAFETY: as for `read8`.
        unsafe { self.ptr::<u16>(offset).read_volatile() }
    }

    /// Writes the 16-bit register at `offset`.
    pub fn write16(&self, offset: usize, v: u16) {
        // SAFETY: as for `read8`.
        unsafe { self.ptr::<u16>(offset).write_volatile(v) }
    }

    /// Reads the 32-bit register at `offset`.
    pub fn read32(&self, offset: usize) -> u32 {
        // SAFETY: as for `read8`.
        unsafe { self.ptr::<u32>(offset).read_volatile() }
    }

    /// Writes the 32-bit register at `offset`.
    pub fn write32(&self, offset: usize, v: u32) {
        // SAFETY: as for `read8`.
        unsafe { self.ptr::<u32>(offset).write_volatile(v) }
    }
}

/// Byte-wide registers numbered from 0, wherever they live.
pub trait RegisterIo {
    /// Reads register `n`.
    fn read(&self, n: usize) -> u8;
    /// Writes register `n`.
    fn write(&self, n: usize, v: u8);
}

/// Byte registers in MMIO, register `n` at `n << shift`, accessed with
/// `width` bytes (1 or 4; wider registers use their low byte).
#[derive(Clone, Copy, Debug)]
pub struct StridedMmio {
    regs: Mmio,
    shift: u8,
    width: AccessWidth,
}

/// Access width for [`StridedMmio`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessWidth {
    /// 8-bit accesses.
    Byte,
    /// 32-bit accesses.
    Dword,
}

impl StridedMmio {
    /// Byte registers in `regs` at stride `1 << shift`.
    pub const fn new(regs: Mmio, shift: u8, width: AccessWidth) -> Self {
        Self { regs, shift, width }
    }
}

impl RegisterIo for StridedMmio {
    fn read(&self, n: usize) -> u8 {
        let off = n << self.shift;
        match self.width {
            AccessWidth::Byte => self.regs.read8(off),
            AccessWidth::Dword => self.regs.read32(off).to_le_bytes()[0],
        }
    }

    fn write(&self, n: usize, v: u8) {
        let off = n << self.shift;
        match self.width {
            AccessWidth::Byte => self.regs.write8(off, v),
            AccessWidth::Dword => self.regs.write32(off, u32::from(v)),
        }
    }
}
