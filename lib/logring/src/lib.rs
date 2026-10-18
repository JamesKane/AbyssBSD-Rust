// SPDX-License-Identifier: BSD-2-Clause
//! The persistent log ring: a byte ring in RAM that survives a warm reset,
//! so the loader can save the previous boot's kernel log on machines
//! without a serial port.
//!
//! Layout: a 64-byte header, then the data. `head` counts every byte ever
//! written; the newest `min(head, capacity)` bytes are the log. The kernel
//! writes; the loader validates, extracts, and clears. A reset can tear a
//! write, so readers treat the contents as best-effort text, but the
//! header is validated before anything is trusted.

#![no_std]
#![forbid(unsafe_code)]

/// `"ABYSSLOG"`, little-endian.
pub const MAGIC: u64 = u64::from_le_bytes(*b"ABYSSLOG");
const VERSION: u32 = 1;
/// Size of the header in front of the data.
pub const HEADER: usize = 64;

const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_CHECK: usize = 12;
const OFF_CAPACITY: usize = 16;
const OFF_HEAD: usize = 24;
const OFF_BOOTS: usize = 32;

/// Why a buffer does not hold a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Too small for a header and some data.
    TooSmall,
    /// No ring here (or it was overwritten).
    NotARing,
}

/// A ring over a byte buffer.
#[derive(Debug)]
pub struct Ring<'a> {
    buf: &'a mut [u8],
}

fn get(b: &[u8], off: usize) -> u64 {
    let mut v = [0; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

fn put(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Ties the header to the buffer size, so a stray "ABYSSLOG" is rejected.
fn check(capacity: u64) -> u32 {
    let mixed = capacity.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(VERSION);
    (mixed >> 32).to_le_bytes()[..4]
        .iter()
        .fold(0u32, |a, &b| a.rotate_left(8) ^ u32::from(b))
}

impl<'a> Ring<'a> {
    /// Writes a fresh, empty ring over `buf`.
    pub fn format(buf: &'a mut [u8], boots: u64) -> Result<Self, Error> {
        if buf.len() <= HEADER {
            return Err(Error::TooSmall);
        }
        let capacity = (buf.len() - HEADER) as u64;
        buf[..HEADER].fill(0);
        put(buf, OFF_MAGIC, MAGIC);
        buf[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
        buf[OFF_CHECK..OFF_CHECK + 4].copy_from_slice(&check(capacity).to_le_bytes());
        put(buf, OFF_CAPACITY, capacity);
        put(buf, OFF_HEAD, 0);
        put(buf, OFF_BOOTS, boots);
        Ok(Self { buf })
    }

    /// Opens an existing ring, validating its header.
    pub fn open(buf: &'a mut [u8]) -> Result<Self, Error> {
        if buf.len() <= HEADER {
            return Err(Error::TooSmall);
        }
        let capacity = (buf.len() - HEADER) as u64;
        let version = u32::from_le_bytes([
            buf[OFF_VERSION],
            buf[OFF_VERSION + 1],
            buf[OFF_VERSION + 2],
            buf[OFF_VERSION + 3],
        ]);
        let stored = u32::from_le_bytes([
            buf[OFF_CHECK],
            buf[OFF_CHECK + 1],
            buf[OFF_CHECK + 2],
            buf[OFF_CHECK + 3],
        ]);
        if get(buf, OFF_MAGIC) != MAGIC
            || version != VERSION
            || get(buf, OFF_CAPACITY) != capacity
            || stored != check(capacity)
        {
            return Err(Error::NotARing);
        }
        Ok(Self { buf })
    }

    fn capacity(&self) -> usize {
        self.buf.len() - HEADER
    }

    /// Bytes written since the ring was formatted (may exceed capacity).
    #[must_use]
    pub fn written(&self) -> u64 {
        get(self.buf, OFF_HEAD)
    }

    /// The boot counter stored at format time.
    #[must_use]
    pub fn boots(&self) -> u64 {
        get(self.buf, OFF_BOOTS)
    }

    /// Appends `bytes`, overwriting the oldest data when full.
    pub fn write(&mut self, bytes: &[u8]) {
        let cap = self.capacity();
        let head = self.written();
        // Only the newest `cap` bytes can survive.
        let skip = bytes.len().saturating_sub(cap);
        let start = head.saturating_add(skip as u64);
        let mut pos = usize::try_from(start % cap as u64).unwrap_or(0);
        for &b in &bytes[skip..] {
            self.buf[HEADER + pos] = b;
            pos = (pos + 1) % cap;
        }
        put(self.buf, OFF_HEAD, head.saturating_add(bytes.len() as u64));
    }

    /// The log, oldest first, as up to two slices.
    #[must_use]
    pub fn contents(&self) -> (&[u8], &[u8]) {
        let cap = self.capacity();
        let head = self.written();
        let data = &self.buf[HEADER..];
        match usize::try_from(head) {
            Ok(h) if h <= cap => (&data[..h], &[]),
            _ => {
                let pos = usize::try_from(head % cap as u64).unwrap_or(0);
                (&data[pos..], &data[..pos])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    use super::*;

    fn text(r: &Ring<'_>) -> Vec<u8> {
        let (a, b) = r.contents();
        [a, b].concat()
    }

    #[test]
    fn round_trip_and_wrap() {
        let mut buf = vec![0u8; HEADER + 8];
        {
            let mut r = Ring::format(&mut buf, 7).unwrap();
            r.write(b"abc");
            assert_eq!(text(&r), b"abc");
            r.write(b"defghij");
            assert_eq!(text(&r), b"cdefghij");
            assert_eq!(r.written(), 10);
        }
        let r = Ring::open(&mut buf).unwrap();
        assert_eq!(r.boots(), 7);
        assert_eq!(text(&r), b"cdefghij");
    }

    #[test]
    fn oversized_write_keeps_the_tail() {
        let mut buf = vec![0u8; HEADER + 4];
        let mut r = Ring::format(&mut buf, 0).unwrap();
        r.write(b"0123456789");
        assert_eq!(text(&r), b"6789");
    }

    #[test]
    fn rejects_foreign_memory() {
        let mut buf = vec![0u8; HEADER + 16];
        assert_eq!(Ring::open(&mut buf).unwrap_err(), Error::NotARing);
        Ring::format(&mut buf, 0).unwrap();
        // The same header on a buffer of another size is not this ring.
        let mut longer = buf.clone();
        longer.extend_from_slice(&[0; 8]);
        assert_eq!(Ring::open(&mut longer).unwrap_err(), Error::NotARing);
        buf[OFF_CHECK] ^= 1;
        assert_eq!(Ring::open(&mut buf).unwrap_err(), Error::NotARing);
        assert_eq!(Ring::open(&mut [0u8; 10]).unwrap_err(), Error::TooSmall);
    }

    #[test]
    fn garbage_head_is_harmless() {
        let mut buf = vec![0u8; HEADER + 16];
        Ring::format(&mut buf, 0).unwrap();
        put(&mut buf, OFF_HEAD, u64::MAX - 3);
        let mut r = Ring::open(&mut buf).unwrap();
        let _ = text(&r);
        r.write(b"0123456789abcdefXYZ");
        assert_eq!(text(&r).len(), 16);
    }
}
