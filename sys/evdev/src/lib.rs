// SPDX-License-Identifier: BSD-2-Clause
//! evdev (USB.md §6.3): input devices as `/dev/input/event*`, FreeBSD's
//! evdev ABI.
//!
//! A driver describes its device with a [`Builder`] (name, identity, the
//! event types and codes it reports, its absolute axes' ranges) and
//! registers it, which makes `/dev/input/eventN` (the lowest free `N`)
//! and `kern.evdev.input.N`. It then reports events with
//! [`Device::push`] and ends each report with [`Device::sync`]; both are
//! callable from completion context.
//!
//! The device keeps the state of its keys, LEDs, switches, and absolute
//! axes, and passes on only changes: a key already down is not pressed
//! again (it repeats, if the device repeats keys), an axis that did not
//! move and a relative motion of 0 are dropped, and a report with
//! nothing in it is not reported. Each open file is a client with its
//! own ring of `8 × report size` events; a client reads only whole
//! reports, stamped when they completed. A client that falls behind
//! loses its queue for a `SYN_DROPPED`. A client may grab the device
//! (`EVIOCGRAB`), and then only it receives events.
//!
//! Behavior reference: sys/dev/evdev/evdev.c, sys/dev/evdev/cdev.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod keys;

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::evdev::*;
use abi::ioctl::{FIOASYNC, FIOGETOWN, FIONBIO, FIONREAD, FIOSETOWN, param_len};
use abi::time::{Timeval, clock};
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};
use sysctl::{NodeId, Number, Tree, Value};
use vfs::{CharDevice, Uio};

/// Who receives events (`kern.evdev.rcpt_mask`): bit 0 `sysmouse`, bit 1
/// `kbdmux`, bit 2 mouse hardware, bit 3 keyboard hardware.
pub const RCPT_SYSMOUSE: i32 = 1 << 0;
pub const RCPT_KBDMUX: i32 = 1 << 1;
pub const RCPT_HW_MOUSE: i32 = 1 << 2;
pub const RCPT_HW_KBD: i32 = 1 << 3;

/// FreeBSD's default: the hardware devices report, the multiplexers do
/// not.
static RCPT_MASK: AtomicI32 = AtomicI32::new(RCPT_HW_MOUSE | RCPT_HW_KBD);

/// Whether the receivers in `bits` get events.
#[must_use]
pub fn receives(bits: i32) -> bool {
    RCPT_MASK.load(Ordering::Relaxed) & bits != 0
}

/// Reports per client ring (`DEF_RING_REPORTS`).
const RING_REPORTS: usize = 8;
/// The longest name, location, and serial (`NAMELEN`, NUL included).
const NAMELEN: usize = 80;

/// The clocks events are stamped with, in nanoseconds.
#[derive(Clone, Copy, Debug)]
pub struct Clocks {
    /// The time of day (`microtime`).
    pub realtime: fn() -> u64,
    /// Time since boot (`microuptime`).
    pub uptime: fn() -> u64,
}

struct Global {
    clocks: Clocks,
    /// `kern.evdev.input`.
    input: Option<NodeId>,
}

static GLOBAL: Once<Global> = Once::new();
/// The units in use.
static UNITS: SpinLock<BTreeSet<u32>> = SpinLock::new(BTreeSet::new());

/// Makes `kern.evdev` and its knobs; call once, before any device
/// registers.
pub fn init(s: &Sleepable, clocks: Clocks) {
    let mut input = None;
    bus::manager().sysctl(s, &mut |t| {
        let root = t.root();
        let Some(kern) = t.child(root, "kern") else {
            return;
        };
        let Ok(evdev) = t.add_node(kern, Number::Auto, "evdev", "Evdev args") else {
            return;
        };
        let _ = t.add_value(
            evdev,
            Number::Auto,
            "rcpt_mask",
            "Who is receiving events: bit0 - sysmouse, bit1 - kbdmux, bit2 - mouse hardware, \
             bit3 - keyboard hardware",
            Value::Int(Box::new(|| RCPT_MASK.load(Ordering::Relaxed))),
        );
        let _ = t.add_value(
            evdev,
            Number::Auto,
            "sysmouse_t_axis",
            "Extract T-axis from 0-none, 1-ums, 2-psm, 3-wsp",
            Value::int(0),
        );
        input = t
            .add_node(evdev, Number::Auto, "input", "Evdev input devices")
            .ok();
    });
    let _ = GLOBAL.set(Global { clocks, input });
}

fn timeval(ns: u64) -> Timeval {
    Timeval {
        tv_sec: i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
        tv_usec: i64::try_from(ns % 1_000_000_000 / 1000).unwrap_or(0),
    }
}

/// A bitmap of `64 × W` codes, laid out as FreeBSD's `bitstr_t` arrays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bits<const W: usize>([u64; W]);

impl<const W: usize> Default for Bits<W> {
    fn default() -> Self {
        Self([0; W])
    }
}

impl<const W: usize> Bits<W> {
    fn test(&self, n: u16) -> bool {
        self.0
            .get(usize::from(n / 64))
            .is_some_and(|w| w & (1 << (n % 64)) != 0)
    }

    fn put(&mut self, n: u16, on: bool) {
        if let Some(w) = self.0.get_mut(usize::from(n / 64)) {
            if on {
                *w |= 1 << (n % 64);
            } else {
                *w &= !(1 << (n % 64));
            }
        }
    }

    /// How many of codes `lo..hi` are set.
    fn count(&self, lo: u16, hi: u16) -> usize {
        (lo..hi).filter(|&n| self.test(n)).count()
    }

    fn any(&self, lo: u16, hi: u16) -> bool {
        (lo..hi).any(|n| self.test(n))
    }

    /// Its bytes (`sizeof` the C bitmap).
    fn bytes(&self) -> Vec<u8> {
        self.0.iter().flat_map(|w| w.to_ne_bytes()).collect()
    }
}

/// Code bitmaps, by type.
#[derive(Clone, Copy, Debug, Default)]
struct Caps {
    types: Bits<1>,
    keys: Bits<12>,
    rel: Bits<1>,
    abs: Bits<1>,
    msc: Bits<1>,
    led: Bits<1>,
    snd: Bits<1>,
    sw: Bits<1>,
    props: Bits<1>,
}

/// What a device's driver does when its clients come and go, or write
/// events to it.
pub trait Methods: Send + Sync {
    /// The first client opened the device (`ev_open`). Must not sleep.
    ///
    /// # Errors
    ///
    /// If the device cannot start; the open fails with it.
    fn open(&self) -> Result<(), Errno> {
        Ok(())
    }
    /// The last client closed it (`ev_close`). Must not sleep.
    fn close(&self) {}
    /// A client wrote an `EV_LED`, `EV_REP`, `EV_MSC`, `EV_SND`, or
    /// `EV_FF` event (`ev_event`). Must not sleep.
    fn event(&self, kind: u16, code: u16, value: i32) {
        let _ = (kind, code, value);
    }
}

/// A device being described.
pub struct Builder {
    name: String,
    phys: String,
    uniq: String,
    id: InputId,
    caps: Caps,
    absinfo: Option<Vec<InputAbsinfo>>,
    methods: Option<Arc<dyn Methods>>,
}

impl core::fmt::Debug for Builder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Builder")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// `s`, cut to fit `NAMELEN` with its NUL.
fn fit(s: &str) -> String {
    let mut end = s.len().min(NAMELEN - 1);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    String::from(&s[..end])
}

impl Builder {
    /// A device named `name` (`evdev_set_name`) that reports `EV_SYN`.
    #[must_use]
    pub fn new(name: &str) -> Self {
        let mut b = Self {
            name: fit(name),
            phys: String::new(),
            uniq: String::new(),
            id: InputId::default(),
            caps: Caps::default(),
            absinfo: None,
            methods: None,
        };
        b.event(EV_SYN);
        b
    }

    /// Its short name, the driver instance (`evdev_set_phys`).
    pub fn phys(&mut self, phys: &str) -> &mut Self {
        self.phys = fit(phys);
        self
    }

    /// Its serial number (`evdev_set_serial`).
    pub fn uniq(&mut self, uniq: &str) -> &mut Self {
        self.uniq = fit(uniq);
        self
    }

    /// Its identity (`evdev_set_id`).
    pub fn id(&mut self, id: InputId) -> &mut Self {
        self.id = id;
        self
    }

    /// Its driver's methods (`evdev_set_methods`).
    pub fn methods(&mut self, m: Arc<dyn Methods>) -> &mut Self {
        self.methods = Some(m);
        self
    }

    /// It reports events of type `kind` (`evdev_support_event`).
    pub fn event(&mut self, kind: u16) -> &mut Self {
        self.caps.types.put(kind, true);
        self
    }

    /// It reports key or button `code` (`evdev_support_key`).
    pub fn key(&mut self, code: u16) -> &mut Self {
        self.caps.keys.put(code, true);
        self
    }

    /// It reports relative axis `code` (`evdev_support_rel`).
    pub fn rel(&mut self, code: u16) -> &mut Self {
        self.caps.rel.put(code, true);
        self
    }

    /// It reports absolute axis `code` over `min..=max`
    /// (`evdev_support_abs`).
    pub fn abs(
        &mut self,
        code: u16,
        min: i32,
        max: i32,
        fuzz: i32,
        flat: i32,
        resolution: i32,
    ) -> &mut Self {
        if code >= ABS_CNT {
            return self;
        }
        let info = self
            .absinfo
            .get_or_insert_with(|| vec![InputAbsinfo::default(); usize::from(ABS_CNT)]);
        info[usize::from(code)] = InputAbsinfo {
            value: 0,
            minimum: min,
            maximum: max,
            fuzz,
            flat,
            resolution,
        };
        self.caps.abs.put(code, true);
        self
    }

    /// It reports miscellaneous event `code` (`evdev_support_msc`).
    pub fn msc(&mut self, code: u16) -> &mut Self {
        self.caps.msc.put(code, true);
        self
    }

    /// It has sound `code` (`evdev_support_snd`).
    pub fn snd(&mut self, code: u16) -> &mut Self {
        self.caps.snd.put(code, true);
        self
    }

    /// It has switch `code` (`evdev_support_sw`).
    pub fn sw(&mut self, code: u16) -> &mut Self {
        self.caps.sw.put(code, true);
        self
    }

    /// It has LED `code` (`evdev_support_led`).
    pub fn led(&mut self, code: u16) -> &mut Self {
        self.caps.led.put(code, true);
        self
    }

    /// It has property `prop` (`evdev_support_prop`).
    pub fn prop(&mut self, prop: u16) -> &mut Self {
        self.caps.props.put(prop, true);
        self
    }

    /// The most events one report can hold, and so a client ring's size
    /// in reports (`evdev_estimate_report_size`).
    fn report_size(&self) -> usize {
        let c = &self.caps;
        // Keyboards report one key at a time; buttons can go together.
        let keys = usize::from(c.keys.any(KEY_OK, KEY_CNT) || c.keys.any(0, BTN_MISC))
            + c.keys.count(BTN_MISC, KEY_OK);
        let abs = if self.absinfo.is_some() {
            c.abs.count(0, ABS_CNT)
        } else {
            0
        };
        keys + c.rel.count(0, REL_CNT)
            + abs
            + c.msc.count(0, MSC_CNT)
            + c.led.count(0, LED_CNT)
            + usize::from(c.snd.any(0, SND_CNT))
            + usize::from(c.sw.any(0, SW_CNT))
            // SYN_REPORT.
            + 1
    }

    /// Registers the device (`evdev_register`): `/dev/input/eventN` and
    /// `kern.evdev.input.N`.
    ///
    /// # Errors
    ///
    /// `ENXIO` if there is no devfs or no free unit.
    pub fn register(&mut self, s: &Sleepable) -> Result<Arc<Device>, Errno> {
        let report_size = self.report_size();
        let dev = Arc::new_cyclic(|_| Device {
            unit: SpinLock::new(None),
            name: core::mem::take(&mut self.name),
            phys: core::mem::take(&mut self.phys),
            uniq: core::mem::take(&mut self.uniq),
            id: self.id,
            caps: self.caps,
            report_size,
            methods: self.methods.take(),
            state: SpinLock::new(State {
                absinfo: self.absinfo.take(),
                ..State::default()
            }),
            clients: SpinLock::new(Vec::new()),
            sysctl: SpinLock::new(None),
        });
        // The lowest free unit (`evdev_cdev_create`).
        let unit = {
            let mut u = UNITS.lock();
            let n = (0..).find(|n| !u.contains(n)).ok_or(Errno::ENXIO)?;
            u.insert(n);
            n
        };
        let path = format!("input/event{unit}");
        let node = Arc::new(Node(Arc::clone(&dev)));
        if vfs::devices::make_dev(path.as_bytes(), 0o600, 0, 0, node).is_none() {
            UNITS.lock().remove(&unit);
            return Err(Errno::ENXIO);
        }
        *dev.unit.lock() = Some(unit);
        dev.add_sysctl(s, unit);
        Ok(dev)
    }
}

#[derive(Default)]
struct State {
    keys: Bits<12>,
    leds: Bits<1>,
    snd: Bits<1>,
    sw: Bits<1>,
    absinfo: Option<Vec<InputAbsinfo>>,
    rep: [i32; 2],
    /// An event since the last `SYN_REPORT`.
    report_open: bool,
    /// The client that grabbed the device.
    grabber: Option<usize>,
}

/// A registered device.
pub struct Device {
    unit: SpinLock<Option<u32>>,
    name: String,
    phys: String,
    uniq: String,
    id: InputId,
    caps: Caps,
    report_size: usize,
    methods: Option<Arc<dyn Methods>>,
    state: SpinLock<State>,
    clients: SpinLock<Vec<Arc<Client>>>,
    sysctl: SpinLock<Option<NodeId>>,
}

impl core::fmt::Debug for Device {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .field("phys", &self.phys)
            .finish_non_exhaustive()
    }
}

/// What a sparse event does (`evdev_sparse_result`).
enum Sparse {
    Skip,
    Report,
}

impl Device {
    /// Its unit (`N` of `eventN`), while registered.
    #[must_use]
    pub fn unit(&self) -> Option<u32> {
        *self.unit.lock()
    }

    /// Whether a client has grabbed it (`evdev_is_grabbed`): then the
    /// driver's events go to that client only, not to the console.
    #[must_use]
    pub fn is_grabbed(&self) -> bool {
        self.state.lock().grabber.is_some()
    }

    fn supports(&self, kind: u16) -> bool {
        self.caps.types.test(kind)
    }

    /// `evdev_check_event`.
    fn check(&self, kind: u16, code: u16) -> Result<(), Errno> {
        if kind >= EV_CNT || (kind != EV_SYN && !self.supports(kind)) {
            return Err(Errno::EINVAL);
        }
        let ok = match kind {
            EV_SYN => code < SYN_CNT,
            EV_KEY => self.caps.keys.test(code),
            EV_REL => self.caps.rel.test(code),
            EV_ABS => self.caps.abs.test(code),
            EV_MSC => self.caps.msc.test(code),
            EV_LED => self.caps.led.test(code),
            EV_SND => self.caps.snd.test(code),
            EV_SW => self.caps.sw.test(code),
            EV_REP => code < REP_CNT,
            _ => false,
        };
        if ok { Ok(()) } else { Err(Errno::EINVAL) }
    }

    /// Reports an event (`evdev_push_event`). Callable from completion
    /// context.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a type or code the device does not report.
    pub fn push(&self, kind: u16, code: u16, value: i32) -> Result<(), Errno> {
        self.check(kind, code)?;
        let mut st = self.state.lock();
        let value = self.modify(&st, kind, code, value);
        if let Sparse::Report = Self::sparse(&mut st, &self.caps, kind, code, value) {
            self.propagate(&st, kind, code, value);
        }
        Ok(())
    }

    /// Ends a report (`evdev_sync`).
    pub fn sync(&self) {
        let _ = self.push(EV_SYN, SYN_REPORT, 1);
    }

    /// Reports key `code` down or up (`evdev_push_key`).
    pub fn push_key(&self, code: u16, down: bool) {
        let _ = self.push(EV_KEY, code, i32::from(down));
    }

    /// `evdev_modify_event`: a key pressed again repeats; absolute
    /// values within the axis's fuzz are smoothed.
    fn modify(&self, st: &State, kind: u16, code: u16, value: i32) -> i32 {
        match kind {
            EV_KEY if self.supports(EV_REP) && st.keys.test(code) && value == KEY_EVENT_DOWN => {
                KEY_EVENT_REPEAT
            }
            EV_ABS => {
                let Some(info) = st.absinfo.as_ref().and_then(|a| a.get(usize::from(code))) else {
                    return value;
                };
                let (old, fuzz) = (info.value, info.fuzz);
                if fuzz == 0 {
                    return value;
                }
                let change = value.saturating_sub(old).saturating_abs();
                if change < fuzz / 2 {
                    old
                } else if change < fuzz {
                    (old.saturating_mul(3).saturating_add(value)) / 4
                } else if change < fuzz.saturating_mul(2) {
                    (old.saturating_add(value)) / 2
                } else {
                    value
                }
            }
            _ => value,
        }
    }

    /// `evdev_sparse_event`: updates the device's state and says whether
    /// the event changes anything.
    fn sparse(st: &mut State, caps: &Caps, kind: u16, code: u16, value: i32) -> Sparse {
        let skip = match kind {
            EV_KEY => match value {
                KEY_EVENT_UP | KEY_EVENT_DOWN => {
                    let down = value == KEY_EVENT_DOWN;
                    if st.keys.test(code) == down {
                        true
                    } else {
                        st.keys.put(code, down);
                        false
                    }
                }
                KEY_EVENT_REPEAT => !st.keys.test(code) || !caps.types.test(EV_REP),
                _ => true,
            },
            EV_LED => {
                let on = value != 0;
                let same = st.leds.test(code) == on;
                st.leds.put(code, on);
                same
            }
            EV_SND => {
                st.snd.put(code, value != 0);
                false
            }
            EV_SW => {
                let on = value != 0;
                let same = st.sw.test(code) == on;
                st.sw.put(code, on);
                same
            }
            EV_REP => match st.rep.get_mut(usize::from(code)) {
                Some(r) if *r != value => {
                    *r = value;
                    false
                }
                _ => true,
            },
            EV_REL => value == 0,
            EV_ABS => match st
                .absinfo
                .as_mut()
                .and_then(|a| a.get_mut(usize::from(code)))
            {
                Some(info) if info.value != value => {
                    info.value = value;
                    false
                }
                _ => true,
            },
            EV_SYN if code == SYN_REPORT => {
                // Empty reports are not reported.
                let open = st.report_open;
                st.report_open = false;
                return if open { Sparse::Report } else { Sparse::Skip };
            }
            _ => false,
        };
        if skip {
            return Sparse::Skip;
        }
        st.report_open = true;
        Sparse::Report
    }

    /// `evdev_propagate_event`: to every client, or only the grabber.
    fn propagate(&self, st: &State, kind: u16, code: u16, value: i32) {
        let clients = self.clients.lock();
        let report = kind == EV_SYN && code == SYN_REPORT;
        for c in clients.iter() {
            if st.grabber.is_some_and(|g| g != c.id) {
                continue;
            }
            c.push(kind, code, value);
            if report {
                c.wait.wake_all();
            }
        }
    }

    /// `evdev_inject_event`: an event a client wrote.
    fn inject(&self, kind: u16, code: u16, value: i32) -> Result<(), Errno> {
        match kind {
            EV_REP | EV_LED | EV_MSC | EV_SND | EV_FF => {
                if let Some(m) = &self.methods {
                    m.event(kind, code, value);
                }
                // The driver reports LEDs and repeats itself.
                if kind == EV_LED || kind == EV_REP {
                    return Ok(());
                }
                self.push(kind, code, value)
            }
            EV_SYN | EV_KEY | EV_REL | EV_ABS | EV_SW => self.push(kind, code, value),
            _ => Err(Errno::EINVAL),
        }
    }

    fn add_sysctl(self: &Arc<Self>, s: &Sleepable, unit: u32) {
        let Some(input) = GLOBAL.get().and_then(|g| g.input) else {
            return;
        };
        let mut id = None;
        bus::manager().sysctl(s, &mut |t: &mut Tree| {
            let Ok(n) = t.add_node(input, Number::Auto, &format!("{unit}"), "device_index") else {
                return;
            };
            id = Some(n);
            let text = |t: &mut Tree, name: &str, desc: &str, v: &String| {
                let v = v.clone();
                let _ = t.add_value(
                    n,
                    Number::Auto,
                    name,
                    desc,
                    Value::Str(Box::new(move |out| out.push_str(&v))),
                );
            };
            text(t, "name", "Input device name", &self.name);
            let ident = self.id.to_bytes();
            let _ = t.add_value(
                n,
                Number::Auto,
                "id",
                "Input device identification",
                Value::Opaque {
                    format: "S,input_id",
                    read: Box::new(move |out| out.extend_from_slice(&ident)),
                },
            );
            text(t, "phys", "Input device short name", &self.phys);
            text(t, "uniq", "Input device unique number", &self.uniq);
            let c = self.caps;
            let bits: [(&str, &str, Vec<u8>); 9] = [
                ("props", "Input device properties", c.props.bytes()),
                (
                    "type_bits",
                    "Input device supported events types",
                    c.types.bytes(),
                ),
                ("key_bits", "Input device supported keys", c.keys.bytes()),
                (
                    "rel_bits",
                    "Input device supported relative events",
                    c.rel.bytes(),
                ),
                (
                    "abs_bits",
                    "Input device supported absolute events",
                    c.abs.bytes(),
                ),
                (
                    "msc_bits",
                    "Input device supported miscellaneous events",
                    c.msc.bytes(),
                ),
                (
                    "led_bits",
                    "Input device supported LED events",
                    c.led.bytes(),
                ),
                (
                    "snd_bits",
                    "Input device supported sound events",
                    c.snd.bytes(),
                ),
                (
                    "sw_bits",
                    "Input device supported switch events",
                    c.sw.bytes(),
                ),
            ];
            for (name, desc, b) in bits {
                let _ = t.add_value(
                    n,
                    Number::Auto,
                    name,
                    desc,
                    Value::Opaque {
                        format: "",
                        read: Box::new(move |out| out.extend_from_slice(&b)),
                    },
                );
            }
        });
        *self.sysctl.lock() = id;
    }

    /// Unregisters the device (`evdev_unregister`): its node and sysctls
    /// go, and its clients are revoked and woken.
    pub fn unregister(&self, s: &Sleepable) {
        if let Some(n) = self.sysctl.lock().take() {
            bus::manager().sysctl(s, &mut |t| {
                t.remove(n);
            });
        }
        let clients: Vec<Arc<Client>> = core::mem::take(&mut *self.clients.lock());
        for c in &clients {
            c.revoked.store(true, Ordering::Release);
            c.wait.wake_all();
        }
        if !clients.is_empty()
            && let Some(m) = &self.methods
        {
            m.close();
        }
        self.state.lock().grabber = None;
        if let Some(unit) = self.unit.lock().take() {
            vfs::devices::destroy_dev(s, format!("input/event{unit}").as_bytes());
            UNITS.lock().remove(&unit);
        }
    }

    /// A new client (`evdev_register_client`): the first one opens the
    /// device.
    fn open(&self) -> Result<Arc<Client>, Errno> {
        if self.unit().is_none() {
            return Err(Errno::ENODEV);
        }
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let size = self.report_size * RING_REPORTS;
        let c = Arc::new(Client {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            ring: SpinLock::new(Ring {
                buf: vec![InputEvent::default(); size.max(2)],
                head: 0,
                tail: 0,
                ready: 0,
            }),
            wait: WaitQueue::new(),
            revoked: AtomicBool::new(false),
            monotonic: AtomicBool::new(false),
        });
        let mut clients = self.clients.lock();
        if clients.is_empty()
            && let Some(m) = &self.methods
        {
            m.open()?;
        }
        clients.push(Arc::clone(&c));
        Ok(c)
    }

    /// A client goes (`evdev_dispose_client`): the last one closes the
    /// device; a grab it held ends.
    fn dispose(&self, c: &Client) {
        let mut clients = self.clients.lock();
        let before = clients.len();
        clients.retain(|x| x.id != c.id);
        if before != clients.len()
            && clients.is_empty()
            && let Some(m) = &self.methods
        {
            m.close();
        }
        drop(clients);
        let mut st = self.state.lock();
        if st.grabber == Some(c.id) {
            st.grabber = None;
        }
    }
}

/// A client's ring of events.
struct Ring {
    buf: Vec<InputEvent>,
    head: usize,
    tail: usize,
    /// The end of the last whole report: readers stop here.
    ready: usize,
}

impl Ring {
    fn len(&self) -> usize {
        (self.ready + self.buf.len() - self.head) % self.buf.len()
    }

    fn is_empty(&self) -> bool {
        self.head == self.ready
    }
}

/// An open `/dev/input/eventN`.
struct Client {
    id: usize,
    ring: SpinLock<Ring>,
    wait: WaitQueue,
    revoked: AtomicBool,
    /// Stamps from the uptime clock (`EVIOCSCLOCKID`), else the time of
    /// day.
    monotonic: AtomicBool,
}

impl Client {
    /// `evdev_client_push`: a full ring is dropped for `SYN_DROPPED`.
    fn push(&self, kind: u16, code: u16, value: i32) {
        let mut r = self.ring.lock();
        let n = r.buf.len();
        if (r.tail + 1) % n == r.head {
            let head = (r.tail + n - 1) % n;
            r.buf[head] = InputEvent {
                kind: EV_SYN,
                code: SYN_DROPPED,
                ..InputEvent::default()
            };
            r.head = head;
            r.ready = head;
        }
        let tail = r.tail;
        r.buf[tail] = InputEvent {
            time: Timeval::default(),
            kind,
            code,
            value,
        };
        r.tail = (tail + 1) % n;
        // Readers see a report only once it is whole.
        if kind == EV_SYN && code == SYN_REPORT {
            let time = timeval(GLOBAL.get().map_or(0, |g| {
                if self.monotonic.load(Ordering::Relaxed) {
                    (g.clocks.uptime)()
                } else {
                    (g.clocks.realtime)()
                }
            }));
            let mut i = r.ready;
            while i != r.tail {
                r.buf[i].time = time;
                i = (i + 1) % n;
            }
            r.ready = r.tail;
        }
    }

    /// `evdev_client_filter_queue`: drops the queued events of `kind`
    /// (state ioctls report them instead), and reports left empty.
    fn filter(&self, kind: u16) {
        let mut r = self.ring.lock();
        let n = r.buf.len();
        let (mut i, mut tail) = (r.head, r.tail);
        let end = r.tail;
        r.ready = r.tail;
        let mut last_was_syn = false;
        while i != end {
            let e = r.buf[i];
            i = (i + 1) % n;
            if e.kind == kind {
                continue;
            }
            let syn = e.kind == EV_SYN && e.code == SYN_REPORT;
            if syn {
                if last_was_syn {
                    continue;
                }
                r.ready = (tail + 1) % n;
            }
            r.buf[tail] = e;
            last_was_syn = syn;
            tail = (tail + 1) % n;
        }
        r.head = i;
        r.tail = tail;
    }
}

/// The device node: each open is a client.
struct Node(Arc<Device>);

impl CharDevice for Node {
    fn instance(&self, _: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        let client = self.0.open()?;
        Ok(Some(Arc::new(Open {
            dev: Arc::clone(&self.0),
            client,
        })))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }
}

/// One open file of a device: its client.
struct Open {
    dev: Arc<Device>,
    client: Arc<Client>,
}

impl Drop for Open {
    fn drop(&mut self) {
        if !self.client.revoked.load(Ordering::Acquire) {
            self.dev.dispose(&self.client);
        }
    }
}

const EVENT_SIZE: usize = size_of::<InputEvent>();

/// Copies up to `len` bytes of `src` to the front of `arg`; returns how
/// many.
fn put(arg: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(arg.len());
    arg[..n].copy_from_slice(&src[..n]);
    n
}

fn int(arg: &[u8]) -> i32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i32::from_ne_bytes)
}

/// A string with its NUL, as `EVIOCGNAME` copies it (truncated without
/// one).
fn put_str(arg: &mut [u8], s: &str) -> usize {
    let mut b = Vec::from(s.as_bytes());
    b.push(0);
    put(arg, &b)
}

impl CharDevice for Open {
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let c = &self.client;
        if c.revoked.load(Ordering::Acquire) {
            return Err(Errno::ENODEV);
        }
        // Zero-length reads check for errors.
        if uio.resid() != 0 && uio.resid() < EVENT_SIZE {
            return Err(Errno::EINVAL);
        }
        let mut remaining = uio.resid() / EVENT_SIZE;
        if c.ring.lock().is_empty() {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            if remaining != 0 {
                c.wait
                    .wait_until_interruptible(s, || {
                        !c.ring.lock().is_empty() || c.revoked.load(Ordering::Acquire)
                    })
                    .map_err(|_| Errno::EINTR)?;
                if c.revoked.load(Ordering::Acquire) {
                    return Err(Errno::ENODEV);
                }
            }
        }
        let mut done = 0;
        while remaining > 0 {
            let e = {
                let mut r = c.ring.lock();
                if r.is_empty() {
                    break;
                }
                let e = r.buf[r.head];
                r.head = (r.head + 1) % r.buf.len();
                e
            };
            done += uio.copy_out(&e.to_bytes())?;
            remaining -= 1;
        }
        Ok(done)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        if self.client.revoked.load(Ordering::Acquire) {
            return Err(Errno::ENODEV);
        }
        if !uio.resid().is_multiple_of(EVENT_SIZE) {
            return Err(Errno::EINVAL);
        }
        let mut done = 0;
        while uio.resid() > 0 {
            let mut b = [0u8; EVENT_SIZE];
            let n = uio.copy_in(&mut b)?;
            if n < EVENT_SIZE {
                break;
            }
            let (kind, code, value) = InputEvent::parse(&b);
            self.dev.inject(kind, code, value)?;
            done += n;
        }
        Ok(done)
    }

    fn ioctl_value(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
        let (dev, c) = (&self.dev, &self.client);
        if c.revoked.load(Ordering::Acquire) {
            return Err(Errno::ENODEV);
        }
        match req {
            FIONBIO | FIOASYNC | FIOSETOWN => return Ok(0),
            FIOGETOWN => {
                put(arg, &0i32.to_ne_bytes());
                return Ok(0);
            }
            FIONREAD => {
                let n = c.ring.lock().len() * EVENT_SIZE;
                put(arg, &i32::try_from(n).unwrap_or(i32::MAX).to_ne_bytes());
                return Ok(0);
            }
            EVIOCGVERSION => {
                put(arg, &EV_VERSION.to_ne_bytes());
                return Ok(0);
            }
            EVIOCGID => {
                put(arg, &dev.id.to_bytes());
                return Ok(0);
            }
            EVIOCGREP | EVIOCSREP if !dev.supports(EV_REP) => return Err(Errno::EOPNOTSUPP),
            EVIOCGREP => {
                let rep = dev.state.lock().rep;
                put(arg, &rep[0].to_ne_bytes());
                put(&mut arg[4..], &rep[1].to_ne_bytes());
                return Ok(0);
            }
            EVIOCSREP => {
                let (delay, period) = (int(arg), int(&arg[4..]));
                dev.inject(EV_REP, REP_DELAY, delay)?;
                dev.inject(EV_REP, REP_PERIOD, period)?;
                return Ok(0);
            }
            // Accepted and ignored, as FreeBSD fakes them.
            EVIOCGKEYCODE | EVIOCSKEYCODE | EVIOCSFF | EVIOCRMFF | EVIOCGEFFECTS => return Ok(0),
            EVIOCGKEYCODE_V2 | EVIOCSKEYCODE_V2 => return Err(Errno::EOPNOTSUPP),
            EVIOCGRAB => {
                let mut st = dev.state.lock();
                return if int(arg) != 0 {
                    if st.grabber.is_some() {
                        return Err(Errno::EBUSY);
                    }
                    st.grabber = Some(c.id);
                    Ok(0)
                } else if st.grabber == Some(c.id) {
                    st.grabber = None;
                    Ok(0)
                } else {
                    Err(Errno::EINVAL)
                };
            }
            EVIOCREVOKE => {
                if int(arg) != 0 {
                    return Err(Errno::EINVAL);
                }
                dev.dispose(c);
                c.revoked.store(true, Ordering::Release);
                c.wait.wake_all();
                return Ok(0);
            }
            EVIOCSCLOCKID => {
                let mono = match u32::try_from(int(arg)) {
                    Ok(clock::REALTIME) => false,
                    Ok(clock::MONOTONIC) => true,
                    _ => return Err(Errno::EINVAL),
                };
                c.monotonic.store(mono, Ordering::Relaxed);
                return Ok(0);
            }
            _ => {}
        }
        let num = req & 0xff;
        if req & 0xff00 != u32::from(b'E') << 8 {
            return Err(Errno::ENOTTY);
        }
        if (eviocgabs(0)..eviocgabs(u32::from(ABS_CNT))).contains(&req) {
            let st = dev.state.lock();
            let info = st.absinfo.as_ref().ok_or(Errno::EINVAL)?;
            put(
                arg,
                &info[usize::try_from(num - 0x40).unwrap_or(0)].to_bytes(),
            );
            return Ok(0);
        }
        if (eviocsabs(0)..eviocsabs(u32::from(ABS_CNT))).contains(&req) {
            let mut st = dev.state.lock();
            let info = st.absinfo.as_mut().ok_or(Errno::EINVAL)?;
            let new = InputAbsinfo::from_bytes(arg).ok_or(Errno::EINVAL)?;
            // `evdev_set_absinfo`.
            info[usize::try_from(num - 0xc0).unwrap_or(0)] = new;
            return Ok(0);
        }
        let len = usize::try_from(param_len(req)).unwrap_or(0);
        let bytes = |n: u16| usize::from(n).div_ceil(64) * 8;
        if req != eviocvar(num, param_len(req)) {
            return Err(Errno::EINVAL);
        }
        let end = len.min(arg.len());
        let arg = &mut arg[..end];
        match num {
            var::EVIOCGNAME => Ok(put_str(arg, &dev.name)),
            var::EVIOCGPHYS if dev.phys.is_empty() => Err(Errno::ENOENT),
            var::EVIOCGPHYS => Ok(put_str(arg, &dev.phys)),
            var::EVIOCGUNIQ if dev.uniq.is_empty() => Err(Errno::ENOENT),
            var::EVIOCGUNIQ => Ok(put_str(arg, &dev.uniq)),
            var::EVIOCGPROP => Ok(put(arg, &dev.caps.props.bytes())),
            var::EVIOCGMTSLOTS => Err(Errno::EINVAL),
            var::EVIOCGKEY => {
                c.filter(EV_KEY);
                Ok(put(arg, &dev.state.lock().keys.bytes()))
            }
            var::EVIOCGLED => {
                c.filter(EV_LED);
                Ok(put(arg, &dev.state.lock().leds.bytes()))
            }
            var::EVIOCGSND => {
                c.filter(EV_SND);
                Ok(put(arg, &dev.state.lock().snd.bytes()))
            }
            var::EVIOCGSW => {
                c.filter(EV_SW);
                Ok(put(arg, &dev.state.lock().sw.bytes()))
            }
            n if (var::EVIOCGBIT..var::EVIOCGBIT + u32::from(EV_CNT)).contains(&n) => {
                let caps = &dev.caps;
                let (bits, count) = match u16::try_from(n - var::EVIOCGBIT).unwrap_or(u16::MAX) {
                    0 => (caps.types.bytes(), EV_CNT),
                    EV_KEY => (caps.keys.bytes(), KEY_CNT),
                    EV_REL => (caps.rel.bytes(), REL_CNT),
                    EV_ABS => (caps.abs.bytes(), ABS_CNT),
                    EV_MSC => (caps.msc.bytes(), MSC_CNT),
                    EV_LED => (caps.led.bytes(), LED_CNT),
                    EV_SND => (caps.snd.bytes(), SND_CNT),
                    EV_SW => (caps.sw.bytes(), SW_CNT),
                    // Force feedback is not supported: all zeros.
                    EV_FF => {
                        arg.fill(0);
                        return Ok(arg.len());
                    }
                    _ => return Err(Errno::ENOTTY),
                };
                arg.fill(0);
                let n = bytes(count).min(arg.len());
                Ok(put(&mut arg[..n], &bits))
            }
            _ => Err(Errno::EINVAL),
        }
    }
}
