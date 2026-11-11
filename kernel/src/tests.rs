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

/// The `x86_64-watchdog` case's first boot: FreeBSD's `watchdogd -t 4
/// -s 1` arms the watchdog through `/dev/fido` and keeps it patted well
/// past its timeout; then it is killed (no disarming on its way out), and
/// the watchdog must reset the machine while the kernel idles. Without
/// `watchdogd` on the memory disk, the class arms it directly and the
/// kernel hangs with interrupts off.
fn watchdog_reset_first_boot(s: &sched::Sleepable) -> ! {
    use abi::watchdog::SBT_1S;
    let name =
        watchdog::first().map_or_else(|| "?".into(), |w| alloc::string::String::from(w.name()));
    let argv: [&[u8]; 6] = [b"watchdogd", b"-d", b"-t", b"4", b"-s", b"1"];
    match crate::proc::Process::spawn(s, b"/usr/sbin/watchdogd", &argv, &[], None) {
        Ok(p) => {
            // Twice the timeout, and more: only the pats keep us up.
            crate::time::sleep_ns(s, 10_000_000_000);
            assert!(!p.has_exited(), "watchdogd exited: {:?}", p.wait(s));
            kprintln!("watchdog: watchdogd kept {name} patted for 10 s past a 4 s timeout");
            let info =
                crate::signal::user_info(abi::signal::SIGKILL, 0, abi::signal::code::SI_KERNEL);
            let _ = crate::signal::post(&p, info);
            let _ = p.wait(s);
            kprintln!("watchdog: patting stopped (watchdogd killed); waiting for the reset");
            loop {
                crate::time::sleep_ns(s, 1_000_000_000);
            }
        }
        Err(e) => {
            kprintln!("watchdog: no watchdogd ({e:?}); arming {name} through the class");
            crate::fido::pat_sbt(3 * SBT_1S).expect("a watchdog takes 3 s");
            kprintln!("watchdog: patting stopped (hanging); waiting for the reset");
            crate::arch::cpu::disable_interrupts();
            loop {
                core::hint::spin_loop();
            }
        }
    }
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
                watchdog_reset_first_boot(_s);
            }
            // A panic with the other CPUs running: they are stopped, and
            // the machine reboots after `kern.panic_reboot_wait_time`.
            if crate::boot::flag("abyss.test.panic_reset") {
                crate::panic::EXPECT_PANIC_RESET.store(true, Ordering::Relaxed);
                panic!("deliberate panic (abyss.test.panic_reset)");
            }
            // As a reboot does: every device stopped first, an IOMMU after
            // the devices it translates.
            if crate::boot::flag("abyss.test.orderly_reset") {
                bus::manager().wait_idle(_s);
                crate::devices::shutdown(_s);
                kprintln!("logring: devices shut down before the reset");
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
    // Tests count pages and read events: buses still finding devices
    // (USB) would be counted with them.
    bus::root_mount_wait(&cx());
    let clock = || crate::time::now_ns() / 1_000_000;
    // `abyss.test.skip=a::b,c::d` leaves out the tests under those paths
    // (the boot tier's reset and watchdog cases skip the slow user-memory
    // tests, which the plain cases run); `abyss.test.only=a::b,c::d` runs
    // only those (the low-memory cases run their one test).
    let skip = crate::boot::value("abyss.test.skip").unwrap_or("");
    let only = crate::boot::value("abyss.test.only").unwrap_or("");
    let under = |list: &str, name: &str| {
        list.split(',')
            .any(|p| !p.is_empty() && name.starts_with(p))
    };
    let keep = |name: &str| !under(skip, name) && (only.is_empty() || under(only, name));
    let _ = ktest::run_selected(&mut crate::console::Writer, Some(&clock), &keep);
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
        // The counts are the whole kernel's: drivers attaching meanwhile
        // (under emulation the attach is still going) move them too, so
        // it is waited out. Other work is rarer; a leak shows in every
        // round, someone else's allocations seldom in three.
        bus::manager().wait_idle(&cx());
        let mut last = None;
        for _ in 0..3 {
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
            let before = (before.live_objects, before.large_pages);
            let after = (after.live_objects, after.large_pages);
            if after == before {
                return;
            }
            last = Some((before, after));
        }
        panic!("the heap's (objects, large pages) did not come back in three rounds: {last:?}");
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
    fn wake_all_wakes_only_the_sleepers_it_finds() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicBool;
        // Sleepers whose condition stays false sleep again as soon as they
        // wake, as `umass`'s workers do when another took the request:
        // each `wake_all` must still end, waking each of them once at most.
        const SLEEPERS: usize = 6;
        struct Pool {
            stop: AtomicBool,
            q: sched::WaitQueue,
        }
        let pool = Arc::new(Pool { stop: AtomicBool::new(false), q: sched::WaitQueue::new() });
        let threads: alloc::vec::Vec<_> = (0..SLEEPERS)
            .map(|_| {
                let pool = Arc::clone(&pool);
                sched::spawn("sleeper", sched::Priority::KERNEL, move |s| {
                    pool.q.wait_until(s, || pool.stop.load(Ordering::Acquire));
                })
                .expect("spawn")
            })
            .collect();
        for _ in 0..5000 {
            let n = pool.q.wake_all();
            assert!(n <= SLEEPERS, "one wake_all woke {n} of {SLEEPERS} sleepers");
        }
        pool.stop.store(true, Ordering::Release);
        pool.q.wake_all();
        for t in &threads {
            t.join(&cx());
        }
    }
}

ktest! {
    fn exited_threads_are_reaped() {
        let kva = crate::mm::kstack::kva_allocated();
        let t = sched::spawn("short", sched::Priority::KERNEL, |_| {}).unwrap();
        t.join(&cx());
        drop(t);
        // Reaped on demand (and any left over from earlier tests, so the
        // total may drop below the baseline), once its CPU has switched
        // away from it.
        for _ in 0..2000 {
            sched::reap_zombies();
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
    fn statclock_ticks_stay_on_their_grid() {
        // A thread on one CPU that sleeps and wakes again and again: each
        // wakeup restarts that CPU's statistics clock, and its next tick
        // must stay on the grid it started on, whole periods on, however
        // the wakeup fell. (Restarting the period at each wakeup put every
        // tick after a short burst of work, in idle time.)
        let ncpu = u32::try_from(sched::online_cpus()).unwrap();
        let cpu = hal::CpuId(ncpu - 1);
        let t = sched::Builder::new("grid")
            .bind(cpu)
            .spawn(move |s| {
                let i = cpu.index();
                let start = crate::statclock::next_tick(i);
                let mut moved = false;
                for _ in 0..40 {
                    crate::time::sleep_ns(s, 1_300_000);
                    let t0 = crate::time::now_ns();
                    while crate::time::now_ns() - t0 < 200_000 {
                        core::hint::spin_loop();
                    }
                    let next = crate::statclock::next_tick(i);
                    assert!(next >= start, "the grid went back: {next} < {start}");
                    assert_eq!(
                        (next - start) % crate::statclock::PERIOD_NS,
                        0,
                        "off the grid: {next} from {start}"
                    );
                    moved |= next != start;
                }
                assert!(moved, "no tick in 40 wakeups");
            })
            .unwrap();
        t.join(&cx());
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
        // microsecond plus the read gap, once what the discipline moved
        // the timehands by is taken out: the kernel's clock is the raw
        // counter, and a slew (`adjtime`, `ntpd`; `tests/user/ntp` makes
        // one) moves the timehands' uptime, as FreeBSD's moves
        // `binuptime`. A windup just before makes that what it is now,
        // and with no adjustment ever made it is 0. The closest of a few
        // tries, as an interrupt (or, under emulation, a slow vCPU) can
        // come between the two reads.
        wind();
        let moved = i128::from(crate::shared_page::disciplined_ns());
        let (a, b) = (0..16)
            .map(|_| (to_ns(uptime()), crate::time::now_ns()))
            .min_by_key(|&(a, b)| (i128::from(a) - i128::from(b) - moved).unsigned_abs())
            .unwrap_or_default();
        let off = i128::from(a) - i128::from(b) - moved;
        assert!(
            off.unsigned_abs() < 100_000,
            "timehands {a} ns, clock {b} ns, disciplined by {moved} ns; ntp {:?}",
            crate::ntp::state()
        );
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
        // `uname -i`, and the kernel file's string `freebsd-version -k` reads.
        let ident = crate::sysctl::read(&s, |t| {
            let oid = t.name2oid("kern.ident").expect("kern.ident");
            let mut v = alloc::vec::Vec::new();
            t.read(&oid, &mut v).expect("read");
            v
        });
        assert_eq!(ident, b"ABYSSBSD\0");
        let what = crate::sysctl::what();
        assert!(
            what.starts_with(b"@(#)FreeBSD 15.1-RELEASE ABYSSBSD (AbyssBSD "),
            "{}",
            core::str::from_utf8(what).unwrap_or("?")
        );
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
    fn irq_cascaded_inputs_dispatch_from_their_parent() {
        // A GPIO-like device: one parent input whose filter runs the
        // pending inputs of the device's cascaded domain. An edge input
        // is handled in its filter; a level input's thread runs with the
        // input masked, and it is unmasked after.
        use alloc::sync::Arc;
        use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
        use ::irq::cascade::{Cascade, CascadeDomain};
        use ::irq::{Filter, IrqError, Polarity, Trigger};
        static PENDING: AtomicU32 = AtomicU32::new(0);
        static MASKED: AtomicU32 = AtomicU32::new(u32::MAX);
        static EDGES: AtomicU32 = AtomicU32::new(0);
        static LEVEL_RAN: AtomicBool = AtomicBool::new(false);
        static CASCADE: sync::Once<CascadeDomain> = sync::Once::new();
        struct Pins;
        impl Cascade for Pins {
            fn name(&self) -> &'static str {
                "ktest-pins"
            }
            fn inputs(&self) -> u32 {
                8
            }
            fn configure(&self, hw: u32, t: Trigger, p: Polarity) -> Result<(), IrqError> {
                match (hw, t, p) {
                    (_, Trigger::Level, Polarity::Both) => Err(IrqError::Unsupported),
                    _ => Ok(()),
                }
            }
            fn mask(&self, hw: u32) {
                MASKED.fetch_or(1 << hw, SeqCst);
            }
            fn unmask(&self, hw: u32) {
                MASKED.fetch_and(!(1 << hw), SeqCst);
            }
        }
        struct Parent;
        impl ::irq::IrqDomain for Parent {
            fn name(&self) -> &'static str {
                "ktest-parent"
            }
            fn connect(
                &self,
                _: u32,
                _: Trigger,
                _: Polarity,
                cpu: hal::CpuId,
            ) -> Result<u32, IrqError> {
                #[cfg(target_arch = "x86_64")]
                return ::irq::alloc_vector(cpu).ok_or(IrqError::NoVector);
                #[cfg(target_arch = "aarch64")]
                {
                    let _ = cpu;
                    Ok(285)
                }
            }
            fn disconnect(&self, _: u32, cpu: hal::CpuId, root: u32) {
                #[cfg(target_arch = "x86_64")]
                ::irq::free_vector(cpu, root);
                let _ = (cpu, root);
            }
            fn mask(&self, _: u32) {}
            fn unmask(&self, _: u32) {}
        }
        static PARENT: Parent = Parent;
        // The parent's filter: what a GPIO controller's does.
        fn parent(_: usize, cx: &sched::IrqCx) -> Filter {
            let pending = PENDING.swap(0, SeqCst) & !MASKED.load(SeqCst);
            let c = CASCADE.get().expect("cascade");
            let mut any = false;
            for hw in 0..8 {
                if pending & 1 << hw != 0 {
                    any = true;
                    if !c.dispatch(hw, cx) {
                        MASKED.fetch_or(1 << hw, SeqCst);
                    }
                }
            }
            if any { Filter::Handled } else { Filter::Stray }
        }
        fn edge(_: usize, _: &sched::IrqCx) -> Filter {
            EDGES.fetch_add(1, SeqCst);
            Filter::Handled
        }
        fn level(_: usize, _: &sched::Sleepable) {
            // Masked while its thread runs.
            assert_ne!(MASKED.load(SeqCst) & 1 << 5, 0, "level input unmasked in its thread");
            LEVEL_RAN.store(true, SeqCst);
        }
        let s = cx();
        let cascade = CascadeDomain::new(Arc::new(Pins)).expect("a cascade slot");
        let _ = CASCADE.set(cascade);
        let c = CASCADE.get().expect("cascade");
        let p = ::irq::install(
            &PARENT,
            0,
            Trigger::Level,
            Polarity::High,
            hal::CpuId::BOOT,
            ::irq::Handler { name: "ktest-parent", filter: Some(parent), thread: None, arg: 0 },
        )
        .expect("parent");
        let handler = |name, filter, thread| ::irq::Handler { name, filter, thread, arg: 0 };
        let e = ::irq::install(
            c.domain(),
            3,
            Trigger::Edge,
            Polarity::Both,
            hal::CpuId::BOOT,
            handler("ktest-edge", Some(edge as ::irq::FilterFn), None),
        )
        .expect("edge input");
        let l = ::irq::install(
            c.domain(),
            5,
            Trigger::Level,
            Polarity::Low,
            hal::CpuId::BOOT,
            handler("ktest-level", None, Some(level as ::irq::ThreadFn)),
        )
        .expect("level input");
        assert_eq!(
            ::irq::install(
                c.domain(),
                6,
                Trigger::Level,
                Polarity::Both,
                hal::CpuId::BOOT,
                handler("ktest-bad", Some(edge as ::irq::FilterFn), None),
            )
            .map(|_| ()),
            Err(IrqError::Unsupported)
        );
        assert!(c.connected(3) && c.connected(5) && !c.connected(6));
        // Cascaded inputs are numbered past the pins.
        let numbers: alloc::vec::Vec<u32> = ::irq::sources()
            .into_iter()
            .filter(|(_, name, _)| name.starts_with("ktest-edge") || name.starts_with("ktest-level"))
            .map(|(n, _, _)| n)
            .collect();
        assert!(numbers.len() == 2 && numbers.iter().all(|&n| n >= ::irq::MSI_FIRST), "{numbers:?}");
        assert_eq!(MASKED.load(SeqCst) & (1 << 3 | 1 << 5), 0, "installed inputs unmasked");
        // Inputs 3, 5, and 7 (no handler) pending at once.
        PENDING.store(1 << 3 | 1 << 5 | 1 << 7, SeqCst);
        let root = p.root();
        let fire = sched::spawn_on("ktest-fire", sched::Priority::KERNEL, Some(hal::CpuId::BOOT), move |_| {
            let irq = hal::irq::disable();
            // SAFETY: interrupts are masked for as long as the token is
            // used, as in the dispatcher.
            let cx = unsafe { sched::IrqCx::assume() };
            assert!(::irq::dispatch(root, &cx), "parent dispatched");
            hal::irq::restore(irq);
        })
        .expect("spawn");
        fire.join(&s);
        assert_eq!(EDGES.load(SeqCst), 1);
        assert_eq!(e.count(), 1);
        assert_ne!(MASKED.load(SeqCst) & 1 << 7, 0, "an input with no handler is masked");
        let t0 = crate::time::now_ns();
        while (!LEVEL_RAN.load(SeqCst) || MASKED.load(SeqCst) & 1 << 5 != 0)
            && crate::time::now_ns() - t0 < 1_000_000_000
        {
            crate::time::sleep_ns(&s, 1_000_000);
        }
        assert!(LEVEL_RAN.load(SeqCst), "the level input's thread ran");
        assert_eq!(MASKED.load(SeqCst) & 1 << 5, 0, "unmasked once its thread was done");
        l.remove(&s);
        e.remove(&s);
        p.remove(&s);
        assert!(!c.connected(3) && !c.connected(5));
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
    /// Models 23, 24, and 25 wait for a supplier to settle (a settle
    /// edge): one whose attach fails (`broken`, model 5), one no driver
    /// matches (model 98), and one that attaches slowly (model 7, which
    /// model 25 has as its bus data).
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
            let (failed, unmatched) = (add(23), add(24));
            let (broken, nobody, slow) = (add(5), add(98), add(7));
            let after_slow = cx.add_child(
                Ident::Test(TestIdent { model: 25 }),
                Some(alloc::sync::Arc::new(slow)),
                vec![],
            );
            use devtree::graph::{EdgeKind::Required, EdgeKind::Settle, Origin::Dep, Origin::Iort};
            cx.add_dependency(early, late, Required, Dep);
            cx.add_dependency(a, b, Required, Dep);
            cx.add_dependency(b, a, Required, Dep);
            cx.add_dependency(failed, broken, Settle, Iort);
            cx.add_dependency(unmatched, nobody, Settle, Iort);
            cx.add_dependency(after_slow, slow, Settle, Iort);
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
            TestId {
                model: 23,
                rank: Rank::DEFAULT,
            },
            TestId {
                model: 24,
                rank: Rank::DEFAULT,
            },
            TestId {
                model: 25,
                rank: Rank::DEFAULT,
            },
        ]);
        fn describe(_: &Ident) -> String {
            "Ordered test device".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            if let Ident::Test(t) = cx.ident() {
                // Model 25 records whether its supplier had attached: 125
                // if so, 225 if not.
                let model = match cx.bus_data::<bus::DeviceId>() {
                    Some(&sup)
                        if bus::manager().state(sup, cx.sleepable())
                            == Some(bus::NodeState::Attached) =>
                    {
                        t.model + 100
                    }
                    Some(_) => t.model + 200,
                    None => t.model,
                };
                ATTACHED.lock().push(model);
            }
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    pub(crate) const DEP_BUS: DriverDecl = DriverDecl::of::<DepBus>();
    pub(crate) const ORDERED: DriverDecl = DriverDecl::of::<Ordered>();

    /// What the test clock and reset provider was told.
    pub(crate) static PROVIDER_CALLS: sync::SpinLock<alloc::vec::Vec<(&'static str, u32)>> =
        sync::SpinLock::new(alloc::vec::Vec::new());

    /// A bus adding a clock and reset provider (model 14) and a consumer
    /// (model 15) whose firmware lines name it.
    pub(crate) struct LineBus;

    impl Driver for LineBus {
        const NAME: &'static str = "linebus";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 13,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Provider test bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            use bus::{Line, LineKind};
            use devtree::graph::{EdgeKind::Required, Origin::Platform};
            // The consumer first: its supplier edge holds it back.
            let user = cx.add_child(Ident::Test(TestIdent { model: 15 }), None, vec![]);
            let prov = cx.add_child(Ident::Test(TestIdent { model: 14 }), None, vec![]);
            cx.add_dependency(user, prov, Required, Platform);
            let line = |kind, name: Option<&str>, id| Line {
                kind,
                name: name.map(String::from),
                supplier: prov,
                id,
                detail: bus::LineDetail::None,
            };
            cx.set_lines(
                user,
                vec![
                    line(LineKind::Clock, Some("apb"), 6),
                    line(LineKind::Clock, Some("core"), 7),
                    line(LineKind::Reset, None, 3),
                ],
            );
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Records every call.
    struct Recorder;

    impl Recorder {
        fn record(what: &'static str, id: u32) -> Result<(), bus::provider::ProviderError> {
            PROVIDER_CALLS.lock().push((what, id));
            Ok(())
        }
    }

    impl bus::provider::Clock for Recorder {
        fn enable(
            &self,
            _: &sched::Sleepable,
            id: u32,
        ) -> Result<(), bus::provider::ProviderError> {
            Self::record("enable", id)
        }
        fn disable(
            &self,
            _: &sched::Sleepable,
            id: u32,
        ) -> Result<(), bus::provider::ProviderError> {
            Self::record("disable", id)
        }
        fn rate(
            &self,
            _: &sched::Sleepable,
            _: u32,
        ) -> Result<bus::provider::Hz, bus::provider::ProviderError> {
            Ok(bus::provider::Hz(24_000_000))
        }
        fn round_rate(
            &self,
            _: &sched::Sleepable,
            _: u32,
            want: bus::provider::Hz,
        ) -> Result<bus::provider::Hz, bus::provider::ProviderError> {
            Ok(want)
        }
        fn set_rate(
            &self,
            _: &sched::Sleepable,
            id: u32,
            _: bus::provider::Hz,
        ) -> Result<(), bus::provider::ProviderError> {
            Self::record("set_rate", id)
        }
    }

    impl bus::provider::Reset for Recorder {
        fn assert(
            &self,
            _: &sched::Sleepable,
            line: u32,
        ) -> Result<(), bus::provider::ProviderError> {
            Self::record("assert", line)
        }
        fn deassert(
            &self,
            _: &sched::Sleepable,
            line: u32,
        ) -> Result<(), bus::provider::ProviderError> {
            Self::record("deassert", line)
        }
    }

    /// The provider: publishes its clocks and resets in `start`.
    pub(crate) struct LineProvider;

    impl Driver for LineProvider {
        const NAME: &'static str = "lineprov";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 14,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Test clock and reset provider".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            let r = alloc::sync::Arc::new(Recorder);
            cx.provide_clocks(r.clone());
            cx.provide_resets(r);
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// The consumer: takes its clock and reset by name, as a SoC block's
    /// driver would.
    pub(crate) struct LineUser {
        _core: bus::provider::ClockHandle,
        _reset: bus::provider::ResetHandle,
    }

    impl Driver for LineUser {
        const NAME: &'static str = "lineuser";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 15,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Test clock and reset consumer".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            let s = cx.sleepable();
            let fail = |e| AttachError::Resource(e);
            // Two named clocks: an unnamed request is ambiguous.
            assert_eq!(cx.clock(None).err(), Some(bus::ResourceError::NoSuchLine));
            assert_eq!(
                cx.clock(Some("pll")).err(),
                Some(bus::ResourceError::NoSuchLine)
            );
            let mut core = cx.clock(Some("core")).map_err(fail)?;
            core.set_rate(s, bus::provider::Hz(800_000_000))
                .map_err(|e| fail(bus::ResourceError::Provider(e)))?;
            core.enable(s)
                .map_err(|e| fail(bus::ResourceError::Provider(e)))?;
            let mut reset = cx.reset(None).map_err(fail)?;
            reset
                .deassert(s)
                .map_err(|e| fail(bus::ResourceError::Provider(e)))?;
            Ok(Self {
                _core: core,
                _reset: reset,
            })
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// What `CompanionBus`'s devices attached, in order.
    pub(crate) static COMPANION_ORDER: sync::SpinLock<alloc::vec::Vec<&'static str>> =
        sync::SpinLock::new(alloc::vec::Vec::new());

    /// A PCI function standing for a namespace object.
    fn companion_ident(slot: u8, path: &str) -> Ident {
        Ident::Pci(
            devtree::ids::PciIdent {
                vendor: 0xfffe,
                device: 0x0001,
                subvendor: 0,
                subdevice: 0,
                class: 0xff_0000,
                revision: 0,
            },
            devtree::strings::PciAddress {
                domain: 0,
                bus: 0xfe,
                slot,
                function: 0,
            },
            Some(path.into()),
        )
    }

    /// A bus whose two "PCI functions" are ACPI companions depending on
    /// a third device: one added before the dependency is recorded, one
    /// after (as the PCI bus adds functions after `acpi0` has started).
    pub(crate) struct CompanionBus;

    impl Driver for CompanionBus {
        const NAME: &'static str = "compbus";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 16,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Companion test bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            use devtree::graph::{EdgeKind::Required, Origin::ResourceSource};
            let _early = cx.add_child(companion_ident(1, "\\_SB_.CMPA"), None, vec![]);
            let sup = cx.add_child(Ident::Test(TestIdent { model: 17 }), None, vec![]);
            cx.add_companion_dependency("\\_SB_.CMPA".into(), sup, Required, ResourceSource);
            cx.add_companion_dependency("\\_SB_.CMPB".into(), sup, Required, ResourceSource);
            let _late = cx.add_child(companion_ident(2, "\\_SB_.CMPB"), None, vec![]);
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Records its attach: the supplier (model 17), or a companion by its
    /// path.
    pub(crate) struct CompanionDev;

    impl Driver for CompanionDev {
        const NAME: &'static str = "compdev";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 17,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Companion supplier".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            // Slow enough that a consumer not held back would come first.
            crate::time::sleep_ns(cx.sleepable(), 20_000_000);
            COMPANION_ORDER.lock().push("supplier");
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    pub(crate) struct CompanionFn;

    impl Driver for CompanionFn {
        const NAME: &'static str = "compfn";
        const IDS: Ids = Ids::Pci(&[devtree::ids::PciId::device(0xfffe, 0x0001)]);
        fn describe(_: &Ident) -> String {
            "Companion PCI function".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            let which = match cx.ident().acpi_path() {
                Some(p) if p.ends_with("CMPA") => "CMPA",
                _ => "CMPB",
            };
            COMPANION_ORDER.lock().push(which);
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// RAM pages the shared-range bus hands its devices.
    pub(crate) static SHARED_PAGES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
    /// What each shared-range device was granted: its label, and the
    /// grant's sharing or the refusal.
    pub(crate) static SHARED_GRANTS: sync::SpinLock<
        alloc::vec::Vec<(u8, Result<bus::Sharing, bus::ResourceError>)>,
    > = sync::SpinLock::new(alloc::vec::Vec::new());

    /// A bus recording a firmware-shared range in its first page and a
    /// declared-shared one at the start of its second, as `acpi0` records
    /// AML's regions and the platform's declarations, with four children:
    /// (1) the first page, which AML touches; (2) the second page whole
    /// and (3) its declared start, like the Sky1's mailbox and SCMI
    /// shared memory; (4) a range of the second page outside the
    /// declaration.
    pub(crate) struct SharedBus;

    impl Driver for SharedBus {
        const NAME: &'static str = "sharebus";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 18,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Shared-range test bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            use bus::{AddressSpace::Memory, Range};
            use devtree::graph::{EdgeKind::Required, Origin::ResourceSource};
            let [a, b] = [0, 1].map(|k| SHARED_PAGES[k].load(Ordering::Relaxed));
            cx.share_with_firmware(Range {
                space: Memory,
                base: a + 0x10,
                len: 4,
            });
            cx.declare_shared(Range {
                space: Memory,
                base: b,
                len: 0x80,
            });
            let mem = |base, len| Resource::Memory {
                base,
                len,
                prefetchable: false,
            };
            let mut ids = alloc::vec::Vec::new();
            for (label, r) in [
                (1u8, mem(a, 4096)),
                (2, mem(b, 4096)),
                (3, mem(b, 0x80)),
                (4, mem(b + 0x100, 0x80)),
            ] {
                ids.push(cx.add_child(
                    Ident::Test(TestIdent { model: 19 }),
                    Some(alloc::sync::Arc::new(label)),
                    vec![r],
                ));
            }
            // The fourth maps after the two it overlaps.
            for sup in [ids[1], ids[2]] {
                cx.add_dependency(ids[3], sup, Required, ResourceSource);
            }
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Maps its range, asking knowingly only after a plain request is
    /// refused as firmware-shared, and records what it got.
    pub(crate) struct SharedDev {
        _regs: MmioGrant<regs::Bytes>,
    }

    impl Driver for SharedDev {
        const NAME: &'static str = "sharedev";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 19,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Shared-range test device".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            let label = *cx.bus_data::<u8>().expect("label");
            let mut got = cx.mmio_bytes(0);
            if let Err(e) = got {
                SHARED_GRANTS.lock().push((label, Err(e)));
                if e == bus::ResourceError::FirmwareShared {
                    got = cx.mmio_bytes_shared(0);
                }
            }
            let regs = got?;
            SHARED_GRANTS.lock().push((label, Ok(regs.sharing())));
            Ok(Self { _regs: regs })
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    pub(crate) const SHARED_BUS: DriverDecl = DriverDecl::of::<SharedBus>();
    pub(crate) const SHARED_DEV: DriverDecl = DriverDecl::of::<SharedDev>();

    pub(crate) const COMPANION_BUS: DriverDecl = DriverDecl::of::<CompanionBus>();
    pub(crate) const COMPANION_DEV: DriverDecl = DriverDecl::of::<CompanionDev>();
    pub(crate) const COMPANION_FN: DriverDecl = DriverDecl::of::<CompanionFn>();

    pub(crate) const LINE_BUS: DriverDecl = DriverDecl::of::<LineBus>();
    pub(crate) const LINE_PROVIDER: DriverDecl = DriverDecl::of::<LineProvider>();
    pub(crate) const LINE_USER: DriverDecl = DriverDecl::of::<LineUser>();

    /// What the test GPIO and I2C controller was asked.
    pub(crate) static PIN_CALLS: sync::SpinLock<alloc::vec::Vec<(&'static str, u32)>> =
        sync::SpinLock::new(alloc::vec::Vec::new());
    /// The test controller's interrupt domain, while it is attached.
    pub(crate) static PIN_CASCADE: sync::SpinLock<Option<::irq::cascade::CascadeDomain>> =
        sync::SpinLock::new(None);
    /// Interrupts the consumer's handler took.
    pub(crate) static PIN_IRQS: AtomicU64 = AtomicU64::new(0);

    /// A bus adding a GPIO and I2C controller (model 21) and a device
    /// (model 22) whose firmware gives it a `GpioInt`, a `GpioIo`, and an
    /// `I2cSerialBusV2` on it.
    pub(crate) struct PinBus;

    impl Driver for PinBus {
        const NAME: &'static str = "pinbus";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 20,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "GPIO and I2C test bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            use bus::gpio::{GpioMode, GpioUse, Pull, Restriction};
            use bus::{Line, LineDetail, LineKind};
            use devtree::graph::{EdgeKind::Required, Origin::ResourceSource};
            let user = cx.add_child(Ident::Test(TestIdent { model: 22 }), None, vec![]);
            let ctl = cx.add_child(Ident::Test(TestIdent { model: 21 }), None, vec![]);
            cx.add_dependency(user, ctl, Required, ResourceSource);
            let gpio = |pins: alloc::vec::Vec<u32>, mode| Line {
                kind: LineKind::Gpio,
                name: None,
                supplier: ctl,
                id: pins[0],
                detail: LineDetail::Gpio(GpioUse {
                    pins,
                    mode,
                    shared: false,
                    pull: Pull::Up,
                    debounce_us: 0,
                }),
            };
            cx.set_lines(
                user,
                vec![
                    gpio(
                        vec![4],
                        GpioMode::Interrupt {
                            trigger: ::irq::Trigger::Edge,
                            polarity: ::irq::Polarity::Both,
                            wake: false,
                        },
                    ),
                    gpio(vec![1, 2], GpioMode::Io(Restriction::Output)),
                    Line {
                        kind: LineKind::I2c,
                        name: None,
                        supplier: ctl,
                        id: 0x32,
                        detail: LineDetail::I2c(bus::i2c::I2cTarget {
                            address: 0x32,
                            ten_bit: false,
                            speed: 400_000,
                        }),
                    },
                ],
            );
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// The controller's pins and bus, recording what it is asked.
    struct PinCtl;

    impl ::irq::cascade::Cascade for PinCtl {
        fn name(&self) -> &'static str {
            "ktest-pinctl"
        }
        fn inputs(&self) -> u32 {
            8
        }
        fn configure(
            &self,
            hw: u32,
            _: ::irq::Trigger,
            _: ::irq::Polarity,
        ) -> Result<(), ::irq::IrqError> {
            PIN_CALLS.lock().push(("trigger", hw));
            Ok(())
        }
        fn release(&self, hw: u32) {
            PIN_CALLS.lock().push(("unhook", hw));
        }
        fn mask(&self, _: u32) {}
        fn unmask(&self, _: u32) {}
    }

    struct PinGpio(&'static dyn ::irq::IrqDomain);

    impl bus::gpio::Gpio for PinGpio {
        fn pins(&self) -> u32 {
            8
        }
        fn configure(
            &self,
            _: &sched::Sleepable,
            pin: u32,
            mode: bus::gpio::PinMode,
            _: bus::gpio::Pull,
        ) -> Result<(), bus::provider::ProviderError> {
            let what = match mode {
                bus::gpio::PinMode::Input => "input",
                bus::gpio::PinMode::Output { .. } => "output",
            };
            PIN_CALLS.lock().push((what, pin));
            Ok(())
        }
        fn get(&self, _: &sched::Sleepable, _: u32) -> Result<bool, bus::provider::ProviderError> {
            Ok(false)
        }
        fn set(
            &self,
            _: &sched::Sleepable,
            pin: u32,
            high: bool,
        ) -> Result<(), bus::provider::ProviderError> {
            PIN_CALLS
                .lock()
                .push((if high { "high" } else { "low" }, pin));
            Ok(())
        }
        fn release(&self, _: &sched::Sleepable, pin: u32) {
            PIN_CALLS.lock().push(("release", pin));
        }
        fn interrupts(&self) -> Option<&'static dyn ::irq::IrqDomain> {
            Some(self.0)
        }
    }

    impl bus::i2c::I2c for PinCtl {
        fn transfer(
            &self,
            _: &sched::Sleepable,
            msgs: &mut [bus::i2c::Msg<'_>],
        ) -> Result<(), bus::provider::ProviderError> {
            for m in msgs.iter_mut() {
                PIN_CALLS.lock().push(("i2c", u32::from(m.address)));
                if let bus::i2c::Op::Read(b) = &mut m.op {
                    b.fill(0x5a);
                }
            }
            Ok(())
        }
        fn speed(&self) -> u32 {
            100_000
        }
    }

    /// The controller: a GPIO provider with a cascaded domain, and an I2C
    /// bus.
    pub(crate) struct PinController;

    impl Driver for PinController {
        const NAME: &'static str = "pinctl";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 21,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Test GPIO and I2C controller".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            let c = ::irq::cascade::CascadeDomain::new(alloc::sync::Arc::new(PinCtl))
                .map_err(|e| AttachError::Resource(bus::ResourceError::Irq(e)))?;
            *PIN_CASCADE.lock() = Some(c);
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            let domain = PIN_CASCADE.lock().as_ref().map(|c| c.domain());
            let domain = domain.ok_or(AttachError::Device("no domain"))?;
            cx.provide_gpio(alloc::sync::Arc::new(PinGpio(domain)));
            cx.provide_i2c(alloc::sync::Arc::new(PinCtl));
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
            let gone = PIN_CASCADE.lock().take();
            drop(gone);
        }
    }

    /// The device: its interrupt, its two output pins, and its I2C
    /// connection.
    pub(crate) struct PinUser {
        pins: bus::gpio::GpioPins,
        irq: sync::SpinLock<Option<bus::IrqLine>>,
    }

    fn pin_filter(_: &PinUser, _: usize, _: &sched::IrqCx) -> ::irq::Filter {
        PIN_IRQS.fetch_add(1, Ordering::SeqCst);
        ::irq::Filter::Handled
    }

    impl Driver for PinUser {
        const NAME: &'static str = "pinuser";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 22,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "GPIO and I2C test device".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            let s = cx.sleepable();
            let p = |e| AttachError::Resource(bus::ResourceError::Provider(e));
            let irq = cx.gpio_interrupt(0)?;
            assert_eq!(
                cx.gpio_interrupt(1).err(),
                Some(bus::ResourceError::NoSuchLine)
            );
            let pins = cx.gpio(0)?;
            pins.configure(
                s,
                1,
                bus::gpio::PinMode::Output { high: true },
                bus::gpio::Pull::Default,
            )
            .map_err(p)?;
            assert_eq!(
                pins.get(s, 0),
                Err(bus::provider::ProviderError::Unsupported)
            );
            let dev = cx.i2c(0)?;
            let mut id = [0u8; 2];
            dev.write_read(s, &[0x0f], &mut id).map_err(p)?;
            assert_eq!(id, [0x5a, 0x5a]);
            Ok(Self {
                pins,
                irq: sync::SpinLock::new(Some(irq)),
            })
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            let line = self.irq.lock().take();
            if let Some(l) = line {
                cx.interrupt(l, Some(pin_filter), None, 0)?;
            }
            let _ = self.pins.len();
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    pub(crate) const PIN_BUS: DriverDecl = DriverDecl::of::<PinBus>();
    pub(crate) const PIN_CONTROLLER: DriverDecl = DriverDecl::of::<PinController>();
    pub(crate) const PIN_USER: DriverDecl = DriverDecl::of::<PinUser>();

    /// The ACPI path of the I2C test device the bus takes over.
    pub(crate) const IIC_DEV_PATH: &str = "\\_SB_.TI2C.DEV0";
    /// The path of one that attaches where it is, and stays.
    pub(crate) const IIC_ATT_PATH: &str = "\\_SB_.TI2C.DEV1";
    /// What the I2C test device read at attach: its EEPROM's byte 0.
    pub(crate) static IIC_DEV_READ: sync::SpinLock<Option<u8>> = sync::SpinLock::new(None);
    /// What the taker's two `take_child` calls returned.
    pub(crate) static IIC_TAKEN: sync::SpinLock<(Option<bus::DeviceId>, Option<bus::DeviceId>)> =
        sync::SpinLock::new((None, None));

    fn acpi_ident(hid: &str) -> devtree::ids::AcpiIdent {
        devtree::ids::AcpiIdent {
            hid: Some(hid.into()),
            uid: Some("0".into()),
            ..devtree::ids::AcpiIdent::default()
        }
    }

    /// A bus standing in for `acpi0` on a machine with an I2C bus: an I2C
    /// controller (model 31), a device whose `I2cSerialBusV2` names it at
    /// 0x50 (it waits on the controller), and a device a driver takes
    /// where it is.
    pub(crate) struct IicTestBus;

    impl Driver for IicTestBus {
        const NAME: &'static str = "iictestbus";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 30,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "I2C test firmware bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            use bus::{Line, LineDetail, LineKind};
            use devtree::graph::{EdgeKind::Required, Origin::ResourceSource};
            let ctl = cx.add_child(Ident::Test(TestIdent { model: 31 }), None, vec![]);
            let dev = cx.add_child(
                Ident::Acpi(acpi_ident("TSTI2C01"), IIC_DEV_PATH.into()),
                None,
                vec![],
            );
            cx.add_child(
                Ident::Acpi(acpi_ident("TSTATT01"), IIC_ATT_PATH.into()),
                None,
                vec![],
            );
            cx.add_dependency(dev, ctl, Required, ResourceSource);
            cx.set_lines(
                dev,
                vec![Line {
                    kind: LineKind::I2c,
                    name: None,
                    supplier: ctl,
                    id: 0x50,
                    detail: LineDetail::I2c(bus::i2c::I2cTarget {
                        address: 0x50,
                        ten_bit: false,
                        speed: 100_000,
                    }),
                }],
            );
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// A 256-byte EEPROM at 0x50 (a write sets the pointer from its first
    /// byte and stores the rest; a read reads on), bytes 0 to 255 at
    /// first. Nothing else answers.
    struct Eeprom(sync::SpinLock<([u8; 256], u8)>);

    impl bus::i2c::I2c for Eeprom {
        fn transfer(
            &self,
            _: &sched::Sleepable,
            msgs: &mut [bus::i2c::Msg<'_>],
        ) -> Result<(), bus::provider::ProviderError> {
            let mut m = self.0.lock();
            for x in msgs.iter_mut() {
                if x.address != 0x50 {
                    return Err(bus::provider::ProviderError::NoAck);
                }
                match &mut x.op {
                    bus::i2c::Op::Write(b) => {
                        if let Some((&p, rest)) = b.split_first() {
                            m.1 = p;
                            for &v in rest {
                                let i = usize::from(m.1);
                                m.0[i] = v;
                                m.1 = m.1.wrapping_add(1);
                            }
                        }
                    }
                    bus::i2c::Op::Read(b) => {
                        for v in b.iter_mut() {
                            *v = m.0[usize::from(m.1)];
                            m.1 = m.1.wrapping_add(1);
                        }
                    }
                }
            }
            Ok(())
        }
        fn speed(&self) -> u32 {
            100_000
        }
    }

    /// The controller: the EEPROM's bus, its `iicbus`, and a bus taking
    /// over the device on it (as `acpi_iicbus` does, with no namespace
    /// to walk here).
    pub(crate) struct IicTestCtl;

    impl Driver for IicTestCtl {
        const NAME: &'static str = "iictestctl";
        const IDS: Ids = Ids::Test(&[TestId {
            model: 31,
            rank: Rank::DEFAULT,
        }]);
        fn describe(_: &Ident) -> String {
            "Test I2C controller".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            let mut mem = [0u8; 256];
            for (i, b) in mem.iter_mut().enumerate() {
                *b = u8::try_from(i).unwrap_or(0);
            }
            cx.provide_i2c(alloc::sync::Arc::new(Eeprom(sync::SpinLock::new((mem, 0)))));
            cx.add_iicbus();
            cx.add_child(
                Ident::Child {
                    class: "iictaker",
                    desc: "I2C takeover test bus",
                },
                None,
                vec![],
            );
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Takes the device on the controller's bus from the firmware bus,
    /// and tries to take the one attached there.
    pub(crate) struct IicTaker;

    impl Driver for IicTaker {
        const NAME: &'static str = "iictaker";
        const IDS: Ids = Ids::Nexus(&["iictaker"]);
        fn describe(_: &Ident) -> String {
            "I2C takeover test bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
            let make = |old: &Ident| match old {
                Ident::Acpi(i, p) => Some(Ident::Iic(bus::IicIdent {
                    addr: 0x50,
                    acpi: Some((i.clone(), p.clone())),
                })),
                _ => None,
            };
            let a = cx.take_child(IIC_DEV_PATH, "iictestbus", &make);
            let b = cx.take_child(IIC_ATT_PATH, "iictestbus", &make);
            *IIC_TAKEN.lock() = (a, b);
            Ok(())
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// A driver for the I2C device on `iicbus`: it reads the EEPROM's
    /// first byte through the line its firmware gave it.
    pub(crate) struct IicTestDev;

    impl Driver for IicTestDev {
        const NAME: &'static str = "iictestdev";
        const IDS: Ids = Ids::IicAcpi(&[devtree::ids::AcpiId::new("TSTI2C01")]);
        fn describe(_: &Ident) -> String {
            "Test I2C device".into()
        }
        fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            let s = cx.sleepable();
            let dev = cx.i2c(0)?;
            let mut b = [0xffu8];
            dev.write_read(s, &[0x00], &mut b)
                .map_err(|e| AttachError::Resource(bus::ResourceError::Provider(e)))?;
            *IIC_DEV_READ.lock() = Some(b[0]);
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    /// Attaches the device the firmware bus keeps.
    pub(crate) struct IicStays;

    impl Driver for IicStays {
        const NAME: &'static str = "iicstays";
        const IDS: Ids = Ids::Acpi(&[devtree::ids::AcpiId::new("TSTATT01")]);
        fn describe(_: &Ident) -> String {
            "Test device attached on the firmware bus".into()
        }
        fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
            Ok(Self)
        }
        fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
    }

    pub(crate) const IIC_TEST_BUS: DriverDecl = DriverDecl::of::<IicTestBus>();
    pub(crate) const IIC_TEST_CTL: DriverDecl = DriverDecl::of::<IicTestCtl>();
    pub(crate) const IIC_TAKER: DriverDecl = DriverDecl::of::<IicTaker>();
    pub(crate) const IIC_TEST_DEV: DriverDecl = DriverDecl::of::<IicTestDev>();
    pub(crate) const IIC_STAYS: DriverDecl = DriverDecl::of::<IicStays>();
}

ktest! {
    /// An I2C controller's `iicbus` and `iic` (`/dev/iicN`), and a bus
    /// taking over a firmware device on it, which keeps its line to the
    /// controller; then `tests/user/iic.c` and FreeBSD's `i2c(8)` on the
    /// node.
    fn iicbus_and_iic_on_a_controller() {
        use bus::{Ident, NodeState, TestIdent};
        let s = cx();
        let m = bus::manager();
        *bus_test::IIC_DEV_READ.lock() = None;
        let tb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 30 }), None, alloc::vec![], &[]);
        m.wait_idle(&s);
        let kids = m.children(tb, &s);
        assert_eq!(kids.len(), 2, "the device on the I2C bus left the firmware bus");
        let (ctl, stays) = (kids[0], kids[1]);
        assert_eq!(m.state(stays, &s), Some(NodeState::Attached));
        let (taken, kept) = *bus_test::IIC_TAKEN.lock();
        assert_eq!(kept, None, "an attached device is not taken");
        let ck = m.children(ctl, &s);
        assert_eq!(ck.len(), 2);
        let (iicbus, taker) = (ck[0], ck[1]);
        assert_eq!(m.state(iicbus, &s), Some(NodeState::Attached));
        assert!(m.nameunit(iicbus, &s).is_some_and(|n| n.starts_with("iicbus")));
        assert_eq!(m.description(iicbus, &s).as_deref(), Some("Philips I2C bus"));
        let ik = m.children(iicbus, &s);
        assert_eq!(ik.len(), 1);
        let iic = ik[0];
        assert_eq!(m.state(iic, &s), Some(NodeState::Attached));
        assert_eq!(m.description(iic, &s).as_deref(), Some("I2C generic I/O"));
        let name = m.nameunit(iic, &s).expect("iic named");
        let mut loc = alloc::string::String::new();
        m.ident(iic, &s).expect("ident").location(&mut loc);
        assert_eq!(loc, "addr=0");
        // The device taken over: under the taker, matched as an I2C
        // device, its line to the controller intact.
        let dev = m.children(taker, &s)[0];
        assert_eq!(Some(dev), taken);
        assert_eq!(m.state(dev, &s), Some(NodeState::Attached));
        assert_eq!(*bus_test::IIC_DEV_READ.lock(), Some(0));
        let ident = m.ident(dev, &s).expect("ident");
        let (mut loc, mut pnp) = (alloc::string::String::new(), alloc::string::String::new());
        ident.location(&mut loc);
        ident.pnpinfo(&mut pnp);
        assert_eq!(loc, "addr=0x50 handle=\\_SB_.TI2C.DEV0");
        assert_eq!(pnp, "_HID=TSTI2C01 _UID=0 _CID=none");
        // The node and FreeBSD's tool on it.
        let path = alloc::format!("/dev/{name}");
        let prog: &[u8] = b"/usr/tests/abyss/iic";
        if crate::md::archive().is_some() {
            let p = crate::proc::Process::spawn(&s, prog, &[b"iic", path.as_bytes()], &[], None)
                .expect("spawn iic");
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(0), "tests/user/iic.c on {path}");
        }
        // Detaching the controller takes its bus's children with it.
        m.shutdown_under(tb, &s);
        assert_eq!(m.state(iic, &s), None);
        assert_eq!(m.state(dev, &s), None);
        let _ = m.take_events();
    }
}

ktest! {
    fn gpio_and_i2c_consumers_reach_their_controller() {
        use bus::{DetachReason, Ident, NodeState, TestIdent};
        let s = cx();
        let m = bus::manager();
        bus_test::PIN_CALLS.lock().clear();
        let pb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 20 }), None, alloc::vec![], &[]);
        m.wait_idle(&s);
        let kids = m.children(pb, &s);
        let (user, ctl) = (kids[0], kids[1]);
        for c in [user, ctl] {
            assert_eq!(m.state(c, &s), Some(NodeState::Attached));
        }
        // The interrupt pin made an input, the outputs driven, a register
        // read as one transfer, then the interrupt hooked in `start`.
        assert_eq!(
            *bus_test::PIN_CALLS.lock(),
            [("input", 4), ("output", 2), ("i2c", 0x32), ("i2c", 0x32), ("trigger", 4)]
        );
        // The controller's filter finds pin 4 pending.
        let before = bus_test::PIN_IRQS.load(Ordering::SeqCst);
        let fire = sched::spawn_on("ktest-fire", sched::Priority::KERNEL, Some(hal::CpuId::BOOT), move |_| {
            let irq = hal::irq::disable();
            // SAFETY: interrupts are masked for as long as the token is
            // used, as in the dispatcher.
            let cx = unsafe { sched::IrqCx::assume() };
            let c = bus_test::PIN_CASCADE.lock();
            let c = c.as_ref().expect("controller attached");
            assert!(c.dispatch(4, &cx), "pin 4 has a handler");
            assert!(!c.dispatch(5, &cx), "pin 5 has none");
            hal::irq::restore(irq);
        })
        .expect("spawn");
        fire.join(&s);
        assert_eq!(bus_test::PIN_IRQS.load(Ordering::SeqCst), before + 1);
        // Detaching the controller detaches the device first: its handler
        // goes, then its pins.
        bus_test::PIN_CALLS.lock().clear();
        m.detach(ctl, DetachReason::Requested, &s).expect("detach");
        assert_eq!(m.state(user, &s), Some(NodeState::NotPresent));
        let calls = bus_test::PIN_CALLS.lock().clone();
        assert_eq!(calls, [("unhook", 4), ("release", 4), ("release", 1), ("release", 2)]);
        m.shutdown_under(pb, &s);
        let _ = m.take_events();
    }
}

ktest! {
    fn companion_dependencies_hold_the_node() {
        use bus::{Ident, NodeState, TestIdent};
        let s = cx();
        let m = bus::manager();
        bus_test::COMPANION_ORDER.lock().clear();
        let cb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 16 }), None, alloc::vec![], &[]);
        m.wait_idle(&s);
        for c in m.children(cb, &s) {
            assert_eq!(m.state(c, &s), Some(NodeState::Attached));
        }
        let order = bus_test::COMPANION_ORDER.lock().clone();
        assert_eq!(order.first(), Some(&"supplier"), "{order:?}");
        assert_eq!(order.len(), 3, "{order:?}");
        m.shutdown_under(cb, &s);
        let _ = m.take_events();
    }
}

ktest! {
    fn shared_ranges_are_granted_knowingly() {
        use bus::{NodeState, ResourceError, Sharing, TestIdent};
        let s = cx();
        let m = bus::manager();
        let pages: alloc::vec::Vec<addr::PhysAddr> =
            (0..2).map(|_| crate::mm::alloc_pages(vm::Order::ZERO).expect("page")).collect();
        for (slot, pa) in bus_test::SHARED_PAGES.iter().zip(&pages) {
            slot.store(pa.get(), Ordering::Relaxed);
        }
        bus_test::SHARED_GRANTS.lock().clear();
        let claims = m.claims();
        let sb = m.add_child(&s, m.root(), bus::Ident::Test(TestIdent { model: 18 }), None, alloc::vec![], &[]);
        m.wait_idle(&s);
        let mut got = bus_test::SHARED_GRANTS.lock().clone();
        got.sort_by_key(|g| g.0);
        let (firmware, declared) = (
            Sharing { firmware: true, declared: false },
            Sharing { firmware: false, declared: true },
        );
        // AML's range is refused until asked for knowingly; both sides of
        // the declared overlap get it; past the declaration, the second
        // driver is refused.
        assert_eq!(got[..2], [(1, Err(ResourceError::FirmwareShared)), (1, Ok(firmware))]);
        assert_eq!(got[2..4], [(2, Ok(declared)), (3, Ok(declared))]);
        assert_eq!(got[4..], [(4, Err(ResourceError::Claimed))]);
        let kids = m.children(sb, &s);
        let attached = kids.iter().filter(|&&c| m.state(c, &s) == Some(NodeState::Attached)).count();
        assert_eq!(attached, 3, "{got:?}");
        assert_eq!(m.claims(), claims + 3);
        // The bus's records go with it.
        m.shutdown_under(sb, &s);
        assert_eq!(m.claims(), claims);
        let (f, d) = m.shared_ranges();
        let ours = |r: &bus::Range| pages.iter().any(|p| r.base >> 12 == p.get() >> 12);
        assert!(!f.iter().chain(&d).any(ours), "{f:?} {d:?}");
        let _ = m.take_events();
        for pa in pages {
            crate::mm::free_pages(pa, vm::Order::ZERO);
        }
    }
}

ktest! {
    fn providers_serve_lines_by_name() {
        use bus::{DetachReason, Ident, NodeState, TestIdent};
        let s = cx();
        let m = bus::manager();
        bus_test::PROVIDER_CALLS.lock().clear();
        let lb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 13 }), None, alloc::vec![], &[]);
        m.wait_idle(&s);
        let kids = m.children(lb, &s);
        assert_eq!(kids.len(), 2);
        let (user, prov) = (kids[0], kids[1]);
        for c in [user, prov] {
            assert_eq!(m.state(c, &s), Some(NodeState::Attached));
        }
        assert_eq!(
            *bus_test::PROVIDER_CALLS.lock(),
            [("set_rate", 7), ("enable", 7), ("deassert", 3)]
        );
        // Detaching the provider detaches its consumer first, whose
        // handles gate the clock and leave the reset line as it is.
        m.detach(prov, DetachReason::Requested, &s).expect("detach");
        assert_eq!(m.state(user, &s), Some(NodeState::NotPresent));
        assert_eq!(
            *bus_test::PROVIDER_CALLS.lock(),
            [("set_rate", 7), ("enable", 7), ("deassert", 3), ("disable", 7)]
        );
        m.shutdown_under(lb, &s);
        let _ = m.take_events();
    }
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
        // Settle edges: a supplier that failed, one nothing matched, and
        // one that attached each let their consumer go, the last only
        // once it had attached.
        let _ = (at(23), at(24), at(125));
        assert!(!order.contains(&225), "model 25 attached before its supplier: {order:?}");
        for c in m.children(db, &s) {
            let want = match m.ident(c, &s) {
                Some(Ident::Test(TestIdent { model: 5 })) => {
                    assert!(matches!(m.state(c, &s), Some(NodeState::Failed(_))));
                    continue;
                }
                Some(Ident::Test(TestIdent { model: 98 })) => NodeState::NotPresent,
                _ => NodeState::Attached,
            };
            assert_eq!(m.state(c, &s), Some(want));
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

/// A `dev.*` string node's text, if the node exists.
fn dev_text(s: &sched::Sleepable, name: &str) -> Option<alloc::string::String> {
    let oid = crate::sysctl::read(s, |t| t.name2oid(name))?;
    let mut v = alloc::vec::Vec::new();
    crate::sysctl::read_oid(s, &oid, &mut v).ok()?;
    let text = v.strip_suffix(&[0]).unwrap_or(&v);
    alloc::string::String::from_utf8(text.to_vec()).ok()
}

/// `dev.<class>.<unit>` of a node named `name` (`virtio_pci0`).
fn dev_prefix(name: &str) -> alloc::string::String {
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (class, unit) = name.split_at(name.len() - digits);
    alloc::format!("dev.{class}.{unit}")
}

/// The device names `devinfo` reads (`hw.bus.devices`), in record order.
fn devinfo_names(s: &sched::Sleepable) -> alloc::vec::Vec<alloc::string::String> {
    let oid = |name: &str| {
        crate::sysctl::read(s, |t| t.name2oid(name)).unwrap_or_else(|| panic!("{name}"))
    };
    let mut info = alloc::vec::Vec::new();
    crate::sysctl::read_oid(s, &oid("hw.bus.info"), &mut info).unwrap();
    let generation = i32::from_ne_bytes(info[4..8].try_into().unwrap());
    let devices = oid("hw.bus.devices");
    let mut names = alloc::vec::Vec::new();
    for index in 0.. {
        let mut o = devices.clone();
        o.extend([generation, index]);
        let mut b = alloc::vec::Vec::new();
        if crate::sysctl::read_oid(s, &o, &mut b).is_err() {
            break;
        }
        let name: alloc::vec::Vec<u8> = b[28..].iter().copied().take_while(|&c| c != 0).collect();
        names.push(alloc::string::String::from_utf8(name).unwrap());
    }
    names
}

ktest! {
    /// `%iommu` as FreeBSD's `device_sysctl_handler` shows it on both
    /// ISAs (docs/IOMMU.md §4.2): `rid=` and the requester ID for every
    /// PCI function, empty for everything else. QEMU's functions are all
    /// seen by their own ID: on a root bus, or PCIe behind a root port
    /// (as in the bhyve corpus, `dev.ahci.1.%iommu: rid=0x48`).
    fn iommu_rid_strings() {
        use bus::{Ident, NodeState};
        let s = cx();
        let m = bus::manager();
        let mut pci = 0;
        for (id, _, name, _, state) in m.snapshot(&s) {
            if state != NodeState::Attached || name.is_empty() || name == "root0" {
                continue;
            }
            let Some(got) = dev_text(&s, &alloc::format!("{}.%iommu", dev_prefix(&name))) else {
                continue;
            };
            let want = match m.ident(id, &s) {
                Some(Ident::Pci(_, a, _)) => {
                    pci += 1;
                    let mut w = alloc::string::String::new();
                    devtree::strings::iommu_rid(&mut w, Some(pci::requester::rid(a))).unwrap();
                    assert_eq!(m.requester_id(&s, id), Some(pci::requester::rid(a)), "{name}");
                    w
                }
                _ => alloc::string::String::new(),
            };
            assert_eq!(got, want, "{name}.%iommu");
        }
        assert!(pci >= 2, "{pci} PCI functions attached");
    }
}

ktest! {
    /// The IORT's SMMUs under `acpi0` (docs/IOMMU.md §4.2, stage 0): one
    /// `smmu` per SMMUv3 node, attached, FreeBSD's strings, `dev.smmu.N.dma`
    /// 1 when it translates (stage 3: `hw.iommu.dma`, with message-signalled
    /// interrupts off); the host bridges they translate waited for them
    /// and record the unit. None without an IORT SMMU (x86, plain `virt`).
    fn smmu_units_under_acpi0() {
        use bus::NodeState;
        let s = cx();
        let m = bus::manager();
        let listed = crate::acpi::find(*b"IORT")
            .and_then(|t| acpi_tables::iort::parse(&t).ok())
            .map_or(0, |t| t.smmu_v3s().count());
        let snap = m.snapshot(&s);
        let units: alloc::vec::Vec<_> = snap
            .iter()
            .filter(|n| n.2.starts_with("smmu"))
            .collect();
        assert_eq!(units.len(), listed, "{units:?}");
        let devinfo = devinfo_names(&s);
        for (k, (id, _, name, desc, state)) in units.iter().enumerate() {
            assert_eq!(name, &alloc::format!("smmu{k}"));
            assert!(devinfo.contains(name), "{name} not in devinfo's {devinfo:?}");
            assert_eq!(desc, "ARM System Memory Management Unit");
            assert_eq!(*state, NodeState::Attached);
            let parent = m.parent(*id, &s).and_then(|p| m.nameunit(p, &s));
            assert_eq!(parent.as_deref(), Some("acpi0"));
            let p = dev_prefix(name);
            assert_eq!(dev_text(&s, &alloc::format!("{p}.%pnpinfo")).as_deref(), Some("unknown"));
            assert_eq!(dev_text(&s, &alloc::format!("{p}.%location")).as_deref(), Some(""));
            assert_eq!(dev_text(&s, &alloc::format!("{p}.%iommu")).as_deref(), Some(""));
            let dma = crate::sysctl::read(&s, |t| t.name2oid(&alloc::format!("{p}.dma")))
                .expect("dev.smmu.N.dma");
            let mut v = alloc::vec::Vec::new();
            crate::sysctl::read_oid(&s, &dma, &mut v).unwrap();
            let translates = i32::from(iommu_framework::dma());
            assert_eq!(v, translates.to_ne_bytes(), "{p}.dma");
        }
        if listed == 0 {
            return;
        }
        // Every PCI host bridge records smmu0 as its unit (QEMU's virt and
        // the Sky1 have one root complex, behind smmu0).
        let smmu0 = units[0].0;
        let bridges: alloc::vec::Vec<_> = snap
            .iter()
            .filter(|n| matches!(m.ident(n.0, &s), Some(bus::Ident::Acpi(i, _)) if i.hid.as_deref() == Some("PNP0A08")))
            .collect();
        assert!(!bridges.is_empty());
        for b in bridges {
            let tr = m.dma_translation(&s, b.0).expect("the host bridge's translation");
            assert_eq!(tr.unit(), smmu0, "{}", b.2);
            assert_eq!(b.4, NodeState::Attached, "{}", b.2);
        }
        // A function inherits it, and its stream is its requester ID.
        let f = snap
            .iter()
            .find_map(|n| match m.ident(n.0, &s) {
                Some(bus::Ident::Pci(_, a, _)) if a.bus <= 1 => Some((n.0, a)),
                _ => None,
            })
            .expect("a PCI function on bus 0 or 1");
        let tr = m.dma_translation(&s, f.0).expect("inherited");
        assert_eq!(tr.stream(pci::requester::rid(f.1)), Some(u32::from(pci::requester::rid(f.1))));
        // The sysctls (FreeBSD's static ones, arm64).
        for knob in ["hw.iommu.relaxed_ordering", "hw.iommu.check_free", "hw.smmu.bypass_named"] {
            let oid = crate::sysctl::read(&s, |t| t.name2oid(knob)).unwrap_or_else(|| panic!("{knob}"));
            let mut v = alloc::vec::Vec::new();
            crate::sysctl::read_oid(&s, &oid, &mut v).unwrap();
            assert_eq!(v, 0i32.to_ne_bytes(), "{knob}");
        }
    }
}

/// The SMMU's tests (arm64's: `smmu` is in its driver table only).
#[cfg(target_arch = "aarch64")]
mod smmu_running {
    use super::*;

    ktest! {
        /// The SMMU running (docs/IOMMU.md §8, stage 2). Every PCI function
        /// whose driver asked for DMA has a pass-through context: its stream
        /// bypasses. A stream nobody claimed aborts: QEMU's `edu` device (the
        /// SMMU cases add one; no driver takes it) is the idle device whose
        /// DMA engine the test drives. Bypassed, its DMA reaches memory, both
        /// ways; invalid (its state with no context), its write is stopped
        /// and the unit logs FreeBSD's `C_BAD_STE` event for its stream; a
        /// config-abort STE stops it without an event (IHI 0070).
        fn smmu_streams_bypass_and_abort() {
            use bus::{Ident, NodeState};
            use smmu::StreamState;
            let s = cx();
            let m = bus::manager();
            if !smmu::is_running(0) {
                return;
            }
            let smmu0 = m.snapshot(&s).into_iter().find(|n| n.2 == "smmu0").expect("smmu0").0;
            let mut edu = None;
            let mut bypassed = 0;
            for (id, _, name, _, state) in m.snapshot(&s) {
                let Some(Ident::Pci(ident, a, _)) = m.ident(id, &s) else {
                    continue;
                };
                let Some(tr) = m.dma_translation(&s, id) else {
                    continue;
                };
                if tr.unit() != smmu0 {
                    continue;
                }
                let Some(sid) = tr.stream(pci::requester::rid(a)) else {
                    continue;
                };
                if (ident.vendor, ident.device) == (0x1234, 0x11e8) {
                    edu = Some((a, sid));
                    continue;
                }
                // The storage and virtio functions do DMA: each got its
                // context through its tag (or its MSI-X): translated when
                // the unit translates (stage 3), bypassed otherwise.
                let dma = ["nvme", "ahci", "virtio_pci", "xhci"].iter().any(|d| name.starts_with(d));
                if state == NodeState::Attached && dma {
                    let want = if smmu::translates(0) {
                        StreamState::Translated
                    } else {
                        StreamState::Bypass
                    };
                    assert_eq!(smmu::stream(0, sid), Some(want), "{name} (stream {sid:#x})");
                    bypassed += 1;
                }
            }
            assert!(bypassed >= 2, "{bypassed} functions with contexts");
            let Some((a, sid)) = edu else {
                kprintln!("smmu: no edu device; the abort half is the aarch64-smmu case's");
                return;
            };
            assert_eq!(smmu::stream(0, sid), Some(StreamState::Invalid), "edu has no context");
            // Its registers, memory decoding and bus mastering on.
            let cfg = crate::pci::config_for(a).expect("edu's config space");
            let command = cfg.read16(a, pci::header::COMMAND);
            cfg.write16(a, pci::header::COMMAND, command | 0x6);
            let bar = cfg.read32(a, 0x10);
            let mut base = u64::from(bar & !0xf);
            if (bar >> 1) & 3 == 2 {
                base |= u64::from(cfg.read32(a, 0x14)) << 32;
            }
            let regs = crate::mm::map_device(addr::PhysAddr::new(base), 0x100).expect("edu's BAR");
            let reg = |off: u64| (regs.get() + off) as *mut u64;
            // SAFETY: edu's register page, mapped above, unmapped below; the
            // identification register is 32 bits.
            let r32 = |off: u64| unsafe { (reg(off) as *const u32).read_volatile() };
            // SAFETY: as above; the DMA registers take 64-bit accesses.
            let w64 = |off: u64, v: u64| unsafe { reg(off).write_volatile(v) };
            assert_eq!(r32(0), 0x0100_00ed, "edu's identification");
            // Its buffer is at 0x40000 in its own space; a transfer runs on a
            // 100 ms timer.
            const BUF: u64 = 0x4_0000;
            let dma = |src: u64, dst: u64, to_ram: bool| {
                w64(0x80, src);
                w64(0x88, dst);
                w64(0x90, 64);
                w64(0x98, 1 | if to_ram { 2 } else { 0 });
                for _ in 0..200 {
                    if r32(0x98) & 1 == 0 {
                        return;
                    }
                    crate::time::sleep_ns(&s, 10_000_000);
                }
                panic!("edu's DMA did not finish");
            };
            let pages: alloc::vec::Vec<addr::PhysAddr> =
                (0..2).map(|_| crate::mm::alloc_pages(vm::Order::ZERO).expect("page")).collect();
            let bytes = |pa: addr::PhysAddr| bootproto::layout::dmap(pa).unwrap().get() as *mut u8;
            let read = |pa| {
                let mut b = [0u8; 64];
                // SAFETY: a page this test owns, through the direct map; the
                // device wrote it, or not, while the test waited.
                unsafe { core::ptr::copy_nonoverlapping(bytes(pa), b.as_mut_ptr(), 64) };
                b
            };
            // SAFETY: as above.
            let clear = |pa| unsafe { core::ptr::write_bytes(bytes(pa), 0, 4096) };
            let pattern: [u8; 64] = core::array::from_fn(|i| 0xa5 ^ u8::try_from(i).unwrap_or(0));
            // SAFETY: as above.
            unsafe { core::ptr::copy_nonoverlapping(pattern.as_ptr(), bytes(pages[0]), 64) };
            clear(pages[1]);

            // Bypassed: memory to the device and back.
            smmu::set_stream(0, sid, StreamState::Bypass).expect("bypass");
            dma(pages[0].get(), BUF, false);
            dma(BUF, pages[1].get(), true);
            assert_eq!(read(pages[1]), pattern, "a bypassed stream reaches memory");

            // Invalid: the write is stopped, and the event names the stream.
            let before = smmu::stream_events(0, sid).total;
            smmu::set_stream(0, sid, StreamState::Invalid).expect("invalid");
            clear(pages[1]);
            dma(BUF, pages[1].get(), true);
            assert_eq!(read(pages[1]), [0; 64], "an invalid stream's write reached memory");
            let mut ev = smmu::stream_events(0, sid);
            for _ in 0..100 {
                if ev.total > before {
                    break;
                }
                crate::time::sleep_ns(&s, 10_000_000);
                ev = smmu::stream_events(0, sid);
            }
            assert!(ev.total > before, "no event for stream {sid:#x}");
            // QEMU reports each access of the transfer: wait for the last.
            loop {
                crate::time::sleep_ns(&s, 100_000_000);
                let now = smmu::stream_events(0, sid);
                if now.total == ev.total {
                    break;
                }
                ev = now;
            }
            let e = ev.last.expect("the event");
            assert_eq!(e.sid.0, sid);
            assert_eq!(e.record.name(), "C_BAD_STE", "{e:?}");

            // Config abort: stopped, silently.
            smmu::set_stream(0, sid, StreamState::Abort).expect("abort");
            let before = smmu::stream_events(0, sid).total;
            dma(BUF, pages[1].get(), true);
            crate::time::sleep_ns(&s, 100_000_000);
            assert_eq!(read(pages[1]), [0; 64], "an aborted stream's write reached memory");
            assert_eq!(smmu::stream_events(0, sid).total, before, "an abort STE raised an event");

            // Back as it was: no context, no bus mastering.
            smmu::set_stream(0, sid, StreamState::Invalid).expect("invalid");
            cfg.write16(a, pci::header::COMMAND, command);
            crate::mm::unmap_device(regs, 0x100);
            for pa in pages {
                crate::mm::free_pages(pa, vm::Order::ZERO);
            }
        }
    }

    ktest! {
        /// Translated DMA (docs/IOMMU.md §8, stage 3). Every function
        /// with DMA has a translated context: its event lines name it,
        /// and PCIe relaxed ordering is off. Then QEMU's `edu`, given a
        /// context by the broker as a driver's first tag would, moves
        /// memory through IOVAs: a mapped buffer reaches the device both
        /// ways; a DMA to an IOVA never mapped is stopped with a
        /// translation fault naming the device; and once a buffer is
        /// unmapped, its old IOVA faults and the page stays as the CPU
        /// left it (strict unmap).
        fn smmu_translates_dma() {
            use bus::iommu::{Context, Want};
            use bus::{Ident, NodeState};
            use smmu::StreamState;
            use vm::dma::{dir, Backend, DmaBuf, DmaConstraints, DmaTag};
            let s = cx();
            let m = bus::manager();
            if !smmu::translates(0) {
                kprintln!("smmu: not translating (hw.iommu.dma=0); stage 3's checks skipped");
                return;
            }
            let smmu0 = m.snapshot(&s).into_iter().find(|n| n.2 == "smmu0").expect("smmu0").0;
            let mut edu = None;
            let mut nvme = 0;
            for (id, _, name, _, state) in m.snapshot(&s) {
                let Some(Ident::Pci(ident, a, _)) = m.ident(id, &s) else {
                    continue;
                };
                let Some(sid) = m
                    .dma_translation(&s, id)
                    .filter(|t| t.unit() == smmu0)
                    .and_then(|t| t.stream(pci::requester::rid(a)))
                else {
                    continue;
                };
                if (ident.vendor, ident.device) == (0x1234, 0x11e8) {
                    edu = Some((id, a, sid));
                    continue;
                }
                if state != NodeState::Attached || !name.starts_with("nvme") {
                    continue;
                }
                // The §4.10 name, and relaxed ordering off (§4.4).
                assert_eq!(smmu::stream(0, sid), Some(StreamState::Translated), "{name}");
                assert_eq!(
                    smmu::stream_device(0, sid),
                    Some(alloc::format!("{name} (rid {:#x})", pci::requester::rid(a)))
                );
                let cfg = crate::pci::config_for(a).expect("config space");
                let caps = pci::caps::list(cfg, a);
                let pcie = pci::caps::find(&caps, pci::caps::id::PCIE).expect("PCIe");
                let devctl = cfg.read16(a, u16::from(pcie.offset) + 8);
                assert_eq!(devctl & (1 << 4), 0, "{name}: relaxed ordering on ({devctl:#x})");
                // Its messages go through its domain (§4.8): with MSI-X
                // on, each entry in use is written to the doorbell's IOVA,
                // at GITS_TRANSLATER's offset in a page that is not its.
                if let Some(translater) = crate::its::translater()
                    && let Some(msix) = pci::caps::find(&caps, pci::caps::id::MSIX)
                    && cfg.read16(a, u16::from(msix.offset) + 2) & (1 << 15) != 0
                {
                    let table = cfg.read32(a, u16::from(msix.offset) + 4);
                    let bar_at = 0x10 + 4 * u16::try_from(table & 7).unwrap_or(0);
                    let bar = cfg.read32(a, bar_at);
                    let mut base = u64::from(bar & !0xf);
                    if (bar >> 1) & 3 == 2 {
                        base |= u64::from(cfg.read32(a, bar_at + 4)) << 32;
                    }
                    let at = addr::PhysAddr::new(base + u64::from(table & !7));
                    let va = crate::mm::map_device(at, 16).expect("the MSI-X table");
                    let p = va.get() as *const u32;
                    // SAFETY: entry 0 of the table, mapped above and
                    // unmapped below; reads have no side effects.
                    let msg = unsafe {
                        u64::from(p.read_volatile()) | u64::from(p.add(1).read_volatile()) << 32
                    };
                    crate::mm::unmap_device(va, 16);
                    kprintln!("smmu: {name}: MSI-X entry 0 at {msg:#x} (GITS_TRANSLATER {translater:#x})");
                    assert_eq!(msg % 4096, translater % 4096, "{name}: {msg:#x}");
                    assert_ne!(msg & !4095, translater & !4095, "{name}: physical doorbell");
                }
                nvme += 1;
            }
            assert!(nvme >= 1, "no translated NVMe controller");
            let Some((node, a, sid)) = edu else {
                kprintln!("smmu: no edu device; the DMA half is the aarch64-smmu case's");
                return;
            };
            assert_eq!(smmu::stream(0, sid), Some(StreamState::Invalid), "edu has no context");
            // Out of the rate limit's window of the stream's earlier events
            // (the abort test's), so the unit prints this test's.
            crate::time::sleep_ns(&s, 1_100_000_000);
            // The broker's context, as for a driver's first tag.
            let Ok(Some(Context::Translated(domain))) = m.iommu_context(&s, node, Want::Translate)
            else {
                panic!("edu: no translated context");
            };
            assert_eq!(smmu::stream(0, sid), Some(StreamState::Translated));
            let label = smmu::stream_device(0, sid).expect("edu's label");
            assert_eq!(
                label,
                alloc::format!("pci0:{}:{}:{} (rid {:#x})", a.bus, a.slot, a.function, pci::requester::rid(a)),
                "a function no driver has is named by its selector"
            );
            let tag = DmaTag::translated(&crate::mm::dma::KERNEL_DMA, DmaConstraints::UNLIMITED, domain);
            assert_eq!(tag.backend(), Backend::Iommu);

            // edu's registers, memory decoding and bus mastering on.
            let cfg = crate::pci::config_for(a).expect("edu's config space");
            let command = cfg.read16(a, pci::header::COMMAND);
            cfg.write16(a, pci::header::COMMAND, command | 0x6);
            let bar = cfg.read32(a, 0x10);
            let mut base = u64::from(bar & !0xf);
            if (bar >> 1) & 3 == 2 {
                base |= u64::from(cfg.read32(a, 0x14)) << 32;
            }
            let regs = crate::mm::map_device(addr::PhysAddr::new(base), 0x100).expect("edu's BAR");
            let reg = |off: u64| (regs.get() + off) as *mut u64;
            // SAFETY: edu's register page, mapped above, unmapped below.
            let r32 = |off: u64| unsafe { (reg(off) as *const u32).read_volatile() };
            // SAFETY: as above; the DMA registers take 64-bit accesses.
            let w64 = |off: u64, v: u64| unsafe { reg(off).write_volatile(v) };
            const BUF: u64 = 0x4_0000;
            let dma = |src: u64, dst: u64, to_ram: bool| {
                w64(0x80, src);
                w64(0x88, dst);
                w64(0x90, 64);
                w64(0x98, 1 | if to_ram { 2 } else { 0 });
                for _ in 0..200 {
                    if r32(0x98) & 1 == 0 {
                        return;
                    }
                    crate::time::sleep_ns(&s, 10_000_000);
                }
                panic!("edu's DMA did not finish");
            };
            // Waits for the events of a faulting transfer to stop coming,
            // and returns the last.
            let last_event = |before: u64| {
                let mut ev = smmu::stream_events(0, sid);
                for _ in 0..100 {
                    if ev.total > before {
                        break;
                    }
                    crate::time::sleep_ns(&s, 10_000_000);
                    ev = smmu::stream_events(0, sid);
                }
                assert!(ev.total > before, "no event for stream {sid:#x}");
                loop {
                    crate::time::sleep_ns(&s, 100_000_000);
                    let now = smmu::stream_events(0, sid);
                    if now.total == ev.total {
                        break;
                    }
                    ev = now;
                }
                ev.last.expect("the event")
            };

            /// A page of the test's: payload only, as a buffer-cache page.
            struct Page(addr::PhysAddr);
            impl Page {
                fn get(&self) -> [u8; 64] {
                    let mut b = [0; 64];
                    // SAFETY: a page the test owns, through the direct
                    // map; the device writes it only while it is mapped
                    // and the test is waiting.
                    unsafe { core::ptr::copy_nonoverlapping(self.range(0).0, b.as_mut_ptr(), 64) };
                    b
                }
                fn put(&mut self, v: &[u8; 64]) {
                    // SAFETY: as above, and the CPU holds the page.
                    unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), self.range(0).0, 64) };
                }
            }
            // SAFETY: a page the test owns, through the direct map.
            unsafe impl DmaBuf for Page {
                fn range(&self, _: usize) -> (*mut u8, usize) {
                    (bootproto::layout::dmap(self.0).unwrap().get() as *mut u8, 4096)
                }
                fn payload_only(&self) -> bool {
                    true
                }
            }
            let pattern: [u8; 64] = core::array::from_fn(|i| 0x5a ^ u8::try_from(i).unwrap_or(0));
            let mut src = Page(crate::mm::alloc_pages(vm::Order::ZERO).expect("page"));
            let mut dst = Page(crate::mm::alloc_pages(vm::Order::ZERO).expect("page"));
            src.put(&pattern);
            dst.put(&[0; 64]);

            // Mapped: memory to the device and back, through IOVAs.
            let out = tag.map::<dir::ToDevice, _>(src).map_err(|(_, e)| e).expect("map");
            let inn = tag.map::<dir::FromDevice, _>(dst).map_err(|(_, e)| e).expect("map");
            let (o, i) = (out.segments()[0].bus, inn.segments()[0].bus);
            assert!(o < 1 << 32 && i < 1 << 32, "IOVAs {o:#x} {i:#x} are low");
            dma(o, BUF, false);
            dma(BUF, i, true);
            let src = out.unmap();
            let mut dst = inn.unmap();
            assert_eq!(dst.get(), pattern, "a translated transfer");

            // Unmap before reuse: the old IOVA is gone for the device.
            dst.put(&[0x11; 64]);
            let before = smmu::stream_events(0, sid).total;
            dma(BUF, i, true);
            assert_eq!(dst.get(), [0x11; 64], "a write through an unmapped IOVA");

            let e = last_event(before);
            assert_eq!(e.sid.0, sid);
            assert_eq!(e.record.name(), "F_TRANSLATION", "{e:?}");
            let input = e.record.input().expect("an address");
            assert_eq!(input.addr.get() & !0xfff, i & !0xfff);
            assert!(!input.access.read);

            // A deliberate fault: a read of an IOVA never mapped.
            let never = 0x7654_3000;
            let before = smmu::stream_events(0, sid).total;
            dma(never, BUF, false);
            let e = last_event(before);
            assert_eq!(e.record.name(), "F_TRANSLATION", "{e:?}");
            let input = e.record.input().expect("an address");
            assert_eq!((input.addr.get() & !0xfff, input.access.read), (never, true));
            kprintln!("smmu: {}", smmu::event_line(&label, &e));

            // Back as it was: no context, no bus mastering.
            drop(tag);
            smmu::release_context(0, sid).expect("release");
            assert_eq!(smmu::stream(0, sid), Some(StreamState::Invalid));
            cfg.write16(a, pci::header::COMMAND, command);
            crate::mm::unmap_device(regs, 0x100);
            crate::mm::free_pages(src.0, vm::Order::ZERO);
            crate::mm::free_pages(dst.0, vm::Order::ZERO);
        }
    }
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
            // Passed on, so the framebuffer is held while a function's
            // decoding is off, as in `pci::init`.
            fn decoding_paused(&self, a: pci::PciAddress, paused: bool) {
                self.0.decoding_paused(a, paused);
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
    fn processors_are_cpu_devices_numbered_as_their_cpus() {
        use bus::Ident;
        let s = cx();
        let m = bus::manager();
        m.wait_idle(&s);
        let acpi0 = m
            .children(crate::devices::nexus(), &s)
            .into_iter()
            .find(|&c| m.nameunit(c, &s).as_deref() == Some("acpi0"))
            .expect("acpi0 on nexus0");
        // ACPI0007 devices (arm64 QEMU) or Processor objects (q35).
        let mut units = alloc::vec::Vec::new();
        for c in m.children(acpi0, &s) {
            let Some(Ident::Acpi(i, path)) = m.ident(c, &s) else {
                continue;
            };
            if i.kind != devtree::ids::AcpiKind::Processor {
                continue;
            }
            let name = m.nameunit(c, &s).unwrap_or_default();
            kprintln!("acpi: {name} at {path}");
            assert_eq!(m.description(c, &s).as_deref(), Some("ACPI CPU"));
            let unit: u32 = name
                .strip_prefix("cpu")
                .and_then(|u| u.parse().ok())
                .unwrap_or_else(|| panic!("{name} is not a cpu"));
            units.push(unit);
        }
        units.sort_unstable();
        // bhyve's tables declare no processors, so FreeBSD has no `cpuN`
        // there either (tests/corpus/freebsd/bhyve/devinfo-rv.txt).
        let bhyve = m.description(acpi0, &s).as_deref() == Some("BHYVE BVFACP ");
        let cpus: alloc::vec::Vec<u32> = if bhyve {
            alloc::vec::Vec::new()
        } else {
            (0..u32::try_from(sched::online_cpus()).unwrap()).collect()
        };
        assert_eq!(units, cpus, "cpuN for each CPU N");
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
        // x86's `acpi_pcib_acpi`, arm64's `pci_host_generic_acpi`.
        let host = if cfg!(target_arch = "x86_64") {
            "ACPI Host-PCI bridge"
        } else {
            "Generic PCI host controller"
        };
        assert_eq!(m.description(pcib0, &s).as_deref(), Some(host));
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
            // devices than the boot test's), and with `--usb` the xHCI as
            // the USB recording has it.
            let recorded: alloc::vec::Vec<&str> =
                include_str!("../../tests/corpus/freebsd/bhyve/devinfo-rv.txt")
                    .lines()
                    .chain(
                        include_str!("../../tests/corpus/freebsd/usb/bhyve/devinfo-rv.txt")
                            .lines()
                            .filter(|l| l.contains("class=0x0c0330")),
                    )
                    .filter(|l| l.starts_with("        ") && !l.starts_with("         "))
                    .filter_map(|l| l.split_once(" pnpinfo ").map(|(_, rest)| rest))
                    .collect();
            for l in &lines {
                assert!(recorded.contains(&l.as_str()), "{l:?} not in FreeBSD's recording");
            }
            assert!(lines.iter().any(|l| l.ends_with("handle=\\_SB_.PC00.ISA_")), "isab's ACPI handle");
            // The virtio transports and the devices on them (vtblk0, vtrnd0,
            // vtblk1), line for line; with `--usb`, the xHCI and what is
            // under it as the USB recording has them.
            let full: alloc::vec::Vec<&str> =
                include_str!("../../tests/corpus/freebsd/bhyve/devinfo-rv.txt")
                    .lines()
                    .chain(include_str!("../../tests/corpus/freebsd/usb/bhyve/devinfo-rv.txt").lines())
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
        // In u128: a slow host makes the product overflow 64 bits.
        let ns = u64::try_from(
            u128::from(crate::time::counter() - start) * 1_000_000_000
                / u128::from(crate::time::frequency()),
        )
        .unwrap_or(u64::MAX);
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

/// `dev.nvme.0.<name>`, a 64-bit counter (or a 32-bit `u_int`).
fn nvme0_counter(s: &sched::Sleepable, name: &str) -> i64 {
    crate::sysctl::read(s, |t| {
        let oid = t.name2oid(&alloc::format!("dev.nvme.0.{name}"))?;
        let mut out = alloc::vec::Vec::new();
        t.read(&oid, &mut out).ok()?;
        match out.len() {
            4 => Some(i64::from(u32::from_ne_bytes(out[..4].try_into().ok()?))),
            8 => Some(i64::from_ne_bytes(out[..8].try_into().ok()?)),
            _ => None,
        }
    })
    .unwrap_or_else(|| panic!("dev.nvme.0.{name}"))
}

/// Faults injected into `nvme0` for the scope of a test, with a one-second
/// I/O timeout.
struct Nvme0Faults;

impl Nvme0Faults {
    fn new() -> Self {
        nvme::fault::target(Some(0));
        nvme::fault::timeout_ms(Some(1000));
        Self
    }
}

impl Drop for Nvme0Faults {
    fn drop(&mut self) {
        nvme::fault::target(None);
    }
}

/// Writes and reads back 8 KiB at `at` on `nda0` under the faults in
/// force; the milliseconds it took.
fn nvme0_round_trip(s: &sched::Sleepable, at: u64, fill: u8) -> u64 {
    let nda = storage::find("nda0").expect("nda0");
    let data = alloc::vec![fill; 8192];
    let start = crate::time::counter();
    nda.write(s, at, &data).expect("write");
    let back = nda.read(s, at, data.len()).expect("read back");
    assert!(
        back == data,
        "{} bytes at {at:#x} read back differ",
        data.len()
    );
    (crate::time::counter() - start) * 1000 / crate::time::frequency()
}

ktest! {
    fn nvme_timeout_polls_a_missed_interrupt() {
        // Interrupts do nothing: the watchdog finds each command past its
        // deadline, polls the queue (`nvme_qpair_timeout`'s last try),
        // and completes it, with no reset.
        let s = cx();
        let m = bus::manager();
        assert!(find_named(m, &s, crate::devices::nexus(), "nvme0").is_some());
        let _ = m.take_events();
        let (retries, cmds) = (nvme0_counter(&s, "num_retries"), nvme0_counter(&s, "num_cmds"));
        let ms = {
            let _faults = Nvme0Faults::new();
            nvme::fault::ignore_interrupts(true);
            nvme0_round_trip(&s, 2 << 20, 0x3c)
        };
        assert!(ms >= 1000, "completed in {ms} ms, before the timeout");
        let events = m.take_events();
        assert!(
            !events.iter().any(|e| e.contains("type=RESET")),
            "no reset: {events:?}"
        );
        assert_eq!(nvme0_counter(&s, "num_retries"), retries, "nothing sent again");
        assert!(nvme0_counter(&s, "num_cmds") >= cmds + 2);
        // Interrupts again: as fast as ever.
        let after = nvme0_round_trip(&s, 2 << 20, 0xc3);
        assert!(after < 1000, "{after} ms with interrupts back");
        kprintln!("nvme0: missed interrupts made up by the watchdog in {ms} ms");
    }
}

ktest! {
    fn nvme_timeout_resets_and_resends() {
        // A completion lost (the controller never answers, as far as the
        // driver can tell): the watchdog's poll finds nothing, so it
        // resets the controller, which sends the command again.
        let s = cx();
        let m = bus::manager();
        assert!(find_named(m, &s, crate::devices::nexus(), "nvme0").is_some());
        let _ = m.take_events();
        let retries = nvme0_counter(&s, "num_retries");
        let ms = {
            let _faults = Nvme0Faults::new();
            nvme::fault::lose_completions(1);
            nvme0_round_trip(&s, 3 << 20, 0x5a)
        };
        assert!(ms >= 1000, "completed in {ms} ms, before the timeout");
        let events = m.take_events();
        for want in ["event=\"start\"", "event=\"success\""] {
            assert!(
                events
                    .iter()
                    .any(|e| e.contains("system=nvme subsystem=controller type=RESET")
                        && e.contains("name=\"nvme0\"")
                        && e.contains(want)),
                "a RESET {want} event: {events:?}"
            );
        }
        assert!(nvme0_counter(&s, "num_retries") > retries, "the lost command was sent again");
        assert_eq!(nvme0_counter(&s, "ioq.0.recovery"), 0);
        // The controller works as before, and the whole disk too.
        let after = nvme0_round_trip(&s, 3 << 20, 0xa5);
        assert!(after < 1000, "{after} ms after the reset");
        exercise_disk(&s, &storage::find("nda0").expect("nda0"));
        kprintln!("nvme0: reset after a lost completion, I/O done in {ms} ms");
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
    fn free_pages() -> Pages {
        let h = crate::mm::heap_stats();
        Pages {
            free: crate::mm::phys_stats().free,
            slab: h.slab_pages,
            magazine: h.magazine_pages,
            large: h.large_pages,
            stacks: crate::mm::kstack::cached_pages(),
            kva: crate::mm::kstack::kva_allocated() / 4096,
            objects: crate::mm::space::OBJECT_PAGES.sum(core::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Where pages are, for naming what a leak took: the first five sum
    /// to the pages not in use; `kva` (kernel virtual memory in use, in
    /// pages) and `objects` (pages `vm` objects hold) say where the rest
    /// went.
    #[derive(Clone, Copy, Debug)]
    #[allow(dead_code, reason = "`kva` and `objects` are for the failure message")]
    struct Pages {
        free: u64,
        slab: u64,
        magazine: u64,
        large: u64,
        stacks: u64,
        kva: u64,
        objects: i64,
    }

    impl Pages {
        fn unused(&self) -> u64 {
            self.free + self.slab + self.magazine + self.large + self.stacks
        }
    }

    impl PartialEq for Pages {
        fn eq(&self, other: &Self) -> bool {
            self.unused() == other.unused()
        }
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
            let before = settled_free_pages(&s);
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
            // Every length the copy treats differently (words, the loop of
            // 32, `rep movsb`), across a page, both ways; and one byte past
            // the mapping faults however long the copy.
            let pattern: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
            for len in [1, 2, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 255, 256, 257, 1000] {
                let at = a + 6 * PAGE - (len as u64) / 2 - 1;
                copy_out(at, &pattern[..len]).unwrap();
                let mut back = alloc::vec![0u8; len];
                copy_in(&mut back, at).unwrap();
                assert_eq!(back, pattern[..len], "{len} bytes");
                let past = end - len as u64 + 1;
                assert_eq!(copy_in(&mut back, past), Err(Fault), "{len} bytes in");
                assert_eq!(copy_out(past, &pattern[..len]), Err(Fault), "{len} bytes out");
            }
            assert_eq!(copy_out(a + 6 * PAGE, &[]), Ok(()));
            // Read-only: writes fault, reads still work.
            space.protect(&s, VirtAddr::new(a)..VirtAddr::new(a + 4 * PAGE), Prot::READ, None).unwrap();
            assert_eq!(copy_out(a + 3 * PAGE, b"x"), Err(Fault));
            assert_eq!(read_u64(a + 3 * PAGE), Ok(0x1234_5678));
            // Unmapped: gone from the tables too.
            space.unmap(&s, VirtAddr::new(a + 3 * PAGE)..VirtAddr::new(a + 4 * PAGE)).unwrap();
            assert_eq!(read_u64(a + 3 * PAGE), Err(Fault));
            assert!(space.translate(VirtAddr::new(a + 3 * PAGE)).is_none());
            leave();
            drop(space);
            // Another CPU the thread ran on may hold the space until it
            // loads another: counted once every CPU has let it go.
            let after = settled_free_pages(&s);
            assert_eq!(after, before, "pages leaked: {before:?} then {after:?}");
        }
    }

    ktest! {
        fn fork_copies_on_write_between_spaces() {
            let s = cx();
            let before = settled_free_pages(&s);
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
            let after = settled_free_pages(&s);
            assert_eq!(after, before, "pages leaked: {before:?} then {after:?}");
        }
    }

    /// [`free_pages`] once it has stopped changing, exited threads
    /// reaped (earlier tests', and the workers that unload spaces).
    fn settled_free_pages(s: &sched::Sleepable) -> Pages {
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
        sched::reap_zombies();
        let mut last = free_pages();
        loop {
            crate::time::sleep_ns(s, 10_000_000);
            sched::reap_zombies();
            let now = free_pages();
            if now == last {
                return now;
            }
            last = now;
        }
    }

    /// Runs `code` as a process: its own read+execute page, a stack, and
    /// one thread bound to this CPU. Returns the exit status once every
    /// page is back.
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
                None,
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
            crate::proc::Exit::Signal(sig) | crate::proc::Exit::Dumped(sig) => {
                panic!("killed by signal {sig}")
            }
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
            assert!(space.protect(&s, range, Prot::RW, None).is_err(), "write is beyond max_prot");
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
            // Devices may still be attaching (USB enumerates in its own
            // thread): only the bells.
            let events: alloc::vec::Vec<_> = m
                .take_events()
                .into_iter()
                .filter(|e| e.starts_with("!system=VT "))
                .collect();
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
        /// A UART other than the console's is a terminal (ROADMAP P6.6):
        /// with the port looping what it sends back, what is written to
        /// `cuau1` (raw, at 115200) is read back from it, by interrupt
        /// both ways. x86 QEMU's COM2; elsewhere there is none.
        fn a_second_uart_is_a_terminal() {
            use abi::termios::{CLOCAL, CREAD, CS8, Termios, VMIN, VTIME};
            let s = cx();
            // x86's QEMU and bhyve machines have a COM2; arm64's none.
            let port = crate::tty::serial::port(1);
            #[cfg(target_arch = "x86_64")]
            let port = Some(port.expect("no uart1 terminal"));
            let Some(port) = port else {
                return;
            };
            let v = crate::fs::devfs::vnode_of(&s, b"cuau1").expect("cuau1");
            let dev = v.device(&s).unwrap().ops.instance(&s).unwrap().unwrap();
            let mut t = Termios::defaults();
            t.c_iflag = 0;
            t.c_oflag = 0;
            t.c_lflag = 0;
            t.c_cflag = CREAD | CS8 | CLOCAL;
            t.c_ispeed = 115_200;
            t.c_ospeed = 115_200;
            t.c_cc[VMIN] = 1;
            t.c_cc[VTIME] = 0;
            let mut arg = [0u8; 44];
            crate::tty::termios_to(&t, &mut arg);
            dev.ioctl(&s, abi::ioctl::TIOCSETA, &mut arg).unwrap();
            port.set_loopback(true);
            // More than the transmit FIFO and the receive buffer hold, in
            // chunks the receive FIFO takes whole: with no flow control, a
            // guest slow to take its interrupts (TCG) would otherwise lose
            // bytes to overruns, as real hardware would. A lost byte fails
            // the test rather than hanging it.
            let sent: alloc::vec::Vec<u8> = (0..600u32).map(|i| b'a' + (i % 26) as u8).collect();
            let mut got = alloc::vec![0u8; sent.len()];
            for (i, chunk) in sent.chunks(8).enumerate() {
                let n = dev.write(&s, &mut vfs::WriteBuf::new(chunk), 0, false).unwrap();
                assert_eq!(n, chunk.len());
                let at = i * 8;
                let mut have = 0;
                let deadline = crate::time::now_ns() + 5_000_000_000;
                while have < chunk.len() {
                    let mut r = vfs::ReadBuf::new(&mut got[at + have..at + chunk.len()]);
                    match dev.read(&s, &mut r, 0, true) {
                        Ok(n) => have += n,
                        Err(abi::errno::Errno::EAGAIN) => {
                            assert!(
                                crate::time::now_ns() < deadline,
                                "byte {} never came back",
                                at + have
                            );
                            crate::time::sleep_ns(&s, 1_000_000);
                        }
                        Err(e) => panic!("read: {e:?}"),
                    }
                }
            }
            port.set_loopback(false);
            assert_eq!(got, sent);
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
        /// Single-threading under load (ROADMAP P6.2): four copies of
        /// `threads stress` at once, on any CPU, each forking children
        /// whose threads sleep, fault in pages, make system calls, and
        /// block in `read` while another of their threads execs or exits.
        fn single_threading_under_load() {
            use alloc::vec::Vec;
            let s = cx();
            if crate::md::archive().is_none() {
                return;
            }
            let path: &[u8] = b"/usr/tests/abyss/threads";
            let procs: Vec<_> = (0..4)
                .map(|_| {
                    crate::proc::Process::spawn(&s, path, &[b"threads", b"stress", b"25"], &[], None)
                        .unwrap()
                })
                .collect();
            for p in procs {
                assert_eq!(p.wait(&s), crate::proc::Exit::Code(0));
            }
        }
    }

    ktest! {
        /// Process-shared and robust mutexes across processes: two
        /// `pshared` runs at once, each a parent and children sharing
        /// mutexes, condition variables, rwlocks, and semaphores, whose
        /// robust mutexes' owners die holding them.
        fn pshared_across_processes() {
            use alloc::vec::Vec;
            let s = cx();
            if crate::md::archive().is_none() {
                return;
            }
            let path: &[u8] = b"/usr/tests/abyss/pshared";
            let procs: Vec<_> = (0..2)
                .map(|_| crate::proc::Process::spawn(&s, path, &[b"pshared"], &[], None).unwrap())
                .collect();
            for p in procs {
                assert_eq!(p.wait(&s), crate::proc::Exit::Code(0));
            }
        }
    }

    ktest! {
        fn exec_layout_is_randomized_within_bounds() {
            use crate::exec::{Layout, MAXDSIZ, MAXSSIZ, USRSTACK};
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
                    crate::exec::load(
                        &s,
                        &space,
                        &echo,
                        b"/rescue/echo",
                        &[b"echo"],
                        &[],
                        &crate::proc::default_limits(),
                        0,
                        &|_| Err(abi::errno::Errno::ENOENT),
                    )
                    .unwrap();
                // The top page is a guard: mapped, with no access at all.
                let top = VirtAddr::new(USRSTACK);
                assert!(space.fault(&s, top, vm::fault::Access::Read).is_err());
                space.clear(&s);
                image.layout
            };
            let (a, b) = (load(), load());
            assert!(a.anon_loc != b.anon_loc && a.stack_top != b.stack_top, "{a:?} {b:?}");
            for Layout { anon_loc, stack_top, shared_page, .. } in [a, b] {
                // /rescue/echo's data is below 64 MiB; the layout is above
                // the data limit, anon_loc in the lower half of what is left.
                assert!((MAXDSIZ..USRSTACK / 2 + (MAXDSIZ + (64 << 20)) / 2).contains(&anon_loc));
                assert!(anon_loc.is_multiple_of(2 << 20));
                assert!((MAXDSIZ + MAXSSIZ..=USRSTACK).contains(&stack_top));
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
        /// and the largest file's. Then the old `mount(2)` mounts it too
        /// (`misccalls mount`). Skipped when the disk or FreeBSD's
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
                      cd / && umount /mnt || exit 5; \
                      /usr/tests/abyss/misccalls mount /dev/",
                    disk.as_bytes(),
                    b" /mnt || exit 6; exit 7",
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
        /// makes it, `/rescue`'s tools work over it mounted read-write with
        /// `acls` (directories, copies, links, renames, an archive, removal,
        /// access and default ACLs, and fsx when the memory disk has it),
        /// and after unmounting,
        /// FreeBSD's `fsck_ffs -f -n` finds nothing wrong; mounted again,
        /// it holds what was written.
        fn ufs_read_write_survives_fsck() {
            ufs_read_write(false);
        }
    }

    /// The low-memory cases' disk (`xtask`'s `LOWMEM_DISK`), which tells
    /// it apart.
    const LOWMEM_DISK: u64 = 1536 << 20;

    ktest! {
        /// More file data than memory (the boot tier's `*-lowmem` cases,
        /// whose small guest has a 1.5 GiB virtio disk; skipped without
        /// it): files totalling twice the RAM are written to UFS2 with
        /// soft updates, each synced and checksummed, then all read back
        /// and checked. The clean page cache must give way to new pages
        /// and to the kernel's own allocations (`vm::pageout`): the page
        /// daemon frees it, and an allocation short of pages reclaims it.
        fn page_cache_larger_than_memory() {
            use crate::proc::Process;
            let s = cx();
            let Some(disk) = find_disk(&s, "vtbd", |_, d| d.bytes() == LOWMEM_DISK) else {
                kprintln!("lowmem: no low-memory test disk; skipped");
                return;
            };
            let ram_mib = (crate::mm::phys_stats().managed * PAGE) >> 20;
            // Files of a quarter of RAM, twice RAM in all, within the disk.
            let file_mib = (ram_mib / 4).max(16);
            let files = (2 * ram_mib)
                .div_ceil(file_mib)
                .min((LOWMEM_DISK >> 20) * 3 / 4 / file_mib);
            kprintln!(
                "lowmem: {ram_mib} MiB of RAM: {files} files of {file_mib} MiB on /dev/{}",
                disk.name()
            );
            let script = alloc::format!(
                "exec > /tmp/lowmem.out 2>&1; \
                 stats() {{ echo \"$1: free $(sysctl -n vm.stats.vm.v_free_count) \
                     dfree $(sysctl -n vm.stats.vm.v_dfree) \
                     pdwakeups $(sysctl -n vm.stats.vm.v_pdwakeups) \
                     pdpages $(sysctl -n vm.stats.vm.v_pdpages) \
                     reserved $(sysctl -n vm.stats.vm.v_free_reserved) \
                     target $(sysctl -n vm.stats.vm.v_free_target)\"; }}; \
                 newfs -U /dev/{dev} > /dev/null || exit 1; \
                 mount -t ufs /dev/{dev} /mnt || exit 2; \
                 stats start; i=0; while [ $i -lt {files} ]; do \
                     dd if=/dev/random of=/mnt/w$i bs=1m count={file_mib} 2>/dev/null || exit 3; \
                     sync; md5 -q /mnt/w$i > /tmp/lowmem.$i || exit 4; \
                     stats \"w$i written\"; i=$((i+1)); done; \
                 i=0; while [ $i -lt {files} ]; do \
                     [ \"$(md5 -q /mnt/w$i)\" = \"$(cat /tmp/lowmem.$i)\" ] || exit 5; \
                     i=$((i+1)); done; \
                 stats \"read back\"; rm /mnt/w* /tmp/lowmem.[0-9]*; \
                 umount /mnt || exit 6; exit 7",
                dev = disk.name(),
            );
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", script.as_bytes()],
                &[b"PATH=/rescue", b"HOME=/"],
                None,
            )
            .unwrap();
            let exit = p.wait(&s);
            if let Some(out) = read_whole(&s, b"/tmp/lowmem.out") {
                for l in alloc::string::String::from_utf8_lossy(&out).lines() {
                    kprintln!("lowmem: {l}");
                }
            }
            assert_eq!(exit, crate::proc::Exit::Code(7));
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

    ktest! {
        /// U.5: UFS2 on the USB disks, made by FreeBSD's `newfs` with soft
        /// updates: the read-write workload, fsx included, and `fsck_ffs -f
        /// -n` clean, over Bulk-Only (`da0`), UAS on streams (`da1`), and
        /// UAS on USB 2 without them (`da3`). Skipped without the USB set.
        fn ufs_on_usb_disks() {
            let s = cx();
            let usb: Vec<_> = ["da0", "da1", "da3"]
                .into_iter()
                .filter(|d| storage::find(d).is_some())
                .collect();
            if usb.is_empty() {
                kprintln!("ufs: no USB disks; skipped");
                return;
            }
            for d in usb {
                ufs_workload(&s, &alloc::format!("/dev/{d}"), true, 2000);
            }
        }
    }

    ktest! {
        /// U.5: a USB disk pulled from under a mounted file system in use:
        /// `umass0` detached while fsx runs on `da0`. fsx fails, nothing
        /// panics, UFS unmounts it forcibly (as FreeBSD's
        /// `ffs_fsfail_cleanup`), and attached again, the disk is back and
        /// clean. Skipped without the USB set.
        fn usb_disk_unplugged_while_mounted() {
            use crate::proc::Process;
            let s = cx();
            let m = bus::manager();
            let Some((_, node)) = super::find_named(m, &s, crate::devices::nexus(), "umass0")
            else {
                kprintln!("umass: no umass0; skipped");
                return;
            };
            let script = "newfs -U /dev/da0 > /dev/null || exit 1; mkdir -p /tmp/u && \
                mount /dev/da0 /tmp/u || exit 2; \
                /usr/tests/fsx/fsx -q -N 1000000 -S 6 /tmp/u/f > /dev/null 2>&1; exit 7";
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", script.as_bytes()],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            // Until fsx is under way on it.
            let mounted = || {
                crate::fs::namespace()
                    .mounts()
                    .iter()
                    .any(|x| x.on() == b"/tmp/u")
            };
            for _ in 0..600 {
                if mounted() {
                    break;
                }
                crate::time::sleep_ns(&s, 50_000_000);
            }
            assert!(mounted(), "da0 mounted on /tmp/u");
            crate::time::sleep_ns(&s, 2_000_000_000);
            m.detach(node, bus::DetachReason::Requested, &s)
                .expect("detach");
            assert!(storage::find("da0").is_none(), "da0 withdrawn");
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(7));
            for _ in 0..200 {
                if !mounted() {
                    break;
                }
                crate::time::sleep_ns(&s, 50_000_000);
            }
            assert!(!mounted(), "/tmp/u forcibly unmounted");
            m.probe_and_attach(node, &s).expect("attach");
            m.wait_idle(&s);
            assert!(storage::find("da0").is_some(), "da0 back");
            let p = Process::spawn(
                &s,
                b"/rescue/sh",
                &[b"sh", b"-c", b"newfs /dev/da0 > /dev/null && fsck_ffs -f -n /dev/da0 > /dev/null"],
                &[b"PATH=/rescue", b"HOME=/"],
                Some(hal::cpu_id()),
            )
            .unwrap();
            assert_eq!(p.wait(&s), crate::proc::Exit::Code(0));
            let _ = m.take_events();
            kprintln!("umass: da0 pulled out from under fsx; forcibly unmounted, then back");
        }
    }

    /// The read-write workload on the 32 MiB test disk, made by FreeBSD's
    /// `newfs` (with soft updates if `soft`).
    fn ufs_read_write(soft: bool) {
        let s = cx();
        // Its own 32 MiB virtio disk, whatever its unit now.
        let Some(scratch) = find_disk(&s, "vtbd", |_, d| d.bytes() == 32 << 20) else {
            kprintln!("ufs: no read-write test disk; skipped");
            return;
        };
        ufs_workload(&s, &alloc::format!("/dev/{}", scratch.name()), soft, 10_000);
    }

    /// The read-write workload on disk `dev`, fsx running `ops` operations
    /// a seed.
    fn ufs_workload(s: &sched::Sleepable, dev: &str, soft: bool, ops: u32) {
        use crate::proc::Process;
        let what = if soft { " with soft updates" } else { "" };
        kprintln!("ufs: read-write on {dev}{what}");
        let fsx = read_whole(s, b"/usr/tests/fsx/fsx").is_some();
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
                mount -t ufs -o acls DEV /mnt || exit 2; cd /mnt || exit 2; SOFT \
                mkdir -p a/b c && echo hello > a/b/f && dd if=/rescue/rescue of=big bs=64k count=80 2>/dev/null && \
                ln a/b/f hard && ln -s a/b/f soft && mv big c/big && \
                [ \"$(cat soft)\" = hello ] && [ \"$(cat hard)\" = hello ] || exit 3; \
                [ \"$(md5 -q c/big)\" = \"$(dd if=/rescue/rescue bs=64k count=80 2>/dev/null | md5 -q)\" ] || exit 4; \
                tar -cf /tmp/t.tar -C / $(cd / && ls -d usr/tests/abyss/* | head -16) && \
                mkdir x && tar -xf /tmp/t.tar -C x && rm /tmp/t.tar || exit 5; \
                for f in $(ls x/usr/tests/abyss); do \
                    [ \"$(md5 -q x/usr/tests/abyss/$f)\" = \"$(md5 -q /usr/tests/abyss/$f)\" ] || exit 6; done; \
                rm -r a/b x/usr && [ ! -e a/b ] || exit 8; \
                has() { case \"$1\" in *\"$2\"*) return 0;; esac; return 1; }; \
                umask 022; : > acl.f && chmod 640 acl.f && setfacl -m u:1001:rw acl.f && \
                has \"$(getfacl -qn acl.f)\" 'user:1001:rw-' && \
                set -- $(ls -l acl.f) && [ \"$1\" = '-rw-rw----+' ] || \
                { (ls -l acl.f; getfacl -n acl.f) > /tmp/fsck.out 2>&1; exit 18; }; \
                mkdir acld && setfacl -d -m u::rwx,g::rx,o::,u:1001:rwx,m::rwx acld && \
                : > acld/f && mkdir acld/d && set -- $(ls -l acld/f) && [ \"$1\" = '-rw-r-----+' ] && \
                has \"$(getfacl -qn acld/f)\" 'user:1001:rwx' && \
                has \"$(getfacl -qn -d acld/d)\" 'user:1001:rwx' || \
                { (ls -l acld; getfacl -n acld/f; getfacl -n -d acld/d) > /tmp/fsck.out 2>&1; exit 19; }; \
                if [ -x /usr/tests/fsx/fsx ]; then for seed in 1 2 3; do \
                    /usr/tests/fsx/fsx -q -N OPS -S $seed /mnt/fsx.data > /tmp/fsx.out 2>&1 || exit 9; \
                    rm -f /mnt/fsx.data*; done; fi; \
                cd / && umount /mnt || exit 10; \
                fsck_ffs -f -n DEV > /tmp/fsck.out 2>&1 || exit 11; \
                mount -t ufs -o ro,acls DEV /mnt || exit 12; \
                [ \"$(cat /mnt/hard)\" = hello ] && \
                has \"$(getfacl -qn /mnt/acl.f)\" 'user:1001:rw-' && \
                [ \"$(md5 -q /mnt/c/big)\" = \"$(dd if=/rescue/rescue bs=64k count=80 2>/dev/null | md5 -q)\" ] || exit 13; \
                umount /mnt || exit 14; exit 7")
                .replace("SOFT", soft_checks)
                .replace("NEWFS", if soft { "-U" } else { "-u" })
                .replace("OPS", &alloc::format!("{ops}"))
                .replace("DEV", dev);
        let p = Process::spawn(
            s,
            b"/rescue/sh",
            &[b"sh", b"-c", script_text.as_bytes()],
            &[b"PATH=/rescue", b"HOME=/"],
            Some(hal::cpu_id()),
        )
        .unwrap();
        let exit = p.wait(s);
        if exit != crate::proc::Exit::Code(7) {
            for f in [&b"/tmp/fsck.out"[..], b"/tmp/fsx.out"] {
                if let Some(out) = read_whole(s, f) {
                    kprintln!(
                        "{}:\n{}",
                        core::str::from_utf8(f).unwrap_or("?"),
                        alloc::string::String::from_utf8_lossy(&out)
                    );
                }
            }
        }
        assert_eq!(exit, crate::proc::Exit::Code(7));
        let fsck = read_whole(s, b"/tmp/fsck.out").unwrap_or_default();
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
            let _ = crate::fs::parent_of(s, &p, crate::fs::At::Cwd, f, false)
                .and_then(|d| vfs::ops::remove(s, &d.dir, &d.name, &crate::fs::cred(), false));
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

    /// Runs `script` with `/rescue/sh`; returns its exit.
    fn rescue_sh(s: &sched::Sleepable, script: &str) -> crate::proc::Exit {
        let p = crate::proc::Process::spawn(
            s,
            b"/rescue/sh",
            &[b"sh", b"-c", script.as_bytes()],
            &[b"PATH=/rescue", b"HOME=/"],
            Some(hal::cpu_id()),
        )
        .unwrap();
        p.wait(s)
    }

    ktest! {
        /// FreeBSD's own `mount_nullfs`, `mount -t nullfs -o ro`, and
        /// `mount -t fdescfs` (plain and `linrdlnk`) work, and `mount`
        /// lists them as FreeBSD does (the ATF-style checks are
        /// `tests/user/layers.c`).
        fn nullfs_and_fdescfs_mount_with_freebsd_tools() {
            let s = cx();
            let script = "mount -t tmpfs tmpfs /mnt || exit 1; \
                mkdir /mnt/lower /mnt/upper /mnt/fd || exit 2; \
                echo hello > /mnt/lower/f || exit 3; \
                mount_nullfs /mnt/lower /mnt/upper || exit 4; \
                [ \"$(cat /mnt/upper/f)\" = hello ] || exit 5; \
                echo there >> /mnt/upper/f || exit 6; \
                [ \"$(tail -n 1 /mnt/lower/f)\" = there ] || exit 7; \
                case \"$(mount)\" in *'/mnt/lower on /mnt/upper (nullfs, local)'*) ;; *) exit 8;; esac; \
                umount /mnt/upper || exit 9; \
                mount -t nullfs -o ro /mnt/lower /mnt/upper || exit 10; \
                ( : > /mnt/upper/g ) 2>/dev/null && exit 11; \
                case \"$(mount)\" in *'/mnt/lower on /mnt/upper (nullfs, local, read-only)'*) ;; *) exit 12;; esac; \
                umount /mnt 2>/dev/null && exit 13; \
                umount /mnt/upper || exit 14; \
                mount -t fdescfs fdesc /mnt/fd || exit 15; \
                exec 3< /mnt/lower/f; \
                [ -c /mnt/fd/3 ] || exit 16; \
                [ \"$(head -n 1 /mnt/fd/3)\" = hello ] || exit 17; \
                case \"$(mount)\" in *'fdescfs on /mnt/fd (fdescfs)'*) ;; *) exit 18;; esac; \
                umount /mnt/fd || exit 19; \
                mount -t fdescfs -o linrdlnk fdesc /mnt/fd || exit 20; \
                case \"$(ls -l /mnt/fd/3)\" in *'/mnt/fd/3 -> /mnt/lower/f') ;; *) exit 21;; esac; \
                umount /mnt/fd || exit 22; \
                exec 3<&-; \
                umount /mnt || exit 23; exit 0";
            // 13: the tmpfs went though nullfs was on it.
            assert_eq!(rescue_sh(&s, script), crate::proc::Exit::Code(0));
        }
    }

    ktest! {
        /// The ISO 9660 test volume, attached with `mdconfig -t vnode`
        /// and mounted with `mount_cd9660` each way FreeBSD's listings
        /// were made (Rock Ridge, Joliet, plain, versions, owner and
        /// masks): `ls -aliRnT` (file numbers included) and `md5 -r`
        /// print what FreeBSD's cd9660 printed. Skipped without the
        /// volume (no `fetch-base` from the server).
        fn cd9660_reads_as_freebsd_reads_it() {
            let s = cx();
            if read_whole(&s, b"/usr/tests/iso/iso-test.files").is_none() {
                kprintln!("cd9660: no test volume; skipped");
                return;
            }
            const VIEWS: &[(&str, &str)] = &[
                ("rr", ""),
                ("joliet", "-r"),
                ("iso", "-r -j"),
                ("gens", "-r -j -g"),
                ("masks", "-U 5 -G 6 -m 640 -M 755"),
            ];
            let mut script = alloc::string::String::from(
                "mount -t tmpfs tmpfs /mnt || exit 1; mkdir /mnt/cd || exit 2; \
                 cp /usr/tests/iso/iso-test.iso /mnt/t.iso || exit 3; \
                 md=$(mdconfig -a -t vnode -f /mnt/t.iso) || exit 4; \
                 case $md in md[0-9]*) ;; *) exit 5;; esac; \
                 mount -t cd9660 /dev/$md /mnt/cd || exit 6; \
                 ( cd /mnt/cd && while IFS= read -r f; do md5 -r \"$f\"; done \
                   < /usr/tests/iso/iso-test.files > /mnt/iso.md5 ) || exit 7; \
                 mdconfig -d -u $md 2>/dev/null && exit 8; \
                 umount /mnt/cd || exit 9; ",
            );
            for (name, flags) in VIEWS {
                script.push_str(&alloc::format!(
                    "mount_cd9660 {flags} /dev/$md /mnt/cd || exit 10; \
                     ( cd /mnt/cd && TZ=UTC BLOCKSIZE=512 ls -aliRnT . > /mnt/{name}.ls ) || exit 11; \
                     umount /mnt/cd || exit 12; "
                ));
            }
            // The listings stay on the tmpfs, read before it goes.
            script.push_str("mdconfig -d -u $md || exit 13; exit 0");
            let exit = rescue_sh(&s, &script);
            assert_eq!(exit, crate::proc::Exit::Code(0), "the cd9660 script failed");
            // The root's `..` is the mount point's parent, and the root's
            // total counts it: neither is the volume's.
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
                    let words: alloc::vec::Vec<&[u8]> =
                        line.split(|&c| c == b' ').filter(|w| !w.is_empty()).collect();
                    out.extend_from_slice(&words.join(&b' '));
                    out.push(b'\n');
                }
                out
            };
            let mut same = true;
            for (name, _) in VIEWS {
                let want = read_whole(&s, alloc::format!("/usr/tests/iso/iso-test-{name}.ls").as_bytes())
                    .unwrap_or_default();
                let got = read_whole(&s, alloc::format!("/mnt/{name}.ls").as_bytes()).unwrap_or_default();
                let (want, got) = (unrooted(&want), unrooted(&got));
                first_difference(&alloc::format!("cd9660 {name}: ls -aliRnT"), &got, &want);
                if got != want {
                    kprintln!("cd9660 {name}: got:\n{}", alloc::string::String::from_utf8_lossy(&got));
                    same = false;
                }
            }
            let want_md5 = read_whole(&s, b"/usr/tests/iso/iso-test.md5").unwrap_or_default();
            let got_md5 = read_whole(&s, b"/mnt/iso.md5").unwrap_or_default();
            first_difference("cd9660: md5 -r", &got_md5, &want_md5);
            let cleaned = rescue_sh(&s, "umount /mnt");
            assert!(same && got_md5 == want_md5, "cd9660 differs from FreeBSD");
            assert_eq!(cleaned, crate::proc::Exit::Code(0));
            kprintln!("cd9660: {} views and the digests match FreeBSD's", VIEWS.len());
        }
    }

    ktest! {
        /// FreeBSD's `gpart` (from `/rescue`, through `/dev/geom.ctl`) on a
        /// malloc and then a vnode memory disk: create, add with labels,
        /// show four ways, backup, `newfs` and mount by GPT label, what is
        /// refused while mounted (delete, shrink, writing the disk until
        /// `kern.geom.debugflags=16`), a grow while mounted, `fsck_ffs`,
        /// modify (the label's node follows), an uncommitted add undone,
        /// delete, destroy; then a table left on the vnode disk read back
        /// by a new attach. Everything printed must be what FreeBSD 15.1
        /// printed for the same script (tests/corpus/gpart/guest.sh).
        /// With `abyss.test.gpart.dump=1` the image's nonzero sectors are
        /// printed (`gpart-dump: SECTOR HEX`), for FreeBSD to read it.
        fn gpart_does_what_freebsd_does() {
            let s = cx();
            let script = alloc::format!(
                "mount -t tmpfs tmpfs /mnt || exit 99\n{{\n{}\n}} > /mnt/gpart.out 2>&1\n",
                include_str!("../../tests/corpus/gpart/guest.sh")
            );
            let exit = rescue_sh(&s, &script);
            let got = read_whole(&s, b"/mnt/gpart.out").unwrap_or_default();
            let want = include_bytes!("../../tests/corpus/gpart/guest-freebsd.txt");
            first_difference("gpart", &got, want);
            if got != want {
                kprintln!("gpart: got:\n{}", alloc::string::String::from_utf8_lossy(&got));
            }
            if crate::boot::value("abyss.test.gpart.dump") == Some("1") {
                let root = crate::fs::root();
                let l = vfs::namei::Lookup {
                    start: &root,
                    root: &root,
                    follow: true,
                    beneath: false,
                    create: false,
                    cred: &crate::fs::cred(),
                };
                let v = crate::fs::namespace().lookup(&s, &l, b"/mnt/disk.img").unwrap();
                let mut sector = [0u8; 512];
                for i in 0..v.size(&s).unwrap() / 512 {
                    let mut r = vfs::ReadBuf::new(&mut sector);
                    assert_eq!(v.read_at(&s, i * 512, &mut r), Ok(512));
                    if sector.iter().any(|&b| b != 0) {
                        let mut hex = alloc::string::String::new();
                        for b in sector {
                            let _ = core::fmt::Write::write_fmt(&mut hex, format_args!("{b:02x}"));
                        }
                        kprintln!("gpart-dump: {i} {hex}");
                    }
                }
            }
            let cleaned = rescue_sh(&s, "umount /mnt");
            assert_eq!(exit, crate::proc::Exit::Code(0), "the gpart script failed");
            assert!(got == want, "gpart differs from FreeBSD");
            assert_eq!(cleaned, crate::proc::Exit::Code(0));
            kprintln!("gpart: {} lines as FreeBSD printed them", want.split(|&c| c == b'\n').count());
        }
    }

    ktest! {
        /// A USB stick as desktops partition it: FreeBSD 15.1's MBR with
        /// one active FAT32 slice that `newfs_msdos` made
        /// (tests/corpus/gpart/mbr-fat), written to the Bulk-Only disk
        /// (`da0`) through its node. The last writer's close has it tasted:
        /// `da0s1` appears, `gpart show` prints what FreeBSD printed, and
        /// `/rescue`'s `mount_msdosfs` mounts it, a file written there is
        /// read back after `fsck_msdosfs -n` finds the volume clean. Then
        /// an MBR slice with a disklabel in it. Wiped after, so the disk
        /// has no table again. Skipped without the USB set.
        fn mbr_fat_stick_mounts_with_mount_msdosfs() {
            struct Nothing;
            impl vfs::CharDevice for Nothing {
                fn read(&self, _: &sched::Sleepable, _: &mut dyn vfs::Uio, _: u64, _: bool)
                    -> Result<usize, abi::errno::Errno> {
                    Ok(0)
                }
                fn write(&self, _: &sched::Sleepable, _: &mut dyn vfs::Uio, _: u64, _: bool)
                    -> Result<usize, abi::errno::Errno> {
                    Ok(0)
                }
            }
            let s = cx();
            let Some(da0) = storage::find("da0") else {
                kprintln!("mbr: no USB disks; skipped");
                return;
            };
            assert_eq!(da0.bytes(), 64 << 20, "the Bulk-Only disk's size");
            // Written as through its node: the table appears at the close.
            let nothing = alloc::sync::Arc::new(Nothing) as alloc::sync::Arc<dyn vfs::CharDevice>;
            let writer = crate::geom_kern::open(&s, nothing, &da0, "da0", true).expect("da0 for writing");
            // The FAT's tables are within the first MiB; what was there goes.
            da0.write(&s, 0, &alloc::vec![0u8; 1 << 20]).unwrap();
            // A corpus image's nonzero sectors (`SECTOR HEX` lines).
            let write_sectors = |d: &storage::Disk, image: &str| {
                for line in image.lines().skip(1) {
                    let (lba, hex) = line.split_once(' ').unwrap();
                    let sector: alloc::vec::Vec<u8> = (0..512)
                        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
                        .collect();
                    d.write(&s, lba.parse::<u64>().unwrap() * 512, &sector).unwrap();
                }
            };
            write_sectors(&da0, include_str!("../../tests/corpus/gpart/mbr-fat.sectors"));
            drop(writer);
            crate::geom_kern::run(&s, alloc::boxed::Box::new(|_| {}));
            assert!(storage::find("da0s1").is_some(), "da0s1 tasted at the last close");
            let script = "mount -t tmpfs tmpfs /mnt || exit 1; mkdir /mnt/stick || exit 2; \
                gpart show da0 > /mnt/show || exit 3; \
                mount_msdosfs /dev/da0s1 /mnt/stick || exit 4; \
                echo hello > /mnt/stick/HELLO.TXT || exit 5; \
                [ \"$(ls /mnt/stick)\" = HELLO.TXT ] || exit 6; \
                umount /mnt/stick || exit 7; \
                fsck_msdosfs -n /dev/da0s1 > /dev/null 2>&1 || exit 8; \
                mount -t msdosfs -o ro /dev/da0s1 /mnt/stick || exit 9; \
                [ \"$(cat /mnt/stick/HELLO.TXT)\" = hello ] || exit 10; \
                umount /mnt/stick || exit 11; exit 0";
            let exit = rescue_sh(&s, script);
            let show = read_whole(&s, b"/mnt/show").unwrap_or_default();
            let cleaned = rescue_sh(&s, "umount /mnt");
            // FreeBSD's `gpart show md0` of the image, for a disk of the
            // same name's length.
            let want = "=>     1  131071  da0  MBR  (64M)\n       1  131071    1  fat32lba  [active]  (64M)\n\n";
            first_difference("gpart show da0", &show, want.as_bytes());
            assert_eq!(exit, crate::proc::Exit::Code(0), "the stick's script failed");
            assert_eq!(show, want.as_bytes());
            assert_eq!(cleaned, crate::proc::Exit::Code(0));
            // Then a FreeBSD install the old way (tests/corpus/gpart/mbr-bsd):
            // a `freebsd` slice holding a disklabel, whose partitions are
            // tasted in the slice: `a`, `b`, `d` (`c`, the raw one, has
            // no provider), as FreeBSD's `gpart show` printed them.
            let nothing = alloc::sync::Arc::new(Nothing) as alloc::sync::Arc<dyn vfs::CharDevice>;
            let writer = crate::geom_kern::open(&s, nothing, &da0, "da0", true).expect("da0 for writing");
            assert!(storage::find("da0s1").is_none(), "spoiled by the writer");
            da0.write(&s, 0, &alloc::vec![0u8; 1 << 20]).unwrap();
            write_sectors(&da0, include_str!("../../tests/corpus/gpart/mbr-bsd.sectors"));
            drop(writer);
            crate::geom_kern::run(&s, alloc::boxed::Box::new(|_| {}));
            for (name, there) in [("da0s1a", true), ("da0s1b", true), ("da0s1c", false), ("da0s1d", true)] {
                assert_eq!(storage::find(name).is_some(), there, "{name}");
            }
            assert_eq!(storage::find("da0s1b").map(|d| d.bytes()), Some(8 << 20));
            let exit = rescue_sh(
                &s,
                "mount -t tmpfs tmpfs /mnt && gpart show da0s1 > /mnt/show && [ -c /dev/da0s1d ]",
            );
            let show = read_whole(&s, b"/mnt/show").unwrap_or_default();
            let cleaned = rescue_sh(&s, "umount /mnt");
            let want = "=>     0  131071  da0s1  BSD  (64M)\n       0   81920      1  freebsd-ufs  (40M)\n   81920   16384      2  freebsd-swap  (8.0M)\n   98304   32767      4  freebsd-ufs  (16M)\n\n";
            first_difference("gpart show da0s1", &show, want.as_bytes());
            assert_eq!(exit, crate::proc::Exit::Code(0));
            assert_eq!(show, want.as_bytes());
            assert_eq!(cleaned, crate::proc::Exit::Code(0));
            // Wiped: the tables go at the close, the label's with the slice.
            let nothing = alloc::sync::Arc::new(Nothing) as alloc::sync::Arc<dyn vfs::CharDevice>;
            let writer = crate::geom_kern::open(&s, nothing, &da0, "da0", true).expect("da0 for writing");
            assert!(storage::find("da0s1").is_none() && storage::find("da0s1a").is_none(), "spoiled");
            da0.write(&s, 0, &alloc::vec![0u8; 1 << 20]).unwrap();
            drop(writer);
            crate::geom_kern::run(&s, alloc::boxed::Box::new(|_| {}));
            assert!(storage::find("da0s1").is_none());
            kprintln!("mbr: the FAT32 stick mounted with mount_msdosfs; the disklabel's partitions read");
        }
    }

    ktest! {
        /// `/dev/full`: root's, wheel's (0, 0: the test image has no
        /// names for them), 0666; zeros to read, and every
        /// write fails with `ENOSPC`.
        fn dev_full_reads_zeros_and_is_full() {
            let s = cx();
            let script = "[ -c /dev/full ] || exit 1; \
                case \"$(ls -l /dev/full | sed 's/  */ /g')\" in 'crw-rw-rw- 1 0 0 '*) ;; *) exit 2;; esac; \
                if echo x 2>/dev/null > /dev/full; then exit 3; fi; \
                [ \"$(dd if=/dev/full bs=8 count=3 2>/dev/null | md5)\" = \"$(dd if=/dev/zero bs=8 count=3 2>/dev/null | md5)\" ] || exit 4; \
                case \"$(dd if=/dev/zero of=/dev/full count=1 2>&1)\" in *'No space left on device'*) ;; *) exit 5;; esac; \
                exit 0";
            assert_eq!(rescue_sh(&s, script), crate::proc::Exit::Code(0));
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
        fn processes_leave_nothing_on_the_heap() {
            // Every new thread's start record is freed once it runs: with
            // it kept, each of the 200 processes left an object behind.
            use crate::proc::{Exit, Process};
            let s = cx();
            let run = |n: u32| {
                let script = alloc::format!(
                    "i=0; while [ $i -lt {n} ]; do /rescue/sh -c :; i=$((i+1)); done"
                );
                let p = Process::spawn(
                    &s,
                    b"/rescue/sh",
                    &[b"sh", b"-c", script.as_bytes()],
                    &[b"PATH=/rescue", b"HOME=/"],
                    Some(hal::cpu_id()),
                )
                .unwrap();
                assert_eq!(p.wait(&s), Exit::Code(0));
            };
            // Caches the programs and their lookups.
            run(20);
            let before = crate::mm::heap_stats().live_objects;
            run(200);
            let after = crate::mm::heap_stats().live_objects;
            assert!(
                after < before + 100,
                "200 processes left {} heap objects",
                after.saturating_sub(before)
            );
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
            // exited workers keep their stacks until reaped.)
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

ktest! {
    /// GEOM's userland view (`kern.disks`, `kern.geom.conftxt`, `confxml`):
    /// the boot disk and its table; then a table written to the scratch
    /// disk, which appears once its writer closes it, has its labels
    /// withered while a partition is open for writing, and goes once the
    /// disk is opened for writing again and overwritten.
    fn geom_shows_disks_and_retastes_written_tables() {
        use alloc::string::String;
        use alloc::sync::Arc;
        use alloc::vec::Vec;

        /// A device that does nothing, standing in for a disk node's.
        struct Nothing;
        impl vfs::CharDevice for Nothing {
            fn read(
                &self,
                _: &sched::Sleepable,
                _: &mut dyn vfs::Uio,
                _: u64,
                _: bool,
            ) -> Result<usize, abi::errno::Errno> {
                Ok(0)
            }
            fn write(
                &self,
                _: &sched::Sleepable,
                _: &mut dyn vfs::Uio,
                _: u64,
                _: bool,
            ) -> Result<usize, abi::errno::Errno> {
                Ok(0)
            }
        }

        let s = cx();
        let read = |name: &str| -> String {
            let oid = crate::sysctl::read(&s, |t| t.name2oid(name))
                .unwrap_or_else(|| panic!("{name}"));
            let q = crate::sysctl::read(&s, |t| t.query(&oid)).expect("a query node");
            let mut out = Vec::new();
            q(None, &mut out).expect(name);
            assert_eq!(out.pop(), Some(0), "{name} ends in a NUL");
            String::from_utf8(out).expect("text")
        };
        let disks = read("kern.disks");
        assert!(disks.split(' ').any(|d| d == "vtbd0"), "kern.disks: {disks}");
        let txt = read("kern.geom.conftxt");
        assert!(txt.lines().any(|l| l.starts_with("0 DISK vtbd0 ")), "{txt}");
        assert!(
            txt.lines().any(|l| l.starts_with("1 PART vtbd0p1 ")
                && l.ends_with(" ty efi xs GPT xt c12a7328-f81f-11d2-ba4b-00a0c93ec93b")),
            "{txt}"
        );
        let xml = read("kern.geom.confxml");
        assert!(xml.starts_with("<mesh>\n  <class id=\"0x") && xml.ends_with("</mesh>\n"));
        assert!(xml.contains("\t<scheme>GPT</scheme>\n"), "{xml}");
        assert!(read("kern.geom.confdot").starts_with("digraph geom {\n"));

        let nothing = || Arc::new(Nothing) as Arc<dyn vfs::CharDevice>;
        let scratch = storage::find("vtbd1").expect("the scratch disk, vtbd1");
        let size = usize::try_from(scratch.bytes()).unwrap();
        let mut image = alloc::vec![0u8; size];
        gpt::write(
            &mut image,
            gpt::Guid::must_parse("11111111-2222-3333-4444-555555555555"),
            &[gpt::PartitionSpec {
                type_guid: gpt::types::FREEBSD_UFS,
                unique_guid: gpt::Guid::must_parse("0A0B0C0D-0E0F-1011-1213-141516171819"),
                first_lba: 2048,
                last_lba: 4095,
                attributes: 1 << 59,
                name: "geom test",
            }],
        )
        .unwrap();
        let tail = size - 33 * 512;
        let writer = crate::geom_kern::open(&s, nothing(), &scratch, "vtbd1", true).expect("opened for writing");
        scratch.write(&s, 0, &image[..34 * 512]).unwrap();
        scratch.write(&s, tail as u64, &image[tail..]).unwrap();
        assert!(storage::find("vtbd1p1").is_none(), "not tasted while open");
        drop(writer);
        let xml = read("kern.geom.confxml");
        assert!(storage::find("vtbd1p1").is_some(), "tasted at the last close");
        for line in [
            "\t  <name>vtbd1p1</name>\n",
            "\t    <label>geom test</label>\n\t    <attrib>bootme</attrib>\n",
            "\t    <rawuuid>0a0b0c0d-0e0f-1011-1213-141516171819</rawuuid>\n",
            "\t    <efimedia>HD(1,GPT,0a0b0c0d-0e0f-1011-1213-141516171819,0x800,0x800)</efimedia>\n",
            "\t  <name>gpt/geom%20test</name>\n",
            "\t  <name>gptid/0a0b0c0d-0e0f-1011-1213-141516171819</name>\n",
            "\t<state>OK</state>\n",
        ] {
            assert!(xml.contains(line), "{line:?} in {xml}");
        }
        // A writer of the partition: counted, and its labels wither.
        let part = storage::find("vtbd1p1").unwrap();
        let writer = crate::geom_kern::open(&s, nothing(), &part, "vtbd1p1", true).expect("opened for writing");
        let xml = read("kern.geom.confxml");
        assert!(!xml.contains("gpt/geom%20test"), "labels wither while written");
        assert!(xml.contains("\t  <mode>r1w1e0</mode>\n\t  <name>vtbd1p1</name>\n"), "{xml}");
        assert!(xml.contains("\t  <mode>r1w1e1</mode>\n\t  <name>vtbd1</name>\n"), "{xml}");
        drop(writer);
        drop(part);
        assert!(read("kern.geom.confxml").contains("gpt/geom%20test"));
        // Opened for writing, the table goes; overwritten, it stays gone.
        let writer = crate::geom_kern::open(&s, nothing(), &scratch, "vtbd1", true).expect("opened for writing");
        assert!(storage::find("vtbd1p1").is_none(), "spoiled");
        scratch.write(&s, 0, &alloc::vec![0u8; 34 * 512]).unwrap();
        scratch.write(&s, tail as u64, &alloc::vec![0u8; 33 * 512]).unwrap();
        drop(writer);
        assert!(!read("kern.geom.conftxt").contains("vtbd1p1"));
        assert!(storage::find("vtbd1p1").is_none());
    }
}

ktest! {
    /// Loopback UDP and TCP between kernel-held sockets, with the waits
    /// and wakeups the system calls use: a datagram to 127.0.0.1 and ::1,
    /// a connection whose transfer outruns both buffers while the reader
    /// sleeps in another thread, the PCB list netstat reads, `lo0` in the
    /// routing sysctl, and a netlink `RTM_GETLINK` dump.
    fn inet_loopback_udp_tcp_sysctl_netlink() {
        use alloc::sync::Arc;
        use inet::addr::{LOOPBACK6, sockaddr_in, sockaddr_in6};
        use inet::{Cred, Family, Proto, Socket};
        let s = cx();
        let root = Cred::root();
        const LO: [u8; 4] = [127, 0, 0, 1];

        // UDP over both families.
        for (family, to) in [
            (Family::Inet, sockaddr_in(LO, 0)),
            (Family::Inet6, sockaddr_in6(&LOOPBACK6, 0, 0)),
        ] {
            let rx = Socket::new(family, Proto::Udp, &root);
            let tx = Socket::new(family, Proto::Udp, &root);
            rx.bind(&to, &root).unwrap();
            let port = rx.sockname().port;
            let mut dst = to.clone();
            dst[2..4].copy_from_slice(&port.to_be_bytes());
            assert_eq!(tx.send_dgram(b"dgram", Some(&dst), &root), Ok(5));
            // IPv4 loops through `lo0`'s `netisr` queue, as FreeBSD's does.
            let mut r = rx.recv_dgram(64, false);
            for _ in 0..100 {
                if !matches!(r, Err(abi::errno::Errno::EAGAIN)) {
                    break;
                }
                crate::time::sleep_ns(&s, 1_000_000);
                r = rx.recv_dgram(64, false);
            }
            let r = r.unwrap();
            assert_eq!(r.data, b"dgram");
            assert_eq!(r.from.unwrap().port, tx.sockname().port);
            rx.close();
            tx.close();
        }

        // TCP: a writer thread fills both buffers; this thread reads.
        let l = Socket::new(Family::Inet, Proto::Tcp, &root);
        l.bind(&sockaddr_in(LO, 0), &root).unwrap();
        l.listen(1, &root).unwrap();
        let mut dst = sockaddr_in(LO, 0);
        dst[2..4].copy_from_slice(&l.sockname().port.to_be_bytes());
        let c = Socket::new(Family::Inet, Proto::Tcp, &root);
        c.connect(&dst, &root, false).unwrap();
        // The handshake runs on `lo0`'s `netisr` queue, as on FreeBSD.
        let a = crate::fs::socket::wait_until(&s, &l.rd, 0, || l.accept()).unwrap();
        const TOTAL: usize = 512 * 1024;
        let writer = {
            let c = Arc::clone(&c);
            sched::spawn("inet-writer", sched::Priority::KERNEL, move |s| {
                let chunk = alloc::vec![0x5au8; 16 * 1024];
                let mut sent = 0;
                while sent < TOTAL {
                    let n = (TOTAL - sent).min(chunk.len());
                    let r = crate::fs::socket::wait_until(s, &c.wr, 0, || {
                        c.send_stream(&chunk[..n], false)
                    });
                    sent += r.expect("send");
                }
                c.shutdown(abi::socket::SHUT_WR).unwrap();
            })
            .expect("spawn")
        };
        let mut got = 0;
        loop {
            let r = crate::fs::socket::wait_until(&s, &a.rd, 0, || {
                a.recv_stream(32 * 1024, false, false)
            })
            .unwrap();
            if r.data.is_empty() {
                break;
            }
            assert!(r.data.iter().all(|&b| b == 0x5a));
            got += r.data.len();
        }
        assert_eq!(got, TOTAL);
        writer.join(&s);

        // netstat's view: the listener and both ends of the connection.
        let mut out = alloc::vec::Vec::new();
        let oid = crate::sysctl::read(&s, |t| t.name2oid("net.inet.tcp.pcblist")).unwrap();
        crate::sysctl::read_oid(&s, &oid, &mut out).unwrap();
        let gen_ = size_of::<abi::inet::Xinpgen>();
        let rec = size_of::<abi::inet::Xtcpcb>();
        assert!(out.len() >= 2 * gen_ + 3 * rec);
        // `getifaddrs`'s list: lo0 first, with its addresses.
        out.clear();
        let mut oid = crate::sysctl::read(&s, |t| t.name2oid("net.routetable")).unwrap();
        oid.extend_from_slice(&[0, 0, abi::net::net_rt::IFLISTL, 0]);
        crate::sysctl::read_oid(&s, &oid, &mut out).unwrap();
        assert_eq!(out[3], abi::net::rtm::IFINFO);
        assert!(out.windows(3).any(|w| w == b"lo0"));
        // ifconfig's first request.
        let mut req = alloc::vec![0u8; 32];
        req[0..4].copy_from_slice(&32u32.to_ne_bytes());
        req[4..6].copy_from_slice(&abi::netlink::RTM_GETLINK.to_ne_bytes());
        let fl = abi::netlink::NLM_F_REQUEST | abi::netlink::NLM_F_DUMP;
        req[6..8].copy_from_slice(&fl.to_ne_bytes());
        let ctx = inet::netlink::Ctx {
            port: 1,
            ..inet::netlink::Ctx::default()
        };
        let reply = inet::netlink::handle(&ctx, &net::vnet0().net, &req, 0);
        assert_eq!(
            u16::from_ne_bytes([reply[4], reply[5]]),
            abi::netlink::RTM_NEWLINK
        );
        assert!(reply.windows(4).any(|w| w == b"lo0\0"));
        for x in [c, a, l] {
            x.close();
        }
    }
}

ktest! {
    /// FreeBSD's own `ifconfig` and `route` (from `/rescue`) over netlink
    /// and the interface ioctls: `lo0` as FreeBSD prints it once
    /// `rc.d/netif` has run, the commands `rc` runs on it, and a route
    /// lookup, add, and delete.
    fn rescue_ifconfig_and_route_on_lo0() {
        use crate::proc::Process;
        let s = cx();
        if crate::md::archive().is_none() {
            return;
        }
        let script: &[u8] = b"out=$(ifconfig lo0) || exit 1; \
            case \"$out\" in *'lo0: flags=1008049<UP,LOOPBACK,RUNNING,MULTICAST,LOWER_UP> metric 0 mtu 16384'*) ;; *) echo \"$out\"; exit 2;; esac; \
            case \"$out\" in *'options=680003<RXCSUM,TXCSUM,LINKSTATE,RXCSUM_IPV6,TXCSUM_IPV6>'*) ;; *) echo \"$out\"; exit 3;; esac; \
            case \"$out\" in *'inet 127.0.0.1 netmask 0xff000000'*) ;; *) echo \"$out\"; exit 4;; esac; \
            case \"$out\" in *'inet6 ::1 prefixlen 128'*) ;; *) echo \"$out\"; exit 5;; esac; \
            case \"$out\" in *'inet6 fe80::1%lo0 prefixlen 64 scopeid 0x'*) ;; *) echo \"$out\"; exit 6;; esac; \
            case \"$out\" in *'groups: lo'*) ;; *) echo \"$out\"; exit 7;; esac; \
            case \"$out\" in *'nd6 options=21<PERFORMNUD,AUTO_LINKLOCAL>'*) ;; *) echo \"$out\"; exit 8;; esac; \
            ifconfig lo0 inet 127.0.0.1/8 alias || exit 9; \
            ifconfig lo0 up || exit 10; \
            case \"$(ifconfig -l)\" in lo0|'vtnet0 lo0') ;; *) exit 11;; esac; \
            case \"$(route -n get 127.0.0.1)\" in *'interface: lo0'*) ;; *) exit 12;; esac; \
            route -n add -inet6 fe80:: -prefixlen 10 ::1 -reject > /dev/null || exit 13; \
            case \"$(route -n add -inet 127.0.0.1 -iface lo0 2>&1)\" in *'File exists'*) ;; *) exit 14;; esac; \
            route -n delete -inet6 fe80:: -prefixlen 10 > /dev/null || exit 15; \
            exit 42";
        let p = Process::spawn(
            &s,
            b"/rescue/sh",
            &[b"sh", b"-c", script],
            &[b"PATH=/rescue", b"HOME=/"],
            Some(hal::cpu_id()),
        )
        .unwrap();
        assert_eq!(p.wait(&s), crate::proc::Exit::Code(42));
    }
}

fn unhex(s: &str) -> alloc::vec::Vec<u8> {
    let s: alloc::vec::Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
    s.chunks(2)
        .map(|p| u8::from_str_radix(core::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

ktest! {
    fn crypto_matches_the_published_vectors() {
        // Every machine the boot tier uses has them (QEMU's `max` CPU, the
        // build server's): the software fallback is tested on the host.
        assert!(kcrypto::accelerated(), "AES and carry-less multiply instructions not found");
        // GCM test case 4 (McGrew and Viega), and its text under a
        // 256-bit key.
        let p = unhex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72
             1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
        );
        let aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let nonce: [u8; 12] = unhex("cafebabefacedbaddecaf888").try_into().unwrap();
        let k = unhex("feffe9928665731c6d6a8f9467308308");
        for (key, c, t) in [
            (
                k.clone(),
                "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e
                 21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091",
                "5bc94fbc3221a5db94fae95ae7121a47",
            ),
            (
                [k.as_slice(), &k].concat(),
                "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa
                 8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662",
                "76fc6ece0f4e1768cddf8853bb2d551b",
            ),
        ] {
            let g = kcrypto::Gcm::new(&key).unwrap();
            let mut d = p.clone();
            let tag = g.seal(&nonce, &aad, &mut d);
            assert_eq!(d, unhex(c), "GCM ciphertext, {}-byte key", key.len());
            assert_eq!(tag.to_vec(), unhex(t), "GCM tag, {}-byte key", key.len());
            assert!(g.open(&nonce, &aad, &mut d, &tag));
            assert_eq!(d, p);
        }
        // SP 800-38A F.2.1, chained across two calls.
        let cbc = kcrypto::Cbc::new(&unhex("2b7e151628aed2a6abf7158809cf4f3c")).unwrap();
        let pt = unhex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        let mut d = pt.clone();
        let iv: [u8; 16] = core::array::from_fn(|i| i.to_le_bytes()[0]);
        let mut v = iv;
        cbc.with(|e| e.cbc_encrypt(&mut v, &mut d));
        assert_eq!(d, unhex("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2"));
        let mut v = iv;
        cbc.with(|e| {
            e.cbc_decrypt(&mut v, &mut d[..16]);
            e.cbc_decrypt(&mut v, &mut d[16..]);
        });
        assert_eq!(d, pt);
    }
}

ktest! {
    fn crypto_instructions_agree_with_software() {
        let data: alloc::vec::Vec<u8> = (0..700u32).map(|i| (i * 31 + 7).to_le_bytes()[0]).collect();
        for klen in [16u8, 24, 32] {
            let key: alloc::vec::Vec<u8> = (0..klen).map(|i: u8| i * 5 + 1).collect();
            let (hw, sw) = (kcrypto::Gcm::new(&key).unwrap(), kcrypto::Gcm::new_soft(&key).unwrap());
            for (len, alen) in [(0usize, 0usize), (1, 13), (15, 5), (16, 16), (17, 0), (255, 13), (700, 31)] {
                let nonce = [len.to_le_bytes()[0]; 12];
                let (mut a, mut b) = (data[..len].to_vec(), data[..len].to_vec());
                let ta = hw.seal(&nonce, &data[..alen], &mut a);
                let tb = sw.seal(&nonce, &data[..alen], &mut b);
                assert_eq!((a, ta), (b, tb), "GCM, {klen}-byte key, {len} bytes, {alen} of AAD");
            }
            let (hw, sw) = (kcrypto::Cbc::new(&key).unwrap(), kcrypto::Cbc::new_soft(&key).unwrap());
            let (mut a, mut b) = (data[..640].to_vec(), data[..640].to_vec());
            let (mut va, mut vb) = ([3u8; 16], [3u8; 16]);
            hw.with(|e| e.cbc_encrypt(&mut va, &mut a));
            sw.with(|e| e.cbc_encrypt(&mut vb, &mut b));
            assert_eq!((&a, va), (&b, vb), "CBC encryption, {klen}-byte key");
            let (mut va, mut vb) = ([3u8; 16], [3u8; 16]);
            hw.with(|e| e.cbc_decrypt(&mut va, &mut a));
            sw.with(|e| e.cbc_decrypt(&mut vb, &mut b));
            assert_eq!(a, data[..640], "CBC decryption, {klen}-byte key");
            assert_eq!(b, data[..640]);
        }
    }
}

ktest! {
    fn crypto_keeps_a_threads_vector_registers() {
        // A thread with FP state calls the ciphers between loading and
        // storing its registers: the SIMD section must put them back.
        let t = sched::Builder::new("simd")
            .bind(hal::CpuId::BOOT)
            .fpu()
            .spawn(|_| {
                let pattern: [u8; 512] = core::array::from_fn(|i| i.to_le_bytes()[0].wrapping_mul(37) ^ 0xa5);
                let mut out = [0u8; 512];
                #[cfg(target_arch = "x86_64")]
                {
                    // SAFETY: this thread has FP state; buffers are 512 bytes.
                    unsafe { arch::fpu::load_call_store(&pattern, &mut out, crypto_between) };
                    let n = arch::fpu::compared_bytes();
                    for r in 0..16 {
                        assert_eq!(&out[r * 32..r * 32 + n], &pattern[r * 32..r * 32 + n], "register {r}");
                    }
                }
                #[cfg(target_arch = "aarch64")]
                {
                    let vl = arch::fpu::vector_length();
                    let sve: alloc::vec::Vec<u8> = (0..32 * vl).map(|i: usize| i.to_le_bytes()[0] ^ 0x3c).collect();
                    let mut sve_out = alloc::vec![0u8; 32 * vl];
                    // SAFETY: this thread has FP state; buffers are sized
                    // as documented.
                    unsafe { arch::fpu::load_call_store(&pattern, &mut out, &sve, &mut sve_out, crypto_between) };
                    assert_eq!(out, pattern, "V registers");
                    assert_eq!(sve_out, sve, "Z registers");
                }
            })
            .expect("spawn");
        t.join(&cx());
    }
}

extern "C" fn crypto_between() {
    let g = kcrypto::Gcm::new(&[7; 16]).unwrap();
    let mut d = [9u8; 100];
    let _ = g.seal(&[1; 12], b"aad", &mut d);
    let c = kcrypto::Cbc::new(&[8; 32]).unwrap();
    c.with(|e| e.cbc_decrypt(&mut [0; 16], &mut d[..96]));
}
