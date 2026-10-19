// SPDX-License-Identifier: BSD-2-Clause
//! The memory disk: a ustar archive the loader preloaded (FreeBSD's
//! `/rescue`), read in place. [`crate::fs`] mounts it as the root.

use bootproto::BootInfo;
use sync::Once;
use ustar::{Archive, Kind};

static MD: Once<&'static [u8]> = Once::new();

/// Picks up the loader's memory disk, if it passed one.
pub(crate) fn init(info: &BootInfo) {
    let Some(r) = info.md.get() else {
        kprintln!("md: none");
        return;
    };
    let bytes = crate::boot::phys_bytes(r);
    let mut files = 0;
    for e in Archive::new(bytes).entries() {
        match e {
            Ok(e) if e.kind == Kind::File || matches!(e.kind, Kind::HardLink(_)) => files += 1,
            Ok(_) => {}
            Err(err) => {
                kprintln!("md: unreadable ({err:?}); ignored");
                return;
            }
        }
    }
    kprintln!("md: {} KiB, {files} files", bytes.len() / 1024);
    let _ = MD.set(bytes);
}

/// The archive, if there is a usable one.
pub(crate) fn archive() -> Option<Archive<'static>> {
    MD.get().map(|b| Archive::new(b))
}
