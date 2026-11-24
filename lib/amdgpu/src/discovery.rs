// SPDX-License-Identifier: MIT
// Copyright 2018 Advanced Micro Devices, Inc.
// Copyright 2018-2024 Advanced Micro Devices, Inc. All rights reserved.
// Copyright (C) 2018  Advanced Micro Devices, Inc.
// Copyright (c) 2026 The AbyssBSD Project
//! The IP discovery table (specification 01, section 4): the binary header
//! and its table list, the IP table with its dies and blocks (hardware ID,
//! instance, version, segment bases), the harvest table, and the GC info
//! table.

use alloc::vec::Vec;

/// The binary's signature, its first dword.
pub const SIGNATURE: u32 = 0x2821_1407;
/// The region the table occupies in VRAM: 10 KiB. Only the first
/// `binary_size` bytes are the table; the rest is not constant.
pub const REGION_SIZE: usize = 10 * 1024;

const IP_SIGNATURE: u32 = 0x5344_5049; // "IPDS"
const HARVEST_SIGNATURE: u32 = 0x5652_4148; // "HARV"
const GC_TABLE_ID: u32 = 0x4347;
/// The most dies the table can describe.
const MAX_DIES: usize = 16;
/// The most harvest entries.
const MAX_HARVEST: usize = 32;

/// Hardware IDs (01, section 4.5).
pub mod hw {
    /// The system management unit (MP1).
    pub const MP1: u16 = 1;
    /// MP2.
    pub const MP2: u16 = 2;
    /// Thermal.
    pub const THM: u16 = 3;
    /// System management unit IO.
    pub const SMUIO: u16 = 4;
    /// Graphics and compute.
    pub const GC: u16 = 11;
    /// Video codec.
    pub const VCN: u16 = 12;
    /// Memory multimedia hub.
    pub const MMHUB: u16 = 34;
    /// Address translation hub.
    pub const ATHUB: u16 = 35;
    /// The interrupt handler's block.
    pub const OSSSYS: u16 = 40;
    /// Host data path.
    pub const HDP: u16 = 41;
    /// The first system DMA engine.
    pub const SDMA0: u16 = 42;
    /// The second.
    pub const SDMA1: u16 = 43;
    /// Bus interface.
    pub const NBIF: u16 = 108;
    /// Memory channels.
    pub const UMC: u16 = 150;
    /// The platform security processor (MP0).
    pub const MP0: u16 = 255;
    /// Display (DCN).
    pub const DMU: u16 = 271;
}

/// Why a table was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shorter than the structure being read.
    Truncated,
    /// The binary's signature is wrong.
    BadSignature,
    /// `binary_size` is below 12 or above what was read, or above the
    /// 10 KiB region.
    BadSize,
    /// A checksum does not match.
    BadChecksum(&'static str),
    /// A table's signature or ID is wrong.
    BadTable(&'static str),
    /// An offset or count lies outside the table it belongs to.
    OutOfRange(&'static str),
}

fn u16_at(b: &[u8], at: usize) -> Result<u16, Error> {
    let w = b
        .get(at..at.checked_add(2).ok_or(Error::Truncated)?)
        .ok_or(Error::Truncated)?;
    Ok(u16::from_le_bytes([w[0], w[1]]))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, Error> {
    let w = b
        .get(at..at.checked_add(4).ok_or(Error::Truncated)?)
        .ok_or(Error::Truncated)?;
    Ok(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

/// A plain 16-bit sum of bytes, carries above bit 15 discarded.
fn sum16(b: &[u8]) -> u16 {
    b.iter().fold(0u16, |s, &x| s.wrapping_add(u16::from(x)))
}

/// `binary_size` from the 12-byte header, so a reader can fetch exactly
/// the table and no more.
///
/// # Errors
///
/// [`Error::BadSignature`] or [`Error::BadSize`].
pub fn binary_size(head: &[u8; 12]) -> Result<usize, Error> {
    if u32_at(head, 0)? != SIGNATURE {
        return Err(Error::BadSignature);
    }
    let size = usize::from(u16_at(head, 0x0a)?);
    if !(12..=REGION_SIZE).contains(&size) {
        return Err(Error::BadSize);
    }
    Ok(size)
}

/// One IP block instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ip {
    /// Its hardware ID ([`hw`]).
    pub hw_id: u16,
    /// Which instance of that block.
    pub instance: u8,
    /// Version, major.
    pub major: u8,
    /// Version, minor.
    pub minor: u8,
    /// Version, revision (for VCN, bits 7:6 hold codec capability).
    pub revision: u8,
    /// The raw eighth byte: harvest flag (versions up to 2), or
    /// sub-revision and variant (3 and on).
    pub flags: u8,
    /// Segment bases, as dword indices.
    pub bases: Vec<u64>,
}

impl Ip {
    /// The block's version as `(major, minor, revision)`.
    #[must_use]
    pub fn version(&self) -> (u8, u8, u8) {
        (self.major, self.minor, self.revision)
    }
}

/// One die and its blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Die {
    /// Its ID (equal to its index).
    pub id: u16,
    /// Its blocks, in table order.
    pub ips: Vec<Ip>,
}

/// A harvested (fused-off) block instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Harvested {
    /// Its hardware ID.
    pub hw_id: u16,
    /// Its instance.
    pub instance: u8,
}

/// The GC info table's fields (version 1.0 and 1.1; the words are in the
/// order of 01, section 4.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcInfo {
    /// Table version.
    pub version: (u16, u16),
    /// Shader engines.
    pub shader_engines: u32,
    /// WGPs (type 0) per shader array.
    pub wgp0_per_sa: u32,
    /// WGPs (type 1) per shader array.
    pub wgp1_per_sa: u32,
    /// Render backends per shader engine.
    pub rbs_per_se: u32,
    /// L2 cache channels.
    pub l2_channels: u32,
    /// General purpose registers.
    pub gprs: u32,
    /// Wave size.
    pub wave_size: u32,
    /// Maximum waves per SIMD.
    pub max_waves_per_simd: u32,
    /// Local data share, in KiB.
    pub lds_kib: u32,
    /// Shader arrays per shader engine.
    pub shader_arrays_per_se: u32,
    /// The words after the ones named (up to the table's size).
    pub rest: Vec<u32>,
}

impl GcInfo {
    /// Compute units per shader array: twice the workgroup processors.
    #[must_use]
    pub fn cus_per_sa(&self) -> u32 {
        2 * (self.wgp0_per_sa + self.wgp1_per_sa)
    }
}

/// What the table says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    /// Binary version, `(major, minor)`.
    pub version: (u16, u16),
    /// The dies.
    pub dies: Vec<Die>,
    /// Blocks listed as fused off.
    pub harvest: Vec<Harvested>,
    /// The GC info table, if there is one.
    pub gc: Option<GcInfo>,
}

impl Discovery {
    /// The block `hw_id`, instance `instance`, on die 0.
    #[must_use]
    pub fn ip(&self, hw_id: u16, instance: u8) -> Option<&Ip> {
        self.dies
            .first()?
            .ips
            .iter()
            .find(|i| i.hw_id == hw_id && i.instance == instance)
    }

    /// Segment `segment`'s base of block `hw_id` instance `instance`.
    #[must_use]
    pub fn base(&self, hw_id: u16, instance: u8, segment: usize) -> Option<u64> {
        self.ip(hw_id, instance)?.bases.get(segment).copied()
    }

    /// Whether the table lists the instance as fused off.
    #[must_use]
    pub fn harvested(&self, hw_id: u16, instance: u8) -> bool {
        self.harvest
            .iter()
            .any(|h| h.hw_id == hw_id && h.instance == instance)
    }
}

/// A table found through its slot: its bytes, checksum, and offset.
type Found<'a> = (&'a [u8], u16, usize);

/// One slot of the table list.
#[derive(Clone, Copy, Debug)]
struct Slot {
    offset: usize,
    checksum: u16,
    size: usize,
}

/// The table list, with slots that overlap the lowest table offset taken
/// as absent (the trap in version 1 headers, 01 section 4.2).
fn slots(b: &[u8], total: usize, major: u16) -> Result<Vec<Option<Slot>>, Error> {
    let (first, count) = if major >= 2 {
        (0x10, usize::from(u16_at(b, 0x0c)?))
    } else {
        (0x0c, 7)
    };
    let mut raw = Vec::new();
    for i in 0..count {
        let at = first + 8 * i;
        // A slot must lie in the binary at all.
        if at + 8 > total {
            break;
        }
        let offset = usize::from(u16_at(b, at)?);
        raw.push((
            at,
            Slot {
                offset,
                checksum: u16_at(b, at + 2)?,
                size: usize::from(u16_at(b, at + 4)?),
            },
        ));
    }
    let lowest = raw
        .iter()
        .map(|(_, s)| s.offset)
        .filter(|&o| o != 0)
        .min()
        .unwrap_or(usize::MAX);
    Ok(raw
        .into_iter()
        .map(|(at, s)| (s.offset != 0 && at + 8 <= lowest).then_some(s))
        .collect())
}

/// Parses the table from `bytes`, which holds at least `binary_size`
/// bytes; anything past `binary_size` is ignored.
///
/// # Errors
///
/// [`Error`] naming the first rule of 01 section 8 the table breaks.
pub fn parse(bytes: &[u8]) -> Result<Discovery, Error> {
    let head: &[u8; 12] = bytes
        .get(..12)
        .and_then(|h| h.try_into().ok())
        .ok_or(Error::Truncated)?;
    let total = binary_size(head)?;
    let b = bytes.get(..total).ok_or(Error::BadSize)?;
    let (major, minor) = (u16_at(b, 4)?, u16_at(b, 6)?);
    if major > 2 {
        return Err(Error::BadTable("binary version"));
    }
    if sum16(&b[0x0a..]) != u16_at(b, 8)? {
        return Err(Error::BadChecksum("binary"));
    }
    let slots = slots(b, total, major)?;
    let table = |i: usize, name: &'static str| -> Result<Option<Found<'_>>, Error> {
        let Some(Some(s)) = slots.get(i) else {
            return Ok(None);
        };
        let end = s
            .offset
            .checked_add(s.size)
            .ok_or(Error::OutOfRange(name))?;
        let t = b.get(s.offset..end).ok_or(Error::OutOfRange(name))?;
        Ok(Some((t, s.checksum, s.offset)))
    };

    // The IP table (slot 0) is required.
    let (ipt, ip_sum, ip_off) = table(0, "IP table")?.ok_or(Error::BadTable("no IP table"))?;
    if sum16(ipt) != ip_sum {
        return Err(Error::BadChecksum("IP table"));
    }
    let dies = parse_ip_table(ip_off, ipt)?;

    let harvest = match table(2, "harvest table")? {
        Some((t, sum, _)) => parse_harvest(t, sum)?,
        None => Vec::new(),
    };
    let gc = match table(1, "GC info table")? {
        Some((t, sum, _)) => Some(parse_gc(t, sum)?),
        None => None,
    };
    Ok(Discovery {
        version: (major, minor),
        dies,
        harvest,
        gc,
    })
}

fn parse_ip_table(table_start: usize, t: &[u8]) -> Result<Vec<Die>, Error> {
    const HEAD: usize = 0x50;
    if t.len() < HEAD {
        return Err(Error::Truncated);
    }
    if u32_at(t, 0)? != IP_SIGNATURE {
        return Err(Error::BadTable("IP table signature"));
    }
    let version = u16_at(t, 4)?;
    let wide = version >= 4 && t[0x4e] & 1 != 0;
    let die_count = usize::from(u16_at(t, 0x0c)?);
    if die_count > MAX_DIES {
        return Err(Error::OutOfRange("die count"));
    }
    let mut dies = Vec::new();
    for i in 0..die_count {
        let id = u16_at(t, 0x0e + 4 * i)?;
        let off = usize::from(u16_at(t, 0x0e + 4 * i + 2)?);
        let local = off
            .checked_sub(table_start)
            .ok_or(Error::OutOfRange("die offset"))?;
        let head = t
            .get(local..local.checked_add(4).ok_or(Error::OutOfRange("die"))?)
            .ok_or(Error::OutOfRange("die offset"))?;
        let (did, count) = (
            u16::from_le_bytes([head[0], head[1]]),
            usize::from(u16::from_le_bytes([head[2], head[3]])),
        );
        if usize::from(did) != i || id != did {
            return Err(Error::BadTable("die ID"));
        }
        let mut at = local + 4;
        let mut ips = Vec::new();
        for _ in 0..count {
            let e = t
                .get(at..at.checked_add(8).ok_or(Error::OutOfRange("IP"))?)
                .ok_or(Error::OutOfRange("IP entry"))?;
            let n = usize::from(e[3]);
            let width = if wide { 8 } else { 4 };
            let size = 8 + width * n;
            let body = t
                .get(at + 8..at.checked_add(size).ok_or(Error::OutOfRange("IP"))?)
                .ok_or(Error::OutOfRange("IP bases"))?;
            let bases = body
                .chunks_exact(width)
                .map(|c| {
                    let low = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                    // With 64-bit bases only bits 29:0 of the low word are
                    // the segment base.
                    if wide {
                        u64::from(low & 0x3fff_ffff)
                    } else {
                        u64::from(low)
                    }
                })
                .collect();
            ips.push(Ip {
                hw_id: u16::from_le_bytes([e[0], e[1]]),
                instance: e[2],
                major: e[4],
                minor: e[5],
                revision: e[6],
                flags: e[7],
                bases,
            });
            at += size;
        }
        dies.push(Die { id: did, ips });
    }
    Ok(dies)
}

fn parse_harvest(t: &[u8], sum: u16) -> Result<Vec<Harvested>, Error> {
    // 8-byte header and 32 four-byte entries: 136 bytes, summed whole.
    const SIZE: usize = 8 + 4 * MAX_HARVEST;
    let t = t.get(..SIZE).ok_or(Error::Truncated)?;
    if u32_at(t, 0)? != HARVEST_SIGNATURE {
        return Err(Error::BadTable("harvest signature"));
    }
    if sum16(t) != sum {
        return Err(Error::BadChecksum("harvest table"));
    }
    let mut out = Vec::new();
    for e in t[8..].as_chunks::<4>().0 {
        let hw_id = u16::from_le_bytes([e[0], e[1]]);
        if hw_id == 0 {
            break;
        }
        if usize::from(e[2]) >= MAX_HARVEST {
            return Err(Error::OutOfRange("harvest instance"));
        }
        out.push(Harvested {
            hw_id,
            instance: e[2],
        });
    }
    Ok(out)
}

fn parse_gc(t: &[u8], sum: u16) -> Result<GcInfo, Error> {
    if u32_at(t, 0)? != GC_TABLE_ID {
        return Err(Error::BadTable("GC table ID"));
    }
    let (major, minor) = (u16_at(t, 4)?, u16_at(t, 6)?);
    let size = usize::try_from(u32_at(t, 8)?).map_err(|_| Error::OutOfRange("GC size"))?;
    let body = t.get(..size).ok_or(Error::OutOfRange("GC size"))?;
    if sum16(body) != sum {
        return Err(Error::BadChecksum("GC info table"));
    }
    if major != 1 {
        return Err(Error::BadTable("GC table version"));
    }
    let words: Vec<u32> = body[12..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    let w = |i: usize| words.get(i).copied().ok_or(Error::Truncated);
    Ok(GcInfo {
        version: (major, minor),
        shader_engines: w(0)?,
        wgp0_per_sa: w(1)?,
        wgp1_per_sa: w(2)?,
        rbs_per_se: w(3)?,
        l2_channels: w(4)?,
        gprs: w(5)?,
        wave_size: w(11)?,
        max_waves_per_simd: w(12)?,
        lds_kib: w(14)?,
        shader_arrays_per_se: w(16)?,
        rest: words.get(17..).map(<[u32]>::to_vec).unwrap_or_default(),
    })
}

#[cfg(test)]
#[path = "discovery_tests.rs"]
mod tests;
