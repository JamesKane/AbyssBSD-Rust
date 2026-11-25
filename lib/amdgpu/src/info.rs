// SPDX-License-Identifier: BSD-2-Clause
//! The `INFO` queries a graphics client makes of the Raphael device
//! (specification 09 section 3): what each returns, with the values the
//! specification measured, over [`crate::uapi`]'s structures.
//!
//! The device's fixed description is [`Device::raphael`]; what changes while
//! it runs (memory in use, the clock, a register's value) comes in through
//! [`Live`].

use alloc::vec::Vec;

use crate::uapi::{self, DeviceInfo, Heap, HwIpInfo, InfoRequest, ip, query};

/// Why a query was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A query or argument the device does not answer (`EINVAL`).
    Invalid,
    /// A zero return size or null pointer.
    NoRoom,
}

/// What the device is, fixed.
#[derive(Clone, Debug)]
pub struct Device {
    pub info: DeviceInfo,
    /// The real video memory, in bytes.
    pub vram: u64,
    /// The visible part of it.
    pub vram_visible: u64,
    /// The system memory the GPU can map.
    pub gtt: u64,
    /// Firmware versions: (kind, index) to (version, feature).
    pub firmware: Vec<(u32, u32, u32, u32)>,
    /// The hardware IPs.
    pub ips: Vec<IpEntry>,
}

/// One hardware IP type (09 section 3.2).
#[derive(Clone, Copy, Debug)]
pub struct IpEntry {
    pub kind: u32,
    pub major: u32,
    pub minor: u32,
    pub discovery: u32,
    /// The mask of usable submission slots.
    pub rings: u32,
    pub ib_start_alignment: u32,
    pub ib_size_alignment: u32,
    /// How many instances.
    pub count: u32,
}

/// What changes while the device runs.
pub trait Live {
    /// Bytes of video memory in use.
    fn vram_usage(&self) -> u64;
    /// Bytes of system memory mapped for the GPU in use.
    fn gtt_usage(&self) -> u64;
    /// The GPU's free-running counter.
    fn timestamp(&self) -> u64;
    /// The value of a register on the allow-list (a dword index), or `None`.
    fn read_register(&self, index: u32) -> Option<u32>;
    /// The count of resets that lost video memory.
    fn vram_lost(&self) -> u32 {
        0
    }
}

/// The memory kept back for page tables (09 section 3.5).
pub const PAGE_TABLE_RESERVE: u64 = 8 << 20;

impl Device {
    /// The Radeon 610M (Raphael, GC 10.3.6) as the specification measured it
    /// on the X870E: 09 section 3, 06 sections 4 and 14.
    #[must_use]
    pub fn raphael(vram: u64, gtt: u64) -> Self {
        let info = DeviceInfo {
            device_id: 0x164e,
            chip_rev: 1,
            external_rev: 2,
            pci_rev: 0xc2,
            family: 149,
            num_shader_engines: 1,
            num_shader_arrays_per_engine: 1,
            gpu_counter_freq: 100_000,
            max_engine_clock: 2_200_000,
            max_memory_clock: 3_300_000,
            cu_active_number: 2,
            cu_bitmap: [[0x3, 0, 0, 0], [0; 4], [0; 4], [0; 4]],
            enabled_rb_pipes_mask: 1,
            num_rb_pipes: 1,
            num_hw_gfx_contexts: 8,
            pcie_gen: 4,
            // APU, and gang submit supported (must be set).
            ids_flags: 0x11,
            virtual_address_offset: 0x10000,
            virtual_address_max: 0x8000_0000_0000,
            virtual_address_alignment: 4096,
            pte_fragment_size: 2 << 20,
            gart_page_size: 4096,
            ce_ram_size: 65_536,
            vram_type: 10,
            vram_bit_width: 128,
            wave_front_size: 32,
            num_shader_visible_vgprs: 1024,
            num_cu_per_sh: 2,
            num_tcc_blocks: 2,
            gs_vgt_table_depth: 32,
            gs_prim_buffer_depth: 1792,
            max_gs_waves_per_vgt: 32,
            pcie_num_lanes: 16,
            high_va_offset: 0xffff_8000_0000_0000,
            high_va_max: 0xffff_ffff_ffbf_0000,
            pa_sc_tile_steering_override: 0,
            min_engine_clock: 400_000,
            min_memory_clock: 3_300_000,
        };
        Self {
            info,
            vram,
            vram_visible: vram,
            gtt,
            // The firmware the PSP loaded (02 section 11, 09 section 3.4):
            // (kind, index, version, feature).
            firmware: alloc::vec![
                (4, 0, 0xe, 38),
                (5, 0, 0x12, 38),
                (6, 0, 0x3, 38),
                (7, 0, 0x1f, 1),
                (8, 0, 0x1c, 38),
                (8, 1, 0x1c, 38),
                (11, 0, 0x9, 52),
                (13, 0, 0x2100_0119, 0),
                (15, 0, 1, 0),
                (16, 0, 1, 0),
                (17, 0, 1, 0),
                (20, 0, 0x0500_3700, 0),
                (21, 0, 0x7, 0),
            ],
            // GFX 10.0 / discovery 0x0a0306, one ring; compute 10.0, four
            // rings; DMA 5.2 / 0x050206, one ring (09 section 3.2).
            ips: alloc::vec![
                ip_entry(ip::GFX, 10, 0, 0x000a_0306, 0x1, 32, 32),
                ip_entry(ip::COMPUTE, 10, 0, 0x000a_0306, 0xf, 32, 32),
                ip_entry(ip::DMA, 5, 2, 0x0005_0206, 0x1, 256, 4),
            ],
        }
    }

    /// The device as a driver that runs the graphics ring and the DMA engine
    /// reports it: compute answers as absent.
    #[must_use]
    pub fn without_compute(mut self) -> Self {
        self.ips.retain(|i| i.kind != ip::COMPUTE);
        self
    }

    /// The device as a driver that runs only the graphics ring reports it:
    /// the compute and DMA engines answer as absent (no rings, count 0), so
    /// clients keep all their work on the graphics ring.
    #[must_use]
    pub fn graphics_only(mut self) -> Self {
        self.ips.retain(|i| i.kind == ip::GFX);
        self
    }

    fn heaps(&self, live: &dyn Live) -> (Heap, Heap, Heap) {
        let usable = self.vram.saturating_sub(PAGE_TABLE_RESERVE);
        let visible = self.vram_visible.min(usable);
        let vram = Heap {
            total: self.vram,
            usable,
            usage: live.vram_usage(),
            max_allocation: usable / 4 * 3,
        };
        let vis = Heap {
            total: self.vram_visible,
            usable: visible,
            usage: live.vram_usage().min(visible),
            max_allocation: visible / 4 * 3,
        };
        let gtt = Heap {
            total: self.gtt,
            usable: self.gtt,
            usage: live.gtt_usage(),
            max_allocation: self.gtt / 4 * 3,
        };
        (vram, vis, gtt)
    }
}

fn ip_entry(
    kind: u32,
    major: u32,
    minor: u32,
    discovery: u32,
    rings: u32,
    ib_start_alignment: u32,
    ib_size_alignment: u32,
) -> IpEntry {
    IpEntry {
        kind,
        major,
        minor,
        discovery,
        rings,
        ib_start_alignment,
        ib_size_alignment,
        count: 1,
    }
}

/// The registers `READ_MMR_REG` may read (dword indices on Raphael, 09
/// section 3.6).
pub const MMR_ALLOW: [u32; 18] = [
    0x2004, 0x2002, 0x2005, 0x2006, 0x200e, 0x200f, 0x1285, 0x21a0, 0x219d, 0x219e, 0x219c, 0x2087,
    0x2088, 0x2089, 0x2084, 0x2085, 0x2086, 0x263e,
];

/// The configured `GB_ADDR_CONFIG` the interface returns instead of a read.
pub const GB_ADDR_CONFIG_INDEX: u32 = 0x263e;
/// Its value on Raphael.
pub const GB_ADDR_CONFIG_VALUE: u32 = 0x42;

/// Answers a query: the bytes the caller gets, before they are cut to the
/// room it gave.
///
/// # Errors
///
/// [`Error::Invalid`] for a query or argument not answered;
/// [`Error::NoRoom`] for a return size of zero.
pub fn answer(dev: &Device, live: &dyn Live, req: &InfoRequest) -> Result<Vec<u8>, Error> {
    if req.return_size == 0 || req.return_pointer == 0 {
        return Err(Error::NoRoom);
    }
    let le32 = |v: u32| v.to_le_bytes().to_vec();
    let le64 = |v: u64| v.to_le_bytes().to_vec();
    Ok(match req.query {
        query::ACCEL_WORKING => le32(1),
        query::HW_IP_INFO => {
            // An engine type that does not exist answers all zeros.
            let e = dev
                .ips
                .iter()
                .copied()
                .find(|i| i.kind == req.arg0)
                .unwrap_or(IpEntry {
                    kind: req.arg0,
                    major: 0,
                    minor: 0,
                    discovery: 0,
                    rings: 0,
                    ib_start_alignment: 0,
                    ib_size_alignment: 0,
                    count: 0,
                });
            if req.arg1 != 0 {
                return Err(Error::Invalid);
            }
            HwIpInfo {
                version_major: e.major,
                version_minor: e.minor,
                capabilities: 0,
                ib_start_alignment: e.ib_start_alignment,
                ib_size_alignment: e.ib_size_alignment,
                available_rings: e.rings,
                ip_discovery_version: e.discovery,
            }
            .encode()
        }
        query::HW_IP_COUNT => {
            let count = match dev.ips.iter().find(|i| i.kind == req.arg0) {
                Some(i) => i.count,
                // An engine type that exists but is not there counts 0.
                None if req.arg0 <= 6 => 0,
                None => return Err(Error::Invalid),
            };
            le32(count)
        }
        query::TIMESTAMP => le64(live.timestamp()),
        query::FW_VERSION => {
            // fw_type, instance (must be 0), index.
            if req.arg1 != 0 {
                return Err(Error::Invalid);
            }
            let (version, feature) = dev
                .firmware
                .iter()
                .find(|f| f.0 == req.arg0 && f.1 == req.arg2)
                .map(|f| (f.2, f.3))
                .unwrap_or((0, 0));
            uapi::firmware(version, feature)
        }
        query::NUM_BYTES_MOVED | query::NUM_EVICTIONS | query::NUM_VRAM_CPU_PAGE_FAULTS => le64(0),
        query::VRAM_USAGE => le64(live.vram_usage()),
        query::VIS_VRAM_USAGE => le64(live.vram_usage().min(dev.vram_visible)),
        query::GTT_USAGE => le64(live.gtt_usage()),
        query::GDS_CONFIG => alloc::vec![0u8; 32],
        query::VRAM_GTT => {
            let (v, vis, g) = dev.heaps(live);
            uapi::vram_gtt(v.usable, vis.usable, g.usable)
        }
        query::MEMORY => {
            let (v, vis, g) = dev.heaps(live);
            uapi::memory_info(v, vis, g)
        }
        query::DEV_INFO => dev.info.encode(),
        query::VRAM_LOST_COUNTER => le32(live.vram_lost()),
        query::MAX_IBS => {
            // One dword per engine type: how many IBs a submission may hold.
            let mut b = alloc::vec![0u8; 4 * 9];
            for t in [ip::GFX, ip::COMPUTE, ip::DMA] {
                let at = 4 * t as usize;
                b[at..at + 4].copy_from_slice(&4u32.to_le_bytes());
            }
            b
        }
        query::READ_MMR_REG => {
            // dword_offset, count (at most 128), instance word, flags.
            let (index, count) = (req.arg0, req.arg1);
            if count == 0 || count > 128 {
                return Err(Error::Invalid);
            }
            let mut out = Vec::with_capacity(4 * count as usize);
            for i in 0..count {
                let r = index.checked_add(i).ok_or(Error::Invalid)?;
                if !MMR_ALLOW.contains(&r) {
                    return Err(Error::Invalid);
                }
                let v = if r == GB_ADDR_CONFIG_INDEX {
                    GB_ADDR_CONFIG_VALUE
                } else {
                    live.read_register(r).ok_or(Error::Invalid)?
                };
                out.extend_from_slice(&v.to_le_bytes());
            }
            out
        }
        _ => return Err(Error::Invalid),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl Live for Fake {
        fn vram_usage(&self) -> u64 {
            1 << 20
        }
        fn gtt_usage(&self) -> u64 {
            2 << 20
        }
        fn timestamp(&self) -> u64 {
            123_456
        }
        fn read_register(&self, index: u32) -> Option<u32> {
            Some(index ^ 0xff)
        }
    }

    fn dev() -> Device {
        Device::raphael(512 << 20, 47 << 30)
    }

    fn ask(q: u32, a0: u32, a1: u32, a2: u32, room: u32) -> Result<Vec<u8>, Error> {
        answer(
            &dev(),
            &Fake,
            &InfoRequest {
                return_pointer: 0x1000,
                return_size: room,
                query: q,
                arg0: a0,
                arg1: a1,
                arg2: a2,
                arg3: 0,
            },
        )
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }
    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from(u32_at(b, at)) | u64::from(u32_at(b, at + 4)) << 32
    }

    #[test]
    fn the_device_info_is_the_x870es() {
        let b = ask(query::DEV_INFO, 0, 0, 0, 440).unwrap();
        assert_eq!(b.len(), 440);
        assert_eq!(u32_at(&b, 0), 0x164e);
        assert_eq!(u32_at(&b, 8), 2, "external revision: chip 1 plus 1");
        assert_eq!(u32_at(&b, 16), 149);
        assert_eq!(u64_at(&b, 136), 0x11, "APU and gang submit");
        assert_eq!(u32_at(&b, 128), 8);
        assert_eq!(u32_at(&b, 176), 10, "DDR5");
        assert_eq!(u32_at(&b, 180), 128);
        assert_eq!(u64_at(&b, 144), 0x10000);
        assert_eq!(u64_at(&b, 152), 0x8000_0000_0000);
        assert_eq!(u64_at(&b, 336), 0xffff_8000_0000_0000);
        assert_eq!(u32_at(&b, 240), 32);
    }

    #[test]
    fn the_hardware_ips_report_what_the_specification_says() {
        let g = ask(query::HW_IP_INFO, ip::GFX, 0, 0, 40).unwrap();
        assert_eq!((u32_at(&g, 0), u32_at(&g, 4)), (10, 0));
        assert_eq!(
            (
                u32_at(&g, 16),
                u32_at(&g, 20),
                u32_at(&g, 24),
                u32_at(&g, 28)
            ),
            (32, 32, 1, 0x0a0306)
        );
        let c = ask(query::HW_IP_INFO, ip::COMPUTE, 0, 0, 40).unwrap();
        assert_eq!(u32_at(&c, 24), 0xf);
        let d = ask(query::HW_IP_INFO, ip::DMA, 0, 0, 40).unwrap();
        assert_eq!(
            (u32_at(&d, 0), u32_at(&d, 4), u32_at(&d, 16), u32_at(&d, 20)),
            (5, 2, 256, 4)
        );
        assert_eq!(u32_at(&d, 28), 0x050206);
        // An engine type that is absent answers all zeros, not an error.
        let v = ask(query::HW_IP_INFO, 6, 0, 0, 40).unwrap();
        assert!(v.iter().all(|b| *b == 0));
        // An instance other than 0 is an error.
        assert_eq!(
            ask(query::HW_IP_INFO, ip::GFX, 1, 0, 40),
            Err(Error::Invalid)
        );
        assert_eq!(
            u32_at(&ask(query::HW_IP_COUNT, ip::GFX, 0, 0, 4).unwrap(), 0),
            1
        );
        assert_eq!(ask(query::HW_IP_COUNT, 7, 0, 0, 4), Err(Error::Invalid));
    }

    #[test]
    fn a_graphics_only_device_has_no_compute_or_dma() {
        let dev = Device::raphael(512 << 20, 16 << 30).graphics_only();
        let ask = |q: u32, t: u32| {
            let req = InfoRequest {
                return_pointer: 0x1000,
                return_size: if q == query::HW_IP_COUNT { 4 } else { 40 },
                query: q,
                arg0: t,
                arg1: 0,
                arg2: 0,
                arg3: 0,
            };
            answer(&dev, &Fake, &req).unwrap()
        };
        assert_eq!(u32_at(&ask(query::HW_IP_COUNT, ip::GFX), 0), 1);
        assert_eq!(u32_at(&ask(query::HW_IP_COUNT, ip::COMPUTE), 0), 0);
        assert_eq!(u32_at(&ask(query::HW_IP_COUNT, ip::DMA), 0), 0);
        assert!(ask(query::HW_IP_INFO, ip::DMA).iter().all(|b| *b == 0));
    }

    #[test]
    fn firmware_versions_and_memory_sizes() {
        // PFP 0x12 feature 38; MEC2 0x1c; SDMA 9 feature 52; unknown 0.
        let f = |kind, index| ask(query::FW_VERSION, kind, 0, index, 8).unwrap();
        assert_eq!((u32_at(&f(5, 0), 0), u32_at(&f(5, 0), 4)), (0x12, 38));
        assert_eq!(u32_at(&f(8, 1), 0), 0x1c);
        assert_eq!((u32_at(&f(11, 0), 0), u32_at(&f(11, 0), 4)), (9, 52));
        assert_eq!(u32_at(&f(30, 0), 0), 0);
        assert_eq!(ask(query::FW_VERSION, 5, 1, 0, 8), Err(Error::Invalid));
        // Usable video memory is 8 MiB less than the real; the largest
        // allocation is three quarters of it.
        let m = ask(query::MEMORY, 0, 0, 0, 96).unwrap();
        assert_eq!(u64_at(&m, 0), 512 << 20);
        assert_eq!(u64_at(&m, 8), 504 << 20);
        assert_eq!(u64_at(&m, 16), 1 << 20);
        assert_eq!(u64_at(&m, 24), (504u64 << 20) / 4 * 3);
        assert_eq!(u64_at(&m, 64 + 8), 47 << 30);
        let g = ask(query::VRAM_GTT, 0, 0, 0, 24).unwrap();
        assert_eq!((u64_at(&g, 0), u64_at(&g, 8)), (504 << 20, 504 << 20));
        assert_eq!(
            u64_at(&ask(query::TIMESTAMP, 0, 0, 0, 8).unwrap(), 0),
            123_456
        );
        assert_eq!(
            u32_at(&ask(query::ACCEL_WORKING, 0, 0, 0, 4).unwrap(), 0),
            1
        );
    }

    #[test]
    fn register_reads_are_allow_listed_and_gb_addr_config_is_the_configured_value() {
        // GB_ADDR_CONFIG is the configured 0x42, not a read.
        let g = ask(query::READ_MMR_REG, 0x263e, 1, 0, 4).unwrap();
        assert_eq!(u32_at(&g, 0), 0x42);
        // CP_STAT is read live.
        assert_eq!(
            u32_at(&ask(query::READ_MMR_REG, 0x21a0, 1, 0, 4).unwrap(), 0),
            0x21a0 ^ 0xff
        );
        // Two registers in a row where both are allowed: GRBM_STATUS2, then
        // 0x2003 which is not on the list: the whole call fails.
        assert_eq!(
            ask(query::READ_MMR_REG, 0x2002, 2, 0, 8),
            Err(Error::Invalid)
        );
        assert_eq!(
            ask(query::READ_MMR_REG, 0x2000, 1, 0, 4),
            Err(Error::Invalid)
        );
        assert_eq!(
            ask(query::READ_MMR_REG, 0x2004, 0, 0, 4),
            Err(Error::Invalid)
        );
        assert_eq!(
            ask(query::READ_MMR_REG, 0x2004, 129, 0, 4),
            Err(Error::Invalid)
        );
    }

    #[test]
    fn a_zero_room_and_an_unknown_query_are_errors() {
        assert_eq!(ask(query::DEV_INFO, 0, 0, 0, 0), Err(Error::NoRoom));
        assert_eq!(ask(0x7f, 0, 0, 0, 8), Err(Error::Invalid));
        let r = InfoRequest {
            return_pointer: 0,
            return_size: 8,
            query: query::ACCEL_WORKING,
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
        };
        assert_eq!(answer(&dev(), &Fake, &r), Err(Error::NoRoom));
    }
}
