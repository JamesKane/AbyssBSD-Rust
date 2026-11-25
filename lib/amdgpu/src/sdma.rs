// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The system DMA engine, SDMA 5.2 (specification 07): the registers of
//! engine 0's graphics queue, the ring bring-up in the specification's order,
//! and the packets a kernel frame needs.

use alloc::vec::Vec;

use crate::regs::Window;

/// `SDMA0_CNTL`: trap interrupt (bit 0), address translation cache (bit 1),
/// mid-command preemption (bit 5).
pub const CNTL: usize = 0x49f0;
pub const CNTL_TRAP_ENABLE: u32 = 1 << 0;
pub const CNTL_UTC_L1_ENABLE: u32 = 1 << 1;
pub const CNTL_MIDCMD_PREEMPT_ENABLE: u32 = 1 << 5;
pub const SEM_WAIT_FAIL_TIMER_CNTL: usize = 0x4a04;
/// `SDMA0_STATUS_REG`: bit 0 idle.
pub const STATUS_REG: usize = 0x4a14;
/// `SDMA0_F32_CNTL`: bit 0 halts the engine's microcontroller.
pub const F32_CNTL: usize = 0x4a28;
pub const UTCL1_CNTL: usize = 0x4a70;
pub const UTCL1_PAGE: usize = 0x4aa0;
pub const GFX_RB_CNTL: usize = 0x4b80;
pub const GFX_RB_BASE: usize = 0x4b84;
pub const GFX_RB_BASE_HI: usize = 0x4b88;
pub const GFX_RB_RPTR: usize = 0x4b8c;
pub const GFX_RB_RPTR_HI: usize = 0x4b90;
pub const GFX_RB_WPTR: usize = 0x4b94;
pub const GFX_RB_WPTR_HI: usize = 0x4b98;
pub const GFX_RB_WPTR_POLL_CNTL: usize = 0x4b9c;
pub const GFX_RB_RPTR_ADDR_HI: usize = 0x4ba0;
pub const GFX_RB_RPTR_ADDR_LO: usize = 0x4ba4;
pub const GFX_IB_CNTL: usize = 0x4ba8;
pub const GFX_DOORBELL: usize = 0x4bc8;
pub const GFX_DOORBELL_OFFSET: usize = 0x4c2c;
pub const GFX_RB_WPTR_POLL_ADDR_HI: usize = 0x4c48;
pub const GFX_RB_WPTR_POLL_ADDR_LO: usize = 0x4c4c;
pub const GFX_MINOR_PTR_UPDATE: usize = 0x4c54;
/// `SDMA0_GFX_PREEMPT` (07 section 8): 1 preempts the queue's current
/// buffer, 0 lets it run again.
pub const GFX_PREEMPT: usize = 0x4c40;
/// `GRBM_SOFT_RESET` (graphics block) and engine 0's bit.
pub const GRBM_SOFT_RESET: usize = 0x8020;
pub const SOFT_RESET_SDMA0: u32 = 1 << 23;

/// The engine's doorbell: BAR2 byte offset (dword `0x200`, 07 section 5).
pub const DOORBELL_BYTE: usize = 0x800;
/// The IH client and the trap source (07 section 6).
pub const IH_CLIENT: u8 = 0x08;
pub const IH_SOURCE_TRAP: u8 = 224;
/// The invalidation engine the specification gives engine 0 (07 section 7.4).
pub const INVALIDATION_ENGINE: u32 = 13;

/// The ring and what the engine reads and writes around it, in GPU (MC)
/// addresses of memory the host can reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ring {
    /// The ring: a power of two bytes, 256-byte aligned.
    pub base: u64,
    pub bytes: u32,
    /// An 8-byte slot the engine writes its read pointer (a byte offset) to.
    pub rptr_report: u64,
    /// An 8-byte slot the host stores the write pointer in before the
    /// doorbell; the engine polls it.
    pub wptr_poll: u64,
}

fn lo(v: u64) -> u32 {
    (v & 0xffff_ffff) as u32
}

fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

/// `GFX_RB_CNTL` for a ring of `bytes`: `RB_SIZE` is log2 of the size in
/// dwords (bits 5:1), `enable` is bit 0 and the read-pointer write-back is
/// bit 12.
#[must_use]
pub fn rb_cntl(bytes: u32, enable: bool, writeback: bool) -> u32 {
    let log2 = (bytes / 4).trailing_zeros();
    (log2 << 1) | u32::from(enable) | (u32::from(writeback) << 12)
}

/// Brings engine 0's graphics queue up (07 section 4): reset, the ring
/// registers, the doorbell, the translation settings, then enabled. The
/// firmware has been loaded by the security processor. `pause_us` waits.
pub fn start<W: Window + ?Sized>(w: &W, r: &Ring, pause_us: &dyn Fn(u32)) {
    // 1. Pulse the engine's soft reset.
    w.write32(
        GRBM_SOFT_RESET,
        w.read32(GRBM_SOFT_RESET) | SOFT_RESET_SDMA0,
    );
    let _ = w.read32(GRBM_SOFT_RESET);
    pause_us(50);
    w.write32(
        GRBM_SOFT_RESET,
        w.read32(GRBM_SOFT_RESET) & !SOFT_RESET_SDMA0,
    );
    let _ = w.read32(GRBM_SOFT_RESET);
    // 2. Release the microcontroller.
    w.write32(F32_CNTL, w.read32(F32_CNTL) & !1);
    // 4. No semaphore wait-fail timer.
    w.write32(SEM_WAIT_FAIL_TIMER_CNTL, 0);
    // 5. The size, the ring still disabled.
    w.write32(GFX_RB_CNTL, rb_cntl(r.bytes, false, false));
    // 6. The pointers zero.
    for reg in [GFX_RB_RPTR, GFX_RB_RPTR_HI, GFX_RB_WPTR, GFX_RB_WPTR_HI] {
        w.write32(reg, 0);
    }
    // 7. The write-pointer shadow the engine polls.
    w.write32(GFX_RB_WPTR_POLL_ADDR_LO, lo(r.wptr_poll));
    w.write32(GFX_RB_WPTR_POLL_ADDR_HI, hi(r.wptr_poll));
    w.write32(
        GFX_RB_WPTR_POLL_CNTL,
        w.read32(GFX_RB_WPTR_POLL_CNTL) | 1 << 2,
    );
    // 8. The read-pointer write-back.
    w.write32(GFX_RB_RPTR_ADDR_LO, lo(r.rptr_report) & !3);
    w.write32(GFX_RB_RPTR_ADDR_HI, hi(r.rptr_report));
    w.write32(GFX_RB_CNTL, rb_cntl(r.bytes, false, true));
    // 9. The ring's address: bits 39:8, then bits 63:40.
    w.write32(GFX_RB_BASE, lo(r.base >> 8));
    w.write32(GFX_RB_BASE_HI, hi(r.base >> 8));
    // 10. The write pointer zero, under the minor-pointer update.
    w.write32(GFX_MINOR_PTR_UPDATE, 1);
    w.write32(GFX_RB_WPTR, 0);
    w.write32(GFX_RB_WPTR_HI, 0);
    // 11. The doorbell.
    w.write32(GFX_DOORBELL, w.read32(GFX_DOORBELL) | 1 << 28);
    w.write32(GFX_DOORBELL_OFFSET, 0x800);
    // 12.
    w.write32(GFX_MINOR_PTR_UPDATE, 0);
    // 13. Translation and trap settings.
    w.write32(
        CNTL,
        w.read32(CNTL) | CNTL_TRAP_ENABLE | CNTL_UTC_L1_ENABLE | CNTL_MIDCMD_PREEMPT_ENABLE,
    );
    let u = w.read32(UTCL1_CNTL);
    // RESP_MODE (11:9) = 3, REDO_DELAY (5:1) = 9.
    w.write32(
        UTCL1_CNTL,
        (u & !(0x7 << 9) & !(0x1f << 1)) | (3 << 9) | (9 << 1),
    );
    let p = w.read32(UTCL1_PAGE);
    // Clear 31:24 and 15:12; read policy 2, write policy 3, no LLC alloc.
    w.write32(
        UTCL1_PAGE,
        (p & !0xff00_0000 & !0xf000) | (2 << 12) | (3 << 14) | (1 << 24),
    );
    // 14. Run: the microcontroller, the ring, indirect buffers.
    w.write32(F32_CNTL, w.read32(F32_CNTL) & !1);
    w.write32(GFX_RB_CNTL, rb_cntl(r.bytes, true, true));
    w.write32(GFX_IB_CNTL, w.read32(GFX_IB_CNTL) | 1);
}

/// Resets the engine and starts it again (10 section 4.4): the ring stopped,
/// the translation cache off, then the bring-up of [`start`], whose first step
/// is the soft reset pulse. The pointers go back to zero.
pub fn recover<W: Window + ?Sized>(w: &W, r: &Ring, pause_us: &dyn Fn(u32)) {
    stop(w, r.bytes);
    w.write32(CNTL, w.read32(CNTL) & !CNTL_UTC_L1_ENABLE);
    start(w, r, pause_us);
}

/// Stops the engine (07 section 4): the ring and indirect buffers off, the
/// microcontroller halted.
pub fn stop<W: Window + ?Sized>(w: &W, bytes: u32) {
    w.write32(GFX_RB_CNTL, rb_cntl(bytes, false, true));
    w.write32(GFX_IB_CNTL, w.read32(GFX_IB_CNTL) & !1);
    w.write32(F32_CNTL, w.read32(F32_CNTL) | 1);
}

/// SDMA packet opcodes (07 section 7.1).
pub mod op {
    pub const NOP: u32 = 0;
    pub const COPY: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const INDIRECT: u32 = 4;
    pub const FENCE: u32 = 5;
    pub const TRAP: u32 = 6;
    pub const POLL_REGMEM: u32 = 8;
    pub const CONST_FILL: u32 = 11;
    pub const SRBM_WRITE: u32 = 14;
}

/// A stream of SDMA packets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packets(pub Vec<u32>);

impl Packets {
    /// An empty stream.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, dwords: &[u32]) -> &mut Self {
        self.0.extend_from_slice(dwords);
        self
    }

    /// One filler dword.
    pub fn nop(&mut self) -> &mut Self {
        self.push(&[op::NOP])
    }

    /// `WRITE` (linear): `data` dwords to `address` (dword aligned).
    pub fn write_linear(&mut self, address: u64, data: &[u32]) -> &mut Self {
        let n = u32::try_from(data.len()).unwrap_or(1).max(1) - 1;
        self.push(&[op::WRITE, lo(address), hi(address), n & 0xf_ffff]);
        self.push(data)
    }

    /// `COPY` (linear): `bytes` (at most 2^22) from `src` to `dst`.
    pub fn copy_linear(&mut self, src: u64, dst: u64, bytes: u32) -> &mut Self {
        self.push(&[
            op::COPY,
            (bytes - 1) & 0x3f_ffff,
            0,
            lo(src),
            hi(src),
            lo(dst),
            hi(dst),
        ])
    }

    /// `CONST_FILL`: `bytes` at `dst` set to the dword `value`.
    pub fn const_fill(&mut self, dst: u64, value: u32, bytes: u32) -> &mut Self {
        self.push(&[
            op::CONST_FILL,
            lo(dst),
            hi(dst),
            value,
            (bytes - 1) & 0x3f_ffff,
        ])
    }

    /// `FENCE`: the dword `value` to `address` (memory type 3, uncached).
    pub fn fence(&mut self, address: u64, value: u32) -> &mut Self {
        self.push(&[op::FENCE | (3 << 16), lo(address), hi(address), value])
    }

    /// A 64-bit fence: two `FENCE` packets, low then high.
    pub fn fence64(&mut self, address: u64, value: u64) -> &mut Self {
        self.fence(address, lo(value));
        self.fence(address + 4, hi(value))
    }

    /// `TRAP`: an interrupt with `context` in its entry.
    pub fn trap(&mut self, context: u32) -> &mut Self {
        self.push(&[op::TRAP, context & 0x0fff_ffff])
    }

    /// `POLL_REGMEM` on memory: wait until `(dword & mask) == reference`,
    /// polling every `interval`, `retry` times (`0xfff` for ever).
    pub fn poll_mem(
        &mut self,
        address: u64,
        reference: u32,
        mask: u32,
        interval: u32,
        retry: u32,
    ) -> &mut Self {
        self.push(&[
            op::POLL_REGMEM | (3 << 28) | (1 << 31),
            lo(address),
            hi(address),
            reference,
            mask,
            (interval & 0xffff) | ((retry & 0xfff) << 16),
        ])
    }

    /// The host data path flush (07 section 7.3): a poll with the flush bit
    /// on the bus interface's request and done registers (byte addresses).
    pub fn hdp_flush(&mut self, request: u32, done: u32, bit: u32) -> &mut Self {
        self.push(&[
            op::POLL_REGMEM | (1 << 26) | (3 << 28),
            done,
            request,
            bit,
            bit,
            10 | (0xfff << 16),
        ])
    }

    /// `SRBM_WRITE`: the register at dword index `reg` set to `value`.
    pub fn srbm_write(&mut self, reg: u32, value: u32) -> &mut Self {
        self.push(&[op::SRBM_WRITE | (0xf << 28), reg & 0x3_ffff, value])
    }

    /// The page-table switch of address space `vmid` (07 section 7.4): two
    /// `SRBM_WRITE`s of the context's base (`entry`, a directory entry; the
    /// registers are `base_lo` and `base_hi`, dword indices), then the
    /// invalidation poll on `engine` with the request word `request`.
    pub fn vm_flush(
        &mut self,
        vmid: u32,
        base_lo: u32,
        base_hi: u32,
        entry: u64,
        request: u32,
        engine: u32,
    ) -> &mut Self {
        self.srbm_write(base_lo, lo(entry));
        self.srbm_write(base_hi, hi(entry));
        // Opcode 8, sub-opcode 4: its own four dwords; the memory hub field is
        // `0x1f`, none.
        self.push(&[
            op::POLL_REGMEM | (4 << 8) | ((engine & 0x1f) << 16) | (0x1f << 24),
            request,
            0xffff_ffff,
            (1 << vmid) | (0x1f << 16),
        ])
    }

    /// `INDIRECT`: the buffer of `dwords` at `address` (32-byte aligned) in
    /// address space `vmid`. It must end on an 8-dword boundary of the ring:
    /// the ring writer pads before it (see [`Packets::pad_for_indirect`]).
    pub fn indirect(&mut self, address: u64, dwords: u32, vmid: u32) -> &mut Self {
        self.push(&[
            op::INDIRECT | ((vmid & 0xf) << 16),
            lo(address),
            hi(address),
            dwords & 0xf_ffff,
            0,
            0,
        ])
    }

    /// Fills with `NOP`s so an `INDIRECT` placed next ends on an 8-dword
    /// boundary, given that this stream starts at ring position `start`
    /// (dwords).
    pub fn pad_for_indirect(&mut self, start: u64) -> &mut Self {
        while !(start + self.0.len() as u64 + 6).is_multiple_of(8) {
            self.nop();
        }
        self
    }

    /// Pads to a multiple of 8 dwords with `NOP`s (an indirect buffer's
    /// length).
    pub fn pad8(&mut self) -> &mut Self {
        while !self.0.len().is_multiple_of(8) {
            self.nop();
        }
        self
    }
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
            self.regs.borrow().get(&o).copied().unwrap_or(0)
        }
    }

    impl Window for Regs {
        fn read32(&self, o: usize) -> u32 {
            self.get(o)
        }
        fn write32(&self, o: usize, v: u32) {
            self.regs.borrow_mut().insert(o, v);
            self.log.borrow_mut().push((o, v));
        }
    }

    fn ring() -> Ring {
        Ring {
            base: 0xf4_0ab0_0000,
            bytes: 64 << 10,
            rptr_report: 0xf4_0abf_0000,
            wptr_poll: 0xf4_0abf_0100,
        }
    }

    #[test]
    fn the_ring_control_word_has_the_size_in_dwords() {
        // 64 KiB = 16384 dwords = 2^14.
        assert_eq!(rb_cntl(64 << 10, false, false), 14 << 1);
        assert_eq!(rb_cntl(64 << 10, true, true), (14 << 1) | 1 | (1 << 12));
    }

    #[test]
    fn the_engine_starts_in_the_specifications_order() {
        let w = Regs::new();
        // The engine begins halted.
        w.regs.borrow_mut().insert(F32_CNTL, 1);
        start(&w, &ring(), &|_| {});
        let order: Vec<usize> = w.log.borrow().iter().map(|(o, _)| *o).collect();
        let at = |reg: usize| order.iter().position(|o| *o == reg).unwrap();
        // Reset before the ring registers, the base after the write-back
        // address, the doorbell after the write pointer, the ring enabled
        // last of all (the indirect-buffer enable follows).
        assert!(at(GRBM_SOFT_RESET) < at(GFX_RB_BASE));
        assert!(at(GFX_RB_RPTR_ADDR_LO) < at(GFX_RB_BASE));
        assert!(at(GFX_RB_BASE) < at(GFX_DOORBELL));
        assert_eq!(*order.last().unwrap(), GFX_IB_CNTL);
        assert_eq!(w.get(F32_CNTL) & 1, 0, "released");
        assert_eq!(w.get(GFX_RB_CNTL), rb_cntl(64 << 10, true, true));
        assert_eq!(
            u64::from(w.get(GFX_RB_BASE)),
            0xf4_0ab0_0000u64 >> 8 & 0xffff_ffff
        );
        assert_eq!(w.get(GFX_DOORBELL) >> 28 & 1, 1);
        assert_eq!(w.get(GFX_DOORBELL_OFFSET), 0x800);
        assert_eq!(w.get(GFX_RB_WPTR_POLL_ADDR_LO), 0x0abf_0100);
        assert_eq!(w.get(GFX_RB_WPTR_POLL_ADDR_HI), 0xf4);
        assert_eq!(w.get(GFX_RB_WPTR_POLL_CNTL) >> 2 & 1, 1);
        // Translation: response mode 3, redo delay 9, policies 2 and 3.
        assert_eq!(w.get(UTCL1_CNTL) >> 9 & 7, 3);
        assert_eq!(w.get(UTCL1_CNTL) >> 1 & 0x1f, 9);
        assert_eq!(w.get(UTCL1_PAGE) >> 12 & 3, 2);
        assert_eq!(w.get(UTCL1_PAGE) >> 14 & 3, 3);
        assert_eq!(w.get(UTCL1_PAGE) >> 24 & 1, 1);
        // The soft reset bit is clear again.
        assert_eq!(w.get(GRBM_SOFT_RESET) & SOFT_RESET_SDMA0, 0);
    }

    #[test]
    fn stopping_disables_and_halts() {
        let w = Regs::new();
        w.regs.borrow_mut().insert(GFX_IB_CNTL, 1);
        stop(&w, 64 << 10);
        assert_eq!(w.get(GFX_RB_CNTL) & 1, 0);
        assert_eq!(w.get(GFX_IB_CNTL) & 1, 0);
        assert_eq!(w.get(F32_CNTL) & 1, 1);
    }

    #[test]
    fn packets_have_the_documented_layouts() {
        let mut p = Packets::new();
        p.write_linear(0x1_2345_6780, &[0xaa, 0xbb]);
        assert_eq!(p.0, [2, 0x2345_6780, 1, 1, 0xaa, 0xbb]);
        let mut p = Packets::new();
        p.copy_linear(0x1000, 0x2000, 4096);
        assert_eq!(p.0, [1, 4095, 0, 0x1000, 0, 0x2000, 0]);
        let mut p = Packets::new();
        p.const_fill(0x3000, 0xdead_beef, 256);
        assert_eq!(p.0, [11, 0x3000, 0, 0xdead_beef, 255]);
        let mut p = Packets::new();
        p.fence(0x4000, 7).trap(9);
        assert_eq!(p.0, [5 | (3 << 16), 0x4000, 0, 7, 6, 9]);
        let mut p = Packets::new();
        p.fence64(0x5000, 0x1_0000_0002);
        assert_eq!(p.0.len(), 8);
        assert_eq!((p.0[3], p.0[7]), (2, 1));
        assert_eq!(p.0[1] + 4, p.0[5]);
        let mut p = Packets::new();
        p.poll_mem(0x6000, 5, 0xffff_ffff, 4, 0xfff);
        assert_eq!(p.0[0], 8 | (3 << 28) | (1 << 31));
        assert_eq!(p.0[5], 4 | (0xfff << 16));
        let mut p = Packets::new();
        p.srbm_write(0x28c9, 0x1234);
        assert_eq!(p.0, [14 | (0xf << 28), 0x28c9, 0x1234]);
        let mut p = Packets::new();
        p.indirect(0x7000, 16, 1);
        assert_eq!(p.0, [4 | (1 << 16), 0x7000, 0, 16, 0, 0]);
    }

    #[test]
    fn the_vm_flush_matches_the_specifications_vector() {
        // VMID 1 on engine 13 (07 section 7.4), directory entry 0x1_2345_6001.
        let mut p = Packets::new();
        p.vm_flush(1, 0x28c9, 0x28ca, 0x1_2345_6001, 0x00f8_0002, 13);
        assert_eq!(
            p.0,
            [
                0xf000_000e,
                0x0000_28c9,
                0x2345_6001,
                0xf000_000e,
                0x0000_28ca,
                0x1,
                0x1f0d_0408,
                0x00f8_0002,
                0xffff_ffff,
                0x001f_0002,
            ]
        );
    }

    #[test]
    fn an_indirect_packet_ends_on_an_eight_dword_boundary() {
        for start in 0..16u64 {
            let mut p = Packets::new();
            p.pad_for_indirect(start);
            p.indirect(0x7000, 8, 1);
            assert_eq!((start + p.0.len() as u64) % 8, 0, "start {start}");
            // Only fillers before it.
            assert!(p.0.iter().take(p.0.len() - 6).all(|d| *d == 0));
        }
        let mut p = Packets::new();
        p.nop().nop().nop().pad8();
        assert_eq!(p.0.len(), 8);
    }
}
