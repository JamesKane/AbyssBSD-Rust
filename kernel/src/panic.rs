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

fn backtrace(fp: u64) {
    out!("backtrace:\n");
    for (i, ret) in arch::backtrace::walk(fp).enumerate() {
        out!("  #{i:<2} {}\n", Symbolized::ret(ret));
    }
}

/// Ends the run: report to QEMU if asked, power off after a pass, halt
/// after a failure so the screen stays readable.
fn stop(passed: bool) -> ! {
    crate::qemu::exit(passed);
    if passed {
        crate::firmware::reset(efi::ResetKind::Shutdown)
    }
    arch::cpu::halt_forever()
}
