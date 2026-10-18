// SPDX-License-Identifier: BSD-2-Clause
//! The UEFI memory map (UEFI 2.10 §7.2.3).

use crate::{Result, Status};

/// A UEFI memory type. Values from `0x8000_0000` up are OS-defined; the
/// loader uses some to tag what it hands the kernel.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryType(pub u32);

#[allow(missing_docs)]
impl MemoryType {
    pub const RESERVED: Self = Self(0);
    pub const LOADER_CODE: Self = Self(1);
    pub const LOADER_DATA: Self = Self(2);
    pub const BOOT_SERVICES_CODE: Self = Self(3);
    pub const BOOT_SERVICES_DATA: Self = Self(4);
    pub const RUNTIME_SERVICES_CODE: Self = Self(5);
    pub const RUNTIME_SERVICES_DATA: Self = Self(6);
    pub const CONVENTIONAL: Self = Self(7);
    pub const UNUSABLE: Self = Self(8);
    pub const ACPI_RECLAIM: Self = Self(9);
    pub const ACPI_NVS: Self = Self(10);
    pub const MMIO: Self = Self(11);
    pub const MMIO_PORT_SPACE: Self = Self(12);
    pub const PAL_CODE: Self = Self(13);
    pub const PERSISTENT: Self = Self(14);
    pub const UNACCEPTED: Self = Self(15);
}

/// One memory map entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryDescriptor {
    /// Memory type.
    pub kind: MemoryType,
    /// First byte.
    pub start: u64,
    /// Length in 4 KiB pages.
    pub pages: u64,
    /// `EFI_MEMORY_*` attribute bits.
    pub attribute: u64,
}

/// `EFI_MEMORY_RUNTIME`: the region must be mapped for runtime services.
pub const ATTR_RUNTIME: u64 = 1 << 63;

/// A memory map read into a caller buffer.
#[derive(Debug)]
pub struct MemoryMap<'b> {
    bytes: &'b [u8],
    /// The map key `ExitBootServices` needs.
    pub key: usize,
    desc_size: usize,
}

const DESC_MIN: usize = 40;

impl<'b> MemoryMap<'b> {
    pub(crate) fn new(bytes: &'b [u8], key: usize, desc_size: usize) -> Result<Self> {
        if desc_size < DESC_MIN || !bytes.len().is_multiple_of(desc_size) {
            return Err(Status::DEVICE_ERROR);
        }
        Ok(Self {
            bytes,
            key,
            desc_size,
        })
    }

    /// Bytes of descriptors.
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// Size of one descriptor as firmware lays them out.
    pub fn descriptor_size(&self) -> usize {
        self.desc_size
    }

    /// Number of descriptors.
    pub fn len(&self) -> usize {
        self.bytes.len() / self.desc_size
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The descriptors, in firmware order.
    pub fn iter(&self) -> impl Iterator<Item = MemoryDescriptor> + '_ {
        self.bytes.chunks_exact(self.desc_size).map(|d| {
            let u32_at = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
            let u64_at = |o: usize| {
                let mut b = [0; 8];
                b.copy_from_slice(&d[o..o + 8]);
                u64::from_le_bytes(b)
            };
            MemoryDescriptor {
                kind: MemoryType(u32_at(0)),
                start: u64_at(8),
                pages: u64_at(24),
                attribute: u64_at(32),
            }
        })
    }
}
