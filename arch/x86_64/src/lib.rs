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
pub mod fpu;
mod gdt;
pub mod mmu;
pub mod smp;
pub mod trap;
pub mod user;

pub use addr::{PhysAddr, VirtAddr};
pub use gdt::{CpuBlock, cpu_id};
pub use hal::{FaultAccess, PageFault};
pub use trap::{Interrupt, TrapReport};

/// The architecture name FreeBSD uses (`uname -m`).
pub const NAME: &str = "amd64";

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
    (main = $main:path, ap_main = $ap:path, fatal_trap = $trap:path, interrupt = $irq:path, page_fault = $pf:path, syscall = $sc:path, user_trap = $ut:path, user_return = $ur:path $(,)?) => {
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

        #[unsafe(no_mangle)]
        fn abyss_syscall(frame: &mut $crate::trap::TrapFrame) {
            $sc(frame)
        }

        #[unsafe(no_mangle)]
        fn abyss_user_trap(report: &$crate::TrapReport, frame: &mut $crate::trap::TrapFrame) {
            $ut(report, frame)
        }

        #[unsafe(no_mangle)]
        fn abyss_user_return(frame: &mut $crate::trap::TrapFrame) -> bool {
            $ur(frame)
        }
    };
}

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
    /// Defined by [`kernel_hooks!`]: a system call from user mode, with
    /// interrupts masked; returns with them masked.
    fn abyss_syscall(frame: &mut trap::TrapFrame);
    /// Defined by [`kernel_hooks!`]: a trap from user mode the kernel did
    /// not resolve (a signal, once there are signals). Interrupts masked.
    fn abyss_user_trap(report: &TrapReport, frame: &mut trap::TrapFrame);
    /// Defined by [`kernel_hooks!`]: the last word before `frame` returns
    /// to user mode, on every path but NMIs, double faults, and machine
    /// checks (pending signals, FreeBSD's `ast`). Interrupts masked, and
    /// masked again on return. Returns whether it rewrote the frame (which
    /// then needs a full restore).
    fn abyss_user_return(frame: &mut trap::TrapFrame) -> bool;
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
    const _: hal::SetU64Fn = abyss_hal_set_kernel_stack;
    const _: hal::GetU64Fn = abyss_hal_user_tls;
    const _: hal::SetU64Fn = abyss_hal_set_user_tls;

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
    fn abyss_hal_current_thread() -> *const () {
        crate::gdt::current_thread()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_critical_enter() {
        crate::gdt::critical_enter();
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_critical_exit() -> bool {
        crate::gdt::critical_exit()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_critical_depth() -> u32 {
        crate::gdt::critical_depth()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_owe_preempt(owed: bool) {
        crate::gdt::owe_preempt(owed);
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_preempt_owed() -> bool {
        crate::gdt::preempt_owed()
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_interrupt_nest(delta: i32) -> u32 {
        crate::gdt::interrupt_nest(delta)
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_set_current_thread(p: *const ()) {
        crate::gdt::set_current_thread(p);
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

    /// # Safety
    ///
    /// See `hal::user::set_kernel_stack`.
    #[unsafe(no_mangle)]
    unsafe fn abyss_hal_set_kernel_stack(top: u64) {
        crate::gdt::set_kernel_stack(top);
    }

    #[unsafe(no_mangle)]
    fn abyss_hal_user_tls() -> u64 {
        crate::user::tls()
    }

    /// # Safety
    ///
    /// See `hal::user::set_tls`.
    #[unsafe(no_mangle)]
    unsafe fn abyss_hal_set_user_tls(v: u64) {
        // SAFETY: the hal's contract is the arch function's.
        unsafe { crate::user::set_tls(v) }
    }
}
