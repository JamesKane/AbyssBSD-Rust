// SPDX-License-Identifier: BSD-2-Clause
//! What `libmemstat` reads: `vm.zone_stats`, `kern.malloc_stats`
//! (`<vm/uma.h>`, `<sys/malloc.h>`), and `kern.ipc.sfstat`
//! (`<sys/sf_buf.h>`).

/// `UMA_STREAM_VERSION`.
pub const UMA_STREAM_VERSION: u32 = 1;
/// `MALLOC_TYPE_STREAM_VERSION`.
pub const MALLOC_TYPE_STREAM_VERSION: u32 = 1;
/// `UTH_MAX_NAME`, `MALLOC_MAX_NAME`.
pub const NAME_MAX: usize = 32;

/// `struct uma_stream_header`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UmaStreamHeader {
    pub ush_version: u32,
    pub ush_maxcpus: u32,
    pub ush_count: u32,
    pub _ush_pad: u32,
}

/// `struct uma_type_header`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UmaTypeHeader {
    pub uth_name: [u8; NAME_MAX],
    pub uth_align: u32,
    pub uth_size: u32,
    pub uth_rsize: u32,
    pub uth_maxpages: u32,
    pub uth_limit: u32,
    pub uth_pages: u32,
    pub uth_keg_free: u32,
    pub uth_zone_free: u32,
    pub uth_bucketsize: u32,
    pub uth_zone_flags: u32,
    pub uth_allocs: u64,
    pub uth_frees: u64,
    pub uth_fails: u64,
    pub uth_sleeps: u64,
    pub uth_xdomain: u64,
    pub _uth_reserved1: [u64; 1],
}

/// `struct malloc_type_stream_header`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MallocTypeStreamHeader {
    pub mtsh_version: u32,
    pub mtsh_maxcpus: u32,
    pub mtsh_count: u32,
    pub _mtsh_pad: u32,
}

/// `struct malloc_type_header`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MallocTypeHeader {
    pub mth_name: [u8; NAME_MAX],
}

/// `struct sfstat`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Sfstat {
    pub sf_syscalls: u64,
    pub sf_noiocnt: u64,
    pub sf_iocnt: u64,
    pub sf_pages_read: u64,
    pub sf_pages_valid: u64,
    pub sf_rhpages_requested: u64,
    pub sf_rhpages_read: u64,
    pub sf_busy: u64,
    pub sf_allocfail: u64,
    pub sf_allocwait: u64,
    pub sf_pages_bogus: u64,
}
