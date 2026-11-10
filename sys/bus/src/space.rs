// SPDX-License-Identifier: BSD-2-Clause
//! The resource space (DRIVER-MODEL.md §8.1): which memory and I/O-port
//! ranges are granted, and which may be shared and with whom.
//!
//! - A range is granted to one driver at a time. Two grants may overlap
//!   only where the platform description declares a range shared (the
//!   Sky1's SCMI shared memory, which is the first 128 bytes of its
//!   mailbox's window): each grant then reports [`Sharing::declared`].
//! - Ranges that AML also reads or writes (namespace `SystemMemory` and
//!   `SystemIO` operation regions) are *firmware-shared*. A driver asks
//!   for one explicitly ([`crate::AttachCx::mmio_shared`] and the
//!   others), and its grant reports [`Sharing::firmware`]. A plain
//!   request is refused, so no driver races firmware without knowing.
//!   FreeBSD records no such ranges: its drivers and AML touch them
//!   unaware of each other.

use alloc::vec::Vec;

use crate::types::{DeviceId, ResourceError};

/// The address space a range is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddressSpace {
    /// Physical memory addresses (memory-mapped registers).
    Memory,
    /// x86 I/O ports.
    IoPort,
}

/// A range in one address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Range {
    /// Its space.
    pub space: AddressSpace,
    /// The first address.
    pub base: u64,
    /// Its length in bytes (or ports).
    pub len: u64,
}

impl Range {
    /// One past the last address, saturating.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.base.saturating_add(self.len)
    }

    /// The addresses both hold, if any.
    #[must_use]
    pub fn intersection(&self, o: &Range) -> Option<Range> {
        let base = self.base.max(o.base);
        let end = self.end().min(o.end());
        (self.space == o.space && base < end).then(|| Range {
            space: self.space,
            base,
            len: end - base,
        })
    }

    /// Whether every address of `o` is in `self`.
    #[must_use]
    pub fn contains(&self, o: &Range) -> bool {
        self.space == o.space && self.base <= o.base && o.end() <= self.end()
    }
}

/// How a granted range is shared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sharing {
    /// AML also reads or writes part of it: the driver's accesses race
    /// with firmware's, which take no lock the driver can take.
    pub firmware: bool,
    /// The platform description declares part of it shared with another
    /// device, which may hold an overlapping grant.
    pub declared: bool,
}

impl Sharing {
    /// Whether the grant's holder is alone in the range.
    #[must_use]
    pub fn is_exclusive(self) -> bool {
        !self.firmware && !self.declared
    }
}

/// The granted ranges and the ranges that may be shared, each shared
/// range with the bus that recorded it: the records go when it detaches.
#[derive(Debug, Default)]
pub(crate) struct ResourceSpace {
    claims: Vec<Range>,
    firmware: Vec<(DeviceId, Range)>,
    declared: Vec<(DeviceId, Range)>,
}

impl ResourceSpace {
    pub(crate) const fn new() -> Self {
        Self {
            claims: Vec::new(),
            firmware: Vec::new(),
            declared: Vec::new(),
        }
    }

    /// Records, for bus `by`, a range AML reads or writes.
    pub(crate) fn share_with_firmware(&mut self, by: DeviceId, r: Range) {
        if r.len > 0 && !self.firmware.contains(&(by, r)) {
            self.firmware.push((by, r));
        }
    }

    /// Records, for bus `by`, a range the platform description declares
    /// shared.
    pub(crate) fn declare_shared(&mut self, by: DeviceId, r: Range) {
        if r.len > 0 && !self.declared.contains(&(by, r)) {
            self.declared.push((by, r));
        }
    }

    /// Drops what bus `by` recorded: it has detached, and its children
    /// with it.
    pub(crate) fn forget(&mut self, by: DeviceId) {
        self.firmware.retain(|e| e.0 != by);
        self.declared.retain(|e| e.0 != by);
    }

    /// Grants `r`. Refused if it overlaps another grant outside a
    /// declared range, or a firmware-shared range unless `firmware_ok`.
    pub(crate) fn claim(&mut self, r: Range, firmware_ok: bool) -> Result<Sharing, ResourceError> {
        let firmware = self.firmware.iter().any(|f| f.1.intersection(&r).is_some());
        if firmware && !firmware_ok {
            return Err(ResourceError::FirmwareShared);
        }
        for c in &self.claims {
            if let Some(both) = c.intersection(&r)
                && !self.declared.iter().any(|d| d.1.contains(&both))
            {
                return Err(ResourceError::Claimed);
            }
        }
        self.claims.push(r);
        Ok(Sharing {
            firmware,
            declared: self.declared.iter().any(|d| d.1.intersection(&r).is_some()),
        })
    }

    /// Gives back one grant of exactly `r` (two declared-shared grants may
    /// start at the same address).
    pub(crate) fn release(&mut self, r: Range) {
        if let Some(k) = self.claims.iter().position(|c| *c == r) {
            self.claims.swap_remove(k);
        }
    }

    /// The number of grants.
    pub(crate) fn claims(&self) -> usize {
        self.claims.len()
    }

    /// The firmware-shared and the declared ranges.
    pub(crate) fn shared(&self) -> (Vec<Range>, Vec<Range>) {
        (
            self.firmware.iter().map(|e| e.1).collect(),
            self.declared.iter().map(|e| e.1).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUS: DeviceId = DeviceId {
        index: 1,
        generation: 0,
    };

    fn mem(base: u64, len: u64) -> Range {
        Range {
            space: AddressSpace::Memory,
            base,
            len,
        }
    }

    #[test]
    fn grants_are_exclusive() {
        let mut s = ResourceSpace::new();
        assert_eq!(s.claim(mem(0x1000, 0x100), false), Ok(Sharing::default()));
        assert_eq!(
            s.claim(mem(0x10ff, 0x10), false),
            Err(ResourceError::Claimed)
        );
        // Another space, or the next byte, is free.
        let io = Range {
            space: AddressSpace::IoPort,
            ..mem(0x1000, 0x100)
        };
        assert!(s.claim(io, false).is_ok());
        assert!(s.claim(mem(0x1100, 0x10), false).is_ok());
        // Disjoint ranges in either order.
        assert_eq!(mem(0x1000, 0x10).intersection(&mem(0x2000, 0x10)), None);
        assert_eq!(mem(0x2000, 0x10).intersection(&mem(0x1000, 0x10)), None);
        s.release(mem(0x1000, 0x100));
        assert!(s.claim(mem(0x10ff, 0x1), false).is_ok());
        assert_eq!(s.claims(), 3);
    }

    /// The Sky1's SCMI shared memory is the first 128 bytes of its
    /// mailbox's 64 KiB window, and the platform declares it.
    #[test]
    fn declared_ranges_may_overlap() {
        let mut s = ResourceSpace::new();
        let (window, shmem) = (mem(0x0659_0000, 0x1_0000), mem(0x0659_0000, 0x80));
        s.declare_shared(BUS, shmem);
        let shared = Sharing {
            firmware: false,
            declared: true,
        };
        assert_eq!(s.claim(window, false), Ok(shared));
        assert_eq!(s.claim(shmem, false), Ok(shared));
        // Not past the declared range.
        assert_eq!(
            s.claim(mem(0x0659_0040, 0x80), false),
            Err(ResourceError::Claimed)
        );
        // Releasing one leaves the other's grant at the same base.
        s.release(shmem);
        assert_eq!(s.claims(), 1);
        assert_eq!(
            s.claim(mem(0x0659_0080, 4), false),
            Err(ResourceError::Claimed)
        );
    }

    #[test]
    fn firmware_ranges_need_asking() {
        let mut s = ResourceSpace::new();
        s.share_with_firmware(
            BUS,
            Range {
                space: AddressSpace::IoPort,
                base: 0x2e,
                len: 2,
            },
        );
        let sio = Range {
            space: AddressSpace::IoPort,
            base: 0x2e,
            len: 1,
        };
        assert_eq!(s.claim(sio, false), Err(ResourceError::FirmwareShared));
        assert_eq!(s.claims(), 0);
        assert_eq!(
            s.claim(sio, true),
            Ok(Sharing {
                firmware: true,
                declared: false
            })
        );
        // Asking does not lift exclusivity between drivers.
        assert_eq!(s.claim(sio, true), Err(ResourceError::Claimed));
        // Memory at the same numbers is another space.
        assert_eq!(s.claim(mem(0x2e, 2), false), Ok(Sharing::default()));
        // The record goes with the bus that made it.
        s.release(sio);
        s.forget(BUS);
        assert_eq!(s.claim(sio, false), Ok(Sharing::default()));
    }
}
