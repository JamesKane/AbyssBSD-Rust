// SPDX-License-Identifier: BSD-2-Clause
//! The shared page: one page the kernel writes, mapped read-only into
//! every process (at a random address, see [`crate::exec`]; FreeBSD's
//! shared page). It holds the time keeping, `struct vdso_timekeep`, from
//! which libc answers `clock_gettime` and `gettimeofday` without a system
//! call (`AT_TIMEKEEP` points at it), and the signal trampoline, at its
//! start. On amd64 the trampoline is the code of a vdso, a small shared
//! object rtld preloads as `[vdso]` (`AT_KPRELOAD` points at it), whose
//! call frame information lets unwinders step through a signal frame
//! (FreeBSD's `SV_DSO_SIG`; arm64 has none).
//!
//! Time keeping follows FreeBSD's timecounters:
//! - Readers take the counter's low 32 bits (on amd64 after shifting the
//!   TSC right, so it wraps no faster than every few seconds) and add
//!   `scale × (count − offset_count)` to the uptime `offset` the current
//!   timehands slot recorded at `offset_count`. Adding `boottime` gives the
//!   time of day.
//! - A timer "winds up" the timehands often enough that the 32-bit count
//!   never wraps between updates: it fills the next of four slots with the
//!   uptime extended to now, then makes it current. A slot's generation is
//!   0 while it is written and changes with every write, so a reader that
//!   raced an update sees the change and retries.
//! - The kernel's own clock system calls read the slots the same way, so a
//!   value from the system call and one from libc never disagree.
//!
//! Behavior reference: sys/kern/kern_sharedpage.c, sys/kern/kern_tc.c
//! (`tc_windup`), lib/libsys/__vdso_gettimeofday.c, sys/tools/amd64_vdso.sh,
//! sys/amd64/amd64/sigtramp.S.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the timehands count is the counter's low 32 bits, and a bintime's \
              low 64 bits are its fraction, by definition; a 64-bit target"
)]

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering, fence};

use abi::vdso::{Bintime, Timehands, Timekeep};
use addr::VirtAddr;
use sched::{IrqCx, Sleepable};
use sync::{Once, SpinLock};
use vm::PAGE_SIZE;
use vm::map::Prot;
use vm::object::Object;

use crate::mm::space::{AddressSpace, SpaceError};
use crate::time::{NANOS, Timer};

/// Where the time keeping starts in the page; the signal trampoline (and
/// on amd64 its vdso, at most this long, as on FreeBSD) is at the start.
const TIMEKEEP_OFFSET: usize = 2048;
/// Timehands slots (FreeBSD's `VDSO_TH_NUM`).
const SLOTS: u32 = 4;

const _: () = assert!(
    TIMEKEEP_OFFSET + size_of::<Timekeep>() + SLOTS as usize * size_of::<Timehands>()
        <= PAGE_SIZE as usize
);

/// The page.
struct Page {
    object: Arc<Object>,
    /// The page through the DMAP.
    kva: *mut u8,
    /// Where the signal trampoline is in it.
    sigcode: usize,
}

// SAFETY: the page lives forever (the object holds it and is never freed),
// and its contents are only written by the windup, under `WRITER`.
unsafe impl Send for Page {}
// SAFETY: as above.
unsafe impl Sync for Page {}

static PAGE: Once<Page> = Once::new();

/// The writer's state: the current slot and a copy of its contents.
struct Writer {
    current: u32,
    th: Timehands,
}

static WRITER: SpinLock<Writer> = SpinLock::new(Writer {
    current: 0,
    th: Timehands {
        th_algo: 0,
        th_gen: 0,
        th_scale: 0,
        th_offset_count: 0,
        th_counter_mask: 0,
        th_offset: Bintime { sec: 0, frac: 0 },
        th_boottime: Bintime { sec: 0, frac: 0 },
        #[cfg(target_arch = "x86_64")]
        th_x86_shift: 0,
        #[cfg(target_arch = "x86_64")]
        th_x86_hpet_idx: 0,
        #[cfg(target_arch = "x86_64")]
        th_x86_pvc_last_systime: 0,
        #[cfg(target_arch = "x86_64")]
        th_x86_pvc_stable_mask: 0,
        #[cfg(target_arch = "x86_64")]
        th_res: [0; 15],
        #[cfg(target_arch = "aarch64")]
        th_physical: 0,
        #[cfg(target_arch = "aarch64")]
        th_res: [0; 7],
    },
});

/// The counter is read shifted right this far (amd64; always 0 on arm64,
/// whose libc reads the count unshifted).
static SHIFT: AtomicU32 = AtomicU32::new(0);

/// The timer that winds up the timehands, and its period.
static WINDUP: Once<(Pin<&'static Timer>, u64)> = Once::new();

fn timekeep() -> *mut Timekeep {
    let p = PAGE.get().expect("shared page");
    p.kva.wrapping_add(TIMEKEEP_OFFSET).cast()
}

fn slot(i: u32) -> *mut Timehands {
    timekeep()
        .wrapping_add(1)
        .cast::<Timehands>()
        .wrapping_add(i as usize)
}

/// The shifted, truncated count libc reads.
fn count() -> u32 {
    (crate::time::counter() >> SHIFT.load(Ordering::Relaxed)) as u32
}

/// `bt + scale × delta`, exactly (libc splits the product when it could
/// overflow 64 bits; the sum is the same).
fn advance(bt: Bintime, scale: u64, delta: u32) -> Bintime {
    let x = u128::from(scale) * u128::from(delta);
    let (frac, carry) = bt.frac.overflowing_add(x as u64);
    Bintime {
        sec: bt.sec + (x >> 64) as i64 + i64::from(carry),
        frac,
    }
}

fn add(a: Bintime, b: Bintime) -> Bintime {
    let (frac, carry) = a.frac.overflowing_add(b.frac);
    Bintime {
        sec: a.sec + b.sec + i64::from(carry),
        frac,
    }
}

/// Nanoseconds as a bintime (FreeBSD's `timespec2bintime`).
fn from_ns(ns: u64) -> Bintime {
    Bintime {
        sec: (ns / NANOS) as i64,
        frac: (ns % NANOS) * 18_446_744_073,
    }
}

/// A bintime in nanoseconds, rounding the fraction as FreeBSD's
/// `bintime2timespec` does.
pub(crate) fn to_ns(bt: Bintime) -> u64 {
    let nsec = (NANOS * (bt.frac >> 32)) >> 32;
    u64::try_from(bt.sec).unwrap_or(0) * NANOS + nsec
}

/// A bintime in microseconds (`bintime2timeval`).
pub(crate) fn to_us(bt: Bintime) -> u64 {
    let usec = (1_000_000 * (bt.frac >> 32)) >> 32;
    u64::try_from(bt.sec).unwrap_or(0) * 1_000_000 + usec
}

/// The uptime and the boot time, read as libc reads them.
fn read() -> (Bintime, Bintime) {
    let tk = timekeep();
    // SAFETY: the time keeping is in the shared page, which lives forever;
    // its words are read atomically and the slots volatilely, and a slot
    // read that raced its writer is detected by the generation and
    // retried (a seqlock).
    unsafe {
        let current = AtomicU32::from_ptr(&raw mut (*tk).tk_current);
        loop {
            let curr = current.load(Ordering::Acquire);
            let th = slot(curr);
            let gen_ = AtomicU32::from_ptr(&raw mut (*th).th_gen);
            let g = gen_.load(Ordering::Acquire);
            let t = ptr::read_volatile(th);
            let delta = count().wrapping_sub(t.th_offset_count) & t.th_counter_mask;
            fence(Ordering::Acquire);
            if g != 0
                && curr == current.load(Ordering::Relaxed)
                && g == gen_.load(Ordering::Relaxed)
            {
                return (advance(t.th_offset, t.th_scale, delta), t.th_boottime);
            }
        }
    }
}

/// The time since boot (`CLOCK_UPTIME`, `CLOCK_MONOTONIC`).
pub(crate) fn uptime() -> Bintime {
    if PAGE.get().is_none() {
        return from_ns(crate::time::now_ns());
    }
    read().0
}

/// The time of day (`CLOCK_REALTIME`).
pub(crate) fn realtime() -> Bintime {
    if PAGE.get().is_none() {
        return from_ns(crate::time::now_ns());
    }
    let (up, boot) = read();
    add(up, boot)
}

/// Extends the current slot to now into the next one and makes that
/// current; with `boottime`, also sets the time of day.
fn windup(boottime: Option<Bintime>) {
    let mut w = WRITER.lock();
    let tc = count();
    let mut th = w.th;
    let delta = tc.wrapping_sub(th.th_offset_count) & th.th_counter_mask;
    th.th_offset = advance(th.th_offset, th.th_scale, delta);
    th.th_offset_count = tc;
    if let Some(b) = boottime {
        th.th_boottime = b;
    }
    th.th_gen = th.th_gen.wrapping_add(1).max(1);
    let next = (w.current + 1) % SLOTS;
    publish(next, &th);
    w.current = next;
    w.th = th;
}

/// Writes slot `i` and makes it current. Called with `WRITER` held.
fn publish(i: u32, th: &Timehands) {
    let s = slot(i);
    let tk = timekeep();
    // SAFETY: the slot is in the shared page; only the holder of `WRITER`
    // writes it. Generation 0 marks it in progress: readers retry until the
    // final generation is stored after the contents (release ordering).
    unsafe {
        let gen_ = AtomicU32::from_ptr(&raw mut (*s).th_gen);
        gen_.store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        ptr::write_volatile(s, Timehands { th_gen: 0, ..*th });
        gen_.store(th.th_gen, Ordering::Release);
        AtomicU32::from_ptr(&raw mut (*tk).tk_current).store(i, Ordering::Release);
    }
}

fn on_windup(_: usize, _: &IrqCx) {
    windup(None);
    if let Some(&(t, period)) = WINDUP.get() {
        t.reset(period, on_windup, 0);
    }
}

/// Winds up now (tests).
#[cfg(feature = "ktest")]
pub(crate) fn wind() {
    windup(None);
}

/// Sets the time of day: the Unix time is `unix_ns` now.
pub(crate) fn set_realtime(unix_ns: u64) {
    if PAGE.get().is_none() {
        return;
    }
    let now = to_ns(uptime());
    windup(Some(from_ns(unix_ns.saturating_sub(now))));
}

/// Allocates the page, publishes the time keeping, and starts the windup
/// timer (on this CPU). Once, on the boot CPU, after the clock is set up.
pub(crate) fn init() {
    let object = Object::anonymous(crate::mm::space::page_ops());
    // Shared from the start: unmapping it from a process never frees it.
    object.set_shared();
    let pfn = object.get_or_insert(0, None).expect("shared page");
    let kva = bootproto::layout::dmap(pfn.addr())
        .expect("RAM outside the DMAP")
        .get() as *mut u8;
    // SAFETY: the page is fresh, ours, and a page long; nothing else
    // refers to it yet.
    let head = unsafe { core::slice::from_raw_parts_mut(kva, TIMEKEEP_OFFSET) };
    let sigcode = write_sigcode(head);
    if PAGE
        .set(Page {
            object,
            kva,
            sigcode,
        })
        .is_err()
    {
        return;
    }

    let freq = crate::time::frequency().max(1);
    // Keep the shifted rate under 2^31 Hz (FreeBSD's TSC timecounter does
    // the same), so the 32-bit count lasts at least two seconds. libc on
    // arm64 cannot shift, and the generic timer runs at 1 GHz at most.
    #[cfg(target_arch = "x86_64")]
    let shift = (64 - freq.leading_zeros()).saturating_sub(31);
    #[cfg(target_arch = "aarch64")]
    let shift = 0;
    SHIFT.store(shift, Ordering::Relaxed);
    let tc_freq = (freq >> shift).max(1);
    // Wind up four times per wrap of the count, and at least once a second.
    let wrap_ns =
        u64::try_from((1u128 << 32) * u128::from(NANOS) / u128::from(tc_freq)).unwrap_or(u64::MAX);
    let period = (wrap_ns / 4).min(NANOS);

    let base = crate::time::realtime_base_ns();
    {
        let mut w = WRITER.lock();
        let th = &mut w.th;
        // Seconds per shifted tick, from the unshifted rate (exact).
        th.th_scale = u64::try_from((1u128 << (64 + shift)) / u128::from(freq)).unwrap_or(u64::MAX);
        th.th_counter_mask = u32::MAX;
        let (c, up) = crate::time::counter_and_uptime();
        th.th_offset_count = (c >> shift) as u32;
        th.th_offset = from_ns(up);
        th.th_boottime = from_ns(base);
        th.th_gen = 1;
        #[cfg(target_arch = "x86_64")]
        {
            th.th_algo = abi::vdso::TH_ALGO_X86_TSC;
            th.th_x86_shift = shift;
        }
        #[cfg(target_arch = "aarch64")]
        {
            th.th_algo = abi::vdso::TH_ALGO_ARM_GENTIM;
        }
        let th = *th;
        publish(0, &th);
        let tk = timekeep();
        // SAFETY: the shared page, which no process maps yet.
        unsafe {
            (*tk).tk_ver = abi::vdso::TK_VER_CURR;
            (*tk).tk_enabled = 1;
        }
    }

    let t: Pin<&'static Timer> = Pin::static_ref(Box::leak(Box::new(Timer::new())));
    let _ = WINDUP.set((t, period));
    t.reset(period, on_windup, 0);
    kprintln!(
        "time: timehands count at {} MHz (shift {shift}), wound up every {} ms",
        tc_freq / 1_000_000,
        period / 1_000_000
    );
}

/// Maps the shared page into `space` at `at`.
pub(crate) fn map(s: &Sleepable, space: &AddressSpace, at: u64) -> Result<(), SpaceError> {
    let Some(p) = PAGE.get() else {
        return Ok(());
    };
    space.map_private(
        s,
        VirtAddr::new(at)..VirtAddr::new(at + PAGE_SIZE),
        Arc::clone(&p.object),
        0,
        Prot::READ.union(Prot::EXEC),
        Prot::READ.union(Prot::EXEC),
    )
}

/// Writes the signal trampoline to the start of the page (`head`), and
/// returns its offset there. On amd64 it is the code of a vdso.
#[cfg(target_arch = "x86_64")]
fn write_sigcode(head: &mut [u8]) -> usize {
    use abi::ucontext::{McontextX86, Ucontext};
    use core::mem::offset_of;
    use elf::vdso::{Cfa, Vdso};

    use crate::arch::user::{SIGF_UC, sigcode_pushed};

    // DWARF register numbers (the amd64 psABI's figure 3.36).
    const RSP: u8 = 7;
    const RIP: u8 = 16;
    let mc = SIGF_UC as usize + offset_of!(Ucontext<McontextX86>, uc_mcontext);
    // An ordinary function's rules: the return address just below the
    // CFA, which is `rsp` before the call.
    let mut cie = Cfa::new(-8);
    cie.def_cfa(RSP, 8).offset(RIP, -8);
    // The trampoline starts with `rsp` at the signal frame, which is the
    // CFA, and the interrupted registers are in the frame's context. The
    // CFA stays there once the fake return address is pushed. FreeBSD
    // 15.1's order and set of registers.
    let mut fde = Cfa::new(-8);
    fde.def_cfa(RSP, 0);
    let saved = [
        (5, offset_of!(McontextX86, mc_rdi)),
        (4, offset_of!(McontextX86, mc_rsi)),
        (1, offset_of!(McontextX86, mc_rdx)),
        (2, offset_of!(McontextX86, mc_rcx)),
        (8, offset_of!(McontextX86, mc_r8)),
        (9, offset_of!(McontextX86, mc_r9)),
        (0, offset_of!(McontextX86, mc_rax)),
        (3, offset_of!(McontextX86, mc_rbx)),
        (6, offset_of!(McontextX86, mc_rbp)),
        (10, offset_of!(McontextX86, mc_r10)),
        (11, offset_of!(McontextX86, mc_r11)),
        (12, offset_of!(McontextX86, mc_r12)),
        (13, offset_of!(McontextX86, mc_r13)),
        (14, offset_of!(McontextX86, mc_r14)),
        (15, offset_of!(McontextX86, mc_r15)),
        (RIP, offset_of!(McontextX86, mc_rip)),
        (RSP, offset_of!(McontextX86, mc_rsp)),
    ];
    for (reg, off) in saved {
        fde.offset(reg, i64::try_from(mc + off).expect("a small offset"));
    }
    let pushed = u8::try_from(sigcode_pushed()).expect("a short trampoline");
    fde.advance(pushed).def_cfa(RSP, 8);
    let vdso = Vdso {
        machine: elf::Machine::X86_64,
        soname: b"elf-vdso.so.1",
        version: b"FBSD_1.7",
        symbol: b"__vdso_sigcode",
        code: crate::arch::user::sigcode(),
        return_column: RIP,
        cie: &cie,
        fde: &fde,
    };
    vdso.write(head)
        .expect("the vdso fits its part of the page")
        .code
}

/// Writes the signal trampoline to the start of the page (`head`), and
/// returns its offset there.
#[cfg(target_arch = "aarch64")]
fn write_sigcode(head: &mut [u8]) -> usize {
    let code = crate::arch::user::sigcode();
    head.get_mut(..code.len())
        .expect("signal trampoline too long")
        .copy_from_slice(code);
    0
}

/// The user address of the signal trampoline in a shared page mapped at
/// `page`.
pub(crate) fn sigcode_address(page: u64) -> u64 {
    page + PAGE.get().map_or(0, |p| p.sigcode as u64)
}

/// The user address of the vdso (`AT_KPRELOAD`) in a shared page mapped
/// at `page`, if there is one: on amd64 while `kern.elf64.vdso` is on.
pub(crate) fn vdso_address(page: u64) -> Option<u64> {
    let on = cfg!(target_arch = "x86_64") && crate::exec::VDSO.load(Ordering::Relaxed);
    (on && PAGE.get().is_some()).then_some(page)
}

/// The user address of the time keeping (`AT_TIMEKEEP`) in a shared page
/// mapped at `page`, or 0 without one.
pub(crate) fn timekeep_address(page: u64) -> u64 {
    if PAGE.get().is_none() {
        0
    } else {
        page + TIMEKEEP_OFFSET as u64
    }
}
