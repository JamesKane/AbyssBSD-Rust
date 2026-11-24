// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! Reading the memory, interrupt and doorbell registers the way the
//! firmware left them, before stage R2 writes any (specification 05): plain
//! reads of registers in the direct window.
//!
//! The offsets are computed for Raphael's blocks from their discovery
//! bases (05, the notation note), so they are read only on a chip whose
//! table says exactly that; any other is refused, never guessed at.

use alloc::string::String;
use alloc::vec::Vec;

use crate::discovery::{Discovery, Ip, hw};
use crate::display::Reading;
use crate::regs::Window;

pub use crate::memory_regs::REGISTERS;

/// The direct window of Raphael's register BAR, in bytes.
const WINDOW_BYTES: usize = 512 * 1024;

/// Why the registers were not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The table lacks one of the blocks.
    NoBlock(u16),
    /// A block is not Raphael's version, or has other bases: the list does
    /// not apply.
    NotRaphael(u16),
}

/// The blocks the list is computed from: id, version, and the segment bases
/// that matter (05, line 7 and 01 section 9.1).
/// A block: its id, version and the leading segment bases.
type Block = (u16, (u8, u8, u8), &'static [u64]);

const BLOCKS: [Block; 5] = [
    (hw::GC, (10, 3, 6), &[0x1260]),
    (hw::MMHUB, (2, 4, 1), &[0x1_3200, 0x1_a000]),
    (hw::OSSSYS, (5, 2, 1), &[0x10a0]),
    (hw::HDP, (5, 2, 0), &[0xf20]),
    (hw::NBIF, (7, 3, 0), &[0x0, 0x14, 0xd20, 0x1_0400]),
];

fn matches(ip: &Ip, version: (u8, u8, u8), bases: &[u64]) -> bool {
    ip.version() == version && ip.bases.get(..bases.len()) == Some(bases)
}

/// Whether the list applies to the chip `d` describes.
///
/// # Errors
///
/// [`Skip`] saying which block is missing or different.
pub fn check(d: &Discovery) -> Result<(), Skip> {
    for (id, version, bases) in BLOCKS {
        let ip = d.ip(id, 0).ok_or(Skip::NoBlock(id))?;
        if !matches(ip, version, bases) {
            return Err(Skip::NotRaphael(id));
        }
    }
    Ok(())
}

/// Reads every register of the list, every instance.
///
/// # Errors
///
/// [`Skip`] if the chip is not the one the list is for.
pub fn read_all<W: Window + ?Sized>(w: &W, d: &Discovery) -> Result<Vec<Reading>, Skip> {
    check(d)?;
    let mut out = Vec::new();
    for (name, offsets) in REGISTERS {
        for &offset in *offsets {
            // The list is built for this window; a stray offset past it is
            // skipped, not read.
            if offset % 4 != 0 || offset + 4 > WINDOW_BYTES {
                continue;
            }
            out.push(Reading {
                name: String::from(*name),
                offset,
                value: w.read32(offset),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::parse;
    use core::cell::RefCell;

    const FIRST: &[u8] = include_bytes!("../../../tests/corpus/x870e/linux/amdgpu/discovery.bin");

    struct Reads(RefCell<Vec<usize>>);

    impl Window for Reads {
        fn read32(&self, offset: usize) -> u32 {
            self.0.borrow_mut().push(offset);
            u32::try_from(offset).unwrap()
        }
        fn write32(&self, _: usize, _: u32) {
            panic!("the capture writes nothing");
        }
    }

    #[test]
    fn raphaels_blocks_match_the_list() {
        let d = parse(FIRST).unwrap();
        assert_eq!(check(&d), Ok(()));
    }

    #[test]
    fn the_registers_are_read_once_each_and_none_is_a_semaphore() {
        let d = parse(FIRST).unwrap();
        let w = Reads(RefCell::new(Vec::new()));
        let r = read_all(&w, &d).unwrap();
        assert!(r.len() > 100, "{}", r.len());
        let reads = w.0.borrow();
        assert_eq!(reads.len(), r.len());
        // Aligned, in the window, and not listed twice.
        assert!(reads.iter().all(|&o| o % 4 == 0 && o < 0x80000));
        let mut sorted = reads.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), reads.len(), "an offset is listed twice");
        // The semaphores: GFXHUB 0xa1b4 + 4n, MMHUB 0x6a800 + 32n.
        assert!(reads.iter().all(|&o| !(0xa1b4..0xa1fc).contains(&o)));
        assert!(
            reads
                .iter()
                .all(|&o| o < 0x6a800 || (o - 0x6a800) % 32 != 0 || o > 0x6a800 + 32 * 17)
        );
        let find = |n: &str| r.iter().find(|x| x.name == n).unwrap().offset;
        assert_eq!(find("GCMC_VM_FB_LOCATION_BASE"), 0xa570);
        assert_eq!(find("MMVM_CONTEXT0_PAGE_TABLE_END_ADDR_HI32"), 0x6a514);
        assert_eq!(find("MMVM_CONTEXT1_PAGE_TABLE_END_ADDR_HI32"), 0x6a534);
        assert_eq!(find("IH_RB_CNTL"), 0x4480);
        assert_eq!(find("RCC_DEV0_EPF0_RCC_DOORBELL_APER_EN"), 0x3780);
    }

    #[test]
    fn another_chip_is_refused_and_nothing_is_read() {
        let w = Reads(RefCell::new(Vec::new()));
        // A block of another version.
        let mut d = parse(FIRST).unwrap();
        d.dies[0]
            .ips
            .iter_mut()
            .find(|i| i.hw_id == hw::OSSSYS)
            .unwrap()
            .minor = 0;
        assert_eq!(read_all(&w, &d).unwrap_err(), Skip::NotRaphael(hw::OSSSYS));
        // Other bases.
        let mut d = parse(FIRST).unwrap();
        d.dies[0]
            .ips
            .iter_mut()
            .find(|i| i.hw_id == hw::MMHUB)
            .unwrap()
            .bases[1] += 4;
        assert_eq!(read_all(&w, &d).unwrap_err(), Skip::NotRaphael(hw::MMHUB));
        // A block missing.
        let mut d = parse(FIRST).unwrap();
        d.dies[0].ips.retain(|i| i.hw_id != hw::HDP);
        assert_eq!(read_all(&w, &d).unwrap_err(), Skip::NoBlock(hw::HDP));
        assert!(w.0.borrow().is_empty());
    }
}
