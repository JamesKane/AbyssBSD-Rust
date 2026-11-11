// SPDX-License-Identifier: BSD-2-Clause
//! The PART class's partition tables: what a taste reads from a disk, the
//! table each verb of `gpart` changes in memory, and what a commit writes
//! back. GPT is the one scheme ([`Scheme::Gpt`]); [`Scheme::Null`] is the
//! empty table `destroy` leaves until it is committed.
//!
//! The model follows `g_part.c`'s split: the class keeps the table's
//! entries with their `created`, `modified`, and `deleted` marks, and the
//! scheme (`g_part_gpt.c`) its on-disk form: the protective MBR, the
//! header, where each of the four GPT elements lies, and what state each
//! was found in. [`ctlreq`] is the class's `ctlreq` method: it parses a
//! request's arguments, takes the table's consumer open for writing
//! (`gpt_opened`), runs the verb, and commits it when the flags say `C`.
//! The geoms, providers, and I/O it works on are the caller's
//! ([`Topology`]).
//!
//! Behavior reference: sys/geom/part/g_part.c, sys/geom/part/g_part_gpt.c,
//! sys/sys/disk/gpt.h, sys/sys/disk/mbr.h, sys/kern/kern_uuid.c
//! (`parse_uuid`).

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use abi::errno::Errno;
use crc::crc32;
use guid::Guid;

use crate::ctl::{Request, Show};

/// The protective MBR's size, and the boot code GPT keeps (`MBRSIZE`).
pub const MBRSIZE: usize = 512;
const DOSPARTOFF: usize = 446;
const DOSPARTSIZE: usize = 16;
const NDOSPART: usize = 4;
const DOSMAGICOFFSET: usize = 510;
const DOSMAGIC: u16 = 0xaa55;
const GPT_HDR_SIG: &[u8; 8] = b"EFI PART";
const GPT_HDR_REVISION: u32 = 0x0001_0000;
/// `offsetof(struct gpt_hdr, padding)`.
const GPT_HDR_SIZE: u32 = 92;
const GPT_MIN_RESERVED: u64 = 16384;
const ENT_SIZE: u32 = 128;
const GPS_MINENT: u32 = 1;
const GPS_DEFENT: u32 = 128;
const GPS_MAXENT: u32 = 4096;
const MAXENTSIZE: u32 = 1024;
/// `gps_bootcodesz` of GPT.
const BOOTCODESZ: usize = MBRSIZE;
/// GPT partition names are 36 UTF-16 code units.
pub const NAME_UNITS: usize = 36;

/// `GPT_ENT_ATTR_BOOTME`.
pub const ATTR_BOOTME: u64 = 1 << 59;
/// `GPT_ENT_ATTR_BOOTONCE`.
pub const ATTR_BOOTONCE: u64 = 1 << 58;
/// `GPT_ENT_ATTR_BOOTFAILED`.
pub const ATTR_BOOTFAILED: u64 = 1 << 57;

/// `kern.geom.debugflags`: opens and resizes that would shoot one's foot
/// are allowed (`G_F_FOOTSHOOTING`).
pub const G_F_FOOTSHOOTING: u32 = 0x10;

/// The `PART` class's probe priorities (`G_PART_PROBE_PRI_*`).
const PRI_LOW: i32 = -10;
const PRI_HIGH: i32 = 0;

/// What a table lies on: its provider's size and geometry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Media {
    pub mediasize: u64,
    pub sectorsize: u32,
    pub stripesize: u64,
    pub stripeoffset: u64,
    /// `GEOM::fwheads` (0 for none).
    pub fwheads: u32,
    /// `GEOM::fwsectors` (0 for none).
    pub fwsectors: u32,
}

impl Media {
    /// Its size in sectors.
    #[must_use]
    pub fn blocks(&self) -> u64 {
        self.mediasize
            .checked_div(u64::from(self.sectorsize))
            .unwrap_or(0)
    }

    fn ss(&self) -> u64 {
        u64::from(self.sectorsize)
    }

    fn ss_usize(&self) -> usize {
        usize::try_from(self.sectorsize).unwrap_or(usize::MAX)
    }
}

/// A table's scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// `g_part_null_scheme`: what `destroy` leaves until committed.
    Null,
    /// The GUID Partition Table.
    Gpt,
}

impl Scheme {
    /// Its name, as `dumpconf` shows it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Null => "(none)",
            Self::Gpt => "GPT",
        }
    }
}

/// What a GPT element was found to be (`enum gpt_state`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Unknown,
    Missing,
    Corrupt,
    Invalid,
    Unsupported,
    Ok,
}

/// The four elements (`enum gpt_elt`).
const PRIHDR: usize = 0;
const PRITBL: usize = 1;
const SECHDR: usize = 2;
const SECTBL: usize = 3;

/// A GPT header (`struct gpt_hdr`), as read or made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hdr {
    revision: u32,
    size: u32,
    lba_self: u64,
    lba_alt: u64,
    lba_start: u64,
    lba_end: u64,
    uuid: Guid,
    lba_table: u64,
    entries: u32,
    entsz: u32,
    crc_table: u32,
}

/// A GPT entry (`struct gpt_ent`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ent {
    pub type_guid: Guid,
    pub uuid: Guid,
    pub lba_start: u64,
    pub lba_end: u64,
    pub attr: u64,
    /// UTF-16 code units, NUL-padded.
    pub name: [u16; NAME_UNITS],
}

impl Ent {
    const ZERO: Self = Self {
        type_guid: Guid::NIL,
        uuid: Guid::NIL,
        lba_start: 0,
        lba_end: 0,
        attr: 0,
        name: [0; NAME_UNITS],
    };
}

/// A table entry (`struct g_part_entry` with its GPT part).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 1-based.
    pub index: u32,
    pub start: u64,
    pub end: u64,
    /// Added since the last commit.
    pub created: bool,
    /// Changed since the last commit.
    pub modified: bool,
    /// Deleted since the last commit (its provider is gone).
    pub deleted: bool,
    pub ent: Ent,
}

impl Entry {
    /// The provider's name suffix (`g_part_gpt_name`): `s` and the index
    /// for the `freebsd` type, `p` and the index otherwise.
    #[must_use]
    pub fn name(&self) -> String {
        let c = if self.ent.type_guid == TYPE_FREEBSD {
            's'
        } else {
            'p'
        };
        format!("{c}{}", self.index)
    }

    /// The type as `gpart` names it (`g_part_gpt_type`).
    #[must_use]
    pub fn type_name(&self) -> String {
        crate::part::gpt_type(&lower(self.ent.type_guid))
    }
}

/// A GUID's text in lower case, as `sbuf_printf_uuid` writes it.
#[must_use]
pub fn lower(g: Guid) -> String {
    format!("{g}").to_ascii_lowercase()
}

const TYPE_FREEBSD: Guid = Guid::must_parse("516E7CB4-6ECF-11D6-8FF8-00022D09712B");
const TYPE_EFI: Guid = Guid::must_parse("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
const TYPE_APPLE_HFS: Guid = Guid::must_parse("48465300-0000-11AA-AA11-00306543ECAC");

/// A partition table, as the class holds it.
#[derive(Clone, Debug)]
pub struct Table {
    pub scheme: Scheme,
    /// Entries the table has room for.
    pub entries: u32,
    /// The first sector partitions may use.
    pub first: u64,
    /// The last.
    pub last: u64,
    /// The geometry `g_part_geometry` gave it.
    pub heads: u32,
    pub sectors: u32,
    /// How deep it is nested (0 on a disk).
    pub depth: u32,
    /// Found damaged (`state: CORRUPT`).
    pub corrupt: bool,
    /// Its consumer is open for writing: there are changes to commit
    /// (`modified: true`).
    pub opened: bool,
    /// Made by `create` and not committed yet.
    pub created: bool,
    /// Sectors from the start and from the end to wipe at the commit.
    pub smhead: u32,
    pub smtail: u32,
    /// The entries, by index.
    pub list: Vec<Entry>,
    mbr: [u8; MBRSIZE],
    hdr: Option<Hdr>,
    lba: [u64; 4],
    state: [State; 4],
    bootcamp: bool,
}

/// I/O on the provider a table lies on, and the console.
pub trait Io {
    /// Reads `len` bytes at byte `offset`.
    ///
    /// # Errors
    ///
    /// The provider's error.
    fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, Errno>;
    /// Writes `data` at byte `offset`.
    ///
    /// # Errors
    ///
    /// The provider's error.
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), Errno>;
    /// Prints a line on the console (`printf`).
    fn log(&mut self, line: &str);
}

impl Table {
    fn new(scheme: Scheme, depth: u32) -> Self {
        Self {
            scheme,
            entries: 0,
            first: 0,
            last: 0,
            heads: 0,
            sectors: 0,
            depth,
            corrupt: false,
            opened: false,
            created: false,
            smhead: 0,
            smtail: 0,
            list: Vec::new(),
            mbr: [0; MBRSIZE],
            hdr: None,
            lba: [0; 4],
            state: [State::Unknown; 4],
            bootcamp: false,
        }
    }

    /// The disk's GUID, for GPT.
    #[must_use]
    pub fn disk_guid(&self) -> Option<Guid> {
        self.hdr.map(|h| h.uuid)
    }

    /// The entries in use: not deleted.
    pub fn live(&self) -> impl Iterator<Item = &Entry> {
        self.list.iter().filter(|e| !e.deleted)
    }

    /// `g_part_geometry`.
    fn geometry(&mut self, m: &Media) {
        let (heads, sectors) = crate::part::geometry(m.blocks(), Some((m.fwheads, m.fwsectors)));
        self.heads = heads;
        self.sectors = sectors;
    }

    /// `g_part_new_entry`: the entry at `index`, made (in order) if there
    /// is none, with `start` and `end`.
    fn new_entry(&mut self, index: u32, start: u64, end: u64) -> &mut Entry {
        let at = match self.list.binary_search_by_key(&index, |e| e.index) {
            Ok(i) => i,
            Err(i) => {
                self.list.insert(
                    i,
                    Entry {
                        index,
                        start,
                        end,
                        created: false,
                        modified: false,
                        deleted: false,
                        ent: Ent::ZERO,
                    },
                );
                i
            }
        };
        let e = &mut self.list[at];
        e.start = start;
        e.end = end;
        e
    }

    /// `g_part_check_integrity`: the table fits the provider and no two
    /// entries overlap. A failure is an error when `enforce`
    /// (`kern.geom.part.check_integrity`), else marks the table corrupt.
    fn check_integrity(
        &mut self,
        m: &Media,
        name: &str,
        io: &mut dyn Io,
        enforce: bool,
    ) -> Result<(), Errno> {
        let mut failed = self.last < self.first || self.last > m.blocks().wrapping_sub(1);
        let live: Vec<&Entry> = self.live().collect();
        for (i, e1) in live.iter().enumerate() {
            if e1.start < self.first
                || e1.start > self.last
                || e1.end < e1.start
                || e1.end > self.last
            {
                failed = true;
            }
            for e2 in &live[i + 1..] {
                if (e1.start >= e2.start && e1.start <= e2.end)
                    || (e1.end >= e2.start && e1.end <= e2.end)
                    || (e1.start < e2.start && e1.end > e2.end)
                {
                    failed = true;
                }
            }
        }
        if failed {
            io.log(&format!(
                "GEOM_PART: integrity check failed ({name}, {})",
                self.scheme.name()
            ));
            if enforce {
                return Err(Errno::EINVAL);
            }
            self.corrupt = true;
        }
        Ok(())
    }
}

// --- Reading -------------------------------------------------------------

/// A count or size as an index (`usize` holds every `u32` here).
fn us(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

fn guid_at(b: &[u8], at: usize) -> Guid {
    let mut v = [0u8; 16];
    v.copy_from_slice(&b[at..at + 16]);
    Guid::from_uefi_bytes(v)
}

/// `g_part_gpt_probe`: whether a GPT is there, and how sure (a
/// protective MBR makes it certain).
fn probe_gpt(m: &Media, io: &mut dyn Io) -> Result<i32, Errno> {
    if us(m.sectorsize) < MBRSIZE || m.mediasize < 6 * m.ss() {
        return Err(Errno::ENOSPC);
    }
    let ss = m.ss_usize();
    let buf = io.read(0, ss)?;
    let mut pri = PRI_LOW;
    if le16(&buf, DOSMAGICOFFSET) == DOSMAGIC {
        if (0..NDOSPART).any(|i| buf[DOSPARTOFF + DOSPARTSIZE * i + 4] == 0xee) {
            pri = PRI_HIGH;
        }
        let buf = io.read(m.ss(), ss)?;
        if &buf[..8] == GPT_HDR_SIG {
            return Ok(pri);
        }
    }
    let buf = io.read(m.mediasize - m.ss(), ss)?;
    if &buf[..8] == GPT_HDR_SIG {
        Ok(pri)
    } else {
        Err(Errno::ENXIO)
    }
}

impl Table {
    /// `gpt_read_hdr`.
    fn read_hdr(&mut self, m: &Media, io: &mut dyn Io, elt: usize, enforce: bool) -> Option<Hdr> {
        let last = m.blocks() - 1;
        self.state[elt] = State::Missing;
        if elt == SECHDR {
            if self.state[PRIHDR] != State::Ok {
                self.lba[elt] = last;
            }
        } else {
            self.lba[elt] = 1;
        }
        let mut buf = io
            .read(self.lba[elt].checked_mul(m.ss())?, m.ss_usize())
            .ok()?;
        if &buf[..8] != GPT_HDR_SIG {
            return None;
        }
        self.state[elt] = State::Corrupt;
        let size = le32(&buf, 12);
        if size < GPT_HDR_SIZE || size > m.sectorsize {
            return None;
        }
        let crc = le32(&buf, 16);
        buf[16..20].fill(0);
        if crc32(&buf[..us(size)]) != crc {
            return None;
        }
        self.state[elt] = State::Invalid;
        let h = Hdr {
            revision: le32(&buf, 8),
            size,
            lba_self: le64(&buf, 24),
            lba_alt: le64(&buf, 32),
            lba_start: le64(&buf, 40),
            lba_end: le64(&buf, 48),
            uuid: guid_at(&buf, 56),
            lba_table: le64(&buf, 72),
            entries: le32(&buf, 80),
            entsz: le32(&buf, 84),
            crc_table: le32(&buf, 88),
        };
        if h.revision < GPT_HDR_REVISION
            || h.lba_self != self.lba[elt]
            || h.lba_alt == h.lba_self
            || (h.lba_alt > last && enforce)
            || h.lba_start < 2
            || h.lba_start >= last
            || h.lba_end < h.lba_start
            || h.lba_end >= last
            || h.entries == 0
            || h.entsz < 128
            || !h.entsz.is_multiple_of(8)
            || h.lba_table < 2
            || h.lba_table >= last
            || (h.lba_table >= h.lba_start && h.lba_table <= h.lba_end)
        {
            return None;
        }
        let lba = h.lba_table + (u64::from(h.entries) * u64::from(h.entsz)).div_ceil(m.ss()) - 1;
        if lba >= last || (lba >= h.lba_start && lba <= h.lba_end) {
            return None;
        }
        self.state[elt] = State::Ok;
        if elt == PRIHDR {
            self.lba[SECHDR] = h.lba_alt;
        }
        Some(h)
    }

    /// `gpt_read_tbl`.
    fn read_tbl(
        &mut self,
        m: &Media,
        io: &mut dyn Io,
        elt: usize,
        h: Option<&Hdr>,
    ) -> Option<Vec<Ent>> {
        let h = h?;
        if h.entries > GPS_MAXENT || h.entsz > MAXENTSIZE {
            self.state[elt] = State::Unsupported;
            return None;
        }
        self.lba[elt] = h.lba_table;
        self.state[elt] = State::Missing;
        let tblsz = usize::try_from(h.entries * h.entsz).ok()?;
        let sectors = tblsz.div_ceil(m.ss_usize());
        let buf = io
            .read(h.lba_table.checked_mul(m.ss())?, sectors * m.ss_usize())
            .ok()?;
        self.state[elt] = State::Corrupt;
        if crc32(&buf[..tblsz]) != h.crc_table {
            return None;
        }
        self.state[elt] = State::Ok;
        Some(
            buf[..tblsz]
                .chunks_exact(us(h.entsz))
                .map(|p| {
                    let mut name = [0u16; NAME_UNITS];
                    for (j, u) in name.iter_mut().enumerate() {
                        *u = le16(p, 56 + 2 * j);
                    }
                    Ent {
                        type_guid: guid_at(p, 0),
                        uuid: guid_at(p, 16),
                        lba_start: le64(p, 32),
                        lba_end: le64(p, 40),
                        attr: le64(p, 48),
                        name,
                    }
                })
                .collect(),
        )
    }

    /// `g_part_gpt_read`: the protective MBR, both headers and tables,
    /// the better kept (the primary unless its table is bad), and the
    /// entries in use.
    fn read(&mut self, m: &Media, name: &str, io: &mut dyn Io, enforce: bool) -> Result<(), Errno> {
        let last = m.blocks() - 1;
        let buf = io.read(0, m.ss_usize())?;
        self.mbr.copy_from_slice(&buf[..MBRSIZE]);

        let mut pri = self.read_hdr(m, io, PRIHDR, enforce);
        let pritbl = if self.state[PRIHDR] == State::Ok {
            self.read_tbl(m, io, PRITBL, pri.as_ref())
        } else {
            self.state[PRITBL] = State::Missing;
            None
        };
        let mut sec = self.read_hdr(m, io, SECHDR, enforce);
        let sectbl = if self.state[SECHDR] == State::Ok {
            self.read_tbl(m, io, SECTBL, sec.as_ref())
        } else {
            self.state[SECTBL] = State::Missing;
            None
        };

        if self.state[PRITBL] != State::Ok && self.state[SECTBL] != State::Ok {
            if self.state[PRITBL] == State::Unsupported
                && self.state[SECTBL] == State::Unsupported
                && matched(pri.as_ref(), sec.as_ref())
                && let Some(p) = &pri
            {
                io.log(&format!("GEOM: {name}: unsupported GPT detected."));
                io.log(&format!(
                    "GEOM: {name}: number of GPT entries: {}, entry size: {}B.",
                    p.entries, p.entsz
                ));
                io.log(&format!(
                    "GEOM: {name}: maximum supported number of GPT entries: {GPS_MAXENT}, entry size: {MAXENTSIZE}B."
                ));
                io.log(&format!("GEOM: {name}: GPT rejected."));
            } else {
                io.log(&format!("GEOM: {name}: corrupt or invalid GPT detected."));
                io.log(&format!(
                    "GEOM: {name}: GPT rejected -- may not be recoverable."
                ));
            }
            return Err(Errno::EINVAL);
        }

        if self.state[PRIHDR] == State::Ok
            && self.state[SECHDR] == State::Ok
            && !matched(pri.as_ref(), sec.as_ref())
        {
            if self.state[PRITBL] == State::Ok {
                self.state[SECHDR] = State::Invalid;
                self.state[SECTBL] = State::Missing;
                sec = None;
            } else {
                self.state[PRIHDR] = State::Invalid;
                self.state[PRITBL] = State::Missing;
                pri = None;
            }
        }

        let tbl = if self.state[PRITBL] == State::Ok {
            if self.state[SECTBL] != State::Ok {
                io.log(&format!(
                    "GEOM: {name}: the secondary GPT table is corrupt or invalid."
                ));
                io.log(&format!(
                    "GEOM: {name}: using the primary only -- recovery suggested."
                ));
                self.corrupt = true;
            } else if self.lba[SECHDR] != last {
                io.log(&format!(
                    "GEOM: {name}: the secondary GPT header is not in the last LBA."
                ));
                self.corrupt = true;
            }
            self.hdr = pri;
            pritbl
        } else {
            io.log(&format!(
                "GEOM: {name}: the primary GPT table is corrupt or invalid."
            ));
            io.log(&format!(
                "GEOM: {name}: using the secondary instead -- recovery strongly advised."
            ));
            self.corrupt = true;
            self.hdr = sec;
            sectbl
        };
        let (Some(h), Some(tbl)) = (self.hdr, tbl) else {
            return Err(Errno::EINVAL);
        };
        let min = GPT_MIN_RESERVED / m.ss() + 2;
        if h.lba_start < min {
            io.log(&format!(
                "GEOM: warning: {name} lba_start {} < required min {min}",
                h.lba_start
            ));
        }
        self.first = h.lba_start;
        self.last = h.lba_end;
        self.entries = h.entries;
        for (i, ent) in tbl.iter().enumerate().rev() {
            if ent.type_guid == Guid::NIL {
                continue;
            }
            // At most `GPS_MAXENT` entries.
            let index = u32::try_from(i + 1).unwrap_or(u32::MAX);
            self.new_entry(index, ent.lba_start, ent.lba_end).ent = *ent;
        }
        self.bootcamp = self.is_bootcamp(name, io);
        Ok(())
    }

    /// `gpt_is_bootcamp`: the protective MBR covers the table and the ESP
    /// only, and an HFS+ entry follows (Boot Camp).
    fn is_bootcamp(&self, name: &str, io: &mut dyn Io) -> bool {
        let p = &self.mbr[DOSPARTOFF..];
        if p[4] != 0xee || le32(p, 8) != 1 || p[DOSPARTSIZE + 4] != 0xaf {
            return false;
        }
        io.log(&format!("GEOM: {name}: enabling Boot Camp"));
        true
    }
}

/// `gpt_matched_hdrs`.
fn matched(a: Option<&Hdr>, b: Option<&Hdr>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    a.uuid == b.uuid
        && a.revision == b.revision
        && a.size == b.size
        && a.lba_start == b.lba_start
        && a.lba_end == b.lba_end
        && a.entries == b.entries
        && a.entsz == b.entsz
        && a.crc_table == b.crc_table
}

/// `g_part_taste` with the GPT scheme: the table on a provider of media
/// `m` named `name`, if one is there and passes the integrity check
/// (`enforce`: `kern.geom.part.check_integrity`).
#[must_use]
pub fn taste(m: &Media, name: &str, io: &mut dyn Io, enforce: bool) -> Option<Table> {
    if m.mediasize == 0 || m.sectorsize == 0 {
        return None;
    }
    probe_gpt(m, io).ok()?;
    let mut t = Table::new(Scheme::Gpt, 0);
    t.geometry(m);
    t.read(m, name, io, enforce).ok()?;
    t.check_integrity(m, name, io, enforce).ok()?;
    Some(t)
}

// --- Writing -------------------------------------------------------------

/// `gpt_write_mbr_entry`.
fn write_mbr_entry(
    mbr: &mut [u8; MBRSIZE],
    idx: usize,
    typ: u8,
    start: u64,
    end: u64,
) -> Result<(), Errno> {
    if typ == 0 || start > u64::from(u32::MAX) || end > u64::from(u32::MAX) {
        return Err(Errno::EINVAL);
    }
    let e = &mut mbr[DOSPARTOFF + idx * DOSPARTSIZE..][..DOSPARTSIZE];
    e[0] = 0;
    if start == 1 {
        e[1] = 0;
        e[2] = 2;
        e[3] = 0;
    } else {
        e[1..4].fill(0xff);
    }
    e[4] = typ;
    e[5..8].fill(0xff);
    // Both fit: checked above.
    let (s, n) = (
        u32::try_from(start).unwrap_or(u32::MAX),
        u32::try_from(end - start + 1).unwrap_or(u32::MAX),
    );
    e[8..12].copy_from_slice(&s.to_le_bytes());
    e[12..16].copy_from_slice(&n.to_le_bytes());
    Ok(())
}

/// `gpt_map_type`: the MBR type Boot Camp mirrors a GPT type as.
fn mbr_type(t: Guid) -> u8 {
    const MAP: &[(&str, u8)] = &[
        ("426F6F74-0000-11AA-AA11-00306543ECAC", 0xab),
        ("48465300-0000-11AA-AA11-00306543ECAC", 0xaf),
        ("9D087404-1CA5-11DC-8817-01301BB8A9F5", 0xa5),
        ("3D48CE54-1D16-11DC-8696-01301BB8A9F5", 0xa5),
        ("C12A7328-F81F-11D2-BA4B-00A0C93EC93B", 0xee),
        ("516E7CB4-6ECF-11D6-8FF8-00022D09712B", 0xa5),
        ("0FC63DAF-8483-4772-8E79-3D69D8477DE4", 0x0b),
        ("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", 0x0b),
        ("9E1A2D38-C612-4316-AA26-8B49521E5A8B", 0x41),
    ];
    MAP.iter()
        .find(|(g, _)| Guid::parse(g).is_ok_and(|g| g == t))
        .map_or(0, |&(_, m)| m)
}

impl Table {
    /// `gpt_create_pmbr`: one protective entry over the whole disk.
    fn create_pmbr(&mut self, m: &Media) {
        self.mbr[DOSPARTOFF..DOSPARTOFF + DOSPARTSIZE * NDOSPART].fill(0);
        let end = (m.blocks() - 1).min(u64::from(u32::MAX));
        let _ = write_mbr_entry(&mut self.mbr, 0, 0xee, 1, end);
        self.mbr[DOSMAGICOFFSET..DOSMAGICOFFSET + 2].copy_from_slice(&DOSMAGIC.to_le_bytes());
    }

    /// `gpt_update_bootcamp`: the MBR mirrors the first four entries.
    fn update_bootcamp(&mut self, m: &Media) {
        let bootable = (0..NDOSPART)
            .rev()
            .find(|&i| self.mbr[DOSPARTOFF + DOSPARTSIZE * i] != 0);
        self.mbr[DOSPARTOFF..DOSPARTOFF + DOSPARTSIZE * NDOSPART].fill(0);
        let mut slices = 0u32;
        let entries: Vec<(usize, Ent)> = self
            .live()
            .filter_map(|e| {
                let i = usize::try_from(e.index - 1).ok()?;
                (i < NDOSPART).then_some((i, e.ent))
            })
            .collect();
        for (index, ent) in entries {
            let r = match index {
                0 => {
                    if ent.type_guid != TYPE_EFI {
                        break;
                    }
                    write_mbr_entry(&mut self.mbr, 0, 0xee, 1, ent.lba_end)
                }
                1 => {
                    if ent.type_guid != TYPE_APPLE_HFS {
                        break;
                    }
                    write_mbr_entry(&mut self.mbr, 1, 0xaf, ent.lba_start, ent.lba_end)
                }
                _ => write_mbr_entry(
                    &mut self.mbr,
                    index,
                    mbr_type(ent.type_guid),
                    ent.lba_start,
                    ent.lba_end,
                ),
            };
            if r.is_err() {
                continue;
            }
            if Some(index) == bootable {
                self.mbr[DOSPARTOFF + DOSPARTSIZE * index] = 0x80;
            }
            slices |= 1 << index;
        }
        if slices & 3 == 3 {
            return;
        }
        self.bootcamp = false;
        self.create_pmbr(m);
    }

    /// `g_part_gpt_write`: the protective MBR, then each table and its
    /// header, primary first.
    fn write(&mut self, m: &Media, io: &mut dyn Io) -> Result<(), Errno> {
        let h = self.hdr.ok_or(Errno::EINVAL)?;
        let ss = m.ss_usize();
        let tbl = usize::try_from(h.entries * h.entsz).map_err(|_| Errno::EINVAL)?;
        let tblsz = tbl.div_ceil(ss);
        if self.bootcamp {
            self.update_bootcamp(m);
        }
        let mut pmbr = vec![0u8; ss];
        pmbr[..MBRSIZE].copy_from_slice(&self.mbr);
        io.write(0, &pmbr)?;

        let mut buf = vec![0u8; (tblsz + 1) * ss];
        buf[..8].copy_from_slice(GPT_HDR_SIG);
        buf[8..12].copy_from_slice(&h.revision.to_le_bytes());
        buf[12..16].copy_from_slice(&h.size.to_le_bytes());
        buf[40..48].copy_from_slice(&h.lba_start.to_le_bytes());
        buf[48..56].copy_from_slice(&h.lba_end.to_le_bytes());
        buf[56..72].copy_from_slice(&h.uuid.to_uefi_bytes());
        buf[80..84].copy_from_slice(&h.entries.to_le_bytes());
        buf[84..88].copy_from_slice(&h.entsz.to_le_bytes());
        for e in self.live() {
            let at = ss + us(h.entsz) * (us(e.index) - 1);
            let Some(bp) = buf.get_mut(at..at + 128) else {
                continue;
            };
            bp[0..16].copy_from_slice(&e.ent.type_guid.to_uefi_bytes());
            bp[16..32].copy_from_slice(&e.ent.uuid.to_uefi_bytes());
            bp[32..40].copy_from_slice(&e.ent.lba_start.to_le_bytes());
            bp[40..48].copy_from_slice(&e.ent.lba_end.to_le_bytes());
            bp[48..56].copy_from_slice(&e.ent.attr.to_le_bytes());
            for (j, u) in e.ent.name.iter().enumerate() {
                bp[56 + 2 * j..58 + 2 * j].copy_from_slice(&u.to_le_bytes());
            }
        }
        let crc = crc32(&buf[ss..ss + tbl]);
        buf[88..92].copy_from_slice(&crc.to_le_bytes());
        let hsize = us(h.size);
        for (me, alt, table) in [
            (self.lba[PRIHDR], self.lba[SECHDR], self.lba[PRITBL]),
            (self.lba[SECHDR], self.lba[PRIHDR], self.lba[SECTBL]),
        ] {
            buf[16..20].fill(0);
            buf[24..32].copy_from_slice(&me.to_le_bytes());
            buf[32..40].copy_from_slice(&alt.to_le_bytes());
            buf[72..80].copy_from_slice(&table.to_le_bytes());
            let crc = crc32(&buf[..hsize]);
            buf[16..20].copy_from_slice(&crc.to_le_bytes());
            io.write(table * m.ss(), &buf[ss..])?;
            io.write(me * m.ss(), &buf[..ss])?;
        }
        Ok(())
    }

    /// `g_gpt_set_defaults`: where the elements go on a provider of `m`,
    /// and the first and last usable sectors, aligned to 4 KiB unless the
    /// table is being restored (or the entries leave no room).
    fn set_defaults(&mut self, m: &Media, restore: bool) {
        let last = m.blocks() - 1;
        let tblsz = (u64::from(self.entries) * u64::from(ENT_SIZE)).div_ceil(m.ss());
        self.lba = [1, 2, last, last - tblsz];
        self.state = [State::Ok; 4];
        let (mut start, mut end) = (2 + tblsz, last - tblsz - 1);
        let (mut max, mut min) = (start, end);
        for e in self.live() {
            min = min.min(e.ent.lba_start);
            max = max.max(e.ent.lba_end);
        }
        if !restore {
            let spb = 4096 / m.ss();
            if spb > 1 {
                let lba = start
                    + if start.is_multiple_of(spb) {
                        0
                    } else {
                        spb - start % spb
                    };
                if lba <= min {
                    start = lba;
                }
                let lba = end - (end + 1) % spb;
                if max <= lba {
                    end = lba;
                }
            }
        }
        if let Some(h) = &mut self.hdr {
            h.lba_start = start;
            h.lba_end = end;
        }
        self.first = start;
        self.last = end;
    }
}

// --- Names and types -------------------------------------------------------

/// `g_gpt_utf8_to_utf16`: a label in UTF-8 as a GPT name, truncated to
/// fit, a malformed sequence as U+FFFD.
#[must_use]
pub fn utf8_to_utf16(s8: &[u8]) -> [u16; NAME_UNITS] {
    let mut s16 = [0u16; NAME_UNITS];
    let len = NAME_UNITS;
    let (mut i, mut utfchar, mut utfbytes) = (0usize, 0u32, 0u32);
    for &c in crate::ctl::c_str(s8) {
        if i >= len {
            break;
        }
        let c = u32::from(c);
        if c & 0xc0 != 0x80 {
            if utfbytes != 0 {
                s16[i] = 0xfffd;
                i += 1;
            }
            (utfchar, utfbytes) = if c & 0xf8 == 0xf0 {
                (c & 0x07, 3)
            } else if c & 0xf0 == 0xe0 {
                (c & 0x0f, 2)
            } else if c & 0xe0 == 0xc0 {
                (c & 0x1f, 1)
            } else {
                (c & 0x7f, 0)
            };
        } else if utfbytes > 0 {
            utfchar = (utfchar << 6) + (c & 0x3f);
            utfbytes -= 1;
        } else {
            utfbytes = u32::MAX;
        }
        if utfbytes == 0 && i < len {
            if utfchar >= 0x10000 && i < len - 1 {
                s16[i] = u16::try_from(0xd800 | ((utfchar >> 10) - 0x40)).unwrap_or(0xfffd);
                s16[i + 1] = u16::try_from(0xdc00 | (utfchar & 0x3ff)).unwrap_or(0xfffd);
                i += 2;
            } else {
                s16[i] = u16::try_from(utfchar).unwrap_or(0);
                i += 1;
            }
        }
    }
    if utfbytes != 0 && i < len {
        s16[i] = 0xfffd;
    }
    s16
}

/// `gpt_parse_type`: a type by `gpart`'s name for it (any case), or `!`
/// and its GUID.
fn parse_type(t: &[u8]) -> Result<Guid, Errno> {
    if let Some(g) = t.strip_prefix(b"!") {
        let g = parse_uuid(g)?;
        if g == Guid::NIL {
            return Err(Errno::EINVAL);
        }
        return Ok(g);
    }
    crate::part::gpt_type_guid(t)
        .and_then(|g| Guid::parse(g).ok())
        .ok_or(Errno::EINVAL)
}

/// `parse_uuid`: a UUID's text form; empty is the nil UUID, and the
/// variant must be one FreeBSD knows.
fn parse_uuid(s: &[u8]) -> Result<Guid, Errno> {
    if s.is_empty() {
        return Ok(Guid::NIL);
    }
    let s = core::str::from_utf8(s).map_err(|_| Errno::EINVAL)?;
    let g = Guid::parse(s).map_err(|_| Errno::EINVAL)?;
    let c3 =
        u8::from_str_radix(s.get(19..21).ok_or(Errno::EINVAL)?, 16).map_err(|_| Errno::EINVAL)?;
    if c3 & 0x80 != 0 && c3 & 0xc0 != 0x80 && c3 & 0xe0 != 0xc0 {
        return Err(Errno::EINVAL);
    }
    Ok(g)
}

// --- Control requests ------------------------------------------------------

/// The geoms, providers, and I/O a control request works on: GEOM's
/// topology, as `ctlreq` sees it. Geoms of the class are named as the
/// providers they are attached to.
pub trait Topology {
    /// The table of the (not withering) geom named `geom`.
    fn table(&self, geom: &str) -> Option<Table>;
    /// Stores the table of geom `geom`, making the geom (attached to the
    /// provider of that name) if there is none; `None` withers it.
    fn store(&mut self, geom: &str, table: Option<Table>);
    /// The provider named `name`: its media, and (for a partition) the
    /// depth of the table it is in (`PART::depth`).
    fn provider(&self, name: &str) -> Option<(Media, Option<u32>)>;
    /// `g_access(cp, n, n, n)` on geom `geom`'s consumer, `n` 1 if `open`
    /// else -1. A geom being made is attached first.
    ///
    /// # Errors
    ///
    /// `g_access`'s (`EPERM` for a conflicting open).
    fn access(&mut self, geom: &str, open: bool) -> Result<(), Errno>;
    /// Whether geom `geom`'s partition `index`'s provider is open.
    fn busy(&self, geom: &str, index: u32) -> bool;
    /// `g_media_changed` on geom `geom`'s partition `index`.
    fn media_changed(&mut self, geom: &str, index: u32);
    /// Reads from the provider geom `geom` is attached to.
    ///
    /// # Errors
    ///
    /// The provider's.
    fn read(&mut self, geom: &str, offset: u64, len: usize) -> Result<Vec<u8>, Errno>;
    /// Writes to the provider geom `geom` is attached to.
    ///
    /// # Errors
    ///
    /// The provider's.
    fn write(&mut self, geom: &str, offset: u64, data: &[u8]) -> Result<(), Errno>;
    /// A new UUID (`kern_uuidgen`).
    fn uuidgen(&mut self) -> Guid;
    /// `kern.geom.debugflags`.
    fn debugflags(&self) -> u32;
    /// `kern.geom.part.check_integrity`.
    fn check_integrity(&self) -> bool;
    /// Prints a line on the console.
    fn log(&mut self, line: &str);
}

/// A geom's provider through [`Topology`], as [`Io`].
struct GeomIo<'a> {
    topo: &'a mut dyn Topology,
    geom: &'a str,
}

impl Io for GeomIo<'_> {
    fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
        self.topo.read(self.geom, offset, len)
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), Errno> {
        self.topo.write(self.geom, offset, data)
    }

    fn log(&mut self, line: &str) {
        self.topo.log(line);
    }
}

/// The verbs (`enum g_part_ctl`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctl {
    Add,
    Bootcode,
    Commit,
    Create,
    Delete,
    Destroy,
    Modify,
    Move,
    Recover,
    Resize,
    Set,
    Undo,
    Unset,
}

const P_ENTRIES: u32 = 0x0001;
const P_FLAGS: u32 = 0x0002;
const P_GEOM: u32 = 0x0004;
const P_INDEX: u32 = 0x0008;
const P_LABEL: u32 = 0x0010;
const P_OUTPUT: u32 = 0x0020;
const P_PROVIDER: u32 = 0x0040;
const P_SCHEME: u32 = 0x0080;
const P_SIZE: u32 = 0x0100;
const P_START: u32 = 0x0200;
const P_TYPE: u32 = 0x0400;
const P_VERSION: u32 = 0x0800;
const P_BOOTCODE: u32 = 0x1000;
const P_ATTRIB: u32 = 0x2000;
const P_FORCE: u32 = 0x4000;
const P_SKIP_DSN: u32 = 0x8000;

/// A request's parameters (`struct g_part_parms`).
#[derive(Default)]
struct Parms {
    parms: u32,
    attrib: Vec<u8>,
    bootcode: Vec<u8>,
    entries: u32,
    flags: Vec<u8>,
    force: u32,
    geom: String,
    index: u32,
    label: Vec<u8>,
    provider: String,
    size: u64,
    start: u64,
    type_: Vec<u8>,
}

impl Parms {
    fn has(&self, p: u32) -> bool {
        self.parms & p != 0
    }

    fn restore(&self) -> bool {
        self.has(P_FLAGS) && self.flags.windows(7).any(|w| w == b"restore")
    }
}

/// `strtoq(p, &x, 0)` with nothing after the number: `None` if anything
/// follows (or nothing is there), the value clamped as `strtoq` clamps.
fn strtoq(p: &[u8]) -> Option<i64> {
    let mut s = p;
    while let [c, rest @ ..] = s
        && matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    {
        s = rest;
    }
    let neg = match s {
        [b'-', rest @ ..] => {
            s = rest;
            true
        }
        [b'+', rest @ ..] => {
            s = rest;
            false
        }
        _ => false,
    };
    let base = match s {
        [b'0', b'x' | b'X', c, ..] if c.is_ascii_hexdigit() => {
            s = &s[2..];
            16
        }
        [b'0', ..] => 8,
        _ => 10,
    };
    let digits = s
        .iter()
        .take_while(|c| char::from(**c).is_digit(base))
        .count();
    if digits == 0 || digits != s.len() {
        return None;
    }
    let mut v: i128 = 0;
    for &c in s {
        v = v * i128::from(base) + i128::from(char::from(c).to_digit(base).unwrap_or(0));
        v = v.min(i128::from(i64::MAX) + 1);
    }
    let v = if neg { -v } else { v };
    Some(i64::try_from(v).unwrap_or(if neg { i64::MIN } else { i64::MAX }))
}

/// Takes `/dev/` off a name.
fn undev(name: &[u8]) -> String {
    String::from_utf8_lossy(name.strip_prefix(b"/dev/").unwrap_or(name)).into_owned()
}

/// Parses argument `name` as parameter `parm`. `Err(())` once an error
/// is recorded (or, for none at all, `ENOATTR`, which the caller records).
fn parse_parm(
    req: &mut Request,
    topo: &dyn Topology,
    gpp: &mut Parms,
    name: &str,
    parm: u32,
) -> Result<(), Errno> {
    let bad = |req: &mut Request, v: &dyn core::fmt::Display| {
        req.error(format_args!("{} {name} '{v}'", Errno::EINVAL.get()));
        Errno::EINVAL
    };
    let ascii = |req: &mut Request| req.ascii(name).ok_or(Errno::ENOATTR);
    let intmax = |req: &mut Request| -> Result<u32, Errno> {
        let p = req.param(name).ok_or(Errno::ENOATTR)?;
        let v = <[u8; 8]>::try_from(p).map(i64::from_ne_bytes);
        match v {
            Ok(v) if (0..=i64::from(i32::MAX)).contains(&v) => Ok(u32::try_from(v).unwrap_or(0)),
            Ok(v) => Err(bad(req, &v)),
            Err(_) => {
                let v = p.get(..8).map_or(0, |b| {
                    i64::from_ne_bytes(<[u8; 8]>::try_from(b).unwrap_or([0; 8]))
                });
                Err(bad(req, &v))
            }
        }
    };
    let uint32 = |req: &mut Request| -> Result<u32, Errno> {
        let p = req.param(name).ok_or(Errno::ENOATTR)?;
        let v = <[u8; 4]>::try_from(p).map(u32::from_ne_bytes);
        match v {
            Ok(v) if v <= 0x7fff_ffff => Ok(v),
            Ok(v) => Err(bad(req, &v)),
            Err(_) => {
                let v = p.get(..4).map_or(0, |b| {
                    u32::from_ne_bytes(<[u8; 4]>::try_from(b).unwrap_or([0; 4]))
                });
                Err(bad(req, &v))
            }
        }
    };
    let string = |req: &mut Request| -> Result<Vec<u8>, Errno> {
        let p = ascii(req)?;
        if name != "label" && p.is_empty() {
            return Err(bad(req, &""));
        }
        Ok(p)
    };
    let quad = |req: &mut Request| -> Result<u64, Errno> {
        let p = ascii(req)?;
        match strtoq(&p).and_then(|q| u64::try_from(q).ok()) {
            Some(q) => Ok(q),
            None => Err(bad(req, &Show(&p))),
        }
    };
    match parm {
        P_ATTRIB => gpp.attrib = string(req)?,
        P_BOOTCODE => gpp.bootcode = req.param(name).ok_or(Errno::ENOATTR)?.to_vec(),
        P_ENTRIES => gpp.entries = intmax(req)?,
        P_FLAGS => gpp.flags = string(req)?,
        P_FORCE => gpp.force = uint32(req)?,
        P_GEOM => {
            let g = undev(&ascii(req)?);
            if topo.table(&g).is_none() {
                return Err(bad(req, &g));
            }
            gpp.geom = g;
        }
        P_INDEX => gpp.index = intmax(req)?,
        P_LABEL => gpp.label = string(req)?,
        P_OUTPUT => {}
        P_PROVIDER => {
            let p = undev(&ascii(req)?);
            if topo.provider(&p).is_none() {
                return Err(bad(req, &p));
            }
            gpp.provider = p;
        }
        P_SCHEME => {
            let s = ascii(req)?;
            if !s.eq_ignore_ascii_case(b"GPT") {
                return Err(bad(req, &Show(&s)));
            }
        }
        P_SIZE => gpp.size = quad(req)?,
        P_SKIP_DSN | P_VERSION => {
            uint32(req)?;
        }
        P_START => gpp.start = quad(req)?,
        P_TYPE => gpp.type_ = string(req)?,
        _ => {
            req.error(format_args!("{} {name}", Errno::EDOOFUS.get()));
            return Err(Errno::EDOOFUS);
        }
    }
    Ok(())
}

/// `g_part_ctlreq`: runs `verb` on the PART class.
pub fn ctlreq(req: &mut Request, verb: &[u8], topo: &mut dyn Topology) {
    let (ctl, mparms, extra, modifies) = match verb {
        b"add" => (
            Ctl::Add,
            P_GEOM | P_SIZE | P_START | P_TYPE,
            P_INDEX | P_LABEL,
            true,
        ),
        b"bootcode" => (Ctl::Bootcode, P_GEOM | P_BOOTCODE, P_SKIP_DSN, true),
        b"commit" => (Ctl::Commit, P_GEOM, 0, false),
        b"create" => (Ctl::Create, P_PROVIDER | P_SCHEME, P_ENTRIES, true),
        b"delete" => (Ctl::Delete, P_GEOM | P_INDEX, 0, true),
        b"destroy" => (Ctl::Destroy, P_GEOM, P_FORCE, true),
        b"modify" => (Ctl::Modify, P_GEOM | P_INDEX, P_LABEL | P_TYPE, true),
        b"move" => (Ctl::Move, P_GEOM | P_INDEX, 0, true),
        b"recover" => (Ctl::Recover, P_GEOM, 0, true),
        b"resize" => (Ctl::Resize, P_GEOM | P_INDEX | P_SIZE, 0, true),
        b"set" => (Ctl::Set, P_ATTRIB | P_GEOM, P_INDEX, true),
        b"undo" => (Ctl::Undo, P_GEOM, 0, false),
        b"unset" => (Ctl::Unset, P_ATTRIB | P_GEOM, P_INDEX, true),
        _ => {
            req.error(format_args!(
                "{} verb '{}'",
                Errno::EINVAL.get(),
                Show(verb)
            ));
            return;
        }
    };
    let oparms = P_FLAGS | P_OUTPUT | P_VERSION | extra;

    let mut gpp = Parms::default();
    let names: Vec<Vec<u8>> = req.args.iter().map(|a| a.name.clone()).collect();
    for n in &names {
        let parm = match n.as_slice() {
            b"arg0" => mparms & (P_GEOM | P_PROVIDER),
            b"attrib" => P_ATTRIB,
            b"bootcode" => P_BOOTCODE,
            b"class" | b"verb" => continue,
            b"entries" => P_ENTRIES,
            b"flags" => P_FLAGS,
            b"force" => P_FORCE,
            b"index" => P_INDEX,
            b"label" => P_LABEL,
            b"output" => P_OUTPUT,
            b"scheme" => P_SCHEME,
            b"size" => P_SIZE,
            b"start" => P_START,
            b"skip_dsn" => P_SKIP_DSN,
            b"type" => P_TYPE,
            b"version" => P_VERSION,
            _ => 0,
        };
        let name = String::from_utf8_lossy(n).into_owned();
        if parm & (mparms | oparms) == 0 {
            req.error(format_args!("{} param '{name}'", Errno::EINVAL.get()));
            return;
        }
        match parse_parm(req, topo, &mut gpp, &name, parm) {
            Ok(()) => gpp.parms |= parm,
            Err(Errno::ENOATTR) => {
                req.error(format_args!("{} param '{name}'", Errno::ENOATTR.get()));
                return;
            }
            Err(_) => return,
        }
    }
    if gpp.parms & mparms != mparms {
        let missing = mparms - (gpp.parms & mparms);
        req.error(format_args!("{} param '{missing:x}'", Errno::ENOATTR.get()));
        return;
    }

    // Obtain permissions if possible/necessary.
    let mut close_on_error = false;
    if modifies
        && gpp.has(P_GEOM)
        && let Some(mut t) = topo.table(&gpp.geom)
    {
        if t.corrupt && ctl != Ctl::Destroy && ctl != Ctl::Recover && topo.check_integrity() {
            req.error(format_args!(
                "{} table '{}' is corrupt",
                Errno::EPERM.get(),
                gpp.geom
            ));
            return;
        }
        if !t.opened {
            if let Err(e) = topo.access(&gpp.geom, true) {
                req.error(format_args!("{} geom '{}'", e.get(), gpp.geom));
                return;
            }
            t.opened = true;
            topo.store(&gpp.geom, Some(t));
            close_on_error = true;
        }
    }

    let mut r = match ctl {
        Ctl::Add => ctl_add(req, &gpp, topo),
        Ctl::Bootcode => ctl_bootcode(req, &gpp, topo),
        Ctl::Commit => ctl_commit(req, &gpp, topo),
        Ctl::Create => ctl_create(req, &mut gpp, topo),
        Ctl::Delete => ctl_delete(req, &gpp, topo),
        Ctl::Destroy => ctl_destroy(req, &gpp, topo),
        Ctl::Modify => ctl_modify(req, &gpp, topo),
        Ctl::Move => {
            req.error(format_args!("{} verb 'move'", Errno::ENOSYS.get()));
            Err(Errno::ENOSYS)
        }
        Ctl::Recover => ctl_recover(req, &gpp, topo),
        Ctl::Resize => ctl_resize(req, &gpp, topo),
        Ctl::Set => ctl_setunset(req, &gpp, topo, true),
        Ctl::Undo => ctl_undo(req, &gpp, topo),
        Ctl::Unset => ctl_setunset(req, &gpp, topo, false),
    };

    // Automatic commit.
    if r.is_ok() && modifies && gpp.has(P_FLAGS) && gpp.flags.contains(&b'C') {
        r = ctl_commit(req, &gpp, topo);
    }

    if r.is_err() && close_on_error {
        let _ = topo.access(&gpp.geom, false);
        if let Some(mut t) = topo.table(&gpp.geom) {
            t.opened = false;
            topo.store(&gpp.geom, Some(t));
        }
    }
}

/// The table of the request's geom, which parsing found.
fn table_of(gpp: &Parms, topo: &dyn Topology) -> Result<Table, Errno> {
    topo.table(&gpp.geom).ok_or(Errno::ENXIO)
}

/// Fails the request with `"%d"` of `e`.
fn fail(req: &mut Request, e: Errno) -> Result<(), Errno> {
    req.error(format_args!("{}", e.get()));
    Err(e)
}

fn ctl_add(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    let m = topo.provider(&gpp.geom).map(|p| p.0).unwrap_or_default();
    let end = (gpp.start + gpp.size).wrapping_sub(1);
    if gpp.start < t.first || gpp.start > t.last {
        req.error(format_args!(
            "{} start '{}'",
            Errno::EINVAL.get(),
            gpp.start
        ));
        return Err(Errno::EINVAL);
    }
    if end < gpp.start || end > t.last {
        req.error(format_args!("{} size '{}'", Errno::EINVAL.get(), gpp.size));
        return Err(Errno::EINVAL);
    }
    if gpp.index > t.entries {
        req.error(format_args!(
            "{} index '{}'",
            Errno::EINVAL.get(),
            gpp.index
        ));
        return Err(Errno::EINVAL);
    }
    let mut index = gpp.index.max(1);
    let mut delent = None;
    for (i, e) in t.list.iter().enumerate() {
        if e.deleted {
            if e.index == index {
                delent = Some(i);
            }
            continue;
        }
        if e.index == index {
            index = e.index + 1;
        }
        if gpp.start >= e.start && gpp.start <= e.end {
            req.error(format_args!(
                "{} start '{}'",
                Errno::ENOSPC.get(),
                gpp.start
            ));
            return Err(Errno::ENOSPC);
        }
        if end >= e.start && end <= e.end {
            req.error(format_args!("{} end '{end}'", Errno::ENOSPC.get()));
            return Err(Errno::ENOSPC);
        }
        if gpp.start < e.start && end > e.end {
            req.error(format_args!("{} size '{}'", Errno::ENOSPC.get(), gpp.size));
            return Err(Errno::ENOSPC);
        }
    }
    if gpp.index > 0 && index != gpp.index {
        req.error(format_args!(
            "{} index '{}'",
            Errno::EEXIST.get(),
            gpp.index
        ));
        return Err(Errno::EEXIST);
    }
    if index > t.entries {
        req.error(format_args!("{} index '{index}'", Errno::ENOSPC.get()));
        return Err(Errno::ENOSPC);
    }
    // A deleted entry with that index is taken again.
    let delent = delent.filter(|&i| t.list[i].index == index);
    let mut entry = delent.map_or_else(
        || Entry {
            index,
            start: gpp.start,
            end,
            created: false,
            modified: false,
            deleted: false,
            ent: Ent::ZERO,
        },
        |i| t.list[i].clone(),
    );
    entry.index = index;
    entry.start = gpp.start;
    entry.end = end;
    // G_PART_ADD.
    if t.scheme != Scheme::Gpt {
        return fail(req, Errno::ENXIO);
    }
    let type_guid = match parse_type(&gpp.type_) {
        Ok(g) => g,
        Err(e) => return fail(req, e),
    };
    entry.ent.type_guid = type_guid;
    entry.ent.uuid = topo.uuidgen();
    entry.ent.lba_start = entry.start;
    entry.ent.lba_end = entry.end;
    if entry.deleted {
        entry.ent.attr = 0;
        entry.ent.name = [0; NAME_UNITS];
    }
    if gpp.has(P_LABEL) {
        entry.ent.name = utf8_to_utf16(&gpp.label);
    }
    if let Some(i) = delent {
        entry.deleted = false;
        entry.modified = true;
        t.list[i] = entry.clone();
    } else {
        entry.created = true;
        let at = t.list.partition_point(|e| e.index < index);
        t.list.insert(at, entry.clone());
    }
    let name = format!("{}{}", gpp.geom, entry.name());
    topo.store(&gpp.geom, Some(t));
    if gpp.has(P_OUTPUT) {
        let offset = entry.start * m.ss();
        let mut stripeoffset = m.stripeoffset + offset;
        if m.stripesize > 0 {
            stripeoffset %= m.stripesize;
        }
        if m.stripesize > 0 && stripeoffset != 0 {
            req.set_output(&format!(
                "{name} added, but partition is not aligned on {} bytes\n",
                m.stripesize
            ));
        } else {
            req.set_output(&format!("{name} added\n"));
        }
    }
    Ok(())
}

fn ctl_bootcode(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    if t.scheme != Scheme::Gpt {
        return fail(req, Errno::ENODEV);
    }
    if gpp.bootcode.len() > BOOTCODESZ {
        return fail(req, Errno::EFBIG);
    }
    t.mbr[..DOSPARTOFF].fill(0);
    let n = DOSPARTOFF.min(gpp.bootcode.len());
    t.mbr[..n].copy_from_slice(&gpp.bootcode[..n]);
    topo.store(&gpp.geom, Some(t));
    if gpp.has(P_OUTPUT) {
        req.set_output(&format!("bootcode written to {}\n", gpp.geom));
    }
    Ok(())
}

fn ctl_commit(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    if !t.opened {
        return fail(req, Errno::EPERM);
    }
    let m = topo.provider(&gpp.geom).map(|p| p.0).unwrap_or_default();
    let ss = m.ss_usize();
    let zero = vec![0u8; ss];
    while t.smhead != 0 {
        let i = t.smhead.trailing_zeros();
        if let Err(e) = topo.write(&gpp.geom, u64::from(i) * m.ss(), &zero) {
            topo.store(&gpp.geom, Some(t));
            return fail(req, e);
        }
        t.smhead &= !(1 << i);
    }
    while t.smtail != 0 {
        let i = t.smtail.trailing_zeros();
        let at = m.mediasize - (u64::from(i) + 1) * m.ss();
        if let Err(e) = topo.write(&gpp.geom, at, &zero) {
            topo.store(&gpp.geom, Some(t));
            return fail(req, e);
        }
        t.smtail &= !(1 << i);
    }
    if t.scheme == Scheme::Null {
        let _ = topo.access(&gpp.geom, false);
        topo.store(&gpp.geom, None);
        return Ok(());
    }
    let r = t.write(
        &m,
        &mut GeomIo {
            topo,
            geom: &gpp.geom,
        },
    );
    if let Err(e) = r {
        topo.store(&gpp.geom, Some(t));
        return fail(req, e);
    }
    let mut changed = Vec::new();
    t.list.retain_mut(|e| {
        if e.deleted {
            return false;
        }
        if e.modified {
            changed.push(e.index);
        }
        e.created = false;
        e.modified = false;
        true
    });
    t.created = false;
    t.opened = false;
    topo.store(&gpp.geom, Some(t));
    for i in changed {
        if !topo.busy(&gpp.geom, i) {
            topo.media_changed(&gpp.geom, i);
        }
    }
    let _ = topo.access(&gpp.geom, false);
    Ok(())
}

fn ctl_create(req: &mut Request, gpp: &mut Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let pp = gpp.provider.clone();
    let null = topo.table(&pp);
    if null.as_ref().is_some_and(|n| n.scheme != Scheme::Null) {
        req.error(format_args!("{} geom '{pp}'", Errno::EEXIST.get()));
        return Err(Errno::EEXIST);
    }
    if gpp.has(P_ENTRIES) && !(GPS_MINENT..=GPS_MAXENT).contains(&gpp.entries) {
        req.error(format_args!(
            "{} entries '{}'",
            Errno::EINVAL.get(),
            gpp.entries
        ));
        return Err(Errno::EINVAL);
    }
    let Some((m, parent_depth)) = topo.provider(&pp) else {
        return fail(req, Errno::ENXIO);
    };
    let mut t = Table::new(Scheme::Gpt, 0);
    t.entries = if gpp.has(P_ENTRIES) {
        gpp.entries
    } else {
        GPS_DEFENT
    };
    if let Some(n) = &null {
        t.opened = n.opened;
        t.smhead = n.smhead;
        t.smtail = n.smtail;
    } else {
        if let Err(e) = topo.access(&pp, true) {
            req.error(format_args!("{} geom '{pp}'", e.get()));
            return Err(e);
        }
        t.opened = true;
    }
    let undo = |topo: &mut dyn Topology, req: &mut Request, e: Errno| {
        if null.is_none() {
            let _ = topo.access(&pp, false);
            topo.store(&pp, None);
        }
        req.error(format_args!("{} provider", e.get()));
        Err(e)
    };
    if m.mediasize == 0 || m.sectorsize == 0 {
        return undo(topo, req, Errno::ENODEV);
    }
    t.depth = parent_depth.map_or(0, |d| d + 1);
    t.geometry(&m);
    // G_PART_CREATE: GPT nests only where allowed (it is not).
    if t.depth != 0 {
        return undo(topo, req, Errno::ENXIO);
    }
    let tblsz = (u64::from(t.entries) * u64::from(ENT_SIZE)).div_ceil(m.ss());
    if us(m.sectorsize) < MBRSIZE || m.mediasize < (3 + 2 * tblsz + u64::from(t.entries)) * m.ss() {
        return undo(topo, req, Errno::ENOSPC);
    }
    t.create_pmbr(&m);
    t.hdr = Some(Hdr {
        revision: GPT_HDR_REVISION,
        size: GPT_HDR_SIZE,
        lba_self: 0,
        lba_alt: 0,
        lba_start: 0,
        lba_end: 0,
        uuid: topo.uuidgen(),
        lba_table: 0,
        entries: t.entries,
        entsz: ENT_SIZE,
        crc_table: 0,
    });
    t.set_defaults(&m, gpp.restore());
    t.created = true;
    topo.store(&pp, Some(t));
    gpp.parms |= P_GEOM;
    gpp.geom.clone_from(&pp);
    if gpp.has(P_OUTPUT) {
        req.set_output(&format!("{pp} created\n"));
    }
    Ok(())
}

/// The position of live entry `index` in `t`, or `ENOENT` recorded.
fn find(req: &mut Request, t: &Table, index: u32) -> Result<usize, Errno> {
    if let Some(i) = t.list.iter().position(|e| !e.deleted && e.index == index) {
        return Ok(i);
    }
    req.error(format_args!("{} index '{index}'", Errno::ENOENT.get()));
    Err(Errno::ENOENT)
}

fn ctl_delete(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    let i = find(req, &t, gpp.index)?;
    if topo.busy(&gpp.geom, gpp.index) {
        return fail(req, Errno::EBUSY);
    }
    let name = format!("{}{}", gpp.geom, t.list[i].name());
    if t.list[i].created {
        t.list.remove(i);
    } else {
        t.list[i].modified = false;
        t.list[i].deleted = true;
    }
    topo.store(&gpp.geom, Some(t));
    if gpp.has(P_OUTPUT) {
        req.set_output(&format!("{name} deleted\n"));
    }
    Ok(())
}

fn ctl_destroy(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    for e in t.live() {
        if gpp.force != 0 && !topo.busy(&gpp.geom, e.index) {
            continue;
        }
        return fail(req, Errno::EBUSY);
    }
    if gpp.force != 0 {
        t.list.clear();
    }
    if t.scheme != Scheme::Gpt {
        return fail(req, Errno::ENXIO);
    }
    let m = topo.provider(&gpp.geom).map(|p| p.0).unwrap_or_default();
    // G_PART_DESTROY: wipe what holds valid metadata.
    let mut null = Table::new(Scheme::Null, t.depth);
    null.smhead = t.smhead;
    null.smtail = t.smtail;
    if t.state[PRIHDR] == State::Ok {
        null.smhead |= 3;
    }
    if t.state[SECHDR] == State::Ok && t.lba[SECHDR] == m.blocks() - 1 {
        null.smtail |= 1;
    }
    null.last = m.blocks() - 1;
    null.opened = t.opened;
    topo.store(&gpp.geom, Some(null));
    if gpp.has(P_OUTPUT) {
        req.set_output(&format!("{} destroyed\n", gpp.geom));
    }
    Ok(())
}

fn ctl_modify(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    let i = find(req, &t, gpp.index)?;
    if t.scheme != Scheme::Gpt {
        return fail(req, Errno::ENXIO);
    }
    let e = &mut t.list[i];
    if gpp.has(P_TYPE) {
        match parse_type(&gpp.type_) {
            Ok(g) => e.ent.type_guid = g,
            Err(err) => return fail(req, err),
        }
    }
    if gpp.has(P_LABEL) {
        e.ent.name = utf8_to_utf16(&gpp.label);
    }
    if !e.created {
        e.modified = true;
    }
    let name = format!("{}{}", gpp.geom, e.name());
    topo.store(&gpp.geom, Some(t));
    if gpp.has(P_OUTPUT) {
        req.set_output(&format!("{name} modified\n"));
    }
    Ok(())
}

fn ctl_recover(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    let mut recovered = false;
    if t.corrupt {
        let m = topo.provider(&gpp.geom).map(|p| p.0).unwrap_or_default();
        let r = if t.scheme == Scheme::Gpt {
            t.create_pmbr(&m);
            t.set_defaults(&m, false);
            t.corrupt = false;
            let enforce = topo.check_integrity();
            let geom = gpp.geom.clone();
            t.check_integrity(&m, &geom, &mut GeomIo { topo, geom: &geom }, enforce)
        } else {
            Err(Errno::ENXIO)
        };
        if let Err(e) = r {
            req.error(format_args!("{} recovering '{}' failed", e.get(), gpp.geom));
            return Err(e);
        }
        recovered = true;
        topo.store(&gpp.geom, Some(t));
    }
    if gpp.has(P_OUTPUT) {
        if recovered {
            req.set_output(&format!("{} recovered\n", gpp.geom));
        } else {
            req.set_output(&format!("{} recovering is not needed\n", gpp.geom));
        }
    }
    Ok(())
}

fn ctl_resize(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    let i = find(req, &t, gpp.index)?;
    let entry = t.list[i].clone();
    let end = (entry.start + gpp.size).wrapping_sub(1);
    if gpp.size < 1 || end > t.last {
        req.error(format_args!("{} size '{}'", Errno::EINVAL.get(), gpp.size));
        return Err(Errno::EINVAL);
    }
    for pe in t.live() {
        if pe.index == entry.index {
            continue;
        }
        if end >= pe.start && end <= pe.end {
            req.error(format_args!("{} end '{end}'", Errno::ENOSPC.get()));
            return Err(Errno::ENOSPC);
        }
        if entry.start < pe.start && end > pe.end {
            req.error(format_args!("{} size '{}'", Errno::ENOSPC.get(), gpp.size));
            return Err(Errno::ENOSPC);
        }
    }
    if topo.debugflags() & G_F_FOOTSHOOTING == 0
        && topo.busy(&gpp.geom, entry.index)
        && entry.end - entry.start + 1 > gpp.size
    {
        // Deny shrinking of an opened partition.
        return fail(req, Errno::EBUSY);
    }
    if t.scheme != Scheme::Gpt {
        return fail(req, Errno::ENXIO);
    }
    let e = &mut t.list[i];
    e.end = e.start + gpp.size - 1;
    e.ent.lba_end = e.end;
    if !e.created {
        e.modified = true;
    }
    let name = format!("{}{}", gpp.geom, e.name());
    topo.store(&gpp.geom, Some(t));
    if gpp.has(P_OUTPUT) {
        req.set_output(&format!("{name} resized\n"));
    }
    Ok(())
}

fn ctl_setunset(
    req: &mut Request,
    gpp: &Parms,
    topo: &mut dyn Topology,
    set: bool,
) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    let i = if gpp.has(P_INDEX) {
        Some(find(req, &t, gpp.index)?)
    } else {
        None
    };
    let m = topo.provider(&gpp.geom).map(|p| p.0).unwrap_or_default();
    let attrib = gpp.attrib.to_ascii_lowercase();
    let r = if t.scheme != Scheme::Gpt {
        Err(Errno::ENXIO)
    } else if attrib == b"active" {
        if t.bootcamp {
            match i {
                None => Err(Errno::ENXIO),
                Some(i) if us(t.list[i].index) > NDOSPART => Err(Errno::EINVAL),
                Some(i) => {
                    let idx = us(t.list[i].index) - 1;
                    for j in 0..NDOSPART {
                        t.mbr[DOSPARTOFF + j * DOSPARTSIZE] =
                            if j == idx && set { 0x80 } else { 0 };
                    }
                    Ok(())
                }
            }
        } else if i.is_some() {
            Err(Errno::ENXIO)
        } else {
            for j in 0..NDOSPART {
                let p = DOSPARTOFF + j * DOSPARTSIZE;
                t.mbr[p] = if t.mbr[p + 4] == 0xee && set { 0x80 } else { 0 };
            }
            Ok(())
        }
    } else if attrib == b"lenovofix" {
        if i.is_some() {
            Err(Errno::ENXIO)
        } else {
            t.mbr[DOSPARTOFF..DOSPARTOFF + DOSPARTSIZE * NDOSPART].fill(0);
            let end = (m.blocks() - 1).min(u64::from(u32::MAX));
            let _ = write_mbr_entry(&mut t.mbr, usize::from(set), 0xee, 1, end);
            Ok(())
        }
    } else if let Some(i) = i {
        let mut attr = match attrib.as_slice() {
            b"bootme" => ATTR_BOOTME,
            b"bootonce" if set => ATTR_BOOTONCE | ATTR_BOOTME,
            b"bootonce" => ATTR_BOOTONCE,
            b"bootfailed" => ATTR_BOOTFAILED,
            _ => 0,
        };
        if attr == 0 {
            Err(Errno::EINVAL)
        } else {
            let e = &mut t.list[i];
            attr = if set {
                e.ent.attr | attr
            } else {
                e.ent.attr & !attr
            };
            if attr != e.ent.attr {
                e.ent.attr = attr;
                if !e.created {
                    e.modified = true;
                }
            }
            Ok(())
        }
    } else {
        Err(Errno::ENODEV)
    };
    if let Err(e) = r {
        req.error(format_args!("{} attrib '{}'", e.get(), Show(&gpp.attrib)));
        return Err(e);
    }
    let what = i.map_or_else(
        || gpp.geom.clone(),
        |i| format!("{}{}", gpp.geom, t.list[i].name()),
    );
    topo.store(&gpp.geom, Some(t));
    if gpp.has(P_OUTPUT) {
        req.set_output(&format!(
            "{} {}set on {what}\n",
            Show(&gpp.attrib),
            if set { "" } else { "un" }
        ));
    }
    Ok(())
}

fn ctl_undo(req: &mut Request, gpp: &Parms, topo: &mut dyn Topology) -> Result<(), Errno> {
    let mut t = table_of(gpp, topo)?;
    if !t.opened {
        return fail(req, Errno::EPERM);
    }
    t.list.retain_mut(|e| {
        e.modified = false;
        !(e.created || e.deleted)
    });
    let m = topo.provider(&gpp.geom).map(|p| p.0).unwrap_or_default();
    let geom = gpp.geom.clone();
    let enforce = topo.check_integrity();
    let reprobe = t.scheme == Scheme::Null || t.created;
    if reprobe {
        if !t.list.is_empty() {
            topo.store(&geom, Some(t));
            return fail(req, Errno::EBUSY);
        }
        let probed = probe_gpt(&m, &mut GeomIo { topo, geom: &geom });
        if probed.is_err() {
            let _ = topo.access(&geom, false);
            topo.store(&geom, None);
            return Ok(());
        }
        let depth = t.depth;
        t = Table::new(Scheme::Gpt, depth);
        t.geometry(&m);
        t.opened = true;
    }
    let mut io = GeomIo { topo, geom: &geom };
    let r = t
        .read(&m, &geom, &mut io, enforce)
        .and_then(|()| t.check_integrity(&m, &geom, &mut io, enforce));
    if let Err(e) = r {
        topo.store(&geom, Some(t));
        return fail(req, e);
    }
    t.opened = false;
    topo.store(&geom, Some(t));
    let _ = topo.access(&geom, false);
    Ok(())
}

#[cfg(test)]
mod tests;
