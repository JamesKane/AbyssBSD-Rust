// SPDX-License-Identifier: BSD-2-Clause
//! The firmware's ACPI tables, read through the DMAP.

use acpi_tables::{PhysMem, Sdt, Tables};
use bootproto::layout::dmap;
use sync::Once;

/// Physical memory through the DMAP, limited to RAM the memory map lists
/// (anything else is not mapped, and reading it would fault).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DmapMem;

impl PhysMem<'static> for DmapMem {
    fn bytes(&self, pa: u64, len: usize) -> Option<&'static [u8]> {
        let end = pa.checked_add(u64::try_from(len).ok()?)?;
        let in_ram = crate::boot::memory_map().iter().any(|r| {
            let range = r.range();
            r.kind.is_ram() && range.start.get() <= pa && end <= range.end.get()
        });
        if !in_ram {
            return None;
        }
        let va = dmap(addr::PhysAddr::new(pa))?;
        // SAFETY: the range is RAM, which the DMAP maps read/write for the
        // life of the kernel; ACPI tables are never reclaimed while in use.
        Some(unsafe { core::slice::from_raw_parts(va.get() as *const u8, len) })
    }
}

static TABLES: Once<Option<Tables<'static, DmapMem>>> = Once::new();

/// The validated root tables, if firmware provided any.
pub(crate) fn tables() -> Option<&'static Tables<'static, DmapMem>> {
    if TABLES.get().is_none() {
        let found = crate::boot::info().firmware.rsdp.get().and_then(|rsdp| {
            match Tables::new(DmapMem, rsdp.get()) {
                Ok(t) => Some(t),
                Err(e) => {
                    kprintln!("acpi: unusable RSDP at {rsdp:?}: {e:?}");
                    None
                }
            }
        });
        // A racing first caller set the same value.
        let _ = TABLES.set(found);
    }
    TABLES.get()?.as_ref()
}

/// The first valid table with signature `sig`.
pub(crate) fn find(sig: [u8; 4]) -> Option<Sdt<'static>> {
    tables()?.find(sig)
}

/// Prints every table, hex-encoded, for `cargo xtask acpidump`
/// (`abyss.acpidump`): `acpidump: table SIG LEN`, then the bytes 48 to a
/// line, then `acpidump: end`; `acpidump: done` after the last table.
pub(crate) fn dump() {
    let Some(t) = tables() else {
        kprintln!("acpidump: no tables");
        return;
    };
    let emit = |sdt: Sdt<'_>| {
        let sig = sdt.signature();
        kprintln!(
            "acpidump: table {} {}",
            core::str::from_utf8(&sig).unwrap_or("????"),
            sdt.bytes().len()
        );
        for chunk in sdt.bytes().chunks(48) {
            let mut line = alloc::string::String::with_capacity(96);
            for b in chunk {
                use core::fmt::Write;
                let _ = write!(line, "{b:02x}");
            }
            kprintln!("acpidump: {line}");
        }
        kprintln!("acpidump: end");
    };
    for sdt in t.iter() {
        match sdt {
            Ok(sdt) => emit(sdt),
            Err(e) => kprintln!("acpidump: bad table: {e:?}"),
        }
    }
    // The DSDT is reached through the FADT, not the XSDT.
    let dsdt = find(*b"FACP")
        .and_then(|f| acpi_tables::fadt::parse(&f).ok())
        .and_then(|f| f.dsdt());
    if let Some(pa) = dsdt {
        match t.read(pa) {
            Ok(sdt) => emit(sdt),
            Err(e) => kprintln!("acpidump: bad DSDT: {e:?}"),
        }
    }
    kprintln!("acpidump: done");
}
