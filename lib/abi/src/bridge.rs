// SPDX-License-Identifier: BSD-2-Clause
//! The bridge's control requests (`net/if_bridgevar.h`), which `ifconfig`
//! sends through `SIOCGDRVSPEC` and `SIOCSDRVSPEC` (`struct ifdrv`, in
//! `net/if.h`): each a command number and an argument of a fixed size.

use crate::layout::{Arches, ConstValue};
use crate::net::IFNAMSIZ;

/// `struct ifdrv`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifdrv {
    pub ifd_name: [u8; IFNAMSIZ],
    pub ifd_cmd: u64,
    pub ifd_len: u64,
    pub ifd_data: u64,
}

/// The `ifd_cmd`s (`BRDG*`).
pub mod cmd {
    pub const ADD: u64 = 0;
    pub const DEL: u64 = 1;
    pub const GIFFLGS: u64 = 2;
    pub const SIFFLGS: u64 = 3;
    pub const SCACHE: u64 = 4;
    pub const GCACHE: u64 = 5;
    pub const GIFS: u64 = 6;
    pub const RTS: u64 = 7;
    pub const SADDR: u64 = 8;
    pub const STO: u64 = 9;
    pub const GTO: u64 = 10;
    pub const DADDR: u64 = 11;
    pub const FLUSH: u64 = 12;
    pub const GPRI: u64 = 13;
    pub const SPRI: u64 = 14;
    pub const GHT: u64 = 15;
    pub const SHT: u64 = 16;
    pub const GFD: u64 = 17;
    pub const SFD: u64 = 18;
    pub const GMA: u64 = 19;
    pub const SMA: u64 = 20;
    pub const SIFPRIO: u64 = 21;
    pub const SIFCOST: u64 = 22;
    pub const ADDS: u64 = 23;
    pub const DELS: u64 = 24;
    pub const PARAM: u64 = 25;
    pub const GRTE: u64 = 26;
    pub const GIFSSTP: u64 = 27;
    pub const SPROTO: u64 = 28;
    pub const STXHC: u64 = 29;
    pub const SIFAMAX: u64 = 30;
    pub const SIFPVID: u64 = 31;
    pub const SIFVLANSET: u64 = 32;
    pub const GIFVLANSET: u64 = 33;
    pub const GFLAGS: u64 = 34;
    pub const SFLAGS: u64 = 35;
    pub const GDEFPVID: u64 = 36;
    pub const SDEFPVID: u64 = 37;
    pub const SIFVLANPROTO: u64 = 38;
}

/// `IFBRF_*`: the bridge's own flags.
pub mod ifbrf {
    pub const VLANFILTER: u32 = 1 << 0;
    pub const DEFQINQ: u32 = 1 << 1;
}

/// `IFBIF_*`: a member's flags.
pub mod ifbif {
    pub const LEARNING: u32 = 0x0001;
    pub const DISCOVER: u32 = 0x0002;
    pub const STP: u32 = 0x0004;
    pub const SPAN: u32 = 0x0008;
    pub const STICKY: u32 = 0x0010;
    pub const BSTP_EDGE: u32 = 0x0020;
    pub const BSTP_AUTOEDGE: u32 = 0x0040;
    pub const BSTP_PTP: u32 = 0x0080;
    pub const BSTP_AUTOPTP: u32 = 0x0100;
    pub const BSTP_ADMEDGE: u32 = 0x0200;
    pub const BSTP_ADMCOST: u32 = 0x0400;
    pub const PRIVATE: u32 = 0x0800;
    pub const QINQ: u32 = 0x1000;
    /// `IFBIFMASK`: what `BRDGSIFFLGS` keeps (the STP state options are
    /// the port's, not saved).
    pub const MASK: u32 =
        !(BSTP_EDGE | BSTP_AUTOEDGE | BSTP_PTP | BSTP_AUTOPTP | BSTP_ADMEDGE | BSTP_ADMCOST);
}

/// `IFBF_FLUSHDYN`, `IFBF_FLUSHALL`.
pub const IFBF_FLUSHDYN: u32 = 0;
pub const IFBF_FLUSHALL: u32 = 1;

/// `IFBAF_*`: an address's type.
pub mod ifbaf {
    pub const TYPEMASK: u8 = 0x03;
    pub const DYNAMIC: u8 = 0x00;
    pub const STATIC: u8 = 0x01;
    pub const STICKY: u8 = 0x02;
}

/// `BRDG_VLAN_OP_*`.
pub const BRDG_VLAN_OP_SET: u8 = 1;
pub const BRDG_VLAN_OP_ADD: u8 = 2;
pub const BRDG_VLAN_OP_DEL: u8 = 3;

/// `BRVLAN_SETSIZE`, and the words of an `ifbvlan_set_t`.
pub const BRVLAN_SETSIZE: usize = 4096;
pub const BRVLAN_WORDS: usize = BRVLAN_SETSIZE / 64;

/// `struct ifbreq`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifbreq {
    pub ifbr_ifsname: [u8; IFNAMSIZ],
    pub ifbr_ifsflags: u32,
    pub ifbr_stpflags: u32,
    pub ifbr_path_cost: u32,
    pub ifbr_portno: u8,
    pub ifbr_priority: u8,
    pub ifbr_proto: u8,
    pub ifbr_role: u8,
    pub ifbr_state: u8,
    pub _pad0: [u8; 3],
    pub ifbr_addrcnt: u32,
    pub ifbr_addrmax: u32,
    pub ifbr_addrexceeded: u32,
    pub ifbr_pvid: u16,
    pub ifbr_vlanproto: u16,
    pub pad: [u8; 28],
}

/// `struct ifbifconf` and `struct ifbaconf`: a buffer's length and
/// address.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifbconf {
    pub len: u32,
    pub _pad: u32,
    pub buf: u64,
}

/// `struct ifbareq`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifbareq {
    pub ifba_ifsname: [u8; IFNAMSIZ],
    pub ifba_expire: u64,
    pub ifba_flags: u8,
    pub ifba_dst: [u8; 6],
    pub _pad0: u8,
    pub ifba_vlan: u16,
    pub _pad1: [u8; 6],
}

/// `struct ifbrparam`: a union of an `int32`, `int16`, and `int8`,
/// each read from its first bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifbrparam {
    pub ifbrp_ifbrpu: u32,
}

impl Ifbrparam {
    /// `ifbrpu_int16`.
    #[must_use]
    pub fn int16(&self) -> u16 {
        let b = self.ifbrp_ifbrpu.to_ne_bytes();
        u16::from_ne_bytes([b[0], b[1]])
    }

    /// `ifbrpu_int8`.
    #[must_use]
    pub fn int8(&self) -> u8 {
        self.ifbrp_ifbrpu.to_ne_bytes()[0]
    }

    /// Stores `ifbrpu_int16`, leaving the other bytes.
    pub fn set_int16(&mut self, v: u16) {
        let mut b = self.ifbrp_ifbrpu.to_ne_bytes();
        b[..2].copy_from_slice(&v.to_ne_bytes());
        self.ifbrp_ifbrpu = u32::from_ne_bytes(b);
    }

    /// Stores `ifbrpu_int8`, leaving the other bytes.
    pub fn set_int8(&mut self, v: u8) {
        let mut b = self.ifbrp_ifbrpu.to_ne_bytes();
        b[0] = v;
        self.ifbrp_ifbrpu = u32::from_ne_bytes(b);
    }
}

/// `struct ifbropreq`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifbropreq {
    pub ifbop_holdcount: u8,
    pub ifbop_maxage: u8,
    pub ifbop_hellotime: u8,
    pub ifbop_fwddelay: u8,
    pub ifbop_protocol: u8,
    pub _pad0: u8,
    pub ifbop_priority: u16,
    pub ifbop_root_port: u16,
    pub _pad1: u16,
    pub ifbop_root_path_cost: u32,
    pub ifbop_bridgeid: u64,
    pub ifbop_designated_root: u64,
    pub ifbop_designated_bridge: u64,
    pub ifbop_last_tc_time: crate::time::Timeval,
}

/// `struct ifbpstpreq`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifbpstpreq {
    pub ifbp_portno: u8,
    pub _pad0: [u8; 3],
    pub ifbp_fwd_trans: u32,
    pub ifbp_design_cost: u32,
    pub ifbp_design_port: u32,
    pub ifbp_design_bridge: u64,
    pub ifbp_design_root: u64,
}

/// `struct ifbif_vlan_req`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IfbifVlanReq {
    pub bv_ifname: [u8; IFNAMSIZ],
    pub bv_op: u8,
    pub _pad0: [u8; 7],
    /// `bv_set`: one bit per VLAN, in `long`s.
    pub bv_set: [u64; BRVLAN_WORDS],
}

impl Default for IfbifVlanReq {
    fn default() -> Self {
        Self {
            bv_ifname: [0; IFNAMSIZ],
            bv_op: 0,
            _pad0: [0; 7],
            bv_set: [0; BRVLAN_WORDS],
        }
    }
}

/// STP's defaults (`net/bridgestp.h`), which `BRDGPARAM` and the
/// members' requests report: times in seconds, priorities, and the
/// protocol (`BSTP_PROTO_RSTP`).
pub mod bstp {
    pub const DEFAULT_MAX_AGE: u8 = 20;
    pub const DEFAULT_HELLO_TIME: u8 = 2;
    pub const DEFAULT_FORWARD_DELAY: u8 = 15;
    pub const DEFAULT_HOLD_COUNT: u8 = 6;
    pub const DEFAULT_BRIDGE_PRIORITY: u16 = 0x8000;
    pub const DEFAULT_PORT_PRIORITY: u8 = 0x80;
    pub const DEFAULT_PATH_COST: u32 = 55;
    pub const MAX_PATH_COST: u32 = 200_000_000;
    pub const PROTO_STP: u8 = 0;
    pub const PROTO_RSTP: u8 = 2;
    pub const IFSTATE_DISCARDING: u8 = 5;
    pub const ROLE_DISABLED: u8 = 0;
    /// `BSTP_PORT_AUTOEDGE`, `BSTP_PORT_AUTOPTP`, `BSTP_PORT_PNDCOST`,
    /// `BSTP_PORT_ADMCOST`.
    pub const PORT_AUTOEDGE: u32 = 0x0010;
    pub const PORT_AUTOPTP: u32 = 0x0020;
    pub const PORT_PNDCOST: u32 = 0x0080;
    pub const PORT_ADMCOST: u32 = 0x0008;
    /// The limits `bstp_set_*` check.
    pub const MIN_MAX_AGE: u8 = 6;
    pub const MAX_MAX_AGE: u8 = 40;
    pub const MIN_HELLO_TIME: u8 = 1;
    pub const MAX_HELLO_TIME: u8 = 2;
    pub const MIN_FORWARD_DELAY: u8 = 4;
    pub const MAX_FORWARD_DELAY: u8 = 30;
    pub const MIN_HOLD_COUNT: u8 = 1;
    pub const MAX_HOLD_COUNT: u8 = 10;
    pub const MAX_PRIORITY: u16 = 61440;
    pub const MAX_PORT_PRIORITY: u8 = 240;
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("BRDGADD", cmd::ADD),
    c("BRDGDEL", cmd::DEL),
    c("BRDGGIFFLGS", cmd::GIFFLGS),
    c("BRDGSIFFLGS", cmd::SIFFLGS),
    c("BRDGSCACHE", cmd::SCACHE),
    c("BRDGGCACHE", cmd::GCACHE),
    c("BRDGGIFS", cmd::GIFS),
    c("BRDGRTS", cmd::RTS),
    c("BRDGSADDR", cmd::SADDR),
    c("BRDGSTO", cmd::STO),
    c("BRDGGTO", cmd::GTO),
    c("BRDGDADDR", cmd::DADDR),
    c("BRDGFLUSH", cmd::FLUSH),
    c("BRDGGPRI", cmd::GPRI),
    c("BRDGSPRI", cmd::SPRI),
    c("BRDGGHT", cmd::GHT),
    c("BRDGSHT", cmd::SHT),
    c("BRDGGFD", cmd::GFD),
    c("BRDGSFD", cmd::SFD),
    c("BRDGGMA", cmd::GMA),
    c("BRDGSMA", cmd::SMA),
    c("BRDGSIFPRIO", cmd::SIFPRIO),
    c("BRDGSIFCOST", cmd::SIFCOST),
    c("BRDGADDS", cmd::ADDS),
    c("BRDGDELS", cmd::DELS),
    c("BRDGPARAM", cmd::PARAM),
    c("BRDGGRTE", cmd::GRTE),
    c("BRDGGIFSSTP", cmd::GIFSSTP),
    c("BRDGSPROTO", cmd::SPROTO),
    c("BRDGSTXHC", cmd::STXHC),
    c("BRDGSIFAMAX", cmd::SIFAMAX),
    c("BRDGSIFPVID", cmd::SIFPVID),
    c("BRDGSIFVLANSET", cmd::SIFVLANSET),
    c("BRDGGIFVLANSET", cmd::GIFVLANSET),
    c("BRDGGFLAGS", cmd::GFLAGS),
    c("BRDGSFLAGS", cmd::SFLAGS),
    c("BRDGGDEFPVID", cmd::GDEFPVID),
    c("BRDGSDEFPVID", cmd::SDEFPVID),
    c("BRDGSIFVLANPROTO", cmd::SIFVLANPROTO),
    c("IFBRF_VLANFILTER", ifbrf::VLANFILTER as u64),
    c("IFBRF_DEFQINQ", ifbrf::DEFQINQ as u64),
    c("IFBIF_LEARNING", ifbif::LEARNING as u64),
    c("IFBIF_DISCOVER", ifbif::DISCOVER as u64),
    c("IFBIF_STP", ifbif::STP as u64),
    c("IFBIF_SPAN", ifbif::SPAN as u64),
    c("IFBIF_STICKY", ifbif::STICKY as u64),
    c("IFBIF_BSTP_EDGE", ifbif::BSTP_EDGE as u64),
    c("IFBIF_BSTP_AUTOEDGE", ifbif::BSTP_AUTOEDGE as u64),
    c("IFBIF_BSTP_PTP", ifbif::BSTP_PTP as u64),
    c("IFBIF_BSTP_AUTOPTP", ifbif::BSTP_AUTOPTP as u64),
    c("IFBIF_BSTP_ADMEDGE", ifbif::BSTP_ADMEDGE as u64),
    c("IFBIF_BSTP_ADMCOST", ifbif::BSTP_ADMCOST as u64),
    c("IFBIF_PRIVATE", ifbif::PRIVATE as u64),
    c("IFBIF_QINQ", ifbif::QINQ as u64),
    c("IFBF_FLUSHDYN", IFBF_FLUSHDYN as u64),
    c("IFBF_FLUSHALL", IFBF_FLUSHALL as u64),
    c("IFBAF_TYPEMASK", ifbaf::TYPEMASK as u64),
    c("IFBAF_DYNAMIC", ifbaf::DYNAMIC as u64),
    c("IFBAF_STATIC", ifbaf::STATIC as u64),
    c("IFBAF_STICKY", ifbaf::STICKY as u64),
    c("BRDG_VLAN_OP_SET", BRDG_VLAN_OP_SET as u64),
    c("BRDG_VLAN_OP_ADD", BRDG_VLAN_OP_ADD as u64),
    c("BRDG_VLAN_OP_DEL", BRDG_VLAN_OP_DEL as u64),
    c("BRVLAN_SETSIZE", BRVLAN_SETSIZE as u64),
];

#[allow(clippy::cast_possible_wrap, reason = "small constants")]
const fn c(c_name: &'static str, value: u64) -> ConstValue {
    ConstValue {
        c_name,
        header: "net/if_bridgevar.h",
        value: value as i64,
        arches: Arches::All,
    }
}
