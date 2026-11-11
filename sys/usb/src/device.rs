// SPDX-License-Identifier: BSD-2-Clause
//! Devices on a bus, and the interfaces drivers get as bus data.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use bus::DeviceId;
use devtree::ids::{UsbIdent, UsbPlace};
use sched::Sleepable;
use sync::SpinLock;
use usb::descriptor::{Configuration, DeviceDescriptor, Endpoint, Interface};
use usb::{Setup, Speed};
use vm::object::IoBuf;

use crate::UsbError;
use crate::bus::Bus;
use crate::hc::{HostController, SlotId, Tt};
use crate::transfer::{Rejected, Stats, Transfer, Waiter};

/// How long a control request may take (FreeBSD's `USB_DEFAULT_TIMEOUT`
/// is 1 s per stage; descriptor reads get 5 s in all).
pub const CONTROL_TIMEOUT_MS: u32 = 5000;

/// A device on a bus.
pub struct UsbDevice {
    pub(crate) hc: Arc<dyn HostController>,
    pub(crate) bus_ref: Weak<Bus>,
    /// Its transaction translator, if it is low or full speed behind a
    /// high-speed hub.
    pub(crate) tt: Option<Tt>,
    /// `None` for the root hub, which has no slot.
    pub(crate) slot: Option<SlotId>,
    /// The bus unit (`usbusN`).
    pub bus: u32,
    /// Its index on the bus: `ugenB.I`, and the `devaddr` of its
    /// location.
    pub index: u8,
    /// Its USB address: the `addr` of its description.
    pub address: u8,
    /// Its speed.
    pub speed: Speed,
    /// The parent hub's index (0 for the root hub itself).
    pub hub_index: u8,
    /// The parent hub's port.
    pub port: u8,
    /// Its route string.
    pub route: u32,
    /// The root port it is on.
    pub root_port: u8,
    /// Hub tiers above it, below the root hub (0 on a root port).
    pub depth: u8,
    /// Its device descriptor.
    pub desc: DeviceDescriptor,
    /// Its active configuration.
    pub config: Configuration,
    /// The configuration descriptor's bytes, for class descriptors.
    pub raw_config: Vec<u8>,
    /// The manufacturer string, or FreeBSD's stand-in (`vendor 0x0627`).
    pub manufacturer: String,
    /// The product string, or FreeBSD's stand-in (`product 0x0001`).
    pub product: String,
    /// The serial number string, or empty.
    pub serial: String,
    /// The device descriptor's bytes, as the device sent them.
    pub raw_desc: [u8; 18],
    /// When it was plugged in (FreeBSD's `plugtime`, in ticks of a
    /// millisecond): libusb tells a device from its successor by it. A
    /// re-enumeration keeps it.
    pub plugtime: u32,
    /// The power its configuration draws, in mA (`bMaxPower` × 2).
    pub power: u16,
    /// Its configuration is self-powered (and, if it can be either, the
    /// device said so).
    pub self_powered: bool,
    /// A hub's downstream ports; 0 for other devices.
    pub nports: u8,
    /// The parent hub's USB address (0 for the root hub).
    pub hub_address: u8,
    /// The ports from the root hub down to it (empty for the root hub).
    pub port_path: Vec<u8>,
    /// Its requests, by transfer type.
    pub(crate) stats: Arc<Stats>,
    /// `USB_POWER_MODE_*`.
    pub(crate) power_mode: AtomicU8,
    /// The configuration index: 0, or `USB_UNCONFIG_INDEX` once
    /// unconfigured (powered off, or `USB_SET_CONFIG` to none).
    pub(crate) config_index: AtomicU8,
    /// What the explore thread is to do to it ([`Pending`]).
    pub(crate) pending: AtomicU8,
    /// For [`Pending::SetConfig`]: the index.
    pub(crate) next_config: AtomicU8,
    /// Its interfaces' nodes, by interface index; `None` once a driver
    /// was detached from userland (`USB_IFACE_DRIVER_DETACH`).
    pub(crate) ifaces: SpinLock<Vec<Option<DeviceId>>>,
    /// The transfer type of each endpoint, by `(address & 0xf) | in << 4`.
    ep_types: [u8; 32],
    /// Tells it from a device plugged in later at the same place; a
    /// re-enumeration keeps it.
    pub session: u64,
    slot_freed: AtomicBool,
    gone: AtomicBool,
}

/// What the explore thread is asked to do to a device (FreeBSD's
/// `re_enumerate_wait`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Pending {
    /// Nothing (`USB_RE_ENUM_DONE`).
    Done = 0,
    /// Reset it and enumerate it again (`USB_RE_ENUM_START`).
    Start = 1,
    /// Unconfigure it and disable its port (`USB_RE_ENUM_PWR_OFF`).
    PowerOff = 2,
    /// Select configuration [`UsbDevice::next_config`]
    /// (`USB_RE_ENUM_SET_CONFIG`).
    SetConfig = 3,
}

impl Pending {
    fn of(v: u8) -> Self {
        match v {
            1 => Self::Start,
            2 => Self::PowerOff,
            3 => Self::SetConfig,
            _ => Self::Done,
        }
    }
}

impl core::fmt::Debug for UsbDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UsbDevice")
            .field("ugen", &self.ugen())
            .field("speed", &self.speed)
            .finish_non_exhaustive()
    }
}

/// The fields of a new device, before it is shared.
pub(crate) struct DeviceInit {
    pub hc: Arc<dyn HostController>,
    pub bus_ref: Weak<Bus>,
    pub tt: Option<Tt>,
    pub slot: Option<SlotId>,
    pub bus: u32,
    pub index: u8,
    pub address: u8,
    pub speed: Speed,
    pub hub_index: u8,
    pub port: u8,
    pub route: u32,
    pub root_port: u8,
    pub depth: u8,
    pub desc: DeviceDescriptor,
    pub config: Configuration,
    pub raw_config: Vec<u8>,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    pub raw_desc: [u8; 18],
    pub plugtime: u32,
    pub session: u64,
    pub power: u16,
    pub self_powered: bool,
    pub nports: u8,
    pub hub_address: u8,
    pub port_path: Vec<u8>,
    /// `USB_POWER_MODE_*` to start in.
    pub power_mode: u8,
}

/// FreeBSD's `ticks` (a millisecond each): a device's `plugtime`.
pub(crate) fn ticks() -> u32 {
    // Wraps, as FreeBSD's does.
    u32::try_from(sched::now_ns() / 1_000_000 % (1 << 32)).unwrap_or(0)
}

/// The transfer types of a configuration's endpoints, by
/// `(address & 0xf) | in << 4` (control for the default endpoint).
fn ep_types(c: &Configuration) -> [u8; 32] {
    let mut t = [0; 32];
    for e in c.interfaces.iter().flat_map(|i| &i.endpoints) {
        t[ep_slot(e.address)] = e.attributes & 3;
    }
    t
}

fn ep_slot(address: u8) -> usize {
    usize::from(address & 0xf) | usize::from(address >> 7) << 4
}

impl UsbDevice {
    pub(crate) fn new(i: DeviceInit) -> Self {
        let iface_count = i.config.defaults().count();
        Self {
            ep_types: ep_types(&i.config),
            raw_desc: i.raw_desc,
            plugtime: i.plugtime,
            power: i.power,
            self_powered: i.self_powered,
            nports: i.nports,
            hub_address: i.hub_address,
            port_path: i.port_path,
            stats: Arc::new(Stats::default()),
            power_mode: AtomicU8::new(i.power_mode),
            config_index: AtomicU8::new(0),
            pending: AtomicU8::new(Pending::Done as u8),
            next_config: AtomicU8::new(0),
            ifaces: SpinLock::new(vec![None; iface_count]),
            session: i.session,
            slot_freed: AtomicBool::new(false),
            hc: i.hc,
            bus_ref: i.bus_ref,
            tt: i.tt,
            slot: i.slot,
            bus: i.bus,
            index: i.index,
            address: i.address,
            speed: i.speed,
            hub_index: i.hub_index,
            port: i.port,
            route: i.route,
            root_port: i.root_port,
            depth: i.depth,
            desc: i.desc,
            config: i.config,
            raw_config: i.raw_config,
            manufacturer: i.manufacturer,
            product: i.product,
            serial: i.serial,
            gone: AtomicBool::new(false),
        }
    }

    /// Its slot, if it is not the root hub.
    #[must_use]
    pub fn slot(&self) -> Option<SlotId> {
        self.slot
    }

    /// The controller.
    #[must_use]
    pub fn hc(&self) -> &Arc<dyn HostController> {
        &self.hc
    }

    /// `ugenB.I`.
    #[must_use]
    pub fn ugen(&self) -> String {
        alloc::format!("ugen{}.{}", self.bus, self.index)
    }

    /// Whether it was unplugged.
    #[must_use]
    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// Its configuration index (`curr_config_index`): 0, or
    /// `USB_UNCONFIG_INDEX`.
    #[must_use]
    pub fn config_index(&self) -> u8 {
        self.config_index.load(Ordering::Acquire)
    }

    /// Whether a configuration is selected.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.config_index() != abi::usb::USB_UNCONFIG_INDEX
    }

    /// `USB_POWER_MODE_*`.
    #[must_use]
    pub fn power_mode(&self) -> u8 {
        self.power_mode.load(Ordering::Acquire)
    }

    pub(crate) fn set_power_mode(&self, m: u8) {
        self.power_mode.store(m, Ordering::Release);
    }

    /// What the explore thread has yet to do to it.
    pub(crate) fn pending(&self) -> Pending {
        Pending::of(self.pending.load(Ordering::Acquire))
    }

    /// Asks the explore thread to do `p` unless something else is asked
    /// already; `false` if it is.
    pub(crate) fn ask(&self, p: Pending) -> bool {
        self.pending
            .compare_exchange(
                Pending::Done as u8,
                p as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn done(&self) {
        self.pending.store(Pending::Done as u8, Ordering::Release);
    }

    /// The configuration index [`Pending::SetConfig`] selects.
    pub(crate) fn next_config(&self) -> u8 {
        self.next_config.load(Ordering::Acquire)
    }

    /// It has no configuration now.
    pub(crate) fn unconfigured(&self) {
        self.config_index
            .store(abi::usb::USB_UNCONFIG_INDEX, Ordering::Release);
    }

    /// Its slot was given back to the controller (by a re-enumeration);
    /// it must not be freed again.
    pub(crate) fn slot_freed(&self) {
        self.slot_freed.store(true, Ordering::Release);
    }

    /// Whether its slot is still its own.
    pub(crate) fn owns_slot(&self) -> bool {
        !self.slot_freed.load(Ordering::Acquire)
    }

    /// The endpoint at `address` in its configuration.
    #[must_use]
    pub fn endpoint(&self, address: u8) -> Option<&Endpoint> {
        if !self.is_configured() {
            return None;
        }
        self.config
            .interfaces
            .iter()
            .flat_map(|i| &i.endpoints)
            .find(|e| e.address == address)
    }

    /// Its requests by transfer type: successes, then failures.
    #[must_use]
    pub fn stats(&self) -> ([u32; 4], [u32; 4]) {
        self.stats.get()
    }

    /// `t`, counted against endpoint `address`'s type.
    fn counted(&self, address: u8, mut t: Transfer) -> Transfer {
        let kind = if address & 0x7f == 0 {
            0
        } else {
            self.ep_types[ep_slot(address)]
        };
        t.stats = Some((Arc::clone(&self.stats), kind));
        t
    }

    /// The interface node at `index`, if a driver may have it.
    #[must_use]
    pub fn interface_node(&self, index: u8) -> Option<DeviceId> {
        self.ifaces
            .lock()
            .get(usize::from(index))
            .copied()
            .flatten()
    }

    pub(crate) fn set_gone(&self) {
        self.gone.store(true, Ordering::Release);
    }

    /// FreeBSD's description (`usb_devinfo`).
    #[must_use]
    pub fn description(&self) -> String {
        let d = &self.desc;
        let (u, r) = (d.usb.to_be_bytes(), d.release.to_be_bytes());
        if d.class == 0xff {
            alloc::format!(
                "{} {}, rev {:x}.{:02x}/{:x}.{:02x}, addr {}",
                self.manufacturer,
                self.product,
                u[0],
                u[1],
                r[0],
                r[1],
                self.address
            )
        } else {
            alloc::format!(
                "{} {}, class {}/{}, rev {:x}.{:02x}/{:x}.{:02x}, addr {}",
                self.manufacturer,
                self.product,
                d.class,
                d.subclass,
                u[0],
                u[1],
                r[0],
                r[1],
                self.address
            )
        }
    }

    /// Prints `name: <description> on usbusB`, as FreeBSD's
    /// `device_set_usb_desc` does at the start of a USB driver's attach
    /// (the framework's own line is the bare `name on uhubN`; see
    /// [`bus::Announce::Bare`]).
    pub fn announce(&self, name: &str, unit: u32) {
        ::bus::manager().log(format_args!(
            "{name}{unit}: <{}> on usbus{}",
            self.description(),
            self.bus
        ));
    }

    /// Runs a control request on the default endpoint and waits for it.
    /// `out` is the data stage of an OUT request; an IN request reads up
    /// to `setup.length` bytes and returns them.
    ///
    /// # Errors
    ///
    /// The transfer's error.
    pub fn control(
        &self,
        s: &Sleepable,
        setup: Setup,
        out: Option<&[u8]>,
    ) -> Result<Vec<u8>, UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        let Some(slot) = self.slot else {
            // The root hub: the controller driver's emulation
            // (`xhci_roothub_exec`), which moves no data out.
            if !setup.is_in() && setup.length != 0 {
                return Err(UsbError::Invalid);
            }
            let mut v = usb::ugen::root_hub_request(&setup, &self.manufacturer, 1)
                .ok_or(UsbError::Stalled)?;
            v.truncate(usize::from(setup.length));
            return Ok(v);
        };
        let w = Waiter::new();
        let buf = match out {
            Some(d) if !setup.is_in() => d[..d.len().min(usize::from(setup.length))].to_vec(),
            _ if setup.is_in() => vec![0; usize::from(setup.length)],
            _ => Vec::new(),
        };
        let t = self.counted(0, w.transfer(IoBuf::from(buf), CONTROL_TIMEOUT_MS));
        self.hc
            .submit(slot, 0, Some(setup), t)
            .map_err(|r| r.error)?;
        let (buf, r) = w.wait(s);
        let n = r?;
        let IoBuf::Owned(mut v) = buf else {
            return Err(UsbError::Invalid);
        };
        v.truncate(n);
        Ok(v)
    }

    /// Starts a control request without waiting: `t` carries the data
    /// stage (empty for none) and its completion. For callers that cannot
    /// sleep (a keyboard's LEDs).
    ///
    /// # Errors
    ///
    /// The transfer back, if it cannot be queued.
    pub fn control_async(&self, setup: Setup, t: Transfer) -> Result<(), Rejected> {
        let Some(slot) = self.slot else {
            return Err(Rejected {
                transfer: t,
                error: UsbError::Invalid,
            });
        };
        if self.is_gone() {
            return Err(Rejected {
                transfer: t,
                error: UsbError::Gone,
            });
        }
        self.hc.submit(slot, 0, Some(setup), self.counted(0, t))
    }

    /// Submits a transfer on endpoint `endpoint` (an address).
    ///
    /// # Errors
    ///
    /// The transfer back, if it cannot be queued.
    pub fn submit(&self, endpoint: u8, t: Transfer) -> Result<(), Rejected> {
        let Some(slot) = self.slot else {
            return Err(Rejected {
                transfer: t,
                error: UsbError::Invalid,
            });
        };
        if self.is_gone() {
            return Err(Rejected {
                transfer: t,
                error: UsbError::Gone,
            });
        }
        self.hc
            .submit(slot, endpoint, None, self.counted(endpoint, t))
    }

    /// Clears a halted endpoint: `CLEAR_FEATURE(ENDPOINT_HALT)` on the
    /// device, then the controller's side.
    ///
    /// # Errors
    ///
    /// If either step fails.
    pub fn clear_halt(&self, s: &Sleepable, endpoint: u8) -> Result<(), UsbError> {
        let slot = self.slot.ok_or(UsbError::Invalid)?;
        self.hc.reset_endpoint(s, slot, endpoint)?;
        self.control(s, Setup::clear_halt(endpoint), None)
            .map(|_| ())
    }

    /// Gives bulk endpoints streams: the count, stream IDs 1 to it
    /// ([`HostController::open_streams`]).
    ///
    /// # Errors
    ///
    /// If they cannot have streams.
    pub fn open_streams(
        &self,
        s: &Sleepable,
        endpoints: &[Endpoint],
        streams: u16,
    ) -> Result<u16, UsbError> {
        let slot = self.slot.ok_or(UsbError::Invalid)?;
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        self.hc.open_streams(s, slot, endpoints, streams)
    }

    /// Cancels everything queued on an endpoint.
    pub fn abort(&self, s: &Sleepable, endpoint: u8) {
        if let Some(slot) = self.slot {
            self.hc.abort(s, slot, endpoint);
        }
    }

    /// Cancels what is queued on one stream of an endpoint.
    pub fn abort_stream(&self, s: &Sleepable, endpoint: u8, stream: u16) {
        if let Some(slot) = self.slot {
            self.hc.abort_stream(s, slot, endpoint, stream);
        }
    }
}

/// An interface of a device: what a USB driver gets as bus data.
#[derive(Debug)]
pub struct UsbInterface {
    /// The device.
    pub device: Arc<UsbDevice>,
    /// The interface's default setting, with its endpoints.
    pub desc: Interface,
}

impl UsbInterface {
    /// The identity a driver matches on.
    #[must_use]
    pub fn ident(&self) -> UsbIdent {
        let d = &self.device.desc;
        UsbIdent {
            vendor: d.vendor,
            product: d.product,
            release: d.release,
            class: d.class,
            subclass: d.subclass,
            protocol: d.protocol,
            if_class: self.desc.class,
            if_subclass: self.desc.subclass,
            if_protocol: self.desc.protocol,
            interface: self.desc.number,
        }
    }

    /// Where it is, and its device's serial number and description.
    #[must_use]
    pub fn place(&self) -> UsbPlace {
        let d = &self.device;
        UsbPlace {
            bus: d.bus,
            hub_addr: d.hub_index,
            port: d.port,
            dev_addr: d.index,
            ugen: d.ugen(),
            serial: d.serial.clone(),
            desc: d.description(),
        }
    }

    /// The class descriptors that followed endpoint `e`'s, as `(type,
    /// bytes)`.
    pub fn endpoint_descriptors<'a>(
        &'a self,
        e: &'a Endpoint,
    ) -> impl Iterator<Item = (u8, &'a [u8])> + 'a {
        e.extra
            .iter()
            .filter_map(|(t, r)| Some((*t, self.device.raw_config.get(r.clone())?)))
    }

    /// The class descriptors that followed the interface descriptor, as
    /// `(type, bytes)`.
    pub fn class_descriptors(&self) -> impl Iterator<Item = (u8, &[u8])> {
        self.desc
            .extra
            .iter()
            .filter_map(|(t, r)| Some((*t, self.device.raw_config.get(r.clone())?)))
    }
}

/// Runs a control request on `slot`'s default endpoint and waits for it,
/// as [`UsbDevice::control`] does: for enumeration, before there is a
/// device.
pub(crate) fn control(
    hc: &dyn HostController,
    s: &Sleepable,
    slot: SlotId,
    setup: Setup,
    out: Option<&[u8]>,
) -> Result<Vec<u8>, UsbError> {
    let buf = match out {
        Some(d) if !setup.is_in() => d[..d.len().min(usize::from(setup.length))].to_vec(),
        _ if setup.is_in() => vec![0; usize::from(setup.length)],
        _ => Vec::new(),
    };
    let w = Waiter::new();
    let t = w.transfer(IoBuf::from(buf), CONTROL_TIMEOUT_MS);
    hc.submit(slot, 0, Some(setup), t).map_err(|r| r.error)?;
    let (buf, r) = w.wait(s);
    let n = r?;
    let IoBuf::Owned(mut v) = buf else {
        return Err(UsbError::Invalid);
    };
    v.truncate(n);
    Ok(v)
}
