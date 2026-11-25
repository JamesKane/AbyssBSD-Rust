// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The hot-plug detect line (specification 08 section 4.3): whether a
//! monitor is connected to the first connector, read from the sense bit of
//! the line's status register, and a debounce so a bouncing line does not
//! make plug and unplug events out of noise.

/// `HPD0_DC_HPD_INT_STATUS` (byte address; the blocks are `0x20` apart).
pub const HPD0_INT_STATUS: usize = 0x14f50;

/// `HPD0_DC_HPD_INT_CONTROL` (08 section 4.3): the interrupt's acknowledge,
/// polarity and enable.
pub const HPD0_INT_CONTROL: usize = 0x14f54;
/// The distance between the hot-plug lines' register blocks.
pub const LINE_STRIDE: usize = 0x20;
/// How many hot-plug lines the display has.
pub const LINES: usize = 5;

/// `INT_STATUS` of line `n`.
#[must_use]
pub const fn int_status(n: usize) -> usize {
    HPD0_INT_STATUS + n * LINE_STRIDE
}

/// `INT_CONTROL` of line `n`.
#[must_use]
pub const fn int_control(n: usize) -> usize {
    HPD0_INT_CONTROL + n * LINE_STRIDE
}

/// Status bit 1: the line's sense, 1 = a monitor is connected.
pub const SENSE: u32 = 1 << 1;
/// Status bit 4: the sense after the connect and disconnect filter.
pub const SENSE_DEBOUNCED: u32 = 1 << 4;
/// Control bit 0: acknowledge the event.
pub const CONTROL_ACK: u32 = 1 << 0;
/// Control bit 8: the polarity the line interrupts on (1: connected).
pub const CONTROL_POLARITY: u32 = 1 << 8;
/// Control bit 16: the interrupt is enabled.
pub const CONTROL_ENABLE: u32 = 1 << 16;

/// The interrupt's source number in an interrupt entry of the display's
/// client (4); the line is the entry's first source data word.
pub const IH_SOURCE: u8 = 9;

/// `INT_CONTROL` after an event, or when arming the interrupt: the event
/// acknowledged (if `ack`), the interrupt enabled, and the polarity the
/// opposite of the filtered sense in `status`, so the line interrupts again
/// at the next change, in either direction (the interrupt is level-type).
#[must_use]
pub const fn control_for(control: u32, status: u32, ack: bool) -> u32 {
    let mut c = control | CONTROL_ENABLE;
    if ack {
        c |= CONTROL_ACK;
    }
    if status & SENSE_DEBOUNCED != 0 {
        c &= !CONTROL_POLARITY;
    } else {
        c |= CONTROL_POLARITY;
    }
    c
}

/// Whether the status register says a monitor is connected.
#[must_use]
pub const fn sensed(status: u32) -> bool {
    status & SENSE != 0
}

/// A debounce: the state changes only after the new value has been read
/// `need` times in a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Debounce {
    state: bool,
    seen: u8,
    need: u8,
}

impl Debounce {
    /// Starts in `state`; a change takes `need` (at least 1) agreeing reads.
    #[must_use]
    pub const fn new(state: bool, need: u8) -> Self {
        Self {
            state,
            seen: 0,
            need: if need == 0 { 1 } else { need },
        }
    }

    /// The settled state.
    #[must_use]
    pub const fn state(&self) -> bool {
        self.state
    }

    /// Takes a read; `Some(new)` when the settled state just changed.
    pub fn update(&mut self, read: bool) -> Option<bool> {
        if read == self.state {
            self.seen = 0;
            return None;
        }
        self.seen += 1;
        if self.seen >= self.need {
            self.state = read;
            self.seen = 0;
            Some(read)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sense_bit_is_bit_one() {
        // 0x12 is what the X870E reads with the monitor connected.
        assert!(sensed(0x12));
        assert!(!sensed(0x10));
        assert!(!sensed(0));
    }

    #[test]
    fn the_interrupt_arms_for_the_opposite_polarity_of_the_filtered_sense() {
        // Connected (filtered sense set): interrupt on disconnect, polarity 0.
        let c = control_for(0, 0x12, false);
        assert_eq!(c, CONTROL_ENABLE);
        // Disconnected: polarity 1, so the next connect interrupts.
        let c = control_for(CONTROL_ENABLE, 0x02, true);
        assert_eq!(c, CONTROL_ENABLE | CONTROL_POLARITY | CONTROL_ACK);
        // Other bits are left alone; the polarity follows the sense.
        let c = control_for(0x0000_0100 | 0x20, 0x10, true);
        assert_eq!(c & CONTROL_POLARITY, 0);
        assert_eq!(c & 0x20, 0x20);
    }

    #[test]
    fn the_lines_registers_are_a_stride_apart() {
        assert_eq!(int_status(0), 0x14f50);
        assert_eq!(int_control(0), 0x14f54);
        assert_eq!(int_status(4), 0x14fd0);
        assert_eq!(int_control(1), 0x14f74);
    }

    #[test]
    fn a_change_needs_enough_agreeing_reads() {
        let mut d = Debounce::new(true, 3);
        assert_eq!(d.update(false), None);
        assert_eq!(d.update(false), None);
        assert_eq!(d.update(false), Some(false));
        assert!(!d.state());
        assert_eq!(d.update(false), None);
    }

    #[test]
    fn a_bounce_does_not_change_the_state() {
        let mut d = Debounce::new(true, 3);
        assert_eq!(d.update(false), None);
        assert_eq!(d.update(false), None);
        assert_eq!(d.update(true), None);
        assert_eq!(d.update(false), None);
        assert_eq!(d.update(false), None);
        assert!(d.state());
        assert_eq!(d.update(false), Some(false));
        assert_eq!(d.update(true), None);
        assert_eq!(d.update(true), None);
        assert_eq!(d.update(true), Some(true));
    }

    #[test]
    fn zero_reads_needed_means_one() {
        let mut d = Debounce::new(false, 0);
        assert_eq!(d.update(true), Some(true));
    }
}
