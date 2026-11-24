// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! Flipping the display's surface (the specification's 08 §2.4): the
//! firmware's display is adopted as it is, and only the address of the
//! surface it shows changes. Pitch, format, timing and watermarks stay as
//! the firmware left them (stage R1 of GRAPHICS.md §16).

use crate::regs::Window;

/// `DCN_VM_FB_LOCATION_BASE` (byte offset in BAR5): the first video memory
/// address the display sees, in 16 MiB units.
pub const FB_LOCATION_BASE: usize = 0xe4d4;
/// `OTG0_OTG_CONTROL`: bit 0 is the timing generator's enable.
pub const OTG0_CONTROL: usize = 0x14004;
/// `HUBPREQ0_DCSURF_SURFACE_PITCH`: the pitch in pixels, minus one.
pub const SURFACE_PITCH: usize = 0xeb1c;
/// `HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS`: bits 31:0 of the surface's
/// address; writing it latches the whole address.
pub const SURFACE_ADDRESS: usize = 0xeb28;
/// `HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH`: bits 47:32.
pub const SURFACE_ADDRESS_HIGH: usize = 0xeb2c;
/// `HUBPREQ0_DCSURF_FLIP_CONTROL`: bit 1 is the flip type (0: at the next
/// vertical update), bit 8 reads 1 while a flip is pending.
pub const FLIP_CONTROL: usize = 0xeb6c;
/// The flip-pending bit of [`FLIP_CONTROL`].
pub const FLIP_PENDING: u32 = 1 << 8;
/// The immediate-flip bit of [`FLIP_CONTROL`].
pub const FLIP_IMMEDIATE: u32 = 1 << 1;

/// The host-data-path flush: write to this byte offset of BAR5 once it has
/// been pointed there by [`HDP_MEM_FLUSH_REMAP`] (05 §7).
pub const HDP_FLUSH_PAGE: usize = 0x7f000;
/// `BIF_BX0_REMAP_HDP_MEM_FLUSH_CNTL`.
pub const HDP_MEM_FLUSH_REMAP: usize = 0x3934;
/// `BIF_BX0_REMAP_HDP_REG_FLUSH_CNTL`.
pub const HDP_REG_FLUSH_REMAP: usize = 0x3938;
/// `HDP_MISC_CNTL`: bit 0 is flush-invalidate.
pub const HDP_MISC_CNTL: usize = 0x3fcc;
/// `RCC_CONFIG_MEMSIZE`, read to complete a posted write.
const MEMSIZE: usize = 0x378c;

/// Why the display cannot be adopted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotAdoptable {
    /// The timing generator is off: nothing is on the screen.
    NoTiming,
    /// A flip is in progress already.
    FlipPending,
    /// The surface is not at the start of video memory, where the firmware's
    /// framebuffer is.
    SurfaceMoved {
        /// Where the surface is.
        address: u64,
        /// Where video memory starts.
        base: u64,
    },
}

/// The display's state read before anything is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adopted {
    /// The GPU address of the first byte of video memory.
    pub vram_base: u64,
    /// The surface's pitch in pixels.
    pub pitch_pixels: u32,
}

/// The GPU address the surface shows now.
pub fn surface<W: Window + ?Sized>(w: &W) -> u64 {
    let low = u64::from(w.read32(SURFACE_ADDRESS));
    let high = u64::from(w.read32(SURFACE_ADDRESS_HIGH) & 0xffff);
    (high << 32) | low
}

/// Checks that the display is the firmware's, running, and showing the first
/// byte of video memory, and returns what a flip needs.
///
/// # Errors
///
/// [`NotAdoptable`] says which part is not as expected.
pub fn adopt<W: Window + ?Sized>(w: &W) -> Result<Adopted, NotAdoptable> {
    if w.read32(OTG0_CONTROL) & 1 == 0 {
        return Err(NotAdoptable::NoTiming);
    }
    if w.read32(FLIP_CONTROL) & FLIP_PENDING != 0 {
        return Err(NotAdoptable::FlipPending);
    }
    let base = u64::from(w.read32(FB_LOCATION_BASE) & 0xffff) << 24;
    let address = surface(w);
    if address != base {
        return Err(NotAdoptable::SurfaceMoved { address, base });
    }
    Ok(Adopted {
        vram_base: base,
        pitch_pixels: (w.read32(SURFACE_PITCH) & 0x3fff) + 1,
    })
}

/// Points the host-data-path flush at its page and enables it (once, before
/// the first [`hdp_flush`]).
pub fn hdp_init<W: Window + ?Sized>(w: &W) {
    let misc = w.read32(HDP_MISC_CNTL);
    w.write32(HDP_MISC_CNTL, misc | 1);
    w.write32(HDP_MEM_FLUSH_REMAP, 0x7f000);
    w.write32(HDP_REG_FLUSH_REMAP, 0x7f004);
}

/// Makes the processor's writes to video memory visible to the display.
pub fn hdp_flush<W: Window + ?Sized>(w: &W) {
    w.write32(HDP_FLUSH_PAGE, 0);
    // The read completes the posted write.
    let _ = w.read32(MEMSIZE);
}

/// Shows the surface at GPU address `address`, from the next vertical
/// update (a tear-free flip). The high half goes first: the write of the
/// low half latches the address.
#[allow(clippy::cast_possible_truncation)]
pub fn flip<W: Window + ?Sized>(w: &W, address: u64) {
    let control = w.read32(FLIP_CONTROL);
    w.write32(FLIP_CONTROL, control & !FLIP_IMMEDIATE);
    w.write32(SURFACE_ADDRESS_HIGH, ((address >> 32) & 0xffff) as u32);
    w.write32(SURFACE_ADDRESS, (address & 0xffff_ffff) as u32);
}

/// Whether a flip is still waiting for its vertical update.
pub fn pending<W: Window + ?Sized>(w: &W) -> bool {
    w.read32(FLIP_CONTROL) & FLIP_PENDING != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    struct Model {
        regs: RefCell<alloc::vec::Vec<u32>>,
        log: RefCell<alloc::vec::Vec<(usize, u32)>>,
    }

    impl Model {
        fn gop() -> Self {
            let m = Self {
                regs: RefCell::new(alloc::vec![0; 0x20000]),
                log: RefCell::new(alloc::vec::Vec::new()),
            };
            m.set(OTG0_CONTROL, 1);
            m.set(FB_LOCATION_BASE, 0xf400);
            m.set(SURFACE_ADDRESS_HIGH, 0xf4);
            m.set(SURFACE_PITCH, 0x77f);
            m.set(FLIP_CONTROL, 0x0410_0000);
            m
        }
        fn set(&self, at: usize, v: u32) {
            self.regs.borrow_mut()[at / 4] = v;
        }
    }

    impl Window for Model {
        fn read32(&self, offset: usize) -> u32 {
            self.regs.borrow()[offset / 4]
        }
        fn write32(&self, offset: usize, value: u32) {
            self.log.borrow_mut().push((offset, value));
            self.regs.borrow_mut()[offset / 4] = value;
        }
    }

    #[test]
    fn the_gop_state_is_adopted() {
        let m = Model::gop();
        assert_eq!(
            adopt(&m),
            Ok(Adopted {
                vram_base: 0xf4_0000_0000,
                pitch_pixels: 1920
            })
        );
        assert!(m.log.borrow().is_empty(), "adopting writes nothing");
    }

    #[test]
    fn a_display_that_is_not_the_firmwares_is_refused() {
        let m = Model::gop();
        m.set(OTG0_CONTROL, 0);
        assert_eq!(adopt(&m), Err(NotAdoptable::NoTiming));
        let m = Model::gop();
        m.set(FLIP_CONTROL, 0x0410_0100);
        assert_eq!(adopt(&m), Err(NotAdoptable::FlipPending));
        let m = Model::gop();
        m.set(SURFACE_ADDRESS, 0x0100_0000);
        assert_eq!(
            adopt(&m),
            Err(NotAdoptable::SurfaceMoved {
                address: 0xf4_0100_0000,
                base: 0xf4_0000_0000
            })
        );
    }

    #[test]
    fn a_flip_writes_the_high_half_first_and_the_low_half_last() {
        let m = Model::gop();
        flip(&m, 0xf4_0100_0000);
        let log = m.log.borrow();
        let high = log.iter().position(|w| w.0 == SURFACE_ADDRESS_HIGH);
        let low = log.iter().position(|w| w.0 == SURFACE_ADDRESS);
        assert!(high < low && low == Some(log.len() - 1));
        assert_eq!(surface(&m), 0xf4_0100_0000);
        // The immediate bit stays clear (the flip waits for the vertical update).
        assert_eq!(m.read32(FLIP_CONTROL) & FLIP_IMMEDIATE, 0);
    }

    #[test]
    fn the_flush_writes_its_page_and_completes_it_with_a_read() {
        let m = Model::gop();
        hdp_init(&m);
        hdp_flush(&m);
        assert!(m.log.borrow().contains(&(HDP_FLUSH_PAGE, 0)));
        assert_eq!(m.read32(HDP_MEM_FLUSH_REMAP), 0x7f000);
        assert_eq!(m.read32(HDP_MISC_CNTL) & 1, 1);
    }
}
