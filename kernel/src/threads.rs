// SPDX-License-Identifier: BSD-2-Clause
//! The scheduler's platform hooks: kernel stacks from `mm::kstack`, the
//! CPU timestamp for accounting, and (later) IPIs.

use alloc::boxed::Box;

use crate::arch;

use crate::mm::kstack::KernelStack;

/// Pages per kernel thread stack (plus a guard page).
pub(crate) const STACK_PAGES: u64 = 4;

struct Platform;

impl sched::Platform for Platform {
    fn alloc_stack(&self, min_bytes: Option<usize>) -> Option<sched::Stack> {
        let pages = min_bytes.map_or(STACK_PAGES, |b| (b as u64).div_ceil(4096).max(STACK_PAGES));
        let stack = KernelStack::new(pages).ok()?;
        let top = stack.top().get();
        Some(sched::Stack {
            top,
            handle: Box::into_raw(Box::new(stack)) as usize,
        })
    }

    fn free_stack(&self, stack: sched::Stack) {
        // SAFETY: `handle` came from `Box::into_raw` in `alloc_stack`, and
        // the scheduler frees each stack once.
        drop(unsafe { Box::from_raw(stack.handle as *mut KernelStack) });
    }

    fn now(&self) -> u64 {
        crate::time::counter()
    }

    fn frequency(&self) -> u64 {
        crate::time::frequency()
    }

    fn kick(&self, cpu: hal::CpuId) {
        crate::intc::send_resched(cpu);
    }

    fn rearm_timer(&self) {
        crate::time::rearm();
    }
}

static PLATFORM: Platform = Platform;

/// Starts the scheduler on the boot CPU; the caller becomes thread
/// "kmain".
pub(crate) fn init() {
    sched::init(&PLATFORM);
    sched::lock::set_reporter(|args| kprintln!("{args}"));
    let irq = hal::irq::disable();
    sched::init_cpu("kmain");
    hal::irq::restore(irq);
    sched::spawn("reaper", sched::Priority::KERNEL, |s| sched::reaper(s)).expect("reaper thread");
    kprintln!("sched: boot CPU running threads");
}

/// Enables FP/SIMD on this CPU (the boot CPU also reports what it found).
pub(crate) fn init_fpu(boot: bool) {
    // SAFETY: once per CPU, before threads with FP state run here.
    unsafe { arch::fpu::init_cpu(boot) };
    if boot {
        let (kind, detail) = arch::fpu::describe();
        #[cfg(target_arch = "x86_64")]
        kprintln!(
            "fpu: {kind}, XCR0 {detail:#x}, {} byte save areas",
            arch::fpu::area_size()
        );
        #[cfg(target_arch = "aarch64")]
        kprintln!(
            "fpu: {kind}, {detail}-bit vectors, {} byte save areas",
            arch::fpu::area_size()
        );
    }
}
