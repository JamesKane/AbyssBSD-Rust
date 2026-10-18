// SPDX-License-Identifier: BSD-2-Clause
//! The v0 mangling scheme (RFC 2603, "v0 Symbol Format" in the rustc book).
//!
//! A recursive-descent printer over the symbol bytes. Backrefs re-enter the
//! printer at an earlier offset, so every recursive entry counts against a
//! depth cap and a total-work cap, and a backref must point strictly before
//! itself. Output uses the alternate form: crate disambiguators and the type
//! suffixes of constant generic arguments are omitted.

use crate::Error;
use crate::sink::{Sink, dry_run_then_print};
use core::fmt;

/// Maximum nesting of paths, types and constants (backrefs included).
const MAX_DEPTH: u32 = 64;
/// Maximum number of grammar nodes visited, printed or skipped.
const MAX_STEPS: u32 = 1 << 16;
/// Longest punycode-decoded identifier, in characters.
const MAX_PUNYCODE_CHARS: usize = 128;

/// Demangles `sym`, the part of a v0 symbol after `_R`.
pub(crate) fn demangle(sym: &[u8], out: &mut dyn fmt::Write) -> Result<(), Error> {
    // An encoding version number would follow `_R` directly; only the
    // implicit version 0 exists.
    if sym.first().is_none_or(u8::is_ascii_digit) {
        return Err(Error::Invalid);
    }
    // Vendor-specific suffixes (`.llvm.1234`, `.0`, `$...`) are dropped; the
    // grammar itself never uses either character.
    let end = sym
        .iter()
        .position(|&b| b == b'.' || b == b'$')
        .unwrap_or(sym.len());
    let sym = sym.get(..end).ok_or(Error::Invalid)?;
    dry_run_then_print(out, |sink| {
        Printer {
            p: Parser { sym, pos: 0 },
            out: sink,
            skip: 0,
            depth: 0,
            steps: 0,
            bound_lifetimes: 0,
        }
        .symbol()
    })
}

/// A cursor over the symbol bytes.
#[derive(Clone, Copy)]
struct Parser<'s> {
    sym: &'s [u8],
    pos: usize,
}

/// A decoded identifier: plain ASCII, or the two halves of a punycode one.
#[derive(Clone, Copy)]
struct Ident<'s> {
    ascii: &'s [u8],
    punycode: &'s [u8],
}

impl Ident<'_> {
    fn is_empty(&self) -> bool {
        self.ascii.is_empty() && self.punycode.is_empty()
    }
}

impl<'s> Parser<'s> {
    fn peek(&self) -> Option<u8> {
        self.sym.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos = self.pos.saturating_add(1);
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Result<u8, Error> {
        let b = self.peek().ok_or(Error::Invalid)?;
        self.pos = self.pos.saturating_add(1);
        Ok(b)
    }

    fn back(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    /// `base-62-number = {digit | lower | upper} "_"`, where `_` alone is 0
    /// and digits `n_` encode `n + 1`.
    fn integer_62(&mut self) -> Result<u64, Error> {
        if self.eat(b'_') {
            return Ok(0);
        }
        let mut x: u64 = 0;
        loop {
            let b = self.next()?;
            let d = match b {
                b'_' => break,
                b'0'..=b'9' => b - b'0',
                b'a'..=b'z' => b - b'a' + 10,
                b'A'..=b'Z' => b - b'A' + 36,
                _ => return Err(Error::Invalid),
            };
            x = x
                .checked_mul(62)
                .and_then(|x| x.checked_add(u64::from(d)))
                .ok_or(Error::Invalid)?;
        }
        x.checked_add(1).ok_or(Error::Invalid)
    }

    /// `[tag base-62-number]`: 0 when absent, the number plus one otherwise.
    fn opt_integer_62(&mut self, tag: u8) -> Result<u64, Error> {
        if !self.eat(tag) {
            return Ok(0);
        }
        self.integer_62()?.checked_add(1).ok_or(Error::Invalid)
    }

    /// `disambiguator = "s" base-62-number`.
    fn disambiguator(&mut self) -> Result<u64, Error> {
        self.opt_integer_62(b's')
    }

    /// `decimal-number = "0" | non-zero-digit {digit}`.
    fn decimal(&mut self) -> Result<usize, Error> {
        let first = self.next()?;
        if !first.is_ascii_digit() {
            return Err(Error::Invalid);
        }
        let mut x = usize::from(first - b'0');
        if x == 0 {
            return Ok(0);
        }
        while let Some(b) = self.peek().filter(u8::is_ascii_digit) {
            self.pos = self.pos.saturating_add(1);
            x = x
                .checked_mul(10)
                .and_then(|x| x.checked_add(usize::from(b - b'0')))
                .ok_or(Error::Invalid)?;
        }
        Ok(x)
    }

    /// `[\"u\"] decimal-number [\"_\"] bytes`.
    fn undisambiguated_ident(&mut self) -> Result<Ident<'s>, Error> {
        let is_punycode = self.eat(b'u');
        let len = self.decimal()?;
        self.eat(b'_');
        let end = self.pos.checked_add(len).ok_or(Error::Invalid)?;
        let bytes = self.sym.get(self.pos..end).ok_or(Error::Invalid)?;
        self.pos = end;
        if !is_punycode {
            return Ok(Ident {
                ascii: bytes,
                punycode: &[],
            });
        }
        // The last `_` (Rust's stand-in for `-`) separates the literal
        // characters from the encoded insertions.
        Ok(match bytes.iter().rposition(|&b| b == b'_') {
            Some(i) => Ident {
                ascii: bytes.get(..i).ok_or(Error::Invalid)?,
                punycode: bytes.get(i.saturating_add(1)..).ok_or(Error::Invalid)?,
            },
            None => Ident {
                ascii: &[],
                punycode: bytes,
            },
        })
    }

    /// `identifier = [disambiguator] undisambiguated-identifier`.
    fn ident(&mut self) -> Result<(u64, Ident<'s>), Error> {
        let dis = self.disambiguator()?;
        Ok((dis, self.undisambiguated_ident()?))
    }

    /// Lowercase hex digits up to and including the terminating `_`.
    fn hex_digits(&mut self) -> Result<&'s [u8], Error> {
        let start = self.pos;
        loop {
            match self.next()? {
                b'0'..=b'9' | b'a'..=b'f' => {}
                b'_' => break,
                _ => return Err(Error::Invalid),
            }
        }
        self.sym
            .get(start..self.pos.saturating_sub(1))
            .ok_or(Error::Invalid)
    }

    /// `backref = "B" base-62-number` (the `B` already consumed): a parser
    /// positioned at the target, which must precede the backref itself.
    fn backref(&mut self) -> Result<Parser<'s>, Error> {
        let at = self.pos.saturating_sub(1);
        let target = usize::try_from(self.integer_62()?).map_err(|_| Error::Invalid)?;
        if target >= at {
            return Err(Error::Invalid);
        }
        Ok(Parser {
            sym: self.sym,
            pos: target,
        })
    }
}

/// Basic type codes.
fn basic_type(b: u8) -> Option<&'static str> {
    Some(match b {
        b'a' => "i8",
        b'b' => "bool",
        b'c' => "char",
        b'd' => "f64",
        b'e' => "str",
        b'f' => "f32",
        b'h' => "u8",
        b'i' => "isize",
        b'j' => "usize",
        b'l' => "i32",
        b'm' => "u32",
        b'n' => "i128",
        b'o' => "u128",
        b's' => "i16",
        b't' => "u16",
        b'u' => "()",
        b'v' => "...",
        b'x' => "i64",
        b'y' => "u64",
        b'z' => "!",
        b'p' => "_",
        _ => return None,
    })
}

struct Printer<'s, 'a, 'o> {
    p: Parser<'s>,
    out: &'a mut Sink<'o>,
    /// Nonzero while parsing something that is not printed (impl paths,
    /// the instantiating crate).
    skip: u32,
    depth: u32,
    steps: u32,
    /// Lifetimes bound by enclosing `for<...>` binders.
    bound_lifetimes: u64,
}

impl Printer<'_, '_, '_> {
    fn emit(&mut self, s: &str) -> Result<(), Error> {
        if self.skip > 0 {
            return Ok(());
        }
        self.out.str(s)
    }

    fn emit_char(&mut self, c: char) -> Result<(), Error> {
        if self.skip > 0 {
            return Ok(());
        }
        self.out.char(c)
    }

    fn emit_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<(), Error> {
        if self.skip > 0 {
            return Ok(());
        }
        self.out.fmt(args)
    }

    /// Counts one unit of work.
    fn step(&mut self) -> Result<(), Error> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > MAX_STEPS {
            return Err(Error::TooComplex);
        }
        Ok(())
    }

    /// Runs `f` one level deeper, enforcing the depth and work caps.
    fn nested(&mut self, f: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<(), Error> {
        self.step()?;
        if self.depth >= MAX_DEPTH {
            return Err(Error::TooComplex);
        }
        self.depth = self.depth.saturating_add(1);
        let r = f(self);
        self.depth = self.depth.saturating_sub(1);
        r
    }

    /// Runs `f` on the backref target, then resumes after the backref.
    fn at_backref(&mut self, f: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<(), Error> {
        let target = self.p.backref()?;
        let resume = core::mem::replace(&mut self.p, target);
        let r = f(self);
        self.p = resume;
        r
    }

    /// Parses `f` without printing.
    fn skipped(&mut self, f: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<(), Error> {
        self.skip = self.skip.saturating_add(1);
        let r = f(self);
        self.skip = self.skip.saturating_sub(1);
        r
    }

    /// Runs `f` for each element up to the closing `E`, printing `sep`
    /// between them; returns the element count.
    fn list(
        &mut self,
        sep: &str,
        mut f: impl FnMut(&mut Self) -> Result<(), Error>,
    ) -> Result<usize, Error> {
        let mut n: usize = 0;
        while !self.p.eat(b'E') {
            self.step()?;
            if n > 0 {
                self.emit(sep)?;
            }
            f(self)?;
            n = n.saturating_add(1);
        }
        Ok(n)
    }

    /// `symbol-name = "_R" path [instantiating-crate]` (vendor suffix already
    /// stripped).
    fn symbol(mut self) -> Result<(), Error> {
        self.path(true)?;
        if self.p.peek().is_some() {
            self.skipped(|this| this.path(false))?;
        }
        if self.p.peek().is_some() {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    fn ident(&mut self, id: Ident<'_>) -> Result<(), Error> {
        if id.punycode.is_empty() {
            let s = core::str::from_utf8(id.ascii).map_err(|_| Error::Invalid)?;
            return self.emit(s);
        }
        let mut buf = ['\0'; MAX_PUNYCODE_CHARS];
        match punycode_decode(id.ascii, id.punycode, &mut buf) {
            Some(chars) => {
                for &c in chars {
                    self.emit_char(c)?;
                }
                Ok(())
            }
            None => {
                // Undecodable: show the encoded form.
                let ascii = core::str::from_utf8(id.ascii).map_err(|_| Error::Invalid)?;
                let puny = core::str::from_utf8(id.punycode).map_err(|_| Error::Invalid)?;
                self.emit("punycode{")?;
                if !ascii.is_empty() {
                    self.emit(ascii)?;
                    self.emit("-")?;
                }
                self.emit(puny)?;
                self.emit("}")
            }
        }
    }

    /// Prints a lifetime given its de Bruijn index (0 is the erased `'_`).
    fn lifetime(&mut self, index: u64) -> Result<(), Error> {
        if index == 0 {
            return self.emit("'_");
        }
        let depth = self
            .bound_lifetimes
            .checked_sub(index)
            .ok_or(Error::Invalid)?;
        match u8::try_from(depth).ok().filter(|&d| d < 26) {
            Some(d) => {
                self.emit("'")?;
                self.emit_char(char::from(b'a' + d))
            }
            None => self.emit_fmt(format_args!("'_{depth}")),
        }
    }

    /// `binder = "G" base-62-number`: prints `for<'a, ...> ` and binds the
    /// lifetimes. The caller restores `bound_lifetimes` afterwards.
    fn binder(&mut self) -> Result<(), Error> {
        let count = self.p.opt_integer_62(b'G')?;
        if count == 0 {
            return Ok(());
        }
        self.emit("for<")?;
        for i in 0..count {
            self.step()?;
            if i > 0 {
                self.emit(", ")?;
            }
            self.bound_lifetimes = self
                .bound_lifetimes
                .checked_add(1)
                .ok_or(Error::TooComplex)?;
            self.lifetime(1)?;
        }
        self.emit("> ")
    }

    /// Runs `f` with any binder it parses scoped to it.
    fn in_binder_scope(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let saved = self.bound_lifetimes;
        let r = f(self);
        self.bound_lifetimes = saved;
        r
    }

    fn path(&mut self, in_value: bool) -> Result<(), Error> {
        self.nested(|this| this.path_inner(in_value))
    }

    fn path_inner(&mut self, in_value: bool) -> Result<(), Error> {
        match self.p.next()? {
            // Crate root; the disambiguator hash is not printed.
            b'C' => {
                let (_, id) = self.p.ident()?;
                self.ident(id)
            }
            b'N' => {
                let ns = self.p.next()?;
                if !ns.is_ascii_alphabetic() {
                    return Err(Error::Invalid);
                }
                self.path(in_value)?;
                let (dis, id) = self.p.ident()?;
                if ns.is_ascii_lowercase() {
                    // Implementation-internal namespaces may be unnamed
                    // (constructors, lint statics); nothing to print then.
                    if id.is_empty() {
                        return Ok(());
                    }
                    self.emit("::")?;
                    return self.ident(id);
                }
                self.emit("::{")?;
                match ns {
                    b'C' => self.emit("closure")?,
                    b'S' => self.emit("shim")?,
                    other => self.emit_char(char::from(other))?,
                }
                if !id.is_empty() {
                    self.emit(":")?;
                    self.ident(id)?;
                }
                self.emit_fmt(format_args!("#{dis}}}"))
            }
            b'M' => {
                self.impl_path()?;
                self.emit("<")?;
                self.ty()?;
                self.emit(">")
            }
            b'X' => {
                self.impl_path()?;
                self.emit("<")?;
                self.ty()?;
                self.emit(" as ")?;
                self.path(false)?;
                self.emit(">")
            }
            b'Y' => {
                self.emit("<")?;
                self.ty()?;
                self.emit(" as ")?;
                self.path(false)?;
                self.emit(">")
            }
            b'I' => {
                self.path(in_value)?;
                if in_value {
                    self.emit("::")?;
                }
                self.emit("<")?;
                self.list(", ", Self::generic_arg)?;
                self.emit(">")
            }
            b'B' => self.at_backref(|this| this.path(in_value)),
            _ => Err(Error::Invalid),
        }
    }

    /// `impl-path = [disambiguator] path`, which is never printed.
    fn impl_path(&mut self) -> Result<(), Error> {
        self.p.disambiguator()?;
        self.skipped(|this| this.path(false))
    }

    /// Prints a path that may end in generic arguments, leaving their `<`
    /// open so a `dyn` trait's associated-type bindings can join the list.
    /// Returns whether a `<` was left open.
    fn path_maybe_open(&mut self) -> Result<bool, Error> {
        let mut open = false;
        self.nested(|this| {
            if this.p.eat(b'B') {
                let target = this.p.backref()?;
                let resume = core::mem::replace(&mut this.p, target);
                let r = this.path_maybe_open();
                this.p = resume;
                open = r?;
                Ok(())
            } else if this.p.eat(b'I') {
                this.path(false)?;
                this.emit("<")?;
                this.list(", ", Self::generic_arg)?;
                open = true;
                Ok(())
            } else {
                this.path(false)
            }
        })?;
        Ok(open)
    }

    /// `generic-arg = lifetime | type | "K" const`.
    fn generic_arg(&mut self) -> Result<(), Error> {
        if self.p.eat(b'L') {
            let lt = self.p.integer_62()?;
            self.lifetime(lt)
        } else if self.p.eat(b'K') {
            self.konst(false)
        } else {
            self.ty()
        }
    }

    fn ty(&mut self) -> Result<(), Error> {
        self.nested(Self::ty_inner)
    }

    fn ty_inner(&mut self) -> Result<(), Error> {
        let tag = self.p.next()?;
        if let Some(name) = basic_type(tag) {
            return self.emit(name);
        }
        match tag {
            b'R' | b'Q' => {
                self.emit("&")?;
                if self.p.eat(b'L') {
                    let lt = self.p.integer_62()?;
                    if lt != 0 {
                        self.lifetime(lt)?;
                        self.emit(" ")?;
                    }
                }
                if tag == b'Q' {
                    self.emit("mut ")?;
                }
                self.ty()
            }
            b'P' => {
                self.emit("*const ")?;
                self.ty()
            }
            b'O' => {
                self.emit("*mut ")?;
                self.ty()
            }
            b'A' => {
                self.emit("[")?;
                self.ty()?;
                self.emit("; ")?;
                self.konst(true)?;
                self.emit("]")
            }
            b'S' => {
                self.emit("[")?;
                self.ty()?;
                self.emit("]")
            }
            b'T' => {
                self.emit("(")?;
                let n = self.list(", ", Self::ty)?;
                if n == 1 {
                    self.emit(",")?;
                }
                self.emit(")")
            }
            b'F' => self.in_binder_scope(Self::fn_sig),
            b'D' => self.dyn_bounds(),
            b'B' => self.at_backref(Self::ty),
            _ => {
                self.p.back();
                self.path(false)
            }
        }
    }

    /// `fn-sig = [binder] ["U"] ["K" abi] {type} "E" type`.
    fn fn_sig(&mut self) -> Result<(), Error> {
        self.binder()?;
        if self.p.eat(b'U') {
            self.emit("unsafe ")?;
        }
        if self.p.eat(b'K') {
            self.emit("extern \"")?;
            if self.p.eat(b'C') {
                self.emit("C")?;
            } else {
                let abi = self.p.undisambiguated_ident()?;
                if !abi.punycode.is_empty() {
                    return Err(Error::Invalid);
                }
                // `_` stands for `-` (`system_unwind` is "system-unwind").
                for part in abi.ascii.split(|&b| b == b'_').enumerate() {
                    if part.0 > 0 {
                        self.emit("-")?;
                    }
                    let s = core::str::from_utf8(part.1).map_err(|_| Error::Invalid)?;
                    self.emit(s)?;
                }
            }
            self.emit("\" ")?;
        }
        self.emit("fn(")?;
        self.list(", ", Self::ty)?;
        self.emit(")")?;
        if !self.p.eat(b'u') {
            self.emit(" -> ")?;
            self.ty()?;
        }
        Ok(())
    }

    /// `"D" dyn-bounds lifetime`, with `dyn-bounds = [binder] {dyn-trait} "E"`.
    fn dyn_bounds(&mut self) -> Result<(), Error> {
        self.emit("dyn ")?;
        self.in_binder_scope(|this| {
            this.binder()?;
            this.list(" + ", Self::dyn_trait)?;
            Ok(())
        })?;
        if !self.p.eat(b'L') {
            return Err(Error::Invalid);
        }
        let lt = self.p.integer_62()?;
        if lt != 0 {
            self.emit(" + ")?;
            self.lifetime(lt)?;
        }
        Ok(())
    }

    /// `dyn-trait = path {"p" undisambiguated-identifier type}`.
    fn dyn_trait(&mut self) -> Result<(), Error> {
        let mut open = self.path_maybe_open()?;
        while self.p.eat(b'p') {
            self.step()?;
            self.emit(if open { ", " } else { "<" })?;
            open = true;
            let name = self.p.undisambiguated_ident()?;
            self.ident(name)?;
            self.emit(" = ")?;
            self.ty()?;
        }
        if open {
            self.emit(">")?;
        }
        Ok(())
    }

    fn konst(&mut self, in_value: bool) -> Result<(), Error> {
        self.nested(|this| this.konst_inner(in_value))
    }

    /// `const = type const-data | "p" | backref`, plus the structural
    /// constants (`R`, `Q`, `A`, `T`, `V`) of the extended grammar.
    fn konst_inner(&mut self, in_value: bool) -> Result<(), Error> {
        let tag = self.p.next()?;
        match tag {
            b'p' => self.emit("_"),
            b'B' => self.at_backref(|this| this.konst(in_value)),
            b'h' | b't' | b'm' | b'y' | b'o' | b'j' => {
                let hex = self.p.hex_digits()?;
                self.uint(hex)
            }
            b'a' | b's' | b'l' | b'x' | b'n' | b'i' => {
                if self.p.eat(b'n') {
                    self.emit("-")?;
                }
                let hex = self.p.hex_digits()?;
                self.uint(hex)
            }
            b'b' => match hex_value(self.p.hex_digits()?) {
                Some(0) => self.emit("false"),
                Some(1) => self.emit("true"),
                _ => Err(Error::Invalid),
            },
            b'c' => {
                let c = hex_value(self.p.hex_digits()?)
                    .and_then(|v| u32::try_from(v).ok())
                    .and_then(char::from_u32)
                    .ok_or(Error::Invalid)?;
                self.emit_fmt(format_args!("{c:?}"))
            }
            b'e' => self.str_literal(),
            b'R' | b'Q' | b'A' | b'T' | b'V' => {
                // Structural constants read as expressions; in type position
                // they are wrapped in a block.
                if !in_value {
                    self.emit("{")?;
                }
                self.structural_const(tag)?;
                if !in_value {
                    self.emit("}")?;
                }
                Ok(())
            }
            _ => Err(Error::Invalid),
        }
    }

    fn structural_const(&mut self, tag: u8) -> Result<(), Error> {
        match tag {
            b'R' | b'Q' => {
                // `&"str"` is printed as just the literal.
                if tag == b'R' && self.p.eat(b'e') {
                    return self.str_literal();
                }
                self.emit(if tag == b'Q' { "&mut " } else { "&" })?;
                self.konst(true)
            }
            b'A' => {
                self.emit("[")?;
                self.list(", ", |this| this.konst(true))?;
                self.emit("]")
            }
            b'T' => {
                self.emit("(")?;
                let n = self.list(", ", |this| this.konst(true))?;
                if n == 1 {
                    self.emit(",")?;
                }
                self.emit(")")
            }
            b'V' => {
                self.path(true)?;
                match self.p.next()? {
                    b'U' => Ok(()),
                    b'T' => {
                        self.emit("(")?;
                        self.list(", ", |this| this.konst(true))?;
                        self.emit(")")
                    }
                    b'S' => {
                        self.emit(" { ")?;
                        self.list(", ", |this| {
                            let (_, name) = this.p.ident()?;
                            this.ident(name)?;
                            this.emit(": ")?;
                            this.konst(true)
                        })?;
                        self.emit(" }")
                    }
                    _ => Err(Error::Invalid),
                }
            }
            _ => Err(Error::Invalid),
        }
    }

    /// `e` constant data: the UTF-8 bytes as hex pairs, ending in `_`.
    fn str_literal(&mut self) -> Result<(), Error> {
        let (pairs, odd) = self.p.hex_digits()?.as_chunks::<2>();
        if !odd.is_empty() {
            return Err(Error::Invalid);
        }
        self.emit("\"")?;
        let mut buf = [0u8; 4];
        let mut have = 0usize;
        for pair in pairs {
            self.step()?;
            let byte = pair
                .iter()
                .try_fold(0u8, |acc, &d| Some(acc.checked_mul(16)? | hex_digit(d)?))
                .ok_or(Error::Invalid)?;
            *buf.get_mut(have).ok_or(Error::Invalid)? = byte;
            have = have.saturating_add(1);
            let pending = buf.get(..have).ok_or(Error::Invalid)?;
            match core::str::from_utf8(pending) {
                Ok(s) => {
                    for c in s.chars() {
                        if c == '\'' {
                            self.emit_char(c)?;
                        } else {
                            self.emit_fmt(format_args!("{}", c.escape_debug()))?;
                        }
                    }
                    have = 0;
                }
                // Incomplete sequence: wait for more bytes.
                Err(e) if e.error_len().is_none() => {}
                Err(_) => return Err(Error::Invalid),
            }
        }
        if have != 0 {
            return Err(Error::Invalid);
        }
        self.emit("\"")
    }

    /// Prints hex digits as a decimal integer, or as `0x...` beyond 128 bits.
    fn uint(&mut self, hex: &[u8]) -> Result<(), Error> {
        match hex_value(hex) {
            Some(v) => self.emit_fmt(format_args!("{v}")),
            None => {
                let s = core::str::from_utf8(hex).map_err(|_| Error::Invalid)?;
                self.emit("0x")?;
                self.emit(s)
            }
        }
    }
}

fn hex_digit(d: u8) -> Option<u8> {
    match d {
        b'0'..=b'9' => Some(d - b'0'),
        b'a'..=b'f' => Some(d - b'a' + 10),
        _ => None,
    }
}

/// The value of lowercase hex digits (empty is 0), or `None` past 128 bits.
fn hex_value(hex: &[u8]) -> Option<u128> {
    hex.iter().try_fold(0u128, |acc, &d| {
        acc.checked_mul(16)?.checked_add(u128::from(hex_digit(d)?))
    })
}

/// RFC 3492 punycode decoding (with `_` as the delimiter, already split off)
/// into `buf`. `None` if malformed or longer than the buffer.
fn punycode_decode<'b>(ascii: &[u8], puny: &[u8], buf: &'b mut [char]) -> Option<&'b [char]> {
    const BASE: u32 = 36;
    const T_MIN: u32 = 1;
    const T_MAX: u32 = 26;

    let mut len = 0usize;
    for &b in ascii {
        if !b.is_ascii() {
            return None;
        }
        *buf.get_mut(len)? = char::from(b);
        len = len.checked_add(1)?;
    }

    let mut n: u32 = 0x80;
    let mut i: u32 = 0;
    let mut bias: u32 = 72;
    let mut digits = puny.iter();
    let mut first = true;
    while !digits.as_slice().is_empty() {
        let old_i = i;
        let mut w: u32 = 1;
        let mut k = BASE;
        loop {
            let d = match *digits.next()? {
                b @ b'a'..=b'z' => u32::from(b - b'a'),
                b @ b'0'..=b'9' => u32::from(b - b'0') + 26,
                _ => return None,
            };
            i = i.checked_add(d.checked_mul(w)?)?;
            let t = if k <= bias {
                T_MIN
            } else if k >= bias.checked_add(T_MAX)? {
                T_MAX
            } else {
                k - bias
            };
            if d < t {
                break;
            }
            w = w.checked_mul(BASE - t)?;
            k = k.checked_add(BASE)?;
        }
        len = len.checked_add(1)?;
        let len32 = u32::try_from(len).ok()?;

        // Bias adaptation.
        let mut delta = i.checked_sub(old_i)?;
        delta /= if first { 700 } else { 2 };
        first = false;
        delta = delta.checked_add(delta / len32)?;
        let mut k = 0u32;
        while delta > ((BASE - T_MIN) * T_MAX) / 2 {
            delta /= BASE - T_MIN;
            k = k.checked_add(BASE)?;
        }
        bias = k.checked_add(((BASE - T_MIN + 1) * delta) / (delta.checked_add(38)?))?;

        n = n.checked_add(i / len32)?;
        i %= len32;
        let c = char::from_u32(n)?;
        let at = usize::try_from(i).ok()?;
        // Insert `c` at `at`, shifting the tail right by one.
        let tail = buf.get_mut(at..len)?;
        tail.rotate_right(1);
        *tail.first_mut()? = c;
        i = i.checked_add(1)?;
    }
    buf.get(..len)
}
