// SPDX-License-Identifier: BSD-2-Clause
//! Platform providers: clocks, resets, and power domains
//! (DRIVER-MODEL.md §8.5, §9.3).
//!
//! A provider driver implements [`Clock`], [`Reset`], or [`PowerDomain`]
//! for the lines it serves, by the provider's own identifiers (an SCMI
//! clock ID, a reset index). Consumers never call it directly: the
//! framework wraps it in a [`ClockSource`], [`ResetSource`], or
//! [`DomainSource`], which owns the sharing rules, and hands consumers
//! counted handles from it:
//!
//! - a clock is enabled while any handle has it enabled;
//! - a reset line shared by several consumers is asserted only when every
//!   holder asks, and a pulse on a shared line is refused unless the
//!   provider says resetting the whole group is safe (the Sky1's four
//!   GPIO controllers share `RST1` line 26);
//! - a power domain runs at the highest level any consumer votes for, and
//!   a dropped vote is withdrawn.
//!
//! Operations that talk to firmware sleep, so every call takes a
//! [`Sleepable`]: attach, start, stop, or a thread, never an interrupt
//! handler. Each source serializes its provider's calls with its counts,
//! so the hardware state always matches them.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use sched::Sleepable;
use sched::lock::{LockClass, Mutex};

static SOURCE: LockClass = LockClass::new("provider source", 260);

/// A frequency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hz(pub u64);

/// A duration in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Micros(pub u32);

/// A power or performance level: 0 is off, and a two-level domain is on
/// at 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PerfLevel(pub u32);

impl PerfLevel {
    /// Off.
    pub const OFF: Self = Self(0);
    /// On, for a two-level domain.
    pub const ON: Self = Self(1);
}

/// Why a provider operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderError {
    /// The provider serves no such line.
    NoSuchLine,
    /// The provider cannot do this (a fixed-rate clock's `set_rate`).
    Unsupported,
    /// A rate or level out of range.
    OutOfRange,
    /// A pulse on a reset line others hold, whose group the provider does
    /// not declare safe to reset.
    Shared,
    /// Firmware refused or failed (an SCMI status).
    Firmware(i32),
    /// The provider did not answer in time.
    Timeout,
    /// The provider's driver is detaching.
    Revoked,
}

/// Clocks, by the provider's identifier.
pub trait Clock: Send + Sync {
    /// Ungates clock `id`.
    fn enable(&self, s: &Sleepable, id: u32) -> Result<(), ProviderError>;
    /// Gates clock `id`.
    fn disable(&self, s: &Sleepable, id: u32) -> Result<(), ProviderError>;
    /// Clock `id`'s current rate.
    fn rate(&self, s: &Sleepable, id: u32) -> Result<Hz, ProviderError>;
    /// The rate `set_rate(id, want)` would give.
    fn round_rate(&self, s: &Sleepable, id: u32, want: Hz) -> Result<Hz, ProviderError>;
    /// Sets clock `id`'s rate, rounded as [`Clock::round_rate`] says.
    fn set_rate(&self, s: &Sleepable, id: u32, want: Hz) -> Result<(), ProviderError>;
}

/// Reset lines, by the provider's index.
pub trait Reset: Send + Sync {
    /// Holds the block on `line` in reset.
    fn assert(&self, s: &Sleepable, line: u32) -> Result<(), ProviderError>;
    /// Releases the block on `line` from reset.
    fn deassert(&self, s: &Sleepable, line: u32) -> Result<(), ProviderError>;
    /// Whether resetting everything on `line` at once is harmless, so a
    /// holder may pulse it while others hold it too. No by default.
    fn group_safe(&self, line: u32) -> bool {
        let _ = line;
        false
    }
}

/// Power (or performance) domains, by the provider's identifier.
pub trait PowerDomain: Send + Sync {
    /// The number of levels domain `id` has (2 for on/off).
    fn levels(&self, id: u32) -> Result<u32, ProviderError>;
    /// Runs domain `id` at `level`.
    fn set_level(&self, s: &Sleepable, id: u32, level: PerfLevel) -> Result<(), ProviderError>;
}

/// A clock provider and how many handles have each clock enabled.
pub struct ClockSource {
    imp: Arc<dyn Clock>,
    enabled: Mutex<BTreeMap<u32, u32>>,
    revoked: AtomicBool,
}

impl ClockSource {
    /// Wraps a provider.
    #[must_use]
    pub fn new(imp: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            imp,
            enabled: Mutex::new(&SOURCE, BTreeMap::new()),
            revoked: AtomicBool::new(false),
        })
    }

    /// A handle to clock `id`, initially not enabled by it.
    #[must_use]
    pub fn handle(self: &Arc<Self>, id: u32) -> ClockHandle {
        ClockHandle {
            source: Arc::clone(self),
            id,
            enabled: false,
        }
    }

    /// How many handles have clock `id` enabled.
    pub fn enables(&self, s: &Sleepable, id: u32) -> u32 {
        self.enabled.lock(s).get(&id).copied().unwrap_or(0)
    }

    fn enable(&self, s: &Sleepable, id: u32) -> Result<(), ProviderError> {
        self.live()?;
        let mut counts = self.enabled.lock(s);
        let n = counts.get(&id).copied().unwrap_or(0);
        if n == 0 {
            self.imp.enable(s, id)?;
        }
        counts.insert(id, n + 1);
        Ok(())
    }

    fn disable(&self, s: &Sleepable, id: u32) -> Result<(), ProviderError> {
        self.live()?;
        let mut counts = self.enabled.lock(s);
        let n = counts.get(&id).copied().unwrap_or(0);
        if n == 1 {
            // Counted as off even if gating fails: the handle is done with
            // it either way.
            counts.remove(&id);
            return self.imp.disable(s, id);
        }
        if n > 1 {
            counts.insert(id, n - 1);
        }
        Ok(())
    }
}

/// A consumer's clock. Enabling is counted per handle: a second `enable`
/// on the same handle does nothing, and dropping the handle withdraws its
/// enable.
pub struct ClockHandle {
    source: Arc<ClockSource>,
    id: u32,
    enabled: bool,
}

impl ClockHandle {
    /// The provider's identifier for this clock.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Ungates the clock (if no other handle has).
    pub fn enable(&mut self, s: &Sleepable) -> Result<(), ProviderError> {
        if !self.enabled {
            self.source.enable(s, self.id)?;
            self.enabled = true;
        }
        Ok(())
    }

    /// Withdraws this handle's enable; the clock gates when no handle has
    /// it enabled.
    pub fn disable(&mut self, s: &Sleepable) -> Result<(), ProviderError> {
        if !self.enabled {
            return Ok(());
        }
        self.enabled = false;
        self.source.disable(s, self.id)
    }

    /// The current rate.
    pub fn rate(&self, s: &Sleepable) -> Result<Hz, ProviderError> {
        self.source.live()?;
        self.source.imp.rate(s, self.id)
    }

    /// The rate [`ClockHandle::set_rate`] would give.
    pub fn round_rate(&self, s: &Sleepable, want: Hz) -> Result<Hz, ProviderError> {
        self.source.live()?;
        self.source.imp.round_rate(s, self.id, want)
    }

    /// Sets the rate.
    pub fn set_rate(&self, s: &Sleepable, want: Hz) -> Result<(), ProviderError> {
        self.source.live()?;
        let _serial = self.source.enabled.lock(s);
        self.source.imp.set_rate(s, self.id, want)
    }

    /// Drops the handle, withdrawing its enable. Use this rather than
    /// letting the handle drop, which can only gate the clock if the
    /// dropping thread may sleep ([`Drop`] has no [`Sleepable`]).
    pub fn release(mut self, s: &Sleepable) -> Result<(), ProviderError> {
        self.disable(s)
    }
}

impl Drop for ClockHandle {
    fn drop(&mut self) {
        // A handle dropped still enabled (a driver's instance dropped at
        // detach, which runs in thread context) withdraws its enable.
        if self.enabled {
            // SAFETY: handles are dropped by attach failure or detach,
            // both in thread context (DRIVER-MODEL.md §10.2).
            let s = unsafe { Sleepable::assume() };
            let _ = self.disable(&s);
        }
    }
}

/// A reset line's holders and how many of them ask for it asserted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Line {
    holders: u32,
    asserting: u32,
    /// What the provider was last told: `None` before anyone said
    /// (firmware may have left the block in reset, or not).
    applied: Option<bool>,
}

impl Line {
    /// Whether the line should be asserted: every holder asks.
    fn wanted(&self) -> bool {
        self.holders > 0 && self.asserting == self.holders
    }
}

/// A reset controller and the holders of each of its lines.
pub struct ResetSource {
    imp: Arc<dyn Reset>,
    lines: Mutex<BTreeMap<u32, Line>>,
    revoked: AtomicBool,
}

impl ResetSource {
    /// Wraps a provider.
    #[must_use]
    pub fn new(imp: Arc<dyn Reset>) -> Arc<Self> {
        Arc::new(Self {
            imp,
            lines: Mutex::new(&SOURCE, BTreeMap::new()),
            revoked: AtomicBool::new(false),
        })
    }

    /// A handle to `line`, which joins its holders without changing it.
    pub fn handle(self: &Arc<Self>, s: &Sleepable, line: u32) -> ResetHandle {
        self.lines.lock(s).entry(line).or_default().holders += 1;
        ResetHandle {
            source: Arc::clone(self),
            line,
            asserting: false,
            held: true,
        }
    }

    /// `(holders, asserting)` for `line`.
    pub fn holders(&self, s: &Sleepable, line: u32) -> (u32, u32) {
        let lines = self.lines.lock(s);
        lines
            .get(&line)
            .map_or((0, 0), |l| (l.holders, l.asserting))
    }

    /// Changes `line`'s counts with `f`, then tells the provider if what
    /// the holders want differs from what it was last told. `force`
    /// tells it even when the last word matches (a holder releasing a
    /// line firmware may have left in reset).
    fn update(
        &self,
        s: &Sleepable,
        line: u32,
        force: bool,
        f: impl FnOnce(&mut Line),
    ) -> Result<(), ProviderError> {
        let mut lines = self.lines.lock(s);
        let l = lines.entry(line).or_default();
        f(l);
        self.live()?;
        let want = l.wanted();
        let r = if force || l.applied != Some(want) {
            if want {
                self.imp.assert(s, line)
            } else {
                self.imp.deassert(s, line)
            }
        } else {
            Ok(())
        };
        if r.is_ok() {
            l.applied = Some(want);
        }
        r
    }
}

/// A consumer's reset line.
pub struct ResetHandle {
    source: Arc<ResetSource>,
    line: u32,
    asserting: bool,
    /// Still among the holders (not yet released).
    held: bool,
}

impl ResetHandle {
    /// The provider's index for this line.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.line
    }

    /// Asks for the line asserted; it is, once every holder asks.
    pub fn assert(&mut self, s: &Sleepable) -> Result<(), ProviderError> {
        if self.asserting {
            return Ok(());
        }
        self.asserting = true;
        self.source
            .update(s, self.line, false, |l| l.asserting += 1)
    }

    /// Releases the line from reset. The provider is always told, as
    /// firmware may have left the block in reset (sky1-soc 02 §2.2).
    pub fn deassert(&mut self, s: &Sleepable) -> Result<(), ProviderError> {
        let was = core::mem::replace(&mut self.asserting, false);
        self.source.update(s, self.line, true, |l| {
            if was {
                l.asserting -= 1;
            }
        })
    }

    /// Asserts the line, waits `hold`, and releases it. Refused with
    /// [`ProviderError::Shared`] if others hold the line and the provider
    /// does not declare its group safe to reset.
    pub fn pulse(&mut self, s: &Sleepable, hold: Micros) -> Result<(), ProviderError> {
        self.source.live()?;
        let mut lines = self.source.lines.lock(s);
        let l = lines.entry(self.line).or_default();
        if l.holders > 1 && !self.source.imp.group_safe(self.line) {
            return Err(ProviderError::Shared);
        }
        self.source.imp.assert(s, self.line)?;
        l.applied = Some(true);
        crate::manager::try_sleep_ns(s, u64::from(hold.0) * 1000);
        self.source.imp.deassert(s, self.line)?;
        l.applied = Some(false);
        if core::mem::replace(&mut self.asserting, false) {
            l.asserting -= 1;
        }
        Ok(())
    }

    /// Drops the handle, leaving the holders. The line stays as it is
    /// unless the remaining holders all ask for it asserted.
    pub fn release(mut self, s: &Sleepable) -> Result<(), ProviderError> {
        self.leave(s)
    }

    fn leave(&mut self, s: &Sleepable) -> Result<(), ProviderError> {
        if !core::mem::replace(&mut self.held, false) {
            return Ok(());
        }
        let was = core::mem::replace(&mut self.asserting, false);
        self.source.update(s, self.line, false, |l| {
            l.holders -= 1;
            if was {
                l.asserting -= 1;
            }
        })
    }
}

impl Drop for ResetHandle {
    fn drop(&mut self) {
        if self.held {
            // SAFETY: handles are dropped by attach failure or detach,
            // both in thread context (DRIVER-MODEL.md §10.2).
            let s = unsafe { Sleepable::assume() };
            let _ = self.leave(&s);
        }
    }
}

/// A power domain provider and every consumer's vote.
pub struct DomainSource {
    imp: Arc<dyn PowerDomain>,
    state: Mutex<Votes>,
    revoked: AtomicBool,
}

#[derive(Default)]
struct Votes {
    next: u64,
    /// `(domain, voter) -> level`.
    votes: BTreeMap<(u32, u64), PerfLevel>,
    /// The level each domain was last set to.
    applied: BTreeMap<u32, PerfLevel>,
}

impl Votes {
    fn combined(&self, id: u32) -> PerfLevel {
        self.votes
            .range((id, 0)..=(id, u64::MAX))
            .map(|(_, &l)| l)
            .max()
            .unwrap_or(PerfLevel::OFF)
    }
}

impl DomainSource {
    /// Wraps a provider.
    #[must_use]
    pub fn new(imp: Arc<dyn PowerDomain>) -> Arc<Self> {
        Arc::new(Self {
            imp,
            state: Mutex::new(&SOURCE, Votes::default()),
            revoked: AtomicBool::new(false),
        })
    }

    /// A vote on domain `id`, initially for [`PerfLevel::OFF`].
    pub fn handle(self: &Arc<Self>, s: &Sleepable, id: u32) -> PowerVote {
        let mut st = self.state.lock(s);
        let voter = st.next;
        st.next += 1;
        st.votes.insert((id, voter), PerfLevel::OFF);
        PowerVote {
            source: Arc::clone(self),
            id,
            voter,
            voting: true,
        }
    }

    /// The level domain `id` runs at: the highest vote.
    pub fn level(&self, s: &Sleepable, id: u32) -> PerfLevel {
        self.state.lock(s).combined(id)
    }

    fn vote(
        &self,
        s: &Sleepable,
        id: u32,
        voter: u64,
        level: Option<PerfLevel>,
    ) -> Result<(), ProviderError> {
        let mut st = self.state.lock(s);
        if level.is_none() {
            // A withdrawn vote is gone even if the provider is not.
            st.votes.remove(&(id, voter));
        }
        self.live()?;
        if let Some(l) = level
            && l.0 >= self.imp.levels(id)?
        {
            return Err(ProviderError::OutOfRange);
        }
        let old = st.votes.get(&(id, voter)).copied();
        match level {
            Some(l) => st.votes.insert((id, voter), l),
            None => st.votes.remove(&(id, voter)),
        };
        let want = st.combined(id);
        // Unchanged, or off where nobody has set it yet: firmware's state
        // stands until a consumer asks for something.
        match st.applied.get(&id) {
            Some(&a) if a == want => return Ok(()),
            None if want == PerfLevel::OFF => return Ok(()),
            _ => {}
        }
        if let Err(e) = self.imp.set_level(s, id, want) {
            // Undo the vote: the domain is still where it was.
            match old {
                Some(o) => st.votes.insert((id, voter), o),
                None => st.votes.remove(&(id, voter)),
            };
            return Err(e);
        }
        st.applied.insert(id, want);
        Ok(())
    }
}

/// A consumer's vote on a power domain. Dropping it withdraws the vote.
pub struct PowerVote {
    source: Arc<DomainSource>,
    id: u32,
    voter: u64,
    /// Not yet withdrawn.
    voting: bool,
}

impl PowerVote {
    /// The provider's identifier for the domain.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Votes for `level`; the domain runs at the highest vote.
    pub fn set(&self, s: &Sleepable, level: PerfLevel) -> Result<(), ProviderError> {
        self.source.vote(s, self.id, self.voter, Some(level))
    }

    /// Withdraws the vote.
    pub fn release(mut self, s: &Sleepable) -> Result<(), ProviderError> {
        self.voting = false;
        self.source.vote(s, self.id, self.voter, None)
    }
}

impl Drop for PowerVote {
    fn drop(&mut self) {
        if self.voting {
            // SAFETY: votes are dropped by attach failure or detach, both
            // in thread context (DRIVER-MODEL.md §10.2).
            let s = unsafe { Sleepable::assume() };
            let _ = self.source.vote(&s, self.id, self.voter, None);
        }
    }
}

macro_rules! debug_by {
    ($t:ty, $name:literal, $($f:ident),*) => {
        impl core::fmt::Debug for $t {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct($name)$(.field(stringify!($f), &self.$f))*.finish_non_exhaustive()
            }
        }
    };
}

debug_by!(ClockSource, "ClockSource",);
debug_by!(ResetSource, "ResetSource",);
debug_by!(DomainSource, "DomainSource",);
debug_by!(ClockHandle, "ClockHandle", id, enabled);
debug_by!(ResetHandle, "ResetHandle", line, asserting, held);
debug_by!(PowerVote, "PowerVote", id, voter, voting);

macro_rules! revocable {
    ($($t:ty),*) => {$(
        impl $t {
            /// Stops every call to the provider: its driver is detaching.
            /// Calls through handles fail with [`ProviderError::Revoked`]
            /// from now on, and dropping a handle no longer reaches it.
            pub fn revoke(&self) {
                self.revoked.store(true, Ordering::Release);
            }

            /// Whether [`Self::revoke`] has run.
            pub fn is_revoked(&self) -> bool {
                self.revoked.load(Ordering::Acquire)
            }

            fn live(&self) -> Result<(), ProviderError> {
                if self.is_revoked() {
                    Err(ProviderError::Revoked)
                } else {
                    Ok(())
                }
            }
        }
    )*};
}

revocable!(ClockSource, ResetSource, DomainSource);

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec::Vec;
    use std::sync::Mutex as StdMutex;

    use super::*;

    /// What a provider was told.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Call {
        Enable(u32),
        Disable(u32),
        SetRate(u32, u64),
        Assert(u32),
        Deassert(u32),
        Level(u32, u32),
    }

    /// Records calls; fails the next one if `fail` is set.
    #[derive(Default)]
    struct Fake {
        calls: StdMutex<Vec<Call>>,
        fail: StdMutex<bool>,
        group_safe: bool,
    }

    impl Fake {
        fn record(&self, c: Call) -> Result<(), ProviderError> {
            if core::mem::take(&mut *self.fail.lock().unwrap()) {
                return Err(ProviderError::Firmware(-1));
            }
            self.calls.lock().unwrap().push(c);
            Ok(())
        }

        fn take(&self) -> Vec<Call> {
            core::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    impl Clock for Fake {
        fn enable(&self, _: &Sleepable, id: u32) -> Result<(), ProviderError> {
            self.record(Call::Enable(id))
        }
        fn disable(&self, _: &Sleepable, id: u32) -> Result<(), ProviderError> {
            self.record(Call::Disable(id))
        }
        fn rate(&self, _: &Sleepable, _: u32) -> Result<Hz, ProviderError> {
            Ok(Hz(100_000_000))
        }
        fn round_rate(&self, _: &Sleepable, _: u32, want: Hz) -> Result<Hz, ProviderError> {
            Ok(Hz(want.0 / 1_000_000 * 1_000_000))
        }
        fn set_rate(&self, _: &Sleepable, id: u32, want: Hz) -> Result<(), ProviderError> {
            self.record(Call::SetRate(id, want.0))
        }
    }

    impl Reset for Fake {
        fn assert(&self, _: &Sleepable, line: u32) -> Result<(), ProviderError> {
            self.record(Call::Assert(line))
        }
        fn deassert(&self, _: &Sleepable, line: u32) -> Result<(), ProviderError> {
            self.record(Call::Deassert(line))
        }
        fn group_safe(&self, _: u32) -> bool {
            self.group_safe
        }
    }

    impl PowerDomain for Fake {
        fn levels(&self, _: u32) -> Result<u32, ProviderError> {
            Ok(4)
        }
        fn set_level(&self, _: &Sleepable, id: u32, l: PerfLevel) -> Result<(), ProviderError> {
            self.record(Call::Level(id, l.0))
        }
    }

    fn s() -> Sleepable {
        // SAFETY: host tests run on threads that may block.
        unsafe { Sleepable::assume() }
    }

    #[test]
    fn clocks_are_counted_per_handle() {
        let s = s();
        let fake = Arc::new(Fake::default());
        let src = ClockSource::new(fake.clone());
        let mut a = src.handle(7);
        let mut b = src.handle(7);
        a.enable(&s).unwrap();
        a.enable(&s).unwrap();
        b.enable(&s).unwrap();
        assert_eq!(fake.take(), [Call::Enable(7)]);
        assert_eq!(src.enables(&s, 7), 2);
        a.disable(&s).unwrap();
        assert_eq!(fake.take(), []);
        b.set_rate(&s, Hz(800_000_000)).unwrap();
        assert_eq!(b.round_rate(&s, Hz(1_234_567)).unwrap(), Hz(1_000_000));
        // Dropping an enabled handle gates the clock.
        drop(b);
        assert_eq!(
            fake.take(),
            [Call::SetRate(7, 800_000_000), Call::Disable(7)]
        );
        // A failed enable is not counted.
        *fake.fail.lock().unwrap() = true;
        assert_eq!(a.enable(&s), Err(ProviderError::Firmware(-1)));
        assert_eq!(src.enables(&s, 7), 0);
        a.enable(&s).unwrap();
        a.release(&s).unwrap();
        assert_eq!(fake.take(), [Call::Enable(7), Call::Disable(7)]);
    }

    #[test]
    fn a_shared_reset_asserts_when_every_holder_asks() {
        let s = s();
        let fake = Arc::new(Fake::default());
        let src = ResetSource::new(fake.clone());
        // Four GPIO controllers on one line.
        let mut h: Vec<ResetHandle> = (0..4).map(|_| src.handle(&s, 26)).collect();
        assert_eq!(src.holders(&s, 26), (4, 0));
        // Each releases it: firmware may have left the block in reset.
        for x in &mut h {
            x.deassert(&s).unwrap();
        }
        assert_eq!(fake.take(), [Call::Deassert(26); 4]);
        for x in &mut h[..3] {
            x.assert(&s).unwrap();
        }
        assert_eq!(fake.take(), []);
        h[3].assert(&s).unwrap();
        assert_eq!(fake.take(), [Call::Assert(26)]);
        h[0].deassert(&s).unwrap();
        assert_eq!(fake.take(), [Call::Deassert(26)]);
        // A pulse while others hold the line is refused.
        assert_eq!(h[0].pulse(&s, Micros(20)), Err(ProviderError::Shared));
        assert_eq!(fake.take(), []);
        // The holder that did not ask leaves: the rest all ask.
        let first = h.remove(0);
        first.release(&s).unwrap();
        assert_eq!(src.holders(&s, 26), (3, 3));
        assert_eq!(fake.take(), [Call::Assert(26)]);
        drop(h);
        assert_eq!(src.holders(&s, 26), (0, 0));
    }

    #[test]
    fn a_pulse_on_a_sole_or_safe_line() {
        let s = s();
        let fake = Arc::new(Fake::default());
        let src = ResetSource::new(fake.clone());
        let mut a = src.handle(&s, 3);
        a.assert(&s).unwrap();
        a.pulse(&s, Micros(20)).unwrap();
        assert_eq!(
            fake.take(),
            [Call::Assert(3), Call::Assert(3), Call::Deassert(3)]
        );
        assert_eq!(src.holders(&s, 3), (1, 0));
        // Already released: asserting again tells the provider.
        a.assert(&s).unwrap();
        assert_eq!(fake.take(), [Call::Assert(3)]);

        let safe = Arc::new(Fake {
            group_safe: true,
            ..Fake::default()
        });
        let src = ResetSource::new(safe.clone());
        let mut a = src.handle(&s, 9);
        let _b = src.handle(&s, 9);
        a.pulse(&s, Micros(10)).unwrap();
        assert_eq!(safe.take(), [Call::Assert(9), Call::Deassert(9)]);
    }

    #[test]
    fn a_domain_runs_at_the_highest_vote() {
        let s = s();
        let fake = Arc::new(Fake::default());
        let src = DomainSource::new(fake.clone());
        let a = src.handle(&s, 2);
        let b = src.handle(&s, 2);
        // Withdrawing a vote nobody acted on leaves firmware's state.
        drop(src.handle(&s, 2));
        assert_eq!(fake.take(), []);
        a.set(&s, PerfLevel::ON).unwrap();
        b.set(&s, PerfLevel(3)).unwrap();
        a.set(&s, PerfLevel(2)).unwrap();
        assert_eq!(fake.take(), [Call::Level(2, 1), Call::Level(2, 3)]);
        assert_eq!(src.level(&s, 2), PerfLevel(3));
        assert_eq!(b.set(&s, PerfLevel(4)), Err(ProviderError::OutOfRange));
        b.release(&s).unwrap();
        assert_eq!(fake.take(), [Call::Level(2, 2)]);
        // A refused level leaves the vote where it was.
        *fake.fail.lock().unwrap() = true;
        assert!(a.set(&s, PerfLevel(3)).is_err());
        assert_eq!(src.level(&s, 2), PerfLevel(2));
        drop(a);
        assert_eq!(fake.take(), [Call::Level(2, 0)]);
    }
}
