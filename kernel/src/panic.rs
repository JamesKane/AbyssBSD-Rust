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
