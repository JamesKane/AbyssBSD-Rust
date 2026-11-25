// SPDX-License-Identifier: BSD-2-Clause
//! The Sky1 general purpose timer's registers: two 32-bit timers (even at
//! the instance's base, odd 0x1000 above) chained into one 64-bit
//! free-running counter with a 64-bit compare.
//!
//! Behavior reference: the clean-room specification `sky1-soc/03-gpt-timer`
//! (§3.2 to §3.5), and the AbyssBSD Project's own `sky1_gpt` for FreeBSD.

/// The register window: 32-bit accesses at offsets from the instance's
/// base.
pub trait Regs {
    fn read32(&self, off: usize) -> u32;
    fn write32(&self, off: usize, v: u32);
}

/// The even timer (the low 32 bits) and the odd (the high 32 bits).
pub const EVEN: usize = 0x0000;
pub const ODD: usize = 0x1000;
/// The register window a whole instance needs: the odd timer's last
/// register is `INTCLR`.
pub const WINDOW: usize = ODD + INTCLR + 4;

/// Each timer's registers.
pub const FREE_CNT: usize = 0x1c;
pub const COMP: usize = 0x20;
pub const TCTL: usize = 0x24;
pub const INTSTAT: usize = 0x30;
pub const INTCLR: usize = 0x34;

/// `TCTL`: mode 2 is free-running; the interrupt enable; the odd timer's
/// 64-bit chaining; the free-run enable; and the timer enable, which is
/// set in a write of its own.
pub const TCTL_MODE_FREE: u32 = 0x2;
pub const TCTL_INT_EN: u32 = 1 << 6;
pub const TCTL_CHAIN64: u32 = 1 << 12;
pub const TCTL_FREE_EN: u32 = 1 << 15;
pub const TCTL_TIMER_EN: u32 = 1 << 17;
/// Written to the odd timer's `INTCLR`: clears the free-run compare
/// interrupt.
pub const INT_FREE_RUN: u32 = 0x20;

/// The compare time was already past when it was written, or passed while
/// it was: no interrupt can be expected for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Missed;

/// One instance.
#[derive(Debug)]
pub struct Gpt<R: Regs> {
    regs: R,
}

impl<R: Regs> Gpt<R> {
    #[must_use]
    pub const fn new(regs: R) -> Self {
        Self { regs }
    }

    /// §3.3: both timers set up as free-running (the odd one chained), then
    /// each enabled in a write of its own, the even one first.
    pub fn init(&self) {
        let even = TCTL_FREE_EN | TCTL_MODE_FREE | TCTL_INT_EN;
        let odd = even | TCTL_CHAIN64;
        self.regs.write32(EVEN + TCTL, even);
        self.regs.write32(ODD + TCTL, odd);
        self.regs.write32(EVEN + TCTL, even | TCTL_TIMER_EN);
        self.regs.write32(ODD + TCTL, odd | TCTL_TIMER_EN);
    }

    /// §3.4: the counter. The halves are read separately, so a carry can
    /// land between the reads: the high word, the low, and the high again,
    /// and if it changed the low word is read again under the new one.
    #[must_use]
    pub fn counter(&self) -> u64 {
        let mut hi = self.regs.read32(ODD + FREE_CNT);
        loop {
            let lo = self.regs.read32(EVEN + FREE_CNT);
            let again = self.regs.read32(ODD + FREE_CNT);
            if again == hi {
                return u64::from(hi) << 32 | u64::from(lo);
            }
            hi = again;
        }
    }

    /// §3.5: the compare, the high word first so the 64-bit value never
    /// briefly matches a wrong time.
    pub fn set_compare(&self, target: u64) {
        let (lo, hi) = split(target);
        self.regs.write32(ODD + COMP, hi);
        self.regs.write32(EVEN + COMP, lo);
    }

    /// Whether the timer's interrupt is pending (the odd timer's status).
    #[must_use]
    pub fn pending(&self) -> bool {
        self.regs.read32(ODD + INTSTAT) != 0
    }

    /// Clears the pending interrupt.
    pub fn clear(&self) {
        self.regs.write32(ODD + INTCLR, INT_FREE_RUN);
    }

    /// §3.5's policy for stopping or parking it: the interrupt off, the
    /// compare set to just behind the counter so it cannot match soon, and
    /// what is pending cleared.
    pub fn park(&self) {
        let odd = self.regs.read32(ODD + TCTL);
        self.regs.write32(ODD + TCTL, odd & !TCTL_INT_EN);
        let now = self.counter();
        self.set_compare(now.wrapping_sub(3));
        self.clear();
    }

    /// One-shot: an interrupt `delta` ticks from now, the interrupt enabled
    /// again. §3.5: the counter is read again after the compare is written,
    /// and a time already reached is missed.
    ///
    /// # Errors
    ///
    /// [`Missed`] if the counter has reached the target.
    pub fn arm(&self, delta: u64) -> Result<(), Missed> {
        let target = self.counter().wrapping_add(delta);
        self.clear();
        self.set_compare(target);
        let odd = self.regs.read32(ODD + TCTL);
        self.regs.write32(ODD + TCTL, odd | TCTL_INT_EN);
        if self.counter() >= target {
            return Err(Missed);
        }
        Ok(())
    }
}

/// A 64-bit value as its low and high words.
#[must_use]
pub fn split(v: u64) -> (u32, u32) {
    let b = v.to_le_bytes();
    (
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
    )
}
