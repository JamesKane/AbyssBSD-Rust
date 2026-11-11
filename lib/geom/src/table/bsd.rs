// SPDX-License-Identifier: BSD-2-Clause
//! The BSD scheme, read only: a disklabel in the second sector (inside
//! a `freebsd` MBR slice, or on a whole disk), its partitions `a`, `b`,
//! `d`... relative to the raw partition `c`'s offset, which is kept
//! internal (no provider).
//!
//! Behavior reference: sys/geom/part/g_part_bsd.c, sys/sys/disk/bsd.h.

use alloc::format;
use alloc::string::String;

use abi::errno::Errno;

use super::{Io, Media, Table, le16, le32};

/// `BSD_MAGIC` (`DISKMAGIC`).
const BSD_MAGIC: u32 = 0x8256_4557;
/// The boot area the label lives in (`BBSIZE`).
const BBSIZE: u64 = 8192;
/// `sizeof(struct disklabel)` with its minimum eight partitions.
const LABEL_SIZE: u32 = 148 + 8 * 16;
/// `RAW_PART`: `c`, the whole slice.
const RAW_PART: usize = 2;
const NPARTS_MIN: u32 = 8;
const NPARTS_MAX: u32 = 20;

const FS_SWAP: u8 = 1;
const FS_BSDFFS: u8 = 7;
const FS_VINUM: u8 = 14;
const FS_ZFS: u8 = 27;
const FS_NANDFS: u8 = 30;

/// A disklabel partition (`struct partition`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Part {
    pub size: u32,
    pub offset: u32,
    pub fsize: u32,
    pub fstype: u8,
    pub frag: u8,
    pub cpg: u16,
}

/// `g_part_bsd_type`.
#[must_use]
pub fn type_name(fstype: u8) -> String {
    String::from(match fstype {
        FS_NANDFS => "freebsd-nandfs",
        FS_SWAP => "freebsd-swap",
        FS_BSDFFS => "freebsd-ufs",
        FS_VINUM => "freebsd-vinum",
        FS_ZFS => "freebsd-zfs",
        _ => return format!("!{fstype}"),
    })
}

/// `g_part_bsd_probe`: both magic numbers of a label in sector 1.
pub(super) fn probe(m: &Media, io: &mut dyn Io) -> Result<i32, Errno> {
    if m.sectorsize < LABEL_SIZE || m.mediasize < BBSIZE {
        return Err(Errno::ENOSPC);
    }
    if !BBSIZE.is_multiple_of(m.ss()) {
        return Err(Errno::ENOTBLK);
    }
    let buf = io.read(m.ss(), m.ss_usize())?;
    if le32(&buf, 0) == BSD_MAGIC && le32(&buf, 132) == BSD_MAGIC {
        Ok(super::PRI_HIGH)
    } else {
        Err(Errno::ENXIO)
    }
}

impl Table {
    /// `g_part_bsd_read`.
    pub(super) fn read_bsd(&mut self, m: &Media, name: &str, io: &mut dyn Io) -> Result<(), Errno> {
        let msize = m.blocks().min(u64::from(u32::MAX));
        let area = io.read(0, usize::try_from(BBSIZE).unwrap_or(0))?;
        let buf = &area[m.ss_usize()..];
        let invalid = |io: &mut dyn Io| {
            io.log(&format!("GEOM: {name}: invalid disklabel."));
            Err(Errno::EINVAL)
        };
        if le32(buf, 40) != m.sectorsize {
            return invalid(io);
        }
        let sectors = le32(buf, 44);
        if !(1..=255).contains(&sectors) {
            return invalid(io);
        }
        if sectors != self.sectors && !self.fixgeom {
            let (chs, heads) = crate::part::geometry_heads(msize, u64::from(sectors));
            if chs != 0 {
                self.sectors = sectors;
                self.heads = u32::try_from(heads).unwrap_or(255);
            }
        }
        let heads = le32(buf, 48);
        if !(1..=255).contains(&heads) {
            return invalid(io);
        }
        if heads != self.heads && !self.fixgeom {
            self.heads = heads;
        }
        if le32(buf, 60) < 1 {
            return invalid(io);
        }
        self.first = 0;
        self.last = msize - 1;
        self.isleaf = true;
        self.entries = u32::from(le16(buf, 138));
        if !(NPARTS_MIN..=NPARTS_MAX).contains(&self.entries) {
            return invalid(io);
        }
        let offset = le32(buf, 148 + RAW_PART * 16 + 4);
        for index in (0..super::us(self.entries)).rev() {
            let p = &buf[148 + index * 16..];
            let part = Part {
                size: le32(p, 0),
                offset: le32(p, 4),
                fsize: le32(p, 8),
                fstype: p[12],
                frag: p[13],
                cpg: le16(p, 14),
            };
            if part.size == 0 || part.offset < offset {
                continue;
            }
            let start = u64::from(part.offset - offset);
            if start > self.last {
                return invalid(io);
            }
            let i = u32::try_from(index + 1).unwrap_or(0);
            let e = self.new_entry(i, start, start + u64::from(part.size) - 1);
            e.bsd = part;
            e.internal = index == RAW_PART;
        }
        Ok(())
    }
}
