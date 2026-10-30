// SPDX-License-Identifier: BSD-2-Clause
//! `<sys/inotify.h>`: the events a watch asks for and a read returns, and
//! the record a read returns them in.

use crate::layout::{Arches, ConstValue};

/// `inotify_init1` flags (`O_NONBLOCK`, `O_CLOEXEC`).
pub const IN_NONBLOCK: u32 = 0x0000_0004;
pub const IN_CLOEXEC: u32 = 0x0010_0000;

/// Events.
pub const IN_ACCESS: u32 = 0x0000_0001;
pub const IN_MODIFY: u32 = 0x0000_0002;
pub const IN_ATTRIB: u32 = 0x0000_0004;
pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
pub const IN_OPEN: u32 = 0x0000_0020;
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
pub const IN_ALL_EVENTS: u32 = 0x0000_0fff;

/// The events a directory's own watch reports about its entries, not
/// about itself (`_IN_DIR_EVENTS`).
pub const IN_DIR_EVENTS: u32 = IN_CLOSE_WRITE | IN_DELETE | IN_MODIFY | IN_MOVED_FROM | IN_MOVED_TO;

/// Watch flags.
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
pub const IN_EXCL_UNLINK: u32 = 0x0400_0000;
pub const IN_MASK_CREATE: u32 = 0x1000_0000;
pub const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ONESHOT: u32 = 0x8000_0000;
pub const IN_ALL_FLAGS: u32 =
    IN_ONLYDIR | IN_DONT_FOLLOW | IN_EXCL_UNLINK | IN_MASK_CREATE | IN_MASK_ADD | IN_ONESHOT;

/// Flags a read returns.
pub const IN_UNMOUNT: u32 = 0x0000_2000;
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;
pub const IN_ISDIR: u32 = 0x4000_0000;

/// `struct inotify_event`, without the name that follows it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InotifyEvent {
    pub wd: i32,
    pub mask: u32,
    pub cookie: u32,
    pub len: u32,
}

/// `_IN_NAMESIZE`: a name's room after the record, its NUL included,
/// rounded to the record's alignment; none for no name.
#[must_use]
pub const fn name_size(len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (len + 1).next_multiple_of(align_of::<InotifyEvent>())
    }
}

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/inotify.h",
        value: value as i64,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("IN_NONBLOCK", IN_NONBLOCK),
    c("IN_CLOEXEC", IN_CLOEXEC),
    c("IN_ACCESS", IN_ACCESS),
    c("IN_MODIFY", IN_MODIFY),
    c("IN_ATTRIB", IN_ATTRIB),
    c("IN_CLOSE_WRITE", IN_CLOSE_WRITE),
    c("IN_CLOSE_NOWRITE", IN_CLOSE_NOWRITE),
    c("IN_OPEN", IN_OPEN),
    c("IN_MOVED_FROM", IN_MOVED_FROM),
    c("IN_MOVED_TO", IN_MOVED_TO),
    c("IN_CREATE", IN_CREATE),
    c("IN_DELETE", IN_DELETE),
    c("IN_DELETE_SELF", IN_DELETE_SELF),
    c("IN_MOVE_SELF", IN_MOVE_SELF),
    c("IN_ALL_EVENTS", IN_ALL_EVENTS),
    c("_IN_DIR_EVENTS", IN_DIR_EVENTS),
    c("IN_ONLYDIR", IN_ONLYDIR),
    c("IN_DONT_FOLLOW", IN_DONT_FOLLOW),
    c("IN_EXCL_UNLINK", IN_EXCL_UNLINK),
    c("IN_MASK_CREATE", IN_MASK_CREATE),
    c("IN_MASK_ADD", IN_MASK_ADD),
    c("IN_ONESHOT", IN_ONESHOT),
    c("_IN_ALL_FLAGS", IN_ALL_FLAGS),
    c("IN_UNMOUNT", IN_UNMOUNT),
    c("IN_Q_OVERFLOW", IN_Q_OVERFLOW),
    c("IN_IGNORED", IN_IGNORED),
    c("IN_ISDIR", IN_ISDIR),
];
