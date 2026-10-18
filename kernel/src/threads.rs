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
    fn alloc_stack(&self) -> Option<sched::Stack> {
        let stack = KernelStack::new(STACK_PAGES).ok()?;
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
        arch::cpu::timestamp()
    }

    fn kick(&self, _cpu: hal::CpuId) {}
}

static PLATFORM: Platform = Platform;

/// Starts the scheduler on the boot CPU; the caller becomes thread
/// "kmain".
pub(crate) fn init() {
    sched::init(&PLATFORM);
    let irq = hal::irq::disable();
    sched::init_cpu("kmain");
    hal::irq::restore(irq);
    sched::spawn("reaper", sched::Priority::KERNEL, || sched::reaper()).expect("reaper thread");
    kprintln!("sched: boot CPU running threads");
}
