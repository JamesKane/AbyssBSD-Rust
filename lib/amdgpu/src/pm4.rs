// SPDX-License-Identifier: BSD-2-Clause
//! PM4 command packets for the graphics and compute command processors
//! (specification 06 section 10): the type-3 header and the packets a kernel
//! driver writes into a ring or an indirect buffer.
//!
//! A packet's header count is its body in dwords less one; the packet
//! tables of the specification list the body's dwords, and that is what
//! the builders count.

use alloc::vec::Vec;

/// Packet opcodes (06 section 10).
pub mod op {
    pub const NOP: u8 = 0x10;
    pub const CLEAR_STATE: u8 = 0x12;
    pub const COND_EXEC: u8 = 0x22;
    pub const CONTEXT_CONTROL: u8 = 0x28;
    pub const INDIRECT_BUFFER_CNST: u8 = 0x33;
    pub const WRITE_DATA: u8 = 0x37;
    pub const WAIT_REG_MEM: u8 = 0x3c;
    pub const INDIRECT_BUFFER: u8 = 0x3f;
    pub const COPY_DATA: u8 = 0x40;
    pub const PFP_SYNC_ME: u8 = 0x42;
    pub const EVENT_WRITE: u8 = 0x46;
    pub const PREAMBLE_CNTL: u8 = 0x4a;
    pub const RELEASE_MEM: u8 = 0x49;
    pub const ACQUIRE_MEM: u8 = 0x58;
    pub const SET_CONFIG_REG: u8 = 0x68;
    pub const SET_CONTEXT_REG: u8 = 0x69;
    pub const SET_SH_REG: u8 = 0x76;
    pub const SET_UCONFIG_REG: u8 = 0x79;
    pub const SWITCH_BUFFER: u8 = 0x8b;
    pub const FRAME_CONTROL: u8 = 0x90;
    pub const INVALIDATE_TLBS: u8 = 0x98;
}

/// The register ranges the `SET_*` packets address (06 section 10.2):
/// (first dword, one past the last), in the GC's own numbering.
pub mod range {
    pub const CONFIG: (u32, u32) = (0x2000, 0x2c00);
    pub const SH: (u32, u32) = (0x2c00, 0x3000);
    pub const CONTEXT: (u32, u32) = (0xa000, 0xa400);
    pub const UCONFIG: (u32, u32) = (0xc000, 0xc400);
}

/// A type-3 header: `body` dwords follow; `compute` marks a compute packet.
#[must_use]
pub const fn header(opcode: u8, body: u32, compute: bool) -> u32 {
    (3 << 30) | ((body - 1) << 16) | ((opcode as u32) << 8) | if compute { 2 } else { 0 }
}

/// A type-2 filler: one dword that does nothing.
pub const FILLER: u32 = 0x8000_0000;

/// Cache-flush bits of `RELEASE_MEM`'s first dword (06 section 10.3).
pub mod gcr {
    pub const GLM_WB: u32 = 1 << 12;
    pub const GLM_INV: u32 = 1 << 13;
    pub const GLV_INV: u32 = 1 << 14;
    pub const GL1_INV: u32 = 1 << 15;
    pub const GL2_US: u32 = 1 << 16;
    pub const GL2_RANGE: u32 = 1 << 17;
    pub const GL2_DISCARD: u32 = 1 << 19;
    pub const GL2_INV: u32 = 1 << 20;
    pub const GL2_WB: u32 = 1 << 21;
    pub const SEQ: u32 = 1 << 22;
}

/// The cache-control bits of `ACQUIRE_MEM`'s last dword (06 section 10.3).
pub mod acquire {
    pub const GLI_INV: u32 = 1 << 0;
    pub const GL1_RANGE: u32 = 1 << 2;
    pub const GLM_WB: u32 = 1 << 4;
    pub const GLM_INV: u32 = 1 << 5;
    pub const GLK_WB: u32 = 1 << 6;
    pub const GLK_INV: u32 = 1 << 7;
    pub const GLV_INV: u32 = 1 << 8;
    pub const GL1_INV: u32 = 1 << 9;
    pub const GL2_US: u32 = 1 << 10;
    pub const GL2_RANGE: u32 = 1 << 11;
    pub const GL2_DISCARD: u32 = 1 << 13;
    pub const GL2_INV: u32 = 1 << 14;
    pub const GL2_WB: u32 = 1 << 15;
    pub const SEQ: u32 = 1 << 16;
    /// The reference's full flush.
    pub const FULL: u32 =
        GL2_INV | GL2_WB | GLM_INV | GLM_WB | GL1_INV | GLV_INV | GLK_INV | GLI_INV;
}

/// The event `CACHE_FLUSH_AND_INV_TS_EVENT`, the fence's.
pub const EVENT_CACHE_FLUSH_AND_INV_TS: u32 = 0x14;

/// A stream of dwords being built (a ring's write, or an indirect buffer).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packets(pub Vec<u32>);

fn lo(v: u64) -> u32 {
    (v & 0xffff_ffff) as u32
}

fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

impl Packets {
    /// An empty stream.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    fn packet(&mut self, opcode: u8, compute: bool, body: &[u32]) -> &mut Self {
        self.0.push(header(
            opcode,
            u32::try_from(body.len()).unwrap_or(0x4000),
            compute,
        ));
        self.0.extend_from_slice(body);
        self
    }

    /// A NOP that skips `body` dwords (at least 1, at most `0x3fff`).
    pub fn nop(&mut self, body: u32) -> &mut Self {
        let n = body.clamp(1, 0x3fff);
        self.0.push(header(op::NOP, n, false));
        self.0.extend(core::iter::repeat_n(0, n as usize));
        self
    }

    /// Pads with fillers until the length is a multiple of `align` dwords
    /// (a ring's or an indirect buffer's alignment), with one long NOP.
    pub fn pad(&mut self, align: usize) -> &mut Self {
        while !self.0.len().is_multiple_of(align) {
            let missing = align - self.0.len() % align;
            if missing == 1 {
                self.0.push(FILLER);
            } else {
                self.nop(u32::try_from(missing).unwrap_or(2) - 1);
            }
        }
        self
    }

    /// `WRITE_DATA` to a memory address: `data` dwords at `address`.
    /// Destination 5 (memory), the write confirmed (06 section 10.3).
    pub fn write_data_mem(&mut self, address: u64, data: &[u32]) -> &mut Self {
        let mut body = Vec::with_capacity(3 + data.len());
        body.push((5 << 8) | (1 << 20));
        body.push(lo(address));
        body.push(hi(address));
        body.extend_from_slice(data);
        self.packet(op::WRITE_DATA, false, &body)
    }

    /// `WRITE_DATA` to a register: dword index `reg`, one value.
    pub fn write_data_reg(&mut self, reg: u32, value: u32) -> &mut Self {
        self.packet(op::WRITE_DATA, false, &[1 << 20, reg, 0, value])
    }

    /// `SET_*_REG`: `values` into consecutive registers from dword `reg`,
    /// in the range `(start, end)` of [`range`]; the offset is relative to
    /// the range start. `None` if the registers are not in the range.
    pub fn set_regs(
        &mut self,
        opcode: u8,
        range: (u32, u32),
        reg: u32,
        values: &[u32],
        compute: bool,
    ) -> Option<&mut Self> {
        let end = reg.checked_add(u32::try_from(values.len()).ok()?)?;
        if values.is_empty() || reg < range.0 || end > range.1 {
            return None;
        }
        let mut body = Vec::with_capacity(1 + values.len());
        body.push(reg - range.0);
        body.extend_from_slice(values);
        Some(self.packet(opcode, compute, &body))
    }

    /// `SET_UCONFIG_REG` of one register (the ring test's scratch write).
    pub fn set_uconfig(&mut self, reg: u32, value: u32) -> Option<&mut Self> {
        self.set_regs(op::SET_UCONFIG_REG, range::UCONFIG, reg, &[value], false)
    }

    /// `RELEASE_MEM`, the end-of-pipe fence (06 section 10.3): the event
    /// (`event`, index `event_index`), the cache flushes `gcr`, the cache
    /// policy, what to write (`data_select` 1 = 32 bits, 2 = 64 bits, 3 =
    /// the GPU's clock), whether to raise the interrupt (`interrupt_select`
    /// 2) and where.
    #[allow(clippy::too_many_arguments, reason = "the packet's own fields")]
    pub fn release_mem(
        &mut self,
        event: u32,
        event_index: u32,
        gcr: u32,
        cache_policy: u32,
        data_select: u32,
        interrupt_select: u32,
        address: u64,
        data: u64,
    ) -> &mut Self {
        let d1 = (event & 0x3f) | ((event_index & 0xf) << 8) | gcr | ((cache_policy & 3) << 25);
        let d2 = ((interrupt_select & 7) << 24) | ((data_select & 7) << 29);
        self.packet(
            op::RELEASE_MEM,
            false,
            &[d1, d2, lo(address), hi(address), lo(data), hi(data), 0],
        )
    }

    /// The reference's fence: a flush-and-invalidate end-of-pipe event that
    /// writes `value` (32 or 64 bits) to `address`, with the interrupt if
    /// `interrupt`.
    pub fn fence(&mut self, address: u64, value: u64, wide: bool, interrupt: bool) -> &mut Self {
        self.release_mem(
            EVENT_CACHE_FLUSH_AND_INV_TS,
            5,
            gcr::SEQ | gcr::GL2_WB | gcr::GLM_INV | gcr::GLM_WB,
            3,
            if wide { 2 } else { 1 },
            if interrupt { 2 } else { 0 },
            address,
            value,
        )
    }

    /// `WAIT_REG_MEM`: wait until `(read & mask) function reference` for the
    /// memory word at `address` (function 3 is "equal"), polling every
    /// `interval`, on the prefetch parser (`pfp`) or the micro engine.
    pub fn wait_mem(
        &mut self,
        function: u32,
        address: u64,
        reference: u32,
        mask: u32,
        interval: u32,
        pfp: bool,
    ) -> &mut Self {
        let d1 = (function & 7) | (1 << 4) | (u32::from(pfp) << 8);
        self.packet(
            op::WAIT_REG_MEM,
            false,
            &[d1, lo(address), hi(address), reference, mask, interval],
        )
    }

    /// `INDIRECT_BUFFER`: run `length_dwords` dwords at `address` (dword
    /// aligned) in VM `vmid`.
    pub fn indirect_buffer(
        &mut self,
        address: u64,
        length_dwords: u32,
        vmid: u32,
        compute: bool,
    ) -> &mut Self {
        let mut d3 = (length_dwords & 0xf_ffff) | ((vmid & 0xf) << 24);
        if compute {
            d3 |= 1 << 23;
        }
        self.packet(
            op::INDIRECT_BUFFER,
            compute,
            &[lo(address) & !3, hi(address), d3],
        )
    }

    /// `ACQUIRE_MEM`: make memory coherent for what follows: the whole
    /// address space (size `0xffffffff`/`0xffffff`, base 0), the cache
    /// controls `gcr_control` (see [`acquire`]).
    pub fn acquire_mem(&mut self, gcr_control: u32, compute: bool) -> &mut Self {
        self.packet(
            op::ACQUIRE_MEM,
            compute,
            &[0, 0xffff_ffff, 0xff_ffff, 0, 0, 0xa, gcr_control],
        )
    }

    /// `CONTEXT_CONTROL` with load enable and the given second set of bits.
    pub fn context_control(&mut self, bits: u32) -> &mut Self {
        self.packet(op::CONTEXT_CONTROL, false, &[0x8000_0000 | bits, 0])
    }

    /// `PREAMBLE_CNTL`: begin (`true`) or end the clear-state preamble.
    pub fn preamble(&mut self, begin: bool) -> &mut Self {
        self.packet(
            op::PREAMBLE_CNTL,
            false,
            &[if begin { 0x2000_0000 } else { 0x3000_0000 }],
        )
    }

    /// `CLEAR_STATE`.
    pub fn clear_state(&mut self) -> &mut Self {
        self.packet(op::CLEAR_STATE, false, &[0])
    }

    /// `PFP_SYNC_ME`.
    pub fn pfp_sync_me(&mut self) -> &mut Self {
        self.packet(op::PFP_SYNC_ME, false, &[0])
    }

    /// `FRAME_CONTROL`: start (`false`) or end (`true`) of a frame, not
    /// secure.
    pub fn frame_control(&mut self, end: bool) -> &mut Self {
        self.packet(op::FRAME_CONTROL, false, &[if end { 1 << 28 } else { 0 }])
    }

    /// `EVENT_WRITE`: an event of `event_type` and `index`.
    pub fn event_write(&mut self, event_type: u32, index: u32) -> &mut Self {
        self.packet(
            op::EVENT_WRITE,
            false,
            &[(event_type & 0x3f) | ((index & 0xf) << 8)],
        )
    }

    /// `COPY_DATA` of the register `reg` into memory at `address`: source 0
    /// (register), destination 5 (memory), the write confirmed.
    pub fn copy_reg_to_mem(&mut self, reg: u32, address: u64) -> &mut Self {
        self.packet(
            op::COPY_DATA,
            false,
            &[(5 << 8) | (1 << 20), reg, 0, lo(address), hi(address)],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_type_three_with_count_one_less_than_the_body() {
        // A NOP of one body dword: type 3, count 0, opcode 0x10.
        assert_eq!(header(op::NOP, 1, false), 0xc000_1000);
        // WRITE_DATA with a three-dword body plus one data dword.
        assert_eq!(header(op::WRITE_DATA, 4, false), 0xc003_3700);
        // The compute flag is bit 1.
        assert_eq!(header(op::SET_SH_REG, 2, true), 0xc001_7602);
        assert_eq!(FILLER, 0x8000_0000);
    }

    #[test]
    fn write_data_to_memory_and_to_a_register() {
        let mut p = Packets::new();
        p.write_data_mem(0xf4_0000_1000, &[0xdead_beef, 7]);
        assert_eq!(
            p.0,
            [
                header(op::WRITE_DATA, 5, false),
                (5 << 8) | (1 << 20),
                0x0000_1000,
                0xf4,
                0xdead_beef,
                7
            ]
        );
        let mut p = Packets::new();
        p.write_data_reg(0x2040, 0xcafe);
        assert_eq!(
            p.0,
            [header(op::WRITE_DATA, 4, false), 1 << 20, 0x2040, 0, 0xcafe]
        );
    }

    #[test]
    fn set_registers_are_relative_to_their_range_and_checked() {
        let mut p = Packets::new();
        // SCRATCH_REG0 is in the UCONFIG range: dword 0xc040 -> offset 0x40.
        p.set_uconfig(0xc040, 0x1234).unwrap();
        assert_eq!(p.0, [header(op::SET_UCONFIG_REG, 2, false), 0x40, 0x1234]);
        // Context registers are addressed by their segment-1 index.
        let mut p = Packets::new();
        p.set_regs(
            op::SET_CONTEXT_REG,
            range::CONTEXT,
            0xa000 + 5,
            &[1, 2, 3],
            false,
        )
        .unwrap();
        assert_eq!(p.0[1..2], [5]);
        assert_eq!(p.0.len(), 5);
        // Outside the range, or empty: refused, nothing written.
        let mut p = Packets::new();
        assert!(p.set_uconfig(0x2040, 1).is_none());
        assert!(
            p.set_regs(op::SET_SH_REG, range::SH, 0x2fff, &[1, 2], true)
                .is_none()
        );
        assert!(
            p.set_regs(op::SET_SH_REG, range::SH, 0x2c00, &[], true)
                .is_none()
        );
        assert!(p.0.is_empty());
    }

    #[test]
    fn the_fence_is_the_references_release_mem() {
        let mut p = Packets::new();
        p.fence(0xf4_0000_2000, 0x1_0000_0007, true, true);
        // Seven body dwords: count 6.
        assert_eq!(p.0.len(), 8);
        assert_eq!(p.0[0], header(op::RELEASE_MEM, 7, false));
        assert_eq!(p.0[0], 0xc006_4900);
        // Event 0x14, index 5, SEQ + GL2_WB + GLM_INV + GLM_WB, policy 3.
        assert_eq!(
            p.0[1],
            0x14 | (5 << 8) | gcr::SEQ | gcr::GL2_WB | gcr::GLM_INV | gcr::GLM_WB | (3 << 25)
        );
        // Interrupt select 2, 64-bit data (select 2).
        assert_eq!(p.0[2], (2 << 24) | (2 << 29));
        assert_eq!(p.0[3..8], [0x2000, 0xf4, 7, 1, 0]);
        // A 32-bit fence without an interrupt.
        let mut q = Packets::new();
        q.fence(0x1000, 5, false, false);
        assert_eq!(q.0[2], 1 << 29);
    }

    #[test]
    fn indirect_buffer_wait_and_acquire() {
        let mut p = Packets::new();
        p.indirect_buffer(0x7000_0000_1003, 64, 3, false);
        // The address is dword aligned down; vmid in bits 27:24.
        assert_eq!(
            p.0,
            [
                header(op::INDIRECT_BUFFER, 3, false),
                0x0000_1000,
                0x7000,
                64 | (3 << 24)
            ]
        );
        let mut p = Packets::new();
        p.indirect_buffer(0x1000, 8, 1, true);
        assert_eq!(p.0[0] & 2, 2);
        assert_eq!(p.0[3], 8 | (1 << 24) | (1 << 23));
        let mut p = Packets::new();
        p.wait_mem(3, 0xf4_0000_2000, 7, 0xffff_ffff, 0x20, true);
        assert_eq!(
            p.0,
            [
                header(op::WAIT_REG_MEM, 6, false),
                3 | (1 << 4) | (1 << 8),
                0x2000,
                0xf4,
                7,
                0xffff_ffff,
                0x20
            ]
        );
        let mut p = Packets::new();
        p.acquire_mem(acquire::FULL, false);
        assert_eq!(p.0.len(), 8);
        assert_eq!(
            p.0[1..],
            [0, 0xffff_ffff, 0xff_ffff, 0, 0, 0xa, acquire::FULL]
        );
        assert_eq!(acquire::FULL, 0xc3b1);
    }

    #[test]
    fn small_packets() {
        let mut p = Packets::new();
        p.context_control(0x8001)
            .preamble(true)
            .clear_state()
            .preamble(false);
        p.pfp_sync_me().frame_control(false).frame_control(true);
        assert_eq!(p.0[1..3], [0x8000_8001, 0]);
        assert_eq!(p.0[4], 0x2000_0000);
        let mut q = Packets::new();
        q.event_write(0x2c, 0).copy_reg_to_mem(0x2040, 0x5000);
        assert_eq!(q.0[1], 0x2c);
        assert_eq!(q.0[3..], [(5 << 8) | (1 << 20), 0x2040, 0, 0x5000, 0]);
    }

    #[test]
    fn padding_reaches_the_alignment_with_a_nop() {
        let mut p = Packets::new();
        p.pfp_sync_me();
        p.pad(8);
        assert_eq!(p.0.len(), 8);
        // A two-dword packet then a NOP header plus its five dwords.
        assert_eq!(p.0[2], header(op::NOP, 5, false));
        // One short of the alignment: a single filler.
        let mut p = Packets::new();
        p.0.extend([0; 7]);
        p.pad(8);
        assert_eq!(p.0[7], FILLER);
        // Already aligned: nothing added.
        let mut p = Packets::new();
        p.0.extend([0; 8]);
        p.pad(8);
        assert_eq!(p.0.len(), 8);
    }
}
