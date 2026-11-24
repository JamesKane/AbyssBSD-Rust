// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The interrupt handler's ring (IH 5.2, specification 05 section 8): the
//! register sequence that makes the ring, and the decoding and draining of
//! the 32-byte entries the hardware writes to it.
//!
//! Everything here works on a register window and on closures that read the
//! ring's memory, so the sequences and the ring arithmetic are tested on the
//! host. The memory (the ring and its write-back slot) is the caller's.

use crate::regs::Window;

/// `IH_RB_CNTL`.
pub const RB_CNTL: usize = 0x4480;
/// `IH_RB_BASE`: the ring's address >> 8.
pub const RB_BASE: usize = 0x4484;
/// `IH_RB_BASE_HI`: bits 47:40 of the ring's address.
pub const RB_BASE_HI: usize = 0x4488;
/// `IH_RB_RPTR`: the read pointer, a byte offset.
pub const RB_RPTR: usize = 0x448c;
/// `IH_RB_WPTR`: bit 0 overflow, bits 17:2 the write pointer's byte offset.
pub const RB_WPTR: usize = 0x4490;
/// `IH_RB_WPTR_ADDR_HI`.
pub const RB_WPTR_ADDR_HI: usize = 0x4494;
/// `IH_RB_WPTR_ADDR_LO`.
pub const RB_WPTR_ADDR_LO: usize = 0x4498;
/// `IH_DOORBELL_RPTR`.
pub const DOORBELL_RPTR: usize = 0x449c;
/// `IH_RB_CNTL_RING1` and `_RING2`: the two rings this driver leaves off.
pub const RB_CNTL_RING1: usize = 0x44b0;
/// See [`RB_CNTL_RING1`].
pub const RB_CNTL_RING2: usize = 0x44e0;
/// `IH_CNTL2`.
pub const CNTL2: usize = 0x4584;
/// `BIF_BX0_INTERRUPT_CNTL` (the bus interface's).
pub const BIF_INTERRUPT_CNTL: usize = 0x3844;
/// `BIF_BX0_INTERRUPT_CNTL2`: the dummy page's address >> 8.
pub const BIF_INTERRUPT_CNTL2: usize = 0x3848;

/// `IH_RB_CNTL` bits.
pub mod cntl {
    /// `RB_ENABLE`.
    pub const ENABLE: u32 = 1 << 0;
    /// `RB_SIZE` (5:1): log2 of the ring's size in dwords.
    pub const SIZE_SHIFT: u32 = 1;
    /// `RB_GPU_TS_ENABLE`.
    pub const GPU_TS: u32 = 1 << 7;
    /// `WPTR_WRITEBACK_ENABLE`.
    pub const WPTR_WRITEBACK: u32 = 1 << 8;
    /// `WPTR_OVERFLOW_ENABLE`.
    pub const WPTR_OVERFLOW_ENABLE: u32 = 1 << 16;
    /// `ENABLE_INTR`.
    pub const ENABLE_INTR: u32 = 1 << 17;
    /// `MC_SNOOP`.
    pub const MC_SNOOP: u32 = 1 << 20;
    /// `RPTR_REARM`.
    pub const RPTR_REARM: u32 = 1 << 21;
    /// `MC_RO`.
    pub const MC_RO: u32 = 1 << 22;
    /// `MC_VMID` (27:24).
    pub const MC_VMID_MASK: u32 = 0xf << 24;
    /// `MC_SPACE` (30:28), as a shifted field.
    pub const MC_SPACE_SHIFT: u32 = 28;
    /// `MC_SPACE`'s mask.
    pub const MC_SPACE_MASK: u32 = 0x7 << MC_SPACE_SHIFT;
    /// `WPTR_OVERFLOW_CLEAR`.
    pub const WPTR_OVERFLOW_CLEAR: u32 = 1 << 31;
}

/// The size of an entry.
pub const ENTRY_BYTES: u32 = 32;

/// `MC_SPACE` for a GPU (MC) address.
const MC_SPACE_GPU: u32 = 4;

/// Where the ring is and how big.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ring {
    /// The ring's GPU (MC) address, 256-byte aligned.
    pub address: u64,
    /// Its size in bytes: a power of two, a whole number of entries.
    pub bytes: u32,
    /// The GPU address of the 8-byte write-back slot.
    pub writeback: u64,
}

/// Why a ring cannot be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// The size is not a power of two of at least one entry, or too big
    /// for the write pointer's field.
    Size,
    /// An address is misaligned or beyond 48 bits.
    Address,
}

impl Ring {
    /// Checks the ring is one the hardware takes.
    ///
    /// # Errors
    ///
    /// [`RingError`] for a size or address it does not take.
    pub fn check(&self) -> Result<(), RingError> {
        if !self.bytes.is_power_of_two() || self.bytes < ENTRY_BYTES || self.bytes > 1 << 18 {
            return Err(RingError::Size);
        }
        if !self.address.is_multiple_of(256)
            || self.address >> 48 != 0
            || !self.writeback.is_multiple_of(4)
            || self.writeback >> 48 != 0
        {
            return Err(RingError::Address);
        }
        Ok(())
    }

    /// The value of `IH_RB_CNTL` that makes this ring, from the register's
    /// current value `old`: the ring's size, a GPU address space, the
    /// write pointer written back, snooping on, relaxed ordering and the
    /// context off, overflow handling on. Not yet enabled.
    #[must_use]
    pub fn control(&self, old: u32, rearm: bool) -> u32 {
        let mut v = old;
        v &= !(0x1f << cntl::SIZE_SHIFT | cntl::MC_RO | cntl::MC_VMID_MASK | cntl::MC_SPACE_MASK);
        v |= (self.bytes / 4).trailing_zeros() << cntl::SIZE_SHIFT;
        v |= MC_SPACE_GPU << cntl::MC_SPACE_SHIFT;
        v |= cntl::WPTR_OVERFLOW_CLEAR
            | cntl::WPTR_OVERFLOW_ENABLE
            | cntl::WPTR_WRITEBACK
            | cntl::MC_SNOOP;
        if rearm {
            v |= cntl::RPTR_REARM;
        }
        v &= !(cntl::ENABLE | cntl::ENABLE_INTR | cntl::GPU_TS);
        v
    }
}

/// Makes the ring and turns the interrupt handler on, in the order of
/// specification 05 section 8.2 (steps 1, 2, 3, 4, 6 and 7; the doorbell
/// steps wait for stage R2.4, and the caller enables bus mastering).
///
/// The ring's memory must be zeroed first, and `dummy_page` (a GPU address
/// the interrupt handler reads when it has nothing to read) must be one the
/// caller keeps for as long as the handler runs.
///
/// # Errors
///
/// [`RingError`] for a ring the hardware does not take; nothing is written.
pub fn bring_up<W: Window + ?Sized>(
    w: &W,
    ring: &Ring,
    dummy_page: u64,
    rearm: bool,
) -> Result<(), RingError> {
    ring.check()?;
    // 1. With the ring disabled: the bus interface's dummy page, and its
    //    dummy-read override and non-snoop request bits clear.
    w.write32(RB_CNTL, w.read32(RB_CNTL) & !cntl::ENABLE);
    w.write32(
        BIF_INTERRUPT_CNTL2,
        u32::try_from(dummy_page >> 8).unwrap_or(0),
    );
    w.write32(
        BIF_INTERRUPT_CNTL,
        w.read32(BIF_INTERRUPT_CNTL) & !(1 << 0 | 1 << 3),
    );
    // 2. The ring's base.
    w.write32(
        RB_BASE,
        u32::try_from((ring.address >> 8) & 0xffff_ffff).unwrap_or(0),
    );
    w.write32(
        RB_BASE_HI,
        u32::try_from((ring.address >> 40) & 0xff).unwrap_or(0),
    );
    // 3. The control word, the ring still off.
    let control = ring.control(w.read32(RB_CNTL), rearm);
    w.write32(RB_CNTL, control);
    // 4. The write-back slot, and the pointers at 0.
    w.write32(
        RB_WPTR_ADDR_LO,
        u32::try_from(ring.writeback & 0xffff_fffc).unwrap_or(0),
    );
    w.write32(
        RB_WPTR_ADDR_HI,
        u32::try_from((ring.writeback >> 32) & 0xffff).unwrap_or(0),
    );
    w.write32(RB_RPTR, 0);
    w.write32(RB_WPTR, 0);
    // 6. On.
    w.write32(
        RB_CNTL,
        control | cntl::ENABLE | cntl::ENABLE_INTR | cntl::GPU_TS,
    );
    // 7. The self-interrupt's forced write pointer update, and the unused
    //    rings' thresholds at 0 (their size fields stay 0: the rings are off).
    w.write32(CNTL2, (w.read32(CNTL2) & !0x1f) | 8 | 1 << 8);
    Ok(())
}

/// Turns the handler off: the ring disabled, its interrupt off.
pub fn disable<W: Window + ?Sized>(w: &W) {
    let v = w.read32(RB_CNTL);
    w.write32(RB_CNTL, v & !(cntl::ENABLE | cntl::ENABLE_INTR));
}

/// An entry the interrupt handler wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The client (05 section 8.5's table: the display is 4).
    pub client: u8,
    /// The source within the client.
    pub source: u8,
    /// The ring (queue) that raised it.
    pub ring: u8,
    /// The VMID.
    pub vmid: u8,
    /// Whether the VMID is the memory hub's (not the graphics hub's).
    pub vmid_mmhub: bool,
    /// The 48-bit timestamp.
    pub timestamp: u64,
    /// The process address space ID.
    pub pasid: u16,
    /// The source data words.
    pub data: [u32; 4],
}

impl Entry {
    /// Decodes the eight dwords of an entry (05 section 8.4).
    #[must_use]
    pub fn decode(d: [u32; 8]) -> Self {
        Self {
            client: (d[0] & 0xff) as u8,
            source: (d[0] >> 8 & 0xff) as u8,
            ring: (d[0] >> 16 & 0xff) as u8,
            vmid: (d[0] >> 24 & 0xf) as u8,
            vmid_mmhub: d[0] >> 31 & 1 != 0,
            timestamp: u64::from(d[1]) | u64::from(d[2] & 0xffff) << 32,
            pasid: (d[3] & 0xffff) as u16,
            data: [d[4], d[5], d[6], d[7]],
        }
    }
}

/// The write pointer from a write-back slot's first dword (or the
/// register): the byte offset, and whether the ring overflowed.
#[must_use]
pub const fn write_pointer(v: u32) -> (u32, bool) {
    (v & 0x3_fffc, v & 1 != 0)
}

/// What a drain did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drained {
    /// The entries handled.
    pub entries: u32,
    /// The read pointer to give the hardware.
    pub rptr: u32,
    /// Whether the hardware reported the ring overflowed (the entries since
    /// were lost; the read pointer was moved past them).
    pub overflowed: bool,
}

/// Handles the entries from read pointer `rptr` up to write pointer
/// `wptr_word` (the write-back slot's value), reading each with `read` (a
/// dword at a byte offset of the ring) and calling `f` on it. On overflow
/// the oldest entries are gone: reading restarts at the write pointer plus
/// one entry, as the specification says (05 section 8.3 step 1).
///
/// A write pointer that is not inside the ring, or not a whole entry from
/// the read pointer, handles nothing: the hardware's word is not believed.
pub fn drain(
    ring_bytes: u32,
    rptr: u32,
    wptr_word: u32,
    read: &dyn Fn(u32) -> u32,
    f: &mut dyn FnMut(Entry),
) -> Drained {
    let mask = ring_bytes.wrapping_sub(1);
    let (wptr, overflow) = write_pointer(wptr_word);
    let mut out = Drained {
        entries: 0,
        rptr,
        overflowed: overflow,
    };
    if wptr >= ring_bytes
        || rptr >= ring_bytes
        || !wptr.is_multiple_of(4)
        || !rptr.is_multiple_of(32)
    {
        return out;
    }
    let mut at = if overflow {
        // The entries between the write pointer and one lap on are the
        // oldest the ring still holds.
        (wptr + ENTRY_BYTES) & mask
    } else {
        rptr
    };
    // A write pointer in the middle of an entry has not finished it.
    let end = wptr & !(ENTRY_BYTES - 1);
    // At most one lap, so a hostile pointer cannot loop forever.
    let mut left = ring_bytes / ENTRY_BYTES;
    while at != end && left > 0 {
        let mut d = [0u32; 8];
        for (word, offset) in d.iter_mut().zip((0..ENTRY_BYTES).step_by(4)) {
            *word = read(at + offset);
        }
        f(Entry::decode(d));
        out.entries += 1;
        at = (at + ENTRY_BYTES) & mask;
        left -= 1;
    }
    out.rptr = at;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    struct Model {
        regs: RefCell<alloc::collections::BTreeMap<usize, u32>>,
        log: RefCell<Vec<(usize, u32)>>,
    }

    impl Model {
        fn new() -> Self {
            let m = Self {
                regs: RefCell::new(alloc::collections::BTreeMap::new()),
                log: RefCell::new(Vec::new()),
            };
            // The firmware's state (the capture): ring off, reset control.
            m.regs.borrow_mut().insert(RB_CNTL, 0x4061_0000);
            m.regs
                .borrow_mut()
                .insert(BIF_INTERRUPT_CNTL, 0x1_0000 | 1 << 3);
            m.regs.borrow_mut().insert(CNTL2, 8);
            m
        }
        fn reg(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
    }

    impl Window for Model {
        fn read32(&self, offset: usize) -> u32 {
            self.reg(offset)
        }
        fn write32(&self, offset: usize, value: u32) {
            self.log.borrow_mut().push((offset, value));
            self.regs.borrow_mut().insert(offset, value);
        }
    }

    const RING: Ring = Ring {
        address: 0xf4_0800_0000,
        bytes: 64 * 1024,
        writeback: 0xf4_0801_0000,
    };

    #[test]
    fn the_ring_is_checked_before_anything_is_written() {
        let m = Model::new();
        let mut bad = RING;
        bad.bytes = 3000;
        assert_eq!(
            bring_up(&m, &bad, 0xf4_0802_0000, true),
            Err(RingError::Size)
        );
        bad = RING;
        bad.address += 16;
        assert_eq!(
            bring_up(&m, &bad, 0xf4_0802_0000, true),
            Err(RingError::Address)
        );
        bad = RING;
        bad.bytes = 1 << 19;
        assert_eq!(
            bring_up(&m, &bad, 0xf4_0802_0000, true),
            Err(RingError::Size)
        );
        assert!(m.log.borrow().is_empty());
    }

    #[test]
    fn bring_up_follows_the_specification_and_the_firmwares_state() {
        let m = Model::new();
        bring_up(&m, &RING, 0xf4_0802_0000, true).unwrap();
        assert_eq!(
            u64::from(m.reg(RB_BASE)),
            (0xf4_0800_0000u64 >> 8) & 0xffff_ffff
        );
        assert_eq!(m.reg(RB_BASE_HI), 0);
        assert_eq!(m.reg(RB_WPTR_ADDR_LO), 0x0801_0000);
        assert_eq!(m.reg(RB_WPTR_ADDR_HI), 0xf4);
        assert_eq!(m.reg(RB_RPTR), 0);
        assert_eq!(m.reg(RB_WPTR), 0);
        assert_eq!(
            m.reg(BIF_INTERRUPT_CNTL2),
            u32::try_from(0xf4_0802_0000u64 >> 8).unwrap()
        );
        // Bits 0 and 3 cleared, bit 16 (the firmware's) kept.
        assert_eq!(m.reg(BIF_INTERRUPT_CNTL), 0x1_0000);
        let c = m.reg(RB_CNTL);
        assert_eq!(c & cntl::ENABLE, cntl::ENABLE);
        assert_eq!(c & cntl::ENABLE_INTR, cntl::ENABLE_INTR);
        assert_eq!(c & cntl::GPU_TS, cntl::GPU_TS);
        assert_eq!(c >> cntl::SIZE_SHIFT & 0x1f, 14, "64 KiB is 2^14 dwords");
        assert_eq!(c >> cntl::MC_SPACE_SHIFT & 7, 4);
        assert_eq!(c & cntl::MC_RO, 0, "the firmware's MC_RO is cleared");
        assert_eq!(c & cntl::MC_SNOOP, cntl::MC_SNOOP);
        assert_eq!(c & cntl::RPTR_REARM, cntl::RPTR_REARM);
        assert_eq!(c & cntl::WPTR_OVERFLOW_ENABLE, cntl::WPTR_OVERFLOW_ENABLE);
        assert_eq!(m.reg(CNTL2) & 0x1ff, 8 | 1 << 8);
        // The ring is enabled last.
        let log = m.log.borrow();
        let on = log.iter().rposition(|w| w.0 == RB_CNTL).unwrap();
        let last_pointer = log.iter().rposition(|w| w.0 == RB_WPTR).unwrap();
        assert!(last_pointer < on);
        // Without a message-signalled interrupt the re-arm bit is not set.
        let m = Model::new();
        bring_up(&m, &RING, 0xf4_0802_0000, false).unwrap();
        assert_eq!(
            m.reg(RB_CNTL) & cntl::RPTR_REARM,
            cntl::RPTR_REARM,
            "the firmware's value stays"
        );
    }

    #[test]
    fn disabling_turns_the_ring_and_its_interrupt_off_and_nothing_else() {
        let m = Model::new();
        bring_up(&m, &RING, 0xf4_0802_0000, true).unwrap();
        let before = m.reg(RB_CNTL);
        disable(&m);
        assert_eq!(m.reg(RB_CNTL), before & !(cntl::ENABLE | cntl::ENABLE_INTR));
    }

    fn entry_words(client: u8, source: u8, n: u32) -> [u32; 8] {
        [
            u32::from(client) | u32::from(source) << 8 | 0x3 << 16,
            n,
            0x1234,
            0xbeef,
            n + 1,
            n + 2,
            n + 3,
            n + 4,
        ]
    }

    #[test]
    fn an_entry_is_decoded_as_the_specification_lays_it_out() {
        let e = Entry::decode([
            0x8a05_3c04, // client 4, source 0x3c, ring 5, vmid 0xa, mmhub
            0x1111_2222,
            0x8000_3333, // timestamp bits 47:32, and the source bit
            0x0042_beef, // pasid 0xbeef, node 0x42
            1,
            2,
            3,
            4,
        ]);
        assert_eq!((e.client, e.source, e.ring, e.vmid), (4, 0x3c, 5, 0xa));
        assert!(e.vmid_mmhub);
        assert_eq!(e.timestamp, 0x3333_1111_2222);
        assert_eq!(e.pasid, 0xbeef);
        assert_eq!(e.data, [1, 2, 3, 4]);
    }

    /// A ring of `entries` entries in memory, with `n` of them written from slot `from`.
    fn ring_with(entries: u32, from: u32, n: u32) -> Vec<u32> {
        let mut m = alloc::vec![0u32; (entries * 8) as usize];
        for k in 0..n {
            let slot = ((from + k) % entries) as usize;
            m[slot * 8..slot * 8 + 8].copy_from_slice(&entry_words(4, 0x3c, from + k));
        }
        m
    }

    #[test]
    fn draining_reads_from_the_read_pointer_to_the_write_pointer() {
        let mem = ring_with(8, 0, 3);
        let mut seen = Vec::new();
        let d = drain(8 * 32, 0, 3 * 32, &|o| mem[(o / 4) as usize], &mut |e| {
            seen.push(e.timestamp & 0xffff_ffff)
        });
        assert_eq!(
            d,
            Drained {
                entries: 3,
                rptr: 96,
                overflowed: false
            }
        );
        assert_eq!(seen, [0, 1, 2]);
        // Nothing new: nothing handled.
        let d = drain(
            8 * 32,
            96,
            96,
            &|o| mem[(o / 4) as usize],
            &mut |_| panic!(),
        );
        assert_eq!(d.entries, 0);
    }

    #[test]
    fn the_ring_wraps() {
        let mem = ring_with(8, 6, 4); // slots 6, 7, 0, 1
        let mut seen = Vec::new();
        let d = drain(
            8 * 32,
            6 * 32,
            2 * 32,
            &|o| mem[(o / 4) as usize],
            &mut |e| {
                seen.push(e.timestamp & 0xffff_ffff);
            },
        );
        assert_eq!(d.entries, 4);
        assert_eq!(d.rptr, 2 * 32);
        assert_eq!(seen, [6, 7, 8, 9]);
    }

    #[test]
    fn an_overflow_restarts_at_the_oldest_entry_the_ring_holds() {
        // The ring is full of entries 8..16; the write pointer is at slot 3.
        let mut mem = alloc::vec![0u32; 8 * 8];
        for k in 8u32..16 {
            let slot = (k % 8) as usize;
            mem[slot * 8..slot * 8 + 8].copy_from_slice(&entry_words(4, 0x3c, k));
        }
        let mut seen = Vec::new();
        let d = drain(
            8 * 32,
            5 * 32,
            (3 * 32) | 1,
            &|o| mem[(o / 4) as usize],
            &mut |e| {
                seen.push(e.timestamp & 0xffff_ffff);
            },
        );
        assert!(d.overflowed);
        assert_eq!(
            seen,
            [12, 13, 14, 15, 8, 9, 10],
            "slots 4 to 7, then 0 to 2: one entry short of a lap"
        );
        assert_eq!(d.rptr, 3 * 32);
    }

    #[test]
    fn a_pointer_the_hardware_could_not_have_written_handles_nothing() {
        let mem = ring_with(8, 0, 8);
        let read = |o: u32| mem[(o / 4) as usize];
        // A write pointer past the ring (the field is wider than it), a read
        // pointer past it, or a read pointer in the middle of an entry.
        for (rptr, w) in [(0, 8 * 32), (0, 0x3_fffc), (8 * 32, 32), (16, 32)] {
            let d = drain(8 * 32, rptr, w, &read, &mut |_| panic!("handled an entry"));
            assert_eq!((d.entries, d.rptr), (0, rptr));
        }
    }

    #[test]
    fn a_write_pointer_inside_an_entry_waits_for_the_rest() {
        let mem = ring_with(8, 0, 3);
        let mut n = 0;
        let d = drain(
            8 * 32,
            0,
            2 * 32 + 12,
            &|o| mem[(o / 4) as usize],
            &mut |_| n += 1,
        );
        assert_eq!((n, d.rptr), (2, 64));
    }
}
