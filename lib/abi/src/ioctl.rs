// SPDX-License-Identifier: BSD-2-Clause
//! `ioctl` requests (`sys/ioccom.h`, `sys/filio.h`, `sys/ttycom.h`), and
//! the terminal ones' modem bits and line discipline.
//!
//! A request encodes its direction, the size of its argument, a group
//! letter, and a number. The kernel copies the argument in or out by that
//! size, so a request is known by its whole value.

use crate::layout::{Arches, ConstValue};

/// The argument is copied out (`IOC_OUT`), in (`IOC_IN`), or neither
/// (`IOC_VOID`).
pub const IOC_VOID: u32 = 0x2000_0000;
pub const IOC_OUT: u32 = 0x4000_0000;
pub const IOC_IN: u32 = 0x8000_0000;
/// The argument's size is in these bits (`IOCPARM_MASK`, shifted by 16).
pub const IOCPARM_MASK: u32 = 0x1fff;
/// The largest argument (`IOCPARM_MAX`).
pub const IOCPARM_MAX: u32 = 1 << 13;

const fn ioc(dir: u32, group: u8, num: u8, len: u32) -> u32 {
    dir | ((len & IOCPARM_MASK) << 16) | (group as u32) << 8 | num as u32
}

/// The size of a request's argument.
#[must_use]
pub const fn param_len(req: u32) -> u32 {
    (req >> 16) & IOCPARM_MASK
}

pub const FIOCLEX: u32 = ioc(IOC_VOID, b'f', 1, 0);
pub const FIONCLEX: u32 = ioc(IOC_VOID, b'f', 2, 0);
pub const FIONREAD: u32 = ioc(IOC_OUT, b'f', 127, 4);
pub const FIONBIO: u32 = ioc(IOC_IN, b'f', 126, 4);
pub const FIOASYNC: u32 = ioc(IOC_IN, b'f', 125, 4);
pub const FIOSETOWN: u32 = ioc(IOC_IN, b'f', 124, 4);
pub const FIOGETOWN: u32 = ioc(IOC_OUT, b'f', 123, 4);
pub const FIODTYPE: u32 = ioc(IOC_OUT, b'f', 122, 4);
pub const FIONWRITE: u32 = ioc(IOC_OUT, b'f', 119, 4);
pub const FIONSPACE: u32 = ioc(IOC_OUT, b'f', 118, 4);

/// `struct termios` is 44 bytes, `struct winsize` 8.
const TERMIOS: u32 = 44;
const WINSIZE: u32 = 8;

pub const TIOCEXCL: u32 = ioc(IOC_VOID, b't', 13, 0);
pub const TIOCNXCL: u32 = ioc(IOC_VOID, b't', 14, 0);
pub const TIOCFLUSH: u32 = ioc(IOC_IN, b't', 16, 4);
pub const TIOCGETA: u32 = ioc(IOC_OUT, b't', 19, TERMIOS);
pub const TIOCSETA: u32 = ioc(IOC_IN, b't', 20, TERMIOS);
pub const TIOCSETAW: u32 = ioc(IOC_IN, b't', 21, TERMIOS);
pub const TIOCSETAF: u32 = ioc(IOC_IN, b't', 22, TERMIOS);
pub const TIOCGETD: u32 = ioc(IOC_OUT, b't', 26, 4);
pub const TIOCSETD: u32 = ioc(IOC_IN, b't', 27, 4);
pub const TIOCDRAIN: u32 = ioc(IOC_VOID, b't', 94, 0);
pub const TIOCSCTTY: u32 = ioc(IOC_VOID, b't', 97, 0);
pub const TIOCGSID: u32 = ioc(IOC_OUT, b't', 99, 4);
pub const TIOCSTAT: u32 = ioc(IOC_VOID, b't', 101, 0);
pub const TIOCSWINSZ: u32 = ioc(IOC_IN, b't', 103, WINSIZE);
pub const TIOCGWINSZ: u32 = ioc(IOC_OUT, b't', 104, WINSIZE);
pub const TIOCMGET: u32 = ioc(IOC_OUT, b't', 106, 4);
pub const TIOCMBIC: u32 = ioc(IOC_IN, b't', 107, 4);
pub const TIOCMBIS: u32 = ioc(IOC_IN, b't', 108, 4);
pub const TIOCMSET: u32 = ioc(IOC_IN, b't', 109, 4);
pub const TIOCSTART: u32 = ioc(IOC_VOID, b't', 110, 0);
pub const TIOCSTOP: u32 = ioc(IOC_VOID, b't', 111, 0);
pub const TIOCNOTTY: u32 = ioc(IOC_VOID, b't', 113, 0);
pub const TIOCSTI: u32 = ioc(IOC_IN, b't', 114, 1);
pub const TIOCOUTQ: u32 = ioc(IOC_OUT, b't', 115, 4);
pub const TIOCSPGRP: u32 = ioc(IOC_IN, b't', 118, 4);
pub const TIOCGPGRP: u32 = ioc(IOC_OUT, b't', 119, 4);
pub const TIOCCDTR: u32 = ioc(IOC_VOID, b't', 120, 0);
pub const TIOCSDTR: u32 = ioc(IOC_VOID, b't', 121, 0);
pub const TIOCCBRK: u32 = ioc(IOC_VOID, b't', 122, 0);
pub const TIOCSBRK: u32 = ioc(IOC_VOID, b't', 123, 0);

/// Modem lines (`TIOCM_*`).
pub const TIOCM_LE: u32 = 0o001;
pub const TIOCM_DTR: u32 = 0o002;
pub const TIOCM_RTS: u32 = 0o004;
pub const TIOCM_CTS: u32 = 0o040;
pub const TIOCM_DCD: u32 = 0o100;
pub const TIOCM_DSR: u32 = 0o400;

/// `TIOCGETD`'s one line discipline.
pub const TTYDISC: u32 = 0;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("IOC_VOID", "sys/ioccom.h", IOC_VOID),
    c("IOC_OUT", "sys/ioccom.h", IOC_OUT),
    c("IOC_IN", "sys/ioccom.h", IOC_IN),
    c("IOCPARM_MASK", "sys/ioccom.h", IOCPARM_MASK),
    c("IOCPARM_MAX", "sys/ioccom.h", IOCPARM_MAX),
    c("FIOCLEX", "sys/filio.h", FIOCLEX),
    c("FIONCLEX", "sys/filio.h", FIONCLEX),
    c("FIONREAD", "sys/filio.h", FIONREAD),
    c("FIONBIO", "sys/filio.h", FIONBIO),
    c("FIOASYNC", "sys/filio.h", FIOASYNC),
    c("FIOSETOWN", "sys/filio.h", FIOSETOWN),
    c("FIOGETOWN", "sys/filio.h", FIOGETOWN),
    c("FIODTYPE", "sys/filio.h", FIODTYPE),
    c("FIONWRITE", "sys/filio.h", FIONWRITE),
    c("FIONSPACE", "sys/filio.h", FIONSPACE),
    c("TIOCEXCL", "sys/ioctl.h", TIOCEXCL),
    c("TIOCNXCL", "sys/ioctl.h", TIOCNXCL),
    c("TIOCFLUSH", "sys/ioctl.h", TIOCFLUSH),
    c("TIOCGETA", "sys/ioctl.h", TIOCGETA),
    c("TIOCSETA", "sys/ioctl.h", TIOCSETA),
    c("TIOCSETAW", "sys/ioctl.h", TIOCSETAW),
    c("TIOCSETAF", "sys/ioctl.h", TIOCSETAF),
    c("TIOCGETD", "sys/ioctl.h", TIOCGETD),
    c("TIOCSETD", "sys/ioctl.h", TIOCSETD),
    c("TIOCDRAIN", "sys/ioctl.h", TIOCDRAIN),
    c("TIOCSCTTY", "sys/ioctl.h", TIOCSCTTY),
    c("TIOCGSID", "sys/ioctl.h", TIOCGSID),
    c("TIOCSTAT", "sys/ioctl.h", TIOCSTAT),
    c("TIOCSWINSZ", "sys/ioctl.h", TIOCSWINSZ),
    c("TIOCGWINSZ", "sys/ioctl.h", TIOCGWINSZ),
    c("TIOCMGET", "sys/ioctl.h", TIOCMGET),
    c("TIOCMBIC", "sys/ioctl.h", TIOCMBIC),
    c("TIOCMBIS", "sys/ioctl.h", TIOCMBIS),
    c("TIOCMSET", "sys/ioctl.h", TIOCMSET),
    c("TIOCSTART", "sys/ioctl.h", TIOCSTART),
    c("TIOCSTOP", "sys/ioctl.h", TIOCSTOP),
    c("TIOCNOTTY", "sys/ioctl.h", TIOCNOTTY),
    c("TIOCSTI", "sys/ioctl.h", TIOCSTI),
    c("TIOCOUTQ", "sys/ioctl.h", TIOCOUTQ),
    c("TIOCSPGRP", "sys/ioctl.h", TIOCSPGRP),
    c("TIOCGPGRP", "sys/ioctl.h", TIOCGPGRP),
    c("TIOCCDTR", "sys/ioctl.h", TIOCCDTR),
    c("TIOCSDTR", "sys/ioctl.h", TIOCSDTR),
    c("TIOCCBRK", "sys/ioctl.h", TIOCCBRK),
    c("TIOCSBRK", "sys/ioctl.h", TIOCSBRK),
    c("TIOCM_LE", "sys/ioctl.h", TIOCM_LE),
    c("TIOCM_DTR", "sys/ioctl.h", TIOCM_DTR),
    c("TIOCM_RTS", "sys/ioctl.h", TIOCM_RTS),
    c("TIOCM_CTS", "sys/ioctl.h", TIOCM_CTS),
    c("TIOCM_DCD", "sys/ioctl.h", TIOCM_DCD),
    c("TIOCM_DSR", "sys/ioctl.h", TIOCM_DSR),
    c("TTYDISC", "sys/ioctl.h", TTYDISC),
];

const fn c(c_name: &'static str, header: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header,
        // `ioctl` takes an `unsigned long`; compare the bits.
        value: value as i64,
        arches: Arches::All,
    }
}
