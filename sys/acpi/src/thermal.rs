// SPDX-License-Identifier: BSD-2-Clause
//! `acpi_tz`: ACPI thermal zones (ACPI 6.5 §11).
//!
//! A zone reports its temperature (`_TMP`, in tenths of a kelvin) and the
//! trip points at which the OS cools it:
//!
//! - active cooling: at `_ACx` the devices of `_ALx` (fans) are switched
//!   on, hottest level first;
//! - passive cooling: from `_PSV` the processors of `_PSL` are slowed, in
//!   steps the thermal constants `_TC1` and `_TC2` size, every `_TSP`
//!   tenths of a second, through cpufreq(4) at the kernel's priority;
//! - `_HOT` and `_CRT`: the system powers off, once the temperature has
//!   held there for three polls (a single absurd reading is a known
//!   firmware failure); `_CR3` would suspend, and is only reported.
//!
//! One thread (`acpi_thermal`) polls every zone each
//! `hw.acpi.thermal.polling_rate` seconds, or when a `Notify` asks; each
//! zone that cools passively has a thread of its own (`acpi_coolingN`).
//! `hw.acpi.thermal.tzN` shows a zone, as FreeBSD's does.
//!
//! Passive cooling sets the cpufreq device of the first processor in
//! `_PSL` that has one, and only the first zone of each device cools it
//! (the AbyssBSD Project's FreeBSD change for drivers that set a domain of
//! CPUs, as `acpi_cppc` does); a zone whose `_PSL` names no such
//! processor slows nothing. `_SCP` (the cooling policy) is not set: there
//! is no power profile yet.
//!
//! Behavior reference: sys/dev/acpica/acpi_thermal.c, with the AbyssBSD
//! Project's per-domain cooling.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype};
use aml::{Host, Interpreter, NodeId, Value};
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StopCx, StopReason};
use cpufreq::{PRIO_KERN, Target};
use devtree::ids::AcpiKind;
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use sysctl::{NodeId as Oid, Number, Tree, Value as SysValue};

use crate::bus::AcpiDevice;
use crate::service::{NotifyId, Service, service};

/// 0 °C in tenths of a kelvin.
pub const ZEROC: i32 = 2731;
/// The active cooling levels (`_AC0`–`_AC9`).
pub const LEVELS: usize = 10;
/// No active cooling, or a value firmware does not give.
pub const NONE: i32 = -1;
/// Active cooling not yet known (every cooler is switched off first).
const UNKNOWN: i32 = -2;
/// Polls a critical temperature must hold for.
const VALIDCHECKS: u32 = 3;
/// The poll at which userland is warned (one before the shutdown).
const NOTIFYCOUNT: u32 = VALIDCHECKS - 1;

/// Notifications (ACPI 6.5 §11.3, table 5.169).
const NOTIFY_TEMPERATURE: u64 = 0x80;
const NOTIFY_LEVELS: u64 = 0x81;
const NOTIFY_DEVICES: u64 = 0x82;
/// The notification userland gets near a critical temperature.
const NOTIFY_CRITICAL: u64 = 0xcc;

/// `thermal_flags`: the trip points reached.
pub mod flag {
    /// `_PSV`.
    pub const PSV: i32 = 1 << 0;
    /// `_CR3`.
    pub const CR3: i32 = 1 << 2;
    /// `_HOT`.
    pub const HOT: i32 = 1 << 3;
    /// `_CRT`.
    pub const CRT: i32 = 1 << 4;
}

/// A zone's settings (`acpi_tz_establish`): temperatures in tenths of a
/// kelvin, [`NONE`] where firmware gives none or an absurd one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `_AC0`–`_AC9`, hottest first.
    pub ac: [i32; LEVELS],
    /// The devices of `_AL0`–`_AL9`.
    pub al: [Vec<NodeId>; LEVELS],
    /// `_CRT`, `_HOT`, `_CR3`, `_PSV`.
    pub crt: i32,
    /// `_HOT`.
    pub hot: i32,
    /// `_CR3`.
    pub cr3: i32,
    /// `_PSV`.
    pub psv: i32,
    /// `_TC1`, `_TC2`: the passive cooling constants.
    pub tc1: i32,
    /// `_TC2`.
    pub tc2: i32,
    /// `_TSP`: the passive sampling period, in tenths of a second.
    pub tsp: i32,
    /// `_TZP`: firmware's polling period (FreeBSD does not use it).
    pub tzp: i32,
    /// The CPUs `_PSL` names, in its order.
    pub psl: Vec<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ac: [NONE; LEVELS],
            al: Default::default(),
            crt: NONE,
            hot: NONE,
            cr3: NONE,
            psv: NONE,
            tc1: NONE,
            tc2: NONE,
            tsp: NONE,
            tzp: NONE,
            psl: Vec::new(),
        }
    }
}

impl Settings {
    /// Whether the zone can cool passively (`acpi_tz_cooling_is_available`).
    #[must_use]
    pub fn passive(&self) -> bool {
        self.tc1 != NONE
            && self.tc2 != NONE
            && self.tsp != NONE
            && self.tsp != 0
            && self.psv != NONE
    }

    /// The active level a temperature calls for: the hottest `_ACx` it
    /// has reached, or [`NONE`].
    #[must_use]
    pub fn active_level(&self, temp: i32) -> i32 {
        let mut level = NONE;
        for i in (0..LEVELS).rev() {
            if self.ac[i] != NONE && temp >= self.ac[i] {
                level = i32::try_from(i).unwrap_or(NONE);
            }
        }
        level
    }

    /// The trip points a temperature has reached.
    #[must_use]
    pub fn flags(&self, temp: i32) -> i32 {
        let mut f = 0;
        for (t, bit) in [
            (self.psv, flag::PSV),
            (self.cr3, flag::CR3),
            (self.hot, flag::HOT),
            (self.crt, flag::CRT),
        ] {
            if t != NONE && temp >= t {
                f |= bit;
            }
        }
        f
    }
}

/// A temperature between 0 °C and 200 °C, else [`NONE`]
/// (`acpi_tz_sanity`).
#[must_use]
pub fn sane(v: i32) -> i32 {
    if v != NONE && !(ZEROC..=ZEROC + 2000).contains(&v) {
        NONE
    } else {
        v
    }
}

/// A temperature as FreeBSD prints one: `40.1C`.
fn celsius(v: i32) -> impl core::fmt::Display {
    struct C(i32);
    impl core::fmt::Display for C {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            let d = self.0 - ZEROC;
            write!(f, "{}.{}C", d / 10, (d % 10).abs())
        }
    }
    C(v)
}

/// How passive cooling moves the clock, as `acpi_tz_cpufreq_update` decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing to change.
    Stay,
    /// Not slowed yet, and no need: cooling ends.
    Deactivate,
    /// Back to the level before cooling: cooling ends.
    Restore,
    /// To this level (MHz).
    Set(i32),
}

/// The step for a change of `req` percent of the highest level (positive
/// slows down), from `freq` MHz, with `levels` highest first. `updated`
/// says whether cooling has slowed the clock, from `saved` MHz.
#[must_use]
pub fn step(levels: &[i32], freq: i32, req: i32, updated: bool, saved: i32) -> Step {
    let Some(&max) = levels.first().filter(|&&m| m > 0) else {
        return Step::Stay;
    };
    let perf = (100 * freq / max - req).clamp(0, 100);
    let mut desired = max * perf / 100;
    let i = if desired < freq {
        // The nearest level at or below, else the lowest.
        levels
            .iter()
            .position(|&l| l <= desired)
            .unwrap_or(levels.len() - 1)
    } else {
        // Never above the clock cooling started from.
        if !updated {
            return Step::Deactivate;
        }
        desired = desired.min(saved);
        let i = levels.iter().rposition(|&l| l >= desired).unwrap_or(0);
        if i == 0 || desired == saved {
            return Step::Restore;
        }
        i
    };
    if levels[i] == freq {
        Step::Stay
    } else {
        Step::Set(levels[i])
    }
}

/// The change passive cooling asks for, in percent (ACPI 6.5 §11.1.5.1):
/// `_TC1` times the temperature's rise since the last sample, plus `_TC2`
/// times its excess over `_PSV`, in tenths.
#[must_use]
pub fn passive_change(s: &Settings, curr: i32, prev: i32) -> i32 {
    (s.tc1 * (curr - prev) + s.tc2 * (curr - s.psv)) / 10
}

/// An integer object of the zone, or [`NONE`] (`acpi_tz_getparam`).
fn param(interp: &mut Interpreter, host: &mut dyn Host, node: NodeId, name: &str) -> i32 {
    match interp.evaluate_child(node, name, Vec::new(), host) {
        // `acpi_GetInteger` keeps an int: Ones is -1.
        #[allow(clippy::cast_possible_truncation, reason = "as FreeBSD's int")]
        Ok(Some(Value::Integer(v))) => v as u32 as i32,
        _ => NONE,
    }
}

/// The objects a package of references names.
fn references(
    interp: &mut Interpreter,
    host: &mut dyn Host,
    node: NodeId,
    name: &str,
) -> Vec<NodeId> {
    match interp.evaluate_child(node, name, Vec::new(), host) {
        Ok(Some(Value::Package(p))) => p
            .iter()
            .filter_map(|v| interp.resolve_value_name(v))
            .collect(),
        _ => Vec::new(),
    }
}

/// Reads a zone's settings.
pub fn establish(
    interp: &mut Interpreter,
    host: &mut dyn Host,
    ops: &dyn crate::Ops,
    node: NodeId,
) -> Settings {
    let mut s = Settings::default();
    for i in 0..LEVELS {
        s.ac[i] = sane(param(interp, host, node, &alloc::format!("_AC{i}")));
        s.al[i] = references(interp, host, node, &alloc::format!("_AL{i}"));
    }
    s.crt = sane(param(interp, host, node, "_CRT"));
    s.hot = sane(param(interp, host, node, "_HOT"));
    s.cr3 = sane(param(interp, host, node, "_CR3"));
    s.psv = sane(param(interp, host, node, "_PSV"));
    s.tc1 = param(interp, host, node, "_TC1");
    s.tc2 = param(interp, host, node, "_TC2");
    s.tsp = param(interp, host, node, "_TSP");
    s.tzp = param(interp, host, node, "_TZP");
    let procs = crate::cpc::processors(interp, host);
    s.psl = references(interp, host, node, "_PSL")
        .into_iter()
        .filter_map(|n| {
            let &(_, uid) = procs.iter().find(|p| p.0 == n)?;
            ops.cpu_of_uid(uid)
        })
        .collect();
    s
}

/// `hw.acpi.thermal`'s knobs.
static MIN_RUNTIME: AtomicI32 = AtomicI32::new(0);
static POLLING_RATE: AtomicI32 = AtomicI32::new(10);
static USER_OVERRIDE: AtomicI32 = AtomicI32::new(0);

/// Zone flags the monitor thread takes.
const GETSETTINGS: u32 = 1 << 2;

/// A zone, shared by its driver, the threads, and its sysctls.
struct Zone {
    unit: u32,
    node: NodeId,
    path: String,
    svc: &'static Service,
    settings: SpinLock<Settings>,
    temperature: AtomicI32,
    active: AtomicI32,
    requested: AtomicI32,
    thflags: AtomicI32,
    flags: AtomicU32,
    /// Monitor state.
    state: SpinLock<Monitor>,
    /// Passive cooling.
    cooling_enabled: AtomicBool,
    cooling_running: AtomicBool,
    cooling_q: WaitQueue,
    /// Coolers this zone switched on.
    coolers_on: SpinLock<Vec<NodeId>>,
}

#[derive(Default)]
struct Monitor {
    validchecks: u32,
    insane_notified: bool,
    /// When the active level last changed, in nanoseconds.
    cooling_started: u64,
}

/// The zones, in unit order.
static ZONES: SpinLock<Vec<Arc<Zone>>> = SpinLock::new(Vec::new());
/// Wakes the monitor thread.
static WAKE: WaitQueue = WaitQueue::new();
static PENDING: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static SYSCTL: SpinLock<Option<Oid>> = SpinLock::new(None);

fn signal(z: &Zone, flags: u32) {
    z.flags.fetch_or(flags, Ordering::AcqRel);
    PENDING.store(true, Ordering::Release);
    WAKE.wake_all();
}

fn log(z: &Zone, args: core::fmt::Arguments<'_>) {
    z.svc.ops().log(format_args!("acpi_tz{}: {args}", z.unit));
}

/// `acpi_UserNotify("Thermal", ...)`.
fn user_notify(z: &Zone, notify: u64) {
    let data = alloc::format!("notify=0x{notify:02x}");
    bus::manager().notify("ACPI", "Thermal", &z.path, Some(&data));
}

impl Zone {
    fn settings(&self) -> Settings {
        self.settings.lock().clone()
    }

    fn reestablish(&self, s: &Sleepable) {
        let (node, ops) = (self.node, self.svc.ops());
        let new = self.svc.run(s, move |i, h| establish(i, h, ops, node));
        *self.settings.lock() = new;
    }

    /// `_TMP`, checked (`acpi_tz_get_temperature`).
    fn read_temperature(&self, s: &Sleepable) -> bool {
        let node = self.node;
        let raw = self.svc.run(s, move |i, h| param(i, h, node, "_TMP"));
        let t = sane(raw);
        let mut m = self.state.lock();
        if t == NONE {
            if raw != NONE && !m.insane_notified {
                m.insane_notified = true;
                drop(m);
                log(
                    self,
                    format_args!("_TMP value is absurd, ignored ({})", celsius(raw)),
                );
            }
            return false;
        }
        m.insane_notified = false;
        self.temperature.store(t, Ordering::Release);
        true
    }

    fn switch_coolers(&self, s: &Sleepable, level: usize, on: bool) {
        let devices = self.settings.lock().al[level].clone();
        for d in devices {
            let was_on = self.coolers_on.lock().contains(&d);
            if was_on == on {
                continue;
            }
            self.svc.run(s, move |i, h| {
                if on {
                    let _ = crate::power::to_d0(i, d, h);
                } else {
                    crate::power::to_d3(i, d, h);
                }
            });
            let mut c = self.coolers_on.lock();
            if on {
                c.push(d);
            } else {
                c.retain(|&x| x != d);
            }
        }
    }

    /// Checks the temperature and acts on it (`acpi_tz_monitor`).
    fn monitor(&self, s: &Sleepable) {
        if !self.read_temperature(s) {
            return;
        }
        let temp = self.temperature.load(Ordering::Acquire);
        let z = self.settings();
        let mut newactive = z.active_level(temp);
        let active = self.active.load(Ordering::Acquire);
        let now = self.svc.ops().timer_100ns().saturating_mul(100);
        // A minimum run time before cooling steps down.
        let min = i64::from(MIN_RUNTIME.load(Ordering::Relaxed));
        if min > 0 && active >= 0 && (newactive == NONE || newactive > active) {
            let since = now.saturating_sub(self.state.lock().cooling_started);
            if i64::try_from(since / 1_000_000_000).unwrap_or(i64::MAX) < min {
                newactive = active;
            }
        }
        let requested = self.requested.load(Ordering::Acquire);
        if requested != NONE && (newactive == NONE || requested < newactive) {
            newactive = requested;
        }
        let newflags = z.flags(temp);
        let mut active = active;
        if active == UNKNOWN {
            // Which coolers are on is unknown: all off, then those needed.
            for level in 0..LEVELS {
                let devices = z.al[level].clone();
                self.coolers_on.lock().extend(devices);
                self.switch_coolers(s, level, false);
            }
            active = NONE;
            self.active.store(NONE, Ordering::Release);
        }
        if newactive != active {
            let idx = |a: i32| usize::try_from(a).unwrap_or(LEVELS);
            for level in idx(active)..idx(newactive) {
                self.switch_coolers(s, level, false);
            }
            for level in (idx(newactive)..idx(active)).rev() {
                self.switch_coolers(s, level, true);
            }
            self.active.store(newactive, Ordering::Release);
            self.state.lock().cooling_started = now;
        }
        // Critical: shut down once it holds.
        if newflags & (flag::CR3 | flag::HOT | flag::CRT) != 0 {
            let checks = {
                let mut m = self.state.lock();
                m.validchecks += 1;
                m.validchecks
            };
            if checks == VALIDCHECKS {
                log(
                    self,
                    format_args!(
                        "WARNING - current temperature ({}) exceeds safe limits",
                        celsius(temp)
                    ),
                );
                if newflags & (flag::HOT | flag::CRT) != 0 {
                    self.svc.ops().critical_temperature();
                } else {
                    log(self, format_args!("no sleep state to enter at _CR3"));
                }
            } else if checks == NOTIFYCOUNT {
                user_notify(self, NOTIFY_CRITICAL);
            }
        } else {
            self.state.lock().validchecks = 0;
        }
        self.thflags.store(newflags, Ordering::Release);
    }
}

/// The monitor thread (`acpi_tz_thread`).
fn monitor_thread(s: &Sleepable) {
    // Passive cooling once the boot's devices (the cpufreq ones) are in
    // (FreeBSD's `acpi_tz_startup`, at SI_SUB_KICK_SCHEDULER).
    bus::manager().wait_idle(s);
    start_cooling(s);
    loop {
        PENDING.store(false, Ordering::Release);
        let zones = ZONES.lock().clone();
        for z in &zones {
            let flags = z.flags.swap(0, Ordering::AcqRel);
            z.monitor(s);
            if flags & GETSETTINGS != 0 {
                z.reestablish(s);
                z.monitor(s);
            }
        }
        let secs = u64::try_from(POLLING_RATE.load(Ordering::Relaxed).max(1)).unwrap_or(10);
        let _ = bus::manager().wait_timeout(
            s,
            &WAKE,
            &mut || PENDING.load(Ordering::Acquire),
            secs * 1_000_000_000,
        );
    }
}

/// The cpufreq device a zone slows: that of the first CPU `_PSL` names
/// that has one.
fn cooling_device(z: &Zone) -> Option<Arc<cpufreq::Cpufreq>> {
    z.settings.lock().psl.iter().find_map(|&c| cpufreq::find(c))
}

/// Starts passive cooling on the first zone of each cpufreq device.
fn start_cooling(s: &Sleepable) {
    let zones = ZONES.lock().clone();
    let mut cooled: Vec<u32> = Vec::new();
    for z in &zones {
        if !z.settings.lock().passive() {
            continue;
        }
        let Some(cf) = cooling_device(z) else {
            continue;
        };
        if cooled.contains(&cf.cpu()) {
            continue;
        }
        cooled.push(cf.cpu());
        z.cooling_enabled.store(true, Ordering::Release);
        if start_cooling_thread(z).is_err() {
            z.cooling_enabled.store(false, Ordering::Release);
        }
    }
    let _ = s;
}

fn start_cooling_thread(z: &Arc<Zone>) -> Result<(), Errno> {
    if z.cooling_running.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let zone = Arc::clone(z);
    let name = alloc::format!("acpi_cooling{}", z.unit);
    sched::spawn(&name, sched::Priority::KERNEL, move |s| {
        cooling_thread(s, &zone)
    })
    .map(|_| ())
    .map_err(|_| {
        z.cooling_running.store(false, Ordering::Release);
        Errno::ENOMEM
    })
}

/// Passive cooling's state.
#[derive(Default)]
struct Cooling {
    active: bool,
    updated: bool,
    saved: i32,
}

impl Cooling {
    /// Restores the level before cooling (`acpi_tz_cpufreq_restore`).
    fn restore(&mut self, s: &Sleepable, z: &Zone) -> Result<(), Errno> {
        if !self.updated {
            return Ok(());
        }
        let cf = cooling_device(z).ok_or(Errno::ENXIO)?;
        log(
            z,
            format_args!(
                "temperature {}: resuming previous clock speed ({} MHz)",
                celsius(z.temperature.load(Ordering::Acquire)),
                self.saved
            ),
        );
        cf.set(s, Target::Restore, PRIO_KERN)?;
        self.updated = false;
        Ok(())
    }

    /// Moves the clock by `req` percent (`acpi_tz_cpufreq_update`).
    fn update(&mut self, s: &Sleepable, z: &Zone, req: i32) -> Result<(), Errno> {
        let cf = cooling_device(z).ok_or(Errno::ENXIO)?;
        let freq = cf.get().ok_or(Errno::ENXIO)?;
        let levels: Vec<i32> = cf.levels().iter().map(|l| l.freq).collect();
        match step(&levels, freq, req, self.updated, self.saved) {
            Step::Stay => Ok(()),
            Step::Deactivate => {
                self.active = false;
                Ok(())
            }
            Step::Restore => {
                self.restore(s, z)?;
                self.active = false;
                Ok(())
            }
            Step::Set(to) => {
                log(
                    z,
                    format_args!(
                        "temperature {}: {}creasing clock speed from {freq} MHz to {to} MHz",
                        celsius(z.temperature.load(Ordering::Acquire)),
                        if freq > to { "de" } else { "in" }
                    ),
                );
                cf.set(s, Target::Level(to), PRIO_KERN)?;
                if !self.updated {
                    self.saved = freq;
                    self.updated = true;
                }
                Ok(())
            }
        }
    }
}

/// A zone's passive cooling thread (`acpi_tz_cooling_thread`).
fn cooling_thread(s: &Sleepable, z: &Arc<Zone>) {
    let mut c = Cooling::default();
    let mut prev = z.temperature.load(Ordering::Acquire);
    while z.cooling_enabled.load(Ordering::Acquire) {
        if c.active {
            let _ = z.read_temperature(s);
        }
        let curr = z.temperature.load(Ordering::Acquire);
        let settings = z.settings();
        if curr >= settings.psv {
            c.active = true;
        }
        if c.active {
            let perf = passive_change(&settings, curr, prev);
            if perf != 0
                && let Err(e) = c.update(s, z, perf)
                && e != Errno::EPERM
            {
                log(
                    z,
                    format_args!("failed to set new freq, disabling passive cooling"),
                );
                z.cooling_enabled.store(false, Ordering::Release);
            }
        }
        prev = curr;
        let tenths = u64::try_from(settings.tsp.max(1)).unwrap_or(10);
        let _ = bus::manager().wait_timeout(
            s,
            &z.cooling_q,
            &mut || !z.cooling_enabled.load(Ordering::Acquire),
            tenths * 100_000_000,
        );
    }
    if c.active {
        let _ = c.restore(s, z);
    }
    z.cooling_running.store(false, Ordering::Release);
}

/// `acpi_tzN`.
pub struct AcpiTz {
    zone: Arc<Zone>,
    notify: NotifyId,
    sysctl: Option<Oid>,
}

impl core::fmt::Debug for AcpiTz {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AcpiTz")
            .field("unit", &self.zone.unit)
            .field("path", &self.zone.path)
            .finish_non_exhaustive()
    }
}

fn int_value(f: impl Fn() -> i32 + Send + Sync + 'static) -> SysValue {
    SysValue::Int(Box::new(f))
}

/// `hw.acpi.thermal`, made by the first zone.
fn thermal_node(t: &mut Tree) -> Option<Oid> {
    let mut made = SYSCTL.lock();
    if let Some(n) = *made {
        return Some(n);
    }
    let root = t.root();
    let hw = t.child(root, "hw")?;
    let acpi = match t.child(hw, "acpi") {
        Some(a) => a,
        None => t.add_node(hw, Number::Auto, "acpi", "").ok()?,
    };
    let thermal = t.add_node(acpi, Number::Auto, "thermal", "").ok()?;
    for (name, descr, v) in [
        (
            "min_runtime",
            "minimum cooling run time in sec",
            &MIN_RUNTIME,
        ),
        (
            "polling_rate",
            "monitor polling interval in seconds",
            &POLLING_RATE,
        ),
        (
            "user_override",
            "allow override of thermal settings",
            &USER_OVERRIDE,
        ),
    ] {
        let _ = t.add_writable(
            thermal,
            Number::Auto,
            name,
            descr,
            0,
            int_value(move || v.load(Ordering::Relaxed)),
            sysctl::int_writer(move |x| {
                v.store(x, Ordering::Relaxed);
                if core::ptr::eq(v, &POLLING_RATE) {
                    PENDING.store(true, Ordering::Release);
                    WAKE.wake_all();
                }
                Ok(())
            }),
        );
    }
    *made = Some(thermal);
    Some(thermal)
}

/// A setting a sysctl shows and, with `user_override`, changes.
#[derive(Clone, Copy)]
enum Knob {
    Psv,
    Cr3,
    Hot,
    Crt,
    Tc1,
    Tc2,
    Tsp,
}

impl Knob {
    fn get(self, s: &Settings) -> i32 {
        match self {
            Self::Psv => s.psv,
            Self::Cr3 => s.cr3,
            Self::Hot => s.hot,
            Self::Crt => s.crt,
            Self::Tc1 => s.tc1,
            Self::Tc2 => s.tc2,
            Self::Tsp => s.tsp,
        }
    }

    fn slot(self, s: &mut Settings) -> &mut i32 {
        match self {
            Self::Psv => &mut s.psv,
            Self::Cr3 => &mut s.cr3,
            Self::Hot => &mut s.hot,
            Self::Crt => &mut s.crt,
            Self::Tc1 => &mut s.tc1,
            Self::Tc2 => &mut s.tc2,
            Self::Tsp => &mut s.tsp,
        }
    }

    fn temperature(self) -> bool {
        matches!(self, Self::Psv | Self::Cr3 | Self::Hot | Self::Crt)
    }
}

/// `hw.acpi.thermal.tzN` and its leaves, in FreeBSD's order.
fn add_zone_sysctl(t: &mut Tree, z: &Arc<Zone>) -> Option<Oid> {
    let thermal = thermal_node(t)?;
    let node = t
        .add_node(
            thermal,
            Number::Auto,
            &alloc::format!("tz{}", z.unit),
            "thermal_zone",
        )
        .ok()?;
    let zz = Arc::clone(z);
    let _ = t.add_formatted(
        node,
        Number::Auto,
        "temperature",
        "current thermal zone temperature",
        ctltype::INT | ctlflag::RD | ctlflag::MPSAFE,
        "IK",
        int_value(move || zz.temperature.load(Ordering::Acquire)),
    );
    let (zr, zw) = (Arc::clone(z), Arc::clone(z));
    let _ = t.add_writable(
        node,
        Number::Auto,
        "active",
        "cooling is active",
        ctlflag::MPSAFE,
        int_value(move || zr.active.load(Ordering::Acquire)),
        sysctl::int_writer(move |a| {
            if !(-1..i32::try_from(LEVELS).unwrap_or(10)).contains(&a) {
                return Err(Errno::EINVAL);
            }
            zw.requested.store(a, Ordering::Release);
            signal(&zw, 0);
            Ok(())
        }),
    );
    let (zr, zw) = (Arc::clone(z), Arc::clone(z));
    let _ = t.add_writable(
        node,
        Number::Auto,
        "passive_cooling",
        "enable passive (speed reduction) cooling",
        ctlflag::MPSAFE,
        int_value(move || i32::from(zr.cooling_enabled.load(Ordering::Acquire))),
        sysctl::int_writer(move |e| match e {
            0 => {
                zw.cooling_enabled.store(false, Ordering::Release);
                zw.cooling_q.wake_all();
                Ok(())
            }
            1 => {
                if !zw.settings.lock().passive() || cooling_device(&zw).is_none() {
                    return Err(Errno::ENODEV);
                }
                zw.cooling_enabled.store(true, Ordering::Release);
                start_cooling_thread(&zw).inspect_err(|_| {
                    zw.cooling_enabled.store(false, Ordering::Release);
                })
            }
            _ => Err(Errno::EINVAL),
        }),
    );
    let zz = Arc::clone(z);
    let _ = t.add_value(
        node,
        Number::Auto,
        "thermal_flags",
        "thermal zone flags",
        int_value(move || zz.thflags.load(Ordering::Acquire)),
    );
    let knob = |t: &mut Tree, name: &str, descr: &str, k: Knob| {
        let (zr, zw) = (Arc::clone(z), Arc::clone(z));
        let id = t.add_writable(
            node,
            Number::Auto,
            name,
            descr,
            ctlflag::MPSAFE,
            int_value(move || k.get(&zr.settings.lock())),
            sysctl::int_writer(move |v| {
                // Only with the override (`acpi_tz_temp_sysctl`).
                if USER_OVERRIDE.load(Ordering::Relaxed) == 0 {
                    return Err(Errno::EPERM);
                }
                let v = if k.temperature() {
                    let v = sane(v);
                    if v == NONE {
                        return Err(Errno::EINVAL);
                    }
                    v
                } else {
                    v
                };
                *k.slot(&mut zw.settings.lock()) = v;
                Ok(())
            }),
        );
        if let Ok(id) = id
            && k.temperature()
        {
            t.set_format(id, "IK");
        }
    };
    knob(t, "_PSV", "passive cooling temp setpoint", Knob::Psv);
    knob(t, "_CR3", "too warm temp setpoint (standby now)", Knob::Cr3);
    knob(t, "_HOT", "too hot temp setpoint (suspend now)", Knob::Hot);
    knob(
        t,
        "_CRT",
        "critical temp setpoint (shutdown now)",
        Knob::Crt,
    );
    let zz = Arc::clone(z);
    let _ = t.add_formatted(
        node,
        Number::Auto,
        "_ACx",
        "",
        ctltype::INT | ctlflag::RD | ctlflag::MPSAFE,
        "IK",
        SysValue::Opaque {
            format: "IK",
            read: Box::new(move |out: &mut Vec<u8>| {
                for a in zz.settings.lock().ac {
                    out.extend_from_slice(&a.to_ne_bytes());
                }
            }),
        },
    );
    knob(
        t,
        "_TC1",
        "thermal constant 1 for passive cooling",
        Knob::Tc1,
    );
    knob(
        t,
        "_TC2",
        "thermal constant 2 for passive cooling",
        Knob::Tc2,
    );
    knob(
        t,
        "_TSP",
        "thermal sampling period for passive cooling",
        Knob::Tsp,
    );
    Some(node)
}

impl Driver for AcpiTz {
    const NAME: &'static str = "acpi_tz";
    const IDS: Ids = Ids::AcpiKind(AcpiKind::ThermalZone);

    fn describe(_: &Ident) -> String {
        "Thermal Zone".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let svc = service().ok_or(AttachError::Decline)?;
        let dev = cx.bus_data::<AcpiDevice>().ok_or(AttachError::Decline)?;
        let (node, ops) = (dev.node, svc.ops());
        // A zone without _TMP is ignored.
        let (has_tmp, settings) = svc.run(cx.sleepable(), move |i, h| {
            (i.child(node, "_TMP").is_some(), establish(i, h, ops, node))
        });
        if !has_tmp {
            return Err(AttachError::Decline);
        }
        let zone = Arc::new(Zone {
            unit: cx.unit(),
            node,
            path: dev.path.clone(),
            svc,
            settings: SpinLock::new(settings),
            temperature: AtomicI32::new(0),
            active: AtomicI32::new(UNKNOWN),
            requested: AtomicI32::new(NONE),
            thflags: AtomicI32::new(0),
            flags: AtomicU32::new(0),
            state: SpinLock::new(Monitor::default()),
            cooling_enabled: AtomicBool::new(false),
            cooling_running: AtomicBool::new(false),
            cooling_q: WaitQueue::new(),
            coolers_on: SpinLock::new(Vec::new()),
        });
        let z = Arc::clone(&zone);
        let notify = svc.on_notify(
            node,
            Arc::new(move |value| {
                match value {
                    NOTIFY_TEMPERATURE => signal(&z, 0),
                    NOTIFY_LEVELS | NOTIFY_DEVICES => signal(&z, GETSETTINGS),
                    _ => log(&z, format_args!("unknown Notify event 0x{value:x}")),
                }
                user_notify(&z, value);
            }),
        );
        let mut sysctl = None;
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            sysctl = add_zone_sysctl(t, &zone);
        });
        ZONES.lock().push(Arc::clone(&zone));
        if !STARTED.swap(true, Ordering::AcqRel)
            && sched::spawn("acpi_thermal", sched::Priority::KERNEL, monitor_thread).is_err()
        {
            ops.log(format_args!("acpi_tz: could not create thread"));
        }
        // The monitor's first pass reads the temperature.
        signal(&zone, 0);
        Ok(Self {
            zone,
            notify,
            sysctl,
        })
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        if let Some(svc) = service() {
            svc.remove_notify(self.notify);
        }
        ZONES.lock().retain(|z| !Arc::ptr_eq(z, &self.zone));
        self.zone.cooling_enabled.store(false, Ordering::Release);
        self.zone.cooling_q.wake_all();
        if let Some(n) = self.sysctl {
            bus::manager().sysctl(cx.sleepable(), &mut |t| {
                t.remove(n);
            });
        }
    }
}

/// The declaration for the build's driver table.
pub const THERMAL: DriverDecl = DriverDecl::of::<AcpiTz>();

#[cfg(test)]
mod tests {
    use super::*;

    /// Sky1's `TZB1`: passive at 85 °C, critical at 98 °C.
    fn tzb1() -> Settings {
        Settings {
            psv: 0x0dfe,
            crt: 0x0e80,
            tc1: 4,
            tc2: 3,
            tsp: 3,
            tzp: 10,
            psl: alloc::vec![0, 1],
            ..Settings::default()
        }
    }

    const fn c(deg: i32) -> i32 {
        ZEROC + deg * 10
    }

    #[test]
    fn trip_points_and_sanity() {
        let z = tzb1();
        assert!(z.passive());
        assert_eq!(z.flags(c(40)), 0);
        assert_eq!(z.flags(c(86)), flag::PSV);
        assert_eq!(z.flags(c(99)), flag::PSV | flag::CRT);
        assert_eq!(sane(c(40)), c(40));
        assert_eq!(sane(c(-1)), NONE);
        assert_eq!(sane(c(201)), NONE);
        assert_eq!(sane(NONE), NONE);
        assert_eq!(alloc::format!("{}", celsius(0x0dfe)), "85.1C");
        assert_eq!(alloc::format!("{}", celsius(c(-2) - 3)), "-2.3C");
        let mut a = Settings::default();
        a.ac[0] = c(70);
        a.ac[1] = c(50);
        assert_eq!(a.active_level(c(40)), NONE);
        assert_eq!(a.active_level(c(60)), 1);
        assert_eq!(a.active_level(c(75)), 0);
        assert!(!a.passive());
    }

    #[test]
    fn passive_cooling_steps_down_and_restores() {
        let z = tzb1();
        // 2 °C over _PSV and rising 1 °C: (4*10 + 3*20)/10 = 10%.
        assert_eq!(passive_change(&z, z.psv + 20, z.psv + 10), 10);
        let levels: Vec<i32> = (8..=26).rev().map(|m| m * 100).collect();
        // 10% of 2600 below 2600 is 2340: the level at or below, 2300.
        assert_eq!(step(&levels, 2600, 10, false, 0), Step::Set(2300));
        // Cooler again: back up, rounding up (88% + 5% of 2600 is 2418),
        // never past the clock it started from.
        assert_eq!(step(&levels, 2300, -5, true, 2600), Step::Set(2500));
        assert_eq!(step(&levels, 2500, -20, true, 2600), Step::Restore);
        // Not slowed yet and no call to: cooling ends.
        assert_eq!(step(&levels, 2600, -3, false, 0), Step::Deactivate);
        // Beyond the lowest stays at the lowest.
        assert_eq!(step(&levels, 900, 90, true, 2600), Step::Set(800));
        assert_eq!(step(&levels, 800, 50, true, 2600), Step::Stay);
    }
}
