// SPDX-License-Identifier: BSD-2-Clause
//! Transfer request blocks (xHCI 1.2 §6.4): the 16-byte records on every
//! ring, built and decoded here. The cycle bit is the ring's business
//! ([`crate::ring`]): builders leave it clear.

/// One TRB: a 64-bit parameter, a status word, and a control word whose
/// bit 0 is the cycle bit and bits 15:10 the type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trb(pub [u32; 4]);

/// Control-word flags.
pub mod flag {
    /// The cycle bit.
    pub const CYCLE: u32 = 1 << 0;
    /// Link TRB: toggle the consumer's cycle state.
    pub const TOGGLE_CYCLE: u32 = 1 << 1;
    /// Interrupt on short packet.
    pub const ISP: u32 = 1 << 2;
    /// Chain: the next TRB belongs to the same TD.
    pub const CHAIN: u32 = 1 << 4;
    /// Interrupt on completion.
    pub const IOC: u32 = 1 << 5;
    /// Immediate data: the parameter holds the data (setup TRBs).
    pub const IDT: u32 = 1 << 6;
    /// Address Device: block the `SET_ADDRESS` request.
    pub const BSR: u32 = 1 << 9;
    /// Configure Endpoint: deconfigure.
    pub const DECONFIGURE: u32 = 1 << 9;
    /// Transfer event: the parameter is event data, not a TRB address.
    pub const EVENT_DATA: u32 = 1 << 2;
    /// Data and status stages: the direction is IN.
    pub const DIR_IN: u32 = 1 << 16;
}

/// TRB types (xHCI 1.2 Table 6-91).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TrbType {
    /// Normal (bulk and interrupt data).
    Normal = 1,
    /// Control setup stage.
    Setup = 2,
    /// Control data stage.
    Data = 3,
    /// Control status stage.
    Status = 4,
    /// Link to the next segment.
    Link = 6,
    /// Event data.
    EventData = 7,
    /// Transfer no-op.
    NoOp = 8,
    /// Enable Slot command.
    EnableSlot = 9,
    /// Disable Slot command.
    DisableSlot = 10,
    /// Address Device command.
    AddressDevice = 11,
    /// Configure Endpoint command.
    ConfigureEndpoint = 12,
    /// Evaluate Context command.
    EvaluateContext = 13,
    /// Reset Endpoint command.
    ResetEndpoint = 14,
    /// Stop Endpoint command.
    StopEndpoint = 15,
    /// Set TR Dequeue Pointer command.
    SetTrDequeue = 16,
    /// Reset Device command.
    ResetDevice = 17,
    /// No-op command.
    NoOpCommand = 23,
    /// Transfer event.
    TransferEvent = 32,
    /// Command completion event.
    CommandCompletion = 33,
    /// Port status change event.
    PortStatusChange = 34,
    /// Bandwidth request event.
    BandwidthRequest = 35,
    /// Doorbell event.
    Doorbell = 36,
    /// Host controller event.
    HostController = 37,
    /// Device notification event.
    DeviceNotification = 38,
    /// Microframe index wrap event.
    MfindexWrap = 39,
}

impl TrbType {
    /// The type for a raw value, if this crate knows it.
    #[must_use]
    pub fn from_raw(v: u8) -> Option<Self> {
        use TrbType::*;
        Some(match v {
            1 => Normal,
            2 => Setup,
            3 => Data,
            4 => Status,
            6 => Link,
            7 => EventData,
            8 => NoOp,
            9 => EnableSlot,
            10 => DisableSlot,
            11 => AddressDevice,
            12 => ConfigureEndpoint,
            13 => EvaluateContext,
            14 => ResetEndpoint,
            15 => StopEndpoint,
            16 => SetTrDequeue,
            17 => ResetDevice,
            23 => NoOpCommand,
            32 => TransferEvent,
            33 => CommandCompletion,
            34 => PortStatusChange,
            35 => BandwidthRequest,
            36 => Doorbell,
            37 => HostController,
            38 => DeviceNotification,
            39 => MfindexWrap,
            _ => return None,
        })
    }
}

/// The low byte of `v`.
pub(crate) fn byte(v: u32) -> u8 {
    v.to_le_bytes()[0]
}

impl Trb {
    /// A TRB of type `t` with the given parameter, status, and control
    /// flags (cycle bit clear).
    #[must_use]
    pub fn new(t: TrbType, param: u64, status: u32, flags: u32) -> Self {
        let [lo, hi] = split(param);
        Self([
            lo,
            hi,
            status,
            (u32::from(t as u8) << 10) | (flags & !flag::CYCLE),
        ])
    }

    /// The 64-bit parameter.
    #[must_use]
    pub fn param(&self) -> u64 {
        u64::from(self.0[0]) | (u64::from(self.0[1]) << 32)
    }

    /// The status word.
    #[must_use]
    pub fn status(&self) -> u32 {
        self.0[2]
    }

    /// The control word.
    #[must_use]
    pub fn control(&self) -> u32 {
        self.0[3]
    }

    /// The raw type field.
    #[must_use]
    pub fn raw_type(&self) -> u8 {
        byte((self.0[3] >> 10) & 0x3f)
    }

    /// The type, if known.
    #[must_use]
    pub fn trb_type(&self) -> Option<TrbType> {
        TrbType::from_raw(self.raw_type())
    }

    /// The cycle bit.
    #[must_use]
    pub fn cycle(&self) -> bool {
        self.0[3] & flag::CYCLE != 0
    }

    /// The slot ID field (bits 31:24 of the control word).
    #[must_use]
    pub fn slot(&self) -> u8 {
        byte(self.0[3] >> 24)
    }

    /// The endpoint ID field (bits 20:16 of the control word).
    #[must_use]
    pub fn endpoint(&self) -> u8 {
        byte((self.0[3] >> 16) & 0x1f)
    }

    /// The completion code (bits 31:24 of the status word), for events.
    #[must_use]
    pub fn code(&self) -> CompletionCode {
        CompletionCode::from_raw(byte(self.0[2] >> 24))
    }
}

fn split(v: u64) -> [u32; 2] {
    let b = v.to_le_bytes();
    [
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
    ]
}

/// The slot field of a command's control word.
fn slot_field(slot: u8) -> u32 {
    u32::from(slot) << 24
}

/// The endpoint ID field of a command's control word.
fn endpoint_field(dci: u8) -> u32 {
    u32::from(dci & 0x1f) << 16
}

/// The interrupter target field of a transfer TRB's status word.
fn interrupter_field(i: u16) -> u32 {
    u32::from(i & 0x3ff) << 22
}

/// Transfer TRBs carry at most this many bytes, and must not cross a 64
/// KiB boundary (xHCI 1.2 §4.11.7.1).
pub const MAX_TRB_BYTES: u32 = 64 * 1024;

/// A control transfer's setup packet (USB 2.0 §9.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupPacket {
    /// Direction, type, and recipient.
    pub request_type: u8,
    /// The request.
    pub request: u8,
    /// wValue.
    pub value: u16,
    /// wIndex.
    pub index: u16,
    /// wLength.
    pub length: u16,
}

impl SetupPacket {
    /// The device-to-host bit of `bmRequestType`.
    pub const DIR_IN: u8 = 0x80;

    /// The eight bytes, as the setup TRB's immediate parameter.
    #[must_use]
    pub fn to_u64(&self) -> u64 {
        u64::from(self.request_type)
            | (u64::from(self.request) << 8)
            | (u64::from(self.value) << 16)
            | (u64::from(self.index) << 32)
            | (u64::from(self.length) << 48)
    }

    /// Whether the data stage, if any, is IN.
    #[must_use]
    pub fn is_in(&self) -> bool {
        self.request_type & Self::DIR_IN != 0
    }
}

/// Transfer TRB builders.
pub mod transfer {
    use super::*;

    /// A control transfer's setup stage. `data` says whether a data stage
    /// follows and its direction.
    #[must_use]
    pub fn setup(p: &SetupPacket, interrupter: u16) -> Trb {
        let trt: u32 = match (p.length, p.is_in()) {
            (0, _) => 0,
            (_, false) => 2,
            (_, true) => 3,
        };
        Trb::new(
            TrbType::Setup,
            p.to_u64(),
            8 | interrupter_field(interrupter),
            flag::IDT | (trt << 16),
        )
    }

    /// A control transfer's data stage, or one more piece of it (`chain`
    /// on all but the last).
    #[must_use]
    pub fn data(buf: u64, len: u32, td_size: u8, is_in: bool, flags: u32, interrupter: u16) -> Trb {
        let dir = if is_in { flag::DIR_IN } else { 0 };
        Trb::new(
            TrbType::Data,
            buf,
            status(len, td_size, interrupter),
            flags | dir,
        )
    }

    /// A control transfer's status stage. Its direction is the opposite
    /// of the data stage's, or IN with none.
    #[must_use]
    pub fn status_stage(data_in: Option<bool>, interrupter: u16) -> Trb {
        let dir = match data_in {
            Some(true) => 0,
            Some(false) | None => flag::DIR_IN,
        };
        Trb::new(
            TrbType::Status,
            0,
            interrupter_field(interrupter),
            flag::IOC | dir,
        )
    }

    /// A normal TRB (bulk and interrupt data).
    #[must_use]
    pub fn normal(buf: u64, len: u32, td_size: u8, flags: u32, interrupter: u16) -> Trb {
        Trb::new(
            TrbType::Normal,
            buf,
            status(len, td_size, interrupter),
            flags,
        )
    }

    /// A transfer no-op.
    #[must_use]
    pub fn no_op(interrupter: u16) -> Trb {
        Trb::new(TrbType::NoOp, 0, interrupter_field(interrupter), flag::IOC)
    }

    fn status(len: u32, td_size: u8, interrupter: u16) -> u32 {
        (len & 0x1_ffff) | (u32::from(td_size.min(31)) << 17) | interrupter_field(interrupter)
    }

    /// The TD Size field for a TRB (xHCI 1.2 §4.11.2.4): the packets left
    /// in the TD after this TRB, capped at 31; 0 on the last TRB.
    #[must_use]
    pub fn td_size(remaining_after: u32, max_packet: u16, last: bool) -> u8 {
        if last || max_packet == 0 {
            return 0;
        }
        let packets = remaining_after.div_ceil(u32::from(max_packet));
        byte(packets.min(31))
    }
}

/// Command TRB builders.
pub mod command {
    use super::*;

    /// Enable Slot, for slot type `slot_type` (from the protocol's
    /// supported-protocol capability; 0 for USB).
    #[must_use]
    pub fn enable_slot(slot_type: u8) -> Trb {
        Trb::new(TrbType::EnableSlot, 0, 0, u32::from(slot_type & 0x1f) << 16)
    }

    /// Disable Slot.
    #[must_use]
    pub fn disable_slot(slot: u8) -> Trb {
        Trb::new(TrbType::DisableSlot, 0, 0, slot_field(slot))
    }

    /// Address Device with the input context at `input`; `block` sets BSR
    /// (enable the default control endpoint without `SET_ADDRESS`).
    #[must_use]
    pub fn address_device(slot: u8, input: u64, block: bool) -> Trb {
        let bsr = if block { flag::BSR } else { 0 };
        Trb::new(TrbType::AddressDevice, input, 0, slot_field(slot) | bsr)
    }

    /// Configure Endpoint with the input context at `input`.
    #[must_use]
    pub fn configure_endpoint(slot: u8, input: u64) -> Trb {
        Trb::new(TrbType::ConfigureEndpoint, input, 0, slot_field(slot))
    }

    /// Configure Endpoint with DC set: every endpoint but 0 deconfigured.
    #[must_use]
    pub fn deconfigure(slot: u8) -> Trb {
        Trb::new(
            TrbType::ConfigureEndpoint,
            0,
            0,
            slot_field(slot) | flag::DECONFIGURE,
        )
    }

    /// Evaluate Context with the input context at `input`.
    #[must_use]
    pub fn evaluate_context(slot: u8, input: u64) -> Trb {
        Trb::new(TrbType::EvaluateContext, input, 0, slot_field(slot))
    }

    /// Reset Endpoint (after a halt).
    #[must_use]
    pub fn reset_endpoint(slot: u8, dci: u8) -> Trb {
        Trb::new(
            TrbType::ResetEndpoint,
            0,
            0,
            slot_field(slot) | endpoint_field(dci),
        )
    }

    /// Stop Endpoint.
    #[must_use]
    pub fn stop_endpoint(slot: u8, dci: u8) -> Trb {
        Trb::new(
            TrbType::StopEndpoint,
            0,
            0,
            slot_field(slot) | endpoint_field(dci),
        )
    }

    /// Set TR Dequeue Pointer to `dequeue` with consumer cycle state
    /// `cycle`, on stream `stream` (0 without streams; `sct` the stream
    /// context type, 1 for a primary stream ring).
    #[must_use]
    pub fn set_tr_dequeue(slot: u8, dci: u8, dequeue: u64, cycle: bool, stream: u16) -> Trb {
        let sct: u64 = if stream == 0 { 0 } else { 1 << 1 };
        Trb::new(
            TrbType::SetTrDequeue,
            (dequeue & !0xf) | sct | u64::from(cycle),
            u32::from(stream) << 16,
            slot_field(slot) | endpoint_field(dci),
        )
    }

    /// Reset Device.
    #[must_use]
    pub fn reset_device(slot: u8) -> Trb {
        Trb::new(TrbType::ResetDevice, 0, 0, slot_field(slot))
    }

    /// The no-op command.
    #[must_use]
    pub fn no_op() -> Trb {
        Trb::new(TrbType::NoOpCommand, 0, 0, 0)
    }
}

/// A link TRB to `next`; `toggle` flips the consumer's cycle state (set
/// on the link that closes a ring).
#[must_use]
pub fn link(next: u64, toggle: bool) -> Trb {
    let tc = if toggle { flag::TOGGLE_CYCLE } else { 0 };
    Trb::new(TrbType::Link, next, 0, tc)
}

/// Completion codes (xHCI 1.2 §6.4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionCode {
    /// 0: not a valid code.
    Invalid,
    /// 1.
    Success,
    /// 2: the controller could not keep up with the data buffer.
    DataBuffer,
    /// 3: the device sent more than it should.
    Babble,
    /// 4: a USB transaction error.
    UsbTransaction,
    /// 5: a TRB error.
    Trb,
    /// 6: the endpoint stalled.
    Stall,
    /// 7: out of controller resources.
    Resource,
    /// 8: not enough bandwidth.
    Bandwidth,
    /// 9: no slots available.
    NoSlots,
    /// 11: the slot is not enabled.
    SlotNotEnabled,
    /// 12: the endpoint is not enabled.
    EndpointNotEnabled,
    /// 13: a short packet.
    ShortPacket,
    /// 17: a parameter error.
    Parameter,
    /// 19: a context in the wrong state for the command.
    ContextState,
    /// 21: the event ring was full.
    EventRingFull,
    /// 22: the device is incompatible.
    IncompatibleDevice,
    /// 24: the command ring stopped.
    CommandRingStopped,
    /// 25: the command was aborted.
    CommandAborted,
    /// 26: the endpoint was stopped.
    Stopped,
    /// 27: stopped; the length is invalid.
    StoppedLengthInvalid,
    /// 28: stopped after a short packet.
    StoppedShortPacket,
    /// 36: a split transaction error.
    SplitTransaction,
    /// Any other code.
    Other(u8),
}

impl CompletionCode {
    /// The code for a raw value.
    #[must_use]
    pub fn from_raw(v: u8) -> Self {
        use CompletionCode::*;
        match v {
            0 => Invalid,
            1 => Success,
            2 => DataBuffer,
            3 => Babble,
            4 => UsbTransaction,
            5 => Trb,
            6 => Stall,
            7 => Resource,
            8 => Bandwidth,
            9 => NoSlots,
            11 => SlotNotEnabled,
            12 => EndpointNotEnabled,
            13 => ShortPacket,
            17 => Parameter,
            19 => ContextState,
            21 => EventRingFull,
            22 => IncompatibleDevice,
            24 => CommandRingStopped,
            25 => CommandAborted,
            26 => Stopped,
            27 => StoppedLengthInvalid,
            28 => StoppedShortPacket,
            36 => SplitTransaction,
            v => Other(v),
        }
    }

    /// Whether a transfer with this code moved its data: success, or a
    /// short packet.
    #[must_use]
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Success | Self::ShortPacket)
    }
}
