// SPDX-License-Identifier: BSD-2-Clause
//! Queue descriptors (MQDs, "memory queue descriptors") for the graphics
//! and compute command processors, and the kernel interface queue (KIQ)
//! through which the other queues are mapped (specification 06 sections 7.2,
//! 8 and 9).
//!
//! An MQD is 2,048 bytes; the command processor reads some of its dwords
//! when a queue is mapped. The live queue registers of a compute queue
//! mirror the MQD in order, one dword each from MQD byte `0x200` on: the
//! byte offset of the register that mirrors MQD offset `b` is
//! `0xc824 + (b - 0x200)` on Raphael (the specification's own table of
//! registers fixes the mapping: `CP_HQD_ACTIVE` `0xc82c` is MQD `0x208`,
//! `CP_HQD_PERSISTENT_STATE` `0xc834` is `0x210`, `CP_HQD_QUANTUM` `0xc840`
//! is `0x21c`, and so on).

use alloc::vec::Vec;

use crate::pm4::{Packets, header};

/// The size of an MQD in bytes.
pub const MQD_BYTES: usize = 2048;
const DWORDS: usize = MQD_BYTES / 4;

/// The byte offset of the register that mirrors MQD byte offset `mqd_offset`
/// (for offsets from `0x200`).
#[must_use]
pub const fn hqd_register(mqd_offset: usize) -> usize {
    0xc824 + (mqd_offset - 0x200)
}

/// The MQD byte offsets of the fields (06 sections 7.2 and 8).
pub mod field {
    pub const HEADER: usize = 0x000;
    pub const PIPELINE_STATS_ENABLE: usize = 0x02c;
    pub const STATIC_THREAD_MGMT_SE0: usize = 0x05c;
    pub const STATIC_THREAD_MGMT_SE1: usize = 0x060;
    pub const STATIC_THREAD_MGMT_SE2: usize = 0x068;
    pub const STATIC_THREAD_MGMT_SE3: usize = 0x06c;
    pub const MISC_RESERVED: usize = 0x080;
    pub const MQD_ADDR_LO: usize = 0x200;
    pub const MQD_ADDR_HI: usize = 0x204;
    pub const ACTIVE: usize = 0x208;
    pub const VMID: usize = 0x20c;
    pub const PERSISTENT_STATE: usize = 0x210;
    pub const PIPE_PRIORITY: usize = 0x214;
    pub const QUEUE_PRIORITY: usize = 0x218;
    pub const QUANTUM: usize = 0x21c;
    pub const QUEUE_ADDR_LO: usize = 0x220;
    pub const QUEUE_ADDR_HI: usize = 0x224;
    pub const RPTR: usize = 0x228;
    pub const RPTR_REPORT_LO: usize = 0x22c;
    pub const RPTR_REPORT_HI: usize = 0x230;
    pub const WPTR_POLL_LO: usize = 0x234;
    pub const WPTR_POLL_HI: usize = 0x238;
    pub const DOORBELL_CONTROL: usize = 0x23c;
    pub const QUEUE_CONTROL: usize = 0x244;
    pub const IB_CONTROL: usize = 0x254;
    pub const GFX_WPTR_LO: usize = 0x254;
    pub const GFX_WPTR_HI: usize = 0x258;
    pub const DEQUEUE_REQUEST: usize = 0x260;
    pub const MQD_CONTROL: usize = 0x288;
    pub const EOP_ADDR_LO: usize = 0x294;
    pub const EOP_ADDR_HI: usize = 0x298;
    pub const EOP_CONTROL: usize = 0x29c;
    pub const WPTR_LO: usize = 0x2d8;
    pub const WPTR_HI: usize = 0x2dc;
}

/// A queue's memory: all GPU addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Queue {
    /// Where the MQD is (256-byte aligned).
    pub mqd: u64,
    /// The ring (256-byte aligned) and its size in bytes (a power of two).
    pub ring: u64,
    pub ring_bytes: u32,
    /// Where the processor writes its read pointer (dword aligned).
    pub rptr_report: u64,
    /// Where it polls for the write pointer.
    pub wptr_poll: u64,
    /// The queue's doorbell, as a dword index in the doorbell aperture.
    pub doorbell_dword: u32,
    /// A compute queue's end-of-pipe buffer, 2 KiB, 256-byte aligned.
    pub eop: u64,
}

fn lo(v: u64) -> u32 {
    (v & 0xffff_ffff) as u32
}

fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

/// `DOORBELL_OFFSET` in bits 27:2 and `DOORBELL_EN` in bit 30.
#[must_use]
pub const fn doorbell_control(dword_index: u32) -> u32 {
    ((dword_index & 0x03ff_ffff) << 2) | (1 << 30)
}

/// The queue size field of a compute queue's control register:
/// `log2(bytes / 4) - 1`.
#[must_use]
pub fn queue_size_field(ring_bytes: u32) -> u32 {
    (ring_bytes / 4).trailing_zeros().saturating_sub(1)
}

/// A compute queue's MQD (the KIQ's too, 06 section 8): the dwords the
/// reference writes, all others zero.
#[must_use]
pub fn compute_mqd(q: &Queue) -> Vec<u32> {
    let mut m = alloc::vec![0u32; DWORDS];
    let mut put = |at: usize, v: u32| m[at / 4] = v;
    put(field::HEADER, 0xc031_0800);
    put(field::PIPELINE_STATS_ENABLE, 1);
    for at in [
        field::STATIC_THREAD_MGMT_SE0,
        field::STATIC_THREAD_MGMT_SE1,
        field::STATIC_THREAD_MGMT_SE2,
        field::STATIC_THREAD_MGMT_SE3,
    ] {
        put(at, 0xffff_ffff);
    }
    put(field::MISC_RESERVED, 3);
    put(field::MQD_ADDR_LO, lo(q.mqd) & !3);
    put(field::MQD_ADDR_HI, hi(q.mqd));
    put(field::ACTIVE, 1);
    put(field::VMID, 0);
    // PRELOAD_SIZE (bits 17:8) = 0x53.
    put(field::PERSISTENT_STATE, 0x53 << 8);
    put(field::QUEUE_ADDR_LO, lo(q.ring >> 8));
    put(field::QUEUE_ADDR_HI, hi(q.ring >> 8));
    put(field::RPTR_REPORT_LO, lo(q.rptr_report) & !3);
    put(field::RPTR_REPORT_HI, hi(q.rptr_report) & 0xffff);
    put(field::WPTR_POLL_LO, lo(q.wptr_poll));
    put(field::WPTR_POLL_HI, hi(q.wptr_poll) & 0xffff);
    put(field::DOORBELL_CONTROL, doorbell_control(q.doorbell_dword));
    // QUEUE_SIZE, RPTR_BLOCK_SIZE 9, UNORD_DISPATCH, PRIV_STATE, KMD_QUEUE.
    put(
        field::QUEUE_CONTROL,
        queue_size_field(q.ring_bytes) | (9 << 8) | (1 << 28) | (1 << 30) | (1 << 31),
    );
    // MIN_IB_AVAIL_SIZE (bits 21:20) = 3.
    put(field::IB_CONTROL, 3 << 20);
    put(field::EOP_ADDR_LO, lo(q.eop >> 8));
    put(field::EOP_ADDR_HI, hi(q.eop >> 8));
    // EOP_SIZE (bits 5:0) = 8: a 2,048-byte buffer.
    put(field::EOP_CONTROL, 8);
    m
}

/// A graphics queue's MQD (06 section 7.2).
#[must_use]
pub fn gfx_mqd(q: &Queue) -> Vec<u32> {
    let mut m = alloc::vec![0u32; DWORDS];
    let mut put = |at: usize, v: u32| m[at / 4] = v;
    put(field::MQD_ADDR_LO, lo(q.mqd) & !3);
    put(field::MQD_ADDR_HI, hi(q.mqd));
    put(field::ACTIVE, 1);
    put(field::VMID, 0);
    put(field::QUEUE_ADDR_LO, lo(q.ring >> 8));
    put(field::QUEUE_ADDR_HI, hi(q.ring >> 8));
    put(field::RPTR_REPORT_LO, lo(q.rptr_report) & !3);
    put(field::RPTR_REPORT_HI, hi(q.rptr_report) & 0xffff);
    put(field::WPTR_POLL_LO, lo(q.wptr_poll));
    put(field::WPTR_POLL_HI, hi(q.wptr_poll) & 0xffff);
    put(field::DOORBELL_CONTROL, doorbell_control(q.doorbell_dword));
    // RB_BUFSZ = log2(bytes / 4) - 1 in 5:0, RB_BLKSZ = RB_BUFSZ - 2 in 13:8.
    let bufsz = queue_size_field(q.ring_bytes);
    put(field::QUEUE_CONTROL, bufsz | (bufsz.saturating_sub(2) << 8));
    // The write pointer starts at 0 (the dwords at 0x254 and 0x258).
    put(field::GFX_WPTR_LO, 0);
    put(field::GFX_WPTR_HI, 0);
    // VMID 0, PRIV_STATE (bit 8), cache policy 0.
    put(field::MQD_CONTROL, 1 << 8);
    m
}

/// The register writes that program a compute queue directly from its MQD
/// (the KIQ's way, 06 section 9.2): (register byte offset, value), in two
/// parts in the specification's order. The queue is selected and the
/// write-pointer poll off already; the caller sets the queue active last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HqdWrites {
    /// Step 4: the end-of-pipe buffer, the MQD, the queue's base, control,
    /// read-pointer report and write-pointer poll addresses.
    pub queue: Vec<(usize, u32)>,
    /// Step 6, after the doorbell range: the doorbell control, the write
    /// pointer, the VMID, the persistent state.
    pub start: Vec<(usize, u32)>,
}

/// The two parts of the direct programming of the queue `mqd` describes.
#[must_use]
pub fn hqd_writes(mqd: &[u32]) -> HqdWrites {
    let reg = |f: usize| (hqd_register(f), mqd[f / 4]);
    HqdWrites {
        queue: alloc::vec![
            reg(field::EOP_ADDR_LO),
            reg(field::EOP_ADDR_HI),
            reg(field::EOP_CONTROL),
            reg(field::MQD_ADDR_LO),
            reg(field::MQD_ADDR_HI),
            reg(field::MQD_CONTROL),
            reg(field::QUEUE_ADDR_LO),
            reg(field::QUEUE_ADDR_HI),
            reg(field::QUEUE_CONTROL),
            reg(field::RPTR_REPORT_LO),
            reg(field::RPTR_REPORT_HI),
            reg(field::WPTR_POLL_LO),
            reg(field::WPTR_POLL_HI),
        ],
        start: alloc::vec![
            reg(field::DOORBELL_CONTROL),
            reg(field::WPTR_LO),
            reg(field::WPTR_HI),
            reg(field::VMID),
            reg(field::PERSISTENT_STATE),
        ],
    }
}

/// `SET_RESOURCES` (opcode `0xa0`, 7 body dwords; 06 section 9.3): hands the
/// kernel's compute queues (`queue_mask`) to the scheduler. The unmap
/// latency and the cleaner shader are left 0.
#[must_use]
pub fn set_resources(queue_mask: u64) -> Packets {
    let mut p = Packets::new();
    p.0.push(header(0xa0, 7, true));
    p.0.extend([0, lo(queue_mask), hi(queue_mask), 0, 0, 0, 0]);
    p
}

/// `MAP_QUEUES` for one graphics queue (opcode `0xa2`, 6 body dwords): the
/// micro engine 1, `pipe`, queue 0, engine select 4, one queue; the
/// doorbell, the MQD's address and the write-pointer poll address.
#[must_use]
pub fn map_gfx_queue(pipe: u32, doorbell_dword: u32, mqd: u64, wptr_poll: u64) -> Packets {
    let mut p = Packets::new();
    p.0.push(header(0xa2, 6, true));
    let d1 = ((pipe & 3) << 16) | (1 << 18) | (4 << 26) | (1 << 29);
    p.0.extend([
        d1,
        ((doorbell_dword & 0x03ff_ffff) << 2) | 2,
        lo(mqd),
        hi(mqd),
        lo(wptr_poll),
        hi(wptr_poll),
    ]);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> Queue {
        Queue {
            mqd: 0xf4_0ab0_4000,
            ring: 0xf4_0ab0_0000,
            ring_bytes: 4096,
            rptr_report: 0xf4_0ab0_3000,
            wptr_poll: 0xf4_0ab0_3100,
            doorbell_dword: 0x116,
            eop: 0xf4_0ab0_2000,
        }
    }

    #[test]
    fn the_register_mapping_matches_the_specifications_own_registers() {
        // The specification lists these live compute queue registers; the
        // MQD offsets they mirror are in its MQD table.
        assert_eq!(hqd_register(field::ACTIVE), 0xc82c);
        assert_eq!(hqd_register(field::PERSISTENT_STATE), 0xc834);
        assert_eq!(hqd_register(field::QUANTUM), 0xc840);
        assert_eq!(hqd_register(field::DOORBELL_CONTROL), 0xc860);
        assert_eq!(hqd_register(field::QUEUE_CONTROL), 0xc868);
        assert_eq!(hqd_register(field::IB_CONTROL), 0xc878);
        assert_eq!(hqd_register(field::DEQUEUE_REQUEST), 0xc884);
        assert_eq!(hqd_register(field::MQD_CONTROL), 0xc8ac);
        assert_eq!(hqd_register(field::EOP_CONTROL), 0xc8c0);
    }

    #[test]
    fn the_compute_mqd_has_the_specifications_values_and_zeros_elsewhere() {
        let m = compute_mqd(&q());
        assert_eq!(m.len(), 512);
        let at = |f: usize| m[f / 4];
        assert_eq!(at(0x000), 0xc031_0800);
        assert_eq!(at(0x02c), 1);
        assert_eq!(
            [at(0x05c), at(0x060), at(0x068), at(0x06c)],
            [0xffff_ffff; 4]
        );
        assert_eq!(at(0x080), 3);
        assert_eq!((at(0x200), at(0x204)), (0xab0_4000, 0xf4));
        assert_eq!(at(0x208), 1);
        assert_eq!(at(0x210), 0x5300);
        assert_eq!((at(0x220), at(0x224)), (0xf40ab000, 0));
        assert_eq!((at(0x22c), at(0x230)), (0xab0_3000, 0xf4));
        assert_eq!(at(0x23c), (0x116 << 2) | (1 << 30));
        // 4096-byte queue: log2(1024) - 1 = 9; block size 9; the flags.
        assert_eq!(at(0x244), 9 | (9 << 8) | (1 << 28) | (1 << 30) | (1 << 31));
        assert_eq!(at(0x254), 0x30_0000);
        assert_eq!((at(0x294), at(0x298)), (0xf40a_b020, 0));
        assert_eq!(at(0x29c), 8);
        // Everything else is zero: count the non-zero dwords.
        let nonzero = m.iter().filter(|v| **v != 0).count();
        assert_eq!(nonzero, 21);
    }

    #[test]
    fn the_gfx_mqd_has_only_the_dwords_the_specification_lists() {
        let m = gfx_mqd(&q());
        let at = |f: usize| m[f / 4];
        assert_eq!(at(0x000), 0, "no header on a graphics queue");
        assert_eq!((at(0x200), at(0x204)), (0xab0_4000, 0xf4));
        assert_eq!(at(0x208), 1);
        assert_eq!((at(0x220), at(0x224)), (0xf40a_b000, 0));
        assert_eq!(at(0x23c), (0x116 << 2) | (1 << 30));
        assert_eq!(at(0x244), 9 | (7 << 8));
        assert_eq!(at(0x288), 1 << 8);
        let nonzero = m.iter().filter(|v| **v != 0).count();
        assert_eq!(nonzero, 11);
    }

    #[test]
    fn the_queue_size_and_doorbell_fields() {
        assert_eq!(queue_size_field(4096), 9);
        assert_eq!(queue_size_field(65_536), 13);
        assert_eq!(doorbell_control(0), 1 << 30);
        assert_eq!(doorbell_control(0x116), 0x4000_0458);
    }

    #[test]
    fn the_direct_programming_covers_the_kiqs_registers_in_order() {
        let m = compute_mqd(&q());
        let w = hqd_writes(&m);
        let q_regs: Vec<usize> = w.queue.iter().map(|(r, _)| *r).collect();
        // EOP first, the wptr poll address last of the queue part.
        assert_eq!(q_regs[0], 0xc8b8);
        assert_eq!(*q_regs.last().unwrap(), 0xc85c);
        let s_regs: Vec<usize> = w.start.iter().map(|(r, _)| *r).collect();
        assert_eq!(s_regs[0], 0xc860);
        assert_eq!(*s_regs.last().unwrap(), 0xc834);
        // The doorbell control takes the MQD's value.
        assert_eq!(w.start[0].1, doorbell_control(0x116));
        // Active is in neither part (the caller sets it last).
        assert!(!q_regs.contains(&0xc82c) && !s_regs.contains(&0xc82c));
    }

    #[test]
    fn the_kiq_packets() {
        let r = set_resources(0);
        assert_eq!(r.0.len(), 8);
        assert_eq!(r.0[0], 0xc006_a002);
        let p = map_gfx_queue(0, 0x116, 0xf4_0ab0_4000, 0xf4_0ab0_3100);
        assert_eq!(p.0.len(), 7);
        assert_eq!(p.0[0], 0xc005_a202);
        // ME 1, pipe 0, queue 0, graphics engine, one queue.
        assert_eq!(p.0[1], (1 << 18) | (4 << 26) | (1 << 29));
        assert_eq!(p.0[2], (0x116 << 2) | 2);
        assert_eq!(p.0[3..], [0xab0_4000, 0xf4, 0xab0_3100, 0xf4]);
        // Pipe 1 for the second graphics ring.
        assert_eq!(map_gfx_queue(1, 0x118, 0, 0).0[1] & (3 << 16), 1 << 16);
    }
}
