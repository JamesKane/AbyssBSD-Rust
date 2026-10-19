// SPDX-License-Identifier: BSD-2-Clause
//! `ioctl` requests (`sys/ioccom.h`, `sys/filio.h`, `sys/ttycom.h`).
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

/// `struct termios` is 44 bytes.
pub const TIOCGETA: u32 = ioc(IOC_OUT, b't', 19, 44);
/// `struct winsize` is 8 bytes.
pub const TIOCGWINSZ: u32 = ioc(IOC_OUT, b't', 104, 8);
pub const TIOCGPGRP: u32 = ioc(IOC_OUT, b't', 119, 4);

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
    c("TIOCGETA", "termios.h", TIOCGETA),
    c("TIOCGWINSZ", "sys/ioctl.h", TIOCGWINSZ),
    c("TIOCGPGRP", "sys/ioctl.h", TIOCGPGRP),
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
