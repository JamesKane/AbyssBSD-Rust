// SPDX-License-Identifier: BSD-2-Clause
//! Hubs: `uhub`, on the root hub and on external hubs alike, and device
//! enumeration on their ports (USB.md §5.2, §5.3).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use bus::{
    AttachCx, AttachError, DetachReason, DeviceId, Driver, DriverDecl, Ident, Ids, StartCx, StopCx,
    StopReason,
};
use devtree::ids::UsbId;
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use usb::descriptor::{
    self, Configuration, DeviceDescriptor, Endpoint, LANG_EN_US, TransferType, class, kind,
};
use usb::hub::{self as h, HubDescriptor, PortSpeed, PortStatus, feature};
use usb::{Setup, Speed};
use vm::object::IoBuf;

use crate::UsbError;
use crate::bus::{Bus, Explorer};
use crate::device::{self, DeviceInit, UsbDevice, UsbInterface};
use crate::hc::{HostController, HubInfo, NewDevice, PortState, SlotId, Tt};
use crate::transfer::Transfer;

/// A hub's ports, root or external.
pub(crate) trait Ports: Send + Sync {
    /// The number of ports.
    fn count(&self) -> u8;
    /// Port `port`'s state.
    fn state(&self, s: &Sleepable, port: u8) -> Result<PortState, UsbError>;
    /// Clears its change bits.
    fn clear(&self, s: &Sleepable, port: u8);
    /// Resets it, waiting for the reset to finish.
    fn reset(&self, s: &Sleepable, port: u8) -> Result<PortState, UsbError>;
    /// Disables it.
    fn disable(&self, s: &Sleepable, port: u8);
}

struct RootPorts(Arc<dyn HostController>);

impl Ports for RootPorts {
    fn count(&self) -> u8 {
        self.0.root_ports()
    }
    fn state(&self, _: &Sleepable, port: u8) -> Result<PortState, UsbError> {
        Ok(self.0.root_port_state(port))
    }
    fn clear(&self, _: &Sleepable, port: u8) {
        self.0.root_port_clear(port);
    }
    fn reset(&self, s: &Sleepable, port: u8) -> Result<PortState, UsbError> {
        self.0.root_port_reset(s, port)
    }
    fn disable(&self, _: &Sleepable, port: u8) {
        self.0.root_port_disable(port);
    }
}

/// An external hub's ports, through hub-class requests.
struct HubPorts {
    dev: Arc<UsbDevice>,
    ports: u8,
    super_speed: bool,
}

/// How long a hub port reset may take (USB 2.0 §11.5.1.5 allows 10-20
/// ms; FreeBSD waits up to `USB_PORT_RESET_DELAY` many times over).
const RESET_POLLS: u32 = 50;

impl HubPorts {
    fn status(&self, s: &Sleepable, port: u8) -> Result<PortStatus, UsbError> {
        let b = self.dev.control(s, h::get_port_status(port), None)?;
        let b: [u8; 4] = b
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or(UsbError::Protocol("short port status"))?;
        Ok(PortStatus::parse(b, self.super_speed))
    }
}

fn port_state(p: &PortStatus) -> PortState {
    PortState {
        connected: p.connected(),
        enabled: p.enabled(),
        speed: Some(match p.speed() {
            PortSpeed::Low => Speed::Low,
            PortSpeed::Full => Speed::Full,
            PortSpeed::High => Speed::High,
            PortSpeed::Super => Speed::Super,
        }),
        changed: p.change != 0,
        connect_changed: p.change & 1 != 0,
        over_current: p.status & (1 << 3) != 0,
    }
}

impl Ports for HubPorts {
    fn count(&self) -> u8 {
        self.ports
    }
    fn state(&self, s: &Sleepable, port: u8) -> Result<PortState, UsbError> {
        self.status(s, port).map(|p| port_state(&p))
    }
    fn clear(&self, s: &Sleepable, port: u8) {
        if let Ok(p) = self.status(s, port) {
            for f in p.changes() {
                let _ = self.dev.control(s, h::clear_port_feature(port, f), None);
            }
        }
    }
    fn reset(&self, s: &Sleepable, port: u8) -> Result<PortState, UsbError> {
        self.dev
            .control(s, h::set_port_feature(port, feature::PORT_RESET), None)?;
        for _ in 0..RESET_POLLS {
            ::bus::manager().sleep_ms(s, 10);
            let p = self.status(s, port)?;
            if !p.connected() {
                return Err(UsbError::Gone);
            }
            if !p.resetting() && p.enabled() {
                let _ =
                    self.dev
                        .control(s, h::clear_port_feature(port, feature::C_PORT_RESET), None);
                // Reset recovery (USB 2.0 §7.1.7.5).
                ::bus::manager().sleep_ms(s, 10);
                return Ok(port_state(&p));
            }
        }
        Err(UsbError::TimedOut)
    }
    fn disable(&self, s: &Sleepable, port: u8) {
        let _ = self
            .dev
            .control(s, h::clear_port_feature(port, feature::PORT_ENABLE), None);
    }
}

/// What is on a port.
struct Attached {
    dev: Arc<UsbDevice>,
}

#[derive(Default)]
struct Port {
    attached: Option<Attached>,
    /// Enumeration failed; not retried until the connection changes.
    failed: bool,
}

/// A hub the explore thread walks.
pub struct Hub {
    /// The `uhub` node, the parent of its devices' interfaces.
    node: DeviceId,
    /// The hub as a device.
    dev: Arc<UsbDevice>,
    ports: Box<dyn Ports>,
    port_state: SpinLock<Vec<Port>>,
    /// Set by `uhub`'s stop: explore leaves it alone.
    dead: AtomicBool,
    /// The explore thread is in it.
    busy: AtomicBool,
    idle: WaitQueue,
}

impl core::fmt::Debug for Hub {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hub")
            .field("dev", &self.dev.ugen())
            .finish_non_exhaustive()
    }
}

/// Enumeration attempts per connection (FreeBSD tries a device's
/// enumeration a few times before giving up on the port).
const ATTEMPTS: u32 = 3;

impl Hub {
    fn is_root(&self) -> bool {
        self.dev.slot.is_none()
    }

    /// Checks every port, detaching what left and enumerating what came.
    pub(crate) fn explore(&self, s: &Sleepable, bus: &Arc<Bus>) {
        if self.dead.load(Ordering::Acquire) {
            return;
        }
        self.busy.store(true, Ordering::Release);
        for port in 1..=self.ports.count() {
            if self.dead.load(Ordering::Acquire) {
                break;
            }
            let Ok(st) = self.ports.state(s, port) else {
                continue;
            };
            if st.changed {
                self.ports.clear(s, port);
            }
            let (present, failed) = {
                let p = self.port_state.lock();
                p.get(usize::from(port - 1))
                    .map_or((false, true), |p| (p.attached.is_some(), p.failed))
            };
            if present && (!st.connected || st.connect_changed) {
                self.detach_port(s, bus, port);
            }
            if (st.connect_changed || !st.connected)
                && let Some(p) = self.port_state.lock().get_mut(usize::from(port - 1))
            {
                p.failed = false;
            }
            let failed = failed && !st.connect_changed;
            if st.connected && !st.over_current && !failed && !self.has_device(port) {
                let mut ok = false;
                for _ in 0..ATTEMPTS {
                    match self.attach_port(s, bus, port) {
                        Ok(()) => {
                            ok = true;
                            break;
                        }
                        Err(UsbError::Gone) => break,
                        Err(e) => ::bus::manager().log(format_args!(
                            "uhub: {}: port {port}: enumeration failed: {e:?}",
                            self.dev.ugen()
                        )),
                    }
                }
                if !ok {
                    self.ports.disable(s, port);
                    if let Some(p) = self.port_state.lock().get_mut(usize::from(port - 1)) {
                        p.failed = true;
                    }
                }
            }
        }
        self.busy.store(false, Ordering::Release);
        self.idle.wake_all();
    }

    fn has_device(&self, port: u8) -> bool {
        self.port_state
            .lock()
            .get(usize::from(port - 1))
            .is_some_and(|p| p.attached.is_some())
    }

    fn detach_port(&self, s: &Sleepable, bus: &Bus, port: u8) {
        let a = self
            .port_state
            .lock()
            .get_mut(usize::from(port - 1))
            .and_then(|p| p.attached.take());
        if let Some(a) = a {
            gone(s, bus, &a.dev, true);
            // Its interfaces' nodes are this hub's children whose location
            // names the port.
            let mgr = ::bus::manager();
            for c in mgr.children(self.node, s) {
                let here = matches!(
                    mgr.ident(c, s),
                    Some(Ident::Usb(_, p)) if p.port == port && p.dev_addr == a.dev.index
                );
                if here {
                    let _ = mgr.detach(c, DetachReason::Gone, s);
                }
            }
            finish(s, bus, &a.dev);
        }
    }

    /// Enumerates the device on `port`.
    fn attach_port(&self, s: &Sleepable, bus: &Arc<Bus>, port: u8) -> Result<(), UsbError> {
        let hc = &bus.hc;
        let mut st = self.ports.state(s, port)?;
        if !st.connected {
            return Err(UsbError::Gone);
        }
        // A USB 2 port needs a reset to be enabled; a USB 3 port is once
        // its link trains.
        if !st.enabled {
            st = self.ports.reset(s, port)?;
        }
        let speed = st.speed.ok_or(UsbError::Protocol("unknown port speed"))?;
        let parent = &self.dev;
        let (route, root_port, depth) = if self.is_root() {
            (0, port, 0)
        } else {
            (
                usb::route(parent.route, parent.depth, port),
                parent.root_port,
                parent.depth + 1,
            )
        };
        let tt = match (speed, parent.slot) {
            (Speed::Low | Speed::Full, Some(hub)) if parent.speed == Speed::High => Some(Tt {
                hub,
                port,
                multi: parent.desc.protocol == 2,
            }),
            _ => parent.tt,
        };
        let slot = hc.enable_device(
            s,
            &NewDevice {
                speed,
                route,
                root_port,
                tt,
            },
        )?;
        let r = enumerate(s, &**hc, slot, speed);
        let e = match r {
            Ok(e) => e,
            Err(err) => {
                hc.disable_device(s, slot);
                return Err(err);
            }
        };
        let Some(index) = bus.alloc_index() else {
            hc.disable_device(s, slot);
            return Err(UsbError::NoMemory);
        };
        let dev = Arc::new(UsbDevice::new(DeviceInit {
            hc: Arc::clone(hc),
            bus_ref: Arc::downgrade(bus),
            tt,
            slot: Some(slot),
            bus: bus.unit,
            index,
            speed,
            hub_index: parent.index,
            port,
            route,
            root_port,
            depth,
            desc: e.desc,
            config: e.config,
            raw_config: e.raw_config,
            manufacturer: e.manufacturer,
            product: e.product,
            serial: e.serial,
        }));
        ::bus::manager().log(format_args!(
            "{}: <{} {}> at usbus{}",
            dev.ugen(),
            dev.manufacturer,
            dev.product,
            bus.unit
        ));
        if let Some(p) = self.port_state.lock().get_mut(usize::from(port - 1)) {
            p.attached = Some(Attached {
                dev: Arc::clone(&dev),
            });
        }
        let mut ifaces: Vec<_> = dev.config.defaults().cloned().collect();
        ifaces.sort_by_key(|i| i.number);
        for desc in ifaces {
            let i = UsbInterface {
                device: Arc::clone(&dev),
                desc,
            };
            ::bus::manager().add_child(
                s,
                self.node,
                Ident::Usb(i.ident(), i.place()),
                Some(Arc::new(i)),
                Vec::new(),
                &[],
            );
        }
        Ok(())
    }

    /// For `uhub`'s stop: no more exploring, and every device below goes.
    fn shut(&self, s: &Sleepable) {
        self.dead.store(true, Ordering::Release);
        self.idle
            .wait_until(s, || !self.busy.load(Ordering::Acquire));
        let Some(bus) = self.dev.bus_ref.upgrade() else {
            return;
        };
        let devs: Vec<Arc<UsbDevice>> = self
            .port_state
            .lock()
            .iter_mut()
            .filter_map(|p| p.attached.take().map(|a| a.dev))
            .collect();
        for d in devs {
            gone(s, &bus, &d, false);
            finish(s, &bus, &d);
        }
    }
}

/// Marks a device gone and says so.
fn gone(_: &Sleepable, bus: &Bus, dev: &UsbDevice, unplugged: bool) {
    dev.set_gone();
    if unplugged {
        ::bus::manager().log(format_args!(
            "{}: <{} {}> at usbus{} (disconnected)",
            dev.ugen(),
            dev.manufacturer,
            dev.product,
            bus.unit
        ));
    }
}

/// Frees a gone device's slot and index, once its drivers are detached.
fn finish(s: &Sleepable, bus: &Bus, dev: &UsbDevice) {
    if let Some(slot) = dev.slot {
        bus.hc.disable_device(s, slot);
    }
    bus.free_index(dev.index);
}

/// What enumeration learned.
struct Enumerated {
    desc: DeviceDescriptor,
    config: Configuration,
    raw_config: Vec<u8>,
    manufacturer: String,
    product: String,
    serial: String,
}

/// Reads a device's descriptors and configures it (USB.md §5.2).
fn enumerate(
    s: &Sleepable,
    hc: &dyn HostController,
    slot: SlotId,
    speed: Speed,
) -> Result<Enumerated, UsbError> {
    let ctl = |setup: Setup| device::control(hc, s, slot, setup, None);
    let short = |_| UsbError::Protocol("bad descriptor");
    // The first eight bytes say the default endpoint's max packet size.
    let b = ctl(Setup::get_descriptor(kind::DEVICE, 0, 0, 8))?;
    let first = DeviceDescriptor::parse_prefix(&b).map_err(short)?;
    let mp0 = first.ep0_max_packet();
    if mp0 != speed.default_max_packet() && mp0 != 0 {
        hc.set_max_packet0(s, slot, mp0)?;
    }
    let len = u16::try_from(DeviceDescriptor::LEN).unwrap_or(18);
    let b = ctl(Setup::get_descriptor(kind::DEVICE, 0, 0, len))?;
    let desc = DeviceDescriptor::parse(&b).map_err(short)?;
    let head_len = u16::try_from(descriptor::CONFIGURATION_LEN).unwrap_or(9);
    let head = ctl(Setup::get_descriptor(kind::CONFIGURATION, 0, 0, head_len))?;
    let total = descriptor::configuration_total(&head).map_err(short)?;
    let raw_config = ctl(Setup::get_descriptor(kind::CONFIGURATION, 0, 0, total))?;
    let config = Configuration::parse(&raw_config).map_err(short)?;

    let string = |index: u8| -> Option<String> {
        if index == 0 {
            return None;
        }
        let h = ctl(Setup::get_descriptor(kind::STRING, index, LANG_EN_US, 2)).ok()?;
        let n = u16::from(*h.first()?);
        let b = ctl(Setup::get_descriptor(kind::STRING, index, LANG_EN_US, n)).ok()?;
        descriptor::string(&b)
            .ok()
            .map(|t| String::from(t.trim()))
            .filter(|t| !t.is_empty())
    };
    let manufacturer =
        string(desc.manufacturer).unwrap_or_else(|| alloc::format!("vendor 0x{:04x}", desc.vendor));
    let product = string(desc.product_string)
        .unwrap_or_else(|| alloc::format!("product 0x{:04x}", desc.product));
    let serial = string(desc.serial).unwrap_or_default();

    let hub = if desc.class == class::HUB {
        let b = ctl(h::get_hub_descriptor(speed.is_super(), 32))?;
        let d = HubDescriptor::parse(&b).map_err(short)?;
        Some(HubInfo {
            ports: d.ports,
            think_time: d.tt_think_time(),
            multi_tt: desc.protocol == 2,
        })
    } else {
        None
    };
    let endpoints: Vec<Endpoint> = config
        .defaults()
        .flat_map(|i| i.endpoints.iter().cloned())
        .filter(|e| e.transfer_type() != TransferType::Control)
        .collect();
    hc.configure(s, slot, &endpoints, hub)?;
    ctl(Setup::set_configuration(config.value))?;
    Ok(Enumerated {
        desc,
        config,
        raw_config,
        manufacturer,
        product,
        serial,
    })
}

/// The `uhub` driver: the root hub and external hubs.
pub struct Uhub {
    hub: Arc<Hub>,
    unit: u32,
    removable: u8,
    self_powered: bool,
    /// External hubs: the status change endpoint.
    status_ep: Option<(u8, u16)>,
}

impl core::fmt::Debug for Uhub {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Uhub").field("unit", &self.unit).finish()
    }
}

/// Arms an external hub's status change endpoint: each completion kicks
/// the explore thread and rearms it, until the hub goes.
fn arm(dev: Arc<UsbDevice>, ep: u8, len: u16, e: Arc<Explorer>) {
    let d = Arc::clone(&dev);
    let t = Transfer::new(
        IoBuf::from(alloc::vec![0; usize::from(len)]),
        0,
        move |_, r| {
            e.kick();
            if r.is_ok() && !d.is_gone() {
                arm(d, ep, len, e);
            }
        },
    );
    let _ = dev.submit(ep, t);
}

impl Driver for Uhub {
    const NAME: &'static str = "uhub";
    const IDS: Ids = Ids::Usb(&[UsbId::interface(class::HUB)]);

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Usb(_, p) => p.desc.clone(),
            _ => String::new(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let iface = cx
            .bus_data_arc::<UsbInterface>()
            .ok_or(AttachError::Decline)?;
        let dev = Arc::clone(&iface.device);
        let s = cx.sleepable();
        let (ports, count, removable, self_powered, status_ep): (Box<dyn Ports>, _, _, _, _) =
            if dev.slot.is_none() {
                let n = dev.hc.root_ports();
                (Box::new(RootPorts(Arc::clone(&dev.hc))), n, n, true, None)
            } else {
                let ss = dev.speed.is_super();
                let b = dev
                    .control(s, h::get_hub_descriptor(ss, 32), None)
                    .map_err(|_| AttachError::Device("no hub descriptor"))?;
                let d = HubDescriptor::parse(&b)
                    .map_err(|_| AttachError::Device("bad hub descriptor"))?;
                if ss {
                    dev.control(s, h::set_hub_depth(dev.depth), None)
                        .map_err(|_| AttachError::Device("SET_HUB_DEPTH failed"))?;
                }
                for p in 1..=d.ports {
                    let _ = dev.control(s, h::set_port_feature(p, feature::PORT_POWER), None);
                }
                ::bus::manager().sleep_ms(s, u64::from(d.power_on_ms.max(100)));
                let ep = iface
                    .desc
                    .endpoints
                    .iter()
                    .find(|e| e.is_in() && e.transfer_type() == TransferType::Interrupt)
                    .map(|e| (e.address, e.max_packet()));
                let self_powered = dev.config.attributes & 0x40 != 0;
                (
                    Box::new(HubPorts {
                        dev: Arc::clone(&dev),
                        ports: d.ports,
                        super_speed: ss,
                    }),
                    d.ports,
                    d.ports,
                    self_powered,
                    ep,
                )
            };
        let hub = Arc::new(Hub {
            node: cx_node(cx),
            dev,
            ports,
            port_state: SpinLock::new((0..count).map(|_| Port::default()).collect()),
            dead: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            idle: WaitQueue::new(),
        });
        Ok(Self {
            hub,
            unit: cx.unit(),
            removable,
            self_powered,
            status_ep,
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let n = self.hub.ports.count();
        ::bus::manager().log(format_args!(
            "uhub{}: {} port{} with {} removable, {} powered",
            self.unit,
            n,
            if n == 1 { "" } else { "s" },
            self.removable,
            if self.self_powered { "self" } else { "bus" }
        ));
        let Some(bus) = self.hub.dev.bus_ref.upgrade() else {
            return Err(AttachError::Device("bus gone"));
        };
        bus.hubs.lock().push(Arc::clone(&self.hub));
        if let Some((ep, len)) = self.status_ep {
            arm(
                Arc::clone(&self.hub.dev),
                ep,
                len,
                Arc::clone(&bus.explorer),
            );
        }
        bus.explorer.kick();
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        if let Some(bus) = self.hub.dev.bus_ref.upgrade() {
            bus.hubs.lock().retain(|h| !Arc::ptr_eq(h, &self.hub));
        }
        if let Some((ep, _)) = self.status_ep {
            self.hub.dev.abort(s, ep);
        }
        self.hub.shut(s);
    }
}

/// The node being attached.
fn cx_node(cx: &AttachCx<'_>) -> DeviceId {
    cx.node()
}

/// The declaration for the build's driver table.
pub const UHUB: DriverDecl = DriverDecl::of::<Uhub>();
