// SPDX-License-Identifier: BSD-2-Clause
//! CRC-32 (IEEE 802.3, reflected polynomial `0xEDB88320`), as used by GPT,
//! Ethernet, gzip and zlib; and CRC-32C (Castagnoli, reflected polynomial
//! `0x82F63B78`), as UFS check-hashes use it.

#![no_std]
#![forbid(unsafe_code)]

const TABLE: [u32; 256] = build_table(0xEDB8_8320);
const TABLE_C: [u32; 256] = build_table(0x82F6_3B78);

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
        let mut c = self.state;
        for &b in data {
            c = TABLE[usize::from(c.to_le_bytes()[0] ^ b)] ^ (c >> 8);
        }
        self.state = c;
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
    data.iter().fold(crc, |c, &b| {
        TABLE_C[usize::from(c.to_le_bytes()[0] ^ b)] ^ (c >> 8)
    })
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
