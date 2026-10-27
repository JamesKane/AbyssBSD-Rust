// SPDX-License-Identifier: BSD-2-Clause
//! The architecture seam: the CPU primitives that arch-neutral kernel
//! crates (`sys/*`, `drivers/*`) need, without naming an architecture.
//!
//! Binding is static and happens at link time: this crate declares
//! `extern "Rust"` hooks, and the one `arch/<isa>` crate linked into the
//! kernel defines them (checked against the `*Fn` types below). On the
//! host, where `sys/*` crates run their unit tests, the hooks are inert:
//! one CPU, interrupts always "off".
//!
//! Richer per-architecture interfaces (page tables, trap frames, context
//! switching) are reached by the kernel crate through the concrete arch
//! crate; this crate holds only what lower layers must share.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::fmt;

/// A logical CPU number, dense from 0 (the boot CPU).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CpuId(pub u32);

impl CpuId {
    /// The boot CPU.
    pub const BOOT: Self = Self(0);

    /// The number as an index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for CpuId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cpu{}", self.0)
    }
}

/// The most CPUs the kernel supports.
pub const MAX_CPUS: usize = 64;

/// What a faulting access tried to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultAccess {
    Read,
    Write,
    Execute,
}

/// A page fault, offered to the kernel's VM before it is treated as a
/// fixup or as fatal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFault {
    /// The faulting address.
    pub address: u64,
    pub access: FaultAccess,
    /// From user mode.
    pub user: bool,
    /// From kernel code allowed to fault (a user-copy routine).
    pub in_copy: bool,
    /// Interrupts were enabled where the fault happened, so resolving it
    /// may sleep.
    pub interrupts_enabled: bool,
}

/// Saved callee-saved register state of a thread that is not running.
/// The layout is the arch crate's business; 16 words fit both ISAs.
#[repr(C, align(16))]
#[derive(Clone, Debug, Default)]
pub struct Context {
    regs: [u64; 16],
}

impl Context {
    /// The raw words, for the arch crate that defines their meaning.
    pub fn words_mut(&mut self) -> &mut [u64; 16] {
        &mut self.regs
    }
}

/// A new thread's first function: called with the argument given to
/// [`Context::init`], on the fresh stack, with interrupts masked.
pub type ThreadEntry = extern "C" fn(usize) -> !;

impl Context {
    /// A context that, when switched to, calls `entry(arg)` on the stack
    /// whose top (16-byte aligned) is `stack_top`.
    #[must_use]
    pub fn init(stack_top: u64, entry: ThreadEntry, arg: usize) -> Self {
        #[cfg_attr(not(target_os = "none"), allow(unused_mut))]
        let mut c = Self::default();
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        unsafe {
            abyss_hal_context_init(&mut c, stack_top, entry, arg);
        }
        #[cfg(not(target_os = "none"))]
        let _ = (stack_top, entry, arg);
        c
    }
}

/// Saves the current thread's registers into `from` and resumes `to`.
/// Returns when something switches back to `from`.
///
/// # Safety
///
/// Interrupts must be masked. `to` must hold a context saved by this
/// function or made by [`Context::init`], whose stack is still mapped and
/// not in use by any other CPU, and `from` must stay valid until it is
/// resumed.
pub unsafe fn switch(from: *mut Context, to: *const Context) {
    // SAFETY: per the caller's contract; the symbol is the arch crate's
    // assembly routine with this C signature.
    #[cfg(target_os = "none")]
    unsafe {
        abyss_context_switch(from, to);
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (from, to);
        unimplemented!("context switching needs a kernel target");
    }
}

/// The per-CPU pointer the kernel keeps for this CPU (null until set).
#[inline]
#[must_use]
pub fn cpu_local() -> *mut () {
    #[cfg(target_os = "none")]
    // SAFETY: defined by the linked arch crate with this signature.
    return unsafe { abyss_hal_cpu_local() };
    #[cfg(not(target_os = "none"))]
    core::ptr::null_mut()
}

/// Sets this CPU's kernel per-CPU pointer.
///
/// # Safety
///
/// `p` must stay valid for as long as anything on this CPU may read it.
pub unsafe fn set_cpu_local(p: *mut ()) {
    #[cfg(target_os = "none")]
    // SAFETY: defined by the linked arch crate with this signature.
    unsafe {
        abyss_hal_set_cpu_local(p);
    }
    #[cfg(not(target_os = "none"))]
    let _ = p;
}

/// The running thread's pointer, as last set on this CPU by
/// [`set_current_thread`] (null until then). Safe to call with interrupts
/// enabled: a preemption cannot make it return another CPU's thread.
#[inline]
#[must_use]
pub fn current_thread() -> *const () {
    #[cfg(target_os = "none")]
    // SAFETY: defined by the linked arch crate with this signature.
    return unsafe { abyss_hal_current_thread() };
    #[cfg(not(target_os = "none"))]
    core::ptr::null()
}

/// Records the thread now running on this CPU.
///
/// # Safety
///
/// Call with interrupts masked, as part of a context switch; `p` must
/// stay valid while it runs here (readers of [`current_thread`] trust it).
pub unsafe fn set_current_thread(p: *const ()) {
    #[cfg(target_os = "none")]
    // SAFETY: defined by the linked arch crate with this signature.
    unsafe {
        abyss_hal_set_current_thread(p);
    }
    #[cfg(not(target_os = "none"))]
    let _ = p;
}

/// Waits until an interrupt is pending. Call with interrupts masked; it
/// returns with them masked, possibly after servicing the interrupt.
pub fn wait_for_interrupt() {
    #[cfg(target_os = "none")]
    // SAFETY: defined by the linked arch crate with this signature.
    unsafe {
        abyss_hal_wait_for_interrupt();
    }
}

/// FP/SIMD register state, saved and restored for threads that use it
/// (the kernel itself is soft-float).
pub mod fpu {
    /// Bytes in a save area.
    #[must_use]
    pub fn area_size() -> usize {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        return unsafe { super::abyss_hal_fpu_size() };
        #[cfg(not(target_os = "none"))]
        0
    }

    /// The alignment save areas need.
    pub const ALIGN: usize = 64;

    /// Fills `area` with the initial state.
    ///
    /// # Safety
    ///
    /// `area` is [`area_size`] writable bytes aligned to [`ALIGN`].
    pub unsafe fn init(area: *mut u8) {
        #[cfg(target_os = "none")]
        // SAFETY: per the contract.
        unsafe {
            super::abyss_hal_fpu_init(area);
        }
        #[cfg(not(target_os = "none"))]
        let _ = area;
    }

    /// Saves this CPU's FP/SIMD registers into `area`.
    ///
    /// # Safety
    ///
    /// As for [`init`].
    pub unsafe fn save(area: *mut u8) {
        #[cfg(target_os = "none")]
        // SAFETY: per the contract.
        unsafe {
            super::abyss_hal_fpu_save(area);
        }
        #[cfg(not(target_os = "none"))]
        let _ = area;
    }

    /// Loads this CPU's FP/SIMD registers from `area`.
    ///
    /// # Safety
    ///
    /// As for [`init`]; `area` holds a state from [`init`] or [`save`].
    pub unsafe fn restore(area: *const u8) {
        #[cfg(target_os = "none")]
        // SAFETY: per the contract.
        unsafe {
            super::abyss_hal_fpu_restore(area);
        }
        #[cfg(not(target_os = "none"))]
        let _ = area;
    }
}

/// Per-thread state of user mode that the scheduler switches: where traps
/// from user mode enter the kernel, and the user's thread pointer (TLS).
pub mod user {
    /// Sets the kernel stack traps from user mode switch to (x86_64's TSS
    /// `rsp0`; aarch64 needs nothing: `SP_EL1` stays the thread's).
    ///
    /// # Safety
    ///
    /// Call with interrupts masked, for the thread about to run here;
    /// `top` is the top of its kernel stack.
    pub unsafe fn set_kernel_stack(top: u64) {
        #[cfg(target_os = "none")]
        // SAFETY: per the contract.
        unsafe {
            super::abyss_hal_set_kernel_stack(top);
        }
        #[cfg(not(target_os = "none"))]
        let _ = top;
    }

    /// This CPU's user thread pointer (`FS` base; `TPIDR_EL0`).
    #[must_use]
    pub fn tls() -> u64 {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        return unsafe { super::abyss_hal_user_tls() };
        #[cfg(not(target_os = "none"))]
        0
    }

    /// Sets this CPU's user thread pointer.
    ///
    /// # Safety
    ///
    /// Call for the thread running (or about to run) here; the kernel
    /// itself does not use the register.
    pub unsafe fn set_tls(v: u64) {
        #[cfg(target_os = "none")]
        // SAFETY: per the contract.
        unsafe {
            super::abyss_hal_set_user_tls(v);
        }
        #[cfg(not(target_os = "none"))]
        let _ = v;
    }

    /// This CPU's second user thread register: amd64's GS base; arm64 has
    /// none (0).
    #[must_use]
    pub fn tls2() -> u64 {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        return unsafe { super::abyss_hal_user_tls2() };
        #[cfg(not(target_os = "none"))]
        0
    }

    /// Sets this CPU's second user thread register.
    ///
    /// # Safety
    ///
    /// As for [`set_tls`].
    pub unsafe fn set_tls2(v: u64) {
        #[cfg(target_os = "none")]
        // SAFETY: per the contract.
        unsafe {
            super::abyss_hal_set_user_tls2(v);
        }
        #[cfg(not(target_os = "none"))]
        let _ = v;
    }
}

/// Signature of `abyss_hal_set_kernel_stack`, `abyss_hal_set_user_tls`.
pub type SetU64Fn = unsafe fn(u64);
/// Signature of `abyss_hal_user_tls`.
pub type GetU64Fn = fn() -> u64;

/// Signature of `abyss_hal_fpu_size`.
pub type FpuSizeFn = fn() -> usize;
/// Signature of `abyss_hal_fpu_init`, `abyss_hal_fpu_save`.
pub type FpuAreaFn = unsafe fn(*mut u8);
/// Signature of `abyss_hal_fpu_restore`.
pub type FpuRestoreFn = unsafe fn(*const u8);

/// Signature of `abyss_hal_context_init`.
pub type ContextInitFn = fn(&mut Context, u64, ThreadEntry, usize);
/// Signature of `abyss_hal_cpu_local`.
pub type CpuLocalFn = fn() -> *mut ();
/// Signature of `abyss_hal_set_cpu_local`.
pub type SetCpuLocalFn = fn(*mut ());
/// Signature of `abyss_hal_wait_for_interrupt`.
pub type WaitForInterruptFn = fn();
/// Signature of `abyss_hal_critical_enter`.
pub type CriticalEnterFn = fn();
/// Signature of `abyss_hal_critical_exit`: true when the outermost section
/// ended.
pub type CriticalExitFn = fn() -> bool;
/// Signature of `abyss_hal_critical_depth`.
pub type CriticalDepthFn = fn() -> u32;
/// Signature of `abyss_hal_owe_preempt`.
pub type OwePreemptFn = fn(bool);
/// Signature of `abyss_hal_preempt_owed`.
pub type PreemptOwedFn = fn() -> bool;
/// Signature of `abyss_hal_interrupt_nest`: adds `delta` to this CPU's
/// interrupt nesting and returns the new value.
pub type InterruptNestFn = fn(i32) -> u32;
/// Signature of `abyss_hal_current_thread`.
pub type CurrentThreadFn = fn() -> *const ();
/// Signature of `abyss_hal_set_current_thread`.
pub type SetCurrentThreadFn = fn(*const ());

/// Signature of `abyss_hal_irq_disable`: masks interrupts on this CPU and
/// returns whether they were enabled.
pub type IrqDisableFn = fn() -> bool;
/// Signature of `abyss_hal_irq_enable`: unmasks interrupts on this CPU.
pub type IrqEnableFn = fn();
/// Signature of `abyss_hal_cpu_id`: the current CPU's number.
pub type CpuIdFn = fn() -> u32;

#[cfg(target_os = "none")]
unsafe extern "Rust" {
    fn abyss_hal_irq_disable() -> bool;
    fn abyss_hal_irq_enable();
    fn abyss_hal_cpu_id() -> u32;
    fn abyss_hal_context_init(c: &mut Context, stack_top: u64, entry: ThreadEntry, arg: usize);
    fn abyss_hal_cpu_local() -> *mut ();
    fn abyss_hal_set_cpu_local(p: *mut ());
    fn abyss_hal_wait_for_interrupt();
    fn abyss_hal_current_thread() -> *const ();
    fn abyss_hal_critical_enter();
    fn abyss_hal_critical_exit() -> bool;
    fn abyss_hal_critical_depth() -> u32;
    fn abyss_hal_owe_preempt(owed: bool);
    fn abyss_hal_preempt_owed() -> bool;
    fn abyss_hal_interrupt_nest(delta: i32) -> u32;
    fn abyss_hal_set_current_thread(p: *const ());
    fn abyss_hal_fpu_size() -> usize;
    fn abyss_hal_fpu_init(area: *mut u8);
    fn abyss_hal_fpu_save(area: *mut u8);
    fn abyss_hal_fpu_restore(area: *const u8);
    fn abyss_hal_set_kernel_stack(top: u64);
    fn abyss_hal_user_tls() -> u64;
    fn abyss_hal_set_user_tls(v: u64);
    fn abyss_hal_user_tls2() -> u64;
    fn abyss_hal_set_user_tls2(v: u64);
}

#[cfg(target_os = "none")]
unsafe extern "C" {
    fn abyss_context_switch(from: *mut Context, to: *const Context);
}

/// Critical sections: preemption held off on this CPU without masking
/// interrupts (FreeBSD's `critical_enter`). Masking interrupts is costly on
/// some CPUs (about 12 ns a pair on Alder Lake under bhyve); a critical
/// section is a per-CPU counter changed with one instruction.
///
/// An interrupt that wants to preempt inside a critical section records
/// that it owes one ([`owe_preempt`]); leaving the outermost section then
/// runs the scheduler's handler ([`set_preempt_handler`]). Code in a
/// critical section must not sleep. Interrupt handlers may still run, so
/// per-CPU data used in a critical section must not be touched from
/// interrupt context (see [`crate::interrupt::active`]).
pub mod critical {
    use core::sync::atomic::{AtomicPtr, Ordering};

    static HANDLER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

    /// Enters a critical section (nests).
    #[inline]
    pub fn enter() {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        unsafe {
            super::abyss_hal_critical_enter();
        }
    }

    /// Leaves a critical section; at the outermost level, runs an owed
    /// preemption.
    #[inline]
    pub fn exit() {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        if unsafe { super::abyss_hal_critical_exit() } && unsafe { super::abyss_hal_preempt_owed() }
        {
            let h = HANDLER.load(Ordering::Acquire);
            if !h.is_null() {
                // SAFETY: only `set_preempt_handler` stores here, a `fn()`.
                let f: fn() = unsafe { core::mem::transmute::<*mut (), fn()>(h) };
                f();
            }
        }
    }

    /// The nesting depth on this CPU.
    #[inline]
    #[must_use]
    pub fn depth() -> u32 {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        return unsafe { super::abyss_hal_critical_depth() };
        #[cfg(not(target_os = "none"))]
        0
    }

    /// Records (or clears) an owed preemption on this CPU. Interrupts must
    /// be masked.
    #[inline]
    pub fn owe_preempt(owed: bool) {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        unsafe {
            super::abyss_hal_owe_preempt(owed);
        }
        #[cfg(not(target_os = "none"))]
        let _ = owed;
    }

    /// Sets what [`exit`] runs when a preemption is owed.
    pub fn set_preempt_handler(f: fn()) {
        HANDLER.store(f as *mut (), Ordering::Release);
    }
}

/// Interrupt-context tracking, maintained by the kernel's dispatcher.
pub mod interrupt {
    /// An interrupt handler starts on this CPU (interrupts masked).
    #[inline]
    pub fn enter() {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        unsafe {
            super::abyss_hal_interrupt_nest(1);
        }
    }

    /// The handler is done (interrupts masked).
    #[inline]
    pub fn exit() {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        unsafe {
            super::abyss_hal_interrupt_nest(-1);
        }
    }

    /// Whether this CPU is running an interrupt handler. In thread context
    /// a racing migration can make this read another CPU's state and
    /// answer true; callers only use a true answer to take a slower, safe
    /// path.
    #[inline]
    #[must_use]
    pub fn active() -> bool {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        return unsafe { super::abyss_hal_interrupt_nest(0) } != 0;
        #[cfg(not(target_os = "none"))]
        false
    }
}

/// Interrupt masking on the current CPU.
pub mod irq {
    /// Interrupt state saved by [`disable`]; restore it with [`restore`].
    #[must_use = "interrupts stay masked until the state is restored"]
    #[derive(Debug)]
    pub struct Saved {
        were_enabled: bool,
    }

    /// Masks interrupts, returning the previous state.
    #[inline]
    pub fn disable() -> Saved {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        let were_enabled = unsafe { super::abyss_hal_irq_disable() };
        #[cfg(not(target_os = "none"))]
        let were_enabled = false;
        Saved { were_enabled }
    }

    /// Restores the state [`disable`] saved.
    #[inline]
    pub fn restore(saved: Saved) {
        if saved.were_enabled {
            // SAFETY: as above.
            #[cfg(target_os = "none")]
            unsafe {
                super::abyss_hal_irq_enable();
            }
        }
    }

    /// Unmasks interrupts unconditionally (a new thread's first step).
    #[inline]
    pub fn enable() {
        #[cfg(target_os = "none")]
        // SAFETY: defined by the linked arch crate with this signature.
        unsafe {
            super::abyss_hal_irq_enable();
        }
    }

    /// Whether `saved` recorded interrupts as enabled.
    #[must_use]
    pub fn were_enabled(saved: &Saved) -> bool {
        saved.were_enabled
    }
}

/// The current CPU. Only stable while the caller cannot migrate
/// (interrupts masked, or preemption disabled).
#[inline]
#[must_use]
pub fn cpu_id() -> CpuId {
    #[cfg(target_os = "none")]
    // SAFETY: defined by the linked arch crate with this signature.
    let id = unsafe { abyss_hal_cpu_id() };
    #[cfg(not(target_os = "none"))]
    let id = 0;
    CpuId(id)
}
