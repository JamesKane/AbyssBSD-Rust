// SPDX-License-Identifier: BSD-2-Clause
//! What a host controller driver provides to the bus.

use alloc::sync::Arc;

use sched::Sleepable;
use usb::descriptor::Endpoint;
use usb::{Setup, Speed};

use crate::UsbError;
use crate::bus::Explorer;
use crate::transfer::{Rejected, Transfer};

/// A device's handle on its controller (the xHCI slot ID).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotId(pub u8);

/// A port's state, for a root port or a hub's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortState {
    /// A device is connected.
    pub connected: bool,
    /// The port is enabled (a USB 2 port after reset; a USB 3 port once
    /// its link trained).
    pub enabled: bool,
    /// The speed of what is connected, if known.
    pub speed: Option<Speed>,
    /// Some change bit is set (connect, enable, reset, over-current,
    /// link state).
    pub changed: bool,
    /// The connection changed since the last clear.
    pub connect_changed: bool,
    /// The port's enable changed since the last clear.
    pub enable_changed: bool,
    /// Over-current.
    pub over_current: bool,
}

/// For a low- or full-speed device behind a high-speed hub: the hub's
/// transaction translator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tt {
    /// The high-speed hub's slot.
    pub hub: SlotId,
    /// The hub port the device is on.
    pub port: u8,
    /// The hub has a TT per port.
    pub multi: bool,
}

/// What the controller needs to know to give a new device a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewDevice {
    /// Its speed.
    pub speed: Speed,
    /// Its route string.
    pub route: u32,
    /// The root port it is ultimately on.
    pub root_port: u8,
    /// Its transaction translator, if any.
    pub tt: Option<Tt>,
}

/// A device [`HostController::enable_device`] addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addressed {
    /// Its slot.
    pub slot: SlotId,
    /// The USB address the controller gave it.
    pub address: u8,
}

/// A hub's facts, for its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubInfo {
    /// Downstream ports.
    pub ports: u8,
    /// USB 2 high-speed hubs: TT think time (0 to 3).
    pub think_time: u8,
    /// A TT per port.
    pub multi_tt: bool,
}

/// A host controller, as the bus sees it. Root ports are numbered from 1.
/// Methods taking a [`Sleepable`] may sleep; the others may not, so a
/// completion can resubmit.
pub trait HostController: Send + Sync {
    /// The controller's name (`xhci0`), for messages.
    fn name(&self) -> &str;

    /// FreeBSD's description of the bus's speed for the `usbus` line:
    /// `5.0Gbps Super Speed USB v3.0`.
    fn bus_speed(&self) -> &str;

    /// The root hub's manufacturer string (FreeBSD's `sc_vendor`: `Intel`,
    /// `AMD`, or `(0x1b36)`), for its `ugen` and `uhub` lines.
    fn root_hub_vendor(&self) -> &str;

    /// The number of root ports.
    fn root_ports(&self) -> u8;

    /// Root port `port`'s state.
    fn root_port_state(&self, port: u8) -> PortState;

    /// Clears root port `port`'s change bits.
    fn root_port_clear(&self, port: u8);

    /// Resets root port `port` and waits for it to finish.
    ///
    /// # Errors
    ///
    /// If the reset does not complete, or the device left.
    fn root_port_reset(&self, s: &Sleepable, port: u8) -> Result<PortState, UsbError>;

    /// Disables root port `port`.
    fn root_port_disable(&self, port: u8);

    /// Where to report root port changes.
    fn set_explorer(&self, e: Arc<Explorer>);

    /// Gives a new device on a reset port a slot and an address, with its
    /// default control endpoint's max packet size the speed's default.
    ///
    /// # Errors
    ///
    /// If the controller has no slot, or the device did not take the
    /// address.
    fn enable_device(&self, s: &Sleepable, d: &NewDevice) -> Result<Addressed, UsbError>;

    /// Sets the default control endpoint's max packet size, once the
    /// device descriptor said.
    ///
    /// # Errors
    ///
    /// If the controller refused.
    fn set_max_packet0(&self, s: &Sleepable, slot: SlotId, max_packet: u16)
    -> Result<(), UsbError>;

    /// Configures the endpoints of the interfaces being selected (and the
    /// slot's hub fields for a hub).
    ///
    /// # Errors
    ///
    /// If the controller refused (bandwidth, resources).
    fn configure(
        &self,
        s: &Sleepable,
        slot: SlotId,
        endpoints: &[Endpoint],
        hub: Option<HubInfo>,
    ) -> Result<(), UsbError>;

    /// Gives configured bulk endpoints streams (USB 3.2 §8.12.1.4): up to
    /// `streams` each, as many as the controller and every endpoint's
    /// companion allow. Returns the count, stream IDs 1 to it; transfers
    /// then name one ([`Transfer::stream`]). Nothing may be queued on
    /// them.
    ///
    /// # Errors
    ///
    /// If an endpoint or the controller has no streams, or the
    /// controller refused.
    fn open_streams(
        &self,
        s: &Sleepable,
        slot: SlotId,
        endpoints: &[Endpoint],
        streams: u16,
    ) -> Result<u16, UsbError>;

    /// Frees the device's slot. Every transfer still outstanding on it
    /// completes with [`UsbError::Gone`].
    fn disable_device(&self, s: &Sleepable, slot: SlotId);

    /// Submits a transfer on endpoint `endpoint` (an endpoint address;
    /// 0 for the default control endpoint, which needs `setup`).
    ///
    /// # Errors
    ///
    /// The transfer back, if it cannot be queued.
    fn submit(
        &self,
        slot: SlotId,
        endpoint: u8,
        setup: Option<Setup>,
        t: Transfer,
    ) -> Result<(), Rejected>;

    /// Restarts a halted endpoint (after the device's halt was cleared).
    ///
    /// # Errors
    ///
    /// If the controller refused.
    fn reset_endpoint(&self, s: &Sleepable, slot: SlotId, endpoint: u8) -> Result<(), UsbError>;

    /// Cancels every transfer queued on an endpoint; each completes with
    /// [`UsbError::Cancelled`].
    fn abort(&self, s: &Sleepable, slot: SlotId, endpoint: u8);

    /// Cancels what is queued on one stream of an endpoint (each
    /// completes with [`UsbError::Cancelled`]); the other streams' go on.
    fn abort_stream(&self, s: &Sleepable, slot: SlotId, endpoint: u8, stream: u16);
}
