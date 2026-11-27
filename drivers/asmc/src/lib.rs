// SPDX-License-Identifier: BSD-2-Clause
//! `asmc`: the Apple System Management Controller (`APP0001`) of Intel
//! Macs, as FreeBSD's `sys/dev/asmc`: its keys, its fans and temperature
//! sensors, and, where the firmware's device properties give a fan table,
//! a fan floor and the SMC's platform limit (ADR-0027 §3).
//!
//! - Back end: the memory window when it answers (`LDKN` >= 2, the Mac Pro
//!   (Late 2013) and later), else the port window, as FreeBSD's
//!   `asmc_try_probe`. Every transaction is bounded, and a jammed SMC is
//!   retired ([`smc`]).
//! - `dev.asmc.N.fan.F.{id,speed,safespeed,minspeed,maxspeed,targetspeed,
//!   manual}`: a fan's keys. Speeds are `fpe2`, or `flt ` where the key
//!   says so (T2 Macs); the type is read from the key, not guessed from the
//!   back end (FreeBSD treats every memory-window SMC as a T2's). Writes
//!   are range-checked rather than wrapped.
//! - `dev.asmc.N.temp.KEY`: each two-byte `sp78` key from `T` to `U`
//!   (`asmc_detect_sensors`), in deciKelvin.
//! - `dev.asmc.N.fanpolicy.*`: the fan floor ([`policy`]), from the device
//!   properties `apple,fan-floor-*`. The fan stays in automatic mode; the
//!   floor goes back to the firmware minimum when the driver stops
//!   (shutdown, reboot, detach). After a panic it stays raised: a fan that
//!   fails high is safe, and a power-off clears it (the fork's choice).
//! - `dev.asmc.N.plimit.{active,level,gpu,cpu}`: the SMC's platform limit,
//!   from `apple,power-limit-keys`; [`platform_limit`] and
//!   [`on_platform_limit`] for GPU drivers.
//!
//! Not here: the sudden motion sensor, the keyboard backlight and the light
//! sensor (laptops).
//!
//! Behavior references: FreeBSD's sys/dev/asmc (`asmc.c`, `asmc_mmio.c`,
//! `asmc_pio.c`, `asmc_sysctl.c`), and the FreeBSD fork's `asmc_fan.c` and
//! `asmc_plimit.c` (docs/MACPRO.md §2).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod policy;
pub mod smc;
pub mod value;

#[cfg(test)]
mod tests;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IoGrant, MmioGrant, StartCx, StopCx,
    StopReason,
};
use devtree::ids::AcpiId;
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use sysctl::{NodeId, Number};

use policy::{Curve, Floor, Limit, Reading};
use regs::Bytes;
use smc::{Clock, Key, MmioIo, PortIo, Smc, SmcError};

/// The most fans (`ASMC_MAXFANS`).
const MAX_FANS: u8 = 6;
/// The most temperature sensors (`ASMC_TEMP_MAX`).
const MAX_TEMPS: usize = 80;

/// The port window.
#[allow(missing_debug_implementations)]
pub struct Ports(IoGrant);

impl PortIo for Ports {
    fn read8(&self, off: u16) -> u8 {
        self.0.read8(off)
    }
    fn write8(&self, off: u16, v: u8) {
        self.0.write8(off, v);
    }
}

/// The memory window.
#[allow(missing_debug_implementations)]
pub struct Memory(MmioGrant<Bytes>);

impl MmioIo for Memory {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read8(&self, off: usize) -> u8 {
        self.0.read8(off)
    }
    fn write8(&self, off: usize, v: u8) {
        self.0.write8(off, v);
    }
    fn write32(&self, off: usize, v: u32) {
        self.0.write32(off, v);
    }
}

/// The kernel's clock.
#[allow(missing_debug_implementations)]
pub struct BusClock;

impl Clock for BusClock {
    fn delay_us(&self, us: u64) {
        bus::manager().delay_us(us);
    }
    fn now_us(&self) -> u64 {
        sched::now_ns() / 1000
    }
}

type KernelSmc = Smc<Ports, Memory, BusClock>;

// The platform limit, for GPU drivers. One SMC per machine.

/// Whether the SMC's platform limit is on.
static LIMIT_ON: AtomicBool = AtomicBool::new(false);

/// A platform-limit listener.
pub type LimitFn = Arc<dyn Fn(bool) + Send + Sync>;

static LISTENERS: SpinLock<Vec<LimitFn>> = SpinLock::new(Vec::new());

/// Whether the SMC's platform limit is on: the GPUs should run at their
/// lowest level (the Mac Pro (Late 2013); macOS drops them to 300 MHz).
#[must_use]
pub fn platform_limit() -> bool {
    LIMIT_ON.load(Ordering::Acquire)
}

/// Registers `f`, called with the limit's state on each change, and once
/// now with the current state, so a driver that loads while the limit is
/// latched (it is across a warm reboot) learns of it.
pub fn on_platform_limit(f: LimitFn) {
    let now = platform_limit();
    LISTENERS.lock().push(Arc::clone(&f));
    f(now);
}

/// Removes a listener [`on_platform_limit`] registered.
pub fn off_platform_limit(f: &LimitFn) {
    LISTENERS.lock().retain(|g| !Arc::ptr_eq(g, f));
}

fn publish_limit(on: bool) {
    LIMIT_ON.store(on, Ordering::Release);
    let listeners = LISTENERS.lock().clone();
    for f in listeners {
        f(on);
    }
}

/// A key name for fan `n` from a pattern with `%` where the digit goes
/// (`"F%Mn"`).
fn fan_key(pattern: &[u8; 4], n: u8) -> Key {
    pattern.map(|b| if b == b'%' { b'0' + n } else { b })
}

/// The fan floor's configuration and state.
struct FanFloor {
    fan: u8,
    settings: SpinLock<Floor>,
    enabled: AtomicBool,
    gpu_keys: Vec<Key>,
    cpu_keys: Vec<Key>,
    /// The last floor written, rpm; `u32::MAX` before the first.
    floor: AtomicU32,
}

/// The platform limit's keys and state.
struct LimitKeys {
    level: Key,
    gpu: Key,
    cpu: Key,
    state: SpinLock<Limit>,
    gpu_step: AtomicU32,
    cpu_step: AtomicU32,
}

/// What the driver keeps.
struct Inner {
    smc: KernelSmc,
    unit: u32,
    fans: u8,
    temps: Vec<Key>,
    floor: Option<FanFloor>,
    limit: Option<LimitKeys>,
    stopping: AtomicBool,
    wake: WaitQueue,
    worker: SpinLock<Option<Arc<sched::Thread>>>,
    nodes: SpinLock<Vec<NodeId>>,
}

impl Inner {
    fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("asmc{}: {args}", self.unit));
    }

    /// A fan speed in rpm, by the key's own type.
    fn fan_speed(&self, key: Key) -> Result<u32, SmcError> {
        let info = self.smc.info(key)?;
        if &info.ty == b"flt " && info.len == 4 {
            let mut b = [0u8; 4];
            self.smc.read(key, &mut b)?;
            Ok(value::float_to_u32(u32::from_le_bytes(b)))
        } else {
            let mut b = [0u8; 2];
            self.smc.read(key, &mut b)?;
            Ok(value::fpe2(b))
        }
    }

    /// Sets a fan speed key, in its own type; out of range is `EINVAL`.
    fn set_fan_speed(&self, key: Key, rpm: u32) -> Result<(), Errno> {
        let info = self.smc.info(key).map_err(|_| Errno::EIO)?;
        let r = if &info.ty == b"flt " && info.len == 4 {
            if rpm > 65_535 {
                return Err(Errno::EINVAL);
            }
            self.smc
                .write_checked(key, &value::u32_to_float(rpm).to_le_bytes())
        } else {
            let b = value::to_fpe2(rpm).ok_or(Errno::EINVAL)?;
            self.smc.write_checked(key, &b)
        };
        r.map_err(|_| Errno::EIO)
    }

    /// A two-byte sensor in millidegrees.
    fn sensor(&self, key: Key) -> Option<i32> {
        let info = self.smc.info(key).ok()?;
        if info.len != 2 {
            return None;
        }
        let mut b = [0u8; 2];
        self.smc.read(key, &mut b).ok()?;
        value::sensor_milli(&info.ty, b)
    }

    /// One fan floor sample; writes the floor when it should.
    fn floor_tick(&self, f: &FanFloor) {
        if !f.enabled.load(Ordering::Acquire) {
            return;
        }
        let settings = *f.settings.lock();
        let read = |keys: &[Key]| {
            keys.iter()
                .map(|&k| self.sensor(k))
                .collect::<Vec<Reading>>()
        };
        let (gpu, cpu) = (read(&f.gpu_keys), read(&f.cpu_keys));
        let last = match f.floor.load(Ordering::Acquire) {
            u32::MAX => None,
            n => Some(n),
        };
        let (next, write) = settings.step(last, &gpu, &cpu);
        if write {
            let key = fan_key(b"F%Mn", f.fan);
            if self.set_fan_speed(key, next).is_ok() {
                f.floor.store(next, Ordering::Release);
            }
        }
    }

    /// Gives the fan back its firmware minimum.
    fn floor_restore(&self, f: &FanFloor) {
        let fwmin = f.settings.lock().fwmin;
        if self.set_fan_speed(fan_key(b"F%Mn", f.fan), fwmin).is_ok() {
            f.floor.store(fwmin, Ordering::Release);
        }
    }

    /// One platform-limit sample. The limit is published on every sample
    /// while on (the fork's `421c666bda5a`), and on the change to off.
    fn limit_tick(&self, l: &LimitKeys) {
        let mut b = [0u8; 4];
        let level = self
            .smc
            .read(l.level, &mut b)
            .ok()
            .map(|()| value::limit(b));
        let (changed, on) = {
            let mut s = l.state.lock();
            let changed = s.sample(level);
            (changed, s.active())
        };
        if on || changed {
            publish_limit(on);
        }
        let mut one = [0u8; 1];
        if self.smc.read(l.gpu, &mut one).is_ok() {
            l.gpu_step.store(u32::from(one[0]), Ordering::Relaxed);
        }
        if self.smc.read(l.cpu, &mut one).is_ok() {
            l.cpu_step.store(u32::from(one[0]), Ordering::Relaxed);
        }
    }
}

/// `asmcN`.
pub struct Asmc {
    inner: Arc<Inner>,
}

impl core::fmt::Debug for Asmc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Asmc")
            .field("unit", &self.inner.unit)
            .field("fans", &self.inner.fans)
            .finish_non_exhaustive()
    }
}

/// Device property `name` as an integer.
fn prop_int(cx: &AttachCx<'_>, name: &'static str) -> Option<u64> {
    match acpi::properties::value(cx, name)? {
        aml::Value::Integer(n) => Some(n),
        _ => None,
    }
}

/// Device property `name` as a package of integers.
fn prop_ints(cx: &AttachCx<'_>, name: &'static str) -> Option<Vec<u64>> {
    match acpi::properties::value(cx, name)? {
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

/// Device property `name` as SMC keys (four-character strings).
fn prop_keys(cx: &AttachCx<'_>, name: &'static str) -> Option<Vec<Key>> {
    match acpi::properties::value(cx, name)? {
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

/// A curve from a two-integer range property.
fn curve(v: &[u64]) -> Option<Curve> {
    let [start, full] = v else { return None };
    Some(Curve {
        start: i32::try_from(*start).ok()?,
        full: i32::try_from(*full).ok()?,
    })
}

/// The fan floor, if the properties give one and the fan's minimum key is
/// the two-byte `fpe2` the policy writes.
fn fan_floor(
    cx: &AttachCx<'_>,
    smc: &KernelSmc,
    fans: u8,
) -> Result<Option<FanFloor>, &'static str> {
    let Some(fan) = prop_int(cx, "apple,fan-floor-fan") else {
        return Ok(None);
    };
    let fan = u8::try_from(fan)
        .ok()
        .filter(|&f| f < fans)
        .ok_or("no such fan")?;
    let fwmin = prop_int(cx, "apple,fan-floor-firmware-minimum").ok_or("no firmware minimum")?;
    let top = prop_int(cx, "apple,fan-floor-top").ok_or("no top")?;
    let gpu = prop_ints(cx, "apple,fan-floor-gpu-range")
        .as_deref()
        .and_then(curve)
        .ok_or("no GPU range")?;
    let cpu = prop_ints(cx, "apple,fan-floor-cpu-range")
        .as_deref()
        .and_then(curve)
        .ok_or("no CPU range")?;
    let gpu_keys = prop_keys(cx, "apple,fan-floor-gpu-keys").ok_or("no GPU keys")?;
    let cpu_keys = prop_keys(cx, "apple,fan-floor-cpu-keys").ok_or("no CPU keys")?;
    let info = smc
        .info(fan_key(b"F%Mn", fan))
        .map_err(|_| "no minimum key")?;
    if info.len != 2 || &info.ty != b"fpe2" {
        return Err("the minimum key is not a two-byte fpe2");
    }
    let (fwmin, top) = (
        u32::try_from(fwmin).map_err(|_| "firmware minimum")?,
        u32::try_from(top).map_err(|_| "top")?,
    );
    if top < fwmin || top > 4000 {
        return Err("top out of range");
    }
    Ok(Some(FanFloor {
        fan,
        settings: SpinLock::new(Floor {
            fwmin,
            top,
            gpu,
            cpu,
        }),
        enabled: AtomicBool::new(true),
        gpu_keys,
        cpu_keys,
        floor: AtomicU32::new(u32::MAX),
    }))
}

/// The platform limit, if the properties name its keys and the level key
/// is a four-byte `flt `.
fn limit_keys(cx: &AttachCx<'_>, smc: &KernelSmc) -> Result<Option<LimitKeys>, &'static str> {
    let Some(keys) = prop_keys(cx, "apple,power-limit-keys") else {
        return Ok(None);
    };
    let [level, gpu, cpu] = keys[..] else {
        return Err("want three keys");
    };
    let info = smc.info(level).map_err(|_| "no level key")?;
    if info.len != 4 || &info.ty != b"flt " {
        return Err("the level key is not a four-byte flt");
    }
    Ok(Some(LimitKeys {
        level,
        gpu,
        cpu,
        state: SpinLock::new(Limit::default()),
        gpu_step: AtomicU32::new(0),
        cpu_step: AtomicU32::new(0),
    }))
}

/// The two-byte `sp78` keys from `T` to `U` (`asmc_detect_sensors`), found
/// by a binary search for each end over the key table.
fn temperature_keys(smc: &KernelSmc, nkeys: u32) -> Vec<Key> {
    let search = |prefix: &Key| -> Option<u32> {
        let (mut lo, mut hi) = (0, nkeys);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if smc.key_at(mid).ok()? < *prefix {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Some(lo)
    };
    let (Some(start), Some(end)) = (search(b"T\0\0\0"), search(b"U\0\0\0")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in start..end {
        let Ok(k) = smc.key_at(i) else { continue };
        if smc
            .info(k)
            .is_ok_and(|info| info.len == 2 && &info.ty == b"sp78")
        {
            out.push(k);
            if out.len() == MAX_TEMPS {
                break;
            }
        }
    }
    out
}

impl Driver for Asmc {
    const NAME: &'static str = "asmc";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("APP0001")]);

    fn describe(_: &Ident) -> String {
        "Apple SMC".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let unit = cx.unit();
        let log = |a: core::fmt::Arguments<'_>| bus::manager().log(format_args!("asmc{unit}: {a}"));
        // The memory window first; the port range can be claimable but
        // dead (`asmc_try_probe`).
        let mut smc = None;
        if let Ok(m) = cx.mmio_bytes(0) {
            match KernelSmc::probe_mmio(Memory(m), BusClock) {
                Ok(s) => smc = Some(s),
                Err(_) => log(format_args!("memory window does not answer; trying ports")),
            }
        }
        let smc = match smc {
            Some(s) => s,
            None => {
                let p = cx.io_ports(0).map_err(|_| AttachError::Decline)?;
                KernelSmc::with_ports(Ports(p), BusClock)
            }
        };
        let mut rev = [0u8; 6];
        match smc.read(*b"REV ", &mut rev) {
            Ok(()) => log(format_args!(
                "SMC revision: {:x}.{:x}{:x}{:x} ({:?})",
                rev[0],
                rev[1],
                rev[2],
                u16::from_be_bytes([rev[4], rev[5]]),
                smc.backend()
            )),
            Err(_) => {
                let mut n = [0u8; 4];
                smc.read(*b"#KEY", &mut n)
                    .map_err(|_| AttachError::Device("the SMC does not answer"))?;
            }
        }
        let mut b = [0u8; 4];
        let nkeys = smc
            .read(*b"#KEY", &mut b)
            .map_or(0, |()| u32::from_be_bytes(b));
        let mut one = [0u8; 1];
        let fans = smc
            .read(*b"FNum", &mut one)
            .map_or(0, |()| one[0])
            .min(MAX_FANS);
        let temps = temperature_keys(&smc, nkeys);
        let floor = fan_floor(cx, &smc, fans).unwrap_or_else(|why| {
            log(format_args!("fan policy: {why}"));
            None
        });
        let limit = limit_keys(cx, &smc).unwrap_or_else(|why| {
            log(format_args!("power limit: {why}"));
            None
        });
        if let Some(f) = &floor {
            let s = f.settings.lock();
            log(format_args!(
                "fan {} floor policy on (firmware minimum {} rpm)",
                f.fan, s.fwmin
            ));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                smc,
                unit,
                fans,
                temps,
                floor,
                limit,
                stopping: AtomicBool::new(false),
                wake: WaitQueue::new(),
                worker: SpinLock::new(None),
                nodes: SpinLock::new(Vec::new()),
            }),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let s = cx.sleepable();
        sysctls(&self.inner, s);
        if self.inner.floor.is_some() || self.inner.limit.is_some() {
            let inner = Arc::clone(&self.inner);
            let t = sched::Builder::new(&alloc::format!("asmc{}", self.inner.unit))
                .priority(sched::Priority::KERNEL)
                .spawn(move |s| worker(&inner, s))
                .map_err(|_| AttachError::NoMemory)?;
            *self.inner.worker.lock() = Some(t);
        }
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        let inner = &self.inner;
        inner.stopping.store(true, Ordering::Release);
        inner.wake.wake_all();
        if let Some(t) = inner.worker.lock().take() {
            t.join(s);
        }
        // Shutdown, reboot and detach all give the fan its firmware minimum
        // back: a warm reboot keeps a written minimum otherwise.
        if let Some(f) = &inner.floor {
            inner.floor_restore(f);
        }
        if let Some(l) = &inner.limit
            && l.state.lock().release()
        {
            publish_limit(false);
        }
        let nodes = core::mem::take(&mut *inner.nodes.lock());
        bus::manager().sysctl(s, &mut |t| {
            for &n in nodes.iter().rev() {
                t.remove(n);
            }
        });
    }
}

/// The worker: a fan floor and a platform-limit sample every
/// [`policy::INTERVAL_S`].
fn worker(inner: &Arc<Inner>, s: &Sleepable) {
    let stopping = || inner.stopping.load(Ordering::Acquire);
    while !stopping() {
        if let Some(f) = &inner.floor {
            inner.floor_tick(f);
        }
        if let Some(l) = &inner.limit {
            inner.limit_tick(l);
        }
        if inner.smc.retired() {
            inner.log(format_args!(
                "the SMC stopped answering; leaving the fan as it is"
            ));
            break;
        }
        let mut done = stopping;
        bus::manager().wait_timeout(
            s,
            &inner.wake,
            &mut done,
            policy::INTERVAL_S * 1_000_000_000,
        );
    }
}

/// A sysctl's write: the value written.
type SetFn = Arc<dyn Fn(i32) -> Result<(), Errno> + Send + Sync>;

/// A platform-limit leaf: name, description, reader.
type LimitLeaf = (&'static str, &'static str, fn(&LimitKeys) -> i32);

const RO_INT: u32 = ctltype::INT | ctlflag::RD | ctlflag::MPSAFE;
const RW_INT: u32 = ctltype::INT | ctlflag::RW | ctlflag::MPSAFE;
const RO_STR: u32 = ctltype::STRING | ctlflag::RD | ctlflag::MPSAFE;

/// An integer sysctl: `get` reads, `set` (if any) takes a write.
fn int_query(
    get: impl Fn() -> Result<i32, Errno> + Send + Sync + 'static,
    set: Option<SetFn>,
) -> sysctl::QueryFn {
    Arc::new(move |written, out| {
        let v = get()?;
        if let Some(w) = written {
            let set = set.as_ref().ok_or(Errno::EPERM)?;
            let w: [u8; 4] = w.try_into().map_err(|_| Errno::EINVAL)?;
            set(i32::from_ne_bytes(w))?;
        }
        out.extend_from_slice(&v.to_ne_bytes());
        Ok(())
    })
}

fn rpm(v: i32) -> Result<u32, Errno> {
    u32::try_from(v).map_err(|_| Errno::EINVAL)
}

/// The sysctl tree under `dev.asmc.N`.
fn sysctls(inner: &Arc<Inner>, s: &Sleepable) {
    bus::manager().sysctl(s, &mut |t| {
        let Some(dev) = bus::device_node(t, "asmc", inner.unit) else {
            return;
        };
        let mut nodes = inner.nodes.lock();
        // Fans.
        if inner.fans > 0
            && let Ok(fan) = t.add_node(dev, Number::Auto, "fan", "Fan Root Tree")
        {
            nodes.push(fan);
            for n in 0..inner.fans {
                let Ok(node) = t.add_node(fan, Number::Auto, &alloc::format!("{n}"), "Fan Subtree")
                else {
                    continue;
                };
                let i = Arc::clone(inner);
                let id: sysctl::QueryFn = Arc::new(move |w, out| {
                    if w.is_some() {
                        return Err(Errno::EPERM);
                    }
                    let mut b = [0u8; 16];
                    i.smc
                        .read(fan_key(b"F%ID", n), &mut b)
                        .map_err(|_| Errno::ENXIO)?;
                    let name = &b[4..];
                    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                    out.extend_from_slice(&name[..end]);
                    out.push(0);
                    Ok(())
                });
                let _ = t.add_query(node, Number::Auto, "id", "Fan ID", RO_STR, "A", id);
                let speeds: [(&str, &str, &[u8; 4], bool); 5] = [
                    ("speed", "Fan speed in RPM", b"F%Ac", false),
                    ("safespeed", "Fan safe speed in RPM", b"F%Sf", false),
                    ("minspeed", "Fan minimum speed in RPM", b"F%Mn", true),
                    ("maxspeed", "Fan maximum speed in RPM", b"F%Mx", true),
                    ("targetspeed", "Fan target speed in RPM", b"F%Tg", true),
                ];
                for (name, descr, pattern, writable) in speeds {
                    let key = fan_key(pattern, n);
                    let (g, w) = (Arc::clone(inner), Arc::clone(inner));
                    let set: Option<SetFn> = writable
                        .then(|| Arc::new(move |v: i32| w.set_fan_speed(key, rpm(v)?)) as SetFn);
                    let get = move || {
                        g.fan_speed(key)
                            .map_err(|_| Errno::ENXIO)
                            .and_then(|r| i32::try_from(r).map_err(|_| Errno::ERANGE))
                    };
                    let kind = if writable { RW_INT } else { RO_INT };
                    let _ = t.add_query(
                        node,
                        Number::Auto,
                        name,
                        descr,
                        kind,
                        "I",
                        int_query(get, set),
                    );
                }
                let (g, w) = (Arc::clone(inner), Arc::clone(inner));
                let _ = t.add_query(
                    node,
                    Number::Auto,
                    "manual",
                    "Fan manual mode (0=auto, 1=manual)",
                    RW_INT,
                    "I",
                    int_query(
                        move || manual(&g, n),
                        Some(Arc::new(move |v| set_manual(&w, n, v))),
                    ),
                );
            }
        }
        // Temperatures.
        if !inner.temps.is_empty()
            && let Ok(temp) = t.add_node(dev, Number::Auto, "temp", "Temperature sensors")
        {
            nodes.push(temp);
            for &k in &inner.temps {
                let name = String::from_utf8_lossy(&k).into_owned();
                let i = Arc::clone(inner);
                let get = move || i.sensor(k).map(value::milli_c_to_dk).ok_or(Errno::EIO);
                let _ = t.add_query(
                    temp,
                    Number::Auto,
                    name.trim_end(),
                    "Temperature sensor",
                    RO_INT,
                    "IK",
                    int_query(get, None),
                );
            }
        }
        if inner.floor.is_some()
            && let Ok(node) = t.add_node(
                dev,
                Number::Auto,
                "fanpolicy",
                "Fan floor policy: raises the minimum from temperature",
            )
        {
            nodes.push(node);
            floor_sysctls(t, node, inner);
        }
        if inner.limit.is_some()
            && let Ok(node) = t.add_node(
                dev,
                Number::Auto,
                "plimit",
                "Platform power limit the SMC raises under load",
            )
        {
            nodes.push(node);
            let leaves: [LimitLeaf; 4] = [
                (
                    "active",
                    "The limit is on (GPUs are asked to run at their lowest level)",
                    |l| i32::from(l.state.lock().active()),
                ),
                ("level", "MSLT, 0..100", |l| {
                    i32::try_from(l.state.lock().level).unwrap_or(0)
                }),
                ("gpu", "MSLG, the GPU limit step", |l| {
                    i32::try_from(l.gpu_step.load(Ordering::Relaxed)).unwrap_or(0)
                }),
                ("cpu", "MSTc, the CPU limit step", |l| {
                    i32::try_from(l.cpu_step.load(Ordering::Relaxed)).unwrap_or(0)
                }),
            ];
            for (name, descr, f) in leaves {
                let i = Arc::clone(inner);
                let get = move || Ok(i.limit.as_ref().map_or(0, f));
                let _ = t.add_query(
                    node,
                    Number::Auto,
                    name,
                    descr,
                    RO_INT,
                    "I",
                    int_query(get, None),
                );
            }
        }
    });
}

/// The fan floor of a driver that has one.
fn floor_of(i: &Inner) -> &FanFloor {
    i.floor.as_ref().expect("floor sysctls only with a floor")
}

/// The fan floor's sysctls (the fork's `dev.asmc.N.fanpolicy`).
fn floor_sysctls(t: &mut sysctl::Tree, node: NodeId, inner: &Arc<Inner>) {
    let (g, w) = (Arc::clone(inner), Arc::clone(inner));
    let _ = t.add_query(
        node,
        Number::Auto,
        "enabled",
        "Run the policy (0 restores the firmware minimum)",
        RW_INT,
        "I",
        int_query(
            move || Ok(i32::from(floor_of(&g).enabled.load(Ordering::Acquire))),
            Some(Arc::new(move |v| {
                let f = floor_of(&w);
                match v {
                    0 => {
                        if f.enabled.swap(false, Ordering::AcqRel) {
                            w.floor_restore(f);
                        }
                    }
                    1 => {
                        if !f.enabled.swap(true, Ordering::AcqRel) {
                            f.floor.store(u32::MAX, Ordering::Release);
                            w.wake.wake_all();
                        }
                    }
                    _ => return Err(Errno::EINVAL),
                }
                Ok(())
            })),
        ),
    );
    let g = Arc::clone(inner);
    let _ = t.add_query(
        node,
        Number::Auto,
        "floor",
        "Last minimum written, rpm",
        RO_INT,
        "I",
        int_query(
            move || match floor_of(&g).floor.load(Ordering::Acquire) {
                u32::MAX => Ok(-1),
                n => i32::try_from(n).map_err(|_| Errno::ERANGE),
            },
            None,
        ),
    );
    // The curve's settings: degrees C, or rpm for fwmin and top.
    type Get = fn(&Floor) -> i32;
    type Put = fn(&mut Floor, i32) -> Result<(), Errno>;
    let params: [(&str, &str, Get, Option<Put>); 6] = [
        (
            "fwmin",
            "Firmware minimum, rpm",
            |f| i32::try_from(f.fwmin).unwrap_or(0),
            None,
        ),
        (
            "gpu_start",
            "GPU die C where the floor rises",
            |f| f.gpu.start,
            Some(|f, v| {
                f.gpu.start = in_degrees(v)?;
                Ok(())
            }),
        ),
        (
            "gpu_full",
            "GPU die C for the top floor",
            |f| f.gpu.full,
            Some(|f, v| {
                f.gpu.full = in_degrees(v)?;
                Ok(())
            }),
        ),
        (
            "cpu_start",
            "CPU C where the floor rises",
            |f| f.cpu.start,
            Some(|f, v| {
                f.cpu.start = in_degrees(v)?;
                Ok(())
            }),
        ),
        (
            "cpu_full",
            "CPU C for the top floor",
            |f| f.cpu.full,
            Some(|f, v| {
                f.cpu.full = in_degrees(v)?;
                Ok(())
            }),
        ),
        (
            "top",
            "Highest floor written, rpm",
            |f| i32::try_from(f.top).unwrap_or(0),
            Some(|f, v| {
                let v = u32::try_from(v).map_err(|_| Errno::EINVAL)?;
                if v < f.fwmin || v > 4000 {
                    return Err(Errno::EINVAL);
                }
                f.top = v;
                Ok(())
            }),
        ),
    ];
    for (name, descr, get, put) in params {
        let g = Arc::clone(inner);
        let w = Arc::clone(inner);
        let set: Option<SetFn> = put
            .map(|put| Arc::new(move |v: i32| put(&mut floor_of(&w).settings.lock(), v)) as SetFn);
        let kind = if put.is_some() { RW_INT } else { RO_INT };
        let _ = t.add_query(
            node,
            Number::Auto,
            name,
            descr,
            kind,
            "I",
            int_query(move || Ok(get(&floor_of(&g).settings.lock())), set),
        );
    }
}

/// A curve point: 20 to 110 °C, as the fork accepts.
fn in_degrees(v: i32) -> Result<i32, Errno> {
    if (20..=110).contains(&v) {
        Ok(v)
    } else {
        Err(Errno::EINVAL)
    }
}

/// Fan `n`'s manual mode: its own `F<n>Md` key where there is one (T2
/// Macs), else its bit of `FS! ` (`asmc_mb_sysctl_fanmanual`).
fn manual(inner: &Inner, n: u8) -> Result<i32, Errno> {
    let md = fan_key(b"F%Md", n);
    let mut b = [0u8; 2];
    if inner.smc.info(md).is_ok_and(|i| i.len == 1) {
        inner.smc.read(md, &mut b[..1]).map_err(|_| Errno::EIO)?;
        return Ok(i32::from(b[0] != 0));
    }
    inner.smc.read(*b"FS! ", &mut b).map_err(|_| Errno::EIO)?;
    Ok(i32::from(u16::from_be_bytes(b) & (1 << n) != 0))
}

fn set_manual(inner: &Inner, n: u8, v: i32) -> Result<(), Errno> {
    let on = match v {
        0 => false,
        1 => true,
        _ => return Err(Errno::EINVAL),
    };
    let md = fan_key(b"F%Md", n);
    if inner.smc.info(md).is_ok_and(|i| i.len == 1) {
        return inner.smc.write(md, &[u8::from(on)]).map_err(|_| Errno::EIO);
    }
    // One transaction's read and the next's write: FreeBSD's too.
    let mut b = [0u8; 2];
    inner.smc.read(*b"FS! ", &mut b).map_err(|_| Errno::EIO)?;
    let mut mask = u16::from_be_bytes(b);
    if on {
        mask |= 1 << n;
    } else {
        mask &= !(1 << n);
    }
    inner
        .smc
        .write(*b"FS! ", &mask.to_be_bytes())
        .map_err(|_| Errno::EIO)
}

/// The declaration for the kernel's driver list.
pub const DRIVER: DriverDecl = DriverDecl::of::<Asmc>();
