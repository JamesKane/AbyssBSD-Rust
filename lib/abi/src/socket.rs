// SPDX-License-Identifier: BSD-2-Clause
//! Sockets (`sys/socket.h`, `sys/un.h`): the parts UNIX-domain sockets
//! use.

use crate::layout::{Arches, ConstValue};

/// `AF_UNIX` (`AF_LOCAL`).
pub const AF_UNIX: u8 = 1;

pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;
pub const SOCK_SEQPACKET: u32 = 5;
/// Flags `socket` takes with the type.
pub const SOCK_CLOEXEC: u32 = 0x1000_0000;
pub const SOCK_NONBLOCK: u32 = 0x2000_0000;
pub const SOCK_CLOFORK: u32 = 0x4000_0000;
/// The longest address a call takes.
pub const SOCK_MAXADDRLEN: u32 = 255;

/// `struct sockaddr_un`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SockaddrUn {
    pub sun_len: u8,
    pub sun_family: u8,
    pub sun_path: [u8; 104],
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("AF_UNIX", "sys/socket.h", AF_UNIX as i64),
    c("SOCK_STREAM", "sys/socket.h", SOCK_STREAM as i64),
    c("SOCK_DGRAM", "sys/socket.h", SOCK_DGRAM as i64),
    c("SOCK_SEQPACKET", "sys/socket.h", SOCK_SEQPACKET as i64),
    c("SOCK_CLOEXEC", "sys/socket.h", SOCK_CLOEXEC as i64),
    c("SOCK_NONBLOCK", "sys/socket.h", SOCK_NONBLOCK as i64),
    c("SOCK_CLOFORK", "sys/socket.h", SOCK_CLOFORK as i64),
    c("SOCK_MAXADDRLEN", "sys/socket.h", SOCK_MAXADDRLEN as i64),
];

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}
