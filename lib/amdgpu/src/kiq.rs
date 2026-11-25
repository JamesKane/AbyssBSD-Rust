// SPDX-License-Identifier: BSD-2-Clause
//! The kernel interface queue (KIQ, 06 section 9): a compute queue the
//! driver programs register by register, through which every other queue is
//! then mapped by packets. Stage R4.4 of docs/GRAPHICS.md §16.

use crate::gc;
use crate::mqd::{self, hqd_register};
use crate::regs::Window;

/// `RLC_CP_SCHEDULERS`: which hardware queue is the KIQ (bits 7:0).
pub const RLC_CP_SCHEDULERS: usize = 0x3b284;
/// `CP_MEC_CNTL`: halt bits 30 (ME1) and 28 (ME2); 0 runs both.
pub const CP_MEC_CNTL: usize = 0x86d4;
/// `CP_PQ_WPTR_POLL_CNTL`: bit 31 enables write-pointer polling.
pub const CP_PQ_WPTR_POLL_CNTL: usize = 0xc20c;
/// `CP_MEC_DOORBELL_RANGE_LOWER` (a byte offset in the doorbell aperture).
pub const CP_MEC_DOORBELL_RANGE_LOWER: usize = 0xc170;
/// `CP_MEC_DOORBELL_RANGE_UPPER`.
pub const CP_MEC_DOORBELL_RANGE_UPPER: usize = 0xc174;
/// `CP_HQD_ACTIVE` of the selected queue.
pub const CP_HQD_ACTIVE: usize = hqd_register(mqd::field::ACTIVE);
/// The dequeue request.
pub const CP_HQD_DEQUEUE_REQUEST: usize = hqd_register(mqd::field::DEQUEUE_REQUEST);
/// `CP_HQD_PQ_DOORBELL_CONTROL`.
pub const CP_HQD_PQ_DOORBELL_CONTROL: usize = hqd_register(mqd::field::DOORBELL_CONTROL);

/// The KIQ's place: micro engine 2, pipe 1, queue 0.
pub const KIQ_ME: u32 = 2;
/// Its pipe.
pub const KIQ_PIPE: u32 = 1;
/// Its queue.
pub const KIQ_QUEUE: u32 = 0;
/// `RLC_CP_SCHEDULERS`' low byte for it: enable, ME 2 << 5, pipe 1 << 3.
pub const KIQ_SCHEDULER_BYTE: u32 = 0xc8;
/// The KIQ's doorbell (a dword index in the doorbell aperture).
pub const KIQ_DOORBELL_DWORD: u32 = 0;
/// The last doorbell the compute queues use, as a byte offset (06 section
/// 9.2, 05 section 10.3).
pub const MEC_DOORBELL_UPPER_BYTES: u32 = 0x450;

/// Programs the KIQ from its MQD (06 section 9.2), then runs the MEC: the
/// scheduler byte, the queue selected, polling off, an active queue
/// dequeued, the doorbell cleared, the queue's registers, the doorbell
/// range, the doorbell control and the rest, active; the selection put
/// back; the MEC unhalted. `pause_us` is called with 1 while a dequeue is
/// waited for. Returns whether the queue read back as active.
pub fn start<W: Window + ?Sized>(w: &W, mqd: &[u32], pause_us: &dyn Fn(u32)) -> bool {
    let sched = w.read32(RLC_CP_SCHEDULERS);
    w.write32(RLC_CP_SCHEDULERS, (sched & !0xff) | KIQ_SCHEDULER_BYTE);
    gc::select(w, KIQ_ME, KIQ_PIPE, KIQ_QUEUE, 0);
    // No write-pointer polling.
    w.write32(
        CP_PQ_WPTR_POLL_CNTL,
        w.read32(CP_PQ_WPTR_POLL_CNTL) & !(1 << 31),
    );
    if w.read32(CP_HQD_ACTIVE) & 1 != 0 {
        w.write32(CP_HQD_DEQUEUE_REQUEST, 1);
        for _ in 0..1000 {
            if w.read32(CP_HQD_ACTIVE) & 1 == 0 {
                break;
            }
            pause_us(1);
        }
    }
    w.write32(CP_HQD_PQ_DOORBELL_CONTROL, 0);
    let writes = mqd::hqd_writes(mqd);
    for (reg, v) in &writes.queue {
        w.write32(*reg, *v);
    }
    w.write32(CP_MEC_DOORBELL_RANGE_LOWER, KIQ_DOORBELL_DWORD * 4);
    w.write32(CP_MEC_DOORBELL_RANGE_UPPER, MEC_DOORBELL_UPPER_BYTES);
    for (reg, v) in &writes.start {
        w.write32(*reg, *v);
    }
    w.write32(CP_HQD_ACTIVE, 1);
    let active = w.read32(CP_HQD_ACTIVE) & 1 != 0;
    gc::select(w, 0, 0, 0, 0);
    // The micro engines run.
    w.write32(CP_MEC_CNTL, 0);
    active
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqd::{Queue, compute_mqd};
    use core::cell::RefCell;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    struct Regs {
        regs: RefCell<BTreeMap<usize, u32>>,
        log: RefCell<Vec<(usize, u32)>>,
    }

    impl Regs {
        fn new() -> Self {
            Self {
                regs: RefCell::new(BTreeMap::new()),
                log: RefCell::new(Vec::new()),
            }
        }
        fn get(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
    }

    impl Window for Regs {
        fn read32(&self, o: usize) -> u32 {
            self.get(o)
        }
        fn write32(&self, o: usize, v: u32) {
            self.log.borrow_mut().push((o, v));
            self.regs.borrow_mut().insert(o, v);
        }
    }

    fn mqd() -> Vec<u32> {
        compute_mqd(&Queue {
            mqd: 0xf4_0ab0_1000,
            ring: 0xf4_0ab0_0000,
            ring_bytes: 4096,
            rptr_report: 0xf4_0ab0_3000,
            wptr_poll: 0xf4_0ab0_3100,
            doorbell_dword: 0,
            eop: 0xf4_0ab0_2000,
        })
    }

    #[test]
    fn the_kiq_is_programmed_in_the_specifications_order() {
        let w = Regs::new();
        w.regs.borrow_mut().insert(RLC_CP_SCHEDULERS, 0xabcd_ef00);
        w.regs
            .borrow_mut()
            .insert(CP_PQ_WPTR_POLL_CNTL, 0x8000_0010);
        w.regs.borrow_mut().insert(CP_MEC_CNTL, 0x5000_0000);
        assert!(start(&w, &mqd(), &|_| {}));
        let log = w.log.borrow();
        let first = |r: usize| log.iter().position(|(x, _)| *x == r).unwrap();
        let last = |r: usize| log.iter().rposition(|(x, _)| *x == r).unwrap();
        // The scheduler byte keeps the upper bits.
        assert_eq!(w.get(RLC_CP_SCHEDULERS), 0xabcd_efc8);
        // The queue selected (ME 2, pipe 1) before anything is programmed.
        assert!(log.contains(&(gc::GRBM_GFX_CNTL, 1 | (2 << 2))));
        // Polling off, other bits kept.
        assert_eq!(w.get(CP_PQ_WPTR_POLL_CNTL), 0x0000_0010);
        // Order: scheduler, poll off, doorbell cleared, queue registers,
        // doorbell range, doorbell control, active, selection back, MEC.
        assert!(first(RLC_CP_SCHEDULERS) < first(CP_PQ_WPTR_POLL_CNTL));
        assert!(first(CP_PQ_WPTR_POLL_CNTL) < first(CP_HQD_PQ_DOORBELL_CONTROL));
        assert!(first(0xc8b8) < first(CP_MEC_DOORBELL_RANGE_LOWER));
        assert!(first(CP_MEC_DOORBELL_RANGE_UPPER) < last(CP_HQD_PQ_DOORBELL_CONTROL));
        assert!(last(CP_HQD_PQ_DOORBELL_CONTROL) < first(CP_HQD_ACTIVE));
        assert_eq!(*log.last().unwrap(), (CP_MEC_CNTL, 0));
        assert_eq!(log[log.len() - 2], (gc::GRBM_GFX_CNTL, 0));
        // The range and the doorbell control.
        assert_eq!(w.get(CP_MEC_DOORBELL_RANGE_LOWER), 0);
        assert_eq!(w.get(CP_MEC_DOORBELL_RANGE_UPPER), 0x450);
        assert_eq!(w.get(CP_HQD_PQ_DOORBELL_CONTROL), 1 << 30);
        assert_eq!(w.get(CP_HQD_ACTIVE), 1);
    }

    #[test]
    fn an_active_queue_is_dequeued_first() {
        let w = Regs::new();
        w.regs.borrow_mut().insert(CP_HQD_ACTIVE, 1);
        // The model never clears it by itself: the wait is bounded.
        let waited = RefCell::new(0u32);
        start(&w, &mqd(), &|us| *waited.borrow_mut() += us);
        assert!(w.log.borrow().contains(&(CP_HQD_DEQUEUE_REQUEST, 1)));
        assert_eq!(*waited.borrow(), 1000);
    }
}
