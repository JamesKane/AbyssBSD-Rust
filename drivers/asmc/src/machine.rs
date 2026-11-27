// SPDX-License-Identifier: BSD-2-Clause
//! What the driver does with the SMC, apart from the kernel: fan speeds by
//! each key's type, sensors, the fan floor's and the platform limit's
//! samples, manual mode, and reading their configuration from device
//! properties. Generic over the SMC's windows, so the host tests run it
//! against a fake SMC with the Mac Pro (Late 2013)'s keys.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use abi::errno::Errno;
use sync::SpinLock;

use crate::policy::{Curve, Floor, Limit, Reading};
use crate::smc::{Clock, Key, MmioIo, PortIo, Smc, SmcError};
use crate::value;

/// The most temperature sensors (`ASMC_TEMP_MAX`).
pub(crate) const MAX_TEMPS: usize = 80;

/// A key name for fan `n` from a pattern with `%` where the digit goes
/// (`"F%Mn"`).
pub(crate) fn fan_key(pattern: &[u8; 4], n: u8) -> Key {
    pattern.map(|b| if b == b'%' { b'0' + n } else { b })
}

/// Device properties, as the driver reads them (`_DSD`, then the
/// platform's: `sys/acpi::properties`).
pub(crate) trait Props {
    /// An integer.
    fn int(&self, name: &'static str) -> Option<u64>;
    /// A package of integers.
    fn ints(&self, name: &'static str) -> Option<Vec<u64>>;
    /// A package of four-character strings, as SMC keys.
    fn keys(&self, name: &'static str) -> Option<Vec<Key>>;
}

/// The fan floor's configuration and state.
pub(crate) struct FanFloor {
    pub(crate) fan: u8,
    pub(crate) settings: SpinLock<Floor>,
    pub(crate) enabled: AtomicBool,
    pub(crate) gpu_keys: Vec<Key>,
    pub(crate) cpu_keys: Vec<Key>,
    /// The last floor written, rpm; `u32::MAX` before the first.
    pub(crate) floor: AtomicU32,
    /// Held across a sample and across turning the policy off, so a sample
    /// cannot write a raised floor after the firmware minimum is restored
    /// (the fork holds its `sx` lock the same way).
    pub(crate) busy: SpinLock<()>,
}

/// The platform limit's keys and state.
pub(crate) struct LimitKeys {
    pub(crate) level: Key,
    pub(crate) gpu: Key,
    pub(crate) cpu: Key,
    pub(crate) state: SpinLock<Limit>,
    pub(crate) gpu_step: AtomicU32,
    pub(crate) cpu_step: AtomicU32,
}

/// The SMC and what the driver found on it.
pub(crate) struct Machine<P, M, C> {
    pub(crate) smc: Smc<P, M, C>,
    pub(crate) fans: u8,
    pub(crate) temps: Vec<Key>,
    pub(crate) floor: Option<FanFloor>,
    pub(crate) limit: Option<LimitKeys>,
}

/// A curve from a two-integer range property.
fn curve(v: &[u64]) -> Option<Curve> {
    let [start, full] = v else { return None };
    Some(Curve {
        start: i32::try_from(*start).ok()?,
        full: i32::try_from(*full).ok()?,
    })
}

impl<P: PortIo, M: MmioIo, C: Clock> Machine<P, M, C> {
    /// A fan speed in rpm, by the key's own type.
    pub(crate) fn fan_speed(&self, key: Key) -> Result<u32, SmcError> {
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
    pub(crate) fn set_fan_speed(&self, key: Key, rpm: u32) -> Result<(), Errno> {
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
    pub(crate) fn sensor(&self, key: Key) -> Option<i32> {
        let info = self.smc.info(key).ok()?;
        if info.len != 2 {
            return None;
        }
        let mut b = [0u8; 2];
        self.smc.read(key, &mut b).ok()?;
        value::sensor_milli(&info.ty, b)
    }

    /// One fan floor sample; writes the floor when it should.
    pub(crate) fn floor_tick(&self, f: &FanFloor) {
        let _busy = f.busy.lock();
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
        if write && self.set_fan_speed(fan_key(b"F%Mn", f.fan), next).is_ok() {
            f.floor.store(next, Ordering::Release);
        }
    }

    /// Gives the fan back its firmware minimum.
    pub(crate) fn floor_restore(&self, f: &FanFloor) {
        let fwmin = f.settings.lock().fwmin;
        if self.set_fan_speed(fan_key(b"F%Mn", f.fan), fwmin).is_ok() {
            f.floor.store(fwmin, Ordering::Release);
        }
    }

    /// Turns the floor policy on or off (`fanpolicy.enabled`): off gives
    /// the fan its firmware minimum back; on writes at the next sample.
    pub(crate) fn set_floor_enabled(&self, f: &FanFloor, on: bool) {
        let _busy = f.busy.lock();
        let was = f.enabled.swap(on, Ordering::AcqRel);
        if was && !on {
            self.floor_restore(f);
        } else if !was && on {
            f.floor.store(u32::MAX, Ordering::Release);
        }
    }

    /// One platform-limit sample: whether to publish, and the state. The
    /// limit is published on every sample while on (the fork's
    /// `421c666bda5a`), and on the change to off.
    pub(crate) fn limit_tick(&self, l: &LimitKeys) -> Option<bool> {
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
        let mut one = [0u8; 1];
        if self.smc.read(l.gpu, &mut one).is_ok() {
            l.gpu_step.store(u32::from(one[0]), Ordering::Relaxed);
        }
        if self.smc.read(l.cpu, &mut one).is_ok() {
            l.cpu_step.store(u32::from(one[0]), Ordering::Relaxed);
        }
        (on || changed).then_some(on)
    }

    /// Fan `n`'s manual mode: its own `F<n>Md` key where there is one (T2
    /// Macs), else its bit of `FS! ` (`asmc_mb_sysctl_fanmanual`).
    pub(crate) fn manual(&self, n: u8) -> Result<i32, Errno> {
        let md = fan_key(b"F%Md", n);
        let mut b = [0u8; 2];
        if self.smc.info(md).is_ok_and(|i| i.len == 1) {
            self.smc.read(md, &mut b[..1]).map_err(|_| Errno::EIO)?;
            return Ok(i32::from(b[0] != 0));
        }
        self.smc.read(*b"FS! ", &mut b).map_err(|_| Errno::EIO)?;
        Ok(i32::from(u16::from_be_bytes(b) & (1 << n) != 0))
    }

    /// Sets fan `n`'s manual mode (0 or 1).
    pub(crate) fn set_manual(&self, n: u8, v: i32) -> Result<(), Errno> {
        let on = match v {
            0 => false,
            1 => true,
            _ => return Err(Errno::EINVAL),
        };
        let md = fan_key(b"F%Md", n);
        if self.smc.info(md).is_ok_and(|i| i.len == 1) {
            return self.smc.write(md, &[u8::from(on)]).map_err(|_| Errno::EIO);
        }
        // One transaction's read and the next's write: FreeBSD's too.
        let mut b = [0u8; 2];
        self.smc.read(*b"FS! ", &mut b).map_err(|_| Errno::EIO)?;
        let mut mask = u16::from_be_bytes(b);
        if on {
            mask |= 1 << n;
        } else {
            mask &= !(1 << n);
        }
        self.smc
            .write(*b"FS! ", &mask.to_be_bytes())
            .map_err(|_| Errno::EIO)
    }

    /// The fan floor, if the properties give one and the fan's minimum key
    /// is the two-byte `fpe2` the policy writes.
    pub(crate) fn fan_floor(&self, props: &impl Props) -> Result<Option<FanFloor>, &'static str> {
        let Some(fan) = props.int("apple,fan-floor-fan") else {
            return Ok(None);
        };
        let fan = u8::try_from(fan)
            .ok()
            .filter(|&f| f < self.fans)
            .ok_or("no such fan")?;
        let fwmin = props
            .int("apple,fan-floor-firmware-minimum")
            .ok_or("no firmware minimum")?;
        let top = props.int("apple,fan-floor-top").ok_or("no top")?;
        let gpu = props
            .ints("apple,fan-floor-gpu-range")
            .as_deref()
            .and_then(curve)
            .ok_or("no GPU range")?;
        let cpu = props
            .ints("apple,fan-floor-cpu-range")
            .as_deref()
            .and_then(curve)
            .ok_or("no CPU range")?;
        let gpu_keys = props
            .keys("apple,fan-floor-gpu-keys")
            .ok_or("no GPU keys")?;
        let cpu_keys = props
            .keys("apple,fan-floor-cpu-keys")
            .ok_or("no CPU keys")?;
        let info = self
            .smc
            .info(fan_key(b"F%Mn", fan))
            .map_err(|_| "no minimum key")?;
        if info.len != 2 || &info.ty != b"fpe2" {
            return Err("the minimum key is not a two-byte fpe2");
        }
        let fwmin = u32::try_from(fwmin).map_err(|_| "firmware minimum")?;
        let top = u32::try_from(top).map_err(|_| "top")?;
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
            busy: SpinLock::new(()),
        }))
    }

    /// The platform limit, if the properties name its keys and the level
    /// key is a four-byte `flt `.
    pub(crate) fn limit_keys(&self, props: &impl Props) -> Result<Option<LimitKeys>, &'static str> {
        let Some(keys) = props.keys("apple,power-limit-keys") else {
            return Ok(None);
        };
        let [level, gpu, cpu] = keys[..] else {
            return Err("want three keys");
        };
        let info = self.smc.info(level).map_err(|_| "no level key")?;
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
}

/// The two-byte `sp78` keys from `T` to `U` (`asmc_detect_sensors`),
/// found by a binary search for each end over the key table.
pub(crate) fn temperature_keys<P: PortIo, M: MmioIo, C: Clock>(
    smc: &Smc<P, M, C>,
    nkeys: u32,
) -> Vec<Key> {
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
