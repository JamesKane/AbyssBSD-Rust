// SPDX-License-Identifier: BSD-2-Clause
//! Rock Ridge (`cd9660_rrip.c`): the System Use entries of a directory
//! record and its continuation areas, read for attributes (`PX`, `TF`,
//! `PN`), a name (`NM`, with `CL`, `PL`, and `RE` for relocated
//! directories), or a symbolic link (`SL`). `RR` says which entries to
//! expect, `CE` continues elsewhere, and `ST` stops.
//!
//! Each entry is checked against its area, and an entry too short for
//! its kind makes the record's answer the default, as FreeBSD's fix of
//! PR 296853 does; at mount, a malformed root is refused.

use alloc::vec::Vec;

use crate::{BlockIo, Error, Inode, MAXPATHLEN, NAME_MAX, Volume, b8, idx, n733, rec};

/// What an analysis looks for and found (`ISO_SUSP_*`).
const ATTR: u32 = 0x0001;
const DEVICE: u32 = 0x0002;
const SLINK: u32 = 0x0004;
const ALTNAME: u32 = 0x0008;
const CLINK: u32 = 0x0010;
const PLINK: u32 = 0x0020;
const RELDIR: u32 = 0x0040;
const TSTAMP: u32 = 0x0080;
const IDFLAG: u32 = 0x0100;
const EXTREF: u32 = 0x0200;
const CONT: u32 = 0x0400;
const STOP: u32 = 0x1000;

/// Component flags of `NM` and `SL` (`ISO_SUSP_CFLAG_*`).
const CFLAG_CONTINUE: u8 = 0x01;
const CFLAG_CURRENT: u8 = 0x02;
const CFLAG_PARENT: u8 = 0x04;
const CFLAG_ROOT: u8 = 0x08;
const CFLAG_VOLROOT: u8 = 0x10;
const CFLAG_HOST: u8 = 0x20;

/// `TF`'s flags.
const TS_CREAT: u8 = 0x01;
const TS_MODIFY: u8 = 0x02;
const TS_ACCESS: u8 = 0x04;
const TS_ATTR: u8 = 0x08;
const TS_FORM17: u8 = 0x80;

/// An entry too short for its kind.
struct Bad;

/// A continuation area (`CE`): block, offset, length.
type Ce = Option<(u64, u64, u64)>;

/// What [`walk`] calls for each entry: its kind, the entry, what is still
/// wanted, and where entries continue.
type EntryFn<'a> = dyn FnMut(&[u8; 2], &[u8], &mut u32, &mut Ce) -> Result<u32, Bad> + 'a;

/// Walks the System Use entries of record `r` and its continuation
/// areas, calling `f` with each entry of version 1, its kind, `fields`
/// (what is still wanted), and the continuation to follow next
/// (`cd9660_rrip_loop`). Returns what `f` found, or `Bad`.
fn walk<B: BlockIo + ?Sized>(
    v: &Volume,
    io: &B,
    cx: &B::Cx,
    r: &[u8],
    fields: &mut u32,
    f: &mut EntryFn<'_>,
) -> Result<u32, Bad> {
    // System Use follows the name and its padding to an even offset.
    let name_len = usize::from(b8(r, 32));
    let mut start = 33 + name_len;
    if name_len & 1 == 0 {
        start += 1;
    }
    let (c, _) = crate::name::isochar(rec::name(r), v.joliet_level);
    // The root's `.` skips what its `SP` says only from the second on.
    start += if c != 0 || u64::from(rec::extent(r)) != v.root_extent {
        v.rr_skip
    } else {
        v.rr_skip0
    };
    let end = rec::len(r).min(r.len());
    let mut area: Vec<u8> = r.get(start..end).unwrap_or(&[]).to_vec();
    let mut result = 0;
    loop {
        let mut ce: Ce = None;
        let mut at = 0;
        while area.len() >= at + 4 {
            let len = usize::from(area[at + 2]);
            if len < 4 || at + len > area.len() {
                break;
            }
            let e = &area[at..at + len];
            if e[3] == 1 {
                result |= f(&[e[0], e[1]], e, fields, &mut ce)?;
                if *fields == 0 {
                    break;
                }
            }
            if result & STOP != 0 {
                result &= !STOP;
                break;
            }
            at += len;
        }
        match ce {
            Some((blk, off, len)) if *fields != 0 && len != 0 => {
                if blk >= v.volume_space_size || off + len > u64::from(v.block_size) {
                    break;
                }
                let Ok(block) = read_block(v, io, cx, blk) else {
                    break;
                };
                area = block.get(idx(off)..idx(off + len)).unwrap_or(&[]).to_vec();
            }
            _ => break,
        }
    }
    Ok(result)
}

fn read_block<B: BlockIo + ?Sized>(
    v: &Volume,
    io: &B,
    cx: &B::Cx,
    blk: u64,
) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    io.read(
        cx,
        blk << v.block_size.trailing_zeros(),
        idx(u64::from(v.block_size)),
        &mut |b| {
            out.extend_from_slice(b);
        },
    )?;
    Ok(out)
}

/// `CE`: where the entries continue.
fn cont(e: &[u8], ce: &mut Ce) -> Result<u32, Bad> {
    if e.len() < 28 {
        return Err(Bad);
    }
    *ce = Some((
        u64::from(n733(e, 4)),
        u64::from(n733(e, 12)),
        u64::from(n733(e, 20)),
    ));
    Ok(CONT)
}

/// `RR`: what entries the record has; with `RE` among them, as `RE`.
fn idflag(e: &[u8], fields: &mut u32) -> Result<u32, Bad> {
    if e.len() < 5 {
        return Err(Bad);
    }
    *fields &= u32::from(e[4]) | !0xff;
    if *fields & RELDIR != 0 {
        return Ok(reldir(fields));
    }
    Ok(IDFLAG)
}

/// `RE`: a directory's original place, hidden.
fn reldir(fields: &mut u32) -> u32 {
    *fields = 0;
    RELDIR | ALTNAME | CLINK | PLINK
}

/// FreeBSD's `major()` and `minor()` of a 32-bit device number, and
/// `makedev()`.
fn major(x: u32) -> u32 {
    (x >> 8) & 0xff
}
fn minor(x: u32) -> u32 {
    ((x >> 24) & 0xff00) | (x & 0xffff_00ff)
}
fn makedev(x: u32, y: u32) -> u64 {
    let (x, y) = (u64::from(x), u64::from(y));
    ((x & 0xffff_ff00) << 32) | ((x & 0xff) << 8) | ((y & 0xff00) << 24) | (y & 0xffff_00ff)
}

/// Fills `ip`'s attributes and times from record `r`
/// (`cd9660_rrip_analyze`), with the defaults FreeBSD falls back on.
pub(crate) fn analyze<B: BlockIo + ?Sized>(
    v: &Volume,
    io: &B,
    cx: &B::Cx,
    r: &[u8],
    ip: &mut Inode,
) -> Result<(), Error> {
    let mut fields = ATTR | TSTAMP | DEVICE;
    let found = walk(
        v,
        io,
        cx,
        r,
        &mut fields,
        &mut |kind, e, fields, ce| match kind {
            b"PX" => {
                if e.len() < 36 {
                    return Err(Bad);
                }
                // `iso_mode` and `iso_links` are 16 bits wide.
                ip.mode = u16::from_le_bytes([e[4], e[5]]);
                ip.links = i16::from_le_bytes([e[12], e[13]]);
                ip.uid = n733(e, 20);
                ip.gid = n733(e, 28);
                *fields &= !ATTR;
                Ok(ATTR)
            }
            b"TF" => {
                tstamp(e, ip)?;
                *fields &= !TSTAMP;
                Ok(TSTAMP)
            }
            b"PN" => {
                if e.len() < 20 {
                    return Err(Bad);
                }
                let high = n733(e, 4);
                let low = n733(e, 12);
                ip.rdev = if high == 0 {
                    makedev(major(low), minor(low))
                } else {
                    makedev(high, minor(low))
                };
                *fields &= !DEVICE;
                Ok(DEVICE)
            }
            b"RR" => idflag(e, fields),
            b"CE" => cont(e, ce),
            b"ST" => Ok(STOP),
            _ => Ok(0),
        },
    )
    .unwrap_or(0);
    if found & ATTR == 0 {
        // A required field; FreeBSD says "RRIP without PX field?".
        v.defattr(r, ip, None);
    }
    if found & TSTAMP == 0 {
        v.deftstamp(r, ip, None);
    }
    Ok(())
}

/// `TF`: the times present, in either form (FreeBSD 15.1 keeps no birth
/// time; a missing modification time is the epoch).
fn tstamp(e: &[u8], ip: &mut Inode) -> Result<(), Bad> {
    if e.len() < 5 {
        return Err(Bad);
    }
    let flags = e[4];
    let size = if flags & TS_FORM17 != 0 { 17 } else { 7 };
    let count = [TS_CREAT, TS_MODIFY, TS_ACCESS, TS_ATTR]
        .iter()
        .filter(|&&b| flags & b != 0)
        .count();
    if 5 + count * size > e.len() {
        return Err(Bad);
    }
    let mut at = 5;
    let conv = |at: usize| {
        if size == 17 {
            crate::tstamp17(&e[at..at + 17]).0
        } else {
            let mut t = [0u8; 7];
            t.copy_from_slice(&e[at..at + 7]);
            crate::tstamp7(&t, false).0
        }
    };
    if flags & TS_CREAT != 0 {
        at += size;
    }
    if flags & TS_MODIFY != 0 {
        ip.mtime = conv(at);
        at += size;
    } else {
        ip.mtime = crate::Time::default();
    }
    if flags & TS_ACCESS != 0 {
        ip.atime = conv(at);
        at += size;
    } else {
        ip.atime = ip.mtime;
    }
    ip.ctime = if flags & TS_ATTR != 0 {
        conv(at)
    } else {
        ip.mtime
    };
    Ok(())
}

/// The name of record `r` into `out` (`cd9660_rrip_getname`), `*ino`
/// moved by a child or parent link; an empty name for a relocated
/// directory's original place.
pub(crate) fn getname<B: BlockIo + ?Sized>(
    v: &Volume,
    io: &B,
    cx: &B::Cx,
    r: &[u8],
    out: &mut Vec<u8>,
    ino: &mut u64,
) -> Result<(), Error> {
    out.clear();
    let mut fields = ALTNAME | RELDIR | CLINK | PLINK;
    let (c, _) = crate::name::isochar(rec::name(r), v.joliet_level);
    // `.` and `..` keep their names; only their links are read.
    let dots = c == 0 || c == 1;
    if dots {
        defname(v, r, out);
        fields &= !ALTNAME;
    }
    let host = || io.hostname(cx);
    let bshift = v.block_size.trailing_zeros();
    let mut cont_name = false;
    let mut name: Vec<u8> = Vec::new();
    let found = walk(
        v,
        io,
        cx,
        r,
        &mut fields,
        &mut |kind, e, fields, ce| match kind {
            b"NM" if !dots => {
                if e.len() < 5 {
                    return Err(Bad);
                }
                let flags = e[4];
                let piece: Vec<u8> = match flags {
                    CFLAG_CURRENT => b".".to_vec(),
                    CFLAG_PARENT => b"..".to_vec(),
                    CFLAG_HOST => host(),
                    CFLAG_CONTINUE | 0 => e[5..].to_vec(),
                    // FreeBSD 15.1 then treats it as no name.
                    _ => {
                        *fields &= !ALTNAME;
                        name.clear();
                        return Ok(0);
                    }
                };
                cont_name = flags == CFLAG_CONTINUE;
                if name.len() + piece.len() > NAME_MAX {
                    *fields &= !ALTNAME;
                    name.clear();
                    return Ok(0);
                }
                name.extend_from_slice(&piece);
                if cont_name {
                    return Ok(0);
                }
                *fields &= !ALTNAME;
                Ok(ALTNAME)
            }
            b"CL" | b"PL" => {
                if e.len() < 12 {
                    return Err(Bad);
                }
                *ino = u64::from(n733(e, 4)) << bshift;
                *fields &= !(CLINK | PLINK);
                Ok(if kind == b"CL" { CLINK } else { PLINK })
            }
            b"RE" => {
                name.clear();
                Ok(reldir(fields))
            }
            b"RR" => {
                let r = idflag(e, fields)?;
                if r & RELDIR != 0 {
                    name.clear();
                }
                Ok(r)
            }
            b"CE" => cont(e, ce),
            b"ST" => Ok(STOP),
            _ => Ok(0),
        },
    );
    match found {
        Ok(found) => {
            if found & RELDIR != 0 {
                out.clear();
                return Ok(());
            }
            if !dots {
                if found & ALTNAME != 0 {
                    *out = name;
                } else {
                    defname(v, r, out);
                }
            }
        }
        Err(Bad) => {
            // Malformed: as if it had nothing but its ISO name.
            out.clear();
            defname(v, r, out);
        }
    }
    Ok(())
}

/// The ISO name as recorded, `.` and `..` for the special ones
/// (`cd9660_rrip_defname`).
fn defname(v: &Volume, r: &[u8], out: &mut Vec<u8>) {
    let n = crate::name::isofntrans(rec::name(r), true, rec::flags(r) & 4 != 0, v.joliet_level);
    *out = match n.first() {
        Some(0) => b".".to_vec(),
        Some(1) => b"..".to_vec(),
        _ => n,
    };
}

/// A symbolic link's target from record `r` (`cd9660_rrip_getsymname`).
pub(crate) fn getsymname<B: BlockIo + ?Sized>(
    v: &Volume,
    io: &B,
    cx: &B::Cx,
    r: &[u8],
    mounted_on: &[u8],
) -> Result<Vec<u8>, Error> {
    let mut fields = SLINK;
    let mut out: Vec<u8> = Vec::new();
    // No slash before the first component.
    let mut slash_next = false;
    let host = || io.hostname(cx);
    let found = walk(
        v,
        io,
        cx,
        r,
        &mut fields,
        &mut |kind, e, fields, ce| match kind {
            b"SL" => {
                if e.len() < 6 {
                    return Err(Bad);
                }
                let mut at = 5;
                while at < e.len() {
                    let cflag = e[at];
                    let clen = usize::from(b8(e, at + 1));
                    let next = at + 2 + clen;
                    if next > e.len() {
                        return Err(Bad);
                    }
                    if slash_next && out.len() < MAXPATHLEN {
                        out.push(b'/');
                    }
                    slash_next = true;
                    let piece: Vec<u8> = match cflag {
                        CFLAG_CURRENT => b".".to_vec(),
                        CFLAG_PARENT => b"..".to_vec(),
                        CFLAG_ROOT => Vec::new(),
                        CFLAG_VOLROOT => {
                            out.clear();
                            mounted_on.to_vec()
                        }
                        CFLAG_HOST => host(),
                        CFLAG_CONTINUE | 0 => {
                            slash_next = cflag != CFLAG_CONTINUE;
                            e[at + 2..next].to_vec()
                        }
                        _ => return Err(Bad),
                    };
                    if out.len() + piece.len() > MAXPATHLEN {
                        return Err(Bad);
                    }
                    out.extend_from_slice(&piece);
                    at = next;
                }
                if e[4] == 0 {
                    *fields &= !SLINK;
                    return Ok(SLINK);
                }
                Ok(0)
            }
            b"RR" => idflag(e, fields),
            b"CE" => cont(e, ce),
            b"ST" => Ok(STOP),
            _ => Ok(0),
        },
    );
    match found {
        Ok(f) if f & SLINK != 0 => Ok(out),
        _ => Err(Error::Invalid),
    }
}

/// Whether the root's `.` (`dot`) says Rock Ridge (`cd9660_rrip_offset`):
/// its `SP` (where System Use starts) and an `ER` naming RRIP. Returns
/// the bytes each record's System Use skips, `None` without Rock Ridge.
///
/// # Errors
///
/// [`Error::Invalid`] for malformed entries (PR 296853).
pub(crate) fn offset<B: BlockIo + ?Sized>(
    v: &mut Volume,
    io: &B,
    cx: &B::Cx,
    dot: &[u8],
) -> Result<Option<usize>, Error> {
    const SP: &[u8] = b"SP\x07\x01\xbe\xef";
    v.rr_skip0 = 0;
    let mut p = 34;
    if dot.get(p..p + 6) != Some(SP) {
        // Maybe a CD-ROM XA disc.
        v.rr_skip0 = 15;
        p += 15;
        if dot.get(p..p + 6) != Some(SP) {
            return Ok(None);
        }
    }
    let skip = usize::from(b8(dot, p + 6));
    let mut fields = EXTREF;
    let found = walk(
        v,
        io,
        cx,
        dot,
        &mut fields,
        &mut |kind, e, fields, ce| match kind {
            b"ER" => {
                if e.len() < 8 {
                    return Err(Bad);
                }
                let len = usize::from(e[4]);
                let id = e.get(8..8 + len).unwrap_or(&[]);
                let known = matches!(id, b"RRIP_1991A" | b"IEEE_P1282" | b"IEEE_1282");
                if known && e[7] == 1 {
                    *fields &= !EXTREF;
                    return Ok(EXTREF);
                }
                Ok(0)
            }
            b"CE" => cont(e, ce),
            b"ST" => Ok(STOP),
            _ => Ok(0),
        },
    )
    .map_err(|Bad| Error::Invalid)?;
    if found & EXTREF == 0 {
        return Ok(None);
    }
    Ok(Some(skip))
}
