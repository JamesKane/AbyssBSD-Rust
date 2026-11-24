// SPDX-License-Identifier: MIT
// Copyright (C) 2022  Advanced Micro Devices, Inc.
// Copyright 2012-2021 Advanced Micro Devices, Inc.
// Copyright 2012-15 Advanced Micro Devices, Inc.
// Copyright 2019 Advanced Micro Devices, Inc.
// Copyright 2019-2026 Advanced Micro Devices, Inc.
// Copyright (c) 2026 The AbyssBSD Project
//! Reading the display state the firmware left (specification 08, section
//! 2.6): plain reads of registers in the direct window, none with side
//! effects.
//!
//! The offsets are computed for Raphael's display block (DCN 3.1.5) from its
//! discovery bases, so they are read only on a chip whose table says exactly
//! that; any other is refused, never guessed at.

use alloc::string::String;
use alloc::vec::Vec;

use crate::discovery::{Discovery, hw};
use crate::regs::Window;

pub use crate::display_regs::REGISTERS;

/// The display block's segment bases on Raphael (01, section 9.1), which the
/// register list was computed from.
const RAPHAEL_DMU_BASES: [u64; 5] = [0x12, 0xc0, 0x34c0, 0x9000, 0x240_3c00];

/// The direct window of Raphael's register BAR, in bytes.
const WINDOW_BYTES: usize = 512 * 1024;

/// Why the display registers were not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The table has no display block.
    NoDisplayBlock,
    /// The display block is not Raphael's (DCN 3.1.5 with its bases): the
    /// register list does not apply.
    NotRaphael,
    /// The display block is listed as fused off.
    Harvested,
}

/// Whether the list applies to the chip `d` describes.
///
/// # Errors
///
/// [`Skip`] saying why not.
pub fn check(d: &Discovery) -> Result<(), Skip> {
    let dmu = d.ip(hw::DMU, 0).ok_or(Skip::NoDisplayBlock)?;
    if d.harvested(hw::DMU, 0) {
        return Err(Skip::Harvested);
    }
    if dmu.version() != (3, 1, 5) || dmu.bases != RAPHAEL_DMU_BASES {
        return Err(Skip::NotRaphael);
    }
    Ok(())
}

/// One register read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Its name, with the instance in place of `<i>`.
    pub name: String,
    /// Its byte offset in the window.
    pub offset: usize,
    /// What it read.
    pub value: u32,
}

/// Reads every register of the list, every instance.
///
/// # Errors
///
/// [`Skip`] if the chip is not the one the list is for.
pub fn read_all<W: Window + ?Sized>(w: &W, d: &Discovery) -> Result<Vec<Reading>, Skip> {
    check(d)?;
    let mut out = Vec::new();
    for (pattern, offsets) in REGISTERS {
        for (i, &offset) in offsets.iter().enumerate() {
            // The list is built for this window; a stray offset past it is
            // skipped, not read.
            if offset % 4 != 0 || offset + 4 > WINDOW_BYTES {
                continue;
            }
            let name = if pattern.contains("<i>") {
                pattern.replace("<i>", &alloc::format!("{i}"))
            } else {
                String::from(*pattern)
            };
            out.push(Reading {
                name,
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

    /// A window that returns each register's own offset and logs reads.
    struct Reads(RefCell<Vec<usize>>);

    impl Window for Reads {
        fn read32(&self, offset: usize) -> u32 {
            self.0.borrow_mut().push(offset);
            u32::try_from(offset).unwrap()
        }
        fn write32(&self, _: usize, _: u32) {
            panic!("the display dump writes nothing");
        }
    }

    #[test]
    fn raphaels_registers_are_read_and_named() {
        let d = parse(FIRST).unwrap();
        assert_eq!(check(&d), Ok(()));
        let w = Reads(RefCell::new(Vec::new()));
        let r = read_all(&w, &d).unwrap();
        assert!(r.len() > 150, "{}", r.len());
        let find = |n: &str| r.iter().find(|x| x.name == n).unwrap();
        assert_eq!(find("OTG0_OTG_CONTROL").offset, 0x14004);
        assert_eq!(find("OTG3_OTG_H_TOTAL").offset, 0x145a8);
        assert_eq!(
            find("HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS").offset,
            0xeb28
        );
        assert_eq!(find("DIG4_DIG_BE_CNTL").offset, 0x165c0);
        assert_eq!(find("DCN_VM_FB_LOCATION_BASE").value, 0xe4d4);
        // Every read was in the direct window, aligned, and listed once.
        let reads = w.0.borrow();
        assert!(reads.iter().all(|&o| o % 4 == 0 && o < 0x80000));
        assert_eq!(reads.len(), r.len());
    }

    #[test]
    fn another_chip_is_refused_and_nothing_is_read() {
        let mut d = parse(FIRST).unwrap();
        let w = Reads(RefCell::new(Vec::new()));
        // A display block of another version.
        let dmu = d.dies[0]
            .ips
            .iter_mut()
            .find(|i| i.hw_id == hw::DMU)
            .unwrap();
        dmu.minor = 0;
        assert_eq!(read_all(&w, &d).unwrap_err(), Skip::NotRaphael);
        // The right version with other bases (a different chip's table).
        let mut d = parse(FIRST).unwrap();
        let dmu = d.dies[0]
            .ips
            .iter_mut()
            .find(|i| i.hw_id == hw::DMU)
            .unwrap();
        dmu.bases[2] += 4;
        assert_eq!(read_all(&w, &d).unwrap_err(), Skip::NotRaphael);
        // No display block at all, or fused off.
        let mut d = parse(FIRST).unwrap();
        d.dies[0].ips.retain(|i| i.hw_id != hw::DMU);
        assert_eq!(read_all(&w, &d).unwrap_err(), Skip::NoDisplayBlock);
        let mut d = parse(FIRST).unwrap();
        d.harvest.push(crate::discovery::Harvested {
            hw_id: hw::DMU,
            instance: 0,
        });
        assert_eq!(read_all(&w, &d).unwrap_err(), Skip::Harvested);
        assert!(w.0.borrow().is_empty());
    }

    #[test]
    fn the_list_is_what_the_specification_names() {
        // Spot checks against the table in 08 section 2.6.
        let find = |n: &str| REGISTERS.iter().find(|(p, _)| *p == n).unwrap().1;
        assert_eq!(find("DMCUB_CNTL"), [0xdad8]);
        assert_eq!(
            find("DIG<i>_DIG_BE_EN_CNTL"),
            [0x155c4, 0x159c4, 0x15dc4, 0x161c4, 0x165c4]
        );
        assert!(REGISTERS.iter().all(|(_, o)| o.iter().all(|x| x % 4 == 0)));
    }
}
