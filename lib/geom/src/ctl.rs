// SPDX-License-Identifier: BSD-2-Clause
//! A control request (`struct gctl_req`) as a class's `ctlreq` method
//! sees it once `/dev/geom.ctl` has copied it in: named arguments, each
//! with its flags and bytes, and the error text and number the request
//! answers with.
//!
//! As `geom_ctl.c` does, only the first error is kept ([`Request::error`]
//! finishes the text), a value is written back only to an argument the
//! caller made writable ([`Request::set_param`]), and an ASCII argument
//! must end in its NUL ([`Request::ascii`]).
//!
//! Behavior reference: sys/geom/geom_ctl.c (`gctl_error`, `gctl_msg`,
//! `gctl_set_param`, `gctl_get_param_flags`, `gctl_get_asciiparam`).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use abi::errno::Errno;

/// The caller may read the value (the kernel copies it in).
pub const GCTL_PARAM_RD: u32 = 1;
/// The caller may have the value written (the kernel copies it out).
pub const GCTL_PARAM_WR: u32 = 2;
/// Both.
pub const GCTL_PARAM_RW: u32 = GCTL_PARAM_RD | GCTL_PARAM_WR;
/// The value is a NUL-terminated string.
pub const GCTL_PARAM_ASCII: u32 = 4;

/// One argument: its name (without the NUL), flags, and value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arg {
    pub name: Vec<u8>,
    pub flag: u32,
    /// The value: copied in for a readable argument, zeros (its length)
    /// for a write-only one.
    pub value: Vec<u8>,
    /// Written by the class, so copied back out (`GCTL_PARAM_CHANGED`).
    pub changed: bool,
}

impl Arg {
    /// An argument named `name` with `flag` and `value`.
    #[must_use]
    pub fn new(name: &str, flag: u32, value: &[u8]) -> Self {
        Self {
            name: name.as_bytes().to_vec(),
            flag,
            value: value.to_vec(),
            changed: false,
        }
    }

    /// A readable string argument (`gctl_ro_param(r, name, -1, s)`).
    #[must_use]
    pub fn ascii(name: &str, value: &str) -> Self {
        let mut v = value.as_bytes().to_vec();
        v.push(0);
        Self {
            name: name.as_bytes().to_vec(),
            flag: GCTL_PARAM_RD | GCTL_PARAM_ASCII,
            value: v,
            changed: false,
        }
    }
}

/// A request in the kernel.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub args: Vec<Arg>,
    /// The error text so far (`serror`).
    text: String,
    /// The text is final: nothing more is added (`sbuf_done`).
    finished: bool,
    /// The request's error (`nerror`).
    pub nerror: Option<Errno>,
}

/// Bytes up to the first NUL, as C reads a string.
#[must_use]
pub fn c_str(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |n| &b[..n])
}

/// `%s` of C string bytes (not UTF-8 is shown as replacement characters).
#[derive(Clone, Copy, Debug)]
pub struct Show<'a>(pub &'a [u8]);

impl fmt::Display for Show<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in c_str(self.0).utf8_chunks() {
            f.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                f.write_char(char::REPLACEMENT_CHARACTER)?;
            }
        }
        Ok(())
    }
}

impl Request {
    /// A request with `args`.
    #[must_use]
    pub fn new(args: Vec<Arg>) -> Self {
        Self {
            args,
            ..Self::default()
        }
    }

    /// `gctl_error`: records `msg` as the request's error text, if none is
    /// final yet, and makes the request fail (`EINVAL` unless it already
    /// had an error). Returns the request's error.
    pub fn error(&mut self, msg: fmt::Arguments<'_>) -> Errno {
        if self.finished {
            return *self.nerror.get_or_insert(Errno::EEXIST);
        }
        let e = *self.nerror.get_or_insert(Errno::EINVAL);
        if !self.text.is_empty() {
            self.text.push_str("\n\t");
        }
        let _ = self.text.write_fmt(msg);
        self.finished = true;
        e
    }

    /// `gctl_msg`: adds a message (an error if `errno` is given) without
    /// finishing the text; [`Self::post_messages`] finishes it.
    pub fn msg(&mut self, errno: Option<Errno>, msg: fmt::Arguments<'_>) {
        if self.finished {
            return;
        }
        if self.nerror.is_none() {
            self.nerror = errno;
        }
        if !self.text.is_empty() {
            self.text.push_str("\n\t");
        }
        let _ = self.text.write_fmt(msg);
    }

    /// `gctl_post_messages`: the text is final.
    pub fn post_messages(&mut self) {
        self.finished = true;
    }

    /// The text to copy back to the caller, if final.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.finished.then_some(self.text.as_str())
    }

    /// `gctl_get_param_flags`: the first argument named `name` with at
    /// least `flags`.
    #[must_use]
    pub fn param_flags(&self, name: &str, flags: u32) -> Option<&Arg> {
        self.args
            .iter()
            .find(|a| a.name == name.as_bytes() && a.flag & flags == flags)
    }

    /// `gctl_get_param`: a readable argument's value.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&[u8]> {
        self.param_flags(name, GCTL_PARAM_RD)
            .map(|a| a.value.as_slice())
    }

    /// `gctl_get_asciiparam`: a readable argument's string, without its
    /// NUL. One with no length, or not NUL-terminated, is an error (and
    /// `None`), as is (silently) none at all.
    pub fn ascii(&mut self, name: &str) -> Option<Vec<u8>> {
        let v = self.param(name)?.to_vec();
        if v.is_empty() {
            self.error(format_args!("Argument without length ({name})"));
            return None;
        }
        if v.last() != Some(&0) {
            self.error(format_args!("Unterminated argument ({name})"));
            return None;
        }
        Some(c_str(&v).to_vec())
    }

    /// `gctl_set_param`: writes `data` to the writable argument `name`,
    /// truncated to its length (`ENOSPC`). `EPERM` if it is not
    /// writable, `EINVAL` if there is none.
    pub fn set_param(&mut self, name: &str, data: &[u8]) -> Result<(), Errno> {
        let a = self
            .args
            .iter_mut()
            .find(|a| a.name == name.as_bytes())
            .ok_or(Errno::EINVAL)?;
        if a.flag & GCTL_PARAM_WR == 0 {
            return Err(Errno::EPERM);
        }
        a.changed = true;
        let n = data.len().min(a.value.len());
        a.value[..n].copy_from_slice(&data[..n]);
        if n < data.len() {
            return Err(Errno::ENOSPC);
        }
        Ok(())
    }

    /// `gctl_set_param(req, "output", s, strlen(s) + 1)`: sets the text a
    /// class reports, if the caller asked for it.
    pub fn set_output(&mut self, s: &str) {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        let _ = self.set_param("output", &v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn only_the_first_error_counts() {
        let mut r = Request::default();
        assert_eq!(r.message(), None);
        assert_eq!(
            r.error(format_args!("{} geom '{}'", 17, "md0")),
            Errno::EINVAL
        );
        assert_eq!(r.error(format_args!("second")), Errno::EINVAL);
        assert_eq!(r.message(), Some("17 geom 'md0'"));
        let mut r = Request::default();
        r.msg(None, format_args!("one"));
        r.msg(Some(Errno::EBUSY), format_args!("two"));
        r.post_messages();
        assert_eq!(r.message(), Some("one\n\ttwo"));
        assert_eq!(r.nerror, Some(Errno::EBUSY));
    }

    #[test]
    fn params() {
        let mut r = Request::new(vec![
            Arg::ascii("class", "PART"),
            Arg::new("bad", GCTL_PARAM_RD, b"x"),
            Arg::new("empty", GCTL_PARAM_RD, b""),
            Arg::new("output", GCTL_PARAM_WR | GCTL_PARAM_ASCII, &[0; 8]),
        ]);
        assert_eq!(r.ascii("class").as_deref(), Some(&b"PART"[..]));
        assert_eq!(r.ascii("none"), None);
        assert_eq!(r.message(), None);
        assert_eq!(r.ascii("bad"), None);
        assert_eq!(r.message(), Some("Unterminated argument (bad)"));
        // Write-only arguments are not read.
        assert_eq!(r.param("output"), None);
        assert_eq!(r.set_param("class", b"x"), Err(Errno::EPERM));
        assert_eq!(r.set_param("none", b"x"), Err(Errno::EINVAL));
        assert_eq!(r.set_param("output", b"0123456789"), Err(Errno::ENOSPC));
        assert_eq!(r.args[3].value, b"01234567");
        assert!(r.args[3].changed);
        assert_eq!(Show(b"a\xffb\0c").to_string(), "a\u{fffd}b");
    }

    use alloc::string::ToString;
}
