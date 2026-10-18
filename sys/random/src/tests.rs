// SPDX-License-Identifier: BSD-2-Clause

use super::*;

// The generator is global: one test walks through its life in order.
#[test]
fn life_of_the_generator() {
    let cpu = CpuId(0);
    let mut out = [0u8; 48];
    assert_eq!(fill(cpu, &mut out), Err(Unseeded), "refuses before seeding");
    assert!(!seed(&[0; 32]), "all zeros is not a seed");
    assert!(!seeded());
    assert!(seed(&[0x5a; 32]), "the first seed seeds");
    assert!(!seed(&[0x5b; 32]), "later seeds only reseed");

    fill(cpu, &mut out).unwrap();
    let mut again = [0u8; 48];
    fill(cpu, &mut again).unwrap();
    assert_ne!(out, again, "fast key erasure: no repeats");
    assert!(out.iter().any(|&b| b != 0));

    // Other CPUs get their own streams.
    let mut other = [0u8; 48];
    fill(CpuId(1), &mut other).unwrap();
    assert_ne!(other, again);

    // Strong harvests reseed the root once enough has come in; timing
    // events mix in without counting.
    let g = generation();
    harvest(Source::Timing, &[1; 128]);
    assert_eq!(generation(), g);
    harvest(Source::Device, &[2; 32]);
    assert_eq!(generation(), g);
    harvest(Source::Device, &[3; 32]);
    assert_eq!(generation(), g + 1, "64 strong bytes reseed");
    // The per-CPU stream rekeys from the reseeded root.
    let before = PER_CPU[0].lock().as_ref().map(|(_, g)| *g);
    fill(cpu, &mut out).unwrap();
    let after = PER_CPU[0].lock().as_ref().map(|(_, g)| *g);
    assert_ne!(before, after);
    assert_eq!(after, Some(generation()));
}
