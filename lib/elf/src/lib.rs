// SPDX-License-Identifier: BSD-2-Clause
//! ELF64 little-endian parsing (System V gABI, with the x86_64 and AArch64
//! psABI relocation numbers).
//!
//! Everything reads from a byte slice with bounds checks; a malformed file
//! yields an [`Error`], never a panic. Used by the loader (program headers,
//! relocations, symbol sections), the kernel (symbolized backtraces), and
//! later `execve`.

#![no_std]
#![forbid(unsafe_code)]

mod sym;
pub mod vdso;

pub use sym::{Symbol, SymbolTable};

/// Why a file was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shorter than a structure it must contain.
    Truncated,
    /// Not an ELF file.
    BadMagic,
    /// Not ELF64 little-endian version 1.
    Unsupported,
    /// A header field is inconsistent (entry sizes, counts, alignment).
    Malformed(&'static str),
}

/// `e_machine` values we care about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Machine {
    /// `EM_X86_64`.
    X86_64,
    /// `EM_AARCH64`.
    Aarch64,
    /// Anything else.
    Other(u16),
}

impl Machine {
    const fn from_raw(v: u16) -> Self {
        match v {
            62 => Self::X86_64,
            183 => Self::Aarch64,
            v => Self::Other(v),
        }
    }

    /// The `e_machine` value.
    #[must_use]
    pub const fn raw(self) -> u16 {
        match self {
            Self::X86_64 => 62,
            Self::Aarch64 => 183,
            Self::Other(v) => v,
        }
    }
}

/// `e_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    /// `ET_EXEC`: fixed addresses.
    Executable,
    /// `ET_DYN`: position independent.
    Shared,
    /// Anything else.
    Other(u16),
}

/// Little-endian field reads with bounds checks.
pub(crate) fn read<const N: usize>(b: &[u8], off: usize) -> Result<[u8; N], Error> {
    let end = off.checked_add(N).ok_or(Error::Truncated)?;
    b.get(off..end)
        .and_then(|s| s.try_into().ok())
        .ok_or(Error::Truncated)
}
pub(crate) fn u16_at(b: &[u8], off: usize) -> Result<u16, Error> {
    read::<2>(b, off).map(u16::from_le_bytes)
}
pub(crate) fn u32_at(b: &[u8], off: usize) -> Result<u32, Error> {
    read::<4>(b, off).map(u32::from_le_bytes)
}
pub(crate) fn u64_at(b: &[u8], off: usize) -> Result<u64, Error> {
    read::<8>(b, off).map(u64::from_le_bytes)
}
pub(crate) fn usize_from(v: u64) -> Result<usize, Error> {
    usize::try_from(v).map_err(|_| Error::Truncated)
}

/// A parsed ELF64 file.
#[derive(Clone, Copy, Debug)]
pub struct Elf<'a> {
    data: &'a [u8],
    /// `e_type`.
    pub file_type: FileType,
    /// `e_machine`.
    pub machine: Machine,
    /// `e_entry`.
    pub entry: u64,
    /// `EI_OSABI`.
    pub os_abi: u8,
    /// `e_flags`.
    pub flags: u32,
    phoff: usize,
    phnum: usize,
    shoff: usize,
    shnum: usize,
    shstrndx: usize,
}

const PHDR_SIZE: usize = 56;
const SHDR_SIZE: usize = 64;

impl<'a> Elf<'a> {
    /// Parses and validates the file header.
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        Self::parse_with(data, true)
    }

    /// Parses the file header from the start of a file, for loading it:
    /// only the program header table must lie in `head`. The sections,
    /// which loading never reads and which usually sit at the end, are
    /// left out (as if there were none).
    pub fn parse_head(head: &'a [u8]) -> Result<Self, Error> {
        Self::parse_with(head, false)
    }

    fn parse_with(data: &'a [u8], sections: bool) -> Result<Self, Error> {
        if read::<4>(data, 0)? != *b"\x7fELF" {
            return Err(Error::BadMagic);
        }
        // EI_CLASS 2 (64-bit), EI_DATA 1 (LE), EI_VERSION 1.
        if read::<3>(data, 4)? != [2, 1, 1] || u32_at(data, 20)? != 1 {
            return Err(Error::Unsupported);
        }
        let file_type = match u16_at(data, 16)? {
            2 => FileType::Executable,
            3 => FileType::Shared,
            t => FileType::Other(t),
        };
        let phnum = usize::from(u16_at(data, 56)?);
        let shnum = if sections {
            usize::from(u16_at(data, 60)?)
        } else {
            0
        };
        if phnum > 0 && usize::from(u16_at(data, 54)?) != PHDR_SIZE {
            return Err(Error::Malformed("e_phentsize"));
        }
        if shnum > 0 && usize::from(u16_at(data, 58)?) != SHDR_SIZE {
            return Err(Error::Malformed("e_shentsize"));
        }
        let elf = Self {
            data,
            file_type,
            machine: Machine::from_raw(u16_at(data, 18)?),
            entry: u64_at(data, 24)?,
            os_abi: read::<1>(data, 7)?[0],
            flags: u32_at(data, 48)?,
            phoff: usize_from(u64_at(data, 32)?)?,
            phnum,
            shoff: if sections {
                usize_from(u64_at(data, 40)?)?
            } else {
                0
            },
            shnum,
            shstrndx: usize::from(u16_at(data, 62)?),
        };
        // Both tables must lie inside the file.
        elf.table_end(elf.phoff, elf.phnum, PHDR_SIZE)?;
        if sections {
            elf.table_end(elf.shoff, elf.shnum, SHDR_SIZE)?;
        }
        Ok(elf)
    }

    fn table_end(&self, off: usize, n: usize, size: usize) -> Result<usize, Error> {
        let end = n
            .checked_mul(size)
            .and_then(|l| l.checked_add(off))
            .ok_or(Error::Truncated)?;
        if end > self.data.len() {
            return Err(Error::Truncated);
        }
        Ok(end)
    }

    /// The program header table's file offset and entry count.
    #[must_use]
    pub const fn program_headers(&self) -> (u64, usize) {
        (self.phoff as u64, self.phnum)
    }

    /// The notes in a `PT_NOTE` segment (malformed ones end the list).
    pub fn notes(&self, s: &Segment) -> Result<Notes<'a>, Error> {
        Ok(Notes {
            data: self.segment_data(s)?,
        })
    }

    /// The whole file.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The program headers.
    pub fn segments(&self) -> impl Iterator<Item = Result<Segment, Error>> + 'a {
        let (data, phoff) = (self.data, self.phoff);
        (0..self.phnum).map(move |i| Segment::parse(data, phoff + i * PHDR_SIZE))
    }

    /// The section headers.
    pub fn sections(&self) -> impl Iterator<Item = Result<Section, Error>> + 'a {
        let (data, shoff) = (self.data, self.shoff);
        (0..self.shnum).map(move |i| Section::parse(data, shoff + i * SHDR_SIZE))
    }

    /// Section `i`.
    pub fn section(&self, i: usize) -> Result<Section, Error> {
        if i >= self.shnum {
            return Err(Error::Malformed("section index"));
        }
        Section::parse(self.data, self.shoff + i * SHDR_SIZE)
    }

    /// The name of `s`, from the section header string table.
    pub fn section_name(&self, s: &Section) -> Result<&'a [u8], Error> {
        let strtab = self.section(self.shstrndx)?;
        cstr(self.bytes(strtab.offset, strtab.size)?, s.name)
    }

    /// The section called `name`, if present.
    pub fn section_by_name(&self, name: &[u8]) -> Result<Option<Section>, Error> {
        for s in self.sections() {
            let s = s?;
            if self.section_name(&s)? == name {
                return Ok(Some(s));
            }
        }
        Ok(None)
    }

    /// The file bytes `[offset, offset + size)`.
    pub fn bytes(&self, offset: u64, size: u64) -> Result<&'a [u8], Error> {
        let start = usize_from(offset)?;
        let end = start
            .checked_add(usize_from(size)?)
            .ok_or(Error::Truncated)?;
        self.data.get(start..end).ok_or(Error::Truncated)
    }

    /// The contents of a segment's file image.
    pub fn segment_data(&self, s: &Segment) -> Result<&'a [u8], Error> {
        self.bytes(s.offset, s.file_size)
    }

    /// The `.symtab`/`.strtab` pair, if the file has one.
    pub fn symbol_table(&self) -> Result<Option<SymbolTable<'a>>, Error> {
        for s in self.sections() {
            let s = s?;
            if s.kind == SectionKind::SymbolTable {
                let strtab = self.section(usize_from(u64::from(s.link))?)?;
                return SymbolTable::new(
                    self.bytes(s.offset, s.size)?,
                    self.bytes(strtab.offset, strtab.size)?,
                )
                .map(Some);
            }
        }
        Ok(None)
    }

    /// The dynamic relocations, found through `PT_DYNAMIC` (`DT_RELA`,
    /// `DT_RELASZ`, `DT_RELAENT`). Empty if there is no dynamic segment.
    pub fn relocations(&self) -> Result<Relocations<'a>, Error> {
        let Some(dynamic) = self
            .segments()
            .find(|s| s.as_ref().is_ok_and(|s| s.kind == SegmentKind::Dynamic))
            .transpose()?
        else {
            return Ok(Relocations::EMPTY);
        };
        let table = self.segment_data(&dynamic)?;
        let (mut rela, mut relasz, mut relaent) = (None, 0, RELA_SIZE as u64);
        for entry in table.as_chunks::<16>().0 {
            let (tag, val) = (u64_at(entry, 0)?, u64_at(entry, 8)?);
            match tag {
                0 => break, // DT_NULL
                7 => rela = Some(val),
                8 => relasz = val,
                9 => relaent = val,
                _ => {}
            }
        }
        let Some(rela) = rela else {
            return Ok(Relocations::EMPTY);
        };
        if relaent != RELA_SIZE as u64 {
            return Err(Error::Malformed("DT_RELAENT"));
        }
        // DT_RELA is a virtual address; find the file bytes behind it.
        let off = self.vaddr_to_offset(rela, relasz)?;
        Ok(Relocations {
            data: self.bytes(off, relasz)?,
            machine: self.machine,
        })
    }

    /// The file offset holding `[vaddr, vaddr + len)`, which must lie in the
    /// file image of one `PT_LOAD` segment.
    pub fn vaddr_to_offset(&self, vaddr: u64, len: u64) -> Result<u64, Error> {
        for s in self.segments() {
            let s = s?;
            if s.kind != SegmentKind::Load || vaddr < s.vaddr {
                continue;
            }
            let rel = vaddr - s.vaddr;
            if rel.checked_add(len).is_some_and(|end| end <= s.file_size) {
                return Ok(s.offset + rel);
            }
        }
        Err(Error::Malformed("address outside loaded file image"))
    }
}

/// Reads the NUL-terminated string at `off` in `table`.
pub(crate) fn cstr(table: &[u8], off: u32) -> Result<&[u8], Error> {
    let rest = table
        .get(usize_from(u64::from(off))?..)
        .ok_or(Error::Truncated)?;
    let len = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or(Error::Malformed("unterminated string"))?;
    Ok(&rest[..len])
}

/// One ELF note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note<'a> {
    /// The owner, without its NUL.
    pub name: &'a [u8],
    /// `n_type`.
    pub kind: u32,
    pub desc: &'a [u8],
}

/// The notes of a segment.
#[derive(Clone, Debug)]
pub struct Notes<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for Notes<'a> {
    type Item = Note<'a>;

    fn next(&mut self) -> Option<Note<'a>> {
        let b = self.data;
        let namesz = usize::try_from(u32_at(b, 0).ok()?).ok()?;
        let descsz = usize::try_from(u32_at(b, 4).ok()?).ok()?;
        let kind = u32_at(b, 8).ok()?;
        // Name and descriptor are each padded to 4 bytes.
        let name_end = 12usize.checked_add(namesz)?;
        let desc_at = 12 + namesz.next_multiple_of(4);
        let desc_end = desc_at.checked_add(descsz)?;
        let name = b.get(12..name_end)?;
        let desc = b.get(desc_at..desc_end)?;
        self.data = b.get(desc_end.next_multiple_of(4).min(b.len())..)?;
        Some(Note {
            name: name.strip_suffix(&[0]).unwrap_or(name),
            kind,
            desc,
        })
    }
}

/// `p_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// `PT_LOAD`.
    Load,
    /// `PT_DYNAMIC`.
    Dynamic,
    /// `PT_INTERP`.
    Interp,
    /// `PT_NOTE`.
    Note,
    /// `PT_PHDR`.
    Phdr,
    /// `PT_TLS`.
    Tls,
    /// `PT_GNU_RELRO`.
    GnuRelro,
    /// `PT_GNU_STACK`.
    GnuStack,
    /// Anything else.
    Other(u32),
}

/// Segment permissions (`p_flags`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    /// `PF_R`.
    pub read: bool,
    /// `PF_W`.
    pub write: bool,
    /// `PF_X`.
    pub exec: bool,
}

/// One program header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// `p_type`.
    pub kind: SegmentKind,
    /// `p_flags`.
    pub perms: Perms,
    /// `p_offset`.
    pub offset: u64,
    /// `p_vaddr`.
    pub vaddr: u64,
    /// `p_filesz`.
    pub file_size: u64,
    /// `p_memsz`.
    pub mem_size: u64,
    /// `p_align`.
    pub align: u64,
}

impl Segment {
    fn parse(b: &[u8], off: usize) -> Result<Self, Error> {
        let flags = u32_at(b, off + 4)?;
        let s = Self {
            kind: match u32_at(b, off)? {
                1 => SegmentKind::Load,
                2 => SegmentKind::Dynamic,
                3 => SegmentKind::Interp,
                4 => SegmentKind::Note,
                6 => SegmentKind::Phdr,
                7 => SegmentKind::Tls,
                0x6474_e551 => SegmentKind::GnuStack,
                0x6474_e552 => SegmentKind::GnuRelro,
                t => SegmentKind::Other(t),
            },
            perms: Perms {
                read: flags & 4 != 0,
                write: flags & 2 != 0,
                exec: flags & 1 != 0,
            },
            offset: u64_at(b, off + 8)?,
            vaddr: u64_at(b, off + 16)?,
            file_size: u64_at(b, off + 32)?,
            mem_size: u64_at(b, off + 40)?,
            align: u64_at(b, off + 48)?,
        };
        if s.file_size > s.mem_size {
            return Err(Error::Malformed("p_filesz > p_memsz"));
        }
        if s.vaddr.checked_add(s.mem_size).is_none() {
            return Err(Error::Malformed("segment wraps"));
        }
        Ok(s)
    }
}

/// `sh_type` values we care about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    /// `SHT_NULL`.
    Null,
    /// `SHT_PROGBITS`.
    ProgBits,
    /// `SHT_SYMTAB`.
    SymbolTable,
    /// `SHT_STRTAB`.
    StringTable,
    /// `SHT_NOBITS`.
    NoBits,
    /// Anything else.
    Other(u32),
}

/// One section header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    /// Offset of the name in the section name table.
    pub name: u32,
    /// `sh_type`.
    pub kind: SectionKind,
    /// `sh_addr`.
    pub addr: u64,
    /// `sh_offset`.
    pub offset: u64,
    /// `sh_size`.
    pub size: u64,
    /// `sh_link`.
    pub link: u32,
}

impl Section {
    fn parse(b: &[u8], off: usize) -> Result<Self, Error> {
        Ok(Self {
            name: u32_at(b, off)?,
            kind: match u32_at(b, off + 4)? {
                0 => SectionKind::Null,
                1 => SectionKind::ProgBits,
                2 => SectionKind::SymbolTable,
                3 => SectionKind::StringTable,
                8 => SectionKind::NoBits,
                t => SectionKind::Other(t),
            },
            addr: u64_at(b, off + 16)?,
            offset: u64_at(b, off + 24)?,
            size: u64_at(b, off + 32)?,
            link: u32_at(b, off + 40)?,
        })
    }
}

const RELA_SIZE: usize = 24;

/// A relocation, decoded for the file's machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relocation {
    /// `R_*_NONE`: nothing to do.
    None,
    /// `R_X86_64_RELATIVE` / `R_AARCH64_RELATIVE`: store `base + addend` at
    /// `offset`, where `base` is the load bias.
    Relative {
        /// Link-time virtual address to patch.
        offset: u64,
        /// The addend.
        addend: i64,
    },
    /// Any other type (symbolic relocations a static kernel never has).
    Unsupported {
        /// The raw `ELF64_R_TYPE`.
        kind: u32,
        /// Link-time virtual address it applies to.
        offset: u64,
    },
}

/// The entries of a `SHT_RELA` table.
#[derive(Clone, Copy, Debug)]
pub struct Relocations<'a> {
    data: &'a [u8],
    machine: Machine,
}

impl<'a> Relocations<'a> {
    const EMPTY: Self = Self {
        data: &[],
        machine: Machine::Other(0),
    };

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.data.len() / RELA_SIZE
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The decoded entries.
    pub fn iter(&self) -> impl Iterator<Item = Relocation> + 'a {
        let machine = self.machine;
        self.data.as_chunks::<RELA_SIZE>().0.iter().map(move |e| {
            let [o, i, a] = [0, 8, 16].map(|at| {
                let mut b = [0; 8];
                b.copy_from_slice(&e[at..at + 8]);
                u64::from_le_bytes(b)
            });
            let kind = u32::try_from(i & 0xffff_ffff).unwrap_or(u32::MAX);
            let relative = match machine {
                Machine::X86_64 => 8,
                Machine::Aarch64 => 1027,
                Machine::Other(_) => u32::MAX,
            };
            match kind {
                0 => Relocation::None,
                k if k == relative => Relocation::Relative {
                    offset: o,
                    addend: i64::from_le_bytes(a.to_le_bytes()),
                },
                kind => Relocation::Unsupported { kind, offset: o },
            }
        })
    }
}

#[cfg(test)]
mod tests;
