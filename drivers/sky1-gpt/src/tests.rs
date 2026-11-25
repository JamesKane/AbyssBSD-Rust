// SPDX-License-Identifier: BSD-2-Clause
//! The timer against a model of its two chained 32-bit timers.

extern crate std;

use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::hw::{
    COMP, EVEN, FREE_CNT, Gpt, INT_FREE_RUN, INTCLR, INTSTAT, Missed, ODD, Regs, TCTL,
    TCTL_CHAIN64, TCTL_INT_EN, TCTL_TIMER_EN, WINDOW, split,
};

/// The counter advances by `step` on every read of the even timer's
/// `FREE_CNT` (and the model's registers are the two timers').
#[derive(Default)]
struct Model {
    counter: Cell<u64>,
    step: Cell<u64>,
    comp: Cell<u64>,
    tctl: [Cell<u32>; 2],
    /// Writes, in order.
    log: RefCell<Vec<(usize, u32)>>,
    pending: Cell<bool>,
}

#[derive(Clone)]
struct M(Rc<Model>);

impl Regs for M {
    fn read32(&self, off: usize) -> u32 {
        let (t, r) = if off >= ODD { (1, off - ODD) } else { (0, off) };
        match r {
            FREE_CNT => {
                if t == 0 {
                    self.0
                        .counter
                        .set(self.0.counter.get().wrapping_add(self.0.step.get()));
                }
                let (lo, hi) = split(self.0.counter.get());
                if t == 0 { lo } else { hi }
            }
            COMP => {
                let (lo, hi) = split(self.0.comp.get());
                if t == 0 { lo } else { hi }
            }
            TCTL => self.0.tctl[t].get(),
            INTSTAT => u32::from(self.0.pending.get() && t == 1),
            _ => 0,
        }
    }

    fn write32(&self, off: usize, v: u32) {
        self.0.log.borrow_mut().push((off, v));
        let (t, r) = if off >= ODD { (1, off - ODD) } else { (0, off) };
        match r {
            TCTL => self.0.tctl[t].set(v),
            COMP => {
                let c = self.0.comp.get();
                let (lo, hi) = split(c);
                self.0.comp.set(if t == 0 {
                    u64::from(hi) << 32 | u64::from(v)
                } else {
                    u64::from(v) << 32 | u64::from(lo)
                });
            }
            INTCLR if t == 1 && v == INT_FREE_RUN => self.0.pending.set(false),
            _ => {}
        }
    }
}

fn gpt(counter: u64, step: u64) -> (Gpt<M>, M) {
    let m = M(Rc::new(Model::default()));
    m.0.counter.set(counter);
    m.0.step.set(step);
    (Gpt::new(m.clone()), m)
}

#[test]
fn init_writes_each_control_register_twice_with_the_enable_last() {
    let (g, m) = gpt(0, 1);
    g.init();
    let w = m.0.log.borrow();
    assert_eq!(
        *w,
        [
            (EVEN + TCTL, 0x8042),
            (ODD + TCTL, 0x9042),
            (EVEN + TCTL, 0x28042),
            (ODD + TCTL, 0x29042),
        ]
    );
    // The enable is in a write of its own, the 64-bit chaining on the odd
    // timer only.
    assert_eq!(0x28042 & TCTL_TIMER_EN, TCTL_TIMER_EN);
    assert_eq!(0x8042 & TCTL_CHAIN64, 0);
    assert_eq!(0x9042 & TCTL_CHAIN64, TCTL_CHAIN64);
}

#[test]
fn the_counter_is_the_two_halves() {
    let (g, _) = gpt(0x0000_0007_0000_1000, 1);
    // The even timer advances on each read: 0x1001.
    assert_eq!(g.counter(), 0x0000_0007_0000_1001);
}

#[test]
fn a_carry_between_the_reads_does_not_tear_the_counter() {
    // The low word wraps between the high word's reads: a naive read gives
    // 0x0_0000_0001, which is in the past.
    let (g, _) = gpt(0xffff_fffd, 4);
    assert_eq!(g.counter(), 0x1_0000_0005);
    // And one that is not near a carry reads once.
    let (g, m) = gpt(0x10, 1);
    let _ = g.counter();
    assert!(m.0.log.borrow().is_empty());
}

#[test]
fn the_compare_is_written_high_word_first() {
    let (g, m) = gpt(0, 1);
    g.set_compare(0x0000_0002_dead_beef);
    let w = m.0.log.borrow();
    assert_eq!(*w, [(ODD + COMP, 2), (EVEN + COMP, 0xdead_beef)]);
    assert_eq!(m.0.comp.get(), 0x0000_0002_dead_beef);
}

#[test]
fn parking_turns_the_interrupt_off_puts_the_compare_behind_and_clears() {
    let (g, m) = gpt(1000, 0);
    m.0.tctl[1].set(0x29042);
    m.0.pending.set(true);
    g.park();
    assert_eq!(m.0.tctl[1].get() & TCTL_INT_EN, 0);
    assert_eq!(m.0.comp.get(), 997);
    assert!(!g.pending());
    assert!(m.0.log.borrow().contains(&(ODD + INTCLR, INT_FREE_RUN)));
}

#[test]
fn arming_sets_the_compare_ahead_and_enables_the_interrupt() {
    let (g, m) = gpt(1000, 1);
    m.0.tctl[1].set(0x29042 & !TCTL_INT_EN);
    assert_eq!(g.arm(500), Ok(()));
    assert!(m.0.comp.get() >= 1500);
    assert_eq!(m.0.tctl[1].get() & TCTL_INT_EN, TCTL_INT_EN);
}

#[test]
fn a_time_already_reached_is_missed() {
    // The counter runs 100 ticks a read: by the re-read it is past.
    let (g, _) = gpt(0, 100);
    assert_eq!(g.arm(50), Err(Missed));
}

#[test]
fn the_window_covers_the_odd_timers_last_register() {
    assert_eq!(WINDOW, 0x1038);
}

#[test]
fn split_is_low_then_high() {
    assert_eq!(split(0x0123_4567_89ab_cdef), (0x89ab_cdef, 0x0123_4567));
}
