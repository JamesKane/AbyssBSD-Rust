// SPDX-License-Identifier: BSD-2-Clause
//! Names as GEOM makes them: the LABEL class's provider names for a GPT
//! partition (`gpt/<label>`, `gptid/<uuid>`), a GPT name in UTF-8, and
//! the escaping of text in `kern.geom.confxml`.
//!
//! Behavior reference: sys/geom/label/g_label.c (`g_label_mangle_name`,
//! `g_label_is_name_ok`, `g_label_create`), g_label_gpt.c,
//! sys/geom/part/g_part_gpt.c (`g_gpt_printf_utf16`),
//! sys/geom/geom_dump.c (`g_conf_cat_escaped`).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// The longest label `g_label_mangle_name` keeps (its buffer is 128
/// bytes, the terminating NUL included); a longer one is dropped.
const LABEL_MAX: usize = 127;
/// The longest provider name `g_label_create` makes (`name[64]`).
const NAME_MAX: usize = 63;

/// Appends `s` to `out` as `g_conf_cat_escaped` does: XML's special
/// characters and bytes past `~` as `&#xHH;`, other control characters
/// (but tab, newline, and return) as `?`.
pub fn escape(out: &mut String, s: &str) {
    for &c in s.as_bytes() {
        if matches!(c, b'&' | b'<' | b'>' | b'\'' | b'"') || c > 0x7e {
            let _ = write!(out, "&#x{c:X};");
        } else if matches!(c, b'\t' | b'\n' | b'\r') || c > 0x1f {
            out.push(char::from(c));
        } else {
            out.push('?');
        }
    }
}

/// A GPT partition name (UTF-16LE code units, up to the first NUL) in
/// UTF-8, as `g_gpt_printf_utf16` writes it: a byte-order mark is
/// skipped (a swapped one swaps the order of the rest), and a surrogate
/// not followed by its other half becomes U+FFFD, the unit after it
/// consumed.
#[must_use]
pub fn utf16_name(units: &[u16]) -> String {
    let mut out = String::new();
    let mut swapped = false;
    let mut i = 0;
    let unit = |u: u16, swapped: bool| if swapped { u.swap_bytes() } else { u };
    while i < units.len() && units[i] != 0 {
        let mut ch = u32::from(unit(units[i], swapped));
        i += 1;
        if ch & 0xf800 == 0xd800 {
            let low = if i < units.len() {
                let c = unit(units[i], swapped);
                i += 1;
                c
            } else {
                0xfffd
            };
            ch = if ch & 0x400 == 0 && low & 0xfc00 == 0xdc00 {
                ((ch & 0x3ff) << 10) + u32::from(low & 0x3ff) + 0x10000
            } else {
                0xfffd
            };
        } else if ch == 0xfffe {
            swapped = !swapped;
            continue;
        } else if ch == 0xfeff {
            continue;
        }
        // Every value here is a scalar value: surrogates were combined
        // or replaced above.
        out.push(char::from_u32(ch).unwrap_or('\u{fffd}'));
    }
    out
}

/// `isspace` in the kernel's libkern.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// A label as `g_label_mangle_name` makes it a file name: blanks trimmed
/// from both ends, and blanks, unprintable bytes, `"`, and `%` written as
/// `%HH`. `None` if nothing is left, or the result is too long.
#[must_use]
pub fn mangle(label: &str) -> Option<String> {
    let b = label.as_bytes();
    let end = b.iter().rposition(|&c| !is_space(c))? + 1;
    let start = b.iter().position(|&c| !is_space(c))?;
    let mut out = String::new();
    for &c in &b[start..end] {
        if !(b' '..=b'~').contains(&c) || is_space(c) || c == b'"' || c == b'%' {
            let _ = write!(out, "%{c:02X}");
        } else {
            out.push(char::from(c));
        }
    }
    (out.len() <= LABEL_MAX).then_some(out)
}

/// Whether `g_label_is_name_ok` takes `label`: no `..` component that
/// could climb out of the directory.
#[must_use]
pub fn name_ok(label: &str) -> bool {
    !(label.starts_with("../") || label.contains("/../") || label.ends_with("/.."))
}

/// The provider a label method names (`gpt/`, `gptid/` and the label),
/// if `g_label_create` makes one: the label mangled, acceptable, and the
/// whole name short enough.
#[must_use]
pub fn provider_name(prefix: &str, label: &str) -> Option<String> {
    let label = mangle(label)?;
    if !name_ok(&label) {
        return None;
    }
    let mut name = String::from(prefix);
    name.push_str(&label);
    (name.len() <= NAME_MAX).then_some(name)
}

/// The name the `gpt` label method gives a partition named `units`.
#[must_use]
pub fn gpt(units: &[u16]) -> Option<String> {
    provider_name("gpt/", &utf16_name(units))
}

/// The name the `gptid` label method gives a partition with GUID `uuid`
/// (its text form, in lower case).
#[must_use]
pub fn gptid(uuid: &str) -> Option<String> {
    provider_name("gptid/", uuid)
}

/// `units` as UTF-16 code units, for tests and callers holding text.
#[must_use]
pub fn to_units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_as_dumpconf_does() {
        let mut s = String::new();
        escape(&mut s, "a<b>&'\"\u{e9}\x01\tz");
        assert_eq!(s, "a&#x3C;b&#x3E;&#x26;&#x27;&#x22;&#xC3;&#xA9;?\tz");
    }

    #[test]
    fn names_from_utf16() {
        assert_eq!(utf16_name(&to_units("rootfs0")), "rootfs0");
        // Stops at the first NUL.
        assert_eq!(utf16_name(&[0x61, 0, 0x62]), "a");
        // A pair; a lone high surrogate eats the next unit.
        assert_eq!(utf16_name(&[0xd83d, 0xde00]), "\u{1f600}");
        assert_eq!(utf16_name(&[0xd83d, 0x41, 0x42]), "\u{fffd}B");
        // Byte-order marks: skipped, and a swapped one swaps the rest.
        assert_eq!(utf16_name(&[0xfeff, 0x41]), "A");
        assert_eq!(utf16_name(&[0xfffe, 0x4100, 0x4200]), "AB");
    }

    #[test]
    fn mangles_as_glabel_does() {
        assert_eq!(mangle("data disk").as_deref(), Some("data%20disk"));
        assert_eq!(
            mangle("  EFI system partition  ").as_deref(),
            Some("EFI%20system%20partition")
        );
        assert_eq!(mangle("50%\"").as_deref(), Some("50%25%22"));
        assert_eq!(mangle("\u{e9}").as_deref(), Some("%C3%A9"));
        assert_eq!(mangle("   "), None);
        assert_eq!(mangle(""), None);
        assert!(mangle(&"a".repeat(127)).is_some());
        assert_eq!(mangle(&"a".repeat(128)), None);
    }

    #[test]
    fn provider_names() {
        assert_eq!(gpt(&to_units("efiboot0")).as_deref(), Some("gpt/efiboot0"));
        assert_eq!(gpt(&[]), None);
        assert_eq!(gpt(&to_units("../etc")), None);
        assert_eq!(gpt(&to_units("a/../b")), None);
        assert_eq!(gpt(&to_units("a/..")), None);
        assert_eq!(gpt(&to_units("a/b")).as_deref(), Some("gpt/a/b"));
        // `gpt/` and 59 more bytes fit `name[64]`; 60 do not.
        assert!(gpt(&to_units(&"x".repeat(36))).is_some());
        assert!(provider_name("gpt/", &"x".repeat(59)).is_some());
        assert!(provider_name("gpt/", &"x".repeat(60)).is_none());
        assert_eq!(
            gptid("01010101-0101-0101-0101-010101010101").as_deref(),
            Some("gptid/01010101-0101-0101-0101-010101010101")
        );
    }
}
