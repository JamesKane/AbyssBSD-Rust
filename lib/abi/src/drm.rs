// SPDX-License-Identifier: MIT
// Copyright 1999 Precision Insight, Inc., Cedar Park, Texas.
// Copyright 2000 VA Linux Systems, Inc., Sunnyvale, California.
// Copyright (c) 2007 Dave Airlie <airlied@linux.ie>
// Copyright (c) 2007 Jakob Bornecrantz <wallbraker@gmail.com>
// Copyright (c) 2008 Red Hat Inc.
// Copyright (c) 2007-2008 Tungsten Graphics, Inc., Cedar Park, TX., USA
// Copyright (c) 2007-2008 Intel Corporation
// Copyright (c) 2026 The AbyssBSD Project
//! The Direct Rendering Manager user interface (`drm.h`, `drm_mode.h`: MIT-licensed), as
//! libdrm built for FreeBSD sees it: the structures `/dev/dri/card*`
//! and `renderD*` ioctls pass, the events `read` returns, and the
//! capability and object numbers.
//!
//! Request numbers use FreeBSD's `<sys/ioccom.h>` encoding (a 13-bit
//! size field, `IOC_IN|IOC_OUT` for read-write), not Linux's `_IOC`.
//! Pointers are 64-bit user addresses (`u64`); the layouts are for
//! LP64 only, as the target is.
//!
//! Behavior reference: `docs/GRAPHICS.md` §2.

use crate::ioctl::{IOC_IN, IOC_OUT, IOC_VOID, ioc_of};

/// The group letter every Direct Rendering Manager request uses (`DRM_IOCTL_BASE`).
pub const DRM_IOCTL_BASE: u8 = b'd';
/// Driver-specific requests start here (`DRM_COMMAND_BASE`).
pub const DRM_COMMAND_BASE: u8 = 0x40;
/// Driver-specific requests end here, exclusive (`DRM_COMMAND_END`).
pub const DRM_COMMAND_END: u8 = 0xa0;

const fn io(nr: u8) -> u32 {
    ioc_of(IOC_VOID, DRM_IOCTL_BASE, nr, 0)
}
const fn iow(nr: u8, len: usize) -> u32 {
    ioc_of(IOC_IN, DRM_IOCTL_BASE, nr, len)
}
const fn ior(nr: u8, len: usize) -> u32 {
    ioc_of(IOC_OUT, DRM_IOCTL_BASE, nr, len)
}
const fn iowr(nr: u8, len: usize) -> u32 {
    ioc_of(IOC_IN | IOC_OUT, DRM_IOCTL_BASE, nr, len)
}

/// The request number inside a Direct Rendering Manager request (`DRM_IOCTL_NR`).
#[must_use]
pub const fn ioctl_nr(req: u32) -> u8 {
    (req & 0xff) as u8
}

/// Whether `req` is a Direct Rendering Manager request: the group letter is `'d'`.
#[must_use]
pub const fn is_drm_ioctl(req: u32) -> bool {
    ((req >> 8) & 0xff) as u8 == DRM_IOCTL_BASE
}

// ---- core (drm.h) ----

/// `struct drm_version`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmVersion {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    pub name_len: u64,
    pub name: u64,
    pub date_len: u64,
    pub date: u64,
    pub desc_len: u64,
    pub desc: u64,
}

/// `struct drm_unique`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmUnique {
    pub unique_len: u64,
    pub unique: u64,
}

/// `struct drm_auth`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmAuth {
    pub magic: u32,
}

/// `struct drm_gem_close`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmGemClose {
    pub handle: u32,
    pub pad: u32,
}

/// `struct drm_gem_flink`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmGemFlink {
    pub handle: u32,
    pub name: u32,
}

/// `struct drm_gem_open`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmGemOpen {
    pub name: u32,
    pub handle: u32,
    pub size: u64,
}

/// `struct drm_get_cap`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmGetCap {
    pub capability: u64,
    pub value: u64,
}

/// `struct drm_set_client_cap`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmSetClientCap {
    pub capability: u64,
    pub value: u64,
}

/// `struct drm_set_client_name`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmSetClientName {
    pub name_len: u64,
    pub name: u64,
}

/// `struct drm_modeset_ctl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModesetCtl {
    pub crtc: u32,
    pub cmd: u32,
}

/// `struct drm_prime_handle`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmPrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

/// `struct drm_wait_vblank_request`, the request half of `union
/// drm_wait_vblank` (`signal` is an `unsigned long`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmWaitVblankRequest {
    pub kind: u32,
    pub sequence: u32,
    pub signal: u64,
}

/// `struct drm_wait_vblank_reply`, the reply half.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmWaitVblankReply {
    pub kind: u32,
    pub sequence: u32,
    pub tval_sec: i64,
    pub tval_usec: i64,
}

/// `union drm_wait_vblank`: the request in, the reply out. Both halves
/// are 24 bytes and share their first two fields, so one type holds
/// either.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmWaitVblank {
    pub kind: u32,
    pub sequence: u32,
    /// The request's `signal`, or the reply's `tval_sec`.
    pub word0: u64,
    /// The reply's `tval_usec` (unused in a request).
    pub word1: u64,
}

impl DrmWaitVblank {
    /// The reply for a wait that finished at `sec`.`usec`.
    #[must_use]
    pub fn reply(kind: u32, sequence: u32, sec: i64, usec: i64) -> Self {
        let r = DrmWaitVblankReply {
            kind,
            sequence,
            tval_sec: sec,
            tval_usec: usec,
        };
        Self {
            kind: r.kind,
            sequence: r.sequence,
            word0: u64::from_ne_bytes(r.tval_sec.to_ne_bytes()),
            word1: u64::from_ne_bytes(r.tval_usec.to_ne_bytes()),
        }
    }
}

/// `struct drm_crtc_get_sequence`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmCrtcGetSequence {
    pub crtc_id: u32,
    pub active: u32,
    pub sequence: u64,
    pub sequence_ns: i64,
}

/// `struct drm_crtc_queue_sequence`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmCrtcQueueSequence {
    pub crtc_id: u32,
    pub flags: u32,
    pub sequence: u64,
    pub user_data: u64,
}

/// `struct drm_event`: the header of every event `read` returns.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmEvent {
    pub kind: u32,
    pub length: u32,
}

/// `struct drm_event_vblank`: `DRM_EVENT_VBLANK` and
/// `DRM_EVENT_FLIP_COMPLETE`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmEventVblank {
    pub base: DrmEvent,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
}

impl DrmEventVblank {
    /// Its bytes, as `read` returns them.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut b = [0u8; 32];
        b[0..4].copy_from_slice(&self.base.kind.to_ne_bytes());
        b[4..8].copy_from_slice(&self.base.length.to_ne_bytes());
        b[8..16].copy_from_slice(&self.user_data.to_ne_bytes());
        b[16..20].copy_from_slice(&self.tv_sec.to_ne_bytes());
        b[20..24].copy_from_slice(&self.tv_usec.to_ne_bytes());
        b[24..28].copy_from_slice(&self.sequence.to_ne_bytes());
        b[28..32].copy_from_slice(&self.crtc_id.to_ne_bytes());
        b
    }
}

/// `struct drm_event_crtc_sequence`: `DRM_EVENT_CRTC_SEQUENCE`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmEventCrtcSequence {
    pub base: DrmEvent,
    pub user_data: u64,
    pub time_ns: i64,
    pub sequence: u64,
}

/// `drm_crtc_queue_sequence`'s flags: `sequence` counts from now, and a
/// missed absolute one goes to the next frame.
pub const DRM_CRTC_SEQUENCE_RELATIVE: u32 = 0x1;
pub const DRM_CRTC_SEQUENCE_NEXT_ON_MISS: u32 = 0x2;

pub const DRM_EVENT_VBLANK: u32 = 0x01;
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;
pub const DRM_EVENT_CRTC_SEQUENCE: u32 = 0x03;

/// `DRM_CLOEXEC` and `DRM_RDWR` (`PRIME_HANDLE_TO_FD` flags): libdrm
/// defines them as `O_CLOEXEC` and `O_RDWR`, which are FreeBSD's values here.
pub const DRM_CLOEXEC: u32 = crate::fcntl::O_CLOEXEC;
pub const DRM_RDWR: u32 = crate::fcntl::O_RDWR;

// `drm_wait_vblank` types.
pub const DRM_VBLANK_ABSOLUTE: u32 = 0x0;
pub const DRM_VBLANK_RELATIVE: u32 = 0x1;
pub const DRM_VBLANK_HIGH_CRTC_MASK: u32 = 0x0000_003e;
pub const DRM_VBLANK_HIGH_CRTC_SHIFT: u32 = 1;
pub const DRM_VBLANK_EVENT: u32 = 0x400_0000;
pub const DRM_VBLANK_FLIP: u32 = 0x800_0000;
pub const DRM_VBLANK_NEXTONMISS: u32 = 0x1000_0000;
pub const DRM_VBLANK_SECONDARY: u32 = 0x2000_0000;
pub const DRM_VBLANK_SIGNAL: u32 = 0x4000_0000;

// `DRM_IOCTL_GET_CAP` capabilities.
pub const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
pub const DRM_CAP_VBLANK_HIGH_CRTC: u64 = 0x2;
pub const DRM_CAP_DUMB_PREFERRED_DEPTH: u64 = 0x3;
pub const DRM_CAP_DUMB_PREFER_SHADOW: u64 = 0x4;
pub const DRM_CAP_PRIME: u64 = 0x5;
pub const DRM_PRIME_CAP_IMPORT: u64 = 0x1;
pub const DRM_PRIME_CAP_EXPORT: u64 = 0x2;
pub const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
pub const DRM_CAP_ASYNC_PAGE_FLIP: u64 = 0x7;
pub const DRM_CAP_CURSOR_WIDTH: u64 = 0x8;
pub const DRM_CAP_CURSOR_HEIGHT: u64 = 0x9;
pub const DRM_CAP_ADDFB2_MODIFIERS: u64 = 0x10;
pub const DRM_CAP_PAGE_FLIP_TARGET: u64 = 0x11;
pub const DRM_CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
pub const DRM_CAP_SYNCOBJ: u64 = 0x13;
pub const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
pub const DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP: u64 = 0x15;

// `DRM_IOCTL_SET_CLIENT_CAP` capabilities.
pub const DRM_CLIENT_CAP_STEREO_3D: u64 = 1;
pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
pub const DRM_CLIENT_CAP_ASPECT_RATIO: u64 = 4;
pub const DRM_CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;
pub const DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;

/// `DMA_BUF_BASE`: the group letter of the dma-buf requests.
pub const DMA_BUF_BASE: u8 = b'b';
/// `struct dma_buf_sync`: bracket CPU access with start and end.
pub const DMA_BUF_SYNC_READ: u64 = 1;
pub const DMA_BUF_SYNC_WRITE: u64 = 2;
pub const DMA_BUF_SYNC_RW: u64 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;
pub const DMA_BUF_SYNC_START: u64 = 0;
pub const DMA_BUF_SYNC_END: u64 = 1 << 2;
/// `DMA_BUF_IOCTL_SYNC`: `_IOW('b', 0, struct dma_buf_sync)`, one
/// 64-bit flags word.
pub const DMA_BUF_IOCTL_SYNC: u32 = ioc_of(IOC_IN, DMA_BUF_BASE, 0, 8);

pub const DRM_IOCTL_VERSION: u32 = iowr(0x00, size_of::<DrmVersion>());
pub const DRM_IOCTL_GET_UNIQUE: u32 = iowr(0x01, size_of::<DrmUnique>());
pub const DRM_IOCTL_GET_MAGIC: u32 = ior(0x02, size_of::<DrmAuth>());
pub const DRM_IOCTL_MODESET_CTL: u32 = iow(0x08, size_of::<DrmModesetCtl>());
pub const DRM_IOCTL_GEM_CLOSE: u32 = iow(0x09, size_of::<DrmGemClose>());
pub const DRM_IOCTL_GEM_FLINK: u32 = iowr(0x0a, size_of::<DrmGemFlink>());
pub const DRM_IOCTL_GEM_OPEN: u32 = iowr(0x0b, size_of::<DrmGemOpen>());
pub const DRM_IOCTL_GET_CAP: u32 = iowr(0x0c, size_of::<DrmGetCap>());
pub const DRM_IOCTL_SET_CLIENT_CAP: u32 = iow(0x0d, size_of::<DrmSetClientCap>());
pub const DRM_IOCTL_SET_UNIQUE: u32 = iow(0x10, size_of::<DrmUnique>());
pub const DRM_IOCTL_AUTH_MAGIC: u32 = iow(0x11, size_of::<DrmAuth>());
pub const DRM_IOCTL_SET_MASTER: u32 = io(0x1e);
pub const DRM_IOCTL_DROP_MASTER: u32 = io(0x1f);
pub const DRM_IOCTL_PRIME_HANDLE_TO_FD: u32 = iowr(0x2d, size_of::<DrmPrimeHandle>());
pub const DRM_IOCTL_PRIME_FD_TO_HANDLE: u32 = iowr(0x2e, size_of::<DrmPrimeHandle>());
pub const DRM_IOCTL_WAIT_VBLANK: u32 = iowr(0x3a, size_of::<DrmWaitVblank>());
pub const DRM_IOCTL_CRTC_GET_SEQUENCE: u32 = iowr(0x3b, size_of::<DrmCrtcGetSequence>());
pub const DRM_IOCTL_CRTC_QUEUE_SEQUENCE: u32 = iowr(0x3c, size_of::<DrmCrtcQueueSequence>());
pub const DRM_IOCTL_SET_CLIENT_NAME: u32 = iowr(0xd1, size_of::<DrmSetClientName>());

// ---- modesetting (drm_mode.h) ----

pub const DRM_DISPLAY_MODE_LEN: usize = 32;
pub const DRM_PROP_NAME_LEN: usize = 32;
pub const DRM_CONNECTOR_NAME_LEN: usize = 32;

/// `struct drm_mode_modeinfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeModeinfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub kind: u32,
    pub name: [u8; DRM_DISPLAY_MODE_LEN],
}

/// `struct drm_mode_card_res`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

/// `struct drm_mode_crtc`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCrtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: DrmModeModeinfo,
}

/// `struct drm_mode_set_plane`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeSetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_h: u32,
    pub src_w: u32,
}

/// `struct drm_mode_get_plane`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeGetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: u64,
}

/// `struct drm_mode_get_plane_res`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeGetPlaneRes {
    pub plane_id_ptr: u64,
    pub count_planes: u32,
}

/// `struct drm_mode_get_encoder`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeGetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

/// `struct drm_mode_get_connector`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeGetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

/// `struct drm_mode_property_enum`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModePropertyEnum {
    pub value: u64,
    pub name: [u8; DRM_PROP_NAME_LEN],
}

/// `struct drm_mode_get_property`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeGetProperty {
    pub values_ptr: u64,
    pub enum_blob_ptr: u64,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [u8; DRM_PROP_NAME_LEN],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

/// `struct drm_mode_connector_set_property`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeConnectorSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub connector_id: u32,
}

/// `struct drm_mode_obj_get_properties`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeObjGetProperties {
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}

/// `struct drm_mode_obj_set_property`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeObjSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub obj_id: u32,
    pub obj_type: u32,
}

/// `struct drm_mode_get_blob`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeGetBlob {
    pub blob_id: u32,
    pub length: u32,
    pub data: u64,
}

/// `struct drm_mode_fb_cmd` (`ADDFB`, `GETFB`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeFbCmd {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
}

/// `struct drm_mode_fb_cmd2` (`ADDFB2`, `GETFB2`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeFbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub modifier: [u64; 4],
}

/// `struct drm_mode_fb_dirty_cmd`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeFbDirtyCmd {
    pub fb_id: u32,
    pub flags: u32,
    pub color: u32,
    pub num_clips: u32,
    pub clips_ptr: u64,
}

/// `struct drm_mode_cursor`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCursor {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
}

/// `struct drm_mode_cursor2`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCursor2 {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}

/// `struct drm_mode_crtc_lut`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCrtcLut {
    pub crtc_id: u32,
    pub gamma_size: u32,
    pub red: u64,
    pub green: u64,
    pub blue: u64,
}

/// `struct drm_color_lut`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmColorLut {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
    pub reserved: u16,
}

/// `struct drm_color_ctm`: nine S31.32 sign-magnitude values.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmColorCtm {
    pub matrix: [u64; 9],
}

/// `struct drm_mode_crtc_page_flip`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCrtcPageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}

/// `struct drm_mode_crtc_page_flip_target`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCrtcPageFlipTarget {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub sequence: u32,
    pub user_data: u64,
}

/// `struct drm_mode_create_dumb`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

/// `struct drm_mode_map_dumb`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeMapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}

/// `struct drm_mode_destroy_dumb`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeDestroyDumb {
    pub handle: u32,
}

/// `struct drm_mode_atomic`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeAtomic {
    pub flags: u32,
    pub count_objs: u32,
    pub objs_ptr: u64,
    pub count_props_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub reserved: u64,
    pub user_data: u64,
}

/// `struct drm_format_modifier_blob`: the header of an `IN_FORMATS`
/// blob.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmFormatModifierBlob {
    pub version: u32,
    pub flags: u32,
    pub count_formats: u32,
    pub formats_offset: u32,
    pub count_modifiers: u32,
    pub modifiers_offset: u32,
}

/// `struct drm_format_modifier`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmFormatModifier {
    pub formats: u64,
    pub offset: u32,
    pub pad: u32,
    pub modifier: u64,
}

/// `struct drm_mode_create_blob`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCreateBlob {
    pub data: u64,
    pub length: u32,
    pub blob_id: u32,
}

/// `struct drm_mode_destroy_blob`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeDestroyBlob {
    pub blob_id: u32,
}

/// `struct drm_mode_create_lease`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeCreateLease {
    pub object_ids: u64,
    pub object_count: u32,
    pub flags: u32,
    pub lessee_id: u32,
    pub fd: u32,
}

/// `struct drm_mode_list_lessees`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeListLessees {
    pub count_lessees: u32,
    pub pad: u32,
    pub lessees_ptr: u64,
}

/// `struct drm_mode_get_lease`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeGetLease {
    pub count_objects: u32,
    pub pad: u32,
    pub objects_ptr: u64,
}

/// `struct drm_mode_revoke_lease`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeRevokeLease {
    pub lessee_id: u32,
}

/// `struct drm_mode_rect`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeRect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

/// `struct drm_mode_closefb`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrmModeClosefb {
    pub fb_id: u32,
    pub pad: u32,
}

// Mode types and flags.
pub const DRM_MODE_TYPE_BUILTIN: u32 = 1 << 0;
pub const DRM_MODE_TYPE_CLOCK_C: u32 = (1 << 1) | DRM_MODE_TYPE_BUILTIN;
pub const DRM_MODE_TYPE_CRTC_C: u32 = (1 << 2) | DRM_MODE_TYPE_BUILTIN;
pub const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
pub const DRM_MODE_TYPE_DEFAULT: u32 = 1 << 4;
pub const DRM_MODE_TYPE_USERDEF: u32 = 1 << 5;
pub const DRM_MODE_TYPE_DRIVER: u32 = 1 << 6;
pub const DRM_MODE_FLAG_PHSYNC: u32 = 1 << 0;
pub const DRM_MODE_FLAG_NHSYNC: u32 = 1 << 1;
pub const DRM_MODE_FLAG_PVSYNC: u32 = 1 << 2;
pub const DRM_MODE_FLAG_NVSYNC: u32 = 1 << 3;
pub const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;
pub const DRM_MODE_FLAG_DBLSCAN: u32 = 1 << 5;
pub const DRM_MODE_FLAG_CSYNC: u32 = 1 << 6;
pub const DRM_MODE_FLAG_PCSYNC: u32 = 1 << 7;
pub const DRM_MODE_FLAG_NCSYNC: u32 = 1 << 8;
pub const DRM_MODE_FLAG_HSKEW: u32 = 1 << 9;
pub const DRM_MODE_FLAG_DBLCLK: u32 = 1 << 12;
pub const DRM_MODE_FLAG_CLKDIV2: u32 = 1 << 13;
pub const DRM_MODE_FLAG_3D_MASK: u32 = 0x1f << 14;
pub const DRM_MODE_FLAG_PIC_AR_MASK: u32 = 0x0f << 19;

// DPMS, scaling, link status.
pub const DRM_MODE_DPMS_ON: u64 = 0;
pub const DRM_MODE_DPMS_STANDBY: u64 = 1;
pub const DRM_MODE_DPMS_SUSPEND: u64 = 2;
pub const DRM_MODE_DPMS_OFF: u64 = 3;
pub const DRM_MODE_LINK_STATUS_GOOD: u64 = 0;
pub const DRM_MODE_LINK_STATUS_BAD: u64 = 1;

// Encoder types.
pub const DRM_MODE_ENCODER_NONE: u32 = 0;
pub const DRM_MODE_ENCODER_DAC: u32 = 1;
pub const DRM_MODE_ENCODER_TMDS: u32 = 2;
pub const DRM_MODE_ENCODER_LVDS: u32 = 3;
pub const DRM_MODE_ENCODER_TVDAC: u32 = 4;
pub const DRM_MODE_ENCODER_VIRTUAL: u32 = 5;
pub const DRM_MODE_ENCODER_DSI: u32 = 6;
pub const DRM_MODE_ENCODER_DPMST: u32 = 7;
pub const DRM_MODE_ENCODER_DPI: u32 = 8;

// Connector types.
pub const DRM_MODE_CONNECTOR_UNKNOWN: u32 = 0;
pub const DRM_MODE_CONNECTOR_VGA: u32 = 1;
pub const DRM_MODE_CONNECTOR_DVII: u32 = 2;
pub const DRM_MODE_CONNECTOR_DVID: u32 = 3;
pub const DRM_MODE_CONNECTOR_DVIA: u32 = 4;
pub const DRM_MODE_CONNECTOR_COMPOSITE: u32 = 5;
pub const DRM_MODE_CONNECTOR_SVIDEO: u32 = 6;
pub const DRM_MODE_CONNECTOR_LVDS: u32 = 7;
pub const DRM_MODE_CONNECTOR_COMPONENT: u32 = 8;
pub const DRM_MODE_CONNECTOR_9PIN_DIN: u32 = 9;
pub const DRM_MODE_CONNECTOR_DISPLAYPORT: u32 = 10;
pub const DRM_MODE_CONNECTOR_HDMIA: u32 = 11;
pub const DRM_MODE_CONNECTOR_HDMIB: u32 = 12;
pub const DRM_MODE_CONNECTOR_TV: u32 = 13;
pub const DRM_MODE_CONNECTOR_EDP: u32 = 14;
pub const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
pub const DRM_MODE_CONNECTOR_DSI: u32 = 16;
pub const DRM_MODE_CONNECTOR_DPI: u32 = 17;
pub const DRM_MODE_CONNECTOR_WRITEBACK: u32 = 18;
pub const DRM_MODE_CONNECTOR_SPI: u32 = 19;
pub const DRM_MODE_CONNECTOR_USB: u32 = 20;

// Connector status (`enum drm_connector_status`, as `connection` reports).
pub const DRM_MODE_CONNECTED: u32 = 1;
pub const DRM_MODE_DISCONNECTED: u32 = 2;
pub const DRM_MODE_UNKNOWNCONNECTION: u32 = 3;

// Property flags.
pub const DRM_MODE_PROP_PENDING: u32 = 1 << 0;
pub const DRM_MODE_PROP_RANGE: u32 = 1 << 1;
pub const DRM_MODE_PROP_IMMUTABLE: u32 = 1 << 2;
pub const DRM_MODE_PROP_ENUM: u32 = 1 << 3;
pub const DRM_MODE_PROP_BLOB: u32 = 1 << 4;
pub const DRM_MODE_PROP_BITMASK: u32 = 1 << 5;
pub const DRM_MODE_PROP_LEGACY_TYPE: u32 =
    DRM_MODE_PROP_RANGE | DRM_MODE_PROP_ENUM | DRM_MODE_PROP_BLOB | DRM_MODE_PROP_BITMASK;
pub const DRM_MODE_PROP_EXTENDED_TYPE: u32 = 0x0000_ffc0;
pub const DRM_MODE_PROP_OBJECT: u32 = 1 << 6;
pub const DRM_MODE_PROP_SIGNED_RANGE: u32 = 2 << 6;
pub const DRM_MODE_PROP_ATOMIC: u32 = 0x8000_0000;

// Object types.
pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub const DRM_MODE_OBJECT_ENCODER: u32 = 0xe0e0_e0e0;
pub const DRM_MODE_OBJECT_MODE: u32 = 0xdede_dede;
pub const DRM_MODE_OBJECT_PROPERTY: u32 = 0xb0b0_b0b0;
pub const DRM_MODE_OBJECT_FB: u32 = 0xfbfb_fbfb;
pub const DRM_MODE_OBJECT_BLOB: u32 = 0xbbbb_bbbb;
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
pub const DRM_MODE_OBJECT_COLOROP: u32 = 0xfafa_fafa;
pub const DRM_MODE_OBJECT_ANY: u32 = 0;

pub const DRM_MODE_FB_INTERLACED: u32 = 1 << 0;
pub const DRM_MODE_FB_MODIFIERS: u32 = 1 << 1;
pub const DRM_MODE_FB_DIRTY_ANNOTATE_COPY: u32 = 0x01;
pub const DRM_MODE_FB_DIRTY_ANNOTATE_FILL: u32 = 0x02;
pub const DRM_MODE_FB_DIRTY_FLAGS: u32 = 0x03;
pub const DRM_MODE_FB_DIRTY_MAX_CLIPS: u32 = 256;
pub const DRM_MODE_CURSOR_BO: u32 = 0x01;
pub const DRM_MODE_CURSOR_MOVE: u32 = 0x02;
pub const DRM_MODE_CURSOR_FLAGS: u32 = 0x03;

pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub const DRM_MODE_PAGE_FLIP_ASYNC: u32 = 0x02;
pub const DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE: u32 = 0x4;
pub const DRM_MODE_PAGE_FLIP_TARGET_RELATIVE: u32 = 0x8;
pub const DRM_MODE_PAGE_FLIP_TARGET: u32 =
    DRM_MODE_PAGE_FLIP_TARGET_ABSOLUTE | DRM_MODE_PAGE_FLIP_TARGET_RELATIVE;
pub const DRM_MODE_PAGE_FLIP_FLAGS: u32 =
    DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_PAGE_FLIP_ASYNC | DRM_MODE_PAGE_FLIP_TARGET;

pub const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
pub const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
pub const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;
pub const DRM_MODE_ATOMIC_FLAGS: u32 = DRM_MODE_PAGE_FLIP_EVENT
    | DRM_MODE_PAGE_FLIP_ASYNC
    | DRM_MODE_ATOMIC_TEST_ONLY
    | DRM_MODE_ATOMIC_NONBLOCK
    | DRM_MODE_ATOMIC_ALLOW_MODESET;

pub const DRM_MODE_ROTATE_0: u64 = 1 << 0;
pub const DRM_MODE_ROTATE_90: u64 = 1 << 1;
pub const DRM_MODE_ROTATE_180: u64 = 1 << 2;
pub const DRM_MODE_ROTATE_270: u64 = 1 << 3;
pub const DRM_MODE_REFLECT_X: u64 = 1 << 4;
pub const DRM_MODE_REFLECT_Y: u64 = 1 << 5;

/// `drm_mode_atomic`'s `FORMAT_BLOB_CURRENT`.
pub const FORMAT_BLOB_CURRENT: u32 = 1;

pub const DRM_IOCTL_MODE_GETRESOURCES: u32 = iowr(0xA0, size_of::<DrmModeCardRes>());
pub const DRM_IOCTL_MODE_GETCRTC: u32 = iowr(0xA1, size_of::<DrmModeCrtc>());
pub const DRM_IOCTL_MODE_SETCRTC: u32 = iowr(0xA2, size_of::<DrmModeCrtc>());
pub const DRM_IOCTL_MODE_CURSOR: u32 = iowr(0xA3, size_of::<DrmModeCursor>());
pub const DRM_IOCTL_MODE_GETGAMMA: u32 = iowr(0xA4, size_of::<DrmModeCrtcLut>());
pub const DRM_IOCTL_MODE_SETGAMMA: u32 = iowr(0xA5, size_of::<DrmModeCrtcLut>());
pub const DRM_IOCTL_MODE_GETENCODER: u32 = iowr(0xA6, size_of::<DrmModeGetEncoder>());
pub const DRM_IOCTL_MODE_GETCONNECTOR: u32 = iowr(0xA7, size_of::<DrmModeGetConnector>());
pub const DRM_IOCTL_MODE_GETPROPERTY: u32 = iowr(0xAA, size_of::<DrmModeGetProperty>());
pub const DRM_IOCTL_MODE_SETPROPERTY: u32 = iowr(0xAB, size_of::<DrmModeConnectorSetProperty>());
pub const DRM_IOCTL_MODE_GETPROPBLOB: u32 = iowr(0xAC, size_of::<DrmModeGetBlob>());
pub const DRM_IOCTL_MODE_GETFB: u32 = iowr(0xAD, size_of::<DrmModeFbCmd>());
pub const DRM_IOCTL_MODE_ADDFB: u32 = iowr(0xAE, size_of::<DrmModeFbCmd>());
pub const DRM_IOCTL_MODE_RMFB: u32 = iowr(0xAF, size_of::<u32>());
pub const DRM_IOCTL_MODE_PAGE_FLIP: u32 = iowr(0xB0, size_of::<DrmModeCrtcPageFlip>());
pub const DRM_IOCTL_MODE_DIRTYFB: u32 = iowr(0xB1, size_of::<DrmModeFbDirtyCmd>());
pub const DRM_IOCTL_MODE_CREATE_DUMB: u32 = iowr(0xB2, size_of::<DrmModeCreateDumb>());
pub const DRM_IOCTL_MODE_MAP_DUMB: u32 = iowr(0xB3, size_of::<DrmModeMapDumb>());
pub const DRM_IOCTL_MODE_DESTROY_DUMB: u32 = iowr(0xB4, size_of::<DrmModeDestroyDumb>());
pub const DRM_IOCTL_MODE_GETPLANERESOURCES: u32 = iowr(0xB5, size_of::<DrmModeGetPlaneRes>());
pub const DRM_IOCTL_MODE_GETPLANE: u32 = iowr(0xB6, size_of::<DrmModeGetPlane>());
pub const DRM_IOCTL_MODE_SETPLANE: u32 = iowr(0xB7, size_of::<DrmModeSetPlane>());
pub const DRM_IOCTL_MODE_ADDFB2: u32 = iowr(0xB8, size_of::<DrmModeFbCmd2>());
pub const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: u32 = iowr(0xB9, size_of::<DrmModeObjGetProperties>());
pub const DRM_IOCTL_MODE_OBJ_SETPROPERTY: u32 = iowr(0xBA, size_of::<DrmModeObjSetProperty>());
pub const DRM_IOCTL_MODE_CURSOR2: u32 = iowr(0xBB, size_of::<DrmModeCursor2>());
pub const DRM_IOCTL_MODE_ATOMIC: u32 = iowr(0xBC, size_of::<DrmModeAtomic>());
pub const DRM_IOCTL_MODE_CREATEPROPBLOB: u32 = iowr(0xBD, size_of::<DrmModeCreateBlob>());
pub const DRM_IOCTL_MODE_DESTROYPROPBLOB: u32 = iowr(0xBE, size_of::<DrmModeDestroyBlob>());
pub const DRM_IOCTL_MODE_CREATE_LEASE: u32 = iowr(0xC6, size_of::<DrmModeCreateLease>());
pub const DRM_IOCTL_MODE_LIST_LESSEES: u32 = iowr(0xC7, size_of::<DrmModeListLessees>());
pub const DRM_IOCTL_MODE_GET_LEASE: u32 = iowr(0xC8, size_of::<DrmModeGetLease>());
pub const DRM_IOCTL_MODE_REVOKE_LEASE: u32 = iowr(0xC9, size_of::<DrmModeRevokeLease>());
pub const DRM_IOCTL_MODE_GETFB2: u32 = iowr(0xCE, size_of::<DrmModeFbCmd2>());
pub const DRM_IOCTL_MODE_CLOSEFB: u32 = iowr(0xD0, size_of::<DrmModeClosefb>());

// ---- field codecs ----

/// A value at a fixed place in a structure's bytes, in native order.
pub trait WireField: Sized {
    /// Its size in bytes.
    const SIZE: usize;
    /// Reads it from the start of `b` (at least `SIZE` bytes).
    fn get(b: &[u8]) -> Self;
    /// Writes it to the start of `b`.
    fn put(&self, b: &mut [u8]);
}

macro_rules! wire_prim {
    ($($t:ty),*) => {$(
        impl WireField for $t {
            const SIZE: usize = size_of::<$t>();
            fn get(b: &[u8]) -> Self {
                let mut x = [0u8; size_of::<$t>()];
                x.copy_from_slice(&b[..Self::SIZE]);
                Self::from_ne_bytes(x)
            }
            fn put(&self, b: &mut [u8]) {
                b[..Self::SIZE].copy_from_slice(&self.to_ne_bytes());
            }
        }
    )*};
}
wire_prim!(u8, u16, u32, u64, i32, i64);

impl<T: WireField, const N: usize> WireField for [T; N] {
    const SIZE: usize = N * T::SIZE;
    fn get(b: &[u8]) -> Self {
        core::array::from_fn(|i| T::get(&b[i * T::SIZE..]))
    }
    fn put(&self, b: &mut [u8]) {
        for (i, v) in self.iter().enumerate() {
            v.put(&mut b[i * T::SIZE..]);
        }
    }
}

/// A structure that crosses the user boundary: decoded from and encoded
/// to its C layout field by field, at the offsets `offset_of!` gives, so
/// no padding is read and no `unsafe` is needed.
pub trait Wire: Sized {
    /// `sizeof` the C structure.
    const SIZE: usize;
    /// Decodes it from `b`, which must hold `SIZE` bytes.
    fn decode(b: &[u8]) -> Self;
    /// Encodes it into `b`, which must hold `SIZE` bytes; padding is left
    /// as it was.
    fn encode(&self, b: &mut [u8]);
}

macro_rules! wire {
    ($name:ident { $($f:ident: $t:ty),* $(,)? }) => {
        impl Wire for $name {
            const SIZE: usize = size_of::<$name>();
            fn decode(b: &[u8]) -> Self {
                Self { $($f: <$t as WireField>::get(&b[core::mem::offset_of!($name, $f)..])),* }
            }
            fn encode(&self, b: &mut [u8]) {
                $(WireField::put(&self.$f, &mut b[core::mem::offset_of!($name, $f)..]);)*
            }
        }
        impl WireField for $name {
            const SIZE: usize = size_of::<$name>();
            fn get(b: &[u8]) -> Self {
                <Self as Wire>::decode(b)
            }
            fn put(&self, b: &mut [u8]) {
                <Self as Wire>::encode(self, b);
            }
        }
    };
}

wire!(DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: u64,
    name: u64,
    date_len: u64,
    date: u64,
    desc_len: u64,
    desc: u64
});
wire!(DrmUnique {
    unique_len: u64,
    unique: u64
});
wire!(DrmAuth { magic: u32 });
wire!(DrmGemClose {
    handle: u32,
    pad: u32
});
wire!(DrmGemFlink {
    handle: u32,
    name: u32
});
wire!(DrmGemOpen {
    name: u32,
    handle: u32,
    size: u64
});
wire!(DrmGetCap {
    capability: u64,
    value: u64
});
wire!(DrmSetClientCap {
    capability: u64,
    value: u64
});
wire!(DrmSetClientName {
    name_len: u64,
    name: u64
});
wire!(DrmModesetCtl {
    crtc: u32,
    cmd: u32
});
wire!(DrmPrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32
});
wire!(DrmWaitVblankRequest {
    kind: u32,
    sequence: u32,
    signal: u64
});
wire!(DrmWaitVblankReply {
    kind: u32,
    sequence: u32,
    tval_sec: i64,
    tval_usec: i64
});
wire!(DrmWaitVblank {
    kind: u32,
    sequence: u32,
    word0: u64,
    word1: u64
});
wire!(DrmCrtcGetSequence {
    crtc_id: u32,
    active: u32,
    sequence: u64,
    sequence_ns: i64
});
wire!(DrmCrtcQueueSequence {
    crtc_id: u32,
    flags: u32,
    sequence: u64,
    user_data: u64
});
wire!(DrmEvent {
    kind: u32,
    length: u32
});
wire!(DrmEventVblank {
    base: DrmEvent,
    user_data: u64,
    tv_sec: u32,
    tv_usec: u32,
    sequence: u32,
    crtc_id: u32
});
wire!(DrmEventCrtcSequence {
    base: DrmEvent,
    user_data: u64,
    time_ns: i64,
    sequence: u64
});
wire!(DrmModeModeinfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    kind: u32,
    name: [u8; DRM_DISPLAY_MODE_LEN]
});
wire!(DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32
});
wire!(DrmModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: DrmModeModeinfo
});
wire!(DrmModeSetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    crtc_x: i32,
    crtc_y: i32,
    crtc_w: u32,
    crtc_h: u32,
    src_x: u32,
    src_y: u32,
    src_h: u32,
    src_w: u32
});
wire!(DrmModeGetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    possible_crtcs: u32,
    gamma_size: u32,
    count_format_types: u32,
    format_type_ptr: u64
});
wire!(DrmModeGetPlaneRes {
    plane_id_ptr: u64,
    count_planes: u32
});
wire!(DrmModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32
});
wire!(DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32
});
wire!(DrmModePropertyEnum {
    value: u64,
    name: [u8; DRM_PROP_NAME_LEN]
});
wire!(DrmModeGetProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; DRM_PROP_NAME_LEN],
    count_values: u32,
    count_enum_blobs: u32
});
wire!(DrmModeConnectorSetProperty {
    value: u64,
    prop_id: u32,
    connector_id: u32
});
wire!(DrmModeObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32
});
wire!(DrmModeObjSetProperty {
    value: u64,
    prop_id: u32,
    obj_id: u32,
    obj_type: u32
});
wire!(DrmModeGetBlob {
    blob_id: u32,
    length: u32,
    data: u64
});
wire!(DrmModeFbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32
});
wire!(DrmModeFbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4]
});
wire!(DrmModeFbDirtyCmd {
    fb_id: u32,
    flags: u32,
    color: u32,
    num_clips: u32,
    clips_ptr: u64
});
wire!(DrmModeCursor {
    flags: u32,
    crtc_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    handle: u32
});
wire!(DrmModeCursor2 {
    flags: u32,
    crtc_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    handle: u32,
    hot_x: i32,
    hot_y: i32
});
wire!(DrmModeCrtcLut {
    crtc_id: u32,
    gamma_size: u32,
    red: u64,
    green: u64,
    blue: u64
});
wire!(DrmColorLut {
    red: u16,
    green: u16,
    blue: u16,
    reserved: u16
});
wire!(DrmColorCtm { matrix: [u64; 9] });
wire!(DrmModeCrtcPageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64
});
wire!(DrmModeCrtcPageFlipTarget {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    sequence: u32,
    user_data: u64
});
wire!(DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64
});
wire!(DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64
});
wire!(DrmModeDestroyDumb { handle: u32 });
wire!(DrmModeAtomic {
    flags: u32,
    count_objs: u32,
    objs_ptr: u64,
    count_props_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    reserved: u64,
    user_data: u64
});
wire!(DrmFormatModifierBlob {
    version: u32,
    flags: u32,
    count_formats: u32,
    formats_offset: u32,
    count_modifiers: u32,
    modifiers_offset: u32
});
wire!(DrmFormatModifier {
    formats: u64,
    offset: u32,
    pad: u32,
    modifier: u64
});
wire!(DrmModeCreateBlob {
    data: u64,
    length: u32,
    blob_id: u32
});
wire!(DrmModeDestroyBlob { blob_id: u32 });
wire!(DrmModeCreateLease {
    object_ids: u64,
    object_count: u32,
    flags: u32,
    lessee_id: u32,
    fd: u32
});
wire!(DrmModeListLessees {
    count_lessees: u32,
    pad: u32,
    lessees_ptr: u64
});
wire!(DrmModeGetLease {
    count_objects: u32,
    pad: u32,
    objects_ptr: u64
});
wire!(DrmModeRevokeLease { lessee_id: u32 });
wire!(DrmModeRect {
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32
});
wire!(DrmModeClosefb {
    fb_id: u32,
    pad: u32
});

// Layouts, from compiling `drm.h` and `drm_mode.h` for LP64 with a C
// compiler. `abi-check` compares against FreeBSD's base headers, and
// these are libdrm's (a port), so each size is pinned here instead.
const _: () = {
    assert!(size_of::<DrmVersion>() == 64);
    assert!(size_of::<DrmUnique>() == 16);
    assert!(size_of::<DrmAuth>() == 4);
    assert!(size_of::<DrmGemClose>() == 8);
    assert!(size_of::<DrmGemFlink>() == 8);
    assert!(size_of::<DrmGemOpen>() == 16);
    assert!(size_of::<DrmGetCap>() == 16);
    assert!(size_of::<DrmSetClientCap>() == 16);
    assert!(size_of::<DrmSetClientName>() == 16);
    assert!(size_of::<DrmModesetCtl>() == 8);
    assert!(size_of::<DrmPrimeHandle>() == 12);
    assert!(size_of::<DrmWaitVblank>() == 24);
    assert!(size_of::<DrmCrtcGetSequence>() == 24);
    assert!(size_of::<DrmCrtcQueueSequence>() == 24);
    assert!(size_of::<DrmEvent>() == 8);
    assert!(size_of::<DrmEventVblank>() == 32);
    assert!(size_of::<DrmEventCrtcSequence>() == 32);
    assert!(size_of::<DrmModeModeinfo>() == 68);
    assert!(size_of::<DrmModeCardRes>() == 64);
    assert!(size_of::<DrmModeCrtc>() == 104);
    assert!(size_of::<DrmModeSetPlane>() == 48);
    assert!(size_of::<DrmModeGetPlane>() == 32);
    assert!(size_of::<DrmModeGetPlaneRes>() == 16);
    assert!(size_of::<DrmModeGetEncoder>() == 20);
    assert!(size_of::<DrmModeGetConnector>() == 80);
    assert!(size_of::<DrmModePropertyEnum>() == 40);
    assert!(size_of::<DrmModeGetProperty>() == 64);
    assert!(size_of::<DrmModeConnectorSetProperty>() == 16);
    assert!(size_of::<DrmModeObjGetProperties>() == 32);
    assert!(size_of::<DrmModeObjSetProperty>() == 24);
    assert!(size_of::<DrmModeGetBlob>() == 16);
    assert!(size_of::<DrmModeFbCmd>() == 28);
    assert!(size_of::<DrmModeFbCmd2>() == 104);
    assert!(size_of::<DrmModeFbDirtyCmd>() == 24);
    assert!(size_of::<DrmModeCursor>() == 28);
    assert!(size_of::<DrmModeCursor2>() == 36);
    assert!(size_of::<DrmModeCrtcLut>() == 32);
    assert!(size_of::<DrmModeCrtcPageFlip>() == 24);
    assert!(size_of::<DrmModeCrtcPageFlipTarget>() == 24);
    assert!(size_of::<DrmModeCreateDumb>() == 32);
    assert!(size_of::<DrmModeMapDumb>() == 16);
    assert!(size_of::<DrmModeDestroyDumb>() == 4);
    assert!(size_of::<DrmModeAtomic>() == 56);
    assert!(size_of::<DrmFormatModifierBlob>() == 24);
    assert!(size_of::<DrmFormatModifier>() == 24);
    assert!(size_of::<DrmModeCreateBlob>() == 16);
    assert!(size_of::<DrmModeDestroyBlob>() == 4);
    assert!(size_of::<DrmModeCreateLease>() == 24);
    assert!(size_of::<DrmModeListLessees>() == 16);
    assert!(size_of::<DrmModeGetLease>() == 16);
    assert!(size_of::<DrmModeRevokeLease>() == 4);
    assert!(size_of::<DrmModeRect>() == 16);
    assert!(size_of::<DrmModeClosefb>() == 8);
    assert!(size_of::<DrmColorLut>() == 8);
    assert!(size_of::<DrmColorCtm>() == 72);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_use_freebsd_encoding() {
        // _IOWR('d', 0, struct drm_version): IN|OUT, 64 bytes.
        assert_eq!(DRM_IOCTL_VERSION, 0xc040_6400);
        // _IO('d', 0x1e)
        assert_eq!(DRM_IOCTL_SET_MASTER, 0x2000_641e);
        // _IOW('d', 0x09, struct drm_gem_close)
        assert_eq!(DRM_IOCTL_GEM_CLOSE, 0x8008_6409);
        // _IOWR('d', 0xBC, struct drm_mode_atomic)
        assert_eq!(DRM_IOCTL_MODE_ATOMIC, 0xc038_64bc);
        assert!(is_drm_ioctl(DRM_IOCTL_MODE_CREATE_DUMB));
        assert_eq!(ioctl_nr(DRM_IOCTL_MODE_CREATE_DUMB), 0xb2);
    }

    #[test]
    fn wire_round_trips_and_skips_padding() {
        // `drm_version` has 4 bytes of padding after its third int.
        let v = DrmVersion {
            version_major: 1,
            version_minor: 2,
            version_patchlevel: 3,
            name_len: 4,
            name: 0x1000,
            date_len: 5,
            date: 0x2000,
            desc_len: 6,
            desc: 0x3000,
        };
        let mut b = [0xeeu8; 64];
        v.encode(&mut b);
        assert_eq!(&b[12..16], &[0xee; 4]);
        assert_eq!(DrmVersion::decode(&b), v);

        // A nested structure and a name array.
        let mut c = DrmModeCrtc {
            crtc_id: 7,
            mode_valid: 1,
            ..Default::default()
        };
        c.mode.hdisplay = 1920;
        c.mode.vrefresh = 60;
        c.mode.name[..4].copy_from_slice(b"1080");
        let mut b = [0u8; 104];
        c.encode(&mut b);
        assert_eq!(DrmModeCrtc::decode(&b), c);
        // `mode` starts after seven 32-bit fields and the 64-bit pointer.
        assert_eq!(&b[36 + 4..36 + 6], &1920u16.to_ne_bytes());
    }

    #[test]
    fn event_bytes() {
        let e = DrmEventVblank {
            base: DrmEvent {
                kind: DRM_EVENT_FLIP_COMPLETE,
                length: 32,
            },
            user_data: 0x1122_3344_5566_7788,
            tv_sec: 1,
            tv_usec: 2,
            sequence: 3,
            crtc_id: 4,
        };
        let b = e.to_bytes();
        assert_eq!(&b[0..4], &2u32.to_ne_bytes());
        assert_eq!(&b[8..16], &0x1122_3344_5566_7788u64.to_ne_bytes());
        assert_eq!(&b[28..32], &4u32.to_ne_bytes());
    }
}
