// SPDX-License-Identifier: BSD-2-Clause
//! `reboot(2)` and the paths to it (`kern_shutdown.c`): init's own
//! request, and `shutdown_nice` for the power button and the console's
//! reboot keys, which ask init. docs/MULTIUSER.md §11.4.

use abi::errno::Errno;
use abi::signal::{SIGINT, SIGUSR1, SIGUSR2, SIGWINCH};
use sched::Sleepable;
use security::Priv;

/// `RB_NOSYNC`: no sync before the reboot.
const RB_NOSYNC: i32 = 0x004;
/// `RB_HALT`: halt rather than reboot.
pub(crate) const RB_HALT: i32 = 0x008;
/// `RB_POWEROFF`: power off if possible.
pub(crate) const RB_POWEROFF: i32 = 0x4000;
/// `RB_REROOT`: unmount the root and mount it again.
const RB_REROOT: i32 = 0x0020_0000;
/// `RB_POWERCYCLE`: power cycle if possible.
pub(crate) const RB_POWERCYCLE: i32 = 0x0040_0000;

/// `reboot(opt)`: with `PRIV_REBOOT`; returns only on an error.
pub(crate) fn reboot(s: &Sleepable, opt: u64) -> crate::syscall::SysResult {
    let p = crate::proc::Process::current().ok_or(Errno::EINVAL)?;
    p.priv_check(Priv::Reboot)?;
    let howto = crate::syscall::int(opt);
    if howto & RB_REROOT != 0 {
        // `kern_reroot`: not yet (MULTIUSER.md §11.4).
        return Err(Errno::EOPNOTSUPP);
    }
    kern_reboot(s, howto)
}

/// `shutdown_nice`: asks init to shut down as `howto` says (`SIGUSR2` to
/// power off, `SIGWINCH` to power cycle, `SIGUSR1` to halt, `SIGINT` to
/// reboot); with no init, reboots without syncing.
pub(crate) fn shutdown_nice(s: &Sleepable, howto: i32) {
    let Some(init) = crate::proc::find(crate::proc::INIT) else {
        kern_reboot(s, howto | RB_NOSYNC)
    };
    let sig = if howto & RB_POWEROFF != 0 {
        SIGUSR2
    } else if howto & RB_POWERCYCLE != 0 {
        SIGWINCH
    } else if howto & RB_HALT != 0 {
        SIGUSR1
    } else {
        SIGINT
    };
    let info = crate::signal::user_info(sig, 0, abi::signal::code::SI_KERNEL);
    let _ = crate::signal::post(&init, info);
}

/// `kern_reboot`: syncs and unmounts every file system unless
/// `RB_NOSYNC`, prints the uptime, then powers off, halts, or resets.
pub(crate) fn kern_reboot(s: &Sleepable, howto: i32) -> ! {
    if howto & RB_NOSYNC == 0 {
        crate::fs::shutdown(s);
    }
    print_uptime();
    // `acpi_shutdown_final` (and PSCI's) power off for `RB_POWEROFF`
    // ahead of `shutdown_halt`, `RB_HALT` or not.
    if howto & RB_POWEROFF != 0 {
        crate::power::prepare(s);
        crate::devices::shutdown(s);
        crate::power::off()
    }
    if howto & RB_HALT != 0 {
        // `shutdown_halt`; there is no reading the console here, so it
        // stays halted.
        kprintln!("\nThe operating system has halted.\nPlease press any key to reboot.\n");
        crate::devices::shutdown(s);
        crate::arch::cpu::halt_forever()
    }
    // `shutdown_reset`; `RB_POWERCYCLE` falls back to it as on machines
    // without a power-cycle method.
    kprintln!("Rebooting...");
    crate::devices::shutdown(s);
    crate::power::reset()
}

/// `print_uptime`.
fn print_uptime() {
    let mut secs = crate::time::now_ns() / 1_000_000_000;
    let mut out = alloc::string::String::from("Uptime: ");
    let mut f = false;
    for (unit, name) in [(86_400, 'd'), (3_600, 'h'), (60, 'm')] {
        if f || secs >= unit {
            out.push_str(&alloc::format!("{}{name}", secs / unit));
            secs %= unit;
            f = true;
        }
    }
    kprintln!("{out}{secs}s");
}

/// `suspend_blocked`: init blocks suspending while it shuts down. There is
/// no suspend yet, so it is only kept.
static SUSPEND_BLOCKED: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(0);

/// `kern.suspend_blocked`.
pub(crate) fn sysctl_init(s: &Sleepable) {
    use core::sync::atomic::Ordering;
    use sysctl::{Number, Value};
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        t.add_writable(
            kern,
            Number::Auto,
            "suspend_blocked",
            "Block suspend due to a pending shutdown",
            0,
            Value::Int(alloc::boxed::Box::new(|| {
                SUSPEND_BLOCKED.load(Ordering::Relaxed)
            })),
            sysctl::int_writer(|v| {
                SUSPEND_BLOCKED.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.suspend_blocked");
    });
}
