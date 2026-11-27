// SPDX-License-Identifier: BSD-2-Clause
//! Who owns the FP/SIMD registers: loaded lazily, as FreeBSD's amd64
//! `fpu.c` does (`fpudna`, `fpudrop`, `fpugetregs`, `fpusetregs`).
//!
//! A thread with a save area is in one of three states:
//!
//! - `LIVE`: the registers on its CPU are its own, newer than the area.
//!   The CPU runs with CR0.TS clear.
//! - `AREA`: the area holds its state; the registers do not. Switching a
//!   thread in loads it (so it becomes `LIVE`); a thread that is `AREA`
//!   while running (a `sigreturn` set it) runs with TS set and loads on
//!   its first FP/SIMD instruction.
//! - `INIT`: its state is the initial one, nothing loaded, no area
//!   contents to keep (`fpstate_drop`: a signal handler, a new image).
//!   The thread runs with TS set; its first instruction loads the initial
//!   state.
//!
//! The rule that keeps it sound: a running thread with an area has TS
//! clear exactly when it is `LIVE`, so it never reads another thread's
//! registers. A switch out saves a `LIVE` thread (`AREA` afterwards); a
//! switch in loads `AREA` (`LIVE`) and sets TS for `INIT`. Every change
//! happens on the thread's own CPU with interrupts masked, or while the
//! thread is off CPU (a debugger's, ordered by the `on_cpu` handoff).
//!
//! Without [`hal::fpu::LAZY`] (arm64) every thread stays `LIVE`: saved on
//! every switch out and loaded on every switch in.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::{PerCpu, Thread, current, this_cpu};

pub(crate) const AREA: u8 = 0;
pub(crate) const LIVE: u8 = 1;
pub(crate) const INIT: u8 = 2;

/// The initial state in a save area, made once (the first save area asks)
/// and never changed.
static INITIAL: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

/// An area holding the initial state, or null before any thread has one.
pub(crate) fn initial() -> *const u8 {
    let p = INITIAL.load(Ordering::Acquire);
    if !p.is_null() {
        return p;
    }
    let Ok(layout) =
        core::alloc::Layout::from_size_align(hal::fpu::area_size().max(16), hal::fpu::ALIGN)
    else {
        return core::ptr::null();
    };
    // SAFETY: a nonzero size and a valid alignment.
    let fresh = unsafe { alloc::alloc::alloc(layout) };
    if fresh.is_null() {
        return core::ptr::null();
    }
    // SAFETY: a fresh allocation of the area's size and alignment.
    unsafe { hal::fpu::init(fresh) };
    match INITIAL.compare_exchange(
        core::ptr::null_mut(),
        fresh,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => fresh,
        Err(other) => {
            // SAFETY: allocated above with this layout, never shared.
            unsafe { alloc::alloc::dealloc(fresh, layout) };
            other
        }
    }
}

/// Sets or clears TS on `c`'s CPU, writing the register only when it
/// changes.
fn ts(c: &PerCpu, on: bool) {
    if c.fpu_ts.get() != on {
        hal::fpu::set_ts(on);
        c.fpu_ts.set(on);
    }
}

/// The part of a switch out that is FP/SIMD: a thread with live registers
/// saves them.
///
/// # Safety
///
/// `t` is the thread leaving this CPU, interrupts masked.
pub(crate) unsafe fn switch_out(t: &Thread) {
    let Some(f) = &t.fpu else { return };
    if !hal::fpu::LAZY {
        // SAFETY: the area is this thread's, of the HAL's size.
        unsafe { hal::fpu::save(f.as_ptr()) };
    } else if t.fpu_state.load(Ordering::Relaxed) == LIVE {
        // SAFETY: as above; `LIVE` means TS is clear.
        unsafe { hal::fpu::save(f.as_ptr()) };
        t.fpu_state.store(AREA, Ordering::Relaxed);
    }
}

/// The part of a switch in that is FP/SIMD, for the thread now running:
/// load its state, or set TS for one that has none to load.
pub(crate) fn switch_in(c: &PerCpu) {
    let t = current();
    let Some(f) = &t.fpu else { return };
    if !hal::fpu::LAZY {
        // SAFETY: the area holds this thread's state, saved when it last
        // left a CPU (or the initial state); `on_cpu` ordered that save
        // before this load.
        unsafe { hal::fpu::restore(f.as_ptr()) };
        return;
    }
    match t.fpu_state.load(Ordering::Relaxed) {
        INIT => ts(c, true),
        _ => {
            ts(c, false);
            // SAFETY: as above.
            unsafe { hal::fpu::restore(f.as_ptr()) };
            t.fpu_state.store(LIVE, Ordering::Relaxed);
        }
    }
}

/// #NM: the running thread used an FP/SIMD instruction with TS set. Loads
/// its state (`fpudna`). Returns false if it has no area to load from.
pub fn device_not_available() -> bool {
    if !hal::fpu::LAZY {
        return false;
    }
    let irq = hal::irq::disable();
    let t = current();
    let c = this_cpu();
    let done = match &t.fpu {
        None => false,
        Some(f) => {
            // TS is cleared whatever the shadow says: a hypervisor may set it
            // at odd moments, and a load needs it clear.
            hal::fpu::set_ts(false);
            c.fpu_ts.set(false);
            match t.fpu_state.load(Ordering::Relaxed) {
                LIVE => {}
                state => {
                    let src = if state == INIT {
                        initial()
                    } else {
                        f.as_ptr().cast_const()
                    };
                    // SAFETY: the initial image or the thread's own area.
                    unsafe { hal::fpu::restore(src) };
                    t.fpu_state.store(LIVE, Ordering::Relaxed);
                }
            }
            true
        }
    };
    hal::irq::restore(irq);
    done
}

/// Puts the running thread's state back to the initial one without
/// loading anything (`fpstate_drop`): a signal handler starts with it, and
/// the next FP/SIMD instruction loads it. amd64 only.
pub fn drop_state() {
    let irq = hal::irq::disable();
    let t = current();
    if t.fpu.is_some() && hal::fpu::LAZY {
        t.fpu_state.store(INIT, Ordering::Relaxed);
        ts(this_cpu(), true);
    }
    hal::irq::restore(irq);
}

/// Copies the running thread's FP/SIMD state to `dst` (a save area's size
/// and alignment), without loading or dropping anything; registers that
/// are the thread's are saved to it, which is all the cost.
///
/// # Safety
///
/// `dst` is `hal::fpu::area_size()` writable bytes aligned to
/// `hal::fpu::ALIGN`.
pub unsafe fn read_state(dst: *mut u8) {
    let irq = hal::irq::disable();
    let t = current();
    match &t.fpu {
        Some(_) if t.fpu_state.load(Ordering::Relaxed) == LIVE => {
            // SAFETY: per the contract; `LIVE` means TS is clear.
            unsafe { hal::fpu::save(dst) };
        }
        Some(f) => {
            let src = if t.fpu_state.load(Ordering::Relaxed) == INIT {
                initial()
            } else {
                f.as_ptr().cast_const()
            };
            if !src.is_null() {
                // SAFETY: both are save areas of the HAL's size; they do
                // not overlap (`dst` is the caller's).
                unsafe { core::ptr::copy_nonoverlapping(src, dst, hal::fpu::area_size()) };
            }
        }
        // No area: the registers are not this thread's, so there is no
        // state of its own to give; the initial one.
        // SAFETY: per the contract.
        None => unsafe { hal::fpu::init(dst) },
    }
    hal::irq::restore(irq);
}

/// Makes `src` the running thread's FP/SIMD state: loaded now if its
/// registers are live (`fpusetregs`), otherwise kept in its area for the
/// first instruction (or the next switch in) to load.
///
/// # Safety
///
/// `src` is a save area's size, aligned to `hal::fpu::ALIGN`, in a state
/// the CPU loads without faulting (saved, initial, or checked).
pub unsafe fn write_state(src: *const u8) {
    let irq = hal::irq::disable();
    let t = current();
    if let Some(f) = &t.fpu {
        if !hal::fpu::LAZY || t.fpu_state.load(Ordering::Relaxed) == LIVE {
            // SAFETY: per the contract; `LIVE` means TS is clear.
            unsafe { hal::fpu::restore(src) };
        } else {
            // SAFETY: both are save areas of the HAL's size; they do not
            // overlap (`src` is the caller's).
            unsafe { core::ptr::copy_nonoverlapping(src, f.as_ptr(), hal::fpu::area_size()) };
            t.fpu_state.store(AREA, Ordering::Relaxed);
            ts(this_cpu(), true);
        }
    }
    hal::irq::restore(irq);
}

/// A new thread's state copied from the running one, into `area` (the new
/// thread's, holding the initial state): what `fork` gives the child.
/// Returns the child's state.
///
/// # Safety
///
/// `area` is a fresh save area of the HAL's size.
pub(crate) unsafe fn inherit(area: *mut u8) -> u8 {
    let irq = hal::irq::disable();
    let t = current();
    let state = match &t.fpu {
        Some(_) if !hal::fpu::LAZY || t.fpu_state.load(Ordering::Relaxed) == LIVE => {
            // SAFETY: per the contract; `LIVE` means TS is clear.
            unsafe { hal::fpu::save(area) };
            AREA
        }
        Some(f) if t.fpu_state.load(Ordering::Relaxed) == AREA => {
            // SAFETY: save areas of the HAL's size, distinct.
            unsafe { core::ptr::copy_nonoverlapping(f.as_ptr(), area, hal::fpu::area_size()) };
            AREA
        }
        // The initial state, which `area` already holds.
        _ => INIT,
    };
    hal::irq::restore(irq);
    state
}

/// `t`'s area, holding its state, for a debugger to read or change while
/// it is stopped: an `INIT` thread's initial state is put there first, and
/// a thread that is the caller with live registers saves them.
pub(crate) fn materialize(t: &Thread) {
    if !hal::fpu::LAZY {
        return;
    }
    let Some(f) = &t.fpu else { return };
    let irq = hal::irq::disable();
    match t.fpu_state.load(Ordering::Relaxed) {
        INIT => {
            let init = initial();
            if !init.is_null() {
                // SAFETY: save areas of the HAL's size, distinct.
                unsafe { core::ptr::copy_nonoverlapping(init, f.as_ptr(), hal::fpu::area_size()) };
                t.fpu_state.store(AREA, Ordering::Relaxed);
            }
        }
        LIVE if core::ptr::eq(t, current()) => {
            // SAFETY: the running thread's own area; `LIVE` means TS is
            // clear. It stays `LIVE`: the registers are still its own.
            unsafe { hal::fpu::save(f.as_ptr()) };
        }
        _ => {}
    }
    hal::irq::restore(irq);
}

/// Opens a kernel FP/SIMD section (`crate::simd`), preemption held off:
/// TS cleared, live registers saved. Returns whether the registers were
/// live and what TS was.
pub(crate) fn simd_enter() -> (bool, bool) {
    let c = this_cpu();
    let t = current();
    let was_ts = c.fpu_ts.get();
    let mut was_live = false;
    if hal::fpu::LAZY {
        ts(c, false);
    }
    if let Some(f) = &t.fpu
        && (!hal::fpu::LAZY || t.fpu_state.load(Ordering::Relaxed) == LIVE)
    {
        // SAFETY: the area is this thread's, of the size and alignment the
        // HAL wants; saving only reads the registers, and TS is clear.
        unsafe { hal::fpu::save(f.as_ptr()) };
        t.fpu_state.store(AREA, Ordering::Relaxed);
        was_live = true;
    }
    (was_live, was_ts)
}

/// Ends the section: loads back what `simd_enter` saved, or restores TS.
pub(crate) fn simd_leave(was_live: bool, was_ts: bool) {
    let c = this_cpu();
    let t = current();
    if was_live && let Some(f) = &t.fpu {
        // SAFETY: the area holds the state `simd_enter` saved, and no
        // switch has happened since (preemption is held off).
        unsafe { hal::fpu::restore(f.as_ptr()) };
        t.fpu_state.store(LIVE, Ordering::Relaxed);
    } else if hal::fpu::LAZY {
        ts(c, was_ts);
    }
}
