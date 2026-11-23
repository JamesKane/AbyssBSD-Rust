// SPDX-License-Identifier: BSD-2-Clause
//! `amdtemp`: the on-die temperature sensors of AMD Family 17h, 19h and 1Ah
//! processors (Zen and later), a child of the host bridge that reads them
//! through the System Management Network ([`amdsmn`]).
//!
//! - `dev.amdtemp.N.core0.sensor0`: Tctl, the temperature the processor
//!   reports for fan control (deciKelvin, the `IK` format, as `sysctl`
//!   prints it in Celsius).
//! - `dev.amdtemp.N.ccdM`: the temperature of each core complex die
//!   (Tccd) that has a sensor.
//! - `dev.cpu.C.temperature`: Tctl again, on each CPU of the package, once
//!   the CPUs have attached (an intrhook, as FreeBSD's).
//! - `dev.amdtemp.N.sensor_offset`: whole degrees added to every reading
//!   (some processors' Tctl is offset from the real die temperature).
//!
//! Older families, whose sensors are in PCI configuration space of other
//! functions, are not supported.
//!
//! Behavior reference: sys/dev/amdtemp/amdtemp.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype};
use amdsmn::Smn;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use pci::hostb::HostbChild;
use sync::SpinLock;
use sysctl::{NodeId, Number};

/// Zero degrees Celsius in deciKelvin (`AMDTEMP_ZERO_C_TO_K`).
pub const ZERO_C_TO_K: i32 = 2731;
/// The range adjustment in tenths of a degree: 49.0 °C
/// (`AMDTEMP_CURTMP_RANGE_ADJUST`).
const RANGE_ADJUST: i32 = 490;
/// The temperature field of the reported-temperature registers.
const CURTMP_MASK: u32 = 0x7ff;
/// Where Tctl's field starts in `THM_TCON_CUR_TMP`.
const CURTMP_SHIFT: u32 = 21;

/// `SMU::THM::THM_TCON_CUR_TMP`: Tctl.
pub const CUR_TMP: u32 = 0x0005_9800;
/// Set: the range is -49 to 206 °C (clear: 0 to 255).
const CUR_TMP_RANGE_SEL: u32 = 1 << 19;
/// Both set: the field is read-write, and the 49 degree offset applies.
const CUR_TMP_TJ_SEL: u32 = (1 << 17) | (1 << 16);
/// The first per-CCD register of Zen 1-3, and Zen 4's Genoa and Raphael
/// bases (the registers follow each other, four bytes apart).
const CCD_BASE_ZEN: u32 = 0x0005_9954;
const CCD_BASE_ZEN4_GENOA: u32 = 0x0005_9b00;
const CCD_BASE_ZEN4: u32 = 0x0005_9b08;
/// A per-CCD register holds a reading.
const CCD_VALID: u32 = 1 << 11;

/// A temperature in deciKelvin from a register field in units of 0.125 °C
/// (`amdtemp_decode_fam10h_to_17h`): `minus49` if the range starts at
/// -49 °C, and `offset` whole degrees added.
#[must_use]
pub fn decode_field(offset: i32, field: u32, minus49: bool) -> i32 {
    let mut t = i32::try_from((field & CURTMP_MASK) * 5 / 4).unwrap_or(i32::MAX);
    if minus49 {
        t -= RANGE_ADJUST;
    }
    t + ZERO_C_TO_K + offset * 10
}

/// Tctl from `THM_TCON_CUR_TMP` (`amdtemp_decode_fam17h_tctl`).
#[must_use]
pub fn decode_tctl(offset: i32, reg: u32) -> i32 {
    let minus49 = reg & CUR_TMP_RANGE_SEL != 0 || reg & CUR_TMP_TJ_SEL == CUR_TMP_TJ_SEL;
    decode_field(offset, reg >> CURTMP_SHIFT, minus49)
}

/// A core complex die's temperature from its register, if it has a reading
/// (`amdtemp_decode_fam10h_to_17h` with the -49 range).
#[must_use]
pub fn decode_ccd(offset: i32, reg: u32) -> Option<i32> {
    (reg & CCD_VALID != 0).then(|| decode_field(offset, reg, true))
}

/// The per-CCD registers of a CPU: the first one's address and how many
/// to try (`amdtemp_probe_ccd_sensors17h/19h/1ah`). `None` for a model
/// FreeBSD does not recognise.
#[must_use]
pub const fn ccd_registers(family: u32, model: u32) -> Option<(u32, u32)> {
    match (family, model) {
        // Zen, Zen+.
        (0x17, 0x00..=0x2f) => Some((CCD_BASE_ZEN, 4)),
        // Zen 2: Castle Peak, Rome, Renoir, Matisse, Van Gogh.
        (0x17, 0x30..=0x3f | 0x60..=0x7f | 0x90..=0x9f) => Some((CCD_BASE_ZEN, 8)),
        // Zen 3: Milan, Vermeer, Cezanne.
        (0x19, 0x00..=0x0f | 0x20..=0x2f | 0x50..=0x5f) => Some((CCD_BASE_ZEN, 8)),
        // Zen 4 Genoa.
        (0x19, 0x10..=0x1f) => Some((CCD_BASE_ZEN4_GENOA, 12)),
        // Rembrandt, Raphael, Phoenix.
        (0x19, 0x40..=0x4f | 0x60..=0x7f) => Some((CCD_BASE_ZEN4, 8)),
        // Zen 5: Turin, Granite Ridge.
        (0x1a, 0x00..=0x2f | 0x40..=0x4f | 0x60..=0x7f) => Some((CCD_BASE_ZEN4, 8)),
        _ => None,
    }
}

/// The CPU's model (CPUID 1, with the extended model from family 0Fh on).
fn cpu_model() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        let eax = core::arch::x86_64::__cpuid(1).eax;
        ((eax >> 4) & 0xf) | ((eax >> 12) & 0xf0)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// How many cores the package has (CPUID 8000_0008h ECX[7:0] + 1 when
/// the CMP bit of 8000_0001h says so).
fn cores_per_package() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        let ext = core::arch::x86_64::__cpuid(0x8000_0000).eax;
        if ext >= 0x8000_0008 && core::arch::x86_64::__cpuid(0x8000_0001).ecx & (1 << 1) != 0 {
            return (core::arch::x86_64::__cpuid(0x8000_0008).ecx & 0xff) + 1;
        }
    }
    1
}

/// What the sensors read from.
struct Sensors {
    smn: Arc<Smn>,
    offset: AtomicI32,
    /// The package's CPUs, which get `dev.cpu.N.temperature`.
    ncores: u32,
    /// What the driver added to the sysctl tree under the CPUs.
    cpu_nodes: SpinLock<Vec<NodeId>>,
}

impl Sensors {
    fn offset(&self) -> i32 {
        self.offset.load(Ordering::Relaxed)
    }

    fn tctl(&self) -> Result<i32, Errno> {
        let reg = self.smn.read(CUR_TMP).ok_or(Errno::EIO)?;
        Ok(decode_tctl(self.offset(), reg))
    }

    fn ccd(&self, addr: u32) -> Result<i32, Errno> {
        let reg = self.smn.read(addr).ok_or(Errno::EIO)?;
        decode_ccd(self.offset(), reg).ok_or(Errno::ENXIO)
    }
}

/// A read-only deciKelvin sysctl leaf (`CTLTYPE_INT | CTLFLAG_RD`, `IK`).
fn temperature(f: impl Fn() -> Result<i32, Errno> + Send + Sync + 'static) -> sysctl::QueryFn {
    Arc::new(move |written, out| {
        if written.is_some() {
            return Err(Errno::EPERM);
        }
        out.extend_from_slice(&f()?.to_ne_bytes());
        Ok(())
    })
}

const KIND_TEMP: u32 = ctltype::INT | ctlflag::RD | ctlflag::MPSAFE;

/// `amdtempN`.
#[derive(Debug)]
pub struct AmdTemp {
    sensors: Arc<Sensors>,
    unit: u32,
    family: u32,
    model: u32,
}

impl core::fmt::Debug for Sensors {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sensors")
            .field("ncores", &self.ncores)
            .finish_non_exhaustive()
    }
}

impl Driver for AmdTemp {
    const NAME: &'static str = "amdtemp";
    const IDS: Ids = Ids::Nexus(&["amdtemp"]);

    fn describe(_: &Ident) -> String {
        alloc::format!(
            "AMD Family {:02X}h CPU On-Die Thermal Sensors",
            amdsmn::cpu_family()
        )
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let host = cx
            .bus_data_arc::<HostbChild>()
            .ok_or(AttachError::Decline)?;
        let id = host.bridge.function.header.ident;
        if !amdsmn::has_smn(id.vendor, id.device) {
            return Err(AttachError::Decline);
        }
        let family = amdsmn::cpu_family();
        if !matches!(family, 0x17 | 0x19 | 0x1a) {
            return Err(AttachError::Decline);
        }
        let smn =
            amdsmn::find(&host.bridge).ok_or(AttachError::Device("No amdsmn(4) device found"))?;
        let ncores = cores_per_package();
        Ok(Self {
            sensors: Arc::new(Sensors {
                smn,
                offset: AtomicI32::new(0),
                ncores,
                cpu_nodes: SpinLock::new(Vec::new()),
            }),
            unit: cx.unit(),
            family,
            model: cpu_model(),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let s = cx.sleepable();
        let unit = self.unit;
        let sensors = Arc::clone(&self.sensors);
        let (family, model) = (self.family, self.model);
        bus::manager().sysctl(s, &mut |t| {
            let Some(node) = bus::device_node(t, Self::NAME, unit) else {
                return;
            };
            let sn = Arc::clone(&sensors);
            let _ = t.add_query(
                node,
                Number::Auto,
                "sensor_offset",
                "Temperature sensor offset",
                ctltype::INT | ctlflag::RW | ctlflag::MPSAFE,
                "I",
                Arc::new(move |written, out| {
                    if let Some(w) = written {
                        let w: [u8; 4] = w.try_into().map_err(|_| Errno::EINVAL)?;
                        sn.offset.store(i32::from_ne_bytes(w), Ordering::Relaxed);
                    }
                    out.extend_from_slice(&sn.offset().to_ne_bytes());
                    Ok(())
                }),
            );
            if let Ok(core0) = t.add_node(node, Number::Auto, "core0", "Core 0") {
                let sn = Arc::clone(&sensors);
                let _ = t.add_query(
                    core0,
                    Number::Auto,
                    "sensor0",
                    "Core 0 / Sensor 0 temperature",
                    KIND_TEMP,
                    "IK",
                    temperature(move || sn.tctl()),
                );
            }
            // The core complex dies that have a sensor.
            if let Some((base, count)) = ccd_registers(family, model) {
                for i in 0..count {
                    let addr = base + i * 4;
                    if sensors
                        .smn
                        .read(addr)
                        .is_none_or(|r| decode_ccd(0, r).is_none())
                    {
                        continue;
                    }
                    let sn = Arc::clone(&sensors);
                    let name = alloc::format!("ccd{i}");
                    let descr = alloc::format!("CCD {i} temperature (Tccd{i})");
                    let _ = t.add_query(
                        node,
                        Number::Auto,
                        &name,
                        &descr,
                        KIND_TEMP,
                        "IK",
                        temperature(move || sn.ccd(addr)),
                    );
                }
            }
        });
        // `dev.cpu.N.temperature`, once the CPUs have attached
        // (`amdtemp_intrhook`).
        let sn = Arc::clone(&self.sensors);
        bus::config_intrhook_oneshot(s, "amdtemp", move |s| {
            let sn2 = Arc::clone(&sn);
            bus::manager().sysctl(s, &mut |t| {
                for cpu in 0..sn2.ncores {
                    let Some(node) = bus::device_node(t, "cpu", cpu) else {
                        continue;
                    };
                    let sn3 = Arc::clone(&sn2);
                    if let Ok(id) = t.add_query(
                        node,
                        Number::Auto,
                        "temperature",
                        "Current temperature",
                        KIND_TEMP,
                        "IK",
                        temperature(move || sn3.tctl()),
                    ) {
                        sn2.cpu_nodes.lock().push(id);
                    }
                }
            });
        });
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let nodes = core::mem::take(&mut *self.sensors.cpu_nodes.lock());
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            for &n in &nodes {
                t.remove(n);
            }
        });
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<AmdTemp>();

#[cfg(test)]
mod tests {
    use super::*;

    /// Deci-Kelvin to tenths of a degree Celsius.
    fn c10(dk: i32) -> i32 {
        dk - ZERO_C_TO_K
    }

    /// What the X870E's Ryzen 9 7900X read when probed from userland
    /// (2026-11-23), idle: Tctl, then the two core complex dies.
    #[test]
    fn the_registers_the_board_gave() {
        // 0x5ffb0000: field 767 (95.9 C), the -49 range selected: 46.9 C.
        assert_eq!(c10(decode_tctl(0, 0x5ffb_0000)), 468);
        // The dies, with the valid bit: 42.3 C and 38.1 C.
        assert_eq!(c10(decode_ccd(0, 0x0adb).unwrap()), 423);
        assert_eq!(c10(decode_ccd(0, 0x0ab9).unwrap()), 381);
        // An absent die has no reading.
        assert_eq!(decode_ccd(0, 0), None);
    }

    #[test]
    fn the_range_and_the_offset() {
        // Range select clear and no read-write select: 0 to 255 C, no
        // adjustment.
        assert_eq!(c10(decode_tctl(0, 0x2ff << 21)), 958);
        // The read-write select alone (both bits) also takes the 49 C off.
        assert_eq!(c10(decode_tctl(0, 0x2ff << 21 | 0x3 << 16)), 468);
        // One bit of the pair is not enough.
        assert_eq!(c10(decode_tctl(0, 0x2ff << 21 | 1 << 16)), 958);
        // The offset is whole degrees.
        assert_eq!(
            decode_tctl(-10, 0x2ff << 21 | 1 << 19),
            decode_tctl(0, 0x2ff << 21 | 1 << 19) - 100
        );
    }

    #[test]
    fn the_per_die_registers_by_model() {
        // Raphael: the Zen 4 base, eight dies.
        assert_eq!(ccd_registers(0x19, 0x61), Some((0x59b08, 8)));
        assert_eq!(ccd_registers(0x19, 0x11), Some((0x59b00, 12)));
        assert_eq!(ccd_registers(0x19, 0x21), Some((0x59954, 8)));
        assert_eq!(ccd_registers(0x17, 0x71), Some((0x59954, 8)));
        assert_eq!(ccd_registers(0x1a, 0x44), Some((0x59b08, 8)));
        // A model FreeBSD does not recognise has none.
        assert_eq!(ccd_registers(0x19, 0x30), None);
        assert_eq!(ccd_registers(0x17, 0x40), None);
    }
}
