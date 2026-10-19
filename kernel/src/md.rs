// SPDX-License-Identifier: BSD-2-Clause
//! The memory disk: a ustar archive the loader preloaded (FreeBSD's
//! `/rescue`), read in place. `exec` loads programs from it until the VFS
//! (P4.5) mounts it as a file system.

use bootproto::BootInfo;
use sync::Once;
use ustar::{Archive, Entry, Kind};

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

/// The regular file at `path` (hard links followed).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // exec (P4.3)
pub(crate) fn file(path: &[u8]) -> Option<Entry<'static>> {
    archive()?
        .find(path)
        .ok()
        .flatten()
        .filter(|e| e.kind == Kind::File)
}

/// A memory-disk file as a pager: its pages come from the archive bytes.
struct FilePager {
    data: &'static [u8],
}

impl vm::object::Pager for FilePager {
    fn size(&self) -> u64 {
        (self.data.len() as u64).div_ceil(vm::PAGE_SIZE)
    }

    fn fill(&self, index: u64, page: &mut [u8]) {
        let start = usize::try_from(index * vm::PAGE_SIZE).unwrap_or(usize::MAX);
        let src = self.data.get(start..).unwrap_or(&[]);
        let n = src.len().min(page.len());
        page[..n].copy_from_slice(&src[..n]);
        page[n..].fill(0);
    }
}

/// Each file's object, by its data's address: one per file, so every
/// mapping of it shares the cached pages (FreeBSD's vnode object).
static OBJECTS: sync::SpinLock<
    alloc::collections::BTreeMap<usize, alloc::sync::Arc<vm::object::Object>>,
> = sync::SpinLock::new(alloc::collections::BTreeMap::new());

/// The memory object of file `e` (from [`file`]).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // exec (P4.3)
pub(crate) fn object(e: &Entry<'static>) -> alloc::sync::Arc<vm::object::Object> {
    let key = e.data.as_ptr() as usize;
    let mut objects = OBJECTS.lock();
    alloc::sync::Arc::clone(objects.entry(key).or_insert_with(|| {
        vm::object::Object::with_pager(
            crate::mm::space::page_ops(),
            alloc::sync::Arc::new(FilePager { data: e.data }),
        )
    }))
}
