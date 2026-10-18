// SPDX-License-Identifier: BSD-2-Clause
//! Robustness: deterministic mutations of the corpus DSDTs (byte flips,
//! truncation, splices, insertions, deletions), each loaded and then
//! exercised through every device's `_STA`, `_HID`, `_CRS`, and `_PRT`.
//! Errors are expected; panics, hangs, and stack overflows are bugs.
//!
//! `AML_FUZZ_ITERS` overrides the number of mutations per DSDT, and
//! `AML_FUZZ_SEED` the starting seed, for longer runs.

extern crate std;

use alloc::vec::Vec;
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::corpus::DSDTS;
use super::util::{FakeHost, fix_checksum, table, test_config};
use crate::{Config, Interpreter, Limits, Value};

/// xorshift64* (Marsaglia, Vigna).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }

    fn byte(&mut self) -> u8 {
        self.next().to_le_bytes()[0]
    }
}

/// One mutation of `base` (a whole table), with its header length and
/// checksum fixed so that the AML itself is what gets tested.
fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    const HDR: usize = 36;
    let mut t = base.to_vec();
    let body = t.len() - HDR;
    let rounds = 1 + rng.below(3);
    for _ in 0..rounds {
        let body = t.len() - HDR;
        match rng.below(6) {
            0 | 1 => {
                for _ in 0..=rng.below(8) {
                    let i = HDR + rng.below(body);
                    if i < t.len() {
                        t[i] = rng.byte();
                    }
                }
            }
            2 => {
                // Bit flips in what is likely a length or opcode.
                let i = HDR + rng.below(body);
                if i < t.len() {
                    t[i] ^= 1 << rng.below(8);
                }
            }
            3 => t.truncate(HDR + rng.below(body + 1)),
            4 => {
                // Splice: copy a chunk over another place.
                let n = 1 + rng.below(64);
                let src = HDR + rng.below(body);
                let dst = HDR + rng.below(body);
                let chunk: Vec<u8> = t.iter().skip(src).take(n).copied().collect();
                for (k, b) in chunk.into_iter().enumerate() {
                    if let Some(x) = t.get_mut(dst + k) {
                        *x = b;
                    }
                }
            }
            _ => {
                // Insert random bytes or delete a range.
                let at = HDR + rng.below(body);
                if rng.below(2) == 0 {
                    let n = 1 + rng.below(16);
                    let ins: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
                    t.splice(at..at, ins);
                } else {
                    let n = 1 + rng.below(32);
                    let end = (at + n).min(t.len());
                    t.drain(at..end);
                }
            }
        }
    }
    let _ = body;
    let len = u32::try_from(t.len()).unwrap();
    t[4..8].copy_from_slice(&len.to_le_bytes());
    fix_checksum(&mut t);
    t
}

/// Small limits keep each case fast; the default limits are exercised by
/// the other tests.
fn fuzz_config() -> Config {
    Config {
        limits: Limits {
            max_loop_iterations: 2000,
            max_call_depth: 16,
            max_depth: 128,
            max_object_size: 1 << 16,
            ..test_config().limits
        },
        ..test_config()
    }
}

/// Loads and exercises one table; returns the number of devices found.
fn exercise(t: &[u8]) -> usize {
    let mut i = Interpreter::new(fuzz_config());
    let mut h = FakeHost {
        fill: 0xff,
        ..FakeHost::default()
    };
    if i.load_table(t, &mut h).is_err() {
        return 0;
    }
    let devices = i.devices();
    for &d in &devices {
        let _ = i.sta(d, &mut h);
        let _ = i.hid(d, &mut h);
        let _ = i.crs(d, &mut h);
        let _ = i.prt(d, &mut h);
    }
    // Any namespace walk must stay consistent.
    for n in i.nodes() {
        let _ = i.path(n);
        let _ = i.object_type(n);
    }
    devices.len()
}

fn iterations() -> (u64, u64) {
    let get = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok());
    (
        get("AML_FUZZ_ITERS").unwrap_or(0),
        get("AML_FUZZ_SEED").unwrap_or(1),
    )
}

#[test]
fn mutated_corpus_never_panics() {
    let (iters, seed) = iterations();
    for (k, (name, dsdt)) in DSDTS.iter().enumerate() {
        // The Q8B table is 15–50 times larger than the others.
        let n = if iters > 0 {
            iters
        } else if *name == "radxa-q8b" {
            500
        } else {
            2000
        };
        let mut devices = 0;
        for case in 0..n {
            let s = seed
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(u64::try_from(k).unwrap() << 32 | case);
            let mut rng = Rng(s | 1);
            let t = mutate(&mut rng, dsdt);
            let r = catch_unwind(AssertUnwindSafe(|| exercise(&t)));
            match r {
                Ok(d) => devices += d,
                Err(_) => panic!("{name}: case {case} (rng seed {s:#x}) panicked"),
            }
        }
        // Most mutations leave most of the namespace loadable.
        assert!(devices > 0, "{name}");
        std::println!("{name}: {n} mutations, {devices} devices exercised");
    }
}

#[test]
fn random_aml_never_panics() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    for case in 0..2000 {
        let n = rng.below(256);
        // Bias toward opcodes and name characters.
        let body: Vec<u8> = (0..n)
            .map(|_| match rng.below(4) {
                0 => b"_ABCSTA0123\\^."[rng.below(14)],
                1 => [
                    0x10, 0x14, 0x5b, 0x82, 0x80, 0x81, 0x08, 0x12, 0x11, 0xa0, 0xa2,
                ][rng.below(11)],
                _ => rng.byte(),
            })
            .collect();
        let t = table(b"DSDT", 2, &body);
        let r = catch_unwind(AssertUnwindSafe(|| exercise(&t)));
        assert!(r.is_ok(), "random case {case} panicked: {body:x?}");
    }
}

/// Invokes every method of a mutated table, with arguments drawn from
/// `rng`.
fn exercise_methods(t: &[u8], rng: &mut Rng) {
    let mut i = Interpreter::new(fuzz_config());
    let mut h = FakeHost::default();
    if i.load_table(t, &mut h).is_err() {
        return;
    }
    for n in i.nodes() {
        let Some(m) = i.method_info(n) else {
            continue;
        };
        let args = (0..m.args)
            .map(|_| match rng.below(4) {
                0 => Value::Buffer((0..rng.below(24)).map(|_| rng.byte()).collect()),
                1 => Value::String("Windows 2015".into()),
                _ => Value::Integer(rng.next() % 8),
            })
            .collect();
        let _ = i.evaluate_node(n, args, &mut h);
    }
    let _ = i.initialize(&mut h);
}

#[test]
fn mutated_methods_never_panic() {
    let (iters, seed) = iterations();
    for (k, (name, dsdt)) in DSDTS.iter().enumerate() {
        let n = if iters > 0 {
            iters / 4
        } else if *name == "radxa-q8b" {
            40
        } else {
            300
        };
        for case in 0..n {
            let s = seed
                .wrapping_mul(0xd1b5_4a32_d192_ed03)
                .wrapping_add(u64::try_from(k).unwrap() << 32 | case);
            let mut rng = Rng(s | 1);
            // Every tenth case runs the pristine table with random arguments.
            let t = if case % 10 == 0 {
                dsdt.to_vec()
            } else {
                mutate(&mut rng, dsdt)
            };
            let r = catch_unwind(AssertUnwindSafe(|| exercise_methods(&t, &mut rng)));
            assert!(r.is_ok(), "{name}: case {case} (rng seed {s:#x}) panicked");
        }
    }
}
