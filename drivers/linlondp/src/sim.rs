// SPDX-License-Identifier: BSD-2-Clause
//! A simulated display processor for the tests.
//!
//! It is written from the same specification as the driver, so what it
//! proves is that the driver does what the specification says and is
//! consistent with itself, not that the silicon agrees. The questions the
//! specification leaves open are listed in `docs/GRAPHICS.md`.
//!
//! The model: self-describing blocks laid out as the vendor's fixed
//! offsets say (specification 02 §2.17); shadowed registers that go live
//! at a frame boundary after the config-valid write, but only while the
//! timing generator runs or is being started; the interrupt tree.

use std::prelude::rust_2024::*;
use std::sync::Mutex;
use std::{vec, vec::Vec};

use crate::discover::Delay;
use crate::regs::{BLOCK, Words, bs, common, gcu};

/// A frame, in microseconds (60 hertz).
const FRAME_US: u64 = 16_667;

/// Slots of one pipeline's blocks, in the vendor's order.
const SLOTS: [(u8, u8); 16] = [
    (0x01, 0), // LPU
    (0x10, 0), // layers 0..3
    (0x10, 1),
    (0x10, 2),
    (0x10, 3),
    (0x11, 0), // write-back
    (0x02, 0), // compositor
    (0x20, 0), // splitter
    (0x21, 0), // scalers
    (0x21, 1),
    (0x22, 0), // merger
    (0x03, 0), // display output
    (0x30, 0), // image processor
    (0x32, 0), // gamma table
    (0x31, 0), // timing generator
    (0xFF, 0), // empty slot
];

/// What the simulation can be made to do wrong.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Faults {
    /// The soft reset never finishes.
    pub stuck_reset: bool,
    /// The config-valid bit never clears.
    pub never_latch: bool,
    /// The operating mode never takes.
    pub stuck_mode: bool,
}

struct State {
    live: Vec<u32>,
    shadow: Vec<u32>,
    pending: [bool; 2],
    time_us: u64,
    frames: u64,
    faults: Faults,
}

/// The device.
pub(crate) struct Sim {
    s: Mutex<State>,
}

impl Sim {
    /// A Linlon-D6 as the Sky1 has it: two pipelines of four layers, two
    /// scalers, a splitter and a merger each, no legacy block, no
    /// integrated translation units, dual link supported.
    #[must_use]
    pub(crate) fn d6() -> Self {
        let words = 0x2_0000 / 4;
        let mut live = vec![0u32; words];
        let put = |live: &mut Vec<u32>, at: usize, v: u32| live[at / 4] = v;
        put(&mut live, gcu::CORE_ID, 0x0060_0000);
        put(&mut live, gcu::CORE_INFO, 33 | 2 << 8);
        put(&mut live, gcu::CONFIG_ID0, 4096 | 4096 << 16);
        put(&mut live, gcu::CONFIG_ID1, 1 << 16 | 2);
        for p in 0..2usize {
            for (i, &(ty, inst)) in SLOTS.iter().enumerate() {
                let at = (p * 16 + 1 + i) * BLOCK;
                let n = match ty {
                    0x10 => p * 4 + usize::from(inst),
                    0x21 => p * 2 + usize::from(inst),
                    _ => p,
                };
                put(
                    &mut live,
                    at + common::BLOCK_INFO,
                    u32::from(ty) << 8 | u32::try_from(n).unwrap_or(0) << 4,
                );
                if ty != 0xFF {
                    put(&mut live, at + common::MAX_LINE_SIZE, 4096);
                }
                if ty == 0x10 {
                    // Layers 0 and 1 are rich, with a 2048 pixel limit for
                    // chroma-subsampled formats.
                    let rich = u32::from(inst < 2);
                    put(&mut live, at + 0x0C0, rich | 2048 << 16);
                }
                if ty == 0x31 {
                    // The timing generator resets to video mode.
                    put(&mut live, at + bs::CONTROL, bs::CTRL_VM);
                }
            }
        }
        let shadow = live.clone();
        Self {
            s: Mutex::new(State {
                live,
                shadow,
                pending: [false; 2],
                time_us: 0,
                frames: 0,
                faults: Faults::default(),
            }),
        }
    }

    /// Makes it misbehave.
    pub(crate) fn fault(&self, f: Faults) {
        self.s.lock().unwrap().faults = f;
    }

    /// A register as the hardware is using it (after a latch).
    #[must_use]
    pub(crate) fn live(&self, offset: usize) -> u32 {
        self.s.lock().unwrap().live[offset / 4]
    }

    /// A register as software last wrote it.
    #[must_use]
    pub(crate) fn shadow(&self, offset: usize) -> u32 {
        self.s.lock().unwrap().shadow[offset / 4]
    }

    /// Frames elapsed.
    #[must_use]
    pub(crate) fn frames(&self) -> u64 {
        self.s.lock().unwrap().frames
    }

    /// One frame boundary passes.
    pub(crate) fn frame(&self) {
        let mut s = self.s.lock().unwrap();
        s.frame();
    }

    /// The interrupt line is asserted.
    #[must_use]
    pub(crate) fn irq_pending(&self) -> bool {
        let s = self.s.lock().unwrap();
        s.top() != 0
    }
}

impl State {
    fn bs_of(p: usize) -> usize {
        // Slot 15 of the pipeline.
        (p * 16 + 15) * BLOCK
    }

    fn dou_of(p: usize) -> usize {
        (p * 16 + 12) * BLOCK
    }

    fn cu_of(p: usize) -> usize {
        (p * 16 + 7) * BLOCK
    }

    fn lpu_of(p: usize) -> usize {
        (p * 16 + 1) * BLOCK
    }

    fn running(&self, p: usize) -> bool {
        self.live[(Self::bs_of(p) + bs::CONTROL) / 4] & bs::CTRL_EN != 0
    }

    fn starting(&self, p: usize) -> bool {
        self.shadow[(Self::bs_of(p) + bs::CONTROL) / 4] & bs::CTRL_EN != 0
    }

    fn frame(&mut self) {
        self.frames += 1;
        for p in 0..2usize {
            if self.pending[p] && !self.faults.never_latch && (self.running(p) || self.starting(p))
            {
                let first = (p * 16 + 1) * BLOCK / 4;
                let last = (p * 16 + 17) * BLOCK / 4;
                for i in first..last {
                    // Read-only registers (below 0x080 in a block) do not
                    // change; everything else goes live.
                    if i % (BLOCK / 4) >= 0x080 / 4 {
                        self.live[i] = self.shadow[i];
                    }
                }
                self.pending[p] = false;
                self.live[(common::IRQ_RAW_STATUS) / 4] |= gcu::irq_cval(p);
            }
            if self.running(p) {
                self.live[(Self::dou_of(p) + common::IRQ_RAW_STATUS) / 4] |= 1 << 13;
            }
        }
    }

    /// The summary of what is raised and unmasked.
    fn top(&self) -> u32 {
        let mut t = 0;
        let gcu_on = self.live[common::IRQ_RAW_STATUS / 4] & self.live[common::IRQ_MASK / 4];
        if gcu_on != 0 {
            t |= gcu::SUMMARY_GCU;
        }
        for p in 0..2usize {
            for (at, bit) in [
                (Self::lpu_of(p), gcu::summary_lpu(p)),
                (Self::cu_of(p), gcu::summary_cu(p)),
                (Self::dou_of(p), gcu::summary_dou(p)),
            ] {
                let raw = self.live[(at + common::IRQ_RAW_STATUS) / 4];
                let mask = self.live[(at + common::IRQ_MASK) / 4];
                if raw & mask != 0 {
                    t |= bit;
                }
            }
        }
        t
    }
}

impl Words for Sim {
    fn read(&self, offset: usize) -> u32 {
        let s = self.s.lock().unwrap();
        assert!(
            offset / 4 < s.live.len(),
            "read past the window: {offset:#x}"
        );
        match offset {
            gcu::IRQ_STATUS => s.top(),
            o if o == gcu::config_valid(0) || o == gcu::config_valid(1) => {
                u32::from(s.pending[(o - gcu::config_valid(0)) / 4])
            }
            // Raw status reads are what is raised; masked status is
            // raw & mask.
            o if o % BLOCK == common::IRQ_STATUS => {
                let b = o - common::IRQ_STATUS;
                s.live[(b + common::IRQ_RAW_STATUS) / 4] & s.live[(b + common::IRQ_MASK) / 4]
            }
            // Software sees its shadow of the shadowed registers; the
            // interrupt and status registers (0x0A0 to 0x0B4) and the
            // control unit's are live.
            o if o >= BLOCK && o % BLOCK >= 0x080 && !(0x0A0..=0x0B4).contains(&(o % BLOCK)) => {
                s.shadow[o / 4]
            }
            o => s.live[o / 4],
        }
    }

    fn write(&self, offset: usize, v: u32) {
        let mut s = self.s.lock().unwrap();
        assert!(
            offset / 4 < s.live.len(),
            "write past the window: {offset:#x}"
        );
        let within = offset % BLOCK;
        match offset {
            gcu::CONTROL => {
                if v & gcu::SRST != 0 && !s.faults.stuck_reset {
                    // Reset: shadow and live registers return to their
                    // describing values; keep the read-only ones.
                    for i in 0..s.live.len() {
                        if i % (BLOCK / 4) >= 0x080 / 4 {
                            s.live[i] = 0;
                            s.shadow[i] = 0;
                        }
                    }
                    for p in 0..2usize {
                        let at = (State::bs_of(p) + bs::CONTROL) / 4;
                        s.live[at] = bs::CTRL_VM;
                        s.shadow[at] = bs::CTRL_VM;
                    }
                    s.pending = [false; 2];
                } else if v & gcu::SRST != 0 {
                    // Stays set.
                    s.live[gcu::CONTROL / 4] = gcu::SRST;
                } else if !s.faults.stuck_mode {
                    s.live[gcu::CONTROL / 4] = v;
                }
            }
            o if o == gcu::config_valid(0) || o == gcu::config_valid(1) => {
                if v & 1 != 0 {
                    s.pending[(o - gcu::config_valid(0)) / 4] = true;
                }
            }
            // Clearing writes (write only register).
            o if within == common::IRQ_CLEAR => {
                let b = o - common::IRQ_CLEAR;
                s.live[(b + common::IRQ_RAW_STATUS) / 4] &= !v;
            }
            // The interrupt mask and sticky status are not shadowed: they
            // act at once.
            o if within == common::IRQ_MASK || within == common::STATUS => {
                s.live[o / 4] = v;
                s.shadow[o / 4] = v;
            }
            o if within >= 0x080 && o >= BLOCK => s.shadow[o / 4] = v,
            _ => {}
        }
    }
}

impl Delay for Sim {
    fn delay_us(&self, us: u64) {
        let mut s = self.s.lock().unwrap();
        let before = s.time_us / FRAME_US;
        s.time_us += us;
        let after = s.time_us / FRAME_US;
        for _ in before..after {
            s.frame();
        }
    }
}

impl Sim {
    /// Overwrites a register in both copies (to describe other hardware).
    pub(crate) fn poke(&self, offset: usize, v: u32) {
        let mut s = self.s.lock().unwrap();
        s.live[offset / 4] = v;
        s.shadow[offset / 4] = v;
    }
}
