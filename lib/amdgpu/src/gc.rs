// SPDX-License-Identifier: BSD-2-Clause
//! The graphics block (GC 10.3.6, specification 06): the golden settings,
//! the per-VMID shader memory configuration, the clear-state buffer the RLC
//! replays, and graphics ring 0 programmed directly (stage R4.2 and R4.4 of
//! docs/GRAPHICS.md §16).
//!
//! Register addresses are Raphael's byte offsets in BAR5.

use alloc::vec::Vec;

use crate::gc_tables::{CSB_NONZERO, GOLDEN_10_3_6};
use crate::pm4::{self, Packets, op, range};
use crate::regs::Window;

/// `GRBM_GFX_CNTL`: bank select for per-queue and per-VMID registers.
pub const GRBM_GFX_CNTL: usize = 0x8088;
/// `GRBM_GFX_INDEX`: SE, SA and instance select; broadcast when idle.
pub const GRBM_GFX_INDEX: usize = 0x30800;
/// Everything selected: the value to leave `GRBM_GFX_INDEX` at.
pub const GFX_INDEX_BROADCAST: u32 = 0xe000_0000;
/// `SH_MEM_BASES`.
pub const SH_MEM_BASES: usize = 0x8c28;
/// `SH_MEM_CONFIG`.
pub const SH_MEM_CONFIG: usize = 0x8c34;
/// `SH_MEM_CONFIG` for every VMID (06 section 5).
pub const SH_MEM_CONFIG_VALUE: u32 = 0x0000_c00c;
/// `SH_MEM_BASES` for VMIDs 1 to 7 (the private and shared apertures).
pub const SH_MEM_BASES_GRAPHICS: u32 = 0x2000_1000;
/// `SH_MEM_BASES` for VMIDs 8 to 15 (the compute runtime's).
pub const SH_MEM_BASES_COMPUTE: u32 = 0x6000_6000;
/// `RLC_CSIB_ADDR_LO` / `_HI` / `RLC_CSIB_LENGTH`.
pub const RLC_CSIB_ADDR_LO: usize = 0x3b288;
/// The high half.
pub const RLC_CSIB_ADDR_HI: usize = 0x3b28c;
/// The length in dwords.
pub const RLC_CSIB_LENGTH: usize = 0x3b290;

// The graphics ring (06 section 7.1).
pub const CP_RB0_BASE: usize = 0xc100;
pub const CP_RB0_BASE_HI: usize = 0xc2c4;
pub const CP_RB0_CNTL: usize = 0xc104;
pub const CP_RB0_RPTR_ADDR: usize = 0xc10c;
pub const CP_RB0_RPTR_ADDR_HI: usize = 0xc110;
pub const CP_RB0_WPTR: usize = 0xc150;
pub const CP_RB0_WPTR_HI: usize = 0xc154;
pub const CP_RB_WPTR_POLL_ADDR_LO: usize = 0xc3ac;
pub const CP_RB_WPTR_POLL_ADDR_HI: usize = 0xc3b0;
pub const CP_RB_VMID: usize = 0xc144;
pub const CP_RB_ACTIVE: usize = 0xc680;
pub const CP_RB_WPTR_DELAY: usize = 0x8704;
pub const CP_MAX_CONTEXT: usize = 0xc2b8;
pub const CP_DEVICE_ID: usize = 0xc12c;
/// `CP_ME_CNTL`: bit 24 CE halt, 26 PFP halt, 28 ME halt.
pub const CP_ME_CNTL: usize = 0x86d8;
/// `CP_STAT`: 0 when the command processor is idle.
pub const CP_STAT: usize = 0x8680;
/// `CP_RB0_RPTR`: the live read pointer.
pub const CP_RB0_RPTR: usize = 0x8700;
/// The halt bits of `CP_ME_CNTL`.
pub const ME_HALT_ALL: u32 = (1 << 24) | (1 << 26) | (1 << 28);
/// `SCRATCH_REG0`, the ring test's register (dword index `0xc040` in the
/// `SET_UCONFIG_REG` range).
pub const SCRATCH_REG0: usize = 0x30100;
/// Its dword index for a packet.
pub const SCRATCH_REG0_DWORD: u32 = 0xc040;

/// Applies the golden settings of Raphael (06 section 3.1): each (register,
/// mask, value) as a write if the mask is all ones, else a read-modify-write.
/// With the shader engines in broadcast.
pub fn apply_golden<W: Window + ?Sized>(w: &W) {
    w.write32(GRBM_GFX_INDEX, GFX_INDEX_BROADCAST);
    for (_, at, mask, value) in GOLDEN_10_3_6 {
        let v = if mask == u32::MAX {
            value
        } else {
            (w.read32(at) & !mask) | (value & mask)
        };
        w.write32(at, v);
    }
}

/// Selects the queue and VMID that the banked registers address.
pub fn select<W: Window + ?Sized>(w: &W, me: u32, pipe: u32, queue: u32, vmid: u32) {
    w.write32(
        GRBM_GFX_CNTL,
        (pipe & 3) | ((me & 3) << 2) | ((vmid & 0xf) << 4) | ((queue & 7) << 8),
    );
}

/// The per-VMID shader memory configuration (06 section 5): for VMIDs 0 to
/// 15 `SH_MEM_CONFIG`, and for 1 to 15 `SH_MEM_BASES`; the selection is put
/// back to zero.
pub fn shader_memory<W: Window + ?Sized>(w: &W) {
    for vmid in 0..16 {
        select(w, 0, 0, 0, vmid);
        w.write32(SH_MEM_CONFIG, SH_MEM_CONFIG_VALUE);
        match vmid {
            1..=7 => w.write32(SH_MEM_BASES, SH_MEM_BASES_GRAPHICS),
            8..=15 => w.write32(SH_MEM_BASES, SH_MEM_BASES_COMPUTE),
            _ => {}
        }
    }
    select(w, 0, 0, 0, 0);
}

/// The extents of the clear-state buffer: (offset in the context register
/// space, number of registers), 921 in all (06 section 6.2).
const CSB_EXTENTS: [(u32, usize); 8] = [
    (0x000, 215),
    (0x0d8, 272),
    (0x1f5, 4),
    (0x1ff, 158),
    (0x2a0, 2),
    (0x2a3, 1),
    (0x2a5, 66),
    (0x2f5, 203),
];

/// The clear-state buffer: 949 dwords of PM4 that reset every context
/// register (06 section 6.2).
#[must_use]
pub fn clear_state_buffer() -> Vec<u32> {
    let value = |offset: u32| {
        CSB_NONZERO
            .iter()
            .find(|(o, _)| u32::from(*o) == offset)
            .map_or(0, |(_, v)| *v)
    };
    let mut p = Packets::new();
    p.preamble(true).context_control(0);
    // The second dword of the specification's CONTEXT_CONTROL is also load
    // enable: `0x80000000`, `0x80000000`.
    let at = p.0.len() - 1;
    p.0[at] = 0x8000_0000;
    for (start, count) in CSB_EXTENTS {
        let values: Vec<u32> = (0u32..).take(count).map(|i| value(start + i)).collect();
        p.set_regs(
            op::SET_CONTEXT_REG,
            range::CONTEXT,
            range::CONTEXT.0 + start,
            &values,
            false,
        );
    }
    // PA_SC_TILE_STEERING_OVERRIDE, zero, on GC 10.3.
    p.set_regs(
        op::SET_CONTEXT_REG,
        range::CONTEXT,
        range::CONTEXT.0 + 0xd7,
        &[0],
        false,
    );
    p.preamble(false).clear_state();
    p.0
}

/// The graphics ring and what the command processor reads and writes
/// around it, all in GPU (MC) addresses of memory the host can reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ring {
    /// The ring, a power of two bytes, 256-byte aligned.
    pub base: u64,
    /// Its size in bytes.
    pub bytes: u32,
    /// Where the processor writes its read pointer (dword aligned).
    pub rptr_report: u64,
    /// Where it polls for the write pointer.
    pub wptr_poll: u64,
}

/// `CP_RB0_CNTL` for a ring of `bytes` (06 section 7.1): the size as
/// log2(bytes / 8) in bits 5:0, the block size two less in 13:8.
#[must_use]
pub fn ring_cntl(bytes: u32) -> u32 {
    let bufsz = (bytes / 8).trailing_zeros();
    bufsz | ((bufsz.saturating_sub(2)) << 8)
}

fn lo(v: u64) -> u32 {
    (v & 0xffff_ffff) as u32
}

fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

/// Programs graphics ring 0 directly, with the command processor halted:
/// the VMID 0, the size, the write pointer zero, the read-pointer report and
/// write-pointer poll addresses, the base, and active (06 section 7.1).
/// `pause_us` waits between the two writes of the control register.
pub fn program_ring<W: Window + ?Sized>(w: &W, r: &Ring, pause_us: &dyn Fn(u32)) {
    w.write32(CP_RB_WPTR_DELAY, 0);
    w.write32(CP_RB_VMID, 0);
    let cntl = ring_cntl(r.bytes);
    w.write32(CP_RB0_CNTL, cntl);
    w.write32(CP_RB0_WPTR, 0);
    w.write32(CP_RB0_WPTR_HI, 0);
    w.write32(CP_RB0_RPTR_ADDR, lo(r.rptr_report) & !3);
    w.write32(CP_RB0_RPTR_ADDR_HI, hi(r.rptr_report) & 0xffff);
    w.write32(CP_RB_WPTR_POLL_ADDR_LO, lo(r.wptr_poll) & !3);
    w.write32(CP_RB_WPTR_POLL_ADDR_HI, hi(r.wptr_poll) & 0xffff);
    pause_us(1000);
    w.write32(CP_RB0_CNTL, cntl);
    w.write32(CP_RB0_BASE, lo(r.base >> 8));
    w.write32(CP_RB0_BASE_HI, hi(r.base >> 8));
    w.write32(CP_RB_ACTIVE, 1);
}

/// Releases the graphics command processors (06 section 7.3): the context
/// count and the device id, then CE, PFP and ME unhalted; looks at
/// `CP_STAT` up to `polls` times and returns whether it read 0. A processor
/// that has been released runs its firmware and normally reads busy: the
/// caller does not treat that as a failure.
pub fn unhalt<W: Window + ?Sized>(w: &W, polls: u32, pause_us: &dyn Fn(u32), wait_us: u32) -> bool {
    w.write32(CP_MAX_CONTEXT, 7);
    w.write32(CP_DEVICE_ID, 1);
    w.write32(CP_ME_CNTL, w.read32(CP_ME_CNTL) & !ME_HALT_ALL);
    for _ in 0..polls {
        if w.read32(CP_STAT) == 0 {
            return true;
        }
        pause_us(wait_us);
    }
    false
}

/// The packets that follow the clear-state buffer on ring 0 (06 section
/// 7.3): `SET_BASE` index 3 with the two partition sizes `0x8000`.
#[must_use]
pub fn set_base_packets() -> Vec<u32> {
    let mut v = Vec::new();
    v.push(pm4::header(0x11, 3, false));
    v.extend([3, 0x8000, 0x8000]);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use std::collections::BTreeMap;

    struct Regs {
        regs: RefCell<BTreeMap<usize, u32>>,
        log: RefCell<Vec<(usize, u32)>>,
    }

    impl Regs {
        fn new() -> Self {
            Self {
                regs: RefCell::new(BTreeMap::new()),
                log: RefCell::new(Vec::new()),
            }
        }
        fn get(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
    }

    impl Window for Regs {
        fn read32(&self, o: usize) -> u32 {
            self.get(o)
        }
        fn write32(&self, o: usize, v: u32) {
            self.log.borrow_mut().push((o, v));
            self.regs.borrow_mut().insert(o, v);
        }
    }

    #[test]
    fn the_clear_state_buffer_is_949_dwords_with_the_specifications_structure() {
        let csb = clear_state_buffer();
        assert_eq!(csb.len(), 949);
        // PREAMBLE_CNTL begin, CONTEXT_CONTROL with load enable, ...
        assert_eq!(csb[0], pm4::header(op::PREAMBLE_CNTL, 1, false));
        assert_eq!(csb[1], 0x2000_0000);
        assert_eq!(csb[2], pm4::header(op::CONTEXT_CONTROL, 2, false));
        assert_eq!(csb[3..5], [0x8000_0000, 0x8000_0000]);
        // ... the first SET_CONTEXT_REG: offset 0, 215 registers.
        assert_eq!(csb[5], pm4::header(op::SET_CONTEXT_REG, 216, false));
        assert_eq!(csb[6], 0);
        // PA_SC_SCREEN_SCISSOR_BR (offset 0xd) is 0x40004000.
        assert_eq!(csb[7 + 0xd], 0x4000_4000);
        // The end: PREAMBLE_CNTL end, CLEAR_STATE 0.
        let n = csb.len();
        assert_eq!(
            csb[n - 4..],
            [
                pm4::header(op::PREAMBLE_CNTL, 1, false),
                0x3000_0000,
                pm4::header(op::CLEAR_STATE, 1, false),
                0
            ]
        );
        // Exactly 85 non-zero context values, and 921 values in 8 packets.
        let packet_values: usize = [215, 272, 4, 158, 2, 1, 66, 203].iter().sum();
        assert_eq!(packet_values, 921);
        assert_eq!(CSB_NONZERO.len(), 85);
    }

    #[test]
    fn every_nonzero_value_lands_at_its_offset_in_its_extent() {
        let csb = clear_state_buffer();
        // Walk the SET_CONTEXT_REG packets and collect (offset, value).
        let mut at = 5;
        let mut found = Vec::new();
        for _ in 0..8 {
            let h = csb[at];
            let body = ((h >> 16) & 0x3fff) as usize + 1;
            let start = csb[at + 1];
            for (i, v) in csb[at + 2..at + 1 + body].iter().enumerate() {
                if *v != 0 {
                    found.push((start + u32::try_from(i).unwrap(), *v));
                }
            }
            at += 1 + body;
        }
        let want: Vec<(u32, u32)> = CSB_NONZERO
            .iter()
            .map(|(o, v)| (u32::from(*o), *v))
            .collect();
        assert_eq!(found, want);
    }

    #[test]
    fn golden_settings_write_or_read_modify_write_by_mask() {
        let w = Regs::new();
        // DB_DEBUG3 has a full mask: written whole. GB_ADDR_CONFIG has a
        // partial one: the other bits kept.
        w.regs.borrow_mut().insert(0x98f8, 0xf0f0_f800);
        apply_golden(&w);
        assert_eq!(w.get(0x9838), 0x280);
        assert_eq!(w.get(0x98f8), (0xf0f0_f800 & !0x0c18_07ff) | 0x42);
        assert_eq!(GOLDEN_10_3_6.len(), 22);
        // Broadcast first.
        assert_eq!(w.log.borrow()[0], (GRBM_GFX_INDEX, GFX_INDEX_BROADCAST));
    }

    #[test]
    fn shader_memory_is_set_for_every_vmid_and_the_selection_returns_to_zero() {
        let w = Regs::new();
        shader_memory(&w);
        let log = w.log.borrow();
        let configs = log.iter().filter(|(r, _)| *r == SH_MEM_CONFIG).count();
        assert_eq!(configs, 16);
        let bases: Vec<u32> = log
            .iter()
            .filter(|(r, _)| *r == SH_MEM_BASES)
            .map(|(_, v)| *v)
            .collect();
        assert_eq!(bases.len(), 15);
        assert!(bases[..7].iter().all(|b| *b == 0x2000_1000));
        assert!(bases[7..].iter().all(|b| *b == 0x6000_6000));
        assert_eq!(*log.last().unwrap(), (GRBM_GFX_CNTL, 0));
        // VMID 5 selected is bits 7:4.
        assert!(log.contains(&(GRBM_GFX_CNTL, 5 << 4)));
    }

    #[test]
    fn the_ring_control_encodes_the_size_in_eight_byte_units() {
        // 64 KiB: log2(65536 / 8) = 13, block size 11.
        assert_eq!(ring_cntl(64 * 1024), 13 | (11 << 8));
        assert_eq!(ring_cntl(4096), 9 | (7 << 8));
        let w = Regs::new();
        let r = Ring {
            base: 0xf4_0aa0_0000,
            bytes: 65536,
            rptr_report: 0xf4_0ab0_0000,
            wptr_poll: 0xf4_0ab0_0100,
        };
        program_ring(&w, &r, &|_| {});
        assert_eq!(w.get(CP_RB0_BASE), 0xf40a_a000);
        assert_eq!(w.get(CP_RB0_BASE_HI), 0);
        assert_eq!(w.get(CP_RB0_CNTL), 13 | (11 << 8));
        assert_eq!(w.get(CP_RB0_RPTR_ADDR), 0x0ab0_0000);
        assert_eq!(w.get(CP_RB0_RPTR_ADDR_HI), 0xf4);
        assert_eq!(w.get(CP_RB_WPTR_POLL_ADDR_LO), 0x0ab0_0100);
        assert_eq!(w.get(CP_RB_VMID), 0);
        assert_eq!(w.get(CP_RB_ACTIVE), 1);
        // The control register is written twice, around the pause.
        assert_eq!(
            w.log
                .borrow()
                .iter()
                .filter(|(r, _)| *r == CP_RB0_CNTL)
                .count(),
            2
        );
    }

    #[test]
    fn unhalting_clears_only_the_halt_bits_and_waits_for_idle() {
        let w = Regs::new();
        w.regs.borrow_mut().insert(CP_ME_CNTL, 0x15ff_0000);
        w.regs.borrow_mut().insert(CP_STAT, 0);
        assert!(unhalt(&w, 3, &|_| {}, 1));
        assert_eq!(w.get(CP_ME_CNTL), 0x00ff_0000);
        assert_eq!(w.get(CP_MAX_CONTEXT), 7);
        assert_eq!(w.get(CP_DEVICE_ID), 1);
        // A processor that stays busy: not reported idle.
        let w = Regs::new();
        w.regs.borrow_mut().insert(CP_STAT, 0x8000_0000);
        assert!(!unhalt(&w, 3, &|_| {}, 1));
    }

    #[test]
    fn set_base_is_the_ce_partition_packet() {
        assert_eq!(
            set_base_packets(),
            [pm4::header(0x11, 3, false), 3, 0x8000, 0x8000]
        );
    }
}
