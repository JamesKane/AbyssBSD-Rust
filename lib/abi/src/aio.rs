// SPDX-License-Identifier: BSD-2-Clause
//! POSIX asynchronous I/O: `struct aiocb` and the `LIO_*` and `AIO_*`
//! values.
//!
//! Behavior reference: sys/sys/aio.h.

use crate::event::Sigevent;
use crate::layout::ConstValue;

/// `aio_cancel`'s results.
pub const AIO_CANCELED: i32 = 0x1;
pub const AIO_NOTCANCELED: i32 = 0x2;
pub const AIO_ALLDONE: i32 = 0x3;

/// `aio_lio_opcode`s.
pub const LIO_NOP: i32 = 0x0;
pub const LIO_WRITE: i32 = 0x1;
pub const LIO_READ: i32 = 0x2;
pub const LIO_VECTORED: i32 = 0x4;
pub const LIO_WRITEV: i32 = LIO_WRITE | LIO_VECTORED;
pub const LIO_READV: i32 = LIO_READ | LIO_VECTORED;
/// The kernel's own: `aio_fsync`'s and `aio_mlock`'s (`_WANT_ALL_LIO_OPCODES`).
pub const LIO_SYNC: i32 = 0x8;
pub const LIO_DSYNC: i32 = 0x10 | LIO_SYNC;
pub const LIO_MLOCK: i32 = 0x20;
/// Use the file's offset, not `aio_offset` (`_WANT_ALL_LIO_OPCODES`).
pub const LIO_FOFFSET: i32 = 0x40;

/// `lio_listio` modes.
pub const LIO_NOWAIT: i32 = 0x0;
pub const LIO_WAIT: i32 = 0x1;

/// `AIO_LISTIO_MAX`: the least a system may allow.
pub const AIO_LISTIO_MAX: i32 = 16;

/// `struct aiocb`. `aio_buf` is also `aio_iov`, and `aio_nbytes`
/// `aio_iovcnt`, for the vectored calls.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Aiocb {
    pub aio_fildes: i32,
    pub _pad0: i32,
    pub aio_offset: i64,
    pub aio_buf: u64,
    pub aio_nbytes: u64,
    pub __spare__: [i32; 2],
    pub __spare2__: u64,
    pub aio_lio_opcode: i32,
    pub aio_reqprio: i32,
    /// `_aiocb_private.status`.
    pub status: i64,
    /// `_aiocb_private.error`.
    pub error: i64,
    /// `_aiocb_private.spare`.
    pub kernelinfo: u64,
    pub aio_sigevent: Sigevent,
}

const _: () = assert!(size_of::<Aiocb>() == 160);

const fn c(c_name: &'static str, value: i32) -> ConstValue {
    ConstValue {
        c_name,
        header: "aio.h",
        value: value as i64,
        arches: crate::layout::Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("AIO_CANCELED", AIO_CANCELED),
    c("AIO_NOTCANCELED", AIO_NOTCANCELED),
    c("AIO_ALLDONE", AIO_ALLDONE),
    c("LIO_NOP", LIO_NOP),
    c("LIO_WRITE", LIO_WRITE),
    c("LIO_READ", LIO_READ),
    c("LIO_VECTORED", LIO_VECTORED),
    c("LIO_WRITEV", LIO_WRITEV),
    c("LIO_READV", LIO_READV),
    c("LIO_NOWAIT", LIO_NOWAIT),
    c("LIO_WAIT", LIO_WAIT),
    c("AIO_LISTIO_MAX", AIO_LISTIO_MAX),
];
