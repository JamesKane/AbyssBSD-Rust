// SPDX-License-Identifier: BSD-2-Clause
//! One buffer segment: an mbuf of `MSIZE` bytes holding up to `MLEN`
//! bytes inline, or describing external storage (a cluster from one of the
//! zones, or caller-owned pages).
//!
//! A cluster is reference-counted so copies by reference share it, as
//! FreeBSD's do: the count lives in the mbuf the cluster was first
//! attached to (`EXT_FLAG_EMBREF`), and that mbuf outlives its own free
//! until the last reference goes. Shared data is read-only.
//!
//! Behavior reference: sys/kern/kern_mbuf.c (`mb_free_ext`),
//! sys/kern/uipc_mbuf.c (`mb_dupcl`), sys/sys/mbuf.h (`M_WRITABLE`,
//! `M_LEADINGSPACE`, `M_TRAILINGSPACE`).

use alloc::sync::Arc;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU32, Ordering, fence};

use crate::zone::{self, Zone};
use crate::{MCLBYTES, MJUM9BYTES, MJUM16BYTES, MJUMPAGESIZE, MLEN, MSIZE, NoBufs};

/// `m_flags` bits kept on a segment.
pub(crate) const M_EXT: u32 = 0x1;
pub(crate) const M_EOR: u32 = 0x4;
pub(crate) const M_RDONLY: u32 = 0x8;
pub(crate) const M_EXTPG: u32 = 0x100;

/// A cluster size, with its zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cluster {
    /// `MCLBYTES`.
    Std,
    /// `MJUMPAGESIZE`.
    Page,
    /// `MJUM9BYTES`.
    Jumbo9,
    /// `MJUM16BYTES`.
    Jumbo16,
}

impl Cluster {
    #[must_use]
    pub const fn size(self) -> u32 {
        match self {
            Self::Std => MCLBYTES,
            Self::Page => MJUMPAGESIZE,
            Self::Jumbo9 => MJUM9BYTES,
            Self::Jumbo16 => MJUM16BYTES,
        }
    }

    pub(crate) fn zone(self) -> &'static Zone {
        match self {
            Self::Std => &zone::CLUSTER,
            Self::Page => &zone::JUMBOP,
            Self::Jumbo9 => &zone::JUMBO9,
            Self::Jumbo16 => &zone::JUMBO16,
        }
    }

    /// The smallest that holds `len`, if one does.
    #[must_use]
    pub const fn fitting(len: usize) -> Option<Self> {
        Some(if len <= MCLBYTES as usize {
            Self::Std
        } else if len <= MJUMPAGESIZE as usize {
            Self::Page
        } else if len <= MJUM9BYTES as usize {
            Self::Jumbo9
        } else if len <= MJUM16BYTES as usize {
            Self::Jumbo16
        } else {
            return None;
        })
    }
}

/// Pages an mbuf refers to rather than owns (FreeBSD's `M_EXTPG`): page
/// cache pages for `sendfile`, a TLS record's. They stay while any mbuf
/// refers to them; their bytes are read-only.
pub trait Pages: Send + Sync {
    /// The bytes.
    fn bytes(&self) -> &[u8];
}

/// What external storage an mbuf has.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Cluster(Cluster),
    Pages,
}

/// `struct m_ext`, in the inline area of an mbuf with external storage.
#[repr(C)]
struct Ext {
    base: *mut u8,
    /// The mbuf holding the cluster's count (`EXT_FLAG_EMBREF`).
    mref: *mut Node,
    /// The pages' owner, for `Kind::Pages`.
    pages: Option<Arc<dyn Pages>>,
    size: u32,
    /// References to the cluster, in the `mref` mbuf only.
    refs: AtomicU32,
    kind: Kind,
}

#[repr(C)]
union Body {
    inline: [MaybeUninit<u8>; MLEN as usize],
    ext: ManuallyDrop<Ext>,
}

/// An mbuf as it lies in its zone's buffer: `m_next`, `m_data`, `m_len`,
/// `m_flags`, then the inline data or `m_ext`.
#[repr(C)]
pub(crate) struct Node {
    pub(crate) next: *mut Node,
    data: *mut u8,
    len: u32,
    flags: u32,
    _spare: u64,
    body: Body,
}

const _: () = assert!(size_of::<Node>() == MSIZE as usize);
const _: () = assert!(size_of::<Ext>() <= MLEN as usize);

/// One owned mbuf, not in a chain.
pub struct Mbuf(NonNull<Node>);

// SAFETY: an `Mbuf` owns its node; shared clusters are only read through
// it, and their count is atomic. `Pages` are `Send + Sync`.
unsafe impl Send for Mbuf {}
// SAFETY: `&Mbuf` only reads.
unsafe impl Sync for Mbuf {}

impl core::fmt::Debug for Mbuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mbuf")
            .field("len", &self.len())
            .field("ext", &self.ext_size())
            .field("eor", &self.eor())
            .finish()
    }
}

/// A fresh node from the mbuf zone, empty, with no storage set up.
fn node() -> Result<NonNull<Node>, NoBufs> {
    let p = zone::MBUF.alloc().ok_or(NoBufs)?.cast::<Node>();
    // SAFETY: a fresh `MSIZE` buffer, aligned for a `Node`; the header is
    // written before anything reads it.
    unsafe {
        let n = p.as_ptr();
        (&raw mut (*n).next).write(ptr::null_mut());
        (&raw mut (*n).len).write(0);
        (&raw mut (*n).flags).write(0);
        let inline = (&raw mut (*n).body.inline).cast::<u8>();
        (&raw mut (*n).data).write(inline);
    }
    Ok(p)
}

impl Mbuf {
    /// An empty mbuf with `MLEN` bytes inline (`m_get`).
    ///
    /// # Errors
    ///
    /// `NoBufs` at the mbuf zone's limit or out of memory.
    pub fn new() -> Result<Self, NoBufs> {
        node().map(Self)
    }

    /// An empty mbuf with a cluster (`m_getjcl`).
    ///
    /// # Errors
    ///
    /// `NoBufs` at either zone's limit or out of memory.
    pub fn with_cluster(c: Cluster) -> Result<Self, NoBufs> {
        let n = node()?;
        let Some(base) = c.zone().alloc() else {
            // SAFETY: just allocated from this zone, never shared.
            unsafe { zone::MBUF.free(n.cast()) };
            return Err(NoBufs);
        };
        let p = n.as_ptr();
        // SAFETY: a fresh node, ours alone.
        unsafe {
            (&raw mut (*p).body.ext).write(ManuallyDrop::new(Ext {
                base: base.as_ptr(),
                mref: p,
                pages: None,
                size: c.size(),
                refs: AtomicU32::new(1),
                kind: Kind::Cluster(c),
            }));
            (*p).data = base.as_ptr();
            (*p).flags = M_EXT;
        }
        Ok(Self(n))
    }

    /// An mbuf referring to `len` bytes of `pages` from `off`, read-only,
    /// charged as `size` bytes of storage (`m_get` with `M_EXTPG`).
    ///
    /// # Errors
    ///
    /// `NoBufs` at the mbuf zone's limit; nothing if the range is not in
    /// `pages` (it is cut to what is).
    pub fn with_pages(
        pages: Arc<dyn Pages>,
        off: usize,
        len: usize,
        size: u32,
    ) -> Result<Self, NoBufs> {
        let n = node()?;
        let all = pages.bytes();
        let off = off.min(all.len());
        let len = len.min(all.len() - off);
        let base = all.as_ptr().cast_mut();
        let p = n.as_ptr();
        // SAFETY: a fresh node; `base + off .. + len` lies in `pages`,
        // which the node keeps alive and never writes.
        unsafe {
            (*p).data = base.add(off);
            #[allow(clippy::cast_possible_truncation, reason = "segments are under 4 GiB")]
            {
                (*p).len = len as u32;
            }
            (&raw mut (*p).body.ext).write(ManuallyDrop::new(Ext {
                base,
                mref: p,
                pages: Some(pages),
                size,
                refs: AtomicU32::new(1),
                kind: Kind::Pages,
            }));
            (*p).flags = M_EXT | M_EXTPG | M_RDONLY;
        }
        Ok(Self(n))
    }

    pub(crate) fn node(&self) -> &Node {
        // SAFETY: we own a live node.
        unsafe { self.0.as_ref() }
    }

    pub(crate) fn into_raw(self) -> NonNull<Node> {
        ManuallyDrop::new(self).0
    }

    /// # Safety
    ///
    /// `n` is a live node no one else owns, with no successor.
    pub(crate) unsafe fn from_raw(n: NonNull<Node>) -> Self {
        Self(n)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.node().len as usize
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.node().len == 0
    }

    /// Its data.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        Node::bytes(self.node())
    }

    /// Its data, if writable (`M_WRITABLE`).
    pub fn bytes_mut(&mut self) -> Option<&mut [u8]> {
        if !self.writable() {
            return None;
        }
        let n = self.node();
        // SAFETY: writable means no one else sees the data; `data .. len`
        // is initialized storage of this mbuf.
        Some(unsafe { core::slice::from_raw_parts_mut(n.data, n.len as usize) })
    }

    /// Whether its data may be written: not read-only, and a cluster not
    /// shared (`M_WRITABLE`).
    #[must_use]
    pub fn writable(&self) -> bool {
        Node::writable(self.node())
    }

    /// Its external storage's size, 0 for none.
    #[must_use]
    pub fn ext_size(&self) -> u32 {
        Node::ext_size(self.node())
    }

    /// What a socket buffer charges for it (`sballoc`): `MSIZE`, plus its
    /// external storage.
    #[must_use]
    pub fn cost(&self) -> u32 {
        MSIZE + self.ext_size()
    }

    /// It ends a record (`M_EOR`).
    #[must_use]
    pub fn eor(&self) -> bool {
        self.node().flags & M_EOR != 0
    }

    pub fn set_eor(&mut self, on: bool) {
        let n = self.0.as_ptr();
        // SAFETY: we own the node.
        unsafe {
            if on {
                (*n).flags |= M_EOR;
            } else {
                (*n).flags &= !M_EOR;
            }
        }
    }

    /// Room before its data (`M_LEADINGSPACE`), 0 if not writable.
    #[must_use]
    pub fn leading_space(&self) -> usize {
        Node::leading(self.node())
    }

    /// Room after its data (`M_TRAILINGSPACE`), 0 if not writable.
    #[must_use]
    pub fn trailing_space(&self) -> usize {
        Node::trailing(self.node())
    }

    /// Moves an empty mbuf's data start `n` bytes in (`m_align`-style), to
    /// leave room for headers before it.
    pub fn reserve(&mut self, n: usize) {
        if self.is_empty() && n <= self.trailing_space() {
            // SAFETY: within its storage, per the check.
            unsafe {
                let p = self.0.as_ptr();
                (*p).data = (*p).data.add(n);
            }
        }
    }

    /// Copies as much of `src` as fits after its data; returns how much.
    pub fn append(&mut self, src: &[u8]) -> usize {
        self.fill(src.len(), |b| {
            b.copy_from_slice(&src[..b.len()]);
            Ok::<_, ()>(b.len())
        })
        .unwrap_or(0)
    }

    /// Lets `f` write up to `max` bytes after its data (as much as fits)
    /// and keeps what it says it wrote: a copy-in straight into the
    /// buffer, without clearing it first. Returns that count.
    ///
    /// # Errors
    ///
    /// What `f` returns; nothing is kept then.
    pub fn fill<E>(
        &mut self,
        max: usize,
        f: impl FnOnce(&mut [u8]) -> Result<usize, E>,
    ) -> Result<usize, E> {
        let room = self.trailing_space().min(max);
        if room == 0 {
            return Ok(0);
        }
        // The header is reached through the raw pointer only: inline data
        // lies in the same buffer.
        let n = self.0.as_ptr();
        // SAFETY: we own the node. `room` bytes after the data lie in this
        // mbuf's storage, which is writable (`trailing_space` is 0
        // otherwise) and seen by no one else. `f` only writes them; the
        // length grows only by what it says it wrote.
        let b = unsafe { core::slice::from_raw_parts_mut((*n).data.add((*n).len as usize), room) };
        let got = f(b)?.min(room);
        // SAFETY: as above.
        #[allow(clippy::cast_possible_truncation, reason = "at most a cluster")]
        unsafe {
            (*n).len += got as u32;
        }
        Ok(got)
    }

    /// Drops `n` bytes from the front (`m_adj` with `n > 0`).
    pub fn trim_front(&mut self, n: usize) {
        let p = self.0.as_ptr();
        // SAFETY: we own the node; the new start is within the data.
        #[allow(clippy::cast_possible_truncation, reason = "at most its length")]
        unsafe {
            let n = n.min((*p).len as usize);
            (*p).data = (*p).data.add(n);
            (*p).len -= n as u32;
        }
    }

    /// Drops `n` bytes from the back.
    pub fn trim_back(&mut self, n: usize) {
        let p = self.0.as_ptr();
        // SAFETY: we own the node.
        #[allow(clippy::cast_possible_truncation, reason = "at most its length")]
        unsafe {
            (*p).len -= n.min((*p).len as usize) as u32;
        }
    }

    /// Extends its data `n` bytes at the front into its leading space,
    /// returning them; `None` if there is not that much.
    pub fn prepend(&mut self, n: usize) -> Option<&mut [u8]> {
        if n > self.leading_space() {
            return None;
        }
        let p = self.0.as_ptr();
        // SAFETY: we own the node; the bytes are within its writable
        // storage, per the check.
        #[allow(clippy::cast_possible_truncation, reason = "at most its storage")]
        unsafe {
            (*p).data = (*p).data.sub(n);
            (*p).len += n as u32;
            Some(core::slice::from_raw_parts_mut((*p).data, n))
        }
    }

    /// `len` bytes of it from `off`, by reference (`m_copym` of one
    /// mbuf): a cluster or pages are shared, inline data copied.
    ///
    /// # Errors
    ///
    /// `NoBufs` at the mbuf zone's limit.
    pub fn share(&self, off: usize, len: usize) -> Result<Self, NoBufs> {
        let src = self.node();
        let off = off.min(src.len as usize);
        let len = len.min(src.len as usize - off);
        if src.flags & M_EXT == 0 {
            let mut m = Self::new()?;
            m.append(&self.bytes()[off..off + len]);
            return Ok(m);
        }
        let n = node()?;
        let p = n.as_ptr();
        // SAFETY: `src` has external storage, so its `ext` is the live
        // variant; its `mref` is alive while `src` refers to it. The new
        // node is ours.
        unsafe {
            let ext = &src.body.ext;
            let pages = ext.pages.clone();
            if pages.is_none() {
                ext_of(ext.mref).refs.fetch_add(1, Ordering::Relaxed);
            }
            (&raw mut (*p).body.ext).write(ManuallyDrop::new(Ext {
                base: ext.base,
                mref: ext.mref,
                pages,
                size: ext.size,
                refs: AtomicU32::new(0),
                kind: ext.kind,
            }));
            (*p).data = src.data.add(off);
            #[allow(clippy::cast_possible_truncation, reason = "at most its length")]
            {
                (*p).len = len as u32;
            }
            (*p).flags = src.flags & (M_EXT | M_EXTPG | M_RDONLY);
        }
        Ok(Self(n))
    }
}

impl Drop for Mbuf {
    fn drop(&mut self) {
        // SAFETY: we own the node, which has no successor.
        unsafe { free_node(self.0.as_ptr()) };
    }
}

impl Node {
    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: `data .. len` is initialized storage of this mbuf, kept
        // alive by it.
        unsafe { core::slice::from_raw_parts(self.data, self.len as usize) }
    }

    pub(crate) fn eor(&self) -> bool {
        self.flags & M_EOR != 0
    }

    fn writable(&self) -> bool {
        if self.flags & M_RDONLY != 0 {
            return false;
        }
        if self.flags & M_EXT == 0 {
            return true;
        }
        // SAFETY: an mbuf with external storage holds its `ext`, and its
        // `mref` lives while it refers to it.
        unsafe { ext_of(self.body.ext.mref).refs.load(Ordering::Acquire) == 1 }
    }

    pub(crate) fn ext_size(&self) -> u32 {
        if self.flags & M_EXT == 0 {
            return 0;
        }
        // SAFETY: as in `writable`.
        unsafe { self.body.ext.size }
    }

    /// The start and size of its storage.
    fn storage(&self) -> (*const u8, usize) {
        if self.flags & M_EXT == 0 {
            return ((&raw const self.body.inline).cast::<u8>(), MLEN as usize);
        }
        // SAFETY: as in `writable`.
        unsafe { (self.body.ext.base.cast_const(), self.body.ext.size as usize) }
    }

    fn leading(&self) -> usize {
        if !self.writable() {
            return 0;
        }
        let (base, _) = self.storage();
        self.data.addr() - base.addr()
    }

    fn trailing(&self) -> usize {
        if !self.writable() {
            return 0;
        }
        let (base, size) = self.storage();
        (base.addr() + size) - (self.data.addr() + self.len as usize)
    }

    /// Writes `b` after a node's data if it is writable and has room.
    ///
    /// # Safety
    ///
    /// `n` is a live node the caller owns, with no reference to it live.
    pub(crate) unsafe fn try_append(n: *mut Self, b: &[u8]) -> bool {
        // SAFETY: per the contract; the shared borrow ends here.
        if b.len() > unsafe { (*n).trailing() } {
            return false;
        }
        // SAFETY: room in its writable storage, per the check.
        #[allow(clippy::cast_possible_truncation, reason = "at most its storage")]
        unsafe {
            ptr::copy_nonoverlapping(b.as_ptr(), (*n).data.add((*n).len as usize), b.len());
            (*n).len += b.len() as u32;
        }
        true
    }
}

/// The `m_ext` of a node with external storage.
///
/// # Safety
///
/// `n` is live, with `M_EXT` set, for `'a`.
unsafe fn ext_of<'a>(n: *mut Node) -> &'a Ext {
    // SAFETY: per the contract, `ext` is the union's live variant;
    // `ManuallyDrop` is transparent.
    unsafe { &*(&raw const (*n).body.ext).cast::<Ext>() }
}

/// Frees one node and its storage reference (`m_free`, `mb_free_ext`).
///
/// # Safety
///
/// `n` is a live node owned by the caller, not used after.
pub(crate) unsafe fn free_node(n: *mut Node) {
    // SAFETY: per the contract; an mbuf with `M_EXT` holds its `ext`.
    unsafe {
        if (*n).flags & M_EXT == 0 {
            zone::MBUF.free(NonNull::new_unchecked(n.cast()));
            return;
        }
        let ext = &mut (*n).body.ext;
        if ext.kind == Kind::Pages {
            ManuallyDrop::drop(ext);
            zone::MBUF.free(NonNull::new_unchecked(n.cast()));
            return;
        }
        let mref = ext.mref;
        let refs = &ext_of(mref).refs;
        // The only reference needs no atomic decrement.
        let last = refs.load(Ordering::Acquire) == 1 || refs.fetch_sub(1, Ordering::Release) == 1;
        if last {
            fence(Ordering::Acquire);
            let Kind::Cluster(c) = ext_of(mref).kind else {
                return;
            };
            c.zone().free(NonNull::new_unchecked(ext_of(mref).base));
            if mref != n {
                zone::MBUF.free(NonNull::new_unchecked(mref.cast()));
            }
        }
        // The holder of the count stays until the last reference goes.
        if mref != n || last {
            zone::MBUF.free(NonNull::new_unchecked(n.cast()));
        }
    }
}
