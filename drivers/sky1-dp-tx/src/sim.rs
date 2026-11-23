// SPDX-License-Identifier: BSD-2-Clause
//! A simulated transmitter core with a sink behind its AUX channel, for
//! the tests. Written from the same specification as the driver, so it
//! checks the driver against the specification and itself, not against
//! the silicon.

use std::collections::{HashMap, VecDeque};
use std::prelude::rust_2024::*;
use std::sync::Mutex;
use std::vec;

use crate::Delay;
use crate::dpcd::{Drive, Rate};
use crate::link::Phy;
use crate::regs::{self, Words};

/// The sink: DPCD, an EDID, and how it behaves in training.
pub(crate) struct Sink {
    pub dpcd: Vec<u8>,
    pub edid: Vec<u8>,
    pub hpd: bool,
    /// The drive the sink needs for clock recovery and equalisation.
    pub need: Drive,
    /// The highest rate at which clock recovery can work.
    pub works_up_to: Rate,
    /// The next this many native requests are deferred.
    pub defer: u32,
    /// Replies stop coming (the channel times out).
    pub dead: bool,
    /// Writes to these DPCD addresses are refused.
    pub nack_write: Vec<u32>,
}

impl Sink {
    pub(crate) fn new() -> Self {
        let mut dpcd = vec![0u8; 0x700];
        dpcd[0] = 0x14;
        dpcd[1] = Rate::Hbr2.code();
        // Four lanes, enhanced framing, TPS3.
        dpcd[2] = 4 | 0x80 | 0x40;
        // Down-spread, no TPS4.
        dpcd[3] = 1;
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
        Self {
            dpcd,
            edid,
            hpd: true,
            need: Drive::default(),
            works_up_to: Rate::Hbr3,
            defer: 0,
            dead: false,
            nack_write: Vec::new(),
        }
    }

    /// The status bytes at `0x202..0x208`, from what the source has
    /// written.
    fn training_status(&self) -> [u8; 6] {
        let pat = self.dpcd[0x102] & 7;
        let lanes = usize::from(self.dpcd[0x101] & 0x1F);
        let rate = Rate::from_code(self.dpcd[0x100]);
        let mut st = [0u8; 6];
        let mut all_cr = true;
        let mut all_eq = true;
        for i in 0..lanes {
            let d = self.dpcd[0x103 + i];
            let (swing, emph) = (d & 3, (d >> 3) & 3);
            let rate_ok = rate.is_some_and(|r| r <= self.works_up_to);
            let cr = pat != 0 && rate_ok && swing >= self.need.swing;
            let eq = cr && pat != 1 && emph >= self.need.emphasis;
            all_cr &= cr;
            all_eq &= eq;
            let nib = u8::from(cr) | u8::from(eq) << 1 | u8::from(eq) << 2;
            st[i / 2] |= nib << (4 * (i % 2));
        }
        st[2] = u8::from(all_cr && all_eq && pat > 1);
        let req = self.need.swing | self.need.emphasis << 2;
        for i in 0..lanes {
            st[4 + i / 2] |= req << (4 * (i % 2));
        }
        st
    }
}

struct State {
    regs: HashMap<usize, u32>,
    sink: Sink,
    reply: VecDeque<u8>,
    reply_count: u32,
    reply_code: u32,
    reply_ready: bool,
    reply_error: bool,
    fifo: Vec<u8>,
    addr: u32,
    i2c_offset: usize,
    cause: u32,
    ready_mask: u32,
    delays_us: u64,
    /// Every AUX request: command, address, length.
    log: Vec<(u32, u32, usize)>,
}

pub(crate) struct Sim {
    s: Mutex<State>,
}

impl Sim {
    pub(crate) fn new() -> Self {
        Self {
            s: Mutex::new(State {
                regs: HashMap::new(),
                sink: Sink::new(),
                reply: VecDeque::new(),
                reply_count: 0,
                reply_code: 0,
                reply_ready: false,
                reply_error: false,
                fifo: Vec::new(),
                addr: 0,
                i2c_offset: 0,
                cause: 0,
                ready_mask: 0xF,
                delays_us: 0,
                log: Vec::new(),
            }),
        }
    }

    pub(crate) fn sink<R>(&self, f: impl FnOnce(&mut Sink) -> R) -> R {
        f(&mut self.s.lock().unwrap().sink)
    }

    pub(crate) fn reg(&self, off: usize) -> u32 {
        *self.s.lock().unwrap().regs.get(&off).unwrap_or(&0)
    }

    pub(crate) fn set_ready_mask(&self, m: u32) {
        self.s.lock().unwrap().ready_mask = m;
    }

    pub(crate) fn raise(&self, bits: u32) {
        self.s.lock().unwrap().cause |= bits;
    }

    pub(crate) fn requests(&self) -> Vec<(u32, u32, usize)> {
        self.s.lock().unwrap().log.clone()
    }

    pub(crate) fn waited_us(&self) -> u64 {
        self.s.lock().unwrap().delays_us
    }
}

impl State {
    fn run_aux(&mut self, v: u32) {
        let cmd = (v >> 8) & 0xF;
        let addr_only = v & regs::AUX_ADDRESS_ONLY != 0;
        let len = if addr_only { 0 } else { (v & 0xF) as usize + 1 };
        let native = cmd & 8 != 0;
        self.log.push((cmd, self.addr, len));
        self.reply.clear();
        self.reply_ready = false;
        self.reply_error = false;
        if self.sink.dead {
            self.reply_error = true;
            return;
        }
        let code = |field: u32| if native { field } else { field << 2 };
        if native && self.sink.defer > 0 {
            self.sink.defer -= 1;
            self.reply_code = code(2);
            self.reply_count = 0;
            self.reply_ready = true;
            return;
        }
        let a = self.addr as usize;
        self.reply_code = 0;
        let data: Vec<u8> = std::mem::take(&mut self.fifo);
        match cmd & 0xB {
            // Native read.
            9 => {
                for i in 0..len {
                    self.reply.push_back(self.read_dpcd(a + i));
                }
            }
            // Native write.
            8 => {
                if self.sink.nack_write.contains(&self.addr) {
                    self.reply_code = code(1);
                } else {
                    for (i, b) in data.iter().enumerate() {
                        if a + i < self.sink.dpcd.len() {
                            self.sink.dpcd[a + i] = *b;
                        }
                    }
                }
            }
            // I2C write (the offset, or the segment).
            0 => {
                if self.addr == 0x50
                    && let Some(o) = data.first()
                {
                    self.i2c_offset = usize::from(*o);
                }
            }
            // I2C read.
            1 => {
                for i in 0..len {
                    let b = self
                        .sink
                        .edid
                        .get(self.i2c_offset + i)
                        .copied()
                        .unwrap_or(0);
                    self.reply.push_back(b);
                }
                self.i2c_offset += len;
            }
            _ => {}
        }
        self.reply_count = u32::try_from(self.reply.len()).unwrap();
        self.reply_ready = true;
    }

    fn read_dpcd(&self, a: usize) -> u8 {
        if (0x202..0x208).contains(&a) {
            return self.sink.training_status()[a - 0x202];
        }
        self.sink.dpcd.get(a).copied().unwrap_or(0)
    }
}

impl Words for Sim {
    fn read(&self, off: usize) -> u32 {
        let mut s = self.s.lock().unwrap();
        match off {
            regs::HPD_INPUT_STATE => u32::from(s.sink.hpd),
            regs::AUX_STATUS => {
                u32::from(s.reply_ready)
                    | if s.reply_error {
                        regs::AUX_REPLY_ERROR
                    } else {
                        0
                    }
            }
            regs::AUX_REPLY_CODE => s.reply_code,
            regs::AUX_REPLY_DATA_COUNT => s.reply_count,
            regs::AUX_REPLY_DATA => u32::from(s.reply.pop_front().unwrap_or(0)),
            regs::INTERRUPT_CAUSE => std::mem::take(&mut s.cause),
            regs::PHY_STATUS => s.ready_mask,
            o => *s.regs.get(&o).unwrap_or(&0),
        }
    }

    fn write(&self, off: usize, v: u32) {
        let mut s = self.s.lock().unwrap();
        match off {
            regs::AUX_WRITE_FIFO => s.fifo.push(u8::try_from(v & 0xFF).unwrap()),
            regs::AUX_ADDRESS => s.addr = v,
            regs::AUX_COMMAND => s.run_aux(v),
            o => {
                s.regs.insert(o, v);
            }
        }
    }
}

impl Delay for Sim {
    fn delay_us(&self, us: u64) {
        self.s.lock().unwrap().delays_us += us;
    }
}

/// A PHY that records what it is told.
pub(crate) struct TestPhy {
    pub lanes: u8,
    pub log: Mutex<Vec<String>>,
}

impl TestPhy {
    pub(crate) fn new(lanes: u8) -> Self {
        Self {
            lanes,
            log: Mutex::new(Vec::new()),
        }
    }
}

impl Phy for TestPhy {
    fn set_link(&self, rate: Rate, lanes: u8) {
        self.log
            .lock()
            .unwrap()
            .push(format!("link {} x{lanes}", rate.khz()));
    }

    fn set_drive(&self, d: Drive) {
        self.log
            .lock()
            .unwrap()
            .push(format!("drive {} {}", d.swing, d.emphasis));
    }

    fn lanes(&self) -> u8 {
        self.lanes
    }
}
