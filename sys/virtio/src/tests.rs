// SPDX-License-Identifier: BSD-2-Clause
#![allow(clippy::cast_possible_truncation)] // test arena sizes and indices are small

extern crate std;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::NonNull;
use std::sync::Mutex;

use devtree::ids::PciIdent;
use pci::header::{Header, HeaderType};
use pci::{Bar, BarKind, Capability, ConfigSpace, Function, PciAddress};
use vm::PAGE_SIZE;
use vm::dma::{DmaConstraints, DmaMemory, DmaTag};

use crate::pci::{CapRef, modern_caps};
use crate::ring::{Ring, RingError, Used};

/// Fake physical memory: byte `pa` of a leaked arena is physical `pa`.
struct Fake {
    base: usize,
    size: u64,
    next: Mutex<u64>,
}

impl Fake {
    fn new(size: u64) -> &'static Self {
        let mem = vec![0u8; size as usize + PAGE_SIZE as usize].leak();
        let base = (mem.as_mut_ptr() as usize).next_multiple_of(PAGE_SIZE as usize);
        Box::leak(Box::new(Self {
            base,
            size,
            next: Mutex::new(0),
        }))
    }
}

// SAFETY: a bump allocator over the leaked arena: allocations are disjoint
// and never reused.
unsafe impl DmaMemory for Fake {
    fn alloc(&self, bytes: u64, align: u64, below: u64) -> Option<u64> {
        let mut next = self.next.lock().unwrap();
        let pa = next.next_multiple_of(align);
        (pa + bytes <= self.size.min(below)).then(|| {
            *next = pa + bytes;
            pa
        })
    }

    unsafe fn free(&self, _: u64, _: u64) {}

    fn virt(&self, pa: u64) -> NonNull<u8> {
        NonNull::new((self.base + pa as usize) as *mut u8).unwrap()
    }

    fn phys(&self, va: *const u8) -> Option<u64> {
        Some((va as usize - self.base) as u64)
    }

    fn clean(&self, _: *const u8, _: usize) {}

    fn invalidate(&self, _: *const u8, _: usize) {}
}

fn tag() -> DmaTag {
    DmaTag::new(Fake::new(1 << 20), DmaConstraints::UNLIMITED, 1 << 20)
}

/// A chain as the device sees it: its head, and each descriptor's
/// address, length, and flags.
type Chain = (u16, Vec<(u64, u32, u16)>);

/// Plays the device on a ring: takes what the driver made available and
/// completes it.
struct Device {
    seen: u16,
    used: u16,
}

impl Device {
    /// The chains made available since last time.
    fn take(&mut self, r: &Ring) -> Vec<Chain> {
        let m = r.memory();
        let n = usize::from(r.size());
        let avail = 16 * n;
        let idx = m.read_at::<u16>(avail + 2);
        let mut out = Vec::new();
        while self.seen != idx {
            let head = m.read_at::<u16>(avail + 4 + 2 * (usize::from(self.seen) % n));
            let mut chain = Vec::new();
            let mut d = head;
            loop {
                let off = 16 * usize::from(d);
                let flags = m.read_at::<u16>(off + 12);
                chain.push((m.read_at::<u64>(off), m.read_at::<u32>(off + 8), flags));
                if flags & 1 == 0 {
                    break;
                }
                d = m.read_at::<u16>(off + 14);
            }
            out.push((head, chain));
            self.seen = self.seen.wrapping_add(1);
        }
        out
    }

    /// Completes `id` with `len` bytes written.
    fn complete(&mut self, r: &Ring, id: u32, len: u32) {
        let m = r.memory();
        let n = usize::from(r.size());
        let used = (16 * n + 6 + 2 * n).next_multiple_of(4096);
        let at = used + 4 + 8 * (usize::from(self.used) % n);
        m.write_at::<u32>(at, id);
        m.write_at::<u32>(at + 4, len);
        self.used = self.used.wrapping_add(1);
        m.write_at::<u16>(used + 2, self.used);
    }
}

#[test]
fn layout_is_legacy_compatible() {
    let r = Ring::new(&tag(), 256).unwrap();
    assert_eq!(r.avail_addr() - r.desc_addr(), 16 * 256);
    assert_eq!(
        r.used_addr() - r.desc_addr(),
        8192,
        "used ring on the next page"
    );
    assert_eq!(r.used_addr() % 4096, 0);
    assert_eq!(Ring::bytes(256), 8192 + 6 + 8 * 256);
    assert!(Ring::new(&tag(), 6).is_err(), "sizes are powers of two");
}

#[test]
fn chains_go_round_and_come_back() {
    let mut r = Ring::new(&tag(), 4).unwrap();
    let mut dev = Device { seen: 0, used: 0 };
    // Many more chains than entries: indices wrap, descriptors recycle.
    for i in 0..50u64 {
        let next = r.next_head();
        let h = r
            .push(&[(0x1000 + i, 16)], &[(0x2000 + i, 512), (0x3000, 1)])
            .unwrap();
        assert_eq!(next, Some(h), "next_head names the head push returns");
        assert_eq!(r.free(), 1);
        let got = dev.take(&r);
        assert_eq!(got.len(), 1);
        let (head, chain) = &got[0];
        assert_eq!(*head, h);
        assert_eq!(
            chain
                .iter()
                .map(|&(a, l, f)| (a, l, f & 2 != 0))
                .collect::<Vec<_>>(),
            vec![
                (0x1000 + i, 16, false),
                (0x2000 + i, 512, true),
                (0x3000, 1, true)
            ]
        );
        assert!(r.needs_notify());
        assert!(!r.has_used());
        dev.complete(&r, u32::from(h), 100);
        assert!(r.has_used());
        assert_eq!(r.pop(), Ok(Some(Used { head: h, len: 100 })));
        assert_eq!(r.pop(), Ok(None));
        assert_eq!(r.free(), 4);
    }
}

#[test]
fn out_of_order_completion_and_full() {
    let mut r = Ring::new(&tag(), 4).unwrap();
    let mut dev = Device { seen: 0, used: 0 };
    let a = r.push(&[(1, 1)], &[(2, 8)]).unwrap();
    let b = r.push(&[], &[(3, 8)]).unwrap();
    assert_eq!(
        r.push(&[(4, 1)], &[(5, 1)]),
        Err(crate::Full),
        "one free, two needed"
    );
    assert_eq!(
        r.push(&[], &[]),
        Err(crate::Full),
        "empty chains are refused"
    );
    let c = r.push(&[(6, 1)], &[]).unwrap();
    assert_eq!(dev.take(&r).len(), 3);
    dev.complete(&r, u32::from(c), 0);
    dev.complete(&r, u32::from(a), 1_000_000);
    assert_eq!(
        r.pop(),
        Ok(Some(Used { head: c, len: 0 })),
        "nothing writable"
    );
    assert_eq!(
        r.pop(),
        Ok(Some(Used { head: a, len: 8 })),
        "a length beyond the writable buffers is clamped"
    );
    assert_eq!(r.free(), 3);
    dev.complete(&r, u32::from(b), 8);
    assert_eq!(r.pop(), Ok(Some(Used { head: b, len: 8 })));
    assert_eq!(r.free(), 4);
}

#[test]
fn hostile_device_is_refused() {
    let mut r = Ring::new(&tag(), 4).unwrap();
    let mut dev = Device { seen: 0, used: 0 };
    let a = r.push(&[(1, 1)], &[]).unwrap();
    dev.take(&r);
    // A head that is not outstanding, then one out of range.
    dev.complete(&r, u32::from(a) + 1, 0);
    assert_eq!(r.pop(), Err(RingError::BadHead(u32::from(a) + 1)));
    dev.complete(&r, 9999, 0);
    assert_eq!(r.pop(), Err(RingError::BadHead(9999)));
    // The genuine completion still works, once.
    dev.complete(&r, u32::from(a), 0);
    assert_eq!(r.pop(), Ok(Some(Used { head: a, len: 0 })));
    dev.complete(&r, u32::from(a), 0);
    assert_eq!(r.pop(), Err(RingError::BadIndex(4)), "nothing outstanding");
    assert_eq!(r.free(), 4);
}

#[test]
fn device_flags() {
    let mut r = Ring::new(&tag(), 8).unwrap();
    let used = r.used_addr() - r.desc_addr();
    r.memory().write_at::<u16>(used as usize, 1);
    assert!(!r.needs_notify(), "the device said not to notify");
    r.set_interrupts(false);
    assert_eq!(r.memory().read_at::<u16>(16 * 8), 1);
    r.set_interrupts(true);
    assert_eq!(r.memory().read_at::<u16>(16 * 8), 0);
}

#[test]
fn event_indices_decide_notifications() {
    let mut r = Ring::new(&tag(), 8).unwrap();
    r.set_event_idx(true);
    let n = 8usize;
    let used = (r.used_addr() - r.desc_addr()) as usize;
    let avail_event = used + 4 + 8 * n;
    // The flag no longer counts: the device asks by index.
    r.memory().write_at::<u16>(used, 1);
    // It wants to hear when entry 0 is made available.
    r.memory().write_at::<u16>(avail_event, 0);
    r.push(&[(1, 1)], &[]).unwrap();
    assert!(r.needs_notify(), "entry 0 published: notify");
    // Still wants entry 0, which it was told about: no notification.
    r.push(&[(1, 1)], &[]).unwrap();
    assert!(
        !r.needs_notify(),
        "the device has not caught up to its event"
    );
    // It asks for entry 2; entries 2 and 3 published in one batch.
    r.memory().write_at::<u16>(avail_event, 2);
    r.push(&[(1, 1)], &[]).unwrap();
    r.push(&[(1, 1)], &[]).unwrap();
    assert!(r.needs_notify(), "the batch passed the event: notify once");
    assert!(!r.needs_notify(), "nothing new");
}

#[test]
fn event_indices_rearm_interrupts() {
    let mut r = Ring::new(&tag(), 4).unwrap();
    r.set_event_idx(true);
    let mut dev = Device { seen: 0, used: 0 };
    let used_event = 16 * 4 + 4 + 2 * 4;
    // Wrap the ring several times: after catching up, the driver always
    // asks for an interrupt at its next completion.
    for i in 0..20u32 {
        let h = r.push(&[], &[(1, 8)]).unwrap();
        for (head, _) in dev.take(&r) {
            dev.complete(&r, u32::from(head), 8);
        }
        assert_eq!(r.pop().unwrap().map(|u| u.head), Some(h), "completion {i}");
        assert_eq!(r.pop().unwrap(), None);
        assert_eq!(
            r.memory().read_at::<u16>(used_event),
            dev.used,
            "used_event after {i}"
        );
    }
    // Caught up, with the event written: the next completion is found.
    let h = r.push(&[], &[(1, 8)]).unwrap();
    for (head, _) in dev.take(&r) {
        dev.complete(&r, u32::from(head), 8);
    }
    assert_eq!(r.pop().unwrap().map(|u| u.head), Some(h));
}

#[test]
fn rearm_reports_completions_that_raced_it() {
    let mut r = Ring::new(&tag(), 4).unwrap();
    let mut dev = Device { seen: 0, used: 0 };
    r.set_interrupts(false);
    assert!(r.rearm(), "nothing pending: interrupts back on");
    assert_eq!(r.memory().read_at::<u16>(16 * 4), 0);
    let h = r.push(&[], &[(1, 8)]).unwrap();
    r.set_interrupts(false);
    for (head, _) in dev.take(&r) {
        dev.complete(&r, u32::from(head), 8);
    }
    assert!(!r.rearm(), "a completion landed while interrupts were off");
    assert_eq!(r.pop().unwrap().map(|u| u.head), Some(h));
    assert!(r.rearm());
}

#[test]
fn interrupt_mask_suppresses_until_rearm() {
    let mut r = Ring::new(&tag(), 4).unwrap();
    let flags = 16 * 4;
    let mask = r.interrupt_mask();
    mask.suppress();
    assert_eq!(r.memory().read_at::<u16>(flags), 1, "NO_INTERRUPT");
    assert!(r.rearm());
    assert_eq!(r.memory().read_at::<u16>(flags), 0);
    // With event indices the flags stay 0 (virtio 1.2 §2.7.7).
    r.set_event_idx(true);
    let mask = r.interrupt_mask();
    mask.suppress();
    r.set_interrupts(false);
    assert_eq!(r.memory().read_at::<u16>(flags), 0);
}

/// One function's configuration space.
struct Cfg(RefCell<[u8; 256]>);

impl ConfigSpace for Cfg {
    fn read32(&self, _: PciAddress, offset: u16) -> u32 {
        let b = self.0.borrow();
        let o = usize::from(offset);
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    fn write32(&self, _: PciAddress, offset: u16, v: u32) {
        let o = usize::from(offset);
        self.0.borrow_mut()[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// A modern function with vendor capabilities `caps` (type, bar, offset,
/// length) chained from 0x40, and a 16 KiB memory BAR 4.
fn function(caps: &[(u8, u8, u32, u32)]) -> (Cfg, Function) {
    let mut space = [0u8; 256];
    let mut list = Vec::new();
    for (i, &(kind, bar, offset, len)) in caps.iter().enumerate() {
        let at = 0x40 + 20 * i;
        let next = if i + 1 == caps.len() { 0 } else { at + 20 };
        space[at..at + 4].copy_from_slice(&[0x09, next as u8, 20, kind]);
        space[at + 4] = bar;
        space[at + 8..at + 12].copy_from_slice(&offset.to_le_bytes());
        space[at + 12..at + 16].copy_from_slice(&len.to_le_bytes());
        space[at + 16..at + 20].copy_from_slice(&4u32.to_le_bytes());
        list.push(Capability {
            id: 0x09,
            offset: at as u8,
        });
    }
    let f = Function {
        addr: PciAddress {
            domain: 0,
            bus: 0,
            slot: 3,
            function: 0,
        },
        header: Header {
            ident: PciIdent {
                vendor: 0x1af4,
                device: 0x1044,
                subvendor: 0x1af4,
                subdevice: 0x1100,
                class: 0x00ff_0000,
                revision: 1,
            },
            kind: HeaderType::Device,
            multifunction: false,
            interrupt_pin: 1,
        },
        bars: vec![Bar {
            index: 4,
            kind: BarKind::Memory64,
            base: 0x8000_0000,
            size: 0x4000,
            prefetchable: true,
        }],
        bad_bars: Vec::new(),
        caps: list,
        ext_caps: Vec::new(),
        bridge: None,
        depth: 0,
    };
    (Cfg(RefCell::new(space)), f)
}

#[test]
fn modern_capabilities_as_qemu_lays_them_out() {
    let (cfg, f) = function(&[
        (1, 4, 0x0000, 0x1000),
        (3, 4, 0x1000, 0x1000),
        (4, 4, 0x2000, 0x1000),
        (2, 4, 0x3000, 0x1000),
    ]);
    let c = modern_caps(|at| cfg.read32(f.addr, at), &f).expect("all structures found");
    assert_eq!(
        c.common,
        CapRef {
            bar: 4,
            offset: 0,
            len: 0x1000
        }
    );
    assert_eq!(c.isr.offset, 0x1000);
    assert_eq!(c.device.map(|d| d.offset), Some(0x2000));
    assert_eq!(c.notify.offset, 0x3000);
    assert_eq!(c.notify_multiplier, 4);
}

#[test]
fn modern_capabilities_are_validated() {
    // Common configuration outside its BAR, then a good one: the first
    // that fits wins.
    let (cfg, f) = function(&[
        (1, 4, 0x3f00, 0x1000),
        (1, 4, 0x0000, 0x38),
        (3, 4, 0x1000, 1),
        (2, 4, 0x2000, 2),
    ]);
    let c = modern_caps(|at| cfg.read32(f.addr, at), &f).unwrap();
    assert_eq!(c.common.offset, 0);
    assert_eq!(c.device, None, "the device configuration is optional");
    // A BAR that is not implemented, a common structure too small for its
    // registers: nothing usable, so the modern driver declines.
    let (cfg, f) = function(&[(1, 2, 0, 0x1000), (3, 4, 0x1000, 1), (2, 4, 0x2000, 2)]);
    assert_eq!(modern_caps(|at| cfg.read32(f.addr, at), &f), None);
    let (cfg, f) = function(&[(1, 4, 0, 0x37), (3, 4, 0x1000, 1), (2, 4, 0x2000, 2)]);
    assert_eq!(modern_caps(|at| cfg.read32(f.addr, at), &f), None);
    // Legacy devices have no vendor capabilities at all.
    let (cfg, f) = function(&[]);
    assert_eq!(modern_caps(|at| cfg.read32(f.addr, at), &f), None);
}

#[test]
fn names_match_freebsd() {
    assert_eq!(crate::device_name(2), Some("Block"));
    assert_eq!(crate::device_name(4), Some("Entropy"));
    assert_eq!(crate::device_name(9), Some("9P Transport"));
    assert_eq!(crate::device_name(10), None);
}
