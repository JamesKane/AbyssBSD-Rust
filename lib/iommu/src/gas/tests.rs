// SPDX-License-Identifier: BSD-2-Clause
//! The allocator against a reference model: a sorted list of intervals,
//! searched gap by gap with a brute-force placement (docs/IOMMU.md §7).

extern crate std;

use std::vec::Vec;

use super::*;
use crate::testutil::Rng;

fn iova(v: u64) -> Iova {
    Iova::new(v)
}

/// The reference: entries in address order, placeholders included.
struct Model {
    end: u64,
    entries: Vec<(u64, u64, Option<Kind>)>,
}

impl Model {
    fn new(end: u64) -> Self {
        Self {
            end,
            entries: std::vec![(0, 0, None), (end, end, None)],
        }
    }

    fn insert(&mut self, s: u64, e: u64, k: Kind) {
        let at = self
            .entries
            .iter()
            .position(|&(ps, pe, _)| (pe, ps) > (e, s))
            .unwrap();
        self.entries.insert(at, (s, e, Some(k)));
    }

    fn overlaps(&self, s: u64, e: u64) -> Vec<(u64, u64, Option<Kind>)> {
        self.entries
            .iter()
            .copied()
            .filter(|&(a, b, _)| a < b && a < e && b > s)
            .collect()
    }

    /// The lowest valid start in a gap, by trying every candidate.
    fn place(beg: u64, end: u64, r: &Request) -> Option<(u64, u64)> {
        let step = r.align.max(PAGE);
        let first = beg.checked_add(PAGE)?.checked_next_multiple_of(step)?;
        let last_ok = end.checked_sub(PAGE + 1)?.min(r.limit);
        let fits = |c: u64, size: u64| {
            c.checked_add(r.offset + size - 1)
                .is_some_and(|l| l <= last_ok)
        };
        let crosses = |c: u64, size: u64| {
            r.boundary != 0 && (c + r.offset) / r.boundary != (c + r.offset + size - 1) / r.boundary
        };
        // Within one boundary period of the first candidate, a placement
        // that does not cross exists if any does.
        let tries = if r.boundary == 0 {
            1
        } else {
            r.boundary / step + 2
        };
        let mut c = first;
        for _ in 0..tries {
            if !fits(c, r.size) {
                break;
            }
            if !crosses(c, r.size) {
                return Some((c, r.size));
            }
            c = c.checked_add(step)?;
        }
        if r.can_split && fits(first, r.size) && crosses(first, r.size) {
            let b = (first + r.offset) / r.boundary * r.boundary + r.boundary;
            return Some((first, b - first - r.offset));
        }
        None
    }

    fn alloc(&mut self, r: &Request) -> Result<Allocation, GasError> {
        r.validate()?;
        for w in 0..self.entries.len() - 1 {
            let (beg, end) = (self.entries[w].1, self.entries[w + 1].0);
            if let Some((s, size)) = Self::place(beg, end, r) {
                let e = s + (size + r.offset).next_multiple_of(PAGE);
                self.insert(s, e, Kind::Mapped);
                return Ok(Allocation {
                    start: iova(s),
                    end: iova(e),
                    addr: iova(s + r.offset),
                    size,
                });
            }
        }
        Err(GasError::NoSpace)
    }

    fn valid(&self, s: u64, e: u64) -> bool {
        s.is_multiple_of(PAGE) && e.is_multiple_of(PAGE) && s < e && e <= self.end
    }

    fn remove(&mut self, s: u64, kinds: &[Kind]) -> Result<Kind, GasError> {
        let at = self
            .entries
            .iter()
            .position(|&(a, b, k)| a == s && a < b && k.is_some_and(|k| kinds.contains(&k)))
            .ok_or(GasError::NotFound)?;
        Ok(self.entries.remove(at).2.unwrap())
    }

    fn reserve(&mut self, s: u64, e: u64) -> Result<(), GasError> {
        if !self.valid(s, e) {
            return Err(GasError::Invalid);
        }
        if !self.overlaps(s, e).is_empty() {
            return Err(GasError::Busy);
        }
        self.insert(s, e, Kind::Reserved);
        Ok(())
    }

    fn reserve_extend(&mut self, s: u64, e: u64) -> Result<(), GasError> {
        let e = e.min(self.end);
        if s >= e {
            return Ok(());
        }
        if !self.valid(s, e) {
            return Err(GasError::Invalid);
        }
        // The free runs between the entries overlapping the range.
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let mut cursor = s;
        for (a, b, _) in self.overlaps(s, e) {
            if a > cursor {
                runs.push((cursor, a));
            }
            cursor = cursor.max(b);
        }
        if cursor < e {
            runs.push((cursor, e));
        }
        for (a, b) in runs {
            self.insert(a, b, Kind::Reserved);
        }
        Ok(())
    }

    fn identity(&mut self, mut s: u64, mut e: u64) -> Result<Option<(Iova, Iova)>, GasError> {
        if !self.valid(s, e) {
            return Err(GasError::Invalid);
        }
        if let Some(&(a, b, k)) = self.overlaps(s, e).iter().find(|&&(a, _, _)| a <= s) {
            if k != Some(Kind::Identity) {
                return Err(GasError::Busy);
            }
            debug_assert!(a <= s);
            s = b;
        }
        if s >= e {
            return Ok(None);
        }
        if let Some(&(a, _, k)) = self
            .overlaps(s, e)
            .iter()
            .find(|&&(a, b, _)| b >= e && a > s)
        {
            if k != Some(Kind::Identity) {
                return Err(GasError::Busy);
            }
            e = a;
        }
        if !self.overlaps(s, e).is_empty() {
            return Err(GasError::Busy);
        }
        self.insert(s, e, Kind::Identity);
        Ok(Some((iova(s), iova(e))))
    }

    fn release_reserved(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        self.entries.retain(|&(a, b, k)| match k {
            Some(kind @ (Kind::Reserved | Kind::Identity)) => {
                out.push(Entry {
                    start: iova(a),
                    end: iova(b),
                    kind,
                });
                false
            }
            _ => true,
        });
        out
    }

    fn list(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .filter_map(|&(a, b, k)| {
                k.map(|kind| Entry {
                    start: iova(a),
                    end: iova(b),
                    kind,
                })
            })
            .collect()
    }
}

fn entries(s: &Space) -> Vec<Entry> {
    s.iter().collect()
}

fn alloc(s: &mut Space, size: u64) -> u64 {
    s.alloc(&Request::new(size)).unwrap().start.get()
}

#[test]
fn lowest_first_with_guard_pages() {
    let mut s = Space::new(SPACE_48).unwrap();
    // IOVA 0 is never handed out; each allocation leaves a guard page.
    assert_eq!(alloc(&mut s, 1), 0x1000);
    assert_eq!(alloc(&mut s, PAGE), 0x3000);
    assert_eq!(alloc(&mut s, PAGE + 1), 0x5000);
    assert_eq!(alloc(&mut s, 3 * PAGE), 0x8000);
    s.free(iova(0x3000)).unwrap();
    // The hole [0x2000, 0x5000) holds one page between guards.
    assert_eq!(alloc(&mut s, PAGE), 0x3000);
    s.free(iova(0x3000)).unwrap();
    // But not two: they go after the last entry.
    assert_eq!(alloc(&mut s, 2 * PAGE), 0xc000);
    s.check().unwrap();
    assert_eq!(s.len(), 4);
}

#[test]
fn offset_and_size_round_to_pages() {
    let mut s = Space::new(SPACE_48).unwrap();
    let a = s.alloc(&Request::new(0x1800).offset(0x900)).unwrap();
    assert_eq!(
        a,
        Allocation {
            start: iova(0x1000),
            end: iova(0x4000),
            addr: iova(0x1900),
            size: 0x1800
        }
    );
    let b = s.alloc(&Request::new(0x700).offset(0x900)).unwrap();
    assert_eq!((b.start, b.end), (iova(0x5000), iova(0x6000)));
}

#[test]
fn alignment_and_boundary() {
    let mut s = Space::new(SPACE_48).unwrap();
    let a = s.alloc(&Request::new(PAGE).align(0x10000)).unwrap();
    assert_eq!(a.start, iova(0x10000));
    // Below it, a small one still fits lowest.
    assert_eq!(alloc(&mut s, PAGE), 0x1000);
    // [0x3000, 0x6000) would cross 0x4000: it moves to 0x4000.
    let b = s.alloc(&Request::new(0x3000).boundary(0x4000)).unwrap();
    assert_eq!(b.start, iova(0x4000));
    // A request larger than its boundary never fits whole...
    assert_eq!(
        s.alloc(&Request::new(0x5000).boundary(0x4000)),
        Err(GasError::NoSpace)
    );
    // ...but splits when allowed: cut at the next boundary.
    let c = s
        .alloc(&Request::new(0x5000).boundary(0x4000).can_split())
        .unwrap();
    assert_eq!(
        (c.start, c.size, c.end),
        (iova(0x8000), 0x4000, iova(0xc000))
    );
    // An offset counts against the boundary.
    let d = s
        .alloc(&Request::new(0x4000).offset(0x10).boundary(0x4000))
        .unwrap_err();
    assert_eq!(d, GasError::NoSpace);
    s.check().unwrap();
}

#[test]
fn bad_requests() {
    let mut s = Space::new(SPACE_48).unwrap();
    for r in [
        Request::new(0),
        Request::new(1).offset(PAGE),
        Request::new(1).align(3),
        Request::new(1).boundary(0x3000),
        Request::new(1).boundary(0x800),
        Request::new(u64::MAX),
    ] {
        assert_eq!(s.alloc(&r), Err(GasError::Invalid), "{r:?}");
    }
    assert_eq!(Space::new(0x2000).unwrap_err(), GasError::Invalid);
    assert_eq!(Space::new(0x3001).unwrap_err(), GasError::Invalid);
    assert!(s.is_empty());
}

#[test]
fn limits_and_the_end() {
    let mut s = Space::new(4 * PAGE).unwrap();
    assert_eq!(alloc(&mut s, PAGE), 0x1000);
    // [0x2000, 0x4000): a start at 0x3000 would leave no guard below the
    // end.
    assert_eq!(s.alloc(&Request::new(1)), Err(GasError::NoSpace));
    let mut s = Space::new(SPACE_48).unwrap();
    assert_eq!(
        s.alloc(&Request::new(PAGE).limit(0x1fff)).unwrap().start,
        iova(0x1000)
    );
    assert_eq!(
        s.alloc(&Request::new(PAGE).limit(0x3ffe)),
        Err(GasError::NoSpace)
    );
    assert_eq!(
        s.alloc(&Request::new(PAGE).limit(0x3fff)).unwrap().start,
        iova(0x3000)
    );
    // The whole top of a 48-bit space.
    let big = s.alloc(&Request::new(SPACE_48 - 0x6000)).unwrap();
    assert_eq!(
        (big.start, big.end),
        (iova(0x5000), iova(SPACE_48 - 0x1000))
    );
    assert_eq!(s.alloc(&Request::new(1)), Err(GasError::NoSpace));
    s.check().unwrap();
}

#[test]
fn fragmentation() {
    let mut s = Space::new(SPACE_48).unwrap();
    let starts: Vec<u64> = (0..64).map(|_| alloc(&mut s, PAGE)).collect();
    assert_eq!(starts[63], 0x1000 + 63 * 0x2000);
    // Free every other one: one-page holes of three free pages each.
    for &a in starts.iter().skip(1).step_by(2) {
        s.free(iova(a)).unwrap();
    }
    // One page reuses the first hole; two pages need two adjacent frees.
    assert_eq!(alloc(&mut s, PAGE), starts[1]);
    s.free(iova(starts[2])).unwrap();
    // starts[1..4] gone: [starts[0] + 0x1000, starts[4]) is free, seven
    // pages: three pages and their guards, then one page in what is left.
    s.free(iova(starts[1])).unwrap();
    assert_eq!(alloc(&mut s, 3 * PAGE), starts[1]);
    assert_eq!(alloc(&mut s, PAGE), starts[3]);
    s.check().unwrap();
}

#[test]
fn reserved_regions() {
    let mut s = Space::new(SPACE_48).unwrap();
    // From 0 (the board port's 1464aadaf77).
    s.reserve(iova(0), iova(0x10000)).unwrap();
    assert_eq!(alloc(&mut s, PAGE), 0x11000);
    assert_eq!(s.reserve(iova(0x8000), iova(0x12000)), Err(GasError::Busy));
    assert_eq!(s.reserve(iova(0x11000), iova(0x12000)), Err(GasError::Busy));
    // Abutting is allowed: the guard is the allocator's, not a region's.
    s.reserve(iova(0x12000), iova(0x20000)).unwrap();
    for (a, b) in [(0x1, 0x2000), (0x2000, 0x2000), (0x3000, 0x2000)] {
        assert_eq!(s.reserve(iova(a), iova(b)), Err(GasError::Invalid));
    }
    assert_eq!(
        s.reserve(iova(SPACE_48 - PAGE), iova(SPACE_48 + PAGE)),
        Err(GasError::Invalid)
    );
    // Up to the very end works (FreeBSD refuses an entry ending there).
    s.reserve(iova(SPACE_48 - PAGE), iova(SPACE_48)).unwrap();
    // Extending from 0 over everything terminates and fills the gaps.
    s.reserve_extend(iova(0), iova(0x40000)).unwrap();
    let e = entries(&s);
    assert_eq!(
        e.iter()
            .filter(|e| e.start.get() < 0x40000)
            .map(|e| (e.start.get(), e.end.get(), e.kind))
            .collect::<Vec<_>>(),
        [
            (0, 0x10000, Kind::Reserved),
            (0x10000, 0x11000, Kind::Reserved),
            (0x11000, 0x12000, Kind::Mapped),
            (0x12000, 0x20000, Kind::Reserved),
            (0x20000, 0x40000, Kind::Reserved),
        ]
    );
    // Allocation skips them all.
    assert_eq!(alloc(&mut s, PAGE), 0x41000);
    // Clamped at the end.
    s.reserve_extend(iova(SPACE_48 - 0x10000), iova(!0xfff))
        .unwrap();
    assert_eq!(s.free_region(iova(0x11000)), Err(GasError::NotFound));
    assert_eq!(s.free(iova(0)), Err(GasError::NotFound));
    assert_eq!(s.free_region(iova(0)), Ok(Kind::Reserved));
    s.check().unwrap();
}

#[test]
fn identity_regions() {
    let mut s = Space::new(SPACE_48).unwrap();
    let fb = 0x8480_0000;
    assert_eq!(
        s.identity(iova(fb), iova(fb + 0x80_0000)),
        Ok(Some((iova(fb), iova(fb + 0x80_0000))))
    );
    // Overlapping identity regions are clipped at either end.
    assert_eq!(
        s.identity(iova(fb + 0x40_0000), iova(fb + 0x90_0000)),
        Ok(Some((iova(fb + 0x80_0000), iova(fb + 0x90_0000))))
    );
    assert_eq!(
        s.identity(iova(fb - 0x1000), iova(fb + 0x1000)),
        Ok(Some((iova(fb - 0x1000), iova(fb))))
    );
    assert_eq!(s.identity(iova(fb), iova(fb + 0x1000)), Ok(None));
    // One strictly inside is refused, as is anything not identity.
    assert_eq!(
        s.identity(iova(fb - 0x10_0000), iova(fb + 0x100_0000)),
        Err(GasError::Busy)
    );
    let a = alloc(&mut s, PAGE);
    assert_eq!(s.identity(iova(a), iova(a + PAGE)), Err(GasError::Busy));
    s.check().unwrap();

    // Free-on-fini (the board port's cf75a5f809b).
    s.reserve(iova(0x6000_0000), iova(0x8000_0000)).unwrap();
    assert_eq!(s.fini(), Err(GasError::InUse));
    let mut s = Space::new(SPACE_48).unwrap();
    s.reserve(iova(0x6000_0000), iova(0x8000_0000)).unwrap();
    s.identity(iova(fb), iova(fb + 0x80_0000)).unwrap();
    let a = alloc(&mut s, PAGE);
    let mut released = Vec::new();
    s.release_reserved(|e| released.push(e));
    assert_eq!(
        released,
        [
            Entry {
                start: iova(0x6000_0000),
                end: iova(0x8000_0000),
                kind: Kind::Reserved
            },
            Entry {
                start: iova(fb),
                end: iova(fb + 0x80_0000),
                kind: Kind::Identity
            },
        ]
    );
    assert_eq!(s.len(), 1);
    s.free(iova(a)).unwrap();
    s.check().unwrap();
    s.fini().unwrap();
}

#[test]
fn many_entries_stay_balanced() {
    let mut s = Space::new(SPACE_48).unwrap();
    let n = 20_000;
    let starts: Vec<u64> = (0..n).map(|_| alloc(&mut s, PAGE)).collect();
    s.check().unwrap();
    let h = s.nodes[s.root as usize].height;
    assert!(h <= 21, "height {h} for {n} entries");
    for &a in starts.iter().step_by(3) {
        s.free(iova(a)).unwrap();
    }
    s.check().unwrap();
    // The lowest hole is found again.
    assert_eq!(alloc(&mut s, PAGE), starts[0]);
    // Nodes are recycled: no growth for as many allocations as frees.
    let cap = s.nodes.len();
    for _ in 0..(n / 3) {
        alloc(&mut s, PAGE);
    }
    assert_eq!(s.nodes.len(), cap);
}

fn random_request(rng: &mut Rng, end: u64) -> Request {
    let size = match rng.below(4) {
        0 => 1 + rng.below(PAGE),
        1 => (1 + rng.below(8)) * PAGE,
        2 => 1 + rng.below(16 * PAGE),
        _ => 1 + rng.below(end / 8),
    };
    let mut r = Request::new(size);
    if rng.chance(3) {
        r = r.offset(rng.below(PAGE));
    }
    if rng.chance(4) {
        r = r.align(1 << (12 + rng.below(6)));
    }
    if rng.chance(4) {
        r = r.boundary(1 << (12 + rng.below(8)));
        if rng.chance(2) {
            r = r.can_split();
        }
    }
    if rng.chance(4) {
        r = r.limit(rng.below(end));
    }
    r
}

fn random_region(rng: &mut Rng, end: u64) -> (u64, u64) {
    let pages = end / PAGE;
    let a = rng.below(pages) * PAGE;
    let most = if rng.chance(4) { pages / 4 } else { 16 };
    let len = (1 + rng.below(most)) * PAGE;
    let a = if rng.chance(8) { 0 } else { a };
    (
        a,
        (a + len).min(end + if rng.chance(16) { PAGE } else { 0 }),
    )
}

/// Randomized allocate/free/reserve sequences against the model, at a
/// small space size so the space fills, fragments, and runs out.
fn run_random(seed: u64, end: u64, ops: usize) -> [usize; 8] {
    // Outcomes seen: alloc ok, alloc failed, free, reserve ok, reserve
    // busy, extend, identity added, identity refused.
    let mut seen = [0; 8];
    let mut rng = Rng(seed);
    let mut s = Space::new(end).unwrap();
    let mut m = Model::new(end);
    let mut live: Vec<u64> = Vec::new();
    for op in 0..ops {
        match rng.below(16) {
            0..=6 => {
                let r = random_request(&mut rng, end);
                let got = s.alloc(&r);
                assert_eq!(got, m.alloc(&r), "op {op}: {r:?}");
                seen[usize::from(got.is_err())] += 1;
                if let Ok(a) = got {
                    // The guard pages, at the time of allocation.
                    let all = m.list();
                    let i = all.iter().position(|e| e.start == a.start).unwrap();
                    let below = if i == 0 { 0 } else { all[i - 1].end.get() };
                    assert!(a.start.get() >= below + PAGE, "op {op}: guard below");
                    let above = all.get(i + 1).map_or(end, |e| e.start.get());
                    assert!(a.end.get() + PAGE <= above, "op {op}: guard above");
                    assert!(a.addr.get() + a.size - 1 <= r.limit);
                    assert!(a.start.get().is_multiple_of(r.align.max(PAGE)));
                    if let (Some(lo), Some(hi)) = (
                        a.addr.get().checked_div(r.boundary),
                        (a.addr.get() + a.size - 1).checked_div(r.boundary),
                    ) {
                        assert_eq!(lo, hi, "op {op}: crosses the boundary");
                    }
                    if !r.can_split {
                        assert_eq!(a.size, r.size);
                    }
                    live.push(a.start.get());
                }
            }
            7..=10 if !live.is_empty() => {
                let i = usize::try_from(rng.below(live.len() as u64)).unwrap();
                let a = live.swap_remove(i);
                assert_eq!(s.free(iova(a)), Ok(()));
                m.remove(a, &[Kind::Mapped]).unwrap();
                seen[2] += 1;
            }
            11 => {
                let (a, b) = random_region(&mut rng, end);
                let got = s.reserve(iova(a), iova(b));
                assert_eq!(got, m.reserve(a, b), "op {op}");
                seen[if got.is_ok() { 3 } else { 4 }] += 1;
            }
            12 => {
                let (a, b) = random_region(&mut rng, end);
                assert_eq!(
                    s.reserve_extend(iova(a), iova(b)),
                    m.reserve_extend(a, b),
                    "op {op}"
                );
                seen[5] += 1;
            }
            13 => {
                let (a, b) = random_region(&mut rng, end);
                let got = s.identity(iova(a), iova(b));
                assert_eq!(got, m.identity(a, b), "op {op}");
                seen[if matches!(got, Ok(Some(_))) { 6 } else { 7 }] += 1;
            }
            14 => {
                let regions: Vec<Entry> = m
                    .list()
                    .into_iter()
                    .filter(|e| e.kind != Kind::Mapped)
                    .collect();
                if let Some(e) = regions.get(usize::try_from(rng.below(4)).unwrap()) {
                    let k = m.remove(e.start.get(), &[Kind::Reserved, Kind::Identity]);
                    assert_eq!(s.free_region(e.start), k, "op {op}");
                }
                // A free of something that is not an allocation.
                let x = rng.below(end / PAGE) * PAGE;
                if !live.contains(&x) {
                    assert_eq!(s.free(iova(x)), Err(GasError::NotFound));
                }
            }
            _ => {
                if rng.chance(8) {
                    let mut got = Vec::new();
                    s.release_reserved(|e| got.push(e));
                    assert_eq!(got, m.release_reserved(), "op {op}");
                }
            }
        }
        assert_eq!(entries(&s), m.list(), "op {op}");
        s.check().unwrap();
    }
    for a in live {
        s.free(iova(a)).unwrap();
    }
    s.release_reserved(|_| {});
    s.fini().unwrap();
    seen
}

#[test]
fn random_small_space() {
    let mut seen = [0; 8];
    for seed in 1..=8_u64 {
        let s = run_random(seed.wrapping_mul(0x2545_f491_4f6c_dd1d), 1 << 22, 3000);
        for (t, n) in seen.iter_mut().zip(s) {
            *t += n;
        }
    }
    std::eprintln!("outcomes {seen:?}");
    assert!(seen.iter().all(|&n| n > 50), "outcomes {seen:?}");
}

#[test]
fn random_48_bit_space() {
    let seen = run_random(0x0123_4567_89ab_cdef, SPACE_48, 3000);
    std::eprintln!("outcomes {seen:?}");
    assert!(seen[0] > 500 && seen[2] > 200, "outcomes {seen:?}");
}

#[test]
fn model_places_like_freebsd_rule() {
    // The brute-force placement and FreeBSD's two-step rule agree on
    // random gaps and requests.
    let mut rng = Rng(77);
    for _ in 0..200_000 {
        let beg = rng.below(1 << 20) * PAGE;
        let end = beg + rng.below(64) * PAGE;
        let r = random_request(&mut rng, 1 << 24);
        if r.validate().is_err() {
            continue;
        }
        assert_eq!(
            place(beg, end, &r),
            Model::place(beg, end, &r),
            "{beg:#x} {end:#x} {r:?}"
        );
    }
}
