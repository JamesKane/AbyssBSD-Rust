// SPDX-License-Identifier: BSD-2-Clause
//! In-kernel tests, run under QEMU by `cargo xtask test --tier boot`.
//!
//! After the registered tests pass, [`run`] takes a deliberate fatal page
//! fault on the boot stack's guard page; the trap report (registers,
//! symbolized backtrace) and the pass verdict come from the fatal-trap
//! handler. xtask checks the report's shape in the serial log.

use core::sync::atomic::Ordering;

use addr::VirtAddr;
use bootproto::MemoryKind;
use ktest::ktest;

use crate::arch;

/// A [`sched::Sleepable`] for test code, which runs only in threads (the
/// boot thread or ones it spawns), never in interrupt handlers.
fn cx() -> sched::Sleepable {
    // SAFETY: see above; tests take no spin locks around their sleeps.
    unsafe { sched::Sleepable::assume() }
}

/// Runs every test, then the deliberate fault. Does not return.
///
/// With `abyss.test.warm_reset`, the first boot instead writes a marker to
/// the console (and so the log ring) and warm-resets; the loader saves that
/// log to the ESP on the second boot, which then runs the tests. xtask
/// checks the saved file.
pub(crate) fn run(_s: &sched::Sleepable) -> ! {
    if crate::boot::flag("abyss.test.warm_reset") {
        let log = crate::boot::info().log.get().expect("log ring");
        if log.recovered == 0 {
            kprintln!("logring: first boot marker, boot {}", log.boots);
            crate::firmware::reset(efi::ResetKind::Warm);
        }
        kprintln!(
            "logring: loader recovered {} bytes from the previous boot",
            log.recovered
        );
    }
    let clock = || crate::time::now_ns() / 1_000_000;
    let _ = ktest::run_tests_timed(&mut crate::console::Writer, Some(&clock));
    crate::panic::EXPECT_FATAL_TRAP.store(true, Ordering::Relaxed);
    kprintln!("ktest: taking a deliberate fatal page fault on the stack guard page");
    deliberate_fault();
}

#[inline(never)]
fn deliberate_fault() -> ! {
    let guard = crate::mm::layout().guard as *const u64;
    // SAFETY: deliberately unsound: the guard page is unmapped, so this read
    // traps and the fatal-trap handler ends the run.
    let v = unsafe { guard.read_volatile() };
    panic!("guard page read returned {v:#x}");
}

ktest! {
    fn memory_map_is_sane() {
        let map = crate::boot::memory_map();
        assert!(!map.is_empty());
        assert!(map.windows(2).all(|w| w[0].range().end <= w[1].start), "sorted, disjoint");
        assert!(map.iter().any(|r| r.kind == MemoryKind::Free));
        let info = crate::boot::info();
        let k = map
            .iter()
            .find(|r| r.kind == MemoryKind::Kernel)
            .expect("kernel region");
        assert!(k.range().contains(info.kernel.phys));
    }
}

ktest! {
    fn kaslr_slide_is_aligned() {
        let info = crate::boot::info();
        assert_eq!(info.kernel.slide % bootproto::layout::KASLR_ALIGN, 0);
        assert_eq!(
            info.kernel.virt.get(),
            bootproto::layout::KERNEL_LINK_BASE + info.kernel.slide
        );
        // Run-time addresses agree with the slide.
        assert_eq!(crate::mm::layout().start, info.kernel.virt.get());
    }
}

ktest! {
    fn recoverable_page_fault_is_reported() {
        static VALUE: u64 = 0x5a5a_1234_abcd_0001;
        assert_eq!(arch::fixup::probe_read((&raw const VALUE) as u64), Ok(VALUE));
        // Unmapped kernel address: the fault is taken and recovered.
        let hole = bootproto::layout::KVA_BASE + bootproto::layout::KVA_SIZE - 4096;
        assert_eq!(arch::fixup::probe_read(hole), Err(arch::fixup::Fault));
    }
}

ktest! {
    fn kernel_is_mapped_w_xor_x() {
        let l = crate::mm::layout();
        let t = |va: u64| crate::mm::translate(VirtAddr::new(va)).expect("mapped");
        let text = t(l.start);
        assert!(text.attrs.exec && !text.attrs.write);
        let ro = t(l.text_end);
        assert!(!ro.attrs.exec && !ro.attrs.write);
        let data = t(l.rodata_end);
        assert!(!data.attrs.exec && data.attrs.write);
        assert!(crate::mm::translate(VirtAddr::new(l.guard)).is_none(), "guard page");
        assert!(t(l.guard + 4096).attrs.write);
    }
}

ktest! {
    fn identity_map_is_gone() {
        // The kernel's own physical address was identity mapped by the
        // loader; it must not be any more.
        let phys = crate::boot::info().kernel.phys.get();
        assert_eq!(arch::fixup::probe_read(phys), Err(arch::fixup::Fault));
        assert!(crate::mm::translate(VirtAddr::new(phys)).is_none());
    }
}

ktest! {
    fn dmap_maps_ram() {
        let info = crate::boot::info();
        let va = bootproto::layout::dmap(info.kernel.phys).unwrap();
        let t = crate::mm::translate(va).expect("DMAP");
        assert_eq!(t.pa, info.kernel.phys);
        assert!(!t.attrs.exec);
        // The DMAP alias of the image holds the same bytes.
        let first = arch::fixup::probe_read(va.get()).unwrap();
        assert_eq!(first, arch::fixup::probe_read(crate::mm::layout().start).unwrap());
    }
}

ktest! {
    fn symbols_resolve() {
        let f = symbol_probe as fn() -> u64;
        let addr = f as *const () as u64;
        let (name, off) = crate::symbols::lookup(addr).expect("symbol for a kernel fn");
        assert_eq!(off, 0);
        let mut s = Buf::default();
        core::fmt::write(&mut s, format_args!("{}", demangle::Demangled(name))).unwrap();
        assert_eq!(s.as_str(), "kernel::tests::symbol_probe");
    }
}

#[inline(never)]
fn symbol_probe() -> u64 {
    core::hint::black_box(42)
}

ktest! {
    fn log_ring_is_live() {
        let log = crate::boot::info().log.get().expect("log ring");
        assert!(log.boots >= 1);
        let va = bootproto::layout::dmap(log.region.start).unwrap();
        let t = crate::mm::translate(va).expect("ring in the DMAP");
        assert!(t.attrs.write && !t.attrs.exec);
    }
}

ktest! {
    fn consoles_present() {
        let info = crate::boot::info();
        assert!(info.serial.get().is_some(), "serial console");
        assert!(info.framebuffer.get().is_some(), "GOP framebuffer");
    }
}

ktest! {
    fn buddy_allocator_round_trips() {
        let before = crate::mm::phys_stats();
        assert!(before.free > 0 && before.free <= before.managed);
        let order = vm::Order::new(3).unwrap();
        let pa = crate::mm::alloc_pages(order).expect("eight pages");
        assert!(pa.is_aligned(order.bytes()));
        assert!(pa.get() >= crate::mm::LOW_RESERVE);
        let va = bootproto::layout::dmap(pa).unwrap();
        // SAFETY: the block is ours and mapped in the DMAP.
        let block = unsafe {
            core::slice::from_raw_parts_mut(va.get() as *mut u64, 8 * 512)
        };
        block.fill(0x0bad_cafe);
        assert_eq!(crate::mm::phys_stats().free, before.free - 8);
        crate::mm::free_pages(pa, order);
        assert_eq!(crate::mm::phys_stats(), before);
    }
}

ktest! {
    fn heap_serves_collections() {
        use alloc::boxed::Box;
        use alloc::vec::Vec;
        let before = crate::mm::heap_stats();
        {
            let mut v: Vec<u64> = Vec::new();
            for i in 0..50_000 {
                v.push(i);
            }
            assert_eq!(v.iter().sum::<u64>(), 50_000 * 49_999 / 2);
            let boxes: Vec<Box<[u8; 100]>> = (0..1000u32).map(|i| Box::new([i.to_le_bytes()[0]; 100])).collect();
            assert!(boxes.iter().enumerate().all(|(i, b)| b[99] == i.to_le_bytes()[0]));
            let big = alloc::vec![7u8; 3 << 20];
            assert_eq!(big[(3 << 20) - 1], 7);
            assert!(crate::mm::heap_stats().large_pages >= 768);
        }
        let after = crate::mm::heap_stats();
        assert_eq!(after.live_objects, before.live_objects);
        assert_eq!(after.large_pages, before.large_pages);
    }
}

ktest! {
    fn kernel_stacks_have_guard_pages() {
        use crate::mm::kstack::KernelStack;
        // The first stack in a region also creates intermediate page
        // tables, which stay; measure after one has come and gone.
        drop(KernelStack::new(4).expect("stack"));
        let kva = crate::mm::kstack::kva_allocated();
        let free = crate::mm::phys_stats().free;
        let stack = KernelStack::new(4).expect("stack");
        assert_eq!(stack.top() - stack.bottom().get(), VirtAddr::new(4 * 4096));
        let t = crate::mm::translate(stack.top() - 8).expect("stack mapped");
        assert!(t.attrs.write && !t.attrs.exec);
        assert!(crate::mm::translate(stack.guard()).is_none());
        assert_eq!(arch::fixup::probe_read(stack.guard().get()), Err(arch::fixup::Fault));
        // Fresh stacks are zeroed, and usable.
        assert_eq!(arch::fixup::probe_read(stack.bottom().get()), Ok(0));
        // SAFETY: the stack's pages are mapped and ours.
        unsafe { (stack.bottom().get() as *mut u64).write_volatile(5) };
        assert_eq!(arch::fixup::probe_read(stack.bottom().get()), Ok(5));
        let bottom = stack.bottom();
        drop(stack);
        assert!(crate::mm::translate(bottom).is_none());
        assert_eq!(arch::fixup::probe_read(bottom.get()), Err(arch::fixup::Fault));
        assert_eq!(crate::mm::kstack::kva_allocated(), kva);
        assert_eq!(crate::mm::phys_stats().free, free);
    }
}

ktest! {
    fn threads_run_and_join() {
        use alloc::sync::Arc;
        use alloc::vec::Vec;
        use core::sync::atomic::AtomicU64;
        let count = Arc::new(AtomicU64::new(0));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let count = Arc::clone(&count);
                sched::spawn("worker", sched::Priority::KERNEL, move |_| {
                    for _ in 0..100 {
                        count.fetch_add(i, Ordering::Relaxed);
                        sched::yield_now();
                    }
                })
                .expect("spawn")
            })
            .collect();
        for t in &threads {
            t.join(&cx());
            assert!(t.has_exited());
        }
        assert_eq!(count.load(Ordering::Relaxed), 100 * (0..8).sum::<u64>());
    }
}

ktest! {
    fn wait_queues_ping_pong() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicU64;
        struct Court {
            turn: AtomicU64,
            q: sched::WaitQueue,
        }
        let court = Arc::new(Court { turn: AtomicU64::new(0), q: sched::WaitQueue::new() });
        let player = |me: u64| {
            let court = Arc::clone(&court);
            move |_: &sched::Sleepable| {
                for round in 0..500u64 {
                    let want = round * 2 + me;
                    court.q.wait_until(&cx(), || court.turn.load(Ordering::Acquire) == want);
                    court.turn.store(want + 1, Ordering::Release);
                    court.q.wake_all();
                }
            }
        };
        let a = sched::spawn("ping", sched::Priority::KERNEL, player(0)).unwrap();
        let b = sched::spawn("pong", sched::Priority::KERNEL, player(1)).unwrap();
        a.join(&cx());
        b.join(&cx());
        assert_eq!(court.turn.load(Ordering::Relaxed), 1000);
    }
}

ktest! {
    fn exited_threads_are_reaped() {
        let kva = crate::mm::kstack::kva_allocated();
        let t = sched::spawn("short", sched::Priority::KERNEL, |_| {}).unwrap();
        t.join(&cx());
        drop(t);
        // The reaper frees the stack once it runs (and any left over from
        // earlier tests, so the total may drop below the baseline).
        for _ in 0..2000 {
            if crate::mm::kstack::kva_allocated() <= kva {
                return;
            }
            crate::time::sleep_ns(&cx(), 1_000_000);
        }
        panic!("stack not reaped: {:#x} bytes of KVA out, {kva:#x} before", crate::mm::kstack::kva_allocated());
    }
}

ktest! {
    fn clock_advances_and_sleeps_last() {
        let t0 = crate::time::now_ns();
        crate::time::sleep_ns(&cx(), 20_000_000);
        let slept = crate::time::now_ns() - t0;
        assert!(slept >= 20_000_000, "slept only {slept} ns");
        assert!(slept < 2_000_000_000, "slept {slept} ns");
        assert!(crate::time::ticks(hal::CpuId::BOOT) > 0, "timer interrupts arrive");
    }
}

ktest! {
    fn timers_fire_in_order_and_stop() {
        use core::pin::pin;
        use core::sync::atomic::AtomicU64;
        use crate::time::Timer;
        struct Log {
            digits: AtomicU64,
            q: sched::WaitQueue,
        }
        static LOG: Log = Log {
            digits: AtomicU64::new(0),
            q: sched::WaitQueue::new(),
        };
        fn push(digit: usize, _: &sched::IrqCx) {
            let v = LOG.digits.load(Ordering::Relaxed);
            LOG.digits.store(v * 10 + digit as u64, Ordering::Release);
            LOG.q.wake_all();
        }
        let timers = [
            pin!(Timer::new()),
            pin!(Timer::new()),
            pin!(Timer::new()),
            pin!(Timer::new()),
        ];
        timers[0].as_ref().reset(30_000_000, push, 3);
        timers[1].as_ref().reset(10_000_000, push, 1);
        timers[2].as_ref().reset(20_000_000, push, 9);
        timers[3].as_ref().reset(20_000_000, push, 2);
        assert!(timers[2].stop());
        assert!(!timers[2].stop(), "already stopped");
        // Re-arming replaces the pending deadline.
        timers[0].as_ref().reset(40_000_000, push, 4);
        timers[0].as_ref().reset(30_000_000, push, 3);
        LOG.q.wait_until(&cx(), || LOG.digits.load(Ordering::Acquire) >= 100);
        assert_eq!(LOG.digits.load(Ordering::Relaxed), 123);
        assert!(!timers[0].stop(), "fired");
    }
}

ktest! {
    fn timed_waits_time_out() {
        use alloc::sync::Arc;
        let q = Arc::new(sched::WaitQueue::new());
        let t0 = crate::time::now_ns();
        assert!(!crate::time::wait_timeout(&cx(), &q, || false, 15_000_000));
        assert!(crate::time::now_ns() - t0 >= 15_000_000);
        assert!(crate::time::wait_timeout(&cx(), &q, || true, 15_000_000));
    }
}

ktest! {
    fn busy_threads_are_preempted() {
        use alloc::sync::Arc;
        use alloc::vec::Vec;
        use core::sync::atomic::{AtomicBool, AtomicU64};
        // Two threads spin without yielding; both must make progress, which
        // only time-slice preemption allows on one CPU.
        let stop = Arc::new(AtomicBool::new(false));
        let counts: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let threads: Vec<_> = counts
            .iter()
            .map(|c| {
                let (c, stop) = (Arc::clone(c), Arc::clone(&stop));
                sched::spawn_on("spinner", sched::Priority::KERNEL, Some(hal::CpuId::BOOT), move |_| {
                    while !stop.load(Ordering::Relaxed) {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                })
                .unwrap()
            })
            .collect();
        crate::time::sleep_ns(&cx(), 100_000_000);
        stop.store(true, Ordering::Relaxed);
        for t in &threads {
            t.join(&cx());
        }
        for (i, c) in counts.iter().enumerate() {
            assert!(
                c.load(Ordering::Relaxed) > 0,
                "spinner {i} never ran; threads {:?}; me on {}; ticks {:?}",
                threads
                    .iter()
                    .map(|t| (t.last_cpu().0, t.switches(), t.state()))
                    .collect::<Vec<_>>(),
                hal::cpu_id(),
                (0..4).map(|c| crate::time::ticks(hal::CpuId(c))).collect::<Vec<_>>()
            );
        }
        assert!(threads.iter().all(|t| t.switches() >= 2), "no preemption");
    }
}

ktest! {
    fn mutexes_serialize_threads() {
        use alloc::sync::Arc;
        use alloc::vec::Vec;
        use sched::lock::{LockClass, Mutex};
        static CLASS: LockClass = LockClass::new("test counter", 10);
        let m = Arc::new(Mutex::new(&CLASS, 0u64));
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let m = Arc::clone(&m);
                sched::spawn("locker", sched::Priority::KERNEL, move |_| {
                    for i in 0..2000u64 {
                        let mut g = m.lock(&cx());
                        let v = *g;
                        if i % 64 == 0 {
                            sched::yield_now();
                        }
                        *g = v + 1;
                    }
                })
                .unwrap()
            })
            .collect();
        for t in &threads {
            t.join(&cx());
        }
        assert_eq!(*m.lock(&cx()), 12_000);
        assert!(!m.is_locked());
    }
}

ktest! {
    fn mutex_owner_inherits_priority() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;
        use sched::lock::{LockClass, Mutex};
        use sched::Priority;
        static CLASS: LockClass = LockClass::new("test inherit", 11);
        let m = Arc::new(Mutex::new(&CLASS, ()));
        let held = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let low = {
            let (m, held, release) = (Arc::clone(&m), Arc::clone(&held), Arc::clone(&release));
            // A fixed (kernel-range) priority; time-sharing ones float.
            sched::spawn("low", Priority::new(110), move |_| {
                let _g = m.lock(&cx());
                held.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    crate::time::sleep_ns(&cx(), 1_000_000);
                }
            })
            .unwrap()
        };
        while !held.load(Ordering::Acquire) {
            crate::time::sleep_ns(&cx(), 1_000_000);
        }
        let high = {
            let m = Arc::clone(&m);
            sched::spawn("high", Priority::new(40), move |_| {
                drop(m.lock(&cx()));
            })
            .unwrap()
        };
        // Once `high` blocks, `low` runs at `high`'s priority.
        let mut lent = false;
        for _ in 0..1000 {
            if low.priority() == Priority::new(40) {
                lent = true;
                break;
            }
            crate::time::sleep_ns(&cx(), 1_000_000);
        }
        release.store(true, Ordering::Release);
        high.join(&cx());
        low.join(&cx());
        assert!(lent, "priority was not lent (low at {:?})", low.priority());
        assert_eq!(low.priority(), Priority::new(110), "lent priority returned");
    }
}

ktest! {
    fn rw_locks_share_and_exclude() {
        use alloc::sync::Arc;
        use alloc::vec::Vec;
        use core::sync::atomic::AtomicU64;
        use sched::lock::{LockClass, SxLock};
        static CLASS: LockClass = LockClass::new("test sx", 12);
        let l = Arc::new(SxLock::new(&CLASS, (0u64, 0u64)));
        let readers_in = Arc::new(AtomicU64::new(0));
        let threads: Vec<_> = (0..6)
            .map(|i| {
                let (l, readers_in) = (Arc::clone(&l), Arc::clone(&readers_in));
                sched::spawn("rw", sched::Priority::KERNEL, move |_| {
                    for _ in 0..500 {
                        if i % 3 == 0 {
                            let mut g = l.write(&cx());
                            assert_eq!(readers_in.load(Ordering::Relaxed), 0);
                            g.0 += 1;
                            g.1 += 1;
                        } else {
                            let g = l.read(&cx());
                            readers_in.fetch_add(1, Ordering::Relaxed);
                            assert_eq!(g.0, g.1, "torn write seen");
                            readers_in.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                })
                .unwrap()
            })
            .collect();
        for t in &threads {
            t.join(&cx());
        }
        assert_eq!(l.read(&cx()).0, 1000);
    }
}

ktest! {
    fn epochs_wait_for_readers() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;
        use sched::lock::Epoch;
        static EPOCH: Epoch = Epoch::new();
        let inside = Arc::new(AtomicBool::new(false));
        let left = Arc::new(AtomicBool::new(false));
        let reader = {
            let (inside, left) = (Arc::clone(&inside), Arc::clone(&left));
            sched::spawn("reader", sched::Priority::KERNEL, move |_| {
                let g = EPOCH.enter();
                inside.store(true, Ordering::Release);
                // A section must not sleep; stay in it by spinning.
                crate::time::spin_ns(50_000_000);
                left.store(true, Ordering::Release);
                drop(g);
            })
            .unwrap()
        };
        while !inside.load(Ordering::Acquire) {
            crate::time::sleep_ns(&cx(), 1_000_000);
        }
        EPOCH.synchronize(|| crate::time::sleep_ns(&cx(), 1_000_000));
        assert!(left.load(Ordering::Acquire), "synchronize returned while a reader was inside");
        reader.join(&cx());
        // With no readers it returns at once.
        EPOCH.synchronize(|| panic!("no reader to wait for"));
    }
}

ktest! {
    fn witness_catches_order_reversal() {
        use sched::lock::{witness, LockClass, Mutex};
        static OUTER: LockClass = LockClass::new("test outer", 20);
        static INNER: LockClass = LockClass::new("test inner", 21);
        let a = Mutex::new(&OUTER, ());
        let b = Mutex::new(&INNER, ());
        if !witness::ENABLED {
            return;
        }
        let before = witness::violations();
        {
            let _a = a.lock(&cx());
            let _b = b.lock(&cx());
        }
        assert_eq!(witness::violations(), before, "declared order");
        {
            let _b = b.lock(&cx());
            let _a = a.lock(&cx());
        }
        assert_eq!(witness::violations(), before + 1, "reversal reported");
    }
}

extern "C" fn yield_between() {
    sched::yield_now();
}

ktest! {
    fn fpu_state_survives_switches() {
        use alloc::vec::Vec;
        // Four FP threads share one CPU, so every yield switches between
        // them; each checks its registers still hold its own pattern.
        let threads: Vec<_> = (0..4u8)
            .map(|id| {
                sched::Builder::new("fpu")
                    .bind(hal::CpuId::BOOT)
                    .fpu()
                    .spawn(move |_| fpu_rounds(id))
                    .expect("spawn")
            })
            .collect();
        for t in &threads {
            t.join(&cx());
        }
    }
}

fn fpu_rounds(id: u8) {
    for round in 0..50u8 {
        let mut pattern = [0u8; 512];
        for (i, b) in pattern.iter_mut().enumerate() {
            *b = id.wrapping_mul(97) ^ round.wrapping_mul(13) ^ i.to_le_bytes()[0];
        }
        let mut out = [0u8; 512];
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: this thread has FP state; buffers are 512 bytes.
            unsafe { arch::fpu::load_call_store(&pattern, &mut out, yield_between) };
            let n = arch::fpu::compared_bytes();
            for r in 0..16 {
                let s = r * 32;
                assert_eq!(
                    &out[s..s + n],
                    &pattern[s..s + n],
                    "fpu {id}: register {r} changed"
                );
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            let vl = arch::fpu::vector_length();
            let sve_pattern: alloc::vec::Vec<u8> = (0..32 * vl)
                .map(|i: usize| id.wrapping_mul(31) ^ round ^ i.to_le_bytes()[0].rotate_left(3))
                .collect();
            let mut sve_out = alloc::vec![0u8; 32 * vl];
            // SAFETY: this thread has FP state; buffers are sized as
            // documented.
            unsafe {
                arch::fpu::load_call_store(
                    &pattern,
                    &mut out,
                    &sve_pattern,
                    &mut sve_out,
                    yield_between,
                );
            }
            assert_eq!(out, pattern, "fpu {id}: V registers changed");
            assert_eq!(sve_out, sve_pattern, "fpu {id}: Z registers changed");
        }
    }
}

/// A small fixed buffer for formatting in tests.
struct Buf {
    bytes: [u8; 128],
    len: usize,
}

impl Default for Buf {
    fn default() -> Self {
        Self {
            bytes: [0; 128],
            len: 0,
        }
    }
}

impl Buf {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap()
    }
}

impl core::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        self.bytes
            .get_mut(self.len..end)
            .ok_or(core::fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

ktest! {
    fn sysctl_boot_nodes() {
        let s = cx();
        let (oid, value, next) = crate::sysctl::read(&s, |t| {
            let oid = t.name2oid("kern.osreldate").expect("kern.osreldate");
            let mut v = alloc::vec::Vec::new();
            t.read(&oid, &mut v).expect("read");
            (oid, v, t.next(&[1, 1], true))
        });
        assert_eq!(oid, [1, 24]);
        assert_eq!(value, 1_501_000i32.to_ne_bytes());
        assert_eq!(next, Some(alloc::vec![1, 2]), "kern.osrelease follows kern.ostype");
        let ncpu = crate::sysctl::read(&s, |t| {
            let mut v = alloc::vec::Vec::new();
            t.read(&[6, 3], &mut v).expect("hw.ncpu");
            i32::from_ne_bytes(v.try_into().expect("an int"))
        });
        assert_eq!(usize::try_from(ncpu).unwrap(), sched::online_cpus());
        // Dynamic nodes come and go.
        let id = crate::sysctl::with(&s, |t| {
            let d = t.add_node(t.root(), sysctl::Number::Auto, "ktest", "").unwrap();
            assert!(t.name2oid("ktest").unwrap()[0] >= abi::sysctl::CTL_AUTO_START);
            d
        });
        crate::sysctl::with(&s, |t| assert!(t.remove(id)));
        assert!(crate::sysctl::read(&s, |t| t.name2oid("ktest")).is_none());
    }
}

ktest! {
    fn device_interrupt_through_irq_domain() {
        use core::sync::atomic::{AtomicU64, AtomicUsize};
        static HITS: AtomicU64 = AtomicU64::new(0);
        static BASE: AtomicUsize = AtomicUsize::new(0);
        let s = cx();
        HITS.store(0, Ordering::Relaxed);

        // The console UART raises a transmit interrupt on request; the
        // filter acknowledges it and turns it off again.
        #[cfg(target_arch = "x86_64")]
        let (domain, hw, trigger, polarity, filter, start, stop) = {
            fn filter(arg: usize, _: &sched::IrqCx) -> ::irq::Filter {
                // SAFETY: COM1's IIR and IER; the console only polls LSR
                // and writes THR, which this does not disturb.
                unsafe {
                    if arch::cpu::inb(0x3fa) & 1 != 0 {
                        return ::irq::Filter::Stray;
                    }
                    arch::cpu::outb(0x3f9, 0);
                }
                // `arg` 1: leave the counting to the interrupt thread.
                if arg == 1 {
                    return ::irq::Filter::Schedule;
                }
                HITS.fetch_add(1, Ordering::Relaxed);
                ::irq::Filter::Handled
            }
            fn start() {
                // SAFETY: COM1's MCR (OUT2 gates the IRQ line) and IER
                // (enable the transmit-empty interrupt).
                unsafe {
                    let mcr = arch::cpu::inb(0x3fc);
                    arch::cpu::outb(0x3fc, mcr | 0x08);
                    arch::cpu::outb(0x3f9, 0x02);
                }
            }
            fn stop() {
                // SAFETY: as in `start`.
                unsafe { arch::cpu::outb(0x3f9, 0) };
            }
            let (gsi, trigger, polarity) = crate::intc::isa_irq(4);
            (crate::intc::gsi_domain(), gsi, trigger, polarity, filter as ::irq::FilterFn, start as fn(), stop as fn())
        };
        #[cfg(target_arch = "aarch64")]
        let (domain, hw, trigger, polarity, filter, start, stop) = {
            let spcr = crate::acpi::find(*b"SPCR")
                .and_then(|t| acpi_tables::spcr::parse(&t).ok())
                .expect("SPCR");
            let va = crate::mm::map_device(addr::PhysAddr::new(spcr.port.base.address), 4096)
                .expect("map the PL011");
            BASE.store(usize::try_from(va.get()).unwrap(), Ordering::Relaxed);
            fn uart() -> regs::Mmio {
                // SAFETY: the PL011 window mapped above, for good.
                unsafe { regs::Mmio::new(BASE.load(Ordering::Relaxed), 4096) }
            }
            fn filter(arg: usize, _: &sched::IrqCx) -> ::irq::Filter {
                let u = uart();
                // UARTMIS bit 5: transmit interrupt.
                if u.read32(0x40) & (1 << 5) == 0 {
                    return ::irq::Filter::Stray;
                }
                u.write32(0x44, 1 << 5); // UARTICR
                u.write32(0x38, u.read32(0x38) & !(1 << 5)); // UARTIMSC
                // `arg` 1: leave the counting to the interrupt thread.
                if arg == 1 {
                    return ::irq::Filter::Schedule;
                }
                HITS.fetch_add(1, Ordering::Relaxed);
                ::irq::Filter::Handled
            }
            fn start() {
                let u = uart();
                u.write32(0x38, u.read32(0x38) | (1 << 5));
                // A transmitted character raises it.
                kprintln!("ktest: pl011 transmit interrupt requested");
            }
            fn stop() {
                let u = uart();
                u.write32(0x38, u.read32(0x38) & !(1 << 5));
            }
            (crate::intc::gsi_domain(), spcr.gsiv.expect("SPCR GSIV"), ::irq::Trigger::Level, ::irq::Polarity::High, filter as ::irq::FilterFn, start as fn(), stop as fn())
        };
        let _ = &BASE;
        fn thread(_: usize, _: &sched::Sleepable) {
            HITS.fetch_add(1, Ordering::Relaxed);
        }
        // First a filter alone, then a filter that hands off to an
        // interrupt thread.
        for (arg, thread) in [(0, None), (1, Some(thread as ::irq::ThreadFn))] {
            HITS.store(0, Ordering::Relaxed);
            let h = ::irq::install(
                domain,
                hw,
                trigger,
                polarity,
                hal::CpuId::BOOT,
                ::irq::Handler { name: "ktest-uart", filter: Some(filter), thread, arg },
            )
            .expect("install");
            start();
            let t0 = crate::time::now_ns();
            while HITS.load(Ordering::Relaxed) == 0 && crate::time::now_ns() - t0 < 500_000_000 {
                crate::time::sleep_ns(&s, 1_000_000);
            }
            stop();
            assert!(HITS.load(Ordering::Relaxed) >= 1, "no interrupt from the UART (arg {arg})");
            assert!(h.count() >= 1);
            assert!(!h.storm_masked());
            h.remove(&s);
        }
    }
}

ktest! {
    fn dma_direct_coherent_and_bounced() {
        use vm::dma::{dir, Backend, DmaBuf, DmaConstraints, DmaMemory, DmaStream, DmaTag};
        let top = crate::mm::dma::top_of_ram();
        let mem: &'static dyn DmaMemory = &crate::mm::dma::KERNEL_DMA;

        // Coherent memory for a device that reaches everything.
        regs::device_shared! { struct Desc { addr: u64, len: u32, flags: u32 } }
        let tag = DmaTag::new(mem, DmaConstraints::UNLIMITED, top);
        assert_eq!(tag.backend(), Backend::Direct);
        let ring = tag.coherent::<Desc>(256).expect("coherent");
        ring.write(5, Desc { addr: 0x1234, len: 64, flags: 1 });
        assert_eq!(ring.read(5).addr, 0x1234);
        assert_eq!(mem.phys(mem.virt(ring.bus_addr()).as_ptr()), Some(ring.bus_addr()));
        drop(ring);

        // A heap buffer, mapped directly: the segments are its bytes.
        let s = DmaStream::new(&tag, alloc::vec![5u8; 10_000]);
        let out = s.map::<dir::ToDevice>().map_err(|(_, e)| e).expect("map");
        assert!(!out.bounced());
        let total: u64 = out.segments().iter().map(|g| g.len).sum();
        assert_eq!(total, 10_000);
        for g in out.segments() {
            // SAFETY: reading the device's view of our own buffer.
            assert_eq!(unsafe { mem.virt(g.bus).as_ptr().read() }, 5);
        }
        drop(out.unmap());

        // A device that reaches only the memory below the highest power of
        // two under the top of RAM: a page above it must bounce.
        let bits = u8::try_from(63 - (top - 1).leading_zeros()).unwrap();
        let low = DmaTag::new(mem, DmaConstraints { addr_bits: bits, ..DmaConstraints::UNLIMITED }, top);
        assert_eq!(low.backend(), Backend::Bounce);
        let limit = 1u64 << bits;
        // With RAM starting at the limit (aarch64 `virt` with 1 GiB at
        // 1 GiB), no power-of-two limit splits RAM: nothing to bounce into.
        let Some(probe) = mem.alloc(4096, 4096, limit) else {
            kprintln!("ktest: no RAM below {limit:#x}; bounce path not exercised");
            return;
        };
        // SAFETY: just allocated, unused.
        unsafe { mem.free(probe, 4096) };
        let mut held = alloc::vec::Vec::new();
        let high = loop {
            let pa = crate::mm::alloc_pages(vm::Order::ZERO).expect("a page");
            if pa.get() >= limit {
                break pa;
            }
            held.push(pa);
            assert!(held.len() < 1_000_000, "no page above {limit:#x}");
        };
        for pa in held.drain(..) {
            crate::mm::free_pages(pa, vm::Order::ZERO);
        }
        struct Page(addr::PhysAddr);
        impl DmaBuf for Page {
            fn bytes(&self) -> &[u8] {
                let va = bootproto::layout::dmap(self.0).unwrap().get() as *const u8;
                // SAFETY: a page we own, through the direct map.
                unsafe { core::slice::from_raw_parts(va, 4096) }
            }
            fn bytes_mut(&mut self) -> &mut [u8] {
                let va = bootproto::layout::dmap(self.0).unwrap().get() as *mut u8;
                // SAFETY: as above, exclusively.
                unsafe { core::slice::from_raw_parts_mut(va, 4096) }
            }
        }
        let mut s = DmaStream::new(&low, Page(high));
        s.buf_mut().bytes_mut().fill(0xa5);
        let out = s.map::<dir::ToDevice>().map_err(|(_, e)| e).expect("bounce map");
        assert!(out.bounced());
        let g = out.segments()[0];
        assert!(g.bus + g.len <= limit);
        // SAFETY: the bounce page the device would read.
        assert_eq!(unsafe { mem.virt(g.bus).as_ptr().add(4095).read() }, 0xa5);
        let s = out.unmap();
        let inn = s.map::<dir::FromDevice>().map_err(|(_, e)| e).expect("bounce map");
        let g = inn.segments()[0];
        // SAFETY: the device writing its bounce page.
        unsafe { mem.virt(g.bus).as_ptr().write_bytes(0x3c, 4096) };
        let page = inn.unmap().into_inner();
        assert!(page.bytes().iter().all(|&b| b == 0x3c), "copied back");
        crate::mm::free_pages(page.0, vm::Order::ZERO);
    }
}

/// Drivers for the device manager's tests, listed in the ktest build's
/// driver table (`devices::DRIVERS`).
pub(crate) mod bus_test {
    use alloc::string::String;
    use alloc::vec;
    use core::sync::atomic::{AtomicU64, Ordering};

    use bus::{
        AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, Resource, StartCx,
        StopCx, StopReason, TestId, TestIdent,
    };
    use devtree::matching::Rank;

    regs::register_block! {
        /// A fake device's registers (a RAM page stands in).
        pub(crate) struct Fake: 4096 {
            VALUE @ 0 => u32, Rw;
        }
    }

    /// RAM pages the test bus hands its devices as "registers".
    pub(crate) static PAGES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

    pub(crate) struct TestBus;

    impl Driver for TestBus {
        const NAME: &'static str = "testbus";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 1,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Test bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            for page in &PAGES {
                let mem = Resource::Memory {
                    base: page.load(Ordering::Relaxed),
                    len: 4096,
                    prefetchable: false,
                };
                cx.add_child(Ident::Test(TestIdent { model: 2 }), None, vec![mem]);
            }
            cx.add_child(Ident::Test(TestIdent { model: 4 }), None, vec![]);
            cx.add_child(Ident::Test(TestIdent { model: 5 }), None, vec![]);
            let slow = cx.add_child(Ident::Test(TestIdent { model: 7 }), None, vec![]);
            cx.add_child(
                Ident::Test(TestIdent { model: 6 }),
                Some(alloc::sync::Arc::new(slow)),
                vec![],
            );
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
            // Children stop before their bus.
            for page in &PAGES {
                let pa = addr::PhysAddr::new(page.load(Ordering::Relaxed));
                let va = bootproto::layout::dmap(pa).unwrap().get() as *const u32;
                // SAFETY: the test's RAM page, through the direct map.
                let word = unsafe { va.read_volatile() };
                assert_eq!(word, 0x5709, "a child outlived its bus's stop");
            }
        }
    }

    pub(crate) struct TestDev {
        regs: MmioGrant<Fake>,
    }

    impl Driver for TestDev {
        const NAME: &'static str = "testdev";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 2,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Test device".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            let regs = cx.mmio::<Fake>(0)?;
            regs.write(Fake::VALUE, 0xabcd_0000 | cx.unit());
            Ok(Self { regs })
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
            self.regs.write(Fake::VALUE, 0x5709);
        }
    }

    /// Claims model 4 at a high rank, then declines it.
    pub(crate) struct Picky;

    impl Driver for Picky {
        const NAME: &'static str = "picky";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 4,
            rank: Rank::VENDOR,
        }]);
        fn describe(_: &Ident) -> String {
            "Picky".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Err(AttachError::Decline)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Takes model 4 at a generic rank.
    pub(crate) struct Fallback;

    impl Driver for Fallback {
        const NAME: &'static str = "fallback";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 4,
            rank: Rank::GENERIC,
        }]);
        fn describe(_: &Ident) -> String {
            "Fallback".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Fails to attach model 5.
    pub(crate) struct Broken;

    impl Driver for Broken {
        const NAME: &'static str = "broken";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 5,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Broken".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Err(AttachError::Device("broken on purpose"))
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Takes a while to attach (model 7).
    pub(crate) struct Slow;

    impl Driver for Slow {
        const NAME: &'static str = "slow";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 7,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Slow".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            crate::time::sleep_ns(cx.sleepable(), 20_000_000);
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Needs the slow device (named by its bus data) attached first, and
    /// says so rather than failing (model 6).
    pub(crate) struct Needy;

    /// How many times `Needy::attach` ran.
    pub(crate) static NEEDY_TRIES: AtomicU64 = AtomicU64::new(0);

    impl Driver for Needy {
        const NAME: &'static str = "needy";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 6,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Needy".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            NEEDY_TRIES.fetch_add(1, Ordering::Relaxed);
            let sup = *cx.bus_data::<bus::DeviceId>().expect("supplier id");
            if bus::manager().state(sup, cx.sleepable()) != Some(bus::NodeState::Attached) {
                return Err(AttachError::Wait(bus::Need::Device(sup)));
            }
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    pub(crate) const SLOW: DriverDecl = DriverDecl::of::<Slow>();
    pub(crate) const NEEDY: DriverDecl = DriverDecl::of::<Needy>();
    pub(crate) const TEST_BUS: DriverDecl = DriverDecl::of::<TestBus>();
    pub(crate) const TEST_DEV: DriverDecl = DriverDecl::of::<TestDev>();
    pub(crate) const PICKY: DriverDecl = DriverDecl::of::<Picky>();
    pub(crate) const FALLBACK: DriverDecl = DriverDecl::of::<Fallback>();
    pub(crate) const BROKEN: DriverDecl = DriverDecl::of::<Broken>();

    /// The models `Ordered` attached, in order.
    pub(crate) static ATTACHED: sync::SpinLock<alloc::vec::Vec<u32>> =
        sync::SpinLock::new(alloc::vec::Vec::new());

    /// A bus whose children name dependencies after adding them: model 8
    /// needs model 9, added after it; models 10 and 11 need each other.
    pub(crate) struct DepBus;

    impl Driver for DepBus {
        const NAME: &'static str = "depbus";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 12,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Dependency test bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            let mut add = |model| cx.add_child(Ident::Test(TestIdent { model }), None, vec![]);
            let (early, late, a, b) = (add(8), add(9), add(10), add(11));
            use devtree::graph::EdgeKind::Required;
            cx.add_dependency(early, late, Required);
            cx.add_dependency(a, b, Required);
            cx.add_dependency(b, a, Required);
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Records the order its devices attach in.
    pub(crate) struct Ordered;

    impl Driver for Ordered {
        const NAME: &'static str = "ordered";
        const IDS: Ids = Ids::Test(&[
            TestId {
                model: 8,
                rank: Rank::DEFAULT,
            },
            TestId {
                model: 9,
                rank: Rank::DEFAULT,
            },
            TestId {
                model: 10,
                rank: Rank::DEFAULT,
            },
            TestId {
                model: 11,
                rank: Rank::DEFAULT,
            },
        ]);
        fn describe(_: &Ident) -> String {
            "Ordered test device".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            if let Ident::Test(t) = cx.ident() {
                ATTACHED.lock().push(t.model);
            }
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    pub(crate) const DEP_BUS: DriverDecl = DriverDecl::of::<DepBus>();
    pub(crate) const ORDERED: DriverDecl = DriverDecl::of::<Ordered>();
}

ktest! {
    fn forward_dependencies_and_cycles() {
        use bus::{Ident, NodeState, TestIdent};
        let s = cx();
        let m = bus::manager();
        bus_test::ATTACHED.lock().clear();
        let db = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 12 }), None, alloc::vec![], &[]);
        m.wait_idle(&s);
        assert_eq!(m.state(db, &s), Some(NodeState::Attached));
        let order = bus_test::ATTACHED.lock().clone();
        let at = |model| order.iter().position(|&x| x == model).unwrap_or_else(|| panic!("model {model} never attached: {order:?}"));
        // A dependency on a sibling added later still holds the device
        // back; a cycle is broken so both attach.
        assert!(at(9) < at(8), "model 8 attached before the model 9 it needs: {order:?}");
        let _ = (at(10), at(11));
        for c in m.children(db, &s) {
            assert_eq!(m.state(c, &s), Some(NodeState::Attached));
        }
        m.shutdown_under(db, &s);
        let _ = m.take_events();
    }
}

ktest! {
    fn device_manager_lifecycle() {
        use bus::{DetachReason, Ident, NodeState, TestIdent};
        use bus_test::PAGES;
        let s = cx();
        let m = bus::manager();
        let pages: alloc::vec::Vec<addr::PhysAddr> =
            (0..2).map(|_| crate::mm::alloc_pages(vm::Order::ZERO).expect("page")).collect();
        for (slot, pa) in PAGES.iter().zip(&pages) {
            slot.store(pa.get(), Ordering::Relaxed);
        }
        let word = |pa: addr::PhysAddr| {
            let va = bootproto::layout::dmap(pa).unwrap().get() as *const u32;
            // SAFETY: a RAM page this test owns, through the direct map.
            unsafe { va.read_volatile() }
        };
        let _ = m.take_events();

        for round in 0..2 {
            let tb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 1 }), None, alloc::vec![], &[]);
            m.wait_idle(&s);
            let snap = m.snapshot(&s);
            let names: alloc::vec::Vec<&str> = snap.iter().map(|n| n.2.as_str()).collect();
            for want in ["testbus0", "testdev0", "testdev1", "fallback0", "slow0", "needy0"] {
                assert!(names.contains(&want), "round {round}: {want} missing from {names:?}");
            }
            assert!(!names.contains(&"picky0"), "the declining driver keeps no unit");
            let broken = snap.iter().find(|n| matches!(n.4, NodeState::Failed(_)));
            assert!(broken.is_some(), "the failing device is Failed");
            assert_eq!(broken.unwrap().2, "", "a failed attach frees its unit");
            // The drivers wrote through their mappings.
            let mut written = [word(pages[0]), word(pages[1])];
            written.sort_unstable();
            assert_eq!(written, [0xabcd_0000, 0xabcd_0001]);

            check_bus_sysctls(&s);

            let events = m.take_events();
            assert!(events.iter().any(|e| e.starts_with("+testbus0 at  model=1 on root0")), "{events:?}");
            assert!(events.iter().any(|e| e.starts_with("+testdev1 at  model=2 on testbus0")), "{events:?}");

            // Detach: children first, stop runs, mappings go, children are
            // deleted with their bus.
            m.detach(tb, DetachReason::Requested, &s).expect("detach");
            assert_eq!(word(pages[0]), 0x5709, "stop ran before the unmap");
            let after: alloc::vec::Vec<alloc::string::String> =
                m.snapshot(&s).into_iter().map(|n| n.2).collect();
            assert!(!after.iter().any(|n| n.starts_with("testdev") || n.starts_with("fallback") || n.starts_with("needy")), "{after:?}");
            let events = m.take_events();
            assert!(events.iter().any(|e| e.starts_with("-testdev0 at ")), "{events:?}");
            assert!(events.iter().any(|e| e.starts_with("-testbus0 at ")), "{events:?}");
            assert_eq!(m.state(tb, &s), Some(NodeState::NotPresent));
            let gone = crate::sysctl::read(&s, |t| t.name2oid("dev.testdev.0"));
            assert!(gone.is_none(), "detach removes dev.testdev.0");
        }
        for pa in pages {
            crate::mm::free_pages(pa, vm::Order::ZERO);
        }
    }
}

/// `devinfo`'s view (hw.bus.info, hw.bus.devices) and `dev.*` match the
/// tree.
fn check_bus_sysctls(s: &sched::Sleepable) {
    use abi::bus::{UDevice, ds};
    let read = |oid: &[i32]| {
        let mut out = alloc::vec::Vec::new();
        crate::sysctl::read_oid(s, oid, &mut out).map(|()| out)
    };
    let oid = |name: &str| {
        crate::sysctl::read(s, |t| t.name2oid(name)).unwrap_or_else(|| panic!("{name}"))
    };
    let text = |name: &str| {
        let v = read(&oid(name)).unwrap();
        alloc::string::String::from_utf8(v[..v.len() - 1].to_vec()).unwrap()
    };
    assert_eq!(text("dev.nexus.0.%parent"), "root0");
    assert_eq!(text("dev.nexus.0.%driver"), "nexus");
    #[cfg(target_arch = "x86_64")]
    assert_eq!(text("dev.apic.0.%desc"), "APIC resources");
    let m = bus::manager();
    assert_eq!(
        m.nameunit(crate::devices::nexus(), s).as_deref(),
        Some("nexus0")
    );
    assert_eq!(
        m.detach(crate::devices::nexus(), bus::DetachReason::Requested, s),
        Err(bus::DetachError::Busy),
        "the platform root refuses a requested detach"
    );
    assert_eq!(text("dev.testdev.1.%parent"), "testbus0");
    assert_eq!(text("dev.testdev.1.%driver"), "testdev");
    assert_eq!(text("dev.testdev.1.%desc"), "Test device");
    assert_eq!(text("dev.testdev.1.%pnpinfo"), "model=2");
    assert!(
        crate::sysctl::read(s, |t| t.name2oid("dev.broken.0")).is_none(),
        "failed attach leaves no node"
    );

    let info = read(&oid("hw.bus.info")).unwrap();
    assert_eq!(info.len(), 8);
    let generation = i32::from_ne_bytes(info[4..8].try_into().unwrap());
    let devices = oid("hw.bus.devices");
    let mut names = alloc::vec::Vec::new();
    let mut root_handle = 0;
    for index in 0.. {
        let mut o = devices.clone();
        o.extend([generation, index]);
        match read(&o) {
            Ok(b) => {
                assert_eq!(b.len(), size_of::<UDevice>());
                let handle = usize::from_ne_bytes(b[..8].try_into().unwrap());
                let parent = usize::from_ne_bytes(b[8..16].try_into().unwrap());
                let state = i32::from_ne_bytes(b[24..28].try_into().unwrap());
                let name: alloc::vec::Vec<u8> =
                    b[28..].iter().copied().take_while(|&c| c != 0).collect();
                let name = alloc::string::String::from_utf8(name).unwrap();
                if parent == 0 {
                    assert_eq!(name, "root0");
                    root_handle = handle;
                }
                if name == "testbus0" {
                    assert_eq!(parent, root_handle);
                    assert_eq!(state, ds::ATTACHED);
                }
                names.push(name);
            }
            Err(sysctl::ReadError::NotFound) => break,
            Err(e) => panic!("hw.bus.devices.{generation}.{index}: {e:?}"),
        }
    }
    for want in [
        "root0", "nexus0", "testbus0", "testdev0", "testdev1", "needy0",
    ] {
        assert!(names.iter().any(|n| n == want), "{want} not in {names:?}");
    }
    // A stale generation is refused, as devinfo expects.
    let mut o = devices.clone();
    o.extend([generation.wrapping_sub(1), 0]);
    assert_eq!(read(&o), Err(sysctl::ReadError::Invalid));
}

ktest! {
    fn device_shutdown_order() {
        use bus::{Ident, NodeState, TestIdent};
        let s = cx();
        let m = bus::manager();
        let pages: alloc::vec::Vec<addr::PhysAddr> =
            (0..2).map(|_| crate::mm::alloc_pages(vm::Order::ZERO).expect("page")).collect();
        for (slot, pa) in bus_test::PAGES.iter().zip(&pages) {
            slot.store(pa.get(), Ordering::Relaxed);
        }
        let tb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 1 }), None, alloc::vec![], &[]);
        m.wait_idle(&s);
        assert_eq!(m.state(tb, &s), Some(NodeState::Attached));
        // TestBus::stop checks its children were stopped first. Only the
        // test bus's subtree: other tests need acpi0.
        m.shutdown_under(tb, &s);
        assert_eq!(m.state(tb, &s), Some(NodeState::NotPresent));
        assert_eq!(m.state(crate::devices::nexus(), &s), Some(NodeState::Attached), "kernel-owned devices stay");
        let attached: alloc::vec::Vec<_> = m
            .children(tb, &s)
            .into_iter()
            .filter(|&c| m.state(c, &s) == Some(NodeState::Attached))
            .collect();
        assert!(attached.is_empty(), "still attached after shutdown: {attached:?}");
        let _ = m.take_events();
        for pa in pages {
            crate::mm::free_pages(pa, vm::Order::ZERO);
        }
    }
}

ktest! {
    fn device_attach_fault_injection() {
        use bus::{DetachReason, Ident, NodeState, TestIdent};
        let s = cx();
        let m = bus::manager();
        let pages: alloc::vec::Vec<addr::PhysAddr> =
            (0..2).map(|_| crate::mm::alloc_pages(vm::Order::ZERO).expect("page")).collect();
        for (slot, pa) in bus_test::PAGES.iter().zip(&pages) {
            slot.store(pa.get(), Ordering::Relaxed);
        }
        let claims = m.claims();
        // Every other grant fails, with varying seeds: attach error paths
        // must give back what they claimed (the ledger check asserts it
        // per detach) and free their units.
        for seed in 1..=8 {
            m.inject_faults(2, seed);
            let tb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 1 }), None, alloc::vec![], &[]);
            m.wait_idle(&s);
            if m.state(tb, &s) == Some(NodeState::Attached) {
                m.detach(tb, DetachReason::Requested, &s).expect("detach");
            }
            m.inject_faults(0, 0);
            assert_eq!(m.claims(), claims, "seed {seed}: a failed attach kept a claim");
        }
        // With faults off, units start from 0 again: none leaked.
        let tb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 1 }), None, alloc::vec![], &[]);
        m.wait_idle(&s);
        let names: alloc::vec::Vec<_> = m.snapshot(&s).into_iter().map(|n| n.2).collect();
        for want in ["testdev0", "testdev1", "needy0"] {
            assert!(names.iter().any(|n| n == want), "{want} missing: {names:?}");
        }
        m.detach(tb, DetachReason::Requested, &s).expect("detach");
        assert_eq!(m.claims(), claims);
        let _ = m.take_events();
        for pa in pages {
            crate::mm::free_pages(pa, vm::Order::ZERO);
        }
    }
}

ktest! {
    fn gates_drain_and_refuse() {
        use alloc::sync::Arc;
        use core::sync::atomic::{AtomicBool, AtomicU64};
        let s = cx();
        let gate = Arc::new(bus::Gate::new());
        let stop = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicU64::new(0));
        let workers: alloc::vec::Vec<_> = (0..3)
            .map(|_| {
                let (g, st, n) = (Arc::clone(&gate), Arc::clone(&stop), Arc::clone(&entered));
                sched::spawn("gate", sched::Priority::KERNEL, move |_| {
                    while !st.load(Ordering::Acquire) {
                        if let Ok(_guard) = g.enter() {
                            n.fetch_add(1, Ordering::Relaxed);
                            core::hint::spin_loop();
                        }
                    }
                })
                .unwrap()
            })
            .collect();
        while entered.load(Ordering::Relaxed) < 1000 {
            crate::time::sleep_ns(&s, 100_000);
        }
        gate.revoke(&s);
        let after = entered.load(Ordering::Relaxed);
        crate::time::sleep_ns(&s, 2_000_000);
        assert!(gate.enter().is_err());
        // Calls that entered before the revocation finished; none since.
        assert!(entered.load(Ordering::Relaxed) <= after + 3);
        stop.store(true, Ordering::Release);
        for w in workers {
            w.join(&s);
        }
    }
}

ktest! {
    fn pci_enumeration_through_ecam() {
        let windows = crate::pci::windows();
        assert!(!windows.is_empty(), "MCFG gave no ECAM window");
        let w = &windows[0];
        /// Counts configuration accesses, to keep enumeration's cost in view.
        struct Counting<'a>(&'a dyn pci::ConfigSpace, core::cell::Cell<u32>, core::cell::Cell<[u64; 4]>);
        impl Counting<'_> {
            fn time<R>(&self, kind: usize, f: impl FnOnce() -> R) -> R {
                self.1.set(self.1.get() + 1);
                let t = crate::time::counter();
                let r = f();
                let mut v = self.2.get();
                v[kind] += crate::time::counter() - t;
                self.2.set(v);
                r
            }
        }
        impl pci::ConfigSpace for Counting<'_> {
            fn read32(&self, a: pci::PciAddress, o: u16) -> u32 {
                self.time(0, || self.0.read32(a, o))
            }
            fn write32(&self, a: pci::PciAddress, o: u16, v: u32) {
                let kind = if o == pci::header::COMMAND { 1 } else if (0x10..0x28).contains(&o) { 2 } else { 3 };
                self.time(kind, || self.0.write32(a, o, v));
            }
        }
        let counting = Counting(w, core::cell::Cell::new(0), core::cell::Cell::new([0; 4]));
        let t0 = crate::time::counter();
        let (fns, problems) = pci::enumerate(&counting, w.segment(), w.buses());
        let us = (crate::time::counter() - t0) * 1_000_000 / crate::time::frequency();
        let f = crate::time::frequency() / 1_000_000;
        let k = counting.2.get().map(|t| t / f.max(1));
        kprintln!(
            "pci: enumeration took {us} us for {} accesses (reads {} us, command writes {} us, BAR writes {} us, other {} us)",
            counting.1.get(), k[0], k[1], k[2], k[3]
        );
        for f in &fns {
            let i = &f.header.ident;
            kprintln!(
                "pci: {:02x}:{:02x}.{} {:04x}:{:04x} class {:06x} bars {} caps {}",
                f.addr.bus, f.addr.slot, f.addr.function, i.vendor, i.device, i.class,
                f.bars.len(), f.caps.len()
            );
        }
        assert!(problems.is_empty(), "{problems:?}");
        let host = fns.first().expect("functions on the root bus");
        assert_eq!((host.addr.bus, host.addr.slot, host.addr.function), (0, 0, 0));
        assert_eq!(host.header.ident.class, 0x06_00_00, "a host bridge at 00:00.0");
        // virtio-blk and virtio-rng, with sized memory BARs.
        let virtio: alloc::vec::Vec<_> = fns.iter().filter(|f| f.header.ident.vendor == 0x1af4).collect();
        assert!(virtio.len() >= 2, "virtio functions: {}", virtio.len());
        for f in virtio {
            // Firmware may leave a BAR unassigned (aarch64 EDK2 does the
            // legacy I/O BAR); nothing else is acceptable.
            assert!(
                f.bad_bars.iter().all(|(_, e)| *e == pci::BarError::Unassigned),
                "{:?}",
                f.bad_bars
            );
            assert!(
                f.bars.iter().any(|b| b.kind != pci::BarKind::Io && b.size >= 0x1000),
                "a memory BAR on {:?}",
                f.addr
            );
            // Modern devices (IDs from 0x1040) describe their transport in
            // vendor capabilities; bhyve's are legacy-only.
            if f.header.ident.device >= 0x1040 {
                assert!(pci::caps::find(&f.caps, pci::caps::id::VENDOR).is_some());
            }
            // Config space reads back the same through the lookup.
            let cfg = crate::pci::config_for(f.addr).unwrap();
            assert_eq!(cfg.read16(f.addr, 0), 0x1af4);
        }
    }
}

ktest! {
    fn acpi_bus_children() {
        use bus::Ident;
        let s = cx();
        let m = bus::manager();
        m.wait_idle(&s);
        let nexus = crate::devices::nexus();
        let acpi0 = m
            .children(nexus, &s)
            .into_iter()
            .find(|&c| m.nameunit(c, &s).as_deref() == Some("acpi0"))
            .expect("acpi0 on nexus0");
        let desc = m.description(acpi0, &s).unwrap();
        kprintln!("acpi: acpi0 <{desc}>");
        // QEMU's OEM ID, or bhyve's, which FreeBSD shows as
        // "BHYVE BVFACP " (tests/corpus/freebsd/bhyve).
        assert!(desc.starts_with("BOCHS ") || desc == "BHYVE BVFACP ", "{desc:?}");
        let children = m.children(acpi0, &s);
        let paths: alloc::vec::Vec<(alloc::string::String, alloc::string::String)> = children
            .iter()
            .filter_map(|&c| match m.ident(c, &s) {
                Some(Ident::Acpi(i, p)) => Some((i.hid.unwrap_or_default(), p)),
                _ => None,
            })
            .collect();
        for (hid, path) in &paths {
            kprintln!("acpi:   {hid} at {path}");
        }
        if desc == "BHYVE BVFACP " {
            // Differential: the same children, in the same order, with the
            // same pnpinfo, as FreeBSD 15.1's devinfo in the same VM.
            let ours: alloc::vec::Vec<alloc::string::String> = children
                .iter()
                .filter_map(|&c| {
                    let mut line = alloc::string::String::new();
                    let ident = m.ident(c, &s)?;
                    ident.pnpinfo(&mut line);
                    line.push_str(" at ");
                    ident.location(&mut line);
                    Some(line)
                })
                .collect();
            let recorded: alloc::vec::Vec<&str> =
                include_str!("../../tests/corpus/freebsd/bhyve/devinfo-rv.txt")
                    .lines()
                    .filter(|l| l.starts_with("    ") && !l.starts_with("     "))
                    .filter_map(|l| l.split_once(" pnpinfo ").map(|(_, rest)| rest))
                    .filter(|rest| rest.starts_with("_HID="))
                    .collect();
            assert_eq!(recorded.len(), 21, "the recording's ACPI children");
            assert_eq!(ours, recorded);
        }
        // The host bridge, a flat child of acpi0 as on FreeBSD.
        // (PNP0A08 at \_SB_.PCI0 on QEMU, PNP0A03 at \_SB_.PC00 on bhyve.)
        assert!(
            paths.iter().any(|(h, p)| (h == "PNP0A08" || h == "PNP0A03") && p.starts_with("\\_SB_.PC")),
            "{paths:?}"
        );
        #[cfg(target_arch = "x86_64")]
        {
            // q35's PCI interrupt links and an ISA device under the LPC
            // bridge (\_SB_.PCI0.SF8_ is q35's LPC).
            assert!(paths.iter().filter(|(h, _)| h == "PNP0C0F").count() >= 4, "{paths:?}");
            assert!(paths.iter().any(|(h, _)| h == "PNP0303" || h == "PNP0501" || h == "PNP0B00"));
        }
    }
}

ktest! {
    fn pci_bus_children() {
        use bus::Ident;
        let s = cx();
        let m = bus::manager();
        let named = |parent, name: &str| {
            m.children(parent, &s)
                .into_iter()
                .find(|&c| m.nameunit(c, &s).as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        let acpi0 = named(crate::devices::nexus(), "acpi0");
        let pcib0 = named(acpi0, "pcib0");
        assert_eq!(m.description(pcib0, &s).as_deref(), Some("ACPI Host-PCI bridge"));
        let pci0 = named(pcib0, "pci0");
        assert_eq!(m.description(pci0, &s).as_deref(), Some("ACPI PCI bus"));
        let mut lines = alloc::vec::Vec::new();
        let mut virtio = 0;
        for c in m.children(pci0, &s) {
            let Some(ident @ Ident::Pci(i, ..)) = m.ident(c, &s) else {
                panic!("a non-PCI child of pci0");
            };
            let mut line = alloc::string::String::new();
            ident.pnpinfo(&mut line);
            line.push_str(" at ");
            ident.location(&mut line);
            kprintln!("pci0: {line}");
            virtio += usize::from(i.vendor == 0x1af4);
            lines.push(line);
        }
        assert!(virtio >= 1, "virtio functions on pci0");
        if m.description(acpi0, &s).as_deref() == Some("BHYVE BVFACP ") {
            // Differential: each function's pnpinfo and location as FreeBSD
            // 15.1 recorded them in bhyve (the recording's VM has more
            // devices than the boot test's).
            let recorded: alloc::vec::Vec<&str> =
                include_str!("../../tests/corpus/freebsd/bhyve/devinfo-rv.txt")
                    .lines()
                    .filter(|l| l.starts_with("        ") && !l.starts_with("         "))
                    .filter_map(|l| l.split_once(" pnpinfo ").map(|(_, rest)| rest))
                    .collect();
            for l in &lines {
                assert!(recorded.contains(&l.as_str()), "{l:?} not in FreeBSD's recording");
            }
            assert!(lines.iter().any(|l| l.ends_with("handle=\\_SB_.PC00.ISA_")), "isab's ACPI handle");
            // The virtio transports and the devices on them (vtblk0, vtrnd0,
            // vtblk1), line for line.
            let full: alloc::vec::Vec<&str> =
                include_str!("../../tests/corpus/freebsd/bhyve/devinfo-rv.txt")
                    .lines()
                    .map(str::trim_start)
                    .collect();
            // As `devinfo -v` prints a device: empty fields left out.
            let line = |id: bus::DeviceId| {
                let ident = m.ident(id, &s).unwrap();
                let mut l = m.nameunit(id, &s).unwrap_or_default();
                let desc = m.description(id, &s).unwrap_or_default();
                if !desc.is_empty() {
                    l.push_str(&alloc::format!(" <{desc}>"));
                }
                let mut pnp = alloc::string::String::new();
                ident.pnpinfo(&mut pnp);
                if !pnp.is_empty() {
                    l.push_str(" pnpinfo ");
                    l.push_str(&pnp);
                }
                let mut loc = alloc::string::String::new();
                ident.location(&mut loc);
                if !loc.is_empty() {
                    l.push_str(" at ");
                    l.push_str(&loc);
                }
                if m.is_disabled(id, &s) {
                    l.push_str(" (disabled)");
                }
                l
            };
            // Every attached function (the virtio transports, nvme0,
            // ahci0) and its children, attached or disabled.
            let mut transports = 0;
            for c in m.children(pci0, &s) {
                if m.state(c, &s) != Some(bus::NodeState::Attached) {
                    continue;
                }
                transports += usize::from(m.nameunit(c, &s).is_some_and(|n| n.starts_with("virtio_pci")));
                let l = line(c);
                kprintln!("pci0: {l}");
                assert!(full.contains(&l.as_str()), "{l:?} not in FreeBSD's recording");
                for v in m.children(c, &s) {
                    let l = line(v);
                    kprintln!("pci0:   {l}");
                    assert!(full.contains(&l.as_str()), "{l:?} not in FreeBSD's recording");
                }
            }
            assert!(transports >= 2, "virtio_pci attached to the virtio functions");
        }
    }
}

/// A node named `name` under `root`, with its parent.
fn find_named(
    m: &bus::Manager,
    s: &sched::Sleepable,
    root: bus::DeviceId,
    name: &str,
) -> Option<(bus::DeviceId, bus::DeviceId)> {
    let mut todo = alloc::vec![root];
    while let Some(p) = todo.pop() {
        for c in m.children(p, s) {
            if m.nameunit(c, s).as_deref() == Some(name) {
                return Some((p, c));
            }
            todo.push(c);
        }
    }
    None
}

ktest! {
    fn vtrnd_reads_entropy_by_interrupt() {
        let s = cx();
        let m = bus::manager();
        let nexus = crate::devices::nexus();
        let (transport, vtrnd) = find_named(m, &s, nexus, "vtrnd0").expect("vtrnd0 attached");
        let tname = m.nameunit(transport, &s).unwrap_or_default();
        let tdesc = m.description(transport, &s).unwrap_or_default();
        assert!(tname.starts_with("virtio_pci"), "vtrnd0 is on {tname}");
        // bhyve's devices are legacy-only; QEMU's are transitional, and
        // driven through the modern interface as FreeBSD 16 does.
        let bhyve = find_named(m, &s, nexus, "acpi0")
            .and_then(|(_, a)| m.description(a, &s))
            .as_deref()
            == Some("BHYVE BVFACP ");
        let kind = if bhyve { "legacy" } else { "modern" };
        assert_eq!(tdesc, alloc::format!("VirtIO PCI ({kind}) Entropy adapter"));
        assert_eq!(m.description(vtrnd, &s).as_deref(), Some("VirtIO Entropy Adapter"));
        assert_eq!(m.state(vtrnd, &s), Some(bus::NodeState::Attached));

        let before = virtio_random::interrupts().expect("an entropy source");
        let mut buf = [0u8; 64];
        let mut nonzero = false;
        const READS: u64 = 32;
        let start = crate::time::counter();
        for _ in 0..READS {
            buf.fill(0);
            assert_eq!(virtio_random::read(&s, &mut buf), Some(64));
            nonzero |= buf.iter().any(|&b| b != 0);
        }
        let ns = (crate::time::counter() - start) * 1_000_000_000 / crate::time::frequency();
        // A device may complete a request before its interrupt arrives
        // (bhyve does the work in the notify exit): let the last land.
        for _ in 0..100 {
            if virtio_random::interrupts().unwrap() - before >= READS {
                break;
            }
            crate::time::sleep_ns(&s, 1_000_000);
        }
        let taken = virtio_random::interrupts().unwrap() - before;
        let shared = virtio_random::shared_line();
        let via = match shared {
            Some(false) => "MSI-X",
            Some(true) => "INTx",
            None => "polling",
        };
        kprintln!(
            "vtrnd: {READS} reads of 64 bytes, {} us each, {taken} interrupt(s) by {via}, on {tname} <{tdesc}>",
            ns / READS / 1000
        );
        assert!(nonzero, "the device wrote entropy");
        // Messages for an LPI or vector still pending merge into one (the
        // next request can complete first), so not one per read; but the
        // reads slept on interrupts, and a lost one would have hung them.
        assert!((1..=READS).contains(&taken), "{taken} interrupts");
        // Every platform here composes messages (amd64's local APIC, the
        // arm64 ITS) unless the boot case turned them off.
        let msix = crate::boot::value("hw.pci.enable_msix") != Some("0");
        assert_eq!(shared, Some(!msix), "MSI-X {}", if msix { "on" } else { "off" });
    }
}

ktest! {
    fn pci_bridge_and_the_bus_behind_it() {
        let s = cx();
        let m = bus::manager();
        let nexus = crate::devices::nexus();
        let acpi0 = find_named(m, &s, nexus, "acpi0").map(|(_, a)| a);
        if acpi0.and_then(|a| m.description(a, &s)).as_deref() == Some("BHYVE BVFACP ") {
            kprintln!("(the bhyve VM has no bridges)");
            return;
        }
        // QEMU's virtio-rng sits behind a PCIe root port (xtask).
        let (_, pci0) = find_named(m, &s, nexus, "pci0").expect("pci0");
        let bridge = m
            .children(pci0, &s)
            .into_iter()
            .find(|&c| m.nameunit(c, &s).is_some_and(|n| n.starts_with("pcib")))
            .expect("a PCI-PCI bridge on pci0");
        let desc = m.description(bridge, &s).unwrap_or_default();
        assert!(desc == "ACPI PCI-PCI bridge" || desc == "PCI-PCI bridge", "{desc:?}");
        let buses = m.children(bridge, &s);
        assert_eq!(buses.len(), 1, "one bus behind the bridge");
        let pci1 = buses[0];
        let Some(bus::Ident::PciBus { bus: number, acpi, .. }) = m.ident(pci1, &s) else {
            panic!("the bridge's child is not a PCI bus");
        };
        assert!(number > 0);
        assert_eq!(acpi, desc.starts_with("ACPI"), "pci1 is an ACPI bus when its bridge is");
        assert_eq!(m.state(pci1, &s), Some(bus::NodeState::Attached));
        let (transport, _) = find_named(m, &s, pci1, "vtrnd0").expect("vtrnd0 behind the bridge");
        kprintln!(
            "{} <{desc}> -> {} <{}> (bus {number}) -> {} -> vtrnd0",
            m.nameunit(bridge, &s).unwrap_or_default(),
            m.nameunit(pci1, &s).unwrap_or_default(),
            m.description(pci1, &s).unwrap_or_default(),
            m.nameunit(transport, &s).unwrap_or_default(),
        );
    }
}

ktest! {
    fn acpi_power_button() {
        use core::sync::atomic::Ordering;
        use crate::acpi_bus::{POWER_BUTTON_PRESSES, POWER_BUTTON_TEST};
        if crate::boot::value("abyss.test.power_button") != Some("1") {
            kprintln!("(no way to press this VM's power button)");
            return;
        }
        let s = cx();
        // Counted, not acted on, from now to the end of the run.
        POWER_BUTTON_TEST.store(true, Ordering::Release);
        let before = POWER_BUTTON_PRESSES.load(Ordering::Acquire);
        // q35: a fixed PM1 event on the SCI. arm64 virt: a GED interrupt
        // whose _EVT notifies the PNP0C0C device.
        kprintln!("{}", ktest::POWER_BUTTON_MARKER);
        let start = crate::time::counter();
        let ms = || (crate::time::counter() - start) * 1000 / crate::time::frequency();
        while POWER_BUTTON_PRESSES.load(Ordering::Acquire) == before && ms() < 10_000 {
            crate::time::sleep_ns(&s, 1_000_000);
        }
        assert!(POWER_BUTTON_PRESSES.load(Ordering::Acquire) > before, "no press arrived");
        kprintln!("power button press arrived after {} ms", ms());
    }
}

ktest! {
    fn virtio_transport_detach_and_reattach() {
        let s = cx();
        let m = bus::manager();
        let nexus = crate::devices::nexus();
        let (transport, _) = find_named(m, &s, nexus, "vtrnd0").expect("vtrnd0");
        let kva = || crate::mm::KVA.lock().allocated();
        let before = kva();
        // devctl detach: vtrnd first (its gate revoked, the device reset),
        // then the transport (vectors removed, MSI-X off, the table and BARs
        // unmapped).
        m.detach(transport, bus::DetachReason::Requested, &s).expect("detach");
        assert_eq!(m.state(transport, &s), Some(bus::NodeState::NotPresent));
        assert_eq!(virtio_random::read(&s, &mut [0u8; 16]), None, "no entropy source after detach");
        // At least the MSI-X table's page; a modern transport's register
        // BAR too (a legacy one's registers are I/O ports).
        let freed = before.saturating_sub(kva());
        assert!(freed >= 4096, "detach gave back {freed} bytes of mappings");
        // devctl attach: the same transport and a new vtrnd, working.
        m.probe_and_attach(transport, &s).expect("attach");
        m.wait_idle(&s);
        assert_eq!(m.state(transport, &s), Some(bus::NodeState::Attached));
        let (again, _) = find_named(m, &s, nexus, "vtrnd0").expect("vtrnd0 again");
        assert_eq!(again, transport);
        let mut buf = [0u8; 64];
        assert_eq!(virtio_random::read(&s, &mut buf), Some(64));
        assert!(buf.iter().any(|&b| b != 0));
        kprintln!("detach freed {} KiB of device mappings; re-attached and read", freed / 1024);
        let _ = m.take_events();
    }
}

ktest! {
    fn vtblk_reads_the_boot_disk_and_its_gpt() {
        let s = cx();
        let names = storage::disks();
        kprintln!("disks: {}", names.join(" "));
        let boot = storage::find("vtbd0").expect("vtbd0");
        assert_eq!(boot.sector_size(), 512);
        // The protective MBR's signature, then the GPT header.
        let head = boot.read(&s, 0, 1024).expect("read");
        assert_eq!(&head[510..512], &[0x55, 0xaa], "MBR signature");
        assert_eq!(&head[512..520], b"EFI PART", "GPT header");
        // The table, as `gpart show` lays it out.
        let parts = storage::partitions(&boot);
        kprintln!("=> {:>9} {:>9}  {}  GPT  ({}M)", 34, boot.sectors() - 67, boot.name(), boot.bytes() >> 20);
        for p in &parts {
            let (_, e) = p.partition().expect("a partition");
            kprintln!(
                "   {:>9} {:>9}  {:>2}  {}  ({}M) {}",
                e.first_lba,
                p.sectors(),
                e.index + 1,
                e.type_name(),
                p.bytes() >> 20,
                e.label
            );
        }
        // The ESP was found and published as a partition.
        assert_eq!(parts.first().and_then(|p| p.partition()).map(|(_, e)| e.type_name()).as_deref(), Some("efi"));
        let esp = storage::find("vtbd0p1").expect("vtbd0p1, the ESP");
        let boot_sector = esp.read(&s, 0, 512).expect("ESP boot sector");
        assert_eq!(&boot_sector[510..512], &[0x55, 0xaa], "FAT boot sector");
        assert_eq!(boot_sector[11..13], 512u16.to_le_bytes(), "FAT bytes per sector");
        // Requests are checked: misaligned and past the end are refused.
        assert_eq!(boot.read(&s, 3, 512), Err(storage::BioError::Invalid));
        assert_eq!(boot.read(&s, boot.bytes(), 512), Err(storage::BioError::Invalid));
        // A large read goes in several requests.
        let big = boot.read(&s, 0, 1 << 20).expect("1 MiB");
        assert_eq!(&big[..1024], &head[..]);
    }
}

/// Writes and reads back patterns at the start, across a 64 KiB boundary,
/// and at the end of `disk`, then 256 requests from four threads at once.
fn exercise_disk(s: &sched::Sleepable, disk: &alloc::sync::Arc<storage::Disk>) {
    let mut seed = crate::time::counter() | 1;
    let mut pattern = |n: usize| -> alloc::vec::Vec<u8> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed.to_le_bytes()[0]
            })
            .collect()
    };
    let tail = disk.sector_size() as usize * 300;
    let end = disk.bytes();
    for (at, len) in [(0, 4096), (65536 - 1024, 8192), (end - tail as u64, tail)] {
        let data = pattern(len);
        disk.write(s, at, &data).expect("write");
        let back = disk.read(s, at, len).expect("read back");
        if let Some(i) = back.iter().zip(&data).position(|(a, b)| a != b) {
            panic!(
                "{}: {len} bytes at {at:#x} differ from byte {i:#x}",
                disk.name()
            );
        }
    }
    disk.flush(s).expect("flush");
    let threads: alloc::vec::Vec<_> = (0..4u64)
        .map(|t| {
            let d = alloc::sync::Arc::clone(disk);
            sched::spawn("disk-test", sched::Priority::KERNEL, move |s| {
                for i in 0..64u64 {
                    let at = (t * 64 + i) * 4096;
                    let data = alloc::vec![(t * 64 + i).to_le_bytes()[0]; 4096];
                    d.write(s, at, &data).expect("write");
                    assert_eq!(d.read(s, at, 4096).expect("read"), data);
                }
            })
            .unwrap()
        })
        .collect();
    for t in threads {
        t.join(s);
    }
    assert_eq!(
        storage::bio::dropped(),
        0,
        "no request was dropped uncompleted"
    );
}

ktest! {
    fn vtblk_writes_and_reads_back() {
        let s = cx();
        let scratch = storage::find("vtbd1").expect("the scratch disk, vtbd1");
        assert_eq!(scratch.bytes(), 16 << 20);
        exercise_disk(&s, &scratch);
        kprintln!("{}: written and read back", scratch.name());
    }
}

ktest! {
    fn nvme_writes_and_reads_back() {
        let s = cx();
        let m = bus::manager();
        let (_, nvme) = find_named(m, &s, crate::devices::nexus(), "nvme0").expect("nvme0");
        assert_eq!(m.description(nvme, &s).as_deref(), Some("Generic NVMe Device"));
        let nda = storage::find("nda0").expect("nda0");
        assert_eq!(nda.bytes(), 16 << 20);
        let start = crate::time::counter();
        exercise_disk(&s, &nda);
        let ms = (crate::time::counter() - start) * 1000 / crate::time::frequency();
        kprintln!("{}: written and read back in {ms} ms", nda.name());
    }
}

ktest! {
    fn ahci_writes_and_reads_back() {
        let s = cx();
        let m = bus::manager();
        let (_, ahci) = find_named(m, &s, crate::devices::nexus(), "ahci0").expect("ahci0");
        let desc = m.description(ahci, &s).unwrap_or_default();
        assert!(desc.ends_with("AHCI SATA controller"), "{desc:?}");
        let channels = m.children(ahci, &s);
        let attached = channels
            .iter()
            .filter(|&&c| m.state(c, &s) == Some(bus::NodeState::Attached))
            .count();
        assert!(attached >= 1, "an attached channel");
        let ada = storage::find("ada0").expect("ada0");
        assert_eq!(ada.bytes(), 16 << 20);
        let start = crate::time::counter();
        exercise_disk(&s, &ada);
        let ms = (crate::time::counter() - start) * 1000 / crate::time::frequency();
        kprintln!(
            "{}: written and read back in {ms} ms ({} <{desc}>, {} channel(s), {attached} attached)",
            ada.name(),
            m.nameunit(ahci, &s).unwrap_or_default(),
            channels.len()
        );
    }
}

ktest! {
    fn random_is_seeded_and_fed() {
        let s = cx();
        assert!(random::seeded(), "seeded at boot (EFI_RNG, or the CPU's RNG)");
        let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
        crate::random::fill(&mut a).unwrap();
        crate::random::fill(&mut b).unwrap();
        assert_ne!(a, b);
        // Bits set about half the time: a gross check that it is not
        // stuck (the RFC vectors check the primitives on the host).
        let mut big = alloc::vec![0u8; 4096];
        crate::random::fill(&mut big).unwrap();
        let ones: u32 = big.iter().map(|b| b.count_ones()).sum();
        assert!((15_000..17_800).contains(&ones), "{ones} of 32768 bits set");
        // The harvester reseeds the root as the CPU's RNG and the entropy
        // device feed it.
        let g = random::generation();
        let start = crate::time::counter();
        while random::generation() == g
            && (crate::time::counter() - start) < 5 * crate::time::frequency()
        {
            crate::time::sleep_ns(&s, 10_000_000);
        }
        assert!(random::generation() > g, "no reseed in 5 s");
        kprintln!("random: generation {} (from {g})", random::generation());
    }
}
