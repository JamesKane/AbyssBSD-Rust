// SPDX-License-Identifier: BSD-2-Clause
//! The input class (USB.md §6.3): keyboards, as FreeBSD's kbd layer
//! presents them, and the system mouse ([`sysmouse`]).
//!
//! A keyboard driver (`hkbd`, `vtinput`) registers a [`Keyboard`] and
//! reports key presses and releases by HID usage; it may sleep for
//! nothing, since reports arrive in completion context. The class queues
//! them, and its thread does the rest, as `hkbd` emulates an AT
//! keyboard: it repeats the last key pressed (the keyboard's
//! `kb_delay1`, then every `kb_delay2`), and reads each key in the
//! keyboard's mode ([`scan`]): AT set 1 scancodes (`K_RAW`), keycodes
//! (`K_CODE`), or key actions through the keyboard's keymap (`K_XLATE`,
//! [`xlate`], [`keymap`]).
//!
//! Every keyboard is `kbdN`, with a device node by its driver's name
//! (`/dev/hkbd0`) and the link `/dev/kbdN`. A keyboard belongs to one
//! owner at a time (`kbd_allocate`): `kbdmux0`, which takes each keyboard
//! as it registers, in `K_RAW`, and reads its scancodes as its own; the
//! console, which owns `kbdmux0` as `vt` does and makes its actions
//! terminal input ([`term`]) for the [`Consumer`]; or an open of the
//! device node, which reads what the keyboard reads. The node of a
//! keyboard someone else owns does not open (`EBUSY`), which is every
//! one of them on a running system. The `KD*` and keymap requests work
//! on an open node; `kbdmux0` passes the lock, LED, repeat, and keymap
//! ones on to its keyboards. Lock keys set the LEDs of every keyboard
//! through `kbdmux0`.
//!
//! A keyboard with an evdev device reports there too, as it goes: keys
//! as they are pressed and released (and repeated, which evdev reports
//! as such), the end of each report, and LED and repeat changes. While a
//! client has grabbed the evdev device, the keyboard's keys go only
//! there. `kbdmux0` has its evdev device as well, silent under
//! FreeBSD's default `kern.evdev.rcpt_mask` (and its keys not reported
//! there yet under any).
//!
//! `PIO_KEYMAP` and its kin load tables without FreeBSD's check that
//! only a privileged process changes the reboot and debugger keys (the
//! nodes are root's alone; `vt`'s requests will need it).
//!
//! Behavior reference: sys/dev/kbd/kbd.c, sys/dev/kbdmux/kbdmux.c,
//! sys/dev/hid/hkbd.c, sys/dev/vt/vt_core.c (`vt_allocate_keyboard`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod keymap;
pub mod scan;
pub mod sysmouse;
pub mod term;
pub mod xlate;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use abi::errno::Errno;
use abi::evdev::{
    BUS_VIRTUAL, EV_KEY, EV_LED, EV_REP, InputId, LED_CAPSL, LED_NUML, LED_SCROLLL, REP_DELAY,
    REP_PERIOD,
};
use abi::kbio::*;
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vfs::{CharDevice, Uio};

use crate::keymap::Maps;
use crate::scan::{Decoder, Held, NN};
use crate::xlate::{RELEASE, Xlate};

/// Where the console's typing goes.
pub trait Consumer: Send + Sync {
    /// Delivers bytes typed.
    fn input(&self, s: &Sleepable, bytes: &[u8]);
    /// A key the console acts on: a screen switch, Ctrl-Alt-Delete.
    fn event(&self, s: &Sleepable, e: term::Event) {
        let _ = (s, e);
    }
    /// The values read from `kbdmux0` in `mode`, for a consumer that
    /// acts on them itself, as `vt_processkey` does; `false` leaves them
    /// to [`term`], which makes [`Consumer::input`] and
    /// [`Consumer::event`] of them.
    fn keys(&self, s: &Sleepable, mode: i32, codes: &[u32]) -> bool {
        let _ = (s, mode, codes);
        false
    }
}

/// A request to the console's keyboard, `kbdmux0` unless
/// [`set_console`] chose another (`kbdd_ioctl` on `vd_keyboard`); `None`
/// if the console has no keyboard or no keyboard layer knows the
/// request (`ENOIOCTL`).
pub fn console_ioctl(req: u32, arg: &mut [u8]) -> Option<Result<(), Errno>> {
    console_keyboard()?.request(req, arg)
}

fn console_keyboard() -> Option<Arc<Keyboard>> {
    CLASS.keyboard.lock().clone()
}

/// The console's keyboard's index, if it has one.
#[must_use]
pub fn console_index() -> Option<u32> {
    CLASS.keyboard.lock().as_ref().map(|k| k.index)
}

/// Makes keyboard `index` the console's (`CONS_SETKBD`): it is taken,
/// `save` runs while the old one is still the console's, and the old
/// one is let go.
///
/// # Errors
///
/// `EINVAL` if there is no keyboard `index`; `EPERM` if it is busy.
pub fn set_console(index: i32, save: &mut dyn FnMut()) -> Result<(), Errno> {
    // `kbd_get_keyboard`.
    let k = usize::try_from(index)
        .ok()
        .and_then(|i| CLASS.kbds.lock().get(i).cloned().flatten())
        .filter(|k| !k.gone.load(Ordering::Acquire))
        .ok_or(Errno::EINVAL)?;
    if !k.allocate(Owner::Console) {
        return Err(Errno::EPERM);
    }
    if let Some(old) = console_keyboard() {
        save();
        let _ = old.release(&Owner::Console);
    }
    *CLASS.keyboard.lock() = Some(k);
    Ok(())
}

/// Lets the console's keyboard go (`CONS_RELKBD`), after `save`.
///
/// # Errors
///
/// As `kbd_release` returns them.
pub fn release_console(save: &mut dyn FnMut()) -> Result<(), Errno> {
    let Some(k) = console_keyboard() else {
        return Ok(());
    };
    save();
    k.release(&Owner::Console)?;
    let mut kbd = CLASS.keyboard.lock();
    if kbd.as_ref().is_some_and(|c| Arc::ptr_eq(c, &k)) {
        *kbd = None;
    }
    Ok(())
}

/// The lock LEDs to show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Leds {
    /// Num Lock.
    pub num: bool,
    /// Caps Lock.
    pub caps: bool,
    /// Scroll Lock.
    pub scroll: bool,
}

impl Leds {
    fn of(bits: i32) -> Self {
        Self {
            num: bits & LED_NUM != 0,
            caps: bits & LED_CAP != 0,
            scroll: bits & LED_SCR != 0,
        }
    }
}

/// `kb_flags` (sys/dev/kbd/kbdreg.h): a registered keyboard's.
const KB_VALID: i32 = 1 << 16;
const KB_PROBED: i32 = 1 << 18;
const KB_INITIALIZED: i32 = 1 << 19;
const KB_REGISTERED: i32 = 1 << 20;
const KB_BUSY: i32 = 1 << 21;

/// `KBD_DELAY1`, `KBD_DELAY2`: the repeat delay and period until
/// `kbdcontrol -r`.
const DELAY: [i32; 2] = [500, 100];
/// The thread's polling period while a key repeats.
const TICK_MS: u32 = 10;
/// `KB_QSIZE`: a device node's queue.
const QSIZE: usize = 512;

/// A registered keyboard.
pub struct Keyboard {
    /// Its `kbdN` index.
    pub index: u32,
    driver: String,
    unit: i32,
    kind: Kind,
    inner: SpinLock<Inner>,
    gone: AtomicBool,
}

enum Kind {
    /// A keyboard driver's, which shows LEDs and has its evdev device.
    Driver {
        leds: Box<dyn Fn(Leds) + Send + Sync>,
        evdev: Option<Arc<evdev::Device>>,
    },
    /// `kbdmux0`.
    Mux { evdev: Option<Arc<evdev::Device>> },
}

/// Who reads a keyboard (`kb_token` and `kb_callback`).
#[derive(Clone)]
enum Owner {
    Free,
    Mux,
    Console,
    Node(Arc<Reader>),
}

impl Owner {
    fn is(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Free, Self::Free) | (Self::Mux, Self::Mux) | (Self::Console, Self::Console) => {
                true
            }
            (Self::Node(a), Self::Node(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// A held key's repeat: the usage, and how long until it next repeats.
#[derive(Clone, Copy, Debug)]
struct Repeat {
    usage: u16,
    left_ms: u32,
}

struct Inner {
    mode: i32,
    xl: Xlate,
    /// `kb_led`.
    led: i32,
    /// `kb_delay1`, `kb_delay2`.
    delay: [i32; 2],
    maps: Maps,
    owner: Owner,
    /// A driver's keyboard: the keys down, and the repeating one.
    held: Held,
    repeat: Option<Repeat>,
    /// `kbdmux0`: its scancode prefix, and its keyboards, newest first.
    decoder: Decoder,
    slaves: Vec<Arc<Keyboard>>,
}

impl Inner {
    fn new(mode: i32) -> Self {
        Self {
            mode,
            xl: Xlate::default(),
            led: 0,
            delay: DELAY,
            maps: Maps::default_maps(),
            owner: Owner::Free,
            held: Held::default(),
            repeat: None,
            decoder: Decoder::default(),
            slaves: Vec::new(),
        }
    }

    /// `clear_state`.
    fn clear(&mut self) {
        self.xl.clear();
        self.held.clear();
        self.repeat = None;
    }

    /// `hkbd_read_char`: what a usage pressed or released reads as.
    fn read_usage(&mut self, usage: u16, down: bool, r: &mut Read) {
        let kc = scan::keycode(usage, &self.held);
        if kc == NN {
            return;
        }
        if self.mode == K_RAW {
            r.out.extend(scan::scancodes(kc, &self.held, !down));
            return;
        }
        if kc == 0x38 {
            self.xl.compose_key(down);
        }
        self.read_code(u32::from(kc) | if down { 0 } else { RELEASE }, r);
    }

    /// `kbdmux_read_char`: what one of its keyboards' scancodes reads
    /// as.
    fn read_scancode(&mut self, scancode: u32, r: &mut Read) {
        if self.mode == K_RAW {
            r.out.push(scancode);
            return;
        }
        let Some(code) = self.decoder.scancode(scancode) else {
            return;
        };
        if code & 0x7f == 0x38 {
            self.xl.compose_key(code & RELEASE == 0);
        }
        self.read_code(code, r);
    }

    /// A keycode, in `K_CODE` or `K_XLATE`.
    fn read_code(&mut self, code: u32, r: &mut Read) {
        if self.mode == K_CODE {
            r.out.push(code);
            return;
        }
        let a = self.xl.code(&self.maps, code);
        r.leds = a.leds.or(r.leds);
        if a.action != NOKEY && a.action != ERRKEY {
            r.out.push(a.action);
        }
        if let Some(c) = self.xl.take_composed().filter(|&c| c != ERRKEY) {
            r.out.push(c);
        }
    }
}

/// What a keyboard read: its values, and the lock state to show.
#[derive(Default)]
struct Read {
    out: Vec<u32>,
    leds: Option<i32>,
}

impl core::fmt::Debug for Keyboard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Keyboard")
            .field("index", &self.index)
            .field("driver", &self.driver)
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

/// What the thread hands the consumer, in order.
enum Item {
    /// Values read in a mode, for the console.
    Keys(i32, Vec<u32>),
}

struct Class {
    /// The keyboards by index (`keyboard[]`); slot 0 waits for
    /// `kbdmux0`.
    kbds: SpinLock<Vec<Option<Arc<Keyboard>>>>,
    events: SpinLock<VecDeque<(Arc<Keyboard>, u16, bool)>>,
    queue: WaitQueue,
    consumer: SpinLock<Option<Arc<dyn Consumer>>>,
    /// The keyboard the console reads (`vd_keyboard`).
    keyboard: SpinLock<Option<Arc<Keyboard>>>,
    console: SpinLock<term::Console>,
    started: AtomicBool,
}

static CLASS: Class = Class {
    kbds: SpinLock::new(Vec::new()),
    events: SpinLock::new(VecDeque::new()),
    queue: WaitQueue::new(),
    consumer: SpinLock::new(None),
    keyboard: SpinLock::new(None),
    console: SpinLock::new(term::Console::new()),
    started: AtomicBool::new(false),
};

/// Sets where the console's typing goes.
pub fn set_consumer(c: Arc<dyn Consumer>) {
    *CLASS.consumer.lock() = Some(c);
}

/// Brings up `kbdmux0` and the class's thread, as FreeBSD's `kbdmux`
/// registers itself at boot, and the console takes `kbdmux0` and every
/// keyboard already registered, as `vt` does. Call once, once the
/// device manager and evdev are up.
///
/// # Errors
///
/// If the class's thread cannot start.
pub fn start(s: &Sleepable) -> Result<(), sched::SpawnError> {
    if CLASS.started.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let r = sched::Builder::new("kbd")
        .priority(sched::Priority::KERNEL)
        .spawn(thread);
    if let Err(e) = r {
        CLASS.started.store(false, Ordering::Release);
        return Err(e);
    }
    // `kbdmux_init`'s evdev device.
    let mut b = evdev::Builder::new("System keyboard multiplexer");
    b.phys("kbdmux0").id(InputId {
        bustype: BUS_VIRTUAL,
        ..InputId::default()
    });
    b.event(EV_KEY).event(EV_LED).event(EV_REP);
    for &k in evdev::keys::KNOWN_KEYS {
        b.key(k);
    }
    b.led(LED_NUML).led(LED_CAPSL).led(LED_SCROLLL);
    let evdev = b.register(s).ok();
    let mux = register("kbdmux", 0, Kind::Mux { evdev }, K_XLATE);
    // `vt`'s mouse, made at the same stage.
    sysmouse::init(s);
    // `kbdmux_init`'s LEDs and repeat.
    let _ = mux.ioctl_value(KDSETLED, 0);
    let mut rep = [0u8; 8];
    rep[..4].copy_from_slice(&DELAY[0].to_ne_bytes());
    rep[4..].copy_from_slice(&DELAY[1].to_ne_bytes());
    let _ = mux.ioctl(KDSETREPEAT, &mut rep);
    // `vt_allocate_keyboard`.
    if mux.allocate(Owner::Console) {
        *CLASS.keyboard.lock() = Some(Arc::clone(&mux));
        let others: Vec<_> = keyboards().into_iter().filter(|k| k.index != 0).collect();
        for k in others {
            if !k.busy() {
                let _ = mux.ioctl(KBADDKBD, &mut k.info().to_bytes());
            }
        }
    }
    Ok(())
}

/// Every registered keyboard.
fn keyboards() -> Vec<Arc<Keyboard>> {
    CLASS.kbds.lock().iter().flatten().cloned().collect()
}

/// `kbd_find_keyboard`.
fn find(driver: &[u8], unit: i32) -> Option<Arc<Keyboard>> {
    keyboards().into_iter().find(|k| {
        (driver == b"*" || k.driver.as_bytes() == driver) && (unit == -1 || k.unit == unit)
    })
}

fn mux() -> Option<Arc<Keyboard>> {
    CLASS.kbds.lock().first().cloned().flatten()
}

/// `kbd_register` and `kbd_attach`: the lowest free index (0 is
/// `kbdmux0`'s), the device node and its `kbdN` link, and a place in
/// `kbdmux0`.
fn register(driver: &str, unit: i32, kind: Kind, mode: i32) -> Arc<Keyboard> {
    let is_mux = matches!(kind, Kind::Mux { .. });
    let mut kbds = CLASS.kbds.lock();
    if kbds.is_empty() {
        kbds.push(None);
    }
    let index = if is_mux {
        0
    } else {
        kbds.iter()
            .skip(1)
            .position(Option::is_none)
            .map_or(kbds.len(), |i| i + 1)
    };
    let k = Arc::new(Keyboard {
        index: u32::try_from(index).unwrap_or(u32::MAX),
        driver: String::from(driver),
        unit,
        kind,
        inner: SpinLock::new(Inner::new(mode)),
        gone: AtomicBool::new(false),
    });
    if index == kbds.len() {
        kbds.push(Some(Arc::clone(&k)));
    } else {
        kbds[index] = Some(Arc::clone(&k));
    }
    drop(kbds);
    let name = k.name();
    let _ = vfs::devices::make_dev(name.as_bytes(), 0o600, 0, 0, Arc::new(Node(Arc::clone(&k))));
    let _ = vfs::devices::make_dev_alias(format!("kbd{index}").as_bytes(), name.as_bytes());
    bus::manager().log(format_args!("kbd{index} at {name}"));
    if !is_mux && let Some(m) = mux() {
        let _ = m.ioctl(KBADDKBD, &mut k.info().to_bytes());
    }
    k
}

/// Registers a keyboard driver instance `name` (`hkbd0`): `leds` is what
/// to call to show the lock LEDs, `evdev` its evdev device. Logs
/// FreeBSD's `kbdN at hkbd0`, and joins `kbdmux0`, which sets its LEDs.
/// Keys typed before [`start`] wait for it.
pub fn attach(
    name: &str,
    leds: impl Fn(Leds) + Send + Sync + 'static,
    evdev: Option<Arc<evdev::Device>>,
) -> Arc<Keyboard> {
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (driver, unit) = name.split_at(name.len() - digits);
    let kind = Kind::Driver {
        leds: Box::new(leds),
        evdev,
    };
    register(driver, unit.parse().unwrap_or(0), kind, K_XLATE)
}

/// The receivers bit hardware keyboards report under.
const RCPT: i32 = evdev::RCPT_HW_KBD;

fn int(arg: &[u8]) -> i32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i32::from_ne_bytes)
}

fn put_int(arg: &mut [u8], v: i32) {
    if let Some(b) = arg.get_mut(..4) {
        b.copy_from_slice(&v.to_ne_bytes());
    }
}

/// `KDSETRAD`'s value for a delay and period (`kbdmux`'s lookup).
fn typematic(delay: i32, period: i32) -> i32 {
    let d = (1..KBDELAYS.len())
        .rev()
        .find(|&i| delay >= KBDELAYS[i])
        .unwrap_or(0);
    let r = (1..KBRATES.len())
        .rev()
        .find(|&i| period >= KBRATES[i])
        .unwrap_or(0);
    i32::try_from(d << 5 | r).unwrap_or(0)
}

/// The delay and period for a `KDSETRAD` value.
fn rates(mode: i32) -> [i32; 2] {
    let m = usize::try_from(mode & 0x7f).unwrap_or(0);
    [KBDELAYS[(m >> 5) & 3], KBRATES[m & 0x1f]]
}

impl Keyboard {
    /// Its device name (`hkbd0`).
    fn name(&self) -> String {
        format!("{}{}", self.driver, self.unit)
    }

    fn evdev(&self) -> Option<&Arc<evdev::Device>> {
        match &self.kind {
            Kind::Driver { evdev, .. } | Kind::Mux { evdev } => evdev.as_ref(),
        }
    }

    fn busy(&self) -> bool {
        !matches!(self.inner.lock().owner, Owner::Free)
    }

    /// `KDGKBINFO`.
    fn info(&self) -> KeyboardInfo {
        let mut name = [0u8; 16];
        let n = self.driver.len().min(15);
        name[..n].copy_from_slice(&self.driver.as_bytes()[..n]);
        let busy = if self.busy() { KB_BUSY } else { 0 };
        KeyboardInfo {
            kb_index: i32::try_from(self.index).unwrap_or(-1),
            kb_name: name,
            kb_unit: self.unit,
            kb_type: if matches!(self.kind, Kind::Mux { .. }) {
                KB_101
            } else {
                KB_OTHER
            },
            kb_config: 0,
            kb_flags: KB_VALID | KB_PROBED | KB_INITIALIZED | KB_REGISTERED | busy,
        }
    }

    /// `kbd_allocate`: takes the keyboard for `owner`, if it is free.
    fn allocate(&self, owner: Owner) -> bool {
        let mut st = self.inner.lock();
        if !matches!(st.owner, Owner::Free) || self.gone.load(Ordering::Acquire) {
            return false;
        }
        st.owner = owner;
        st.clear();
        true
    }

    /// `kbd_release`.
    fn release(&self, owner: &Owner) -> Result<(), Errno> {
        let mut st = self.inner.lock();
        if matches!(st.owner, Owner::Free) {
            return Err(Errno::EINVAL);
        }
        if !st.owner.is(owner) {
            return Err(Errno::EPERM);
        }
        st.owner = Owner::Free;
        st.clear();
        Ok(())
    }

    /// Reports key `usage` (HID keyboard page) pressed or released.
    /// Callable from completion context: it only queues.
    pub fn key(self: &Arc<Self>, usage: u16, down: bool) {
        if let Some(e) = self.evdev() {
            if evdev::receives(RCPT) {
                let code = u8::try_from(usage).map_or(0, evdev::keys::hid2key);
                e.push_key(code, down);
            }
            if e.is_grabbed() {
                return;
            }
        }
        CLASS
            .events
            .lock()
            .push_back((Arc::clone(self), usage, down));
        CLASS.queue.wake_all();
    }

    /// A report ended: its keys go to evdev clients as one report.
    pub fn sync(&self) {
        if let Some(e) = self.evdev()
            && evdev::receives(RCPT)
        {
            e.sync();
        }
    }

    /// A held key repeats: evdev reports it so.
    fn repeat_evdev(&self, usage: u16) {
        if let Some(e) = self.evdev()
            && evdev::receives(RCPT)
        {
            let code = u8::try_from(usage).map_or(0, evdev::keys::hid2key);
            e.push_key(code, true);
            e.sync();
        }
    }

    /// The keyboard is going away (`kbd_unregister`, `kbd_detach`): its
    /// owner lets it go, and its device node and link go.
    pub fn detach(self: &Arc<Self>, s: &Sleepable) {
        self.gone.store(true, Ordering::Release);
        let owner = core::mem::replace(&mut self.inner.lock().owner, Owner::Free);
        match owner {
            Owner::Mux => {
                if let Some(m) = mux() {
                    m.inner.lock().slaves.retain(|k| !Arc::ptr_eq(k, self));
                }
            }
            Owner::Node(r) => {
                r.wait.wake_all();
            }
            // `vt_kbdevent`'s `KBDIO_UNLOADING`.
            Owner::Console => {
                let mut kbd = CLASS.keyboard.lock();
                if kbd.as_ref().is_some_and(|c| Arc::ptr_eq(c, self)) {
                    *kbd = None;
                }
            }
            Owner::Free => {}
        }
        {
            let mut kbds = CLASS.kbds.lock();
            if let Some(slot) = usize::try_from(self.index)
                .ok()
                .and_then(|i| kbds.get_mut(i))
            {
                *slot = None;
            }
        }
        vfs::devices::destroy_dev(s, format!("kbd{}", self.index).as_bytes());
        vfs::devices::destroy_dev(s, self.name().as_bytes());
    }

    /// A key request on the keyboard (`kbdd_ioctl`); `ENODEV` for one it
    /// does not know, as its device node answers.
    ///
    /// # Errors
    ///
    /// As FreeBSD's drivers return them.
    pub fn ioctl(self: &Arc<Self>, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        self.request(req, arg).unwrap_or(Err(Errno::ENODEV))
    }

    /// A request with an `int` argument.
    fn ioctl_value(self: &Arc<Self>, req: u32, v: i32) -> Result<(), Errno> {
        self.ioctl(req, &mut v.to_ne_bytes())
    }

    /// The driver's request, or `None` for one no layer knows
    /// (`ENOIOCTL`).
    fn request(self: &Arc<Self>, req: u32, arg: &mut [u8]) -> Option<Result<(), Errno>> {
        match self.kind {
            Kind::Driver { .. } => self.driver_request(req, arg),
            Kind::Mux { .. } => self.mux_request(req, arg),
        }
    }

    /// `KDSKBMODE`, as `hkbd` and `kbdmux` do it.
    fn set_mode(&self, mode: i32) -> Result<(), Errno> {
        if ![K_XLATE, K_RAW, K_CODE].contains(&mode) {
            return Err(Errno::EINVAL);
        }
        let mut st = self.inner.lock();
        if mode == K_XLATE && st.mode != K_XLATE {
            // The lock state matches the LEDs.
            let led = st.led;
            st.xl.set_locks(led);
        }
        if st.mode != mode {
            st.clear();
            st.mode = mode;
        }
        Ok(())
    }

    /// `hkbd_ioctl`.
    fn driver_request(self: &Arc<Self>, req: u32, arg: &mut [u8]) -> Option<Result<(), Errno>> {
        let v = int(arg);
        Some(match req {
            KDGKBMODE => {
                put_int(arg, self.inner.lock().mode);
                Ok(())
            }
            KDSKBMODE => self.set_mode(v),
            KDGETLED => {
                put_int(arg, self.inner.lock().led);
                Ok(())
            }
            KDSETLED => self.set_led(v),
            KDGKBSTATE => {
                put_int(arg, self.inner.lock().xl.locks());
                Ok(())
            }
            KDSKBSTATE => {
                if v & !LOCK_MASK != 0 {
                    return Some(Err(Errno::EINVAL));
                }
                self.inner.lock().xl.set_locks(v);
                // Emulators may not take LEDs; that fails nothing.
                let _ = self.set_led(v);
                Ok(())
            }
            KDSETREPEAT => {
                let d = [int(arg).max(250), int(arg.get(4..)?).max(34)];
                self.set_delay(d, true);
                Ok(())
            }
            KDSETRAD => {
                if v & !0x7f != 0 {
                    return Some(Err(Errno::EINVAL));
                }
                self.set_delay(rates(v), true);
                Ok(())
            }
            PIO_KEYMAP | PIO_KEYMAPENT | PIO_DEADKEYMAP => {
                self.inner.lock().xl.clear_accents();
                return self.common_request(req, arg);
            }
            _ => return self.common_request(req, arg),
        })
    }

    /// The repeat delay and period, which evdev reports
    /// (`evdev_push_repeats`) if `report`.
    fn set_delay(&self, d: [i32; 2], report: bool) {
        self.inner.lock().delay = d;
        if let Some(e) = self.evdev().filter(|_| report) {
            let _ = e.push(EV_REP, REP_DELAY, d[0]);
            let _ = e.push(EV_REP, REP_PERIOD, d[1]);
        }
    }

    /// `KDSETLED` on a driver's keyboard: its LEDs, and evdev's.
    fn set_led(&self, v: i32) -> Result<(), Errno> {
        if v & !LOCK_MASK != 0 {
            return Err(Errno::EINVAL);
        }
        let shown = {
            let mut st = self.inner.lock();
            let mut i = v;
            // AltGr lock shows on Caps Lock's LED with an AltGr keymap.
            if st.mode == K_XLATE && usize::from(st.maps.n_keys) > ALTGR_OFFSET {
                i = if i & ALKED != 0 {
                    i | CLKED
                } else {
                    i & !CLKED
                };
            }
            st.led = v;
            i
        };
        if let Kind::Driver { leds, evdev } = &self.kind {
            leds(Leds::of(shown));
            if let Some(e) = evdev {
                push_leds(e, shown);
            }
        }
        Ok(())
    }

    /// `kbdmux_ioctl`.
    fn mux_request(self: &Arc<Self>, req: u32, arg: &mut [u8]) -> Option<Result<(), Errno>> {
        let v = int(arg);
        let slaves = || self.inner.lock().slaves.clone();
        Some(match req {
            KBADDKBD => self.add(arg),
            KBRELKBD => self.remove(arg),
            KDGKBMODE => {
                put_int(arg, self.inner.lock().mode);
                Ok(())
            }
            KDSKBMODE => self.set_mode(v),
            KDGETLED => {
                put_int(arg, self.inner.lock().led);
                Ok(())
            }
            KDSETLED => {
                if v & !LOCK_MASK != 0 {
                    return Some(Err(Errno::EINVAL));
                }
                self.inner.lock().led = v;
                if let Some(e) = self.evdev().filter(|_| evdev::receives(evdev::RCPT_KBDMUX)) {
                    push_leds(e, v);
                }
                for k in slaves() {
                    let _ = k.request(KDSETLED, arg);
                }
                Ok(())
            }
            KDGKBSTATE => {
                put_int(arg, self.inner.lock().xl.locks());
                Ok(())
            }
            KDSKBSTATE => {
                if v & !LOCK_MASK != 0 {
                    return Some(Err(Errno::EINVAL));
                }
                self.inner.lock().xl.set_locks(v);
                for k in slaves() {
                    let _ = k.request(KDSKBSTATE, arg);
                }
                return self.mux_request(KDSETLED, arg);
            }
            KDSETREPEAT | KDSETRAD => {
                let mode = if req == KDSETREPEAT {
                    typematic(v, int(arg.get(4..)?))
                } else {
                    v
                };
                if mode & !0x7f != 0 {
                    return Some(Err(Errno::EINVAL));
                }
                self.set_delay(rates(mode), evdev::receives(evdev::RCPT_KBDMUX));
                for k in slaves() {
                    let _ = k.request(req, arg);
                }
                Ok(())
            }
            PIO_KEYMAP | PIO_KEYMAPENT | PIO_DEADKEYMAP => {
                self.inner.lock().xl.clear_accents();
                for k in slaves() {
                    let _ = k.request(req, arg);
                }
                return self.common_request(req, arg);
            }
            _ => return self.common_request(req, arg),
        })
    }

    /// `KBADDKBD`: takes a keyboard into `kbdmux0`, in `K_RAW`, with its
    /// lock state.
    fn add(self: &Arc<Self>, arg: &[u8]) -> Result<(), Errno> {
        let ki = KeyboardInfo::from_bytes(arg).ok_or(Errno::EINVAL)?;
        let name = ki.name();
        if ki.kb_unit < 0 || name.is_empty() || name == b"*" {
            return Err(Errno::EINVAL);
        }
        let has = |k: &Arc<Keyboard>| k.unit == ki.kb_unit && k.driver.as_bytes() == name;
        if self.inner.lock().slaves.iter().any(has) {
            return Ok(());
        }
        let k = find(name, ki.kb_unit)
            .filter(|k| !Arc::ptr_eq(k, self) && k.allocate(Owner::Mux))
            .ok_or(Errno::EINVAL)?;
        let locks = self.inner.lock().xl.locks();
        let r = k.set_mode_request(K_RAW).and_then(|()| {
            k.request(KDSKBSTATE, &mut locks.to_ne_bytes())
                .unwrap_or(Ok(()))
        });
        if let Err(e) = r {
            let _ = k.release(&Owner::Mux);
            return Err(e);
        }
        self.inner.lock().slaves.insert(0, k);
        Ok(())
    }

    fn set_mode_request(self: &Arc<Self>, mode: i32) -> Result<(), Errno> {
        self.request(KDSKBMODE, &mut mode.to_ne_bytes())
            .unwrap_or(Err(Errno::ENODEV))
    }

    /// `KBRELKBD`.
    fn remove(&self, arg: &[u8]) -> Result<(), Errno> {
        let ki = KeyboardInfo::from_bytes(arg).ok_or(Errno::EINVAL)?;
        let name = ki.name();
        if ki.kb_unit < 0 || name.is_empty() || name == b"*" {
            return Err(Errno::EINVAL);
        }
        let k = {
            let st = self.inner.lock();
            st.slaves
                .iter()
                .find(|k| k.unit == ki.kb_unit && k.driver.as_bytes() == name)
                .cloned()
        };
        let k = k.ok_or(Errno::ENXIO)?;
        k.release(&Owner::Mux)?;
        self.inner.lock().slaves.retain(|s| !Arc::ptr_eq(s, &k));
        Ok(())
    }

    /// `genkbd_commonioctl`.
    fn common_request(&self, req: u32, arg: &mut [u8]) -> Option<Result<(), Errno>> {
        match self.common(req, arg) {
            Err(Errno::ENOTTY) => None,
            r => Some(r),
        }
    }

    /// [`Keyboard::common_request`], with `ENOTTY` for an unknown
    /// request.
    fn common(&self, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let len = arg.len();
        let fit = |need: usize| {
            if len < need {
                Err(Errno::EINVAL)
            } else {
                Ok(())
            }
        };
        match req {
            KDGKBINFO => {
                fit(KeyboardInfo::SIZE)?;
                arg[..KeyboardInfo::SIZE].copy_from_slice(&self.info().to_bytes());
            }
            KDGKBTYPE => put_int(arg, self.info().kb_type),
            KDGETREPEAT => {
                fit(8)?;
                let d = self.inner.lock().delay;
                put_int(arg, d[0]);
                put_int(&mut arg[4..], d[1]);
            }
            GIO_KEYMAP => {
                fit(Keymap::SIZE)?;
                self.inner.lock().maps.write_keymap(arg);
            }
            PIO_KEYMAP => {
                let mut st = self.inner.lock();
                if !st.maps.read_keymap(arg) {
                    return Err(Errno::EINVAL);
                }
                st.maps.clear_accents();
            }
            GIO_KEYMAPENT | PIO_KEYMAPENT => {
                let n = KeyArg::keynum(arg).map(usize::from).ok_or(Errno::EINVAL)?;
                if n >= NUM_KEYS {
                    return Err(Errno::EINVAL);
                }
                let mut st = self.inner.lock();
                if req == GIO_KEYMAPENT {
                    KeyArg::set_key(arg, &st.maps.keys[n]);
                } else {
                    st.maps.keys[n] = KeyArg::key(arg).ok_or(Errno::EINVAL)?;
                }
            }
            GIO_DEADKEYMAP => {
                fit(AccentMap::SIZE)?;
                self.inner.lock().maps.write_accents(arg);
            }
            PIO_DEADKEYMAP => {
                if !self.inner.lock().maps.read_accents(arg) {
                    return Err(Errno::EINVAL);
                }
            }
            GETFKEY | SETFKEY => {
                fit(FkeyArg::SIZE)?;
                let n = usize::from(u16::from_ne_bytes([arg[0], arg[1]]));
                if n >= NUM_FKEYS {
                    return Err(Errno::EINVAL);
                }
                let mut st = self.inner.lock();
                let f = &mut st.maps.fkeys[n];
                if req == GETFKEY {
                    let n = usize::from(f.len).min(MAXFK);
                    arg[2..2 + n].copy_from_slice(&f.str[..n]);
                    arg[18] = f.len;
                } else {
                    // `flen` is a `char`: a negative one is nothing.
                    let n = if arg[18] > 0x7f {
                        0
                    } else {
                        usize::from(arg[18]).min(MAXFK)
                    };
                    f.str[..n].copy_from_slice(&arg[2..2 + n]);
                    f.len = u8::try_from(n).unwrap_or(0);
                }
            }
            _ => return Err(Errno::ENOTTY),
        }
        Ok(())
    }
}

/// `evdev_push_leds` (which ends no report).
fn push_leds(e: &evdev::Device, bits: i32) {
    for (code, bit) in [
        (LED_NUML, LED_NUM),
        (LED_CAPSL, LED_CAP),
        (LED_SCROLLL, LED_SCR),
    ] {
        let _ = e.push(EV_LED, code, i32::from(bits & bit != 0));
    }
}

/// An open device node's queue (`genkbd_softc_t`).
struct Reader {
    q: SpinLock<VecDeque<u8>>,
    wait: WaitQueue,
}

impl Reader {
    fn new() -> Self {
        Self {
            q: SpinLock::new(VecDeque::new()),
            wait: WaitQueue::new(),
        }
    }

    fn put(&self, b: u8) {
        let mut q = self.q.lock();
        if q.len() < QSIZE {
            q.push_back(b);
        }
    }

    /// `genkbd_event`: what the keyboard read, as bytes.
    fn event(&self, mode: i32, out: &[u32]) {
        for &c in out {
            let ch = keychar(c).to_le_bytes()[0];
            if mode != K_XLATE {
                self.put(ch);
                continue;
            }
            if c & RELKEY != 0 {
                continue;
            }
            if c & SPCLKEY != 0 {
                // Only the backtab: function keys are special keys too,
                // so `genkbd` never sends their strings.
                if keychar(c) == spcl::BTAB {
                    for b in *b"\x1b[Z" {
                        self.put(b);
                    }
                }
                continue;
            }
            match keyflags(c) {
                0 => self.put(ch),
                MKEY => {
                    self.put(0x1b);
                    self.put(ch);
                }
                _ => {}
            }
        }
        if !self.q.lock().is_empty() {
            self.wait.wake_all();
        }
    }
}

/// A keyboard's device node (`genkbd`'s cdev).
struct Node(Arc<Keyboard>);

impl CharDevice for Node {
    fn instance(&self, _: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if self.0.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        let reader = Arc::new(Reader::new());
        if !self.0.allocate(Owner::Node(Arc::clone(&reader))) {
            return Err(Errno::EBUSY);
        }
        Ok(Some(Arc::new(Open {
            kbd: Arc::clone(&self.0),
            reader,
        })))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }
}

/// An open device node: it owns the keyboard.
struct Open {
    kbd: Arc<Keyboard>,
    reader: Arc<Reader>,
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = self.kbd.release(&Owner::Node(Arc::clone(&self.reader)));
    }
}

impl CharDevice for Open {
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let gone = || self.kbd.gone.load(Ordering::Acquire);
        if gone() {
            return Err(Errno::ENXIO);
        }
        let r = &self.reader;
        while r.q.lock().is_empty() {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            r.wait
                .wait_until_interruptible(s, || !r.q.lock().is_empty() || gone())
                .map_err(|_| Errno::EINTR)?;
            if gone() {
                return Err(Errno::ENXIO);
            }
        }
        let mut done = 0;
        while uio.resid() > 0 {
            let chunk: Vec<u8> = {
                let mut q = r.q.lock();
                let n = q.len().min(uio.resid()).min(64);
                q.drain(..n).collect()
            };
            if chunk.is_empty() {
                break;
            }
            done += uio.copy_out(&chunk)?;
        }
        Ok(done)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        if self.kbd.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        if self.kbd.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        self.kbd.ioctl(req, arg)
    }
}

/// Reads one keyboard's input and hands it to its owner.
struct Work<'a> {
    items: &'a mut Vec<Item>,
}

impl Work<'_> {
    /// A usage pressed, released, or repeated on a driver's keyboard.
    fn usage(&mut self, k: &Arc<Keyboard>, usage: u16, down: bool, repeat: bool) {
        let mut r = Read::default();
        let (owner, mode) = {
            let mut st = k.inner.lock();
            if !repeat {
                st.held.set(usage, down);
                if !scan::is_modifier(usage) {
                    if down {
                        let left_ms = u32::try_from(st.delay[0]).unwrap_or(0);
                        st.repeat = Some(Repeat { usage, left_ms });
                    } else if st.repeat.is_some_and(|r| r.usage == usage) {
                        st.repeat = None;
                    }
                }
            }
            st.read_usage(usage, down, &mut r);
            (st.owner.clone(), st.mode)
        };
        if let Some(l) = r.leds {
            let _ = k.set_led(l);
        }
        self.deliver(owner, mode, &r.out);
    }

    /// Scancodes from one of `kbdmux0`'s keyboards.
    fn scancodes(&mut self, codes: &[u32]) {
        let Some(m) = mux() else { return };
        let mut r = Read::default();
        let (owner, mode) = {
            let mut st = m.inner.lock();
            for &c in codes {
                st.read_scancode(c, &mut r);
            }
            (st.owner.clone(), st.mode)
        };
        if let Some(l) = r.leds {
            let _ = m.ioctl_value(KDSETLED, l);
        }
        self.deliver(owner, mode, &r.out);
    }

    fn deliver(&mut self, owner: Owner, mode: i32, out: &[u32]) {
        if out.is_empty() {
            return;
        }
        match owner {
            Owner::Free => {}
            Owner::Mux => self.scancodes(out),
            Owner::Node(r) => r.event(mode, out),
            Owner::Console => match self.items.last_mut() {
                Some(Item::Keys(m, codes)) if *m == mode => codes.extend_from_slice(out),
                _ => self.items.push(Item::Keys(mode, out.to_vec())),
            },
        }
    }
}

/// The class's thread: repeats, reads, delivery.
fn thread(s: &Sleepable) {
    loop {
        let repeating = |k: &Arc<Keyboard>| k.inner.lock().repeat.is_some();
        if keyboards().iter().any(repeating) {
            bus::manager().sleep_ms(s, u64::from(TICK_MS));
            for k in keyboards() {
                if let Some(r) = &mut k.inner.lock().repeat {
                    r.left_ms = r.left_ms.saturating_sub(TICK_MS);
                }
            }
        } else {
            CLASS
                .queue
                .wait_until(s, || !CLASS.events.lock().is_empty());
        }
        let mut items = Vec::new();
        let mut w = Work { items: &mut items };
        loop {
            let e = CLASS.events.lock().pop_front();
            let Some((k, usage, down)) = e else { break };
            w.usage(&k, usage, down, false);
        }
        for k in keyboards() {
            let due = {
                let mut st = k.inner.lock();
                let period = u32::try_from(st.delay[1]).unwrap_or(0);
                match &mut st.repeat {
                    Some(r) if r.left_ms == 0 => {
                        r.left_ms = period;
                        Some(r.usage)
                    }
                    _ => None,
                }
            };
            if let Some(usage) = due {
                if k.gone.load(Ordering::Acquire) {
                    k.inner.lock().repeat = None;
                    continue;
                }
                k.repeat_evdev(usage);
                w.usage(&k, usage, true, true);
            }
        }
        let consumer = CLASS.consumer.lock().clone();
        if let Some(c) = consumer {
            for Item::Keys(mode, codes) in items {
                if c.keys(s, mode, &codes) {
                    continue;
                }
                let mut bytes = Vec::new();
                for &k in &codes {
                    let e = CLASS.console.lock().key(k, mode, &mut bytes);
                    if let Some(e) = e {
                        if !bytes.is_empty() {
                            c.input(s, &core::mem::take(&mut bytes));
                        }
                        c.event(s, e);
                    }
                }
                if !bytes.is_empty() {
                    c.input(s, &bytes);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typematic_rounds_down_to_the_table() {
        assert_eq!(typematic(500, 100), 1 << 5 | 12);
        assert_eq!(rates(typematic(500, 100)), [500, 100]);
        assert_eq!(rates(typematic(0, 0)), [250, 34]);
        assert_eq!(rates(typematic(5000, 5000)), [1000, 504]);
    }

    #[test]
    fn a_usb_keyboard_types_through_the_mux() {
        // hkbd in K_RAW, kbdmux in K_XLATE: Shift-a.
        let mut hkbd = Inner::new(K_RAW);
        let mut mux = Inner::new(K_XLATE);
        let mut typed = Vec::new();
        for (usage, down) in [(0xe1, true), (0x04, true), (0x04, false), (0xe1, false)] {
            hkbd.held.set(usage, down);
            let mut r = Read::default();
            hkbd.read_usage(usage, down, &mut r);
            let mut m = Read::default();
            for c in r.out {
                mux.read_scancode(c, &mut m);
            }
            typed.extend(m.out);
        }
        assert_eq!(
            typed,
            [
                SPCLKEY | spcl::LSH,
                u32::from(b'A'),
                SPCLKEY | RELKEY | spcl::LSH
            ]
        );
    }

    #[test]
    fn keycode_mode_reports_presses_and_releases() {
        let mut k = Inner::new(K_CODE);
        let mut r = Read::default();
        k.read_usage(0x52, true, &mut r);
        k.read_usage(0x52, false, &mut r);
        assert_eq!(r.out, [0x5f, 0xdf], "Up");
    }
}
