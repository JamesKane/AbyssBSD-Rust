// SPDX-License-Identifier: BSD-2-Clause
//! Credentials and privilege (docs/MULTIUSER.md §10.1): FreeBSD's
//! `struct ucred`, the set-id rules of kern_prot.c, `priv_check` with the
//! superuser and jail policies, and securelevel.
//!
//! A [`Cred`] is shared by reference and replaced whole when it changes,
//! so a call in progress keeps the one it started with. As in FreeBSD 15,
//! the supplementary groups do not include the effective gid. The rules
//! are FreeBSD's with `_POSIX_SAVED_IDS` and `POSIX_APPENDIX_B_4_2_2`.
//!
//! The jail a credential is in is a [`Jail`], which the kernel implements
//! (its prisons hold vnodes and processes); a credential in no jail is in
//! `prison0`, whose settings are this crate's globals ([`prison0`]).
//!
//! Behavior reference: sys/kern/kern_prot.c, sys/kern/kern_priv.c,
//! sys/kern/kern_jail.c (`prison_priv_check`, `prison_check`).

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

pub mod privilege;
#[cfg(test)]
mod tests;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use abi::errno::Errno;

pub use privilege::Priv;

/// `NGROUPS_MAX`: how many supplementary groups a process may have.
pub const NGROUPS_MAX: usize = 1023;

/// `CRED_FLAG_CAPMODE`: in Capsicum capability mode.
pub const CRED_FLAG_CAPMODE: u32 = 0x1;

/// The jail allow flags (`PR_ALLOW_*`) the policies read.
pub mod allow {
    pub const SET_HOSTNAME: u32 = 0x0000_0001;
    pub const SYSVIPC: u32 = 0x0000_0002;
    pub const RAW_SOCKETS: u32 = 0x0000_0004;
    pub const CHFLAGS: u32 = 0x0000_0008;
    pub const MOUNT: u32 = 0x0000_0010;
    pub const QUOTAS: u32 = 0x0000_0020;
    pub const SOCKET_AF: u32 = 0x0000_0040;
    pub const MLOCK: u32 = 0x0000_0080;
    pub const READ_MSGBUF: u32 = 0x0000_0100;
    pub const UNPRIV_DEBUG: u32 = 0x0000_0200;
    pub const SUSER: u32 = 0x0000_0400;
    pub const RESERVED_PORTS: u32 = 0x0000_8000;
    pub const KMEM_ACCESS: u32 = 0x0001_0000;
    pub const NFSD: u32 = 0x0002_0000;
    pub const EXTATTR: u32 = 0x0004_0000;
    pub const ADJTIME: u32 = 0x0008_0000;
    pub const SETTIME: u32 = 0x0010_0000;
    pub const ROUTING: u32 = 0x0020_0000;
    pub const UNPRIV_PARENT_TAMPER: u32 = 0x0040_0000;
    pub const SETAUDIT: u32 = 0x0080_0000;
    /// Every flag allocated at build time (`PR_ALLOW_ALL_STATIC`).
    pub const ALL_STATIC: u32 = 0x00ff_87ff;
    /// prison0's (`PR_ALLOW_PRISON0`).
    pub const PRISON0: u32 = ALL_STATIC & !UNPRIV_PARENT_TAMPER;
}

/// A jail, as the policies here see it; the kernel's prisons implement
/// it. prison0 is no `Jail`: a credential in it has none.
pub trait Jail: Send + Sync + core::fmt::Debug {
    /// Its id (`pr_id`).
    fn id(&self) -> i32;
    /// Its parent, `None` for a child of prison0.
    fn parent(&self) -> Option<Arc<dyn Jail>>;
    /// Its `PR_ALLOW_*` flags (`pr_allow`).
    fn allow(&self) -> u32;
    /// Its `enforce_statfs`.
    fn enforce_statfs(&self) -> i32;
    /// Its securelevel, never below its parent's (`pr_securelevel`).
    fn securelevel(&self) -> i32;
    /// Whether it has a network stack of its own (`PR_VNET`).
    fn vnet(&self) -> bool {
        false
    }
    /// For the kernel to reach its own prison.
    fn as_any(&self) -> &dyn Any;
}

/// prison0's settings: the system's, which jails inherit from.
pub mod prison0 {
    use super::{AtomicI32, AtomicU32, allow};

    /// Its `PR_ALLOW_*` flags; `security.bsd.suser_enabled` and
    /// `security.bsd.unprivileged_proc_debug` are two of them.
    pub static ALLOW: AtomicU32 = AtomicU32::new(allow::PRISON0);
    /// `kern.securelevel`: -1 (insecure) until raised; it only rises.
    pub static SECURELEVEL: AtomicI32 = AtomicI32::new(-1);
}

/// The `security.bsd` knobs that are no jail flag.
pub mod knobs {
    use super::AtomicBool;

    /// `security.bsd.unprivileged_mlock`.
    pub static UNPRIVILEGED_MLOCK: AtomicBool = AtomicBool::new(true);
    /// `security.bsd.unprivileged_read_msgbuf`.
    pub static UNPRIVILEGED_READ_MSGBUF: AtomicBool = AtomicBool::new(true);
    /// `security.bsd.see_other_uids`.
    pub static SEE_OTHER_UIDS: AtomicBool = AtomicBool::new(true);
    /// `security.bsd.see_other_gids`.
    pub static SEE_OTHER_GIDS: AtomicBool = AtomicBool::new(true);
    /// `security.bsd.see_jail_proc`.
    pub static SEE_JAIL_PROC: AtomicBool = AtomicBool::new(true);
    /// `security.bsd.conservative_signals`.
    pub static CONSERVATIVE_SIGNALS: AtomicBool = AtomicBool::new(true);
    /// `security.bsd.hardlink_check_uid`.
    pub static HARDLINK_CHECK_UID: AtomicBool = AtomicBool::new(false);
    /// `security.bsd.hardlink_check_gid`.
    pub static HARDLINK_CHECK_GID: AtomicBool = AtomicBool::new(false);
}

/// A login class name (`cr_loginclass`); `None` is `default`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginClass(Option<Arc<[u8]>>);

impl LoginClass {
    /// The class `name` (`default` is the default one).
    #[must_use]
    pub fn new(name: &[u8]) -> Self {
        if name == b"default" {
            Self(None)
        } else {
            Self(Some(name.into()))
        }
    }

    #[must_use]
    pub fn name(&self) -> &[u8] {
        self.0.as_deref().unwrap_or(b"default")
    }
}

/// A process's credential (FreeBSD's `struct ucred`).
#[derive(Clone, Debug)]
pub struct Cred {
    /// The effective uid: what permission checks use.
    pub uid: u32,
    pub ruid: u32,
    pub svuid: u32,
    /// The effective gid.
    pub gid: u32,
    pub rgid: u32,
    pub svgid: u32,
    /// The supplementary groups, sorted, without repeats.
    pub groups: Vec<u32>,
    /// `CRED_FLAG_*`.
    pub flags: u32,
    /// The jail it is in, `None` for prison0.
    pub jail: Option<Arc<dyn Jail>>,
    pub loginclass: LoginClass,
}

impl PartialEq for Cred {
    fn eq(&self, o: &Self) -> bool {
        self.uid == o.uid
            && self.ruid == o.ruid
            && self.svuid == o.svuid
            && self.gid == o.gid
            && self.rgid == o.rgid
            && self.svgid == o.svgid
            && self.groups == o.groups
            && self.flags == o.flags
            && same_jail(self.jail.as_ref(), o.jail.as_ref())
            && self.loginclass == o.loginclass
    }
}

impl Eq for Cred {}

fn same_jail(a: Option<&Arc<dyn Jail>>, b: Option<&Arc<dyn Jail>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.id() == b.id(),
        _ => false,
    }
}

/// Which ids a `setres*id` call sets; `None` leaves one as it is (-1).
#[derive(Clone, Copy, Debug)]
pub struct Ids {
    pub real: Option<u32>,
    pub effective: Option<u32>,
    pub saved: Option<u32>,
}

/// What `setcred` sets (`struct setcred` with its `SETCREDF_*` flags).
#[derive(Clone, Debug, Default)]
pub struct SetCred {
    pub uid: Option<u32>,
    pub ruid: Option<u32>,
    pub svuid: Option<u32>,
    pub gid: Option<u32>,
    pub rgid: Option<u32>,
    pub svgid: Option<u32>,
    pub groups: Option<Vec<u32>>,
}

impl Cred {
    /// The superuser in prison0, with no supplementary groups.
    #[must_use]
    pub const fn root() -> Self {
        Self {
            uid: 0,
            ruid: 0,
            svuid: 0,
            gid: 0,
            rgid: 0,
            svgid: 0,
            groups: Vec::new(),
            flags: 0,
            jail: None,
            loginclass: LoginClass(None),
        }
    }

    /// User `uid` in group `gid` (all three ids of each), in prison0.
    #[must_use]
    pub fn user(uid: u32, gid: u32, groups: &[u32]) -> Self {
        let mut c = Self::root();
        (c.uid, c.ruid, c.svuid) = (uid, uid, uid);
        (c.gid, c.rgid, c.svgid) = (gid, gid, gid);
        c.groups = normalize(groups);
        c
    }

    #[must_use]
    pub const fn euid(&self) -> u32 {
        self.uid
    }

    #[must_use]
    pub const fn egid(&self) -> u32 {
        self.gid
    }

    #[must_use]
    pub fn groups(&self) -> &[u32] {
        &self.groups
    }

    /// Whether `gid` is the effective or a supplementary group
    /// (`groupmember`).
    #[must_use]
    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.groups.contains(&gid)
    }

    /// Whether `gid` is the real or a supplementary group
    /// (`realgroupmember`).
    #[must_use]
    pub fn in_real_group(&self, gid: u32) -> bool {
        self.rgid == gid || self.groups.contains(&gid)
    }

    /// Whether it is in a jail (`jailed`).
    #[must_use]
    pub const fn jailed(&self) -> bool {
        self.jail.is_some()
    }

    /// Its jail's id, 0 for prison0.
    #[must_use]
    pub fn jail_id(&self) -> i32 {
        self.jail.as_ref().map_or(0, |j| j.id())
    }

    /// Whether it is in capability mode (`IN_CAPABILITY_MODE`).
    #[must_use]
    pub const fn in_capability_mode(&self) -> bool {
        self.flags & CRED_FLAG_CAPMODE != 0
    }

    /// Its jail's allow flags (`prison_allow`).
    #[must_use]
    pub fn allows(&self, flag: u32) -> bool {
        let a = self
            .jail
            .as_ref()
            .map_or_else(|| prison0::ALLOW.load(Ordering::Relaxed), |j| j.allow());
        a & flag != 0
    }

    /// Whether the superuser policy applies (`suser_enabled`).
    fn suser_enabled(&self) -> bool {
        self.allows(allow::SUSER)
    }

    /// The superuser at all, as `PRIV_VFS_LOOKUP`'s fast path asks: root,
    /// with the superuser policy enabled.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.uid == 0 && self.suser_enabled()
    }

    /// `priv_check_cred`: `Ok` if the credential holds `p`, `EPERM` if not.
    pub fn priv_check(&self, p: Priv) -> Result<(), Errno> {
        match p {
            Priv::VfsLookup => {
                return if self.is_root() {
                    Ok(())
                } else {
                    Err(Errno::EPERM)
                };
            }
            Priv::VfsGeneration => {
                return if !self.jailed() && self.is_root() {
                    Ok(())
                } else {
                    Err(Errno::EPERM)
                };
            }
            _ => {}
        }
        self.prison_priv_check(p)?;
        if knobs::UNPRIVILEGED_MLOCK.load(Ordering::Relaxed)
            && matches!(p, Priv::VmMlock | Priv::VmMunlock)
        {
            return Ok(());
        }
        if knobs::UNPRIVILEGED_READ_MSGBUF.load(Ordering::Relaxed) && p == Priv::Msgbuf {
            return Ok(());
        }
        if self.suser_enabled() {
            match p {
                // These go by the real uid.
                Priv::Maxfiles | Priv::Maxproc | Priv::ProcLimit => {
                    if self.ruid == 0 {
                        return Ok(());
                    }
                }
                Priv::VfsReadDir if self.jailed() => {}
                _ => {
                    if self.uid == 0 {
                        return Ok(());
                    }
                }
            }
        }
        // Reading kernel memory is for anyone who can open it; writing a
        // process's was already checked as debugging.
        if matches!(p, Priv::KmemRead | Priv::ProcMemWrite) {
            return Ok(());
        }
        if p == Priv::DebugUnpriv && self.allows(allow::UNPRIV_DEBUG) {
            return Ok(());
        }
        Err(Errno::EPERM)
    }

    /// The jail policy (`prison_priv_check`): what root in a jail is
    /// refused.
    fn prison_priv_check(&self, p: Priv) -> Result<(), Errno> {
        let Some(j) = &self.jail else {
            return Ok(());
        };
        let allowed = |flag| {
            if j.allow() & flag != 0 {
                Ok(())
            } else {
                Err(Errno::EPERM)
            }
        };
        // A VNET jail administers its own network stack.
        if j.vnet()
            && matches!(
                p,
                Priv::NetRoute
                    | Priv::NetSetifmtu
                    | Priv::NetSetifflags
                    | Priv::NetSetifcap
                    | Priv::NetSetifdescr
                    | Priv::NetSetifname
                    | Priv::NetSetifmetric
                    | Priv::NetSetlladdr
                    | Priv::NetAddifgroup
                    | Priv::NetDelifgroup
                    | Priv::NetIfcreate
                    | Priv::NetIfdestroy
                    | Priv::NetAddifaddr
                    | Priv::NetDelifaddr
                    | Priv::NetSetifvnet
                    | Priv::NetSetiffib
                    | Priv::NetinetRaw
                    | Priv::NetinetAddrctl6
                    | Priv::NetinetNd6
                    | Priv::NetinetBindany
            )
        {
            return Ok(());
        }
        match p {
            Priv::Ktrace
            | Priv::CredSetcred
            | Priv::CredSetuid
            | Priv::CredSeteuid
            | Priv::CredSetgid
            | Priv::CredSetegid
            | Priv::CredSetgroups
            | Priv::CredSetreuid
            | Priv::CredSetregid
            | Priv::CredSetresuid
            | Priv::CredSetresgid
            | Priv::Seeothergids
            | Priv::Seeotheruids
            | Priv::Seejailproc
            | Priv::DebugDiffcred
            | Priv::DebugSugid
            | Priv::DebugUnpriv
            | Priv::DebugDiffjail
            | Priv::ProcLimit
            | Priv::ProcSetlogin
            | Priv::ProcSetrlimit
            | Priv::ProcMemWrite
            | Priv::IpcRead
            | Priv::IpcWrite
            | Priv::IpcAdmin
            | Priv::IpcMsgsize
            | Priv::MqAdmin
            | Priv::JailAttach
            | Priv::JailSet
            | Priv::JailRemove
            | Priv::SchedDiffcred
            | Priv::SchedCpuset
            | Priv::SchedDiffjail
            | Priv::SignalDiffcred
            | Priv::SignalSugid
            | Priv::SignalDiffjail
            | Priv::SysctlWritejail
            | Priv::VfsGetquota
            | Priv::VfsSetquota
            | Priv::VfsRead
            | Priv::VfsWrite
            | Priv::VfsAdmin
            | Priv::VfsExec
            | Priv::VfsBlockreserve
            | Priv::VfsChflagsDev
            | Priv::VfsChown
            | Priv::VfsChroot
            | Priv::VfsRetainsugid
            | Priv::VfsFchroot
            | Priv::VfsLink
            | Priv::VfsSetgid
            | Priv::VfsStat
            | Priv::VfsStickyfile
            | Priv::KmemRead
            | Priv::VfsReadDir
            | Priv::NetinetReuseport
            | Priv::NetinetGetcred
            | Priv::ProcSetloginclass
            | Priv::VmmPptdev => Ok(()),
            Priv::AuditGetaudit | Priv::AuditSetaudit => allowed(allow::SETAUDIT),
            Priv::VfsSysflags => allowed(allow::CHFLAGS),
            Priv::VfsMount | Priv::VfsUnmount | Priv::VfsMountNonuser | Priv::VfsMountOwner => {
                if j.allow() & allow::MOUNT != 0 && j.enforce_statfs() < 2 {
                    Ok(())
                } else {
                    Err(Errno::EPERM)
                }
            }
            Priv::VfsExtattrSystem => allowed(allow::EXTATTR),
            Priv::VmMlock | Priv::VmMunlock => allowed(allow::MLOCK),
            Priv::NetinetReservedport => allowed(allow::RESERVED_PORTS),
            Priv::NetinetRaw => allowed(allow::RAW_SOCKETS),
            Priv::Msgbuf => allowed(allow::READ_MSGBUF),
            Priv::Adjtime | Priv::NtpAdjtime => allowed(allow::ADJTIME | allow::SETTIME),
            Priv::Settimeofday | Priv::ClockSettime => allowed(allow::SETTIME),
            Priv::NetRoute => allowed(allow::ROUTING),
            _ => Err(Errno::EPERM),
        }
    }

    /// The securelevel it is held to: its jail's, or the system's.
    #[must_use]
    pub fn securelevel(&self) -> i32 {
        let host = prison0::SECURELEVEL.load(Ordering::Relaxed);
        self.jail
            .as_ref()
            .map_or(host, |j| j.securelevel().max(host))
    }

    /// `securelevel_gt`: `EPERM` if the securelevel is above `level`.
    pub fn securelevel_gt(&self, level: i32) -> Result<(), Errno> {
        if self.securelevel() > level {
            Err(Errno::EPERM)
        } else {
            Ok(())
        }
    }

    /// `securelevel_ge`: `EPERM` if the securelevel is `level` or above.
    pub fn securelevel_ge(&self, level: i32) -> Result<(), Errno> {
        if self.securelevel() >= level {
            Err(Errno::EPERM)
        } else {
            Ok(())
        }
    }

    /// `prison_check`: `ESRCH` unless `other` is in this credential's
    /// jail or one below it.
    pub fn prison_check(&self, other: &Self) -> Result<(), Errno> {
        let Some(mine) = &self.jail else {
            return Ok(());
        };
        let mut cur = other.jail.clone();
        while let Some(j) = cur {
            if j.id() == mine.id() {
                return Ok(());
            }
            cur = j.parent();
        }
        Err(Errno::ESRCH)
    }

    /// `cr_canseeotheruids`: `ESRCH` if `see_other_uids` is off, the ids
    /// differ, and it lacks `PRIV_SEEOTHERUIDS`.
    pub fn can_see_other_uids(&self, other: &Self) -> Result<(), Errno> {
        if !knobs::SEE_OTHER_UIDS.load(Ordering::Relaxed)
            && self.ruid != other.ruid
            && self.priv_check(Priv::Seeotheruids).is_err()
        {
            return Err(Errno::ESRCH);
        }
        Ok(())
    }

    /// `cr_canseeothergids`: `ESRCH` if `see_other_gids` is off, it shares
    /// no group with `other`, and it lacks `PRIV_SEEOTHERGIDS`.
    pub fn can_see_other_gids(&self, other: &Self) -> Result<(), Errno> {
        if knobs::SEE_OTHER_GIDS.load(Ordering::Relaxed) {
            return Ok(());
        }
        let shared = core::iter::once(&self.rgid)
            .chain(self.groups.iter())
            .any(|&g| other.in_real_group(g));
        if !shared && self.priv_check(Priv::Seeothergids).is_err() {
            return Err(Errno::ESRCH);
        }
        Ok(())
    }

    /// `cr_canseejailproc`: `ESRCH` if `see_jail_proc` is off, `other` is
    /// in a jail below this one's, and it lacks `PRIV_SEEJAILPROC`.
    pub fn can_see_jail_proc(&self, other: &Self) -> Result<(), Errno> {
        if knobs::SEE_JAIL_PROC.load(Ordering::Relaxed)
            || same_jail(self.jail.as_ref(), other.jail.as_ref())
            || self.priv_check(Priv::Seejailproc).is_ok()
        {
            return Ok(());
        }
        Err(Errno::ESRCH)
    }

    /// `cr_cansee`: whether processes with this credential may see one
    /// with `other` (`ESRCH` if not).
    pub fn can_see(&self, other: &Self) -> Result<(), Errno> {
        self.prison_check(other)?;
        self.bsd_visible(other)
    }

    /// `cr_bsd_visible`: the `see_other_*` and `see_jail_proc` rules.
    pub fn bsd_visible(&self, other: &Self) -> Result<(), Errno> {
        self.can_see_other_uids(other)?;
        self.can_see_other_gids(other)?;
        self.can_see_jail_proc(other)
    }

    /// `cr_can_tamper_with_subjail`: acting on `other` in a jail below
    /// this one's needs `p`, or `allow.unprivileged_parent_tampering` on
    /// `other`'s jail.
    pub fn can_tamper_with_subjail(&self, other: &Self, p: Priv) -> Result<(), Errno> {
        if same_jail(self.jail.as_ref(), other.jail.as_ref())
            || self.priv_check(p).is_ok()
            || other.allows(allow::UNPRIV_PARENT_TAMPER)
        {
            return Ok(());
        }
        Err(Errno::EPERM)
    }

    /// `cr_cansignal`: whether it may send `sig` to a process with
    /// credential `target`, `sugid` if that one is set-id tainted.
    pub fn can_signal(&self, target: &Self, sugid: bool, sig: u32) -> Result<(), Errno> {
        use abi::signal::{
            SIGALRM, SIGHUP, SIGINT, SIGKILL, SIGSTOP, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU, SIGUSR1,
            SIGUSR2,
        };
        self.prison_check(target)?;
        self.bsd_visible(target)?;
        if knobs::CONSERVATIVE_SIGNALS.load(Ordering::Relaxed)
            && sugid
            && !matches!(
                sig,
                0 | SIGKILL
                    | SIGINT
                    | SIGTERM
                    | SIGALRM
                    | SIGSTOP
                    | SIGTTIN
                    | SIGTTOU
                    | SIGTSTP
                    | SIGHUP
                    | SIGUSR1
                    | SIGUSR2
            )
        {
            self.priv_check(Priv::SignalSugid)?;
        }
        if self.ruid != target.ruid
            && self.ruid != target.svuid
            && self.uid != target.ruid
            && self.uid != target.svuid
        {
            self.priv_check(Priv::SignalDiffcred)?;
        }
        self.can_tamper_with_subjail(target, Priv::SignalDiffjail)
    }

    /// `p_cansched`'s credential rules, for a process with credential
    /// `target`.
    pub fn can_sched(&self, target: &Self) -> Result<(), Errno> {
        self.prison_check(target)?;
        self.bsd_visible(target)?;
        if self.ruid != target.ruid && self.uid != target.ruid {
            self.priv_check(Priv::SchedDiffcred)?;
        }
        self.can_tamper_with_subjail(target, Priv::SchedDiffjail)
    }

    /// `cr_xids_subset`: whether every id of `other` is its effective uid
    /// or one of its groups.
    #[must_use]
    pub fn xids_subset(&self, other: &Self) -> bool {
        let groups = other.groups.iter().all(|&g| self.in_group(g))
            && self.in_group(other.gid)
            && self.in_group(other.rgid)
            && self.in_group(other.svgid);
        groups && self.uid == other.uid && self.uid == other.svuid && self.uid == other.ruid
    }

    /// `p_candebug`'s credential rules, for a process with credential
    /// `target`, `sugid` if that one is set-id tainted.
    pub fn can_debug(&self, target: &Self, sugid: bool) -> Result<(), Errno> {
        self.priv_check(Priv::DebugUnpriv)?;
        self.prison_check(target)?;
        self.bsd_visible(target)?;
        if !self.xids_subset(target) {
            self.priv_check(Priv::DebugDiffcred)?;
        }
        if sugid {
            self.priv_check(Priv::DebugSugid)?;
        }
        self.can_tamper_with_subjail(target, Priv::DebugDiffjail)
    }

    /// The same credential with the real ids in the effective ones'
    /// place: what `access` checks with (FreeBSD's `kern_accessat`).
    #[must_use]
    pub fn as_real(&self) -> Self {
        let mut c = self.clone();
        c.uid = self.ruid;
        c.gid = self.rgid;
        c
    }

    /// `setuid(uid)`.
    pub fn setuid(&self, uid: u32) -> Result<Self, Errno> {
        if uid != self.ruid && uid != self.svuid && uid != self.uid {
            self.priv_check(Priv::CredSetuid)?;
        }
        let mut c = self.clone();
        // Appendix B.4.2.2: setting the effective uid back, or with the
        // privilege, sets all three.
        if uid == self.uid || self.priv_check(Priv::CredSetuid).is_ok() {
            c.ruid = uid;
            c.svuid = uid;
        }
        c.uid = uid;
        Ok(c)
    }

    /// `seteuid(euid)`.
    pub fn seteuid(&self, euid: u32) -> Result<Self, Errno> {
        if euid != self.ruid && euid != self.svuid {
            self.priv_check(Priv::CredSeteuid)?;
        }
        let mut c = self.clone();
        c.uid = euid;
        Ok(c)
    }

    /// `setgid(gid)`.
    pub fn setgid(&self, gid: u32) -> Result<Self, Errno> {
        if gid != self.rgid && gid != self.svgid && gid != self.gid {
            self.priv_check(Priv::CredSetgid)?;
        }
        let mut c = self.clone();
        if gid == self.gid || self.priv_check(Priv::CredSetgid).is_ok() {
            c.rgid = gid;
            c.svgid = gid;
        }
        c.gid = gid;
        Ok(c)
    }

    /// `setegid(egid)`.
    pub fn setegid(&self, egid: u32) -> Result<Self, Errno> {
        if egid != self.rgid && egid != self.svgid {
            self.priv_check(Priv::CredSetegid)?;
        }
        let mut c = self.clone();
        c.gid = egid;
        Ok(c)
    }

    /// `setgroups(groups)`: with `PRIV_CRED_SETGROUPS` only. The groups are
    /// kept sorted without repeats (`groups_normalize`).
    pub fn setgroups(&self, groups: &[u32]) -> Result<Self, Errno> {
        if groups.len() > NGROUPS_MAX {
            return Err(Errno::EINVAL);
        }
        self.priv_check(Priv::CredSetgroups)?;
        let mut c = self.clone();
        c.groups = normalize(groups);
        Ok(c)
    }

    /// `setresuid(ruid, euid, suid)`: each must be one of the three
    /// current uids unless the caller holds `PRIV_CRED_SETRESUID`.
    pub fn setresuid(&self, ids: Ids) -> Result<Self, Errno> {
        let cur = [self.ruid, self.uid, self.svuid];
        let ok = |id: Option<u32>| id.is_none_or(|i| cur.contains(&i));
        if !(ok(ids.real) && ok(ids.effective) && ok(ids.saved)) {
            self.priv_check(Priv::CredSetresuid)?;
        }
        let mut c = self.clone();
        c.ruid = ids.real.unwrap_or(c.ruid);
        c.uid = ids.effective.unwrap_or(c.uid);
        c.svuid = ids.saved.unwrap_or(c.svuid);
        Ok(c)
    }

    /// `setresgid(rgid, egid, sgid)`.
    pub fn setresgid(&self, ids: Ids) -> Result<Self, Errno> {
        let cur = [self.rgid, self.gid, self.svgid];
        let ok = |id: Option<u32>| id.is_none_or(|i| cur.contains(&i));
        if !(ok(ids.real) && ok(ids.effective) && ok(ids.saved)) {
            self.priv_check(Priv::CredSetresgid)?;
        }
        let mut c = self.clone();
        c.rgid = ids.real.unwrap_or(c.rgid);
        c.gid = ids.effective.unwrap_or(c.gid);
        c.svgid = ids.saved.unwrap_or(c.svgid);
        Ok(c)
    }

    /// `setreuid(ruid, euid)`: the real uid may become the saved one or
    /// stay; the effective uid may become the real or saved one. The saved
    /// uid follows the effective one when the real uid is set, or when the
    /// effective uid moves off the real one.
    pub fn setreuid(&self, ruid: Option<u32>, euid: Option<u32>) -> Result<Self, Errno> {
        let r_ok = ruid.is_none_or(|r| r == self.ruid || r == self.svuid);
        let e_ok = euid.is_none_or(|e| e == self.uid || e == self.ruid || e == self.svuid);
        if !(r_ok && e_ok) {
            self.priv_check(Priv::CredSetreuid)?;
        }
        let mut c = self.clone();
        if let Some(e) = euid {
            c.uid = e;
        }
        if let Some(r) = ruid {
            c.ruid = r;
        }
        if (ruid.is_some() || c.uid != c.ruid) && c.svuid != c.uid {
            c.svuid = c.uid;
        }
        Ok(c)
    }

    /// `setregid(rgid, egid)`, as [`Self::setreuid`].
    pub fn setregid(&self, rgid: Option<u32>, egid: Option<u32>) -> Result<Self, Errno> {
        let r_ok = rgid.is_none_or(|r| r == self.rgid || r == self.svgid);
        let e_ok = egid.is_none_or(|e| e == self.gid || e == self.rgid || e == self.svgid);
        if !(r_ok && e_ok) {
            self.priv_check(Priv::CredSetregid)?;
        }
        let mut c = self.clone();
        if let Some(e) = egid {
            c.gid = e;
        }
        if let Some(r) = rgid {
            c.rgid = r;
        }
        if (rgid.is_some() || c.gid != c.rgid) && c.svgid != c.gid {
            c.svgid = c.gid;
        }
        Ok(c)
    }

    /// `setcred(flags, wcred)` (`kern_setcred`): sets what `w` names, with
    /// `PRIV_CRED_SETCRED` only.
    pub fn setcred(&self, w: &SetCred) -> Result<Self, Errno> {
        if w.groups.as_ref().is_some_and(|g| g.len() > NGROUPS_MAX) {
            return Err(Errno::EINVAL);
        }
        let mut c = self.clone();
        c.uid = w.uid.unwrap_or(c.uid);
        c.ruid = w.ruid.unwrap_or(c.ruid);
        c.svuid = w.svuid.unwrap_or(c.svuid);
        if let Some(g) = &w.groups {
            c.groups = normalize(g);
        }
        c.gid = w.gid.unwrap_or(c.gid);
        c.rgid = w.rgid.unwrap_or(c.rgid);
        c.svgid = w.svgid.unwrap_or(c.svgid);
        self.priv_check(Priv::CredSetcred)?;
        Ok(c)
    }
}

/// Groups sorted without repeats (`groups_normalize`).
#[must_use]
pub fn normalize(groups: &[u32]) -> Vec<u32> {
    let mut g = groups.to_vec();
    g.sort_unstable();
    g.dedup();
    g
}
