// SPDX-License-Identifier: BSD-2-Clause
//! The firmware image (specification 05 §5.2): the header and its entries, checked as the
//! specification requires, giving the sections to load into the MCU's address space.
//!
//! The image is untrusted input: every offset and length is checked, and a malformed image is an
//! error, never a panic.

/// The image's path in the firmware store for a GPU of `arch_major.arch_minor` (05 §5.1).
pub fn path(arch_major: u8, arch_minor: u8, out: &mut [u8; 64]) -> usize {
    use core::fmt::Write;
    struct Buf<'a>(&'a mut [u8; 64], usize);
    impl Write for Buf<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.1 + s.len();
            if end > self.0.len() {
                return Err(core::fmt::Error);
            }
            self.0[self.1..end].copy_from_slice(s.as_bytes());
            self.1 = end;
            Ok(())
        }
    }
    let mut b = Buf(out, 0);
    let _ = write!(b, "arm/mali/arch{arch_major}.{arch_minor}/mali_csffw.bin");
    b.1
}

const MAGIC: u32 = 0xC3F1_3A6E;
const HEADER: usize = 0x14;
/// The MCU address of the shared interface section (05 §5.4).
pub const SHARED_VA: u32 = 0x0400_0000;
/// The MCU's page size.
pub const PAGE: u32 = 0x1000;

// Section flags (05 §5.2.3).
pub const FLAG_RD: u32 = 1 << 0;
pub const FLAG_WR: u32 = 1 << 1;
pub const FLAG_EX: u32 = 1 << 2;
pub const FLAG_PROT: u32 = 1 << 5;
pub const FLAG_SHARED: u32 = 1 << 30;
pub const FLAG_ZERO: u32 = 1 << 31;
const CACHE_SHIFT: u32 = 3;
const FLAGS_KNOWN: u32 =
    FLAG_RD | FLAG_WR | FLAG_EX | 3 << CACHE_SHIFT | FLAG_PROT | FLAG_SHARED | FLAG_ZERO;

/// Why an image is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Short,
    Magic,
    HeaderVersion(u8),
    Size,
    /// An entry at this file offset is malformed (what was wrong).
    Entry(usize, &'static str),
    /// An entry type the parser does not know and may not skip.
    UnknownEntry(u8),
    NoSharedSection,
}

/// A section's cache mode (05 §5.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    None,
    Cached,
    UncachedCoherent,
    CachedCoherent,
}

/// A section to load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub flags: u32,
    pub va_start: u32,
    pub va_end: u32,
    /// The file range of its initial contents.
    pub data_start: u32,
    pub data_end: u32,
}

impl Section {
    pub fn size(&self) -> u32 {
        self.va_end - self.va_start
    }
    pub fn writable(&self) -> bool {
        self.flags & FLAG_WR != 0
    }
    pub fn executable(&self) -> bool {
        self.flags & FLAG_EX != 0
    }
    pub fn shared(&self) -> bool {
        self.flags & FLAG_SHARED != 0
    }
    pub fn zero(&self) -> bool {
        self.flags & FLAG_ZERO != 0
    }
    pub fn cache(&self) -> CacheMode {
        match self.flags >> CACHE_SHIFT & 3 {
            0 => CacheMode::None,
            1 => CacheMode::Cached,
            2 => CacheMode::UncachedCoherent,
            _ => CacheMode::CachedCoherent,
        }
    }
    /// Whether the MCU maps it cacheable (05 §5.3.3: only mode 1; the coherent modes are mapped
    /// non-cacheable).
    pub fn cacheable(&self) -> bool {
        self.cache() == CacheMode::Cached
    }
}

/// What the parse found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image<const N: usize> {
    /// The firmware version hash in the header (informational).
    pub version_hash: u32,
    pub sections: [Option<Section>; N],
    pub count: usize,
    /// Protected-mode sections skipped (05 §5.2.3 rule 7).
    pub skipped_protected: usize,
    /// The build-info entry's revision, as a file range (without the `git_sha: ` prefix and the
    /// final NUL), if it is well formed.
    pub git_sha: Option<(usize, usize)>,
}

fn word(f: &[u8], at: usize) -> Option<u32> {
    let b = f.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Parses `file`, keeping at most `N` sections.
pub fn parse<const N: usize>(file: &[u8]) -> Result<Image<N>, Error> {
    if file.len() < HEADER {
        return Err(Error::Short);
    }
    let w = |at| word(file, at).ok_or(Error::Short);
    if w(0)? != MAGIC {
        return Err(Error::Magic);
    }
    let major = file[5];
    if major != 0 {
        return Err(Error::HeaderVersion(major));
    }
    let size = w(0x10)? as usize;
    if size > file.len() || size < HEADER {
        return Err(Error::Size);
    }
    let mut img = Image {
        version_hash: w(0x08)?,
        sections: [None; N],
        count: 0,
        skipped_protected: 0,
        git_sha: None,
    };
    let mut at = HEADER;
    while at < size {
        let hdr = word(file, at).ok_or(Error::Entry(at, "header past the image"))?;
        let ty = (hdr & 0xFF) as u8;
        let len = (hdr >> 8 & 0xFF) as usize;
        let optional = hdr & 1 << 31 != 0;
        if len < 4 || !len.is_multiple_of(4) {
            return Err(Error::Entry(at, "entry size"));
        }
        if at + len > size {
            return Err(Error::Entry(at, "entry past the image"));
        }
        let payload = &file[at + 4..at + len];
        match ty {
            0 => {
                if let Some(s) = section(file, at, payload)? {
                    if s.flags & FLAG_PROT != 0 {
                        img.skipped_protected += 1;
                    } else if s.va_end != s.va_start {
                        let slot = img
                            .sections
                            .get_mut(img.count)
                            .ok_or(Error::Entry(at, "too many sections"))?;
                        *slot = Some(s);
                        img.count += 1;
                    }
                }
            }
            1..=4 => {}
            6 => img.git_sha = build_info(file, payload),
            _ if optional => {}
            _ => return Err(Error::UnknownEntry(ty)),
        }
        at += len;
    }
    let shared = img
        .sections
        .iter()
        .flatten()
        .filter(|s| s.va_start == SHARED_VA)
        .count();
    if shared != 1 {
        return Err(Error::NoSharedSection);
    }
    Ok(img)
}

/// A type-0 entry (05 §5.2.3), validated in the specification's order.
fn section(file: &[u8], at: usize, p: &[u8]) -> Result<Option<Section>, Error> {
    let e = |why| Error::Entry(at, why);
    if p.len() < 20 {
        return Err(e("section entry too short"));
    }
    let w = |k: usize| u32::from_le_bytes([p[k], p[k + 1], p[k + 2], p[k + 3]]);
    let s = Section {
        flags: w(0),
        va_start: w(4),
        va_end: w(8),
        data_start: w(12),
        data_end: w(16),
    };
    if s.data_end < s.data_start {
        return Err(e("data end before start"));
    }
    if s.va_end < s.va_start {
        return Err(e("address end before start"));
    }
    if s.data_end as usize > file.len() {
        return Err(e("data past the file"));
    }
    if !s.va_start.is_multiple_of(PAGE) || !s.va_end.is_multiple_of(PAGE) {
        return Err(e("addresses not page aligned"));
    }
    if s.flags & !FLAGS_KNOWN != 0 {
        return Err(e("unknown section flags"));
    }
    if s.flags & FLAG_PROT != 0 {
        return Ok(Some(s));
    }
    if s.va_start == SHARED_VA && !s.shared() {
        return Err(e("the interface section is not shared"));
    }
    if s.data_end - s.data_start > s.va_end - s.va_start {
        return Err(e("more data than the section holds"));
    }
    Ok(Some(s))
}

/// A type-6 entry (05 §5.2.4): never fatal.
fn build_info(file: &[u8], p: &[u8]) -> Option<(usize, usize)> {
    let start = u32::from_le_bytes(p.get(0..4)?.try_into().ok()?) as usize;
    let len = u32::from_le_bytes(p.get(4..8)?.try_into().ok()?) as usize;
    let blob = file.get(start..start.checked_add(len)?)?;
    const PREFIX: &[u8] = b"git_sha: ";
    if len <= PREFIX.len() || !blob.starts_with(PREFIX) || blob[len - 1] != 0 {
        return None;
    }
    Some((start + PREFIX.len(), start + len - 1))
}
