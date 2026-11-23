// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's side of the Direct Rendering Manager core (`sys/drm`): the caller's memory
//! for requests that carry pointers, the page pool buffers draw on, and
//! the console for what the core refuses.

use abi::errno::Errno;

/// User memory, for `VERSION`'s name buffers.
struct UserCopies;

impl drm::UserMemory for UserCopies {
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        crate::uaccess::copy_in(dst, src).map_err(|_| Errno::EFAULT)
    }

    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        crate::uaccess::copy_out(dst, src).map_err(|_| Errno::EFAULT)
    }
}

/// A frame timer: a softclock callout, so its handler runs in thread
/// context and may take sleeping locks.
struct FrameTimer(core::pin::Pin<alloc::boxed::Box<crate::softclock::Callout>>);

impl drm::Ticker for FrameTimer {
    fn arm(&self, delay_ns: u64, f: fn(&sched::Sleepable, usize), arg: usize) {
        self.0.as_ref().reset(delay_ns, f, arg);
    }

    fn cancel(&self) {
        self.0.stop();
    }
}

/// Gives the core its environment; before a display driver registers.
pub(crate) fn init() {
    drm::init(drm::Env {
        user: &UserCopies,
        pages: crate::mm::space::page_ops(),
        log: |args| kprintln!("{args}"),
        now_ns: crate::time::now_ns,
        ticker: || alloc::boxed::Box::new(FrameTimer(crate::softclock::Callout::new())),
    });
}
