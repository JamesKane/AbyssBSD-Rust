// SPDX-License-Identifier: BSD-2-Clause
//! The jail system calls (docs/MULTIUSER.md §10.4): `jail_set` and
//! `jail_get` with FreeBSD's name-value parameters, `jail_attach`,
//! `jail_remove`, and the old `jail`.
//!
//! Parameters come as `iovec` pairs, a NUL-terminated name and a value
//! (`vfs_buildopts`); a boolean is its name alone (`persist`, or
//! `nopersist` to clear it). Every parameter must be one the call knows,
//! or it fails with `EINVAL` and an `errmsg` saying which. There are no
//! jail descriptors (`desc`), and no network parameters but the empty
//! answers `jls` reads.
//!
//! Behavior reference: sys/kern/kern_jail.c (`kern_jail_set`,
//! `kern_jail_get`, `do_jail_attach`), sys/kern/vfs_mount.c
//! (`vfs_buildopts`, `vfs_setopt`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use sched::Sleepable;

use super::{SysResult, int, process};
use crate::cred::{Priv, Ucred};
use crate::prison::{self, Prison, flags};
use crate::uaccess::{self, UserPtr};

/// `jail_set`/`jail_get` flags (`JAIL_*`).
const JAIL_CREATE: u32 = 0x01;
const JAIL_UPDATE: u32 = 0x02;
const JAIL_ATTACH: u32 = 0x04;
const JAIL_DYING: u32 = 0x08;
const JAIL_USE_DESC: u32 = 0x10;
const JAIL_AT_DESC: u32 = 0x20;
const JAIL_GET_DESC: u32 = 0x40;
const JAIL_OWN_DESC: u32 = 0x80;
const JAIL_SET_MASK: u32 = 0xff;
const JAIL_GET_MASK: u32 = 0xf8;
/// A jailsys parameter's values (`JAIL_SYS_*`).
const JAIL_SYS_DISABLE: i32 = 0;
const JAIL_SYS_NEW: i32 = 1;
const JAIL_SYS_INHERIT: i32 = 2;

/// One parameter: its name, its value as passed, and where it came from
/// (for `jail_get` to write back into).
struct Opt {
    name: Vec<u8>,
    value: Option<Vec<u8>>,
    addr: u64,
    len: usize,
    seen: Cell<bool>,
    /// Where its value's `iov_len` is, for `jail_get` to write back.
    len_at: u64,
}

/// The parameters of one call (`vfsoptlist`).
struct Opts(Vec<Opt>);

impl Opts {
    /// From `iovcnt` `iovec`s at `iov`, in name-value pairs
    /// (`vfs_buildopts`): `EINVAL` for an odd count or a name without its
    /// NUL.
    fn read(iov: u64, iovcnt: u64) -> Result<Self, Errno> {
        let n = usize::try_from(iovcnt).map_err(|_| Errno::EINVAL)?;
        if n % 2 != 0 || n > 1024 {
            return Err(Errno::EINVAL);
        }
        let entry = |i: usize| -> Result<abi::uio::Iovec, Errno> {
            UserPtr::<abi::uio::Iovec>::new(iov + (i * size_of::<abi::uio::Iovec>()) as u64)
                .read()
                .map_err(|_| Errno::EFAULT)
        };
        let mut out = Vec::new();
        for i in (0..n).step_by(2) {
            let ne = entry(i)?;
            let nlen = usize::try_from(ne.iov_len).map_err(|_| Errno::EINVAL)?;
            if nlen == 0 || nlen > 256 {
                return Err(Errno::EINVAL);
            }
            let mut name = alloc::vec![0u8; nlen];
            uaccess::copy_in(&mut name, ne.iov_base).map_err(|_| Errno::EFAULT)?;
            if name.pop() != Some(0) || name.contains(&0) {
                return Err(Errno::EINVAL);
            }
            let ve = entry(i + 1)?;
            let len = usize::try_from(ve.iov_len).map_err(|_| Errno::EINVAL)?;
            if len > 64 * 1024 {
                return Err(Errno::EINVAL);
            }
            let value = if ve.iov_base == 0 {
                None
            } else {
                let mut v = alloc::vec![0u8; len];
                uaccess::copy_in(&mut v, ve.iov_base).map_err(|_| Errno::EFAULT)?;
                Some(v)
            };
            out.push(Opt {
                name,
                value,
                addr: ve.iov_base,
                len,
                seen: Cell::new(false),
                len_at: iov
                    + ((i + 1) * size_of::<abi::uio::Iovec>()
                        + core::mem::offset_of!(abi::uio::Iovec, iov_len))
                        as u64,
            });
        }
        Ok(Self(out))
    }

    fn find(&self, name: &str) -> Option<&Opt> {
        let o = self.0.iter().find(|o| o.name == name.as_bytes())?;
        o.seen.set(true);
        Some(o)
    }

    /// Whether `name` was passed (`vfs_flagopt`).
    fn flag(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// A fixed-size value (`vfs_copyopt`): `EINVAL` if not that size.
    fn copy<const N: usize>(&self, name: &str) -> Result<Option<[u8; N]>, Errno> {
        let Some(o) = self.find(name) else {
            return Ok(None);
        };
        let v = o.value.as_deref().unwrap_or(&[]);
        <[u8; N]>::try_from(v).map(Some).map_err(|_| Errno::EINVAL)
    }

    fn int(&self, name: &str) -> Result<Option<i32>, Errno> {
        Ok(self.copy::<4>(name)?.map(i32::from_ne_bytes))
    }

    /// A string, NUL-terminated within `max` (`vfs_getopt`'s checks).
    fn string(&self, name: &str, max: usize) -> Result<Option<String>, Errno> {
        Ok(self
            .bytes(name, max)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// [`Opts::string`]'s bytes, as they are (a host name need not be
    /// UTF-8).
    fn bytes(&self, name: &str, max: usize) -> Result<Option<Vec<u8>>, Errno> {
        let Some(o) = self.find(name) else {
            return Ok(None);
        };
        let v = o.value.as_deref().unwrap_or(&[]);
        if v.last() != Some(&0) {
            return Err(Errno::EINVAL);
        }
        if v.len() > max {
            return Err(Errno::ENAMETOOLONG);
        }
        // A C string: up to its first NUL, in a buffer that may be
        // larger (`vfs_getopts`).
        let end = v.iter().position(|&c| c == 0).unwrap_or(v.len() - 1);
        Ok(Some(v[..end].to_vec()))
    }

    /// Writes `bytes` as `name`'s value (`vfs_setopt`): `EINVAL` unless
    /// its buffer is exactly that size. Absent: nothing.
    fn set(&self, name: &str, bytes: &[u8]) -> Result<(), Errno> {
        let Some(o) = self.find(name) else {
            return Ok(());
        };
        if o.addr == 0 {
            return Ok(());
        }
        if o.len != bytes.len() {
            return Err(Errno::EINVAL);
        }
        uaccess::copy_out(o.addr, bytes).map_err(|_| Errno::EFAULT)
    }

    fn set_int(&self, name: &str, v: i32) -> Result<(), Errno> {
        self.set(name, &v.to_ne_bytes())
    }

    /// Writes string `s` as `name`'s value (`vfs_setopts`): `EINVAL` if it
    /// and its NUL do not fit.
    fn set_str(&self, name: &str, s: &[u8]) -> Result<(), Errno> {
        let Some(o) = self.find(name) else {
            return Ok(());
        };
        if o.addr == 0 {
            return Ok(());
        }
        if s.len() + 1 > o.len {
            return Err(Errno::EINVAL);
        }
        let mut v = s.to_vec();
        v.push(0);
        uaccess::copy_out(o.addr, &v).map_err(|_| Errno::EFAULT)
    }

    /// The first parameter not looked at, besides `errmsg`.
    fn unknown(&self) -> Option<String> {
        self.0
            .iter()
            .find(|o| !o.seen.get() && o.name != b"errmsg")
            .map(|o| String::from_utf8_lossy(&o.name).into_owned())
    }

    /// Fails with `e`, saying `msg` in `errmsg` if given
    /// (`vfs_opterror`).
    fn error(&self, e: Errno, msg: &str) -> Errno {
        if let Some(o) = self.0.iter().find(|o| o.name == b"errmsg")
            && o.addr != 0
            && o.len > 0
        {
            let n = msg.len().min(o.len - 1);
            let mut v = msg.as_bytes()[..n].to_vec();
            v.push(0);
            let _ = uaccess::copy_out(o.addr, &v);
        }
        e
    }
}

/// `jail_set(iov, niov, flags)` (`kern_jail_set`).
pub(super) fn jail_set(s: &Sleepable, iov: u64, iovcnt: u64, flags: u64) -> SysResult {
    let flags = u32::try_from(flags & 0xffff_ffff).unwrap_or(u32::MAX);
    let p = process()?;
    let mypr = prison::of(&p.cred());
    if flags & (JAIL_CREATE | JAIL_AT_DESC) == JAIL_CREATE && mypr.settings().children_max == 0 {
        return Err(Errno::EPERM);
    }
    if flags & !JAIL_SET_MASK != 0
        || flags & (JAIL_USE_DESC | JAIL_AT_DESC) == JAIL_USE_DESC | JAIL_AT_DESC
    {
        return Err(Errno::EINVAL);
    }
    let opts = Opts::read(iov, iovcnt)?;
    let r = descs(s, &p, &opts, flags, false).and_then(|d| {
        let mypr = d.at.clone().unwrap_or(mypr);
        if flags & JAIL_CREATE != 0 && d.at.is_some() && mypr.settings().children_max == 0 {
            return Err(plain(Errno::EPERM));
        }
        let jid = set(s, &p, &mypr, &opts, flags, d.used.as_ref())?;
        d.give(s, &p, &opts, jid)?;
        Ok(jid)
    });
    if let Err((e, msg)) = &r
        && !msg.is_empty()
    {
        return Err(opts.error(*e, msg));
    }
    r.map(|jid| u64::from(jid.cast_unsigned()))
        .map_err(|(e, _)| e)
}

type SetError = (Errno, String);

fn plain(e: Errno) -> SetError {
    (e, String::new())
}

fn said(e: Errno, msg: impl Into<String>) -> SetError {
    (e, msg.into())
}

/// The work of `jail_set`, failing with an error and what `errmsg` says.
#[allow(
    clippy::too_many_lines,
    reason = "kern_jail_set's checks, in its order"
)]
fn set(
    s: &Sleepable,
    p: &Arc<crate::proc::Process>,
    mypr: &Arc<Prison>,
    opts: &Opts,
    flags: u32,
    used: Option<&(Arc<Prison>, Arc<Ucred>)>,
) -> Result<i32, SetError> {
    let cuflags = flags & (JAIL_CREATE | JAIL_UPDATE);
    if cuflags == 0 {
        return Err(said(Errno::EINVAL, "no valid operation (create or update)"));
    }
    // With `JAIL_USE_DESC`, the descriptor's creator's privileges count,
    // and the descriptor names the jail.
    let cred = used.map_or_else(|| p.cred(), |(_, c)| Arc::clone(c));
    cred.priv_check(Priv::JailSet).map_err(plain)?;
    if flags & JAIL_ATTACH != 0 {
        cred.priv_check(Priv::JailAttach).map_err(plain)?;
    }
    let mut jid = opts.int("jid").map_err(plain)?.unwrap_or(0);
    if let Some((pr, _)) = used {
        if cuflags == JAIL_CREATE {
            return Err(said(
                Errno::EEXIST,
                alloc::format!("jail {} already exists", pr.id()),
            ));
        }
        jid = pr.id();
    }
    let slevel = opts.int("securelevel").map_err(plain)?;
    let childmax = opts.int("children.max").map_err(plain)?;
    let enforce = opts.int("enforce_statfs").map_err(plain)?;
    if enforce.is_some_and(|e| !(0..=2).contains(&e)) {
        return Err(plain(Errno::EINVAL));
    }
    let rsnum = opts.int("devfs_ruleset").map_err(plain)?;
    // Boolean flags: `persist` sets, `nopersist` clears.
    let (mut pr_flags, mut ch_flags) = (0u32, 0u32);
    if opts.flag("persist") {
        pr_flags |= flags::PERSIST;
    }
    if opts.flag("nopersist") {
        ch_flags |= flags::PERSIST;
    }
    ch_flags |= pr_flags;
    // Jailsys parameters: `host` (new or inherit), and the rest, which are
    // remembered as their answer only.
    for name in [
        "host", "vnet", "ip4", "ip6", "sysvmsg", "sysvsem", "sysvshm",
    ] {
        let Some(v) = opts.int(name).map_err(plain)? else {
            continue;
        };
        match (name, v) {
            ("host", JAIL_SYS_NEW) => {
                pr_flags |= flags::HOST;
                ch_flags |= flags::HOST;
            }
            ("host", JAIL_SYS_INHERIT) => ch_flags |= flags::HOST,
            ("host" | "vnet", JAIL_SYS_DISABLE) => return Err(plain(Errno::EINVAL)),
            // No virtual network stacks.
            ("vnet", JAIL_SYS_NEW) => return Err(plain(Errno::EINVAL)),
            (_, JAIL_SYS_DISABLE | JAIL_SYS_NEW | JAIL_SYS_INHERIT) => {}
            _ => return Err(plain(Errno::EINVAL)),
        }
        if flags & JAIL_UPDATE != 0 && name == "vnet" {
            return Err(said(Errno::EINVAL, "vnet cannot be changed after creation"));
        }
    }
    if flags & (JAIL_CREATE | JAIL_ATTACH) == JAIL_CREATE && pr_flags & flags::PERSIST == 0 {
        return Err(said(Errno::EINVAL, "new jail must persist or attach"));
    }
    // allow.* and allow.no*, allow.mount.* likewise.
    let (mut pr_allow, mut ch_allow) = (0u32, 0u32);
    let flag_names = prison::ALLOW_NAMES
        .iter()
        .map(|&(n, b)| {
            (
                alloc::format!("allow.{n}"),
                alloc::format!("allow.no{n}"),
                b,
            )
        })
        .chain(core::iter::once((
            String::from("allow.mount"),
            String::from("allow.nomount"),
            security::allow::MOUNT,
        )))
        .chain(prison::MOUNT_NAMES.iter().map(|&(n, b)| {
            (
                alloc::format!("allow.mount.{n}"),
                alloc::format!("allow.mount.no{n}"),
                b,
            )
        }));
    for (yes, no, bit) in flag_names {
        // `libjail` passes a boolean as an `int`, or as its name alone.
        if let Some(o) = opts.find(&yes) {
            match o.value.as_deref() {
                Some(v) if v.len() == 4 && i32::from_ne_bytes([v[0], v[1], v[2], v[3]]) == 0 => {
                    ch_allow |= bit;
                }
                _ => pr_allow |= bit,
            }
        }
        if opts.flag(&no) {
            ch_allow |= bit;
        }
    }
    ch_allow |= pr_allow;
    let name = opts.string("name", prison::MAXHOSTNAMELEN).map_err(plain)?;
    let mut host_given = false;
    let host = opts
        .bytes("host.hostname", prison::MAXHOSTNAMELEN)
        .map_err(plain)?;
    let domain = opts
        .bytes("host.domainname", prison::MAXHOSTNAMELEN)
        .map_err(plain)?;
    let uuid = opts
        .string("host.hostuuid", prison::HOSTUUIDLEN)
        .map_err(plain)?;
    let hostid = opts
        .copy::<8>("host.hostid")
        .map_err(plain)?
        .map(u64::from_ne_bytes);
    if host.is_some() || domain.is_some() || uuid.is_some() || hostid.is_some() {
        host_given = true;
        pr_flags |= flags::HOST;
        ch_flags |= flags::HOST;
    }
    let osrelease = opts
        .string("osrelease", prison::OSRELEASELEN)
        .map_err(plain)?;
    let osreldate = opts.int("osreldate").map_err(plain)?;
    if flags & JAIL_UPDATE != 0 && osrelease.is_some() {
        return Err(said(
            Errno::EINVAL,
            "osrelease cannot be changed after creation",
        ));
    }
    if flags & JAIL_UPDATE != 0 && osreldate.is_some() {
        return Err(said(
            Errno::EINVAL,
            "osreldate cannot be changed after creation",
        ));
    }
    let edits = [
        ("env", meta_edits(opts, "env").map_err(plain)?),
        ("meta", meta_edits(opts, "meta").map_err(plain)?),
    ];
    // The root, looked up from the caller's.
    let root = match opts.string("path", prison::MAXPATHLEN).map_err(plain)? {
        None => None,
        Some(_) if flags & JAIL_UPDATE != 0 => {
            return Err(said(Errno::EINVAL, "path cannot be changed after creation"));
        }
        Some(path) => {
            let v = crate::fs::lookup(
                s,
                p,
                crate::fs::At::Cwd,
                path.as_bytes(),
                crate::fs::Walk::FOLLOW,
            )
            .map_err(plain)?;
            if !v.is_dir() {
                return Err(plain(Errno::ENOTDIR));
            }
            let global = crate::fs::namespace()
                .path_of(s, &v, &crate::fs::root())
                .map_err(plain)?;
            Some((v, global))
        }
    };
    // A new jail named by a number takes it as its id.
    if cuflags == JAIL_CREATE
        && jid == 0
        && let Some(n) = &name
    {
        let last = n.rsplit('.').next().unwrap_or(n);
        if let Ok(id) = last.parse::<i32>() {
            jid = id;
        }
    }
    if !mypr.is_alive() {
        return Err(plain(Errno::EAGAIN));
    }
    // The jail named, or its parent.
    let mut pr: Option<Arc<Prison>> = None;
    let mut ppr = Arc::clone(mypr);
    if jid != 0 {
        if jid < 0 {
            return Err(said(Errno::EINVAL, "negative jid"));
        }
        let existing = prison::find_child(prison::prison0(), jid);
        if cuflags == JAIL_CREATE && (existing.is_some() || prison::id_taken(jid)) {
            return Err(said(
                Errno::EEXIST,
                alloc::format!("jail {jid} already exists"),
            ));
        }
        match existing {
            Some(e) if mypr.encloses(&e) && !Arc::ptr_eq(&e, mypr) => pr = Some(e),
            _ if cuflags == JAIL_UPDATE => {
                return Err(said(Errno::ENOENT, alloc::format!("jail {jid} not found")));
            }
            Some(_) => {
                return Err(said(Errno::ENOENT, alloc::format!("jail {jid} not found")));
            }
            None => {}
        }
        if let Some(e) = &pr
            && let Some(par) = e.parent()
        {
            ppr = Arc::clone(par);
        }
    }
    let mut short = None;
    if let Some(full) = &name {
        let (parent_name, last) = match full.rfind('.') {
            Some(i) => (Some(&full[..i]), &full[i + 1..]),
            None => (None, &full[..]),
        };
        if let Some(pn) = parent_name {
            if let Some(e) = &pr {
                if ppr.name_from(mypr) != pn {
                    return Err(said(Errno::EINVAL, "cannot change jail's parent"));
                }
                let _ = e;
            } else {
                ppr = prison::find_name(mypr, pn).ok_or_else(|| {
                    said(Errno::ENOENT, alloc::format!("jail \"{pn}\" not found"))
                })?;
            }
        }
        if !last.is_empty() {
            let sibling = prison::descendants(&ppr).into_iter().find(|j| {
                j.parent().is_some_and(|q| Arc::ptr_eq(q, &ppr))
                    && j.is_alive()
                    && j.name_from(&ppr) == last
                    && !pr.as_ref().is_some_and(|e| Arc::ptr_eq(e, j))
            });
            if let Some(j) = sibling {
                if cuflags == JAIL_CREATE || pr.is_some() {
                    return Err(said(
                        Errno::EEXIST,
                        alloc::format!("jail \"{full}\" already exists"),
                    ));
                }
                pr = Some(j);
            } else if pr.is_none() && cuflags == JAIL_UPDATE {
                return Err(said(
                    Errno::ENOENT,
                    alloc::format!("jail \"{full}\" not found"),
                ));
            }
            // A number that is not its jid is no name (`prison_find_name`
            // takes it for one).
            if last
                .parse::<i32>()
                .is_ok_and(|n| pr.as_ref().map_or(n != jid, |e| n != e.id()))
            {
                return Err(said(
                    Errno::EINVAL,
                    "name cannot be numeric (unless it is the jid)",
                ));
            }
            short = Some(String::from(last));
        }
    } else if pr.is_none() && cuflags == JAIL_UPDATE {
        return Err(said(Errno::ENOENT, "update specified no jail"));
    }
    let created = pr.is_none();
    let pr = match pr {
        Some(e) => e,
        None => {
            // Every ancestor's limit holds.
            let mut a = Some(Arc::clone(&ppr));
            while let Some(q) = a {
                if i32::try_from(q.children_cur()).unwrap_or(i32::MAX) >= q.settings().children_max
                {
                    return Err(said(Errno::EPERM, "prison limit exceeded"));
                }
                a = q.parent().cloned();
            }
            if jid == 0 {
                jid = prison::next_id();
                if jid == 0 {
                    return Err(said(Errno::EAGAIN, "no available jail IDs"));
                }
            }
            let (root, path) = root
                .clone()
                .unwrap_or_else(|| (mypr.root(), mypr.path_from(prison::prison0())));
            prison::create(prison::New {
                id: jid,
                parent: Arc::clone(&ppr),
                name: short.clone().unwrap_or_else(|| alloc::format!("{jid}")),
                root,
                path,
                osrelease,
                osreldate,
            })
        }
    };
    let ps = ppr.settings();
    // Children only narrow (`Do final error checking`).
    let fail = |e: SetError| {
        if created {
            pr.remove();
        }
        Err(e)
    };
    if slevel.is_some_and(|l| l < ps.securelevel) {
        return fail(plain(Errno::EPERM));
    }
    if childmax.is_some_and(|m| m >= ps.children_max) {
        return fail(plain(Errno::EPERM));
    }
    if enforce.is_some_and(|e| e < ps.enforce_statfs) {
        return fail(plain(Errno::EPERM));
    }
    let rsnum = match rsnum {
        Some(r) if !(0..=65535).contains(&r) => return fail(plain(Errno::EINVAL)),
        // A nested jail keeps its parent's ruleset.
        Some(r) if p.cred().jailed() => {
            if r > 0 && r != ps.devfs_ruleset {
                return fail(plain(Errno::EPERM));
            }
            Some(ps.devfs_ruleset)
        }
        r => r,
    };
    let differences = security::allow::UNPRIV_DEBUG | security::allow::UNPRIV_PARENT_TAMPER;
    if pr_allow & !ps.allow & !differences != 0 {
        return fail(plain(Errno::EPERM));
    }
    // Source address selection: always on, with no addresses to choose
    // between.
    for name in [
        "ip4.saddrsel",
        "ip4.nosaddrsel",
        "ip6.saddrsel",
        "ip6.nosaddrsel",
    ] {
        let _ = opts.flag(name);
    }
    if let Some(u) = opts.unknown() {
        return fail(said(
            Errno::EINVAL,
            alloc::format!("unknown parameter: {u}"),
        ));
    }
    // Set what was asked.
    if let Some(l) = slevel {
        pr.update(|st| st.securelevel = l);
        for d in prison::descendants(&pr) {
            d.update(|st| st.securelevel = st.securelevel.max(l));
        }
    }
    if let Some(m) = childmax {
        pr.update(|st| st.children_max = m);
    }
    if let Some(e) = enforce {
        pr.update(|st| st.enforce_statfs = e);
        for d in prison::descendants(&pr) {
            d.update(|st| st.enforce_statfs = st.enforce_statfs.max(e));
        }
    }
    if let Some(r) = rsnum {
        pr.update(|st| st.devfs_ruleset = r);
    }
    if let Some(short) = short
        && !created
    {
        let full = if ppr.is_prison0() {
            short
        } else {
            alloc::format!("{}.{short}", ps.name)
        };
        pr.update(|st| st.name = full);
    }
    if flags::HOST & ch_flags & !pr_flags != 0 {
        // `host=inherit`: the parent's identity.
        pr.update(|st| {
            st.hostname.clone_from(&ps.hostname);
            st.domainname.clone_from(&ps.domainname);
            st.hostuuid.clone_from(&ps.hostuuid);
            st.hostid = ps.hostid;
        });
    } else if host_given {
        let apply = |st: &mut prison::Settings| {
            if let Some(h) = &host {
                st.hostname.clone_from(h);
            }
            if let Some(d) = &domain {
                st.domainname.clone_from(d);
            }
            if let Some(u) = &uuid {
                st.hostuuid.clone_from(u);
            }
            if let Some(i) = hostid {
                st.hostid = i;
            }
        };
        pr.update(apply);
        for d in prison::descendants(&pr) {
            if d.settings().flags & flags::HOST == 0 {
                d.update(apply);
            }
        }
    }
    for (which, list) in edits {
        if list.is_empty() {
            continue;
        }
        let st = pr.settings();
        let old = if which == "env" { st.env } else { st.meta };
        let new = apply_meta(old, &list).map_err(plain)?;
        pr.update(|st| {
            if which == "env" {
                st.env = new;
            } else {
                st.meta = new;
            }
        });
    }
    pr.update(|st| {
        st.allow = (st.allow & !ch_allow) | pr_allow;
        st.flags = (st.flags & !ch_flags) | pr_flags;
    });
    // Cleared allow flags go from the descendants too.
    let cleared = ch_allow & !pr_allow;
    if cleared != 0 {
        for d in prison::descendants(&pr) {
            d.update(|st| st.allow &= !cleared);
        }
    }
    if flags & JAIL_ATTACH != 0 {
        if let Err(e) = attach(s, p, &pr) {
            if created {
                pr.remove();
            }
            return Err(said(e, "attach failed"));
        }
    } else {
        // Losing `persist` with nothing in it ends it.
        pr.maybe_die();
    }
    Ok(pr.id())
}

fn meta_max() -> usize {
    prison::META_HARD.load(Ordering::Relaxed) as usize
}

/// One change to a jail's `meta` or `env` (`jm_osd_method_set`): the whole
/// of it, or one `key=value` line (`None`: the key goes).
enum MetaEdit {
    Whole(Vec<u8>),
    Key(Vec<u8>, Option<Vec<u8>>),
}

/// The changes to metadata `which` among the parameters, in order:
/// `which` itself, or `which.<key>`. `EFBIG` past the limit, `EINVAL` for a
/// value without its NUL or an empty key.
fn meta_edits(opts: &Opts, which: &str) -> Result<Vec<MetaEdit>, Errno> {
    let mut out = Vec::new();
    for o in &opts.0 {
        let Some(rest) = o.name.strip_prefix(which.as_bytes()) else {
            continue;
        };
        if !rest.is_empty() && rest[0] != b'.' {
            continue;
        }
        o.seen.set(true);
        if o.len > meta_max() {
            return Err(Errno::EFBIG);
        }
        let value = match &o.value {
            Some(v) if v.last() != Some(&0) => return Err(Errno::EINVAL),
            Some(v) => Some(v[..v.len() - 1].to_vec()),
            None => None,
        };
        if rest.is_empty() {
            out.push(MetaEdit::Whole(value.unwrap_or_default()));
        } else {
            let key = rest[1..].to_vec();
            if key.is_empty() {
                return Err(Errno::EINVAL);
            }
            out.push(MetaEdit::Key(key, value));
        }
    }
    Ok(out)
}

/// Whether `line` is `key=...`.
fn is_key(line: &[u8], key: &[u8]) -> bool {
    line.strip_prefix(key)
        .is_some_and(|r| r.first() == Some(&b'='))
}

/// `old` with `edits` applied: a key's lines go, and its new `key=value`
/// comes first (`jm_h_cut_occurrences`, `jm_h_prepend`).
fn apply_meta(old: Vec<u8>, edits: &[MetaEdit]) -> Result<Vec<u8>, Errno> {
    let mut buf = old;
    for e in edits {
        match e {
            MetaEdit::Whole(v) => buf.clone_from(v),
            MetaEdit::Key(key, value) => {
                let kept: Vec<&[u8]> = buf
                    .split(|&c| c == b'\n')
                    .filter(|l| !is_key(l, key))
                    .collect();
                let mut new = Vec::new();
                if let Some(v) = value {
                    new.extend_from_slice(key);
                    new.push(b'=');
                    new.extend_from_slice(v);
                }
                for l in kept.into_iter().filter(|l| !l.is_empty()) {
                    if !new.is_empty() {
                        new.push(b'\n');
                    }
                    new.extend_from_slice(l);
                }
                buf = new;
            }
        }
    }
    if buf.len() + 1 > meta_max() {
        return Err(Errno::EFBIG);
    }
    Ok(buf)
}

/// Answers `which` and `which.<key>` from metadata `v` (`jm_osd_method_get`):
/// the whole of it, or a key's value; `EINVAL` if one does not fit.
fn meta_get(opts: &Opts, which: &str, v: &[u8]) -> Result<(), Errno> {
    for o in &opts.0 {
        let Some(rest) = o.name.strip_prefix(which.as_bytes()) else {
            continue;
        };
        if !rest.is_empty() && rest[0] != b'.' {
            continue;
        }
        let answer = if rest.is_empty() {
            Some(v.to_vec())
        } else {
            v.split(|&c| c == b'\n')
                .rfind(|l| is_key(l, &rest[1..]))
                .map(|l| l[rest.len()..].to_vec())
        };
        o.seen.set(true);
        let Some(a) = answer else {
            // A key that is not there comes back with no value
            // (`kern_jail_get`: "Communicate back a missing key").
            UserPtr::<u64>::new(o.len_at)
                .write(&0)
                .map_err(|_| Errno::EFAULT)?;
            continue;
        };
        if o.addr == 0 {
            continue;
        }
        if a.len() + 1 > o.len {
            return Err(Errno::EINVAL);
        }
        let mut z = a;
        z.push(0);
        uaccess::copy_out(o.addr, &z).map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

/// `do_jail_attach`: the process moves into `pr`, rooted and working at
/// its root, and is set-id tainted.
fn attach(s: &Sleepable, p: &Arc<crate::proc::Process>, pr: &Arc<Prison>) -> Result<(), Errno> {
    let root = pr.root();
    if !root.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    vfs::acl::access(s, &root, &p.cred(), vfs::ops::VEXEC)?;
    let old = prison::of(&p.cred());
    pr.proc_joined();
    p.change_cred(|c| Ok(prison::cred_in(c, pr)))?;
    let use_ref = root.use_ref();
    drop(p.root.lock().replace(use_ref.clone()));
    drop(core::mem::replace(&mut *p.cwd.lock(), use_ref));
    if !old.is_prison0() {
        old.proc_left();
    }
    if !pr.is_alive() {
        let _ = crate::signal::post(
            p,
            crate::signal::user_info(abi::signal::SIGKILL, 0, abi::signal::code::SI_KERNEL),
        );
    }
    Ok(())
}

/// What a call's `desc` parameter and `JAIL_*_DESC` flags ask for.
struct Descs {
    /// `JAIL_USE_DESC`: the jail it names, and its creator's credential.
    used: Option<(Arc<Prison>, Arc<Ucred>)>,
    /// `JAIL_AT_DESC`: the jail to look up and create under.
    at: Option<Arc<Prison>>,
    /// `JAIL_GET_DESC`, `JAIL_OWN_DESC`: a new descriptor, and its flags.
    out: Option<(prison::JailDesc, u32)>,
}

/// The jail descriptor `fd` (`jaildesc_find`): `EINVAL` for another kind
/// of descriptor, `ENOENT` for one whose jail is dying.
fn desc_jail(
    s: &Sleepable,
    p: &crate::proc::Process,
    fd: i32,
) -> Result<(Arc<Prison>, Arc<Ucred>), Errno> {
    let f = super::io::file(
        s,
        p,
        u64::from(fd.cast_unsigned()),
        &crate::capsicum::rights::NO,
    )?;
    match f.kind() {
        crate::fs::file::FileKind::Jail(j) => j.find(),
        _ => Err(Errno::EINVAL),
    }
}

/// Reads the `desc` parameter as `kern_jail_set` and `kern_jail_get`
/// do: a descriptor flag needs one (`ENOENT`, "missing desc"; for
/// `jail_get`, `JAIL_USE_DESC` without one looks up as usual), and one
/// needs a flag.
fn descs(
    s: &Sleepable,
    p: &crate::proc::Process,
    opts: &Opts,
    flags: u32,
    getting: bool,
) -> Result<Descs, SetError> {
    let named = |e: Errno| {
        said(
            e,
            if e == Errno::ENOENT {
                "descriptor to dead jail"
            } else {
                "not a jail descriptor"
            },
        )
    };
    let mut d = Descs {
        used: None,
        at: None,
        out: None,
    };
    let all = JAIL_USE_DESC | JAIL_AT_DESC | JAIL_GET_DESC | JAIL_OWN_DESC;
    let Some(fd) = opts.int("desc").map_err(plain)? else {
        let need = if getting { all & !JAIL_USE_DESC } else { all };
        if flags & need != 0 {
            return Err(said(Errno::ENOENT, "missing desc"));
        }
        return Ok(d);
    };
    if flags & all == 0 {
        return Err(said(Errno::EINVAL, "unexpected desc"));
    }
    if flags & JAIL_USE_DESC != 0 {
        d.used = Some(desc_jail(s, p, fd).map_err(named)?);
    }
    if flags & JAIL_AT_DESC != 0 {
        d.at = Some(desc_jail(s, p, fd).map_err(named)?.0);
    }
    if flags & (JAIL_GET_DESC | JAIL_OWN_DESC) != 0 {
        d.out = Some(prison::JailDesc::new(p.cred(), flags & JAIL_OWN_DESC != 0).map_err(plain)?);
    }
    Ok(d)
}

impl Descs {
    /// Hands the caller the descriptor it asked for, naming jail `jid`,
    /// through the `desc` parameter.
    fn give(
        self,
        s: &Sleepable,
        p: &crate::proc::Process,
        opts: &Opts,
        jid: i32,
    ) -> Result<(), SetError> {
        let Some((jd, fflags)) = self.out else {
            return Ok(());
        };
        let pr = prison::find_any(jid).ok_or_else(|| plain(Errno::ENOENT))?;
        jd.set_prison(&pr);
        let file = crate::fs::file::File::new(crate::fs::file::FileKind::Jail(jd), fflags);
        let fd = super::io::install(s, p, file, crate::fs::fdtable::FdFlags::default())
            .map_err(plain)?;
        let fd = i32::try_from(fd).unwrap_or(i32::MAX);
        opts.set_int("desc", fd).map_err(plain)
    }
}

/// `jail_attach_jd(fd)`: into the living jail the descriptor names, with
/// its creator's privilege.
pub(super) fn jail_attach_jd(s: &Sleepable, fd: u64) -> SysResult {
    let p = process()?;
    let (pr, cred) = desc_jail(s, &p, int(fd))?;
    cred.priv_check(Priv::JailAttach)?;
    if !pr.is_alive() {
        return Err(Errno::EINVAL);
    }
    attach(s, &p, &pr)?;
    Ok(0)
}

/// `jail_remove_jd(fd)`: the jail the descriptor names, with its
/// creator's privilege.
pub(super) fn jail_remove_jd(s: &Sleepable, fd: u64) -> SysResult {
    let p = process()?;
    let (pr, cred) = desc_jail(s, &p, int(fd))?;
    cred.priv_check(Priv::JailRemove)?;
    pr.remove();
    Ok(0)
}

/// `jail_attach(jid)`: into a living jail below the caller's.
pub(super) fn jail_attach(s: &Sleepable, jid: u64) -> SysResult {
    let p = process()?;
    p.priv_check(Priv::JailAttach)?;
    let mypr = prison::of(&p.cred());
    let pr = prison::find_child(&mypr, int(jid)).ok_or(Errno::EINVAL)?;
    if !pr.is_alive() {
        return Err(Errno::EINVAL);
    }
    attach(s, &p, &pr)?;
    Ok(0)
}

/// `jail_remove(jid)`: a jail below the caller's, its processes killed.
pub(super) fn jail_remove(jid: u64) -> SysResult {
    let p = process()?;
    p.priv_check(Priv::JailRemove)?;
    let mypr = prison::of(&p.cred());
    let pr = prison::find_child(&mypr, int(jid)).ok_or(Errno::EINVAL)?;
    pr.remove();
    Ok(0)
}

/// `jail_get(iov, niov, flags)` (`kern_jail_get`): the jail `jid`, `name`,
/// or the next after `lastjid` names, its parameters written back.
pub(super) fn jail_get(s: &Sleepable, iov: u64, iovcnt: u64, flags: u64) -> SysResult {
    let flags = u32::try_from(flags & 0xffff_ffff).unwrap_or(u32::MAX);
    if flags & !JAIL_GET_MASK != 0 {
        return Err(Errno::EINVAL);
    }
    let opts = Opts::read(iov, iovcnt)?;
    let p = process()?;
    let mypr = prison::of(&p.cred());
    let r = descs(s, &p, &opts, flags, true).and_then(|d| {
        let mypr = d.at.clone().unwrap_or(mypr);
        let jid = get(&opts, &mypr, flags, d.used.as_ref().map(|(pr, _)| pr))?;
        d.give(s, &p, &opts, jid)?;
        Ok(jid)
    });
    match r {
        Ok(jid) => Ok(u64::from(jid.cast_unsigned())),
        Err((e, msg)) if !msg.is_empty() => Err(opts.error(e, &msg)),
        Err((e, _)) => Err(e),
    }
}

fn get(
    opts: &Opts,
    mypr: &Arc<Prison>,
    flags: u32,
    used: Option<&Arc<Prison>>,
) -> Result<i32, SetError> {
    let dying = flags & JAIL_DYING != 0;
    let pr = if let Some(pr) = used {
        if !(pr.is_alive() || dying) {
            return Err(said(
                Errno::ENOENT,
                alloc::format!("jail {} is dying", pr.id()),
            ));
        }
        Arc::clone(pr)
    } else if let Some(last) = opts.int("lastjid").map_err(plain)? {
        prison::next_after(mypr, last, dying)
            .ok_or_else(|| said(Errno::ENOENT, alloc::format!("no jail after {last}")))?
    } else if let Some(jid) = opts.int("jid").map_err(plain)?.filter(|&j| j != 0) {
        prison::find_child(mypr, jid)
            .ok_or_else(|| said(Errno::ENOENT, alloc::format!("jail {jid} not found")))?
    } else if let Some(name) = opts.string("name", prison::MAXHOSTNAMELEN).map_err(plain)? {
        prison::find_name(mypr, &name)
            .ok_or_else(|| said(Errno::ENOENT, alloc::format!("jail \"{name}\" not found")))?
    } else {
        return Err(said(Errno::ENOENT, "no jail specified"));
    };
    let st = pr.settings();
    let e = plain;
    opts.set_int("jid", pr.id()).map_err(e)?;
    let parent = pr
        .parent()
        .filter(|q| !Arc::ptr_eq(q, mypr))
        .map_or(0, |q| q.id());
    opts.set_int("parent", parent).map_err(e)?;
    opts.set_str("name", pr.name_from(mypr).as_bytes())
        .map_err(e)?;
    opts.set_int("cpuset.id", 0).map_err(e)?;
    opts.set_str("path", &pr.path_from(mypr)).map_err(e)?;
    opts.set_int("securelevel", st.securelevel).map_err(e)?;
    opts.set_int(
        "children.cur",
        i32::try_from(pr.children_cur()).unwrap_or(i32::MAX),
    )
    .map_err(e)?;
    opts.set_int("children.max", st.children_max).map_err(e)?;
    opts.set_str("host.hostname", &st.hostname).map_err(e)?;
    opts.set_str("host.domainname", &st.domainname).map_err(e)?;
    opts.set_str("host.hostuuid", st.hostuuid.as_bytes())
        .map_err(e)?;
    opts.set("host.hostid", &st.hostid.to_ne_bytes())
        .map_err(e)?;
    opts.set_int("enforce_statfs", st.enforce_statfs)
        .map_err(e)?;
    opts.set_int("devfs_ruleset", st.devfs_ruleset).map_err(e)?;
    opts.set_int("osreldate", st.osreldate).map_err(e)?;
    opts.set_str("osrelease", st.osrelease.as_bytes())
        .map_err(e)?;
    let b = |on: bool| i32::from(on);
    let persist = st.flags & flags::PERSIST != 0;
    opts.set_int("persist", b(persist)).map_err(e)?;
    opts.set_int("nopersist", b(!persist)).map_err(e)?;
    opts.set_int("dying", b(st.dying)).map_err(e)?;
    opts.set_int("nodying", b(!st.dying)).map_err(e)?;
    let host = if st.flags & flags::HOST != 0 {
        JAIL_SYS_NEW
    } else {
        JAIL_SYS_INHERIT
    };
    opts.set_int("host", host).map_err(e)?;
    opts.set_int("vnet", JAIL_SYS_INHERIT).map_err(e)?;
    for name in ["ip4", "ip6", "sysvmsg", "sysvsem", "sysvshm"] {
        opts.set_int(name, JAIL_SYS_INHERIT).map_err(e)?;
    }
    for &(n, bit) in prison::ALLOW_NAMES {
        let on = st.allow & bit != 0;
        opts.set_int(&alloc::format!("allow.{n}"), b(on))
            .map_err(e)?;
        opts.set_int(&alloc::format!("allow.no{n}"), b(!on))
            .map_err(e)?;
    }
    let mount = st.allow & security::allow::MOUNT != 0;
    opts.set_int("allow.mount", b(mount)).map_err(e)?;
    opts.set_int("allow.nomount", b(!mount)).map_err(e)?;
    for &(n, bit) in prison::MOUNT_NAMES {
        let on = st.allow & bit != 0;
        opts.set_int(&alloc::format!("allow.mount.{n}"), b(on))
            .map_err(e)?;
        opts.set_int(&alloc::format!("allow.mount.no{n}"), b(!on))
            .map_err(e)?;
    }
    for (name, v) in [("env", &st.env), ("meta", &st.meta)] {
        meta_get(opts, name, v).map_err(plain)?;
    }
    // jls asks for the addresses; there are none. Source address
    // selection is on, as for every new prison (`_PR_IP_SADDRSEL`).
    opts.set("ip4.addr", &[]).map_err(e)?;
    opts.set("ip6.addr", &[]).map_err(e)?;
    for family in ["ip4", "ip6"] {
        opts.set_int(&alloc::format!("{family}.saddrsel"), 1)
            .map_err(e)?;
        opts.set_int(&alloc::format!("{family}.nosaddrsel"), 0)
            .map_err(e)?;
    }
    if let Some(u) = opts.unknown() {
        return Err(said(
            Errno::EINVAL,
            alloc::format!("unknown parameter: {u}"),
        ));
    }
    Ok(pr.id())
}

// SAFETY: integers and addresses, padding explicit (`abi-check` holds
// the C layout).
unsafe impl crate::uaccess::Plain for abi::jail::Jail {}

/// `jail(jail)` (`kern_jail`): creates a jail with the old structure's
/// path, hostname, and name, and enters it. No addresses.
pub(super) fn jail(s: &Sleepable, ptr: u64) -> SysResult {
    let version = UserPtr::<u32>::new(ptr).read().map_err(|_| Errno::EFAULT)?;
    if version != abi::jail::JAIL_API_VERSION {
        return Err(Errno::EINVAL);
    }
    let j = UserPtr::<abi::jail::Jail>::new(ptr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if j.ip4s > 0 || j.ip6s > 0 {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    p.priv_check(Priv::JailSet)?;
    p.priv_check(Priv::JailAttach)?;
    let mypr = prison::of(&p.cred());
    if mypr.settings().children_max == 0 {
        return Err(Errno::EPERM);
    }
    let string = |a: u64, max: usize| -> Result<Option<String>, Errno> {
        if a == 0 {
            return Ok(None);
        }
        let v = uaccess::copy_in_str_vec(a, max).map_err(|_| Errno::EFAULT)?;
        Ok(Some(String::from_utf8_lossy(&v).into_owned()))
    };
    let path = string(j.path, prison::MAXPATHLEN)?.ok_or(Errno::EFAULT)?;
    let host = string(j.hostname, prison::MAXHOSTNAMELEN)?;
    let name = string(j.jailname, prison::MAXHOSTNAMELEN)?;
    let v = crate::fs::lookup(
        s,
        &p,
        crate::fs::At::Cwd,
        path.as_bytes(),
        crate::fs::Walk::FOLLOW,
    )?;
    if !v.is_dir() {
        return Err(Errno::ENOTDIR);
    }
    let global = crate::fs::namespace().path_of(s, &v, &crate::fs::root())?;
    let id = prison::next_id();
    if id == 0 {
        return Err(Errno::EAGAIN);
    }
    let pr = prison::create(prison::New {
        id,
        parent: Arc::clone(&mypr),
        name: name.unwrap_or_else(|| alloc::format!("{id}")),
        root: v,
        path: global,
        osrelease: None,
        osreldate: None,
    });
    if let Some(h) = host {
        pr.update(|st| st.hostname = h.into_bytes());
    }
    if let Err(e) = attach(s, &p, &pr) {
        pr.remove();
        return Err(e);
    }
    Ok(u64::from(id.cast_unsigned()))
}
