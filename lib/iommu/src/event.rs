// SPDX-License-Identifier: BSD-2-Clause
//! Event queue records (Arm IHI 0070 §7.3): what the SMMU reports about
//! traffic it could not handle (docs/IOMMU.md §4.10).
//!
//! Each record is four 64-bit words. Word 0 holds the type (7:0), SSV (11),
//! the SubstreamID (31:12), and the StreamID (63:32). Address faults add
//! in word 1 the stall tag (15:0), Stall (31), PnU (33), InD (34), RnW (35,
//! set for a read), S2 (39), and CLASS (41:40); in word 2 the input
//! address; and in word 3 the IPA (51:12). Fetch aborts carry the fetch
//! address in word 3 (51:3).
//!
//! Behavior reference: sys/arm64/iommu/smmu.c (`smmu_print_event`, the
//! `events` table).

use crate::{Iova, PhysAddr, StreamId, bit, field, lo8, lo16, lo32, put};

/// A record's size in 64-bit words.
pub const EVENT_WORDS: usize = 4;
/// A record's size in bytes.
pub const EVENT_BYTES: u64 = 32;

/// Event type codes.
pub mod code {
    pub const F_UUT: u8 = 0x01;
    pub const C_BAD_STREAMID: u8 = 0x02;
    pub const F_STE_FETCH: u8 = 0x03;
    pub const C_BAD_STE: u8 = 0x04;
    pub const F_BAD_ATS_TREQ: u8 = 0x05;
    pub const F_STREAM_DISABLED: u8 = 0x06;
    pub const F_TRANSL_FORBIDDEN: u8 = 0x07;
    pub const C_BAD_SUBSTREAMID: u8 = 0x08;
    pub const F_CD_FETCH: u8 = 0x09;
    pub const C_BAD_CD: u8 = 0x0a;
    pub const F_WALK_EABT: u8 = 0x0b;
    pub const F_TRANSLATION: u8 = 0x10;
    pub const F_ADDR_SIZE: u8 = 0x11;
    pub const F_ACCESS: u8 = 0x12;
    pub const F_PERMISSION: u8 = 0x13;
    pub const F_TLB_CONFLICT: u8 = 0x20;
    pub const F_CFG_CONFLICT: u8 = 0x21;
    pub const E_PAGE_REQUEST: u8 = 0x24;
    pub const F_VMS_FETCH: u8 = 0x25;
}

/// The access that faulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Access {
    /// RnW: a read (else a write).
    pub read: bool,
    /// InD: an instruction fetch (else data).
    pub instruction: bool,
    /// PnU: privileged (else unprivileged, as PCIe requests are).
    pub privileged: bool,
}

impl Access {
    /// An unprivileged data read.
    pub const READ: Self = Self {
        read: true,
        instruction: false,
        privileged: false,
    };
    /// An unprivileged data write.
    pub const WRITE: Self = Self {
        read: false,
        instruction: false,
        privileged: false,
    };
}

/// Which stage of the translation faulted (CLASS).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultClass {
    /// Fetching the CD.
    Cd,
    /// Walking the translation tables.
    Tt,
    /// The input address itself.
    In,
    /// The reserved encoding.
    Reserved,
}

impl FaultClass {
    const fn bits(self) -> u64 {
        match self {
            Self::Cd => 0,
            Self::Tt => 1,
            Self::In => 2,
            Self::Reserved => 3,
        }
    }

    const fn from_bits(v: u64) -> Self {
        match v & 3 {
            0 => Self::Cd,
            1 => Self::Tt,
            2 => Self::In,
            _ => Self::Reserved,
        }
    }
}

/// An input transaction a record names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input {
    /// The input (bus) address.
    pub addr: Iova,
    /// The access.
    pub access: Access,
}

/// A translation fault's details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    /// The input (bus) address.
    pub addr: Iova,
    /// The access.
    pub access: Access,
    /// The stage of the walk that faulted.
    pub class: FaultClass,
    /// Stage 2 faulted (never here: no stage 2).
    pub s2: bool,
    /// The stall tag when the transaction is stalled (never here).
    pub stall: Option<u16>,
    /// The IPA (stage 2 only; 0 otherwise), page aligned.
    pub ipa: u64,
}

impl Fault {
    /// A stage 1 fault of `access` at `addr` in the table walk.
    #[must_use]
    pub const fn walk(addr: Iova, access: Access) -> Self {
        Self {
            addr,
            access,
            class: FaultClass::Tt,
            s2: false,
            stall: None,
            ipa: 0,
        }
    }
}

/// What a record reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// F_UUT: an unsupported upstream transaction.
    Uut(Input),
    /// C_BAD_STREAMID: the StreamID is out of the stream table's range.
    BadStreamId,
    /// F_STE_FETCH: fetching the STE aborted.
    SteFetch {
        /// The address fetched.
        fetch: PhysAddr,
    },
    /// C_BAD_STE: the STE is invalid or illegal.
    BadSte,
    /// F_BAD_ATS_TREQ: an ATS request the STE disallows.
    BadAtsTreq(Input),
    /// F_STREAM_DISABLED: non-substream traffic disabled.
    StreamDisabled,
    /// F_TRANSL_FORBIDDEN: a translated transaction where bypass is not
    /// allowed.
    TranslForbidden(Input),
    /// C_BAD_SUBSTREAMID.
    BadSubstreamId,
    /// F_CD_FETCH: fetching the CD aborted.
    CdFetch {
        /// The address fetched.
        fetch: PhysAddr,
    },
    /// C_BAD_CD: the CD is invalid or illegal.
    BadCd,
    /// F_WALK_EABT: fetching a translation table descriptor aborted.
    WalkEabt {
        /// The transaction.
        input: Input,
        /// The stage that faulted.
        class: FaultClass,
        /// The descriptor address fetched.
        fetch: PhysAddr,
    },
    /// F_TRANSLATION: no valid translation.
    Translation(Fault),
    /// F_ADDR_SIZE: an address beyond the output size.
    AddrSize(Fault),
    /// F_ACCESS: the access flag is clear.
    Access(Fault),
    /// F_PERMISSION: the access is not permitted.
    Permission(Fault),
    /// F_TLB_CONFLICT.
    TlbConflict(Input),
    /// F_CFG_CONFLICT.
    CfgConflict,
    /// E_PAGE_REQUEST: a speculative page request hint.
    PageRequest(Input),
    /// F_VMS_FETCH.
    VmsFetch {
        /// The address fetched.
        fetch: PhysAddr,
    },
    /// Any other type, kept raw.
    Other {
        /// The type.
        code: u8,
        /// Words 1–3.
        words: [u64; 3],
    },
}

/// An event record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// The stream.
    pub sid: StreamId,
    /// The substream, when the transaction had one (SSV).
    pub ssid: Option<u32>,
    /// What happened.
    pub record: Record,
}

const FETCH_MASK: u64 = ((1 << 52) - 1) & !7;
const IPA_MASK: u64 = ((1 << 52) - 1) & !0xfff;

fn access_bits(a: Access) -> u64 {
    (a.privileged as u64) << 33 | (a.instruction as u64) << 34 | (a.read as u64) << 35
}

fn access_of(w1: u64) -> Access {
    Access {
        read: bit(w1, 35),
        instruction: bit(w1, 34),
        privileged: bit(w1, 33),
    }
}

impl Record {
    /// The type code.
    #[must_use]
    pub const fn code(&self) -> u8 {
        match self {
            Self::Uut(_) => code::F_UUT,
            Self::BadStreamId => code::C_BAD_STREAMID,
            Self::SteFetch { .. } => code::F_STE_FETCH,
            Self::BadSte => code::C_BAD_STE,
            Self::BadAtsTreq(_) => code::F_BAD_ATS_TREQ,
            Self::StreamDisabled => code::F_STREAM_DISABLED,
            Self::TranslForbidden(_) => code::F_TRANSL_FORBIDDEN,
            Self::BadSubstreamId => code::C_BAD_SUBSTREAMID,
            Self::CdFetch { .. } => code::F_CD_FETCH,
            Self::BadCd => code::C_BAD_CD,
            Self::WalkEabt { .. } => code::F_WALK_EABT,
            Self::Translation(_) => code::F_TRANSLATION,
            Self::AddrSize(_) => code::F_ADDR_SIZE,
            Self::Access(_) => code::F_ACCESS,
            Self::Permission(_) => code::F_PERMISSION,
            Self::TlbConflict(_) => code::F_TLB_CONFLICT,
            Self::CfgConflict => code::F_CFG_CONFLICT,
            Self::PageRequest(_) => code::E_PAGE_REQUEST,
            Self::VmsFetch { .. } => code::F_VMS_FETCH,
            Self::Other { code, .. } => *code,
        }
    }

    /// The architecture's name ("F_TRANSLATION"), as FreeBSD prints it.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Uut(_) => "F_UUT",
            Self::BadStreamId => "C_BAD_STREAMID",
            Self::SteFetch { .. } => "F_STE_FETCH",
            Self::BadSte => "C_BAD_STE",
            Self::BadAtsTreq(_) => "F_BAD_ATS_TREQ",
            Self::StreamDisabled => "F_STREAM_DISABLED",
            Self::TranslForbidden(_) => "F_TRANSL_FORBIDDEN",
            Self::BadSubstreamId => "C_BAD_SUBSTREAMID",
            Self::CdFetch { .. } => "F_CD_FETCH",
            Self::BadCd => "C_BAD_CD",
            Self::WalkEabt { .. } => "F_WALK_EABT",
            Self::Translation(_) => "F_TRANSLATION",
            Self::AddrSize(_) => "F_ADDR_SIZE",
            Self::Access(_) => "F_ACCESS",
            Self::Permission(_) => "F_PERMISSION",
            Self::TlbConflict(_) => "F_TLB_CONFLICT",
            Self::CfgConflict => "F_CFG_CONFLICT",
            Self::PageRequest(_) => "E_PAGE_REQUEST",
            Self::VmsFetch { .. } => "F_VMS_FETCH",
            Self::Other { .. } => "unknown",
        }
    }

    /// A short description for the log line (docs/IOMMU.md §4.10:
    /// "translation fault at 0x1234000 (write)").
    #[must_use]
    pub const fn describe(&self) -> &'static str {
        match self {
            Self::Uut(_) => "unsupported transaction",
            Self::BadStreamId => "stream ID out of range",
            Self::SteFetch { .. } => "stream table fetch abort",
            Self::BadSte => "invalid stream table entry",
            Self::BadAtsTreq(_) => "ATS request disallowed",
            Self::StreamDisabled => "stream disabled",
            Self::TranslForbidden(_) => "translated transaction forbidden",
            Self::BadSubstreamId => "invalid substream",
            Self::CdFetch { .. } => "context descriptor fetch abort",
            Self::BadCd => "invalid context descriptor",
            Self::WalkEabt { .. } => "table walk abort",
            Self::Translation(_) => "translation fault",
            Self::AddrSize(_) => "address size fault",
            Self::Access(_) => "access flag fault",
            Self::Permission(_) => "permission fault",
            Self::TlbConflict(_) => "TLB conflict",
            Self::CfgConflict => "configuration cache conflict",
            Self::PageRequest(_) => "page request",
            Self::VmsFetch { .. } => "VMS fetch abort",
            Self::Other { .. } => "event",
        }
    }

    /// The input address and access, when the record names one.
    #[must_use]
    pub const fn input(&self) -> Option<Input> {
        match *self {
            Self::Uut(i)
            | Self::BadAtsTreq(i)
            | Self::TranslForbidden(i)
            | Self::TlbConflict(i)
            | Self::PageRequest(i)
            | Self::WalkEabt { input: i, .. } => Some(i),
            Self::Translation(f) | Self::AddrSize(f) | Self::Access(f) | Self::Permission(f) => {
                Some(Input {
                    addr: f.addr,
                    access: f.access,
                })
            }
            _ => None,
        }
    }
}

impl Event {
    /// Encodes the record.
    #[must_use]
    pub fn encode(&self) -> [u64; EVENT_WORDS] {
        let ssid = match self.ssid {
            Some(s) => 1 << 11 | put(s as u64, 12, 20),
            None => 0,
        };
        let w0 = self.record.code() as u64 | ssid | (self.sid.0 as u64) << 32;
        let input = |i: Input| access_bits(i.access);
        let fault = |f: Fault| {
            let stall = match f.stall {
                Some(tag) => tag as u64 | 1 << 31,
                None => 0,
            };
            [
                w0,
                stall | access_bits(f.access) | (f.s2 as u64) << 39 | put(f.class.bits(), 40, 2),
                f.addr.get(),
                f.ipa & IPA_MASK,
            ]
        };
        match self.record {
            Record::Uut(i)
            | Record::BadAtsTreq(i)
            | Record::TranslForbidden(i)
            | Record::TlbConflict(i)
            | Record::PageRequest(i) => [w0, input(i), i.addr.get(), 0],
            Record::SteFetch { fetch } | Record::CdFetch { fetch } | Record::VmsFetch { fetch } => {
                [w0, 0, 0, fetch.get() & FETCH_MASK]
            }
            Record::WalkEabt {
                input: i,
                class,
                fetch,
            } => [
                w0,
                access_bits(i.access) | put(class.bits(), 40, 2),
                i.addr.get(),
                fetch.get() & FETCH_MASK,
            ],
            Record::Translation(f)
            | Record::AddrSize(f)
            | Record::Access(f)
            | Record::Permission(f) => fault(f),
            Record::Other { words, .. } => [w0, words[0], words[1], words[2]],
            Record::BadStreamId
            | Record::BadSte
            | Record::StreamDisabled
            | Record::BadSubstreamId
            | Record::BadCd
            | Record::CfgConflict => [w0, 0, 0, 0],
        }
    }

    /// Decodes a record.
    #[must_use]
    pub fn decode(w: &[u64; EVENT_WORDS]) -> Self {
        let (w0, w1) = (w[0], w[1]);
        let input = Input {
            addr: Iova::new(w[2]),
            access: access_of(w1),
        };
        let fault = Fault {
            addr: Iova::new(w[2]),
            access: access_of(w1),
            class: FaultClass::from_bits(field(w1, 40, 2)),
            s2: bit(w1, 39),
            stall: if bit(w1, 31) { Some(lo16(w1)) } else { None },
            ipa: w[3] & IPA_MASK,
        };
        let fetch = PhysAddr::new(w[3] & FETCH_MASK);
        let record = match lo8(w0) {
            code::F_UUT => Record::Uut(input),
            code::C_BAD_STREAMID => Record::BadStreamId,
            code::F_STE_FETCH => Record::SteFetch { fetch },
            code::C_BAD_STE => Record::BadSte,
            code::F_BAD_ATS_TREQ => Record::BadAtsTreq(input),
            code::F_STREAM_DISABLED => Record::StreamDisabled,
            code::F_TRANSL_FORBIDDEN => Record::TranslForbidden(input),
            code::C_BAD_SUBSTREAMID => Record::BadSubstreamId,
            code::F_CD_FETCH => Record::CdFetch { fetch },
            code::C_BAD_CD => Record::BadCd,
            code::F_WALK_EABT => Record::WalkEabt {
                input,
                class: FaultClass::from_bits(field(w1, 40, 2)),
                fetch,
            },
            code::F_TRANSLATION => Record::Translation(fault),
            code::F_ADDR_SIZE => Record::AddrSize(fault),
            code::F_ACCESS => Record::Access(fault),
            code::F_PERMISSION => Record::Permission(fault),
            code::F_TLB_CONFLICT => Record::TlbConflict(input),
            code::F_CFG_CONFLICT => Record::CfgConflict,
            code::E_PAGE_REQUEST => Record::PageRequest(input),
            code::F_VMS_FETCH => Record::VmsFetch { fetch },
            c => Record::Other {
                code: c,
                words: [w[1], w[2], w[3]],
            },
        };
        Self {
            sid: StreamId(lo32(w0 >> 32)),
            ssid: if bit(w0, 11) {
                Some(lo32(field(w0, 12, 20)))
            } else {
                None
            },
            record,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn every_record() -> std::vec::Vec<Record> {
        let i = Input {
            addr: Iova::new(0x1234_5000),
            access: Access::WRITE,
        };
        let f = Fault::walk(Iova::new(0xdead_b000), Access::READ);
        let odd = Fault {
            addr: Iova::new(u64::MAX),
            access: Access {
                read: true,
                instruction: true,
                privileged: true,
            },
            class: FaultClass::In,
            s2: true,
            stall: Some(0xabcd),
            ipa: 0x8_0000_1000,
        };
        let fetch = PhysAddr::new(0x8_1234_5678);
        std::vec![
            Record::Uut(i),
            Record::BadStreamId,
            Record::SteFetch { fetch },
            Record::BadSte,
            Record::BadAtsTreq(i),
            Record::StreamDisabled,
            Record::TranslForbidden(i),
            Record::BadSubstreamId,
            Record::CdFetch { fetch },
            Record::BadCd,
            Record::WalkEabt {
                input: i,
                class: FaultClass::Tt,
                fetch,
            },
            Record::Translation(f),
            Record::Translation(odd),
            Record::AddrSize(f),
            Record::Access(f),
            Record::Permission(f),
            Record::Permission(odd),
            Record::TlbConflict(i),
            Record::CfgConflict,
            Record::PageRequest(i),
            Record::VmsFetch { fetch },
            Record::Other {
                code: 0xe3,
                words: [1, 2, 3],
            },
        ]
    }

    #[test]
    fn every_event_round_trips() {
        for (n, record) in every_record().into_iter().enumerate() {
            for ssid in [None, Some(0xf_ffff)] {
                let e = Event {
                    sid: StreamId(0xc100 + u32::try_from(n).unwrap()),
                    ssid,
                    record,
                };
                let w = e.encode();
                assert_eq!(Event::decode(&w), e, "{}", record.name());
                assert_eq!(lo8(w[0]), record.code());
            }
        }
    }

    #[test]
    fn field_positions() {
        // A write translation fault as the model and hardware lay it out.
        let e = Event {
            sid: StreamId(0xc100),
            ssid: None,
            record: Record::Translation(Fault::walk(Iova::new(0x123_4000), Access::WRITE)),
        };
        let w = e.encode();
        assert_eq!(w, [(0xc100 << 32) | 0x10, 1 << 40, 0x123_4000, 0]);
        // As FreeBSD reads it: 32-bit words 1 (SID) and 4/5 (address).
        let words32: std::vec::Vec<u32> =
            w.iter().flat_map(|d| [lo32(*d), lo32(d >> 32)]).collect();
        assert_eq!(words32[0] & 0xff, 0x10);
        assert_eq!(words32[1], 0xc100);
        assert_eq!(
            u64::from(words32[4]) | u64::from(words32[5]) << 32,
            0x123_4000
        );
        // RnW at bit 35 for a read; a fetch address in word 3.
        let r = Event {
            sid: StreamId(1),
            ssid: Some(5),
            record: Record::Permission(Fault::walk(Iova::new(0x1000), Access::READ)),
        };
        assert_eq!(r.encode()[0], (1 << 32) | (5 << 12) | (1 << 11) | 0x13);
        assert_eq!(r.encode()[1], (1 << 35) | (1 << 40));
        let f = Event {
            sid: StreamId(2),
            ssid: None,
            record: Record::SteFetch {
                fetch: PhysAddr::new(0x8000_0040),
            },
        };
        assert_eq!(f.encode(), [(2 << 32) | 0x03, 0, 0, 0x8000_0040]);
        assert_eq!(Record::BadSte.name(), "C_BAD_STE");
        assert_eq!(
            Record::Translation(Fault::walk(Iova::ZERO, Access::READ)).describe(),
            "translation fault"
        );
        assert_eq!(Record::BadCd.input(), None);
        assert_eq!(r.record.input().map(|i| i.addr), Some(Iova::new(0x1000)));
    }
}
