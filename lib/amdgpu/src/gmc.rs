// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The memory controller's two translation hubs (GMC 10; specification 05
//! sections 2 to 6): where each hub's registers are, and the order in which
//! the reference sets a hub up. Everything works on a register window, so the
//! sequence is tested on the host against a model of the hub.
//!
//! The graphics hub (GFXHUB) and the multimedia hub (MMHUB) are programmed
//! the same way but lay their registers out differently: arrays on the
//! graphics hub, interleaved eight-dword blocks on the memory hub.

use crate::regs::Window;

/// Which hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hub {
    /// The graphics hub (inside GC): graphics, compute and SDMA.
    Gfx,
    /// The multimedia hub: multimedia engines, display and the interrupt
    /// handler.
    Mm,
}

/// A hub register that is the same for every instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reg {
    /// The video memory's physical offset, in 16 MiB units.
    FbOffset,
    /// The default page for aperture misses (`SYSTEM_APERTURE_DEFAULT_ADDR_LSB`).
    DefaultAddrLsb,
    /// Its `_MSB`.
    DefaultAddrMsb,
    /// The first video memory address, 16 MiB units.
    FbLocationBase,
    /// The last.
    FbLocationTop,
    /// `AGP_TOP`.
    AgpTop,
    /// `AGP_BOT`.
    AgpBot,
    /// `AGP_BASE`.
    AgpBase,
    /// `SYSTEM_APERTURE_LOW_ADDR`.
    ApertureLow,
    /// `SYSTEM_APERTURE_HIGH_ADDR`.
    ApertureHigh,
    /// `MX_L1_TLB_CNTL`.
    L1Tlb,
    /// `VM_L2_CNTL`.
    L2Cntl,
    /// `VM_L2_CNTL2`.
    L2Cntl2,
    /// `VM_L2_CNTL3`.
    L2Cntl3,
    /// `VM_L2_CNTL4`.
    L2Cntl4,
    /// `VM_L2_CNTL5`.
    L2Cntl5,
    /// `VM_L2_PROTECTION_FAULT_CNTL`.
    FaultCntl,
    /// `VM_L2_PROTECTION_FAULT_CNTL2`.
    FaultCntl2,
    /// `VM_L2_PROTECTION_FAULT_STATUS`.
    FaultStatus,
    /// `VM_L2_PROTECTION_FAULT_DEFAULT_ADDR_LO32`.
    FaultDefaultLo,
    /// `VM_L2_PROTECTION_FAULT_DEFAULT_ADDR_HI32`.
    FaultDefaultHi,
}

impl Hub {
    /// The byte offset of `r` in the register window.
    #[must_use]
    pub const fn reg(self, r: Reg) -> usize {
        match self {
            Self::Gfx => match r {
                Reg::FbOffset => 0xa51c,
                Reg::DefaultAddrLsb => 0xa520,
                Reg::DefaultAddrMsb => 0xa524,
                Reg::FbLocationBase => 0xa570,
                Reg::FbLocationTop => 0xa574,
                Reg::AgpTop => 0xa578,
                Reg::AgpBot => 0xa57c,
                Reg::AgpBase => 0xa580,
                Reg::ApertureLow => 0xa584,
                Reg::ApertureHigh => 0xa588,
                Reg::L1Tlb => 0xa58c,
                Reg::L2Cntl => 0xa070,
                Reg::L2Cntl2 => 0xa074,
                Reg::L2Cntl3 => 0xa078,
                Reg::L2Cntl4 => 0xa0d0,
                Reg::L2Cntl5 => 0xa0f0,
                Reg::FaultCntl => 0xa090,
                Reg::FaultCntl2 => 0xa094,
                Reg::FaultStatus => 0xa0a0,
                Reg::FaultDefaultLo => 0xa0ac,
                Reg::FaultDefaultHi => 0xa0b0,
            },
            Self::Mm => match r {
                Reg::FbOffset => 0x6a35c,
                Reg::DefaultAddrLsb => 0x6a360,
                Reg::DefaultAddrMsb => 0x6a364,
                Reg::FbLocationBase => 0x6a3b0,
                Reg::FbLocationTop => 0x6a3b4,
                Reg::AgpTop => 0x6a3b8,
                Reg::AgpBot => 0x6a3bc,
                Reg::AgpBase => 0x6a3c0,
                Reg::ApertureLow => 0x6a3c4,
                Reg::ApertureHigh => 0x6a3c8,
                Reg::L1Tlb => 0x6a3cc,
                Reg::L2Cntl => 0x69c00,
                Reg::L2Cntl2 => 0x69c04,
                Reg::L2Cntl3 => 0x69c08,
                Reg::L2Cntl4 => 0x69c60,
                Reg::L2Cntl5 => 0x69c80,
                Reg::FaultCntl => 0x69c20,
                Reg::FaultCntl2 => 0x69c24,
                Reg::FaultStatus => 0x69c30,
                Reg::FaultDefaultLo => 0x69c3c,
                Reg::FaultDefaultHi => 0x69c40,
            },
        }
    }

    /// `CONTEXTn_CNTL` (one dword a context on both hubs).
    #[must_use]
    pub const fn context_cntl(self, n: usize) -> usize {
        4 * n
            + match self {
                Self::Gfx => 0xa170,
                Self::Mm => 0x69d00,
            }
    }

    /// The first of a context's six page-table registers: base LO, base HI,
    /// start LO, start HI, end LO, end HI, in that order, found with
    /// [`Hub::context_table`].
    const fn context_base(self, n: usize, field: usize) -> usize {
        // field: 0 base LO, 1 base HI, 2 start LO, 3 start HI, 4 end LO, 5 end HI.
        match self {
            // Arrays: each field an array of two dwords a context.
            Self::Gfx => {
                let first = [0xa31c, 0xa320, 0xa39c, 0xa3a0, 0xa41c, 0xa420][field];
                first + 8 * n
            }
            // Interleaved: a block of eight dwords a context.
            Self::Mm => 0x6a500 + 32 * n + 4 * field,
        }
    }

    /// A context's page-table base, start and end registers, each as
    /// `(low, high)`: `(base, start, end)`.
    #[must_use]
    pub const fn context_table(self, n: usize) -> [(usize, usize); 3] {
        [
            (self.context_base(n, 0), self.context_base(n, 1)),
            (self.context_base(n, 2), self.context_base(n, 3)),
            (self.context_base(n, 4), self.context_base(n, 5)),
        ]
    }

    /// An invalidation engine's registers: `(sem, req, ack, range_lo, range_hi)`.
    #[must_use]
    pub const fn engine(self, e: usize) -> (usize, usize, usize, usize, usize) {
        match self {
            Self::Gfx => (
                0xa1b4 + 4 * e,
                0xa1fc + 4 * e,
                0xa244 + 4 * e,
                0xa28c + 8 * e,
                0xa290 + 8 * e,
            ),
            Self::Mm => {
                let b = 0x6a800 + 32 * e;
                (b, b + 4, b + 8, b + 12, b + 16)
            }
        }
    }
}

/// The identity aperture registers (graphics hub only; the memory hub's are
/// not in the specification's table): context 1's low and high addresses
/// (each LO, HI) and the physical offset (LO, HI).
pub const GFX_IDENTITY: [usize; 6] = [0xa0b8, 0xa0bc, 0xa0c0, 0xa0c4, 0xa0c8, 0xa0cc];

/// Field values of the hub registers.
pub mod bits {
    /// `MX_L1_TLB_CNTL`: `ENABLE_L1_TLB`.
    pub const L1_ENABLE: u32 = 1 << 0;
    /// `SYSTEM_ACCESS_MODE` (4:3) = 3.
    pub const L1_ACCESS_MODE: u32 = 3 << 3;
    /// `SYSTEM_APERTURE_UNMAPPED_ACCESS` (5).
    pub const L1_UNMAPPED_ACCESS: u32 = 1 << 5;
    /// `ENABLE_ADVANCED_DRIVER_MODEL` (6).
    pub const L1_ADVANCED_DRIVER_MODEL: u32 = 1 << 6;
    /// `MTYPE` (13:11) = 3 (uncached).
    pub const L1_MTYPE_UC: u32 = 3 << 11;
    /// The whole `MTYPE` field.
    pub const L1_MTYPE_MASK: u32 = 7 << 11;

    /// `VM_L2_CNTL`: `ENABLE_L2_CACHE`.
    pub const L2_ENABLE: u32 = 1 << 0;
    /// `ENABLE_L2_FRAGMENT_PROCESSING`.
    pub const L2_FRAGMENT_PROCESSING: u32 = 1 << 1;
    /// `L2_PDE0_CACHE_TAG_GENERATION_MODE`.
    pub const L2_PDE0_TAG_MODE: u32 = 1 << 8;
    /// `ENABLE_DEFAULT_PAGE_OUT_TO_SYSTEM_MEMORY`.
    pub const L2_DEFAULT_PAGE_OUT: u32 = 1 << 11;
    /// `PDE_FAULT_CLASSIFICATION`.
    pub const L2_PDE_FAULT_CLASSIFICATION: u32 = 1 << 18;
    /// `CONTEXT1_IDENTITY_ACCESS_MODE` (20:19) and `IDENTITY_MODE_FRAGMENT_SIZE`
    /// (25:21): the fields.
    pub const L2_IDENTITY_MASK: u32 = 3 << 19 | 0x1f << 21;
    /// `CONTEXT1_IDENTITY_ACCESS_MODE` = 1.
    pub const L2_IDENTITY_MODE_1: u32 = 1 << 19;
    /// `VM_L2_CNTL2`: invalidate all L1 TLBs and the L2 cache.
    pub const L2_INVALIDATE: u32 = 3;
    /// `VM_L2_CNTL3`: `BANK_SELECT` (5:0) = 9, `L2_CACHE_BIGK_FRAGMENT_SIZE`
    /// (19:15) = 6.
    pub const L2_CNTL3_MASK: u32 = 0x3f | 0x1f << 15;
    /// The values.
    pub const L2_CNTL3_VALUE: u32 = 9 | 6 << 15;
    /// `VM_L2_CNTL4`: the two physical-request tap bits (6, 7).
    pub const L2_CNTL4_TAP_PHYSICAL: u32 = 3 << 6;
    /// `VM_L2_CNTL5`: `L2_CACHE_SMALLK_FRAGMENT_SIZE` (4:0).
    pub const L2_CNTL5_SMALLK_MASK: u32 = 0x1f;

    /// `PROTECTION_FAULT_CNTL`: the default-enable bits (2 to 12).
    pub const FAULT_DEFAULTS: u32 = 0x1fff << 2 & !3;
    /// The two crash bits (30, 31).
    pub const FAULT_CRASH: u32 = 3 << 30;
    /// `PROTECTION_FAULT_CNTL2`: `ACTIVE_PAGE_MIGRATION_PTE_READ_RETRY` (18).
    pub const FAULT2_READ_RETRY: u32 = 1 << 18;
    /// `ENABLE_RETRY_FAULT_INTERRUPT` (19): graphics hub only.
    pub const FAULT2_RETRY_INTERRUPT: u32 = 1 << 19;

    /// `CONTEXTn_CNTL`: `ENABLE_CONTEXT`.
    pub const CTX_ENABLE: u32 = 1 << 0;
    /// `PAGE_TABLE_DEPTH` (2:1).
    pub const CTX_DEPTH_SHIFT: u32 = 1;
    /// `RETRY_PERMISSION_OR_INVALID_PAGE_FAULT` (7).
    pub const CTX_RETRY: u32 = 1 << 7;
    /// The seven protection-fault default-enable bits (10, 12, 14, 16, 18,
    /// 20, 22).
    pub const CTX_FAULT_DEFAULTS: u32 =
        1 << 10 | 1 << 12 | 1 << 14 | 1 << 16 | 1 << 18 | 1 << 20 | 1 << 22;
    /// Their interrupt enables (9, 11, 13, 15, 17, 19, 21).
    pub const CTX_FAULT_INTERRUPTS: u32 =
        1 << 9 | 1 << 11 | 1 << 13 | 1 << 15 | 1 << 17 | 1 << 19 | 1 << 21;
}

/// The number of invalidation engines on a hub.
pub const ENGINES: usize = 18;
/// The engine the processor uses for its own invalidations (05 section 5.3).
pub const CPU_ENGINE: usize = 17;
/// The number of VM contexts on a hub.
pub const CONTEXTS: usize = 16;

/// What the hub is set up with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    /// The physical address of the GART's page table (a flat array of 8-byte
    /// entries): for a table in video memory, its offset plus the video
    /// memory's physical offset (05 section 2.3).
    pub gart_table: u64,
    /// The GART's first address (4 GiB aligned).
    pub gart_start: u64,
    /// Its last address.
    pub gart_end: u64,
    /// The page for faults and aperture misses (a system address; 4 KiB
    /// aligned).
    pub dummy_page: u64,
    /// Whether to set the retry-fault interrupt (graphics hub).
    pub retry_interrupt: bool,
}

/// Why a hub is not set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The GART's range is empty, unaligned, or its table address is
    /// misaligned.
    Gart,
    /// The dummy page is not 4 KiB aligned.
    DummyPage,
}

impl Setup {
    /// Checks the setup.
    ///
    /// # Errors
    ///
    /// [`SetupError`] for a GART or page the hub does not take.
    pub fn check(&self) -> Result<(), SetupError> {
        if self.gart_end <= self.gart_start
            || !self.gart_start.is_multiple_of(1 << 32)
            || !self.gart_table.is_multiple_of(4096)
            || self.gart_end >> 48 != 0
        {
            return Err(SetupError::Gart);
        }
        if !self.dummy_page.is_multiple_of(4096) {
            return Err(SetupError::DummyPage);
        }
        Ok(())
    }
}

fn lo(v: u64) -> u32 {
    u32::try_from(v & 0xffff_ffff).unwrap_or(0)
}

fn hi(v: u64) -> u32 {
    u32::try_from(v >> 32).unwrap_or(0)
}

fn rmw<W: Window + ?Sized>(w: &W, at: usize, clear: u32, set: u32) {
    w.write32(at, (w.read32(at) & !clear) | set);
}

/// Sets `hub` up in the order of specification 05 section 5.1: the GART
/// aperture, the system aperture and AGP, the fault defaults, the L1
/// translation buffer, the L2 cache, context 0 (the GART's), the identity
/// aperture off (graphics hub), the user contexts, and the invalidation
/// engines' ranges. Registers the hardware keeps bits in (the L1 control's
/// ECO bits, the L2 controls' reset values) are written read-modify-write.
///
/// # Errors
///
/// [`SetupError`] for a setup the hub does not take; nothing is written.
pub fn init<W: Window + ?Sized>(w: &W, hub: Hub, s: &Setup) -> Result<(), SetupError> {
    s.check()?;
    // 1. The GART: a flat table (depth 0), the whole range, context 0 off
    //    until the end.
    let [(base_lo, base_hi), (start_lo, start_hi), (end_lo, end_hi)] = hub.context_table(0);
    w.write32(base_lo, lo(s.gart_table));
    w.write32(base_hi, hi(s.gart_table));
    w.write32(start_lo, lo(s.gart_start >> 12));
    w.write32(start_hi, hi(s.gart_start >> 12));
    w.write32(end_lo, lo(s.gart_end >> 12));
    w.write32(end_hi, hi(s.gart_end >> 12));
    // 2. The system aperture stays as the firmware set it (it covers video
    //    memory); the AGP aperture is disabled by a bottom above its top.
    w.write32(hub.reg(Reg::AgpBot), 0x00ff_ffff);
    w.write32(hub.reg(Reg::AgpTop), 0);
    w.write32(hub.reg(Reg::AgpBase), 0);
    // 3. Faults and aperture misses go to a dummy page.
    w.write32(hub.reg(Reg::FaultDefaultLo), lo(s.dummy_page >> 12));
    w.write32(hub.reg(Reg::FaultDefaultHi), hi(s.dummy_page >> 12));
    w.write32(hub.reg(Reg::DefaultAddrLsb), lo(s.dummy_page >> 12));
    w.write32(hub.reg(Reg::DefaultAddrMsb), hi(s.dummy_page >> 12));
    // 4. Fault control 2.
    let mut fault2 = bits::FAULT2_READ_RETRY;
    if s.retry_interrupt {
        fault2 |= bits::FAULT2_RETRY_INTERRUPT;
    }
    rmw(w, hub.reg(Reg::FaultCntl2), 0, fault2);
    // 5. The L1 translation buffer.
    rmw(
        w,
        hub.reg(Reg::L1Tlb),
        bits::L1_UNMAPPED_ACCESS | bits::L1_MTYPE_MASK,
        bits::L1_ENABLE | bits::L1_ACCESS_MODE | bits::L1_ADVANCED_DRIVER_MODEL | bits::L1_MTYPE_UC,
    );
    // 6. The L2 cache.
    rmw(
        w,
        hub.reg(Reg::L2Cntl),
        bits::L2_FRAGMENT_PROCESSING
            | bits::L2_PDE0_TAG_MODE
            | bits::L2_PDE_FAULT_CLASSIFICATION
            | bits::L2_IDENTITY_MASK,
        bits::L2_ENABLE | bits::L2_DEFAULT_PAGE_OUT | bits::L2_IDENTITY_MODE_1,
    );
    w.write32(hub.reg(Reg::L2Cntl2), bits::L2_INVALIDATE);
    rmw(
        w,
        hub.reg(Reg::L2Cntl3),
        bits::L2_CNTL3_MASK,
        bits::L2_CNTL3_VALUE,
    );
    rmw(w, hub.reg(Reg::L2Cntl4), bits::L2_CNTL4_TAP_PHYSICAL, 0);
    rmw(w, hub.reg(Reg::L2Cntl5), bits::L2_CNTL5_SMALLK_MASK, 0);
    // The fault defaults (the specification does this after the hubs are
    // up; they are the same registers).
    rmw(
        w,
        hub.reg(Reg::FaultCntl),
        bits::FAULT_CRASH,
        bits::FAULT_DEFAULTS,
    );
    // 7. Context 0: on, flat, no retry.
    rmw(
        w,
        hub.context_cntl(0),
        3 << bits::CTX_DEPTH_SHIFT | bits::CTX_RETRY,
        bits::CTX_ENABLE,
    );
    // 8. The identity aperture off (graphics hub): low above high.
    if hub == Hub::Gfx {
        w.write32(GFX_IDENTITY[0], 0xffff_ffff);
        w.write32(GFX_IDENTITY[1], 0xf);
        w.write32(GFX_IDENTITY[2], 0);
        w.write32(GFX_IDENTITY[3], 0);
        w.write32(GFX_IDENTITY[4], 0);
        w.write32(GFX_IDENTITY[5], 0);
    }
    // 9. The user contexts: four levels, no retry, every fault default and
    //    interrupt on, the whole 48-bit space.
    for n in 1..CONTEXTS {
        w.write32(
            hub.context_cntl(n),
            bits::CTX_ENABLE
                | 3 << bits::CTX_DEPTH_SHIFT
                | bits::CTX_FAULT_DEFAULTS
                | bits::CTX_FAULT_INTERRUPTS,
        );
        let [_, (start_lo, start_hi), (end_lo, end_hi)] = hub.context_table(n);
        w.write32(start_lo, 0);
        w.write32(start_hi, 0);
        w.write32(end_lo, 0xffff_ffff);
        w.write32(end_hi, 0xf);
    }
    // 10. Every invalidation engine takes the full range.
    for e in 0..ENGINES {
        let (_, _, _, range_lo, range_hi) = hub.engine(e);
        w.write32(range_lo, 0xffff_ffff);
        w.write32(range_hi, 0x1f);
    }
    Ok(())
}

/// The request word for invalidating VMID `vmid` everywhere (05 section
/// 5.3): that context, the legacy flush type, all the L2 levels and the L1.
#[must_use]
pub const fn invalidate_request(vmid: u32) -> u32 {
    (1 << vmid) | 0xf << 19 | 1 << 23
}

/// The packets a ring writes to switch VMID `vmid`'s page table to the root
/// at `root` (a physical address; `system` if it is in system memory) and
/// flush its translations, through invalidation engine `engine` (05 section
/// 5.3): the base written low then high (the root as a directory entry:
/// valid, plus system), the request, a wait on the acknowledgement of that
/// VMID's bit, then `PFP_SYNC_ME`. The graphics hub only.
#[must_use]
pub fn ring_vm_flush(vmid: u32, root: u64, system: bool, engine: usize) -> crate::pm4::Packets {
    ring_vm_flush_on(vmid, root, system, engine, true)
}

/// [`ring_vm_flush`] for either kind of ring: a compute ring has no prefetch
/// parser, so its wait runs on the micro engine and there is no
/// `PFP_SYNC_ME` (06 section 10.4).
#[must_use]
pub fn ring_vm_flush_on(
    vmid: u32,
    root: u64,
    system: bool,
    engine: usize,
    pfp: bool,
) -> crate::pm4::Packets {
    let hub = Hub::Gfx;
    let (base_lo, base_hi) = hub.context_table(vmid as usize)[0];
    let (_, req, ack, _, _) = hub.engine(engine);
    let entry = crate::vm::pde(root, system);
    let dword = |byte: usize| u32::try_from(byte / 4).unwrap_or(0);
    let mut p = crate::pm4::Packets::new();
    p.write_data_reg(dword(base_lo), (entry & 0xffff_ffff) as u32);
    p.write_data_reg(dword(base_hi), (entry >> 32) as u32);
    p.write_data_reg(dword(req), invalidate_request(vmid));
    p.wait_reg(3, dword(ack), 1 << vmid, 1 << vmid, 0x20, pfp);
    if pfp {
        p.pfp_sync_me();
    }
    p
}

/// Why an invalidation did not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidateError {
    /// The engine did not acknowledge in time.
    Timeout,
    /// The memory hub's semaphore was not acquired in time.
    Semaphore,
}

/// Invalidates VMID `vmid` through the processor's engine and waits for the
/// acknowledgement, polling up to `polls` times with `pause` between. The
/// memory hub's semaphore is taken around it, as the specification says
/// (reading it takes it; writing 0 releases it). The caller has flushed the
/// host data path.
///
/// # Errors
///
/// [`InvalidateError`] if the semaphore or the acknowledgement does not come.
pub fn invalidate<W: Window + ?Sized>(
    w: &W,
    hub: Hub,
    vmid: u32,
    polls: u32,
    pause: &dyn Fn(),
) -> Result<(), InvalidateError> {
    let (sem, req, ack, _, _) = hub.engine(CPU_ENGINE);
    if hub == Hub::Mm {
        let mut tries = 0;
        while w.read32(sem) & 1 == 0 {
            tries += 1;
            if tries > polls {
                return Err(InvalidateError::Semaphore);
            }
            pause();
        }
    }
    w.write32(req, invalidate_request(vmid));
    let mut result = Err(InvalidateError::Timeout);
    for _ in 0..polls {
        if w.read32(ack) & (1 << vmid) != 0 {
            result = Ok(());
            break;
        }
        pause();
    }
    if hub == Hub::Mm {
        w.write32(sem, 0);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// A hub in memory: the registers' values, the writes in order, and an
    /// invalidation engine that acknowledges after a few polls.
    struct Model {
        regs: RefCell<BTreeMap<usize, u32>>,
        log: RefCell<Vec<(usize, u32)>>,
        hub: Hub,
        /// Reads of ACK before it shows the request's bit.
        ack_after: RefCell<u32>,
        sem: RefCell<bool>,
    }

    impl Model {
        fn new(hub: Hub) -> Self {
            let m = Self {
                regs: RefCell::new(BTreeMap::new()),
                log: RefCell::new(Vec::new()),
                hub,
                ack_after: RefCell::new(2),
                sem: RefCell::new(false),
            };
            // The firmware's state (the capture): contexts as reset, the L1
            // control with its ECO bits, L2 at reset values.
            for n in 0..2 {
                m.regs.borrow_mut().insert(hub.context_cntl(n), 0x007f_fe80);
            }
            m.regs.borrow_mut().insert(hub.reg(Reg::L1Tlb), 0x501);
            m.regs
                .borrow_mut()
                .insert(hub.reg(Reg::L2Cntl), 0x0008_0602);
            m.regs
                .borrow_mut()
                .insert(hub.reg(Reg::L2Cntl3), 0x8010_0007);
            m.regs.borrow_mut().insert(hub.reg(Reg::L2Cntl4), 0xc1);
            m.regs.borrow_mut().insert(hub.reg(Reg::L2Cntl5), 0x3fe0);
            m.regs
                .borrow_mut()
                .insert(hub.reg(Reg::FaultCntl), 0x3fff_fffc);
            m.regs
                .borrow_mut()
                .insert(hub.reg(Reg::FaultCntl2), 0x0002_0000);
            m
        }
        fn reg(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
    }

    impl Window for Model {
        fn read32(&self, offset: usize) -> u32 {
            let (sem, req, ack, _, _) = self.hub.engine(CPU_ENGINE);
            if offset == sem {
                // Reading takes it.
                let was = *self.sem.borrow();
                *self.sem.borrow_mut() = true;
                return u32::from(!was);
            }
            if offset == ack {
                let mut n = self.ack_after.borrow_mut();
                if *n > 0 {
                    *n -= 1;
                    return 0;
                }
                return self.reg(req) & 0xffff;
            }
            self.reg(offset)
        }
        fn write32(&self, offset: usize, value: u32) {
            let (sem, ..) = self.hub.engine(CPU_ENGINE);
            if offset == sem {
                *self.sem.borrow_mut() = false;
            }
            self.log.borrow_mut().push((offset, value));
            self.regs.borrow_mut().insert(offset, value);
        }
    }

    const SETUP: Setup = Setup {
        gart_table: 0x18_5fc0_0000,
        gart_start: 0,
        gart_end: 0x3fff_ffff,
        dummy_page: 0x1_2345_6000,
        retry_interrupt: true,
    };

    #[test]
    fn the_register_maps_match_the_specification() {
        // 05 section 4.1 and 4.2.
        assert_eq!(Hub::Gfx.reg(Reg::FbLocationBase), 0xa570);
        assert_eq!(Hub::Mm.reg(Reg::FbLocationBase), 0x6a3b0);
        assert_eq!(Hub::Gfx.context_cntl(1), 0xa174);
        assert_eq!(Hub::Mm.context_cntl(1), 0x69d04);
        // The graphics hub's arrays are +2 dwords a context; the memory hub's blocks are 8.
        assert_eq!(Hub::Gfx.context_table(1)[0], (0xa324, 0xa328));
        assert_eq!(Hub::Mm.context_table(1)[0], (0x6a520, 0x6a524));
        assert_eq!(Hub::Gfx.context_table(0)[2], (0xa41c, 0xa420));
        assert_eq!(Hub::Mm.context_table(0)[2], (0x6a510, 0x6a514));
        // Engines: +1 a dword and +2 for the range on the graphics hub; 8-dword blocks on the memory hub.
        assert_eq!(
            Hub::Gfx.engine(17),
            (0xa1f8, 0xa240, 0xa288, 0xa314, 0xa318)
        );
        assert_eq!(
            Hub::Mm.engine(17),
            (0x6aa20, 0x6aa24, 0x6aa28, 0x6aa2c, 0x6aa30)
        );
    }

    #[test]
    fn a_setup_the_hub_does_not_take_writes_nothing() {
        let m = Model::new(Hub::Gfx);
        for bad in [
            Setup {
                gart_end: 0,
                ..SETUP
            },
            Setup {
                gart_start: 0x1000,
                ..SETUP
            },
            Setup {
                gart_table: 0x18_5fc0_0010,
                ..SETUP
            },
            Setup {
                dummy_page: 0x1_2345_6008,
                ..SETUP
            },
        ] {
            assert!(init(&m, Hub::Gfx, &bad).is_err());
        }
        assert!(m.log.borrow().is_empty());
    }

    #[test]
    fn the_graphics_hub_is_set_up_as_the_specification_orders() {
        let m = Model::new(Hub::Gfx);
        init(&m, Hub::Gfx, &SETUP).unwrap();
        let h = Hub::Gfx;
        // The GART.
        let [(bl, bh), (sl, sh), (el, eh)] = h.context_table(0);
        assert_eq!((m.reg(bl), m.reg(bh)), (0x5fc0_0000, 0x18));
        assert_eq!((m.reg(sl), m.reg(sh)), (0, 0));
        assert_eq!((m.reg(el), m.reg(eh)), (0x3_ffff, 0));
        // Context 0 on, flat, no retry; the rest of the firmware's bits kept.
        let c0 = m.reg(h.context_cntl(0));
        assert_eq!(c0 & 1, 1);
        assert_eq!(c0 >> 1 & 3, 0);
        assert_eq!(c0 & bits::CTX_RETRY, 0);
        assert_eq!(c0 & bits::CTX_FAULT_DEFAULTS, bits::CTX_FAULT_DEFAULTS);
        // The user contexts: enabled, depth 3, the whole space.
        for n in 1..CONTEXTS {
            let c = m.reg(h.context_cntl(n));
            assert_eq!(c & 1, 1, "context {n}");
            assert_eq!(c >> 1 & 3, 3);
            let [_, (sl, sh), (el, eh)] = h.context_table(n);
            assert_eq!((m.reg(sl), m.reg(sh)), (0, 0));
            assert_eq!((m.reg(el), m.reg(eh)), (0xffff_ffff, 0xf));
        }
        // AGP disabled: bottom above top.
        assert!(m.reg(h.reg(Reg::AgpBot)) > m.reg(h.reg(Reg::AgpTop)));
        // The L1 control keeps the ECO bits (10:7 = 0xa gave 0x501's bits 8 and 10).
        let l1 = m.reg(h.reg(Reg::L1Tlb));
        assert_eq!(l1 & 0x500, 0x500, "the firmware's ECO bits survive");
        assert_eq!(l1 & 1, 1);
        assert_eq!(l1 >> 3 & 3, 3);
        assert_eq!(l1 >> 6 & 1, 1);
        assert_eq!(l1 >> 11 & 7, 3);
        // The L2.
        let l2 = m.reg(h.reg(Reg::L2Cntl));
        assert_eq!(l2 & bits::L2_ENABLE, 1);
        assert_eq!(l2 & bits::L2_DEFAULT_PAGE_OUT, bits::L2_DEFAULT_PAGE_OUT);
        assert_eq!(
            l2 & (bits::L2_FRAGMENT_PROCESSING | bits::L2_PDE0_TAG_MODE),
            0
        );
        assert_eq!(l2 >> 19 & 3, 1);
        assert_eq!(m.reg(h.reg(Reg::L2Cntl2)), 3);
        let l3 = m.reg(h.reg(Reg::L2Cntl3));
        assert_eq!((l3 & 0x3f, l3 >> 15 & 0x1f), (9, 6));
        assert_eq!(m.reg(h.reg(Reg::L2Cntl4)) & 0xc0, 0);
        assert_eq!(m.reg(h.reg(Reg::L2Cntl5)) & 0x1f, 0);
        // Faults: defaults on, crash bits off, fault control 2 as Linux leaves it.
        let f = m.reg(h.reg(Reg::FaultCntl));
        assert_eq!(f & bits::FAULT_CRASH, 0);
        assert_eq!(f & bits::FAULT_DEFAULTS, bits::FAULT_DEFAULTS);
        assert_eq!(m.reg(h.reg(Reg::FaultCntl2)), 0x000e_0000);
        assert_eq!(m.reg(h.reg(Reg::FaultDefaultLo)), 0x12_3456);
        assert_eq!(m.reg(h.reg(Reg::FaultDefaultHi)), 0);
        // The identity aperture is off, and every engine takes the full range.
        assert!(m.reg(GFX_IDENTITY[0]) == 0xffff_ffff && m.reg(GFX_IDENTITY[2]) == 0);
        for e in 0..ENGINES {
            let (_, _, _, l, hh) = h.engine(e);
            assert_eq!((m.reg(l), m.reg(hh)), (0xffff_ffff, 0x1f));
        }
    }

    #[test]
    fn the_memory_hub_has_no_retry_interrupt_and_leaves_the_identity_registers_alone() {
        let m = Model::new(Hub::Mm);
        let s = Setup {
            retry_interrupt: false,
            ..SETUP
        };
        init(&m, Hub::Mm, &s).unwrap();
        // 0x20000 from the firmware plus bit 18; bit 19 only for the graphics hub.
        assert_eq!(m.reg(Hub::Mm.reg(Reg::FaultCntl2)), 0x0006_0000);
        assert!(m.log.borrow().iter().all(|w| !GFX_IDENTITY.contains(&w.0)));
    }

    #[test]
    fn the_hub_is_on_only_after_the_gart_is_described_and_the_l2_set_up() {
        let m = Model::new(Hub::Gfx);
        init(&m, Hub::Gfx, &SETUP).unwrap();
        let log = m.log.borrow();
        let h = Hub::Gfx;
        let pos = |at: usize| log.iter().position(|w| w.0 == at).unwrap();
        let on = pos(h.context_cntl(0));
        assert!(pos(h.context_table(0)[0].0) < on);
        assert!(pos(h.context_table(0)[2].0) < on);
        assert!(pos(h.reg(Reg::L1Tlb)) < on);
        assert!(pos(h.reg(Reg::L2Cntl)) < on);
        assert!(pos(h.reg(Reg::AgpBot)) < pos(h.context_table(0)[0].0) + 99);
        // The invalidation engines' ranges come last.
        assert!(pos(h.engine(0).3) > pos(h.context_cntl(15)));
    }

    #[test]
    fn an_invalidation_waits_for_the_acknowledgement_of_its_context() {
        let m = Model::new(Hub::Gfx);
        let n = RefCell::new(0u32);
        assert_eq!(
            invalidate(&m, Hub::Gfx, 0, 10, &|| *n.borrow_mut() += 1),
            Ok(())
        );
        assert_eq!(*n.borrow(), 2, "it polled until the third read");
        let (_, req, ..) = Hub::Gfx.engine(CPU_ENGINE);
        assert_eq!(m.reg(req), 1 | 0xf << 19 | 1 << 23);
        // An engine that never acknowledges times out.
        let m = Model::new(Hub::Gfx);
        *m.ack_after.borrow_mut() = 1_000;
        assert_eq!(
            invalidate(&m, Hub::Gfx, 0, 5, &|| {}),
            Err(InvalidateError::Timeout)
        );
    }

    #[test]
    fn the_memory_hubs_semaphore_is_taken_and_given_back() {
        let m = Model::new(Hub::Mm);
        assert_eq!(invalidate(&m, Hub::Mm, 0, 10, &|| {}), Ok(()));
        assert!(!*m.sem.borrow(), "released");
        let (sem, ..) = Hub::Mm.engine(CPU_ENGINE);
        assert_eq!(m.log.borrow().last().copied(), Some((sem, 0)));
        // Held by another user: never acquired.
        let m = Model::new(Hub::Mm);
        *m.sem.borrow_mut() = true;
        assert_eq!(
            invalidate(&m, Hub::Mm, 0, 5, &|| {}),
            Err(InvalidateError::Semaphore)
        );
        // The request was never written.
        let (_, req, ..) = Hub::Mm.engine(CPU_ENGINE);
        assert!(m.log.borrow().iter().all(|w| w.0 != req));
    }

    #[test]
    fn the_graphics_hub_never_touches_a_semaphore() {
        let m = Model::new(Hub::Gfx);
        invalidate(&m, Hub::Gfx, 0, 10, &|| {}).unwrap();
        let (sem, ..) = Hub::Gfx.engine(CPU_ENGINE);
        assert!(m.log.borrow().iter().all(|w| w.0 != sem));
        assert!(!*m.sem.borrow());
    }

    #[test]
    fn a_ring_switches_a_page_table_and_flushes_in_the_specifications_order() {
        let p = ring_vm_flush(1, 0x18_4a00_0000, false, 0);
        let (lo, hi) = Hub::Gfx.context_table(1)[0];
        let (_, req, ack, _, _) = Hub::Gfx.engine(0);
        // base low, base high, request, wait for the acknowledgement, sync.
        let w = &p.0;
        assert_eq!(
            w[0],
            crate::pm4::header(crate::pm4::op::WRITE_DATA, 4, false)
        );
        assert_eq!(w[2] as usize, lo / 4);
        assert_eq!(w[4], 0x4a00_0001, "the root as a valid directory entry");
        assert_eq!(w[7] as usize, hi / 4);
        assert_eq!(w[9], 0x18);
        assert_eq!(w[12] as usize, req / 4);
        assert_eq!(w[14], invalidate_request(1));
        assert_eq!(
            w[15],
            crate::pm4::header(crate::pm4::op::WAIT_REG_MEM, 6, false)
        );
        assert_eq!(w[17] as usize, ack / 4);
        assert_eq!((w[19], w[20]), (2, 2));
        assert_eq!(*w.last().unwrap(), 0);
        // A root in system memory carries the system bit.
        let p = ring_vm_flush(2, 0x1_2345_6000, true, 1);
        assert_eq!(p.0[4], 0x2345_6003);
        assert_eq!(p.0[9], 1);
    }
}
