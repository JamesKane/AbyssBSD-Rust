// SPDX-License-Identifier: BSD-2-Clause
//! SMBIOS, as FreeBSD's loader reads it: an entry point (2.1's 32-bit
//! `_SM_` or 3.0's 64-bit `_SM3_`), and the variables it puts in the
//! kernel environment from the structure table (`smbios.bios.*`,
//! `smbios.system.*`, `smbios.planar.*`, `smbios.chassis.*`,
//! `smbios.processor.version`, `smbios.memory.enabled`, `smbios.socket.*`,
//! `smbios.version`, `smbios.entry_point_type`), with serial numbers, asset
//! tags, and the system UUID (`SMBIOS_SERIAL_NUMBERS`, set in a default
//! build), the UUID in network byte order before SMBIOS 2.6 and
//! little-endian from it.
//!
//! Firmware data is untrusted: every read is bounded by the table, and a
//! structure or string that runs past the table ends the walk (FreeBSD
//! reads on; for a well-formed table the variables are the same).
//!
//! Behavior reference: stand/libsa/smbios.c (`smbios_sigsearch`,
//! `smbios_probe`, `smbios_parse_table`, `smbios_detect`), the DMTF SMBIOS
//! specification (DSP0134) for the layouts.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// Which entry point the table came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// SMBIOS 2.1's 32-bit entry point (`_SM_`, with `_DMI_` at 0x10).
    V2,
    /// SMBIOS 3.0's 64-bit entry point (`_SM3_`).
    V3,
}

/// A checked entry point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryPoint {
    pub kind: Kind,
    /// The structure table's physical address.
    pub table: u64,
    /// Its length (2.1) or maximum size (3.0), in bytes.
    pub length: u32,
    /// How many structures it holds (2.1); 3.0 does not say.
    pub count: Option<u16>,
    pub major: u8,
    pub minor: u8,
}

/// The longest entry point (3.0's is 0x18 bytes, 2.1's 0x1f).
pub const ENTRY_POINT_MAX: usize = 0x20;

/// The `(name, value)` variables the loader sets, in the order it sets
/// them (it keeps its environment sorted by name, so the order shows only
/// in which of two settings of a name wins: the later).
pub type Vars = Vec<(&'static str, Vec<u8>)>;

fn get8(b: &[u8], at: usize) -> Option<u8> {
    b.get(at).copied()
}

fn get16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(*b.get(at..)?.first_chunk()?))
}

fn get32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(*b.get(at..)?.first_chunk()?))
}

fn get64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(*b.get(at..)?.first_chunk()?))
}

fn checksum(b: &[u8], len: usize) -> Option<u8> {
    Some(
        b.get(..len)?
            .iter()
            .fold(0u8, |sum, &x| sum.wrapping_add(x)),
    )
}

impl EntryPoint {
    /// The entry point at the start of `b` (bytes from where the UEFI
    /// configuration table points), if its anchor and checksums are good
    /// (`smbios_sigsearch` over one paragraph, then `smbios_probe`).
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.starts_with(b"_SM3_")
            // The table's size and address are there only from revision 1.
            && get8(b, 0x0a)? != 0
            && checksum(b, usize::from(get8(b, 0x06)?))? == 0
        {
            return Some(Self {
                kind: Kind::V3,
                table: get64(b, 0x10)?,
                length: get32(b, 0x0c)?,
                count: None,
                major: get8(b, 0x07)?,
                minor: get8(b, 0x08)?,
            });
        }
        if b.starts_with(b"_SM_")
            && checksum(b, usize::from(get8(b, 0x05)?))? == 0
            && b.get(0x10..0x15)? == b"_DMI_"
            && checksum(b.get(0x10..)?, 0x0f)? == 0
        {
            // The BCD revision, unless it is 0 or not BCD; then the major
            // and minor version fields.
            let bcd = get8(b, 0x1e)?;
            let (major, minor) = if bcd != 0 && bcd >> 4 <= 9 && bcd & 0xf <= 9 {
                (bcd >> 4, bcd & 0xf)
            } else {
                (get8(b, 0x06)?, get8(b, 0x07)?)
            };
            return Some(Self {
                kind: Kind::V2,
                table: u64::from(get32(b, 0x18)?),
                length: u32::from(get16(b, 0x16)?),
                count: Some(get16(b, 0x1c)?),
                major,
                minor,
            });
        }
        None
    }

    /// `smbios.ver`: major and minor, 0xMMmm.
    #[must_use]
    pub fn version(&self) -> u16 {
        u16::from(self.major) << 8 | u16::from(self.minor)
    }
}

/// String `offset` of the structure at `s` (its formatted part `len`
/// bytes long) in `t`: the index there names one of the strings after it,
/// from 1 (`smbios_getstring`).
fn string(t: &[u8], s: usize, len: usize, offset: usize) -> Option<&[u8]> {
    let idx = get8(t, s + offset)?;
    if idx == 0 {
        return None;
    }
    let mut at = s + len;
    for _ in 1..idx {
        let n = t.get(at..)?.iter().position(|&c| c == 0)?;
        at += n + 1;
    }
    let rest = t.get(at..)?;
    let n = rest.iter().position(|&c| c == 0)?;
    Some(&rest[..n])
}

/// `smbios_parse_chassis_type`: the whole byte (with the lock bit, a
/// type is "Undefined").
fn chassis_type(t: u8) -> &'static str {
    const NAMES: [&str; 0x24] = [
        "Other",
        "Unknown",
        "Desktop",
        "Low Profile Desktop",
        "Pizza Box",
        "Mini Tower",
        "Tower",
        "Portable",
        "Laptop",
        "Notebook",
        "Hand Held",
        "Docking Station",
        "All in One",
        "Sub Notebook",
        "Lunch Box",
        "Space-saving",
        "Main Server Chassis",
        "Expansion Chassis",
        "SubChassis",
        "Bus Expansion Chassis",
        "Peripheral Chassis",
        "RAID Chassis",
        "Rack Mount Chassis",
        "Sealed-case PC",
        "Multi-system chassis",
        "Compact PCI",
        "Advanced TCA",
        "Blade",
        "Blade Enclosure",
        "Tablet",
        "Convertible",
        "Detachable",
        "IoT Gateway",
        "Embedded PC",
        "Mini PC",
        "Stick PC",
    ];
    usize::from(t)
        .checked_sub(1)
        .and_then(|i| NAMES.get(i))
        .copied()
        .unwrap_or("Undefined")
}

/// `smbios_setuuid`: the 16 bytes at `u`, unless all zeros or all ones.
fn uuid(u: &[u8], version: u16) -> Option<Vec<u8>> {
    let u = u.get(..16)?;
    let words: Vec<u32> = u
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    let (mut ones, mut zeros) = (0, 0);
    for &w in &words {
        let n = w.wrapping_add(1);
        if zeros == 0 && n == 0 {
            ones += 1;
        } else if ones == 0 && n == 1 {
            zeros += 1;
        } else {
            break;
        }
    }
    if ones == 4 || zeros == 4 {
        return None;
    }
    let (f1, f2, f3) = if version < 0x0206 {
        (
            u32::from_be_bytes([u[0], u[1], u[2], u[3]]),
            u16::from_be_bytes([u[4], u[5]]),
            u16::from_be_bytes([u[6], u[7]]),
        )
    } else {
        (
            u32::from_le_bytes([u[0], u[1], u[2], u[3]]),
            u16::from_le_bytes([u[4], u[5]]),
            u16::from_le_bytes([u[6], u[7]]),
        )
    };
    let mut s = String::new();
    let _ = write!(s, "{f1:08x}-{f2:04x}-{f3:04x}-{:02x}{:02x}-", u[8], u[9]);
    for b in &u[10..16] {
        let _ = write!(s, "{b:02x}");
    }
    Some(s.into_bytes())
}

/// What the walk adds up (`struct smbios_attr`).
#[derive(Default)]
struct Totals {
    enabled_memory: u32,
    old_enabled_memory: u32,
    enabled_sockets: u8,
    populated_sockets: u8,
}

/// The structure at `s` of `t`: sets its variables and returns where the
/// next one starts, or `None` at the end of the table
/// (`smbios_parse_table`).
fn parse_one(
    t: &[u8],
    s: usize,
    ep: &EntryPoint,
    tot: &mut Totals,
    vars: &mut Vars,
) -> Option<usize> {
    let ty = get8(t, s)?;
    let len = usize::from(get8(t, s + 1)?);
    let set = |vars: &mut Vars, name: &'static str, offset: usize| {
        if let Some(v) = string(t, s, len, offset) {
            vars.push((name, v.to_vec()));
        }
    };
    match ty {
        0 => {
            set(vars, "smbios.bios.vendor", 0x04);
            set(vars, "smbios.bios.version", 0x05);
            set(vars, "smbios.bios.reldate", 0x08);
            let (major, minor) = (get8(t, s + 0x14)?, get8(t, s + 0x15)?);
            if major != 0xff && minor != 0xff {
                let mut v = String::new();
                let _ = write!(v, "{major}.{minor}");
                vars.push(("smbios.bios.revision", v.into_bytes()));
            }
        }
        1 => {
            set(vars, "smbios.system.maker", 0x04);
            set(vars, "smbios.system.product", 0x05);
            set(vars, "smbios.system.version", 0x06);
            set(vars, "smbios.system.serial", 0x07);
            if let Some(u) = t.get(s + 0x08..).and_then(|u| uuid(u, ep.version())) {
                vars.push(("smbios.system.uuid", u));
            }
            if ep.major > 2 || (ep.major == 2 && ep.minor >= 4) {
                set(vars, "smbios.system.sku", 0x19);
                set(vars, "smbios.system.family", 0x1a);
            }
        }
        2 => {
            set(vars, "smbios.planar.maker", 0x04);
            set(vars, "smbios.planar.product", 0x05);
            set(vars, "smbios.planar.version", 0x06);
            set(vars, "smbios.planar.serial", 0x07);
            set(vars, "smbios.planar.tag", 0x08);
            set(vars, "smbios.planar.location", 0x0a);
        }
        3 => {
            set(vars, "smbios.chassis.maker", 0x04);
            vars.push((
                "smbios.chassis.type",
                chassis_type(get8(t, s + 0x05)?).as_bytes().to_vec(),
            ));
            set(vars, "smbios.chassis.version", 0x06);
            set(vars, "smbios.chassis.serial", 0x07);
            set(vars, "smbios.chassis.tag", 0x08);
        }
        4 => {
            set(vars, "smbios.processor.version", 0x10);
            let status = get8(t, s + 0x18)?;
            if status & 0x07 == 1 {
                tot.enabled_sockets = tot.enabled_sockets.wrapping_add(1);
            }
            if status & 0x40 != 0 {
                tot.populated_sockets = tot.populated_sockets.wrapping_add(1);
            }
        }
        6 => {
            // Enabled size: 2^n MiB, in KiB.
            let n = get8(t, s + 0x0a)? & 0x7f;
            if n > 0 && n < 22 {
                tot.old_enabled_memory = tot.old_enabled_memory.wrapping_add(1 << (n + 10));
            }
        }
        17 => {
            // Size: KiB with bit 15, else MiB.
            let size = get16(t, s + 0x0c)?;
            if size != 0 && size != 0xffff {
                let kib = if size & 0x8000 != 0 {
                    u32::from(size & 0x7fff)
                } else {
                    u32::from(size) << 10
                };
                tot.enabled_memory = tot.enabled_memory.wrapping_add(kib);
            }
        }
        // End-of-table.
        0x7f => return None,
        _ => {}
    }
    // The strings end at a double NUL.
    let mut at = s + len;
    while get16(t, at)? != 0 {
        at += 1;
    }
    Some(at + 2)
}

/// The variables FreeBSD's loader sets for the table `t` (the bytes at
/// `ep.table`, `ep.length` long) (`smbios_detect`).
#[must_use]
pub fn variables(ep: &EntryPoint, t: &[u8]) -> Vars {
    let mut vars = Vars::new();
    let mut tot = Totals::default();
    let end = t
        .len()
        .min(usize::try_from(ep.length).unwrap_or(usize::MAX));
    let t = &t[..end];
    let count = ep.count.map_or(usize::MAX, usize::from);
    let mut at = Some(0);
    let mut i = 0;
    while let Some(s) = at
        && s < t.len()
        && i < count
    {
        at = parse_one(t, s, ep, &mut tot, &mut vars);
        i += 1;
    }
    vars.push((
        "smbios.entry_point_type",
        match ep.kind {
            Kind::V3 => b"v3 (64-bit)".to_vec(),
            Kind::V2 => b"v2.1 (32-bit)".to_vec(),
        },
    ));
    let num = |n: u32| {
        let mut v = String::new();
        let _ = write!(v, "{n}");
        v.into_bytes()
    };
    let mut v = String::new();
    let _ = write!(v, "{}.{}", ep.major, ep.minor);
    vars.push(("smbios.version", v.into_bytes()));
    if tot.enabled_memory > 0 || tot.old_enabled_memory > 0 {
        let m = if tot.enabled_memory > 0 {
            tot.enabled_memory
        } else {
            tot.old_enabled_memory
        };
        vars.push(("smbios.memory.enabled", num(m)));
    }
    if tot.enabled_sockets > 0 {
        vars.push(("smbios.socket.enabled", num(u32::from(tot.enabled_sockets))));
    }
    if tot.populated_sockets > 0 {
        vars.push((
            "smbios.socket.populated",
            num(u32::from(tot.populated_sockets)),
        ));
    }
    vars
}

/// `%p` of an address, as `hint.smbios.0.mem` holds it: `0x` and
/// lower-case hex without leading zeros.
#[must_use]
pub fn hint_value(addr: u64) -> Vec<u8> {
    let mut s = String::new();
    let _ = write!(s, "{addr:#x}");
    s.into_bytes()
}

#[cfg(test)]
mod tests;
