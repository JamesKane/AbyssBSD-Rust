// SPDX-License-Identifier: BSD-2-Clause
//! Arm Mali GPUs with the Command Stream Frontend (architecture majors 10 to 14), without
//! hardware: the register map, identification, and the GPU-control sequences (soft reset, L2
//! power, cache flushes, the interrupt blocks), over a [`Regs`] window. The first target is the
//! Immortalis-G720 of the CIX Sky1 (architecture 12.8), on the Orange Pi 6 Plus.
//!
//! Everything read from the device is untrusted: an unknown architecture or a value that cannot
//! be right is an error, never a panic.
//!
//! Written from the clean-room specification in `~/Projects/OS/chinese-wall/specs/panthor`
//! (sections 02, 03 and 11). The unit tests run the sequences against a model of the GPU-control
//! block written from the same specification, so they show the code agrees with it, not that the
//! silicon does. The identification test uses the values the board's FreeBSD printed for its
//! GPU (`tests/corpus/opi6p/freebsd/dmesg.boot.txt`).

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod control;
pub mod csf;
pub mod fw;
pub mod id;
pub mod mmu;
pub mod regs;
pub mod ring;
pub mod sky1;

#[cfg(test)]
mod csf_tests;
#[cfg(test)]
mod fw_tests;
#[cfg(test)]
mod mmu_tests;
#[cfg(test)]
mod model;
#[cfg(test)]
mod ring_tests;
#[cfg(test)]
mod tests;

pub use regs::Regs;

/// Waits between polls (the driver sleeps or spins; the tests advance a model).
pub trait Delay {
    /// Waits at least `us` microseconds.
    fn delay_us(&mut self, us: u32);
}

/// What a GPU-control sequence can fail with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// `GPU_ID` read 0: the GPU is unpowered, unclocked or absent.
    NoGpu,
    /// An architecture major this driver does not handle.
    UnsupportedArch(u8),
    /// A wait ran out (what was being waited for).
    Timeout(&'static str),
    /// A cache flush was asked for while one is still outstanding.
    FlushPending,
    /// A value read from the device cannot be right (what it was).
    Bad(&'static str),
}
