// SPDX-License-Identifier: BSD-2-Clause
//! sysctl(3) numbering: the fixed OIDs, node types, and flags.
//!
//! These are ABI. libc and the base utilities ask for fixed-number nodes
//! by number (`{CTL_KERN, KERN_OSRELDATE}`), so the numbers must be
//! FreeBSD's; [`CONSTANTS`] lists each one for `cargo xtask abi-check` to
//! compare with `<sys/sysctl.h>`. Nodes without a fixed number are
//! numbered from [`CTL_AUTO_START`] at runtime and found by name.

use crate::layout::ConstValue;

/// The longest OID, in components.
pub const CTL_MAXNAME: usize = 24;
/// The first number given to nodes registered without one (`OID_AUTO`).
pub const CTL_AUTO_START: i32 = 0x100;

/// The identity reported to userland: the targeted FreeBSD release
/// (docs/COMPATIBILITY.md; `kern.abyss.version` says what is really
/// running).
pub mod identity {
    /// `kern.ostype`.
    pub const OSTYPE: &str = "FreeBSD";
    /// `kern.osrelease`.
    pub const OSRELEASE: &str = "15.1-RELEASE";
    /// `kern.osreldate` (`__FreeBSD_version`).
    pub const OSRELDATE: i32 = 1_501_000;
    /// `kern.osrevision` (`BSD`, from `<sys/param.h>`).
    pub const OSREVISION: i32 = 199_506;
}

/// Top-level nodes.
pub mod ctl {
    /// The `sysctl.*` queries about the tree itself.
    pub const SYSCTL: i32 = 0;
    /// `kern.*`.
    pub const KERN: i32 = 1;
    /// `vm.*`.
    pub const VM: i32 = 2;
    /// `vfs.*`.
    pub const VFS: i32 = 3;
    /// `net.*`.
    pub const NET: i32 = 4;
    /// `debug.*`.
    pub const DEBUG: i32 = 5;
    /// `hw.*`.
    pub const HW: i32 = 6;
    /// `machdep.*`.
    pub const MACHDEP: i32 = 7;
    /// `user.*`.
    pub const USER: i32 = 8;
    /// `p1003_1b.*`.
    pub const P1003_1B: i32 = 9;
}

/// `sysctl.*`: queries about the tree (`{0, n, …}`).
pub mod query {
    /// Print every node (debug).
    pub const DEBUG: i32 = 0;
    /// The name of an OID.
    pub const NAME: i32 = 1;
    /// The next OID, honoring `CTLFLAG_SKIP`.
    pub const NEXT: i32 = 2;
    /// The OID of a name.
    pub const NAME2OID: i32 = 3;
    /// An OID's kind and format.
    pub const OIDFMT: i32 = 4;
    /// An OID's description.
    pub const OIDDESCR: i32 = 5;
    /// An OID's aggregation label.
    pub const OIDLABEL: i32 = 6;
    /// The next OID, ignoring `CTLFLAG_SKIP`.
    pub const NEXTNOSKIP: i32 = 7;
}

/// `kern.*` fixed numbers.
pub mod kern {
    /// `kern.ostype` (string).
    pub const OSTYPE: i32 = 1;
    /// `kern.osrelease` (string).
    pub const OSRELEASE: i32 = 2;
    /// `kern.osrevision` (int).
    pub const OSREV: i32 = 3;
    /// `kern.version` (string).
    pub const VERSION: i32 = 4;
    /// `kern.maxfiles` (int).
    pub const MAXFILES: i32 = 7;
    /// `kern.argmax` (int).
    pub const ARGMAX: i32 = 8;
    /// `kern.securelevel` (int).
    pub const SECURELVL: i32 = 9;
    /// `kern.hostname` (string).
    pub const HOSTNAME: i32 = 10;
    /// `kern.hostid` (unsigned long).
    pub const HOSTID: i32 = 11;
    /// `kern.domainname` (string).
    pub const NISDOMAINNAME: i32 = 22;
    /// `kern.hostuuid` (string).
    pub const HOSTUUID: i32 = 36;
    /// `kern.proc` (node).
    pub const PROC: i32 = 14;
    /// `kern.ngroups` (int).
    pub const NGROUPS: i32 = 18;
    /// `kern.boottime` (struct timeval).
    pub const BOOTTIME: i32 = 21;
    /// `kern.osreldate` (int).
    pub const OSRELDATE: i32 = 24;
    /// `kern.ps_strings` (address).
    pub const PS_STRINGS: i32 = 32;
    /// `kern.usrstack` (address).
    pub const USRSTACK: i32 = 33;
    /// `kern.iov_max` (int).
    pub const IOV_MAX: i32 = 35;
    /// `kern.arandom` (bytes).
    pub const ARND: i32 = 37;
    /// `kern.clockrate` (struct clockinfo).
    pub const CLOCKRATE: i32 = 12;
    /// `kern.bootfile` (string).
    pub const BOOTFILE: i32 = 26;
}

/// `CTL_VM` children (`VM_*`, `vm/vm_param.h`).
pub mod vm {
    /// `vm.vmtotal` (struct vmtotal).
    pub const TOTAL: i32 = 1;
    /// `vm.loadavg` (struct loadavg).
    pub const LOADAVG: i32 = 2;
    /// `vm.v_free_min` and its kin (u_int).
    pub const V_FREE_MIN: i32 = 3;
    pub const V_FREE_TARGET: i32 = 4;
    pub const V_FREE_RESERVED: i32 = 5;
    pub const V_INACTIVE_TARGET: i32 = 6;
    pub const V_PAGEOUT_FREE_MIN: i32 = 9;
}

/// `hw.*` fixed numbers.
pub mod hw {
    /// `hw.machine` (string).
    pub const MACHINE: i32 = 1;
    /// `hw.model` (string).
    pub const MODEL: i32 = 2;
    /// `hw.ncpu` (int).
    pub const NCPU: i32 = 3;
    /// `hw.byteorder` (int).
    pub const BYTEORDER: i32 = 4;
    /// `hw.physmem` (unsigned long).
    pub const PHYSMEM: i32 = 5;
    /// `hw.pagesize` (int).
    pub const PAGESIZE: i32 = 7;
    /// `hw.floatingpoint` (int).
    pub const FLOATINGPT: i32 = 10;
    /// `hw.machine_arch` (string).
    pub const MACHINE_ARCH: i32 = 11;
    /// `hw.realmem` (unsigned long).
    pub const REALMEM: i32 = 12;
    /// `hw.usermem` (unsigned long).
    pub const USERMEM: i32 = 6;
}

/// Node types: the low four bits of a node's kind.
pub mod ctltype {
    /// The mask for the type.
    pub const MASK: u32 = 0xf;
    /// An interior node.
    pub const NODE: u32 = 1;
    /// `int`.
    pub const INT: u32 = 2;
    /// A NUL-terminated string.
    pub const STRING: u32 = 3;
    /// `int64_t`.
    pub const S64: u32 = 4;
    /// A structure (described by the format string).
    pub const OPAQUE: u32 = 5;
    /// `u_int`.
    pub const UINT: u32 = 6;
    /// `long`.
    pub const LONG: u32 = 7;
    /// `u_long`.
    pub const ULONG: u32 = 8;
    /// `uint64_t`.
    pub const U64: u32 = 9;
    /// `uint8_t`.
    pub const U8: u32 = 0xa;
    /// `uint16_t`.
    pub const U16: u32 = 0xb;
    /// `int8_t`.
    pub const S8: u32 = 0xc;
    /// `int16_t`.
    pub const S16: u32 = 0xd;
    /// `int32_t`.
    pub const S32: u32 = 0xe;
    /// `uint32_t`.
    pub const U32: u32 = 0xf;
}

/// Node flags, in the high bits of a node's kind.
pub mod ctlflag {
    /// Readable.
    pub const RD: u32 = 0x8000_0000;
    /// Writable.
    pub const WR: u32 = 0x4000_0000;
    /// Readable and writable.
    pub const RW: u32 = RD | WR;
    /// Any user may write.
    pub const ANYBODY: u32 = 0x1000_0000;
    /// Writable only at securelevel 0 or below.
    pub const SECURE: u32 = 0x0800_0000;
    /// Writable by root in a jail.
    pub const PRISON: u32 = 0x0400_0000;
    /// Registered at runtime; may be removed.
    pub const DYN: u32 = 0x0200_0000;
    /// Left out of listings.
    pub const SKIP: u32 = 0x0100_0000;
    /// A loader tunable.
    pub const TUN: u32 = 0x0008_0000;
    /// The handler needs no Giant.
    pub const MPSAFE: u32 = 0x0004_0000;
    /// Readable in capability mode.
    pub const CAPRD: u32 = 0x0000_8000;
    /// Writable in capability mode.
    pub const CAPWR: u32 = 0x0000_4000;
    /// The securelevel above which a `SECURE` node is not writable, less
    /// one (`CTLMASK_SECURE`, shifted by `CTLSHIFT_SECURE`).
    pub const MASK_SECURE: u32 = 0x00f0_0000;
    pub const SHIFT_SECURE: u32 = 20;
    /// A statistic, not a tunable.
    pub const STATS: u32 = 0x0000_2000;
}

/// Every constant above, for `cargo xtask abi-check`.
pub const CONSTANTS: &[ConstValue] = &[
    c("CTL_MAXNAME", CTL_MAXNAME as i64),
    c("CTL_AUTO_START", CTL_AUTO_START as i64),
    c("CTL_SYSCTL", ctl::SYSCTL as i64),
    c("CTL_KERN", ctl::KERN as i64),
    c("CTL_VM", ctl::VM as i64),
    c("CTL_VFS", ctl::VFS as i64),
    c("CTL_NET", ctl::NET as i64),
    c("CTL_DEBUG", ctl::DEBUG as i64),
    c("CTL_HW", ctl::HW as i64),
    c("CTL_MACHDEP", ctl::MACHDEP as i64),
    c("CTL_USER", ctl::USER as i64),
    c("CTL_P1003_1B", ctl::P1003_1B as i64),
    c("CTL_SYSCTL_DEBUG", query::DEBUG as i64),
    c("CTL_SYSCTL_NAME", query::NAME as i64),
    c("CTL_SYSCTL_NEXT", query::NEXT as i64),
    c("CTL_SYSCTL_NAME2OID", query::NAME2OID as i64),
    c("CTL_SYSCTL_OIDFMT", query::OIDFMT as i64),
    c("CTL_SYSCTL_OIDDESCR", query::OIDDESCR as i64),
    c("CTL_SYSCTL_OIDLABEL", query::OIDLABEL as i64),
    c("CTL_SYSCTL_NEXTNOSKIP", query::NEXTNOSKIP as i64),
    c("KERN_OSTYPE", kern::OSTYPE as i64),
    c("KERN_OSRELEASE", kern::OSRELEASE as i64),
    c("KERN_OSREV", kern::OSREV as i64),
    c("KERN_VERSION", kern::VERSION as i64),
    c("KERN_MAXFILES", kern::MAXFILES as i64),
    c("KERN_ARGMAX", kern::ARGMAX as i64),
    c("KERN_HOSTNAME", kern::HOSTNAME as i64),
    c("KERN_SECURELVL", kern::SECURELVL as i64),
    c("KERN_HOSTID", kern::HOSTID as i64),
    c("KERN_NISDOMAINNAME", kern::NISDOMAINNAME as i64),
    c("KERN_HOSTUUID", kern::HOSTUUID as i64),
    c("KERN_PROC", kern::PROC as i64),
    c("KERN_NGROUPS", kern::NGROUPS as i64),
    c("KERN_BOOTTIME", kern::BOOTTIME as i64),
    c("KERN_OSRELDATE", kern::OSRELDATE as i64),
    c("KERN_PS_STRINGS", kern::PS_STRINGS as i64),
    c("KERN_USRSTACK", kern::USRSTACK as i64),
    c("KERN_IOV_MAX", kern::IOV_MAX as i64),
    c("KERN_ARND", kern::ARND as i64),
    c("KERN_CLOCKRATE", kern::CLOCKRATE as i64),
    c("KERN_BOOTFILE", kern::BOOTFILE as i64),
    v("VM_TOTAL", vm::TOTAL),
    v("VM_LOADAVG", vm::LOADAVG),
    v("VM_V_FREE_MIN", vm::V_FREE_MIN),
    v("VM_V_FREE_TARGET", vm::V_FREE_TARGET),
    v("VM_V_FREE_RESERVED", vm::V_FREE_RESERVED),
    v("VM_V_INACTIVE_TARGET", vm::V_INACTIVE_TARGET),
    v("VM_V_PAGEOUT_FREE_MIN", vm::V_PAGEOUT_FREE_MIN),
    c("HW_MACHINE", hw::MACHINE as i64),
    c("HW_MODEL", hw::MODEL as i64),
    c("HW_NCPU", hw::NCPU as i64),
    c("HW_BYTEORDER", hw::BYTEORDER as i64),
    c("HW_PHYSMEM", hw::PHYSMEM as i64),
    c("HW_PAGESIZE", hw::PAGESIZE as i64),
    c("HW_FLOATINGPT", hw::FLOATINGPT as i64),
    c("HW_MACHINE_ARCH", hw::MACHINE_ARCH as i64),
    c("HW_REALMEM", hw::REALMEM as i64),
    c("HW_USERMEM", hw::USERMEM as i64),
    c("CTLTYPE", ctltype::MASK as i64),
    c("CTLTYPE_NODE", ctltype::NODE as i64),
    c("CTLTYPE_INT", ctltype::INT as i64),
    c("CTLTYPE_STRING", ctltype::STRING as i64),
    c("CTLTYPE_S64", ctltype::S64 as i64),
    c("CTLTYPE_OPAQUE", ctltype::OPAQUE as i64),
    c("CTLTYPE_UINT", ctltype::UINT as i64),
    c("CTLTYPE_LONG", ctltype::LONG as i64),
    c("CTLTYPE_ULONG", ctltype::ULONG as i64),
    c("CTLTYPE_U64", ctltype::U64 as i64),
    c("CTLTYPE_U8", ctltype::U8 as i64),
    c("CTLTYPE_U16", ctltype::U16 as i64),
    c("CTLTYPE_S8", ctltype::S8 as i64),
    c("CTLTYPE_S16", ctltype::S16 as i64),
    c("CTLTYPE_S32", ctltype::S32 as i64),
    c("CTLTYPE_U32", ctltype::U32 as i64),
    c("CTLFLAG_RD", ctlflag::RD as i64),
    c("CTLFLAG_WR", ctlflag::WR as i64),
    c("CTLFLAG_ANYBODY", ctlflag::ANYBODY as i64),
    c("CTLFLAG_SECURE", ctlflag::SECURE as i64),
    c("CTLFLAG_PRISON", ctlflag::PRISON as i64),
    c("CTLFLAG_DYN", ctlflag::DYN as i64),
    c("CTLFLAG_SKIP", ctlflag::SKIP as i64),
    c("CTLFLAG_TUN", ctlflag::TUN as i64),
    c("CTLFLAG_MPSAFE", ctlflag::MPSAFE as i64),
    c("CTLFLAG_CAPRD", ctlflag::CAPRD as i64),
    c("CTLFLAG_CAPWR", ctlflag::CAPWR as i64),
    c("CTLMASK_SECURE", ctlflag::MASK_SECURE as i64),
    c("CTLSHIFT_SECURE", ctlflag::SHIFT_SECURE as i64),
    c("CTLFLAG_STATS", ctlflag::STATS as i64),
];

/// A `vm/vm_param.h` constant.
const fn v(c_name: &'static str, value: i32) -> ConstValue {
    ConstValue {
        c_name,
        header: "vm/vm_param.h",
        value: value as i64,
        arches: crate::layout::Arches::All,
    }
}

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/sysctl.h",
        value,
        arches: crate::layout::Arches::All,
    }
}
