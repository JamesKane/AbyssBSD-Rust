// SPDX-License-Identifier: BSD-2-Clause
//! POSIX.1e access control lists, as UFS keeps them: in the system
//! namespace's `posix1e.acl_access` and `posix1e.acl_default` extended
//! attributes, each a `struct oldacl`, on a mount with `MNT_ACLS`. The
//! mode stays the truth for the owner, the group class (the mask, if
//! there is one), and others: an ACL read back is brought in line with it,
//! and an access ACL written sets it.
//!
//! Behavior reference: sys/kern/subr_acl_posix1e.c, sys/kern/vfs_acl.c,
//! sys/ufs/ufs/ufs_acl.c, and `ufs_accessx` and the `ufs_do_posix1e_acl_
//! inheritance_*` functions in sys/ufs/ufs/ufs_vnops.c.

use alloc::vec::Vec;

use abi::acl::{
    ACL_EXECUTE, ACL_GROUP, ACL_GROUP_OBJ, ACL_MASK, ACL_OTHER, ACL_PERM_BITS, ACL_READ,
    ACL_UNDEFINED_ID, ACL_USER, ACL_USER_OBJ, ACL_WRITE, AclEntry, OLDACL_MAX_ENTRIES,
    POSIX1E_ACL_ACCESS_EXTATTR_NAME, POSIX1E_ACL_DEFAULT_EXTATTR_NAME,
};
use abi::errno::Errno;
use abi::event::NOTE_ATTRIB;
use abi::mount::{MNT_ACLS, MNT_NFS4ACLS};
use abi::stat::{APPEND, IMMUTABLE};
use sched::Sleepable;
use security::{Cred, Priv};

use crate::node::{Attr, Kind, Node, SetAttr};
use crate::ops::{self, VEXEC, VREAD, VWRITE, extattr};
use crate::vnode::Vnode;

/// The mode bits an ACL speaks for (`ACL_OVERRIDE_MASK`); the rest
/// (`ACL_PRESERVE_MASK`) it leaves alone.
const OVERRIDE: u16 = 0o777;

/// What an `__acl_*` call names, its old types already mapped to the new
/// (`acl_type_unold`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Access,
    Default,
    Nfs4,
}

impl Type {
    /// `ACL_TYPE_*`; `EINVAL` for anything else.
    pub fn from_raw(t: u32) -> Result<Self, Errno> {
        use abi::acl::{
            ACL_TYPE_ACCESS, ACL_TYPE_ACCESS_OLD, ACL_TYPE_DEFAULT, ACL_TYPE_DEFAULT_OLD,
            ACL_TYPE_NFS4,
        };
        match t {
            ACL_TYPE_ACCESS | ACL_TYPE_ACCESS_OLD => Ok(Self::Access),
            ACL_TYPE_DEFAULT | ACL_TYPE_DEFAULT_OLD => Ok(Self::Default),
            ACL_TYPE_NFS4 => Ok(Self::Nfs4),
            _ => Err(Errno::EINVAL),
        }
    }

    fn attr_name(self) -> &'static [u8] {
        match self {
            Self::Default => POSIX1E_ACL_DEFAULT_EXTATTR_NAME,
            _ => POSIX1E_ACL_ACCESS_EXTATTR_NAME,
        }
    }
}

/// A POSIX.1e ACL: its entries, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Acl(pub Vec<AclEntry>);

/// `struct oldacl`'s size: a count, then 32 entries of a tag, an id, and
/// a `mode_t` permission, padded.
pub const OLDACL_SIZE: usize = 4 + OLDACL_MAX_ENTRIES * 12;

fn entry(tag: u32, perm: u32) -> AclEntry {
    AclEntry {
        ae_tag: tag,
        ae_id: ACL_UNDEFINED_ID,
        ae_perm: perm,
        ae_entry_type: 0,
        ae_flags: 0,
    }
}

/// The permissions mode `m` gives an `_OBJ` entry or others
/// (`acl_posix1e_mode_to_perm`).
fn perm_of(tag: u32, m: u16) -> u32 {
    let shift = match tag {
        ACL_USER_OBJ => 6,
        ACL_GROUP_OBJ => 3,
        _ => 0,
    };
    u32::from(m >> shift) & ACL_PERM_BITS
}

/// What permissions `p` grant, as access modes.
fn granted(p: u32) -> u32 {
    let mut g = 0;
    if p & ACL_EXECUTE != 0 {
        g |= VEXEC;
    }
    if p & ACL_READ != 0 {
        g |= VREAD;
    }
    if p & ACL_WRITE != 0 {
        g |= VWRITE;
    }
    g
}

impl Acl {
    /// The three entries a mode stands for.
    pub fn of_mode(m: u16) -> Self {
        Self(
            [ACL_USER_OBJ, ACL_GROUP_OBJ, ACL_OTHER]
                .into_iter()
                .map(|t| entry(t, perm_of(t, m)))
                .collect(),
        )
    }

    fn find(&self, tag: u32) -> Option<&AclEntry> {
        self.0.iter().find(|e| e.ae_tag == tag)
    }

    /// `struct oldacl`, as UFS stores it (`acl_copy_oldacl_into_acl`):
    /// `EPERM` for a stored ACL of the wrong size, `EINVAL` for a count
    /// out of range.
    pub fn decode(b: &[u8]) -> Result<Self, Errno> {
        if b.len() != OLDACL_SIZE {
            return Err(Errno::EPERM);
        }
        let word = |at: usize| u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        // `acl_cnt` is an `int`: a negative one is past the limit too.
        let n = usize::try_from(word(0))
            .ok()
            .filter(|&n| n <= OLDACL_MAX_ENTRIES)
            .ok_or(Errno::EINVAL)?;
        Ok(Self(
            (0..n)
                .map(|i| {
                    let at = 4 + i * 12;
                    AclEntry {
                        ae_tag: word(at),
                        ae_id: word(at + 4),
                        ae_perm: u32::from(u16::from_ne_bytes([b[at + 8], b[at + 9]])),
                        ae_entry_type: 0,
                        ae_flags: 0,
                    }
                })
                .collect(),
        ))
    }

    /// The `struct oldacl` to store (`acl_copy_acl_into_oldacl`): `EINVAL`
    /// past its 32 entries.
    pub fn encode(&self) -> Result<Vec<u8>, Errno> {
        if self.0.len() > OLDACL_MAX_ENTRIES {
            return Err(Errno::EINVAL);
        }
        let mut b = alloc::vec![0u8; OLDACL_SIZE];
        let n = u32::try_from(self.0.len()).map_err(|_| Errno::EINVAL)?;
        b[..4].copy_from_slice(&n.to_ne_bytes());
        for (i, e) in self.0.iter().enumerate() {
            let at = 4 + i * 12;
            b[at..at + 4].copy_from_slice(&e.ae_tag.to_ne_bytes());
            b[at + 4..at + 8].copy_from_slice(&e.ae_id.to_ne_bytes());
            // `mode_t`: the permission bits fit (`check` saw to that).
            let perm = u16::try_from(e.ae_perm & ACL_PERM_BITS).unwrap_or(0);
            b[at + 8..at + 10].copy_from_slice(&perm.to_ne_bytes());
        }
        Ok(b)
    }

    /// Whether this is a valid POSIX.1e ACL (`acl_posix1e_check`): one
    /// owner, owning group, and others entry, a mask if there are named
    /// users or groups, an id on each of those, and no permissions but
    /// read, write, and execute. `EINVAL` if not. Whatever id the other
    /// entries carry becomes `ACL_UNDEFINED_ID`, as FreeBSD's does (libc
    /// leaves the ones it parses from text at 0).
    pub fn check(&mut self) -> Result<(), Errno> {
        let mut count = [0usize; 6];
        for e in &mut self.0 {
            let (slot, named) = match e.ae_tag {
                ACL_USER_OBJ => (0, false),
                ACL_USER => (1, true),
                ACL_GROUP_OBJ => (2, false),
                ACL_GROUP => (3, true),
                ACL_MASK => (4, false),
                ACL_OTHER => (5, false),
                _ => return Err(Errno::EINVAL),
            };
            if !named {
                e.ae_id = ACL_UNDEFINED_ID;
            } else if e.ae_id == ACL_UNDEFINED_ID {
                return Err(Errno::EINVAL);
            }
            if e.ae_perm & !ACL_PERM_BITS != 0 {
                return Err(Errno::EINVAL);
            }
            count[slot] += 1;
        }
        let [user_obj, user, group_obj, group, mask, other] = count;
        if user_obj != 1 || group_obj != 1 || other != 1 || mask > 1 {
            return Err(Errno::EINVAL);
        }
        if (user != 0 || group != 0) && mask != 1 {
            return Err(Errno::EINVAL);
        }
        Ok(())
    }

    /// The permission bits this ACL makes the mode (`acl_posix1e_acl_to_mode`):
    /// the owner's, the mask's (or the owning group's), and others'. `None`
    /// for one missing an entry it needs.
    pub fn to_mode(&self) -> Option<u16> {
        let user = self.find(ACL_USER_OBJ)?;
        let group = self.find(ACL_MASK).or_else(|| self.find(ACL_GROUP_OBJ))?;
        let other = self.find(ACL_OTHER)?;
        let bits = |p: u32| (p & ACL_PERM_BITS) as u16;
        Some(bits(user.ae_perm) << 6 | bits(group.ae_perm) << 3 | bits(other.ae_perm))
    }

    /// Brought in line with mode `m` (`ufs_sync_acl_from_inode`): the
    /// owner's, others', and the mask's (or, without one, the owning
    /// group's) permissions are the mode's.
    pub fn sync_from_mode(&mut self, m: u16) {
        let has_mask = self.find(ACL_MASK).is_some();
        for e in &mut self.0 {
            match e.ae_tag {
                ACL_USER_OBJ | ACL_OTHER => {
                    e.ae_perm = perm_of(e.ae_tag, m);
                    e.ae_id = ACL_UNDEFINED_ID;
                }
                ACL_MASK => {
                    e.ae_perm = perm_of(ACL_GROUP_OBJ, m);
                    e.ae_id = ACL_UNDEFINED_ID;
                }
                ACL_GROUP_OBJ => {
                    if !has_mask {
                        e.ae_perm = perm_of(ACL_GROUP_OBJ, m);
                    }
                    e.ae_id = ACL_UNDEFINED_ID;
                }
                _ => {}
            }
        }
    }

    /// Whether `cred` may access a node owned by `uid` and `gid` as `want`
    /// says under this ACL (`vaccess_acl_posix1e`): the owner by its
    /// entry; else a named user's, masked; else the best of the groups the
    /// caller is in, masked; else others'. What the matching entry does
    /// not grant, a privilege may, as in `ops::access`.
    pub fn access(
        &self,
        kind: Kind,
        uid: u32,
        gid: u32,
        cred: &Cred,
        want: u32,
    ) -> Result<(), Errno> {
        let held = |p| cred.priv_check(p).is_ok();
        let mut privileged = 0;
        if want & VEXEC != 0
            && if kind == Kind::Directory {
                held(Priv::VfsLookup)
            } else {
                self.to_mode().is_some_and(|m| m & 0o111 != 0) && held(Priv::VfsExec)
            }
        {
            privileged |= VEXEC;
        }
        if want & VREAD != 0 && held(Priv::VfsRead) {
            privileged |= VREAD;
        }
        if want & VWRITE != 0 && held(Priv::VfsWrite) {
            privileged |= VWRITE;
        }
        let enough = |g: u32| want & (g | privileged) == want;
        if cred.uid == uid
            && let Some(e) = self.find(ACL_USER_OBJ)
        {
            return if enough(granted(e.ae_perm)) {
                Ok(())
            } else {
                Err(Errno::EACCES)
            };
        }
        let Some(other) = self.find(ACL_OTHER) else {
            return Err(Errno::EPERM);
        };
        let mask = self
            .find(ACL_MASK)
            .map_or(VEXEC | VREAD | VWRITE, |e| granted(e.ae_perm));
        if let Some(e) = self
            .0
            .iter()
            .find(|e| e.ae_tag == ACL_USER && e.ae_id == cred.uid)
        {
            return if enough(granted(e.ae_perm) & mask) {
                Ok(())
            } else {
                Err(Errno::EACCES)
            };
        }
        // The groups: any that grants it outright, else any that grants
        // it with privilege; one that matches ends the search either way.
        let groups: Vec<u32> = self
            .0
            .iter()
            .filter(|e| match e.ae_tag {
                ACL_GROUP_OBJ => cred.in_group(gid),
                ACL_GROUP => cred.in_group(e.ae_id),
                _ => false,
            })
            .map(|e| granted(e.ae_perm) & mask)
            .collect();
        if !groups.is_empty() {
            return if groups.iter().any(|&g| want & g == want) || groups.iter().any(|&g| enough(g))
            {
                Ok(())
            } else {
                Err(Errno::EACCES)
            };
        }
        if enough(granted(other.ae_perm)) {
            Ok(())
        } else {
            Err(Errno::EACCES)
        }
    }
}

/// The mode a node made with mode `m` under a directory whose default
/// ACL is `dacl` gets (`acl_posix1e_newfilemode`): no permission the
/// default ACL withholds.
pub fn new_file_mode(m: u16, dacl: &Acl) -> u16 {
    (m & !OVERRIDE) | (m & OVERRIDE & dacl.to_mode().unwrap_or(OVERRIDE))
}

/// Whether `v`'s mount keeps POSIX.1e ACLs.
pub fn enabled(v: &Vnode) -> bool {
    v.mount().flags() & MNT_ACLS != 0
}

/// The stored ACL of `ty` on `node`, if it has one.
fn stored(s: &Sleepable, node: &dyn Node, ty: Type) -> Result<Option<Acl>, Errno> {
    match node.get_extattr(s, extattr::SYSTEM, ty.attr_name()) {
        Ok(b) => Acl::decode(&b).map(Some),
        Err(Errno::ENOATTR) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The default ACL a new node under `dir` inherits, if `dir`'s mount
/// keeps ACLs and it has a non-empty one.
pub(crate) fn inherited(s: &Sleepable, dir: &Vnode, node: &dyn Node) -> Result<Option<Acl>, Errno> {
    if !enabled(dir) {
        return Ok(None);
    }
    Ok(stored(s, node, Type::Default)?.filter(|a| !a.0.is_empty()))
}

/// Gives the new node `v`, made with mode `m` under a directory whose
/// default ACL is `dacl`, its ACLs (`ufs_do_posix1e_acl_inheritance_*`):
/// the default ACL in line with `m` as its access ACL and, for a
/// directory, the default ACL as its own.
pub(crate) fn inherit(s: &Sleepable, v: &Vnode, m: u16, dacl: &Acl) -> Result<(), Errno> {
    let mut access = dacl.clone();
    access.sync_from_mode(m);
    let mut st = v.write(s);
    let node = st.live_mut()?.node_mut();
    node.set_extattr(
        s,
        extattr::SYSTEM,
        POSIX1E_ACL_ACCESS_EXTATTR_NAME,
        &access.encode()?,
    )?;
    if v.is_dir() {
        node.set_extattr(
            s,
            extattr::SYSTEM,
            POSIX1E_ACL_DEFAULT_EXTATTR_NAME,
            &dacl.encode()?,
        )?;
    }
    Ok(())
}

/// Whether `cred` may access `v` as `want` says (`ufs_accessx`): by its
/// access ACL on a mount that keeps them, by its mode otherwise.
pub fn access(s: &Sleepable, v: &Vnode, cred: &Cred, want: u32) -> Result<(), Errno> {
    if !enabled(v) {
        return ops::access(&v.attr(s)?, v.kind(), cred, want);
    }
    let st = v.read(s);
    let node = st.live()?.node();
    access_locked(s, v, node, &node.attr(), cred, want)
}

/// `access`, for `v` locked, its node `node` with attributes `a`.
pub(crate) fn access_locked(
    s: &Sleepable,
    v: &Vnode,
    node: &dyn Node,
    a: &Attr,
    cred: &Cred,
    want: u32,
) -> Result<(), Errno> {
    if enabled(v)
        && let Ok(Some(mut acl)) = stored(s, node, Type::Access)
    {
        if want & VWRITE != 0 && a.flags & IMMUTABLE != 0 {
            return Err(Errno::EPERM);
        }
        acl.sync_from_mode(a.mode);
        return acl.access(v.kind(), a.uid, a.gid, cred, want);
    }
    ops::access(a, v.kind(), cred, want)
}

/// Which kind of ACL a mount keeps: `EOPNOTSUPP` for none, `EINVAL` for
/// an `ACL_TYPE_NFS4` request on a POSIX.1e mount (and the reverse), or
/// a default ACL asked of anything but a directory.
fn supported(v: &Vnode, ty: Type) -> Result<(), Errno> {
    let flags = v.mount().flags();
    if flags & (MNT_ACLS | MNT_NFS4ACLS) == 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let nfs4 = flags & MNT_NFS4ACLS != 0;
    if (ty == Type::Nfs4) != nfs4 {
        return Err(Errno::EINVAL);
    }
    if nfs4 {
        // No mount keeps NFSv4 ACLs (`nfsv4acls` is refused).
        return Err(Errno::EOPNOTSUPP);
    }
    if ty == Type::Default && !v.is_dir() {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// `__acl_get_*` (`ufs_getacl_posix1e`): the access ACL in line with the
/// mode, or the three entries the mode stands for; a directory's default
/// ACL, or an empty one.
pub fn get(s: &Sleepable, v: &Vnode, ty: Type) -> Result<Acl, Errno> {
    supported(v, ty)?;
    let a = v.attr(s)?;
    let st = v.read(s);
    let acl = stored(s, st.live()?.node(), ty)?;
    Ok(match (ty, acl) {
        (Type::Access, Some(mut acl)) => {
            acl.sync_from_mode(a.mode);
            acl
        }
        (Type::Access, None) => Acl::of_mode(a.mode),
        (_, acl) => acl.unwrap_or_default(),
    })
}

/// `__acl_aclcheck_*` (`ufs_aclcheck_posix1e`): whether `acl` could be
/// set on `v` as `ty`, its ids normalized as [`Acl::check`] does.
pub fn check(v: &Vnode, ty: Type, acl: &mut Acl) -> Result<(), Errno> {
    supported(v, ty)?;
    if acl.0.len() > OLDACL_MAX_ENTRIES {
        return Err(Errno::EINVAL);
    }
    acl.check()
}

/// `__acl_set_*`, or with no `acl`, `__acl_delete_*` (`ufs_setacl_posix1e`):
/// the caller must own `v` (or hold `PRIV_VFS_ADMIN`), and `v` must be
/// neither immutable nor append-only. An access ACL sets the mode's
/// permission bits.
pub fn set(
    s: &Sleepable,
    v: &Vnode,
    cred: &Cred,
    ty: Type,
    acl: Option<&Acl>,
) -> Result<(), Errno> {
    let acl = match acl {
        Some(acl) => {
            let mut acl = acl.clone();
            check(v, ty, &mut acl)?;
            Some(acl)
        }
        None => {
            supported(v, ty)?;
            if ty != Type::Default {
                return Err(Errno::EINVAL);
            }
            None
        }
    };
    if v.mount().read_only() {
        return Err(Errno::EROFS);
    }
    let a: Attr = v.attr(s)?;
    if a.flags & (IMMUTABLE | APPEND) != 0 {
        return Err(Errno::EPERM);
    }
    if cred.uid != a.uid {
        cred.priv_check(Priv::VfsAdmin)?;
    }
    {
        let mut st = v.write(s);
        let node = st.live_mut()?.node_mut();
        match &acl {
            Some(acl) => node.set_extattr(s, extattr::SYSTEM, ty.attr_name(), &acl.encode()?)?,
            None => match node.delete_extattr(s, extattr::SYSTEM, ty.attr_name()) {
                Ok(()) | Err(Errno::ENOATTR) => {}
                Err(e) => return Err(e),
            },
        }
        if ty == Type::Access
            && let Some(m) = acl.as_ref().and_then(Acl::to_mode)
        {
            node.setattr(
                s,
                &SetAttr {
                    mode: Some((a.mode & !OVERRIDE) | m),
                    ..SetAttr::default()
                },
            )?;
        }
    }
    v.knote(NOTE_ATTRIB);
    Ok(())
}
