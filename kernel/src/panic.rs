// SPDX-License-Identifier: BSD-2-Clause
//! Panics and fatal traps: the other CPUs stopped, a report with a
//! symbolized backtrace, then a reboot after `kern.panic_reboot_wait_time`
//! (FreeBSD's `panic`, `shutdown_panic`, and `shutdown_reset`).

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::arch;
use crate::symbols::Symbolized;

static PANICKING: AtomicBool = AtomicBool::new(false);

/// `kern.panic_reboot_wait_time`: seconds from a panic's report to the
/// reboot; 0 reboots at once, -1 never (FreeBSD's `PANIC_REBOOT_WAIT_TIME`
/// is 15).
static REBOOT_WAIT: AtomicI32 = AtomicI32::new(15);

/// How long a panic waits for the other CPUs to stop.
const STOP_WAIT_NS: u64 = 1_000_000_000;

/// Set by the test that deliberately takes a fatal fault, which then counts
/// as a pass.
pub(crate) static EXPECT_FATAL_TRAP: AtomicBool = AtomicBool::new(false);

/// Set by the warm-reset case's first boot before it panics on purpose:
/// no verdict then (no failure marker, no QEMU exit), only the reboot.
pub(crate) static EXPECT_PANIC_RESET: AtomicBool = AtomicBool::new(false);

macro_rules! out {
    ($($t:tt)*) => { $crate::console::print_panic(format_args!($($t)*)) };
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    arch::cpu::disable_interrupts();
    if PANICKING.swap(true, Ordering::Relaxed) {
        out!("\nnested panic: {info}\n");
        arch::cpu::halt_forever();
    }
    let stopped = crate::smp::stop_others(STOP_WAIT_NS);
    out!("\npanic: {info}\n");
    out!("cpuid = {}\n", hal::cpu_id().0);
    report_stop(stopped);
    backtrace(arch::backtrace::here());
    if !EXPECT_PANIC_RESET.load(Ordering::Relaxed) {
        out!("{}\n", ktest::FAIL_MARKER);
    }
    stop(false)
}

/// How long a CPU may wait for a spinning lock before the kernel gives up:
/// FreeBSD's `_mtx_lock_indefinite_check` panics after 10 s.
const SPIN_LIMIT_NS: u64 = 10_000_000_000;

/// `sync`'s look at a long wait for lock `lock` begun at `since`: a panic,
/// with this CPU's backtrace, once it has gone on too long ("spin lock
/// held too long"), rather than a machine that hangs in silence.
pub(crate) fn spin_stuck(lock: usize, since: &mut u64) {
    let now = crate::time::now_ns();
    if now == 0 {
        return;
    }
    if *since == 0 {
        *since = now;
    } else if now.saturating_sub(*since) > SPIN_LIMIT_NS {
        panic!(
            "spin lock {lock:#x} ({}) held too long on cpu {}",
            Symbolized::pc(lock as u64),
            hal::cpu_id().0
        );
    }
}

/// The arch trap code's report of an exception the kernel cannot handle.
pub(crate) fn fatal_trap(report: &arch::TrapReport) -> ! {
    arch::cpu::disable_interrupts();
    if PANICKING.swap(true, Ordering::Relaxed) {
        out!("\nfatal trap while panicking:\n{report}");
        arch::cpu::halt_forever();
    }
    let stopped = crate::smp::stop_others(STOP_WAIT_NS);
    out!("\nfatal trap:\n{report}");
    out!("cpuid = {}\n", hal::cpu_id().0);
    report_stop(stopped);
    out!("at {}\n", Symbolized::pc(report.pc()));
    backtrace(report.frame_pointer());
    if EXPECT_FATAL_TRAP.load(Ordering::Relaxed) {
        out!("ktest: deliberate fatal fault reported\n");
        out!("{}\n", ktest::PASS_MARKER);
        stop(true)
    }
    out!("{}\n", ktest::FAIL_MARKER);
    stop(false)
}

/// An NMI (`bhyvectl --inject-nmi`, QEMU's `nmi`): where this CPU is,
/// then back to it, so a hung machine can be asked CPU by CPU.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub(crate) fn nmi(report: &arch::TrapReport) {
    // One CPU's report at a time: an NMI sent to all of them lands at
    // once. Only NMIs take it, and they do not nest.
    static PRINTING: AtomicBool = AtomicBool::new(false);
    // Acquire: pairs with the store below.
    while PRINTING
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    nmi_report(report);
    // Release: this report is out before the next starts.
    PRINTING.store(false, Ordering::Release);
}

fn nmi_report(report: &arch::TrapReport) {
    out!(
        "\nNMI on cpu {}: at {}\n",
        hal::cpu_id().0,
        Symbolized::pc(report.pc())
    );
    // A user-mode PC is a lower-half address, and its frames are not ours.
    if report.pc() >> 63 != 0 {
        backtrace(report.frame_pointer());
    }
}

/// Prints this thread's backtrace (witness reports).
pub(crate) fn backtrace_here() {
    backtrace(arch::cpu::frame_pointer());
}

fn backtrace(fp: u64) {
    out!("backtrace:\n");
    for (i, ret) in arch::backtrace::walk(fp).enumerate() {
        out!("  #{i:<2} {}\n", Symbolized::ret(ret));
    }
}

/// Says so when some other CPU did not stop (it spins with interrupts
/// masked: FreeBSD's "timeout stopping cpus").
fn report_stop((stopped, asked): (u32, u32)) {
    if stopped < asked {
        out!(
            "panic: {} of {asked} other CPUs did not stop\n",
            asked - stopped
        );
    }
}

/// `kern.panic_reboot_wait_time`, from the command line (`TUNABLE_INT`).
pub(crate) fn init(s: &sched::Sleepable) {
    if let Some(n) = crate::boot::value("kern.panic_reboot_wait_time").and_then(|v| v.parse().ok())
    {
        REBOOT_WAIT.store(n, Ordering::Relaxed);
    }
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        t.add_writable(
            kern,
            sysctl::Number::Auto,
            "panic_reboot_wait_time",
            "Seconds to wait before rebooting after a panic",
            abi::sysctl::ctlflag::TUN,
            sysctl::Value::Int(Box::new(|| REBOOT_WAIT.load(Ordering::Relaxed))),
            sysctl::int_writer(|n| {
                REBOOT_WAIT.store(n, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.panic_reboot_wait_time");
    });
}

/// Ends the run: report to QEMU if asked, power off after a pass, and
/// after a failure reboot once `kern.panic_reboot_wait_time` is up, as
/// FreeBSD does (an unattended machine comes back; -1 keeps the report on
/// the screen). `abyss.test.poweroff` skips QEMU's report so a pass powers
/// off for real; [`EXPECT_PANIC_RESET`] so a panic reboots for real.
fn stop(passed: bool) -> ! {
    if !crate::boot::flag("abyss.test.poweroff") && !EXPECT_PANIC_RESET.load(Ordering::Relaxed) {
        crate::qemu::exit(passed);
    }
    if passed {
        crate::power::off()
    }
    let wait = REBOOT_WAIT.load(Ordering::Relaxed);
    if wait < 0 {
        arch::cpu::halt_forever()
    }
    if wait > 0 {
        out!("Automatic reboot in {wait} seconds\n");
        delay_s(wait.unsigned_abs());
    }
    out!("Rebooting...\n");
    crate::power::reset_saying(&|a| out!("{a}\n"))
}

/// Waits `s` seconds, interrupts masked, by the clock (or by spinning
/// before it is set up).
fn delay_s(s: u32) {
    let start = crate::time::now_ns();
    let ns = u64::from(s) * 1_000_000_000;
    if start == 0 {
        for _ in 0..u64::from(s) * 1_000_000_000 {
            core::hint::spin_loop();
        }
        return;
    }
    while crate::time::now_ns().saturating_sub(start) < ns {
        core::hint::spin_loop();
    }
}
