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
use crate::devices::find_named;

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
            if crate::boot::flag("abyss.test.watchdog_reset") {
                // Hang with a watchdog armed: its expiry must reset us.
                let w = watchdog::first().expect("a watchdog to arm");
                let ms = w.arm(3000).expect("arm");
                kprintln!("watchdog: armed for {ms} ms on {}; hanging", w.name());
                crate::arch::cpu::disable_interrupts();
                loop {
                    core::hint::spin_loop();
                }
            }
            // The ACPI reset register (x86) or PSCI (arm64); RAM survives
            // either under QEMU, so the loader finds the log.
            crate::power::reset();
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
        // Five pages: threads' usual four would come from the stack cache.
        // The first stack in a region also creates intermediate page
        // tables, which stay; measure after one has come and gone.
        drop(KernelStack::new(5).expect("stack"));
        // Pages the heap holds count as free: other CPUs' allocations
        // move pages between it and the free lists meanwhile.
        let free_pages = || {
            let h = crate::mm::heap_stats();
            crate::mm::phys_stats().free + h.slab_pages + h.magazine_pages + h.large_pages
        };
        let kva = crate::mm::kstack::kva_allocated();
        let free = free_pages();
        let stack = KernelStack::new(5).expect("stack");
        assert_eq!(stack.top() - stack.bottom().get(), VirtAddr::new(5 * 4096));
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
        assert_eq!(free_pages(), free);
    }
}

ktest! {
    fn freed_thread_stacks_are_reused_mapped() {
        use crate::mm::kstack::{KernelStack, cached_pages, kva_allocated};
        let pages = crate::threads::STACK_PAGES;
        let kva = kva_allocated();
        let stack = KernelStack::new(pages).expect("stack");
        // After the stack, which may have come out of the cache itself.
        let cached = cached_pages();
        let guard = stack.guard();
        drop(stack);
        // Counted as free either way.
        assert_eq!(kva_allocated(), kva);
        if cached_pages() > cached {
            // Kept, still mapped below its guard page; the next stack is
            // that one.
            assert!(crate::mm::translate(guard + 4096).is_some());
            assert!(crate::mm::translate(guard).is_none());
            let again = KernelStack::new(pages).expect("stack");
            assert_eq!(again.guard(), guard);
            assert_eq!(arch::fixup::probe_read(again.guard().get()), Err(arch::fixup::Fault));
        } else {
            // The cache was full: freed outright.
            assert!(crate::mm::translate(guard + 4096).is_none());
        }
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
    fn shared_page_clock_is_monotonic_and_agrees() {
        use crate::shared_page::{realtime, to_ns, uptime, wind};
        let mut last = uptime();
        for i in 0..20_000 {
            if i % 1000 == 0 {
                wind();
            }
            let t = uptime();
            assert!((t.sec, t.frac) >= (last.sec, last.frac), "{t:?} after {last:?}");
            last = t;
        }
        // The kernel's own clock and the timehands agree to well under a
        // microsecond plus the read gap; the closest of a few tries, as an
        // interrupt (or, under emulation, a slow vCPU) can come between
        // the two reads.
        let (a, b) = (0..16)
            .map(|_| (to_ns(uptime()), crate::time::now_ns()))
            .min_by_key(|&(a, b)| a.abs_diff(b))
            .unwrap_or_default();
        assert!(a.abs_diff(b) < 100_000, "timehands {a} ns, clock {b} ns");
        // The time of day moves with the uptime: each sample bounds the
        // boot time between two uptime reads, and the bounds overlap.
        let boot = || {
            let (u1, r, u2) = (to_ns(uptime()), to_ns(realtime()), to_ns(uptime()));
            (r - u2, r - u1)
        };
        let (lo1, hi1) = boot();
        wind();
        let (lo2, hi2) = boot();
        // Rounding to nanoseconds may cost one each way.
        assert!(lo2 <= hi1 + 2 && lo1 <= hi2 + 2, "boot time moved: {lo1}..{hi1} then {lo2}..{hi2}");
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
    fn console_input_through_the_uart_interrupt() {
        // A byte looped back through the console UART raises its receive
        // interrupt: through the IRQ domain to the `uart` driver's filter,
        // which takes it off the port, and its interrupt thread, which
        // hands it to the console's terminal.
        let s = cx();
        let tty = crate::tty::console().expect("the console's uart took its interrupt");
        let before = tty.received();
        crate::console::loop_back(b'Z');
        let t0 = crate::time::now_ns();
        while tty.received() < before + 2 && crate::time::now_ns() - t0 < 500_000_000 {
            crate::time::sleep_ns(&s, 1_000_000);
        }
        assert!(tty.received() >= before + 2, "no input from the UART");
        tty.flush_input(&s);
    }
}

ktest! {
    fn irq_remove_keeps_a_held_level_line_masked() {
        // Removing a level line's last handler while its thread is still
        // running: the thread finishing must not unmask the line (an
        // input still held would then storm with no handler).
        use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
        static MASKS: AtomicU32 = AtomicU32::new(0);
        static UNMASKS: AtomicU32 = AtomicU32::new(0);
        static DISCONNECTED: AtomicBool = AtomicBool::new(false);
        static IN_HANDLER: AtomicBool = AtomicBool::new(false);
        static RELEASE: AtomicBool = AtomicBool::new(false);
        struct Fake;
        impl ::irq::IrqDomain for Fake {
            fn name(&self) -> &'static str {
                "ktest-fake"
            }
            fn connect(
                &self,
                _: u32,
                _: ::irq::Trigger,
                _: ::irq::Polarity,
                cpu: hal::CpuId,
            ) -> Result<u32, ::irq::IrqError> {
                // A root no real input arrives at.
                #[cfg(target_arch = "x86_64")]
                return ::irq::alloc_vector(cpu).ok_or(::irq::IrqError::NoVector);
                #[cfg(target_arch = "aarch64")]
                {
                    let _ = cpu;
                    Ok(287)
                }
            }
            fn disconnect(&self, _: u32, cpu: hal::CpuId, root: u32) {
                #[cfg(target_arch = "x86_64")]
                ::irq::free_vector(cpu, root);
                let _ = (cpu, root);
                DISCONNECTED.store(true, SeqCst);
            }
            fn mask(&self, _: u32) {
                MASKS.fetch_add(1, SeqCst);
            }
            fn unmask(&self, _: u32) {
                UNMASKS.fetch_add(1, SeqCst);
            }
        }
        static FAKE: Fake = Fake;
        fn thread(_: usize, _: &sched::Sleepable) {
            IN_HANDLER.store(true, SeqCst);
            while !RELEASE.load(SeqCst) {
                sched::yield_now();
            }
        }
        let s = cx();
        let h = ::irq::install(
            &FAKE,
            0,
            ::irq::Trigger::Level,
            ::irq::Polarity::High,
            hal::CpuId::BOOT,
            ::irq::Handler { name: "ktest-fake", filter: None, thread: Some(thread), arg: 0 },
        )
        .expect("install");
        let root = h.root();
        let unmasks = UNMASKS.load(SeqCst);
        // One interrupt, dispatched on the line's CPU: it masks the line
        // and wakes the thread.
        let fire = sched::spawn_on("ktest-fire", sched::Priority::KERNEL, Some(hal::CpuId::BOOT), move |_| {
            let irq = hal::irq::disable();
            // SAFETY: interrupts are masked for as long as the token is
            // used, as in the dispatcher.
            let cx = unsafe { sched::IrqCx::assume() };
            assert!(::irq::dispatch(root, &cx), "fake interrupt dispatched");
            hal::irq::restore(irq);
        })
        .expect("spawn");
        fire.join(&s);
        while !IN_HANDLER.load(SeqCst) {
            crate::time::sleep_ns(&s, 1_000_000);
        }
        let masks = MASKS.load(SeqCst);
        // Remove from another thread; once it has masked the line, let the
        // handler finish.
        let remover = sched::spawn("ktest-remove", sched::Priority::KERNEL, move |s| h.remove(s))
            .expect("spawn");
        while MASKS.load(SeqCst) == masks {
            crate::time::sleep_ns(&s, 1_000_000);
        }
        RELEASE.store(true, SeqCst);
        remover.join(&s);
        assert!(DISCONNECTED.load(SeqCst), "line disconnected");
        assert_eq!(UNMASKS.load(SeqCst), unmasks, "the line was unmasked after remove masked it");
    }
}

ktest! {
    fn irq_install_leaves_a_held_shared_line_masked() {
        // A second handler joining a level line while the first one's
        // thread holds it masked: the line stays masked until that thread
        // is done (unmasked early, a held input storms unmasked).
        use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
        static UNMASKS: AtomicU32 = AtomicU32::new(0);
        static IN_HANDLER: AtomicBool = AtomicBool::new(false);
        static RELEASE: AtomicBool = AtomicBool::new(false);
        struct Fake;
        impl ::irq::IrqDomain for Fake {
            fn name(&self) -> &'static str {
                "ktest-fake-shared"
            }
            fn connect(
                &self,
                _: u32,
                _: ::irq::Trigger,
                _: ::irq::Polarity,
                cpu: hal::CpuId,
            ) -> Result<u32, ::irq::IrqError> {
                // A root no real input arrives at.
                #[cfg(target_arch = "x86_64")]
                return ::irq::alloc_vector(cpu).ok_or(::irq::IrqError::NoVector);
                #[cfg(target_arch = "aarch64")]
                {
                    let _ = cpu;
                    Ok(286)
                }
            }
            fn disconnect(&self, _: u32, cpu: hal::CpuId, root: u32) {
                #[cfg(target_arch = "x86_64")]
                ::irq::free_vector(cpu, root);
                let _ = (cpu, root);
            }
            fn mask(&self, _: u32) {}
            fn unmask(&self, _: u32) {
                UNMASKS.fetch_add(1, SeqCst);
            }
        }
        static FAKE: Fake = Fake;
        fn held(_: usize, _: &sched::Sleepable) {
            IN_HANDLER.store(true, SeqCst);
            while !RELEASE.load(SeqCst) {
                sched::yield_now();
            }
        }
        fn other(_: usize, _: &sched::Sleepable) {}
        let s = cx();
        let handler = |name, thread| ::irq::Handler { name, filter: None, thread: Some(thread), arg: 0 };
        let install = |h| {
            ::irq::install(&FAKE, 0, ::irq::Trigger::Level, ::irq::Polarity::High, hal::CpuId::BOOT, h)
                .expect("install")
        };
        let first = install(handler("ktest-held", held as ::irq::ThreadFn));
        let root = first.root();
        let fire = sched::spawn_on("ktest-fire", sched::Priority::KERNEL, Some(hal::CpuId::BOOT), move |_| {
            let irq = hal::irq::disable();
            // SAFETY: interrupts are masked for as long as the token is
            // used, as in the dispatcher.
            let cx = unsafe { sched::IrqCx::assume() };
            assert!(::irq::dispatch(root, &cx), "fake interrupt dispatched");
            hal::irq::restore(irq);
        })
        .expect("spawn");
        fire.join(&s);
        while !IN_HANDLER.load(SeqCst) {
            crate::time::sleep_ns(&s, 1_000_000);
        }
        let unmasks = UNMASKS.load(SeqCst);
        let second = install(handler("ktest-other", other as ::irq::ThreadFn));
        assert_eq!(UNMASKS.load(SeqCst), unmasks, "install unmasked a line a thread holds");
        RELEASE.store(true, SeqCst);
        let t0 = crate::time::now_ns();
        while UNMASKS.load(SeqCst) == unmasks && crate::time::now_ns() - t0 < 1_000_000_000 {
            crate::time::sleep_ns(&s, 1_000_000);
        }
        assert_eq!(UNMASKS.load(SeqCst), unmasks + 1, "unmasked once the thread was done");
        second.remove(&s);
        first.remove(&s);
    }
}

ktest! {
    fn dma_direct_coherent_and_bounced() {
        use vm::dma::{dir, Backend, DmaBuf, DmaConstraints, DmaMemory, DmaTag};
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
        let out = tag.map::<dir::ToDevice, _>(alloc::vec![5u8; 10_000]).map_err(|(_, e)| e).expect("map");
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
        impl Page {
            fn bytes(&self) -> &[u8] {
                // SAFETY: a page we own, through the direct map.
                unsafe { core::slice::from_raw_parts(self.range(0).0, 4096) }
            }
            fn bytes_mut(&mut self) -> &mut [u8] {
                // SAFETY: as above, exclusively.
                unsafe { core::slice::from_raw_parts_mut(self.range(0).0, 4096) }
            }
        }
        // SAFETY: a page we own, through the direct map.
        unsafe impl DmaBuf for Page {
            fn range(&self, _: usize) -> (*mut u8, usize) {
                let va = bootproto::layout::dmap(self.0).unwrap().get() as *mut u8;
                (va, 4096)
            }
        }
        let mut page = Page(high);
        page.bytes_mut().fill(0xa5);
        let out = low.map::<dir::ToDevice, _>(page).map_err(|(_, e)| e).expect("bounce map");
        assert!(out.bounced());
        let g = out.segments()[0];
        assert!(g.bus + g.len <= limit);
        // SAFETY: the bounce page the device would read.
        assert_eq!(unsafe { mem.virt(g.bus).as_ptr().add(4095).read() }, 0xa5);
        let page = out.unmap();
        let inn = low.map::<dir::FromDevice, _>(page).map_err(|(_, e)| e).expect("bounce map");
        let g = inn.segments()[0];
        // SAFETY: the device writing its bounce page.
        unsafe { mem.virt(g.bus).as_ptr().write_bytes(0x3c, 4096) };
        let page = inn.unmap();
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
                    let ident @ Ident::Acpi(..) = m.ident(c, &s)? else {
                        return None;
                    };
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
            // Each attached namespace child named and described as FreeBSD
            // names it (uart0, attimer0, hpet0, atrtc0, pcib0, ...).
            let full: alloc::vec::Vec<&str> =
                include_str!("../../tests/corpus/freebsd/bhyve/devinfo-rv.txt")
                    .lines()
                    .map(str::trim_start)
                    .collect();
            let mut named = 0;
            for &c in &children {
                let Some(ident @ Ident::Acpi(..)) = m.ident(c, &s) else { continue };
                if m.state(c, &s) != Some(bus::NodeState::Attached) {
                    continue;
                }
                let mut l = alloc::format!(
                    "{} <{}> pnpinfo ",
                    m.nameunit(c, &s).unwrap_or_default(),
                    m.description(c, &s).unwrap_or_default()
                );
                ident.pnpinfo(&mut l);
                l.push_str(" at ");
                ident.location(&mut l);
                assert!(full.contains(&l.as_str()), "{l:?} not in FreeBSD's recording");
                named += 1;
            }
            assert!(named >= 8, "{named} attached ACPI children");
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
        // The entropy harvester reads the device too, now and then, and
        // its interrupts may land in between.
        assert!((1..=READS + 2).contains(&taken), "{taken} interrupts");
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
    /// The buffer cache over the scratch disk: delayed writes reach the
    /// disk only when synced, reads come from the cache, and a whole-block
    /// overwrite reads nothing first.
    fn buffer_cache_over_a_disk() {
        use bufcache::{Fill, Limits, WriteMode};
        let s = cx();
        let disk = storage::find("vtbd1").expect("the scratch disk, vtbd1");
        let cache = crate::fs::block::cache(alloc::sync::Arc::clone(&disk), Limits::default());
        let base = 8 << 20;
        cache
            .write(&s, base, 16384, Fill::Overwrite, WriteMode::Delayed, |b| b.fill(0x5a))
            .unwrap();
        cache
            .write(&s, base + 16384, 4096, Fill::Read, WriteMode::Delayed, |b| b[7] = 7)
            .unwrap();
        assert_ne!(disk.read(&s, base, 512).unwrap()[0], 0x5a, "not written yet");
        assert_eq!(cache.read(&s, base, 16384, |b| b[16383]), Ok(0x5a));
        let before = crate::time::now_ns();
        // Not old enough for the syncer, then old enough.
        cache.write_aged(&s, 0).unwrap();
        assert_eq!(cache.usage().1, 20480);
        cache.write_aged(&s, before).unwrap();
        assert_eq!(cache.usage().1, 0);
        assert!(disk.read(&s, base, 16384).unwrap().iter().all(|&b| b == 0x5a));
        assert_eq!(disk.read(&s, base + 16384, 512).unwrap()[7], 7);
        cache
            .write(&s, base, 512, Fill::Read, WriteMode::Now, |b| b[0] = 1)
            .unwrap();
        assert_eq!(disk.read(&s, base, 512).unwrap()[0], 1);
        cache.sync(&s).unwrap();
        cache.invalidate();
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

ktest! {
    fn real_time_clocks_agree() {
        let clocks = clock::read_all();
        for (name, t) in &clocks {
            kprintln!("{name}: {t:?}");
        }
        assert!(!clocks.is_empty(), "a real-time clock");
        let now = crate::time::realtime_ns().expect("the time of day was set") / 1_000_000_000;
        // After this was written, and before 2100.
        assert!((1_790_000_000..4_102_444_800).contains(&now), "{now}");
        // Every clock that answers agrees with it within a few seconds.
        for (name, t) in clocks {
            let Some(t) = t else { continue };
            let secs = t.unix_seconds().expect("valid");
            assert!(secs.abs_diff(now) <= 3, "{name} is {secs}, the kernel {now}");
        }
    }
}

#[cfg(target_arch = "x86_64")]
ktest! {
    fn ichwd_counts_down_and_is_patted() {
        let s = cx();
        let m = bus::manager();
        let nexus = crate::devices::nexus();
        let Some((isa, dog)) = find_named(m, &s, nexus, "ichwd0") else {
            // bhyve's LPC bridge (a PIIX3) has no TCO watchdog.
            assert!(find_named(m, &s, nexus, "isa0").is_some(), "isa0");
            kprintln!("(no ICH watchdog on this chipset)");
            return;
        };
        assert_eq!(m.nameunit(isa, &s).as_deref(), Some("isa0"));
        assert_eq!(m.description(dog, &s).as_deref(), Some("Intel ICH9 watchdog timer"));
        let w = watchdog::first().expect("a registered watchdog");
        assert_eq!(w.arm(60_000), Ok(60_000));
        let first = w.remaining_ms().expect("counting");
        crate::time::sleep_ns(&s, 1_500_000_000);
        let later = w.remaining_ms().expect("counting");
        assert!(later < first, "{later} ms left after {first}: not counting down");
        w.pat();
        let patted = w.remaining_ms().expect("counting");
        assert!(patted > later, "a pat restarts the count ({patted} vs {later})");
        w.disarm();
        assert_eq!(w.remaining_ms(), None, "stopped");
        kprintln!("{}: {first} ms, then {later} ms, patted to {patted} ms", w.name());
    }
}

/// Detaches and re-attaches `node` (a disk's controller) `cycles` times
/// while four threads do random I/O on `disk`, checking after every cycle
/// that grants balanced (the manager asserts it), nothing was dropped, and
/// memory came back.
fn detach_stress(node_name: &str, disk: &'static str, cycles: u32) {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicU64};
    let s = cx();
    let m = bus::manager();
    let (_, node) = find_named(m, &s, crate::devices::nexus(), node_name)
        .unwrap_or_else(|| panic!("{node_name}"));
    let stop = Arc::new(AtomicBool::new(false));
    let (ops, gone) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let workers: alloc::vec::Vec<_> = (0..4u64)
        .map(|t| {
            let (stop, ops, gone) = (Arc::clone(&stop), Arc::clone(&ops), Arc::clone(&gone));
            sched::spawn("detach-io", sched::Priority::KERNEL, move |s| {
                let mut seed = crate::time::counter() ^ ((t + 1) * 0x9e37_79b9);
                while !stop.load(Ordering::Acquire) {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let Some(d) = storage::find(disk) else {
                        crate::time::sleep_ns(s, 1_000_000);
                        continue;
                    };
                    // Each thread keeps to its own quarter of the disk.
                    let quarter = d.bytes() / 4 / 4096;
                    let at = (t * quarter + seed % quarter) * 4096;
                    let data = alloc::vec![(seed >> 8).to_le_bytes()[0]; 4096];
                    let w = d.write(s, at, &data);
                    let r = d.read(s, at, 4096);
                    for res in [w.map(|()| alloc::vec::Vec::new()), r.clone()] {
                        match res {
                            Ok(_) => {}
                            Err(storage::BioError::Gone) => {
                                gone.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => panic!("{disk}: {e:?} at {at:#x}"),
                        }
                    }
                    // Both succeeded: the data must have stuck, across a
                    // detach if one came between them.
                    if let (Ok(()), Ok(back)) = (w, r) {
                        assert_eq!(back, data, "{disk} at {at:#x}");
                    }
                    ops.fetch_add(1, Ordering::Relaxed);
                }
            })
            .unwrap()
        })
        .collect();
    let free_pages = || crate::mm::phys_stats().free;
    let mut baseline = None;
    let start = crate::time::counter();
    for cycle in 0..cycles {
        crate::time::sleep_ns(&s, 30_000_000);
        m.detach(node, bus::DetachReason::Requested, &s)
            .expect("detach");
        assert!(storage::find(disk).is_none(), "{disk} withdrawn");
        m.probe_and_attach(node, &s).expect("attach");
        m.wait_idle(&s);
        assert_eq!(
            m.state(node, &s),
            Some(bus::NodeState::Attached),
            "cycle {cycle}"
        );
        assert!(
            storage::find(disk).is_some(),
            "{disk} back after cycle {cycle}"
        );
        // The first cycle warms caches; after it, memory must not drift.
        let free = free_pages();
        match baseline {
            None => baseline = Some(free),
            Some(b) => assert!(
                free + 64 >= b,
                "cycle {cycle}: {} pages fewer free than after the first",
                b - free
            ),
        }
    }
    stop.store(true, Ordering::Release);
    for w in workers {
        w.join(&s);
    }
    let ms = (crate::time::counter() - start) * 1000 / crate::time::frequency();
    assert_eq!(storage::bio::dropped(), 0, "no request was dropped");
    assert!(ops.load(Ordering::Relaxed) > 0);
    kprintln!(
        "{node_name}: {cycles} detach cycles in {ms} ms under I/O ({} ops, {} refused while gone)",
        ops.load(Ordering::Relaxed),
        gone.load(Ordering::Relaxed)
    );
    let _ = m.take_events();
}

ktest! {
    fn detach_stress_vtblk() {
        let s = cx();
        let m = bus::manager();
        // vtbd1's transport.
        let (transport, _) = find_named(m, &s, crate::devices::nexus(), "vtblk1").expect("vtblk1");
        let name = m.nameunit(transport, &s).unwrap();
        detach_stress(alloc::string::String::leak(name), "vtbd1", 8);
    }
}

ktest! {
    fn detach_stress_nvme() {
        detach_stress("nvme0", "nda0", 8);
    }
}

/// Address spaces (P4.1): user memory reached through the copy routines,
/// faulted in on demand, copy-on-write across `fork`, and torn down with
/// every page accounted for.
mod user_memory {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use addr::VirtAddr;
    use ktest::ktest;
    use vm::map::Prot;

    use super::cx;
    use crate::mm::space::{self, AddressSpace};
    use crate::uaccess::{
        Fault, StrError, UserPtr, copy_in, copy_in_str, copy_in_str_vec, copy_out,
    };

    const PAGE: u64 = 4096;
    const HINT: VirtAddr = VirtAddr::new(0x1000_0000);

    /// Pages that are free or held by the heap (whose caches keep pages
    /// for the map's nodes) or by cached kernel stacks: a page leaked by
    /// the VM drops this.
    fn free_pages() -> u64 {
        let h = crate::mm::heap_stats();
        crate::mm::phys_stats().free
            + h.slab_pages
            + h.magazine_pages
            + h.large_pages
            + crate::mm::kstack::cached_pages()
    }

    fn enter(space: &Arc<AddressSpace>) {
        sched::set_address_space(Some(Arc::clone(space) as Arc<dyn sched::AddressSpace>));
    }

    fn leave() {
        sched::set_address_space(None);
        space::unload();
    }

    fn read_u64(at: u64) -> Result<u64, Fault> {
        UserPtr::<u64>::new(at).read()
    }

    ktest! {
        fn user_memory_faults_in_through_copies() {
            let s = cx();
            let before = free_pages();
            let space = AddressSpace::new().unwrap();
            enter(&space);
            let a = space.map_anonymous(&s, None, HINT, 16 * PAGE, Prot::RW).unwrap().get();
            assert_eq!(a, HINT.get());
            assert!(space.translate(VirtAddr::new(a)).is_none());
            // Untouched memory reads as zeros; writes stick.
            assert_eq!(read_u64(a + 8), Ok(0));
            copy_out(a + PAGE - 2, b"spans").unwrap();
            let mut buf = [0u8; 5];
            copy_in(&mut buf, a + PAGE - 2).unwrap();
            assert_eq!(&buf, b"spans");
            UserPtr::<u64>::new(a + 3 * PAGE).write(&0x1234_5678).unwrap();
            assert_eq!(read_u64(a + 3 * PAGE), Ok(0x1234_5678));
            assert!(space.translate(VirtAddr::new(a + 3 * PAGE)).is_some());
            // Beyond the mapping, and outside the user half.
            assert_eq!(read_u64(a + 16 * PAGE), Err(Fault));
            assert_eq!(copy_in(&mut buf, a + 16 * PAGE - 2), Err(Fault));
            assert_eq!(copy_in(&mut buf, space::USER_END - 2), Err(Fault));
            assert_eq!(copy_in(&mut buf, u64::MAX - 1), Err(Fault));
            assert_eq!(read_u64(0), Err(Fault));
            // A string that ends at the mapping's last byte.
            let end = a + 16 * PAGE;
            copy_out(end - 4, b"abc\0").unwrap();
            let mut name = [0u8; 64];
            assert_eq!(copy_in_str(end - 4, &mut name), Ok(3));
            assert_eq!(&name[..3], b"abc");
            // Unterminated up to the mapping's end: the next page faults.
            copy_out(end - 2, b"xy").unwrap();
            assert_eq!(copy_in_str(end - 2, &mut name), Err(StrError::Fault));
            copy_out(a + 8, b"abcdefgh").unwrap();
            assert_eq!(copy_in_str(a + 8, &mut name[..4]), Err(StrError::TooLong));
            // The same, into a vector, in pieces: across a page, too.
            copy_out(end - 2, b"xy").unwrap();
            assert_eq!(copy_in_str_vec(end - 2, 64), Err(StrError::Fault));
            copy_out(end - 4, b"abc\0").unwrap();
            assert_eq!(copy_in_str_vec(end - 4, 64).as_deref(), Ok(&b"abc"[..]));
            assert_eq!(copy_in_str_vec(a + 8, 4), Err(StrError::TooLong));
            let long: Vec<u8> = (b'a'..=b'z').cycle().take(300).collect();
            let at = a + 2 * PAGE - 200;
            copy_out(at, &long).unwrap();
            copy_out(at + 300, b"\0").unwrap();
            assert_eq!(copy_in_str_vec(at, 1024).as_deref(), Ok(&long[..]));
            assert_eq!(copy_in_str_vec(at, 300), Err(StrError::TooLong));
            // Read-only: writes fault, reads still work.
            space.protect(&s, VirtAddr::new(a)..VirtAddr::new(a + 4 * PAGE), Prot::READ).unwrap();
            assert_eq!(copy_out(a + 3 * PAGE, b"x"), Err(Fault));
            assert_eq!(read_u64(a + 3 * PAGE), Ok(0x1234_5678));
            // Unmapped: gone from the tables too.
            space.unmap(&s, VirtAddr::new(a + 3 * PAGE)..VirtAddr::new(a + 4 * PAGE)).unwrap();
            assert_eq!(read_u64(a + 3 * PAGE), Err(Fault));
            assert!(space.translate(VirtAddr::new(a + 3 * PAGE)).is_none());
            leave();
            drop(space);
            assert_eq!(free_pages(), before, "pages leaked");
        }
    }

    ktest! {
        fn fork_copies_on_write_between_spaces() {
            let s = cx();
            let before = free_pages();
            let parent = AddressSpace::new().unwrap();
            enter(&parent);
            let a = parent.map_anonymous(&s, None, HINT, 4 * PAGE, Prot::RW).unwrap().get();
            for i in 0..4 {
                UserPtr::<u64>::new(a + i * PAGE).write(&(100 + i)).unwrap();
            }
            let child = parent.fork(&s).unwrap();
            let shared = parent.translate(VirtAddr::new(a));

            enter(&child);
            assert_eq!(read_u64(a), Ok(100));
            assert_eq!(child.translate(VirtAddr::new(a)), shared);
            UserPtr::<u64>::new(a).write(&200).unwrap();
            assert_ne!(child.translate(VirtAddr::new(a)), shared);

            enter(&parent);
            assert_eq!(read_u64(a), Ok(100));
            UserPtr::<u64>::new(a + PAGE).write(&300).unwrap();

            enter(&child);
            assert_eq!(read_u64(a), Ok(200));
            assert_eq!(read_u64(a + PAGE), Ok(101));
            assert_eq!(read_u64(a + 3 * PAGE), Ok(103));

            leave();
            drop(child);
            enter(&parent);
            assert_eq!(read_u64(a + PAGE), Ok(300));
            leave();
            drop(parent);
            assert_eq!(free_pages(), before, "pages leaked");
        }
    }

    /// Runs `code` as a process: its own read+execute page, a stack, and
    /// one thread bound to this CPU. Returns the exit status once every
    /// page is back.
    /// [`free_pages`] once it has stopped changing: the reaper frees
    /// earlier tests' threads in its own time.
    fn settled_free_pages(s: &sched::Sleepable) -> u64 {
        // A CPU keeps the last address space it ran until it loads
        // another; earlier tests' spaces would otherwise go mid-count.
        let ncpu = u32::try_from(sched::online_cpus()).unwrap();
        let workers: Vec<_> = (0..ncpu)
            .map(|cpu| {
                sched::Builder::new("unload")
                    .bind(hal::CpuId(cpu))
                    .spawn(|_| crate::mm::space::unload())
                    .unwrap()
            })
            .collect();
        for w in workers {
            w.join(s);
        }
        let mut last = free_pages();
        loop {
            crate::time::sleep_ns(s, 10_000_000);
            let now = free_pages();
            if now == last {
                return now;
            }
            last = now;
        }
    }

    fn run_program(code: &[u8]) -> i32 {
        use crate::proc::{Process, STACK_TOP};
        let s = cx();
        let before = settled_free_pages(&s);
        let space = AddressSpace::new().unwrap();
        let text = 0x40_0000;
        assert!(code.len() as u64 <= PAGE);
        space
            .map_anonymous(&s, Some(VirtAddr::new(text)), HINT, PAGE, Prot::RW)
            .unwrap();
        enter(&space);
        copy_out(text, code).unwrap();
        leave();
        space
            .protect(
                &s,
                VirtAddr::new(text)..VirtAddr::new(text + PAGE),
                Prot::READ.union(Prot::EXEC),
            )
            .unwrap();
        space
            .map_anonymous(
                &s,
                Some(VirtAddr::new(STACK_TOP - 4 * PAGE)),
                HINT,
                4 * PAGE,
                Prot::RW,
            )
            .unwrap();
        // Bound, so only that CPU ever loads the space (and it lets go at
        // exit).
        let p = Process::spawn_code(space, text, STACK_TOP, Some(hal::cpu_id()));
        let status = match p.wait(&s) {
            crate::proc::Exit::Code(c) => c,
            crate::proc::Exit::Signal(sig) => panic!("killed by signal {sig}"),
        };
        drop(p);
        let after = settled_free_pages(&s);
        assert_eq!(after, before, "pages leaked");
        status
    }

    ktest! {
        fn shared_page_is_mapped_read_only() {
            let s = cx();
            let space = AddressSpace::new().unwrap();
            let page = 0x7000_0000_0000;
            crate::shared_page::map(&s, &space, page).unwrap();
            let tk = crate::shared_page::timekeep_address(page);
            assert!((page..page + PAGE).contains(&tk));
            enter(&space);
            // tk_ver 1, tk_enabled 1.
            assert_eq!(read_u64(tk), Ok(1 | 1 << 32));
            assert!(copy_out(tk, &[0]).is_err(), "user writes are refused");
            leave();
            let range = VirtAddr::new(page)..VirtAddr::new(page + PAGE);
            assert!(space.protect(&s, range, Prot::RW).is_err(), "write is beyond max_prot");
            space.clear(&s);
        }
    }

    ktest! {
        /// A bell in a window's output goes to devctl as `vtterm_devctl`
        /// reports it, with the window's pitch: the console window's is
        /// 800 Hz for 50 ms, the others' none until `ESC [ = B`. Off by
        /// default (`kern.vt.enable_bell`), it doesn't sound.
        fn vt_bells_are_reported_to_devctl() {
            let s = cx();
            let bell = crate::sysctl::read(&s, |t| {
                let oid = t.name2oid("kern.vt.enable_bell").expect("kern.vt.enable_bell");
                let mut v = alloc::vec::Vec::new();
                t.read(&oid, &mut v).expect("read");
                v
            });
            assert_eq!(bell, 0i32.to_ne_bytes());
            if vt::size(1).is_none() {
                return;
            }
            let m = bus::manager();
            let _ = m.take_events();
            vt::write(1, b"\x07");
            vt::write(0, b"\x07");
            vt::write(1, b"no bell");
            let events = m.take_events();
            assert_eq!(
                events,
                [
                    "!system=VT subsystem=BELL type=RING enabled=false hushed=false hz=0 duration_ms=0\n",
                    "!system=VT subsystem=BELL type=RING enabled=false hushed=false hz=800 duration_ms=50\n",
                ]
            );
        }
    }

    ktest! {
        /// Runs each program `tests/user/*.c` built into the memory disk;
        /// each checks something from user mode and exits 0 when it holds.
        fn user_test_programs_pass() {
            use alloc::vec::Vec;
            const DIR: &[u8] = b"usr/tests/abyss/";
            let s = cx();
            let archive = crate::md::archive().expect("no memory disk: run `cargo xtask fetch-base`");
            let mut ran = 0;
            for e in archive.entries() {
                let e = e.unwrap();
                if e.kind != ustar::Kind::File {
                    continue;
                }
                let mut path = Vec::from(&b"/"[..]);
                e.path_parts(|p| path.extend_from_slice(p));
                let Some(name) = path[1..].strip_prefix(DIR) else {
                    continue;
                };
                let name = Vec::from(name);
                let p = crate::proc::Process::spawn(&s, &path, &[&name], &[], Some(hal::cpu_id()))
                    .unwrap();
                let status = p.wait(&s);
                assert_eq!(
                    status,
                    crate::proc::Exit::Code(0),
                    "{} ended: {status:?}",
                    core::str::from_utf8(&path).unwrap_or("?")
                );
                ran += 1;
            }
            assert!(ran >= 2, "only {ran} test programs on the memory disk");
        }
    }

    ktest! {
        fn exec_layout_is_randomized_within_bounds() {
            use crate::exec::{Layout, MAXDSIZ, STACK_SIZE, USRSTACK};
            let s = cx();
            let root = crate::fs::root();
            let echo = crate::fs::namespace()
                .lookup(
                    &s,
                    &vfs::namei::Lookup {
                        start: &root,
                        root: &root,
                        follow: true,
                        beneath: false,
                        create: false,
                        cred: &crate::fs::cred(),
                    },
                    b"/rescue/echo",
                )
                .unwrap();
            let load = || {
                let space = AddressSpace::new().unwrap();
                let image =
                    crate::exec::load(&s, &space, &echo, b"/rescue/echo", &[b"echo"], &[], MAXDSIZ)
                        .unwrap();
                // The top page is a guard: mapped, with no access at all.
                let top = VirtAddr::new(USRSTACK);
                assert!(space.fault(&s, top, vm::fault::Access::Read).is_err());
                space.clear(&s);
                image.layout
            };
            let (a, b) = (load(), load());
            assert!(a.anon_loc != b.anon_loc && a.stack_top != b.stack_top, "{a:?} {b:?}");
            for Layout { anon_loc, stack_top, shared_page } in [a, b] {
                // /rescue/echo's data is below 64 MiB; the layout is above
                // the data limit, anon_loc in the lower half of what is left.
                assert!((MAXDSIZ..USRSTACK / 2 + (MAXDSIZ + (64 << 20)) / 2).contains(&anon_loc));
                assert!(anon_loc.is_multiple_of(2 << 20));
                assert!((MAXDSIZ + STACK_SIZE..=USRSTACK).contains(&stack_top));
                assert!(stack_top.is_multiple_of(8));
                assert!((MAXDSIZ..USRSTACK).contains(&shared_page));
            }
        }
    }

    ktest! {
        fn rescue_echo_runs() {
            use crate::proc::Process;
            let s = cx();
            assert!(crate::md::archive().is_some(), "no memory disk: run `cargo xtask fetch-base`");
            let p = Process::spawn(
                &s,
                b"/rescue/echo",
                &[b"echo", b"hello", b"from", b"FreeBSD's", b"echo"],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(0));
        }
    }

    ktest! {
        fn rescue_sh_forks_execs_and_waits() {
            use crate::proc::Process;
            let s = cx();
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", b"/rescue/echo child; echo \"status $?\"; /rescue/sysctl kern.ostype hw.ncpu; exit 3"],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(3));
        }
    }

    ktest! {
        fn rescue_sh_runs_pipelines_over_files() {
            use crate::proc::Process;
            let s = cx();
            // Every step must succeed for the exit status to be 7.
            let script: &[u8] = b"cd /dev && [ \"$(pwd)\" = /dev ] && [ \"$(/rescue/pwd)\" = /dev ] && \
                ls -l null && echo hello | sed s/hello/HELLO/ | cat && \
                [ \"$(ls /rescue | head -1)\" = [ ] && \
                [ \"$(echo a b c | sed 's/ /\\n/g' | tail -1)\" = c ] && \
                [ \"$(md5 -q /dev/null)\" = d41d8cd98f00b204e9800998ecf8427e ] && \
                [ \"$(realpath /dev/../rescue/sh)\" = /rescue/sh ] && \
                cat /rescue/../dev/null && exit 7";
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", script],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(7));
        }
    }

    ktest! {
        /// `/rescue`'s tools build, copy, archive, remove, and restore a
        /// tree on the tmpfs at `/tmp`.
        fn rescue_tools_work_on_tmpfs() {
            use crate::proc::Process;
            let s = cx();
            let script: &[u8] = b"cd /tmp && mkdir -p x/y && echo hello > x/y/f && \
                cp x/y/f x/g && ln -s y/f x/l && [ \"$(cat x/l)\" = hello ] && \
                mv x/g x/h && chmod 600 x/h && set -- $(ls x) && [ $# = 3 ] && \
                case \"$(ls -l x/h)\" in -rw-------*) ;; *) exit 3;; esac && \
                tar -cf t.tar x && rm -r x && [ ! -e x ] && tar -xf t.tar && \
                [ \"$(cat x/y/f)\" = hello ] && case \"$(ls -l x/l)\" in *' -> y/f') ;; *) exit 4;; esac && \
                rm -rf x t.tar && [ ! -e x ] && [ ! -e t.tar ] && \
                mkdir bin && cp /rescue/echo bin/echo && [ \"$(bin/echo from tmpfs)\" = 'from tmpfs' ] && \
                rm -r bin && exit 7";
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", script],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(7));
        }
    }

    ktest! {
        /// FreeBSD's fsx on the tmpfs at `/tmp`, mapped reads and writes
        /// included. Skipped when the memory disk has no fsx (no FreeBSD
        /// tree beside this one to build it from).
        fn fsx_passes_on_tmpfs() {
            use crate::proc::Process;
            let s = cx();
            const FSX: &[u8] = b"/usr/tests/fsx/fsx";
            let root = crate::fs::root();
            let l = vfs::namei::Lookup {
                start: &root,
                root: &root,
                follow: true,
                beneath: false,
                create: false,
                cred: &crate::fs::cred(),
            };
            if crate::fs::namespace().lookup(&s, &l, FSX).is_err() {
                kprintln!("fsx: not on the memory disk; skipped");
                return;
            }
            let p = Process::spawn(
                &s,
                FSX,
                &[b"fsx", b"-q", b"-N", b"20000", b"-S", b"7", b"/tmp/fsx.data"],
                &[],
                Some(hal::cpu_id()),
            )
            .unwrap();
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(0));
            for f in [&b"/tmp/fsx.data"[..], b"/tmp/fsx.data.fsxgood", b"/tmp/fsx.data.fsxlog"] {
                let _ = crate::fs::parent_of(&s, &p, crate::fs::At::Cwd, f, false)
                    .and_then(|d| vfs::ops::remove(&s, &d.dir, &d.name, &crate::fs::cred(), false));
            }
        }
    }

    /// The whole of the file at `path`, if it exists.
    fn read_whole(s: &sched::Sleepable, path: &[u8]) -> Option<alloc::vec::Vec<u8>> {
        let root = crate::fs::root();
        let l = vfs::namei::Lookup {
            start: &root,
            root: &root,
            follow: true,
            beneath: false,
            create: false,
            cred: &crate::fs::cred(),
        };
        let v = crate::fs::namespace().lookup(s, &l, path).ok()?;
        let size = usize::try_from(v.size(s).ok()?).ok()?;
        let mut buf = alloc::vec![0u8; size];
        let mut r = vfs::ReadBuf::new(&mut buf);
        let n = v.read_at(s, 0, &mut r).ok()?;
        buf.truncate(n);
        Some(buf)
    }

    /// The first disk named `prefix` and a number (no partitions) that
    /// `pick` accepts: units may be renumbered once the detach tests have
    /// run, so tests find their disks by what they hold.
    fn find_disk(
        s: &sched::Sleepable,
        prefix: &str,
        mut pick: impl FnMut(&sched::Sleepable, &storage::Disk) -> bool,
    ) -> Option<alloc::sync::Arc<storage::Disk>> {
        storage::disk::all().into_iter().find(|d| {
            d.name()
                .strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
                && pick(s, d)
        })
    }

    /// Whether disk `d` holds a UFS2 file system.
    fn holds_ufs2(s: &sched::Sleepable, d: &storage::Disk) -> bool {
        d.read(s, 65536, 8192)
            .is_ok_and(|b| u32::from_le_bytes([b[1372], b[1373], b[1374], b[1375]]) == 0x1954_0119)
    }

    /// Prints where `got` first differs from `want`, line by line.
    fn first_difference(what: &str, got: &[u8], want: &[u8]) {
        let mut g = got.split(|&c| c == b'\n');
        let mut w = want.split(|&c| c == b'\n');
        for line in 1.. {
            match (g.next(), w.next()) {
                (None, None) => return,
                (a, b) if a == b => {}
                (a, b) => {
                    let show = |x: Option<&[u8]>| {
                        x.map_or_else(
                            || "(end)".into(),
                            |l| alloc::string::String::from_utf8_lossy(l).into_owned(),
                        )
                    };
                    kprintln!("{what}: line {line}: got {:?}", show(a));
                    kprintln!("{what}: line {line}: want {:?}", show(b));
                    return;
                }
            }
        }
    }

    ktest! {
        /// The UFS2 test disk mounts read-only with `/rescue`'s `mount`,
        /// and `ls -lRnT` and `md5 -r` over it print exactly what FreeBSD's
        /// printed over the same image: from virtio-blk (vtbd2), every
        /// file's digest; from NVMe (nda1) and AHCI (ada1), the listing
        /// and the largest file's. Skipped when the disk or FreeBSD's
        /// listings are not there (no `fetch-base` from the server).
        fn ufs_reads_as_freebsd_reads_it() {
            use crate::proc::Process;
            let s = cx();
            let (Some(want_ls), Some(want_md5)) = (
                read_whole(&s, b"/usr/tests/ufs/ufs-test.ls"),
                read_whole(&s, b"/usr/tests/ufs/ufs-test.md5"),
            ) else {
                kprintln!("ufs: no test disk listings; skipped");
                return;
            };
            let rescue_md5: alloc::vec::Vec<u8> = want_md5
                .split(|&c| c == b'\n')
                .find(|l| l.ends_with(b" ./rescue/rescue"))
                .map(|l| [l, b"\n"].concat())
                .unwrap_or_default();
            // The UFS2 test disk: on each controller, the one holding UFS2
            // that is not a 16 MiB scratch disk.
            for (prefix, all) in [("vtbd", true), ("nda", false), ("ada", false)] {
                let Some(d) = find_disk(&s, prefix, |s, d| d.bytes() != 16 << 20 && holds_ufs2(s, d)) else {
                    kprintln!("ufs: no UFS2 test disk on {prefix}; skipped");
                    continue;
                };
                let disk = d.name();
                let files: &[u8] = if all {
                    b"$(cat /usr/tests/ufs/ufs-test.files)"
                } else {
                    b"./rescue/rescue"
                };
                let script = [
                    &b"mount -t ufs -o ro /dev/"[..],
                    disk.as_bytes(),
                    b" /mnt || exit 1; cd /mnt && TZ=UTC ls -lRnT . > /tmp/ufs.ls || exit 2; md5 -r ",
                    files,
                    b" > /tmp/ufs.md5 || exit 3; touch /mnt/x 2>/dev/null && exit 4; \
                      cd / && umount /mnt || exit 5; exit 7",
                ]
                .concat();
                let p = Process::spawn(
                    &s,
                    b"/rescue/sh",
                    &[b"sh", b"-c", &script],
                    &[b"PATH=/rescue", b"HOME=/"],
                    Some(hal::cpu_id()),
                )
                .unwrap();
                let exit = p.wait(&s);
                let got_ls = read_whole(&s, b"/tmp/ufs.ls").unwrap_or_default();
                let got_md5 = read_whole(&s, b"/tmp/ufs.md5").unwrap_or_default();
                let want_md5 = if all { &want_md5 } else { &rescue_md5 };
                first_difference(&alloc::format!("ufs: {disk}: ls -lRnT"), &got_ls, &want_ls);
                first_difference(&alloc::format!("ufs: {disk}: md5 -r"), &got_md5, want_md5);
                assert_eq!(exit, crate::proc::Exit::Code(7), "{disk}");
                assert!(got_ls == want_ls && got_md5 == *want_md5, "{disk}");
                let lines = want_md5.iter().filter(|&&c| c == b'\n').count();
                kprintln!("ufs: {disk}: the listing and {lines} digests match FreeBSD's");
                for f in [&b"/tmp/ufs.ls"[..], b"/tmp/ufs.md5"] {
                    let _ = crate::fs::parent_of(&s, &p, crate::fs::At::Cwd, f, false).and_then(
                        |d| vfs::ops::remove(&s, &d.dir, &d.name, &crate::fs::cred(), false),
                    );
                }
            }
        }
    }

    ktest! {
        /// UFS2 read-write on its own disk (32 MiB, virtio): FreeBSD's `newfs`
        /// makes it, `/rescue`'s tools work over it mounted read-write
        /// (directories, copies, links, renames, an archive, removal, and
        /// fsx when the memory disk has it), and after unmounting,
        /// FreeBSD's `fsck_ffs -f -n` finds nothing wrong; mounted again,
        /// it holds what was written.
        fn ufs_read_write_survives_fsck() {
            ufs_read_write(false);
        }
    }

    ktest! {
        /// The same with soft updates (`newfs -U`): the mount says
        /// "soft-updates", space freed comes back, and `fsck_ffs -f -n`
        /// finds nothing wrong after unmounting.
        fn ufs_soft_updates_survive_fsck() {
            ufs_read_write(true);
        }
    }

    /// The read-write workload on the 32 MiB test disk, made by FreeBSD's
    /// `newfs` (with soft updates if `soft`).
    fn ufs_read_write(soft: bool) {
        use crate::proc::Process;
        let s = cx();
        // Its own 32 MiB virtio disk, whatever its unit now.
        let Some(scratch) = find_disk(&s, "vtbd", |_, d| d.bytes() == 32 << 20) else {
            kprintln!("ufs: no read-write test disk; skipped");
            return;
        };
        let dev = alloc::format!("/dev/{}", scratch.name());
        let what = if soft { " with soft updates" } else { "" };
        kprintln!("ufs: read-write on {dev}{what}");
        let fsx = read_whole(&s, b"/usr/tests/fsx/fsx").is_some();
        // With soft updates: the flag shows, and a tree removed gives
        // its space back (once the syncer has done the frees).
        let soft_checks = if soft {
            "[ -n \"$(mount | sed -n '/soft-updates/p')\" ] || exit 15; \
                 set -- $(df -k /mnt | tail -1); before=$4; \
                 mkdir sp && dd if=/rescue/rescue of=sp/a bs=64k count=60 2>/dev/null && cp sp/a sp/b && \
                 rm -r sp && sync && sleep 3 && \
                 mkdir many && i=0 && while [ $i -lt 4000 ]; do : > many/f$i; i=$((i+1)); done && \
                 set -- $(ls many) && [ $# -eq 4000 ] && rm -r many || exit 17; \
                 set -- $(df -k /mnt | tail -1); after=$4; \
                 [ $((before - after)) -lt 512 ] || exit 16; "
        } else {
            ""
        };
        let script_text = alloc::string::String::from("newfs NEWFS DEV > /dev/null || exit 1; \
                mount -t ufs DEV /mnt || exit 2; cd /mnt || exit 2; SOFT \
                mkdir -p a/b c && echo hello > a/b/f && dd if=/rescue/rescue of=big bs=64k count=80 2>/dev/null && \
                ln a/b/f hard && ln -s a/b/f soft && mv big c/big && \
                [ \"$(cat soft)\" = hello ] && [ \"$(cat hard)\" = hello ] || exit 3; \
                [ \"$(md5 -q c/big)\" = \"$(dd if=/rescue/rescue bs=64k count=80 2>/dev/null | md5 -q)\" ] || exit 4; \
                tar -cf /tmp/t.tar -C / usr/tests && mkdir x && tar -xf /tmp/t.tar -C x && rm /tmp/t.tar || exit 5; \
                for f in $(ls x/usr/tests/abyss); do \
                    [ \"$(md5 -q x/usr/tests/abyss/$f)\" = \"$(md5 -q /usr/tests/abyss/$f)\" ] || exit 6; done; \
                rm -r a/b x/usr && [ ! -e a/b ] || exit 8; \
                if [ -x /usr/tests/fsx/fsx ]; then for seed in 1 2 3; do \
                    /usr/tests/fsx/fsx -q -N 10000 -S $seed /mnt/fsx.data > /tmp/fsx.out 2>&1 || exit 9; \
                    rm -f /mnt/fsx.data*; done; fi; \
                cd / && umount /mnt || exit 10; \
                fsck_ffs -f -n DEV > /tmp/fsck.out 2>&1 || exit 11; \
                mount -t ufs -o ro DEV /mnt || exit 12; \
                [ \"$(cat /mnt/hard)\" = hello ] && \
                [ \"$(md5 -q /mnt/c/big)\" = \"$(dd if=/rescue/rescue bs=64k count=80 2>/dev/null | md5 -q)\" ] || exit 13; \
                umount /mnt || exit 14; exit 7")
                .replace("SOFT", soft_checks)
                .replace("NEWFS", if soft { "-U" } else { "-u" })
                .replace("DEV", &dev);
        let p = Process::spawn(
            &s,
            b"/rescue/sh",
            &[b"sh", b"-c", script_text.as_bytes()],
            &[b"PATH=/rescue", b"HOME=/"],
            Some(hal::cpu_id()),
        )
        .unwrap();
        let exit = p.wait(&s);
        if exit != crate::proc::Exit::Code(7) {
            for f in [&b"/tmp/fsck.out"[..], b"/tmp/fsx.out"] {
                if let Some(out) = read_whole(&s, f) {
                    kprintln!(
                        "{}:\n{}",
                        core::str::from_utf8(f).unwrap_or("?"),
                        alloc::string::String::from_utf8_lossy(&out)
                    );
                }
            }
        }
        assert_eq!(exit, crate::proc::Exit::Code(7));
        let fsck = read_whole(&s, b"/tmp/fsck.out").unwrap_or_default();
        let text = alloc::string::String::from_utf8_lossy(&fsck);
        let questions: alloc::vec::Vec<&str> = text
            .lines()
            .filter(|l| l.contains('?') && !l.contains("CONTINUE?"))
            .collect();
        assert!(
            questions.is_empty(),
            "fsck_ffs asked: {questions:?}\n{text}"
        );
        kprintln!(
            "ufs: read-write on {dev}{what} is fsck clean{}",
            if fsx { ", fsx included" } else { "" }
        );
        for f in [&b"/tmp/fsck.out"[..], b"/tmp/fsx.out"] {
            let _ = crate::fs::parent_of(&s, &p, crate::fs::At::Cwd, f, false)
                .and_then(|d| vfs::ops::remove(&s, &d.dir, &d.name, &crate::fs::cred(), false));
        }
    }

    ktest! {
        /// The ESP on /boot/efi (msdosfs, read-write): the loader's files
        /// are there; long and mixed-case names, copies, renames across
        /// directories, removal, and fsx work over it; FreeBSD's
        /// `fsck_msdosfs -n` finds it clean after unmounting; and mounted
        /// again with `/rescue`'s `mount_msdosfs`, it holds what was
        /// written.
        fn msdosfs_esp_read_write_survives_fsck() {
            use crate::proc::Process;
            let s = cx();
            let Some(esp) = storage::disk::all()
                .into_iter()
                .find(|d| d.partition().is_some_and(|(_, p)| p.type_guid == gpt::types::EFI_SYSTEM))
            else {
                kprintln!("msdosfs: no ESP; skipped");
                return;
            };
            let dev = alloc::format!("/dev/{}", esp.name());
            let script = alloc::string::String::from(
                "cd /boot/efi || exit 1; [ -d EFI/ABYSS ] && [ -f EFI/ABYSS/KERNEL ] || exit 2; \
                 mkdir -p 'Test Dir/Sub' || exit 3; echo hello > 'Test Dir/A Long File Name.txt' || exit 3; \
                 echo upper > SHORT.TXT && [ \"$(cat short.txt)\" = upper ] || exit 4; \
                 dd if=/rescue/rescue of='Test Dir/big.bin' bs=64k count=40 2>/dev/null || exit 5; \
                 [ \"$(md5 -q 'Test Dir/big.bin')\" = \"$(dd if=/rescue/rescue bs=64k count=40 2>/dev/null | md5 -q)\" ] || exit 6; \
                 mv 'Test Dir/A Long File Name.txt' 'Test Dir/Sub/renamed file.TXT' || exit 7; \
                 [ \"$(cat 'test dir/sub/RENAMED FILE.txt')\" = hello ] || exit 8; \
                 mv 'Test Dir/Sub' Moved && [ -d Moved ] && [ ! -e 'Test Dir/Sub' ] || exit 9; \
                 for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo $i > \"many entries number $i\"; done; \
                 rm many* SHORT.TXT || exit 10; \
                 echo old > reuse.txt; exec 3< reuse.txt; rm reuse.txt; echo new > reuse.txt; \
                 [ \"$(cat <&3)\" = old ] && [ \"$(cat reuse.txt)\" = new ] || exit 17; exec 3<&-; rm reuse.txt || exit 18; \
                 if [ -x /usr/tests/fsx/fsx ]; then /usr/tests/fsx/fsx -q -N 2000 -S 9 /boot/efi/fsx.data > /tmp/fsx.out 2>&1 || exit 11; rm -f /boot/efi/fsx.data*; fi; \
                 cd / && umount /boot/efi || exit 12; \
                 fsck_msdosfs -n DEV > /tmp/fsck.out 2>&1 || exit 13; \
                 mount -t msdosfs DEV /boot/efi || exit 14; \
                 [ \"$(cat /boot/efi/Moved/'renamed file.TXT')\" = hello ] || exit 15; \
                 [ -f /boot/efi/EFI/ABYSS/KERNEL ] || exit 16; exit 7",
            )
            .replace("DEV", &dev);
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", script.as_bytes()],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            let exit = p.wait(&s);
            if exit != crate::proc::Exit::Code(7) {
                for f in [&b"/tmp/fsck.out"[..], b"/tmp/fsx.out"] {
                    if let Some(out) = read_whole(&s, f) {
                        kprintln!("{}:\n{}", core::str::from_utf8(f).unwrap_or("?"), alloc::string::String::from_utf8_lossy(&out));
                    }
                }
            }
            assert_eq!(exit, crate::proc::Exit::Code(7));
            let fsck = read_whole(&s, b"/tmp/fsck.out").unwrap_or_default();
            let text = alloc::string::String::from_utf8_lossy(&fsck);
            let problems: alloc::vec::Vec<&str> = text
                .lines()
                .filter(|l| l.contains('?') || l.contains("not correct") || l.contains("DIRTY"))
                .collect();
            assert!(problems.is_empty(), "fsck_msdosfs: {problems:?}\n{text}");
            kprintln!("msdosfs: read-write on {dev} is fsck clean");
            for f in [&b"/tmp/fsck.out"[..], b"/tmp/fsx.out"] {
                let _ = crate::fs::parent_of(&s, &p, crate::fs::At::Cwd, f, false)
                    .and_then(|d| vfs::ops::remove(&s, &d.dir, &d.name, &crate::fs::cred(), false));
            }
        }
    }

    ktest! {
        /// The FAT test disk mounts read-only with `/rescue`'s
        /// `mount_msdosfs`, and `ls -aliRnT` (file numbers included) and
        /// `md5 -r` over it print what FreeBSD's msdosfs printed over the
        /// same volume. Skipped when the disk or the listings are not there
        /// (no `fetch-base` from the server).
        fn msdosfs_reads_as_freebsd_reads_it() {
            use crate::proc::Process;
            let s = cx();
            let (Some(want_ls), Some(want_md5)) = (
                read_whole(&s, b"/usr/tests/fat/fat-test.ls"),
                read_whole(&s, b"/usr/tests/fat/fat-test.md5"),
            ) else {
                kprintln!("msdosfs: no test disk listings; skipped");
                return;
            };
            // The 40 MiB virtio disk with a FAT boot sector.
            let Some(d) = find_disk(&s, "vtbd", |s, d| {
                d.bytes() == 40 << 20 && d.read(s, 0, 512).is_ok_and(|b| b[510] == 0x55 && b[511] == 0xaa)
            }) else {
                kprintln!("msdosfs: no FAT test disk; skipped");
                return;
            };
            let script = alloc::format!(
                "mount -t msdosfs -o ro /dev/{} /mnt || exit 1; cd /mnt && TZ=UTC ls -aliRnT . > /tmp/fat.ls || exit 2; \
                 while IFS= read -r f; do md5 -r \"$f\"; done < /usr/tests/fat/fat-test.files > /tmp/fat.md5 || exit 3; cd / && umount /mnt || exit 4; exit 7",
                d.name()
            );
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", script.as_bytes()],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            let exit = p.wait(&s);
            // The root's `..` is the mount point's parent, and the root's
            // total counts it: neither is the volume's. Spacing aside, the
            // rest must match.
            let unrooted = |l: &[u8]| -> alloc::vec::Vec<u8> {
                let mut out = alloc::vec::Vec::new();
                let mut first = true;
                for line in l.split(|&c| c == b'\n') {
                    if line.is_empty() {
                        first = false;
                    }
                    if first && (line.starts_with(b"total ") || line.ends_with(b" ..")) {
                        continue;
                    }
                    // Column widths follow the widest field, which the
                    // dropped line may have been.
                    let words: alloc::vec::Vec<&[u8]> =
                        line.split(|&c| c == b' ').filter(|w| !w.is_empty()).collect();
                    out.extend_from_slice(&words.join(&b' '));
                    out.push(b'\n');
                }
                out
            };
            let want_ls = unrooted(&want_ls);
            let got_ls = unrooted(&read_whole(&s, b"/tmp/fat.ls").unwrap_or_default());
            let got_md5 = read_whole(&s, b"/tmp/fat.md5").unwrap_or_default();
            first_difference("msdosfs: ls -aliRnT", &got_ls, &want_ls);
            if got_ls != want_ls {
                kprintln!("msdosfs: got:\n{}", alloc::string::String::from_utf8_lossy(&got_ls));
                kprintln!("msdosfs: want:\n{}", alloc::string::String::from_utf8_lossy(&want_ls));
            }
            first_difference("msdosfs: md5 -r", &got_md5, &want_md5);
            assert_eq!(exit, crate::proc::Exit::Code(7));
            assert!(got_ls == want_ls && got_md5 == want_md5);
            let lines = want_md5.iter().filter(|&&c| c == b'\n').count();
            kprintln!("msdosfs: the listing and {lines} digests match FreeBSD's");
            for f in [&b"/tmp/fat.ls"[..], b"/tmp/fat.md5"] {
                let _ = crate::fs::parent_of(&s, &p, crate::fs::At::Cwd, f, false)
                    .and_then(|d| vfs::ops::remove(&s, &d.dir, &d.name, &crate::fs::cred(), false));
            }
        }
    }

    ktest! {
        fn fuzzing_leaves_nothing_behind() {
            // The fuzzer twice with the same seed: the first run warms
            // whatever the kernel caches; the second must leave free
            // memory exactly where the first did, and no process behind.
            use crate::proc::{Exit, Process};
            let s = cx();
            let run = || {
                let p = Process::spawn(
                    &s,
                    b"/usr/tests/abyss/fuzz",
                    &[b"fuzz", b"7", b"16", b"2000"],
                    &[],
                    Some(hal::cpu_id()),
                )
                .unwrap();
                assert_eq!(p.wait(&s), Exit::Code(0));
            };
            run();
            let warm = settled_free_pages(&s);
            run();
            let after = settled_free_pages(&s);
            assert_eq!(after, warm, "pages leaked by fuzzing");
            assert!(
                crate::proc::all().is_empty(),
                "processes left by fuzzing: {:?}",
                crate::proc::all().iter().map(|p| p.pid()).collect::<Vec<_>>()
            );
        }
    }

    ktest! {
        fn rescue_ps_lists_processes() {
            use crate::proc::Process;
            let s = cx();
            // Earlier tests' processes are all gone: none is left a zombie
            // with no one to reap it, nor kept by a reference.
            assert!(
                crate::proc::all().is_empty(),
                "processes outlived their tests: {:?}",
                crate::proc::all().iter().map(|p| p.pid()).collect::<Vec<_>>()
            );
            // `ps` finds itself and its shell; `-p $$` selects by pid.
            let script: &[u8] = b"ps -axo pid,ppid,pgid,stat,comm && \
                [ \"$(ps -p $$ -o comm=)\" = sh ] && \
                [ \"$(ps -o pid,comm -p $$ | sed -n '1s/^ *//p')\" = 'PID COMMAND' ] && exit 9";
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", script],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(9));
        }
    }

    ktest! {
        fn interactive_sh_job_control_on_the_console() {
            // /rescue/sh on the console as its controlling terminal, typed
            // at through the terminal's input path: ^C ends a foreground
            // job, ^Z stops one, `fg` continues it. Every sleep would take
            // 30 s if a keystroke did not reach it.
            use crate::proc::{Exit, Process};
            let s = cx();
            let tty = crate::tty::console().expect("the console is a terminal");
            tty.flush_input(&s);
            let p = Process::spawn_session(
                &s,
                b"/rescue/sh",
                &[b"sh"],
                &[b"PATH=/rescue", b"HOME=/", b"TERM=dumb"],
                None,
                Some(&tty),
            )
            .unwrap();
            let pause = |ms: u64| crate::time::sleep_ns(&s, ms * 1_000_000);
            let t0 = crate::time::now_ns();
            pause(300);
            tty.input(&s, b"echo typed at the console\n");
            tty.input(&s, b"sleep 30\n");
            pause(400);
            tty.input(&s, b"\x03");
            pause(200);
            tty.input(&s, b"sleep 30\n");
            pause(400);
            tty.input(&s, b"\x1a");
            pause(300);
            tty.input(&s, b"jobs\nfg\n");
            pause(400);
            tty.input(&s, b"\x03");
            pause(200);
            tty.input(&s, b"exit 6\n");
            assert_eq!(p.wait(&s), Exit::Code(6));
            let took = (crate::time::now_ns() - t0) / 1_000_000;
            assert!(took < 15_000, "the shell took {took} ms: a keystroke went astray");
        }
    }

    ktest! {
        fn user_code_makes_system_calls() {
            // write returned 6; the unknown call failed with ENOSYS (78)
            // and the carry flag set.
            assert_eq!(run_program(crate::arch::user::test_program()), 6022);
        }
    }

    #[cfg(target_arch = "x86_64")]
    ktest! {
        fn user_sets_its_tls_base() {
            assert_eq!(run_program(crate::arch::user::tls_test_program()), 42);
        }
    }

    ktest! {
        fn concurrent_faults_and_cross_cpu_unmap() {
            let s = cx();
            let space = AddressSpace::new().unwrap();
            let a = space.map_anonymous(&s, None, HINT, 64 * PAGE, Prot::RW).unwrap().get();
            let ncpu = u32::try_from(sched::online_cpus()).unwrap();
            // Every thread faults every page, writing its own slot in it.
            let workers: Vec<_> = (0..ncpu)
                .map(|cpu| {
                    let space = Arc::clone(&space);
                    sched::Builder::new("faulter")
                        .bind(hal::CpuId(cpu))
                        .spawn(move |_| {
                            enter(&space);
                            for p in 0..64 {
                                UserPtr::<u32>::new(a + p * PAGE + u64::from(cpu) * 4)
                                    .write(&(cpu + 1))
                                    .unwrap();
                            }
                            leave();
                        })
                        .unwrap()
                })
                .collect();
            for w in &workers {
                w.join(&s);
            }
            enter(&space);
            for p in 0..64 {
                for cpu in 0..ncpu {
                    let v = UserPtr::<u32>::new(a + p * PAGE + u64::from(cpu) * 4).read();
                    assert_eq!(v, Ok(cpu + 1));
                }
            }

            // A reader on another CPU keeps the page in its TLB; once this
            // CPU unmaps it, the reader must fault, not read the stale
            // translation.
            let other = hal::CpuId((hal::cpu_id().0 + 1) % ncpu);
            let unmapped = Arc::new(AtomicBool::new(false));
            let reads = Arc::new(AtomicU32::new(0));
            let reader = {
                let (space, unmapped, reads) =
                    (Arc::clone(&space), Arc::clone(&unmapped), Arc::clone(&reads));
                sched::Builder::new("reader")
                    .bind(other)
                    .spawn(move |_| {
                        enter(&space);
                        loop {
                            // Once the unmap (and its shootdown) is done,
                            // before the flag, a read must fault.
                            let done = unmapped.load(Ordering::Acquire);
                            let r = read_u64(a);
                            if done {
                                assert_eq!(r, Err(Fault), "stale TLB entry");
                                break;
                            }
                            if r.is_ok() {
                                reads.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        leave();
                    })
                    .unwrap()
            };
            while reads.load(Ordering::Relaxed) < 100 {
                sched::yield_now();
            }
            space.unmap(&s, VirtAddr::new(a)..VirtAddr::new(a + PAGE)).unwrap();
            unmapped.store(true, Ordering::Release);
            reader.join(&s);
            // Exit's teardown: every page goes now, though CPUs the space
            // was loaded on may hold its (empty) tables until they load
            // another. (Page balance is left to the single-threaded tests:
            // the reaper frees the workers' stacks in its own time.)
            space.clear(&s);
            assert!(space.translate(VirtAddr::new(a + PAGE)).is_none());
            assert_eq!(read_u64(a + PAGE), Err(Fault));
            leave();
        }
    }
}

ktest! {
    fn set_id_calls_follow_freebsd() {
        use abi::errno::Errno;

        use crate::cred::{Ids, Ucred};
        let root = Ucred::root();
        // The superuser's setuid sets all three uids.
        let u = root.setuid(1000).unwrap();
        assert_eq!((u.ruid, u.euid(), u.svuid), (1000, 1000, 1000));
        assert_eq!(u.setuid(0), Err(Errno::EPERM));
        assert_eq!(u.setgroups(&[5]), Err(Errno::EPERM));
        // seteuid keeps the real and saved uids, so it can come back.
        let e = root.seteuid(65534).unwrap();
        assert_eq!((e.ruid, e.euid(), e.svuid), (0, 65534, 0));
        assert!(!e.is_root());
        let back = e.seteuid(0).unwrap();
        assert!(back.is_root());
        // Not the superuser: only the real or saved uid.
        assert_eq!(e.seteuid(7), Err(Errno::EPERM));
        // setuid to the saved uid, not as the superuser, moves only the
        // effective uid (the saved one stays for coming back).
        let s = e.setuid(0).unwrap();
        assert_eq!((s.ruid, s.euid(), s.svuid), (0, 0, 0));
        // Groups: sorted without repeats, without the effective gid.
        let g = root.setgroups(&[65533, 5, 65533]).unwrap().setegid(65534).unwrap();
        assert_eq!(g.groups(), &[5, 65533]);
        assert_eq!((g.rgid, g.egid()), (0, 65534));
        assert!(g.in_group(65534) && g.in_group(5) && !g.in_group(0));
        assert_eq!(root.setgroups(&[0; 1024]), Err(Errno::EINVAL));
        // setresuid: unprivileged, any of the three current uids.
        let r = root
            .setresuid(Ids { real: Some(1), effective: Some(2), saved: Some(3) })
            .unwrap();
        let swapped = r
            .setresuid(Ids { real: Some(3), effective: Some(1), saved: None })
            .unwrap();
        assert_eq!((swapped.ruid, swapped.euid(), swapped.svuid), (3, 1, 3));
        assert_eq!(
            r.setresuid(Ids { real: None, effective: Some(9), saved: None }),
            Err(Errno::EPERM)
        );
        // setreuid: setting the real uid sets the saved one to the
        // effective.
        let re = r.setreuid(Some(3), Some(1)).unwrap();
        assert_eq!((re.ruid, re.euid(), re.svuid), (3, 1, 1));
        // access checks with the real ids.
        assert!(e.as_real().is_root());
    }
}
