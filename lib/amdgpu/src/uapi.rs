// SPDX-License-Identifier: BSD-2-Clause
//! The `amdgpu` render interface's argument structures (specification 09;
//! the public, MIT-licensed `amdgpu_drm.h`): decoders for the requests
//! userspace sends and encoders for the answers, over plain byte slices
//! (little-endian, the C layout).
//!
//! The offsets are those a C compiler gives the header's structures; the
//! tests pin them. Every field of a request is untrusted input: a short
//! argument is an error, never a panic.

use alloc::vec::Vec;

/// Why an argument did not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shorter than its structure.
    Short,
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, Error> {
    let w = b.get(at..at + 4).ok_or(Error::Short)?;
    Ok(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64, Error> {
    let w = b.get(at..at + 8).ok_or(Error::Short)?;
    Ok(u64::from_le_bytes([
        w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7],
    ]))
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    if let Some(w) = b.get_mut(at..at + 4) {
        w.copy_from_slice(&v.to_le_bytes());
    }
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    if let Some(w) = b.get_mut(at..at + 8) {
        w.copy_from_slice(&v.to_le_bytes());
    }
}

/// The driver commands (`DRM_AMDGPU_*`, from `DRM_COMMAND_BASE`).
pub mod cmd {
    pub const GEM_CREATE: u8 = 0x00;
    pub const GEM_MMAP: u8 = 0x01;
    pub const CTX: u8 = 0x02;
    pub const BO_LIST: u8 = 0x03;
    pub const CS: u8 = 0x04;
    pub const INFO: u8 = 0x05;
    pub const GEM_METADATA: u8 = 0x06;
    pub const GEM_WAIT_IDLE: u8 = 0x07;
    pub const GEM_VA: u8 = 0x08;
    pub const WAIT_CS: u8 = 0x09;
    pub const GEM_OP: u8 = 0x10;
    pub const GEM_USERPTR: u8 = 0x11;
    pub const WAIT_FENCES: u8 = 0x12;
    pub const VM: u8 = 0x13;
    pub const FENCE_TO_HANDLE: u8 = 0x14;
}

/// Memory domains (`AMDGPU_GEM_DOMAIN_*`).
pub mod domain {
    pub const CPU: u64 = 0x1;
    pub const GTT: u64 = 0x2;
    pub const VRAM: u64 = 0x4;
}

/// Hardware IP types (`AMDGPU_HW_IP_*`).
pub mod ip {
    pub const GFX: u32 = 0;
    pub const COMPUTE: u32 = 1;
    pub const DMA: u32 = 2;
}

/// `DRM_AMDGPU_GEM_CREATE`'s request (32 bytes), answered with a handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GemCreate {
    pub size: u64,
    pub alignment: u64,
    pub domains: u64,
    pub flags: u64,
}

impl GemCreate {
    /// Decodes the request.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 32 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            size: u64_at(b, 0)?,
            alignment: u64_at(b, 8)?,
            domains: u64_at(b, 16)?,
            flags: u64_at(b, 24)?,
        })
    }

    /// Writes the answer over the argument: the handle in the first dword.
    pub fn answer(b: &mut [u8], handle: u32) {
        b.iter_mut().for_each(|x| *x = 0);
        put32(b, 0, handle);
    }
}

/// `DRM_AMDGPU_GEM_MMAP`: the handle in, the fake file offset out.
pub fn gem_mmap_handle(b: &[u8]) -> Result<u32, Error> {
    u32_at(b, 0)
}

/// Writes the mapping offset answer (`addr_ptr`).
pub fn gem_mmap_answer(b: &mut [u8], offset: u64) {
    put64(b, 0, offset);
}

/// `DRM_AMDGPU_CTX` operations.
pub mod ctx_op {
    pub const ALLOC: u32 = 1;
    pub const FREE: u32 = 2;
    pub const QUERY: u32 = 3;
    pub const QUERY2: u32 = 7;
}

/// `DRM_AMDGPU_CTX` (16 bytes in and out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ctx {
    pub op: u32,
    pub flags: u32,
    pub id: u32,
    pub priority: u32,
}

impl Ctx {
    /// Decodes the request.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 16 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            op: u32_at(b, 0)?,
            flags: u32_at(b, 4)?,
            id: u32_at(b, 8)?,
            priority: u32_at(b, 12)?,
        })
    }

    /// The answer: the context id in the first dword, the rest zero.
    pub fn answer(b: &mut [u8], id: u32, flags: u32) {
        b.iter_mut().for_each(|x| *x = 0);
        put32(b, 0, id);
        put32(b, 4, flags);
    }
}

/// `DRM_AMDGPU_BO_LIST` operations.
pub mod bo_list_op {
    pub const CREATE: u32 = 0;
    pub const DESTROY: u32 = 1;
    pub const UPDATE: u32 = 2;
}

/// `DRM_AMDGPU_BO_LIST` (24 bytes in, 8 out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoListRequest {
    pub operation: u32,
    pub list_handle: u32,
    pub count: u32,
    /// The size of one entry the caller uses (at least 8).
    pub entry_size: u32,
    /// The user address of the entries.
    pub entries: u64,
}

impl BoListRequest {
    /// Decodes the request.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 24 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            operation: u32_at(b, 0)?,
            list_handle: u32_at(b, 4)?,
            count: u32_at(b, 8)?,
            entry_size: u32_at(b, 12)?,
            entries: u64_at(b, 16)?,
        })
    }

    /// The answer: the list's handle.
    pub fn answer(b: &mut [u8], handle: u32) {
        b.iter_mut().for_each(|x| *x = 0);
        put32(b, 0, handle);
    }
}

/// One entry of a buffer list: a buffer handle and its priority.
#[must_use]
pub fn bo_list_entry(b: &[u8], at: usize) -> Option<(u32, u32)> {
    Some((u32_at(b, at).ok()?, u32_at(b, at + 4).ok()?))
}

/// `DRM_AMDGPU_CS` (24 bytes in, 8 out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsRequest {
    pub ctx_id: u32,
    pub bo_list: u32,
    pub num_chunks: u32,
    pub flags: u32,
    /// The user address of the array of chunk-pointer values.
    pub chunks: u64,
}

impl CsRequest {
    /// Decodes the request.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 24 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            ctx_id: u32_at(b, 0)?,
            bo_list: u32_at(b, 4)?,
            num_chunks: u32_at(b, 8)?,
            flags: u32_at(b, 12)?,
            chunks: u64_at(b, 16)?,
        })
    }

    /// The answer: the submission's sequence number.
    pub fn answer(b: &mut [u8], handle: u64) {
        b.iter_mut().for_each(|x| *x = 0);
        put64(b, 0, handle);
    }
}

/// Chunk ids (`AMDGPU_CHUNK_ID_*`).
pub mod chunk {
    pub const IB: u32 = 0x01;
    pub const FENCE: u32 = 0x02;
    pub const DEPENDENCIES: u32 = 0x03;
    pub const SYNCOBJ_IN: u32 = 0x04;
    pub const SYNCOBJ_OUT: u32 = 0x05;
    pub const BO_HANDLES: u32 = 0x06;
    pub const SCHEDULED_DEPENDENCIES: u32 = 0x07;
    pub const SYNCOBJ_TIMELINE_WAIT: u32 = 0x08;
    pub const SYNCOBJ_TIMELINE_SIGNAL: u32 = 0x09;
    pub const CP_GFX_SHADOW: u32 = 0x0a;
}

/// A chunk header (16 bytes): its id, its length in dwords, and the user
/// address of its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub id: u32,
    pub length_dw: u32,
    pub data: u64,
}

impl Chunk {
    /// Decodes a chunk header.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 16 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            id: u32_at(b, 0)?,
            length_dw: u32_at(b, 4)?,
            data: u64_at(b, 8)?,
        })
    }
}

/// An indirect-buffer chunk (32 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IbChunk {
    pub flags: u32,
    /// The buffer's GPU virtual address.
    pub va_start: u64,
    pub ib_bytes: u32,
    pub ip_type: u32,
    pub ip_instance: u32,
    pub ring: u32,
}

impl IbChunk {
    /// Decodes it.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 32 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            flags: u32_at(b, 4)?,
            va_start: u64_at(b, 8)?,
            ib_bytes: u32_at(b, 16)?,
            ip_type: u32_at(b, 20)?,
            ip_instance: u32_at(b, 24)?,
            ring: u32_at(b, 28)?,
        })
    }
}

/// A dependency chunk entry (24 bytes): the submission to wait for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dependency {
    pub ip_type: u32,
    pub ip_instance: u32,
    pub ring: u32,
    pub ctx_id: u32,
    pub handle: u64,
}

impl Dependency {
    /// Decodes it.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 24 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            ip_type: u32_at(b, 0)?,
            ip_instance: u32_at(b, 4)?,
            ring: u32_at(b, 8)?,
            ctx_id: u32_at(b, 12)?,
            handle: u64_at(b, 16)?,
        })
    }
}

/// `DRM_AMDGPU_GEM_VA` operations.
pub mod va_op {
    pub const MAP: u32 = 1;
    pub const UNMAP: u32 = 2;
    pub const CLEAR: u32 = 3;
    pub const REPLACE: u32 = 4;
}

/// `DRM_AMDGPU_GEM_VA` (64 bytes, write only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GemVa {
    pub handle: u32,
    pub operation: u32,
    pub flags: u32,
    pub va_address: u64,
    pub offset_in_bo: u64,
    pub map_size: u64,
}

impl GemVa {
    /// Decodes the request.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 40 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            handle: u32_at(b, 0)?,
            operation: u32_at(b, 8)?,
            flags: u32_at(b, 12)?,
            va_address: u64_at(b, 16)?,
            offset_in_bo: u64_at(b, 24)?,
            map_size: u64_at(b, 32)?,
        })
    }
}

/// `DRM_AMDGPU_WAIT_CS` (32 bytes in, 8 out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitCs {
    pub handle: u64,
    pub timeout_ns: u64,
    pub ip_type: u32,
    pub ip_instance: u32,
    pub ring: u32,
    pub ctx_id: u32,
}

impl WaitCs {
    /// Decodes the request.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 32 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            handle: u64_at(b, 0)?,
            timeout_ns: u64_at(b, 8)?,
            ip_type: u32_at(b, 16)?,
            ip_instance: u32_at(b, 20)?,
            ring: u32_at(b, 24)?,
            ctx_id: u32_at(b, 28)?,
        })
    }

    /// The answer: 0 if the fence has signalled, 1 if the wait timed out.
    pub fn answer(b: &mut [u8], timed_out: bool) {
        b.iter_mut().for_each(|x| *x = 0);
        put64(b, 0, u64::from(timed_out));
    }
}

/// `DRM_AMDGPU_GEM_WAIT_IDLE` (16 bytes).
pub fn gem_wait_idle(b: &[u8]) -> Result<(u32, u64), Error> {
    Ok((u32_at(b, 0)?, u64_at(b, 8)?))
}

/// Writes the wait-idle answer: 0 idle, 1 busy.
pub fn gem_wait_idle_answer(b: &mut [u8], busy: bool) {
    b.iter_mut().for_each(|x| *x = 0);
    put64(b, 0, u64::from(busy));
}

/// `DRM_AMDGPU_INFO` (32 bytes, write only): where the answer goes, how
/// much room, which query, and the query's own arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InfoRequest {
    pub return_pointer: u64,
    pub return_size: u32,
    pub query: u32,
    /// The query's first argument dword (`type`, `id`, `dword_offset`,
    /// `fw_type`).
    pub arg0: u32,
    /// The second (`ip_instance`, `count`, `offset`).
    pub arg1: u32,
    /// The third and fourth (`instance`, `index`; `flags`).
    pub arg2: u32,
    pub arg3: u32,
}

impl InfoRequest {
    /// Decodes the request.
    ///
    /// # Errors
    ///
    /// [`Error::Short`] for fewer than 32 bytes.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            return_pointer: u64_at(b, 0)?,
            return_size: u32_at(b, 8)?,
            query: u32_at(b, 12)?,
            arg0: u32_at(b, 16)?,
            arg1: u32_at(b, 20)?,
            arg2: u32_at(b, 24)?,
            arg3: u32_at(b, 28)?,
        })
    }
}

/// The `INFO` query ids (09 section 3.1).
pub mod query {
    pub const ACCEL_WORKING: u32 = 0x00;
    pub const CRTC_FROM_ID: u32 = 0x01;
    pub const HW_IP_INFO: u32 = 0x02;
    pub const HW_IP_COUNT: u32 = 0x03;
    pub const TIMESTAMP: u32 = 0x05;
    pub const FW_VERSION: u32 = 0x0e;
    pub const NUM_BYTES_MOVED: u32 = 0x0f;
    pub const VRAM_USAGE: u32 = 0x10;
    pub const GTT_USAGE: u32 = 0x11;
    pub const GDS_CONFIG: u32 = 0x13;
    pub const VRAM_GTT: u32 = 0x14;
    pub const READ_MMR_REG: u32 = 0x15;
    pub const DEV_INFO: u32 = 0x16;
    pub const VIS_VRAM_USAGE: u32 = 0x17;
    pub const NUM_EVICTIONS: u32 = 0x18;
    pub const MEMORY: u32 = 0x19;
    pub const VBIOS: u32 = 0x1b;
    pub const SENSOR: u32 = 0x1d;
    pub const NUM_VRAM_CPU_PAGE_FAULTS: u32 = 0x1e;
    pub const VRAM_LOST_COUNTER: u32 = 0x1f;
    pub const MAX_IBS: u32 = 0x22;
}

/// The device description (`struct drm_amdgpu_info_device`, 440 bytes):
/// the fields a client reads; the rest is zero.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_id: u32,
    pub chip_rev: u32,
    pub external_rev: u32,
    pub pci_rev: u32,
    pub family: u32,
    pub num_shader_engines: u32,
    pub num_shader_arrays_per_engine: u32,
    /// In kHz.
    pub gpu_counter_freq: u32,
    pub max_engine_clock: u64,
    pub max_memory_clock: u64,
    pub cu_active_number: u32,
    pub cu_bitmap: [[u32; 4]; 4],
    pub enabled_rb_pipes_mask: u32,
    pub num_rb_pipes: u32,
    pub num_hw_gfx_contexts: u32,
    pub pcie_gen: u32,
    pub ids_flags: u64,
    pub virtual_address_offset: u64,
    pub virtual_address_max: u64,
    pub virtual_address_alignment: u32,
    pub pte_fragment_size: u32,
    pub gart_page_size: u32,
    pub ce_ram_size: u32,
    pub vram_type: u32,
    pub vram_bit_width: u32,
    pub wave_front_size: u32,
    pub num_shader_visible_vgprs: u32,
    pub num_cu_per_sh: u32,
    pub num_tcc_blocks: u32,
    pub gs_vgt_table_depth: u32,
    pub gs_prim_buffer_depth: u32,
    pub max_gs_waves_per_vgt: u32,
    pub pcie_num_lanes: u32,
    pub high_va_offset: u64,
    pub high_va_max: u64,
    pub pa_sc_tile_steering_override: u32,
    pub min_engine_clock: u64,
    pub min_memory_clock: u64,
}

/// The size of `struct drm_amdgpu_info_device`.
pub const DEVICE_INFO_SIZE: usize = 440;

impl DeviceInfo {
    /// The structure's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut b = alloc::vec![0u8; DEVICE_INFO_SIZE];
        put32(&mut b, 0, self.device_id);
        put32(&mut b, 4, self.chip_rev);
        put32(&mut b, 8, self.external_rev);
        put32(&mut b, 12, self.pci_rev);
        put32(&mut b, 16, self.family);
        put32(&mut b, 20, self.num_shader_engines);
        put32(&mut b, 24, self.num_shader_arrays_per_engine);
        put32(&mut b, 28, self.gpu_counter_freq);
        put64(&mut b, 32, self.max_engine_clock);
        put64(&mut b, 40, self.max_memory_clock);
        put32(&mut b, 48, self.cu_active_number);
        for (i, row) in self.cu_bitmap.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                put32(&mut b, 56 + 16 * i + 4 * j, *v);
            }
        }
        put32(&mut b, 120, self.enabled_rb_pipes_mask);
        put32(&mut b, 124, self.num_rb_pipes);
        put32(&mut b, 128, self.num_hw_gfx_contexts);
        put32(&mut b, 132, self.pcie_gen);
        put64(&mut b, 136, self.ids_flags);
        put64(&mut b, 144, self.virtual_address_offset);
        put64(&mut b, 152, self.virtual_address_max);
        put32(&mut b, 160, self.virtual_address_alignment);
        put32(&mut b, 164, self.pte_fragment_size);
        put32(&mut b, 168, self.gart_page_size);
        put32(&mut b, 172, self.ce_ram_size);
        put32(&mut b, 176, self.vram_type);
        put32(&mut b, 180, self.vram_bit_width);
        put32(&mut b, 240, self.wave_front_size);
        put32(&mut b, 244, self.num_shader_visible_vgprs);
        put32(&mut b, 248, self.num_cu_per_sh);
        put32(&mut b, 252, self.num_tcc_blocks);
        put32(&mut b, 256, self.gs_vgt_table_depth);
        put32(&mut b, 260, self.gs_prim_buffer_depth);
        put32(&mut b, 264, self.max_gs_waves_per_vgt);
        put32(&mut b, 268, self.pcie_num_lanes);
        put64(&mut b, 336, self.high_va_offset);
        put64(&mut b, 344, self.high_va_max);
        put32(&mut b, 352, self.pa_sc_tile_steering_override);
        put64(&mut b, 368, self.min_engine_clock);
        put64(&mut b, 376, self.min_memory_clock);
        b
    }
}

/// `struct drm_amdgpu_info_hw_ip` (40 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HwIpInfo {
    pub version_major: u32,
    pub version_minor: u32,
    pub capabilities: u64,
    pub ib_start_alignment: u32,
    pub ib_size_alignment: u32,
    pub available_rings: u32,
    pub ip_discovery_version: u32,
}

impl HwIpInfo {
    /// The structure's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut b = alloc::vec![0u8; 40];
        put32(&mut b, 0, self.version_major);
        put32(&mut b, 4, self.version_minor);
        put64(&mut b, 8, self.capabilities);
        put32(&mut b, 16, self.ib_start_alignment);
        put32(&mut b, 20, self.ib_size_alignment);
        put32(&mut b, 24, self.available_rings);
        put32(&mut b, 28, self.ip_discovery_version);
        b
    }
}

/// One heap of `struct drm_amdgpu_memory_info` (32 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Heap {
    pub total: u64,
    pub usable: u64,
    pub usage: u64,
    pub max_allocation: u64,
}

/// `struct drm_amdgpu_memory_info` (96 bytes): VRAM, CPU-visible VRAM, GTT.
#[must_use]
pub fn memory_info(vram: Heap, visible: Heap, gtt: Heap) -> Vec<u8> {
    let mut b = alloc::vec![0u8; 96];
    for (i, h) in [vram, visible, gtt].iter().enumerate() {
        put64(&mut b, 32 * i, h.total);
        put64(&mut b, 32 * i + 8, h.usable);
        put64(&mut b, 32 * i + 16, h.usage);
        put64(&mut b, 32 * i + 24, h.max_allocation);
    }
    b
}

/// `struct drm_amdgpu_info_vram_gtt` (24 bytes).
#[must_use]
pub fn vram_gtt(vram: u64, visible: u64, gtt: u64) -> Vec<u8> {
    let mut b = alloc::vec![0u8; 24];
    put64(&mut b, 0, vram);
    put64(&mut b, 8, visible);
    put64(&mut b, 16, gtt);
    b
}

/// `struct drm_amdgpu_info_firmware` (8 bytes).
#[must_use]
pub fn firmware(version: u32, feature: u32) -> Vec<u8> {
    let mut b = alloc::vec![0u8; 8];
    put32(&mut b, 0, version);
    put32(&mut b, 4, feature);
    b
}

/// The bytes of an answer limited to what the caller has room for, as the
/// interface requires (09 section 3: copy at most the size given).
#[must_use]
pub fn truncated(answer: &[u8], room: u32) -> &[u8] {
    let n = usize::try_from(room).map_or(answer.len(), |r| r.min(answer.len()));
    &answer[..n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    #[test]
    fn the_requests_decode_at_the_offsets_a_c_compiler_gives() {
        // Offsets from the header's structures (checked with a C compiler).
        let mut b = vec![0u8; 32];
        put64(&mut b, 0, 4096);
        put64(&mut b, 8, 256);
        put64(&mut b, 16, domain::GTT);
        put64(&mut b, 24, 7);
        assert_eq!(
            GemCreate::decode(&b),
            Ok(GemCreate {
                size: 4096,
                alignment: 256,
                domains: 2,
                flags: 7
            })
        );
        let mut b = vec![0u8; 16];
        put32(&mut b, 0, ctx_op::ALLOC);
        put32(&mut b, 8, 5);
        put32(&mut b, 12, 2);
        assert_eq!(
            Ctx::decode(&b),
            Ok(Ctx {
                op: 1,
                flags: 0,
                id: 5,
                priority: 2
            })
        );
        let mut b = vec![0u8; 24];
        put32(&mut b, 0, bo_list_op::CREATE);
        put32(&mut b, 8, 3);
        put32(&mut b, 12, 8);
        put64(&mut b, 16, 0x7fff_0000);
        assert_eq!(
            BoListRequest::decode(&b),
            Ok(BoListRequest {
                operation: 0,
                list_handle: 0,
                count: 3,
                entry_size: 8,
                entries: 0x7fff_0000
            })
        );
        let mut b = vec![0u8; 24];
        put32(&mut b, 0, 9);
        put32(&mut b, 4, 10);
        put32(&mut b, 8, 2);
        put64(&mut b, 16, 0x1234);
        assert_eq!(
            CsRequest::decode(&b),
            Ok(CsRequest {
                ctx_id: 9,
                bo_list: 10,
                num_chunks: 2,
                flags: 0,
                chunks: 0x1234
            })
        );
    }

    #[test]
    fn chunks_gem_va_wait_and_info_decode_at_their_offsets() {
        let mut b = vec![0u8; 16];
        put32(&mut b, 0, chunk::IB);
        put32(&mut b, 4, 8);
        put64(&mut b, 8, 0xdead_0000);
        assert_eq!(
            Chunk::decode(&b),
            Ok(Chunk {
                id: 1,
                length_dw: 8,
                data: 0xdead_0000
            })
        );
        let mut b = vec![0u8; 32];
        put64(&mut b, 8, 0x8000_0000);
        put32(&mut b, 16, 256);
        put32(&mut b, 20, ip::GFX);
        put32(&mut b, 28, 1);
        let ib = IbChunk::decode(&b).unwrap();
        assert_eq!(
            (ib.va_start, ib.ib_bytes, ib.ip_type, ib.ring),
            (0x8000_0000, 256, 0, 1)
        );
        let mut b = vec![0u8; 64];
        put32(&mut b, 0, 77);
        put32(&mut b, 8, va_op::MAP);
        put32(&mut b, 12, 0x7);
        put64(&mut b, 16, 0x10_0000);
        put64(&mut b, 24, 0);
        put64(&mut b, 32, 0x4000);
        let va = GemVa::decode(&b).unwrap();
        assert_eq!(
            (va.handle, va.operation, va.va_address, va.map_size),
            (77, 1, 0x10_0000, 0x4000)
        );
        let mut b = vec![0u8; 32];
        put64(&mut b, 0, 42);
        put64(&mut b, 8, 1_000_000);
        put32(&mut b, 28, 3);
        let w = WaitCs::decode(&b).unwrap();
        assert_eq!((w.handle, w.timeout_ns, w.ctx_id), (42, 1_000_000, 3));
        let mut b = vec![0u8; 32];
        put64(&mut b, 0, 0x5000);
        put32(&mut b, 8, 440);
        put32(&mut b, 12, query::DEV_INFO);
        put32(&mut b, 16, ip::DMA);
        let i = InfoRequest::decode(&b).unwrap();
        assert_eq!(
            (i.return_pointer, i.return_size, i.query, i.arg0),
            (0x5000, 440, 0x16, 2)
        );
        // Short arguments are errors.
        assert_eq!(GemCreate::decode(&bytes(31)), Err(Error::Short));
        assert_eq!(InfoRequest::decode(&bytes(31)), Err(Error::Short));
        assert_eq!(CsRequest::decode(&[]), Err(Error::Short));
    }

    #[test]
    fn the_device_info_is_440_bytes_with_fields_where_the_header_puts_them() {
        let d = DeviceInfo {
            device_id: 0x164e,
            family: 149,
            external_rev: 2,
            num_shader_engines: 1,
            gpu_counter_freq: 100_000,
            max_engine_clock: 2_200_000,
            cu_active_number: 2,
            cu_bitmap: [[3, 0, 0, 0], [0; 4], [0; 4], [0; 4]],
            enabled_rb_pipes_mask: 1,
            num_rb_pipes: 1,
            num_hw_gfx_contexts: 8,
            virtual_address_max: 0xffff_ffff_ffff,
            wave_front_size: 64,
            ..DeviceInfo::default()
        };
        let b = d.encode();
        assert_eq!(b.len(), 440);
        assert_eq!(u32_at(&b, 0), Ok(0x164e));
        assert_eq!(u32_at(&b, 8), Ok(2));
        assert_eq!(u32_at(&b, 16), Ok(149));
        assert_eq!(u64_at(&b, 32), Ok(2_200_000));
        assert_eq!(u32_at(&b, 48), Ok(2));
        assert_eq!(u32_at(&b, 56), Ok(3));
        assert_eq!(u32_at(&b, 120), Ok(1));
        assert_eq!(u32_at(&b, 128), Ok(8));
        assert_eq!(u64_at(&b, 152), Ok(0xffff_ffff_ffff));
        assert_eq!(u32_at(&b, 240), Ok(64));
        assert_eq!(HwIpInfo::default().encode().len(), 40);
        assert_eq!(
            memory_info(Heap::default(), Heap::default(), Heap::default()).len(),
            96
        );
        assert_eq!(vram_gtt(1, 2, 3)[16], 3);
        assert_eq!(firmware(0x1c, 38), [0x1c, 0, 0, 0, 38, 0, 0, 0]);
    }

    #[test]
    fn answers_replace_the_argument_and_the_reply_is_cut_to_the_room_given() {
        let mut b = bytes(32);
        GemCreate::answer(&mut b, 9);
        assert_eq!(&b[..8], [9, 0, 0, 0, 0, 0, 0, 0]);
        assert!(b[8..].iter().all(|x| *x == 0));
        let mut b = bytes(8);
        gem_mmap_answer(&mut b, 0x1_0000_0000);
        assert_eq!(u64_at(&b, 0), Ok(0x1_0000_0000));
        let mut b = bytes(32);
        WaitCs::answer(&mut b, true);
        assert_eq!(u64_at(&b, 0), Ok(1));
        // An answer is cut to the room the caller gave.
        let answer = bytes(40);
        assert_eq!(truncated(&answer, 8).len(), 8);
        assert_eq!(truncated(&answer, 400).len(), 40);
        assert_eq!(truncated(&answer, 0).len(), 0);
    }
}
