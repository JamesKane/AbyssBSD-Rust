// SPDX-License-Identifier: BSD-2-Clause
//! The loader's handoff, validated once and then read through the DMAP.

use addr::PhysAddr;
use bootproto::layout::dmap;
use bootproto::{BootInfo, HandoffError, MemoryRegion, Table};
use sync::SpinLock;

static INFO: SpinLock<Option<&'static BootInfo>> = SpinLock::new(None);

/// Validates the handoff at `phys` and keeps it.
pub(crate) fn take(phys: PhysAddr) -> Result<&'static BootInfo, HandoffError> {
    let va = dmap(phys).ok_or(HandoffError::BadMagic(0))?;
    let p = va.get() as *const BootInfo;
    // SAFETY: the loader maps the handoff (boot data, RAM) in the DMAP; the
    // header is read on its own first so that a foreign layout is rejected
    // before the rest is interpreted.
    let header = unsafe { (&raw const (*p).header).read() };
    header.validate()?;
    // SAFETY: same loader version and layout (checked above); boot data is
    // never reclaimed while the kernel uses it in Phase 1.
    let info = unsafe { &*p };
    *INFO.lock() = Some(info);
    Ok(info)
}

/// The handoff. Panics if called before [`take`] succeeded.
pub(crate) fn info() -> &'static BootInfo {
    INFO.lock().expect("boot info used before take()")
}

/// A boot-data table as a slice, through the DMAP.
pub(crate) fn slice<T>(t: Table<T>) -> &'static [T] {
    let Some(va) = dmap(t.base) else {
        return &[];
    };
    let Ok(len) = usize::try_from(t.len) else {
        return &[];
    };
    if len == 0 {
        return &[];
    }
    // SAFETY: the loader allocated `len` elements of `T` at `base` as boot
    // data, mapped in the DMAP and not reclaimed in Phase 1.
    unsafe { core::slice::from_raw_parts(va.get() as *const T, len) }
}

/// Bytes of physical memory in the DMAP.
pub(crate) fn phys_bytes(r: addr::PhysRange) -> &'static [u8] {
    let (Some(va), Ok(len)) = (dmap(r.start), usize::try_from(r.len())) else {
        return &[];
    };
    // SAFETY: callers pass boot-data ranges the loader described.
    unsafe { core::slice::from_raw_parts(va.get() as *const u8, len) }
}

/// The physical memory map.
pub(crate) fn memory_map() -> &'static [MemoryRegion] {
    slice(info().memory_map)
}

/// The kernel command line.
pub(crate) fn cmdline() -> &'static str {
    core::str::from_utf8(slice(info().cmdline)).unwrap_or("")
}

/// Whether `word` (or `word=1`) appears on the command line.
pub(crate) fn flag(word: &str) -> bool {
    cmdline()
        .split_ascii_whitespace()
        .any(|w| w == word || w.strip_prefix(word).and_then(|r| r.strip_prefix('=')) == Some("1"))
}
