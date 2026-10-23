// SPDX-License-Identifier: BSD-2-Clause
//! Devices on a bus, and the interfaces drivers get as bus data.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use devtree::ids::{UsbIdent, UsbPlace};
use sched::Sleepable;
use usb::descriptor::{Configuration, DeviceDescriptor, Interface};
use usb::{Setup, Speed};
use vm::object::IoBuf;

use crate::UsbError;
use crate::bus::Bus;
use crate::hc::{HostController, SlotId, Tt};
use crate::transfer::{Rejected, Transfer, Waiter};

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
    gone: AtomicBool,
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
}

impl UsbDevice {
    pub(crate) fn new(i: DeviceInit) -> Self {
        Self {
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
        let slot = self.slot.ok_or(UsbError::Invalid)?;
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        control(&*self.hc, s, slot, setup, out)
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
        self.hc.submit(slot, 0, Some(setup), t)
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
        self.hc.submit(slot, endpoint, None, t)
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

    /// Cancels everything queued on an endpoint.
    pub fn abort(&self, s: &Sleepable, endpoint: u8) {
        if let Some(slot) = self.slot {
            self.hc.abort(s, slot, endpoint);
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
