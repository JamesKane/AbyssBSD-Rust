// SPDX-License-Identifier: BSD-2-Clause
//! Register-region providers (DRIVER-MODEL.md §9.3, `RegisterRegion`): a
//! block of registers several drivers program, with the read-modify-write
//! done under one lock (a "syscon": the Sky1's system-controller windows).
//!
//! The driver that owns the window implements [`RegisterRegion`] and
//! publishes it with `StartCx::provide_region`. A client asks for the one
//! its firmware names (an ACPI `_DSD` reference such as
//! `cix,usbphy_syscon`) with `AttachCx::region` and gets a
//! [`RegionHandle`]. A handle may be limited to the bits its client owns,
//! so one client cannot change another's fields.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::provider::ProviderError;

/// A window of 32-bit registers.
pub trait RegisterRegion: Send + Sync {
    /// The window's size in bytes.
    fn len(&self) -> usize;
    /// Whether the window is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The register at byte `offset` (aligned, inside the window).
    fn read(&self, offset: usize) -> u32;
    /// Changes the bits of `mask` in the register at `offset` to those of
    /// `value`, atomically with respect to every other client.
    fn update(&self, offset: usize, mask: u32, value: u32);
}

/// A published region.
pub struct RegionSource {
    imp: Arc<dyn RegisterRegion>,
    revoked: AtomicBool,
}

impl core::fmt::Debug for RegionSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegionSource")
            .field("len", &self.imp.len())
            .finish_non_exhaustive()
    }
}

impl RegionSource {
    /// Wraps a provider.
    #[must_use]
    pub fn new(imp: Arc<dyn RegisterRegion>) -> Arc<Self> {
        Arc::new(Self {
            imp,
            revoked: AtomicBool::new(false),
        })
    }

    /// A lease on the whole window.
    #[must_use]
    pub fn handle(self: &Arc<Self>) -> RegionHandle {
        RegionHandle {
            source: Arc::clone(self),
            fields: None,
        }
    }

    /// Stops every access: the driver is detaching.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    fn live(&self) -> Result<&dyn RegisterRegion, ProviderError> {
        if self.revoked.load(Ordering::Acquire) {
            Err(ProviderError::Revoked)
        } else {
            Ok(&*self.imp)
        }
    }
}

/// A client's lease of a region.
#[derive(Debug, Clone)]
pub struct RegionHandle {
    source: Arc<RegionSource>,
    /// The bits this client may change: `(offset, mask)`, or all.
    fields: Option<(usize, u32)>,
}

impl RegionHandle {
    /// Limits the lease to the bits of `mask` in the register at `offset`
    /// (a field a client owns).
    #[must_use]
    pub fn field(mut self, offset: usize, mask: u32) -> Self {
        self.fields = Some((offset, mask));
        self
    }

    fn check(&self, r: &dyn RegisterRegion, offset: usize) -> Result<(), ProviderError> {
        if !offset.is_multiple_of(4) || offset.checked_add(4).is_none_or(|e| e > r.len()) {
            return Err(ProviderError::OutOfRange);
        }
        Ok(())
    }

    /// The register at `offset`.
    ///
    /// # Errors
    ///
    /// [`ProviderError::OutOfRange`] outside the window or unaligned;
    /// [`ProviderError::Revoked`] once the owner is gone.
    pub fn read(&self, offset: usize) -> Result<u32, ProviderError> {
        let r = self.source.live()?;
        self.check(r, offset)?;
        Ok(r.read(offset))
    }

    /// Changes the bits of `mask` at `offset` to those of `value`.
    ///
    /// # Errors
    ///
    /// As [`RegionHandle::read`], and [`ProviderError::Unsupported`] for
    /// bits outside the lease's field.
    pub fn update(&self, offset: usize, mask: u32, value: u32) -> Result<(), ProviderError> {
        let r = self.source.live()?;
        self.check(r, offset)?;
        if let Some((o, m)) = self.fields
            && (o != offset || mask & !m != 0)
        {
            return Err(ProviderError::Unsupported);
        }
        r.update(offset, mask, value & mask);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Fake(Mutex<[u32; 8]>);

    impl RegisterRegion for Fake {
        fn len(&self) -> usize {
            32
        }
        fn read(&self, offset: usize) -> u32 {
            self.0.lock().unwrap()[offset / 4]
        }
        fn update(&self, offset: usize, mask: u32, value: u32) {
            let mut r = self.0.lock().unwrap();
            r[offset / 4] = r[offset / 4] & !mask | value & mask;
        }
    }

    #[test]
    fn updates_change_only_their_bits_and_leases_are_limited() {
        let fake = Arc::new(Fake::default());
        fake.0.lock().unwrap()[2] = 0xF0F0;
        let src = RegionSource::new(fake.clone());
        let all = src.handle();
        all.update(8, 0x0F, 0x05).unwrap();
        assert_eq!(all.read(8), Ok(0xF0F5));
        // A lease on bit 3 of word 2 only.
        let bit = src.handle().field(8, 1 << 3);
        bit.update(8, 1 << 3, u32::MAX).unwrap();
        assert_eq!(all.read(8), Ok(0xF0FD));
        assert_eq!(bit.update(8, 1 << 4, 0), Err(ProviderError::Unsupported));
        assert_eq!(bit.update(12, 1 << 3, 0), Err(ProviderError::Unsupported));
        // The window's edge and alignment.
        assert_eq!(all.read(32), Err(ProviderError::OutOfRange));
        assert_eq!(all.read(6), Err(ProviderError::OutOfRange));
        src.revoke();
        assert_eq!(all.read(0), Err(ProviderError::Revoked));
        assert_eq!(bit.update(8, 1 << 3, 0), Err(ProviderError::Revoked));
    }
}
