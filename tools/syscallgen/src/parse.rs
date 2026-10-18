// SPDX-License-Identifier: BSD-2-Clause
//! The `syscalls.master` grammar.
//!
//! ```text
//! NUMBER[-NUMBER] <tab> AUDIT <tab> TYPE[|FLAG...] { decl }    multi-line
//! NUMBER[-NUMBER] <tab> AUDIT <tab> OBSOL|RESERVED|UNIMPL [comment]
//! ```
//!
//! Lines starting with `;` are comments; `#` and `%%` lines are
//! preprocessor material for the C generators and carry no syscalls.

use std::fmt;

/// A FreeBSD compatibility generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompatLevel {
    /// `COMPAT`: 4.3BSD interfaces (the `o` prefixed calls).
    Bsd43,
    /// `COMPATn`: FreeBSD n.x interfaces.
    FreeBsd(u8),
}

/// What a syscall slot holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `STD`, `NOARGS`, `NOPROTO`: a live syscall.
    Std,
    /// `NOSTD`: a live syscall implemented by a loadable module in FreeBSD.
    NoStd,
    /// `NODEF`: a live syscall without generated prototypes.
    NoDef,
    /// `SYSMUX`: a multiplexer (`syscall`, `__syscall`) handled in MD code.
    SysMux,
    /// `COMPAT*`: only present for old binaries.
    Compat(CompatLevel),
    /// `OBSOL`: removed.
    Obsolete,
    /// `RESERVED`: reserved for local or vendor use.
    Reserved,
    /// `UNIMPL`: placeholder.
    Unimplemented,
    /// `NODEF|NOTSTATIC` without a prototype: a slot FreeBSD hands to
    /// dynamically loaded syscalls (`lkmnosys`).
    ModuleSlot,
}

impl Kind {
    /// Whether current FreeBSD binaries can call this slot.
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Std | Self::NoStd | Self::NoDef | Self::SysMux)
    }
}

/// Modifier flags ORed into the type column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// `CAPENABLED`: allowed in Capsicum capability mode.
    pub cap_enabled: bool,
    /// `NORETURN`: does not return to the caller.
    pub no_return: bool,
    /// `NOTSTATIC`: loadable in FreeBSD.
    pub not_static: bool,
    /// `NOLIB`: no libc/libsys stub.
    pub no_lib: bool,
}

/// One argument of a declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arg {
    /// C type, e.g. `const char *`.
    pub ty: String,
    /// Parameter name.
    pub name: String,
    /// SAL and `_Contains_` annotations, space separated.
    pub annotations: String,
}

/// A syscall prototype.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decl {
    /// C return type.
    pub ret: String,
    /// Function name.
    pub name: String,
    /// Fixed arguments.
    pub args: Vec<Arg>,
    /// Whether the prototype ends in `...`.
    pub variadic: bool,
}

/// One numbered slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Syscall number.
    pub number: u32,
    /// Audit event name.
    pub audit: String,
    /// What the slot holds.
    pub kind: Kind,
    /// Modifier flags.
    pub flags: Flags,
    /// The prototype, for slots that have one.
    pub decl: Option<Decl>,
    /// Trailing text on `OBSOL`/`RESERVED`/`UNIMPL` lines (often a name).
    pub comment: Option<String>,
}

/// Why the file could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number.
    pub line: usize,
    /// What was wrong.
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "syscalls.master:{}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError {
        line,
        message: message.into(),
    })
}

/// Parses the whole file into entries ordered by number, checking that
/// numbering is contiguous from zero.
pub fn parse(src: &str) -> Result<Vec<Entry>, ParseError> {
    let mut entries = Vec::new();
    let mut lines = src.lines().enumerate().map(|(i, l)| (i + 1, l));
    while let Some((lineno, line)) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with(';')
            || trimmed.starts_with('#')
            || trimmed.starts_with("%%")
        {
            continue;
        }
        let Some(([num, audit, ty], rest)) = columns::<3>(trimmed) else {
            return err(lineno, "expected NUMBER AUDIT TYPE");
        };
        let (first, last) = parse_range(num).ok_or_else(|| ParseError {
            line: lineno,
            message: format!("bad syscall number `{num}`"),
        })?;
        let (kind, flags) = parse_type(ty).map_err(|m| ParseError {
            line: lineno,
            message: m,
        })?;

        let (decl, comment) = if let Some(after) = rest.strip_prefix('{') {
            let mut body = String::from(after);
            loop {
                let Some((_, l)) = lines.next() else {
                    return err(lineno, "unterminated `{`");
                };
                let l = l.trim();
                if l.starts_with('}') {
                    if l.len() > 1 {
                        return err(lineno, "alternate names after `}` are not supported");
                    }
                    break;
                }
                body.push(' ');
                body.push_str(l);
            }
            let decl = parse_decl(&body).map_err(|m| ParseError {
                line: lineno,
                message: m,
            })?;
            (Some(decl), None)
        } else {
            (None, (!rest.is_empty()).then(|| rest.to_owned()))
        };
        let kind = match (kind, &decl) {
            (Kind::NoDef, None) if flags.not_static => Kind::ModuleSlot,
            (k, None) if k.is_live() => {
                return err(lineno, "live syscall without a declaration");
            }
            (k, _) => k,
        };

        for number in first..=last {
            let expected = u32::try_from(entries.len()).unwrap_or(u32::MAX);
            if number != expected {
                return err(
                    lineno,
                    format!("syscall {number} out of order (expected {expected})"),
                );
            }
            entries.push(Entry {
                number,
                audit: audit.to_owned(),
                kind,
                flags,
                decl: decl.clone(),
                comment: comment.clone(),
            });
        }
    }
    Ok(entries)
}

/// Splits off the first `N` whitespace-separated columns, returning them and
/// the trimmed remainder of the line.
fn columns<const N: usize>(line: &str) -> Option<([&str; N], &str)> {
    let mut cols = [""; N];
    let mut rest = line;
    for col in &mut cols {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        *col = &rest[..end];
        rest = &rest[end..];
    }
    Some((cols, rest.trim()))
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once('-') {
        Some((a, b)) => {
            let (a, b) = (a.parse().ok()?, b.parse().ok()?);
            (a <= b).then_some((a, b))
        }
        None => {
            let n = s.parse().ok()?;
            Some((n, n))
        }
    }
}

fn parse_type(ty: &str) -> Result<(Kind, Flags), String> {
    let mut flags = Flags::default();
    let mut compat = None;
    let mut base = None;
    for tok in ty.split('|') {
        match tok {
            "STD" | "NOARGS" | "NOPROTO" => base = base.or(Some(Kind::Std)),
            "NOSTD" => base = Some(Kind::NoStd),
            "NODEF" => base = Some(Kind::NoDef),
            "SYSMUX" => base = Some(Kind::SysMux),
            "OBSOL" => base = Some(Kind::Obsolete),
            "RESERVED" => base = Some(Kind::Reserved),
            "UNIMPL" => base = Some(Kind::Unimplemented),
            "CAPENABLED" => flags.cap_enabled = true,
            "NORETURN" => flags.no_return = true,
            "NOTSTATIC" => flags.not_static = true,
            "NOLIB" => flags.no_lib = true,
            "COMPAT" => compat = Some(CompatLevel::Bsd43),
            t => match t.strip_prefix("COMPAT").map(str::parse::<u8>) {
                Some(Ok(n)) => compat = Some(CompatLevel::FreeBsd(n)),
                _ => return Err(format!("unknown type `{t}`")),
            },
        }
    }
    let kind = match (compat, base) {
        (Some(level), _) => Kind::Compat(level),
        (None, Some(kind)) => kind,
        (None, None) => return Err(format!("no base type in `{ty}`")),
    };
    Ok((kind, flags))
}

/// Parses `ret name(args);`.
fn parse_decl(body: &str) -> Result<Decl, String> {
    let body = body.trim();
    let open = body.find('(').ok_or("declaration has no `(`")?;
    let close = matching_paren(body, open).ok_or("unbalanced parentheses")?;
    if body[close + 1..].trim() != ";" {
        return Err(format!(
            "unexpected text after declaration: `{}`",
            &body[close + 1..]
        ));
    }
    let head: Vec<&str> = body[..open].split_whitespace().collect();
    let (name, ret) = head.split_last().ok_or("declaration has no name")?;
    let (stars, name) = split_stars(name);
    let ret = format!("{}{}", ret.join(" "), stars);
    if ret.is_empty() {
        return Err("declaration has no return type".into());
    }

    let mut args = Vec::new();
    let mut variadic = false;
    let inner = body[open + 1..close].trim();
    if inner != "void" && !inner.is_empty() {
        let parts = split_top_level(inner);
        let last = parts.len() - 1;
        for (i, raw) in parts.into_iter().enumerate() {
            let raw = raw.trim();
            // FreeBSD tolerates a trailing comma (fhlinkat has one).
            if raw.is_empty() && i == last && i > 0 {
                continue;
            }
            if raw == "..." {
                variadic = true;
                continue;
            }
            args.push(parse_arg(raw)?);
        }
    }
    Ok(Decl {
        ret,
        name: name.to_owned(),
        args,
        variadic,
    })
}

fn parse_arg(raw: &str) -> Result<Arg, String> {
    let tokens = tokens(raw);
    let (annotations, rest): (Vec<&str>, Vec<&str>) = tokens.iter().partition(|t| {
        t.starts_with('_') && t.as_bytes().get(1).is_some_and(u8::is_ascii_uppercase)
    });
    let (name, ty) = rest
        .split_last()
        .ok_or_else(|| format!("argument `{raw}` has no name"))?;
    let (stars, name) = split_stars(name);
    if ty.is_empty() {
        return Err(format!("argument `{raw}` has no type"));
    }
    let mut ty = ty.join(" ");
    if !stars.is_empty() {
        ty.push(' ');
        ty.push_str(stars);
    }
    Ok(Arg {
        ty,
        name: name.to_owned(),
        annotations: annotations.join(" "),
    })
}

fn split_stars(s: &str) -> (&str, &str) {
    let n = s.len() - s.trim_start_matches('*').len();
    s.split_at(n)
}

/// Splits on whitespace, keeping parenthesised groups such as
/// `_In_reads_(n * 2)` in one token.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if c.is_whitespace() && depth == 0 => {
                if let Some(st) = start.take() {
                    out.push(&s[st..i]);
                }
                continue;
            }
            _ => {}
        }
        start.get_or_insert(i);
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

fn split_top_level(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
; comment
#include <sys/param.h>
%%ABI_HEADERS%%

0\tAUE_NULL\tSYSMUX {
\t\tint syscall(
\t\t    int number,
\t\t    ...
\t\t);
\t}
1\tAUE_EXIT\tSTD|CAPENABLED|NORETURN {
\t\tvoid _exit(
\t\t    int rval
\t\t);
\t}
2\tAUE_FORK\tSTD|CAPENABLED {
\t\tint fork(void);
\t}
3\tAUE_READ\tSTD|CAPENABLED {
\t\tssize_t read(
\t\t    int fd,
\t\t    _Out_writes_bytes_(nbyte) void *buf,
\t\t    size_t nbyte
\t\t);
\t}
4\tAUE_CREAT\tCOMPAT {
\t\tint creat(
\t\t    _In_z_ const char *path,
\t\t    int mode
\t\t);
\t}
5\tAUE_NULL\tOBSOL\texecv
6-7\tAUE_NULL\tRESERVED
8\tAUE_NULL\tNODEF|NOTSTATIC\tlkmnosys lkmnosys nosys_args int
9\tAUE_NULL\tCOMPAT11|NOARGS {
\t\tint nlm(
\t\t    _In_reads_(addr_count) char **addrs
\t\t);
\t}
";

    #[test]
    fn parses_sample() {
        let e = parse(SAMPLE).unwrap();
        assert_eq!(e.len(), 10);
        assert_eq!(e[0].kind, Kind::SysMux);
        assert!(e[0].decl.as_ref().unwrap().variadic);
        assert!(e[1].flags.no_return && e[1].flags.cap_enabled);
        assert_eq!(e[1].decl.as_ref().unwrap().ret, "void");
        assert!(e[2].decl.as_ref().unwrap().args.is_empty());
        let read = e[3].decl.as_ref().unwrap();
        assert_eq!(read.args[1].ty, "void *");
        assert_eq!(read.args[1].name, "buf");
        assert_eq!(read.args[1].annotations, "_Out_writes_bytes_(nbyte)");
        assert_eq!(e[4].kind, Kind::Compat(CompatLevel::Bsd43));
        assert_eq!(e[5].kind, Kind::Obsolete);
        assert_eq!(e[5].comment.as_deref(), Some("execv"));
        assert!(e[6..8].iter().all(|x| x.kind == Kind::Reserved));
        assert_eq!(e[8].kind, Kind::ModuleSlot);
        assert_eq!(e[9].kind, Kind::Compat(CompatLevel::FreeBsd(11)));
        assert_eq!(e[9].decl.as_ref().unwrap().args[0].ty, "char **");
    }

    #[test]
    fn tolerates_trailing_comma() {
        let d = parse_decl("int fhlinkat(int tofd, const char *to,);").unwrap();
        assert_eq!(d.args.len(), 2);
        assert!(parse_decl("int f(int a,,int b);").is_err());
    }

    #[test]
    fn rejects_gaps() {
        let src = "0\tAUE_NULL\tRESERVED\n2\tAUE_NULL\tRESERVED\n";
        assert_eq!(parse(src).unwrap_err().line, 2);
    }

    #[test]
    fn rejects_unknown_type() {
        assert!(parse("0\tAUE_NULL\tBOGUS\n").is_err());
    }
}
