// SPDX-License-Identifier: BSD-2-Clause
//! The Berkeley Packet Filter (`net/bpf.h`, `net/dlt.h`): the `/dev/bpf`
//! requests, the header before each captured packet, and the filter
//! machine's instructions.

use crate::layout::{Arches, ConstValue};
use crate::net::Ifreq;
use crate::time::Timeval;

/// `BPF_MAJOR_VERSION`, `BPF_MINOR_VERSION`.
pub const MAJOR_VERSION: u16 = 1;
pub const MINOR_VERSION: u16 = 1;
/// `BPF_MAXINSNS`.
pub const MAXINSNS: u32 = 512;
/// `BPF_MEMWORDS`.
pub const MEMWORDS: usize = 16;
/// `BPF_ALIGNMENT` (`sizeof(long)`).
pub const ALIGNMENT: usize = 8;

/// `BPF_WORDALIGN`.
#[must_use]
pub const fn wordalign(x: usize) -> usize {
    (x + ALIGNMENT - 1) & !(ALIGNMENT - 1)
}

/// `struct bpf_insn`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BpfInsn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// `struct bpf_program`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BpfProgram {
    pub bf_len: u32,
    pub bf_insns: u64,
}

/// `struct bpf_stat`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BpfStat {
    pub bs_recv: u32,
    pub bs_drop: u32,
}

/// `struct bpf_version`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BpfVersion {
    pub bv_major: u16,
    pub bv_minor: u16,
}

/// `struct bpf_dltlist`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BpfDltlist {
    pub bfl_len: u32,
    pub bfl_list: u64,
}

/// `struct bpf_ts`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BpfTs {
    pub bt_sec: i64,
    pub bt_frac: u64,
}

/// `struct bpf_xhdr`: the header before a packet with `BPF_T_NANOTIME` or
/// `BPF_T_BINTIME` stamps (or none).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BpfXhdr {
    pub bh_tstamp: BpfTs,
    pub bh_caplen: u32,
    pub bh_datalen: u32,
    pub bh_hdrlen: u16,
}

/// `struct bpf_hdr`: the header before a packet with `BPF_T_MICROTIME`
/// stamps.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BpfHdr {
    pub bh_tstamp: Timeval,
    pub bh_caplen: u32,
    pub bh_datalen: u32,
    pub bh_hdrlen: u16,
}

/// The bytes of either header that matter (`SIZEOF_BPF_HDR`): up to the
/// end of `bh_hdrlen`.
pub const SIZEOF_BPF_HDR: usize = core::mem::offset_of!(BpfHdr, bh_hdrlen) + 2;
pub const SIZEOF_BPF_XHDR: usize = core::mem::offset_of!(BpfXhdr, bh_hdrlen) + 2;

/// Data-link types (`net/dlt.h`).
pub mod dlt {
    pub const NULL: u32 = 0;
    pub const EN10MB: u32 = 1;
}

/// Directions (`enum bpf_direction`).
pub mod dir {
    pub const IN: u32 = 0;
    pub const INOUT: u32 = 1;
    pub const OUT: u32 = 2;
}

/// Buffer modes (`BPF_BUFMODE_*`).
pub mod bufmode {
    pub const BUFFER: u32 = 1;
    pub const ZBUF: u32 = 2;
}

/// Timestamp kinds (`BPF_T_*`).
pub mod ts {
    pub const MICROTIME: u32 = 0x0000;
    pub const NANOTIME: u32 = 0x0001;
    pub const BINTIME: u32 = 0x0002;
    pub const NONE: u32 = 0x0003;
    pub const FORMAT_MASK: u32 = 0x0003;
    pub const FAST: u32 = 0x0100;
    pub const MONOTONIC: u32 = 0x0200;
    pub const FLAG_MASK: u32 = 0x0300;

    /// `BPF_T_VALID`.
    #[must_use]
    pub const fn valid(t: u32) -> bool {
        t == NONE || (t & !(FORMAT_MASK | FLAG_MASK) == 0 && t & FORMAT_MASK != NONE)
    }
}

/// Instruction fields.
pub mod op {
    pub const LD: u16 = 0x00;
    pub const LDX: u16 = 0x01;
    pub const ST: u16 = 0x02;
    pub const STX: u16 = 0x03;
    pub const ALU: u16 = 0x04;
    pub const JMP: u16 = 0x05;
    pub const RET: u16 = 0x06;
    pub const MISC: u16 = 0x07;

    pub const W: u16 = 0x00;
    pub const H: u16 = 0x08;
    pub const B: u16 = 0x10;

    pub const IMM: u16 = 0x00;
    pub const ABS: u16 = 0x20;
    pub const IND: u16 = 0x40;
    pub const MEM: u16 = 0x60;
    pub const LEN: u16 = 0x80;
    pub const MSH: u16 = 0xa0;

    pub const ADD: u16 = 0x00;
    pub const SUB: u16 = 0x10;
    pub const MUL: u16 = 0x20;
    pub const DIV: u16 = 0x30;
    pub const OR: u16 = 0x40;
    pub const AND: u16 = 0x50;
    pub const LSH: u16 = 0x60;
    pub const RSH: u16 = 0x70;
    pub const NEG: u16 = 0x80;
    pub const MOD: u16 = 0x90;
    pub const XOR: u16 = 0xa0;

    pub const JA: u16 = 0x00;
    pub const JEQ: u16 = 0x10;
    pub const JGT: u16 = 0x20;
    pub const JGE: u16 = 0x30;
    pub const JSET: u16 = 0x40;

    pub const K: u16 = 0x00;
    pub const X: u16 = 0x08;
    pub const A: u16 = 0x10;

    pub const TAX: u16 = 0x00;
    pub const TXA: u16 = 0x80;

    /// `BPF_CLASS`.
    #[must_use]
    pub const fn class(code: u16) -> u16 {
        code & 0x07
    }
}

const fn ioc(dir: u32, num: u8, len: usize) -> u32 {
    #[allow(clippy::cast_possible_truncation, reason = "ioctl sizes fit")]
    let len = len as u32;
    dir | ((len & crate::ioctl::IOCPARM_MASK) << 16) | (b'B' as u32) << 8 | num as u32
}
const VOID: u32 = crate::ioctl::IOC_VOID;
const IN: u32 = crate::ioctl::IOC_IN;
const OUT: u32 = crate::ioctl::IOC_OUT;
const INOUT: u32 = IN | OUT;
const UINT: usize = 4;

/// The requests (`BIOC*`).
pub mod bioc {
    use super::{
        BpfDltlist, BpfProgram, BpfStat, BpfVersion, IN, INOUT, Ifreq, OUT, UINT, VOID, ioc,
    };
    use crate::time::Timeval;

    pub const GBLEN: u32 = ioc(OUT, 102, UINT);
    pub const SBLEN: u32 = ioc(INOUT, 102, UINT);
    pub const SETF: u32 = ioc(IN, 103, size_of::<BpfProgram>());
    pub const FLUSH: u32 = ioc(VOID, 104, 0);
    pub const PROMISC: u32 = ioc(VOID, 105, 0);
    pub const GDLT: u32 = ioc(OUT, 106, UINT);
    pub const GETIF: u32 = ioc(OUT, 107, size_of::<Ifreq>());
    pub const SETIF: u32 = ioc(IN, 108, size_of::<Ifreq>());
    pub const SRTIMEOUT: u32 = ioc(IN, 109, size_of::<Timeval>());
    pub const GRTIMEOUT: u32 = ioc(OUT, 110, size_of::<Timeval>());
    pub const GSTATS: u32 = ioc(OUT, 111, size_of::<BpfStat>());
    pub const IMMEDIATE: u32 = ioc(IN, 112, UINT);
    pub const VERSION: u32 = ioc(OUT, 113, size_of::<BpfVersion>());
    pub const GRSIG: u32 = ioc(OUT, 114, UINT);
    pub const SRSIG: u32 = ioc(IN, 115, UINT);
    pub const GHDRCMPLT: u32 = ioc(OUT, 116, UINT);
    pub const SHDRCMPLT: u32 = ioc(IN, 117, UINT);
    pub const GDIRECTION: u32 = ioc(OUT, 118, UINT);
    pub const SDIRECTION: u32 = ioc(IN, 119, UINT);
    pub const SDLT: u32 = ioc(IN, 120, UINT);
    pub const GDLTLIST: u32 = ioc(INOUT, 121, size_of::<BpfDltlist>());
    pub const LOCK: u32 = ioc(VOID, 122, 0);
    pub const SETWF: u32 = ioc(IN, 123, size_of::<BpfProgram>());
    pub const FEEDBACK: u32 = ioc(IN, 124, UINT);
    pub const GETBUFMODE: u32 = ioc(OUT, 125, UINT);
    pub const SETBUFMODE: u32 = ioc(IN, 126, UINT);
    pub const GETZMAX: u32 = ioc(OUT, 127, 8);
    /// `struct bpf_zbuf` is three words.
    pub const ROTZBUF: u32 = ioc(OUT, 128, 24);
    pub const SETZBUF: u32 = ioc(IN, 129, 24);
    pub const SETFNR: u32 = ioc(IN, 130, size_of::<BpfProgram>());
    pub const GTSTAMP: u32 = ioc(OUT, 131, UINT);
    pub const STSTAMP: u32 = ioc(IN, 132, UINT);
    pub const SETVLANPCP: u32 = ioc(IN, 133, UINT);
}

pub static CONSTANTS: &[ConstValue] = &[
    c("BPF_MAXINSNS", BPF_H, MAXINSNS as i64),
    c("BPF_MEMWORDS", BPF_H, MEMWORDS as i64),
    c("BPF_ALIGNMENT", BPF_H, ALIGNMENT as i64),
    c("BPF_MAJOR_VERSION", BPF_H, MAJOR_VERSION as i64),
    c("BPF_MINOR_VERSION", BPF_H, MINOR_VERSION as i64),
    c("DLT_NULL", BPF_H, dlt::NULL as i64),
    c("DLT_EN10MB", BPF_H, dlt::EN10MB as i64),
    c("BPF_D_IN", BPF_H, dir::IN as i64),
    c("BPF_D_INOUT", BPF_H, dir::INOUT as i64),
    c("BPF_D_OUT", BPF_H, dir::OUT as i64),
    c("BPF_BUFMODE_BUFFER", BPF_H, bufmode::BUFFER as i64),
    c("BPF_BUFMODE_ZBUF", BPF_H, bufmode::ZBUF as i64),
    c("BPF_T_NANOTIME", BPF_H, ts::NANOTIME as i64),
    c("BPF_T_BINTIME", BPF_H, ts::BINTIME as i64),
    c("BPF_T_NONE", BPF_H, ts::NONE as i64),
    c("BPF_T_FAST", BPF_H, ts::FAST as i64),
    c("BPF_T_MONOTONIC", BPF_H, ts::MONOTONIC as i64),
    c("BIOCGBLEN", BPF_H, bioc::GBLEN as i64),
    c("BIOCSBLEN", BPF_H, bioc::SBLEN as i64),
    c("BIOCSETF", BPF_H, bioc::SETF as i64),
    c("BIOCFLUSH", BPF_H, bioc::FLUSH as i64),
    c("BIOCPROMISC", BPF_H, bioc::PROMISC as i64),
    c("BIOCGDLT", BPF_H, bioc::GDLT as i64),
    c("BIOCGETIF", BPF_H, bioc::GETIF as i64),
    c("BIOCSETIF", BPF_H, bioc::SETIF as i64),
    c("BIOCSRTIMEOUT", BPF_H, bioc::SRTIMEOUT as i64),
    c("BIOCGRTIMEOUT", BPF_H, bioc::GRTIMEOUT as i64),
    c("BIOCGSTATS", BPF_H, bioc::GSTATS as i64),
    c("BIOCIMMEDIATE", BPF_H, bioc::IMMEDIATE as i64),
    c("BIOCVERSION", BPF_H, bioc::VERSION as i64),
    c("BIOCGRSIG", BPF_H, bioc::GRSIG as i64),
    c("BIOCSRSIG", BPF_H, bioc::SRSIG as i64),
    c("BIOCGHDRCMPLT", BPF_H, bioc::GHDRCMPLT as i64),
    c("BIOCSHDRCMPLT", BPF_H, bioc::SHDRCMPLT as i64),
    c("BIOCGDIRECTION", BPF_H, bioc::GDIRECTION as i64),
    c("BIOCSDIRECTION", BPF_H, bioc::SDIRECTION as i64),
    c("BIOCSDLT", BPF_H, bioc::SDLT as i64),
    c("BIOCGDLTLIST", BPF_H, bioc::GDLTLIST as i64),
    c("BIOCLOCK", BPF_H, bioc::LOCK as i64),
    c("BIOCSETWF", BPF_H, bioc::SETWF as i64),
    c("BIOCFEEDBACK", BPF_H, bioc::FEEDBACK as i64),
    c("BIOCGETBUFMODE", BPF_H, bioc::GETBUFMODE as i64),
    c("BIOCSETBUFMODE", BPF_H, bioc::SETBUFMODE as i64),
    c("BIOCGETZMAX", BPF_H, bioc::GETZMAX as i64),
    c("BIOCROTZBUF", BPF_H, bioc::ROTZBUF as i64),
    c("BIOCSETZBUF", BPF_H, bioc::SETZBUF as i64),
    c("BIOCSETFNR", BPF_H, bioc::SETFNR as i64),
    c("BIOCGTSTAMP", BPF_H, bioc::GTSTAMP as i64),
    c("BIOCSTSTAMP", BPF_H, bioc::STSTAMP as i64),
    c("BIOCSETVLANPCP", BPF_H, bioc::SETVLANPCP as i64),
];

/// Every constant's header (`net/bpf.h` needs the types and `ifreq`).
const BPF_H: &str = "net/bpf.h";

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}
