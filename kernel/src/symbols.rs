// SPDX-License-Identifier: BSD-2-Clause
//! Address → symbol lookups for backtraces, from the ELF symbol table the
//! loader left in boot data.

use core::fmt;

use bootproto::BootInfo;
use elf::SymbolTable;
use sync::SpinLock;

#[derive(Clone, Copy)]
struct Symbols {
    table: SymbolTable<'static>,
    slide: u64,
}

static SYMBOLS: SpinLock<Option<Symbols>> = SpinLock::new(None);

/// Picks up the loader's symbol table, if it passed one.
pub(crate) fn init(info: &BootInfo) {
    let Some(s) = info.symbols.get() else {
        kprintln!("symbols: none (backtraces will be raw addresses)");
        return;
    };
    let symtab = crate::boot::phys_bytes(s.symtab);
    let strtab = crate::boot::phys_bytes(s.strtab);
    match SymbolTable::new(symtab, strtab) {
        Ok(table) => {
            *SYMBOLS.lock() = Some(Symbols {
                table,
                slide: info.kernel.slide,
            });
        }
        Err(e) => kprintln!("symbols: unusable ({e:?})"),
    }
}

/// The symbol containing run-time address `addr`, with the offset.
pub(crate) fn lookup(addr: u64) -> Option<(&'static [u8], u64)> {
    // The panic path may interrupt a holder; a missed lookup is acceptable.
    let s = (*SYMBOLS.try_lock()?)?;
    let (sym, off) = s.table.lookup(addr.wrapping_sub(s.slide))?;
    Some((sym.name, off))
}

/// Displays an address with its symbol: `0xffff… name+0x1c`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Symbolized {
    /// The address shown.
    pub(crate) addr: u64,
    /// The address looked up (a return address minus one, so that a call
    /// at the very end of a function is attributed to it).
    pub(crate) lookup: u64,
}

impl Symbolized {
    /// An instruction address.
    pub(crate) fn pc(addr: u64) -> Self {
        Self { addr, lookup: addr }
    }

    /// A return address.
    pub(crate) fn ret(addr: u64) -> Self {
        Self {
            addr,
            lookup: addr.saturating_sub(1),
        }
    }
}

impl fmt::Display for Symbolized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.addr)?;
        if let Some((name, off)) = lookup(self.lookup) {
            let off = off + (self.addr - self.lookup);
            write!(f, " {}+{off:#x}", demangle::Demangled(name))?;
        }
        Ok(())
    }
}
