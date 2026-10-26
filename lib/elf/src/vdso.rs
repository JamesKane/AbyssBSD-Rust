// SPDX-License-Identifier: BSD-2-Clause
//! Writes a vdso: the small shared object a kernel puts in its shared
//! page for the run-time linker to preload (`AT_KPRELOAD`). It holds one
//! versioned function, the signal trampoline, and the call frame
//! information that lets unwinders and debuggers step through a signal
//! frame.
//!
//! The image has FreeBSD's shape (sys/tools/amd64_vdso.sh and
//! sys/conf/vdso_amd64.ldscript): one read-and-execute `PT_LOAD` from the
//! file header on, `PT_DYNAMIC` and `PT_GNU_EH_FRAME` inside it, a System
//! V hash table, a version definition per symbol, no relocations (rtld
//! does not relocate a preloaded object), and section headers past the
//! loaded part for tools that read them.
//!
//! The sections, in order: `.hash`, `.dynsym`, `.dynstr`, `.gnu.version`,
//! `.gnu.version_d`, `.eh_frame_hdr`, `.eh_frame`, `.dynamic`, `.text`,
//! then `.shstrtab`.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "an image is at most 64 KiB (checked), so its offsets fit any field; \
              LEB128 takes seven bits at a time"
)]

use crate::{Error, Machine};

/// DWARF call frame instructions (DWARF 5 §6.4.2), built up in place.
#[derive(Clone, Debug)]
pub struct Cfa {
    buf: [u8; 128],
    len: usize,
    data_align: i8,
    overflow: bool,
}

const DW_CFA_ADVANCE_LOC: u8 = 0x40;
const DW_CFA_OFFSET: u8 = 0x80;
const DW_CFA_DEF_CFA: u8 = 0x0c;
const DW_CFA_OFFSET_EXTENDED_SF: u8 = 0x11;
const DW_CFA_NOP: u8 = 0;

impl Cfa {
    /// Empty, for a CIE whose data alignment factor is `data_align`.
    #[must_use]
    pub const fn new(data_align: i8) -> Self {
        Self {
            buf: [0; 128],
            len: 0,
            data_align,
            overflow: false,
        }
    }

    fn byte(&mut self, b: u8) {
        match self.buf.get_mut(self.len) {
            Some(slot) => {
                *slot = b;
                self.len += 1;
            }
            None => self.overflow = true,
        }
    }

    fn uleb(&mut self, mut v: u64) {
        loop {
            let low = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.byte(low);
                return;
            }
            self.byte(low | 0x80);
        }
    }

    fn sleb(&mut self, mut v: i64) {
        loop {
            let low = (v & 0x7f) as u8;
            v >>= 7;
            if (v == 0 && low & 0x40 == 0) || (v == -1 && low & 0x40 != 0) {
                self.byte(low);
                return;
            }
            self.byte(low | 0x80);
        }
    }

    /// The CFA is `reg` plus `offset`.
    pub fn def_cfa(&mut self, reg: u8, offset: u64) -> &mut Self {
        self.byte(DW_CFA_DEF_CFA);
        self.uleb(u64::from(reg));
        self.uleb(offset);
        self
    }

    /// `reg` is saved at the CFA plus `offset` bytes, which must be a
    /// multiple of the data alignment factor.
    pub fn offset(&mut self, reg: u8, offset: i64) -> &mut Self {
        let align = i64::from(self.data_align);
        if align == 0 || offset % align != 0 {
            self.overflow = true;
            return self;
        }
        let factored = offset / align;
        if factored >= 0 && reg < 0x40 {
            self.byte(DW_CFA_OFFSET | reg);
            self.uleb(factored.unsigned_abs());
        } else {
            self.byte(DW_CFA_OFFSET_EXTENDED_SF);
            self.uleb(u64::from(reg));
            self.sleb(factored);
        }
        self
    }

    /// The rules that follow apply from `delta` bytes further into the
    /// code (the code alignment factor is 1); at most 63.
    pub fn advance(&mut self, delta: u8) -> &mut Self {
        if delta >= 0x40 {
            self.overflow = true;
        } else {
            self.byte(DW_CFA_ADVANCE_LOC | delta);
        }
        self
    }

    fn bytes(&self) -> Result<&[u8], Error> {
        if self.overflow {
            return Err(Error::Malformed("call frame instructions"));
        }
        Ok(&self.buf[..self.len])
    }
}

/// What a vdso holds.
#[derive(Clone, Debug)]
pub struct Vdso<'a> {
    pub machine: Machine,
    /// `DT_SONAME`, also the base version's name.
    pub soname: &'a [u8],
    /// The version `symbol` is defined at.
    pub version: &'a [u8],
    /// The one function, which is all of `code`.
    pub symbol: &'a [u8],
    pub code: &'a [u8],
    /// The CIE's return address column.
    pub return_column: u8,
    /// The CIE's initial instructions (their data alignment factor is
    /// the CIE's).
    pub cie: &'a Cfa,
    /// The FDE's instructions, over all of `code`.
    pub fde: &'a Cfa,
}

/// Where things landed in a written vdso.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// Bytes written, section headers included.
    pub len: usize,
    /// The offset of the code (the symbol's value).
    pub code: usize,
}

const EHDR: usize = 64;
const PHDR: usize = 56;
const SHDR: usize = 64;
const SYM: usize = 24;
const DYN: usize = 16;
const PHNUM: usize = 3;
const SHNUM: usize = 11;
const TEXT_INDEX: u16 = 9;
const SHSTRNDX: u16 = 10;
/// Section names, `.shstrtab` itself last; offsets are found by name.
const SHSTRTAB: &[u8] = b"\0.hash\0.dynsym\0.dynstr\0.gnu.version\0.gnu.version_d\0\
.eh_frame_hdr\0.eh_frame\0.dynamic\0.text\0.shstrtab\0";
const DYNAMIC_ENTRIES: usize = 11;
/// `DW_EH_PE_pcrel | DW_EH_PE_sdata4`.
const PE_PCREL_SDATA4: u8 = 0x1b;
/// `DW_EH_PE_udata4`.
const PE_UDATA4: u8 = 0x03;
/// `DW_EH_PE_datarel | DW_EH_PE_sdata4`.
const PE_DATAREL_SDATA4: u8 = 0x3b;
const CIE_AUGMENTATION: &[u8] = b"zRS\0";

/// The System V ELF hash of a name.
#[must_use]
pub fn elf_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in name {
        h = (h << 4).wrapping_add(u32::from(c));
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

const fn align(v: usize, a: usize) -> usize {
    v.next_multiple_of(a)
}

/// Lengths a CIE or FDE takes once padded with `DW_CFA_nop`, so that
/// it, with its length field, ends 8-aligned.
const fn padded(body: usize) -> usize {
    align(4 + body, 8) - 4
}

struct Out<'b> {
    buf: &'b mut [u8],
}

impl Out<'_> {
    fn put(&mut self, at: usize, v: &[u8]) -> Result<(), Error> {
        let end = at.checked_add(v.len()).ok_or(Error::Truncated)?;
        self.buf
            .get_mut(at..end)
            .ok_or(Error::Truncated)?
            .copy_from_slice(v);
        Ok(())
    }
    fn u8(&mut self, at: usize, v: u8) -> Result<(), Error> {
        self.put(at, &[v])
    }
    fn u16(&mut self, at: usize, v: u16) -> Result<(), Error> {
        self.put(at, &v.to_le_bytes())
    }
    fn u32(&mut self, at: usize, v: u32) -> Result<(), Error> {
        self.put(at, &v.to_le_bytes())
    }
    fn u64(&mut self, at: usize, v: usize) -> Result<(), Error> {
        self.put(at, &(v as u64).to_le_bytes())
    }
    fn i32(&mut self, at: usize, v: isize) -> Result<(), Error> {
        let v = i32::try_from(v).map_err(|_| Error::Malformed("offset"))?;
        self.put(at, &v.to_le_bytes())
    }
}

/// A section header.
struct Shdr {
    name: &'static [u8],
    kind: u32,
    flags: usize,
    at: usize,
    size: usize,
    link: u32,
    info: u32,
    align: usize,
    entsize: usize,
}

const SHF_ALLOC: usize = 2;
const SHF_EXECINSTR: usize = 4;

fn u32_of(v: usize) -> Result<u32, Error> {
    u32::try_from(v).map_err(|_| Error::Malformed("size"))
}

fn diff(to: usize, from: usize) -> isize {
    (to as isize).wrapping_sub(from as isize)
}

fn shname(name: &[u8]) -> u32 {
    let mut at = 0;
    while at < SHSTRTAB.len() {
        let end = SHSTRTAB[at..]
            .iter()
            .position(|&b| b == 0)
            .map_or(SHSTRTAB.len(), |n| at + n);
        if &SHSTRTAB[at..end] == name {
            return at as u32;
        }
        at = end + 1;
    }
    0
}

impl Vdso<'_> {
    /// Writes the image to the start of `out`, which is zeroed first.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`] if `out` is too short, [`Error::Malformed`]
    /// for call frame instructions that did not encode.
    pub fn write(&self, out: &mut [u8]) -> Result<Written, Error> {
        out.fill(0);
        let cie = self.cie.bytes()?;
        let fde = self.fde.bytes()?;
        let mut o = Out { buf: out };

        // Where each section goes.
        let hash = EHDR + PHNUM * PHDR;
        let hash_len = 5 * 4;
        let dynsym = align(hash + hash_len, 8);
        let dynstr = dynsym + 2 * SYM;
        let s_symbol = 1;
        let s_soname = s_symbol + self.symbol.len() + 1;
        let s_version = s_soname + self.soname.len() + 1;
        let dynstr_len = s_version + self.version.len() + 1;
        let versym = align(dynstr + dynstr_len, 2);
        let verdef = align(versym + 2 * 2, 4);
        let verdef_len = 2 * (20 + 8);
        let eh_hdr = align(verdef + verdef_len, 4);
        let eh_hdr_len = 4 + 4 + 4 + 8;
        let eh = align(eh_hdr + eh_hdr_len, 8);
        let cie_body = padded(4 + 1 + CIE_AUGMENTATION.len() + 1 + 1 + 1 + 1 + 1 + cie.len());
        let fde_at = eh + 4 + cie_body;
        let fde_body = padded(4 + 4 + 4 + 1 + fde.len());
        let eh_len = 4 + cie_body + 4 + fde_body + 4;
        let dynamic = align(eh + eh_len, 8);
        let dynamic_len = DYNAMIC_ENTRIES * DYN;
        let text = align(dynamic + dynamic_len, 16);
        let load_end = text + self.code.len();
        let shstrtab = load_end;
        let shoff = align(shstrtab + SHSTRTAB.len(), 8);
        let len = shoff + SHNUM * SHDR;
        if len > usize::from(u16::MAX) {
            return Err(Error::Malformed("vdso too large"));
        }
        if len > o.buf.len() {
            return Err(Error::Truncated);
        }

        // The file header.
        o.put(0, b"\x7fELF\x02\x01\x01\x09")?; // ELFOSABI_FREEBSD
        o.u16(16, 3)?; // ET_DYN
        o.u16(18, self.machine.raw())?;
        o.u32(20, 1)?;
        o.u64(32, EHDR)?; // e_phoff
        o.u64(40, shoff)?;
        o.u16(52, EHDR as u16)?;
        o.u16(54, PHDR as u16)?;
        o.u16(56, PHNUM as u16)?;
        o.u16(58, SHDR as u16)?;
        o.u16(60, SHNUM as u16)?;
        o.u16(62, SHSTRNDX)?;

        // Program headers, all read and execute.
        let phdrs = [
            (1, 0, load_end, 16),                 // PT_LOAD
            (2, dynamic, dynamic_len, 8),         // PT_DYNAMIC
            (0x6474_e550, eh_hdr, eh_hdr_len, 4), // PT_GNU_EH_FRAME
        ];
        for (i, (kind, at, size, al)) in phdrs.into_iter().enumerate() {
            let p = EHDR + i * PHDR;
            o.u32(p, kind)?;
            o.u32(p + 4, 4 | 1)?; // PF_R | PF_X
            o.u64(p + 8, at)?;
            o.u64(p + 16, at)?;
            o.u64(p + 24, at)?;
            o.u64(p + 32, size)?;
            o.u64(p + 40, size)?;
            o.u64(p + 48, al)?;
        }

        // One bucket, holding the one symbol.
        for (i, w) in [1, 2, 1, 0, 0].into_iter().enumerate() {
            o.u32(hash + 4 * i, w)?;
        }

        let sym = dynsym + SYM;
        o.u32(sym, s_symbol as u32)?;
        o.u8(sym + 4, (1 << 4) | 2)?; // STB_GLOBAL, STT_FUNC
        o.u16(sym + 6, TEXT_INDEX)?;
        o.u64(sym + 8, text)?;
        o.u64(sym + 16, self.code.len())?;

        o.put(dynstr + s_symbol, self.symbol)?;
        o.put(dynstr + s_soname, self.soname)?;
        o.put(dynstr + s_version, self.version)?;

        // The symbol is at version 2, the first after the base.
        o.u16(versym + 2, 2)?;
        for (i, (flags, name, off)) in [(1, self.soname, s_soname), (0, self.version, s_version)]
            .into_iter()
            .enumerate()
        {
            let d = verdef + i * 28;
            o.u16(d, 1)?; // VER_DEF_CURRENT
            o.u16(d + 2, flags)?; // VER_FLG_BASE for the first
            o.u16(d + 4, i as u16 + 1)?;
            o.u16(d + 6, 1)?;
            o.u32(d + 8, elf_hash(name))?;
            o.u32(d + 12, 20)?;
            o.u32(d + 16, if i == 0 { 28 } else { 0 })?;
            o.u32(d + 20, off as u32)?;
        }

        // The frame header and its search table of one FDE.
        o.put(eh_hdr, &[1, PE_PCREL_SDATA4, PE_UDATA4, PE_DATAREL_SDATA4])?;
        o.i32(eh_hdr + 4, diff(eh, eh_hdr + 4))?;
        o.u32(eh_hdr + 8, 1)?;
        o.i32(eh_hdr + 12, diff(text, eh_hdr))?;
        o.i32(eh_hdr + 16, diff(fde_at, eh_hdr))?;

        // The CIE: a signal frame ("S"), FDE addresses PC-relative ("R").
        o.u32(eh, u32_of(cie_body)?)?;
        let mut c = Cfa::new(self.cie.data_align);
        c.uleb(1); // the code alignment factor
        c.sleb(i64::from(self.cie.data_align));
        c.uleb(u64::from(self.return_column));
        c.uleb(1);
        c.byte(PE_PCREL_SDATA4);
        let fields = c.bytes()?;
        let mut at = eh + 8;
        o.u8(at, 1)?; // version
        at += 1;
        o.put(at, CIE_AUGMENTATION)?;
        at += CIE_AUGMENTATION.len();
        o.put(at, fields)?;
        at += fields.len();
        o.put(at, cie)?;
        // The rest of its padding is DW_CFA_nop, which is zero.
        debug_assert!(at + cie.len() <= fde_at && DW_CFA_NOP == 0);

        o.u32(fde_at, u32_of(fde_body)?)?;
        o.u32(fde_at + 4, u32_of(fde_at + 4 - eh)?)?; // back to the CIE
        o.i32(fde_at + 8, diff(text, fde_at + 8))?;
        o.u32(fde_at + 12, u32_of(self.code.len())?)?;
        o.u8(fde_at + 16, 0)?; // no augmentation data
        o.put(fde_at + 17, fde)?;
        // The terminator, a zero length, is already there.

        let entries = [
            (14, s_soname),        // DT_SONAME
            (30, 2),               // DT_FLAGS: DF_SYMBOLIC
            (6, dynsym),           // DT_SYMTAB
            (11, SYM),             // DT_SYMENT
            (5, dynstr),           // DT_STRTAB
            (10, dynstr_len),      // DT_STRSZ
            (4, hash),             // DT_HASH
            (0x6fff_fff0, versym), // DT_VERSYM
            (0x6fff_fffc, verdef), // DT_VERDEF
            (0x6fff_fffd, 2),      // DT_VERDEFNUM
            (0, 0),                // DT_NULL
        ];
        for (i, (tag, v)) in entries.into_iter().enumerate() {
            o.u64(dynamic + i * DYN, tag)?;
            o.u64(dynamic + i * DYN + 8, v)?;
        }

        o.put(text, self.code)?;
        o.put(shstrtab, SHSTRTAB)?;

        let sh = |name, kind, flags, at, size| Shdr {
            name,
            kind,
            flags,
            at,
            size,
            link: 0,
            info: 0,
            align: 1,
            entsize: 0,
        };
        let alloc = SHF_ALLOC;
        let sections = [
            Shdr {
                link: 2,
                align: 4,
                entsize: 4,
                ..sh(b".hash", 5, alloc, hash, hash_len)
            },
            Shdr {
                link: 3,
                info: 1, // the first global symbol
                align: 8,
                entsize: SYM,
                ..sh(b".dynsym", 11, alloc, dynsym, 2 * SYM)
            },
            sh(b".dynstr", 3, alloc, dynstr, dynstr_len),
            Shdr {
                link: 2,
                align: 2,
                entsize: 2,
                ..sh(b".gnu.version", 0x6fff_ffff, alloc, versym, 4)
            },
            Shdr {
                link: 3,
                info: 2, // definitions
                align: 4,
                ..sh(b".gnu.version_d", 0x6fff_fffd, alloc, verdef, verdef_len)
            },
            Shdr {
                align: 4,
                ..sh(b".eh_frame_hdr", 1, alloc, eh_hdr, eh_hdr_len)
            },
            Shdr {
                align: 8,
                ..sh(b".eh_frame", 1, alloc, eh, eh_len)
            },
            Shdr {
                link: 3,
                align: 8,
                entsize: DYN,
                ..sh(b".dynamic", 6, alloc, dynamic, dynamic_len)
            },
            Shdr {
                align: 16,
                ..sh(b".text", 1, alloc | SHF_EXECINSTR, text, self.code.len())
            },
            sh(b".shstrtab", 3, 0, shstrtab, SHSTRTAB.len()),
        ];
        for (i, h) in sections.into_iter().enumerate() {
            let s = shoff + (i + 1) * SHDR;
            o.u32(s, shname(h.name))?;
            o.u32(s + 4, h.kind)?;
            o.u64(s + 8, h.flags)?;
            // Loaded sections are where their file offset says.
            o.u64(s + 16, if h.flags & SHF_ALLOC != 0 { h.at } else { 0 })?;
            o.u64(s + 24, h.at)?;
            o.u64(s + 32, h.size)?;
            o.u32(s + 40, h.link)?;
            o.u32(s + 44, h.info)?;
            o.u64(s + 48, h.align)?;
            o.u64(s + 56, h.entsize)?;
        }

        Ok(Written { len, code: text })
    }
}
