// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/fido`: the watchdog class for user space (`watchdogd(8)`,
//! `watchdog(8)`), `root:wheel` mode 0600 as FreeBSD's `watchdog` module
//! makes it, and the `hw.watchdog` sysctls. The requests are
//! [`watchdog::Class`]'s; this gives it the clock, its callouts, and the
//! software watchdog.
//!
//! The software watchdog is FreeBSD's hardclock watchdog
//! (`watchdog_config`, `watchdog_fire`), which the class attaches when
//! the first timeout it is given finds no hardware watchdog (QEMU's
//! `virt`, bhyve): it takes timeouts of a second and up, counting
//! 2^(N - 30) seconds for `WD_INTERVAL` N, and on expiry prints a
//! backtrace and panics. FreeBSD's GENERIC enters DDB there instead
//! (`KDB` without `KDB_UNATTENDED`); there is no debugger here.
//!
//! The pre-timeout and the software timer (`WDIOC_SETSOFT`) run on the
//! softclock thread and do what the pre-timeout action says
//! (`wd_timeout_cb`): `WD_SOFT_DDB` prints a backtrace (no debugger),
//! `WD_SOFT_LOG` logs at `LOG_EMERG`, `WD_SOFT_PRINTF` prints, and
//! `WD_SOFT_PANIC` panics.
//!
//! A halt (`RB_HALT`) disarms the watchdogs, as FreeBSD's `shutdown_halt`
//! does; a reboot or power-off leaves them to their drivers' shutdown
//! (`ichwd` and `amdsbwd` stop them there), and a panic leaves them armed
//! so that the hardware resets the machine.
//!
//! Behavior reference: sys/dev/watchdog/watchdog.c, sys/kern/kern_clock.c,
//! sys/kern/kern_shutdown.c (`shutdown_halt`).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::pin::Pin;

use abi::errno::Errno;
use abi::watchdog::{PATH_WATCHDOG, SBT_1S, Sbintime, WD_TO_1SEC, WD_TO_NEVER, sbt_to_ns};
use sched::{IrqCx, Sleepable};
use sync::Once;
use vfs::{CharDevice, Uio};
use watchdog::{Class, Env, Watchdog, WatchdogError};

use crate::softclock::Callout;
use crate::time::Timer;

/// The class: one per kernel.
static CLASS: Class = Class::new();

/// `wd_pretimeo_handle` and `wd_softtimeo_handle`.
static PRETIMER: Once<Pin<Box<Callout>>> = Once::new();
static SOFTTIMER: Once<Pin<Box<Callout>>> = Once::new();
/// The hardclock watchdog, made at [`init`].
static HARDCLOCK: Once<Arc<HardclockWatchdog>> = Once::new();

/// What the class needs, from this kernel.
struct KernelEnv;

/// A positive `sbt` in nanoseconds; none or less is now.
fn after_ns(sbt: Sbintime) -> u64 {
    u64::try_from(sbt_to_ns(sbt)).unwrap_or(0)
}

impl Env for KernelEnv {
    fn watchdogs(&self) -> Vec<Arc<dyn Watchdog>> {
        watchdog::all()
    }

    fn software_watchdog(&self) -> Option<Arc<dyn Watchdog>> {
        HARDCLOCK.get().map(|h| Arc::clone(h) as Arc<dyn Watchdog>)
    }

    fn uptime_secs(&self) -> i64 {
        i64::try_from(crate::time::now_ns() / 1_000_000_000).unwrap_or(i64::MAX)
    }

    fn set_pretimer(&self, after: Option<Sbintime>) {
        arm(&PRETIMER, after, pre_timeout);
    }

    fn set_softtimer(&self, after: Option<Sbintime>) {
        arm(&SOFTTIMER, after, soft_timeout);
    }
}

fn arm(c: &Once<Pin<Box<Callout>>>, after: Option<Sbintime>, func: crate::softclock::SoftFn) {
    let Some(c) = c.get() else {
        return;
    };
    match after {
        Some(sbt) => c.as_ref().reset(after_ns(sbt), func, 0),
        None => {
            c.stop();
        }
    }
}

/// `wd_timeout_cb`, for the pre-timeout ...
fn pre_timeout(_: &Sleepable, _: usize) {
    timeout("pre");
}

/// ... and the software timer.
fn soft_timeout(_: &Sleepable, _: usize) {
    timeout("soft");
}

fn timeout(kind: &str) {
    let act = CLASS.timeout_action();
    if act.ddb() {
        crate::panic::backtrace_here();
    }
    if act.log() {
        crate::klog::log(
            abi::syslog::LOG_EMERG,
            format_args!("watchdog {kind}-timeout, WD_SOFT_LOG\n"),
        );
    }
    if act.printf() {
        kprintln!("watchdog {kind}-timeout, WD_SOFT_PRINTF");
    }
    if act.panic() {
        panic!("watchdog {kind}-timeout, WD_SOFT_PANIC set");
    }
}

/// FreeBSD's hardclock watchdog: a countdown in timer interrupts.
struct HardclockWatchdog {
    timer: Pin<&'static Timer>,
}

/// `watchdog_fire`.
fn hardclock_fire(_: usize, _: &IrqCx) {
    crate::panic::backtrace_here();
    panic!("watchdog timeout");
}

impl Watchdog for HardclockWatchdog {
    fn name(&self) -> &str {
        "hardclock"
    }

    /// Only through [`Watchdog::event`].
    fn arm(&self, _: u64) -> Result<u64, WatchdogError> {
        Err(WatchdogError::OutOfRange)
    }

    fn pat(&self) {}

    fn disarm(&self) {
        self.timer.stop();
    }

    /// `watchdog_config`: 2^(N - 30) seconds for N of a second and up,
    /// else off. A negative timeout is refused.
    fn event(&self, sbt: Sbintime, pow2ns: u32) -> bool {
        if sbt <= 0 || pow2ns < WD_TO_1SEC {
            self.disarm();
            return false;
        }
        let secs = 1u64 << (pow2ns - WD_TO_1SEC).min(32);
        self.timer
            .as_ref()
            .reset(secs * 1_000_000_000, hardclock_fire, 0);
        true
    }
}

/// `/dev/fido`: requests only.
struct Fido;

impl CharDevice for Fido {
    /// No `d_read` (`enodev`).
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        CLASS.ioctl(&KernelEnv, req, arg)
    }
}

/// Pats every watchdog with `sbt`, or disarms them for 0
/// (`wdog_kern_pat_sbt`), as `/dev/fido`'s `WDIOC_SETTIMEOUT` does.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn pat_sbt(sbt: Sbintime) -> Result<(), Errno> {
    CLASS.pat_sbt(&KernelEnv, sbt)
}

/// Disarms the watchdogs on a halt (`shutdown_halt`'s
/// `wdog_kern_pat(WD_TO_NEVER)`).
pub(crate) fn shutdown_halt() {
    let _ = CLASS.pat(&KernelEnv, WD_TO_NEVER);
}

/// Makes `/dev/fido`, its callouts, and the software watchdog
/// (`watchdog_modevent`'s `MOD_LOAD`, `initclocks`).
pub(crate) fn init() {
    let _ = PRETIMER.set(Callout::new());
    let _ = SOFTTIMER.set(Callout::new());
    let timer = Pin::static_ref(Box::leak(Box::new(Timer::new())));
    let _ = HARDCLOCK.set(Arc::new(HardclockWatchdog { timer }));
    crate::fs::devfs::add(PATH_WATCHDOG, 0o600, 0, 0, Arc::new(Fido));
}

/// `hw.watchdog`.
pub(crate) fn sysctl_init(s: &Sleepable) {
    use sysctl::{Number, Value};
    crate::sysctl::with(s, |t| {
        let hw = t.child(t.root(), "hw").expect("hw node");
        let n = t
            .add_node(hw, Number::Auto, "watchdog", "Main watchdog device")
            .expect("hw.watchdog");
        t.add_value(
            n,
            Number::Auto,
            "wd_last_u",
            "Watchdog last update time",
            Value::Uint(Box::new(|| CLASS.last_timeout())),
        )
        .expect("hw.watchdog.wd_last_u");
        t.add_value(
            n,
            Number::Auto,
            "wd_last_u_secs",
            "Watchdog last update time",
            Value::Uint(Box::new(|| {
                // C's conversion to `u_int`: the low 32 bits.
                u32::try_from((CLASS.last_timeout_sbt() / SBT_1S) & 0xffff_ffff).unwrap_or(0)
            })),
        )
        .expect("hw.watchdog.wd_last_u_secs");
        // `wd_last_sbt_sysctl`, which FreeBSD never sets: always 0.
        t.add_value(
            n,
            Number::Auto,
            "wd_last_msecs",
            "Watchdog last update time (milliseconds)",
            Value::S64(Box::new(|| 0)),
        )
        .expect("hw.watchdog.wd_last_msecs");
    });
}
