// SPDX-License-Identifier: BSD-2-Clause
#![allow(clippy::cast_possible_truncation)] // test sizes and indices are small

extern crate std;

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::context::*;
use crate::event::Event;
use crate::regs::{self, Params, Protocol, portsc};
use crate::ring::*;
use crate::trb::*;

/// Ring memory in a vector, at a made-up bus address.
struct Mem {
    base: u64,
    slots: RefCell<Vec<[u32; 4]>>,
}

impl Mem {
    fn new(n: usize) -> Self {
        Self {
            base: 0x1_0000,
            slots: RefCell::new(vec![[0xdead_beef; 4]; n]),
        }
    }
}

impl TrbMemory for &Mem {
    fn slots(&self) -> usize {
        self.slots.borrow().len()
    }
    fn bus_addr(&self) -> u64 {
        self.base
    }
    fn read(&self, i: usize) -> Trb {
        Trb(self.slots.borrow()[i])
    }
    fn write_body(&self, i: usize, b: [u32; 3]) {
        let mut s = self.slots.borrow_mut();
        s[i][..3].copy_from_slice(&b);
    }
    fn write_control(&self, i: usize, c: u32) {
        self.slots.borrow_mut()[i][3] = c;
    }
}

/// The controller's side of a producer ring: follows cycle bits and link
/// TRBs, as xHCI 1.2 §4.9.2 describes.
struct Consumer {
    dequeue: usize,
    cycle: bool,
}

impl Consumer {
    fn new() -> Self {
        Self {
            dequeue: 0,
            cycle: true,
        }
    }

    /// The next TRB the controller would execute, with its address.
    fn next(&mut self, m: &Mem) -> Option<(u64, Trb)> {
        loop {
            let t = Trb(m.slots.borrow()[self.dequeue]);
            if t.cycle() != self.cycle {
                return None;
            }
            if t.trb_type() == Some(TrbType::Link) {
                assert_eq!(t.param(), m.base, "link goes back to the start");
                if t.control() & flag::TOGGLE_CYCLE != 0 {
                    self.cycle = !self.cycle;
                }
                self.dequeue = 0;
                continue;
            }
            let at = m.base + self.dequeue as u64 * 16;
            self.dequeue += 1;
            return Some((at, t));
        }
    }
}

fn normal(n: u64, chain: bool) -> Trb {
    let f = if chain { flag::CHAIN } else { flag::IOC };
    transfer::normal(n, 8, 0, f, 0)
}

#[test]
fn producer_and_consumer_agree_across_wraps() {
    let m = Mem::new(8);
    let mut r: ProducerRing<&Mem, u32> = ProducerRing::new(&m);
    let mut c = Consumer::new();
    assert_eq!(r.free(), 7);
    let mut sizes = [1usize, 2, 3, 1, 4, 2, 5, 1, 3].iter().cycle();
    for tag in 0..200u32 {
        let len = *sizes.next().unwrap();
        let trbs: Vec<Trb> = (0..len)
            .map(|k| normal(u64::from(tag) * 100 + k as u64, k + 1 < len))
            .collect();
        let p = r.push(&trbs, tag).unwrap();
        // The controller sees exactly this chain, in order.
        let mut last = 0;
        for (k, want) in trbs.iter().enumerate() {
            let (at, got) = c.next(&m).expect("chain visible");
            assert_eq!(got.param(), want.param());
            assert_eq!(got.control() & flag::CHAIN != 0, k + 1 < len);
            if k == 0 {
                assert_eq!(at, p.first);
            }
            last = at;
        }
        assert_eq!(last, p.last);
        assert!(c.next(&m).is_none(), "nothing past the chain");
        let done = r.complete(last).unwrap();
        assert_eq!(
            done,
            Matched {
                tag,
                trb: len - 1,
                len
            }
        );
        assert!(r.is_idle());
        assert_eq!(r.free(), 7);
    }
}

#[test]
fn a_chain_crossing_the_link_chains_the_link() {
    let m = Mem::new(4);
    let mut r: ProducerRing<&Mem, u8> = ProducerRing::new(&m);
    let mut c = Consumer::new();
    r.push(&[normal(1, false)], 1).unwrap();
    let (a, _) = c.next(&m).unwrap();
    r.complete(a).unwrap();
    // Two slots left before the link; a three-TRB chain crosses it.
    let p = r
        .push(&[normal(2, true), normal(3, true), normal(4, false)], 2)
        .unwrap();
    let link = Trb(m.slots.borrow()[3]);
    assert!(link.control() & flag::CHAIN != 0);
    let got: Vec<u64> = core::iter::from_fn(|| c.next(&m).map(|(_, t)| t.param())).collect();
    assert_eq!(got, [2, 3, 4]);
    assert_eq!(p.last, m.base);
    // An event for the chain's middle TRB (a short packet) completes it.
    let m2 = r.complete(m.base + 2 * 16).unwrap();
    assert_eq!((m2.tag, m2.trb), (2, 1));
    // A second event for the same chain (its last TRB) matches nothing.
    assert!(r.complete(p.last).is_none());
    assert!(r.is_idle());
}

#[test]
fn a_full_ring_refuses_until_completions() {
    let m = Mem::new(4);
    let mut r: ProducerRing<&Mem, u8> = ProducerRing::new(&m);
    let a = r.push(&[normal(1, false)], 1).unwrap();
    let b = r.push(&[normal(2, true), normal(3, false)], 2).unwrap();
    assert_eq!(r.push(&[normal(4, false)], 3), Err(PushError::Full));
    assert_eq!(r.push(&[], 3), Err(PushError::BadLength));
    assert_eq!(r.push(&[normal(4, false); 4], 3), Err(PushError::BadLength));
    // Completing out of order frees nothing until the front completes.
    assert_eq!(r.complete(b.last).unwrap().tag, 2);
    assert_eq!(r.free(), 0);
    assert_eq!(r.complete(a.last).unwrap().tag, 1);
    assert_eq!(r.free(), 3);
}

#[test]
fn the_first_trb_is_handed_over_last() {
    // A memory that records the order of control-word writes.
    struct Log<'a>(&'a Mem, RefCell<Vec<usize>>);
    impl TrbMemory for &Log<'_> {
        fn slots(&self) -> usize {
            (&self.0).slots()
        }
        fn bus_addr(&self) -> u64 {
            self.0.base
        }
        fn read(&self, i: usize) -> Trb {
            (&self.0).read(i)
        }
        fn write_body(&self, i: usize, b: [u32; 3]) {
            (&self.0).write_body(i, b);
        }
        fn write_control(&self, i: usize, c: u32) {
            self.1.borrow_mut().push(i);
            (&self.0).write_control(i, c);
        }
    }
    let m = Mem::new(8);
    let log = Log(&m, RefCell::new(Vec::new()));
    let mut r: ProducerRing<&Log<'_>, u8> = ProducerRing::new(&log);
    log.1.borrow_mut().clear();
    r.push(&[normal(1, true), normal(2, true), normal(3, false)], 0)
        .unwrap();
    assert_eq!(*log.1.borrow(), [1, 2, 0]);
}

#[test]
fn drain_returns_outstanding_tags_and_dequeue_points_past_them() {
    let m = Mem::new(8);
    let mut r: ProducerRing<&Mem, u8> = ProducerRing::new(&m);
    r.push(&[normal(1, false)], 1).unwrap();
    r.push(&[normal(2, false)], 2).unwrap();
    let (dq, cycle) = r.dequeue();
    assert_eq!((dq, cycle), (m.base + 32, true));
    let tags: Vec<u8> = r.drain().collect();
    assert_eq!(tags, [1, 2]);
    assert_eq!(r.free(), 7);
}

#[test]
fn event_ring_follows_the_cycle_bit() {
    let m = Mem::new(4);
    let mut e = EventRing::new(&m);
    assert!(e.pop().is_none());
    let produce = |i: usize, cycle: bool, port: u8| {
        let mut t = Trb::new(TrbType::PortStatusChange, u64::from(port) << 24, 1 << 24, 0);
        if cycle {
            t.0[3] |= flag::CYCLE;
        }
        m.slots.borrow_mut()[i] = t.0;
    };
    for i in 0..4 {
        produce(i, true, i as u8 + 1);
    }
    for i in 0..4 {
        let t = e.pop().unwrap();
        assert_eq!(Event::decode(&t), Event::PortStatusChange { port: i + 1 });
    }
    // Wrapped: the consumer now waits for cycle 0; stale 1s are not new.
    assert!(e.pop().is_none());
    assert_eq!(e.erdp(), m.base);
    produce(0, false, 9);
    assert_eq!(
        Event::decode(&e.pop().unwrap()),
        Event::PortStatusChange { port: 9 }
    );
    assert_eq!(e.erdp(), m.base + 16);
}

#[test]
fn events_decode() {
    let mut t = Trb::new(
        TrbType::TransferEvent,
        0x1234_5670,
        (13 << 24) | 5,
        (3 << 24) | (4 << 16),
    );
    assert_eq!(
        Event::decode(&t),
        Event::Transfer {
            pointer: 0x1234_5670,
            residue: 5,
            code: CompletionCode::ShortPacket,
            event_data: false,
            slot: 3,
            endpoint: 4,
        }
    );
    t = Trb::new(TrbType::CommandCompletion, 0x80, (1 << 24) | 7, 2 << 24);
    assert_eq!(
        Event::decode(&t),
        Event::Command {
            pointer: 0x80,
            code: CompletionCode::Success,
            slot: 2,
            parameter: 7,
        }
    );
    assert_eq!(
        Event::decode(&Trb([0, 0, 0, 63 << 10])),
        Event::Other { trb_type: 63 }
    );
}

#[test]
fn fuzzed_events_and_completions_never_panic() {
    // A small xorshift: deterministic garbage.
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let m = Mem::new(16);
    let mut r: ProducerRing<&Mem, u32> = ProducerRing::new(&m);
    let mut outstanding = Vec::new();
    for i in 0..20_000u32 {
        let w = [rnd() as u32, rnd() as u32, rnd() as u32, rnd() as u32];
        let _ = Event::decode(&Trb(w));
        let _ = SlotContext::decode(&[w[0], w[1], w[2], w[3], 0, 0, 0, 0]);
        let _ = EndpointContext::decode(&[w[3], w[2], w[1], w[0], w[1], 0, 0, 0]);
        if rnd() % 3 == 0 {
            let len = (rnd() % 4 + 1) as usize;
            let trbs = vec![normal(0, false); len];
            if let Ok(p) = r.push(&trbs, i) {
                outstanding.push(p.last);
            }
        }
        // Garbage pointers, and sometimes a real one.
        let ptr = if rnd() % 2 == 0 && !outstanding.is_empty() {
            outstanding.swap_remove((rnd() as usize) % outstanding.len())
        } else {
            m.base.wrapping_add(rnd() % 400)
        };
        let _ = r.complete(ptr);
        assert!(r.free() <= 15);
    }
}

#[test]
fn setup_trb_layout() {
    let p = SetupPacket {
        request_type: 0x80,
        request: 6,
        value: 0x0100,
        index: 0,
        length: 18,
    };
    let t = transfer::setup(&p, 0);
    assert_eq!(t.param(), 0x0012_0000_0100_0680);
    assert_eq!(t.status(), 8);
    assert_eq!(t.trb_type(), Some(TrbType::Setup));
    assert_eq!((t.control() >> 16) & 3, 3, "IN data stage");
    assert!(t.control() & flag::IDT != 0);
    let s = transfer::status_stage(Some(true), 0);
    assert_eq!(s.control() & flag::DIR_IN, 0, "status is OUT after IN data");
    let s = transfer::status_stage(None, 0);
    assert_ne!(s.control() & flag::DIR_IN, 0);
}

#[test]
fn td_size_counts_packets_left() {
    assert_eq!(transfer::td_size(0, 512, true), 0);
    assert_eq!(transfer::td_size(1024, 512, false), 2);
    assert_eq!(transfer::td_size(1025, 512, false), 3);
    assert_eq!(transfer::td_size(1 << 20, 512, false), 31);
}

#[test]
fn commands_layout() {
    let t = command::address_device(5, 0x2000, true);
    assert_eq!(t.slot(), 5);
    assert_eq!(t.param(), 0x2000);
    assert!(t.control() & flag::BSR != 0);
    let t = command::set_tr_dequeue(5, 3, 0x3000, true, 0);
    assert_eq!(t.param(), 0x3001);
    assert_eq!(t.endpoint(), 3);
    let t = command::enable_slot(0);
    assert_eq!(t.trb_type(), Some(TrbType::EnableSlot));
}

#[test]
fn contexts_round_trip() {
    let s = SlotContext {
        route: 0x12345,
        speed: Speed::High.psiv(),
        mtt: true,
        hub: true,
        entries: 3,
        root_port: 2,
        ports: 4,
        tt_slot: 7,
        tt_port: 3,
        ttt: 2,
        interrupter: 1,
        address: 9,
        state: 2,
    };
    assert_eq!(SlotContext::decode(&s.encode()), s);
    let e = EndpointContext {
        state: 1,
        mult: 0,
        max_primary_streams: 4,
        lsa: true,
        interval: 7,
        error_count: 3,
        ep_type: Some(EndpointType::BulkIn),
        max_burst: 15,
        max_packet: 1024,
        dequeue: 0xdead_bee0,
        dcs: true,
        average_trb_length: 3072,
        max_esit_payload: 0x12_3456,
    };
    assert_eq!(EndpointContext::decode(&e.encode()), e);
}

#[test]
fn context_offsets() {
    let ep2in = Dci::from_address(0x82, false).unwrap();
    assert_eq!(ep2in.get(), 5);
    assert_eq!(Dci::from_address(0x01, false).unwrap().get(), 2);
    assert_eq!(Dci::from_address(0x80, false), Some(Dci::CONTROL));
    assert_eq!(ContextSize::Bytes32.input_endpoint(Dci::CONTROL), 64);
    assert_eq!(ContextSize::Bytes64.device_endpoint(ep2in), 320);
    assert_eq!(ContextSize::Bytes64.input_bytes(), 33 * 64);
    assert_eq!(Dci::new(0), None);
    assert_eq!(Dci::new(32), None);
}

#[test]
fn intervals() {
    use EndpointType::*;
    // High speed: bInterval 4 is 2^3 microframes.
    assert_eq!(interval(Speed::High, InterruptIn, 4), 3);
    // Full speed interrupt every 10 ms: 80 microframes, rounded down to 64.
    assert_eq!(interval(Speed::Full, InterruptIn, 10), 6);
    // Low speed, 255 ms: capped at 2^10.
    assert_eq!(interval(Speed::Low, InterruptIn, 255), 10);
    assert_eq!(interval(Speed::Full, InterruptIn, 1), 3);
    assert_eq!(interval(Speed::Full, IsochIn, 1), 3);
    assert_eq!(interval(Speed::Super, BulkIn, 9), 0);
}

#[test]
fn portsc_writes_never_disable_or_clear_by_accident() {
    let current = portsc::CCS
        | portsc::PED
        | portsc::PP
        | portsc::CSC
        | portsc::PRC
        | (3 << portsc::SPEED_SHIFT);
    // Clearing CSC alone keeps power and leaves PED and PRC unwritten.
    assert_eq!(
        portsc::write(current, portsc::CSC),
        portsc::PP | portsc::CSC
    );
    // Starting a reset carries no change bits.
    assert_eq!(portsc::write(current, portsc::PR), portsc::PP | portsc::PR);
    // PED in the action is dropped.
    assert_eq!(portsc::write(current, portsc::PED), portsc::PP);
    assert_eq!(portsc::speed(current), 3);
}

#[test]
fn params_and_protocols() {
    let p = Params::decode(
        (32 << 24) | (8 << 8) | 64,
        (1 << 27) | (2 << 21) | (15 << 4),
        (0x50 << 16) | (1 << 2) | 1,
    );
    assert_eq!(p.max_slots, 64);
    assert_eq!(p.max_interrupters, 8);
    assert_eq!(p.max_ports, 32);
    assert_eq!(p.scratchpads, (2 << 5) | 1);
    assert!(p.ac64 && p.csz && !p.ppc);
    assert_eq!(p.xecp, 0x140);
    let proto = Protocol::decode([0x0300_0002, 0x2042_5355, (4 << 8) | 5, 0]);
    assert_eq!((proto.major, proto.first_port, proto.count), (3, 5, 4));
    assert!(proto.has(5) && proto.has(8) && !proto.has(9) && !proto.has(4));
}

#[test]
fn ext_caps_walk_and_stop() {
    // Byte 0 is unused (an offset of 0 means no list).
    let mut words = [0u32; 16];
    words[4] = 0x0000_0401; // legacy support at 16, next 4 words on
    words[8] = 0x0000_0002; // a protocol at 32, the last
    let read = |at: usize| words[at / 4];
    let caps: Vec<(usize, u8)> = regs::ext_caps(16, 64, read).collect();
    assert_eq!(caps, [(16, 1), (32, 2)]);
    assert_eq!(regs::ext_caps(0, 64, read).count(), 0);
    // A list that runs off the end, and an out-of-range start, both stop.
    let long = [0x0000_0101u32; 16];
    assert_eq!(regs::ext_caps(4, 64, |at| long[at / 4]).count(), 15);
    assert_eq!(regs::ext_caps(64, 64, read).count(), 0);
}

#[test]
fn xecp_names_print_as_freebsd() {
    let s = |ids: &[u8]| alloc::format!("{}", regs::XecpNames(ids));
    assert_eq!(s(&[2, 2]), "PROTO,PROTO");
    assert_eq!(s(&[1, 2, 10, 17]), "LEGACY,PROTO,DEBUG,EXT MSI");
    assert_eq!(s(&[0xc0, 0x0b, 7, 3]), "VEND(c0),RES(b),,POWER");
    assert_eq!(s(&[]), "");
}
