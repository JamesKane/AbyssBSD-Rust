// SPDX-License-Identifier: BSD-2-Clause
//! A model of the GPU-control block, written from specification 02 and 03: enough to run the
//! sequences in `control` and `id`. Commands complete after a few microseconds of model time,
//! which only advances through [`Ticker`].

use core::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::vec::Vec;

use crate::Delay;
use crate::regs::{self, Regs};

/// Something the model will do once its time reaches `at`.
#[derive(Clone, Copy, Debug)]
enum Event {
    RawStat(u32),
    L2Ready,
}

pub(crate) struct Model {
    regs: RefCell<BTreeMap<usize, u32>>,
    now: Cell<u64>,
    pending: RefCell<Vec<(u64, Event)>>,
    /// Every write, in order.
    pub(crate) writes: RefCell<Vec<(usize, u32)>>,
    /// Microseconds a command takes.
    pub(crate) latency: u64,
    /// The GPU ignores commands (to test timeouts).
    pub(crate) dead: Cell<bool>,
}

impl Model {
    /// The values the Orange Pi 6 Plus's FreeBSD printed for its GPU (dmesg.boot.txt):
    /// `id 0xc870 major 0x0 minor 0x0 status 0x8`, `L2:0x8130306 Tiler:0x809 Mem:0x301
    /// MMU:0x2830 AS:0xff`, `shader_present=0x550555 l2_present=0x1 tiler_present=0x1`. The
    /// ray-intersection feature is assumed, as the name it printed (`Mali-G720-Immortalis`) needs
    /// it; the other registers are made up.
    pub(crate) fn sky1() -> Self {
        let m = Self {
            regs: RefCell::new(BTreeMap::new()),
            now: Cell::new(0),
            pending: RefCell::new(Vec::new()),
            writes: RefCell::new(Vec::new()),
            latency: 50,
            dead: Cell::new(false),
        };
        for (off, v) in [
            (regs::GPU_ID, 0xC870_0008),
            (regs::L2_FEATURES, 0x0813_0306),
            (regs::TILER_FEATURES, 0x809),
            (regs::MEM_FEATURES, 0x301),
            (regs::MMU_FEATURES, 0x2830),
            (regs::AS_PRESENT, 0xFF),
            (regs::CSF_ID, 0x1234_5678),
            (regs::GPU_FEATURES, 0x4),
            (regs::SHADER_PRESENT, 0x0055_0555),
            (regs::TILER_PRESENT, 1),
            (regs::L2_PRESENT, 1),
            (regs::THREAD_MAX_THREADS, 2048),
            (regs::COHERENCY_FEATURES, 0x21),
        ] {
            m.set(off, v);
        }
        m
    }

    pub(crate) fn set(&self, off: usize, v: u32) {
        self.regs.borrow_mut().insert(off, v);
    }

    pub(crate) fn get(&self, off: usize) -> u32 {
        self.regs.borrow().get(&off).copied().unwrap_or(0)
    }

    pub(crate) fn now(&self) -> u64 {
        self.now.get()
    }

    fn schedule(&self, e: Event) {
        if !self.dead.get() {
            self.pending
                .borrow_mut()
                .push((self.now.get() + self.latency, e));
        }
    }

    fn advance(&self, us: u64) {
        self.now.set(self.now.get() + us);
        let now = self.now.get();
        let due: Vec<Event> = {
            let mut p = self.pending.borrow_mut();
            let (due, rest): (Vec<_>, Vec<_>) = p.drain(..).partition(|(at, _)| *at <= now);
            *p = rest;
            due.into_iter().map(|(_, e)| e).collect()
        };
        for e in due {
            match e {
                Event::RawStat(bits) => {
                    let raw = self.get(regs::GPU_INT + regs::INT_RAWSTAT);
                    self.set(regs::GPU_INT + regs::INT_RAWSTAT, raw | bits);
                }
                Event::L2Ready => {
                    self.set(regs::L2_PWRTRANS, 0);
                    self.set(regs::L2_READY, self.get(regs::L2_PRESENT) & 1);
                }
            }
        }
    }
}

impl Regs for Model {
    fn read32(&self, off: usize) -> u32 {
        match off {
            o if o == regs::GPU_INT + regs::INT_STAT => {
                self.get(regs::GPU_INT + regs::INT_RAWSTAT)
                    & self.get(regs::GPU_INT + regs::INT_MASK)
            }
            o => self.get(o),
        }
    }

    fn write32(&self, off: usize, v: u32) {
        self.writes.borrow_mut().push((off, v));
        match off {
            o if o == regs::GPU_INT + regs::INT_CLEAR
                || o == regs::JOB_INT + regs::INT_CLEAR
                || o == regs::MMU_INT + regs::INT_CLEAR =>
            {
                let raw = o - regs::INT_CLEAR + regs::INT_RAWSTAT;
                self.set(raw, self.get(raw) & !v);
            }
            regs::GPU_CMD => match v & 0xFF {
                1 => {
                    // A reset powers the L2 down.
                    self.set(regs::L2_READY, 0);
                    self.schedule(Event::RawStat(regs::INT_RESET_COMPLETED));
                }
                4 => self.schedule(Event::RawStat(regs::INT_CLEAN_CACHES_COMPLETED)),
                _ => {}
            },
            regs::L2_PWRON => {
                if v & 1 != 0 && !self.dead.get() {
                    self.set(regs::L2_PWRTRANS, 1);
                    self.schedule(Event::L2Ready);
                }
            }
            o if o == regs::L2_PWRON + 4 => {}
            regs::L2_PWROFF => {
                self.set(regs::L2_READY, 0);
            }
            o => self.set(o, v),
        }
    }
}

/// Advances the model's time.
pub(crate) struct Ticker<'a>(pub &'a Model);

impl Delay for Ticker<'_> {
    fn delay_us(&mut self, us: u32) {
        self.0.advance(u64::from(us));
    }
}
