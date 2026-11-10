// SPDX-License-Identifier: BSD-2-Clause
//! `rgegen`: the RTL8125/8126 register programs `drivers/rge` replays,
//! read from FreeBSD's `sys/dev/rge`.
//!
//! Realtek's controllers need vendor microcode and long fixed sequences
//! of PHY register writes that no document explains: they are data,
//! carried over as FreeBSD's driver carries them. This reads them from
//! the source instead of copying them by hand:
//!
//! - the MAC microcode patches (`rtl8126_*_mac_bps_regs` and `_vals`, in
//!   `if_rge_microcode.h`);
//! - the PHY microcode (`mac_r26_*_mcu`);
//! - each chip's PHY configuration function (`rge_phy_config_mac_r26_*`
//!   in `if_rge_hw.c`), as a list of [`Op`]s.
//!
//! The function bodies are a handful of statement shapes; anything else
//! is an error, so a change upstream cannot be dropped silently.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// One step of a PHY configuration program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `rge_write_phy_ocp(reg, val)`.
    Write(u16, u16),
    /// `RGE_PHY_SETBIT(reg, bits)`.
    Set(u16, u16),
    /// `RGE_PHY_CLRBIT(reg, bits)`.
    Clear(u16, u16),
    /// `val = read(reg) & ~mask; write(reg, val | bits)`.
    Modify(u16, u16, u16),
    /// `rge_patch_phy_mcu(on)`.
    PatchMcu(bool),
    /// `rge_phy_config_mcu(version)`: load the PHY microcode unless the
    /// PHY already runs this version.
    LoadMcu(u16),
}

/// What could not be read.
#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error(msg.into()))
}

/// `/* ... */` and `// ...` removed.
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(['/']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(t) = tail.strip_prefix("/*") {
            rest = t.find("*/").map_or("", |e| &t[e + 2..]);
        } else if let Some(t) = tail.strip_prefix("//") {
            rest = t.find('\n').map_or("", |e| &t[e..]);
        } else {
            out.push('/');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// A C integer literal, or a `#define`d name.
fn number(s: &str, defines: &BTreeMap<String, u64>) -> Result<u64, Error> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).map_err(|_| Error(format!("bad number {s:?}")));
    }
    if let Ok(n) = s.parse() {
        return Ok(n);
    }
    defines
        .get(s)
        .copied()
        .ok_or_else(|| Error(format!("unknown name {s:?}")))
}

fn u16_of(s: &str, defines: &BTreeMap<String, u64>) -> Result<u16, Error> {
    let n = number(s, defines)?;
    u16::try_from(n).map_err(|_| Error(format!("{s} does not fit 16 bits")))
}

/// The numeric `#define`s of a header.
#[must_use]
pub fn defines(header: &str) -> BTreeMap<String, u64> {
    let empty = BTreeMap::new();
    let mut out = BTreeMap::new();
    for line in strip_comments(header).lines() {
        let mut w = line.split_whitespace();
        if w.next() != Some("#define") {
            continue;
        }
        let (Some(name), Some(value), None) = (w.next(), w.next(), w.next()) else {
            continue;
        };
        if let Ok(v) = number(value, &empty) {
            out.insert(name.to_string(), v);
        }
    }
    out
}

/// The text between `{` and its `}` after `start` in `text`.
fn braced(text: &str, start: usize) -> Result<&str, Error> {
    let open = start + text[start..].find('{').ok_or(Error("no `{`".into()))?;
    let mut depth = 0;
    for (k, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&text[open + 1..open + k]);
                }
            }
            _ => {}
        }
    }
    err("unbalanced braces")
}

/// The initializer of the array declared as `name[] =`.
fn initializer<'a>(text: &'a str, name: &str) -> Result<&'a str, Error> {
    let decl = format!("{name}[] =");
    let at = text
        .find(&decl)
        .ok_or_else(|| Error(format!("no array {name}")))?;
    braced(text, at)
}

/// The numbers of a `uint16_t` array.
///
/// # Errors
///
/// A missing array or a malformed element.
pub fn u16_array(text: &str, name: &str) -> Result<Vec<u16>, Error> {
    let empty = BTreeMap::new();
    initializer(text, name)?
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| u16_of(s, &empty))
        .collect()
}

/// The (register, value) pairs of a `struct rge_hw_regaddr_array` array.
///
/// # Errors
///
/// A missing array or a malformed element.
pub fn pair_array(text: &str, name: &str) -> Result<Vec<(u16, u16)>, Error> {
    let empty = BTreeMap::new();
    let body = initializer(text, name)?;
    let mut out = Vec::new();
    for item in body.split('}') {
        let item = item.trim().trim_start_matches(',').trim();
        if item.is_empty() {
            continue;
        }
        let inner = item
            .strip_prefix('{')
            .ok_or_else(|| Error(format!("{name}: bad element {item:?}")))?;
        let mut f = inner.split(',').map(str::trim);
        let (Some(r), Some(v), None) = (f.next(), f.next(), f.next()) else {
            return err(format!("{name}: bad element {item:?}"));
        };
        out.push((u16_of(r, &empty)?, u16_of(v, &empty)?));
    }
    Ok(out)
}

/// A call `name(sc, a, b, ...)`: its arguments after `sc`.
fn call<'a>(stmt: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let rest = stmt.strip_prefix(name)?.trim_start().strip_prefix('(')?;
    let inner = rest.strip_suffix(')')?;
    let mut args: Vec<&str> = inner.split(',').map(str::trim).collect();
    if args.first() != Some(&"sc") {
        return None;
    }
    args.remove(0);
    Some(args)
}

/// The statements of a function body, each `for` loop with its body.
fn statements(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0;
    for c in body.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if c == ';' && depth == 0 {
            // A `for` header's own `;`s are inside its parentheses, so a
            // loop and its one-statement body end here together.
            let s = cur.split_whitespace().collect::<Vec<_>>().join(" ");
            if !s.is_empty() {
                out.push(s);
            }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    out
}

/// A `for` loop over a local array: its bounds and its body.
fn for_loop<'a>(
    stmt: &'a str,
    i: usize,
    arrays: &BTreeMap<String, Vec<u16>>,
    defines: &BTreeMap<String, u64>,
) -> Result<Option<(usize, usize, &'a str)>, Error> {
    let Some(rest) = stmt.strip_prefix("for (") else {
        return Ok(None);
    };
    // The header's own `)`, past any inside it (`nitems(a)`).
    let mut depth = 1;
    let close = rest
        .char_indices()
        .find(|&(_, c)| {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            depth == 0
        })
        .map(|(k, _)| k)
        .ok_or(Error("bad for".into()))?;
    let (header, body) = (&rest[..close], rest[close + 1..].trim());
    let parts: Vec<&str> = header.split(';').map(str::trim).collect();
    let [init, cond, step] = parts[..] else {
        return err(format!("bad for header {header:?}"));
    };
    if step != "i++" {
        return err(format!("bad for step {step:?}"));
    }
    let start = match init {
        "i = 0" => 0,
        "" => i,
        _ => return err(format!("bad for init {init:?}")),
    };
    let bound = cond
        .strip_prefix("i < ")
        .ok_or_else(|| Error(format!("bad for condition {cond:?}")))?;
    let end = if let Some(arr) = bound
        .strip_prefix("nitems(")
        .and_then(|b| b.strip_suffix(')'))
    {
        arrays
            .get(arr)
            .ok_or_else(|| Error(format!("unknown array {arr}")))?
            .len()
    } else {
        usize::try_from(number(bound, defines)?).map_err(|_| Error("bound".into()))?
    };
    Ok(Some((start, end, body)))
}

/// The function `name`'s body in `source`.
fn function<'a>(source: &'a str, name: &str) -> Result<&'a str, Error> {
    let sig = format!("\n{name}(struct rge_softc *sc)");
    let at = source
        .find(&sig)
        .ok_or_else(|| Error(format!("no function {name}")))?;
    braced(source, at)
}

/// A PHY configuration function as [`Op`]s.
///
/// # Errors
///
/// Any statement that is not one of the shapes [`Op`] covers.
pub fn program(
    source: &str,
    name: &str,
    defines: &BTreeMap<String, u64>,
) -> Result<Vec<Op>, Error> {
    let body = strip_comments(function(source, name)?);
    // Local arrays first; then everything else is a statement.
    let mut arrays = BTreeMap::new();
    let mut text = body.clone();
    while let Some(at) = text.find("static const uint16_t ") {
        let decl = &text[at + "static const uint16_t ".len()..];
        let aname = decl[..decl.find('[').ok_or(Error("bad array".into()))?].to_string();
        arrays.insert(aname.clone(), u16_array(&text, &aname)?);
        let end = at + text[at..].find("};").ok_or(Error("bad array end".into()))? + 2;
        text.replace_range(at..end, "");
    }
    let mut ops = Vec::new();
    let mut i = 0usize;
    let mut pending: Option<(u16, u16)> = None;
    let n = |s: &str| u16_of(s, defines);
    for stmt in statements(&text) {
        if stmt == "uint16_t val" || stmt == "int i" {
            continue;
        }
        if let Some((reg, mask)) = pending.take() {
            let args = call(&stmt, "rge_write_phy_ocp")
                .ok_or_else(|| Error(format!("{name}: read not followed by write: {stmt}")))?;
            if args.len() != 2 || n(args[0])? != reg {
                return err(format!(
                    "{name}: read-modify-write of another register: {stmt}"
                ));
            }
            let bits = match args[1] {
                "val" => 0,
                v => n(v
                    .strip_prefix("val | ")
                    .ok_or_else(|| Error(format!("{name}: bad modify value {v}")))?)?,
            };
            ops.push(Op::Modify(reg, mask, bits));
            continue;
        }
        if let Some((start, end, body)) = for_loop(&stmt, i, &arrays, defines)? {
            let args = call(body, "rge_write_phy_ocp")
                .ok_or_else(|| Error(format!("{name}: bad loop body {body}")))?;
            let [reg, value] = args[..] else {
                return err(format!("{name}: bad loop body {body}"));
            };
            let arr = value
                .strip_suffix("[i]")
                .and_then(|a| arrays.get(a))
                .ok_or_else(|| Error(format!("{name}: bad loop value {value}")))?;
            let reg = n(reg)?;
            for k in start..end {
                let v = *arr
                    .get(k)
                    .ok_or_else(|| Error(format!("{name}: loop past its array")))?;
                ops.push(Op::Write(reg, v));
            }
            i = end;
            continue;
        }
        if let Some(rest) = stmt.strip_prefix("val = rge_read_phy_ocp(sc, ") {
            let (reg, mask) = rest
                .split_once(") & ~")
                .ok_or_else(|| Error(format!("{name}: bad read {stmt}")))?;
            pending = Some((n(reg)?, n(mask)?));
            continue;
        }
        let two = |f: &str| -> Result<Option<(u16, u16)>, Error> {
            match call(&stmt, f) {
                Some(a) if a.len() == 2 => Ok(Some((n(a[0])?, n(a[1])?))),
                Some(_) => err(format!("{name}: bad call {stmt}")),
                None => Ok(None),
            }
        };
        if let Some((r, v)) = two("rge_write_phy_ocp")? {
            ops.push(Op::Write(r, v));
        } else if let Some((r, v)) = two("RGE_PHY_SETBIT")? {
            ops.push(Op::Set(r, v));
        } else if let Some((r, v)) = two("RGE_PHY_CLRBIT")? {
            ops.push(Op::Clear(r, v));
        } else if let Some(a) = call(&stmt, "rge_patch_phy_mcu") {
            ops.push(Op::PatchMcu(a == ["1"]));
        } else if let Some(a) = call(&stmt, "rge_phy_config_mcu") {
            ops.push(Op::LoadMcu(n(a.first().copied().unwrap_or(""))?));
        } else {
            return err(format!("{name}: unrecognized statement: {stmt}"));
        }
    }
    if pending.is_some() {
        return err(format!("{name}: trailing read"));
    }
    Ok(ops)
}

/// What one chip revision needs.
#[derive(Debug)]
pub struct Chip {
    /// The Rust name's prefix (`R26_2`).
    pub name: &'static str,
    /// The MAC microcode: (register, value) pairs.
    pub mac_bps: Vec<(u16, u16)>,
    /// The PHY microcode.
    pub phy_mcu: Vec<(u16, u16)>,
    /// The PHY configuration.
    pub phy_config: Vec<Op>,
}

/// The chips `drivers/rge` supports, from FreeBSD's sources.
///
/// # Errors
///
/// Whatever the sources do not hold as expected.
pub fn chips(rgereg_h: &str, microcode_h: &str, hw_c: &str) -> Result<Vec<Chip>, Error> {
    let defs = defines(rgereg_h);
    let micro = strip_comments(microcode_h);
    let hw = strip_comments(hw_c);
    let mut out = Vec::new();
    for (name, c) in [("R26_1", "1"), ("R26_2", "2")] {
        let regs = u16_array(&micro, &format!("rtl8126_{c}_mac_bps_regs"))?;
        let vals = u16_array(&micro, &format!("rtl8126_{c}_mac_bps_vals"))?;
        if regs.len() != vals.len() {
            return err(format!(
                "rtl8126_{c}_mac_bps: {} registers, {} values",
                regs.len(),
                vals.len()
            ));
        }
        out.push(Chip {
            name,
            mac_bps: regs.into_iter().zip(vals).collect(),
            phy_mcu: pair_array(&micro, &format!("mac_r26_{c}_mcu"))?,
            phy_config: program(&hw, &format!("rge_phy_config_mac_r26_{c}"), &defs)?,
        });
    }
    Ok(out)
}

/// `drivers/rge/src/generated.rs`.
#[must_use]
pub fn render(chips: &[Chip], provenance: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "// SPDX-License-Identifier: BSD-2-Clause
//
// Copyright (c) 2019, 2020, 2023-2025 Kevin Lo <kevlo@openbsd.org>
// Copyright (c) 2025 Adrian Chadd <adrian@FreeBSD.org>
// Copyright (c) 2026 The AbyssBSD Project
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED \"AS IS\" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! Realtek's register programs for the RTL8126, generated by `cargo xtask
//! rgegen` from {provenance}. Do not edit.

use crate::program::Op;
"
    );
    for c in chips {
        let n = c.name;
        let _ = writeln!(s, "/// The {n} MAC microcode: (MAC OCP register, value).");
        let _ = writeln!(s, "pub static {n}_MAC_BPS: &[(u16, u16)] = &[");
        for (r, v) in &c.mac_bps {
            let _ = writeln!(s, "({r:#06x}, {v:#06x}),");
        }
        let _ = writeln!(s, "];\n");
        let _ = writeln!(s, "/// The {n} PHY microcode: (PHY OCP register, value).");
        let _ = writeln!(s, "pub static {n}_PHY_MCU: &[(u16, u16)] = &[");
        for (r, v) in &c.phy_mcu {
            let _ = writeln!(s, "({r:#06x}, {v:#06x}),");
        }
        let _ = writeln!(s, "];\n");
        let _ = writeln!(s, "/// The {n} PHY configuration.");
        let _ = writeln!(s, "pub static {n}_PHY_CONFIG: &[Op] = &[");
        for op in &c.phy_config {
            let _ = match *op {
                Op::Write(r, v) => writeln!(s, "Op::Write({r:#06x}, {v:#06x}),"),
                Op::Set(r, v) => writeln!(s, "Op::Set({r:#06x}, {v:#06x}),"),
                Op::Clear(r, v) => writeln!(s, "Op::Clear({r:#06x}, {v:#06x}),"),
                Op::Modify(r, m, v) => writeln!(s, "Op::Modify({r:#06x}, {m:#06x}, {v:#06x}),"),
                Op::PatchMcu(on) => writeln!(s, "Op::PatchMcu({on}),"),
                Op::LoadMcu(v) => writeln!(s, "Op::LoadMcu({v:#06x}),"),
            };
        }
        let _ = writeln!(s, "];\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const HW: &str = "
static void
rge_phy_config_mac_rx(struct rge_softc *sc)
{
	uint16_t val;
	int i;
	static const uint16_t vals[] =
	    { 0x0001, 0x0002, 0x0003 };

	rge_phy_config_mcu(sc, VER);
	/* comment */
	RGE_PHY_SETBIT(sc, 0xa442, 0x0800);
	rge_write_phy_ocp(sc, 0xa436, 0x8183);
	val = rge_read_phy_ocp(sc, 0xa438) & ~0xff00;
	rge_write_phy_ocp(sc, 0xa438, val | 0x5900);
	for (i = 0; i < 2; i++)
		rge_write_phy_ocp(sc, 0xb87e, vals[i]);
	RGE_PHY_CLRBIT(sc, 0xa5b4, 0x8000);
	for (; i < nitems(vals); i++)
		rge_write_phy_ocp(sc, 0xb87e, vals[i]);
	rge_patch_phy_mcu(sc, 1);
}
";

    #[test]
    fn programs() {
        let defs = defines("#define VER\t0x0060\n");
        let ops = program(HW, "rge_phy_config_mac_rx", &defs).unwrap();
        assert_eq!(
            ops,
            [
                Op::LoadMcu(0x60),
                Op::Set(0xa442, 0x0800),
                Op::Write(0xa436, 0x8183),
                Op::Modify(0xa438, 0xff00, 0x5900),
                Op::Write(0xb87e, 1),
                Op::Write(0xb87e, 2),
                Op::Clear(0xa5b4, 0x8000),
                Op::Write(0xb87e, 3),
                Op::PatchMcu(true),
            ]
        );
        // Anything else is refused.
        let bad = HW.replace("RGE_PHY_CLRBIT", "RGE_MAC_CLRBIT");
        assert!(program(&bad, "rge_phy_config_mac_rx", &defs).is_err());
    }

    #[test]
    fn tables() {
        let h = "static const uint16_t r[] = {\n\t0xf800, 0xf802,\n};\n\
                 static const struct rge_hw_regaddr_array m[] = {\n\t{ 0xa436, 0x8023 }, { 0xa438, 0x4701 },\n};";
        assert_eq!(u16_array(h, "r").unwrap(), [0xf800, 0xf802]);
        assert_eq!(
            pair_array(h, "m").unwrap(),
            [(0xa436, 0x8023), (0xa438, 0x4701)]
        );
    }
}
