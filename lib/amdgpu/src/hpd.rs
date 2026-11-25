// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The hot-plug detect line (specification 08 section 4.3): whether a
//! monitor is connected to the first connector, read from the sense bit of
//! the line's status register, and a debounce so a bouncing line does not
//! make plug and unplug events out of noise.

/// `HPD0_DC_HPD_INT_STATUS` (byte address; the blocks are `0x20` apart).
pub const HPD0_INT_STATUS: usize = 0x14f50;

/// Status bit 1: the line's sense, 1 = a monitor is connected.
pub const SENSE: u32 = 1 << 1;

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
