// SPDX-License-Identifier: BSD-2-Clause
//! `XPT_DEV_MATCH`: the table's buses, devices, and peripherals that
//! match the caller's patterns, in table order, a buffer at a time.
//!
//! - With no patterns everything matches. A bus or device pattern walks
//!   the table (buses, then each bus's targets and devices, then each
//!   device's peripherals); peripheral patterns alone walk the
//!   peripheral drivers' lists instead.
//! - Each node is matched as `xptbusmatch`, `xptdevicematch`, and
//!   `xptperiphmatch` do: whether to copy it out, and whether to look
//!   below it.
//! - When the buffer is full the walk stops and records where (a cookie
//!   per level and the generations of the lists it was in); the next call
//!   resumes there, or reports `CAM_DEV_MATCH_LIST_CHANGED` if a list
//!   changed meanwhile.
//!
//! Behavior reference: sys/cam/cam_xpt.c (`xptedtmatch`,
//! `xptperiphlistmatch`, and their traversal functions), sys/cam/cam.c
//! (`cam_strmatch`), sys/cam/scsi/scsi_all.c
//! (`scsi_static_inquiry_match`, `scsi_devid_match`).

use alloc::vec::Vec;
use core::mem::offset_of;

use abi::cam::{
    BusMatchPattern, BusMatchResult, DEV_RESULT_UNCONFIGURED, DevMatchPattern, DevMatchResult,
    DeviceMatchPattern, DeviceMatchResult, PeriphMatchPattern, PeriphMatchResult,
    ScsiStaticInquiryPattern, bus_match, dev_match, dev_pos, dm_status, match_type, periph_match,
};

use crate::ccb::{Position, cstr};
use crate::edt::{BusNode, DeviceNode, Edt, Periph};

/// One pattern.
#[allow(clippy::large_enum_variant, reason = "a few per request, never stored")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// `DEV_MATCH_PERIPH`.
    Periph {
        name: [u8; 16],
        unit: u32,
        path_id: u32,
        target_id: u32,
        lun: u64,
        flags: u32,
    },
    /// `DEV_MATCH_DEVICE`.
    Device {
        path_id: u32,
        target_id: u32,
        lun: u64,
        flags: u32,
        data: [u8; 257],
    },
    /// `DEV_MATCH_BUS`.
    Bus {
        path_id: u32,
        name: [u8; 16],
        unit: u32,
        bus_id: u32,
        flags: u32,
    },
    /// Any other type: matches nothing, and stops nothing.
    Other,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[o..o + 4]);
    u32::from_ne_bytes(a)
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_ne_bytes(a)
}

/// The size of one pattern.
pub const PATTERN_SIZE: usize = size_of::<DevMatchPattern>();
/// The size of one result.
pub const RESULT_SIZE: usize = size_of::<DevMatchResult>();

impl Pattern {
    /// Parses `struct dev_match_pattern` (exactly [`PATTERN_SIZE`] bytes).
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() != PATTERN_SIZE {
            return None;
        }
        let p = &b[offset_of!(DevMatchPattern, pattern)..];
        Some(match u32_at(b, offset_of!(DevMatchPattern, type_)) {
            match_type::PERIPH => {
                type P = PeriphMatchPattern;
                let mut name = [0u8; 16];
                name.copy_from_slice(&p[offset_of!(P, periph_name)..][..16]);
                Self::Periph {
                    name,
                    unit: u32_at(p, offset_of!(P, unit_number)),
                    path_id: u32_at(p, offset_of!(P, path_id)),
                    target_id: u32_at(p, offset_of!(P, target_id)),
                    lun: u64_at(p, offset_of!(P, target_lun)),
                    flags: u32_at(p, offset_of!(P, flags)),
                }
            }
            match_type::DEVICE => {
                type D = DeviceMatchPattern;
                let mut data = [0u8; 257];
                data.copy_from_slice(&p[offset_of!(D, data)..][..257]);
                Self::Device {
                    path_id: u32_at(p, offset_of!(D, path_id)),
                    target_id: u32_at(p, offset_of!(D, target_id)),
                    lun: u64_at(p, offset_of!(D, target_lun)),
                    flags: u32_at(p, offset_of!(D, flags)),
                    data,
                }
            }
            match_type::BUS => {
                type B = BusMatchPattern;
                let mut name = [0u8; 16];
                name.copy_from_slice(&p[offset_of!(B, dev_name)..][..16]);
                Self::Bus {
                    path_id: u32_at(p, offset_of!(B, path_id)),
                    name,
                    unit: u32_at(p, offset_of!(B, unit_number)),
                    bus_id: u32_at(p, offset_of!(B, bus_id)),
                    flags: u32_at(p, offset_of!(B, flags)),
                }
            }
            _ => Self::Other,
        })
    }
}

/// One result.
#[allow(clippy::large_enum_variant, reason = "a buffer's worth per request")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchResult {
    /// `DEV_MATCH_BUS`.
    Bus {
        path_id: u32,
        name: &'static str,
        unit: u32,
        bus_id: u32,
    },
    /// `DEV_MATCH_DEVICE`.
    Device {
        path_id: u32,
        target_id: u32,
        lun: u64,
        protocol: u32,
        inq: [u8; 256],
        ident: [u8; 512],
        unconfigured: bool,
    },
    /// `DEV_MATCH_PERIPH`.
    Periph {
        name: &'static str,
        unit: u32,
        path_id: u32,
        target_id: u32,
        lun: u64,
    },
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_ne_bytes());
}

fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_ne_bytes());
}

fn put_name(b: &mut [u8], o: usize, name: &str) {
    let n = name.len().min(15);
    b[o..o + 16].fill(0);
    b[o..o + n].copy_from_slice(&name.as_bytes()[..n]);
}

impl MatchResult {
    /// As `struct dev_match_result`.
    #[must_use]
    pub fn encode(&self) -> [u8; RESULT_SIZE] {
        let mut b = [0u8; RESULT_SIZE];
        let r = offset_of!(DevMatchResult, result);
        let t = offset_of!(DevMatchResult, type_);
        match self {
            Self::Bus {
                path_id,
                name,
                unit,
                bus_id,
            } => {
                type B = BusMatchResult;
                put32(&mut b, t, match_type::BUS);
                put32(&mut b, r + offset_of!(B, path_id), *path_id);
                put_name(&mut b, r + offset_of!(B, dev_name), name);
                put32(&mut b, r + offset_of!(B, unit_number), *unit);
                put32(&mut b, r + offset_of!(B, bus_id), *bus_id);
            }
            Self::Device {
                path_id,
                target_id,
                lun,
                protocol,
                inq,
                ident,
                unconfigured,
            } => {
                type D = DeviceMatchResult;
                put32(&mut b, t, match_type::DEVICE);
                put32(&mut b, r + offset_of!(D, path_id), *path_id);
                put32(&mut b, r + offset_of!(D, target_id), *target_id);
                put64(&mut b, r + offset_of!(D, target_lun), *lun);
                put32(&mut b, r + offset_of!(D, protocol), *protocol);
                b[r + offset_of!(D, inq_data)..][..256].copy_from_slice(inq);
                b[r + offset_of!(D, ident_data)..][..512].copy_from_slice(ident);
                let f = if *unconfigured {
                    DEV_RESULT_UNCONFIGURED
                } else {
                    0
                };
                put32(&mut b, r + offset_of!(D, flags), f);
            }
            Self::Periph {
                name,
                unit,
                path_id,
                target_id,
                lun,
            } => {
                type P = PeriphMatchResult;
                put32(&mut b, t, match_type::PERIPH);
                put_name(&mut b, r + offset_of!(P, periph_name), name);
                put32(&mut b, r + offset_of!(P, unit_number), *unit);
                put32(&mut b, r + offset_of!(P, path_id), *path_id);
                put32(&mut b, r + offset_of!(P, target_id), *target_id);
                put64(&mut b, r + offset_of!(P, target_lun), *lun);
            }
        }
        b
    }
}

/// What a node's match says to do (`dev_match_ret`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    None,
    Stop,
    Descend,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ret {
    copy: bool,
    action: Action,
}

impl Ret {
    const ALL: Self = Self {
        copy: true,
        action: Action::Descend,
    };
}

/// `xptbusmatch`.
fn bus_match(patterns: &[Pattern], bus: &BusNode) -> Ret {
    if patterns.is_empty() {
        return Ret::ALL;
    }
    let mut r = Ret {
        copy: false,
        action: Action::None,
    };
    for p in patterns {
        match *p {
            Pattern::Device { path_id, flags, .. }
                if flags & dev_match::PATH != 0 && path_id != bus.path_id => {}
            Pattern::Periph { path_id, flags, .. }
                if flags & periph_match::PATH != 0 && path_id != bus.path_id => {}
            Pattern::Bus {
                path_id,
                ref name,
                unit,
                bus_id,
                flags,
            } => {
                if (flags & bus_match::PATH != 0 && path_id != bus.path_id)
                    || (flags & bus_match::BUS_ID != 0 && bus_id != bus.sim.bus_id)
                    || (flags & bus_match::UNIT != 0 && unit != bus.sim.unit)
                    || (flags & bus_match::NAME != 0 && cstr(name) != bus.sim.name.as_bytes())
                {
                    continue;
                }
                r.copy = true;
                if r.action == Action::Descend {
                    return r;
                }
            }
            _ => {
                if r.action == Action::None {
                    r.action = Action::Descend;
                }
            }
        }
    }
    if r.action == Action::None {
        r.action = Action::Stop;
    }
    r
}

/// `cam_strmatch`: `pattern` (NUL-terminated) against the `str_len`
/// bytes of `s`; 0 for a match.
fn strmatch(s: &[u8], pattern: &[u8]) -> bool {
    let at = |p: &[u8], i: usize| p.get(i).copied().unwrap_or(0);
    let (mut si, mut pi) = (0usize, 0usize);
    while at(pattern, pi) != 0 && si < s.len() {
        match at(pattern, pi) {
            b'*' => {
                pi += 1;
                if at(pattern, pi) == 0 {
                    return true;
                }
                while si < s.len() {
                    if strmatch(&s[si..], &pattern[pi..]) {
                        return true;
                    }
                    si += 1;
                }
                return false;
            }
            b'[' => {
                let sc = s[si];
                si += 1;
                pi += 1;
                let negate = at(pattern, pi) == b'^';
                if negate {
                    pi += 1;
                }
                let (mut ok, mut pc) = (false, u8::MAX);
                while at(pattern, pi) != b']' && at(pattern, pi) != 0 {
                    if at(pattern, pi) == b'-' {
                        if at(pattern, pi + 1) == 0 {
                            return false;
                        }
                        if sc >= pc && sc <= at(pattern, pi + 1) {
                            ok = true;
                        }
                        pi += 1;
                    } else if at(pattern, pi) == sc {
                        ok = true;
                    }
                    pc = at(pattern, pi);
                    pi += 1;
                }
                if ok == negate {
                    return false;
                }
                pi += 1;
            }
            b'?' => {
                pi += 1;
                si += 1;
            }
            c => {
                if s[si] != c {
                    return false;
                }
                pi += 1;
                si += 1;
            }
        }
    }
    while at(pattern, pi) == b'*' {
        pi += 1;
    }
    if at(pattern, pi) != 0 {
        return false;
    }
    // Trailing spaces, which SAT adds, are eaten.
    while si < s.len() && s[si] == b' ' {
        si += 1;
    }
    si >= s.len() || s[si] == 0
}

/// `scsi_static_inquiry_match`.
fn inquiry_match(inq: &[u8; 256], data: &[u8; 257]) -> bool {
    type I = ScsiStaticInquiryPattern;
    let ty = data[offset_of!(I, type_)];
    let media = data[offset_of!(I, media_type)];
    let removable = inq[1] & 0x80 != 0;
    (ty == 0xff || ty == inq[0] & 0x1f)
        && (if removable {
            media & 0x01
        } else {
            media & 0x02
        }) != 0
        && strmatch(&inq[8..16], &data[offset_of!(I, vendor)..][..9])
        && strmatch(&inq[16..32], &data[offset_of!(I, product)..][..17])
        && strmatch(&inq[32..36], &data[offset_of!(I, revision)..][..5])
}

/// The descriptors of a device identification list (four-byte headers,
/// the identifier's length in the last).
fn descriptors(mut b: &[u8]) -> impl Iterator<Item = &[u8]> {
    core::iter::from_fn(move || {
        let len = 4 + usize::from(*b.get(3)?);
        let d = b.get(..len)?;
        b = &b[len..];
        Some(d)
    })
}

/// `scsi_devid_match`: whether a descriptor of the device's VPD page
/// equals one of the pattern's.
fn devid_match(page: &[u8], id: &[u8]) -> bool {
    // The page's four-byte header, then its descriptors.
    let Some(list) = page.get(4..) else {
        return false;
    };
    descriptors(list).any(|l| descriptors(id).any(|r| l == r))
}

/// `xptdevicematch`.
fn device_match(patterns: &[Pattern], path_id: u32, target: u32, dev: &DeviceNode) -> Ret {
    if patterns.is_empty() {
        return Ret::ALL;
    }
    let mut r = Ret {
        copy: false,
        action: Action::None,
    };
    let zeros = [0u8; 256];
    let inq = dev.info.as_ref().map_or(&zeros, |i| &*i.inq_data);
    let devid: &[u8] = dev.info.as_ref().map_or(&[], |i| &i.devid);
    for p in patterns {
        match *p {
            Pattern::Periph {
                target_id, flags, ..
            } if flags & periph_match::TARGET != 0 && target_id != target => {}
            Pattern::Periph { lun, flags, .. }
                if flags & periph_match::LUN != 0 && lun != dev.lun => {}
            Pattern::Periph { .. } => {
                if r.action == Action::None {
                    r.action = Action::Descend;
                }
            }
            Pattern::Device {
                path_id: pp,
                target_id,
                lun,
                flags,
                ref data,
            } => {
                if flags & (dev_match::INQUIRY | dev_match::DEVID)
                    == (dev_match::INQUIRY | dev_match::DEVID)
                {
                    return Ret {
                        copy: false,
                        action: Action::Error,
                    };
                }
                if (flags & dev_match::PATH != 0 && pp != path_id)
                    || (flags & dev_match::TARGET != 0 && target_id != target)
                    || (flags & dev_match::LUN != 0 && lun != dev.lun)
                {
                    continue;
                }
                let inq_pat: &[u8; 257] = data;
                if flags & dev_match::INQUIRY != 0 {
                    let mut i = [0u8; 256];
                    i.copy_from_slice(inq);
                    if !inquiry_match(&i, inq_pat) {
                        continue;
                    }
                }
                if flags & dev_match::DEVID != 0 {
                    let len = usize::from(data[0]);
                    if devid.len() < 4 || !devid_match(devid, &data[1..1 + len.min(256)]) {
                        continue;
                    }
                }
                r.copy = true;
                if r.action == Action::Descend {
                    return r;
                }
            }
            _ => {}
        }
    }
    if r.action == Action::None {
        r.action = Action::Stop;
    }
    r
}

/// `xptperiphmatch`.
fn periph_match(patterns: &[Pattern], path_id: u32, target: u32, lun: u64, p: &Periph) -> Ret {
    if patterns.is_empty() {
        return Ret {
            copy: true,
            action: Action::Stop,
        };
    }
    for pat in patterns {
        let Pattern::Periph {
            ref name,
            unit,
            path_id: pp,
            target_id,
            lun: pl,
            flags,
        } = *pat
        else {
            continue;
        };
        if (flags & periph_match::PATH != 0 && pp != path_id)
            || (flags & periph_match::TARGET != 0 && target_id != target)
            || (flags & periph_match::LUN != 0 && pl != lun)
            || (flags & periph_match::UNIT != 0 && unit != p.unit)
            || (flags & periph_match::NAME != 0 && cstr(name) != p.name.as_bytes())
        {
            continue;
        }
        return Ret {
            copy: true,
            action: Action::Stop,
        };
    }
    Ret {
        copy: false,
        action: Action::Stop,
    }
}

/// The outcome of one `XPT_DEV_MATCH`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matched {
    /// What matched, in order.
    pub results: Vec<MatchResult>,
    /// `CAM_DEV_MATCH_*`.
    pub status: u32,
    /// Where to resume, when [`dm_status::MORE`].
    pub pos: Position,
}

/// The cookie of a path ID, target, or LUN: never zero.
fn cookie(v: u64) -> u64 {
    v.wrapping_add(1)
}

struct Walk<'a> {
    edt: &'a Edt,
    patterns: &'a [Pattern],
    room: usize,
    pos: Position,
    out: Vec<MatchResult>,
    status: Option<u32>,
}

/// The peripheral drivers, in the order their lists are walked.
const DRIVERS: &[&str] = &["da", "ada", "nda", "pass", "xpt"];

impl Walk<'_> {
    fn full(&self) -> bool {
        self.out.len() >= self.room
    }

    fn more(&mut self, pos: Position) -> bool {
        self.pos = pos;
        self.status = Some(dm_status::MORE);
        false
    }

    fn pos_has(&self, t: u32) -> bool {
        self.pos.position_type & t != 0
    }

    fn device_result(path_id: u32, target: u32, d: &DeviceNode) -> MatchResult {
        let (inq, ident, protocol) = d.info.as_ref().map_or(([0; 256], [0; 512], 0), |i| {
            (*i.inq_data, *i.ident_data, i.protocol)
        });
        MatchResult::Device {
            path_id,
            target_id: target,
            lun: d.lun,
            protocol,
            inq,
            ident,
            unconfigured: d.unconfigured,
        }
    }

    /// `xptedtmatch`: false if it stopped early.
    fn edt(&mut self) -> bool {
        let start = if self.pos_has(dev_pos::BUS) && self.pos.cookie[0] != 0 {
            if self.pos.generations[0] != self.edt.bus_generation {
                self.status = Some(dm_status::LIST_CHANGED);
                return false;
            }
            let Some(i) = self
                .edt
                .buses
                .iter()
                .position(|b| cookie(u64::from(b.path_id)) == self.pos.cookie[0])
            else {
                self.status = Some(dm_status::LIST_CHANGED);
                return false;
            };
            i
        } else {
            0
        };
        let edt = self.edt;
        edt.buses[start..].iter().all(|b| self.bus(b))
    }

    /// `xptedtbusfunc`.
    fn bus(&mut self, bus: &BusNode) -> bool {
        let here =
            self.pos_has(dev_pos::BUS) && self.pos.cookie[0] == cookie(u64::from(bus.path_id));
        let resuming = here && self.pos_has(dev_pos::TARGET) && self.pos.cookie[1] != 0;
        let r = if resuming {
            Ret {
                copy: false,
                action: Action::Descend,
            }
        } else {
            bus_match(self.patterns, bus)
        };
        if r.action == Action::Error {
            self.status = Some(dm_status::ERROR);
            return false;
        }
        if r.copy {
            if self.full() {
                return self.more(Position {
                    position_type: dev_pos::EDT | dev_pos::BUS,
                    generations: [self.edt.bus_generation, 0, 0, 0],
                    cookie: [cookie(u64::from(bus.path_id)), 0, 0, 0, 0],
                });
            }
            self.out.push(MatchResult::Bus {
                path_id: bus.path_id,
                name: bus.sim.name,
                unit: bus.sim.unit,
                bus_id: bus.sim.bus_id,
            });
        }
        if r.action == Action::Stop {
            return true;
        }
        let start = if resuming {
            if self.pos.generations[1] != bus.generation {
                self.status = Some(dm_status::LIST_CHANGED);
                return false;
            }
            match bus
                .targets
                .iter()
                .position(|t| cookie(u64::from(t.id)) == self.pos.cookie[1])
            {
                Some(i) => i,
                None => {
                    self.status = Some(dm_status::LIST_CHANGED);
                    return false;
                }
            }
        } else {
            0
        };
        for t in &bus.targets[start..] {
            // `xptedttargetfunc`.
            let at_target = here
                && self.pos_has(dev_pos::TARGET)
                && self.pos.cookie[1] == cookie(u64::from(t.id))
                && self.pos_has(dev_pos::DEVICE)
                && self.pos.cookie[2] != 0;
            let first = if at_target {
                if self.pos.generations[2] != t.generation {
                    self.status = Some(dm_status::LIST_CHANGED);
                    return false;
                }
                match t
                    .devices
                    .iter()
                    .position(|d| cookie(d.lun) == self.pos.cookie[2])
                {
                    Some(i) => i,
                    None => {
                        self.status = Some(dm_status::LIST_CHANGED);
                        return false;
                    }
                }
            } else {
                0
            };
            for d in &t.devices[first..] {
                if !self.device(bus, t.id, t.generation, d, at_target) {
                    return false;
                }
            }
        }
        true
    }

    /// `xptedtdevicefunc`.
    fn device(
        &mut self,
        bus: &BusNode,
        target: u32,
        tgen: u32,
        d: &DeviceNode,
        at_target: bool,
    ) -> bool {
        let here = at_target && self.pos.cookie[2] == cookie(d.lun);
        let resuming = here && self.pos_has(dev_pos::PERIPH) && self.pos.cookie[3] != 0;
        let r = if resuming {
            Ret {
                copy: false,
                action: Action::Descend,
            }
        } else {
            device_match(self.patterns, bus.path_id, target, d)
        };
        if r.action == Action::Error {
            self.status = Some(dm_status::ERROR);
            return false;
        }
        let base = |extra: u32| Position {
            position_type: dev_pos::EDT | dev_pos::BUS | dev_pos::TARGET | dev_pos::DEVICE | extra,
            generations: [self.edt.bus_generation, bus.generation, tgen, 0],
            cookie: [
                cookie(u64::from(bus.path_id)),
                cookie(u64::from(target)),
                cookie(d.lun),
                0,
                0,
            ],
        };
        if r.copy {
            if self.full() {
                return self.more(base(0));
            }
            self.out.push(Self::device_result(bus.path_id, target, d));
        }
        if r.action == Action::Stop {
            return true;
        }
        let first = if resuming {
            if self.pos.generations[3] != d.generation {
                self.status = Some(dm_status::LIST_CHANGED);
                return false;
            }
            usize::try_from(self.pos.cookie[3] - 1).unwrap_or(usize::MAX)
        } else {
            0
        };
        for (i, p) in d.periphs.iter().enumerate().skip(first) {
            // `xptedtperiphfunc`.
            let r = periph_match(self.patterns, bus.path_id, target, d.lun, p);
            if r.copy {
                if self.full() {
                    let mut pos = base(dev_pos::PERIPH);
                    pos.generations[3] = d.generation;
                    pos.cookie[3] = cookie(i as u64);
                    return self.more(pos);
                }
                self.out.push(MatchResult::Periph {
                    name: p.name,
                    unit: p.unit,
                    path_id: bus.path_id,
                    target_id: target,
                    lun: d.lun,
                });
            }
        }
        true
    }

    /// `xptperiphlistmatch`: the peripheral drivers' lists, each by unit.
    fn periph_lists(&mut self) -> bool {
        let resume = (self.pos_has(dev_pos::PDPTR) && self.pos.cookie[4] != 0)
            .then(|| (self.pos.cookie[4], self.pos.cookie[3]));
        let first_drv = resume.map_or(0, |(d, _)| usize::try_from(d - 1).unwrap_or(usize::MAX));
        for (di, &drv) in DRIVERS.iter().enumerate().skip(first_drv) {
            let mut units: Vec<(&BusNode, u32, &DeviceNode, &Periph)> = self
                .edt
                .periphs()
                .filter(|(_, _, _, p)| p.name == drv)
                .collect();
            units.sort_by_key(|(_, _, _, p)| p.unit);
            let list_gen = self.list_generation(drv);
            let start = match resume {
                Some((d, p)) if usize::try_from(d - 1).ok() == Some(di) && p != 0 => {
                    if self.pos.generations[3] != list_gen {
                        self.status = Some(dm_status::LIST_CHANGED);
                        return false;
                    }
                    match units
                        .iter()
                        .position(|(_, _, _, q)| cookie(u64::from(q.unit)) == p)
                    {
                        Some(i) => i,
                        None => {
                            self.status = Some(dm_status::LIST_CHANGED);
                            return false;
                        }
                    }
                }
                _ => 0,
            };
            for &(b, t, d, p) in &units[start..] {
                let r = periph_match(self.patterns, b.path_id, t, d.lun, p);
                if r.action == Action::Error {
                    self.status = Some(dm_status::ERROR);
                    return false;
                }
                if !r.copy {
                    continue;
                }
                if self.full() {
                    return self.more(Position {
                        position_type: dev_pos::PDRV | dev_pos::PDPTR | dev_pos::PERIPH,
                        generations: [0, 0, 0, list_gen],
                        cookie: [0, 0, 0, cookie(u64::from(p.unit)), cookie(di as u64)],
                    });
                }
                self.out.push(MatchResult::Periph {
                    name: p.name,
                    unit: p.unit,
                    path_id: b.path_id,
                    target_id: t,
                    lun: d.lun,
                });
            }
        }
        true
    }

    /// A peripheral driver's list's generation: changes as its units
    /// come and go (the sum of the devices' generations stands in).
    fn list_generation(&self, drv: &str) -> u32 {
        self.edt
            .periphs()
            .filter(|(_, _, _, p)| p.name == drv)
            .fold(0u32, |a, (_, _, d, p)| {
                a.wrapping_add(d.generation).wrapping_add(p.unit)
            })
    }
}

/// Runs an `XPT_DEV_MATCH` with `patterns` over `edt`, room for `room`
/// results, from `pos`.
#[must_use]
pub(crate) fn dev_match(edt: &Edt, patterns: &[Pattern], room: usize, pos: Position) -> Matched {
    // The walk: the EDT unless only peripherals are asked for.
    let kind = if pos.position_type != dev_pos::NONE {
        pos.position_type & 0xf00
    } else if patterns.is_empty()
        || patterns
            .iter()
            .any(|p| matches!(p, Pattern::Bus { .. } | Pattern::Device { .. }))
    {
        dev_pos::EDT
    } else {
        dev_pos::PDRV
    };
    let mut w = Walk {
        edt,
        patterns,
        room,
        pos,
        out: Vec::new(),
        status: None,
    };
    let done = match kind {
        dev_pos::EDT => w.edt(),
        dev_pos::PDRV => w.periph_lists(),
        _ => {
            w.status = Some(dm_status::ERROR);
            false
        }
    };
    let status = if done {
        dm_status::LAST
    } else {
        w.status.unwrap_or(dm_status::ERROR)
    };
    Matched {
        results: w.out,
        status,
        pos: if status == dm_status::MORE {
            w.pos
        } else {
            pos
        },
    }
}
