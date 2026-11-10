// SPDX-License-Identifier: BSD-2-Clause
//! Queue index arithmetic (Arm IHI 0070 §3.5): a queue of `2^n` entries
//! is indexed by PROD and CONS values of `n` index bits and a wrap bit
//! above them. Equal indexes with equal wrap bits mean empty; equal
//! indexes with different wrap bits mean full.
//!
//! Besides the index and wrap, CMDQ_CONS carries the command error in
//! bits 30:24 ([`crate::regs::CmdError`]), EVENTQ_PROD the overflow flag
//! in bit 31, and EVENTQ_CONS its acknowledgement in bit 31.
//!
//! Behavior reference: sys/arm64/iommu/smmu.c (`smmu_q_has_space`,
//! `smmu_q_empty`, `smmu_q_inc_prod`, `smmu_q_inc_cons`).

/// The overflow flag in EVENTQ_PROD, and its acknowledgement in
/// EVENTQ_CONS.
pub const OVERFLOW: u32 = 1 << 31;

/// A queue's geometry: `2^log2size` entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Queue {
    log2size: u8,
}

/// A PROD or CONS position: index and wrap bit, other bits cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pos(u32);

impl Pos {
    /// The raw value, as written to the register (other fields clear).
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Queue {
    /// The largest queue the architecture allows: 2^19 entries.
    pub const MAX_LOG2: u8 = 19;

    /// A queue of `2^log2size` entries, `log2size` at most
    /// [`Queue::MAX_LOG2`].
    #[must_use]
    pub const fn new(log2size: u8) -> Option<Self> {
        if log2size > Self::MAX_LOG2 {
            None
        } else {
            Some(Self { log2size })
        }
    }

    /// A queue of `2^log2size` entries, or of the largest size if
    /// `log2size` is larger.
    #[must_use]
    pub const fn clamped(log2size: u8) -> Self {
        Self {
            log2size: if log2size > Self::MAX_LOG2 {
                Self::MAX_LOG2
            } else {
                log2size
            },
        }
    }

    /// log2 of the number of entries.
    #[must_use]
    pub const fn log2size(self) -> u8 {
        self.log2size
    }

    /// The number of entries.
    #[must_use]
    pub const fn entries(self) -> u32 {
        1 << self.log2size
    }

    const fn index_mask(self) -> u32 {
        self.entries() - 1
    }

    const fn wrap_bit(self) -> u32 {
        self.entries()
    }

    /// The position in a register value, other fields dropped.
    #[must_use]
    pub const fn pos(self, reg: u32) -> Pos {
        Pos(reg & (self.index_mask() | self.wrap_bit()))
    }

    /// The entry index of a position.
    #[must_use]
    pub const fn index(self, p: Pos) -> u32 {
        p.0 & self.index_mask()
    }

    /// The wrap bit of a position.
    #[must_use]
    pub const fn wrap(self, p: Pos) -> bool {
        p.0 & self.wrap_bit() != 0
    }

    /// The position after `p`, toggling the wrap bit past the last entry.
    #[must_use]
    pub const fn next(self, p: Pos) -> Pos {
        self.advance(p, 1)
    }

    /// The position `n` entries after `p`.
    #[must_use]
    pub const fn advance(self, p: Pos, n: u32) -> Pos {
        // Index and wrap together count modulo twice the size.
        Pos(p.0.wrapping_add(n) & (self.index_mask() | self.wrap_bit()))
    }

    /// Whether the queue is empty.
    #[must_use]
    pub const fn is_empty(self, prod: Pos, cons: Pos) -> bool {
        prod.0 == cons.0
    }

    /// Whether the queue is full.
    #[must_use]
    pub const fn is_full(self, prod: Pos, cons: Pos) -> bool {
        self.index(prod) == self.index(cons) && self.wrap(prod) != self.wrap(cons)
    }

    /// Entries between `cons` and `prod`.
    #[must_use]
    pub const fn used(self, prod: Pos, cons: Pos) -> u32 {
        prod.0.wrapping_sub(cons.0) & (self.index_mask() | self.wrap_bit())
    }

    /// Entries free for the producer.
    #[must_use]
    pub const fn space(self, prod: Pos, cons: Pos) -> u32 {
        self.entries() - self.used(prod, cons)
    }

    /// Whether `p` lies in `[cons, prod)`: an entry not yet consumed.
    #[must_use]
    pub const fn pending(self, p: Pos, prod: Pos, cons: Pos) -> bool {
        self.used(p, cons) < self.used(prod, cons)
    }

    /// Whether the consumer has passed `p`, an entry the producer
    /// published: how a polled `CMD_SYNC` is seen complete (CONS has moved
    /// beyond the sync's slot).
    #[must_use]
    pub const fn consumed(self, p: Pos, prod: Pos, cons: Pos) -> bool {
        !self.pending(p, prod, cons)
    }

    /// The byte offset of entry `p` in a queue of `entry_bytes` entries.
    #[must_use]
    pub const fn offset(self, p: Pos, entry_bytes: u64) -> u64 {
        self.index(p) as u64 * entry_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Rng;

    #[test]
    fn wrap_at_the_boundaries() {
        let q = Queue::new(2).unwrap();
        let mut p = q.pos(0);
        let c = q.pos(0);
        assert!(q.is_empty(p, c) && !q.is_full(p, c));
        assert_eq!(q.space(p, c), 4);
        for i in 1..=4 {
            p = q.next(p);
            assert_eq!(q.used(p, c), i);
        }
        assert_eq!(p.get(), 0b100);
        assert!(q.wrap(p) && q.index(p) == 0);
        assert!(q.is_full(p, c) && !q.is_empty(p, c));
        assert_eq!(q.space(p, c), 0);
        // Four more come back to zero with the wrap bit clear.
        assert_eq!(q.advance(p, 4), q.pos(0));
        // Extra register bits (CMDQ_CONS.ERR, the overflow flags) are
        // ignored.
        assert_eq!(q.pos(0x7f00_0005 | OVERFLOW), q.pos(5));
        assert_eq!(q.pos(5).get(), 5);
        assert_eq!(q.offset(q.pos(0b111), 16), 48);
        assert!(Queue::new(20).is_none());
        assert_eq!(Queue::new(19).unwrap().entries(), 1 << 19);
        // A one-entry queue: index always 0, the wrap bit alternates.
        let one = Queue::new(0).unwrap();
        let z = one.pos(0);
        assert!(one.is_full(one.next(z), z));
        assert_eq!(one.next(one.next(z)), z);
    }

    #[test]
    fn sync_completion() {
        let q = Queue::new(3).unwrap();
        let cons = q.pos(0b0110);
        let sync = q.pos(0b1001);
        let prod = q.next(sync);
        assert!(q.pending(sync, prod, cons));
        // CONS moving onto the sync's slot is not enough...
        assert!(!q.consumed(sync, prod, sync));
        // ...past it is.
        assert!(q.consumed(sync, prod, prod));
    }

    /// Against a model that counts entries with plain integers.
    #[test]
    fn random_against_counter() {
        let mut rng = Rng(0x5eed);
        for log2 in 0..=8 {
            let q = Queue::new(log2).unwrap();
            let n = u64::from(q.entries());
            let (mut prod, mut cons) = (q.pos(0), q.pos(0));
            let (mut produced, mut consumed) = (0u64, 0u64);
            for _ in 0..2000 {
                if rng.chance(2) && produced - consumed < n {
                    prod = q.next(prod);
                    produced += 1;
                } else if produced > consumed {
                    cons = q.next(cons);
                    consumed += 1;
                }
                let used = produced - consumed;
                assert_eq!(u64::from(q.used(prod, cons)), used);
                assert_eq!(q.is_empty(prod, cons), used == 0);
                assert_eq!(q.is_full(prod, cons), used == n);
                assert_eq!(u64::from(q.index(prod)), produced % n);
                assert_eq!(q.wrap(prod), (produced / n) % 2 == 1);
            }
        }
    }
}
