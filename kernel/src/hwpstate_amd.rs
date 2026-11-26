// SPDX-License-Identifier: BSD-2-Clause
//! `hwpstate_amd`: AMD CPU performance control through CPPC
//! (Collaborative Processor Performance Control), a child of each `cpu`
//! device on a processor that has it (Zen 2 and later).
//!
//! Attaching turns CPPC on in the core (`CPPC_ENABLE`, which cannot be
//! turned off again) and programs its request register as FreeBSD does: a
//! minimum and maximum performance at the core's lowest and highest, the
//! most performant energy preference, and the highest performance
//! desired, so a core runs as flat out as it did before CPPC. Four
//! per-CPU knobs then change that:
//!
//! - `dev.hwpstate_amd.N.epp`: the efficiency/performance preference, from
//!   0 (most performant) to 255 (most efficient).
//! - `dev.hwpstate_amd.N.minimum_performance` and `.maximum_performance`:
//!   the bounds, 0 to 255 on the core's own scale.
//! - `dev.hwpstate_amd.N.desired_performance`: 0 hands the choice to the
//!   hardware (autonomous mode), anything else asks for that level.
//!
//! With `machdep.hwpstate_pkg_ctrl` (on by default) a write applies to
//! every core, not only the one whose knob was written.
//!
//! The driver is information only to `cpufreq`: `dev.cpu.N.freq` is the
//! clock rate measured on the core with the performance counters, and
//! cannot be set.
//!
//! Tunables: `machdep.hwpstate_amd_cppc_enable=0` leaves CPPC off.
//!
//! Behavior reference: sys/x86/cpufreq/hwpstate_amd.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype};
use acpi::cpu::CpuChild;
use bus::{
    AttachCx, AttachError, DeviceId, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use cpufreq::amd_cppc::{self as cppc, request};
use cpufreq::{FreqDriver, Setting};
use hal::CpuId;
use sync::SpinLock;
use sysctl::Number;

use crate::arch::cpu::{enable_interrupts, rdmsr, save_disable_interrupts};
use crate::arch::fixup::{rdmsr_safe, wrmsr_safe};

/// How long the clock rate is measured, with the core kept busy.
const MEASURE_US: u64 = 1000;

/// One core: the request register as the driver last knew it.
struct Core {
    cpu: u32,
    /// `CPPC_REQUEST`, once read or written (`HWPFL_CPPC_REQUEST_NOT_READ`
    /// until then).
    request: SpinLock<Option<u64>>,
}

/// The cores under control, for the package-wide knobs.
static CORES: SpinLock<Vec<Arc<Core>>> = SpinLock::new(Vec::new());

/// `machdep.hwpstate_pkg_ctrl`, as the first attach read it: 1 on, 0 off.
static PKG_CTRL: AtomicU32 = AtomicU32::new(u32::MAX);

/// Runs `f` on `cpu`, which the thread is bound to meanwhile.
fn on_cpu<R>(cpu: u32, f: impl FnOnce() -> R) -> R {
    let was = sched::bound_cpu();
    sched::move_to(Some(CpuId(cpu)));
    let r = f();
    sched::move_to(was);
    r
}

/// A boolean tunable, `default` if unset.
fn tunable(name: &str, default: bool) -> bool {
    crate::boot::value(name).map_or(default, |v| !matches!(v, "0" | "no" | "off" | "false"))
}

/// Whether this is an AMD (or Hygon) processor with CPPC
/// (`AMDFEID_CPPC`: CPUID 8000_0008h EBX bit 27).
fn has_cppc() -> bool {
    let id = core::arch::x86_64::__cpuid(0);
    let amd = (id.ebx, id.edx, id.ecx) == (0x6874_7541, 0x6974_6e65, 0x444d_4163);
    let hygon = (id.ebx, id.edx, id.ecx) == (0x6f67_7948, 0x6e65_476e, 0x656e_6975);
    let ext = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    (amd || hygon)
        && ext >= 0x8000_0008
        && core::arch::x86_64::__cpuid(0x8000_0008).ebx & (1 << 27) != 0
}

/// Whether `MPERF` and `APERF` exist (CPUID 6 ECX bit 0).
fn has_perf_counters() -> bool {
    core::arch::x86_64::__cpuid(0).eax >= 6 && core::arch::x86_64::__cpuid(6).ecx & 1 != 0
}

/// What turning CPPC on found and did, on the CPU.
struct Enabled {
    caps: Option<u64>,
    held: Option<u64>,
    wrote: Option<u64>,
}

/// Turns CPPC on and programs the request (`enable_cppc_cb`). `None` if
/// the enable did not take (the register faulted).
fn enable(core: &Core) -> Option<Enabled> {
    // SAFETY: CPPC's own registers, which CPUID said exist; the writes
    // change only what the core's performance controller does.
    unsafe { wrmsr_safe(cppc::MSR_ENABLE, 1) }.ok()?;
    // SAFETY: as above, a read.
    let caps = unsafe { rdmsr_safe(cppc::MSR_CAPS_1) }.ok();
    // SAFETY: as above, a read.
    let held = unsafe { rdmsr_safe(cppc::MSR_REQUEST) }.ok();
    let mut out = Enabled {
        caps,
        held,
        wrote: None,
    };
    let Some(h) = held else { return Some(out) };
    let req = cppc::initial_request(h, caps);
    // SAFETY: as above.
    if unsafe { wrmsr_safe(cppc::MSR_REQUEST, req) }.is_ok() {
        *core.request.lock() = Some(req);
        out.wrote = Some(req);
    }
    Some(out)
}

/// The request register as the driver knows it, reading it from the core
/// if it has not (`get_cppc_request`). On the core.
fn request_now(core: &Core) -> Result<u64, Errno> {
    let mut r = core.request.lock();
    if let Some(v) = *r {
        return Ok(v);
    }
    // SAFETY: a read of CPPC's request register, which exists (CPUID).
    let v = unsafe { rdmsr_safe(cppc::MSR_REQUEST) }.map_err(|_| Errno::EIO)?;
    *r = Some(v);
    Ok(v)
}

/// Changes the request field `bits` to `value` on `core`
/// (`set_cppc_request_cb`). On the core.
fn set_field(core: &Core, bits: u64, value: u64) -> Result<(), Errno> {
    let held = request_now(core)?;
    let new = cppc::updated(held, bits, value).ok_or(Errno::EINVAL)?;
    // SAFETY: as in `enable`.
    unsafe { wrmsr_safe(cppc::MSR_REQUEST, new) }.map_err(|_| Errno::EOPNOTSUPP)?;
    *core.request.lock() = Some(new);
    Ok(())
}

/// The core's clock rate in MHz (`cpu_est_clockrate`): with interrupts
/// off, the reference and actual counters over a short busy interval,
/// scaled by the TSC's rate. On the core.
fn clock_mhz() -> Option<i32> {
    if !has_perf_counters() {
        return None;
    }
    let tsc_hz = crate::time::frequency();
    let irq = save_disable_interrupts();
    // SAFETY: MPERF and APERF exist (CPUID 6), and reading them has no
    // side effects.
    let (m0, a0) = unsafe { (rdmsr(cppc::MSR_MPERF), rdmsr(cppc::MSR_APERF)) };
    let start = crate::time::counter();
    let ticks = u128::from(MEASURE_US) * u128::from(tsc_hz) / 1_000_000;
    while u128::from(crate::time::counter().wrapping_sub(start)) < ticks {
        core::hint::spin_loop();
    }
    // SAFETY: as above.
    let (m1, a1) = unsafe { (rdmsr(cppc::MSR_MPERF), rdmsr(cppc::MSR_APERF)) };
    if irq {
        enable_interrupts();
    }
    let hz = cppc::clock_rate_hz(tsc_hz, m1.wrapping_sub(m0), a1.wrapping_sub(a0))?;
    i32::try_from(hz / 1_000_000).ok()
}

/// A core's frequency as `cpufreq` sees it: information only.
struct Info;

impl FreqDriver for Info {
    fn settings(&self) -> Vec<Setting> {
        Vec::new()
    }

    fn get(&self) -> Option<i32> {
        clock_mhz()
    }

    fn set(&self, _: i32) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }
}

/// `hwpstate_amdN`.
pub(crate) struct HwpstateAmd {
    core: Arc<Core>,
    cpu_device: DeviceId,
    unit: u32,
}

impl core::fmt::Debug for HwpstateAmd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HwpstateAmd")
            .field("cpu", &self.core.cpu)
            .finish_non_exhaustive()
    }
}

/// The four request knobs: name, field, description.
const KNOBS: [(&str, u64, &str); 4] = [
    (
        "epp",
        request::EPP,
        "Efficiency/Performance Preference (from 0, most performant, to 255, most efficient)",
    ),
    (
        "minimum_performance",
        request::MIN,
        "Minimum allowed performance level (from 0 to 255; should be smaller than \
         'maximum_performance'; effective range limited by CPU)",
    ),
    (
        "maximum_performance",
        request::MAX,
        "Maximum allowed performance level (from 0 to 255; should be larger than \
         'minimum_performance'; effective range limited by CPU)",
    ),
    (
        "desired_performance",
        request::DESIRED,
        "Desired performance level (from 0 to 255; 0 enables autonomous mode, otherwise \
         value should be between 'minimum_performance' and 'maximum_performance' inclusive)",
    ),
];

impl Driver for HwpstateAmd {
    const NAME: &'static str = "hwpstate_amd";
    const IDS: Ids = Ids::Nexus(&["hwpstate_amd"]);

    fn describe(_: &Ident) -> String {
        "AMD Collaborative Processor Performance Control (CPPC)".into()
    }

    /// Its CPU's unit (`BUS_ADD_CHILD(parent, 10, "hwpstate_amd",
    /// device_get_unit(parent))`).
    fn unit(_: &Ident, bus_data: Option<&(dyn core::any::Any + Send + Sync)>) -> Option<u32> {
        bus_data?.downcast_ref::<CpuChild>().map(|c| c.cpu)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<CpuChild>().ok_or(AttachError::Decline)?;
        if !has_cppc() || !tunable("machdep.hwpstate_amd_cppc_enable", true) {
            return Err(AttachError::Decline);
        }
        let _ = PKG_CTRL.compare_exchange(
            u32::MAX,
            u32::from(tunable("machdep.hwpstate_pkg_ctrl", true)),
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
        let unit = cx.unit();
        let cpu = child.cpu;
        let core = Arc::new(Core {
            cpu,
            request: SpinLock::new(None),
        });
        let log = |a: core::fmt::Arguments<'_>| bus::manager().log(a);
        let Some(done) = on_cpu(cpu, || enable(&core)) else {
            log(format_args!(
                "hwpstate_amd{unit}: CPU{cpu}: Failed to enable CPPC!"
            ));
            return Err(AttachError::Device("cannot enable CPPC"));
        };
        log(format_args!("hwpstate_amd{unit}: CPU{cpu}: CPPC enabled."));
        // CPPC cannot be turned off again, so attach even if the rest
        // failed; the knobs let the user retry.
        if done.held.is_none() {
            log(format_args!(
                "hwpstate_amd{unit}: CPU{cpu}: CPPC_REQUEST not read (fault or previous errors)"
            ));
        } else if done.wrote.is_none() {
            log(format_args!(
                "hwpstate_amd{unit}: CPU{cpu}: Could not write CPPC_REQUEST!"
            ));
        }
        if done.caps.is_none() {
            log(format_args!(
                "hwpstate_amd{unit}: CPU{cpu}: CPPC_CAPABILITY_1 not read (fault or previous errors)"
            ));
        }
        // Benchmark boots: `abyss.bench.pin_perf=N` holds every core at
        // performance level N (below the boost range), so a comparison with
        // stock FreeBSD's fixed P-state runs at one clock.
        if let Some(n) =
            crate::boot::value("abyss.bench.pin_perf").and_then(|v| v.parse::<u64>().ok())
        {
            let c = Arc::clone(&core);
            let done = on_cpu(cpu, move || {
                set_field(&c, request::MAX, n)
                    .and_then(|()| set_field(&c, request::DESIRED, n))
                    .and_then(|()| set_field(&c, request::MIN, n))
            });
            if done.is_err() {
                log(format_args!(
                    "hwpstate_amd{unit}: CPU{cpu}: could not pin performance to {n}"
                ));
            }
        }
        CORES.lock().push(Arc::clone(&core));
        Ok(Self {
            core,
            cpu_device: child.cpu_device,
            unit,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let unit = self.unit;
        let s = cx.sleepable();
        for (name, bits, desc) in KNOBS {
            let core = Arc::clone(&self.core);
            bus::manager().sysctl(s, &mut |t| {
                let Some(node) = bus::device_node(t, Self::NAME, unit) else {
                    return;
                };
                let core = Arc::clone(&core);
                let _ = t.add_query(
                    node,
                    Number::Auto,
                    name,
                    desc,
                    ctltype::UINT | ctlflag::RW | ctlflag::MPSAFE,
                    "IU",
                    Arc::new(move |written, out| {
                        // A read comes from the driver's copy, read from
                        // the core the first time.
                        let held = on_cpu(core.cpu, || request_now(&core))?;
                        let value = cppc::field(bits, held);
                        if let Some(w) = written {
                            let w: [u8; 4] = w.try_into().map_err(|_| Errno::EINVAL)?;
                            let new = u64::from(u32::from_ne_bytes(w));
                            if new > cppc::field(bits, u64::MAX) {
                                return Err(Errno::EINVAL);
                            }
                            apply(&core, bits, new)?;
                        }
                        out.extend_from_slice(&u32::try_from(value).unwrap_or(0).to_ne_bytes());
                        Ok(())
                    }),
                );
            });
        }
        let driver: Arc<dyn FreqDriver> = Arc::new(Info);
        cpufreq::register(
            s,
            self.cpu_device,
            cx.node(),
            self.core.cpu,
            alloc::format!("{}{unit}", Self::NAME),
            driver,
        );
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        CORES.lock().retain(|c| !Arc::ptr_eq(c, &self.core));
    }
}

/// Applies a write of `value` to the request field `bits`: to `core`, or
/// to every core under package-wide control (`set_cppc_request`). Errors
/// are kept, but every core is tried.
fn apply(core: &Arc<Core>, bits: u64, value: u64) -> Result<(), Errno> {
    let cores: Vec<Arc<Core>> = if PKG_CTRL.load(Ordering::Acquire) == 1 {
        CORES.lock().clone()
    } else {
        alloc::vec![Arc::clone(core)]
    };
    let mut result = Ok(());
    for c in cores {
        if let Err(e) = on_cpu(c.cpu, || set_field(&c, bits, value)) {
            // A read error comes before a write error.
            if result.is_ok() || e == Errno::EIO {
                result = Err(e);
            }
        }
    }
    result
}

/// The declaration for the kernel's driver list.
pub(crate) const DRIVER: DriverDecl = DriverDecl::of::<HwpstateAmd>();
