// SPDX-License-Identifier: BSD-2-Clause
//! x86_64 architecture support: kernel entry from the loader, CPU tables,
//! traps, MMU control, and the small set of privileged instructions the
//! rest of the kernel needs.
//!
//! The kernel crate uses this crate's modules directly (`arch-aarch64`
//! mirrors them name for name); the primitives arch-neutral crates need are
//! exported through the `arch/hal` hooks at the end of this file.
//!
//! The kernel binary connects to this crate with [`kernel_hooks!`], which
//! defines the functions the entry and trap code call into.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod apic;
pub mod backtrace;
mod context;
pub mod cpu;
mod entry;
pub mod fixup;
mod gdt;
pub mod mmu;
pub mod trap;

pub use addr::{PhysAddr, VirtAddr};
pub use gdt::cpu_id;
pub use trap::{Interrupt, TrapReport};

/// The architecture name FreeBSD uses (`uname -m`).
pub const NAME: &str = "amd64";

/// Defines the kernel's entry point and fatal-trap handler.
///
/// ```ignore
/// arch::kernel_hooks!(main = kmain, fatal_trap = on_fatal_trap, interrupt = on_irq);
/// fn kmain(boot_info: arch::PhysAddr) -> ! { … }
/// fn on_fatal_trap(report: &arch::TrapReport) -> ! { … }
/// fn on_irq(irq: arch::Interrupt) { … }
/// ```
#[macro_export]
macro_rules! kernel_hooks {
    (main = $main:path, fatal_trap = $trap:path, interrupt = $irq:path $(,)?) => {
        #[unsafe(no_mangle)]
        fn abyss_kernel_main(boot_info: $crate::PhysAddr) -> ! {
            $main(boot_info)
        }

        #[unsafe(no_mangle)]
        fn abyss_fatal_trap(report: &$crate::TrapReport) -> ! {
            $trap(report)
        }

        #[unsafe(no_mangle)]
        fn abyss_interrupt(irq: $crate::Interrupt) {
            $irq(irq)
        }
    };
}

unsafe extern "Rust" {
    /// Defined by [`kernel_hooks!`].
    fn abyss_kernel_main(boot_info: PhysAddr) -> !;
    /// Defined by [`kernel_hooks!`].
    fn abyss_fatal_trap(report: &TrapReport) -> !;
    /// Defined by [`kernel_hooks!`].
    fn abyss_interrupt(irq: Interrupt);
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

/// The `arch/hal` hooks.
mod hal_hooks {
    const _: hal::IrqDisableFn = abyss_hal_irq_disable;
    const _: hal::IrqEnableFn = abyss_hal_irq_enable;
    const _: hal::CpuIdFn = abyss_hal_cpu_id;
    const _: hal::ContextInitFn = abyss_hal_context_init;
    const _: hal::CpuLocalFn = abyss_hal_cpu_local;
    const _: hal::SetCpuLocalFn = abyss_hal_set_cpu_local;
    const _: hal::WaitForInterruptFn = abyss_hal_wait_for_interrupt;

    #[unsafe(no_mangle)]
    fn abyss_hal_irq_disable() -> bool {
        crate::cpu::save_disable_interrupts()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_irq_enable() {
        crate::cpu::enable_interrupts();
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_cpu_id() -> u32 {
        crate::cpu_id()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_context_init(
        c: &mut hal::Context,
        stack_top: u64,
        entry: hal::ThreadEntry,
        arg: usize,
    ) {
        crate::context::init(c.words_mut(), stack_top, entry, arg);
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_cpu_local() -> *mut () {
        crate::gdt::cpu_local()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_set_cpu_local(p: *mut ()) {
        crate::gdt::set_cpu_local(p);
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_wait_for_interrupt() {
        crate::cpu::wait_for_interrupt();
    }
}
