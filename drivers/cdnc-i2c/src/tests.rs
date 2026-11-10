// SPDX-License-Identifier: BSD-2-Clause
//! The controller against a model of the IP and a register-file target
//! on its bus, the divider choice, and the Sky1's buses as firmware
//! describes them.

extern crate std;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use std::sync::Mutex;

use super::*;

/// A message under way on the modelled bus.
#[derive(Debug)]
struct Active {
    read: bool,
    /// The next written byte is the target's register pointer.
    first: bool,
}

/// The modelled controller and its bus: one target, at 0x32, with a
/// 256-byte register file and an auto-incrementing pointer.
#[derive(Default)]
struct State {
    cr: u32,
    isr: u32,
    /// Receive: bytes still to receive. Probed width: 8 bits.
    count: u32,
    tx: VecDeque<u8>,
    rx: VecDeque<u8>,
    active: Option<Active>,
    /// The bus is held between messages (SCL low, no STOP).
    held: bool,
    busy: bool,
    /// The bus never goes idle (another host).
    stuck: bool,
    regs: Vec<u8>,
    ptr: usize,
    /// START, repeated START, and STOP, in order.
    bus: Vec<String>,
    time_out: u32,
}

#[derive(Default)]
struct Model(Mutex<State>);

impl State {
    fn stop(&mut self) {
        self.bus.push(String::from("P"));
        self.held = false;
        self.busy = false;
        self.active = None;
    }

    /// The bus moves on: queued bytes go out, wanted bytes come in.
    fn progress(&mut self) {
        let hold = self.cr & reg::CR_HOLD != 0;
        let Some(a) = &mut self.active else { return };
        if a.read {
            if self.held {
                return;
            }
            let room = reg::FIFO - self.rx.len();
            let n = (self.count as usize).min(room);
            for _ in 0..n {
                let v = self.regs[self.ptr % self.regs.len()];
                self.ptr += 1;
                self.rx.push_back(v);
            }
            self.count -= u32::try_from(n).unwrap();
            if n > 0 {
                self.isr |= reg::ISR_DATA;
            }
            if self.count == 0 {
                self.isr |= reg::ISR_COMP;
                if hold {
                    self.held = true;
                } else {
                    self.stop();
                }
            }
        } else {
            if self.tx.is_empty() {
                return;
            }
            while let Some(b) = self.tx.pop_front() {
                if a.first {
                    self.ptr = usize::from(b);
                    a.first = false;
                } else {
                    let i = self.ptr % self.regs.len();
                    self.regs[i] = b;
                    self.ptr += 1;
                }
            }
            self.isr |= reg::ISR_COMP;
            if hold {
                self.held = true;
            } else {
                self.stop();
            }
        }
    }
}

impl Words for Arc<Model> {
    fn read32(&self, off: usize) -> u32 {
        let mut st = self.0.lock().unwrap();
        match off {
            reg::CR => st.cr,
            reg::SR => {
                let mut v = 0;
                if !st.rx.is_empty() {
                    v |= reg::SR_RXDV;
                }
                if st.busy || st.stuck {
                    v |= reg::SR_BA;
                }
                v
            }
            reg::ISR => {
                st.progress();
                st.isr
            }
            reg::XFER_SIZE => match &st.active {
                Some(Active { read: false, .. }) | None if st.cr & reg::CR_RW == 0 => {
                    u32::try_from(st.tx.len()).unwrap()
                }
                _ => st.count,
            },
            reg::DATA => match st.rx.pop_front() {
                Some(b) => u32::from(b),
                None => {
                    st.isr |= reg::ISR_RX_UNF;
                    0
                }
            },
            reg::TIME_OUT => st.time_out,
            _ => 0,
        }
    }

    fn write32(&self, off: usize, v: u32) {
        let mut st = self.0.lock().unwrap();
        match off {
            reg::CR => {
                if v & reg::CR_CLR_FIFO != 0 {
                    st.tx.clear();
                    st.rx.clear();
                    st.count = 0;
                }
                st.cr = v & !reg::CR_CLR_FIFO;
                // Letting go of a held bus ends it once nothing is left
                // to move.
                if v & reg::CR_HOLD == 0 && st.held {
                    let pending = match &st.active {
                        Some(a) if a.read => st.count > 0,
                        Some(_) => !st.tx.is_empty(),
                        None => false,
                    };
                    if pending {
                        st.held = false;
                    } else {
                        st.stop();
                    }
                }
            }
            reg::XFER_SIZE => {
                if st.cr & (reg::CR_MS | reg::CR_RW) == reg::CR_MS | reg::CR_RW {
                    st.count = v & 0xff;
                }
            }
            reg::ADDR => {
                let s = if st.held { "Sr" } else { "S" };
                let read = st.cr & reg::CR_RW != 0;
                let dir = if read { 'R' } else { 'W' };
                st.bus.push(format!("{s} {v:#x} {dir}"));
                st.held = false;
                st.busy = true;
                if v == 0x32 {
                    st.active = Some(Active { read, first: true });
                } else {
                    st.isr |= reg::ISR_NACK;
                    st.stop();
                }
            }
            reg::DATA => {
                if st.tx.len() == reg::FIFO {
                    st.isr |= reg::ISR_TX_OVF;
                } else {
                    st.tx.push_back(v.to_le_bytes()[0]);
                }
            }
            reg::ISR => st.isr &= !v,
            reg::TIME_OUT => st.time_out = v,
            _ => {}
        }
    }
}

/// Waits cost nothing; counts them.
#[derive(Default)]
struct Instant(Mutex<(u64, u64)>);

impl Pace for Instant {
    fn delay_us(&self, us: u64) {
        self.0.lock().unwrap().0 += us;
    }
    fn sleep_ms(&self, _: &Sleepable) {
        self.0.lock().unwrap().1 += 1;
    }
}

fn s() -> Sleepable {
    // SAFETY: host tests run on a thread that may block.
    unsafe { Sleepable::assume() }
}

fn setup() -> (Arc<Model>, Controller<Arc<Model>>) {
    let m = Arc::new(Model::default());
    m.0.lock().unwrap().regs = (0..=255).collect();
    let c = Controller::new(Arc::clone(&m), 100_000_000, 400_000).unwrap();
    m.0.lock().unwrap().bus.clear();
    (m, c)
}

fn msg(op: Op<'_>) -> Msg<'_> {
    Msg {
        address: 0x32,
        ten_bit: false,
        op,
    }
}

#[test]
fn dividers_pick_the_fastest_rate_not_above_the_target() {
    // 100 MHz in: 400 kHz wants 11.36 per (A + 1)(B + 1) * 22.
    assert_eq!(dividers(100_000_000, 400_000), Some((0, 11, 378_787)));
    assert_eq!(dividers(100_000_000, 100_000), Some((0, 45, 98_814)));
    // Slow buses need the first divider.
    let (a, b, r) = dividers(20_000_000, 10_000).unwrap();
    assert!(r <= 10_000 && a > 0 && b < 64, "{a} {b} {r}");
    // Out of reach either way.
    assert_eq!(dividers(1_000_000, 100_000), None);
    assert_eq!(dividers(1_000_000_000, 1_000), None);
    assert_eq!(dividers(100_000_000, 0), None);
}

#[test]
fn setup_probes_the_counter_and_programs_the_dividers() {
    let (m, c) = setup();
    assert_eq!(c.max_read(), 252);
    assert_eq!(c.speed(), 378_787);
    let st = m.0.lock().unwrap();
    assert_eq!(st.time_out, 0xff);
    assert_eq!(
        st.cr,
        reg::CR_ACK_EN | reg::CR_NEA | reg::CR_MS | 11 << reg::CR_DIV_B_SHIFT
    );
    drop(st);
    assert_eq!(
        Controller::new(Arc::clone(&m), 0, 400_000).unwrap_err(),
        SetupError::NoClock
    );
}

#[test]
fn a_register_read_is_one_transfer_with_a_repeated_start() {
    let (m, c) = setup();
    let pace = Instant::default();
    let mut t = [0u8; 7];
    c.transfer(
        &pace,
        &s(),
        &mut [msg(Op::Write(&[0x10])), msg(Op::Read(&mut t))],
    )
    .unwrap();
    assert_eq!(t, [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16]);
    let st = m.0.lock().unwrap();
    assert_eq!(st.bus, ["S 0x32 W", "Sr 0x32 R", "P"]);
    assert!(!st.busy && st.cr & reg::CR_HOLD == 0);
}

#[test]
fn long_messages_feed_and_drain_the_fifo() {
    let (m, c) = setup();
    let pace = Instant::default();
    // 40 bytes from register 0x40: the FIFO is fed as it drains.
    let mut data = Vec::from([0x40u8]);
    data.extend((0..40).map(|i| 0xa0 ^ i));
    c.transfer(&pace, &s(), &mut [msg(Op::Write(&data))])
        .unwrap();
    assert_eq!(m.0.lock().unwrap().bus, ["S 0x32 W", "P"]);
    // Read them back in one 40-byte message, the bus held while the FIFO
    // is drained.
    let mut back = [0u8; 40];
    c.transfer(
        &pace,
        &s(),
        &mut [msg(Op::Write(&[0x40])), msg(Op::Read(&mut back))],
    )
    .unwrap();
    assert_eq!(&back[..], &data[1..]);
    assert_eq!(
        m.0.lock().unwrap().bus,
        ["S 0x32 W", "P", "S 0x32 W", "Sr 0x32 R", "P"]
    );
}

#[test]
fn errors_end_the_transfer_and_reset_the_controller() {
    let (m, c) = setup();
    let pace = Instant::default();
    let mut b = [0u8; 2];
    let r = c.transfer(
        &pace,
        &s(),
        &mut [Msg {
            address: 0x50,
            ten_bit: false,
            op: Op::Read(&mut b),
        }],
    );
    assert_eq!(r, Err(ProviderError::NoAck));
    {
        let st = m.0.lock().unwrap();
        assert_eq!(st.bus, ["S 0x50 R", "P"]);
        assert_eq!(st.isr, 0, "status acknowledged");
        assert_eq!(st.cr & reg::CR_HOLD, 0);
    }
    // What the controller cannot do.
    assert_eq!(
        c.transfer(&pace, &s(), &mut [msg(Op::Write(&[]))]),
        Err(ProviderError::Unsupported)
    );
    let mut big = [0u8; 253];
    assert_eq!(
        c.transfer(&pace, &s(), &mut [msg(Op::Read(&mut big))]),
        Err(ProviderError::Unsupported)
    );
    // A bus another host keeps: busy after 500 ms of polling.
    m.0.lock().unwrap().stuck = true;
    assert_eq!(
        c.transfer(&pace, &s(), &mut [msg(Op::Write(&[0]))]),
        Err(ProviderError::BusBusy)
    );
    let (us, ms) = *pace.0.lock().unwrap();
    assert_eq!((us, ms), (1000, 499));
}

/// The Sky1's buses as firmware gives them: each its SCMI clock; only
/// I2C0 and I2C2 name a reset line (so it is optional); I2C6, the EC's,
/// is not present (`_STA` 0: firmware keeps it).
#[test]
fn sky1_buses() {
    use acpi_platform::PlatformSource;
    let (mut i, mut h) = acpi_corpus::load("opi6p");
    let devices = acpi::deps::devices(&mut i, &mut h);
    let x = cix_sky1::PLATFORM.extract(&mut i, &mut h, &devices);
    let mut got = Vec::new();
    for &d in &devices {
        if i.hid(d, &mut h).ok().flatten().as_deref() != Some(HID) {
            continue;
        }
        let clock: Vec<u32> = x
            .description
            .clocks
            .iter()
            .filter(|c| c.consumer == d)
            .map(|c| c.id)
            .collect();
        let reset: Vec<u32> = x
            .description
            .resets
            .iter()
            .filter(|r| r.consumer == d)
            .map(|r| r.index)
            .collect();
        got.push((i.path(d).to_text(), clock, reset));
    }
    let want: Vec<(String, Vec<u32>, Vec<u32>)> = [0u32, 1, 2, 3, 4, 5, 7]
        .into_iter()
        .map(|n| {
            let reset = if matches!(n, 0 | 2) {
                alloc::vec![18 + n]
            } else {
                Vec::new()
            };
            (format!("\\_SB_.I2C{n}"), alloc::vec![253 + n], reset)
        })
        .collect();
    assert_eq!(got, want);
}
