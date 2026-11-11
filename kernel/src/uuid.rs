// SPDX-License-Identifier: BSD-2-Clause
//! The system's UUID generator (`uuidgen(2)`): version-1 UUIDs from the
//! clock and a node, the first Ethernet address an interface registered
//! as it attached (`uuid_ether_add`), or a random multicast address until
//! one has (`guid::v1`).
//!
//! Behavior reference: sys/kern/kern_uuid.c.

use guid::v1::Generator;
use sync::SpinLock;

static GENERATOR: SpinLock<Generator> = SpinLock::new(Generator::new());

/// `arc4random()`: from the random device, or the clock before it is
/// seeded.
fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    if crate::random::fill(&mut b).is_err() {
        #[allow(clippy::cast_possible_truncation, reason = "the clock's low bits")]
        return crate::time::now_ns() as u32;
    }
    u32::from_ne_bytes(b)
}

/// Fills `out` with consecutive UUIDs (`kern_uuidgen`).
pub(crate) fn generate(out: &mut [[u8; 16]]) {
    let ns = crate::time::realtime_ns().unwrap_or_else(crate::time::now_ns);
    #[allow(clippy::cast_possible_truncation, reason = "below a second")]
    let now = guid::v1::clock(ns / 1_000_000_000, (ns % 1_000_000_000) as u32);
    // Random words are drawn before the lock: the generator takes at
    // most four (three for a made-up node, one for a new sequence).
    let words = [random_u32(), random_u32(), random_u32(), random_u32()];
    let mut next = words.iter().copied().cycle();
    let mut random = || next.next().unwrap_or(0);
    GENERATOR.lock().generate(now, &mut random, out);
}

/// An Ethernet interface attached with `addr` (`ether_ifattach`'s
/// `uuid_ether_add`); an unsuitable or repeated address is ignored.
pub(crate) fn ether_add(addr: [u8; 6]) {
    let _ = GENERATOR.lock().ether_add(addr);
}

/// An Ethernet interface with `addr` detached (`ether_ifdetach`'s
/// `uuid_ether_del`).
pub(crate) fn ether_del(addr: [u8; 6]) {
    let _ = GENERATOR.lock().ether_del(addr);
}
