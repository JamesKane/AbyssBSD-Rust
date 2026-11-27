// SPDX-License-Identifier: BSD-2-Clause
//! The protocol against a fake SMC on both windows, the encodings, and the
//! policies against the Mac Pro (Late 2013)'s measured behavior
//! (docs/MACPRO.md §2).

extern crate std;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::vec::Vec;

use crate::policy::{Curve, FALL_STEP, Floor, Limit, OFF_SAMPLES};
use crate::smc::{Backend, Clock, Key, MmioIo, PortIo, RETIRE_AFTER, Smc, SmcError};
use crate::value;

/// A clock whose delays advance it.
#[derive(Default)]
struct FakeClock(AtomicU64);

impl Clock for FakeClock {
    fn delay_us(&self, us: u64) {
        self.0.fetch_add(us, Ordering::Relaxed);
    }
    fn now_us(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// The SMC's keys: type and value, in key order (the index order).
#[derive(Clone, Default)]
struct Keys(BTreeMap<Key, ([u8; 4], Vec<u8>)>);

impl Keys {
    fn with(mut self, k: &[u8; 4], ty: &[u8; 4], v: &[u8]) -> Self {
        self.0.insert(*k, (*ty, v.to_vec()));
        self
    }
    /// The Mac Pro's keys the driver uses, with their types, lengths and
    /// values from a FreeBSD boot after a power-off (the fork's
    /// `fbsd-keys-cold.txt`, 2026-11-26: 770 keys, 104 of them `sp78`).
    /// `#KEY` is set to this table's own count.
    fn mac_pro() -> Self {
        let mut k = Self::default()
            .with(b"LDKN", b"ui8 ", &[0x02])
            .with(b"REV ", b"{rev", &[0x02, 0x20, 0x0f, 0x00, 0x00, 0x18])
            .with(b"FNum", b"ui8 ", &[0x01])
            .with(
                b"F0ID",
                b"{fds",
                &[
                    0, 0, 0x10, 0, b'M', b'a', b'i', b'n', b' ', 0, 0, 0, 0, 0, 0, 0,
                ],
            )
            .with(b"F0Ac", b"fpe2", &[0x0d, 0x38])
            .with(b"F0Mn", b"fpe2", &[0x0c, 0x58])
            .with(b"F0Mx", b"fpe2", &[0x1e, 0xb8])
            .with(b"F0Tg", b"fpe2", &[0x0d, 0xec])
            .with(b"FS! ", b"ui16", &[0x00, 0x00])
            .with(b"MSLG", b"ui8 ", &[0x00])
            .with(b"MSLT", b"flt ", &[0x00, 0x00, 0x00, 0x00])
            .with(b"MSTc", b"ui8 ", &[0x00])
            .with(b"TC0P", b"sp78", &[0x35, 0xc0])
            .with(b"TCXc", b"sp78", &[0x3e, 0x04])
            .with(b"TG0D", b"sp78", &[0x3e, 0x00])
            .with(b"TG1D", b"sp78", &[0x3e, 0xa0]);
        let n = u32::try_from(k.0.len()).unwrap();
        k.0.insert(*b"#KEY", (*b"ui32", n.to_be_bytes().to_vec()));
        k
    }
    fn at(&self, i: u32) -> Option<Key> {
        self.0.keys().nth(usize::try_from(i).ok()?).copied()
    }
}

/// The memory window of an SMC holding `keys`; `jam` makes it never ready.
struct FakeMmio {
    keys: Mutex<Keys>,
    regs: Mutex<[u8; 0x4006]>,
    jam: bool,
}

impl FakeMmio {
    fn new(keys: Keys) -> Self {
        Self {
            keys: Mutex::new(keys),
            regs: Mutex::new([0; 0x4006]),
            jam: false,
        }
    }
    fn run(&self, cmd: u8, r: &mut [u8; 0x4006]) {
        let key: Key = r[0x78..0x7c].try_into().unwrap();
        let mut keys = self.keys.lock().unwrap();
        let status = match cmd {
            0x10 => match keys.0.get(&key) {
                Some((_, v)) => {
                    r[..v.len()].copy_from_slice(v);
                    r[0x7d] = u8::try_from(v.len()).unwrap();
                    0
                }
                None => 0x84,
            },
            0x11 => match keys.0.get_mut(&key) {
                Some((_, v)) => {
                    let n = usize::from(r[0x7d]);
                    *v = r[..n].to_vec();
                    0
                }
                None => 0x84,
            },
            0x12 => match keys.at(u32::from_be_bytes(key)) {
                Some(k) => {
                    r[..4].copy_from_slice(&k);
                    0
                }
                None => 0x84,
            },
            0x13 => match keys.0.get(&key) {
                Some((ty, v)) => {
                    r[..4].copy_from_slice(ty);
                    r[5] = u8::try_from(v.len()).unwrap();
                    0
                }
                None => 0x84,
            },
            _ => 0x80,
        };
        r[0x7f] = status;
        r[0x4005] = 0x20;
    }
}

impl MmioIo for &FakeMmio {
    fn len(&self) -> usize {
        0x10000
    }
    fn read8(&self, off: usize) -> u8 {
        if self.jam && off == 0x4005 {
            return 0;
        }
        self.regs.lock().unwrap()[off]
    }
    fn write8(&self, off: usize, v: u8) {
        let mut r = self.regs.lock().unwrap();
        r[off] = v;
        if off == 0x7f && !self.jam {
            self.run(v, &mut r);
        }
    }
    fn write32(&self, off: usize, v: u32) {
        self.regs.lock().unwrap()[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    }
}

/// The port window of an SMC holding `keys`: the status handshake of
/// `asmc_pio.c`.
struct FakePorts {
    keys: Mutex<Keys>,
    state: Mutex<PortState>,
}

#[derive(Default)]
struct PortState {
    status: u8,
    cmd: u8,
    input: Vec<u8>,
    output: Vec<u8>,
}

impl FakePorts {
    fn new(keys: Keys) -> Self {
        Self {
            keys: Mutex::new(keys),
            state: Mutex::new(PortState::default()),
        }
    }
}

impl PortIo for &FakePorts {
    fn read8(&self, off: u16) -> u8 {
        let mut s = self.state.lock().unwrap();
        if off == 0x04 {
            return s.status;
        }
        let b = if s.output.is_empty() {
            0
        } else {
            s.output.remove(0)
        };
        if s.output.is_empty() {
            s.status = 0;
        }
        b
    }
    fn write8(&self, off: u16, v: u8) {
        let mut s = self.state.lock().unwrap();
        if off == 0x04 {
            *s = PortState {
                status: 0x0c,
                cmd: v,
                ..PortState::default()
            };
            return;
        }
        s.input.push(v);
        let keys = self.keys.lock().unwrap();
        let key = |s: &PortState| -> Key { s.input[..4].try_into().unwrap() };
        match (s.cmd, s.input.len()) {
            (_, 1..=4) => s.status = 0x04,
            // The length after the key: the answer follows.
            // A key the SMC lacks never becomes data-ready, as on the hardware.
            (0x10 | 0x13, 5) if !keys.0.contains_key(&key(&s)) => s.status = 0,
            (0x10, 5) => {
                let v = keys
                    .0
                    .get(&key(&s))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let n = usize::from(s.input[4]);
                let mut out = v;
                out.resize(n, 0);
                s.output = out;
                s.status = 0x05;
            }
            (0x13, 5) => {
                let (ty, v) = keys.0.get(&key(&s)).cloned().unwrap_or_default();
                let mut out = std::vec![u8::try_from(v.len()).unwrap()];
                out.extend_from_slice(&ty);
                out.push(0);
                s.output = out;
                s.status = 0x05;
            }
            (0x12, 5) => {
                let k = keys.at(u32::from_be_bytes(key(&s))).unwrap_or(*b"????");
                s.output = k.to_vec();
                s.status = 0x05;
            }
            (0x11, 5) => s.status = 0x04,
            (0x11, n) => {
                let want = usize::from(s.input[4]);
                if n - 5 == want {
                    let k = key(&s);
                    let v = s.input[5..].to_vec();
                    drop(keys);
                    if let Some(e) = self.keys.lock().unwrap().0.get_mut(&k) {
                        e.1 = v;
                    }
                    s.status = 0;
                } else {
                    s.status = 0x04;
                }
            }
            _ => {}
        }
    }
}

type MmioSmc<'a> = Smc<&'a FakePorts, &'a FakeMmio, FakeClock>;

#[test]
fn the_memory_window_reads_writes_and_lists_keys() {
    let m = FakeMmio::new(Keys::mac_pro());
    let smc: MmioSmc<'_> = Smc::probe_mmio(&m, FakeClock::default())
        .ok()
        .expect("LDKN 2");
    assert_eq!(smc.backend(), Backend::Mmio);
    let mut rev = [0u8; 6];
    smc.read(*b"REV ", &mut rev).unwrap();
    assert_eq!(rev[..3], [2, 0x20, 0x0f]);
    let info = smc.info(*b"F0Mn").unwrap();
    assert_eq!((info.len, &info.ty), (2, b"fpe2"));
    smc.write_checked(*b"F0Mn", &value::to_fpe2(1600).unwrap())
        .unwrap();
    let mut b = [0u8; 2];
    smc.read(*b"F0Mn", &mut b).unwrap();
    assert_eq!(value::fpe2(b), 1600);
    // A write of the wrong length is refused before it reaches the SMC.
    assert_eq!(
        smc.write_checked(*b"F0Mn", &[1, 2, 3]),
        Err(SmcError::Length)
    );
    assert_eq!(smc.read(*b"NOPE", &mut b), Err(SmcError::Refused(0x84)));
    assert_eq!(smc.key_at(0).unwrap(), *b"#KEY");
}

#[test]
fn an_smc_without_ldkn_2_is_not_taken_through_memory() {
    let m = FakeMmio::new(Keys::mac_pro().with(b"LDKN", b"ui8 ", &[1]));
    assert!(MmioSmc::probe_mmio(&m, FakeClock::default()).is_err());
}

#[test]
fn the_port_window_speaks_the_same_protocol() {
    let p = FakePorts::new(Keys::mac_pro());
    let smc: MmioSmc<'_> = Smc::with_ports(&p, FakeClock::default());
    assert_eq!(smc.backend(), Backend::Port);
    let mut n = [0u8; 1];
    smc.read(*b"FNum", &mut n).unwrap();
    assert_eq!(n, [1]);
    let info = smc.info(*b"TG0D").unwrap();
    assert_eq!((info.len, &info.ty), (2, b"sp78"));
    smc.write(*b"F0Mn", &value::to_fpe2(2000).unwrap()).unwrap();
    let mut b = [0u8; 2];
    smc.read(*b"F0Mn", &mut b).unwrap();
    assert_eq!(value::fpe2(b), 2000);
    assert_eq!(smc.key_at(0).unwrap(), *b"#KEY");
}

#[test]
fn a_memory_window_that_is_never_ready_is_not_taken() {
    let jammed = FakeMmio {
        jam: true,
        ..FakeMmio::new(Keys::mac_pro())
    };
    assert!(MmioSmc::probe_mmio(&jammed, FakeClock::default()).is_err());
}

#[test]
fn a_port_smc_that_never_acknowledges_is_retired() {
    /// Ports that never give the command acknowledgement.
    struct Deaf;
    impl PortIo for &Deaf {
        fn read8(&self, _: u16) -> u8 {
            0
        }
        fn write8(&self, _: u16, _: u8) {}
    }
    let d = Deaf;
    let smc: Smc<&Deaf, &FakeMmio, FakeClock> = Smc::with_ports(&d, FakeClock::default());
    let mut b = [0u8; 2];
    for _ in 0..RETIRE_AFTER {
        assert_eq!(smc.read(*b"F0Ac", &mut b), Err(SmcError::NoAnswer));
    }
    assert!(smc.retired());
    assert_eq!(smc.read(*b"F0Ac", &mut b), Err(SmcError::Retired));
}

#[test]
fn encodings_decode_as_freebsds() {
    assert_eq!(value::fpe2((790u16 << 2).to_be_bytes()), 790);
    assert_eq!(value::to_fpe2(16_383), Some([0xff, 0xfc]));
    assert_eq!(value::to_fpe2(16_384), None, "no wrap");
    assert_eq!(value::sensor_milli(b"sp78", [46, 0x80]), Some(46_500));
    assert_eq!(value::sensor_milli(b"sp78", [0xff, 0x00]), Some(-1000));
    assert_eq!(value::sensor_milli(b"ui16", [0x01, 0x00]), Some(256));
    assert_eq!(value::sensor_milli(b"fpe2", [0, 0]), None);
    assert_eq!(value::milli_c_to_dk(45_000), 3181);
    for v in [0, 1, 790, 2600, 65_535] {
        assert_eq!(value::float_to_u32(value::u32_to_float(v)), v);
    }
    assert_eq!(value::u32_to_float(2600), 2600f32.to_bits());
    // MSLT, little-endian: 100.0 is 00 00 c8 42 (the fork's reading).
    assert_eq!(value::limit([0, 0, 0xc8, 0x42]), (100, true));
    assert_eq!(value::limit(0.3f32.to_bits().to_le_bytes()), (0, true));
    assert_eq!(value::limit([0; 4]), (0, false));
    assert_eq!(value::limit((-1f32).to_bits().to_le_bytes()), (0, false));
}

/// The Mac Pro's floor, as `platform/apple-mac` states it.
const MAC_PRO: Floor = Floor {
    fwmin: 790,
    top: 2600,
    gpu: Curve {
        start: 45,
        full: 65,
    },
    cpu: Curve {
        start: 55,
        full: 80,
    },
};

fn c(deg: i32) -> Option<i32> {
    Some(deg * 1000)
}

#[test]
fn the_floor_follows_the_hottest_sensor_of_each_class() {
    // Undriven D700s idling at 59-60 C: 790 + 1810 * 15/20.
    assert_eq!(
        MAC_PRO.demand(MAC_PRO.gpu, &[c(59), c(60)]),
        790 + 1810 * 15 / 20
    );
    assert_eq!(MAC_PRO.demand(MAC_PRO.gpu, &[c(40), c(44)]), 790);
    assert_eq!(MAC_PRO.demand(MAC_PRO.gpu, &[c(70), c(50)]), 2600);
    // A failed, 0 C or 127 C reading is hot.
    assert_eq!(MAC_PRO.demand(MAC_PRO.cpu, &[c(40), None]), 2600);
    assert_eq!(MAC_PRO.demand(MAC_PRO.cpu, &[Some(0)]), 2600);
    assert_eq!(MAC_PRO.demand(MAC_PRO.cpu, &[c(127)]), 2600);
    // The first sample always writes, even at the firmware minimum (a warm
    // reboot may have left a raised one).
    assert_eq!(MAC_PRO.step(None, &[c(40)], &[c(40)]), (790, true));
    assert_eq!(MAC_PRO.step(Some(790), &[c(40)], &[c(40)]), (790, false));
}

#[test]
fn the_floor_rises_at_once_and_falls_slowly() {
    let hot = [c(70)];
    let cool = [c(40)];
    let (f, w) = MAC_PRO.step(Some(790), &hot, &cool);
    assert_eq!((f, w), (2600, true));
    // Raised: rewritten every sample.
    assert_eq!(MAC_PRO.step(Some(2600), &hot, &cool), (2600, true));
    // Cooled: down FALL_STEP a sample, until it reaches demand.
    let mut floor = 2600;
    let mut samples = 0;
    while floor > 790 {
        let (next, write) = MAC_PRO.step(Some(floor), &cool, &cool);
        assert!(write);
        assert!(floor - next <= FALL_STEP);
        floor = next;
        samples += 1;
    }
    assert_eq!(samples, (2600 - 790_u32).div_ceil(FALL_STEP));
    // Within the band it holds.
    let near = MAC_PRO.gpu.start + 1;
    let (f, _) = MAC_PRO.step(Some(900), &[c(near)], &cool);
    assert_eq!(f, 900);
}

#[test]
fn the_limit_latches_and_releases_as_the_smc_does() {
    let mut l = Limit::default();
    assert!(!l.sample(Some((0, false))));
    assert!(l.sample(Some((2, true))), "on at the first positive sample");
    assert!(l.active());
    // MSLT flickers to 0 at onset: still on.
    for _ in 0..OFF_SAMPLES - 1 {
        assert!(!l.sample(Some((0, false))));
        assert!(l.active());
    }
    assert!(!l.sample(Some((100, true))), "back up: the count restarts");
    for _ in 0..OFF_SAMPLES - 1 {
        l.sample(Some((0, false)));
    }
    // A failed read keeps the state.
    assert!(!l.sample(None));
    assert!(l.active());
    assert!(l.sample(Some((0, false))), "off after OFF_SAMPLES zeros");
    assert!(!l.active());
    l.sample(Some((50, true)));
    assert!(l.release());
    assert!(!l.active());
}

#[test]
fn a_late_listener_learns_of_a_latched_limit() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    crate::publish_limit(true);
    let heard = Arc::new(AtomicBool::new(false));
    let h = Arc::clone(&heard);
    let f: crate::LimitFn = Arc::new(move |on| h.store(on, Ordering::SeqCst));
    crate::on_platform_limit(Arc::clone(&f));
    assert!(heard.load(Ordering::SeqCst), "told at registration");
    crate::publish_limit(false);
    assert!(!heard.load(Ordering::SeqCst));
    crate::off_platform_limit(&f);
    crate::publish_limit(true);
    assert!(!heard.load(Ordering::SeqCst), "removed");
    crate::publish_limit(false);
}

#[test]
fn fan_keys_take_the_fan_number() {
    assert_eq!(crate::fan_key(b"F%Mn", 0), *b"F0Mn");
    assert_eq!(crate::fan_key(b"F%ID", 3), *b"F3ID");
}

#[test]
fn missing_keys_do_not_set_a_port_smc_aside() {
    let p = FakePorts::new(Keys::mac_pro());
    let smc: MmioSmc<'_> = Smc::with_ports(&p, FakeClock::default());
    let mut b = [0u8; 2];
    for _ in 0..2 * RETIRE_AFTER {
        assert_eq!(smc.read(*b"NOPE", &mut b), Err(SmcError::Timeout));
    }
    assert!(!smc.retired());
    assert!(smc.read(*b"F0Ac", &mut b).is_ok());
}

#[test]
fn an_smc_set_aside_is_tried_again_after_the_backoff() {
    /// Ports that stop acknowledging while `deaf` is set.
    struct Flaky {
        deaf: std::sync::atomic::AtomicBool,
        ports: FakePorts,
    }
    impl PortIo for &Flaky {
        fn read8(&self, off: u16) -> u8 {
            if self.deaf.load(Ordering::SeqCst) {
                0
            } else {
                (&self.ports).read8(off)
            }
        }
        fn write8(&self, off: u16, v: u8) {
            (&self.ports).write8(off, v);
        }
    }
    let f = Flaky {
        deaf: std::sync::atomic::AtomicBool::new(true),
        ports: FakePorts::new(Keys::mac_pro()),
    };
    let smc: Smc<&Flaky, &FakeMmio, FakeClock> = Smc::with_ports(&f, FakeClock::default());
    let mut b = [0u8; 2];
    for _ in 0..RETIRE_AFTER {
        assert_eq!(smc.read(*b"F0Ac", &mut b), Err(SmcError::NoAnswer));
    }
    f.deaf.store(false, Ordering::SeqCst);
    // Still set aside until the backoff passes.
    assert_eq!(smc.read(*b"F0Ac", &mut b), Err(SmcError::Retired));
    smc.clock().delay_us(crate::smc::BACKOFF_US);
    assert!(smc.read(*b"F0Ac", &mut b).is_ok(), "tried again, and back");
    assert!(!smc.retired());
}

// The driver's own sampling, against the fake SMC with the properties
// `platform/apple-mac` states for `MacPro6,1`.

use crate::machine::{Machine, Props};

/// `platform/apple-mac`'s `MACPRO6_1`, as `sys/acpi` hands it over.
struct MacPro;

impl MacPro {
    fn get(name: &str) -> Option<aml::Value> {
        apple_mac::MACPRO6_1
            .iter()
            .find(|p| p.name == name)
            .map(|p| acpi::properties::to_value(p.value))
    }
}

impl Props for MacPro {
    fn int(&self, name: &'static str) -> Option<u64> {
        match Self::get(name)? {
            aml::Value::Integer(n) => Some(n),
            _ => None,
        }
    }
    fn ints(&self, name: &'static str) -> Option<Vec<u64>> {
        match Self::get(name)? {
            aml::Value::Package(v) => v
                .into_iter()
                .map(|e| match e {
                    aml::Value::Integer(n) => Some(n),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }
    fn keys(&self, name: &'static str) -> Option<Vec<Key>> {
        match Self::get(name)? {
            aml::Value::Package(v) => v
                .into_iter()
                .map(|e| match e {
                    aml::Value::String(s) => s.as_bytes().try_into().ok(),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }
}

type FakeMachine<'a> = Machine<&'a FakePorts, &'a FakeMmio, FakeClock>;

fn machine(m: &FakeMmio) -> FakeMachine<'_> {
    let smc = Smc::probe_mmio(m, FakeClock::default())
        .ok()
        .expect("LDKN 2");
    let mut mach = Machine {
        smc,
        fans: 1,
        temps: Vec::new(),
        floor: None,
        limit: None,
    };
    mach.floor = mach.fan_floor(&MacPro).expect("a floor");
    mach.limit = mach.limit_keys(&MacPro).expect("a limit");
    assert!(mach.floor.is_some() && mach.limit.is_some());
    mach
}

/// `F0Mn` as the fake SMC holds it.
fn min_speed(m: &FakeMmio) -> u32 {
    let v = &m.keys.lock().unwrap().0[b"F0Mn"].1;
    value::fpe2([v[0], v[1]])
}

fn set(m: &FakeMmio, k: &[u8; 4], v: &[u8]) {
    m.keys.lock().unwrap().0.get_mut(k).unwrap().1 = v.to_vec();
}

#[test]
fn a_cold_boot_raises_the_floor_from_the_gpus() {
    let m = FakeMmio::new(Keys::mac_pro());
    let mach = machine(&m);
    assert_eq!(min_speed(&m), 790);
    mach.floor_tick(mach.floor.as_ref().unwrap());
    // GPU dies at 62.0 and 62.6 C: 790 + 1810 * 17/20.
    assert_eq!(min_speed(&m), 790 + 1810 * 17 / 20);
}

#[test]
fn a_stale_floor_from_a_warm_reboot_is_rewritten() {
    let m = FakeMmio::new(Keys::mac_pro());
    set(&m, b"F0Mn", &value::to_fpe2(1600).unwrap());
    for k in [b"TG0D", b"TG1D", b"TCXc", b"TC0P"] {
        set(&m, k, &[40, 0]);
    }
    let mach = machine(&m);
    mach.floor_tick(mach.floor.as_ref().unwrap());
    assert_eq!(min_speed(&m), 790, "the first sample always writes");
}

#[test]
fn a_missing_sensor_sends_the_floor_to_the_top() {
    let m = FakeMmio::new(Keys::mac_pro());
    let mach = machine(&m);
    m.keys.lock().unwrap().0.remove(b"TG1D");
    mach.floor_tick(mach.floor.as_ref().unwrap());
    assert_eq!(min_speed(&m), 2600);
}

#[test]
fn turning_the_policy_off_restores_the_firmware_minimum() {
    let m = FakeMmio::new(Keys::mac_pro());
    let mach = machine(&m);
    let f = mach.floor.as_ref().unwrap();
    mach.floor_tick(f);
    assert!(min_speed(&m) > 790);
    mach.set_floor_enabled(f, false);
    assert_eq!(min_speed(&m), 790);
    // Off: a sample writes nothing.
    mach.floor_tick(f);
    assert_eq!(min_speed(&m), 790);
    // On again: the next sample writes.
    mach.set_floor_enabled(f, true);
    mach.floor_tick(f);
    assert!(min_speed(&m) > 790);
    mach.floor_restore(f);
    assert_eq!(min_speed(&m), 790);
}

#[test]
fn the_limit_follows_mslt_through_the_smc() {
    let m = FakeMmio::new(Keys::mac_pro());
    let mach = machine(&m);
    let l = mach.limit.as_ref().unwrap();
    assert_eq!(
        mach.limit_tick(l),
        None,
        "off and unchanged: nothing to say"
    );
    set(&m, b"MSLT", &[0x00, 0x00, 0xc8, 0x42]);
    set(&m, b"MSLG", &[2]);
    set(&m, b"MSTc", &[0x11]);
    assert_eq!(mach.limit_tick(l), Some(true));
    assert_eq!(l.state.lock().level, 100);
    assert_eq!(l.gpu_step.load(Ordering::Relaxed), 2);
    assert_eq!(l.cpu_step.load(Ordering::Relaxed), 0x11);
    // Said again on every sample while on.
    assert_eq!(mach.limit_tick(l), Some(true));
    set(&m, b"MSLT", &[0; 4]);
    for _ in 0..OFF_SAMPLES - 1 {
        assert_eq!(mach.limit_tick(l), Some(true));
    }
    assert_eq!(mach.limit_tick(l), Some(false), "off once, after the zeros");
    assert_eq!(mach.limit_tick(l), None);
}

#[test]
fn fan_values_decode_from_the_dump() {
    let m = FakeMmio::new(Keys::mac_pro());
    let mach = machine(&m);
    assert_eq!(mach.fan_speed(*b"F0Ac"), Ok(846));
    assert_eq!(mach.fan_speed(*b"F0Mx"), Ok(1966));
    assert_eq!(mach.sensor(*b"TCXc"), Some(62_015));
    assert_eq!(mach.manual(0), Ok(0));
    mach.set_manual(0, 1).unwrap();
    assert_eq!(mach.manual(0), Ok(1));
    assert_eq!(mach.set_manual(0, 2), Err(abi::errno::Errno::EINVAL));
    assert_eq!(
        mach.set_fan_speed(*b"F0Mn", 16_384),
        Err(abi::errno::Errno::EINVAL),
        "no wrap"
    );
}
