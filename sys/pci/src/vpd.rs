// SPDX-License-Identifier: BSD-2-Clause
//! Vital Product Data (PCI Local Bus 3.0 §6.4 and appendix I): the
//! identifier string and the read-only and read-write keywords a function
//! keeps behind its VPD capability, read as FreeBSD reads them, for
//! `/dev/pci`'s `PCIOCLISTVPD`.
//!
//! - The VPD is read a dword at a time through the capability's address
//!   and data registers: the address written with the flag clear, then
//!   polled until the device sets the flag.
//! - The identifier is mandatory; the read-only keywords must end with a
//!   valid `RV` checksum, or the whole VPD is dropped; the read-write
//!   keywords are optional; a missing end tag is tolerated.
//! - The device's data is untrusted: every length is checked against what
//!   is left of its resource, and nothing reads past the 15-bit address
//!   space.
//!
//! Behavior reference: sys/dev/pci/pci.c (`pci_read_vpd`, `pci_parse_vpd`).

use alloc::vec::Vec;

use devtree::strings::PciAddress;

use crate::config::ConfigSpace;

/// The VPD capability's ID.
pub const CAP_ID: u8 = 0x03;
/// The address register (word), flag in bit 15, in the capability.
const VPD_ADDR: u16 = 2;
/// The data register (dword).
const VPD_DATA: u16 = 4;
/// The VPD address space: 15 bits.
const VPD_SPACE: u32 = 0x8000;

const TAG_END: u8 = 0x0f << 3;
const TAG_IDENT: u8 = 0x02 | 0x80;
const TAG_RO: u8 = 0x10 | 0x80;
const TAG_RW: u8 = 0x11 | 0x80;

/// A keyword and its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyword {
    pub keyword: [u8; 2],
    pub value: Vec<u8>,
}

/// What a function's VPD holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Vpd {
    /// The identifier string.
    pub ident: Vec<u8>,
    /// The read-only keywords, `RV` included.
    pub read_only: Vec<Keyword>,
    /// The read-write keywords.
    pub read_write: Vec<Keyword>,
}

/// Reads VPD bytes a dword at a time, keeping FreeBSD's running checksum.
struct Reader<F> {
    read: F,
    val: u32,
    bytes_in_val: u8,
    off: u32,
    cksum: u8,
}

impl<F: FnMut(u16) -> Option<u32>> Reader<F> {
    fn next(&mut self) -> Option<u8> {
        let byte = if self.bytes_in_val == 0 {
            if self.off >= VPD_SPACE {
                return None;
            }
            self.val = (self.read)(u16::try_from(self.off).ok()?)?;
            self.off += 4;
            self.bytes_in_val = 3;
            self.val as u8
        } else {
            self.val >>= 8;
            self.bytes_in_val -= 1;
            self.val as u8
        };
        self.cksum = self.cksum.wrapping_add(byte);
        Some(byte)
    }

    /// Takes the next byte if it is `want`; puts any other back.
    fn expect(&mut self, want: u8) -> Result<(), Miss> {
        let b = self.next().ok_or(Miss::Read)?;
        if b == want {
            return Ok(());
        }
        self.cksum = self.cksum.wrapping_sub(b);
        self.val = (self.val << 8) | u32::from(b);
        self.bytes_in_val += 1;
        Err(Miss::Other)
    }

    /// A resource's length if the next tag is `tag`.
    fn tag_size(&mut self, tag: u8) -> Result<usize, Miss> {
        self.expect(tag)?;
        if tag & 0x80 == 0 {
            return Ok(usize::from(tag & 0x07));
        }
        let lo = self.next().ok_or(Miss::Read)?;
        let hi = self.next().ok_or(Miss::Read)?;
        Ok(usize::from(u16::from_le_bytes([lo, hi])))
    }

    fn value(&mut self, len: usize) -> Option<Vec<u8>> {
        let mut v = Vec::with_capacity(len);
        for _ in 0..len {
            v.push(self.next()?);
        }
        Some(v)
    }

    /// A keyword of at most `room` bytes with its header.
    fn keyword(&mut self, room: usize) -> Option<Keyword> {
        let k0 = self.next()?;
        let k1 = self.next()?;
        let len = usize::from(self.next()?);
        if len + 3 > room {
            return None;
        }
        Some(Keyword {
            keyword: [k0, k1],
            value: self.value(len)?,
        })
    }
}

/// Why a tag was not read.
enum Miss {
    /// The device did not answer.
    Read,
    /// Another tag is there.
    Other,
}

/// Parses VPD whose dwords `read` gives by address. `None` when FreeBSD
/// keeps none: no identifier, or a read-only or read-write section it
/// could not read or whose checksum is wrong.
pub fn parse(read: impl FnMut(u16) -> Option<u32>) -> Option<Vpd> {
    let mut r = Reader {
        read,
        val: 0,
        bytes_in_val: 0,
        off: 0,
        cksum: 0,
    };
    let size = r.tag_size(TAG_IDENT).ok().filter(|&n| n > 0)?;
    let mut vpd = Vpd {
        ident: r.value(size)?,
        ..Vpd::default()
    };
    // Without a read-only section FreeBSD keeps the identifier alone.
    let Some(mut size) = r.tag_size(TAG_RO).ok().filter(|&n| n > 0) else {
        return Some(vpd);
    };
    while size > 0 {
        let k = r.keyword(size)?;
        let used = k.value.len() + 3;
        if k.keyword == *b"RV" {
            // The checksum covers everything up to and including RV's
            // first byte.
            let rest = k.value.iter().skip(1).fold(0u8, |a, &b| a.wrapping_add(b));
            r.cksum = r.cksum.wrapping_sub(rest);
            if r.cksum != 0 {
                return None;
            }
        }
        vpd.read_only.push(k);
        size -= used;
    }
    if r.cksum != 0 {
        return None;
    }
    let mut size = match r.tag_size(TAG_RW) {
        Ok(n) => n,
        Err(Miss::Other) => 0,
        Err(Miss::Read) => return None,
    };
    while size > 0 {
        let k = r.keyword(size)?;
        size -= k.value.len() + 3;
        vpd.read_write.push(k);
    }
    // FreeBSD only complains about a missing end tag.
    let _ = r.tag_size(TAG_END);
    Some(vpd)
}

/// Reads the VPD dword at `addr` through the capability at `cap`, polling
/// with `wait` (given the attempt number, false to give up) until the
/// device has it.
pub fn read_dword(
    cfg: &dyn ConfigSpace,
    a: PciAddress,
    cap: u16,
    addr: u16,
    wait: &mut dyn FnMut(u32) -> bool,
) -> Option<u32> {
    cfg.write16(a, cap + VPD_ADDR, addr & 0x7ffc);
    let mut attempt = 0;
    while cfg.read16(a, cap + VPD_ADDR) & 0x8000 == 0 {
        if !wait(attempt) {
            return None;
        }
        attempt += 1;
    }
    Some(cfg.read32(a, cap + VPD_DATA))
}
