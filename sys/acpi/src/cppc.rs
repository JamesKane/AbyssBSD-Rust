// SPDX-License-Identifier: BSD-2-Clause
//! `acpi_cppc`: CPU frequency through ACPI Collaborative Processor
//! Performance Control (`_CPC`, ACPI 6.5 §8.4.6), a cpufreq(4) driver.
//!
//! A processor's `_CPC` describes an abstract performance scale and the
//! registers through which the OS asks for a level and reads what it got.
//! Without `_PSS`, it is how firmware lets the OS choose CPU frequency, as
//! on many Arm servers and boards.
//!
//! One driver attaches per performance domain (`_PSD`), under the domain's
//! first processor, and offers levels 100 MHz apart, mapped onto the
//! performance scale through `_CPC`'s lowest and nominal frequencies.
//! Setting a level writes the desired performance register of each
//! processor of the domain. `dev.acpi_cppc.N.delivered_mhz` measures the
//! domain's frequency with the feedback counters.
//!
//! Desired performance registers in system memory are supported, and the
//! feedback counters in system memory or arm64's functional fixed
//! hardware (the activity monitors' core and constant cycles); PCC
//! registers are not yet. Autonomous selection is left as firmware set
//! it.
//!
//! Behavior reference: the AbyssBSD Project's FreeBSD
//! sys/dev/acpica/acpi_cppc.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype};
use aml::{Host, Interpreter, NodeId};
use bus::{
    AttachCx, AttachError, DeviceId, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use cpufreq::{FreqDriver, Setting, VAL_UNKNOWN};
use hal::CpuId;
use sysctl::Number;

use crate::Ops;
use crate::cpc::{self, Control, Register, space};
use crate::service::service;

/// Levels' spacing.
const STEP_MHZ: i64 = 100;
/// At most this many levels.
const MAX_LEVELS: usize = 64;
/// How long the delivered frequency is measured.
const MEASURE_US: u64 = 10_000;
/// Arm's functional fixed hardware feedback counters (as Linux defines
/// them for `_CPC`).
const FFH_CONST_CYCLES: u64 = 1;

/// What `cpu` gives the `acpi_cppc` it adds: the domain's processors.
#[derive(Debug)]
pub struct CppcChild {
    /// The device of the domain's first processor, the driver's parent.
    pub cpu_device: DeviceId,
    /// The domain's CPUs and their processor objects, the first first.
    pub cpus: Vec<(u32, NodeId)>,
}

/// The processors of `node`'s frequency domain, by CPU, lowest first, if
/// its frequency is `_CPC`'s to control: those whose `_PSD` names the
/// same domain, or `node`'s alone without one.
pub fn domain_cpus(
    interp: &mut Interpreter,
    host: &mut dyn Host,
    ops: &dyn Ops,
    node: NodeId,
) -> Vec<(u32, NodeId)> {
    if !cpc::controls_frequency(interp, node) {
        return Vec::new();
    }
    let procs: Vec<(u32, NodeId)> = cpc::processors(interp, host)
        .into_iter()
        .filter_map(|(n, uid)| Some((ops.cpu_of_uid(uid)?, n)))
        .collect();
    let Some(&me) = procs.iter().find(|p| p.1 == node) else {
        return Vec::new();
    };
    let Some(domain) = cpc::domain(interp, host, node) else {
        return alloc::vec![me];
    };
    let mut out: Vec<(u32, NodeId)> = procs
        .into_iter()
        .filter(|&(_, n)| {
            n == node
                || (cpc::controls_frequency(interp, n)
                    && cpc::domain(interp, host, n) == Some(domain))
        })
        .collect();
    out.sort_by_key(|p| p.0);
    out
}

/// A domain's performance scale and frequencies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    highest: i64,
    nominal: i64,
    lowest: i64,
    reference: i64,
    lowest_mhz: i64,
    nominal_mhz: i64,
}

impl Scale {
    /// The scale `_CPC` gives.
    #[must_use]
    pub fn of(c: &Control) -> Self {
        let i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        Self {
            highest: i(c.highest),
            nominal: i(c.nominal),
            lowest: i(c.lowest),
            reference: i(c.reference_perf),
            lowest_mhz: i(c.lowest_mhz),
            nominal_mhz: i(c.nominal_mhz),
        }
    }

    /// The frequency of performance `perf`, on the line through the
    /// lowest and nominal points.
    #[must_use]
    pub fn mhz(&self, perf: i64) -> i64 {
        if self.nominal == self.lowest {
            return perf * self.nominal_mhz / self.nominal;
        }
        self.lowest_mhz
            + (perf - self.lowest) * (self.nominal_mhz - self.lowest_mhz)
                / (self.nominal - self.lowest)
    }

    /// The performance of `mhz`, within the scale.
    #[must_use]
    pub fn perf(&self, mhz: i64) -> i64 {
        let perf = if self.nominal == self.lowest {
            mhz * self.nominal / self.nominal_mhz
        } else {
            self.lowest
                + (mhz - self.lowest_mhz) * (self.nominal - self.lowest)
                    / (self.nominal_mhz - self.lowest_mhz)
        };
        perf.clamp(self.lowest, self.highest)
    }

    /// The levels: 100 MHz apart from the highest down, then the lowest.
    #[must_use]
    pub fn levels(&self) -> Vec<i32> {
        let mut out = Vec::new();
        let mut mhz = self.mhz(self.highest);
        while mhz > self.lowest_mhz && out.len() < MAX_LEVELS - 1 {
            out.push(i32::try_from(mhz).unwrap_or(i32::MAX));
            mhz -= STEP_MHZ;
        }
        out.push(i32::try_from(self.lowest_mhz).unwrap_or(i32::MAX));
        out
    }
}

/// A desired performance register to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Desired {
    address: u64,
    bytes: u8,
}

/// A domain's frequency control.
struct Domain {
    ops: &'static dyn Ops,
    head: u32,
    scale: Scale,
    levels: Vec<i32>,
    /// One per distinct register.
    desired: Vec<Desired>,
    reference_ctr: Option<Register>,
    delivered_ctr: Option<Register>,
    current: AtomicI32,
}

impl Domain {
    /// Reads a feedback counter, on the current CPU for one in functional
    /// fixed hardware.
    fn counter(&self, r: &Register) -> Option<u64> {
        match r.space {
            space::MEMORY => self.ops.mem_read(r.address, r.memory_bytes()?),
            space::FFH => self.ops.ffh_read(r.address),
            _ => None,
        }
    }

    /// The domain's frequency as the feedback counters see it over 10 ms
    /// of its first CPU kept busy: the reference counter counts at the
    /// reference performance, the delivered one at the delivered
    /// performance. arm64's constant cycle counter counts at the system
    /// counter's rate, which gives the frequency directly (Sky1's
    /// reference performance, 1000, does not match it: 1 GHz is 3150).
    fn delivered_mhz(&self) -> Result<i32, Errno> {
        let (Some(rc), Some(dc)) = (self.reference_ctr, self.delivered_ctr) else {
            return Err(Errno::EOPNOTSUPP);
        };
        let was = sched::bound_cpu();
        sched::move_to(Some(CpuId(self.head)));
        let r0 = self.counter(&rc);
        let d0 = self.counter(&dc);
        // A busy wait: an idle CPU's delivered counter stops.
        self.ops.stall_us(MEASURE_US);
        let r1 = self.counter(&rc);
        let d1 = self.counter(&dc);
        sched::move_to(was);
        let (Some(r0), Some(d0), Some(r1), Some(d1)) = (r0, d0, r1, d1) else {
            return Err(Errno::EOPNOTSUPP);
        };
        let (dr, dd) = (
            u128::from(r1.wrapping_sub(r0)),
            u128::from(d1.wrapping_sub(d0)),
        );
        if dr == 0 {
            return Ok(0);
        }
        let mhz = match self.ops.ffh_constant_hz() {
            Some(hz) if rc.space == space::FFH && rc.address == FFH_CONST_CYCLES => {
                dd * u128::from(hz / 1000) / dr / 1000
            }
            _ => {
                let perf = dd * u128::try_from(self.scale.reference).unwrap_or(0) / dr;
                u128::try_from(self.scale.mhz(i64::try_from(perf).unwrap_or(i64::MAX))).unwrap_or(0)
            }
        };
        Ok(i32::try_from(mhz).unwrap_or(i32::MAX))
    }
}

impl FreqDriver for Domain {
    fn settings(&self) -> Vec<Setting> {
        self.levels
            .iter()
            .map(|&freq| Setting {
                freq,
                power: VAL_UNKNOWN,
            })
            .collect()
    }

    fn get(&self) -> Option<i32> {
        Some(self.current.load(Ordering::Acquire))
    }

    fn set(&self, freq: i32) -> Result<(), Errno> {
        if !self.levels.contains(&freq) {
            return Err(Errno::EINVAL);
        }
        let perf = u64::try_from(self.scale.perf(i64::from(freq))).unwrap_or(0);
        for d in &self.desired {
            self.ops
                .mem_write(d.address, d.bytes, perf)
                .ok_or(Errno::EIO)?;
        }
        self.current.store(freq, Ordering::Release);
        Ok(())
    }
}

/// `acpi_cppcN`.
pub struct AcpiCppc {
    domain: Arc<Domain>,
    cpu_device: DeviceId,
    unit: u32,
}

impl core::fmt::Debug for AcpiCppc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AcpiCppc")
            .field("head", &self.domain.head)
            .field("levels", &self.domain.levels.len())
            .finish_non_exhaustive()
    }
}

/// The distinct desired performance registers of a domain's `_CPC`s,
/// which must be whole aligned words of system memory.
fn desired_registers(controls: &[(u32, Control)]) -> Result<Vec<Desired>, u32> {
    let mut out: Vec<Desired> = Vec::new();
    for (cpu, c) in controls {
        let d = c
            .desired
            .and_then(|r| {
                Some(Desired {
                    address: r.address,
                    bytes: r.memory_bytes()?,
                })
            })
            .ok_or(*cpu)?;
        if !out.iter().any(|o| o.address == d.address) {
            out.push(d);
        }
    }
    Ok(out)
}

impl Driver for AcpiCppc {
    const NAME: &'static str = "acpi_cppc";
    const IDS: Ids = Ids::Nexus(&["acpi_cppc"]);

    fn describe(_: &Ident) -> String {
        "ACPI CPPC CPU frequency control".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let svc = service().ok_or(AttachError::Decline)?;
        let child = cx.bus_data_arc::<CppcChild>().ok_or(AttachError::Decline)?;
        let ops = svc.ops();
        let &(head, _) = child.cpus.first().ok_or(AttachError::Decline)?;
        let nodes = child.cpus.clone();
        let controls: Vec<(u32, Result<Control, cpc::ControlError>)> =
            svc.run(cx.sleepable(), move |i, h| {
                nodes
                    .iter()
                    .map(|&(cpu, n)| (cpu, cpc::control(i, h, ops, n)))
                    .collect()
            });
        let unit = cx.unit();
        let head_control = match controls.first() {
            Some((_, Ok(c))) => *c,
            Some((_, Err(cpc::ControlError::Inconsistent))) => {
                ops.log(format_args!("acpi_cppc{unit}: inconsistent _CPC bounds"));
                return Err(AttachError::Device("inconsistent _CPC bounds"));
            }
            _ => {
                ops.log(format_args!(
                    "acpi_cppc{unit}: _CPC without performance and frequency bounds \
                     (revision 3) is not supported"
                ));
                return Err(AttachError::Device("unsupported _CPC"));
            }
        };
        let readable: Vec<(u32, Control)> = controls
            .into_iter()
            .map(|(cpu, c)| (cpu, c.unwrap_or(head_control)))
            .collect();
        let desired = desired_registers(&readable).map_err(|cpu| {
            ops.log(format_args!(
                "acpi_cppc{unit}: CPU {cpu}: unsupported desired performance register"
            ));
            AttachError::Device("unsupported desired performance register")
        })?;
        let scale = Scale::of(&head_control);
        let levels = scale.levels();
        // The level firmware left, which also maps the register.
        let current = ops
            .mem_read(desired[0].address, desired[0].bytes)
            .filter(|&p| p != 0)
            .map_or(levels[0], |p| {
                i32::try_from(scale.mhz(i64::try_from(p).unwrap_or(i64::MAX))).unwrap_or(levels[0])
            });
        let counters_ok = |r: Option<Register>| {
            r.filter(|r| match r.space {
                space::MEMORY => r.memory_bytes().is_some(),
                space::FFH => true,
                _ => false,
            })
        };
        let (mut reference_ctr, mut delivered_ctr) = (
            counters_ok(head_control.reference_ctr),
            counters_ok(head_control.delivered_ctr),
        );
        if reference_ctr.is_none() || delivered_ctr.is_none() {
            (reference_ctr, delivered_ctr) = (None, None);
        }
        Ok(Self {
            domain: Arc::new(Domain {
                ops,
                head,
                scale,
                levels,
                desired,
                reference_ctr,
                delivered_ctr,
                current: AtomicI32::new(current),
            }),
            cpu_device: child.cpu_device,
            unit,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let unit = self.unit;
        let s = cx.sleepable();
        let d = Arc::clone(&self.domain);
        bus::manager().sysctl(s, &mut |t| {
            let Some(node) = bus::device_node(t, Self::NAME, unit) else {
                return;
            };
            let d = Arc::clone(&d);
            let _ = t.add_query(
                node,
                Number::Auto,
                "delivered_mhz",
                "Frequency the feedback counters measure (MHz)",
                ctltype::INT | ctlflag::RD | ctlflag::MPSAFE,
                "I",
                Arc::new(move |written, out| {
                    // Read-only, as a CTLFLAG_RD node is.
                    if written.is_some() {
                        return Err(Errno::EPERM);
                    }
                    out.extend_from_slice(&d.delivered_mhz()?.to_ne_bytes());
                    Ok(())
                }),
            );
        });
        cpufreq::add_settings_sysctl(s, Self::NAME, unit, &self.domain.settings());
        let driver: Arc<dyn FreqDriver> = self.domain.clone();
        cpufreq::register(
            s,
            self.cpu_device,
            cx.node(),
            self.domain.head,
            alloc::format!("{}{unit}", Self::NAME),
            driver,
        );
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const CPPC: DriverDecl = DriverDecl::of::<AcpiCppc>();

#[cfg(test)]
mod tests {
    use super::*;

    /// Sky1's big cores (`\_SB.C000.C001.CPU1`).
    fn sky1() -> Scale {
        Scale {
            highest: 8192,
            nominal: 8192,
            lowest: 2520,
            reference: 1000,
            lowest_mhz: 800,
            nominal_mhz: 2600,
        }
    }

    #[test]
    fn sky1_levels_and_their_performance() {
        let s = sky1();
        let l = s.levels();
        // As FreeBSD's acpi_cppc0 on the board: 2600 down to 800.
        assert_eq!(l.len(), 19);
        assert_eq!((l[0], l[1], l[17], l[18]), (2600, 2500, 900, 800));
        assert_eq!(s.perf(2600), 8192);
        assert_eq!(s.perf(800), 2520);
        assert_eq!(s.mhz(s.perf(1800)), 1799);
        // Out of range clamps to the scale.
        assert_eq!(s.perf(5000), 8192);
        assert_eq!(s.perf(100), 2520);
    }

    #[test]
    fn a_flat_scale_is_proportional() {
        let s = Scale {
            highest: 300,
            nominal: 200,
            lowest: 200,
            reference: 200,
            lowest_mhz: 2000,
            nominal_mhz: 2000,
        };
        assert_eq!(s.mhz(300), 3000);
        assert_eq!(s.perf(2500), 250);
        assert_eq!(
            s.levels(),
            [
                3000, 2900, 2800, 2700, 2600, 2500, 2400, 2300, 2200, 2100, 2000
            ]
        );
    }
}
