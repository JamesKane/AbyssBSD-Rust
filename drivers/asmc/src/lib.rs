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

mod machine;
pub mod policy;
pub mod smc;
pub mod value;

#[cfg(test)]
mod tests;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

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

use machine::{FanFloor, LimitKeys, Machine, Props, fan_key, temperature_keys};
use policy::Floor;
use regs::Bytes;
use smc::{Clock, Key, MmioIo, PortIo, Smc};

/// The most fans (`ASMC_MAXFANS`).
const MAX_FANS: u8 = 6;

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
/// latched (it is across a warm reboot) learns of it. The state is read
/// under the listeners' lock, so no change falls between the two.
pub fn on_platform_limit(f: LimitFn) {
    let now = {
        let mut l = LISTENERS.lock();
        l.push(Arc::clone(&f));
        platform_limit()
    };
    f(now);
}

/// Removes a listener [`on_platform_limit`] registered.
pub fn off_platform_limit(f: &LimitFn) {
    LISTENERS.lock().retain(|g| !Arc::ptr_eq(g, f));
}

fn publish_limit(on: bool) {
    let listeners = {
        let l = LISTENERS.lock();
        LIMIT_ON.store(on, Ordering::Release);
        l.clone()
    };
    for f in listeners {
        f(on);
    }
}

type KernelMachine = Machine<Ports, Memory, BusClock>;

/// What the driver keeps: the SMC and its findings ([`Machine`]), and the
/// kernel's part.
struct Inner {
    m: KernelMachine,
    unit: u32,
    stopping: AtomicBool,
    wake: WaitQueue,
    worker: SpinLock<Option<Arc<sched::Thread>>>,
    nodes: SpinLock<Vec<NodeId>>,
}

impl core::ops::Deref for Inner {
    type Target = KernelMachine;
    fn deref(&self) -> &KernelMachine {
        &self.m
    }
}

impl Inner {
    fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("asmc{}: {args}", self.unit));
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

/// The device's properties, through `sys/acpi`.
struct CxProps<'a, 'b>(&'a AttachCx<'b>);

impl Props for CxProps<'_, '_> {
    fn int(&self, name: &'static str) -> Option<u64> {
        match acpi::properties::value(self.0, name)? {
            aml::Value::Integer(n) => Some(n),
            _ => None,
        }
    }

    fn ints(&self, name: &'static str) -> Option<Vec<u64>> {
        match acpi::properties::value(self.0, name)? {
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
        match acpi::properties::value(self.0, name)? {
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
        let mut m = Machine {
            smc,
            fans,
            temps,
            floor: None,
            limit: None,
        };
        let props = CxProps(cx);
        m.floor = m.fan_floor(&props).unwrap_or_else(|why| {
            log(format_args!("fan policy: {why}"));
            None
        });
        m.limit = m.limit_keys(&props).unwrap_or_else(|why| {
            log(format_args!("power limit: {why}"));
            None
        });
        if m.smc.retired() {
            log(format_args!(
                "the SMC stopped answering while probing; it is tried again every {} s",
                smc::BACKOFF_US / 1_000_000
            ));
        }
        let floor = &m.floor;
        if let Some(f) = &floor {
            let s = f.settings.lock();
            log(format_args!(
                "fan {} floor policy on (firmware minimum {} rpm)",
                f.fan, s.fwmin
            ));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                m,
                unit,
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
    let mut was_retired = false;
    while !stopping() {
        if let Some(f) = &inner.floor {
            inner.floor_tick(f);
        }
        if let Some(l) = &inner.limit
            && let Some(on) = inner.limit_tick(l)
        {
            publish_limit(on);
        }
        // Set aside, the SMC is tried again every BACKOFF_US: the fan keeps
        // what was last written meanwhile.
        let retired = inner.smc.retired();
        if retired != was_retired {
            if retired {
                inner.log(format_args!(
                    "the SMC stopped answering; trying again every {} s",
                    smc::BACKOFF_US / 1_000_000
                ));
            } else {
                inner.log(format_args!("the SMC answers again"));
            }
            was_retired = retired;
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
                        move || g.manual(n),
                        Some(Arc::new(move |v| w.set_manual(n, v))),
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
                let on = match v {
                    0 => false,
                    1 => true,
                    _ => return Err(Errno::EINVAL),
                };
                w.set_floor_enabled(floor_of(&w), on);
                w.wake.wake_all();
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

/// The declaration for the kernel's driver list.
pub const DRIVER: DriverDecl = DriverDecl::of::<Asmc>();
