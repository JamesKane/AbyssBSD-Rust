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

/// Runs every test, then the deliberate fault. Does not return.
///
/// With `abyss.test.warm_reset`, the first boot instead writes a marker to
/// the console (and so the log ring) and warm-resets; the loader saves that
/// log to the ESP on the second boot, which then runs the tests. xtask
/// checks the saved file.
pub(crate) fn run() -> ! {
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
    let _ = ktest::run_tests(&mut crate::console::Writer);
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
                sched::spawn("worker", sched::Priority::KERNEL, move || {
                    for _ in 0..100 {
                        count.fetch_add(i, Ordering::Relaxed);
                        sched::yield_now();
                    }
                })
                .expect("spawn")
            })
            .collect();
        for t in &threads {
            t.join();
            assert!(t.has_exited());
            assert!(t.switches() >= 100, "{} switched {} times", t.name(), t.switches());
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
            move || {
                for round in 0..500u64 {
                    let want = round * 2 + me;
                    court.q.wait_until(|| court.turn.load(Ordering::Acquire) == want);
                    court.turn.store(want + 1, Ordering::Release);
                    court.q.wake_all();
                }
            }
        };
        let a = sched::spawn("ping", sched::Priority::KERNEL, player(0)).unwrap();
        let b = sched::spawn("pong", sched::Priority::KERNEL, player(1)).unwrap();
        a.join();
        b.join();
        assert_eq!(court.turn.load(Ordering::Relaxed), 1000);
    }
}

ktest! {
    fn exited_threads_are_reaped() {
        let kva = crate::mm::kstack::kva_allocated();
        let t = sched::spawn("short", sched::Priority::KERNEL, || {}).unwrap();
        t.join();
        drop(t);
        // The reaper frees the stack once it runs (and any left over from
        // earlier tests, so the total may drop below the baseline).
        for _ in 0..100 {
            if crate::mm::kstack::kva_allocated() <= kva {
                return;
            }
            sched::yield_now();
        }
        panic!("stack not reaped: {:#x} bytes of KVA out, {kva:#x} before", crate::mm::kstack::kva_allocated());
    }
}

ktest! {
    fn clock_advances_and_sleeps_last() {
        let t0 = crate::time::now_ns();
        crate::time::sleep_ns(20_000_000);
        let slept = crate::time::now_ns() - t0;
        assert!(slept >= 20_000_000, "slept only {slept} ns");
        assert!(slept < 2_000_000_000, "slept {slept} ns");
        assert!(crate::time::ticks(hal::CpuId::BOOT) > 0, "timer interrupts arrive");
    }
}

ktest! {
    fn callouts_fire_in_order_and_cancel() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicU64;
        use crate::time::Callout;
        let log = Arc::new(AtomicU64::new(0));
        let q = Arc::new(sched::WaitQueue::new());
        let push = |digit: u64| {
            let (log, q) = (Arc::clone(&log), Arc::clone(&q));
            move || {
                let v = log.load(Ordering::Relaxed);
                log.store(v * 10 + digit, Ordering::Release);
                q.wake_all();
            }
        };
        Callout::after(30_000_000, push(3)).forget();
        Callout::after(10_000_000, push(1)).forget();
        let cancelled = Callout::after(20_000_000, push(9));
        Callout::after(20_000_000, push(2)).forget();
        assert!(cancelled.cancel());
        q.wait_until(|| log.load(Ordering::Acquire) >= 100);
        assert_eq!(log.load(Ordering::Relaxed), 123);
    }
}

ktest! {
    fn timed_waits_time_out() {
        use alloc::sync::Arc;
        let q = Arc::new(sched::WaitQueue::new());
        let t0 = crate::time::now_ns();
        assert!(!crate::time::wait_timeout(&q, || false, 15_000_000));
        assert!(crate::time::now_ns() - t0 >= 15_000_000);
        assert!(crate::time::wait_timeout(&q, || true, 15_000_000));
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
                sched::spawn_on("spinner", sched::Priority::KERNEL, Some(hal::CpuId::BOOT), move || {
                    while !stop.load(Ordering::Relaxed) {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                })
                .unwrap()
            })
            .collect();
        crate::time::sleep_ns(100_000_000);
        stop.store(true, Ordering::Relaxed);
        for t in &threads {
            t.join();
        }
        for (i, c) in counts.iter().enumerate() {
            assert!(c.load(Ordering::Relaxed) > 0, "spinner {i} never ran");
        }
        assert!(threads.iter().all(|t| t.switches() >= 2), "no preemption");
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
