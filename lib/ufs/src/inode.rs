// SPDX-License-Identifier: BSD-2-Clause
//! Inodes (`struct ufs2_dinode`).

use crate::sb::{CK_INODE, Superblock};
use crate::{Error, NIADDR};
use crc::crc32c;

/// `sizeof(struct ufs2_dinode)`.
pub(crate) const SIZE: usize = 256;

/// Field offsets in `struct ufs2_dinode` (checked against FreeBSD 15.1's
/// headers).
mod off {
    pub(super) const MODE: usize = 0;
    pub(super) const NLINK: usize = 2;
    pub(super) const UID: usize = 4;
    pub(super) const GID: usize = 8;
    pub(super) const BLKSIZE: usize = 12;
    pub(super) const SIZE: usize = 16;
    pub(super) const BLOCKS: usize = 24;
    pub(super) const ATIME: usize = 32;
    pub(super) const MTIME: usize = 40;
    pub(super) const CTIME: usize = 48;
    pub(super) const BIRTHTIME: usize = 56;
    pub(super) const MTIMENSEC: usize = 64;
    pub(super) const ATIMENSEC: usize = 68;
    pub(super) const CTIMENSEC: usize = 72;
    pub(super) const BIRTHNSEC: usize = 76;
    pub(super) const GEN: usize = 80;
    pub(super) const FLAGS: usize = 88;
    pub(super) const EXTSIZE: usize = 92;
    pub(super) const DB: usize = 112;
    pub(super) const IB: usize = 208;
    pub(super) const DIRDEPTH: usize = 240;
    pub(super) const CKHASH: usize = 244;
}

/// What an inode is (`di_mode & IFMT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fifo,
    CharDevice,
    Directory,
    BlockDevice,
    Regular,
    Symlink,
    Socket,
    Whiteout,
    /// Unallocated, or a type that does not exist.
    None,
}

/// A time as UFS2 keeps it: seconds and nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timestamp {
    pub sec: i64,
    pub nsec: u32,
}

/// A checked UFS2 inode.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct Inode {
    /// Type and permission bits; 0 if unallocated.
    pub mode: u16,
    /// The link count written to the disk.
    pub nlink: u16,
    /// The link count everyone sees (FreeBSD's `i_effnlink`): with soft
    /// updates, a removed name's link goes from [`Self::nlink`] only once
    /// the removal is on the disk.
    pub effnlink: u16,
    pub uid: u32,
    pub gid: u32,
    pub blksize: u32,
    pub size: u64,
    /// Space held, in 512-byte units.
    pub blocks: u64,
    pub atime: Timestamp,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
    pub birthtime: Timestamp,
    pub generation: u32,
    /// `chflags` flags.
    pub flags: u32,
    pub extsize: u32,
    /// Fragment addresses of the direct blocks, and of the single,
    /// double, and triple indirect blocks; 0 is a hole.
    pub db: [u64; 12],
    pub ib: [u64; NIADDR],
    /// A directory's depth from the root (`di_dirdepth`).
    pub dirdepth: u32,
    /// The inode as read, so writing it back keeps what is not decoded.
    pub raw: [u8; SIZE],
    /// A large directory's index (in memory only).
    pub dirhash: Option<alloc::boxed::Box<crate::dirhash::DirHash>>,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(x)
}

fn i64_at(b: &[u8], at: usize) -> i64 {
    u64_at(b, at).cast_signed()
}

impl Inode {
    /// Parses inode bytes `b`, checking its check-hash if the file system
    /// keeps them (FreeBSD's `ffs_verify_dinode_ckhash`) and that its
    /// block addresses lie inside the file system.
    pub fn parse(b: &[u8], sb: &Superblock) -> Result<Self, Error> {
        let b = b.get(..SIZE).ok_or(Error::Corrupt("inode"))?;
        let mode = u16_at(b, off::MODE);
        if mode != 0 && sb.metackhash & CK_INODE != 0 {
            let mut crc = crc32c(!0, &b[..off::CKHASH]);
            crc = crc32c(crc, &[0; 4]);
            crc = crc32c(crc, &b[off::CKHASH + 4..]);
            if crc != u32_at(b, off::CKHASH) {
                return Err(Error::Integrity("inode check-hash"));
            }
        }
        let time = |s, ns| Timestamp {
            sec: i64_at(b, s),
            nsec: u32_at(b, ns),
        };
        let mut db = [0; 12];
        for (i, d) in db.iter_mut().enumerate() {
            *d = u64_at(b, off::DB + i * 8);
        }
        let mut ib = [0; NIADDR];
        for (i, d) in ib.iter_mut().enumerate() {
            *d = u64_at(b, off::IB + i * 8);
        }
        let mut raw = [0u8; SIZE];
        raw.copy_from_slice(b);
        let ip = Self {
            dirdepth: u32_at(b, off::DIRDEPTH),
            raw,
            dirhash: None,
            mode,
            nlink: u16_at(b, off::NLINK),
            effnlink: u16_at(b, off::NLINK),
            uid: u32_at(b, off::UID),
            gid: u32_at(b, off::GID),
            blksize: u32_at(b, off::BLKSIZE),
            size: u64_at(b, off::SIZE),
            blocks: u64_at(b, off::BLOCKS),
            atime: time(off::ATIME, off::ATIMENSEC),
            mtime: time(off::MTIME, off::MTIMENSEC),
            ctime: time(off::CTIME, off::CTIMENSEC),
            birthtime: time(off::BIRTHTIME, off::BIRTHNSEC),
            generation: u32_at(b, off::GEN),
            flags: u32_at(b, off::FLAGS),
            extsize: u32_at(b, off::EXTSIZE),
            db,
            ib,
        };
        if mode == 0 {
            return Ok(ip);
        }
        if ip.size > sb.maxfilesize {
            return Err(Error::Corrupt("inode size"));
        }
        // Only these have blocks: a short symbolic link keeps its target
        // where the addresses go, and a device its number.
        let has_blocks = match ip.kind() {
            Kind::Regular | Kind::Directory => true,
            Kind::Symlink => !ip.is_short_symlink(sb),
            _ => false,
        };
        if has_blocks && ip.db.iter().chain(&ip.ib).any(|&a| a >= sb.size) {
            return Err(Error::Corrupt("inode block address"));
        }
        Ok(ip)
    }

    /// The inode's bytes, from its fields and what was read, with its
    /// check-hash if the file system keeps them (FreeBSD's
    /// `ffs_update_dinode_ckhash`).
    #[must_use]
    pub fn encode(&self, sb: &Superblock) -> [u8; SIZE] {
        let mut b = self.raw;
        let p16 =
            |b: &mut [u8; SIZE], at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let p32 =
            |b: &mut [u8; SIZE], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let p64 =
            |b: &mut [u8; SIZE], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        p16(&mut b, off::MODE, self.mode);
        p16(&mut b, off::NLINK, self.nlink);
        p32(&mut b, off::UID, self.uid);
        p32(&mut b, off::GID, self.gid);
        p32(&mut b, off::BLKSIZE, self.blksize);
        p64(&mut b, off::SIZE, self.size);
        p64(&mut b, off::BLOCKS, self.blocks);
        for (at, ns, t) in [
            (off::ATIME, off::ATIMENSEC, self.atime),
            (off::MTIME, off::MTIMENSEC, self.mtime),
            (off::CTIME, off::CTIMENSEC, self.ctime),
            (off::BIRTHTIME, off::BIRTHNSEC, self.birthtime),
        ] {
            p64(&mut b, at, t.sec.cast_unsigned());
            p32(&mut b, ns, t.nsec);
        }
        p32(&mut b, off::GEN, self.generation);
        p32(&mut b, off::FLAGS, self.flags);
        p32(&mut b, off::EXTSIZE, self.extsize);
        for (i, a) in self.db.iter().enumerate() {
            p64(&mut b, off::DB + i * 8, *a);
        }
        for (i, a) in self.ib.iter().enumerate() {
            p64(&mut b, off::IB + i * 8, *a);
        }
        p32(&mut b, off::DIRDEPTH, self.dirdepth);
        p32(&mut b, off::CKHASH, 0);
        if self.mode != 0 && sb.metackhash & CK_INODE != 0 {
            let crc = crc32c(!0, &b);
            p32(&mut b, off::CKHASH, crc);
        }
        b
    }

    /// Takes a new link (both counts).
    pub fn link_up(&mut self) {
        self.nlink += 1;
        self.effnlink += 1;
    }

    /// Sets a short symbolic link's target into the address fields.
    pub fn set_shortlink(&mut self, target: &[u8]) {
        let mut bytes = [0u8; 120];
        bytes[..target.len().min(120)].copy_from_slice(&target[..target.len().min(120)]);
        for (i, a) in self.db.iter_mut().chain(self.ib.iter_mut()).enumerate() {
            let mut x = [0u8; 8];
            x.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
            *a = u64::from_le_bytes(x);
        }
        self.size = crate::len64(target.len());
    }

    #[must_use]
    pub fn kind(&self) -> Kind {
        match self.mode & 0o170_000 {
            0o010_000 => Kind::Fifo,
            0o020_000 => Kind::CharDevice,
            0o040_000 => Kind::Directory,
            0o060_000 => Kind::BlockDevice,
            0o100_000 => Kind::Regular,
            0o120_000 => Kind::Symlink,
            0o140_000 => Kind::Socket,
            0o160_000 => Kind::Whiteout,
            _ => Kind::None,
        }
    }

    /// The permission bits (with set-id and sticky).
    #[must_use]
    pub fn perm(&self) -> u16 {
        self.mode & 0o7777
    }

    fn is_short_symlink(&self, sb: &Superblock) -> bool {
        self.kind() == Kind::Symlink && self.size < u64::from(sb.maxsymlinklen)
    }

    /// A short symbolic link's target, kept in the address fields.
    #[must_use]
    pub fn shortlink(&self) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec::Vec::with_capacity(120);
        for a in self.db.iter().chain(&self.ib) {
            out.extend_from_slice(&a.to_le_bytes());
        }
        out.truncate(usize::try_from(self.size).unwrap_or(0).min(120));
        out
    }

    /// For a character or block device, its device number (`di_rdev`,
    /// which is `di_db[0]`).
    #[must_use]
    pub fn rdev(&self) -> u64 {
        self.db[0]
    }
}

/// Editing an encoded inode in place, for soft updates' rollbacks.
pub(crate) mod raw {
    use super::{SIZE, off, u64_at};
    use crc::crc32c;

    /// Zeroes inode bytes `b` but the generation number: an allocated
    /// inode whose group is not on the disk yet.
    pub(crate) fn zero_but_generation(b: &mut [u8]) {
        let mut generation = [0; 4];
        generation.copy_from_slice(&b[off::GEN..off::GEN + 4]);
        b[..SIZE].fill(0);
        b[off::GEN..off::GEN + 4].copy_from_slice(&generation);
    }

    /// Block pointer `index`: a direct block (0 to 11) or an indirect
    /// root (12 to 14).
    pub(crate) fn pointer(b: &[u8], index: usize) -> u64 {
        u64_at(b, pointer_at(index))
    }

    pub(crate) fn set_pointer(b: &mut [u8], index: usize, v: u64) {
        let at = pointer_at(index);
        b[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn pointer_at(index: usize) -> usize {
        if index < crate::NDIRECT {
            off::DB + index * 8
        } else {
            off::IB + (index - crate::NDIRECT) * 8
        }
    }

    /// Whether inode bytes `b` hold block pointers (a regular file, a
    /// directory, or a symbolic link too long for the inode).
    pub(crate) fn has_blocks(b: &[u8], maxsymlinklen: u32) -> bool {
        let mode = u16::from_le_bytes([b[off::MODE], b[off::MODE + 1]]);
        match mode & 0o170_000 {
            0o100_000 | 0o040_000 => true,
            0o120_000 => size(b) >= u64::from(maxsymlinklen),
            _ => false,
        }
    }

    pub(crate) fn size(b: &[u8]) -> u64 {
        u64_at(b, off::SIZE)
    }

    pub(crate) fn set_size(b: &mut [u8], v: u64) {
        b[off::SIZE..off::SIZE + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Recomputes the check-hash of allocated inode bytes `b`, if kept.
    pub(crate) fn rehash(b: &mut [u8], keep: bool) {
        b[off::CKHASH..off::CKHASH + 4].fill(0);
        let mode = u16::from_le_bytes([b[off::MODE], b[off::MODE + 1]]);
        if keep && mode != 0 {
            let crc = crc32c(!0, &b[..SIZE]);
            b[off::CKHASH..off::CKHASH + 4].copy_from_slice(&crc.to_le_bytes());
        }
    }
}
