// SPDX-License-Identifier: BSD-2-Clause
//! The `NVFW` container firmware files come in (specification 02, section
//! 5.2): a magic word, then blocks of a four-letter tag, a length, and a
//! value padded to four bytes.

use alloc::vec::Vec;

/// `"NVFW"`.
const MAGIC: [u8; 4] = *b"NVFW";

/// Why a container is not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NvfwError {
    /// It does not start with `NVFW`.
    BadMagic,
    /// A block's header, or its value with padding, runs past the end.
    Truncated,
    /// A tag byte is not ASCII.
    BadTag,
    /// There is no `VERS` block.
    NoVersion,
    /// A string block holds a byte that is not printable ASCII.
    BadString {
        /// The block's tag.
        tag: [u8; 4],
    },
}

/// One block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block<'a> {
    /// Its tag.
    pub tag: [u8; 4],
    /// Its value, without padding.
    pub value: &'a [u8],
}

/// A parsed container, borrowing the file's bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container<'a> {
    blocks: Vec<Block<'a>>,
}

/// The tags that hold strings.
const STRINGS: [[u8; 4]; 3] = [*b"VERS", *b"FILE", *b"BLID"];

/// Parses `file`.
///
/// # Errors
///
/// [`NvfwError`] for anything the specification's four validation rules
/// reject.
pub fn parse(file: &[u8]) -> Result<Container<'_>, NvfwError> {
    let mut rest = file.strip_prefix(&MAGIC).ok_or(NvfwError::BadMagic)?;
    let mut blocks = Vec::new();
    while !rest.is_empty() {
        let (head, after) = rest.split_at_checked(8).ok_or(NvfwError::Truncated)?;
        let tag = [head[0], head[1], head[2], head[3]];
        if !tag.is_ascii() {
            return Err(NvfwError::BadTag);
        }
        let length = usize::try_from(u32::from_le_bytes([head[4], head[5], head[6], head[7]]))
            .map_err(|_| NvfwError::Truncated)?;
        let stored = length
            .checked_next_multiple_of(4)
            .ok_or(NvfwError::Truncated)?;
        if stored > after.len() {
            return Err(NvfwError::Truncated);
        }
        let (value, tail) = after.split_at(stored);
        blocks.push(Block {
            tag,
            value: &value[..length],
        });
        rest = tail;
    }
    for b in &blocks {
        if STRINGS.contains(&b.tag) && !b.value.iter().all(|c| (0x20..=0x7e).contains(c)) {
            return Err(NvfwError::BadString { tag: b.tag });
        }
    }
    if !blocks.iter().any(|b| b.tag == *b"VERS") {
        return Err(NvfwError::NoVersion);
    }
    Ok(Container { blocks })
}

impl<'a> Container<'a> {
    /// The first block tagged `tag`.
    #[must_use]
    pub fn get(&self, tag: &[u8; 4]) -> Option<&'a [u8]> {
        self.blocks.iter().find(|b| b.tag == *tag).map(|b| b.value)
    }

    /// A string block, which parsing has checked is printable.
    #[must_use]
    pub fn string(&self, tag: &[u8; 4]) -> Option<&'a str> {
        self.get(tag).and_then(|v| core::str::from_utf8(v).ok())
    }

    /// A four-byte little-endian integer block; `None` if absent or not
    /// four bytes.
    #[must_use]
    pub fn u32(&self, tag: &[u8; 4]) -> Option<u32> {
        let v: [u8; 4] = self.get(tag)?.try_into().ok()?;
        Some(u32::from_le_bytes(v))
    }

    /// The firmware's version string (`VERS`).
    #[must_use]
    pub fn version(&self) -> &'a str {
        self.string(b"VERS").unwrap_or("")
    }

    /// The blocks in file order.
    #[must_use]
    pub fn blocks(&self) -> &[Block<'a>] {
        &self.blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn block(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut v = tag.to_vec();
        v.extend(u32::try_from(value.len()).unwrap().to_le_bytes());
        v.extend(value);
        v.resize(v.len().next_multiple_of(4), 0);
        v
    }

    fn file(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut f = b"NVFW".to_vec();
        for b in blocks {
            f.extend(b);
        }
        f
    }

    #[test]
    fn a_gsp_manifest() {
        let f = file(&[
            block(b"VERS", b"570.144"),
            block(b"FILE", b"gsp.bin"),
            block(b"SIZE", &0x1234_5678u32.to_le_bytes()),
            block(b"SIGN", &[7; 5]),
        ]);
        let c = parse(&f).unwrap();
        assert_eq!(c.version(), "570.144");
        assert_eq!(c.string(b"FILE"), Some("gsp.bin"));
        assert_eq!(c.u32(b"SIZE"), Some(0x1234_5678));
        // The value is unpadded, though stored padded.
        assert_eq!(c.get(b"SIGN"), Some(&[7u8; 5][..]));
        assert_eq!(c.blocks().len(), 4);
        assert_eq!(c.get(b"BLOB"), None);
        assert_eq!(c.u32(b"SIGN"), None, "five bytes are not an integer");
    }

    #[test]
    fn the_four_rules() {
        assert_eq!(parse(b"NVTL").unwrap_err(), NvfwError::BadMagic);
        assert_eq!(parse(b"").unwrap_err(), NvfwError::BadMagic);
        // No blocks at all: no version.
        assert_eq!(parse(b"NVFW").unwrap_err(), NvfwError::NoVersion);
        assert_eq!(
            parse(&file(&[block(b"SIZE", &[0; 4])])).unwrap_err(),
            NvfwError::NoVersion
        );
        // Non-ASCII tag.
        let mut bad = block(b"VERS", b"1");
        bad[0] = 0x80;
        assert_eq!(parse(&file(&[bad])).unwrap_err(), NvfwError::BadTag);
        // A string with a NUL, a control character, or a high byte.
        for s in [&b"1\0"[..], b"a\nb", b"caf\xc3\xa9"] {
            assert_eq!(
                parse(&file(&[block(b"VERS", s)])).unwrap_err(),
                NvfwError::BadString { tag: *b"VERS" }
            );
        }
    }

    #[test]
    fn hostile_lengths() {
        let ok = block(b"VERS", b"1");
        // Cut anywhere inside the last block (or its padding): truncated.
        for n in 1..ok.len() {
            let mut f = b"NVFW".to_vec();
            f.extend(&ok[..n]);
            assert_eq!(parse(&f).unwrap_err(), NvfwError::Truncated, "{n}");
        }
        // A length that overflows when padded to four bytes, and one that
        // is merely past the end.
        for len in [u32::MAX, u32::MAX - 2, 0x7fff_ffff, 100] {
            let mut f = b"NVFWVERS".to_vec();
            f.extend(len.to_le_bytes());
            f.extend([0; 4]);
            assert_eq!(parse(&f).unwrap_err(), NvfwError::Truncated, "{len:#x}");
        }
        // Trailing bytes that are not a whole header.
        let mut f = file(&[ok]);
        f.extend([1, 2, 3]);
        assert_eq!(parse(&f).unwrap_err(), NvfwError::Truncated);
    }

    #[test]
    fn empty_values_and_exact_fits() {
        let f = file(&[
            block(b"VERS", b"1"),
            block(b"BLOB", &[]),
            block(b"SIGN", &[1; 4]),
        ]);
        let c = parse(&f).unwrap();
        assert_eq!(c.get(b"BLOB"), Some(&[][..]));
        assert_eq!(c.get(b"SIGN"), Some(&[1u8; 4][..]));
        let _ = vec![0u8];
    }
}
