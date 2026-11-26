// SPDX-License-Identifier: MIT
// Copyright (C) 2023 Collabora ltd.
// Copyright (c) 2026 The AbyssBSD Project
//! Panthor's userspace interface: request numbers, structure layouts (as byte offsets) and
//! constants, from the MIT-licensed `panthor_drm.h` (Collabora, 2023), with the semantics of the
//! clean-room specification's section 10. Arguments are read and written little-endian, field
//! by field.

use alloc::vec::Vec;

use abi::errno::Errno;

/// The interface version this driver reports (`DRM_IOCTL_VERSION`): what a 1.N kernel offers,
/// up to N (spec 10 §10.11).
pub(crate) const MAJOR: i32 = 1;
pub(crate) const MINOR: i32 = 6;

// Requests: `DRM_COMMAND_BASE` (0x40) plus the index (10 §10.2).
pub(crate) const DEV_QUERY: u8 = 0x40;
pub(crate) const VM_CREATE: u8 = 0x41;
pub(crate) const VM_DESTROY: u8 = 0x42;
pub(crate) const VM_BIND: u8 = 0x43;
pub(crate) const VM_GET_STATE: u8 = 0x44;
pub(crate) const BO_CREATE: u8 = 0x45;
pub(crate) const BO_MMAP_OFFSET: u8 = 0x46;
pub(crate) const GROUP_CREATE: u8 = 0x47;
pub(crate) const GROUP_DESTROY: u8 = 0x48;
pub(crate) const GROUP_SUBMIT: u8 = 0x49;
pub(crate) const GROUP_GET_STATE: u8 = 0x4A;
pub(crate) const TILER_HEAP_CREATE: u8 = 0x4B;
pub(crate) const TILER_HEAP_DESTROY: u8 = 0x4C;
pub(crate) const BO_SET_LABEL: u8 = 0x4D;
pub(crate) const SET_USER_MMIO_OFFSET: u8 = 0x4E;

// Device queries.
pub(crate) const QUERY_GPU_INFO: u32 = 0;
pub(crate) const QUERY_CSIF_INFO: u32 = 1;
pub(crate) const QUERY_TIMESTAMP_INFO: u32 = 2;
pub(crate) const QUERY_GROUP_PRIORITIES_INFO: u32 = 3;
pub(crate) const GPU_INFO_SIZE: usize = 112;
pub(crate) const GPU_INFO_MIN: usize = 96;
pub(crate) const CSIF_INFO_SIZE: usize = 24;
pub(crate) const TIMESTAMP_INFO_SIZE: usize = 56;
pub(crate) const TIMESTAMP_INFO_MIN: usize = 16;
pub(crate) const PRIORITIES_INFO_SIZE: usize = 4;

/// The user MMIO offsets (10 §10.9): 64-bit callers' default, and the 32-bit one.
pub(crate) const USER_MMIO_64: u64 = 1 << 56;
pub(crate) const USER_MMIO_32: u64 = 1 << 43;

// Sync operations (10 §10.5).
pub(crate) const SYNC_TYPE_MASK: u32 = 0xFF;
pub(crate) const SYNC_BINARY: u32 = 0;
pub(crate) const SYNC_TIMELINE: u32 = 1;
pub(crate) const SYNC_SIGNAL: u32 = 1 << 31;

// VM bind operations (10 §10.4.3).
pub(crate) const BIND_ASYNC: u32 = 1;
pub(crate) const MAP_READONLY: u32 = 1 << 0;
pub(crate) const MAP_NOEXEC: u32 = 1 << 1;
pub(crate) const MAP_UNCACHED: u32 = 1 << 2;
pub(crate) const OP_TYPE_MASK: u32 = 0xF << 28;
pub(crate) const OP_MAP: u32 = 0;
pub(crate) const OP_UNMAP: u32 = 1 << 28;
pub(crate) const OP_SYNC_ONLY: u32 = 2 << 28;

// BO flags (10 §10.8.1).
pub(crate) const BO_NO_MMAP: u32 = 1 << 0;
pub(crate) const BO_WB_MMAP: u32 = 1 << 1;

// Group state (10 §10.6.4).
pub(crate) const STATE_TIMEDOUT: u32 = 1 << 0;
pub(crate) const STATE_FATAL_FAULT: u32 = 1 << 1;

pub(crate) fn u32_at(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4)
        .map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub(crate) fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from(u32_at(b, off)) | u64::from(u32_at(b, off + 4)) << 32
}

pub(crate) fn u8_at(b: &[u8], off: usize) -> u8 {
    b.get(off).copied().unwrap_or(0)
}

pub(crate) fn put_u32(b: &mut [u8], off: usize, v: u32) {
    if let Some(s) = b.get_mut(off..off + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

pub(crate) fn put_u64(b: &mut [u8], off: usize, v: u64) {
    if let Some(s) = b.get_mut(off..off + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// Reads user memory at an address into a buffer.
pub(crate) type CopyIn<'a> = dyn Fn(&mut [u8], u64) -> Result<(), Errno> + 'a;

/// An object array descriptor (10 §10.1.3): stride, count, array.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ObjArray {
    pub(crate) stride: u32,
    pub(crate) count: u32,
    pub(crate) array: u64,
}

impl ObjArray {
    pub(crate) fn at(b: &[u8], off: usize) -> Self {
        Self {
            stride: u32_at(b, off),
            count: u32_at(b, off + 4),
            array: u64_at(b, off + 8),
        }
    }

    /// Copies the elements in with the stride rules: each of `size` bytes, the caller's stride at
    /// least `min`, a shorter one zero-filled, a longer one's extra bytes required zero.
    pub(crate) fn read(
        &self,
        size: usize,
        min: usize,
        max_count: u32,
        copy_in: &CopyIn<'_>,
    ) -> Result<Vec<Vec<u8>>, Errno> {
        if self.count == 0 {
            return Ok(Vec::new());
        }
        if self.count > max_count {
            return Err(Errno::EINVAL);
        }
        let stride = self.stride as usize;
        if stride < min {
            return Err(Errno::EINVAL);
        }
        let mut out = Vec::new();
        out.try_reserve(self.count as usize)
            .map_err(|_| Errno::ENOMEM)?;
        let mut raw = alloc::vec![0u8; stride];
        for k in 0..u64::from(self.count) {
            let addr = self
                .array
                .checked_add(k * self.stride as u64)
                .ok_or(Errno::EFAULT)?;
            copy_in(&mut raw, addr)?;
            if stride > size && raw[size..].iter().any(|&b| b != 0) {
                return Err(Errno::E2BIG);
            }
            let mut e = alloc::vec![0u8; size];
            let n = size.min(stride);
            e[..n].copy_from_slice(&raw[..n]);
            out.push(e);
        }
        Ok(out)
    }
}

/// A sync operation (16 bytes).
#[derive(Clone, Copy, Debug)]
pub(crate) struct SyncOp {
    pub(crate) signal: bool,
    pub(crate) timeline: bool,
    pub(crate) handle: u32,
    pub(crate) point: u64,
}

impl SyncOp {
    pub(crate) fn parse(e: &[u8]) -> Result<Self, Errno> {
        let flags = u32_at(e, 0);
        if flags & !(SYNC_TYPE_MASK | SYNC_SIGNAL) != 0 {
            return Err(Errno::EINVAL);
        }
        let timeline = match flags & SYNC_TYPE_MASK {
            SYNC_BINARY => false,
            SYNC_TIMELINE => true,
            _ => return Err(Errno::EINVAL),
        };
        let point = u64_at(e, 8);
        if !timeline && point != 0 {
            return Err(Errno::EINVAL);
        }
        Ok(Self {
            signal: flags & SYNC_SIGNAL != 0,
            timeline,
            handle: u32_at(e, 4),
            point,
        })
    }
}
