// SPDX-License-Identifier: BSD-2-Clause
//! Capsicum (`sys/capsicum.h`, `sys/caprights.h`): a descriptor's rights
//! (`cap_rights_t`), each right's encoding, and the `fcntl` and `ioctl`
//! limits.
//!
//! A right is a word whose bit 57 + `i` names the array element it lives
//! in (`CAPRIGHT(i, bit)`); element 0 also carries the structure's version
//! in its top two bits. Version 0 has two elements.

use crate::layout::{Arches, ConstValue};

/// `CAP_RIGHTS_VERSION`.
pub const CAP_RIGHTS_VERSION: u32 = 0;

/// `CAPRIGHT(idx, bit)`.
pub const fn right(idx: u32, bit: u64) -> u64 {
    (1 << (57 + idx)) | bit
}

pub const CAP_READ: u64 = right(0, 0x0000000000000001);
pub const CAP_WRITE: u64 = right(0, 0x0000000000000002);
pub const CAP_SEEK_TELL: u64 = right(0, 0x0000000000000004);
pub const CAP_SEEK: u64 = CAP_SEEK_TELL | 0x0000000000000008;
pub const CAP_PREAD: u64 = CAP_SEEK | CAP_READ;
pub const CAP_PWRITE: u64 = CAP_SEEK | CAP_WRITE;
pub const CAP_MMAP: u64 = right(0, 0x0000000000000010);
pub const CAP_MMAP_R: u64 = CAP_MMAP | CAP_SEEK | CAP_READ;
pub const CAP_MMAP_W: u64 = CAP_MMAP | CAP_SEEK | CAP_WRITE;
pub const CAP_MMAP_X: u64 = CAP_MMAP | CAP_SEEK | 0x0000000000000020;
pub const CAP_MMAP_RW: u64 = CAP_MMAP_R | CAP_MMAP_W;
pub const CAP_MMAP_RX: u64 = CAP_MMAP_R | CAP_MMAP_X;
pub const CAP_MMAP_WX: u64 = CAP_MMAP_W | CAP_MMAP_X;
pub const CAP_MMAP_RWX: u64 = CAP_MMAP_R | CAP_MMAP_W | CAP_MMAP_X;
pub const CAP_CREATE: u64 = right(0, 0x0000000000000040);
pub const CAP_FEXECVE: u64 = right(0, 0x0000000000000080);
pub const CAP_FSYNC: u64 = right(0, 0x0000000000000100);
pub const CAP_FTRUNCATE: u64 = right(0, 0x0000000000000200);
pub const CAP_LOOKUP: u64 = right(0, 0x0000000000000400);
pub const CAP_FCHDIR: u64 = right(0, 0x0000000000000800);
pub const CAP_FCHFLAGS: u64 = right(0, 0x0000000000001000);
pub const CAP_CHFLAGSAT: u64 = CAP_FCHFLAGS | CAP_LOOKUP;
pub const CAP_FCHMOD: u64 = right(0, 0x0000000000002000);
pub const CAP_FCHMODAT: u64 = CAP_FCHMOD | CAP_LOOKUP;
pub const CAP_FCHOWN: u64 = right(0, 0x0000000000004000);
pub const CAP_FCHOWNAT: u64 = CAP_FCHOWN | CAP_LOOKUP;
pub const CAP_FCNTL: u64 = right(0, 0x0000000000008000);
pub const CAP_FLOCK: u64 = right(0, 0x0000000000010000);
pub const CAP_FPATHCONF: u64 = right(0, 0x0000000000020000);
pub const CAP_FSCK: u64 = right(0, 0x0000000000040000);
pub const CAP_FSTAT: u64 = right(0, 0x0000000000080000);
pub const CAP_FSTATAT: u64 = CAP_FSTAT | CAP_LOOKUP;
pub const CAP_FSTATFS: u64 = right(0, 0x0000000000100000);
pub const CAP_FUTIMES: u64 = right(0, 0x0000000000200000);
pub const CAP_FUTIMESAT: u64 = CAP_FUTIMES | CAP_LOOKUP;
pub const CAP_LINKAT_TARGET: u64 = CAP_LOOKUP | 0x0000000000400000;
pub const CAP_MKDIRAT: u64 = CAP_LOOKUP | 0x0000000000800000;
pub const CAP_MKFIFOAT: u64 = CAP_LOOKUP | 0x0000000001000000;
pub const CAP_MKNODAT: u64 = CAP_LOOKUP | 0x0000000002000000;
pub const CAP_RENAMEAT_SOURCE: u64 = CAP_LOOKUP | 0x0000000004000000;
pub const CAP_SYMLINKAT: u64 = CAP_LOOKUP | 0x0000000008000000;
pub const CAP_UNLINKAT: u64 = CAP_LOOKUP | 0x0000000010000000;
pub const CAP_ACCEPT: u64 = right(0, 0x0000000020000000);
pub const CAP_BIND: u64 = right(0, 0x0000000040000000);
pub const CAP_CONNECT: u64 = right(0, 0x0000000080000000);
pub const CAP_GETPEERNAME: u64 = right(0, 0x0000000100000000);
pub const CAP_GETSOCKNAME: u64 = right(0, 0x0000000200000000);
pub const CAP_GETSOCKOPT: u64 = right(0, 0x0000000400000000);
pub const CAP_LISTEN: u64 = right(0, 0x0000000800000000);
pub const CAP_PEELOFF: u64 = right(0, 0x0000001000000000);
pub const CAP_RECV: u64 = CAP_READ;
pub const CAP_SEND: u64 = CAP_WRITE;
pub const CAP_SETSOCKOPT: u64 = right(0, 0x0000002000000000);
pub const CAP_SHUTDOWN: u64 = right(0, 0x0000004000000000);
pub const CAP_BINDAT: u64 = CAP_LOOKUP | 0x0000008000000000;
pub const CAP_CONNECTAT: u64 = CAP_LOOKUP | 0x0000010000000000;
pub const CAP_LINKAT_SOURCE: u64 = CAP_LOOKUP | 0x0000020000000000;
pub const CAP_RENAMEAT_TARGET: u64 = CAP_LOOKUP | 0x0000040000000000;
pub const CAP_FCHROOT: u64 = right(0, 0x0000080000000000);
pub const CAP_SOCK_CLIENT: u64 = CAP_CONNECT
    | CAP_GETPEERNAME
    | CAP_GETSOCKNAME
    | CAP_GETSOCKOPT
    | CAP_PEELOFF
    | CAP_RECV
    | CAP_SEND
    | CAP_SETSOCKOPT
    | CAP_SHUTDOWN;
pub const CAP_SOCK_SERVER: u64 = CAP_ACCEPT
    | CAP_BIND
    | CAP_GETPEERNAME
    | CAP_GETSOCKNAME
    | CAP_GETSOCKOPT
    | CAP_LISTEN
    | CAP_PEELOFF
    | CAP_RECV
    | CAP_SEND
    | CAP_SETSOCKOPT
    | CAP_SHUTDOWN;
pub const CAP_ALL0: u64 = right(0, 0x00000fffffffffff);
pub const CAP_MAC_GET: u64 = right(1, 0x0000000000000001);
pub const CAP_MAC_SET: u64 = right(1, 0x0000000000000002);
pub const CAP_SEM_GETVALUE: u64 = right(1, 0x0000000000000004);
pub const CAP_SEM_POST: u64 = right(1, 0x0000000000000008);
pub const CAP_SEM_WAIT: u64 = right(1, 0x0000000000000010);
pub const CAP_EVENT: u64 = right(1, 0x0000000000000020);
pub const CAP_KQUEUE_EVENT: u64 = right(1, 0x0000000000000040);
pub const CAP_IOCTL: u64 = right(1, 0x0000000000000080);
pub const CAP_TTYHOOK: u64 = right(1, 0x0000000000000100);
pub const CAP_PDGETPID: u64 = right(1, 0x0000000000000200);
pub const CAP_PDWAIT: u64 = right(1, 0x0000000000000400);
pub const CAP_PDKILL: u64 = right(1, 0x0000000000000800);
pub const CAP_EXTATTR_DELETE: u64 = right(1, 0x0000000000001000);
pub const CAP_EXTATTR_GET: u64 = right(1, 0x0000000000002000);
pub const CAP_EXTATTR_LIST: u64 = right(1, 0x0000000000004000);
pub const CAP_EXTATTR_SET: u64 = right(1, 0x0000000000008000);
pub const CAP_ACL_CHECK: u64 = right(1, 0x0000000000010000);
pub const CAP_ACL_DELETE: u64 = right(1, 0x0000000000020000);
pub const CAP_ACL_GET: u64 = right(1, 0x0000000000040000);
pub const CAP_ACL_SET: u64 = right(1, 0x0000000000080000);
pub const CAP_KQUEUE_CHANGE: u64 = right(1, 0x0000000000100000);
pub const CAP_KQUEUE: u64 = CAP_KQUEUE_EVENT | CAP_KQUEUE_CHANGE;
pub const CAP_INOTIFY_ADD: u64 = right(1, 0x0000000000200000);
pub const CAP_INOTIFY_RM: u64 = right(1, 0x0000000000400000);
pub const CAP_ALL1: u64 = right(1, 0x00000000007fffff);
pub const CAP_POLL_EVENT: u64 = CAP_EVENT;

/// The `fcntl` commands a descriptor may be limited to (`CAP_FCNTL_*`):
/// a bit per command.
pub const CAP_FCNTL_GETFL: u32 = 1 << crate::fcntl::F_GETFL;
pub const CAP_FCNTL_SETFL: u32 = 1 << crate::fcntl::F_SETFL;
pub const CAP_FCNTL_GETOWN: u32 = 1 << crate::fcntl::F_GETOWN;
pub const CAP_FCNTL_SETOWN: u32 = 1 << crate::fcntl::F_SETOWN;
pub const CAP_FCNTL_ALL: u32 =
    CAP_FCNTL_GETFL | CAP_FCNTL_SETFL | CAP_FCNTL_GETOWN | CAP_FCNTL_SETOWN;

/// `cap_ioctls_get`'s answer when every `ioctl` is allowed
/// (`CAP_IOCTLS_ALL`, `SSIZE_MAX`).
pub const CAP_IOCTLS_ALL: i64 = i64::MAX;
/// The most `ioctl`s a descriptor may be limited to (`IOCTLS_MAX_COUNT`,
/// private to sys/kern/sys_capability.c, so not checked against a header).
pub const IOCTLS_MAX_COUNT: usize = 256;

/// `cap_rights_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct CapRights {
    pub cr_rights: [u64; 2],
}

/// Which element right `r` lives in (`right_to_index`), if it names one.
const fn index_of(r: u64) -> Option<usize> {
    match (r >> 57) & 0x1f {
        1 => Some(0),
        2 => Some(1),
        _ => None,
    }
}

impl CapRights {
    /// No rights (`CAP_NONE`).
    pub const NONE: Self = Self {
        cr_rights: [right(0, 0), right(1, 0)],
    };
    /// Every right (`CAP_ALL`).
    pub const ALL: Self = Self {
        cr_rights: [CAP_ALL0, CAP_ALL1],
    };

    /// The rights in `list` (`cap_rights_init`).
    #[must_use]
    pub const fn of(list: &[u64]) -> Self {
        let mut r = Self::NONE;
        let mut i = 0;
        while i < list.len() {
            r = r.with(list[i]);
            i += 1;
        }
        r
    }

    /// These rights and `right` (`cap_rights_set`).
    #[must_use]
    pub const fn with(mut self, right: u64) -> Self {
        if let Some(i) = index_of(right) {
            self.cr_rights[i] |= right;
        }
        self
    }

    /// These rights and `o`'s (`cap_rights_merge`).
    #[must_use]
    pub const fn union(mut self, o: &Self) -> Self {
        self.cr_rights[0] |= o.cr_rights[0];
        self.cr_rights[1] |= o.cr_rights[1];
        self
    }

    /// Whether every right in `little` is here (`cap_rights_contains`).
    #[must_use]
    pub const fn contains(&self, little: &Self) -> bool {
        self.cr_rights[0] & little.cr_rights[0] == little.cr_rights[0]
            && self.cr_rights[1] & little.cr_rights[1] == little.cr_rights[1]
    }

    /// Whether `right` is here (`cap_rights_is_set`).
    #[must_use]
    pub const fn has(&self, right: u64) -> bool {
        match index_of(right) {
            Some(i) => self.cr_rights[i] & right == right,
            None => false,
        }
    }

    /// The structure's version (`CAPVER`).
    #[must_use]
    pub const fn version(&self) -> u32 {
        (self.cr_rights[0] >> 62) as u32
    }

    /// `cap_rights_is_valid`: version 0, no right unknown, each element
    /// in its own place.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        if self.version() != CAP_RIGHTS_VERSION || !Self::ALL.contains(self) {
            return false;
        }
        matches!(index_of(self.cr_rights[0]), Some(0))
            && matches!(index_of(self.cr_rights[1]), Some(1))
            && self.cr_rights[1] >> 62 == 0
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("CAP_READ", CAP_READ),
    c("CAP_WRITE", CAP_WRITE),
    c("CAP_SEEK_TELL", CAP_SEEK_TELL),
    c("CAP_SEEK", CAP_SEEK),
    c("CAP_PREAD", CAP_PREAD),
    c("CAP_PWRITE", CAP_PWRITE),
    c("CAP_MMAP", CAP_MMAP),
    c("CAP_MMAP_R", CAP_MMAP_R),
    c("CAP_MMAP_W", CAP_MMAP_W),
    c("CAP_MMAP_X", CAP_MMAP_X),
    c("CAP_MMAP_RW", CAP_MMAP_RW),
    c("CAP_MMAP_RX", CAP_MMAP_RX),
    c("CAP_MMAP_WX", CAP_MMAP_WX),
    c("CAP_MMAP_RWX", CAP_MMAP_RWX),
    c("CAP_CREATE", CAP_CREATE),
    c("CAP_FEXECVE", CAP_FEXECVE),
    c("CAP_FSYNC", CAP_FSYNC),
    c("CAP_FTRUNCATE", CAP_FTRUNCATE),
    c("CAP_LOOKUP", CAP_LOOKUP),
    c("CAP_FCHDIR", CAP_FCHDIR),
    c("CAP_FCHFLAGS", CAP_FCHFLAGS),
    c("CAP_CHFLAGSAT", CAP_CHFLAGSAT),
    c("CAP_FCHMOD", CAP_FCHMOD),
    c("CAP_FCHMODAT", CAP_FCHMODAT),
    c("CAP_FCHOWN", CAP_FCHOWN),
    c("CAP_FCHOWNAT", CAP_FCHOWNAT),
    c("CAP_FCNTL", CAP_FCNTL),
    c("CAP_FLOCK", CAP_FLOCK),
    c("CAP_FPATHCONF", CAP_FPATHCONF),
    c("CAP_FSCK", CAP_FSCK),
    c("CAP_FSTAT", CAP_FSTAT),
    c("CAP_FSTATAT", CAP_FSTATAT),
    c("CAP_FSTATFS", CAP_FSTATFS),
    c("CAP_FUTIMES", CAP_FUTIMES),
    c("CAP_FUTIMESAT", CAP_FUTIMESAT),
    c("CAP_LINKAT_TARGET", CAP_LINKAT_TARGET),
    c("CAP_MKDIRAT", CAP_MKDIRAT),
    c("CAP_MKFIFOAT", CAP_MKFIFOAT),
    c("CAP_MKNODAT", CAP_MKNODAT),
    c("CAP_RENAMEAT_SOURCE", CAP_RENAMEAT_SOURCE),
    c("CAP_SYMLINKAT", CAP_SYMLINKAT),
    c("CAP_UNLINKAT", CAP_UNLINKAT),
    c("CAP_ACCEPT", CAP_ACCEPT),
    c("CAP_BIND", CAP_BIND),
    c("CAP_CONNECT", CAP_CONNECT),
    c("CAP_GETPEERNAME", CAP_GETPEERNAME),
    c("CAP_GETSOCKNAME", CAP_GETSOCKNAME),
    c("CAP_GETSOCKOPT", CAP_GETSOCKOPT),
    c("CAP_LISTEN", CAP_LISTEN),
    c("CAP_PEELOFF", CAP_PEELOFF),
    c("CAP_RECV", CAP_RECV),
    c("CAP_SEND", CAP_SEND),
    c("CAP_SETSOCKOPT", CAP_SETSOCKOPT),
    c("CAP_SHUTDOWN", CAP_SHUTDOWN),
    c("CAP_BINDAT", CAP_BINDAT),
    c("CAP_CONNECTAT", CAP_CONNECTAT),
    c("CAP_LINKAT_SOURCE", CAP_LINKAT_SOURCE),
    c("CAP_RENAMEAT_TARGET", CAP_RENAMEAT_TARGET),
    c("CAP_FCHROOT", CAP_FCHROOT),
    c("CAP_SOCK_CLIENT", CAP_SOCK_CLIENT),
    c("CAP_SOCK_SERVER", CAP_SOCK_SERVER),
    c("CAP_ALL0", CAP_ALL0),
    c("CAP_MAC_GET", CAP_MAC_GET),
    c("CAP_MAC_SET", CAP_MAC_SET),
    c("CAP_SEM_GETVALUE", CAP_SEM_GETVALUE),
    c("CAP_SEM_POST", CAP_SEM_POST),
    c("CAP_SEM_WAIT", CAP_SEM_WAIT),
    c("CAP_EVENT", CAP_EVENT),
    c("CAP_KQUEUE_EVENT", CAP_KQUEUE_EVENT),
    c("CAP_IOCTL", CAP_IOCTL),
    c("CAP_TTYHOOK", CAP_TTYHOOK),
    c("CAP_PDGETPID", CAP_PDGETPID),
    c("CAP_PDWAIT", CAP_PDWAIT),
    c("CAP_PDKILL", CAP_PDKILL),
    c("CAP_EXTATTR_DELETE", CAP_EXTATTR_DELETE),
    c("CAP_EXTATTR_GET", CAP_EXTATTR_GET),
    c("CAP_EXTATTR_LIST", CAP_EXTATTR_LIST),
    c("CAP_EXTATTR_SET", CAP_EXTATTR_SET),
    c("CAP_ACL_CHECK", CAP_ACL_CHECK),
    c("CAP_ACL_DELETE", CAP_ACL_DELETE),
    c("CAP_ACL_GET", CAP_ACL_GET),
    c("CAP_ACL_SET", CAP_ACL_SET),
    c("CAP_KQUEUE_CHANGE", CAP_KQUEUE_CHANGE),
    c("CAP_KQUEUE", CAP_KQUEUE),
    c("CAP_INOTIFY_ADD", CAP_INOTIFY_ADD),
    c("CAP_INOTIFY_RM", CAP_INOTIFY_RM),
    c("CAP_ALL1", CAP_ALL1),
    c("CAP_POLL_EVENT", CAP_POLL_EVENT),
    c("CAP_FCNTL_ALL", CAP_FCNTL_ALL as u64),
    c("CAP_RIGHTS_VERSION", CAP_RIGHTS_VERSION as u64),
];

const fn c(c_name: &'static str, value: u64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/capsicum.h",
        value: value.cast_signed(),
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_combine_as_freebsds() {
        let r = CapRights::of(&[CAP_READ, CAP_SEEK, CAP_IOCTL]);
        assert!(r.is_valid());
        assert!(r.has(CAP_READ) && r.has(CAP_SEEK_TELL) && r.has(CAP_IOCTL));
        assert!(!r.has(CAP_WRITE));
        assert!(CapRights::ALL.contains(&r) && !r.contains(&CapRights::ALL));
        assert!(r.contains(&CapRights::of(&[CAP_PREAD])));
        assert_eq!(CapRights::NONE.cr_rights, [1 << 57, 1 << 58]);
        let mut bad = r;
        bad.cr_rights[0] |= 1 << 62;
        assert!(!bad.is_valid());
        bad = r;
        bad.cr_rights[1] = 1 << 57;
        assert!(!bad.is_valid());
    }
}
