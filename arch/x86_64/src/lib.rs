// SPDX-License-Identifier: BSD-2-Clause
//! x86_64 architecture support: kernel entry from the loader, CPU tables,
//! traps, MMU control, and the small set of privileged instructions the
//! rest of the kernel needs.
//!
//! Phase 1 exposes a plain module interface that `arch-aarch64` mirrors
//! name for name; it becomes the `arch/hal` trait set in Phase 2.
//!
//! The kernel binary connects to this crate with [`kernel_hooks!`], which
//! defines the two functions the entry and trap code call into.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod backtrace;
pub mod cpu;
mod entry;
pub mod fixup;
mod gdt;
pub mod mmu;
pub mod trap;

pub use addr::{PhysAddr, VirtAddr};
pub use gdt::cpu_id;
pub use trap::TrapReport;

/// The architecture name FreeBSD uses (`uname -m`).
pub const NAME: &str = "amd64";

/// Defines the kernel's entry point and fatal-trap handler.
///
/// ```ignore
/// arch::kernel_hooks!(main = kmain, fatal_trap = on_fatal_trap);
/// fn kmain(boot_info: arch::PhysAddr) -> ! { … }
/// fn on_fatal_trap(report: &arch::TrapReport) -> ! { … }
/// ```
#[macro_export]
macro_rules! kernel_hooks {
    (main = $main:path, fatal_trap = $trap:path $(,)?) => {
        #[unsafe(no_mangle)]
        fn abyss_kernel_main(boot_info: $crate::PhysAddr) -> ! {
            $main(boot_info)
        }

        #[unsafe(no_mangle)]
        fn abyss_fatal_trap(report: &$crate::TrapReport) -> ! {
            $trap(report)
        }
    };
}

unsafe extern "Rust" {
    /// Defined by [`kernel_hooks!`].
    fn abyss_kernel_main(boot_info: PhysAddr) -> !;
    /// Defined by [`kernel_hooks!`].
    fn abyss_fatal_trap(report: &TrapReport) -> !;
}

/// Wraps a value that is only touched on the boot CPU before SMP, or under
/// its own discipline documented at each use.
#[repr(transparent)]
pub(crate) struct Racy<T>(core::cell::UnsafeCell<T>);

// SAFETY: every `Racy` documents why its accesses do not race; in Phase 1
// all of them happen on the boot CPU with interrupts disabled.
unsafe impl<T> Sync for Racy<T> {}

impl<T> Racy<T> {
    pub(crate) const fn new(v: T) -> Self {
        Self(core::cell::UnsafeCell::new(v))
    }

    pub(crate) fn get(&self) -> *mut T {
        self.0.get()
    }
}

/// `n` as a `u16`, for descriptor-table limits; panics (at compile time in
/// const contexts) if it does not fit.
#[allow(clippy::cast_possible_truncation, reason = "range checked first")]
pub(crate) const fn to_u16(n: usize) -> u16 {
    assert!(n <= 0xffff);
    n as u16
}
