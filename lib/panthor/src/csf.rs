// SPDX-License-Identifier: BSD-2-Clause
//! The firmware's shared interface (specification 05 §5.4 to §5.6, 06): finding the global, group
//! and stream control blocks in the shared section, the initial global configuration, and the
//! ping. Everything in the section was written by the firmware: every address and count is
//! checked before it is used.

use crate::Error;
use crate::fw::SHARED_VA;

/// The shared section, by offset from its start (MCU address `0x04000000`).
pub trait Shared {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn read32(&self, off: usize) -> u32;
    fn write32(&self, off: usize, v: u32);
}

/// An interface version (05 §5.4.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub patch: u16,
}

impl Version {
    pub fn from_word(w: u32) -> Self {
        Self {
            major: (w >> 24) as u8,
            minor: (w >> 16 & 0xFF) as u8,
            patch: (w & 0xFFFF) as u16,
        }
    }
    /// Whether halts go through `GLB_REQ`'s state field (05 §5.4.7).
    pub fn has_state_field(self) -> bool {
        (self.major, self.minor) >= (4, 1)
    }
}

impl core::fmt::Display for Version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The interface as discovered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Iface {
    pub version: Version,
    pub features: u32,
    pub instr_features: u32,
    /// Offsets in the shared section of the global input and output blocks.
    pub glb_input: usize,
    pub glb_output: usize,
    pub groups: u32,
    pub group_stride: u32,
    pub csg_features: u32,
    pub suspend_size: u32,
    pub protm_suspend_size: u32,
    pub streams: u32,
    pub stream_stride: u32,
    pub cs_features: u32,
}

impl Iface {
    /// Work registers per stream (`CS_FEATURES[7:0] + 1`), reported to userspace.
    pub fn work_registers(&self) -> u32 {
        (self.cs_features & 0xFF) + 1
    }
}

// Minimum object sizes (05 §5.4.6).
const GLB_INPUT_SIZE: usize = 0x84;
const GLB_OUTPUT_SIZE: usize = 0x1C;
const CSG_INPUT_SIZE: usize = 0x58;
const CSG_OUTPUT_SIZE: usize = 0x20;
const CS_INPUT_SIZE: usize = 0x58;
const CS_OUTPUT_SIZE: usize = 0xD8;

/// The section offset of MCU address `va` for an object of `size` bytes, if it lies inside.
fn resolve(s: &dyn Shared, va: u32, size: usize) -> Result<usize, Error> {
    let off = va
        .checked_sub(SHARED_VA)
        .ok_or(Error::Bad("interface object outside the section"))? as usize;
    if off.checked_add(size).is_none_or(|end| end > s.len()) {
        return Err(Error::Bad("interface object outside the section"));
    }
    // Every field is a 32-bit word: an object must be word aligned.
    if !off.is_multiple_of(4) {
        return Err(Error::Bad("interface object not word aligned"));
    }
    Ok(off)
}

fn block(s: &dyn Shared, off: usize, size: usize) -> Result<(), Error> {
    if off.checked_add(size).is_none_or(|end| end > s.len()) {
        return Err(Error::Bad("control block outside the section"));
    }
    if !off.is_multiple_of(4) {
        return Err(Error::Bad("control block not word aligned"));
    }
    Ok(())
}

/// Discovers the interface (05 §5.4.5), after the MCU signalled boot.
pub fn discover(s: &dyn Shared) -> Result<Iface, Error> {
    block(s, 0, 0x20)?;
    let word = s.read32(0);
    if word == 0 {
        return Err(Error::Bad("GLB_VERSION is 0: the firmware did not boot"));
    }
    let version = Version::from_word(word);
    let glb_input = resolve(s, s.read32(0x08), GLB_INPUT_SIZE)?;
    let glb_output = resolve(s, s.read32(0x0C), GLB_OUTPUT_SIZE)?;
    let groups = s.read32(0x10);
    let group_stride = s.read32(0x14);
    if !(3..=31).contains(&groups) {
        return Err(Error::Bad("GLB_GROUP_NUM out of range"));
    }
    let instr_features = if (version.major, version.minor) >= (1, 1) {
        s.read32(0x1C)
    } else {
        0
    };
    let mut first: Option<(u32, u32, u32, u32, u32)> = None;
    let mut cs_features = None;
    for g in 0..groups as usize {
        let base = 0x1000 + g * group_stride as usize;
        block(s, base, 0x1C)?;
        let csg = (
            s.read32(base),
            s.read32(base + 0x0C),
            s.read32(base + 0x10),
            s.read32(base + 0x14),
            s.read32(base + 0x18),
        );
        resolve(s, s.read32(base + 0x04), CSG_INPUT_SIZE)?;
        resolve(s, s.read32(base + 0x08), CSG_OUTPUT_SIZE)?;
        let streams = csg.3;
        if !(8..=32).contains(&streams) {
            return Err(Error::Bad("CSG_STREAM_NUM out of range"));
        }
        match first {
            None => first = Some(csg),
            Some(f) if (f.0, f.1, f.2, f.3) == (csg.0, csg.1, csg.2, csg.3) => {}
            Some(_) => return Err(Error::Bad("command-stream groups differ")),
        }
        for k in 0..streams as usize {
            let cs = base + 0x40 + k * csg.4 as usize;
            block(s, cs, 0x0C)?;
            resolve(s, s.read32(cs + 0x04), CS_INPUT_SIZE)?;
            resolve(s, s.read32(cs + 0x08), CS_OUTPUT_SIZE)?;
            let f = s.read32(cs);
            match cs_features {
                None => cs_features = Some(f),
                Some(f0) if f0 == f => {}
                Some(_) => return Err(Error::Bad("command streams differ")),
            }
        }
    }
    let (csg_features, suspend_size, protm_suspend_size, streams, stream_stride) =
        first.ok_or(Error::Bad("no command-stream group"))?;
    Ok(Iface {
        version,
        features: s.read32(0x04),
        instr_features,
        glb_input,
        glb_output,
        groups,
        group_stride,
        csg_features,
        suspend_size,
        protm_suspend_size,
        streams,
        stream_stride,
        cs_features: cs_features.ok_or(Error::Bad("no command stream"))?,
    })
}

// Global input (host writes) and output (firmware writes) fields (05 §5.6.1).
const GLB_REQ: usize = 0x00;
const GLB_ACK_IRQ_MASK: usize = 0x04;
const GLB_PROGRESS_TIMER: usize = 0x10;
const GLB_PWROFF_TIMER: usize = 0x14;
const GLB_ALLOC_EN: usize = 0x18;
const GLB_IDLE_TIMER: usize = 0x80;
const GLB_ACK: usize = 0x00;

const REQ_CFG_PROGRESS_TIMER: u32 = 1 << 1;
const REQ_CFG_ALLOC_EN: u32 = 1 << 2;
const REQ_CFG_PWROFF_TIMER: u32 = 1 << 3;
const REQ_COUNTER_EN: u32 = 1 << 7;
pub const REQ_PING: u32 = 1 << 8;
const REQ_IDLE_EN: u32 = 1 << 10;

/// A timeout of `us` microseconds as the firmware's timer field (05 §5.6.2): units of 1024 ticks
/// of the GPU cycle counter at `core_hz` (source bit 31), or the maximum when the rate is unknown.
pub fn timer(us: u64, core_hz: u64) -> u32 {
    if core_hz == 0 {
        return u32::MAX;
    }
    let v = (us * core_hz).div_ceil(1_000_000 * 1024).min(0x7FFF_FFFF);
    (v as u32) | 1 << 31
}

/// Writes the initial global configuration (05 §5.6.3); the caller then rings doorbell 0.
pub fn configure(s: &dyn Shared, i: &Iface, shader_present: u64, core_hz: u64) {
    let input = i.glb_input;
    let ack = s.read32(i.glb_output + GLB_ACK);
    s.write32(input + GLB_ALLOC_EN, (shader_present & 0xFFFF_FFFF) as u32);
    s.write32(input + GLB_ALLOC_EN + 4, (shader_present >> 32) as u32);
    s.write32(input + GLB_PWROFF_TIMER, timer(10_000, core_hz));
    s.write32(input + GLB_PROGRESS_TIMER, 2_560_000);
    s.write32(input + GLB_IDLE_TIMER, timer(800, core_hz));
    let mask = if i.version.has_state_field() {
        0x0400_750E
    } else {
        0x0400_050E
    };
    s.write32(input + GLB_ACK_IRQ_MASK, mask);
    let toggles = REQ_CFG_ALLOC_EN | REQ_CFG_PWROFF_TIMER | REQ_CFG_PROGRESS_TIMER;
    let mut req = s.read32(input + GLB_REQ) | REQ_IDLE_EN | REQ_COUNTER_EN;
    req = req & !toggles | !ack & toggles;
    s.write32(input + GLB_REQ, req);
}

/// Toggles the ping request (05 §5.8); the caller rings doorbell 0 and waits for [`acked`].
pub fn ping(s: &dyn Shared, i: &Iface) {
    let ack = s.read32(i.glb_output + GLB_ACK);
    let req = s.read32(i.glb_input + GLB_REQ);
    s.write32(i.glb_input + GLB_REQ, req & !REQ_PING | !ack & REQ_PING);
}

/// Whether the firmware has acknowledged the requests in `bits` (request and acknowledge equal).
pub fn acked(s: &dyn Shared, i: &Iface, bits: u32) -> bool {
    (s.read32(i.glb_input + GLB_REQ) ^ s.read32(i.glb_output + GLB_ACK)) & bits == 0
}

/// The configuration toggles [`configure`] requests.
pub const CONFIG_BITS: u32 = REQ_CFG_ALLOC_EN | REQ_CFG_PWROFF_TIMER | REQ_CFG_PROGRESS_TIMER;

/// `GLB_ACK.IDLE`: the firmware's global idle event (06 §6.4.8).
pub const GLB_IDLE: u32 = 1 << 26;
/// `GLB_DB_REQ` and `GLB_DB_ACK` in the global blocks.
const GLB_DB_REQ: usize = 0x08;
const GLB_DB_ACK: usize = 0x08;

/// The three ways to change a request word (06 §6.3.3): toggle `mask` against the acknowledge
/// word, set the field `mask` to `v`, or acknowledge the events `mask`.
pub fn toggle(s: &dyn Shared, req: usize, ack: usize, mask: u32) {
    let r = s.read32(req);
    s.write32(req, !s.read32(ack) & mask | r & !mask);
}

pub fn set_field(s: &dyn Shared, req: usize, mask: u32, v: u32) {
    let r = s.read32(req);
    s.write32(req, v & mask | r & !mask);
}

pub fn ack_events(s: &dyn Shared, req: usize, ack: usize, mask: u32) {
    let r = s.read32(req);
    s.write32(req, s.read32(ack) & mask | r & !mask);
}

/// Pending firmware events in `mask`: request and acknowledge differ.
pub fn events(s: &dyn Shared, req: usize, ack: usize, mask: u32) -> u32 {
    (s.read32(req) ^ s.read32(ack)) & mask
}

/// Toggles CSG slot `g`'s doorbell bit in `GLB_DB_REQ` (06 §6.3.5); the caller rings doorbell 0.
pub fn ring_group(s: &dyn Shared, i: &Iface, g: u32) {
    toggle(
        s,
        i.glb_input + GLB_DB_REQ,
        i.glb_output + GLB_DB_ACK,
        1 << g,
    );
}

/// Acknowledges a pending global idle event (06 §6.4.8); returns whether there was one.
pub fn ack_global_idle(s: &dyn Shared, i: &Iface) -> bool {
    let (req, ack) = (i.glb_input + GLB_REQ, i.glb_output + GLB_ACK);
    if events(s, req, ack, GLB_IDLE) == 0 {
        return false;
    }
    ack_events(s, req, ack, GLB_IDLE);
    true
}

/// A CSG slot's input and output blocks, as section offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blocks {
    pub input: usize,
    pub output: usize,
}

/// CSG slot `g`'s blocks (06 §6.5.1).
pub fn csg(s: &dyn Shared, i: &Iface, g: u32) -> Result<Blocks, Error> {
    if g >= i.groups {
        return Err(Error::Bad("no such group slot"));
    }
    let base = 0x1000 + g as usize * i.group_stride as usize;
    block(s, base, 0x1C)?;
    Ok(Blocks {
        input: resolve(s, s.read32(base + 0x04), CSG_INPUT_SIZE)?,
        output: resolve(s, s.read32(base + 0x08), CSG_OUTPUT_SIZE)?,
    })
}

/// Stream slot `j` of CSG slot `g`'s blocks (06 §6.6.1).
pub fn cs(s: &dyn Shared, i: &Iface, g: u32, j: u32) -> Result<Blocks, Error> {
    if g >= i.groups || j >= i.streams {
        return Err(Error::Bad("no such stream slot"));
    }
    let base = 0x1000
        + g as usize * i.group_stride as usize
        + 0x40
        + j as usize * i.stream_stride as usize;
    block(s, base, 0x0C)?;
    Ok(Blocks {
        input: resolve(s, s.read32(base + 0x04), CS_INPUT_SIZE)?,
        output: resolve(s, s.read32(base + 0x08), CS_OUTPUT_SIZE)?,
    })
}

// CSG input and output fields (06 §6.5.3, §6.5.5).
pub const CSG_REQ: usize = 0x00;
const CSG_ACK_IRQ_MASK: usize = 0x04;
pub const CSG_DB_REQ: usize = 0x08;
pub const CSG_CS_IRQ_ACK: usize = 0x0C;
const CSG_ALLOW_COMPUTE: usize = 0x20;
const CSG_ALLOW_FRAGMENT: usize = 0x28;
const CSG_ALLOW_OTHER: usize = 0x30;
const CSG_EP_REQ: usize = 0x34;
const CSG_EP_REQ_64: usize = 0x38;
const CSG_SUSPEND_BUF: usize = 0x40;
const CSG_PROTM_SUSPEND_BUF: usize = 0x48;
const CSG_CONFIG: usize = 0x50;
pub const CSG_ACK: usize = 0x00;
pub const CSG_DB_ACK: usize = 0x08;
pub const CSG_CS_IRQ_REQ: usize = 0x0C;

/// `CSG_REQ.STATE` values (06 §6.5.2).
pub const CSG_STATE_MASK: u32 = 7;
pub const CSG_TERMINATE: u32 = 0;
pub const CSG_START: u32 = 1;
pub const CSG_SUSPEND: u32 = 2;
pub const CSG_RESUME: u32 = 3;
pub const CSG_ENDPOINT_CONFIG: u32 = 1 << 4;
pub const CSG_STATUS_UPDATE: u32 = 1 << 5;
pub const CSG_SYNC_UPDATE: u32 = 1 << 28;
pub const CSG_IDLE: u32 = 1 << 29;
pub const CSG_PROGRESS_TIMER: u32 = 1 << 31;
pub const CSG_EVENTS: u32 = CSG_SYNC_UPDATE | CSG_IDLE | CSG_PROGRESS_TIMER;

// CS input and output fields (06 §6.6.3, §6.6.4).
pub const CS_REQ: usize = 0x00;
const CS_CONFIG: usize = 0x04;
const CS_ACK_IRQ_MASK: usize = 0x0C;
const CS_BASE: usize = 0x10;
const CS_SIZE: usize = 0x18;
pub const CS_HEAP_START: usize = 0x20;
pub const CS_HEAP_END: usize = 0x28;
const CS_USER_INPUT: usize = 0x30;
const CS_USER_OUTPUT: usize = 0x38;
pub const CS_ACK: usize = 0x00;
pub const CS_STATUS_BLOCKED_REASON: usize = 0x60;
pub const CS_FAULT: usize = 0x80;
pub const CS_FATAL: usize = 0x84;
pub const CS_FAULT_INFO: usize = 0x88;
pub const CS_FATAL_INFO: usize = 0x90;
pub const CS_HEAP_VT_START: usize = 0xC0;
pub const CS_HEAP_VT_END: usize = 0xC4;
pub const CS_HEAP_FRAG_END: usize = 0xCC;
pub const CS_HEAP_ADDRESS: usize = 0xD0;

/// CS events (06 §6.6.2).
pub const CS_TILER_OOM: u32 = 1 << 26;
pub const CS_FATAL_EVENT: u32 = 1 << 30;
pub const CS_FAULT_EVENT: u32 = 1 << 31;

/// Writes a 64-bit field, low word first.
pub fn write_u64(s: &dyn Shared, off: usize, v: u64) {
    s.write32(off, (v & 0xFFFF_FFFF) as u32);
    s.write32(off + 4, (v >> 32) as u32);
}

/// Reads a 64-bit field.
pub fn read64(s: &dyn Shared, off: usize) -> u64 {
    u64::from(s.read32(off)) | u64::from(s.read32(off + 4)) << 32
}

/// A stream slot's configuration (06 §6.6.6, 07 §7.3.3 step 4).
#[derive(Clone, Copy, Debug)]
pub struct Stream {
    /// The ring's GPU address (group address space) and size.
    pub ring: u64,
    pub ring_size: u32,
    /// The ring's input and output pages (MCU addresses).
    pub user_input: u64,
    pub user_output: u64,
    pub priority: u8,
    pub doorbell: u8,
}

/// Programs stream slot `cs` (its input block) to start; the caller copies the output page's
/// `EXTRACT` into the input page first.
pub fn program_stream(s: &dyn Shared, cs: &Blocks, st: &Stream) {
    write_u64(s, cs.input + CS_BASE, st.ring);
    s.write32(cs.input + CS_SIZE, st.ring_size);
    write_u64(s, cs.input + CS_USER_INPUT, st.user_input);
    write_u64(s, cs.input + CS_USER_OUTPUT, st.user_output);
    s.write32(
        cs.input + CS_CONFIG,
        u32::from(st.priority & 0xF) | u32::from(st.doorbell) << 8,
    );
    s.write32(cs.input + CS_ACK_IRQ_MASK, u32::MAX);
    // STATE START, IDLE_SYNC_WAIT and IDLE_EMPTY.
    set_field(s, cs.input + CS_REQ, 0x507, 0x501);
}

/// Sets a stream slot back to STOP after its group left the slot (06 §6.5.6).
pub fn stop_stream(s: &dyn Shared, cs: &Blocks) {
    set_field(s, cs.input + CS_REQ, 7, 0);
}

/// A group's configuration on its CSG slot (07 §7.3.3 step 5).
#[derive(Clone, Copy, Debug)]
pub struct Group {
    pub compute: u64,
    pub fragment: u64,
    pub tiler: u32,
    pub max_compute: u8,
    pub max_fragment: u8,
    pub max_tiler: u8,
    pub priority: u8,
    pub as_slot: u32,
    pub suspend: u64,
    pub protm_suspend: u64,
}

/// The endpoint request word (06 §6.5.4).
pub fn endpoint(g: &Group) -> u32 {
    u32::from(g.max_compute)
        | u32::from(g.max_fragment) << 8
        | u32::from(g.max_tiler & 0xF) << 16
        | u32::from(g.priority & 0xF) << 28
}

/// Programs CSG slot `csg` for `g` and requests `state` (START or RESUME) with the endpoint
/// configuration; `streams` is the mask of stream slots programmed. The caller then rings the
/// group ([`ring_group`]) and doorbell 0, and waits for [`csg_acked`].
pub fn start_group(s: &dyn Shared, i: &Iface, csg: &Blocks, g: &Group, streams: u32, state: u32) {
    write_u64(s, csg.input + CSG_ALLOW_COMPUTE, g.compute);
    write_u64(s, csg.input + CSG_ALLOW_FRAGMENT, g.fragment);
    s.write32(csg.input + CSG_ALLOW_OTHER, g.tiler);
    if i.version.major >= 4 {
        write_u64(s, csg.input + CSG_EP_REQ_64, u64::from(endpoint(g)));
    } else {
        s.write32(csg.input + CSG_EP_REQ, endpoint(g));
    }
    s.write32(csg.input + CSG_CONFIG, g.as_slot);
    write_u64(s, csg.input + CSG_SUSPEND_BUF, g.suspend);
    write_u64(s, csg.input + CSG_PROTM_SUSPEND_BUF, g.protm_suspend);
    s.write32(csg.input + CSG_ACK_IRQ_MASK, u32::MAX);
    toggle(s, csg.input + CSG_DB_REQ, csg.output + CSG_DB_ACK, streams);
    set_field(s, csg.input + CSG_REQ, CSG_STATE_MASK, state);
    toggle(
        s,
        csg.input + CSG_REQ,
        csg.output + CSG_ACK,
        CSG_ENDPOINT_CONFIG,
    );
}

/// Requests `state` (SUSPEND or TERMINATE) of CSG slot `csg`; the caller rings as above.
pub fn stop_group(s: &dyn Shared, csg: &Blocks, state: u32) {
    set_field(s, csg.input + CSG_REQ, CSG_STATE_MASK, state);
}

/// Whether CSG slot `csg` acknowledged its state request and every toggle in `mask` (a state
/// counts only when all three bits match, 07 §7.3.1).
pub fn csg_acked(s: &dyn Shared, csg: &Blocks, mask: u32) -> bool {
    let req = s.read32(csg.input + CSG_REQ);
    let ack = s.read32(csg.output + CSG_ACK);
    (req ^ ack) & (CSG_STATE_MASK | mask) == 0
}

/// The state CSG slot `csg` acknowledged.
pub fn csg_state(s: &dyn Shared, csg: &Blocks) -> u32 {
    s.read32(csg.output + CSG_ACK) & CSG_STATE_MASK
}
