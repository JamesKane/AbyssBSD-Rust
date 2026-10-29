// SPDX-License-Identifier: BSD-2-Clause
//! devfs rules: numbered rulesets of rules that hide, show, or set the
//! owner, group, and mode of the nodes a devfs mount shows, matched by
//! path pattern or device kind (`devfs(8)`).
//!
//! - Ruleset 0 is the empty default: it holds no rules and is never
//!   stored. Others exist while they hold rules, are used by a mount, or
//!   are included by a rule; the last such reference going takes them
//!   away.
//! - Rules are kept by number in their ruleset; rule number 0 asks for
//!   the last rule's number plus 100 (100 in an empty set).
//! - A rule including a ruleset runs it, as deep as the caller says
//!   (FreeBSD's `vfs.devfs.rule_depth`): an include loop is cut there
//!   rather than refused.
//!
//! This is the rules alone; the kernel's devfs keeps each mount's nodes
//! and runs the rules on them.
//!
//! Behavior reference: sys/fs/devfs/devfs_rule.c, sys/libkern/fnmatch.c.

use alloc::vec::Vec;

use abi::devfs::{
    DEVFS_MAGIC, DRA_BACTS, DRA_GID, DRA_INCSET, DRA_MODE, DRA_UID, DRB_HIDE, DRB_UNHIDE,
    DRC_DSWFLAGS, DRC_PATHPTRN, DevfsRule, mkrid, rid2rn, rid2rsn,
};
use abi::errno::Errno;

/// A node as a rule sees and changes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Its path from the mount's root, for a device, a symbolic link, or
    /// a directory but the root; `None` for what no pattern matches (the
    /// root, `.`, `..`).
    pub path: Option<&'a [u8]>,
    /// A device's `d_flags`; `None` for anything else.
    pub dswflags: Option<u32>,
    /// Hidden (`DE_WHITEOUT`).
    pub hidden: bool,
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
}

#[derive(Debug)]
struct Ruleset {
    number: u16,
    /// By rule number.
    rules: Vec<DevfsRule>,
    /// Mounts using it and rules including it.
    refs: u32,
}

/// Every ruleset but 0, by number.
#[derive(Debug, Default)]
pub struct Rules {
    sets: Vec<Ruleset>,
}

/// `rid` with ruleset 0 meaning `current` (`devfs_rid_input`).
#[must_use]
pub fn rid_input(rid: u32, current: u16) -> u32 {
    if rid2rsn(rid) == 0 {
        mkrid(current, rid2rn(rid))
    } else {
        rid
    }
}

/// Checks a rule from the caller and resolves its ruleset
/// (`devfs_rule_input`).
///
/// # Errors
///
/// `ERPCMISMATCH` if it is not of this version.
pub fn rule_input(r: &mut DevfsRule, current: u16) -> Result<(), Errno> {
    if r.dr_magic != DEVFS_MAGIC {
        return Err(Errno::ERPCMISMATCH);
    }
    r.dr_id = rid_input(r.dr_id, current);
    Ok(())
}

impl Rules {
    #[must_use]
    pub const fn new() -> Self {
        Self { sets: Vec::new() }
    }

    fn at(&self, n: u16) -> Result<usize, usize> {
        self.sets.binary_search_by_key(&n, |s| s.number)
    }

    fn set(&self, n: u16) -> Option<&Ruleset> {
        self.at(n).ok().map(|i| &self.sets[i])
    }

    fn set_mut(&mut self, n: u16) -> Option<&mut Ruleset> {
        self.at(n).ok().map(|i| &mut self.sets[i])
    }

    /// Ruleset `n`, made if it does not exist (`devfs_ruleset_create`).
    fn create(&mut self, n: u16) -> &mut Ruleset {
        let i = match self.at(n) {
            Ok(i) => i,
            Err(i) => {
                self.sets.insert(
                    i,
                    Ruleset {
                        number: n,
                        rules: Vec::new(),
                        refs: 0,
                    },
                );
                i
            }
        };
        &mut self.sets[i]
    }

    /// Drops ruleset `n` if nothing holds it (`devfs_ruleset_reap`).
    fn reap(&mut self, n: u16) {
        if let Ok(i) = self.at(n)
            && self.sets[i].rules.is_empty()
            && self.sets[i].refs == 0
        {
            self.sets.remove(i);
        }
    }

    /// Whether ruleset `n` exists (`devfs_ruleset_bynum`).
    #[must_use]
    pub fn exists(&self, n: u16) -> bool {
        self.set(n).is_some()
    }

    /// The rule with id `rid` (`devfs_rule_byid`).
    #[must_use]
    pub fn get(&self, rid: u32) -> Option<&DevfsRule> {
        self.set(rid2rsn(rid))?
            .rules
            .iter()
            .find(|r| rid2rn(r.dr_id) == rid2rn(rid))
    }

    /// `DEVFSIO_RADD` of a rule [`rule_input`] took.
    ///
    /// # Errors
    ///
    /// `EEXIST` if its id is taken, `EIO` for ruleset 0, `ESRCH` if it
    /// includes a ruleset that does not exist, `ERANGE` if numbering runs
    /// out.
    pub fn add(&mut self, r: &DevfsRule) -> Result<(), Errno> {
        if self.get(r.dr_id).is_some() {
            return Err(Errno::EEXIST);
        }
        let rsnum = rid2rsn(r.dr_id);
        if rsnum == 0 {
            return Err(Errno::EIO);
        }
        // `devfs_rule_insert`: the include is checked before anything
        // changes.
        let inc = r.dr_iacts & DRA_INCSET != 0;
        if inc && !self.exists(r.dr_incset) {
            return Err(Errno::ESRCH);
        }
        let ds = self.create(rsnum);
        let mut rn = rid2rn(r.dr_id);
        if rn == 0 {
            match ds.rules.last().map(|l| rid2rn(l.dr_id)) {
                None => rn = 100,
                Some(last) => match last.checked_add(100) {
                    Some(n) => rn = n,
                    None => {
                        self.reap(rsnum);
                        return Err(Errno::ERANGE);
                    }
                },
            }
        }
        let mut k = *r;
        k.dr_id = mkrid(rsnum, rn);
        let at = ds.rules.partition_point(|x| rid2rn(x.dr_id) < rn);
        ds.rules.insert(at, k);
        if inc && let Some(s) = self.set_mut(r.dr_incset) {
            s.refs += 1;
        }
        Ok(())
    }

    /// `DEVFSIO_RDEL` (`devfs_rule_delete`).
    ///
    /// # Errors
    ///
    /// `ENOENT` if there is no such rule.
    pub fn delete(&mut self, rid: u32) -> Result<(), Errno> {
        let rsnum = rid2rsn(rid);
        let ds = self.set_mut(rsnum).ok_or(Errno::ENOENT)?;
        let at = ds
            .rules
            .iter()
            .position(|r| rid2rn(r.dr_id) == rid2rn(rid))
            .ok_or(Errno::ENOENT)?;
        let r = ds.rules.remove(at);
        if r.dr_iacts & DRA_INCSET != 0 {
            if let Some(s) = self.set_mut(r.dr_incset) {
                s.refs = s.refs.saturating_sub(1);
            }
            self.reap(r.dr_incset);
        }
        self.reap(rsnum);
        Ok(())
    }

    /// `DEVFSIO_RGETNEXT`: the first rule of `rid`'s ruleset numbered
    /// after it.
    ///
    /// # Errors
    ///
    /// `ENOENT` if the ruleset does not exist or has no such rule.
    pub fn next_rule(&self, rid: u32) -> Result<DevfsRule, Errno> {
        let ds = self.set(rid2rsn(rid)).ok_or(Errno::ENOENT)?;
        ds.rules
            .iter()
            .find(|r| rid2rn(r.dr_id) > rid2rn(rid))
            .copied()
            .ok_or(Errno::ENOENT)
    }

    /// `DEVFSIO_SGETNEXT`: the first ruleset numbered after `n`.
    #[must_use]
    pub fn next_set(&self, n: u16) -> Option<u16> {
        self.sets.iter().map(|s| s.number).find(|&m| m > n)
    }

    /// A mount moves from ruleset `old` to `new` (`devfs_ruleset_use`):
    /// `new` is made if it does not exist; `old` goes if nothing else
    /// holds it.
    pub fn use_set(&mut self, old: u16, new: u16) {
        self.release(old);
        if new != 0 {
            self.create(new).refs += 1;
        }
    }

    /// A mount using ruleset `n` goes (`devfs_rules_cleanup`).
    pub fn release(&mut self, n: u16) {
        if n == 0 {
            return;
        }
        if let Some(s) = self.set_mut(n) {
            s.refs = s.refs.saturating_sub(1);
        }
        self.reap(n);
    }

    /// Runs `r` on `e` (`devfs_rule_run`), an include `depth` deep at
    /// most.
    pub fn run(&self, r: &DevfsRule, e: &mut Entry<'_>, depth: u32) {
        if !matches(r, e) {
            return;
        }
        if r.dr_iacts & DRA_BACTS != 0 {
            if r.dr_bacts & DRB_HIDE != 0 {
                e.hidden = true;
            }
            if r.dr_bacts & DRB_UNHIDE != 0 {
                e.hidden = false;
            }
        }
        if r.dr_iacts & DRA_UID != 0 {
            e.uid = r.dr_uid;
        }
        if r.dr_iacts & DRA_GID != 0 {
            e.gid = r.dr_gid;
        }
        if r.dr_iacts & DRA_MODE != 0 {
            e.mode = r.dr_mode;
        }
        if r.dr_iacts & DRA_INCSET != 0 && depth > 0 {
            self.apply_set(r.dr_incset, e, depth - 1);
        }
    }

    /// Runs ruleset `n`'s rules on `e`, in order
    /// (`devfs_ruleset_applyde`).
    pub fn apply_set(&self, n: u16, e: &mut Entry<'_>, depth: u32) {
        if let Some(s) = self.set(n) {
            for r in &s.rules {
                self.run(r, e, depth);
            }
        }
    }
}

/// Whether `r`'s conditions hold for `e` (`devfs_rule_match`): one that
/// needs a device fails for anything else, and a path pattern fails for
/// what has no path.
fn matches(r: &DevfsRule, e: &Entry<'_>) -> bool {
    if r.dr_icond & DRC_DSWFLAGS != 0 {
        match e.dswflags {
            Some(f) if f & r.dr_dswflags.cast_unsigned() != 0 => {}
            _ => return false,
        }
    }
    if r.dr_icond & DRC_PATHPTRN != 0 {
        match e.path {
            Some(p) if fnmatch_pathname(r.pattern(), p) => {}
            _ => return false,
        }
    }
    true
}

/// `fnmatch(pattern, s, FNM_PATHNAME) == 0`: `*`, `?`, and bracket
/// expressions (`[!...]` and `[^...]` negate, `a-z` ranges) never match
/// a `/`, and `\` quotes the next character.
#[must_use]
pub fn fnmatch_pathname(pattern: &[u8], s: &[u8]) -> bool {
    let (mut p, mut s) = (pattern, s);
    loop {
        let Some((&c, rest)) = p.split_first() else {
            return s.is_empty();
        };
        p = rest;
        match c {
            b'?' => match s.split_first() {
                Some((&b'/', _)) | None => return false,
                Some((_, r)) => s = r,
            },
            b'*' => {
                while p.first() == Some(&b'*') {
                    p = &p[1..];
                }
                if p.is_empty() {
                    return !s.contains(&b'/');
                }
                if p[0] == b'/' {
                    let Some(i) = s.iter().position(|&b| b == b'/') else {
                        return false;
                    };
                    s = &s[i..];
                    continue;
                }
                loop {
                    if fnmatch_pathname(p, s) {
                        return true;
                    }
                    match s.split_first() {
                        None | Some((&b'/', _)) => return false,
                        Some((_, r)) => s = r,
                    }
                }
            }
            b'[' => {
                let Some((&t, r)) = s.split_first() else {
                    return false;
                };
                if t == b'/' {
                    return false;
                }
                match range(p, t) {
                    Range::Match(after) => {
                        p = after;
                        s = r;
                    }
                    Range::NoMatch => return false,
                    // No closing `]`: the `[` is itself.
                    Range::Error => {
                        if t != b'[' {
                            return false;
                        }
                        s = r;
                    }
                }
            }
            b'\\' => {
                let lit = match p.split_first() {
                    Some((&q, r)) => {
                        p = r;
                        q
                    }
                    None => b'\\',
                };
                match s.split_first() {
                    Some((&t, r)) if t == lit => s = r,
                    _ => return false,
                }
            }
            _ => match s.split_first() {
                Some((&t, r)) if t == c => s = r,
                _ => return false,
            },
        }
    }
}

enum Range<'a> {
    /// Matched; the pattern after the `]`.
    Match(&'a [u8]),
    NoMatch,
    /// Not a bracket expression.
    Error,
}

/// A bracket expression (after its `[`) against `t` (`rangematch`).
fn range(mut p: &[u8], t: u8) -> Range<'_> {
    let negate = matches!(p.first(), Some(b'!' | b'^'));
    if negate {
        p = &p[1..];
    }
    let mut ok = false;
    let mut first = true;
    loop {
        let Some((&c0, rest)) = p.split_first() else {
            return Range::Error;
        };
        if c0 == b']' && !first {
            p = rest;
            break;
        }
        first = false;
        let (c, rest) = if c0 == b'\\' {
            match rest.split_first() {
                Some((&q, r)) => (q, r),
                None => return Range::Error,
            }
        } else {
            (c0, rest)
        };
        if c == b'/' {
            return Range::NoMatch;
        }
        p = rest;
        // `c-c2`, unless the `-` ends the expression.
        if p.first() == Some(&b'-') && p.get(1).is_some_and(|&b| b != b']') {
            let (c2, r) = match p[1] {
                b'\\' => match p.get(2) {
                    Some(&q) => (q, &p[3..]),
                    None => return Range::Error,
                },
                q => (q, &p[2..]),
            };
            p = r;
            if c <= t && t <= c2 {
                ok = true;
            }
        } else if c == t {
            ok = true;
        }
    }
    if ok == negate {
        Range::NoMatch
    } else {
        Range::Match(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(rid: u32, path: &[u8], iacts: i32, bacts: i32) -> DevfsRule {
        let mut r = DevfsRule {
            dr_id: rid,
            dr_iacts: iacts,
            dr_bacts: bacts,
            ..DevfsRule::default()
        };
        if !path.is_empty() {
            r.dr_icond = DRC_PATHPTRN;
            r.dr_pathptrn[..path.len()].copy_from_slice(path);
        }
        r
    }

    fn include(rid: u32, set: u16) -> DevfsRule {
        DevfsRule {
            dr_id: rid,
            dr_iacts: DRA_INCSET,
            dr_incset: set,
            ..DevfsRule::default()
        }
    }

    #[test]
    fn patterns() {
        let m = fnmatch_pathname;
        assert!(m(b"null", b"null"));
        assert!(!m(b"null", b"nul"));
        assert!(m(b"pty?*", b"ptyp0"));
        assert!(m(b"pts/*", b"pts/0"));
        assert!(!m(b"*", b"pts/0"));
        assert!(!m(b"pts*", b"pts/0"));
        assert!(m(b"*/*", b"pts/12"));
        assert!(!m(b"?ts/0", b"/ts/0"));
        assert!(m(b"tty[pqrs]*", b"ttyq3"));
        assert!(!m(b"tty[!pqrs]*", b"ttyq3"));
        assert!(m(b"tty[^a-c]", b"ttyd"));
        assert!(m(b"da[0-9]", b"da7"));
        assert!(!m(b"da[0-9]", b"daa"));
        assert!(m(b"a[]]b", b"a]b"));
        assert!(m(b"a[b", b"a[b"));
        assert!(m(b"a\\*", b"a*"));
        assert!(!m(b"a\\*", b"ab"));
        assert!(!m(b"a[/]b", b"a/b"));
        assert!(m(b"*", b".hidden"));
        assert!(m(b"", b""));
    }

    #[test]
    fn numbering_and_lookup() {
        let mut rs = Rules::new();
        let mut r = rule(mkrid(0, 0), b"null", DRA_BACTS, DRB_UNHIDE);
        rule_input(&mut r, 3).unwrap();
        assert_eq!(r.dr_id, mkrid(3, 0));
        rs.add(&r).unwrap();
        rs.add(&r).unwrap();
        rs.add(&rule(mkrid(3, 150), b"zero", DRA_BACTS, DRB_UNHIDE))
            .unwrap();
        let nums: Vec<u16> = {
            let mut v = Vec::new();
            let mut at = mkrid(3, 0);
            while let Ok(n) = rs.next_rule(at) {
                v.push(rid2rn(n.dr_id));
                at = n.dr_id;
            }
            v
        };
        assert_eq!(nums, [100, 150, 200]);
        assert_eq!(rs.add(&rule(mkrid(3, 150), b"", 0, 0)), Err(Errno::EEXIST));
        assert_eq!(rs.add(&rule(mkrid(0, 5), b"", 0, 0)), Err(Errno::EIO));
        assert_eq!(rs.next_rule(mkrid(3, 200)), Err(Errno::ENOENT));
        assert_eq!(rs.next_rule(mkrid(9, 0)), Err(Errno::ENOENT));
        let mut bad = r;
        bad.dr_magic = 1;
        assert_eq!(rule_input(&mut bad, 3), Err(Errno::ERPCMISMATCH));
        // Numbering past 65535 fails, and makes nothing.
        assert_eq!(rs.add(&rule(mkrid(8, 65500), b"", 0, 0)), Ok(()));
        assert_eq!(rs.add(&rule(mkrid(8, 0), b"", 0, 0)), Err(Errno::ERANGE));
        assert_eq!(rs.add(&rule(mkrid(9, 0), b"", 0, 0)), Ok(()));
        assert_eq!(rs.next_set(0), Some(3));
        assert_eq!(rs.next_set(3), Some(8));
        assert_eq!(rs.next_set(9), None);
        // Emptied, a ruleset goes.
        for n in [100, 150, 200] {
            rs.delete(mkrid(3, n)).unwrap();
        }
        assert!(!rs.exists(3));
        assert_eq!(rs.delete(mkrid(3, 100)), Err(Errno::ENOENT));
    }

    #[test]
    fn includes_hold_and_are_bounded() {
        let mut rs = Rules::new();
        assert_eq!(rs.add(&include(mkrid(4, 0), 1)), Err(Errno::ESRCH));
        assert!(!rs.exists(4), "a refused include makes no ruleset");
        rs.add(&rule(mkrid(1, 0), b"", DRA_BACTS, DRB_HIDE))
            .unwrap();
        rs.add(&rule(mkrid(2, 0), b"null", DRA_BACTS, DRB_UNHIDE))
            .unwrap();
        rs.add(&include(mkrid(4, 0), 1)).unwrap();
        rs.add(&include(mkrid(4, 0), 2)).unwrap();
        let run = |rs: &Rules, path: &'static [u8], depth| {
            let mut e = Entry {
                path: Some(path),
                ..Entry::default()
            };
            rs.apply_set(4, &mut e, depth);
            e.hidden
        };
        assert!(!run(&rs, b"null", 1));
        assert!(run(&rs, b"mem", 1));
        assert!(!run(&rs, b"mem", 0), "depth 0 runs no include");
        // An included ruleset stays while included, emptied or not.
        rs.delete(mkrid(1, 100)).unwrap();
        assert!(rs.exists(1));
        rs.delete(mkrid(4, 100)).unwrap();
        assert!(!rs.exists(1));
        // A ruleset including itself runs only as deep as allowed.
        rs.add(&include(mkrid(2, 0), 2)).unwrap();
        let mut e = Entry {
            path: Some(b"null"),
            hidden: true,
            ..Entry::default()
        };
        rs.apply_set(2, &mut e, 1000);
        assert!(!e.hidden);
        // A mount's use holds a ruleset, even an empty one.
        rs.use_set(0, 7);
        assert!(rs.exists(7));
        rs.use_set(7, 0);
        assert!(!rs.exists(7));
    }

    #[test]
    fn conditions() {
        let rs = Rules::new();
        let mut r = rule(mkrid(1, 1), b"", DRA_MODE | DRA_UID, 0);
        r.dr_icond = DRC_DSWFLAGS;
        r.dr_dswflags = abi::devfs::D_DISK;
        r.dr_mode = 0o600;
        r.dr_uid = 5;
        let mut disk = Entry {
            path: Some(b"da0"),
            dswflags: Some(0x2),
            mode: 0o640,
            ..Entry::default()
        };
        rs.run(&r, &mut disk, 1);
        assert_eq!((disk.mode, disk.uid), (0o600, 5));
        let mut tty = Entry {
            path: Some(b"ttyu0"),
            dswflags: Some(0x4),
            mode: 0o600,
            ..Entry::default()
        };
        rs.run(&r, &mut tty, 1);
        assert_eq!(tty.uid, 0);
        // A link has no device flags; the root no path.
        let mut link = Entry {
            path: Some(b"stdin"),
            ..Entry::default()
        };
        rs.run(&r, &mut link, 1);
        assert_eq!(link.uid, 0);
        let mut root = Entry::default();
        rs.run(&rule(mkrid(1, 2), b"*", DRA_BACTS, DRB_HIDE), &mut root, 1);
        assert!(!root.hidden);
        rs.run(&rule(mkrid(1, 3), b"", DRA_BACTS, DRB_HIDE), &mut root, 1);
        assert!(root.hidden, "a rule without conditions matches everything");
    }
}
