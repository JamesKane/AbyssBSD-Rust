// SPDX-License-Identifier: BSD-2-Clause
//! Device, slot, endpoint, and input contexts (xHCI 1.2 §6.2), as plain
//! values that encode to and decode from their eight words. Where they
//! sit depends on the controller's context size ([`ContextSize`]).

use crate::trb::byte;

/// Context size, from `HCCPARAMS1.CSZ`: 32 or 64 bytes per context; only
/// the first 32 bytes of each are defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextSize {
    /// 32-byte contexts.
    Bytes32,
    /// 64-byte contexts.
    Bytes64,
}

impl ContextSize {
    /// Bytes per context.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::Bytes32 => 32,
            Self::Bytes64 => 64,
        }
    }

    /// Byte offset of the slot context in a device context.
    #[must_use]
    pub fn device_slot(self) -> usize {
        0
    }

    /// Byte offset of endpoint `dci`'s context in a device context.
    #[must_use]
    pub fn device_endpoint(self, dci: Dci) -> usize {
        usize::from(dci.0) * self.bytes()
    }

    /// Byte offset of the slot context in an input context.
    #[must_use]
    pub fn input_slot(self) -> usize {
        self.bytes()
    }

    /// Byte offset of endpoint `dci`'s context in an input context.
    #[must_use]
    pub fn input_endpoint(self, dci: Dci) -> usize {
        (usize::from(dci.0) + 1) * self.bytes()
    }

    /// Bytes in a device context (the slot and 31 endpoints).
    #[must_use]
    pub fn device_bytes(self) -> usize {
        32 * self.bytes()
    }

    /// Bytes in an input context (control, slot, and 31 endpoints).
    #[must_use]
    pub fn input_bytes(self) -> usize {
        33 * self.bytes()
    }
}

/// A device context index: 1 for the default control endpoint, then
/// `2 * number + direction` (IN is 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Dci(u8);

impl Dci {
    /// The default control endpoint.
    pub const CONTROL: Self = Self(1);

    /// The index for endpoint address `address` (`bEndpointAddress`:
    /// number in bits 3:0, IN in bit 7). Control endpoints other than 0
    /// use the OUT index for both directions.
    #[must_use]
    pub fn from_address(address: u8, control: bool) -> Option<Self> {
        let n = address & 0x0f;
        if n == 0 {
            return Some(Self::CONTROL);
        }
        let dir = u8::from(address & 0x80 != 0 && !control);
        Some(Self(n * 2 + dir))
    }

    /// The raw index, 1 to 31.
    #[must_use]
    pub fn get(self) -> u8 {
        self.0
    }

    /// From a raw index, if it is one (an event's endpoint field).
    #[must_use]
    pub fn new(raw: u8) -> Option<Self> {
        (1..=31).contains(&raw).then_some(Self(raw))
    }
}

/// Port speeds, as the default protocol speed IDs (xHCI 1.2 §7.2.2.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Speed {
    /// 1.5 Mb/s.
    Low,
    /// 12 Mb/s.
    Full,
    /// 480 Mb/s.
    High,
    /// 5 Gb/s.
    Super,
    /// 10 Gb/s and up.
    SuperPlus,
}

impl Speed {
    /// From a `PORTSC` speed field with the default speed IDs.
    #[must_use]
    pub fn from_psiv(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Full,
            2 => Self::Low,
            3 => Self::High,
            4 => Self::Super,
            5 => Self::SuperPlus,
            _ => return None,
        })
    }

    /// The speed ID for a slot context.
    #[must_use]
    pub fn psiv(self) -> u8 {
        match self {
            Self::Full => 1,
            Self::Low => 2,
            Self::High => 3,
            Self::Super => 4,
            Self::SuperPlus => 5,
        }
    }

    /// The default control endpoint's max packet size before the device
    /// descriptor says (USB 2.0 §5.5.3; 3.2 §9.6.1).
    #[must_use]
    pub fn default_max_packet(self) -> u16 {
        match self {
            Self::Low | Self::Full => 8,
            Self::High => 64,
            Self::Super | Self::SuperPlus => 512,
        }
    }

    /// Whether this is a SuperSpeed (USB 3) speed.
    #[must_use]
    pub fn is_super(self) -> bool {
        matches!(self, Self::Super | Self::SuperPlus)
    }
}

/// A slot context (xHCI 1.2 §6.2.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotContext {
    /// The route string: hub ports below the root port, four bits each.
    pub route: u32,
    /// The speed ID.
    pub speed: u8,
    /// A multi-TT hub (for a hub's own slot) or behind one (for a
    /// low- or full-speed device).
    pub mtt: bool,
    /// The device is a hub.
    pub hub: bool,
    /// The last valid endpoint context index.
    pub entries: u8,
    /// The root hub port, from 1.
    pub root_port: u8,
    /// For a hub: its downstream ports.
    pub ports: u8,
    /// For a low- or full-speed device behind a high-speed hub: that
    /// hub's slot.
    pub tt_slot: u8,
    /// And the hub port it is on.
    pub tt_port: u8,
    /// For a high-speed hub: its TT think time (0 to 3).
    pub ttt: u8,
    /// The interrupter for events not bound to an endpoint.
    pub interrupter: u16,
    /// Output: the device's USB address.
    pub address: u8,
    /// Output: the slot state (0 disabled/enabled, 1 default, 2
    /// addressed, 3 configured).
    pub state: u8,
}

impl SlotContext {
    /// The eight words.
    #[must_use]
    pub fn encode(&self) -> [u32; 8] {
        let mut w = [0u32; 8];
        w[0] = (self.route & 0xf_ffff)
            | (u32::from(self.speed & 0xf) << 20)
            | (u32::from(self.mtt) << 25)
            | (u32::from(self.hub) << 26)
            | (u32::from(self.entries & 0x1f) << 27);
        w[1] = (u32::from(self.root_port) << 16) | (u32::from(self.ports) << 24);
        w[2] = u32::from(self.tt_slot)
            | (u32::from(self.tt_port) << 8)
            | (u32::from(self.ttt & 3) << 16)
            | (u32::from(self.interrupter & 0x3ff) << 22);
        w[3] = u32::from(self.address) | (u32::from(self.state & 0x1f) << 27);
        w
    }

    /// Decodes eight words.
    #[must_use]
    pub fn decode(w: &[u32; 8]) -> Self {
        Self {
            route: w[0] & 0xf_ffff,
            speed: byte((w[0] >> 20) & 0xf),
            mtt: w[0] & (1 << 25) != 0,
            hub: w[0] & (1 << 26) != 0,
            entries: byte(w[0] >> 27),
            root_port: byte(w[1] >> 16),
            ports: byte(w[1] >> 24),
            tt_slot: byte(w[2]),
            tt_port: byte(w[2] >> 8),
            ttt: byte((w[2] >> 16) & 3),
            interrupter: u16::try_from((w[2] >> 22) & 0x3ff).unwrap_or(0),
            address: byte(w[3]),
            state: byte(w[3] >> 27),
        }
    }
}

/// Endpoint types (xHCI 1.2 Table 6-9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointType {
    /// Isochronous OUT.
    IsochOut = 1,
    /// Bulk OUT.
    BulkOut = 2,
    /// Interrupt OUT.
    InterruptOut = 3,
    /// Control (bidirectional).
    Control = 4,
    /// Isochronous IN.
    IsochIn = 5,
    /// Bulk IN.
    BulkIn = 6,
    /// Interrupt IN.
    InterruptIn = 7,
}

impl EndpointType {
    /// From an endpoint descriptor's transfer type (`bmAttributes` bits
    /// 1:0) and direction.
    #[must_use]
    pub fn from_descriptor(attributes: u8, is_in: bool) -> Self {
        match (attributes & 3, is_in) {
            (0, _) => Self::Control,
            (1, false) => Self::IsochOut,
            (1, true) => Self::IsochIn,
            (2, false) => Self::BulkOut,
            (2, true) => Self::BulkIn,
            (_, false) => Self::InterruptOut,
            (_, true) => Self::InterruptIn,
        }
    }

    /// Interrupt or isochronous.
    #[must_use]
    pub fn is_periodic(self) -> bool {
        matches!(
            self,
            Self::IsochOut | Self::IsochIn | Self::InterruptOut | Self::InterruptIn
        )
    }

    fn raw(self) -> u32 {
        match self {
            Self::IsochOut => 1,
            Self::BulkOut => 2,
            Self::InterruptOut => 3,
            Self::Control => 4,
            Self::IsochIn => 5,
            Self::BulkIn => 6,
            Self::InterruptIn => 7,
        }
    }

    fn from_raw(v: u32) -> Option<Self> {
        Some(match v {
            1 => Self::IsochOut,
            2 => Self::BulkOut,
            3 => Self::InterruptOut,
            4 => Self::Control,
            5 => Self::IsochIn,
            6 => Self::BulkIn,
            7 => Self::InterruptIn,
            _ => return None,
        })
    }

    /// The average TRB length to program (xHCI 1.2 §4.14.1.1).
    #[must_use]
    pub fn average_trb_length(self) -> u16 {
        match self {
            Self::Control => 8,
            Self::InterruptIn | Self::InterruptOut => 1024,
            _ => 3072,
        }
    }
}

/// An endpoint context (xHCI 1.2 §6.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointContext {
    /// Output: the endpoint state (0 disabled, 1 running, 2 halted, 3
    /// stopped, 4 error).
    pub state: u8,
    /// SuperSpeed isochronous: bursts per interval minus one.
    pub mult: u8,
    /// Primary streams: log2 of the stream context array size minus one
    /// (0 without streams).
    pub max_primary_streams: u8,
    /// Linear stream array.
    pub lsa: bool,
    /// The service interval, encoded ([`interval`]).
    pub interval: u8,
    /// Retries before an error halts the endpoint (3; 0 for
    /// isochronous).
    pub error_count: u8,
    /// The type; `None` decodes a disabled context's 0.
    pub ep_type: Option<EndpointType>,
    /// Max burst size minus one (SuperSpeed), or extra transactions per
    /// microframe (high-speed periodic).
    pub max_burst: u8,
    /// `wMaxPacketSize` (its size bits).
    pub max_packet: u16,
    /// The TR dequeue pointer (16-byte aligned) or stream context array.
    pub dequeue: u64,
    /// The dequeue cycle state.
    pub dcs: bool,
    /// The average TRB length.
    pub average_trb_length: u16,
    /// The max ESIT payload in bytes (periodic endpoints).
    pub max_esit_payload: u32,
}

impl EndpointContext {
    /// The eight words.
    #[must_use]
    pub fn encode(&self) -> [u32; 8] {
        let mut w = [0u32; 8];
        let esit_hi = (self.max_esit_payload >> 16) & 0xff;
        let esit_lo = self.max_esit_payload & 0xffff;
        w[0] = u32::from(self.state & 7)
            | (u32::from(self.mult & 3) << 8)
            | (u32::from(self.max_primary_streams & 0x1f) << 10)
            | (u32::from(self.lsa) << 15)
            | (u32::from(self.interval) << 16)
            | (esit_hi << 24);
        w[1] = (u32::from(self.error_count & 3) << 1)
            | (self.ep_type.map_or(0, EndpointType::raw) << 3)
            | (u32::from(self.max_burst) << 8)
            | (u32::from(self.max_packet) << 16);
        let dq = (self.dequeue & !0xf) | u64::from(self.dcs);
        let b = dq.to_le_bytes();
        w[2] = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        w[3] = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        w[4] = u32::from(self.average_trb_length) | (esit_lo << 16);
        w
    }

    /// Decodes eight words.
    #[must_use]
    pub fn decode(w: &[u32; 8]) -> Self {
        let dq = u64::from(w[2]) | (u64::from(w[3]) << 32);
        Self {
            state: byte(w[0] & 7),
            mult: byte((w[0] >> 8) & 3),
            max_primary_streams: byte((w[0] >> 10) & 0x1f),
            lsa: w[0] & (1 << 15) != 0,
            interval: byte(w[0] >> 16),
            error_count: byte((w[1] >> 1) & 3),
            ep_type: EndpointType::from_raw((w[1] >> 3) & 7),
            max_burst: byte(w[1] >> 8),
            max_packet: u16::try_from(w[1] >> 16).unwrap_or(0),
            dequeue: dq & !0xf,
            dcs: dq & 1 != 0,
            average_trb_length: u16::try_from(w[4] & 0xffff).unwrap_or(0),
            max_esit_payload: ((w[0] >> 24) << 16) | (w[4] >> 16),
        }
    }
}

/// A stream context (xHCI 1.2 §6.2.4.1): stream `i`'s transfer ring, as a
/// primary ring (SCT 1), its dequeue pointer and cycle state.
#[must_use]
pub fn stream_context(dequeue: u64, dcs: bool) -> [u32; 4] {
    let dq = (dequeue & !0xf) | (1 << 1) | u64::from(dcs);
    let b = dq.to_le_bytes();
    [
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        0,
        0,
    ]
}

/// The MaxPStreams field for a linear stream context array of `entries`
/// (a power of two, 2 to 2^16): log2 of it minus one.
#[must_use]
pub fn max_primary_streams(entries: u32) -> u8 {
    byte(entries.max(2).ilog2() - 1)
}

/// The Interval field for a periodic endpoint (xHCI 1.2 §6.2.3.6): the
/// service period is 2^Interval × 125 µs. `b_interval` is the endpoint
/// descriptor's `bInterval`. Control and bulk endpoints get 0.
#[must_use]
pub fn interval(speed: Speed, ep_type: EndpointType, b_interval: u8) -> u8 {
    if !ep_type.is_periodic() {
        return 0;
    }
    let isoch = matches!(ep_type, EndpointType::IsochIn | EndpointType::IsochOut);
    match speed {
        // Exponent form, 1 to 16: Interval = bInterval - 1.
        Speed::High | Speed::Super | Speed::SuperPlus => b_interval.clamp(1, 16) - 1,
        // Full-speed isochronous: exponent in frames.
        Speed::Full if isoch => b_interval.clamp(1, 16) + 2,
        // Low- and full-speed interrupt: bInterval frames, rounded down
        // to a power of two in microframes, 8 to 1024 (3 to 10).
        Speed::Low | Speed::Full => {
            let microframes = u32::from(b_interval.max(1)) * 8;
            byte(microframes.ilog2().clamp(3, 10))
        }
    }
}

/// The input control context's words (xHCI 1.2 §6.2.5.1): the contexts
/// to drop and add (bit `i` is context `i`; bit 0 is the slot), and for
/// Configure Endpoint, the configuration, interface, and alternate
/// setting being selected.
#[must_use]
pub fn input_control(drop: u32, add: u32, config: u8, interface: u8, alternate: u8) -> [u32; 8] {
    let mut w = [0u32; 8];
    w[0] = drop & !3;
    w[1] = add;
    w[7] = u32::from(config) | (u32::from(interface) << 8) | (u32::from(alternate) << 16);
    w
}
