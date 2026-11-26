// SPDX-License-Identifier: BSD-2-Clause
//! The SCMI channel against a model of the platform's side, written from DEN0056's power-domain
//! protocol: it answers in the shared memory when rung.

use core::cell::RefCell;
use std::vec::Vec;

use crate::scmi::{self, Channel, Doorbell, Error, Shmem};

struct Platform {
    mem: RefCell<[u32; 32]>,
    states: RefCell<Vec<u32>>,
    /// Messages seen: (protocol, message, token).
    seen: RefCell<Vec<(u32, u32, u32)>>,
    /// A domain that stays off whatever it is told.
    stuck: Option<u32>,
    /// What the call itself returns.
    x0: i64,
}

impl Platform {
    fn new() -> Self {
        let p = Self {
            mem: RefCell::new([0; 32]),
            states: RefCell::new(std::vec![scmi::STATE_OFF; 32]),
            seen: RefCell::new(Vec::new()),
            stuck: None,
            x0: 0,
        };
        // The channel starts free.
        p.mem.borrow_mut()[1] = 1;
        p
    }
    fn word(&self, off: usize) -> u32 {
        self.mem.borrow()[off / 4]
    }
    fn answer(&self, words: &[u32]) {
        let mut m = self.mem.borrow_mut();
        for (k, w) in words.iter().enumerate() {
            m[7 + k] = *w;
        }
        m[5] = u32::try_from(4 + 4 * words.len()).unwrap();
        m[1] = 1;
    }
}

impl Shmem for &Platform {
    fn read32(&self, off: usize) -> u32 {
        self.word(off)
    }
    fn write32(&self, off: usize, v: u32) {
        self.mem.borrow_mut()[off / 4] = v;
    }
}

impl Doorbell for &Platform {
    fn ring(&self) -> i64 {
        if self.x0 != 0 {
            return self.x0;
        }
        let hdr = self.word(0x18);
        let (msg, protocol, token) = (hdr & 0xFF, hdr >> 10 & 0xFF, hdr >> 18 & 0x3FF);
        self.seen.borrow_mut().push((protocol, msg, token));
        assert_eq!(
            self.word(0x04),
            0,
            "the agent marks the channel busy before ringing"
        );
        let arg = |k: usize| self.word(0x1C + 4 * k);
        match (protocol, msg) {
            (0x11, 0x0) => self.answer(&[0, 0x0002_0000]),
            (0x11, 0x1) => self.answer(&[0, 32]),
            (0x11, 0x4) => {
                assert_eq!(
                    self.word(0x14),
                    16,
                    "flags, domain, state: 12 bytes and the header"
                );
                assert_eq!(arg(0), 0, "synchronous");
                let (id, state) = (arg(1), arg(2));
                if id >= 32 {
                    self.answer(&[(-2i32).cast_unsigned()]);
                } else {
                    if self.stuck != Some(id) {
                        self.states.borrow_mut()[id as usize] = state;
                    }
                    self.answer(&[0]);
                }
            }
            (0x11, 0x5) => {
                let id = arg(0);
                match self.states.borrow().get(id as usize) {
                    Some(&s) => self.answer(&[0, s]),
                    None => self.answer(&[(-2i32).cast_unsigned()]),
                }
            }
            _ => self.answer(&[(-1i32).cast_unsigned()]),
        }
        0
    }
}

#[test]
fn the_version_and_domain_count_are_read() {
    let p = Platform::new();
    let mut ch = Channel::new(&p, &p);
    assert_eq!(ch.version(), Ok(0x0002_0000));
    assert_eq!(ch.domains(), Ok(32));
    // Tokens advance, the protocol is the power-domain one.
    assert_eq!(*p.seen.borrow(), [(0x11, 0, 0), (0x11, 1, 1)]);
}

#[test]
fn a_domain_is_switched_and_read_back() {
    let p = Platform::new();
    let mut ch = Channel::new(&p, &p);
    assert_eq!(ch.state(21), Ok(scmi::STATE_OFF));
    assert_eq!(ch.set_state(21, scmi::STATE_ON), Ok(()));
    assert_eq!(ch.state(21), Ok(scmi::STATE_ON));
    // The set is followed by a get.
    let msgs: Vec<u32> = p.seen.borrow().iter().map(|s| s.1).collect();
    assert_eq!(msgs, [5, 4, 5, 5]);
}

#[test]
fn a_domain_that_does_not_switch_is_an_error() {
    let mut p = Platform::new();
    p.stuck = Some(21);
    let mut ch = Channel::new(&p, &p);
    assert_eq!(
        ch.set_state(21, scmi::STATE_ON),
        Err(Error::NotReached(scmi::STATE_OFF))
    );
}

#[test]
fn platform_errors_are_reported() {
    let p = Platform::new();
    let mut ch = Channel::new(&p, &p);
    assert_eq!(ch.set_state(40, scmi::STATE_ON), Err(Error::Status(-2)));
    let mut q = Platform::new();
    q.x0 = -1;
    let mut ch = Channel::new(&q, &q);
    assert_eq!(ch.version(), Err(Error::Call(-1)));
}

#[test]
fn a_busy_channel_is_not_written() {
    let p = Platform::new();
    p.mem.borrow_mut()[1] = 0;
    let mut ch = Channel::new(&p, &p);
    assert_eq!(ch.version(), Err(Error::Busy));
    assert!(p.seen.borrow().is_empty());
    assert_eq!(p.word(0x18), 0);
}

#[test]
fn a_short_or_foreign_answer_is_refused() {
    struct Short<'a>(&'a Platform);
    impl Doorbell for Short<'_> {
        fn ring(&self) -> i64 {
            // Free, but only the header and status.
            self.0.answer(&[0]);
            0
        }
    }
    let p = Platform::new();
    let mut ch = Channel::new(&p, Short(&p));
    assert_eq!(ch.state(1), Err(Error::Channel));
}
