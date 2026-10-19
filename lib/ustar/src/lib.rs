// SPDX-License-Identifier: BSD-2-Clause
//! Read-only access to POSIX ustar archives held in memory: the memory
//! disk the loader preloads (FreeBSD's `/rescue`, as `tar --format ustar`
//! writes it).
//!
//! An archive is a sequence of 512-byte headers, each followed by its
//! file's data padded to 512 bytes, and ends with two zero blocks. Fields
//! are NUL- or space-terminated octal. Hard links are entries of their own
//! that name an earlier entry; [`Archive::find`] follows them, so every
//! `/rescue` name finds the one binary.
//!
//! Malformed input is an error, never a panic: the image comes from a file
//! on the ESP.
//!
//! Behavior reference: the ustar format in POSIX.1-2008 (`pax`).

#![no_std]
#![forbid(unsafe_code)]

/// Header and data blocks are this long.
pub const BLOCK: usize = 512;

/// Why an archive could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A header or its data runs past the end of the archive.
    Truncated,
    /// A header's checksum does not match.
    BadChecksum { offset: usize },
    /// A header is not ustar (bad magic), or a numeric field is not octal.
    BadHeader { offset: usize },
    /// A hard link names nothing before it, or links form a chain longer
    /// than any archive needs.
    BadLink,
}

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    File,
    /// A hard link to the named (earlier) entry.
    HardLink(&'a [u8]),
    /// A symbolic link to the named path.
    Symlink(&'a [u8]),
    Directory,
    /// Devices, FIFOs, extended headers, and the rest.
    Other(u8),
}

/// One entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// The path, without a leading `./` or `/` and without a trailing `/`,
    /// as the two parts of a ustar name (`prefix`, then `name`).
    prefix: &'a [u8],
    name: &'a [u8],
    pub kind: Kind<'a>,
    /// Permission bits (`0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Seconds since the epoch.
    pub mtime: u64,
    /// The file's contents (empty for anything but a file).
    pub data: &'a [u8],
}

impl<'a> Entry<'a> {
    /// Whether the entry's path is `path` (normalized the same way).
    #[must_use]
    pub fn is(&self, path: &[u8]) -> bool {
        let path = normalize(path);
        if self.prefix.is_empty() {
            return self.name == path;
        }
        path.len() == self.prefix.len() + 1 + self.name.len()
            && path.starts_with(self.prefix)
            && path[self.prefix.len()] == b'/'
            && path.ends_with(self.name)
    }

    /// Calls `f` with the path's parts in order (`prefix`, `/`, `name`).
    pub fn path_parts(&self, mut f: impl FnMut(&'a [u8])) {
        if !self.prefix.is_empty() {
            f(self.prefix);
            f(b"/");
        }
        f(self.name);
    }
}

/// Strips a leading `./` or `/` (repeatedly) and a trailing `/`.
fn normalize(mut p: &[u8]) -> &[u8] {
    loop {
        if let Some(rest) = p.strip_prefix(b"./") {
            p = rest;
        } else if let Some(rest) = p.strip_prefix(b"/") {
            p = rest;
        } else {
            break;
        }
    }
    p.strip_suffix(b"/").unwrap_or(p)
}

/// The bytes of a NUL-terminated field.
fn text(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}

/// An octal field (spaces and NULs around the digits allowed).
fn octal(field: &[u8], offset: usize) -> Result<u64, Error> {
    let digits = text(field).trim_ascii();
    let mut v: u64 = 0;
    for &d in digits {
        if !(b'0'..=b'7').contains(&d) {
            return Err(Error::BadHeader { offset });
        }
        v = v
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(d - b'0')))
            .ok_or(Error::BadHeader { offset })?;
    }
    Ok(v)
}

fn small(v: u64, offset: usize) -> Result<u32, Error> {
    u32::try_from(v).map_err(|_| Error::BadHeader { offset })
}

/// An archive in memory.
#[derive(Clone, Copy, Debug)]
pub struct Archive<'a> {
    data: &'a [u8],
}

impl<'a> Archive<'a> {
    /// An archive over `data`; its headers are checked as they are read.
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    /// The entries, in archive order.
    #[must_use]
    pub const fn entries(&self) -> Entries<'a> {
        Entries {
            data: self.data,
            offset: 0,
            done: false,
        }
    }

    /// The entry at `path`, following hard links to the entry holding the
    /// data (the returned entry has the link's path but the target's kind
    /// and data). Symbolic links are not followed.
    pub fn find(&self, path: &[u8]) -> Result<Option<Entry<'a>>, Error> {
        let mut want = path;
        // A link names an earlier entry, so chains end; bound them anyway.
        for _ in 0..8 {
            let mut found = None;
            for e in self.entries() {
                let e = e?;
                if e.is(want) {
                    found = Some(e);
                    break;
                }
            }
            let Some(e) = found else {
                return if want == path {
                    Ok(None)
                } else {
                    Err(Error::BadLink)
                };
            };
            match e.kind {
                Kind::HardLink(target) => want = target,
                _ => return Ok(Some(e)),
            }
        }
        Err(Error::BadLink)
    }
}

/// An iterator over an archive's entries.
#[derive(Clone, Debug)]
pub struct Entries<'a> {
    data: &'a [u8],
    offset: usize,
    done: bool,
}

impl<'a> Entries<'a> {
    fn header(&mut self) -> Result<Option<Entry<'a>>, Error> {
        let at = self.offset;
        let h = self.data.get(at..at + BLOCK).ok_or(Error::Truncated)?;
        if h.iter().all(|&b| b == 0) {
            return Ok(None);
        }
        // The checksum counts the checksum field as spaces.
        let stored = octal(&h[148..156], at)?;
        let sum: u64 = h
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(b)
                }
            })
            .sum();
        if sum != stored {
            return Err(Error::BadChecksum { offset: at });
        }
        if &h[257..262] != b"ustar" {
            return Err(Error::BadHeader { offset: at });
        }
        let size = usize::try_from(octal(&h[124..136], at)?).map_err(|_| Error::Truncated)?;
        let data_at = at + BLOCK;
        let data = self
            .data
            .get(data_at..data_at.checked_add(size).ok_or(Error::Truncated)?)
            .ok_or(Error::Truncated)?;
        self.offset = data_at + size.div_ceil(BLOCK) * BLOCK;
        let link = text(&h[157..257]);
        let kind = match h[156] {
            b'0' | 0 => Kind::File,
            b'1' => Kind::HardLink(normalize(link)),
            b'2' => Kind::Symlink(link),
            b'5' => Kind::Directory,
            t => Kind::Other(t),
        };
        let mut prefix = normalize(text(&h[345..500]));
        let mut name = text(&h[0..100]);
        if prefix.is_empty() {
            name = normalize(name);
        } else {
            name = name.strip_suffix(b"/").unwrap_or(name);
        }
        if name.is_empty() {
            // `./` itself, or a prefix that holds the whole path.
            name = prefix;
            prefix = b"";
        }
        Ok(Some(Entry {
            prefix,
            name,
            kind,
            mode: small(octal(&h[100..108], at)?, at)? & 0o7777,
            uid: small(octal(&h[108..116], at)?, at)?,
            gid: small(octal(&h[116..124], at)?, at)?,
            mtime: octal(&h[136..148], at)?,
            data: if kind == Kind::File { data } else { &[] },
        }))
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.header() {
            Ok(Some(e)) => Some(Ok(e)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests;
