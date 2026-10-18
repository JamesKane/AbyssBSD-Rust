// SPDX-License-Identifier: BSD-2-Clause
//! Generic Timer Description Table (ACPI 6.5 §5.2.25): the Arm generic timer
//! interrupts, memory-mapped timer blocks, and SBSA watchdogs.
//!
//! Behavior reference: `sys/arm/arm/generic_timer.c`.

use crate::raw::{Entries, Fields, LenField, Raw};
use crate::{Error, HEADER_LEN, Sdt};

const CNT_CONTROL_BASE: usize = HEADER_LEN;
const SECURE_EL1: usize = 48;
const NONSECURE_EL1: usize = 56;
const VIRTUAL_EL1: usize = 64;
const NONSECURE_EL2: usize = 72;
const CNT_READ_BASE: usize = 80;
const PLATFORM_COUNT: usize = 88;
const PLATFORM_OFFSET: usize = 92;
const VIRTUAL_EL2: usize = 96;
/// The ACPI 5.0 table: through the non-secure EL2 timer.
const MIN_LEN: usize = 80;

const GT_FRAME_LEN: usize = 40;

const SHORT: Error = Error::Malformed("GTDT platform timer too short");
const WALK: Error = Error::Malformed("GTDT platform timer length");

/// A parsed GTDT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gtdt<'a> {
    bytes: &'a [u8],
    revision: u8,
}

/// Parses a GTDT. The ACPI 5.0 layout (80 bytes) is the minimum.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table, [`Error::BadLength`] if it is
/// shorter than the ACPI 5.0 layout.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Gtdt<'a>, Error> {
    if t.signature() != *b"GTDT" {
        return Err(Error::BadSignature);
    }
    if t.bytes.len() < MIN_LEN {
        return Err(Error::BadLength(*b"GTDT"));
    }
    Ok(Gtdt {
        bytes: t.bytes,
        revision: t.revision(),
    })
}

/// Generic timer interrupt flags (ACPI 6.5 Table 5.125).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerFlags(pub u32);

impl TimerFlags {
    /// The interrupt is edge triggered (else level).
    #[must_use]
    pub const fn edge_triggered(self) -> bool {
        self.0 & 1 != 0
    }

    /// The interrupt is active low (else active high).
    #[must_use]
    pub const fn active_low(self) -> bool {
        self.0 & 2 != 0
    }

    /// The timer keeps counting in all power states. Only defined for the
    /// per-processor timers.
    #[must_use]
    pub const fn always_on(self) -> bool {
        self.0 & 4 != 0
    }
}

/// A timer interrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerInterrupt {
    /// The GSIV (a PPI for per-processor timers).
    pub gsiv: u32,
    /// Trigger mode, polarity, and always-on.
    pub flags: TimerFlags,
}

impl<'a> Gtdt<'a> {
    fn f(&self) -> Fields<'a> {
        Fields::new(self.bytes, Error::BadLength(*b"GTDT"))
    }

    /// An address field where all-ones (and 0) mean "not provided".
    fn address(&self, off: usize) -> Option<u64> {
        self.f().u64(off).ok().filter(|&a| a != 0 && a != u64::MAX)
    }

    /// A GSIV and flags pair, `None` if the GSIV is 0 (not provided) or the
    /// table is too short.
    fn timer(&self, off: usize) -> Option<TimerInterrupt> {
        let gsiv = self.f().u32(off).ok().filter(|&g| g != 0)?;
        Some(TimerInterrupt {
            gsiv,
            flags: TimerFlags(self.f().u32(off + 4).ok()?),
        })
    }

    /// The physical address of the `CNTControlBase` frame, if provided.
    #[must_use]
    pub fn cnt_control_base(&self) -> Option<u64> {
        self.address(CNT_CONTROL_BASE)
    }

    /// The physical address of the `CNTReadBase` frame, if provided.
    #[must_use]
    pub fn cnt_read_base(&self) -> Option<u64> {
        self.address(CNT_READ_BASE)
    }

    /// The secure EL1 physical timer.
    #[must_use]
    pub fn secure_el1(&self) -> Option<TimerInterrupt> {
        self.timer(SECURE_EL1)
    }

    /// The non-secure EL1 physical timer.
    #[must_use]
    pub fn nonsecure_el1(&self) -> Option<TimerInterrupt> {
        self.timer(NONSECURE_EL1)
    }

    /// The EL1 virtual timer.
    #[must_use]
    pub fn virtual_el1(&self) -> Option<TimerInterrupt> {
        self.timer(VIRTUAL_EL1)
    }

    /// The non-secure EL2 physical timer.
    #[must_use]
    pub fn nonsecure_el2(&self) -> Option<TimerInterrupt> {
        self.timer(NONSECURE_EL2)
    }

    /// The EL2 virtual timer (revision 3, ACPI 6.3). Earlier revisions may
    /// place platform timers where this field would be, so it is read only
    /// from revision 3 tables whose platform timers do not overlap it.
    #[must_use]
    pub fn virtual_el2(&self) -> Option<TimerInterrupt> {
        let f = self.f();
        let overlaps = match (f.u32(PLATFORM_COUNT), f.u32(PLATFORM_OFFSET)) {
            (Ok(0), _) => false,
            (Ok(_), Ok(off)) => usize::try_from(off).map_or(true, |o| o < VIRTUAL_EL2 + 8),
            _ => true,
        };
        if self.revision < 3 || overlaps {
            return None;
        }
        self.timer(VIRTUAL_EL2)
    }

    /// The memory-mapped platform timers. Absent in ACPI 5.0 tables. A
    /// malformed structure yields one error and ends the walk.
    #[must_use]
    pub fn platform_timers(&self) -> PlatformTimers<'a> {
        let f = self.f();
        let walk = match (f.u32(PLATFORM_COUNT), f.u32(PLATFORM_OFFSET)) {
            (Err(_) | Ok(0), _) => Entries::new(&[], 0, Some(0), LenField::U16, WALK),
            (Ok(count), Ok(off)) => match usize::try_from(off) {
                Ok(off) if off >= PLATFORM_OFFSET + 4 => {
                    Entries::new(self.bytes, off, Some(count), LenField::U16, WALK)
                }
                _ => Entries::failed(WALK),
            },
            (Ok(_), Err(e)) => Entries::failed(e),
        };
        PlatformTimers(walk)
    }
}

/// Iterator returned by [`Gtdt::platform_timers`].
#[derive(Clone, Debug)]
pub struct PlatformTimers<'a>(Entries<'a>);

impl<'a> Iterator for PlatformTimers<'a> {
    type Item = Result<PlatformTimer<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.0.next()?.and_then(decode))
    }
}

/// A platform timer structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformTimer<'a> {
    /// A memory-mapped generic timer block (type 0).
    GtBlock(GtBlock<'a>),
    /// An SBSA generic watchdog (type 1).
    Watchdog(Watchdog),
    /// A structure this parser does not decode.
    Other {
        /// The structure type.
        kind: u8,
        /// The whole structure, header included.
        bytes: &'a [u8],
    },
}

/// A GT block: a `CNTCTLBase` frame and up to eight timer frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GtBlock<'a> {
    /// Physical address of the `CNTCTLBase` frame.
    pub cnt_ctl_base: u64,
    frames: &'a [u8],
}

impl<'a> GtBlock<'a> {
    /// The timer frames, validated to lie inside the block structure.
    pub fn frames(&self) -> impl Iterator<Item = GtFrame> + 'a {
        self.frames
            .as_chunks::<GT_FRAME_LEN>()
            .0
            .iter()
            .filter_map(|c| frame(c))
    }
}

/// Memory-mapped timer flags (ACPI 6.5 Table 5.129).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GtFrameFlags(pub u32);

impl GtFrameFlags {
    /// The interrupt is edge triggered (else level).
    #[must_use]
    pub const fn edge_triggered(self) -> bool {
        self.0 & 1 != 0
    }

    /// The interrupt is active low (else active high).
    #[must_use]
    pub const fn active_low(self) -> bool {
        self.0 & 2 != 0
    }
}

/// One GT block timer frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GtFrame {
    /// The frame number (0–7).
    pub frame_number: u8,
    /// Physical address of the `CNTBaseN` frame.
    pub cnt_base: u64,
    /// Physical address of the `CNTEL0BaseN` frame, or all-ones if absent.
    pub cnt_el0_base: u64,
    /// The physical timer interrupt.
    pub physical_gsiv: u32,
    /// The physical timer's trigger mode and polarity.
    pub physical_flags: GtFrameFlags,
    /// The virtual timer interrupt (0 if not implemented).
    pub virtual_gsiv: u32,
    /// The virtual timer's trigger mode and polarity.
    pub virtual_flags: GtFrameFlags,
    /// The frame is secure.
    pub secure: bool,
    /// The frame keeps counting in all power states.
    pub always_on: bool,
}

/// An SBSA generic watchdog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watchdog {
    /// Physical address of the refresh frame.
    pub refresh_frame: u64,
    /// Physical address of the control frame.
    pub control_frame: u64,
    /// The watchdog interrupt.
    pub gsiv: u32,
    /// The interrupt is edge triggered (else level).
    pub edge_triggered: bool,
    /// The interrupt is active low (else active high).
    pub active_low: bool,
    /// The watchdog is secure.
    pub secure: bool,
}

fn decode(raw: Raw<'_>) -> Result<PlatformTimer<'_>, Error> {
    let f = Fields::new(raw.bytes, SHORT);
    Ok(match raw.kind {
        0 => {
            let cnt_ctl_base = f.u64(4)?;
            let count = usize::try_from(f.u32(12)?).map_err(|_| SHORT)?;
            let off = usize::try_from(f.u32(16)?).map_err(|_| SHORT)?;
            let frames = count
                .checked_mul(GT_FRAME_LEN)
                .and_then(|n| raw.bytes.get(off..)?.get(..n))
                .ok_or(SHORT)?;
            PlatformTimer::GtBlock(GtBlock {
                cnt_ctl_base,
                frames,
            })
        }
        1 => {
            let flags = f.u32(24)?;
            PlatformTimer::Watchdog(Watchdog {
                refresh_frame: f.u64(4)?,
                control_frame: f.u64(12)?,
                gsiv: f.u32(20)?,
                edge_triggered: flags & 1 != 0,
                active_low: flags & 2 != 0,
                secure: flags & 4 != 0,
            })
        }
        kind => PlatformTimer::Other {
            kind,
            bytes: raw.bytes,
        },
    })
}

fn frame(b: &[u8]) -> Option<GtFrame> {
    let f = Fields::new(b, SHORT);
    let common = f.u32(36).ok()?;
    Some(GtFrame {
        frame_number: f.u8(0).ok()?,
        cnt_base: f.u64(4).ok()?,
        cnt_el0_base: f.u64(12).ok()?,
        physical_gsiv: f.u32(20).ok()?,
        physical_flags: GtFrameFlags(f.u32(24).ok()?),
        virtual_gsiv: f.u32(28).ok()?,
        virtual_flags: GtFrameFlags(f.u32(32).ok()?),
        secure: common & 1 != 0,
        always_on: common & 2 != 0,
    })
}
