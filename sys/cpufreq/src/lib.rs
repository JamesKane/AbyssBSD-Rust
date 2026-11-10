// SPDX-License-Identifier: BSD-2-Clause
//! cpufreq(4): CPU frequency levels, and who may set them.
//!
//! A frequency driver ([`FreqDriver`], such as `acpi_cppc`) knows a clock
//! domain's settings and how to switch between them. It registers with
//! [`register`], which adds a `cpufreqN` device under the CPU it drives
//! (unit N is the CPU's), and that device publishes, as FreeBSD's does:
//!
//! - `dev.cpu.N.freq`: the current frequency in MHz. Writing it sets every
//!   `cpufreq` device to its level nearest the value, at user priority
//!   (`cpufreq_curr_sysctl`: all CPUs, for the MP systems that need it).
//!   `powerd(8)` drives the system through `dev.cpu.0.freq`.
//! - `dev.cpu.N.freq_levels`: the levels, `freq/power` pairs, highest
//!   first.
//! - `dev.cpufreq.N.freq_driver`: the driver behind it.
//!
//! A level is set at a priority ([`PRIO_KERN`] for passive cooling, above
//! [`PRIO_USER`]): a request below the current priority fails with
//! `EPERM`, and one above it saves the current level for [`Target::Restore`]
//! to return to (`cf_set_method`'s saved levels).
//!
//! Only absolute drivers exist here, one per device, so a device's levels
//! are its driver's settings; FreeBSD's relative drivers (throttling, as
//! `p4tcc`) and their combinations are not implemented.
//!
//! Behavior reference: sys/kern/kern_cpu.c.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use abi::errno::Errno;
use abi::sysctl::ctlflag;
use bus::{AttachCx, AttachError, DeviceId, Driver, DriverDecl, Ident, Ids, StopCx, StopReason};
use devtree::graph::EdgeKind;
use hal::CpuId;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::SpinLock;
use sysctl::{NodeId, Number, Tree, Value};

/// Priorities (`sys/cpu.h`).
pub const PRIO_HIGHEST: i32 = 1_000_000;
/// The kernel's: passive cooling.
pub const PRIO_KERN: i32 = 1000;
/// Userland's: `dev.cpu.N.freq`.
pub const PRIO_USER: i32 = 100;
/// The lowest, which every device starts at.
pub const PRIO_LOWEST: i32 = 0;
/// A value a driver does not know (`CPUFREQ_VAL_UNKNOWN`).
pub const VAL_UNKNOWN: i32 = -1;

/// One setting a driver offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting {
    /// The frequency, in MHz.
    pub freq: i32,
    /// The power, in mW, or [`VAL_UNKNOWN`].
    pub power: i32,
}

/// A frequency driver: one clock domain's settings, and the switch
/// between them (`cpufreq_drv_*`).
pub trait FreqDriver: Send + Sync + 'static {
    /// The settings, highest frequency first.
    fn settings(&self) -> Vec<Setting>;
    /// The current frequency in MHz, if the driver knows it.
    fn get(&self) -> Option<i32>;
    /// Switches to the setting of `freq` MHz, one of [`Self::settings`].
    /// Called on the domain's CPU, in thread context.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a frequency it does not offer, or the hardware's
    /// failure.
    fn set(&self, freq: i32) -> Result<(), Errno>;
}

/// What a request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The level of this frequency (MHz).
    Level(i32),
    /// The level saved when a higher priority took over (`level == NULL`).
    Restore,
}

/// A device's current level and priority, and the levels saved below it:
/// `cf_set_method`'s rules, apart from the hardware.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    current: Option<i32>,
    priority: i32,
    saved: Vec<(i32, i32)>,
}

impl Policy {
    /// The level set last, if known.
    #[must_use]
    pub fn current(&self) -> Option<i32> {
        self.current
    }

    /// The priority it was set at.
    #[must_use]
    pub fn priority(&self) -> i32 {
        self.priority
    }

    /// Applies a request at `priority`, switching the hardware with
    /// `switch` unless it is at that level already. Levels below `lowest`
    /// MHz are refused (`debug.cpufreq.lowest`).
    ///
    /// # Errors
    ///
    /// `EPERM` below the current priority, `ENXIO` to restore with nothing
    /// saved, `EINVAL` below `lowest`, and `switch`'s error, which leaves
    /// the policy as it was.
    pub fn apply(
        &mut self,
        target: Target,
        priority: i32,
        lowest: i32,
        switch: impl FnOnce(i32) -> Result<(), Errno>,
    ) -> Result<(), Errno> {
        if priority < self.priority {
            return Err(Errno::EPERM);
        }
        let (freq, priority, restoring) = match target {
            Target::Level(f) => (f, priority, false),
            Target::Restore => {
                let &(f, p) = self.saved.last().ok_or(Errno::ENXIO)?;
                (f, p, true)
            }
        };
        if freq < lowest {
            return Err(Errno::EINVAL);
        }
        if self.current != Some(freq) {
            switch(freq)?;
        }
        if let Some(c) = self.current
            && priority > self.priority
        {
            self.saved.push((c, self.priority));
        }
        self.current = Some(freq);
        self.priority = priority;
        if restoring {
            // FreeBSD forgets the level it restored ("unused"): the next
            // read asks the driver.
            self.current = None;
            self.saved.pop();
        }
        Ok(())
    }
}

/// The setting of `settings` nearest `freq` MHz (the first of two equally
/// near), as `cpufreq_curr_sysctl` picks one per device.
#[must_use]
pub fn nearest(settings: &[Setting], freq: i32) -> Option<i32> {
    let mut best: Option<(i32, i32)> = None;
    for s in settings {
        let d = (s.freq - freq).abs();
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((s.freq, d));
        }
    }
    best.map(|(f, _)| f)
}

/// The `freq/power` list of `dev.cpu.N.freq_levels` and a driver's
/// `freq_settings`: `0` when there are none.
#[must_use]
pub fn levels_string(settings: &[Setting]) -> String {
    let mut s = String::new();
    for (i, l) in settings.iter().enumerate() {
        let _ = write!(s, "{}{}/{}", if i > 0 { " " } else { "" }, l.freq, l.power);
    }
    if s.is_empty() {
        s.push('0');
    }
    s
}

static POLICY: LockClass = LockClass::new("cpufreq", 300);

/// One CPU's frequency control: `cpufreqN`.
pub struct Cpufreq {
    cpu: u32,
    driver_name: String,
    driver: Arc<dyn FreqDriver>,
    levels: Vec<Setting>,
    policy: Mutex<Policy>,
    /// The policy's level, or [`VAL_UNKNOWN`], for readers that may not
    /// sleep (sysctl leaves).
    current: AtomicI32,
}

impl core::fmt::Debug for Cpufreq {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cpufreq")
            .field("cpu", &self.cpu)
            .field("driver", &self.driver_name)
            .finish_non_exhaustive()
    }
}

impl Cpufreq {
    /// The CPU it is under.
    #[must_use]
    pub fn cpu(&self) -> u32 {
        self.cpu
    }

    /// Its levels, highest first.
    #[must_use]
    pub fn levels(&self) -> &[Setting] {
        &self.levels
    }

    /// The current frequency in MHz (`CPUFREQ_GET`): the level set last,
    /// else what the driver reports.
    #[must_use]
    pub fn get(&self) -> Option<i32> {
        match self.current.load(Ordering::Acquire) {
            VAL_UNKNOWN => self.driver.get(),
            f => Some(f),
        }
    }

    /// Sets a level at `priority` (`CPUFREQ_SET`): the driver switches on
    /// the domain's CPU, as FreeBSD binds to it.
    ///
    /// # Errors
    ///
    /// As [`Policy::apply`].
    pub fn set(&self, s: &Sleepable, target: Target, priority: i32) -> Result<(), Errno> {
        if let Target::Level(f) = target
            && !self.levels.iter().any(|l| l.freq == f)
        {
            return Err(Errno::EINVAL);
        }
        let mut p = self.policy.lock(s);
        let r = p.apply(target, priority, LOWEST.load(Ordering::Relaxed), |freq| {
            let was = sched::bound_cpu();
            sched::move_to(Some(CpuId(self.cpu)));
            let r = self.driver.set(freq);
            sched::move_to(was);
            r
        });
        self.current
            .store(p.current().unwrap_or(VAL_UNKNOWN), Ordering::Release);
        r
    }
}

/// `debug.cpufreq.lowest`: levels below it are refused.
static LOWEST: AtomicI32 = AtomicI32::new(0);
/// `debug.cpufreq.verbose`.
static VERBOSE: AtomicI32 = AtomicI32::new(0);
static GLOBALS: AtomicBool = AtomicBool::new(false);

/// The attached devices.
static DEVICES: SpinLock<Vec<Arc<Cpufreq>>> = SpinLock::new(Vec::new());

/// The device of CPU `cpu`, if one is attached (`device_find_child(cpu,
/// "cpufreq")`).
#[must_use]
pub fn find(cpu: u32) -> Option<Arc<Cpufreq>> {
    DEVICES.lock().iter().find(|c| c.cpu == cpu).cloned()
}

/// Every attached device.
#[must_use]
pub fn all() -> Vec<Arc<Cpufreq>> {
    DEVICES.lock().clone()
}

/// Sets every device to its level nearest `freq` MHz at `priority`, as a
/// write of `dev.cpu.N.freq` does. Returns the last error.
///
/// # Errors
///
/// As [`Cpufreq::set`].
pub fn set_all(s: &Sleepable, freq: i32, priority: i32) -> Result<(), Errno> {
    let mut r = Ok(());
    for c in all() {
        if let Some(f) = nearest(c.levels(), freq) {
            r = c.set(s, Target::Level(f), priority);
        }
    }
    r
}

/// Registers `driver`, the frequency driver `driver_name` (`acpi_cppc0`)
/// of the device `provider`, for the CPU `cpu`, whose device is
/// `cpu_device` (`cpufreq_register`). Adds `cpufreqN` under the CPU, which
/// attaches once the provider has.
pub fn register(
    s: &Sleepable,
    cpu_device: DeviceId,
    provider: DeviceId,
    cpu: u32,
    driver_name: String,
    driver: Arc<dyn FreqDriver>,
) -> DeviceId {
    let levels = driver.settings();
    let cf = Arc::new(Cpufreq {
        cpu,
        driver_name,
        driver,
        levels,
        policy: Mutex::new(&POLICY, Policy::default()),
        current: AtomicI32::new(VAL_UNKNOWN),
    });
    bus::manager().add_child(
        s,
        cpu_device,
        Ident::Child {
            class: "cpufreq",
            desc: "CPU frequency control",
        },
        Some(cf),
        Vec::new(),
        &[(provider, EdgeKind::Required)],
    )
}

/// Adds `freq_settings` under the driver's own node `dev.<class>.<unit>`:
/// its settings, as `dev.cpu.N.freq_levels` lists levels.
pub fn add_settings_sysctl(s: &Sleepable, class: &str, unit: u32, settings: &[Setting]) {
    let text = levels_string(settings);
    bus::manager().sysctl(s, &mut |t| {
        if let Some(node) = bus::device_node(t, class, unit) {
            let text = text.clone();
            let _ = t.add_value(
                node,
                Number::Auto,
                "freq_settings",
                "CPU frequency driver settings",
                Value::Str(Box::new(move |out| out.push_str(&text))),
            );
        }
    });
}

/// `debug.cpufreq`, once.
fn add_globals(t: &mut Tree) {
    if GLOBALS.swap(true, Ordering::AcqRel) {
        return;
    }
    let root = t.root();
    let Some(debug) = t.child(root, "debug") else {
        return;
    };
    let Ok(node) = t.add_node(debug, Number::Auto, "cpufreq", "cpufreq debugging") else {
        return;
    };
    for (name, descr, value) in [
        (
            "lowest",
            "Don't provide levels below this frequency.",
            &LOWEST,
        ),
        ("verbose", "Print verbose debugging messages", &VERBOSE),
    ] {
        let _ = t.add_writable(
            node,
            Number::Auto,
            name,
            descr,
            ctlflag::TUN | ctlflag::MPSAFE,
            Value::Int(Box::new(move || value.load(Ordering::Relaxed))),
            sysctl::int_writer(move |v| {
                value.store(v, Ordering::Relaxed);
                Ok(())
            }),
        );
    }
}

/// `cpufreqN`.
pub struct CpufreqDevice {
    cf: Arc<Cpufreq>,
    nodes: Vec<NodeId>,
}

impl core::fmt::Debug for CpufreqDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.cf.fmt(f)
    }
}

impl Driver for CpufreqDevice {
    const NAME: &'static str = "cpufreq";
    const IDS: Ids = Ids::Nexus(&["cpufreq"]);

    fn describe(_: &Ident) -> String {
        "CPU frequency control".into()
    }

    /// Its CPU's unit (`BUS_ADD_CHILD(cpu_dev, 0, "cpufreq",
    /// device_get_unit(cpu_dev))`).
    fn unit(_: &Ident, bus_data: Option<&(dyn core::any::Any + Send + Sync)>) -> Option<u32> {
        bus_data?.downcast_ref::<Cpufreq>().map(|c| c.cpu)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let cf = cx.bus_data_arc::<Cpufreq>().ok_or(AttachError::Decline)?;
        let unit = cx.unit();
        let mut nodes = Vec::new();
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            add_globals(t);
            if let Some(cpu) = bus::device_node(t, "cpu", cf.cpu) {
                let rd = Arc::clone(&cf);
                if let Ok(n) = t.add_writable(
                    cpu,
                    Number::Auto,
                    "freq",
                    "Current CPU frequency",
                    ctlflag::MPSAFE,
                    Value::Int(Box::new(move || rd.get().unwrap_or(VAL_UNKNOWN))),
                    sysctl::int_writer(move |f| {
                        // SAFETY: sysctl writers run in thread context
                        // (the syscall path), after the kernel drops the
                        // tree's lock.
                        let s = unsafe { Sleepable::assume() };
                        set_all(&s, f, PRIO_USER)
                    }),
                ) {
                    nodes.push(n);
                }
                let text = levels_string(cf.levels());
                if let Ok(n) = t.add_value(
                    cpu,
                    Number::Auto,
                    "freq_levels",
                    "CPU frequency levels",
                    Value::Str(Box::new(move |out| out.push_str(&text))),
                ) {
                    nodes.push(n);
                }
            }
            if let Some(own) = bus::device_node(t, "cpufreq", unit) {
                let name = cf.driver_name.clone();
                let _ = t.add_value(
                    own,
                    Number::Auto,
                    "freq_driver",
                    "cpufreq driver used by this cpu",
                    Value::Str(Box::new(move |out| out.push_str(&name))),
                );
            }
        });
        DEVICES.lock().push(Arc::clone(&cf));
        Ok(Self { cf, nodes })
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        DEVICES.lock().retain(|c| !Arc::ptr_eq(c, &self.cf));
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            for &n in &self.nodes {
                t.remove(n);
            }
        });
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<CpufreqDevice>();

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn levels(f: &[i32]) -> Vec<Setting> {
        f.iter()
            .map(|&freq| Setting {
                freq,
                power: VAL_UNKNOWN,
            })
            .collect()
    }

    #[test]
    fn a_higher_priority_saves_the_level_and_restore_returns_to_it() {
        let mut p = Policy::default();
        let mut hw = Vec::new();
        let mut sw = |f| {
            hw.push(f);
            Ok(())
        };
        p.apply(Target::Level(2600), PRIO_USER, 0, &mut sw).unwrap();
        // Passive cooling steps down, at the kernel's priority.
        p.apply(Target::Level(2000), PRIO_KERN, 0, &mut sw).unwrap();
        assert_eq!((p.current(), p.priority()), (Some(2000), PRIO_KERN));
        // Userland may not override it meanwhile.
        assert_eq!(
            p.apply(Target::Level(2600), PRIO_USER, 0, &mut sw),
            Err(Errno::EPERM)
        );
        p.apply(Target::Level(1500), PRIO_KERN, 0, &mut sw).unwrap();
        // Restoring returns to userland's level and priority, forgetting
        // the level (the driver is asked next).
        p.apply(Target::Restore, PRIO_KERN, 0, &mut sw).unwrap();
        assert_eq!((p.current(), p.priority()), (None, PRIO_USER));
        assert_eq!(
            p.apply(Target::Restore, PRIO_USER, 0, &mut sw),
            Err(Errno::ENXIO)
        );
        assert_eq!(hw, [2600, 2000, 1500, 2600]);
    }

    #[test]
    fn the_same_level_is_not_switched_again_and_a_failure_changes_nothing() {
        let mut p = Policy::default();
        let mut n = 0;
        p.apply(Target::Level(800), PRIO_USER, 0, |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        p.apply(Target::Level(800), PRIO_USER, 0, |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 1);
        let before = p.clone();
        assert_eq!(
            p.apply(Target::Level(900), PRIO_KERN, 0, |_| Err(Errno::EIO)),
            Err(Errno::EIO)
        );
        assert_eq!(p, before);
        assert_eq!(
            p.apply(Target::Level(700), PRIO_USER, 750, |_| Ok(())),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn nearest_level_and_the_levels_string() {
        let l = levels(&[2600, 2500, 1800, 800]);
        assert_eq!(nearest(&l, 2550), Some(2600));
        assert_eq!(nearest(&l, 2000), Some(1800));
        assert_eq!(nearest(&l, 0), Some(800));
        assert_eq!(nearest(&[], 1000), None);
        assert_eq!(levels_string(&l[..2]), "2600/-1 2500/-1");
        assert_eq!(levels_string(&[]), "0");
    }
}
