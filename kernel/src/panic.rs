// SPDX-License-Identifier: BSD-2-Clause
//! Panics and fatal traps: report with a symbolized backtrace, then stop.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;
use crate::symbols::Symbolized;

static PANICKING: AtomicBool = AtomicBool::new(false);

/// Set by the test that deliberately takes a fatal fault, which then counts
/// as a pass.
pub(crate) static EXPECT_FATAL_TRAP: AtomicBool = AtomicBool::new(false);

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
    out!("\npanic: {info}\n");
    backtrace(arch::backtrace::here());
    out!("{}\n", ktest::FAIL_MARKER);
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
    out!("\nfatal trap:\n{report}");
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

/// Ends the run: report to QEMU if asked, power off after a pass, halt
/// after a failure so the screen stays readable. `abyss.test.poweroff`
/// skips QEMU's report so a pass powers off for real.
fn stop(passed: bool) -> ! {
    if !crate::boot::flag("abyss.test.poweroff") {
        crate::qemu::exit(passed);
    }
    if passed {
        crate::power::off()
    }
    arch::cpu::halt_forever()
}
