// SPDX-License-Identifier: BSD-2-Clause
//! AArch64 architecture support: kernel entry from the loader (leaving EL2
//! if firmware ran there), exception vectors, MMU control, and the system
//! registers the rest of the kernel needs.
//!
//! The kernel crate uses this crate's modules directly (`arch-x86_64`
//! mirrors them name for name); the primitives arch-neutral crates need are
//! exported through the `arch/hal` hooks at the end of this file.
//!
//! Only [`cache`] builds for the UEFI target too; the loader uses it to make
//! the kernel image visible to instruction fetch before jumping to it.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod cache;

#[cfg(target_os = "none")]
pub mod backtrace;
#[cfg(target_os = "none")]
mod context;
#[cfg(target_os = "none")]
pub mod cpu;
#[cfg(target_os = "none")]
mod entry;
#[cfg(target_os = "none")]
pub mod fixup;
#[cfg(target_os = "none")]
pub mod fpu;
#[cfg(target_os = "none")]
pub mod gic;
#[cfg(target_os = "none")]
pub mod mmu;
#[cfg(target_os = "none")]
pub mod smp;
#[cfg(target_os = "none")]
pub mod timer;
#[cfg(target_os = "none")]
pub mod trap;
pub mod user;

pub use addr::{PhysAddr, VirtAddr};
#[cfg(target_os = "none")]
pub use cpu::cpu_id;
#[cfg(target_os = "none")]
pub use entry::booted_at_el2;
pub use hal::{FaultAccess, PageFault};
#[cfg(target_os = "none")]
pub use trap::{Interrupt, TrapReport};

/// The architecture name FreeBSD uses (`uname -m`).
pub const NAME: &str = "arm64";

/// Defines the kernel's entry point and fatal-trap handler.
///
/// ```ignore
/// arch::kernel_hooks!(
///     main = kmain,
///     ap_main = ap_main,
///     fatal_trap = on_fatal_trap,
///     interrupt = on_irq,
/// );
/// fn kmain(boot_info: arch::PhysAddr) -> ! { … }
/// fn ap_main(arg: usize) -> ! { … }
/// fn on_fatal_trap(report: &arch::TrapReport) -> ! { … }
/// fn on_irq(irq: arch::Interrupt) { … }
/// ```
#[macro_export]
macro_rules! kernel_hooks {
    (main = $main:path, ap_main = $ap:path, fatal_trap = $trap:path, interrupt = $irq:path, page_fault = $pf:path $(,)?) => {
        #[unsafe(no_mangle)]
        fn abyss_kernel_main(boot_info: $crate::PhysAddr) -> ! {
            $main(boot_info)
        }

        #[unsafe(no_mangle)]
        fn abyss_ap_main(arg: usize) -> ! {
            $ap(arg)
        }

        #[unsafe(no_mangle)]
        fn abyss_fatal_trap(report: &$crate::TrapReport) -> ! {
            $trap(report)
        }

        #[unsafe(no_mangle)]
        fn abyss_interrupt(irq: $crate::Interrupt) {
            $irq(irq)
        }

        #[unsafe(no_mangle)]
        fn abyss_page_fault(fault: &$crate::PageFault) -> bool {
            $pf(fault)
        }
    };
}

#[cfg(target_os = "none")]
unsafe extern "Rust" {
    /// Defined by [`kernel_hooks!`].
    fn abyss_kernel_main(boot_info: PhysAddr) -> !;
    /// Defined by [`kernel_hooks!`].
    fn abyss_ap_main(arg: usize) -> !;
    /// Defined by [`kernel_hooks!`].
    fn abyss_fatal_trap(report: &TrapReport) -> !;
    /// Defined by [`kernel_hooks!`].
    fn abyss_interrupt(irq: Interrupt);
    /// Defined by [`kernel_hooks!`]: whether the VM resolved the fault.
    fn abyss_page_fault(fault: &hal::PageFault) -> bool;
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

/// The `arch/hal` hooks.
#[cfg(target_os = "none")]
mod hal_hooks {
    const _: hal::IrqDisableFn = abyss_hal_irq_disable;
    const _: hal::IrqEnableFn = abyss_hal_irq_enable;
    const _: hal::CpuIdFn = abyss_hal_cpu_id;
    const _: hal::ContextInitFn = abyss_hal_context_init;
    const _: hal::CpuLocalFn = abyss_hal_cpu_local;
    const _: hal::SetCpuLocalFn = abyss_hal_set_cpu_local;
    const _: hal::WaitForInterruptFn = abyss_hal_wait_for_interrupt;
    const _: hal::CurrentThreadFn = abyss_hal_current_thread;
    const _: hal::CriticalEnterFn = abyss_hal_critical_enter;
    const _: hal::CriticalExitFn = abyss_hal_critical_exit;
    const _: hal::CriticalDepthFn = abyss_hal_critical_depth;
    const _: hal::OwePreemptFn = abyss_hal_owe_preempt;
    const _: hal::PreemptOwedFn = abyss_hal_preempt_owed;
    const _: hal::InterruptNestFn = abyss_hal_interrupt_nest;
    const _: hal::SetCurrentThreadFn = abyss_hal_set_current_thread;
    const _: hal::FpuSizeFn = abyss_hal_fpu_size;
    const _: hal::FpuAreaFn = abyss_hal_fpu_init;
    const _: hal::FpuAreaFn = abyss_hal_fpu_save;
    const _: hal::FpuRestoreFn = abyss_hal_fpu_restore;

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
        crate::cpu::cpu_local()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_set_cpu_local(p: *mut ()) {
        crate::cpu::set_cpu_local(p);
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_current_thread() -> *const () {
        crate::cpu::current_thread()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_critical_enter() {
        crate::cpu::critical_enter();
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_critical_exit() -> bool {
        crate::cpu::critical_exit()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_critical_depth() -> u32 {
        crate::cpu::critical_depth()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_owe_preempt(owed: bool) {
        crate::cpu::owe_preempt(owed);
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_preempt_owed() -> bool {
        crate::cpu::preempt_owed()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_interrupt_nest(delta: i32) -> u32 {
        crate::cpu::interrupt_nest(delta)
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_set_current_thread(p: *const ()) {
        crate::cpu::set_current_thread(p);
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_wait_for_interrupt() {
        crate::cpu::wait_for_interrupt();
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_fpu_size() -> usize {
        crate::fpu::area_size()
    }

    /// # Safety
    ///
    /// See `hal::fpu::init`.
    #[unsafe(no_mangle)]
    unsafe fn abyss_hal_fpu_init(area: *mut u8) {
        // SAFETY: the hal's contract is the arch function's.
        unsafe { crate::fpu::init_area(area) }
    }

    /// # Safety
    ///
    /// See `hal::fpu::save`.
    #[unsafe(no_mangle)]
    unsafe fn abyss_hal_fpu_save(area: *mut u8) {
        // SAFETY: as above.
        unsafe { crate::fpu::save(area) }
    }

    /// # Safety
    ///
    /// See `hal::fpu::restore`.
    #[unsafe(no_mangle)]
    unsafe fn abyss_hal_fpu_restore(area: *const u8) {
        // SAFETY: as above.
        unsafe { crate::fpu::restore(area) }
    }
}
