// SPDX-License-Identifier: BSD-2-Clause
//! The tunnel devices (`net/if_tun.h`, `net/if_tap.h`): what a program
//! that opens `/dev/tunN` or `/dev/tapN` asks of it.

use crate::layout::{Arches, ConstValue};

/// `TUNMTU`, `TUNMRU`, `TAPMRU`.
pub const TUNMTU: u32 = 1500;
pub const TUNMRU: usize = 65535;
pub const TAPMRU: usize = 65535;

/// `struct tuninfo` (and `tapinfo`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tuninfo {
    pub baudrate: i32,
    pub mtu: u16,
    pub r#type: u8,
    pub dummy: u8,
}

const fn ioc(dir: u32, group: u8, num: u8, len: usize) -> u32 {
    #[allow(clippy::cast_possible_truncation, reason = "ioctl sizes fit")]
    let len = len as u32;
    dir | ((len & crate::ioctl::IOCPARM_MASK) << 16) | (group as u32) << 8 | num as u32
}
const IN: u32 = crate::ioctl::IOC_IN;
const OUT: u32 = crate::ioctl::IOC_OUT;
const VOID: u32 = crate::ioctl::IOC_VOID;
const INT: usize = 4;
const TUNINFO: usize = size_of::<Tuninfo>();
const IFREQ: usize = size_of::<crate::net::Ifreq>();

pub const TUNSDEBUG: u32 = ioc(IN, b't', 90, INT);
pub const TUNGDEBUG: u32 = ioc(OUT, b't', 89, INT);
pub const TUNSIFINFO: u32 = ioc(IN, b't', 91, TUNINFO);
pub const TUNGIFINFO: u32 = ioc(OUT, b't', 92, TUNINFO);
pub const TUNSLMODE: u32 = ioc(IN, b't', 93, INT);
pub const TUNGIFNAME: u32 = ioc(OUT, b't', 93, IFREQ);
pub const TUNSIFMODE: u32 = ioc(IN, b't', 94, INT);
pub const TUNSIFPID: u32 = ioc(VOID, b't', 95, 0);
pub const TUNSIFHEAD: u32 = ioc(IN, b't', 96, INT);
pub const TUNGIFHEAD: u32 = ioc(OUT, b't', 97, INT);
pub const TUNSTRANSIENT: u32 = ioc(IN, b't', 98, INT);
pub const TUNGTRANSIENT: u32 = ioc(OUT, b't', 99, INT);
pub const TAPSVNETHDR: u32 = ioc(IN, b't', 91, INT);
pub const TAPGVNETHDR: u32 = ioc(OUT, b't', 94, INT);
/// `VMIO_SIOCSIFFLAGS` (`_IOWINT`: an `int` passed by value).
pub const VMIO_SIOCSIFFLAGS: u32 = ioc(VOID, b'V', 0, INT);

/// `struct virtio_net_hdr` and `virtio_net_hdr_mrg_rxbuf`'s sizes, the
/// lengths `TAPSVNETHDR` takes besides 0.
pub const VNET_HDR_LEN: usize = 10;
pub const VNET_HDR_MRG_LEN: usize = 12;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("TUNMTU", "net/if_tun.h", TUNMTU as i64),
    c("TUNMRU", "net/if_tun.h", TUNMRU as i64),
    c("TAPMRU", "net/if_tap.h", TAPMRU as i64),
    c("TUNSDEBUG", "net/if_tun.h", TUNSDEBUG as i64),
    c("TUNGDEBUG", "net/if_tun.h", TUNGDEBUG as i64),
    c("TUNSIFINFO", "net/if_tun.h", TUNSIFINFO as i64),
    c("TUNGIFINFO", "net/if_tun.h", TUNGIFINFO as i64),
    c("TUNSLMODE", "net/if_tun.h", TUNSLMODE as i64),
    c("TUNGIFNAME", "net/if_tun.h", TUNGIFNAME as i64),
    c("TUNSIFMODE", "net/if_tun.h", TUNSIFMODE as i64),
    c("TUNSIFPID", "net/if_tun.h", TUNSIFPID as i64),
    c("TUNSIFHEAD", "net/if_tun.h", TUNSIFHEAD as i64),
    c("TUNGIFHEAD", "net/if_tun.h", TUNGIFHEAD as i64),
    c("TUNSTRANSIENT", "net/if_tun.h", TUNSTRANSIENT as i64),
    c("TUNGTRANSIENT", "net/if_tun.h", TUNGTRANSIENT as i64),
    c("TAPSVNETHDR", "net/if_tap.h", TAPSVNETHDR as i64),
    c("TAPGVNETHDR", "net/if_tap.h", TAPGVNETHDR as i64),
    c(
        "VMIO_SIOCSIFFLAGS",
        "net/if_tap.h",
        VMIO_SIOCSIFFLAGS as i64,
    ),
];

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}
