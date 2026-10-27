// SPDX-License-Identifier: BSD-2-Clause
//! `_umtx_op`: its operations and flags (`sys/umtx.h`) and the user
//! objects they work on (`sys/_umtx.h`).

use crate::layout::{Arches, ConstValue};

/// `USYNC_PROCESS_SHARED`: the object may be shared between processes.
pub const USYNC_PROCESS_SHARED: u32 = 0x0001;

/// `umutex.m_flags`.
pub const UMUTEX_PRIO_INHERIT: u32 = 0x0004;
pub const UMUTEX_PRIO_PROTECT: u32 = 0x0008;
pub const UMUTEX_ROBUST: u32 = 0x0010;
pub const UMUTEX_NONCONSISTENT: u32 = 0x0020;

/// `umutex.m_owner`: the owner's tid, the contested bit, and two markers
/// below the lowest tid.
pub const UMUTEX_UNOWNED: u32 = 0x0;
pub const UMUTEX_CONTESTED: u32 = 0x8000_0000;
pub const UMUTEX_RB_OWNERDEAD: u32 = UMUTEX_CONTESTED | 0x10;
pub const UMUTEX_RB_NOTRECOV: u32 = UMUTEX_CONTESTED | 0x11;

/// `struct umtx.u_owner` (the COMPAT_FREEBSD10 lock): unowned, and the
/// contested bit (`LONG_MIN`).
pub const UMTX_UNOWNED: u64 = 0;
pub const UMTX_CONTESTED: u64 = 1 << 63;

/// `urwlock.rw_flags`.
pub const URWLOCK_PREFER_READER: u32 = 0x0002;

/// `urwlock.rw_state`.
pub const URWLOCK_WRITE_OWNER: u32 = 0x8000_0000;
pub const URWLOCK_WRITE_WAITERS: u32 = 0x4000_0000;
pub const URWLOCK_READ_WAITERS: u32 = 0x2000_0000;
pub const URWLOCK_MAX_READERS: u32 = 0x1fff_ffff;

/// `_usem2._count`: the waiters bit, and the count under it.
pub const USEM_HAS_WAITERS: u32 = 0x8000_0000;
pub const USEM_MAX_COUNT: u32 = 0x7fff_ffff;

pub const UMTX_OP_LOCK: u32 = 0;
pub const UMTX_OP_UNLOCK: u32 = 1;
pub const UMTX_OP_WAIT: u32 = 2;
pub const UMTX_OP_WAKE: u32 = 3;
pub const UMTX_OP_MUTEX_TRYLOCK: u32 = 4;
pub const UMTX_OP_MUTEX_LOCK: u32 = 5;
pub const UMTX_OP_MUTEX_UNLOCK: u32 = 6;
pub const UMTX_OP_SET_CEILING: u32 = 7;
pub const UMTX_OP_CV_WAIT: u32 = 8;
pub const UMTX_OP_CV_SIGNAL: u32 = 9;
pub const UMTX_OP_CV_BROADCAST: u32 = 10;
pub const UMTX_OP_WAIT_UINT: u32 = 11;
pub const UMTX_OP_RW_RDLOCK: u32 = 12;
pub const UMTX_OP_RW_WRLOCK: u32 = 13;
pub const UMTX_OP_RW_UNLOCK: u32 = 14;
pub const UMTX_OP_WAIT_UINT_PRIVATE: u32 = 15;
pub const UMTX_OP_WAKE_PRIVATE: u32 = 16;
pub const UMTX_OP_MUTEX_WAIT: u32 = 17;
pub const UMTX_OP_MUTEX_WAKE: u32 = 18;
pub const UMTX_OP_SEM_WAIT: u32 = 19;
pub const UMTX_OP_SEM_WAKE: u32 = 20;
/// Wakes the waiters on each of an array of addresses (libthr's
/// deferred condition-variable wakeups).
pub const UMTX_OP_NWAKE_PRIVATE: u32 = 21;
pub const UMTX_OP_MUTEX_WAKE2: u32 = 22;
pub const UMTX_OP_SEM2_WAIT: u32 = 23;
pub const UMTX_OP_SEM2_WAKE: u32 = 24;
pub const UMTX_OP_SHM: u32 = 25;
pub const UMTX_OP_ROBUST_LISTS: u32 = 26;
pub const UMTX_OP_GET_MIN_TIMEOUT: u32 = 27;
pub const UMTX_OP_SET_MIN_TIMEOUT: u32 = 28;

/// Flags on the operation: the caller's objects and times are i386's, or
/// another 32-bit ABI's.
pub const UMTX_OP__I386: u32 = 0x4000_0000;
pub const UMTX_OP__32BIT: u32 = 0x8000_0000;

/// `UMTX_OP_CV_WAIT`'s flags.
pub const CVWAIT_CHECK_UNPARKING: u32 = 0x01;
pub const CVWAIT_ABSTIME: u32 = 0x02;
pub const CVWAIT_CLOCKID: u32 = 0x04;
/// FreeBSD 16's, absent from 15's headers (so not checked against them).
pub const CVWAIT_UMTX_TIME: u32 = 0x08;

/// `_umtx_time._flags`: the timeout is a time on the clock, not a
/// duration.
pub const UMTX_ABSTIME: u32 = 1;

/// `UMTX_OP_SHM`'s flags.
pub const UMTX_SHM_CREAT: u32 = 0x0001;
pub const UMTX_SHM_LOOKUP: u32 = 0x0002;
pub const UMTX_SHM_DESTROY: u32 = 0x0004;
pub const UMTX_SHM_ALIVE: u32 = 0x0008;

/// `struct umutex`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Umutex {
    pub m_owner: u32,
    pub m_flags: u32,
    pub m_ceilings: [u32; 2],
    pub m_rb_lnk: u64,
    pub m_spare: [u32; 2],
}

/// `struct ucond`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ucond {
    pub c_has_waiters: u32,
    pub c_flags: u32,
    pub c_clockid: u32,
    pub c_spare: [u32; 1],
}

/// `struct urwlock`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Urwlock {
    pub rw_state: i32,
    pub rw_flags: u32,
    pub rw_blocked_readers: u32,
    pub rw_blocked_writers: u32,
    pub rw_spare: [u32; 4],
}

/// `struct _usem` (the COMPAT_FREEBSD10 semaphore).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usem {
    pub has_waiters: u32,
    pub count: u32,
    pub flags: u32,
}

/// `struct _usem2`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usem2 {
    pub count: u32,
    pub flags: u32,
}

/// `struct _umtx_time`: a wait's timeout, when the size `_umtx_op` is
/// given exceeds a `struct timespec`'s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UmtxTime {
    pub timeout: crate::time::Timespec,
    pub flags: u32,
    pub clockid: u32,
}

/// `struct umtx_robust_lists_params`: where a thread's robust-mutex
/// lists start.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RobustListsParams {
    pub robust_list_offset: u64,
    pub robust_priv_list_offset: u64,
    pub robust_inact_offset: u64,
}

/// Offsets into the objects, which the kernel reads and writes word by
/// word.
pub mod offset {
    pub const M_OWNER: u64 = 0;
    pub const M_FLAGS: u64 = 4;
    pub const M_CEILINGS: u64 = 8;
    pub const M_RB_LNK: u64 = 16;
    pub const C_HAS_WAITERS: u64 = 0;
    pub const C_FLAGS: u64 = 4;
    pub const C_CLOCKID: u64 = 8;
    pub const RW_STATE: u64 = 0;
    pub const RW_FLAGS: u64 = 4;
    pub const RW_BLOCKED_READERS: u64 = 8;
    pub const RW_BLOCKED_WRITERS: u64 = 12;
    pub const SEM_HAS_WAITERS: u64 = 0;
    pub const SEM_COUNT: u64 = 4;
    pub const SEM_FLAGS: u64 = 8;
    pub const SEM2_COUNT: u64 = 0;
    pub const SEM2_FLAGS: u64 = 4;
}

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/umtx.h",
        value: value as i64,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("USYNC_PROCESS_SHARED", USYNC_PROCESS_SHARED),
    c("UMUTEX_PRIO_INHERIT", UMUTEX_PRIO_INHERIT),
    c("UMUTEX_PRIO_PROTECT", UMUTEX_PRIO_PROTECT),
    c("UMUTEX_ROBUST", UMUTEX_ROBUST),
    c("UMUTEX_NONCONSISTENT", UMUTEX_NONCONSISTENT),
    c("UMUTEX_UNOWNED", UMUTEX_UNOWNED),
    c("UMUTEX_CONTESTED", UMUTEX_CONTESTED),
    c("UMUTEX_RB_OWNERDEAD", UMUTEX_RB_OWNERDEAD),
    c("UMUTEX_RB_NOTRECOV", UMUTEX_RB_NOTRECOV),
    c("URWLOCK_PREFER_READER", URWLOCK_PREFER_READER),
    c("URWLOCK_WRITE_OWNER", URWLOCK_WRITE_OWNER),
    c("URWLOCK_WRITE_WAITERS", URWLOCK_WRITE_WAITERS),
    c("URWLOCK_READ_WAITERS", URWLOCK_READ_WAITERS),
    c("URWLOCK_MAX_READERS", URWLOCK_MAX_READERS),
    c("USEM_HAS_WAITERS", USEM_HAS_WAITERS),
    c("USEM_MAX_COUNT", USEM_MAX_COUNT),
    c("UMTX_OP_LOCK", UMTX_OP_LOCK),
    c("UMTX_OP_UNLOCK", UMTX_OP_UNLOCK),
    c("UMTX_OP_WAIT", UMTX_OP_WAIT),
    c("UMTX_OP_WAKE", UMTX_OP_WAKE),
    c("UMTX_OP_MUTEX_TRYLOCK", UMTX_OP_MUTEX_TRYLOCK),
    c("UMTX_OP_MUTEX_LOCK", UMTX_OP_MUTEX_LOCK),
    c("UMTX_OP_MUTEX_UNLOCK", UMTX_OP_MUTEX_UNLOCK),
    c("UMTX_OP_SET_CEILING", UMTX_OP_SET_CEILING),
    c("UMTX_OP_CV_WAIT", UMTX_OP_CV_WAIT),
    c("UMTX_OP_CV_SIGNAL", UMTX_OP_CV_SIGNAL),
    c("UMTX_OP_CV_BROADCAST", UMTX_OP_CV_BROADCAST),
    c("UMTX_OP_WAIT_UINT", UMTX_OP_WAIT_UINT),
    c("UMTX_OP_RW_RDLOCK", UMTX_OP_RW_RDLOCK),
    c("UMTX_OP_RW_WRLOCK", UMTX_OP_RW_WRLOCK),
    c("UMTX_OP_RW_UNLOCK", UMTX_OP_RW_UNLOCK),
    c("UMTX_OP_WAIT_UINT_PRIVATE", UMTX_OP_WAIT_UINT_PRIVATE),
    c("UMTX_OP_WAKE_PRIVATE", UMTX_OP_WAKE_PRIVATE),
    c("UMTX_OP_MUTEX_WAIT", UMTX_OP_MUTEX_WAIT),
    c("UMTX_OP_MUTEX_WAKE", UMTX_OP_MUTEX_WAKE),
    c("UMTX_OP_SEM_WAIT", UMTX_OP_SEM_WAIT),
    c("UMTX_OP_SEM_WAKE", UMTX_OP_SEM_WAKE),
    c("UMTX_OP_NWAKE_PRIVATE", UMTX_OP_NWAKE_PRIVATE),
    c("UMTX_OP_MUTEX_WAKE2", UMTX_OP_MUTEX_WAKE2),
    c("UMTX_OP_SEM2_WAIT", UMTX_OP_SEM2_WAIT),
    c("UMTX_OP_SEM2_WAKE", UMTX_OP_SEM2_WAKE),
    c("UMTX_OP_SHM", UMTX_OP_SHM),
    c("UMTX_OP_ROBUST_LISTS", UMTX_OP_ROBUST_LISTS),
    c("UMTX_OP_GET_MIN_TIMEOUT", UMTX_OP_GET_MIN_TIMEOUT),
    c("UMTX_OP_SET_MIN_TIMEOUT", UMTX_OP_SET_MIN_TIMEOUT),
    c("UMTX_OP__I386", UMTX_OP__I386),
    c("UMTX_OP__32BIT", UMTX_OP__32BIT),
    c("CVWAIT_CHECK_UNPARKING", CVWAIT_CHECK_UNPARKING),
    c("CVWAIT_ABSTIME", CVWAIT_ABSTIME),
    c("CVWAIT_CLOCKID", CVWAIT_CLOCKID),
    c("UMTX_ABSTIME", UMTX_ABSTIME),
    c("UMTX_SHM_CREAT", UMTX_SHM_CREAT),
    c("UMTX_SHM_LOOKUP", UMTX_SHM_LOOKUP),
    c("UMTX_SHM_DESTROY", UMTX_SHM_DESTROY),
    c("UMTX_SHM_ALIVE", UMTX_SHM_ALIVE),
];
