// SPDX-License-Identifier: BSD-2-Clause
//! Execution-context tokens.
//!
//! Whether code may sleep is a property of where it runs, so it is carried
//! as a value: functions that can block take a [`Sleepable`], which only
//! thread context has. Each thread's entry function receives one from the
//! scheduler; interrupt handlers receive an [`IrqCx`] instead, which cannot
//! produce one. Blocking in a timer callback or an interrupt filter is
//! then a type error rather than a hang (docs/DRIVER-MODEL.md §6.6). The
//! tokens are zero-sized and neither `Send` nor `Sync`, so they cannot
//! leave the context they describe.
//!
//! ```compile_fail,E0308
//! use sched::lock::{LockClass, Mutex};
//! static CLASS: LockClass = LockClass::new("example", 1);
//! static M: Mutex<u32> = Mutex::new(&CLASS, 0);
//! // An interrupt handler cannot take a sleepable lock.
//! fn handler(cx: &sched::IrqCx) {
//!     *M.lock(cx) += 1;
//! }
//! ```
//!
//! Some rules stay runtime checks (the witness): a thread holding a
//! `Sleepable` must still not sleep inside a spin lock or a critical
//! section.

use core::fmt;
use core::marker::PhantomData;

/// Proof that the holder runs in thread context and may sleep.
pub struct Sleepable {
    _not_send: PhantomData<*const ()>,
}

impl Sleepable {
    /// Asserts thread context.
    ///
    /// # Safety
    ///
    /// The caller runs in a thread (not an interrupt handler) that may
    /// sleep for as long as the token is used. Code that has a `Sleepable`
    /// should pass it on instead; this is for the few places a thread
    /// starts without one (the boot thread).
    #[must_use]
    pub const unsafe fn assume() -> Self {
        Self {
            _not_send: PhantomData,
        }
    }
}

impl fmt::Debug for Sleepable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sleepable")
    }
}

/// Proof that the holder runs in an interrupt handler: it must not sleep,
/// and it cannot produce a [`Sleepable`].
pub struct IrqCx {
    _not_send: PhantomData<*const ()>,
}

impl IrqCx {
    /// Asserts interrupt context.
    ///
    /// # Safety
    ///
    /// For the interrupt dispatcher only: the caller is handling an
    /// interrupt, with interrupts masked, for as long as the token is used.
    #[must_use]
    pub const unsafe fn assume() -> Self {
        Self {
            _not_send: PhantomData,
        }
    }
}

impl fmt::Debug for IrqCx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IrqCx")
    }
}
