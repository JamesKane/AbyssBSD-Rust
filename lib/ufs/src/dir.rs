// SPDX-License-Identifier: BSD-2-Clause
//! Directory blocks (`struct direct`): entries packed in 512-byte chunks
//! that none crosses, each with its inode, record length, type, and name.

use crate::Error;

/// A directory chunk (`DIRBLKSIZ`).
pub const DIRBLKSIZ: usize = 512;
/// The longest name (`UFS_MAXNAMLEN`).
pub const MAXNAMLEN: usize = 255;
/// The header before the name: inode, record length, type, name length.
const HEADER: usize = 8;

/// One directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirEntry<'a> {
    pub ino: u32,
    /// `DT_*`.
    pub kind: u8,
    pub name: &'a [u8],
    /// The byte offset of this entry in the directory, and of the next.
    pub offset: u64,
    pub next: u64,
}

/// The space an entry with an `n`-byte name needs (`DIRSIZ`).
#[must_use]
pub const fn dirsiz(n: usize) -> usize {
    (HEADER + n + 1 + 3) & !3
}

/// Calls `f` with each live entry of directory block `block`, which
/// starts at byte `base` of the directory, from chunk offset `skip` on,
/// until it returns false. A malformed entry is an error (FreeBSD's
/// `ufs_dirbadentry`), as is a block that is not whole chunks.
pub(crate) fn entries(
    block: &[u8],
    base: u64,
    skip: usize,
    f: &mut dyn FnMut(DirEntry<'_>) -> bool,
) -> Result<(), Error> {
    if !block.len().is_multiple_of(DIRBLKSIZ) {
        return Err(Error::Corrupt("directory size"));
    }
    let mut at = skip;
    while at < block.len() {
        let room = DIRBLKSIZ - at % DIRBLKSIZ;
        let e = &block[at..at + room];
        let ino = u32::from_le_bytes([e[0], e[1], e[2], e[3]]);
        let reclen = usize::from(u16::from_le_bytes([e[4], e[5]]));
        let kind = e[6];
        let namlen = usize::from(e[7]);
        if reclen == 0 || reclen % 4 != 0 || reclen > room {
            return Err(Error::Corrupt("directory entry length"));
        }
        let next = at + reclen;
        if ino != 0 {
            if namlen > MAXNAMLEN || dirsiz(namlen) > reclen {
                return Err(Error::Corrupt("directory entry name length"));
            }
            let name = &e[HEADER..HEADER + namlen];
            if name.contains(&0) || e[HEADER + namlen] != 0 {
                return Err(Error::Corrupt("directory entry name"));
            }
            let entry = DirEntry {
                ino,
                kind,
                name,
                offset: base + at as u64,
                next: base + next as u64,
            };
            if !f(entry) {
                return Ok(());
            }
        }
        at = next;
    }
    Ok(())
}
