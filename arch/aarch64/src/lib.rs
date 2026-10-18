// SPDX-License-Identifier: BSD-2-Clause
//! AArch64 architecture support: kernel entry from the loader (leaving EL2
//! if firmware ran there), exception vectors, MMU control, and the system
//! registers the rest of the kernel needs.
//!
//! Phase 1 exposes a plain module interface that `arch-x86_64` mirrors name
//! for name; it becomes the `arch/hal` trait set in Phase 2.

//!
//! Only [`cache`] builds for the UEFI target too; the loader uses it to make
//! the kernel image visible to instruction fetch before jumping to it.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod cache;

#[cfg(target_os = "none")]
pub mod backtrace;
#[cfg(target_os = "none")]
pub mod cpu;
#[cfg(target_os = "none")]
mod entry;
#[cfg(target_os = "none")]
pub mod fixup;
#[cfg(target_os = "none")]
pub mod mmu;
#[cfg(target_os = "none")]
pub mod trap;

pub use addr::{PhysAddr, VirtAddr};
#[cfg(target_os = "none")]
pub use cpu::cpu_id;
#[cfg(target_os = "none")]
pub use entry::booted_at_el2;
#[cfg(target_os = "none")]
pub use trap::TrapReport;

/// The architecture name FreeBSD uses (`uname -m`).
pub const NAME: &str = "arm64";

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

#[cfg(target_os = "none")]
unsafe extern "Rust" {
    /// Defined by [`kernel_hooks!`].
    fn abyss_kernel_main(boot_info: PhysAddr) -> !;
    /// Defined by [`kernel_hooks!`].
    fn abyss_fatal_trap(report: &TrapReport) -> !;
}

#[cfg(target_os = "none")]
/// Wraps a value that is only touched on the boot CPU before SMP, or under
/// its own discipline documented at each use.
#[repr(transparent)]
pub(crate) struct Racy<T>(core::cell::UnsafeCell<T>);

#[cfg(target_os = "none")]
// SAFETY: every `Racy` documents why its accesses do not race; in Phase 1
// all of them happen on the boot CPU with interrupts disabled.
unsafe impl<T> Sync for Racy<T> {}

#[cfg(target_os = "none")]
impl<T> Racy<T> {
    pub(crate) const fn new(v: T) -> Self {
        Self(core::cell::UnsafeCell::new(v))
    }

    pub(crate) fn get(&self) -> *mut T {
        self.0.get()
    }
}
