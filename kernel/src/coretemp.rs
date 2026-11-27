// SPDX-License-Identifier: BSD-2-Clause
//! `coretemp`: the on-die digital thermal sensor of Intel processors, a
//! child of each `cpu` device whose processor has one (CPUID 6 EAX bit 0).
//!
//! - `dev.cpu.N.temperature`: the core's temperature (deciKelvin, `IK`),
//!   TjMax less the sensor's distance below it.
//! - `dev.cpu.N.coretemp.delta`: that distance in degrees; `.resolution`:
//!   the sensor's resolution in degrees; `.tjmax`: the TCC activation
//!   temperature (`IK`).
//! - `dev.cpu.N.coretemp.throttle_log`: 1 once the sensor has tripped the
//!   thermal monitor; writing 0 clears it.
//!
//! A reading whose valid bit is clear is -1. A critical-temperature status
//! is logged and sent to `devd` (`system=coretemp subsystem=Thermal`,
//! `notify=0xcc`), as FreeBSD's, so a rule can shut the machine down.
//!
//! TjMax is 100 °C unless the model says otherwise: Core 2's undocumented
//! `MSR_IA32_EXT_CONFIG` bit for 85, two Penryn and Atom steppings, and
//! from then on `IA32_TEMPERATURE_TARGET` when it reads 70 to 110. The Mac
//! Pro (Late 2013)'s Xeon E5-2697 v2 reads 99 there (docs/MACPRO.md).
//!
//! Behavior reference: sys/dev/coretemp/coretemp.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype};
use acpi::cpu::CpuChild;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use hal::CpuId;
use sync::SpinLock;
use sysctl::{NodeId, Number};

use crate::arch::fixup::{rdmsr_safe, wrmsr_safe};

/// `MSR_THERM_STATUS` (`IA32_THERM_STATUS`).
const MSR_THERM_STATUS: u32 = 0x19c;
/// `MSR_IA32_TEMPERATURE_TARGET`: TjMax in bits 23:16.
const MSR_TEMPERATURE_TARGET: u32 = 0x1a2;
/// `MSR_IA32_EXT_CONFIG`: undocumented, Core Solo/Duo and Core 2 only.
const MSR_EXT_CONFIG: u32 = 0x0ee;

/// Zero degrees Celsius in deciKelvin (`TZ_ZEROC`).
const ZERO_C_TO_K: i32 = 2731;

/// `IA32_THERM_STATUS` fields.
mod status {
    /// The thermal monitor has tripped since the bit was cleared.
    pub(super) const LOG: u64 = 0x02;
    /// The critical temperature is being reached.
    pub(super) const CRITICAL: u64 = 0x10;
    pub(super) const TEMP_SHIFT: u32 = 16;
    pub(super) const TEMP_MASK: u64 = 0x7f;
    pub(super) const RES_SHIFT: u32 = 27;
    pub(super) const RES_MASK: u64 = 0x0f;
    pub(super) const VALID: u64 = 1 << 31;
}

/// What a sysctl reads from the status register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reading {
    Temperature,
    Delta,
    Resolution,
    TjMax,
}

/// The value of `what` from status register `msr` for a core whose TjMax
/// is `tjmax` (`coretemp_get_val_sysctl`): -1 when the reading is invalid.
fn value(msr: u64, tjmax: i32, what: Reading) -> i32 {
    if msr & status::VALID == 0 {
        return -1;
    }
    let field = |shift: u32, mask: u64| i32::try_from((msr >> shift) & mask).unwrap_or(0);
    match what {
        Reading::Temperature => {
            (tjmax - field(status::TEMP_SHIFT, status::TEMP_MASK)) * 10 + ZERO_C_TO_K
        }
        Reading::Delta => field(status::TEMP_SHIFT, status::TEMP_MASK),
        Reading::Resolution => field(status::RES_SHIFT, status::RES_MASK),
        Reading::TjMax => tjmax * 10 + ZERO_C_TO_K,
    }
}

/// Where TjMax comes from, by model and stepping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TjMaxSource {
    /// Too old to have the sensor (`cpu_model < 0xe`): do not attach.
    NoSensor,
    /// 100, or 85 if `MSR_IA32_EXT_CONFIG` bit 30 is set.
    ExtConfig,
    /// A fixed value.
    Fixed(i32),
    /// `IA32_TEMPERATURE_TARGET`, if it reads 70 to 110; else 100.
    TemperatureTarget,
}

/// `coretemp_attach`'s choice for display model `model` and `stepping`.
fn tjmax_source(model: u32, stepping: u32) -> TjMaxSource {
    match (model, stepping) {
        (m, _) if m < 0xe => TjMaxSource::NoSensor,
        (0xf, s) if s >= 2 => TjMaxSource::ExtConfig,
        (0xe, _) => TjMaxSource::ExtConfig,
        // Mobile Core 2 Duo.
        (0x17, 0x6) => TjMaxSource::Fixed(105),
        (0x17, _) => TjMaxSource::Fixed(100),
        // 45 nm Atom D400, N400 and D500.
        (0x1c, 0xa) => TjMaxSource::Fixed(100),
        (0x1c, _) => TjMaxSource::Fixed(90),
        _ => TjMaxSource::TemperatureTarget,
    }
}

/// TjMax from `IA32_TEMPERATURE_TARGET`, if within FreeBSD's "reasonable"
/// 70 to 110 °C (Intel white paper 322683).
fn tjmax_from_target(msr: u64) -> Option<i32> {
    let t = i32::try_from((msr >> 16) & 0xff).unwrap_or(0);
    (70..=110).contains(&t).then_some(t)
}

/// The processor's display family, model and stepping (`CPUID_TO_MODEL`:
/// the extended model counts for families 6 and 15).
fn signature() -> (u32, u32, u32) {
    let eax = core::arch::x86_64::__cpuid(1).eax;
    let family = (eax >> 8) & 0xf;
    let mut model = (eax >> 4) & 0xf;
    if family == 0x6 || family == 0xf {
        model |= ((eax >> 16) & 0xf) << 4;
    }
    (family, model, eax & 0xf)
}

/// Whether this is an Intel processor with a digital thermal sensor
/// (`coretemp_identify`: CPUID 6 present, EAX bit 0).
fn has_sensor() -> bool {
    let id = core::arch::x86_64::__cpuid(0);
    let intel = (id.ebx, id.edx, id.ecx) == (0x756e_6547, 0x4965_6e69, 0x6c65_746e);
    intel && id.eax >= 6 && core::arch::x86_64::__cpuid(6).eax & 1 != 0
}

/// Runs `f` on `cpu`, which the thread is bound to meanwhile.
fn on_cpu<R>(cpu: u32, f: impl FnOnce() -> R) -> R {
    let was = sched::bound_cpu();
    sched::move_to(Some(CpuId(cpu)));
    let r = f();
    sched::move_to(was);
    r
}

/// One core's sensor.
struct Sensor {
    cpu: u32,
    unit: u32,
    tjmax: i32,
    /// `sc_throttle_log`: the sensor has tripped since it was cleared.
    throttled: AtomicBool,
    /// The sysctl nodes added under `dev.cpu.N`.
    nodes: SpinLock<Vec<NodeId>>,
}

impl Sensor {
    /// `IA32_THERM_STATUS`, read on the core.
    fn status(&self) -> Result<u64, Errno> {
        // SAFETY: the status MSR exists on a processor with the sensor
        // (CPUID 6); a read has no side effects.
        on_cpu(self.cpu, || unsafe { rdmsr_safe(MSR_THERM_STATUS) }).map_err(|_| Errno::EIO)
    }

    /// Clears the status register's log bits (`coretemp_clear_thermal_msr`
    /// writes 0, which the read-write-0-to-clear bits take).
    fn clear(&self) {
        // SAFETY: as in `status`; writing 0 clears only the sticky log bits.
        let _ = on_cpu(self.cpu, || unsafe { wrmsr_safe(MSR_THERM_STATUS, 0) });
    }

    /// Notes a trip in `msr` (and clears it), and reports a critical
    /// temperature, as both of FreeBSD's handlers do.
    fn note(&self, msr: u64) {
        if msr & status::LOG == 0 {
            return;
        }
        self.clear();
        self.throttled.store(true, Ordering::Release);
        if msr & status::CRITICAL != 0 {
            let temp = (self.tjmax
                - i32::try_from((msr >> status::TEMP_SHIFT) & status::TEMP_MASK).unwrap_or(0))
                * 10
                + ZERO_C_TO_K;
            let m = bus::manager();
            m.log(format_args!(
                "coretemp{}: critical temperature detected, suggest system shutdown",
                self.unit
            ));
            m.notify(
                "coretemp",
                "Thermal",
                &alloc::format!("{temp}"),
                Some("notify=0xcc"),
            );
        }
    }

    /// A sysctl read of `what`.
    fn read(&self, what: Reading) -> Result<i32, Errno> {
        let msr = self.status()?;
        let v = value(msr, self.tjmax, what);
        self.note(msr);
        Ok(v)
    }
}

/// `coretempN`.
pub(crate) struct CoreTemp {
    sensor: Arc<Sensor>,
}

impl core::fmt::Debug for CoreTemp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CoreTemp")
            .field("cpu", &self.sensor.cpu)
            .field("tjmax", &self.sensor.tjmax)
            .finish_non_exhaustive()
    }
}

const KIND_RO: u32 = ctltype::INT | ctlflag::RD | ctlflag::MPSAFE;
const KIND_RW: u32 = ctltype::INT | ctlflag::RW | ctlflag::MPSAFE;

/// A read-only integer sysctl of `what`.
fn reader(sensor: &Arc<Sensor>, what: Reading) -> sysctl::QueryFn {
    let s = Arc::clone(sensor);
    Arc::new(move |written, out| {
        if written.is_some() {
            return Err(Errno::EPERM);
        }
        out.extend_from_slice(&s.read(what)?.to_ne_bytes());
        Ok(())
    })
}

/// `coretemp_throttle_log_sysctl`: reads the trip flag; writing 0 clears
/// it, anything else is `EINVAL`.
fn throttle_log(sensor: &Arc<Sensor>) -> sysctl::QueryFn {
    let s = Arc::clone(sensor);
    Arc::new(move |written, out| {
        s.note(s.status()?);
        let v = i32::from(s.throttled.load(Ordering::Acquire));
        out.extend_from_slice(&v.to_ne_bytes());
        if let Some(w) = written {
            let w: [u8; 4] = w.try_into().map_err(|_| Errno::EINVAL)?;
            if i32::from_ne_bytes(w) != 0 {
                return Err(Errno::EINVAL);
            }
            s.clear();
            s.throttled.store(false, Ordering::Release);
        }
        Ok(())
    })
}

impl Driver for CoreTemp {
    const NAME: &'static str = "coretemp";
    const IDS: Ids = Ids::Nexus(&["coretemp"]);

    fn describe(_: &Ident) -> String {
        "CPU On-Die Thermal Sensors".into()
    }

    /// Its CPU's unit (`device_add_child(parent, "coretemp",
    /// device_get_unit(parent))`).
    fn unit(_: &Ident, bus_data: Option<&(dyn core::any::Any + Send + Sync)>) -> Option<u32> {
        bus_data?.downcast_ref::<CpuChild>().map(|c| c.cpu)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<CpuChild>().ok_or(AttachError::Decline)?;
        if !has_sensor() || crate::boot::value("hint.coretemp.0.disabled") == Some("1") {
            return Err(AttachError::Decline);
        }
        let (_, model, stepping) = signature();
        let cpu = child.cpu;
        let unit = cx.unit();
        let log = |a: core::fmt::Arguments<'_>| bus::manager().log(a);
        let tjmax = match tjmax_source(model, stepping) {
            TjMaxSource::NoSensor => return Err(AttachError::Decline),
            TjMaxSource::Fixed(t) => t,
            TjMaxSource::ExtConfig => {
                // SAFETY: the Core 2's configuration MSR, read only; a
                // processor without it faults, which the read survives.
                match on_cpu(cpu, || unsafe { rdmsr_safe(MSR_EXT_CONFIG) }) {
                    Ok(m) if m & (1 << 30) != 0 => 85,
                    _ => 100,
                }
            }
            TjMaxSource::TemperatureTarget => {
                // SAFETY: a read of the TCC activation target; a processor
                // without it faults, which the read survives.
                match on_cpu(cpu, || unsafe { rdmsr_safe(MSR_TEMPERATURE_TARGET) }) {
                    Ok(m) => tjmax_from_target(m).unwrap_or_else(|| {
                        log(format_args!(
                            "coretemp{unit}: Tj(target) value {} does not seem right.",
                            (m >> 16) & 0xff
                        ));
                        100
                    }),
                    Err(_) => {
                        log(format_args!(
                            "coretemp{unit}: Can not get Tj(target) from your CPU, using 100C."
                        ));
                        100
                    }
                }
            }
        };
        Ok(Self {
            sensor: Arc::new(Sensor {
                cpu,
                unit,
                tjmax,
                throttled: AtomicBool::new(false),
                nodes: SpinLock::new(Vec::new()),
            }),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let sensor = &self.sensor;
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            let Some(cpu) = bus::device_node(t, "cpu", sensor.cpu) else {
                return;
            };
            let mut nodes = sensor.nodes.lock();
            if let Ok(id) = t.add_query(
                cpu,
                Number::Auto,
                "temperature",
                "Current temperature",
                KIND_RO,
                "IK",
                reader(sensor, Reading::Temperature),
            ) {
                nodes.push(id);
            }
            let Ok(node) = t.add_node(cpu, Number::Auto, "coretemp", "Per-CPU thermal information")
            else {
                return;
            };
            nodes.push(node);
            let leaves: [(&str, &str, u32, &'static str, sysctl::QueryFn); 4] = [
                (
                    "delta",
                    "Delta between TCC activation and current temperature",
                    KIND_RO,
                    "I",
                    reader(sensor, Reading::Delta),
                ),
                (
                    "resolution",
                    "Resolution of CPU thermal sensor",
                    KIND_RO,
                    "I",
                    reader(sensor, Reading::Resolution),
                ),
                (
                    "tjmax",
                    "TCC activation temperature",
                    KIND_RO,
                    "IK",
                    reader(sensor, Reading::TjMax),
                ),
                (
                    "throttle_log",
                    "Set to 1 if the thermal sensor has tripped",
                    KIND_RW,
                    "I",
                    throttle_log(sensor),
                ),
            ];
            for (name, descr, kind, format, query) in leaves {
                let _ = t.add_query(node, Number::Auto, name, descr, kind, format, query);
            }
        });
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let nodes = core::mem::take(&mut *self.sensor.nodes.lock());
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            // The `coretemp` node's leaves go with it.
            for &n in nodes.iter().rev() {
                t.remove(n);
            }
        });
    }
}

/// The declaration for the kernel's driver list.
pub(crate) const DRIVER: DriverDecl = DriverDecl::of::<CoreTemp>();

#[cfg(feature = "ktest")]
mod ktests {
    use ktest::ktest;

    use super::*;

    ktest! {
        fn coretemp_decodes_the_status_register() {
            // Valid, 54 below a TjMax of 99, resolution 1: the Mac Pro's
            // idle reading under FreeBSD (45.0C, delta 54).
            let msr = status::VALID | (1 << status::RES_SHIFT) | (54 << status::TEMP_SHIFT);
            assert_eq!(value(msr, 99, Reading::Temperature), 450 + ZERO_C_TO_K);
            assert_eq!(value(msr, 99, Reading::Delta), 54);
            assert_eq!(value(msr, 99, Reading::Resolution), 1);
            assert_eq!(value(msr, 99, Reading::TjMax), 990 + ZERO_C_TO_K);
            assert_eq!(value(msr & !status::VALID, 99, Reading::Temperature), -1);
        }
    }

    ktest! {
        fn coretemp_picks_tjmax_as_freebsd() {
            assert_eq!(tjmax_source(0xd, 0), TjMaxSource::NoSensor);
            assert_eq!(tjmax_source(0xe, 3), TjMaxSource::ExtConfig);
            assert_eq!(tjmax_source(0xf, 2), TjMaxSource::ExtConfig);
            assert_eq!(tjmax_source(0xf, 1), TjMaxSource::TemperatureTarget);
            assert_eq!(tjmax_source(0x17, 6), TjMaxSource::Fixed(105));
            assert_eq!(tjmax_source(0x1c, 0xa), TjMaxSource::Fixed(100));
            assert_eq!(tjmax_source(0x1c, 2), TjMaxSource::Fixed(90));
            // Ivy Bridge-EP (the Mac Pro's E5-2697 v2): the target MSR,
            // which read 0x00631400 there.
            assert_eq!(tjmax_source(0x3e, 4), TjMaxSource::TemperatureTarget);
            assert_eq!(tjmax_from_target(0x0063_1400), Some(99));
            assert_eq!(tjmax_from_target(0x0040_0000), None);
            assert_eq!(tjmax_from_target(0x006f_0000), None);
        }
    }
}
