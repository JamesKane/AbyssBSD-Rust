// SPDX-License-Identifier: BSD-2-Clause
//! The clocks over scripted replies, and over the Sky1's own AML
//! (`tests/corpus/acpi/opi6p`) talking to a stand-in for the firmware
//! behind `\_SB.PMMX`'s mailbox.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the stand-in firmware splits rates into words and indexes its mailbox"
)]

extern crate std;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use std::sync::Mutex;

use aml::host::{Host, HostError, RegionAccess, RegionSpace};
use bus::provider::{Clock, Hz, ProviderError};
use sched::Sleepable;

use super::*;

fn s() -> Sleepable {
    // SAFETY: host tests run on a thread that may block.
    unsafe { Sleepable::assume() }
}

/// Replies by method, in order; records each call.
#[derive(Default)]
struct Scripted {
    replies: Mutex<BTreeMap<&'static str, Vec<Vec<u8>>>>,
    calls: Mutex<Vec<(&'static str, Vec<u64>)>>,
}

impl Scripted {
    fn reply(&self, method: &'static str, words: &[u32]) {
        let mut r: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        r.resize(0x60, 0);
        self.replies
            .lock()
            .unwrap()
            .entry(method)
            .or_default()
            .push(r);
    }
}

impl Methods for &Scripted {
    fn call(&self, _: &Sleepable, m: &'static str, args: &[u64]) -> Result<Vec<u8>, ProviderError> {
        self.calls.lock().unwrap().push((m, args.to_vec()));
        let mut r = self.replies.lock().unwrap();
        let q = r.get_mut(m).ok_or(ProviderError::Unsupported)?;
        if q.is_empty() {
            return Err(ProviderError::Unsupported);
        }
        Ok(q.remove(0))
    }
}

#[test]
fn operations_are_scmi_messages() {
    let f = Scripted::default();
    for m in ["CLKC", "CLKC", "SCLK"] {
        f.reply(m, &[0]);
    }
    f.reply("GCLK", &[0, 0x0bebc200, 0]);
    let c = FirmwareClocks::new(&f);
    let s = s();
    c.enable(&s, 0x107).unwrap();
    c.disable(&s, 0x107).unwrap();
    assert_eq!(c.rate(&s, 0x107), Ok(Hz(200_000_000)));
    c.set_rate(&s, 7, Hz(0x1_0000_0010)).unwrap();
    assert_eq!(
        *f.calls.lock().unwrap(),
        [
            ("CLKC", alloc::vec![0x107, 1]),
            ("CLKC", alloc::vec![0x107, 0]),
            ("GCLK", alloc::vec![0x107]),
            ("SCLK", alloc::vec![7, 0x10, 1]),
        ]
    );
    // Firmware's refusal, and the AML's timeout.
    f.reply("CLKC", &[(-4i32).cast_unsigned()]);
    assert_eq!(c.enable(&s, 999), Err(ProviderError::NoSuchLine));
    f.replies
        .lock()
        .unwrap()
        .entry("GCLK")
        .or_default()
        .push(alloc::vec![0x0b, 0, 0, 0]);
    assert_eq!(c.rate(&s, 1), Err(ProviderError::Timeout));
}

/// Rates come a page at a time: each request starts at the index of the
/// first rate not yet seen.
#[test]
fn rates_are_described_in_pages() {
    let f = Scripted::default();
    f.reply("CLKD", &[0, 2 | 1 << 16, 100, 0, 300, 0]);
    f.reply("CLKD", &[0, 1, 200, 0]);
    let c = FirmwareClocks::new(&f);
    assert_eq!(c.round_rate(&s(), 3, Hz(250)), Ok(Hz(200)));
    let calls = f.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        [("CLKD", alloc::vec![3, 0]), ("CLKD", alloc::vec![3, 2])]
    );
    // A range answers at once.
    f.reply("CLKD", &[0, 3 | 1 << 12, 100, 0, 1000, 0, 100, 0]);
    assert_eq!(c.round_rate(&s(), 3, Hz(950)), Ok(Hz(900)));
}

/// The mailbox `\_SB.PMMX` writes: a channel-free bit, the header, and
/// the payload, with a doorbell at 0x80.
const MBOX: u64 = 0x065d_0000;
const MBOX_LEN: usize = 0xa0;

/// The firmware's side of the AML's SCMI channel, with a few clocks, over
/// the corpus host for everything else.
struct Firmware {
    inner: acpi_corpus::Machine,
    mbox: [u8; MBOX_LEN],
    /// Rate and whether enabled, by clock.
    clocks: BTreeMap<u32, (u64, bool)>,
    /// Each message: protocol, message, payload.
    seen: Vec<(u8, u8, Vec<u32>)>,
}

impl Firmware {
    fn new(inner: acpi_corpus::Machine) -> Self {
        let mut mbox = [0; MBOX_LEN];
        // The channel starts free.
        mbox[4] = 1;
        let clocks = [(0x107, (100_000_000, true)), (0x10, (24_000_000, false))]
            .into_iter()
            .collect();
        Self {
            inner,
            mbox,
            clocks,
            seen: Vec::new(),
        }
    }

    fn word(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.mbox[off..off + 4].try_into().unwrap())
    }

    fn set_word(&mut self, off: usize, v: u32) {
        self.mbox[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Answers the message in the channel, as firmware does when rung.
    fn answer(&mut self) {
        let header = self.word(0x18);
        let (msg, proto) = ((header & 0xff) as u8, (header >> 10 & 0xff) as u8);
        let len = self.word(0x14) as usize;
        let payload: Vec<u32> = (0..(len - 4) / 4)
            .map(|k| self.word(0x1c + 4 * k))
            .collect();
        self.seen.push((proto, msg, payload.clone()));
        const NOT_FOUND: u32 = (-4i32).cast_unsigned();
        let reply: Vec<u32> = match (proto, msg, &payload[..]) {
            (0x14, scmi::RATE_GET, [id]) => match self.clocks.get(id) {
                Some(&(rate, _)) => alloc::vec![0, rate as u32, (rate >> 32) as u32],
                None => alloc::vec![NOT_FOUND],
            },
            (0x14, scmi::CONFIG_SET, [id, attrs]) => match self.clocks.get_mut(id) {
                Some(c) => {
                    c.1 = attrs & 1 != 0;
                    alloc::vec![0]
                }
                None => alloc::vec![NOT_FOUND],
            },
            (0x14, scmi::RATE_SET, [_flags, id, lo, hi]) => match self.clocks.get_mut(id) {
                Some(c) => {
                    c.0 = u64::from(*hi) << 32 | u64::from(*lo);
                    alloc::vec![0]
                }
                None => alloc::vec![NOT_FOUND],
            },
            // Three discrete rates.
            (0x14, scmi::DESCRIBE_RATES, [_, 0]) => {
                alloc::vec![0, 3, 24_000_000, 0, 50_000_000, 0, 100_000_000, 0]
            }
            _ => alloc::vec![(-1i32).cast_unsigned()],
        };
        for (k, w) in reply.iter().enumerate() {
            self.set_word(0x1c + 4 * k, *w);
        }
        let free = self.word(4) | 1;
        self.set_word(4, free);
    }

    fn mine(a: &RegionAccess) -> Option<usize> {
        let addr = a.address();
        (a.space == RegionSpace::SystemMemory && (MBOX..MBOX + MBOX_LEN as u64).contains(&addr))
            .then(|| (addr - MBOX) as usize)
    }
}

impl Host for Firmware {
    fn read(&mut self, a: &RegionAccess) -> Result<u64, HostError> {
        let Some(off) = Self::mine(a) else {
            return self.inner.read(a);
        };
        let mut v = 0;
        for i in 0..a.width.bytes() as usize {
            v |= u64::from(self.mbox[off + i]) << (8 * i);
        }
        Ok(v)
    }

    fn write(&mut self, a: &RegionAccess, value: u64) -> Result<(), HostError> {
        let Some(off) = Self::mine(a) else {
            return self.inner.write(a, value);
        };
        for (i, b) in (0..a.width.bytes() as usize).zip(value.to_le_bytes()) {
            self.mbox[off + i] = b;
        }
        // The doorbell: firmware answers and the bell clears.
        if off == 0x80 && value & 1 != 0 {
            self.mbox[0x80] &= !1;
            self.answer();
        }
        Ok(())
    }
}

/// The Sky1's AML with the firmware stand-in, as [`Methods`].
struct Sky1 {
    state: Mutex<(aml::Interpreter, Firmware)>,
    node: aml::NodeId,
}

impl Sky1 {
    fn load() -> Self {
        let (i, h) = acpi_corpus::load("opi6p");
        let node = i
            .lookup(&aml::AmlPath::parse("\\_SB.CCLK").unwrap())
            .unwrap();
        Self {
            state: Mutex::new((i, Firmware::new(h))),
            node,
        }
    }
}

impl Methods for &Sky1 {
    fn call(&self, _: &Sleepable, m: &'static str, args: &[u64]) -> Result<Vec<u8>, ProviderError> {
        let mut st = self.state.lock().unwrap();
        let (i, h) = &mut *st;
        invoke(i, h, self.node, m, args)
    }
}

/// The real methods carry the SCMI messages the driver means, and their
/// replies read back as the firmware gave them.
#[test]
fn sky1_methods_speak_scmi() {
    let sky1 = Sky1::load();
    {
        let st = sky1.state.lock().unwrap();
        for m in METHODS {
            assert!(st.0.child(sky1.node, m).is_some(), "{m}");
        }
    }
    let c = FirmwareClocks::new(&sky1);
    let s = s();
    // COM0's `uartclk`, as `CLKT` names it.
    assert_eq!(c.rate(&s, 0x107), Ok(Hz(100_000_000)));
    c.enable(&s, 0x10).unwrap();
    c.set_rate(&s, 0x10, Hz(50_000_000)).unwrap();
    assert_eq!(c.rate(&s, 0x10), Ok(Hz(50_000_000)));
    assert_eq!(c.round_rate(&s, 0x10, Hz(60_000_000)), Ok(Hz(50_000_000)));
    assert_eq!(c.rate(&s, 0x999), Err(ProviderError::NoSuchLine));
    let st = sky1.state.lock().unwrap();
    assert!(st.1.clocks[&0x10].1, "enabled");
    assert_eq!(
        st.1.seen,
        [
            (0x14, scmi::RATE_GET, alloc::vec![0x107]),
            (0x14, scmi::CONFIG_SET, alloc::vec![0x10, 1]),
            (0x14, scmi::RATE_SET, alloc::vec![0, 0x10, 50_000_000, 0]),
            (0x14, scmi::RATE_GET, alloc::vec![0x10]),
            (0x14, scmi::DESCRIBE_RATES, alloc::vec![0x10, 0]),
            (0x14, scmi::RATE_GET, alloc::vec![0x999]),
        ]
    );
}

/// With firmware silent, the AML gives up and says so.
#[test]
fn a_silent_firmware_times_out() {
    let sky1 = Sky1::load();
    // Busy forever: the AML waits for the channel, then returns 6.
    sky1.state.lock().unwrap().1.mbox[4] = 0;
    let c = FirmwareClocks::new(&sky1);
    assert_eq!(c.rate(&s(), 0x107), Err(ProviderError::Timeout));
}

/// The power-management firmware behind the agent's mailbox: it answers
/// a clock message when the doorbell rings, as the platform does, by
/// writing the reply and marking the channel free.
struct Platform {
    regs: Mutex<[u32; 32]>,
    /// Rates by clock ID; an ID not here is `NOT_FOUND`.
    rates: BTreeMap<u32, u64>,
    enabled: Mutex<Vec<(u32, u32)>>,
    /// Never answers (the channel stays the platform's).
    silent: bool,
}

impl Platform {
    fn new(rates: &[(u32, u64)]) -> Self {
        let mut regs = [0u32; 32];
        regs[1] = 1; // channel free
        Self {
            regs: Mutex::new(regs),
            rates: rates.iter().copied().collect(),
            enabled: Mutex::new(Vec::new()),
            silent: false,
        }
    }
}

impl bus::mailbox::Mailbox for Platform {
    fn words(&self) -> usize {
        32
    }
    fn read(&self, w: usize) -> u32 {
        self.regs.lock().unwrap()[w]
    }
    fn write(&self, w: usize, v: u32) {
        self.regs.lock().unwrap()[w] = v;
    }
    fn ring(&self, channel: u32) -> Result<(), ProviderError> {
        assert_eq!(channel, 8);
        if self.silent {
            return Ok(());
        }
        let mut r = self.regs.lock().unwrap();
        let (hdr, len) = (r[6], r[5]);
        assert_eq!(r[1] & 1, 0, "the agent hands the channel over");
        assert_eq!(r[4], 0, "polled");
        let (msg, proto) = (hdr & 0xff, hdr >> 10 & 0xff);
        assert_eq!(proto, 0x14);
        let p: Vec<u32> = r[7..7 + (len as usize - 4) / 4].to_vec();
        let id = if msg == 5 { p[1] } else { p[0] };
        let reply: Vec<u32> = match (msg, self.rates.get(&id)) {
            (_, None) => alloc::vec![(-4i32).cast_unsigned()],
            (6, Some(&hz)) => alloc::vec![0, hz as u32, (hz >> 32) as u32],
            (7, Some(_)) => {
                self.enabled.lock().unwrap().push((id, p[1]));
                alloc::vec![0]
            }
            _ => alloc::vec![(-1i32).cast_unsigned()],
        };
        for (i, w) in reply.iter().enumerate() {
            r[7 + i] = *w;
        }
        r[5] = 4 + 4 * reply.len() as u32;
        r[1] = 1;
        Ok(())
    }
}

fn no_pause(_: &Sleepable, _: u32) {}

fn agent_over(p: Arc<Platform>) -> FirmwareClocks<agent::Agent> {
    let src = bus::mailbox::MailboxSource::new(p);
    FirmwareClocks::new(agent::Agent::with_pause(src.channel(8), no_pause))
}

#[test]
fn the_agent_gates_and_rates_clocks_over_its_mailbox() {
    let s = s();
    // The FCH GPIO's APB clock (262) and I2C3's (256).
    let p = Arc::new(Platform::new(&[(262, 200_000_000), (256, 100_000_000)]));
    let c = agent_over(Arc::clone(&p));
    c.enable(&s, 262).unwrap();
    c.disable(&s, 262).unwrap();
    assert_eq!(*p.enabled.lock().unwrap(), [(262, 1), (262, 0)]);
    assert_eq!(c.rate(&s, 256), Ok(Hz(100_000_000)));
    // An ID the platform does not know.
    assert_eq!(c.enable(&s, 999), Err(ProviderError::NoSuchLine));
    // The header carried a fresh token each time.
    assert_eq!(p.regs.lock().unwrap()[6] >> 18, 4);
    assert_eq!(agent::header(0x14, 7, 1), 0x0004_5007);
}

#[test]
fn a_platform_that_never_answers_times_out() {
    let s = s();
    let mut p = Platform::new(&[(262, 1)]);
    p.silent = true;
    let p = Arc::new(p);
    let c = agent_over(Arc::clone(&p));
    assert_eq!(c.enable(&s, 262), Err(ProviderError::Timeout));
    // The channel is still the platform's: the next request is refused.
    assert_eq!(c.enable(&s, 262), Err(ProviderError::BusBusy));
}
