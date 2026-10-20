// SPDX-License-Identifier: BSD-2-Clause
//! Process credentials (FreeBSD's `struct ucred`): the effective ids the
//! VFS checks, the real and saved ids the set-id calls move between, and
//! the supplementary groups. A process shares its credential by reference
//! and replaces it whole when one changes, so a call in progress keeps
//! the one it started with.
//!
//! The rules are FreeBSD's with `_POSIX_SAVED_IDS` and
//! `POSIX_APPENDIX_B_4_2_2`, and the superuser is euid 0 (no privilege
//! framework yet). As in FreeBSD 15, the supplementary groups do not
//! include the effective gid.
//!
//! Behavior reference: sys/kern/kern_prot.c.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Deref;

use abi::errno::Errno;
use vfs::Cred;

/// `NGROUPS_MAX`: how many supplementary groups a process may have.
pub(crate) const NGROUPS_MAX: usize = 1023;

/// A process's credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ucred {
    /// The effective uid and gid and the supplementary groups: what
    /// permission checks use.
    eff: Cred,
    pub(crate) ruid: u32,
    pub(crate) svuid: u32,
    pub(crate) rgid: u32,
    pub(crate) svgid: u32,
}

impl Deref for Ucred {
    type Target = Cred;

    fn deref(&self) -> &Cred {
        &self.eff
    }
}

/// Which ids a `setres*id` call sets; `None` leaves one as it is (-1).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ids {
    pub(crate) real: Option<u32>,
    pub(crate) effective: Option<u32>,
    pub(crate) saved: Option<u32>,
}

impl Ucred {
    /// The superuser, with no supplementary groups.
    pub(crate) const fn root() -> Self {
        Self {
            eff: Cred::root(),
            ruid: 0,
            svuid: 0,
            rgid: 0,
            svgid: 0,
        }
    }

    /// The superuser, shared.
    pub(crate) fn root_shared() -> Arc<Self> {
        Arc::new(Self::root())
    }

    pub(crate) const fn euid(&self) -> u32 {
        self.eff.uid
    }

    pub(crate) const fn egid(&self) -> u32 {
        self.eff.gid
    }

    pub(crate) fn groups(&self) -> &[u32] {
        &self.eff.groups
    }

    fn privileged(&self) -> bool {
        self.eff.is_root()
    }

    /// The same credential with the real ids in the effective ones'
    /// place: what `access` checks with (FreeBSD's `kern_accessat`).
    pub(crate) fn as_real(&self) -> Self {
        let mut c = self.clone();
        c.eff.uid = self.ruid;
        c.eff.gid = self.rgid;
        c
    }

    /// `setuid(uid)`.
    pub(crate) fn setuid(&self, uid: u32) -> Result<Self, Errno> {
        if uid != self.ruid && uid != self.svuid && uid != self.euid() && !self.privileged() {
            return Err(Errno::EPERM);
        }
        let mut c = self.clone();
        // Appendix B.4.2.2: setting the effective uid back, or the
        // superuser, sets all three.
        if uid == self.euid() || self.privileged() {
            c.ruid = uid;
            c.svuid = uid;
        }
        c.eff.uid = uid;
        Ok(c)
    }

    /// `seteuid(euid)`.
    pub(crate) fn seteuid(&self, euid: u32) -> Result<Self, Errno> {
        if euid != self.ruid && euid != self.svuid && !self.privileged() {
            return Err(Errno::EPERM);
        }
        let mut c = self.clone();
        c.eff.uid = euid;
        Ok(c)
    }

    /// `setgid(gid)`.
    pub(crate) fn setgid(&self, gid: u32) -> Result<Self, Errno> {
        if gid != self.rgid && gid != self.svgid && gid != self.egid() && !self.privileged() {
            return Err(Errno::EPERM);
        }
        let mut c = self.clone();
        if gid == self.egid() || self.privileged() {
            c.rgid = gid;
            c.svgid = gid;
        }
        c.eff.gid = gid;
        Ok(c)
    }

    /// `setegid(egid)`.
    pub(crate) fn setegid(&self, egid: u32) -> Result<Self, Errno> {
        if egid != self.rgid && egid != self.svgid && !self.privileged() {
            return Err(Errno::EPERM);
        }
        let mut c = self.clone();
        c.eff.gid = egid;
        Ok(c)
    }

    /// `setgroups(groups)`: the superuser only. The groups are kept sorted
    /// without repeats (FreeBSD's `groups_normalize`).
    pub(crate) fn setgroups(&self, groups: &[u32]) -> Result<Self, Errno> {
        if groups.len() > NGROUPS_MAX {
            return Err(Errno::EINVAL);
        }
        if !self.privileged() {
            return Err(Errno::EPERM);
        }
        let mut g = groups.to_vec();
        g.sort_unstable();
        g.dedup();
        let mut c = self.clone();
        c.eff.groups = g;
        Ok(c)
    }

    /// `setresuid(ruid, euid, suid)`: each must be one of the three
    /// current uids unless the caller is the superuser.
    pub(crate) fn setresuid(&self, ids: Ids) -> Result<Self, Errno> {
        let cur = [self.ruid, self.euid(), self.svuid];
        let ok = |id: Option<u32>| id.is_none_or(|i| cur.contains(&i));
        if !self.privileged() && !(ok(ids.real) && ok(ids.effective) && ok(ids.saved)) {
            return Err(Errno::EPERM);
        }
        let mut c = self.clone();
        c.ruid = ids.real.unwrap_or(c.ruid);
        c.eff.uid = ids.effective.unwrap_or(c.eff.uid);
        c.svuid = ids.saved.unwrap_or(c.svuid);
        Ok(c)
    }

    /// `setresgid(rgid, egid, sgid)`.
    pub(crate) fn setresgid(&self, ids: Ids) -> Result<Self, Errno> {
        let cur = [self.rgid, self.egid(), self.svgid];
        let ok = |id: Option<u32>| id.is_none_or(|i| cur.contains(&i));
        if !self.privileged() && !(ok(ids.real) && ok(ids.effective) && ok(ids.saved)) {
            return Err(Errno::EPERM);
        }
        let mut c = self.clone();
        c.rgid = ids.real.unwrap_or(c.rgid);
        c.eff.gid = ids.effective.unwrap_or(c.eff.gid);
        c.svgid = ids.saved.unwrap_or(c.svgid);
        Ok(c)
    }

    /// `setreuid(ruid, euid)`: the real uid may become the effective one
    /// or stay; the effective uid may become the real or saved one. The
    /// saved uid follows the effective one when the real uid is set, or
    /// when the effective uid moves off the real one.
    pub(crate) fn setreuid(&self, ruid: Option<u32>, euid: Option<u32>) -> Result<Self, Errno> {
        let r_ok = ruid.is_none_or(|r| r == self.ruid || r == self.svuid);
        let e_ok = euid.is_none_or(|e| e == self.euid() || e == self.ruid || e == self.svuid);
        if !self.privileged() && !(r_ok && e_ok) {
            return Err(Errno::EPERM);
        }
        let mut c = self.clone();
        if let Some(e) = euid {
            c.eff.uid = e;
        }
        if let Some(r) = ruid {
            c.ruid = r;
        }
        if (ruid.is_some() || c.eff.uid != c.ruid) && c.svuid != c.eff.uid {
            c.svuid = c.eff.uid;
        }
        Ok(c)
    }

    /// `setregid(rgid, egid)`, as [`Self::setreuid`].
    pub(crate) fn setregid(&self, rgid: Option<u32>, egid: Option<u32>) -> Result<Self, Errno> {
        let r_ok = rgid.is_none_or(|r| r == self.rgid || r == self.svgid);
        let e_ok = egid.is_none_or(|e| e == self.egid() || e == self.rgid || e == self.svgid);
        if !self.privileged() && !(r_ok && e_ok) {
            return Err(Errno::EPERM);
        }
        let mut c = self.clone();
        if let Some(e) = egid {
            c.eff.gid = e;
        }
        if let Some(r) = rgid {
            c.rgid = r;
        }
        if (rgid.is_some() || c.eff.gid != c.rgid) && c.svgid != c.eff.gid {
            c.svgid = c.eff.gid;
        }
        Ok(c)
    }
}

/// Supplementary groups from user memory, for `setgroups`.
pub(crate) fn groups_from(raw: &[u8]) -> Vec<u32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|&b| u32::from_ne_bytes(b))
        .collect()
}
