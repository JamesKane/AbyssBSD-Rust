// SPDX-License-Identifier: BSD-2-Clause
//! The class's state and `/dev/fido`'s requests: the last timeout, the
//! software timer and pre-timeout, and when the watchdog was last patted.
//!
//! As FreeBSD's:
//! - A pat goes to every watchdog; it succeeds if one takes the timeout,
//!   else (a timeout, but no watchdog for it) it is `EOPNOTSUPP`. Disarming
//!   (a zero timeout) always succeeds. The first pat no watchdog takes
//!   attaches the software watchdog ([`Env::software_watchdog`], FreeBSD's
//!   `wdog_software_attach`, the hardclock watchdog) and tries again.
//! - `WDIOC_GETTIMELEFT` answers the seconds since the last successful
//!   pat, not those left (`wd_get_time_left`), and `ENOENT` before one.
//! - `WDIOC_PATPAT` is defined but taken by no case (`ENOTTY`); patting is
//!   `WDIOC_SETTIMEOUT`, which `watchdogd(8)` sends every `-s` seconds.
//! - A pre-timeout must be shorter than the timeout; each pat restarts it,
//!   and a timeout it no longer fits under drops it.
//! - The software timer (`WDIOC_SETSOFT`) and the pre-timeout both act as
//!   the pre-timeout action says (`wd_timeout_cb`); the software timeout
//!   action is kept but, as in FreeBSD, not used.
//! - `WDIOC_SETSOFT` cannot change modes while a timeout is set.
//! - FreeBSD 14's requests (`COMPAT_FREEBSD14`) are taken too, their
//!   timeouts in whole seconds or (`WDIOCPATPAT`) the `u_int` encoding.
//!
//! Behavior reference: sys/dev/watchdog/watchdog.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::watchdog::*;
use sync::SpinLock;

use crate::Watchdog;

/// What the class needs from the kernel.
pub trait Env {
    /// The registered hardware watchdogs ([`crate::all`]).
    fn watchdogs(&self) -> Vec<Arc<dyn Watchdog>>;
    /// The software watchdog to fall back on, if there is one.
    fn software_watchdog(&self) -> Option<Arc<dyn Watchdog>>;
    /// Whole seconds of uptime (`CLOCK_MONOTONIC_FAST`'s).
    fn uptime_secs(&self) -> i64;
    /// Starts the pre-timeout callout to fire `after` from now, or stops
    /// it. It calls [`Class::timeout_action`] and does that.
    fn set_pretimer(&self, after: Option<Sbintime>);
    /// The same for the software timer.
    fn set_softtimer(&self, after: Option<Sbintime>);
}

/// What a pre-timeout or software timeout does (`WD_SOFT_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftAction(i32);

impl SoftAction {
    /// `WD_SOFT_PANIC`.
    #[must_use]
    pub fn panic(self) -> bool {
        self.0 & WD_SOFT_PANIC != 0
    }
    /// `WD_SOFT_DDB`.
    #[must_use]
    pub fn ddb(self) -> bool {
        self.0 & WD_SOFT_DDB != 0
    }
    /// `WD_SOFT_LOG`.
    #[must_use]
    pub fn log(self) -> bool {
        self.0 & WD_SOFT_LOG != 0
    }
    /// `WD_SOFT_PRINTF`.
    #[must_use]
    pub fn printf(self) -> bool {
        self.0 & WD_SOFT_PRINTF != 0
    }
}

struct State {
    /// `wd_last_sbt`: the timeout last set, 0 for none.
    last_sbt: Sbintime,
    /// `wd_last_u`: the same, as a power of two of nanoseconds.
    last_u: u32,
    /// `wd_softtimer`: nonzero for the software timer.
    softtimer: i32,
    /// `wd_softtimeout_act`.
    soft_act: i32,
    /// `wd_pretimeout_act`.
    pre_act: i32,
    /// `wd_pretimeout`.
    pretimeout: Sbintime,
    /// `wd_lastpat`, once valid.
    lastpat: Option<i64>,
    /// No pat yet (the software watchdog is attached on the first).
    first: bool,
    /// The software watchdog, once attached.
    software: Option<Arc<dyn Watchdog>>,
}

/// The watchdog class (one per kernel).
pub struct Class {
    st: SpinLock<State>,
}

impl core::fmt::Debug for Class {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let st = self.st.lock();
        f.debug_struct("Class")
            .field("last_sbt", &st.last_sbt)
            .field("softtimer", &st.softtimer)
            .finish_non_exhaustive()
    }
}

impl Default for Class {
    fn default() -> Self {
        Self::new()
    }
}

/// C's `(int)` of a 64-bit value.
#[allow(clippy::cast_possible_truncation, reason = "C's (int) conversion")]
fn c_int(v: i64) -> i32 {
    v as i32
}

fn int_arg(arg: &[u8]) -> Result<i32, Errno> {
    let b: [u8; 4] = arg
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .ok_or(Errno::EINVAL)?;
    Ok(i32::from_ne_bytes(b))
}

fn sbt_arg(arg: &[u8]) -> Result<Sbintime, Errno> {
    let b: [u8; 8] = arg
        .get(..8)
        .and_then(|b| b.try_into().ok())
        .ok_or(Errno::EINVAL)?;
    Ok(i64::from_ne_bytes(b))
}

fn put(arg: &mut [u8], bytes: &[u8]) -> Result<(), Errno> {
    arg.get_mut(..bytes.len())
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(bytes);
    Ok(())
}

impl State {
    /// `wdog_kern_pat_sbt`.
    fn pat_sbt(&mut self, env: &dyn Env, sbt: Sbintime) -> Result<(), Errno> {
        let pow2ns = sbt_to_pow2ns(sbt);
        if self.last_sbt != sbt {
            self.last_u = pow2ns;
            self.last_sbt = sbt;
        }
        let mut taken = sbt == 0;
        if self.softtimer != 0 {
            env.set_softtimer((sbt != 0).then_some(sbt));
            taken = true;
        } else {
            taken |= self.invoke(env, sbt, pow2ns);
        }
        if !taken
            && self.first
            && let Some(sw) = env.software_watchdog()
        {
            self.software = Some(sw);
            taken |= self.invoke(env, sbt, pow2ns);
        }
        self.first = false;
        let _ = self.set_pretimeout(env, self.pretimeout, true);
        if !taken {
            return Err(Errno::EOPNOTSUPP);
        }
        self.lastpat = Some(env.uptime_secs());
        Ok(())
    }

    /// The `watchdog_list` event: whether any watchdog took `sbt`.
    fn invoke(&self, env: &dyn Env, sbt: Sbintime, pow2ns: u32) -> bool {
        let mut taken = false;
        for w in env.watchdogs().iter().chain(self.software.iter()) {
            taken |= w.event(sbt, pow2ns);
        }
        taken
    }

    /// `wdog_kern_pat`.
    fn pat(&mut self, env: &dyn Env, u: u32) -> Result<(), Errno> {
        match decode_kern_pat(u) {
            Err(_) => Err(Errno::EINVAL),
            Ok(LegacyPat::LastValue) => {
                self.control(env, WD_CTRL_RESET);
                Ok(())
            }
            Ok(LegacyPat::Interval(i)) => self.pat_sbt(env, pow2ns_to_sbt(i)),
        }
    }

    /// `wdog_control`: errors are dropped.
    fn control(&mut self, env: &dyn Env, ctrl: i32) {
        if ctrl == WD_CTRL_DISABLE {
            let _ = self.pat(env, WD_TO_NEVER);
        }
        if ctrl & (WD_CTRL_RESET | WD_CTRL_ENABLE) != 0 {
            let _ = self.pat_sbt(env, self.last_sbt);
        }
    }

    /// `wd_set_pretimeout`.
    fn set_pretimeout(
        &mut self,
        env: &dyn Env,
        new: Sbintime,
        disable_if_too_long: bool,
    ) -> Result<(), Errno> {
        let mut new = new;
        if new >= self.last_sbt {
            if !disable_if_too_long {
                return Err(Errno::EINVAL);
            }
            new = 0;
        }
        if new == 0 {
            self.pretimeout = 0;
            env.set_pretimer(None);
            return Ok(());
        }
        env.set_pretimer(Some(self.last_sbt.saturating_sub(new)));
        self.pretimeout = new;
        Ok(())
    }

    /// `wd_get_time_left`: seconds since the last pat.
    fn time_left(&self, env: &dyn Env) -> Result<i64, Errno> {
        let last = self.lastpat.ok_or(Errno::ENOENT)?;
        Ok(env.uptime_secs() - last)
    }
}

impl Class {
    /// The class with nothing set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            st: SpinLock::new(State {
                last_sbt: 0,
                last_u: 0,
                softtimer: 0,
                soft_act: WD_SOFT_LOG,
                pre_act: WD_SOFT_LOG,
                pretimeout: 0,
                lastpat: None,
                first: true,
                software: None,
            }),
        }
    }

    /// Pats every watchdog with timeout `sbt`, or disarms them for 0
    /// (`wdog_kern_pat_sbt`).
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` if no watchdog takes a timeout.
    pub fn pat_sbt(&self, env: &dyn Env, sbt: Sbintime) -> Result<(), Errno> {
        self.st.lock().pat_sbt(env, sbt)
    }

    /// The same with a `u_int` timeout (`wdog_kern_pat`): `WD_TO_NEVER`
    /// disarms, `WD_LASTVAL` pats with the last.
    ///
    /// # Errors
    ///
    /// `EINVAL` for `WD_LASTVAL` with an interval; as [`Class::pat_sbt`].
    pub fn pat(&self, env: &dyn Env, u: u32) -> Result<(), Errno> {
        self.st.lock().pat(env, u)
    }

    /// The timeout last set (`wdog_kern_last_timeout_sbt`).
    #[must_use]
    pub fn last_timeout_sbt(&self) -> Sbintime {
        self.st.lock().last_sbt
    }

    /// The same as a power of two of nanoseconds (`wdog_kern_last_timeout`,
    /// `hw.watchdog.wd_last_u`).
    #[must_use]
    pub fn last_timeout(&self) -> u32 {
        self.st.lock().last_u
    }

    /// What a pre-timeout or software timeout does now: the pre-timeout
    /// action, for both (`wd_timeout_cb`).
    #[must_use]
    pub fn timeout_action(&self) -> SoftAction {
        SoftAction(self.st.lock().pre_act)
    }

    /// The software timeout action (`WDIOC_SETSOFTTIMEOUTACT`), kept only.
    #[must_use]
    pub fn soft_timeout_action(&self) -> SoftAction {
        SoftAction(self.st.lock().soft_act)
    }

    /// `/dev/fido`'s request `req` with its argument `arg` (`wd_ioctl`).
    ///
    /// # Errors
    ///
    /// As FreeBSD's; `ENOTTY` for a request it does not know.
    pub fn ioctl(&self, env: &dyn Env, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let mut st = self.st.lock();
        match req {
            WDIOC_SETSOFT => {
                let u = int_arg(arg)?;
                if u == st.softtimer {
                    return Ok(());
                }
                if st.last_u != 0 {
                    return Err(Errno::EINVAL);
                }
                st.softtimer = u;
                Ok(())
            }
            WDIOC_SETSOFTTIMEOUTACT | WDIOC_SETPRETIMEOUTACT => {
                let u = int_arg(arg)?;
                if !valid_act(u) {
                    return Err(Errno::EINVAL);
                }
                if req == WDIOC_SETSOFTTIMEOUTACT {
                    st.soft_act = u;
                } else {
                    st.pre_act = u;
                }
                Ok(())
            }
            WDIOC_GETPRETIMEOUT_14 => put(arg, &c_int(st.pretimeout / SBT_1S).to_ne_bytes()),
            WDIOC_SETPRETIMEOUT_14 => {
                let secs = i64::from(int_arg(arg)?);
                st.set_pretimeout(env, secs * SBT_1S, false)
            }
            WDIOC_GETTIMELEFT_14 => {
                let left = st.time_left(env)?;
                put(arg, &c_int(left).to_ne_bytes())
            }
            WDIOC_SETTIMEOUT_14 => {
                let secs = int_arg(arg)?.cast_unsigned();
                st.pat_sbt(env, ms_to_sbt(i64::from(secs) * 1000))
            }
            WDIOC_GETTIMEOUT_14 => put(arg, &st.last_u.to_ne_bytes()),
            WDIOCPATPAT_14 => match decode_legacy_pat(int_arg(arg)?.cast_unsigned()) {
                Err(LegacyPatError::Invalid) => Err(Errno::EINVAL),
                Err(LegacyPatError::Passive) => Err(Errno::ENOSYS),
                Ok(LegacyPat::LastValue) => st.pat(env, WD_LASTVAL),
                Ok(LegacyPat::Interval(i)) => st.pat(env, i),
            },
            WDIOC_CONTROL => {
                let ctrl = int_arg(arg)?;
                st.control(env, ctrl);
                Ok(())
            }
            WDIOC_SETTIMEOUT => {
                let sbt = sbt_arg(arg)?;
                st.pat_sbt(env, sbt)
            }
            WDIOC_GETTIMEOUT => put(arg, &st.last_sbt.to_ne_bytes()),
            WDIOC_GETTIMELEFT => {
                let left = st.time_left(env)?;
                put(arg, &left.wrapping_mul(SBT_1S).to_ne_bytes())
            }
            WDIOC_GETPRETIMEOUT => put(arg, &st.pretimeout.to_ne_bytes()),
            WDIOC_SETPRETIMEOUT => {
                let sbt = sbt_arg(arg)?;
                st.set_pretimeout(env, sbt, false)
            }
            _ => Err(Errno::ENOTTY),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::sync::Mutex;
    use std::vec;

    use super::*;
    use crate::WatchdogError;

    /// A watchdog that counts up to `max_ms`, recording what it was told.
    struct Dog {
        max_ms: u64,
        armed: Mutex<Option<u64>>,
    }

    impl Dog {
        fn new(max_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                max_ms,
                armed: Mutex::new(None),
            })
        }
        fn armed(&self) -> Option<u64> {
            *self.armed.lock().unwrap()
        }
    }

    impl Watchdog for Dog {
        fn name(&self) -> &str {
            "dog0"
        }
        fn arm(&self, ms: u64) -> Result<u64, WatchdogError> {
            if ms > self.max_ms {
                return Err(WatchdogError::OutOfRange);
            }
            *self.armed.lock().unwrap() = Some(ms);
            Ok(ms)
        }
        fn pat(&self) {}
        fn disarm(&self) {
            *self.armed.lock().unwrap() = None;
        }
    }

    /// The hardclock watchdog's rule: whole seconds from 2^30 ns up.
    struct Soft(Mutex<Option<u32>>);

    impl Watchdog for Soft {
        fn name(&self) -> &str {
            "hardclock"
        }
        fn arm(&self, _: u64) -> Result<u64, WatchdogError> {
            Err(WatchdogError::OutOfRange)
        }
        fn pat(&self) {}
        fn disarm(&self) {}
        fn event(&self, sbt: Sbintime, pow2ns: u32) -> bool {
            let on = sbt > 0 && pow2ns >= WD_TO_1SEC;
            *self.0.lock().unwrap() = on.then_some(pow2ns);
            on
        }
    }

    #[derive(Default)]
    struct TestEnv {
        dogs: Vec<Arc<dyn Watchdog>>,
        soft: Option<Arc<dyn Watchdog>>,
        now: Mutex<i64>,
        pre: Mutex<Option<Sbintime>>,
        softtimer: Mutex<Option<Sbintime>>,
    }

    impl Env for TestEnv {
        fn watchdogs(&self) -> Vec<Arc<dyn Watchdog>> {
            self.dogs.clone()
        }
        fn software_watchdog(&self) -> Option<Arc<dyn Watchdog>> {
            self.soft.clone()
        }
        fn uptime_secs(&self) -> i64 {
            *self.now.lock().unwrap()
        }
        fn set_pretimer(&self, after: Option<Sbintime>) {
            *self.pre.lock().unwrap() = after;
        }
        fn set_softtimer(&self, after: Option<Sbintime>) {
            *self.softtimer.lock().unwrap() = after;
        }
    }

    fn sbt(v: Sbintime) -> [u8; 8] {
        v.to_ne_bytes()
    }

    fn int(v: i32) -> [u8; 4] {
        v.to_ne_bytes()
    }

    fn get_sbt(c: &Class, env: &TestEnv, req: u32) -> Result<Sbintime, Errno> {
        let mut b = [0; 8];
        c.ioctl(env, req, &mut b)?;
        Ok(i64::from_ne_bytes(b))
    }

    /// What watchdogd sends: `WDIOC_SETSOFT` 0, `WDIOC_SETTIMEOUT` with
    /// `-t` (128 s by default) at start and every `-s` seconds, and
    /// `WDIOC_CONTROL` `WD_CTRL_DISABLE` when it exits.
    #[test]
    fn watchdogd_session() {
        let dog = Dog::new(600_000);
        let env = TestEnv {
            dogs: vec![dog.clone() as Arc<dyn Watchdog>],
            ..TestEnv::default()
        };
        let c = Class::new();
        assert_eq!(get_sbt(&c, &env, WDIOC_GETTIMELEFT), Err(Errno::ENOENT));
        c.ioctl(&env, WDIOC_SETSOFT, &mut int(0)).unwrap();
        c.ioctl(&env, WDIOC_SETTIMEOUT, &mut sbt(128 * SBT_1S))
            .unwrap();
        assert_eq!(dog.armed(), Some(128_000));
        assert_eq!(get_sbt(&c, &env, WDIOC_GETTIMEOUT), Ok(128 * SBT_1S));
        assert_eq!(c.last_timeout(), WD_TO_128SEC);
        *env.now.lock().unwrap() = 7;
        assert_eq!(
            get_sbt(&c, &env, WDIOC_GETTIMELEFT),
            Ok(7 * SBT_1S),
            "seconds since the pat, as FreeBSD answers"
        );
        c.ioctl(&env, WDIOC_SETTIMEOUT, &mut sbt(128 * SBT_1S))
            .unwrap();
        assert_eq!(get_sbt(&c, &env, WDIOC_GETTIMELEFT), Ok(0));
        // `-t 2`: whole seconds, not a power of two of nanoseconds.
        c.ioctl(&env, WDIOC_SETTIMEOUT, &mut sbt(2 * SBT_1S))
            .unwrap();
        assert_eq!(dog.armed(), Some(2000));
        c.ioctl(&env, WDIOC_CONTROL, &mut int(WD_CTRL_DISABLE))
            .unwrap();
        assert_eq!(dog.armed(), None);
        assert_eq!(get_sbt(&c, &env, WDIOC_GETTIMEOUT), Ok(0));
        assert_eq!(c.last_timeout(), 0);
        // ENABLE after DISABLE re-pats with the last timeout: none.
        c.ioctl(&env, WDIOC_CONTROL, &mut int(WD_CTRL_ENABLE))
            .unwrap();
        assert_eq!(dog.armed(), None);
    }

    #[test]
    fn control_reset_repats_the_last_timeout() {
        let dog = Dog::new(600_000);
        let env = TestEnv {
            dogs: vec![dog.clone() as Arc<dyn Watchdog>],
            ..TestEnv::default()
        };
        let c = Class::new();
        c.pat_sbt(&env, 10 * SBT_1S).unwrap();
        dog.disarm();
        c.ioctl(&env, WDIOC_CONTROL, &mut int(WD_CTRL_RESET))
            .unwrap();
        assert_eq!(dog.armed(), Some(10_000));
        // Errors are dropped: an unknown value is a no-op success.
        c.ioctl(&env, WDIOC_CONTROL, &mut int(0x100)).unwrap();
    }

    #[test]
    fn an_unsupported_timeout_is_eopnotsupp() {
        let dog = Dog::new(30_000);
        let env = TestEnv {
            dogs: vec![dog.clone() as Arc<dyn Watchdog>],
            ..TestEnv::default()
        };
        let c = Class::new();
        assert_eq!(c.pat_sbt(&env, 60 * SBT_1S), Err(Errno::EOPNOTSUPP));
        assert_eq!(dog.armed(), None, "a timeout it cannot count stops it");
        assert_eq!(c.last_timeout_sbt(), 60 * SBT_1S, "recorded even so");
        assert_eq!(get_sbt(&c, &env, WDIOC_GETTIMELEFT), Err(Errno::ENOENT));
        // No watchdog at all.
        let none = TestEnv::default();
        let c = Class::new();
        assert_eq!(c.pat_sbt(&none, SBT_1S), Err(Errno::EOPNOTSUPP));
        assert_eq!(c.pat_sbt(&none, 0), Ok(()), "disarming always works");
        // `WD_TO_NEVER` through the legacy request.
        let mut b = int(WD_ACTIVE.cast_signed());
        assert_eq!(c.ioctl(&none, WDIOCPATPAT_14, &mut b), Ok(()));
    }

    #[test]
    fn the_first_unsupported_pat_attaches_the_software_watchdog() {
        let soft = Arc::new(Soft(Mutex::new(None)));
        let env = TestEnv {
            soft: Some(soft.clone() as Arc<dyn Watchdog>),
            ..TestEnv::default()
        };
        let c = Class::new();
        c.pat_sbt(&env, 16 * SBT_1S).unwrap();
        assert_eq!(*soft.0.lock().unwrap(), Some(WD_TO_16SEC));
        // Under a second it does not take; it stays attached.
        assert_eq!(c.pat_sbt(&env, SBT_1S / 4), Err(Errno::EOPNOTSUPP));
        assert_eq!(*soft.0.lock().unwrap(), None);
        c.pat_sbt(&env, 4 * SBT_1S).unwrap();
        // Only on the first pat: a class whose first pat a hardware
        // watchdog took never attaches it.
        let dog = Dog::new(1000);
        let soft2 = Arc::new(Soft(Mutex::new(None)));
        let env = TestEnv {
            dogs: vec![dog as Arc<dyn Watchdog>],
            soft: Some(soft2.clone() as Arc<dyn Watchdog>),
            ..TestEnv::default()
        };
        let c = Class::new();
        c.pat_sbt(&env, SBT_1S).unwrap();
        assert_eq!(c.pat_sbt(&env, 8 * SBT_1S), Err(Errno::EOPNOTSUPP));
        assert_eq!(*soft2.0.lock().unwrap(), None);
    }

    #[test]
    fn software_timer() {
        let dog = Dog::new(600_000);
        let env = TestEnv {
            dogs: vec![dog.clone() as Arc<dyn Watchdog>],
            ..TestEnv::default()
        };
        let c = Class::new();
        c.pat_sbt(&env, 5 * SBT_1S).unwrap();
        assert_eq!(
            c.ioctl(&env, WDIOC_SETSOFT, &mut int(1)),
            Err(Errno::EINVAL),
            "not with a timeout set"
        );
        c.ioctl(&env, WDIOC_SETSOFT, &mut int(0)).unwrap();
        c.pat_sbt(&env, 0).unwrap();
        c.ioctl(&env, WDIOC_SETSOFT, &mut int(1)).unwrap();
        c.ioctl(&env, WDIOC_SETTIMEOUT, &mut sbt(3 * SBT_1S))
            .unwrap();
        assert_eq!(*env.softtimer.lock().unwrap(), Some(3 * SBT_1S));
        assert_eq!(dog.armed(), None, "the hardware is left alone");
        c.ioctl(&env, WDIOC_CONTROL, &mut int(WD_CTRL_DISABLE))
            .unwrap();
        assert_eq!(*env.softtimer.lock().unwrap(), None);
        // Actions: the software timeout's is kept; both timers use the
        // pre-timeout's.
        c.ioctl(&env, WDIOC_SETSOFTTIMEOUTACT, &mut int(WD_SOFT_PANIC))
            .unwrap();
        assert!(c.soft_timeout_action().panic());
        assert!(c.timeout_action().log() && !c.timeout_action().panic());
        assert_eq!(
            c.ioctl(&env, WDIOC_SETPRETIMEOUTACT, &mut int(0x10)),
            Err(Errno::EINVAL)
        );
        c.ioctl(&env, WDIOC_SETPRETIMEOUTACT, &mut int(WD_SOFT_PRINTF))
            .unwrap();
        assert!(c.timeout_action().printf() && !c.timeout_action().log());
    }

    #[test]
    fn pretimeout() {
        let dog = Dog::new(600_000);
        let env = TestEnv {
            dogs: vec![dog as Arc<dyn Watchdog>],
            ..TestEnv::default()
        };
        let c = Class::new();
        assert_eq!(
            c.ioctl(&env, WDIOC_SETPRETIMEOUT, &mut sbt(SBT_1S)),
            Err(Errno::EINVAL),
            "no timeout to be shorter than"
        );
        c.pat_sbt(&env, 10 * SBT_1S).unwrap();
        c.ioctl(&env, WDIOC_SETPRETIMEOUT, &mut sbt(3 * SBT_1S))
            .unwrap();
        assert_eq!(*env.pre.lock().unwrap(), Some(7 * SBT_1S));
        assert_eq!(get_sbt(&c, &env, WDIOC_GETPRETIMEOUT), Ok(3 * SBT_1S));
        let mut b = [0; 4];
        c.ioctl(&env, WDIOC_GETPRETIMEOUT_14, &mut b).unwrap();
        assert_eq!(i32::from_ne_bytes(b), 3);
        // A shorter timeout drops it.
        c.pat_sbt(&env, 2 * SBT_1S).unwrap();
        assert_eq!(*env.pre.lock().unwrap(), None);
        assert_eq!(get_sbt(&c, &env, WDIOC_GETPRETIMEOUT), Ok(0));
        assert_eq!(
            c.ioctl(&env, WDIOC_SETPRETIMEOUT_14, &mut int(2)),
            Err(Errno::EINVAL)
        );
        c.ioctl(&env, WDIOC_SETPRETIMEOUT_14, &mut int(1)).unwrap();
        assert_eq!(*env.pre.lock().unwrap(), Some(SBT_1S));
        c.ioctl(&env, WDIOC_SETPRETIMEOUT, &mut sbt(0)).unwrap();
        assert_eq!(*env.pre.lock().unwrap(), None);
    }

    #[test]
    fn freebsd14_requests() {
        let dog = Dog::new(600_000);
        let env = TestEnv {
            dogs: vec![dog.clone() as Arc<dyn Watchdog>],
            ..TestEnv::default()
        };
        let c = Class::new();
        let pat = |u: u32| c.ioctl(&env, WDIOCPATPAT_14, &mut int(u.cast_signed()));
        pat(WD_ACTIVE | WD_TO_8SEC).unwrap();
        // 2^33 ns, rounded up to the millisecond.
        assert_eq!(dog.armed(), Some(8590));
        assert_eq!(pat(WD_PASSIVE | WD_TO_8SEC), Err(Errno::ENOSYS));
        assert_eq!(pat(WD_TO_8SEC), Err(Errno::EINVAL));
        assert_eq!(pat(WD_ACTIVE | WD_LASTVAL | WD_TO_1SEC), Err(Errno::EINVAL));
        dog.disarm();
        pat(WD_ACTIVE | WD_LASTVAL).unwrap();
        assert_eq!(dog.armed(), Some(8590), "the last timeout again");
        let mut b = [0; 4];
        c.ioctl(&env, WDIOC_GETTIMEOUT_14, &mut b).unwrap();
        assert_eq!(u32::from_ne_bytes(b), WD_TO_8SEC + 1);
        c.ioctl(&env, WDIOC_SETTIMEOUT_14, &mut int(30)).unwrap();
        assert_eq!(dog.armed(), Some(30_000));
        *env.now.lock().unwrap() = 4;
        c.ioctl(&env, WDIOC_GETTIMELEFT_14, &mut b).unwrap();
        assert_eq!(i32::from_ne_bytes(b), 4);
        pat(WD_ACTIVE | WD_TO_NEVER).unwrap();
        assert_eq!(dog.armed(), None);
    }

    #[test]
    fn unknown_requests() {
        let env = TestEnv::default();
        let c = Class::new();
        assert_eq!(
            c.ioctl(&env, WDIOC_PATPAT, &mut sbt(SBT_1S)),
            Err(Errno::ENOTTY),
            "no case takes it"
        );
        assert_eq!(c.ioctl(&env, 0x2000_5700, &mut []), Err(Errno::ENOTTY));
        assert_eq!(c.last_timeout_sbt(), 0);
    }

    #[test]
    fn negative_timeouts_are_refused() {
        let dog = Dog::new(600_000);
        let env = TestEnv {
            dogs: vec![dog.clone() as Arc<dyn Watchdog>],
            ..TestEnv::default()
        };
        let c = Class::new();
        assert_eq!(c.pat_sbt(&env, -SBT_1S), Err(Errno::EOPNOTSUPP));
        assert_eq!(dog.armed(), None);
    }
}
