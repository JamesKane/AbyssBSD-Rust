// SPDX-License-Identifier: BSD-2-Clause
//! Opening files and descriptor control (`sys/fcntl.h`, `unistd.h`): the
//! `open` flags, `fcntl` commands, descriptor flags, the `*at` flags, and
//! `lseek`'s whence values, with the path limits of `limits.h` and `sys/param.h`.

use crate::layout::{Arches, ConstValue};

pub const O_RDONLY: u32 = 0x0000;
pub const O_WRONLY: u32 = 0x0001;
pub const O_RDWR: u32 = 0x0002;
pub const O_ACCMODE: u32 = 0x0003;
pub const O_NONBLOCK: u32 = 0x0004;
pub const O_APPEND: u32 = 0x0008;
pub const O_SHLOCK: u32 = 0x0010;
pub const O_EXLOCK: u32 = 0x0020;
pub const O_ASYNC: u32 = 0x0040;
pub const O_FSYNC: u32 = 0x0080;
pub const O_NOFOLLOW: u32 = 0x0100;
pub const O_CREAT: u32 = 0x0200;
pub const O_TRUNC: u32 = 0x0400;
pub const O_EXCL: u32 = 0x0800;
pub const O_NOCTTY: u32 = 0x8000;
pub const O_DIRECT: u32 = 0x0001_0000;
pub const O_DIRECTORY: u32 = 0x0002_0000;
pub const O_EXEC: u32 = 0x0004_0000;
pub const O_TTY_INIT: u32 = 0x0008_0000;
pub const O_CLOEXEC: u32 = 0x0010_0000;
pub const O_VERIFY: u32 = 0x0020_0000;
pub const O_PATH: u32 = 0x0040_0000;
pub const O_RESOLVE_BENEATH: u32 = 0x0080_0000;
pub const O_DSYNC: u32 = 0x0100_0000;
pub const O_EMPTY_PATH: u32 = 0x0200_0000;
pub const O_NAMEDATTR: u32 = 0x0400_0000;
pub const O_CLOFORK: u32 = 0x0800_0000;

/// The kernel's file flags: `FFLAGS(O_RDONLY)` is `FREAD`, and so on
/// (`O_EXEC` stays as it is).
pub const FREAD: u32 = 0x0001;
pub const FWRITE: u32 = 0x0002;
/// The status flags `F_SETFL` may change (`FCNTLFLAGS`).
pub const FCNTLFLAGS: u32 = O_APPEND | O_ASYNC | O_FSYNC | O_DSYNC | O_NONBLOCK | O_DIRECT;

pub const F_DUPFD: u32 = 0;
pub const F_GETFD: u32 = 1;
pub const F_SETFD: u32 = 2;
pub const F_GETFL: u32 = 3;
pub const F_SETFL: u32 = 4;
pub const F_GETOWN: u32 = 5;
pub const F_SETOWN: u32 = 6;
pub const F_DUP2FD: u32 = 10;
pub const F_GETLK: u32 = 11;
pub const F_SETLK: u32 = 12;
pub const F_SETLKW: u32 = 13;
/// `struct flock`: an advisory lock's range and type (`F_GETLK`,
/// `F_SETLK`, `F_SETLKW`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flock {
    pub l_start: i64,
    /// 0: to the end of the file, however far it grows.
    pub l_len: i64,
    pub l_pid: i32,
    pub l_type: i16,
    pub l_whence: i16,
    pub l_sysid: i32,
    /// The C struct's tail padding, explicit so the whole struct can be
    /// copied without leaking bytes.
    pub _pad: [u8; 4],
}

/// `l_type` values.
pub const F_RDLCK: i16 = 1;
pub const F_UNLCK: i16 = 2;
pub const F_WRLCK: i16 = 3;

/// `pathconf` names (`unistd.h`).
pub const _PC_LINK_MAX: u32 = 1;
pub const _PC_MAX_CANON: u32 = 2;
pub const _PC_MAX_INPUT: u32 = 3;
pub const _PC_NAME_MAX: u32 = 4;
pub const _PC_PATH_MAX: u32 = 5;
pub const _PC_PIPE_BUF: u32 = 6;
pub const _PC_CHOWN_RESTRICTED: u32 = 7;
pub const _PC_NO_TRUNC: u32 = 8;
pub const _PC_VDISABLE: u32 = 9;
pub const _PC_ALLOC_SIZE_MIN: u32 = 10;
pub const _PC_FILESIZEBITS: u32 = 12;
pub const _PC_SYMLINK_MAX: u32 = 18;
pub const _PC_MIN_HOLE_SIZE: u32 = 21;
pub const _PC_ASYNC_IO: u32 = 53;
pub const _PC_SYNC_IO: u32 = 55;
pub const _PC_PRIO_IO: u32 = 54;
pub const _PC_REC_INCR_XFER_SIZE: u32 = 14;
pub const _PC_REC_MAX_XFER_SIZE: u32 = 15;
pub const _PC_REC_MIN_XFER_SIZE: u32 = 16;
pub const _PC_REC_XFER_ALIGN: u32 = 17;
pub const _PC_ACL_PATH_MAX: u32 = 60;
pub const _PC_ACL_EXTENDED: u32 = 59;
pub const _PC_CAP_PRESENT: u32 = 61;
pub const _PC_INF_PRESENT: u32 = 62;
pub const _PC_MAC_PRESENT: u32 = 63;
pub const _PC_ACL_NFS4: u32 = 64;
pub const _PC_DEALLOC_PRESENT: u32 = 65;
pub const _PC_NAMEDATTR_ENABLED: u32 = 66;
pub const _PC_HAS_NAMEDATTR: u32 = 67;
pub const _PC_HAS_HIDDENSYSTEM: u32 = 68;
pub const _PC_CLONE_BLKSIZE: u32 = 69;
pub const _PC_CASE_INSENSITIVE: u32 = 70;
/// `_POSIX_ASYNCHRONOUS_IO`.
pub const POSIX_ASYNCHRONOUS_IO: i64 = 200_112;

/// `flock` operations.
pub const LOCK_SH: u32 = 0x01;
pub const LOCK_EX: u32 = 0x02;
pub const LOCK_NB: u32 = 0x04;
pub const LOCK_UN: u32 = 0x08;

pub const F_READAHEAD: u32 = 15;
pub const F_RDAHEAD: u32 = 16;
pub const F_DUPFD_CLOEXEC: u32 = 17;
pub const F_DUP2FD_CLOEXEC: u32 = 18;
pub const F_ADD_SEALS: u32 = 19;
pub const F_GET_SEALS: u32 = 20;
pub const F_ISUNIONSTACK: u32 = 21;
pub const F_KINFO: u32 = 22;
pub const F_DUPFD_CLOFORK: u32 = 23;
pub const F_DUP3FD: u32 = 24;
pub const F_DUP3FD_SHIFT: u32 = 16;

pub const FD_CLOEXEC: u32 = 1;
pub const FD_RESOLVE_BENEATH: u32 = 2;
pub const FD_CLOFORK: u32 = 4;

pub const AT_FDCWD: i32 = -100;
pub const AT_EACCESS: u32 = 0x0100;
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x0200;
pub const AT_SYMLINK_FOLLOW: u32 = 0x0400;
pub const AT_REMOVEDIR: u32 = 0x0800;
pub const AT_RESOLVE_BENEATH: u32 = 0x2000;
pub const AT_EMPTY_PATH: u32 = 0x4000;

pub const SEEK_SET: u32 = 0;
pub const SEEK_CUR: u32 = 1;
pub const SEEK_END: u32 = 2;
pub const SEEK_DATA: u32 = 3;
pub const SEEK_HOLE: u32 = 4;

/// `access` modes.
pub const F_OK: u32 = 0;
pub const X_OK: u32 = 0x01;
pub const W_OK: u32 = 0x02;
pub const R_OK: u32 = 0x04;

/// `copy_file_range`'s one flag: clone blocks or fail.
pub const COPY_FILE_RANGE_CLONE: u32 = 0x0080_0000;

/// `close_range` flags.
pub const CLOSE_RANGE_CLOEXEC: u32 = 1 << 2;
pub const CLOSE_RANGE_CLOFORK: u32 = 1 << 3;

/// The longest path, NUL included (`PATH_MAX`, `MAXPATHLEN`).
pub const PATH_MAX: usize = 1024;
/// The longest path component (`NAME_MAX`).
pub const NAME_MAX: usize = 255;
/// The most symbolic links one lookup follows (`MAXSYMLINKS`).
pub const MAXSYMLINKS: usize = 32;
/// Writes to a pipe up to this size are atomic (`PIPE_BUF`).
pub const PIPE_BUF: usize = 512;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("O_RDONLY", O_RDONLY),
    c("O_WRONLY", O_WRONLY),
    c("O_RDWR", O_RDWR),
    c("O_ACCMODE", O_ACCMODE),
    c("O_NONBLOCK", O_NONBLOCK),
    c("O_APPEND", O_APPEND),
    c("O_SHLOCK", O_SHLOCK),
    c("O_EXLOCK", O_EXLOCK),
    c("O_ASYNC", O_ASYNC),
    c("O_FSYNC", O_FSYNC),
    c("O_NOFOLLOW", O_NOFOLLOW),
    c("O_CREAT", O_CREAT),
    c("O_TRUNC", O_TRUNC),
    c("O_EXCL", O_EXCL),
    c("O_NOCTTY", O_NOCTTY),
    c("O_DIRECT", O_DIRECT),
    c("O_DIRECTORY", O_DIRECTORY),
    c("O_EXEC", O_EXEC),
    c("O_TTY_INIT", O_TTY_INIT),
    c("O_CLOEXEC", O_CLOEXEC),
    c("O_VERIFY", O_VERIFY),
    c("O_PATH", O_PATH),
    c("O_RESOLVE_BENEATH", O_RESOLVE_BENEATH),
    c("O_DSYNC", O_DSYNC),
    c("O_EMPTY_PATH", O_EMPTY_PATH),
    c("O_NAMEDATTR", O_NAMEDATTR),
    c("O_CLOFORK", O_CLOFORK),
    c("F_DUPFD", F_DUPFD),
    c("F_GETFD", F_GETFD),
    c("F_SETFD", F_SETFD),
    c("F_GETFL", F_GETFL),
    c("F_SETFL", F_SETFL),
    c("F_GETOWN", F_GETOWN),
    c("F_SETOWN", F_SETOWN),
    c("F_DUP2FD", F_DUP2FD),
    c("F_GETLK", F_GETLK),
    c("F_SETLK", F_SETLK),
    c("F_SETLKW", F_SETLKW),
    c("F_RDLCK", F_RDLCK as u32),
    c("F_UNLCK", F_UNLCK as u32),
    c("F_WRLCK", F_WRLCK as u32),
    c("LOCK_SH", LOCK_SH),
    c("LOCK_EX", LOCK_EX),
    c("LOCK_NB", LOCK_NB),
    c("LOCK_UN", LOCK_UN),
    u("_PC_LINK_MAX", _PC_LINK_MAX),
    u("_PC_MAX_CANON", _PC_MAX_CANON),
    u("_PC_MAX_INPUT", _PC_MAX_INPUT),
    u("_PC_NAME_MAX", _PC_NAME_MAX),
    u("_PC_PATH_MAX", _PC_PATH_MAX),
    u("_PC_PIPE_BUF", _PC_PIPE_BUF),
    u("_PC_CHOWN_RESTRICTED", _PC_CHOWN_RESTRICTED),
    u("_PC_NO_TRUNC", _PC_NO_TRUNC),
    u("_PC_VDISABLE", _PC_VDISABLE),
    u("_PC_ALLOC_SIZE_MIN", _PC_ALLOC_SIZE_MIN),
    u("_PC_FILESIZEBITS", _PC_FILESIZEBITS),
    u("_PC_SYMLINK_MAX", _PC_SYMLINK_MAX),
    u("_PC_MIN_HOLE_SIZE", _PC_MIN_HOLE_SIZE),
    u("_PC_ASYNC_IO", _PC_ASYNC_IO),
    u("_PC_SYNC_IO", _PC_SYNC_IO),
    u("_PC_PRIO_IO", _PC_PRIO_IO),
    u("_PC_REC_INCR_XFER_SIZE", _PC_REC_INCR_XFER_SIZE),
    u("_PC_REC_MAX_XFER_SIZE", _PC_REC_MAX_XFER_SIZE),
    u("_PC_REC_MIN_XFER_SIZE", _PC_REC_MIN_XFER_SIZE),
    u("_PC_REC_XFER_ALIGN", _PC_REC_XFER_ALIGN),
    u("_PC_ACL_PATH_MAX", _PC_ACL_PATH_MAX),
    u("_PC_ACL_EXTENDED", _PC_ACL_EXTENDED),
    u("_PC_CAP_PRESENT", _PC_CAP_PRESENT),
    u("_PC_INF_PRESENT", _PC_INF_PRESENT),
    u("_PC_MAC_PRESENT", _PC_MAC_PRESENT),
    u("_PC_ACL_NFS4", _PC_ACL_NFS4),
    u("_PC_DEALLOC_PRESENT", _PC_DEALLOC_PRESENT),
    u("_PC_NAMEDATTR_ENABLED", _PC_NAMEDATTR_ENABLED),
    u("_PC_HAS_NAMEDATTR", _PC_HAS_NAMEDATTR),
    u("_PC_HAS_HIDDENSYSTEM", _PC_HAS_HIDDENSYSTEM),
    u("_PC_CLONE_BLKSIZE", _PC_CLONE_BLKSIZE),
    u("_PC_CASE_INSENSITIVE", _PC_CASE_INSENSITIVE),
    c("F_READAHEAD", F_READAHEAD),
    c("F_RDAHEAD", F_RDAHEAD),
    c("F_DUPFD_CLOEXEC", F_DUPFD_CLOEXEC),
    c("F_DUP2FD_CLOEXEC", F_DUP2FD_CLOEXEC),
    c("F_ADD_SEALS", F_ADD_SEALS),
    c("F_GET_SEALS", F_GET_SEALS),
    c("F_ISUNIONSTACK", F_ISUNIONSTACK),
    c("F_KINFO", F_KINFO),
    c("F_DUPFD_CLOFORK", F_DUPFD_CLOFORK),
    c("F_DUP3FD", F_DUP3FD),
    c("F_DUP3FD_SHIFT", F_DUP3FD_SHIFT),
    c("FD_CLOEXEC", FD_CLOEXEC),
    c("FD_RESOLVE_BENEATH", FD_RESOLVE_BENEATH),
    c("FD_CLOFORK", FD_CLOFORK),
    ConstValue {
        c_name: "AT_FDCWD",
        header: "fcntl.h",
        value: AT_FDCWD as i64,
        arches: Arches::All,
    },
    c("AT_EACCESS", AT_EACCESS),
    c("AT_SYMLINK_NOFOLLOW", AT_SYMLINK_NOFOLLOW),
    c("AT_SYMLINK_FOLLOW", AT_SYMLINK_FOLLOW),
    c("AT_REMOVEDIR", AT_REMOVEDIR),
    c("AT_RESOLVE_BENEATH", AT_RESOLVE_BENEATH),
    c("AT_EMPTY_PATH", AT_EMPTY_PATH),
    u("SEEK_SET", SEEK_SET),
    u("SEEK_CUR", SEEK_CUR),
    u("SEEK_END", SEEK_END),
    u("SEEK_DATA", SEEK_DATA),
    u("SEEK_HOLE", SEEK_HOLE),
    u("F_OK", F_OK),
    u("X_OK", X_OK),
    u("W_OK", W_OK),
    u("R_OK", R_OK),
    u("COPY_FILE_RANGE_CLONE", COPY_FILE_RANGE_CLONE),
    u("CLOSE_RANGE_CLOEXEC", CLOSE_RANGE_CLOEXEC),
    u("CLOSE_RANGE_CLOFORK", CLOSE_RANGE_CLOFORK),
    l("PATH_MAX", "limits.h", PATH_MAX),
    l("NAME_MAX", "limits.h", NAME_MAX),
    l("PIPE_BUF", "limits.h", PIPE_BUF),
    l("MAXSYMLINKS", "sys/param.h", MAXSYMLINKS),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "fcntl.h",
        value: value as i64,
        arches: Arches::All,
    }
}

const fn u(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "unistd.h",
        value: value as i64,
        arches: Arches::All,
    }
}

const fn l(c_name: &'static str, header: &'static str, value: usize) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value: value as i64,
        arches: Arches::All,
    }
}
