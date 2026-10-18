// SPDX-License-Identifier: BSD-2-Clause
//! The persistent log ring (format: `lib/logring`).
//!
//! Its address is kept in an NV variable so the next boot finds it. If the
//! memory there still holds a valid ring, its contents (the previous boot's
//! kernel log) are saved to `EFI/ABYSS/PREVLOG.TXT` before the ring is
//! reformatted for this boot. For a new ring the loader picks the lowest
//! free memory above 16 MiB: firmware allocates top-down, so low memory is
//! the least likely to be reused before the loader runs again.

use addr::{PhysAddr, PhysRange};
use efi::{Allocate, File, Guid, Mode, SystemTable};
use logring::Ring;

use crate::memtype;

/// The AbyssBSD vendor GUID for loader variables.
const VENDOR: Guid = Guid::must_parse("6A3C2F1E-8B4D-4E7A-9C15-41B5A59D0B17");
const VARIABLE: [u16; 13] = efi::ucs2("AbyssLogRing");
const SIZE: usize = 256 * 1024;
const PAGES: usize = SIZE / 4096;
const SAVE_PATH: &str = "EFI/ABYSS/PREVLOG.TXT";

/// What happened, for the loader's console.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The previous boot's log was saved.
    Recovered(u64),
    /// No usable previous ring; a fresh one is in place.
    Fresh,
}

impl core::fmt::Display for Outcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Recovered(n) => write!(
                f,
                "saved {n} bytes of the previous boot's log to {SAVE_PATH}"
            ),
            Self::Fresh => f.write_str("no previous log"),
        }
    }
}

/// Sets up the ring for this boot, saving the previous log through `root`.
pub(crate) fn setup(st: &SystemTable, root: &File<'_>) -> Option<(bootproto::LogRing, Outcome)> {
    let mut var = [0u8; 8];
    let previous = st
        .get_variable(&VARIABLE, VENDOR, &mut var)
        .ok()
        .filter(|&n| n == 8)
        .map(|_| u64::from_le_bytes(var))
        .filter(|a| a % 4096 == 0 && *a != 0);

    if let Some(addr) = previous
        && st
            .allocate_pages(Allocate::At(addr), memtype::LOG_RING, PAGES)
            .is_ok()
    {
        let buf = region(addr);
        let (recovered, boots) = match Ring::open(buf) {
            Ok(ring) => {
                let (a, b) = ring.contents();
                let saved = save(root, a, b).then_some((a.len() + b.len()) as u64);
                (saved.unwrap_or(0), ring.boots() + 1)
            }
            Err(_) => (0, 1),
        };
        if Ring::format(region(addr), boots).is_ok() {
            let outcome = if recovered > 0 {
                Outcome::Recovered(recovered)
            } else {
                Outcome::Fresh
            };
            return Some((describe(addr, recovered, boots), outcome));
        }
    }

    let addr = quiet_spot(st)
        .and_then(|a| {
            st.allocate_pages(Allocate::At(a), memtype::LOG_RING, PAGES)
                .ok()
        })
        .or_else(|| {
            st.allocate_pages(Allocate::Anywhere, memtype::LOG_RING, PAGES)
                .ok()
        })?;
    Ring::format(region(addr), 1).ok()?;
    // Without the variable the next boot cannot find the ring; still use
    // it for this boot.
    let _ = st.set_variable(&VARIABLE, VENDOR, &addr.to_le_bytes());
    Some((describe(addr, 0, 1), Outcome::Fresh))
}

fn describe(addr: u64, recovered: u64, boots: u64) -> bootproto::LogRing {
    bootproto::LogRing {
        region: PhysRange::from_len(PhysAddr::new(addr), SIZE as u64).unwrap_or(PhysRange {
            start: PhysAddr::new(addr),
            end: PhysAddr::new(addr),
        }),
        recovered,
        boots,
    }
}

fn region(addr: u64) -> &'static mut [u8] {
    // SAFETY: the caller allocated `PAGES` pages at `addr` as log-ring
    // memory; the loader runs identity mapped and holds no other reference.
    unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, SIZE) }
}

/// Writes the two halves of the log to the ESP, replacing an older copy.
fn save(root: &File<'_>, a: &[u8], b: &[u8]) -> bool {
    if a.is_empty() && b.is_empty() {
        return false;
    }
    if let Ok(old) = root.open(SAVE_PATH, Mode::Create) {
        let _ = old.delete();
    }
    let Ok(f) = root.open(SAVE_PATH, Mode::Create) else {
        return false;
    };
    f.write(a).is_ok() && f.write(b).is_ok() && f.flush().is_ok()
}

/// The middle of the largest free region, 2 MiB aligned.
fn quiet_spot(st: &SystemTable) -> Option<u64> {
    let size = st.memory_map_size().ok()?;
    let mut buf = alloc::vec![0u8; size];
    let map = st.memory_map(&mut buf).ok()?;
    let largest = map
        .iter()
        .filter(|d| d.kind == efi::MemoryType::CONVENTIONAL)
        .max_by_key(|d| d.pages)?;
    let end = largest.start + largest.pages * 4096;
    let mid = addr::align_down(largest.start + largest.pages * 2048, 2 << 20);
    (mid >= largest.start && mid + SIZE as u64 <= end).then_some(mid)
}
