// SPDX-License-Identifier: BSD-2-Clause
//! CRC-32 (IEEE 802.3, reflected polynomial `0xEDB88320`), as used by GPT,
//! Ethernet, gzip and zlib; and CRC-32C (Castagnoli, reflected polynomial
//! `0x82F63B78`), as UFS check-hashes use it.

#![no_std]
#![forbid(unsafe_code)]

/// Eight tables each ("slicing by 8"): `T[k][b]` is the CRC of byte `b`
/// followed by `k` zero bytes, so eight bytes go in one step of eight
/// independent lookups rather than eight dependent ones.
const TABLE: [[u32; 256]; 8] = build_tables(0xEDB8_8320);
const TABLE_C: [[u32; 256]; 8] = build_tables(0x82F6_3B78);

const fn build_table(poly: u32) -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i: u32 = 0;
    while i < 256 {
        let mut c = i;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { poly ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i as usize] = c;
        i += 1;
    }
    table
}

const fn build_tables(poly: u32) -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    t[0] = build_table(poly);
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            i += 1;
        }
        k += 1;
    }
    t
}

/// Continues reflected CRC state `c` over `data` with tables `t`.
fn update(t: &[[u32; 256]; 8], mut c: u32, data: &[u8]) -> u32 {
    let (words, tail) = data.as_chunks::<8>();
    for w in words {
        let lo = (c ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]])).to_le_bytes();
        c = t[7][usize::from(lo[0])]
            ^ t[6][usize::from(lo[1])]
            ^ t[5][usize::from(lo[2])]
            ^ t[4][usize::from(lo[3])]
            ^ t[3][usize::from(w[4])]
            ^ t[2][usize::from(w[5])]
            ^ t[1][usize::from(w[6])]
            ^ t[0][usize::from(w[7])];
    }
    for &b in tail {
        c = t[0][usize::from(c.to_le_bytes()[0] ^ b)] ^ (c >> 8);
    }
    c
}

/// An incremental CRC-32 computation.
#[derive(Clone, Copy, Debug)]
pub struct Crc32 {
    state: u32,
}

impl Crc32 {
    /// Starts a new checksum.
    pub const fn new() -> Self {
        Self { state: !0 }
    }

    /// Feeds more data.
    pub fn update(&mut self, data: &[u8]) {
        self.state = update(&TABLE, self.state, data);
    }

    /// Returns the checksum of everything fed so far.
    pub const fn finish(self) -> u32 {
        !self.state
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// Computes the CRC-32 of `data` in one call.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

/// Continues CRC-32C `crc` over `data` as FreeBSD's `calculate_crc32c`
/// does: from the caller's starting value (`!0` to begin), with no final
/// inversion.
#[must_use]
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    update(&TABLE_C, crc, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_check_value() {
        // CRC-32C's standard check value inverts at both ends.
        assert_eq!(crc32c(!0, b"123456789") ^ !0, 0xE306_9283);
        assert_eq!(
            crc32c(crc32c(!0, b"1234"), b"56789"),
            crc32c(!0, b"123456789")
        );
    }

    #[test]
    fn check_value() {
        // The standard check input for CRC-32/ISO-HDLC.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// The byte at a time CRC the tables speed up.
    fn bytewise(poly: u32, mut c: u32, data: &[u8]) -> u32 {
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { poly ^ (c >> 1) } else { c >> 1 };
            }
        }
        c
    }

    #[test]
    fn slicing_matches_bytewise() {
        let data: [u8; 300] = core::array::from_fn(|i| (i * 7 + 3).to_le_bytes()[0]);
        for start in 0..9 {
            for end in start..data.len() {
                let d = &data[start..end];
                assert_eq!(crc32c(!0, d), bytewise(0x82F6_3B78, !0, d));
                assert_eq!(crc32(d), !bytewise(0xEDB8_8320, !0, d));
            }
        }
    }

    #[test]
    fn empty_input() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn incremental_matches_one_shot() {
        let mut c = Crc32::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.finish(), crc32(b"123456789"));
    }
}
