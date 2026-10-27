// SPDX-License-Identifier: BSD-2-Clause
//! Events: kqueue (`sys/event.h`), `poll` (`sys/poll.h`), interval and
//! POSIX timers (`sys/time.h`, `sys/timespec.h`, `sys/signal.h`),
//! `timerfd` (`sys/timerfd.h`), and `eventfd` (`sys/eventfd.h`,
//! `sys/specialfd.h`).

use crate::layout::{Arches, ConstValue};
use crate::time::{Timespec, Timeval};

/// `struct kevent` (FreeBSD 12's, with `ext`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Kevent {
    pub ident: u64,
    pub filter: i16,
    pub flags: u16,
    pub fflags: u32,
    pub data: i64,
    pub udata: u64,
    pub ext: [u64; 4],
}

const _: () = assert!(size_of::<Kevent>() == 64);

pub const EVFILT_READ: i16 = -1;
pub const EVFILT_WRITE: i16 = -2;
pub const EVFILT_AIO: i16 = -3;
pub const EVFILT_VNODE: i16 = -4;
pub const EVFILT_PROC: i16 = -5;
pub const EVFILT_SIGNAL: i16 = -6;
pub const EVFILT_TIMER: i16 = -7;
pub const EVFILT_PROCDESC: i16 = -8;
pub const EVFILT_FS: i16 = -9;
pub const EVFILT_LIO: i16 = -10;
pub const EVFILT_USER: i16 = -11;
pub const EVFILT_SENDFILE: i16 = -12;
pub const EVFILT_EMPTY: i16 = -13;
pub const EVFILT_JAIL: i16 = -14;
pub const EVFILT_JAILDESC: i16 = -15;
pub const EVFILT_SYSCOUNT: i16 = 15;

pub const EV_ADD: u16 = 0x0001;
pub const EV_DELETE: u16 = 0x0002;
pub const EV_ENABLE: u16 = 0x0004;
pub const EV_DISABLE: u16 = 0x0008;
pub const EV_FORCEONESHOT: u16 = 0x0100;
pub const EV_KEEPUDATA: u16 = 0x0200;
pub const EV_ONESHOT: u16 = 0x0010;
pub const EV_CLEAR: u16 = 0x0020;
pub const EV_RECEIPT: u16 = 0x0040;
pub const EV_DISPATCH: u16 = 0x0080;
pub const EV_SYSFLAGS: u16 = 0xf000;
pub const EV_DROP: u16 = 0x1000;
pub const EV_FLAG1: u16 = 0x2000;
pub const EV_FLAG2: u16 = 0x4000;
pub const EV_EOF: u16 = 0x8000;
pub const EV_ERROR: u16 = 0x4000;

/// `EVFILT_USER`'s input flag operations and trigger.
pub const NOTE_FFNOP: u32 = 0x0000_0000;
pub const NOTE_FFAND: u32 = 0x4000_0000;
pub const NOTE_FFOR: u32 = 0x8000_0000;
pub const NOTE_FFCOPY: u32 = 0xc000_0000;
pub const NOTE_FFCTRLMASK: u32 = 0xc000_0000;
pub const NOTE_FFLAGSMASK: u32 = 0x00ff_ffff;
pub const NOTE_TRIGGER: u32 = 0x0100_0000;

/// `EVFILT_READ` and `EVFILT_WRITE`.
pub const NOTE_LOWAT: u32 = 0x0001;
pub const NOTE_FILE_POLL: u32 = 0x0002;

/// `EVFILT_VNODE`.
pub const NOTE_DELETE: u32 = 0x0001;
pub const NOTE_WRITE: u32 = 0x0002;
pub const NOTE_EXTEND: u32 = 0x0004;
pub const NOTE_ATTRIB: u32 = 0x0008;
pub const NOTE_LINK: u32 = 0x0010;
pub const NOTE_RENAME: u32 = 0x0020;
pub const NOTE_REVOKE: u32 = 0x0040;
pub const NOTE_OPEN: u32 = 0x0080;
pub const NOTE_CLOSE: u32 = 0x0100;
pub const NOTE_CLOSE_WRITE: u32 = 0x0200;
pub const NOTE_READ: u32 = 0x0400;

/// `EVFILT_PROC` and `EVFILT_PROCDESC`.
pub const NOTE_EXIT: u32 = 0x8000_0000;
pub const NOTE_FORK: u32 = 0x4000_0000;
pub const NOTE_EXEC: u32 = 0x2000_0000;
/// FreeBSD 16's: absent from 15's headers, so not checked against them;
/// used here as kernel hints (15's userland never asks for them).
pub const NOTE_PDSIGCHLD: u32 = 0x1000_0000;
pub const NOTE_SIGNAL: u32 = 0x0800_0000;
pub const NOTE_REAP: u32 = 0x0400_0000;
/// 15's mask (16's adds `NOTE_SIGNAL` and `NOTE_REAP` to it).
pub const NOTE_PCTRLMASK: u32 = 0xf000_0000;
pub const NOTE_PDATAMASK: u32 = 0x000f_ffff;
pub const NOTE_TRACK: u32 = 0x0000_0001;
pub const NOTE_TRACKERR: u32 = 0x0000_0002;
pub const NOTE_CHILD: u32 = 0x0000_0004;

/// `EVFILT_TIMER`.
pub const NOTE_SECONDS: u32 = 0x0001;
pub const NOTE_MSECONDS: u32 = 0x0002;
pub const NOTE_USECONDS: u32 = 0x0004;
pub const NOTE_NSECONDS: u32 = 0x0008;
pub const NOTE_ABSTIME: u32 = 0x0010;

/// `kqueuex` flags.
pub const KQUEUE_CLOEXEC: u32 = 0x0001;
pub const KQUEUE_CPONFORK: u32 = 0x0002;

/// `struct pollfd`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pollfd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

pub const POLLIN: i16 = 0x0001;
pub const POLLPRI: i16 = 0x0002;
pub const POLLOUT: i16 = 0x0004;
pub const POLLRDNORM: i16 = 0x0040;
pub const POLLWRNORM: i16 = POLLOUT;
pub const POLLRDBAND: i16 = 0x0080;
pub const POLLWRBAND: i16 = 0x0100;
pub const POLLINIGNEOF: i16 = 0x2000;
pub const POLLRDHUP: i16 = 0x4000;
pub const POLLERR: i16 = 0x0008;
pub const POLLHUP: i16 = 0x0010;
pub const POLLNVAL: i16 = 0x0020;
pub const POLLSTANDARD: i16 = POLLIN
    | POLLPRI
    | POLLOUT
    | POLLRDNORM
    | POLLRDBAND
    | POLLWRBAND
    | POLLERR
    | POLLHUP
    | POLLNVAL;
/// `INFTIM`: `poll` waits for ever.
pub const INFTIM: i32 = -1;
/// `FD_SETSIZE`.
pub const FD_SETSIZE: usize = 1024;

/// `struct itimerval`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Itimerval {
    pub it_interval: Timeval,
    pub it_value: Timeval,
}

/// `struct itimerspec`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Itimerspec {
    pub it_interval: Timespec,
    pub it_value: Timespec,
}

pub const ITIMER_REAL: u32 = 0;
pub const ITIMER_VIRTUAL: u32 = 1;
pub const ITIMER_PROF: u32 = 2;

/// `clock_nanosleep`'s and `timer_settime`'s absolute-time flag.
pub const TIMER_RELTIME: u32 = 0x0;
pub const TIMER_ABSTIME: u32 = 0x1;

/// `struct sigevent`: the union's thread id (`sigev_notify_thread_id`)
/// and `SIGEV_KEVENT` flags overlay its first word.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sigevent {
    pub sigev_notify: i32,
    pub sigev_signo: i32,
    pub sigev_value: u64,
    pub sigev_un: [i64; 8],
}

const _: () = assert!(size_of::<Sigevent>() == 80);

pub const SIGEV_NONE: i32 = 0;
pub const SIGEV_SIGNAL: i32 = 1;
pub const SIGEV_THREAD: i32 = 2;
pub const SIGEV_KEVENT: i32 = 3;
pub const SIGEV_THREAD_ID: i32 = 4;

/// `timerfd_create` and `timerfd_settime` flags.
pub const TFD_TIMER_ABSTIME: u32 = 0x01;
pub const TFD_TIMER_CANCEL_ON_SET: u32 = 0x02;

/// `eventfd` flags.
pub const EFD_SEMAPHORE: u32 = 0x0000_0001;
pub const EFD_NONBLOCK: u32 = 0x0000_0004;
pub const EFD_CLOEXEC: u32 = 0x0010_0000;

/// `__specialfd` types.
pub const SPECIALFD_EVENTFD: i32 = 1;
pub const SPECIALFD_INOTIFY: i32 = 2;

/// `struct specialfd_eventfd`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpecialfdEventfd {
    pub initval: u32,
    pub flags: i32,
}

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}

const fn ev(c_name: &'static str, value: i64) -> ConstValue {
    c(c_name, "sys/event.h", value)
}

const fn poll(c_name: &'static str, value: i16) -> ConstValue {
    c(c_name, "poll.h", value as i64)
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    ev("EVFILT_READ", EVFILT_READ as i64),
    ev("EVFILT_WRITE", EVFILT_WRITE as i64),
    ev("EVFILT_AIO", EVFILT_AIO as i64),
    ev("EVFILT_VNODE", EVFILT_VNODE as i64),
    ev("EVFILT_PROC", EVFILT_PROC as i64),
    ev("EVFILT_SIGNAL", EVFILT_SIGNAL as i64),
    ev("EVFILT_TIMER", EVFILT_TIMER as i64),
    ev("EVFILT_PROCDESC", EVFILT_PROCDESC as i64),
    ev("EVFILT_FS", EVFILT_FS as i64),
    ev("EVFILT_LIO", EVFILT_LIO as i64),
    ev("EVFILT_USER", EVFILT_USER as i64),
    ev("EVFILT_SENDFILE", EVFILT_SENDFILE as i64),
    ev("EVFILT_EMPTY", EVFILT_EMPTY as i64),
    ev("EVFILT_JAIL", EVFILT_JAIL as i64),
    ev("EVFILT_JAILDESC", EVFILT_JAILDESC as i64),
    ev("EVFILT_SYSCOUNT", EVFILT_SYSCOUNT as i64),
    ev("EV_ADD", EV_ADD as i64),
    ev("EV_DELETE", EV_DELETE as i64),
    ev("EV_ENABLE", EV_ENABLE as i64),
    ev("EV_DISABLE", EV_DISABLE as i64),
    ev("EV_FORCEONESHOT", EV_FORCEONESHOT as i64),
    ev("EV_KEEPUDATA", EV_KEEPUDATA as i64),
    ev("EV_ONESHOT", EV_ONESHOT as i64),
    ev("EV_CLEAR", EV_CLEAR as i64),
    ev("EV_RECEIPT", EV_RECEIPT as i64),
    ev("EV_DISPATCH", EV_DISPATCH as i64),
    ev("EV_SYSFLAGS", EV_SYSFLAGS as i64),
    ev("EV_DROP", EV_DROP as i64),
    ev("EV_FLAG1", EV_FLAG1 as i64),
    ev("EV_FLAG2", EV_FLAG2 as i64),
    ev("EV_EOF", EV_EOF as i64),
    ev("EV_ERROR", EV_ERROR as i64),
    ev("NOTE_FFNOP", NOTE_FFNOP as i64),
    ev("NOTE_FFAND", NOTE_FFAND as i64),
    ev("NOTE_FFOR", NOTE_FFOR as i64),
    ev("NOTE_FFCOPY", NOTE_FFCOPY as i64),
    ev("NOTE_FFCTRLMASK", NOTE_FFCTRLMASK as i64),
    ev("NOTE_FFLAGSMASK", NOTE_FFLAGSMASK as i64),
    ev("NOTE_TRIGGER", NOTE_TRIGGER as i64),
    ev("NOTE_LOWAT", NOTE_LOWAT as i64),
    ev("NOTE_FILE_POLL", NOTE_FILE_POLL as i64),
    ev("NOTE_DELETE", NOTE_DELETE as i64),
    ev("NOTE_WRITE", NOTE_WRITE as i64),
    ev("NOTE_EXTEND", NOTE_EXTEND as i64),
    ev("NOTE_ATTRIB", NOTE_ATTRIB as i64),
    ev("NOTE_LINK", NOTE_LINK as i64),
    ev("NOTE_RENAME", NOTE_RENAME as i64),
    ev("NOTE_REVOKE", NOTE_REVOKE as i64),
    ev("NOTE_OPEN", NOTE_OPEN as i64),
    ev("NOTE_CLOSE", NOTE_CLOSE as i64),
    ev("NOTE_CLOSE_WRITE", NOTE_CLOSE_WRITE as i64),
    ev("NOTE_READ", NOTE_READ as i64),
    ev("NOTE_EXIT", NOTE_EXIT as i64),
    ev("NOTE_FORK", NOTE_FORK as i64),
    ev("NOTE_EXEC", NOTE_EXEC as i64),
    ev("NOTE_PCTRLMASK", NOTE_PCTRLMASK as i64),
    ev("NOTE_PDATAMASK", NOTE_PDATAMASK as i64),
    ev("NOTE_TRACK", NOTE_TRACK as i64),
    ev("NOTE_TRACKERR", NOTE_TRACKERR as i64),
    ev("NOTE_CHILD", NOTE_CHILD as i64),
    ev("NOTE_SECONDS", NOTE_SECONDS as i64),
    ev("NOTE_MSECONDS", NOTE_MSECONDS as i64),
    ev("NOTE_USECONDS", NOTE_USECONDS as i64),
    ev("NOTE_NSECONDS", NOTE_NSECONDS as i64),
    ev("NOTE_ABSTIME", NOTE_ABSTIME as i64),
    ev("KQUEUE_CLOEXEC", KQUEUE_CLOEXEC as i64),
    ev("KQUEUE_CPONFORK", KQUEUE_CPONFORK as i64),
    poll("POLLIN", POLLIN),
    poll("POLLPRI", POLLPRI),
    poll("POLLOUT", POLLOUT),
    poll("POLLRDNORM", POLLRDNORM),
    poll("POLLWRNORM", POLLWRNORM),
    poll("POLLRDBAND", POLLRDBAND),
    poll("POLLWRBAND", POLLWRBAND),
    poll("POLLINIGNEOF", POLLINIGNEOF),
    poll("POLLRDHUP", POLLRDHUP),
    poll("POLLERR", POLLERR),
    poll("POLLHUP", POLLHUP),
    poll("POLLNVAL", POLLNVAL),
    poll("POLLSTANDARD", POLLSTANDARD),
    c("INFTIM", "poll.h", INFTIM as i64),
    c("FD_SETSIZE", "sys/select.h", FD_SETSIZE as i64),
    c("ITIMER_REAL", "sys/time.h", ITIMER_REAL as i64),
    c("ITIMER_VIRTUAL", "sys/time.h", ITIMER_VIRTUAL as i64),
    c("ITIMER_PROF", "sys/time.h", ITIMER_PROF as i64),
    c("TIMER_RELTIME", "time.h", TIMER_RELTIME as i64),
    c("TIMER_ABSTIME", "time.h", TIMER_ABSTIME as i64),
    c("SIGEV_NONE", "signal.h", SIGEV_NONE as i64),
    c("SIGEV_SIGNAL", "signal.h", SIGEV_SIGNAL as i64),
    c("SIGEV_THREAD", "signal.h", SIGEV_THREAD as i64),
    c("SIGEV_KEVENT", "signal.h", SIGEV_KEVENT as i64),
    c("SIGEV_THREAD_ID", "signal.h", SIGEV_THREAD_ID as i64),
    c(
        "TFD_TIMER_ABSTIME",
        "sys/timerfd.h",
        TFD_TIMER_ABSTIME as i64,
    ),
    c(
        "TFD_TIMER_CANCEL_ON_SET",
        "sys/timerfd.h",
        TFD_TIMER_CANCEL_ON_SET as i64,
    ),
    c("EFD_SEMAPHORE", "sys/eventfd.h", EFD_SEMAPHORE as i64),
    c("EFD_NONBLOCK", "sys/eventfd.h", EFD_NONBLOCK as i64),
    c("EFD_CLOEXEC", "sys/eventfd.h", EFD_CLOEXEC as i64),
    c(
        "SPECIALFD_EVENTFD",
        "sys/specialfd.h",
        SPECIALFD_EVENTFD as i64,
    ),
    c(
        "SPECIALFD_INOTIFY",
        "sys/specialfd.h",
        SPECIALFD_INOTIFY as i64,
    ),
];
