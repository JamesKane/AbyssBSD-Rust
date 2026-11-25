// SPDX-License-Identifier: BSD-2-Clause
//! Graphics execution manager buffers: a buffer is a VM object, a handle names it in one Direct Rendering Manager
//! file, and a fake file offset names it for `mmap`.
//!
//! Behavior reference: `docs/GRAPHICS.md` §4.1, §4.2.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use addr::PAGE_SIZE;
use vm::object::{Object, PageIndex, PageOps};

/// Where a buffer's pages live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// Ordinary pages from the system's memory, allocated when first
    /// touched: the buffers of a system on a chip whose display and
    /// graphics engines share the processor's memory (unified memory).
    System,
    /// A contiguous run of the system's memory, for hardware that cannot
    /// gather pages and has no address translation of its own.
    Contiguous,
    /// Memory on the device, reached through a window onto it: a discrete
    /// graphics card's own memory. Moves to system memory when shared.
    DeviceLocal,
}

/// How the processor's caches treat a buffer's pages in user and kernel
/// mappings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    WriteBack,
    WriteCombine,
    Uncached,
}

/// Where a buffer lives and how it is cached: what a driver decides per
/// buffer, and what the core then does for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub backing: Backing,
    pub cache: CacheMode,
    /// Whether the devices that use the buffer see what the processor
    /// wrote without cache maintenance. Not on a chip whose display or
    /// graphics engine does not snoop the processor's caches: the core
    /// then asks the driver to clean them before the device reads
    /// ([`crate::DrmDriver::sync_for_device`]).
    pub coherent: bool,
}

impl Placement {
    /// System pages, cached, that devices see coherently: the usual case
    /// on a processor whose devices snoop its caches.
    pub const SYSTEM_COHERENT: Self = Self {
        backing: Backing::System,
        cache: CacheMode::WriteBack,
        coherent: true,
    };

    /// System pages, cached, that a device which does not snoop reads
    /// only after the processor's caches are cleaned.
    pub const SYSTEM_NONCOHERENT: Self = Self {
        backing: Backing::System,
        cache: CacheMode::WriteBack,
        coherent: false,
    };
}

/// The first fake offset, in pages (`DRM_FILE_PAGE_OFFSET_START`: 4 GiB,
/// above any offset a driver might give a register window).
pub const OFFSET_START: PageIndex = 0x1_0000_0000 / PAGE_SIZE;

/// The most handles one file may hold.
pub const MAX_HANDLES: usize = 1 << 20;

/// A buffer object: pages in a VM object, at a fake offset of its own.
///
/// The object's pages sit at indices `offset .. offset + pages`, so a
/// `mmap` of the Direct Rendering Manager file at the buffer's fake offset reaches them with
/// no translation.
#[derive(Debug)]
pub struct Gem {
    object: Arc<Object>,
    /// Size in bytes: a whole number of pages.
    size: u64,
    placement: Placement,
    /// The fake file offset of its first page, in pages.
    offset: PageIndex,
    /// The fences of the jobs using it.
    resv: sync::SpinLock<crate::resv::Reservation>,
}

impl Gem {
    /// A buffer of at least `size` bytes of system memory, populated at
    /// first touch, mapped at page `offset` of the file.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a zero size.
    pub fn new(
        pages: &'static dyn PageOps,
        size: u64,
        offset: PageIndex,
        placement: Placement,
    ) -> Result<Arc<Self>, Errno> {
        // Only cached system pages are built so far; the other
        // placements come with the virtual memory support they need
        // (cache attributes on user mappings, device windows).
        if placement.backing != Backing::System || placement.cache != CacheMode::WriteBack {
            return Err(Errno::EOPNOTSUPP);
        }
        if size == 0 {
            return Err(Errno::EINVAL);
        }
        let size = size
            .checked_next_multiple_of(PAGE_SIZE)
            .ok_or(Errno::EINVAL)?;
        let object = Object::anonymous(pages);
        // Several processes will map it, and none of them privately.
        object.set_shared();
        Ok(Arc::new(Self {
            object,
            size,
            placement,
            offset,
            resv: sync::SpinLock::new(crate::resv::Reservation::default()),
        }))
    }

    /// Its reservation: the fences of the jobs that use it.
    #[must_use]
    pub fn reservation(&self) -> &sync::SpinLock<crate::resv::Reservation> {
        &self.resv
    }

    /// Where it lives and how it is cached.
    #[must_use]
    pub fn placement(&self) -> Placement {
        self.placement
    }

    /// Its size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Its fake file offset in bytes.
    #[must_use]
    pub fn mmap_offset(&self) -> u64 {
        self.offset * PAGE_SIZE
    }

    /// Its pages.
    #[must_use]
    pub fn pages(&self) -> u64 {
        self.size / PAGE_SIZE
    }

    /// The object, for a mapping of `pages` pages from file page `page`:
    /// `EINVAL` unless they lie inside the buffer.
    ///
    /// # Errors
    ///
    /// `EINVAL` when the range leaves the buffer.
    pub fn map_range(&self, page: PageIndex, pages: u64) -> Result<Arc<Object>, Errno> {
        let end = page.checked_add(pages).ok_or(Errno::EINVAL)?;
        if page < self.offset || end > self.offset + self.pages() {
            return Err(Errno::EINVAL);
        }
        Ok(Arc::clone(&self.object))
    }

    /// Its memory for a mapping of `pages` pages from page `page` of
    /// the buffer (page 0 is its first, as a dma-buf descriptor counts):
    /// the object, and the object page the mapping starts at.
    ///
    /// # Errors
    ///
    /// `EINVAL` when the range leaves the buffer.
    pub fn mapping(&self, page: u64, pages: u64) -> Result<(Arc<Object>, PageIndex), Errno> {
        let end = page.checked_add(pages).ok_or(Errno::EINVAL)?;
        if end > self.pages() {
            return Err(Errno::EINVAL);
        }
        Ok((Arc::clone(&self.object), self.offset + page))
    }

    /// The physical frames of every page, populating those not yet
    /// touched: what a device that reads the buffer by itself needs
    /// (scanout). The pages stay as long as the buffer does; nothing
    /// reclaims anonymous pages yet, so no pin is needed, and one is owed
    /// when swapping comes.
    ///
    /// # Errors
    ///
    /// `ENOMEM` when a page cannot be had.
    pub fn frames(&self) -> Result<alloc::vec::Vec<vm::Pfn>, Errno> {
        (0..self.pages())
            .map(|i| {
                self.object
                    .page_for_write(self.offset + i)
                    .map_err(|_| Errno::ENOMEM)
            })
            .collect()
    }

    /// Calls `f` with page `index` (0 is the first) of the buffer,
    /// populating it if nothing touched it yet.
    ///
    /// # Errors
    ///
    /// `EINVAL` past the end; `ENOMEM` when no page is free.
    pub fn with_page(&self, index: u64, f: &mut dyn FnMut(&mut [u8])) -> Result<(), Errno> {
        if index >= self.pages() {
            return Err(Errno::EINVAL);
        }
        let pfn = self
            .object
            .page_for_write(self.offset + index)
            .map_err(|_| Errno::ENOMEM)?;
        self.object.access(pfn, f);
        Ok(())
    }
}

/// A file's handles: small integers from 1 up, lowest free first
/// (`idr_alloc`), each naming a buffer.
#[derive(Debug, Default)]
pub struct HandleTable {
    slots: Vec<Option<Arc<Gem>>>,
}

impl HandleTable {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// Names `gem`; the handle is 1 or more.
    ///
    /// # Errors
    ///
    /// `ENOSPC` when the file holds [`MAX_HANDLES`]; `ENOMEM`.
    pub fn insert(&mut self, gem: Arc<Gem>) -> Result<u32, Errno> {
        let i = match self.slots.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                if self.slots.len() >= MAX_HANDLES {
                    return Err(Errno::ENOSPC);
                }
                self.slots.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.slots[i] = Some(gem);
        u32::try_from(i + 1).map_err(|_| Errno::ENOSPC)
    }

    /// The buffer `handle` names.
    #[must_use]
    pub fn get(&self, handle: u32) -> Option<&Arc<Gem>> {
        let i = usize::try_from(handle).ok()?.checked_sub(1)?;
        self.slots.get(i)?.as_ref()
    }

    /// Drops `handle` (`GEM_CLOSE`); `None` if it names nothing.
    pub fn remove(&mut self, handle: u32) -> Option<Arc<Gem>> {
        let i = usize::try_from(handle).ok()?.checked_sub(1)?;
        let g = self.slots.get_mut(i)?.take();
        while self.slots.last().is_some_and(Option::is_none) {
            self.slots.pop();
        }
        g
    }

    /// The handle that names `gem` in this file, if any: importing a
    /// buffer the file already has gives back the same handle.
    #[must_use]
    pub fn find(&self, gem: &Arc<Gem>) -> Option<u32> {
        let i = self
            .slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|g| Arc::ptr_eq(g, gem)))?;
        u32::try_from(i + 1).ok()
    }

    /// The buffer whose fake offset range holds file page `page`.
    #[must_use]
    pub fn by_offset(&self, page: PageIndex) -> Option<&Arc<Gem>> {
        self.slots
            .iter()
            .flatten()
            .find(|g| page >= g.offset && page < g.offset + g.pages())
    }

    /// How many handles are in use.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Whether no handle is in use.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Hands out fake file offsets: a bump allocator over page indices,
/// which are never reused (a stale mapping can never reach a new buffer).
#[derive(Debug)]
pub struct OffsetSpace {
    next: PageIndex,
}

impl OffsetSpace {
    #[must_use]
    pub const fn new() -> Self {
        Self { next: OFFSET_START }
    }

    /// `pages` pages of offset space, from the returned page index.
    ///
    /// # Errors
    ///
    /// `ENOSPC` when the space past the largest file offset runs out.
    pub fn alloc(&mut self, pages: u64) -> Result<PageIndex, Errno> {
        let start = self.next;
        let end = start.checked_add(pages).ok_or(Errno::ENOSPC)?;
        if end > (i64::MAX.cast_unsigned() / PAGE_SIZE) {
            return Err(Errno::ENOSPC);
        }
        self.next = end;
        Ok(start)
    }
}

impl Default for OffsetSpace {
    fn default() -> Self {
        Self::new()
    }
}
