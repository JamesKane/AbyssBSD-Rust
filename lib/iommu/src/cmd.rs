// SPDX-License-Identifier: BSD-2-Clause
//! Command queue entries (Arm IHI 0070 §4): the configuration and TLB
//! invalidations and the sync the driver issues (docs/IOMMU.md §4.7).
//!
//! Each command is two 64-bit words; the opcode is bits 7:0 of the first.
//! Commands the design does not issue (ATS, PRI, stall, stage 2, EL3)
//! decode as [`CmdDecodeError::Unsupported`], and unknown opcodes as
//! [`CmdDecodeError::Illegal`], which an SMMU reports as `CERROR_ILL`.
//!
//! Behavior reference: sys/arm64/iommu/smmu.c (`make_cmd`), smmureg.h.

use crate::{Asid, Iova, PhysAddr, StreamId, bit, field, lo8, lo16, lo32, put};

/// A command's size in 64-bit words.
pub const CMD_WORDS: usize = 2;
/// A command's size in bytes.
pub const CMD_BYTES: u64 = 16;

/// Opcodes.
pub mod op {
    pub const PREFETCH_CONFIG: u8 = 0x01;
    pub const PREFETCH_ADDR: u8 = 0x02;
    pub const CFGI_STE: u8 = 0x03;
    /// With range 31 this is `CMD_CFGI_ALL`.
    pub const CFGI_STE_RANGE: u8 = 0x04;
    pub const CFGI_CD: u8 = 0x05;
    pub const CFGI_CD_ALL: u8 = 0x06;
    pub const CFGI_VMS_PIDM: u8 = 0x07;
    pub const TLBI_NH_ALL: u8 = 0x10;
    pub const TLBI_NH_ASID: u8 = 0x11;
    pub const TLBI_NH_VA: u8 = 0x12;
    pub const TLBI_NH_VAA: u8 = 0x13;
    pub const TLBI_EL3_ALL: u8 = 0x18;
    pub const TLBI_EL3_VA: u8 = 0x1a;
    pub const TLBI_EL2_ALL: u8 = 0x20;
    pub const TLBI_EL2_ASID: u8 = 0x21;
    pub const TLBI_EL2_VA: u8 = 0x22;
    pub const TLBI_EL2_VAA: u8 = 0x23;
    pub const TLBI_S12_VMALL: u8 = 0x28;
    pub const TLBI_S2_IPA: u8 = 0x2a;
    pub const TLBI_NSNH_ALL: u8 = 0x30;
    pub const ATC_INV: u8 = 0x40;
    pub const PRI_RESP: u8 = 0x41;
    pub const RESUME: u8 = 0x44;
    pub const STALL_TERM: u8 = 0x45;
    pub const SYNC: u8 = 0x46;
}

/// `CMD_CFGI_STE_RANGE`'s range for every StreamID: `CMD_CFGI_ALL`.
pub const RANGE_ALL: u8 = 31;

/// A range invalidation's extent (IDR3.RIL): `(num + 1) << scale` pages
/// of the 4 KiB granule from the address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlbiRange {
    /// NUM, 5 bits.
    pub num: u8,
    /// SCALE, 5 bits.
    pub scale: u8,
}

impl TlbiRange {
    /// The pages covered.
    #[must_use]
    pub const fn pages(&self) -> u64 {
        ((self.num as u64 & 0x1f) + 1) << (self.scale & 0x1f)
    }
}

/// An invalidation by address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlbiVa {
    /// The address (bits 63:12 kept).
    pub addr: Iova,
    /// Leaf: only last-level (leaf) entries need invalidating, not cached
    /// table walks.
    pub leaf: bool,
    /// TTL: the level of the leaf, 0 if unknown.
    pub ttl: u8,
    /// A range (TG = 4 KiB), or a single page.
    pub range: Option<TlbiRange>,
}

impl TlbiVa {
    /// One page at `addr`.
    #[must_use]
    pub const fn page(addr: Iova, leaf: bool) -> Self {
        Self {
            addr,
            leaf,
            ttl: 0,
            range: None,
        }
    }

    /// The pages covered.
    #[must_use]
    pub const fn pages(&self) -> u64 {
        match self.range {
            Some(r) => r.pages(),
            None => 1,
        }
    }
}

/// How a `CMD_SYNC` signals completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncCompletion {
    /// Nothing: the driver polls CONS.
    None,
    /// A 32-bit write of `data` to `addr` (CS = SIG_IRQ with an MSI
    /// address; FreeBSD uses it on a coherent SMMU with IDR0.MSI).
    Msi {
        /// The address written, 4-byte aligned.
        addr: PhysAddr,
        /// The value written.
        data: u32,
    },
    /// A WFE wake-up event (CS = SIG_SEV).
    Sev,
}

/// A command the design issues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Fetch `sid`'s configuration into the SMMU's caches.
    PrefetchConfig {
        /// The stream.
        sid: StreamId,
    },
    /// Invalidate `sid`'s cached STE; unless `leaf`, also its level-1
    /// descriptor.
    CfgiSte {
        /// The stream.
        sid: StreamId,
        /// Leaf: the STE only.
        leaf: bool,
    },
    /// Invalidate the STEs of the `2^(range + 1)` StreamIDs around `sid`
    /// (aligned); [`RANGE_ALL`] is `CMD_CFGI_ALL`.
    CfgiSteRange {
        /// A StreamID in the range.
        sid: StreamId,
        /// Range, 5 bits.
        range: u8,
    },
    /// Invalidate the CD for (`sid`, `ssid`).
    CfgiCd {
        /// The stream.
        sid: StreamId,
        /// The substream (0 without substreams).
        ssid: u32,
        /// Leaf: the CD only, not intermediate CD tables.
        leaf: bool,
    },
    /// Invalidate every CD of `sid`.
    CfgiCdAll {
        /// The stream.
        sid: StreamId,
    },
    /// Invalidate every non-secure EL1 TLB entry (VMID 0 without stage 2).
    TlbiNhAll,
    /// Invalidate the TLB entries of one ASID.
    TlbiNhAsid {
        /// The ASID.
        asid: Asid,
    },
    /// Invalidate the entries for an address in one ASID (global entries
    /// included).
    TlbiNhVa {
        /// The ASID.
        asid: Asid,
        /// The address and extent.
        va: TlbiVa,
    },
    /// Invalidate the entries for an address in every ASID.
    TlbiNhVaa {
        /// The address and extent.
        va: TlbiVa,
    },
    /// Invalidate every EL2 TLB entry.
    TlbiEl2All,
    /// Invalidate every non-secure, non-hypervisor TLB entry.
    TlbiNsnhAll,
    /// Complete everything before it, then signal.
    Sync(SyncCompletion),
}

/// Why two words are not a [`Command`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdDecodeError {
    /// An opcode the architecture does not define: `CERROR_ILL`.
    Illegal(u8),
    /// A defined command this design never issues.
    Unsupported(u8),
}

const SSV: u32 = 11;
const SSEC: u32 = 10;
const LEAF: u32 = 0;
const VA_MASK: u64 = !0xfff;

impl Command {
    /// `CMD_CFGI_ALL`.
    #[must_use]
    pub const fn cfgi_all() -> Self {
        Self::CfgiSteRange {
            sid: StreamId(0),
            range: RANGE_ALL,
        }
    }

    /// The opcode.
    #[must_use]
    pub const fn opcode(&self) -> u8 {
        match self {
            Self::PrefetchConfig { .. } => op::PREFETCH_CONFIG,
            Self::CfgiSte { .. } => op::CFGI_STE,
            Self::CfgiSteRange { .. } => op::CFGI_STE_RANGE,
            Self::CfgiCd { .. } => op::CFGI_CD,
            Self::CfgiCdAll { .. } => op::CFGI_CD_ALL,
            Self::TlbiNhAll => op::TLBI_NH_ALL,
            Self::TlbiNhAsid { .. } => op::TLBI_NH_ASID,
            Self::TlbiNhVa { .. } => op::TLBI_NH_VA,
            Self::TlbiNhVaa { .. } => op::TLBI_NH_VAA,
            Self::TlbiEl2All => op::TLBI_EL2_ALL,
            Self::TlbiNsnhAll => op::TLBI_NSNH_ALL,
            Self::Sync(_) => op::SYNC,
        }
    }

    /// The architecture's name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::PrefetchConfig { .. } => "CMD_PREFETCH_CONFIG",
            Self::CfgiSte { .. } => "CMD_CFGI_STE",
            Self::CfgiSteRange {
                range: RANGE_ALL, ..
            } => "CMD_CFGI_ALL",
            Self::CfgiSteRange { .. } => "CMD_CFGI_STE_RANGE",
            Self::CfgiCd { .. } => "CMD_CFGI_CD",
            Self::CfgiCdAll { .. } => "CMD_CFGI_CD_ALL",
            Self::TlbiNhAll => "CMD_TLBI_NH_ALL",
            Self::TlbiNhAsid { .. } => "CMD_TLBI_NH_ASID",
            Self::TlbiNhVa { .. } => "CMD_TLBI_NH_VA",
            Self::TlbiNhVaa { .. } => "CMD_TLBI_NH_VAA",
            Self::TlbiEl2All => "CMD_TLBI_EL2_ALL",
            Self::TlbiNsnhAll => "CMD_TLBI_NSNH_ALL",
            Self::Sync(_) => "CMD_SYNC",
        }
    }

    /// Encodes the command.
    #[must_use]
    pub fn encode(&self) -> [u64; CMD_WORDS] {
        let opc = self.opcode() as u64;
        let sid = |s: StreamId| (s.0 as u64) << 32;
        let va = |v: &TlbiVa| {
            let (num, scale, tg) = match v.range {
                Some(r) => (r.num as u64, r.scale as u64, 1),
                None => (0, 0, 0),
            };
            (
                put(num, 12, 5) | put(scale, 20, 5),
                (v.addr.get() & VA_MASK)
                    | put(tg, 10, 2)
                    | put(v.ttl as u64, 8, 2)
                    | (v.leaf as u64) << LEAF,
            )
        };
        match *self {
            Self::PrefetchConfig { sid: s } | Self::CfgiCdAll { sid: s } => [opc | sid(s), 0],
            Self::CfgiSte { sid: s, leaf } => [opc | sid(s), (leaf as u64) << LEAF],
            Self::CfgiSteRange { sid: s, range } => [opc | sid(s), put(range as u64, 0, 5)],
            Self::CfgiCd { sid: s, ssid, leaf } => [
                opc | sid(s) | put(ssid as u64, 12, 20) | ((ssid != 0) as u64) << SSV,
                (leaf as u64) << LEAF,
            ],
            Self::TlbiNhAll | Self::TlbiEl2All | Self::TlbiNsnhAll => [opc, 0],
            Self::TlbiNhAsid { asid } => [opc | (asid.0 as u64) << 48, 0],
            Self::TlbiNhVa { asid, va: ref v } => {
                let (w0, w1) = va(v);
                [opc | w0 | (asid.0 as u64) << 48, w1]
            }
            Self::TlbiNhVaa { va: ref v } => {
                let (w0, w1) = va(v);
                [opc | w0, w1]
            }
            Self::Sync(c) => {
                // MSH inner shareable and MSIAttr write-back, as FreeBSD
                // writes them.
                let attrs = 3 << 22 | 0xf << 24;
                match c {
                    SyncCompletion::None => [opc | attrs, 0],
                    SyncCompletion::Sev => [opc | attrs | 2 << 12, 0],
                    SyncCompletion::Msi { addr, data } => [
                        opc | attrs | 1 << 12 | (data as u64) << 32,
                        addr.get() & (((1 << 52) - 1) & !3),
                    ],
                }
            }
        }
    }

    /// Decodes a command as the SMMU reads it.
    pub fn decode(w: &[u64; CMD_WORDS]) -> Result<Self, CmdDecodeError> {
        let (w0, w1) = (w[0], w[1]);
        let opc = lo8(w0);
        let sid = StreamId(lo32(w0 >> 32));
        let va = TlbiVa {
            addr: Iova::new(w1 & VA_MASK),
            leaf: bit(w1, LEAF),
            ttl: lo8(field(w1, 8, 2)),
            range: if field(w1, 10, 2) == 0 {
                None
            } else {
                Some(TlbiRange {
                    num: lo8(field(w0, 12, 5)),
                    scale: lo8(field(w0, 20, 5)),
                })
            },
        };
        let asid = Asid(lo16(w0 >> 48));
        // Secure (SSec) forms are not this design's: no secure streams.
        if bit(w0, SSEC)
            && matches!(
                opc,
                op::PREFETCH_CONFIG | op::CFGI_STE | op::CFGI_STE_RANGE | op::CFGI_CD
            )
        {
            return Err(CmdDecodeError::Unsupported(opc));
        }
        Ok(match opc {
            op::PREFETCH_CONFIG => Self::PrefetchConfig { sid },
            op::CFGI_STE => Self::CfgiSte {
                sid,
                leaf: bit(w1, LEAF),
            },
            op::CFGI_STE_RANGE => Self::CfgiSteRange {
                sid,
                range: lo8(field(w1, 0, 5)),
            },
            op::CFGI_CD => Self::CfgiCd {
                sid,
                ssid: if bit(w0, SSV) {
                    lo32(field(w0, 12, 20))
                } else {
                    0
                },
                leaf: bit(w1, LEAF),
            },
            op::CFGI_CD_ALL => Self::CfgiCdAll { sid },
            op::TLBI_NH_ALL => Self::TlbiNhAll,
            op::TLBI_NH_ASID => Self::TlbiNhAsid { asid },
            op::TLBI_NH_VA => Self::TlbiNhVa { asid, va },
            op::TLBI_NH_VAA => Self::TlbiNhVaa { va },
            op::TLBI_EL2_ALL => Self::TlbiEl2All,
            op::TLBI_NSNH_ALL => Self::TlbiNsnhAll,
            op::SYNC => Self::Sync(match field(w0, 12, 2) {
                0 => SyncCompletion::None,
                1 => SyncCompletion::Msi {
                    addr: PhysAddr::new(w1 & (((1 << 52) - 1) & !3)),
                    data: lo32(w0 >> 32),
                },
                2 => SyncCompletion::Sev,
                _ => return Err(CmdDecodeError::Illegal(opc)),
            }),
            op::PREFETCH_ADDR
            | op::CFGI_VMS_PIDM
            | op::TLBI_EL3_ALL
            | op::TLBI_EL3_VA
            | op::TLBI_EL2_ASID
            | op::TLBI_EL2_VA
            | op::TLBI_EL2_VAA
            | op::TLBI_S12_VMALL
            | op::TLBI_S2_IPA
            | op::ATC_INV
            | op::PRI_RESP
            | op::RESUME
            | op::STALL_TERM => return Err(CmdDecodeError::Unsupported(opc)),
            x => return Err(CmdDecodeError::Illegal(x)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> [Command; 16] {
        let va = TlbiVa::page(Iova::new(0x1234_5678_9000), true);
        [
            Command::PrefetchConfig {
                sid: StreamId(0xc100),
            },
            Command::CfgiSte {
                sid: StreamId(0xffff_ffff),
                leaf: true,
            },
            Command::CfgiSte {
                sid: StreamId(7),
                leaf: false,
            },
            Command::cfgi_all(),
            Command::CfgiSteRange {
                sid: StreamId(0x100),
                range: 7,
            },
            Command::CfgiCd {
                sid: StreamId(3),
                ssid: 0,
                leaf: true,
            },
            Command::CfgiCdAll { sid: StreamId(9) },
            Command::TlbiNhAll,
            Command::TlbiNhAsid { asid: Asid(0xbeef) },
            Command::TlbiNhVa {
                asid: Asid(0xffff),
                va,
            },
            Command::TlbiNhVa {
                asid: Asid(1),
                va: TlbiVa {
                    addr: Iova::new(0x20_0000),
                    leaf: false,
                    ttl: 2,
                    range: Some(TlbiRange { num: 31, scale: 3 }),
                },
            },
            Command::TlbiNhVaa { va },
            Command::TlbiEl2All,
            Command::TlbiNsnhAll,
            Command::Sync(SyncCompletion::Sev),
            Command::Sync(SyncCompletion::Msi {
                addr: PhysAddr::new(0x8_dead_bee0),
                data: 0xcafe_f00d,
            }),
        ]
    }

    #[test]
    fn every_command_round_trips() {
        for c in all() {
            let w = c.encode();
            assert_eq!(Command::decode(&w), Ok(c), "{}", c.name());
            assert_eq!(lo8(w[0]), c.opcode());
        }
        let none = Command::Sync(SyncCompletion::None);
        assert_eq!(Command::decode(&none.encode()), Ok(none));
    }

    #[test]
    fn field_positions() {
        // Against IHI 0070 §4 and FreeBSD's make_cmd.
        assert_eq!(
            Command::CfgiSte {
                sid: StreamId(0xc100),
                leaf: true
            }
            .encode(),
            [0xc100_0000_0003, 1]
        );
        assert_eq!(Command::cfgi_all().encode(), [0x04, 31]);
        assert_eq!(
            Command::CfgiCd {
                sid: StreamId(5),
                ssid: 0x3,
                leaf: true
            }
            .encode(),
            [(5 << 32) | (3 << 12) | (1 << 11) | 0x05, 1]
        );
        assert_eq!(
            Command::TlbiNhVa {
                asid: Asid(0x42),
                va: TlbiVa::page(Iova::new(0x8000_1000), true),
            }
            .encode(),
            [(0x42 << 48) | 0x12, 0x8000_1000 | 1]
        );
        assert_eq!(
            Command::TlbiNhAsid { asid: Asid(0x42) }.encode(),
            [(0x42 << 48) | 0x11, 0]
        );
        // SYNC: CS at 13:12, MSH 23:22 inner, MSIAttr 27:24 WB, MSIData
        // 63:32, MSIAddress 51:2.
        assert_eq!(
            Command::Sync(SyncCompletion::Msi {
                addr: PhysAddr::new(0x1000),
                data: 7
            })
            .encode(),
            [
                (7 << 32) | (0xf << 24) | (3 << 22) | (1 << 12) | 0x46,
                0x1000
            ]
        );
        assert_eq!(
            Command::Sync(SyncCompletion::Sev).encode(),
            [(0xf << 24) | (3 << 22) | (2 << 12) | 0x46, 0]
        );
        // A range: NUM 16:12, SCALE 24:20, TG 11:10 = 4 KiB, TTL 9:8.
        let r = Command::TlbiNhVa {
            asid: Asid(0),
            va: TlbiVa {
                addr: Iova::new(0x4000),
                leaf: true,
                ttl: 3,
                range: Some(TlbiRange { num: 1, scale: 2 }),
            },
        };
        assert_eq!(
            r.encode(),
            [
                (1 << 12) | (2 << 20) | 0x12,
                0x4000 | (1 << 10) | (3 << 8) | 1
            ]
        );
        assert_eq!(TlbiRange { num: 1, scale: 2 }.pages(), 8);
        assert_eq!(TlbiRange { num: 31, scale: 31 }.pages(), 32 << 31);
    }

    #[test]
    fn bad_commands() {
        assert_eq!(Command::decode(&[0, 0]), Err(CmdDecodeError::Illegal(0)));
        assert_eq!(
            Command::decode(&[0xff, 0]),
            Err(CmdDecodeError::Illegal(0xff))
        );
        assert_eq!(Command::decode(&[0x09, 0]), Err(CmdDecodeError::Illegal(9)));
        for opc in [
            op::ATC_INV,
            op::PRI_RESP,
            op::RESUME,
            op::STALL_TERM,
            op::TLBI_S2_IPA,
        ] {
            assert_eq!(
                Command::decode(&[u64::from(opc), 0]),
                Err(CmdDecodeError::Unsupported(opc))
            );
        }
        // CS = 0b11 is reserved.
        assert_eq!(
            Command::decode(&[0x46 | (3 << 12), 0]),
            Err(CmdDecodeError::Illegal(0x46))
        );
        // Secure forms.
        assert_eq!(
            Command::decode(&[0x03 | (1 << 10), 0]),
            Err(CmdDecodeError::Unsupported(3))
        );
        assert_eq!(Command::cfgi_all().name(), "CMD_CFGI_ALL");
    }
}
