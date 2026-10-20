// SPDX-License-Identifier: BSD-2-Clause
//! Memory mapping (`sys/mman.h`): `mmap` flags, `minherit` and `madvise`
//! values. Protections are `PROT_READ` 1, `PROT_WRITE` 2, `PROT_EXEC` 4,
//! with a maximum in the bits above [`PROT_MAX_SHIFT`].

use crate::layout::{Arches, ConstValue};

pub const MAP_SHARED: u32 = 0x0001;
pub const MAP_PRIVATE: u32 = 0x0002;
pub const MAP_FIXED: u32 = 0x0010;
pub const MAP_HASSEMAPHORE: u32 = 0x0200;
pub const MAP_STACK: u32 = 0x0400;
pub const MAP_NOSYNC: u32 = 0x0800;
pub const MAP_ANON: u32 = 0x1000;
pub const MAP_GUARD: u32 = 0x2000;
pub const MAP_EXCL: u32 = 0x4000;
pub const MAP_NOCORE: u32 = 0x0002_0000;
pub const MAP_PREFAULT_READ: u32 = 0x0004_0000;
pub const MAP_32BIT: u32 = 0x0008_0000;
/// `MAP_ALIGNED(n)` is `n << MAP_ALIGNMENT_SHIFT`: align to `2^n` bytes.
pub const MAP_ALIGNMENT_SHIFT: u32 = 24;
/// (C's is a negative `int`, so it is not compared.)
pub const MAP_ALIGNMENT_MASK: u32 = 0xff << MAP_ALIGNMENT_SHIFT;
/// `MAP_ALIGNED(1)`: align to a superpage.
pub const MAP_ALIGNED_SUPER: u32 = 1 << MAP_ALIGNMENT_SHIFT;

/// `PROT_MAX(p)` is `p << PROT_MAX_SHIFT` (C has only the macro).
pub const PROT_MAX_SHIFT: u32 = 16;

pub const INHERIT_SHARE: u32 = 0;
pub const INHERIT_COPY: u32 = 1;
pub const INHERIT_NONE: u32 = 2;
pub const INHERIT_ZERO: u32 = 3;

pub const MADV_NORMAL: u32 = 0;
pub const MADV_RANDOM: u32 = 1;
pub const MADV_SEQUENTIAL: u32 = 2;
pub const MADV_WILLNEED: u32 = 3;
pub const MADV_DONTNEED: u32 = 4;
pub const MADV_FREE: u32 = 5;
pub const MADV_NOSYNC: u32 = 6;
pub const MADV_AUTOSYNC: u32 = 7;
pub const MADV_NOCORE: u32 = 8;
pub const MADV_CORE: u32 = 9;
pub const MADV_PROTECT: u32 = 10;

/// `msync` flags.
pub const MS_SYNC: u32 = 0x0000;
pub const MS_ASYNC: u32 = 0x0001;
pub const MS_INVALIDATE: u32 = 0x0002;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("MAP_SHARED", MAP_SHARED),
    c("MAP_PRIVATE", MAP_PRIVATE),
    c("MAP_FIXED", MAP_FIXED),
    c("MAP_HASSEMAPHORE", MAP_HASSEMAPHORE),
    c("MAP_STACK", MAP_STACK),
    c("MAP_NOSYNC", MAP_NOSYNC),
    c("MAP_ANON", MAP_ANON),
    c("MAP_GUARD", MAP_GUARD),
    c("MAP_EXCL", MAP_EXCL),
    c("MAP_NOCORE", MAP_NOCORE),
    c("MAP_PREFAULT_READ", MAP_PREFAULT_READ),
    c("MAP_32BIT", MAP_32BIT),
    c("MAP_ALIGNMENT_SHIFT", MAP_ALIGNMENT_SHIFT),
    c("MAP_ALIGNED_SUPER", MAP_ALIGNED_SUPER),
    c("INHERIT_SHARE", INHERIT_SHARE),
    c("INHERIT_COPY", INHERIT_COPY),
    c("INHERIT_NONE", INHERIT_NONE),
    c("INHERIT_ZERO", INHERIT_ZERO),
    c("MADV_NORMAL", MADV_NORMAL),
    c("MADV_RANDOM", MADV_RANDOM),
    c("MADV_SEQUENTIAL", MADV_SEQUENTIAL),
    c("MADV_WILLNEED", MADV_WILLNEED),
    c("MADV_DONTNEED", MADV_DONTNEED),
    c("MADV_FREE", MADV_FREE),
    c("MADV_NOSYNC", MADV_NOSYNC),
    c("MADV_AUTOSYNC", MADV_AUTOSYNC),
    c("MADV_NOCORE", MADV_NOCORE),
    c("MADV_CORE", MADV_CORE),
    c("MADV_PROTECT", MADV_PROTECT),
    c("MS_SYNC", MS_SYNC),
    c("MS_ASYNC", MS_ASYNC),
    c("MS_INVALIDATE", MS_INVALIDATE),
    c("PROT_MAX(1)", 1 << PROT_MAX_SHIFT),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/mman.h",
        value: value as i64,
        arches: Arches::All,
    }
}
