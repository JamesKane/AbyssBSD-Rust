// SPDX-License-Identifier: BSD-2-Clause
//! The SMBIOS variables of the kernel environment, as FreeBSD's EFI
//! loader sets them (`efi_smbios_detect`): `hint.smbios.0.mem`, the entry
//! point's address, for each SMBIOS configuration table in turn (so the
//! last one's, unless a 3.0 table parses first), and the `smbios.*`
//! variables of the 3.0 table, or else of the 2.1 one ([`smbios`]).
//!
//! Behavior reference: stand/efi/loader/main.c (`efi_smbios_detect`),
//! stand/libsa/smbios.c.

use alloc::vec::Vec;

use efi::SystemTable;
use smbios::{EntryPoint, Vars};

/// The most of a structure table read (SMBIOS 3.0's limit is 4 GiB; real
/// tables are a few KiB).
const TABLE_MAX: u32 = 1 << 20;

/// Firmware memory at `phys`, `len` bytes.
///
/// # Safety
///
/// Boot services must be up (memory identity-mapped), and `phys..+len`
/// must be firmware's table memory.
unsafe fn bytes(phys: u64, len: usize) -> &'static [u8] {
    // SAFETY: per the caller, identity-mapped table memory, never written
    // while the loader runs.
    unsafe { core::slice::from_raw_parts(phys as *const u8, len) }
}

/// The entry point at `phys` and its table's variables (`smbios_detect`).
fn detect(phys: u64) -> Option<Vars> {
    if phys == 0 {
        return None;
    }
    // SAFETY: the configuration table points at an entry point, whose
    // anchor is at least 5 bytes.
    let anchor = unsafe { bytes(phys, 5) };
    let len = match anchor {
        b"_SM3_" => 0x18,
        [b'_', b'S', b'M', b'_', _] => 0x1f,
        _ => return None,
    };
    // SAFETY: an entry point with that anchor is `len` bytes long.
    let ep = EntryPoint::parse(unsafe { bytes(phys, len) })?;
    if ep.table == 0 {
        return None;
    }
    let tlen = usize::try_from(ep.length.min(TABLE_MAX)).ok()?;
    // SAFETY: the entry point says where the structure table is and how
    // long (its maximum) firmware made it.
    let table = unsafe { bytes(ep.table, tlen) };
    Some(smbios::variables(&ep, table))
}

/// The variables, in the order FreeBSD's loader sets them.
pub(crate) fn environment(st: &SystemTable) -> Vars {
    let mut out = Vars::new();
    let mut v2 = None;
    let mut found = None;
    for t in st.configuration_tables() {
        let guid = t.vendor_guid.get();
        let is_v2 = guid == efi::tables::SMBIOS;
        let is_v3 = guid == efi::tables::SMBIOS3;
        if !is_v2 && !is_v3 {
            continue;
        }
        let phys = t.vendor_table as u64;
        out.retain(|(n, _)| *n != "hint.smbios.0.mem");
        out.push(("hint.smbios.0.mem", smbios::hint_value(phys)));
        if is_v2 {
            // Read only if there is no good 3.0 table.
            v2 = Some(phys);
        } else if let Some(v) = detect(phys) {
            found = Some(v);
            break;
        }
    }
    if found.is_none() {
        found = v2.and_then(detect);
    }
    out.extend(found.unwrap_or_default());
    out
}

/// `name=value` strings, each NUL-terminated, sorted by name as the
/// loader's environment is, the last setting of a name kept.
pub(crate) fn serialize(vars: &Vars) -> Vec<u8> {
    let mut v: Vec<&(&str, Vec<u8>)> = Vec::new();
    for e in vars {
        v.retain(|o| o.0 != e.0);
        v.push(e);
    }
    v.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut out = Vec::new();
    for (n, val) in v {
        out.extend_from_slice(n.as_bytes());
        out.push(b'=');
        out.extend(val.iter().copied().filter(|&c| c != 0));
        out.push(0);
    }
    out
}
