// SPDX-License-Identifier: BSD-2-Clause
//! The MBR scheme: four entries in the first sector, CHS addresses made
//! from the table's geometry, the active flag, and the boot code around
//! them (the disk serial number at 440 kept).
//!
//! Behavior reference: sys/geom/part/g_part_mbr.c, sys/sys/disk/mbr.h.

use alloc::format;
use alloc::string::String;
use alloc::vec;

use abi::errno::Errno;

use super::{
    DOSMAGIC, DOSMAGICOFFSET, DOSPARTOFF, DOSPARTSIZE, Io, MBRSIZE, Media, NDOSPART, Table, le16,
    le32, us,
};

/// `DOSDSNOFF`: the drive serial number Windows keeps.
pub(super) const DOSDSNOFF: usize = 440;
const DOSPTYP_PMBR: u8 = 0xee;
const DOSPTYP_386BSD: u8 = 0xa5;
const DOSPTYP_LINSWP: u8 = 0x82;

/// The MBR scheme's priority among the probes (`G_PART_PROBE_PRI_NORM`).
pub(super) const PRI_NORM: i32 = -5;

/// An MBR entry (`struct dos_partition`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dos {
    pub flag: u8,
    pub shd: u8,
    pub ssect: u8,
    pub scyl: u8,
    pub typ: u8,
    pub ehd: u8,
    pub esect: u8,
    pub ecyl: u8,
    pub start: u32,
    pub size: u32,
}

/// `mbr_alias_match`: the MBR types `gpart` names, in its order.
const ALIASES: &[(u8, &str)] = &[
    (DOSPTYP_386BSD, "freebsd"),
    (0xab, "apple-boot"),
    (0xa8, "apple-ufs"),
    (0xef, "efi"),
    (0x05, "ebr"),
    (0x0f, "ebr"),
    (0x06, "fat16"),
    (0x0b, "fat32"),
    (0x0c, "fat32lba"),
    (0xaf, "apple-hfs"),
    (0x42, "ms-ldm-data"),
    (0x8e, "linux-lvm"),
    (0xfd, "linux-raid"),
    (DOSPTYP_LINSWP, "linux-swap"),
    (0x83, "linux-data"),
    (0x07, "ntfs"),
    (0x41, "prep-boot"),
    (0xfb, "vmware-vmfs"),
    (0xfc, "vmware-vmkdiag"),
];

/// `g_part_mbr_type`: the type's name, or `!` and its number.
#[must_use]
pub fn type_name(typ: u8) -> String {
    ALIASES
        .iter()
        .find(|(t, _)| *t == typ)
        .map_or_else(|| format!("!{typ}"), |(_, n)| String::from(*n))
}

/// `strtol(s, &end, 0)` with nothing after the number.
fn strtol(s: &[u8]) -> Option<i64> {
    super::strtoq(s)
}

/// `mbr_parse_type`: a type by its name (any case), or `!` and a number
/// from 1 to 255.
pub(super) fn parse_type(t: &[u8]) -> Result<u8, Errno> {
    if let Some(n) = t.strip_prefix(b"!") {
        return strtol(n)
            .filter(|v| (1..256).contains(v))
            .and_then(|v| u8::try_from(v).ok())
            .ok_or(Errno::EINVAL);
    }
    ALIASES
        .iter()
        .find(|(_, n)| n.as_bytes().eq_ignore_ascii_case(t))
        .map(|&(t, _)| t)
        .ok_or(Errno::EINVAL)
}

/// `mbr_probe_bpb`: whether a FAT BIOS parameter block could be at `bpb`.
fn probe_bpb(bpb: &[u8]) -> bool {
    let secsz = le16(bpb, 0);
    let clstsz = bpb[2];
    (512..=4096).contains(&secsz)
        && secsz.is_power_of_two()
        && (1..=128).contains(&clstsz)
        && clstsz.is_power_of_two()
}

/// `g_part_mbr_probe`: an MBR, unless nested in one (`ELOOP`), or what
/// looks like a FAT boot sector with no entries.
pub(super) fn probe(
    m: &Media,
    io: &mut dyn Io,
    parent: Option<super::Scheme>,
) -> Result<i32, Errno> {
    if us(m.sectorsize) < MBRSIZE || m.mediasize < m.ss() {
        return Err(Errno::ENOSPC);
    }
    if m.sectorsize > 4096 {
        return Err(Errno::ENXIO);
    }
    if parent == Some(super::Scheme::Mbr) {
        return Err(Errno::ELOOP);
    }
    let buf = io.read(0, m.ss_usize())?;
    if le16(&buf, DOSMAGICOFFSET) != DOSMAGIC {
        return Err(Errno::ENXIO);
    }
    if (0..NDOSPART).any(|i| !matches!(buf[DOSPARTOFF + i * DOSPARTSIZE], 0 | 0x80)) {
        return Err(Errno::ENXIO);
    }
    let sum: u32 = buf[DOSPARTOFF..DOSPARTOFF + NDOSPART * DOSPARTSIZE]
        .iter()
        .map(|&b| u32::from(b))
        .sum();
    if sum != 0 || !probe_bpb(&buf[0x0b..]) {
        Ok(PRI_NORM)
    } else {
        Err(Errno::ENXIO)
    }
}

impl Table {
    /// `mbr_set_chs`: the CHS address of `lba` (all ones past cylinder
    /// 1023): cylinder, head, sector.
    fn chs(&self, lba: u32) -> (u8, u8, u8) {
        let sectors = self.sectors.max(1);
        let heads = self.heads.max(1);
        let mut sec = lba % sectors + 1;
        let lba = lba / sectors;
        let mut hd = lba % heads;
        let mut cyl = lba / heads;
        if cyl > 1023 {
            (sec, hd, cyl) = (u32::MAX, u32::MAX, u32::MAX);
        }
        let low = |v: u32| u8::try_from(v & 0xff).unwrap_or(0);
        (low(cyl), low(hd), low((sec & 0x3f) | ((cyl >> 2) & 0xc0)))
    }

    /// The entry's start and end CHS addresses from its sectors.
    pub(super) fn set_chs(&self, d: &mut Dos, start: bool, end: bool) {
        if start {
            (d.scyl, d.shd, d.ssect) = self.chs(d.start);
        }
        if end {
            (d.ecyl, d.ehd, d.esect) = self.chs(d.start + d.size - 1);
        }
    }

    /// `g_part_mbr_read`.
    pub(super) fn read_mbr(&mut self, m: &Media, io: &mut dyn Io) -> Result<(), Errno> {
        let msize = m.blocks().min(u64::from(u32::MAX));
        let mut first = u64::from(self.sectors);
        let buf = io.read(0, m.ss_usize())?;
        self.mbr.copy_from_slice(&buf[..MBRSIZE]);
        for index in (0..NDOSPART).rev() {
            let p = &buf[DOSPARTOFF + index * DOSPARTSIZE..][..DOSPARTSIZE];
            let d = Dos {
                flag: p[0],
                shd: p[1],
                ssect: p[2],
                scyl: p[3],
                typ: p[4],
                ehd: p[5],
                esect: p[6],
                ecyl: p[7],
                start: le32(p, 8),
                size: le32(p, 12),
            };
            if d.typ == 0 || d.typ == DOSPTYP_PMBR || d.start == 0 || d.size == 0 {
                continue;
            }
            let sectors = u32::from(d.esect & 0x3f);
            if sectors > self.sectors && !self.fixgeom {
                let (chs, heads) = crate::part::geometry_heads(msize, u64::from(sectors));
                if chs != 0 {
                    self.sectors = sectors;
                    self.heads = u32::try_from(heads).unwrap_or(255);
                }
            }
            first = first.min(u64::from(d.start));
            let i = u32::try_from(index + 1).unwrap_or(0);
            let start = u64::from(d.start);
            self.new_entry(i, start, start + u64::from(d.size) - 1).dos = d;
        }
        self.entries = 4;
        self.first = u64::from(self.sectors);
        self.last = msize - 1;
        if first < self.first {
            self.first = 1;
        }
        Ok(())
    }

    /// `g_part_mbr_write`: the entries into the first sector, the rest of
    /// it as read or made.
    pub(super) fn write_mbr(&mut self, m: &Media, io: &mut dyn Io) -> Result<(), Errno> {
        for index in 1..=4u32 {
            let p = DOSPARTOFF + us(index - 1) * DOSPARTSIZE;
            let e = self.live().find(|e| e.index == index).map(|e| e.dos);
            let slot = &mut self.mbr[p..p + DOSPARTSIZE];
            match e {
                Some(d) => {
                    slot[..8].copy_from_slice(&[
                        d.flag, d.shd, d.ssect, d.scyl, d.typ, d.ehd, d.esect, d.ecyl,
                    ]);
                    slot[8..12].copy_from_slice(&d.start.to_le_bytes());
                    slot[12..16].copy_from_slice(&d.size.to_le_bytes());
                }
                None => slot.fill(0),
            }
        }
        let mut sector = vec![0u8; m.ss_usize()];
        sector[..MBRSIZE].copy_from_slice(&self.mbr);
        io.write(0, &sector)
    }

    /// `g_part_mbr_create`.
    pub(super) fn create_mbr(&mut self, m: &Media) -> Result<(), Errno> {
        if us(m.sectorsize) < MBRSIZE {
            return Err(Errno::ENOSPC);
        }
        self.first = u64::from(self.sectors);
        self.last = m.blocks().min(u64::from(u32::MAX)) - 1;
        self.mbr[DOSMAGICOFFSET..DOSMAGICOFFSET + 2].copy_from_slice(&DOSMAGIC.to_le_bytes());
        Ok(())
    }

    /// `g_part_mbr_bootcode`: exactly a sector's 512 bytes; the boot code
    /// before the entries, the disk's serial number kept unless asked.
    pub(super) fn bootcode_mbr(&mut self, code: &[u8], skip_dsn: bool) -> Result<(), Errno> {
        if code.len() != MBRSIZE {
            return Err(Errno::ENODEV);
        }
        let mut dsn = [0u8; 4];
        dsn.copy_from_slice(&self.mbr[DOSDSNOFF..DOSDSNOFF + 4]);
        self.mbr[..DOSPARTOFF].copy_from_slice(&code[..DOSPARTOFF]);
        if dsn != [0; 4] && !skip_dsn {
            self.mbr[DOSDSNOFF..DOSDSNOFF + 4].copy_from_slice(&dsn);
        }
        Ok(())
    }

    /// `g_part_mbr_setunset`: only `active`, on one entry at a time.
    pub(super) fn setunset_mbr(
        &mut self,
        i: Option<usize>,
        attrib: &[u8],
        set: bool,
    ) -> Result<(), Errno> {
        let Some(i) = i else {
            return Err(Errno::ENODEV);
        };
        if attrib != b"active" {
            return Err(Errno::EINVAL);
        }
        for (j, e) in self.list.iter_mut().enumerate() {
            if e.deleted {
                continue;
            }
            let on = e.dos.flag & 0x80 != 0;
            let changed = if j == i {
                if set && !on {
                    e.dos.flag |= 0x80;
                    true
                } else if !set && on {
                    e.dos.flag &= !0x80;
                    true
                } else {
                    false
                }
            } else if set && on {
                e.dos.flag &= !0x80;
                true
            } else {
                false
            };
            if changed && !e.created {
                e.modified = true;
            }
        }
        Ok(())
    }

    /// The disk serial number (`efimedia`'s signature).
    #[must_use]
    pub fn dsn(&self) -> u32 {
        le32(&self.mbr, DOSDSNOFF)
    }
}

/// `HD(index,MBR,%#08x,start,size)`.
#[must_use]
pub fn efimedia(index: u32, dsn: u32, start: u64, size: u64) -> String {
    // `%#08x`: no `0x` for zero.
    let sig = if dsn == 0 {
        String::from("00000000")
    } else {
        format!("{dsn:#08x}")
    };
    format!(
        "HD({index},MBR,{sig},{},{})",
        crate::part::alt_hex(start),
        crate::part::alt_hex(size)
    )
}
