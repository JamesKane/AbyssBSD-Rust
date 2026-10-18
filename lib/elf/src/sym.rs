// SPDX-License-Identifier: BSD-2-Clause
//! Symbol tables (`SHT_SYMTAB` + its string table), for address → name
//! lookups in backtraces.

use crate::{Error, cstr, u16_at, u32_at, u64_at};

const SYM_SIZE: usize = 24;
const STT_FUNC: u8 = 2;
const STT_OBJECT: u8 = 1;

/// One symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbol<'a> {
    /// The (possibly mangled) name.
    pub name: &'a [u8],
    /// `st_value`: the link-time address.
    pub value: u64,
    /// `st_size`.
    pub size: u64,
    /// Whether it is a function (`STT_FUNC`).
    pub is_function: bool,
}

/// A symbol table and its strings.
#[derive(Clone, Copy, Debug)]
pub struct SymbolTable<'a> {
    syms: &'a [u8],
    strings: &'a [u8],
}

impl<'a> SymbolTable<'a> {
    /// Wraps raw `.symtab` and `.strtab` contents.
    pub fn new(syms: &'a [u8], strings: &'a [u8]) -> Result<Self, Error> {
        if !syms.len().is_multiple_of(SYM_SIZE) {
            return Err(Error::Malformed("symbol table size"));
        }
        Ok(Self { syms, strings })
    }

    /// Every named function or object symbol with an address. Entries whose
    /// name is out of range are skipped.
    pub fn symbols(&self) -> impl Iterator<Item = Symbol<'a>> + 'a {
        let strings = self.strings;
        self.syms
            .as_chunks::<SYM_SIZE>()
            .0
            .iter()
            .filter_map(move |e| {
                let info = e[4];
                let kind = info & 0xf;
                if kind != STT_FUNC && kind != STT_OBJECT {
                    return None;
                }
                // SHN_UNDEF: no address.
                if u16_at(e, 6).ok()? == 0 {
                    return None;
                }
                let name = cstr(strings, u32_at(e, 0).ok()?).ok()?;
                if name.is_empty() {
                    return None;
                }
                Some(Symbol {
                    name,
                    value: u64_at(e, 8).ok()?,
                    size: u64_at(e, 16).ok()?,
                    is_function: kind == STT_FUNC,
                })
            })
    }

    /// The symbol containing link-time address `addr`, with the offset into
    /// it. Symbols of size zero match only their exact address.
    pub fn lookup(&self, addr: u64) -> Option<(Symbol<'a>, u64)> {
        self.symbols()
            .filter(|s| addr >= s.value && (addr - s.value < s.size || addr == s.value))
            // Prefer the tightest enclosing symbol.
            .min_by_key(|s| s.size)
            .map(|s| (s, addr - s.value))
    }
}
