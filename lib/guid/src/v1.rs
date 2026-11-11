// SPDX-License-Identifier: BSD-2-Clause
//! Version-1 (time and node) UUIDs, as `uuidgen(2)` makes them.
//!
//! The generator keeps the last UUID it gave out: a new batch whose clock
//! has not moved past the last one's takes the next clock sequence, and
//! a batch for a new node a random one. The node is the first Ethernet
//! address the system registered, or a random multicast address until
//! there is one. A batch of `count` UUIDs takes `count` consecutive
//! clock values. The time and clock-sequence fields are in the host's
//! byte order (`struct uuid`'s fields), the clock sequence big-endian.
//!
//! Behavior reference: sys/kern/kern_uuid.c (`kern_uuidgen`,
//! `uuid_node`, `uuid_time`, `uuid_ether_add`, `uuid_ether_del`).

/// 100-nanosecond intervals from the Gregorian reform (1582-10-15) to the
/// Unix epoch.
const GREGORIAN_OFFSET: u64 = 0x01B2_1DD2_1381_4000;
/// The clock is 60 bits.
const TIME_MASK: u64 = (1 << 60) - 1;
/// How many Ethernet addresses are kept (`UUID_NETHER`).
const NETHER: usize = 4;

/// The clock for a UUID: 100-nanosecond intervals since the Gregorian
/// reform, 60 bits, from the time since the Unix epoch.
#[must_use]
pub const fn clock(unix_secs: u64, nanos: u32) -> u64 {
    GREGORIAN_OFFSET
        .wrapping_add(unix_secs.wrapping_mul(10_000_000))
        .wrapping_add(nanos as u64 / 100)
        & TIME_MASK
}

/// An Ethernet address slot (`struct uuid_macaddr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Node {
    Empty,
    /// Made up for want of an interface: multicast, so no interface's.
    Random([u8; 6]),
    /// A registered interface's.
    Unique([u8; 6]),
}

/// The last UUID given out (`uuid_last`).
#[derive(Clone, Copy, Debug)]
struct Last {
    time: u64,
    /// Without the variant bits.
    seq: u16,
    node: [u8; 6],
}

/// Why an Ethernet address was not taken or removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EtherError {
    /// Multicast, locally administered, or all zeros (`EINVAL`).
    Invalid,
    /// Already registered (`EEXIST`).
    Exists,
    /// No room (`ENOSPC`).
    Full,
    /// Not registered (`ENOENT`).
    Absent,
}

/// The generator's state.
#[derive(Debug)]
pub struct Generator {
    last: Option<Last>,
    ether: [Node; NETHER],
}

impl Default for Generator {
    fn default() -> Self {
        Self::new()
    }
}

impl Generator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: None,
            ether: [Node::Empty; NETHER],
        }
    }

    /// The node: the first slot's address, a random multicast one made
    /// now if there is none (`uuid_node`).
    fn node(&mut self, random: &mut impl FnMut() -> u32) -> [u8; 6] {
        if self.ether[0] == Node::Empty {
            let mut n = [0u8; 6];
            // Three random 16-bit words.
            for pair in n.as_chunks_mut::<2>().0 {
                let [lo, hi, ..] = random().to_le_bytes();
                *pair = [lo, hi];
            }
            n[0] |= 0x01;
            self.ether[0] = Node::Random(n);
        }
        match self.ether[0] {
            Node::Random(n) | Node::Unique(n) => n,
            Node::Empty => [0; 6],
        }
    }

    /// Fills `out` with consecutive UUIDs from clock value `now`
    /// ([`clock`]), `random` supplying the clock sequence when it starts
    /// over and the node while it is made up (`kern_uuidgen`).
    pub fn generate(&mut self, now: u64, random: &mut impl FnMut() -> u32, out: &mut [[u8; 16]]) {
        let node = self.node(random);
        let time = now & TIME_MASK;
        let seq = match self.last {
            Some(l) if l.node == node && l.time != 0 => {
                if l.time >= time {
                    l.seq.wrapping_add(1) & 0x3fff
                } else {
                    l.seq
                }
            }
            #[allow(clippy::cast_possible_truncation, reason = "masked to 14 bits")]
            _ => (random() & 0x3fff) as u16,
        };
        let count = out.len() as u64;
        self.last = Some(Last {
            time: time.wrapping_add(count).wrapping_sub(1) & TIME_MASK,
            seq,
            node,
        });
        let seq = (seq | 0x8000).to_be_bytes();
        let mut t = time;
        for u in out {
            #[allow(clippy::cast_possible_truncation, reason = "the fields' bits")]
            let (low, mid, hi) = (
                t as u32,
                (t >> 32) as u16,
                ((t >> 48) as u16 & 0xfff) | 1 << 12,
            );
            u[0..4].copy_from_slice(&low.to_ne_bytes());
            u[4..6].copy_from_slice(&mid.to_ne_bytes());
            u[6..8].copy_from_slice(&hi.to_ne_bytes());
            u[8..10].copy_from_slice(&seq);
            u[10..16].copy_from_slice(&node);
            t = t.wrapping_add(1);
        }
    }

    /// Registers an interface's address (`uuid_ether_add`): it goes
    /// after the others registered, ahead of a made-up one.
    pub fn ether_add(&mut self, addr: [u8; 6]) -> Result<(), EtherError> {
        if addr[0] & 0x03 != 0 || addr.iter().all(|&b| b == 0) {
            return Err(EtherError::Invalid);
        }
        let mut i = 0;
        while i < NETHER {
            match self.ether[i] {
                Node::Unique(n) if n == addr => return Err(EtherError::Exists),
                Node::Unique(_) => i += 1,
                _ => break,
            }
        }
        if i == NETHER {
            return Err(EtherError::Full);
        }
        if matches!(self.ether[i], Node::Random(_)) && i < NETHER - 1 {
            self.ether[i + 1] = self.ether[i];
        }
        self.ether[i] = Node::Unique(addr);
        Ok(())
    }

    /// Forgets an interface's address (`uuid_ether_del`), the later ones
    /// moving up.
    pub fn ether_del(&mut self, addr: [u8; 6]) -> Result<(), EtherError> {
        let i = self
            .ether
            .iter()
            .take_while(|n| matches!(n, Node::Unique(_)))
            .position(|n| *n == Node::Unique(addr))
            .ok_or(EtherError::Absent)?;
        let mut j = i;
        while j < NETHER - 1 && self.ether[j] != Node::Empty {
            self.ether[j] = self.ether[j + 1];
            j += 1;
        }
        if j == NETHER - 1 {
            self.ether[j] = Node::Empty;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(u: &[u8; 16]) -> (u32, u16, u16, u16, [u8; 6]) {
        (
            u32::from_ne_bytes(u[0..4].try_into().unwrap()),
            u16::from_ne_bytes(u[4..6].try_into().unwrap()),
            u16::from_ne_bytes(u[6..8].try_into().unwrap()),
            u16::from_be_bytes(u[8..10].try_into().unwrap()),
            u[10..16].try_into().unwrap(),
        )
    }

    #[test]
    fn clock_counts_from_the_gregorian_reform() {
        assert_eq!(clock(0, 0), GREGORIAN_OFFSET);
        assert_eq!(clock(1, 250), GREGORIAN_OFFSET + 10_000_000 + 2);
    }

    #[test]
    fn version_variant_and_consecutive_times() {
        let mut g = Generator::new();
        let mut r = || 0x1234_5678;
        let mut out = [[0u8; 16]; 3];
        let now = clock(1_700_000_000, 0);
        g.generate(now, &mut r, &mut out);
        for (k, u) in out.iter().enumerate() {
            let (low, mid, hi, seq, node) = fields(u);
            let t = u64::from(low) | u64::from(mid) << 32 | u64::from(hi & 0xfff) << 48;
            assert_eq!(t, now + k as u64);
            assert_eq!(hi >> 12, 1, "version 1");
            assert_eq!(seq >> 14, 0b10, "the RFC 4122 variant");
            assert_eq!(seq & 0x3fff, 0x1678 & 0x3fff);
            // A made-up node is multicast.
            assert_eq!(node[0] & 1, 1);
        }
    }

    #[test]
    fn a_clock_that_did_not_move_takes_the_next_sequence() {
        let mut g = Generator::new();
        let mut r = || 7;
        let mut a = [[0u8; 16]; 2];
        let now = clock(100, 0);
        g.generate(now, &mut r, &mut a);
        let mut b = [[0u8; 16]; 1];
        // Before the end of the last batch: the sequence moves on.
        g.generate(now + 1, &mut r, &mut b);
        assert_eq!(fields(&b[0]).3 & 0x3fff, 8);
        // Past it: the sequence stays.
        g.generate(now + 5, &mut r, &mut b);
        assert_eq!(fields(&b[0]).3 & 0x3fff, 8);
    }

    #[test]
    fn a_registered_address_becomes_the_node() {
        let mut g = Generator::new();
        let mut r = || 1;
        let mut u = [[0u8; 16]; 1];
        g.generate(clock(5, 0), &mut r, &mut u);
        let mac = [0x58, 0x9c, 0xfc, 0x00, 0x00, 0x01];
        assert_eq!(g.ether_add(mac), Ok(()));
        assert_eq!(g.ether_add(mac), Err(EtherError::Exists));
        assert_eq!(g.ether_add([0x02, 0, 0, 0, 0, 1]), Err(EtherError::Invalid));
        assert_eq!(g.ether_add([0; 6]), Err(EtherError::Invalid));
        let mut r2 = || 0x55;
        g.generate(clock(6, 0), &mut r2, &mut u);
        assert_eq!(fields(&u[0]).4, mac);
        // A new node starts the sequence over.
        assert_eq!(fields(&u[0]).3 & 0x3fff, 0x55);
        assert_eq!(g.ether_del(mac), Ok(()));
        assert_eq!(g.ether_del(mac), Err(EtherError::Absent));
        // The made-up node is back.
        g.generate(clock(7, 0), &mut r2, &mut u);
        assert_eq!(fields(&u[0]).4[0] & 1, 1);
    }

    #[test]
    fn four_addresses_at_most() {
        let mut g = Generator::new();
        for i in 1..=4 {
            assert_eq!(g.ether_add([0x10, 0, 0, 0, 0, i]), Ok(()));
        }
        assert_eq!(g.ether_add([0x10, 0, 0, 0, 0, 9]), Err(EtherError::Full));
    }
}
