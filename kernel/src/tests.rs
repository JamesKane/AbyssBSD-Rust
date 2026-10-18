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
pub(crate) fn run() -> ! {
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
        let hole = 0xffff_c000_0000_0000;
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
    fn consoles_present() {
        let info = crate::boot::info();
        assert!(info.serial.get().is_some(), "serial console");
        assert!(info.framebuffer.get().is_some(), "GOP framebuffer");
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
