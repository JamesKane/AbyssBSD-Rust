// SPDX-License-Identifier: BSD-2-Clause
//! Choosing a driver: match tables as data, ranked with FreeBSD's
//! `BUS_PROBE_*` values (docs/DRIVER-MODEL.md §7.2).
//!
//! Each driver declares a `const` table of IDs for its bus. A new device's
//! identity is checked against every table without running driver code;
//! the best rank wins, and ties are resolved by an explicit `yields` list.
//! A tie nobody resolved is an [`Ambiguity`], which
//! [`find_ambiguities`] reports at build time (`cargo xtask check`), where
//! FreeBSD would silently take registration order.
//!
//! Behavior reference: sys/kern/subr_bus.c (`device_probe_child`).

use core::cmp::Ordering;

/// A probe rank, FreeBSD's `BUS_PROBE_*` scale: higher wins, and
/// [`Rank::SPECIFIC`] (0) is the best a driver may claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rank(pub i32);

impl Rank {
    /// `BUS_PROBE_SPECIFIC`: only this driver can drive the device.
    pub const SPECIFIC: Self = Self(0);
    /// `BUS_PROBE_VENDOR`: the vendor's own driver.
    pub const VENDOR: Self = Self(-10);
    /// `BUS_PROBE_DEFAULT`: the usual choice.
    pub const DEFAULT: Self = Self(-20);
    /// `BUS_PROBE_LOW_PRIORITY`: older or less capable.
    pub const LOW_PRIORITY: Self = Self(-40);
    /// `BUS_PROBE_GENERIC`: a class-code driver.
    pub const GENERIC: Self = Self(-100);
    /// `BUS_PROBE_HOOVER`: catches whatever is left.
    pub const HOOVER: Self = Self(-1_000_000);
}

/// A match-table entry that can judge a device identity of type `I`.
pub trait Matches<I> {
    /// The rank this entry gives `ident`, if it matches.
    fn rank_for(&self, ident: &I) -> Option<Rank>;
}

/// One driver's view for matching: its name (the devclass), its ID table,
/// and the drivers it gives way to on an equal rank.
#[derive(Debug)]
pub struct Candidate<'a, Id> {
    /// The driver name, e.g. `"nvme"`.
    pub name: &'a str,
    /// The ID table.
    pub ids: &'a [Id],
    /// Drivers this one yields to when both match at the same rank.
    pub yields: &'a [&'a str],
}

impl<Id> Candidate<'_, Id> {
    fn best<I>(&self, ident: &I) -> Option<Rank>
    where
        Id: Matches<I>,
    {
        self.ids.iter().filter_map(|id| id.rank_for(ident)).max()
    }
}

/// The outcome of matching one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The index of the winning candidate and its rank.
    Driver {
        /// Index into the candidate slice.
        index: usize,
        /// The winning rank.
        rank: Rank,
    },
    /// No driver matches: a devctl `?` event.
    NoMatch,
    /// Two drivers match at the best rank and neither yields: a build
    /// error (see [`find_ambiguities`]); at run time the first in table
    /// order wins, as in FreeBSD.
    Tie {
        /// The first candidate in table order.
        first: usize,
        /// The other.
        second: usize,
        /// The rank they share.
        rank: Rank,
    },
}

/// Chooses a driver for `ident` among `candidates`, skipping indices
/// `exclude` rejects (drivers that already declined the device).
pub fn select<I, Id: Matches<I>>(
    candidates: &[Candidate<'_, Id>],
    ident: &I,
    exclude: impl Fn(usize) -> bool,
) -> Selection {
    let mut best: Option<(usize, Rank)> = None;
    let mut tie: Option<usize> = None;
    for (i, c) in candidates.iter().enumerate() {
        if exclude(i) {
            continue;
        }
        let Some(r) = c.best(ident) else {
            continue;
        };
        match best {
            None => best = Some((i, r)),
            Some((b, br)) => match r.cmp(&br) {
                Ordering::Greater => {
                    best = Some((i, r));
                    tie = None;
                }
                Ordering::Equal => {
                    let old = &candidates[b];
                    if c.yields.contains(&old.name) {
                        // The newcomer gives way.
                    } else if old.yields.contains(&c.name) {
                        best = Some((i, r));
                        tie = None;
                    } else if tie.is_none() {
                        tie = Some(i);
                    }
                }
                Ordering::Less => {}
            },
        }
    }
    match (best, tie) {
        (None, _) => Selection::NoMatch,
        (Some((first, rank)), Some(second)) => Selection::Tie {
            first,
            second,
            rank,
        },
        (Some((index, rank)), None) => Selection::Driver { index, rank },
    }
}

/// Two drivers tied on a device identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ambiguity<'a> {
    /// The identity's position in the corpus.
    pub ident: usize,
    /// The drivers.
    pub drivers: (&'a str, &'a str),
    /// Their shared rank.
    pub rank: Rank,
}

/// Every identity in `corpus` on which two drivers tie unresolved.
pub fn find_ambiguities<'a, I, Id: Matches<I>>(
    candidates: &[Candidate<'a, Id>],
    corpus: &[I],
) -> alloc::vec::Vec<Ambiguity<'a>> {
    corpus
        .iter()
        .enumerate()
        .filter_map(|(n, ident)| match select(candidates, ident, |_| false) {
            Selection::Tie {
                first,
                second,
                rank,
            } => Some(Ambiguity {
                ident: n,
                drivers: (candidates[first].name, candidates[second].name),
                rank,
            }),
            _ => None,
        })
        .collect()
}
