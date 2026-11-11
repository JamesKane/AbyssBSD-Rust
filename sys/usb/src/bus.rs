// SPDX-License-Identifier: BSD-2-Clause
//! `usbus`: one per controller. It owns the bus's device indices and its
//! explore thread, and adds the root hub.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bus::{
    AttachCx, AttachError, DeviceId, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use sched::{Sleepable, Thread, WaitQueue};
use sync::SpinLock;
use usb::Speed;
use usb::descriptor::{Configuration, DeviceDescriptor};

use crate::device::{DeviceInit, UsbDevice, UsbInterface};
use crate::hc::HostController;
use crate::hub::Hub;
use crate::ugen::fs::Pipe;

/// What a controller gives its `usbus` child as bus data.
pub struct UsbusData {
    /// The controller.
    pub hc: Arc<dyn HostController>,
}

impl core::fmt::Debug for UsbusData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UsbusData")
            .field("hc", &self.hc.name())
            .finish()
    }
}

/// Wakes the explore thread: a root port changed, or a hub's status
/// endpoint reported.
#[derive(Debug, Default)]
pub struct Explorer {
    pending: AtomicBool,
    stop: AtomicBool,
    queue: WaitQueue,
}

impl Explorer {
    /// Asks for an explore pass.
    pub fn kick(&self) {
        self.pending.store(true, Ordering::Release);
        self.queue.wake_all();
    }
}

/// The highest device index (FreeBSD's `USB_MAX_DEVICES` less one).
const MAX_INDEX: u8 = 127;

/// Every bus, by unit, for finding a device by its location
/// (`devclass_get_softc(usb_devclass_ptr, ...)`).
static BUSES: SpinLock<Vec<Weak<Bus>>> = SpinLock::new(Vec::new());

/// Tells a device from one plugged in later at the same place.
static SESSIONS: AtomicU64 = AtomicU64::new(1);

/// Woken as each bus's root hub has its driver ([`Bus::root_done`]).
static ROOTS: WaitQueue = WaitQueue::new();

/// A new device's session (kept by a re-enumeration).
pub(crate) fn new_session() -> u64 {
    // Unique only: nothing is ordered by it.
    SESSIONS.fetch_add(1, Ordering::Relaxed)
}

/// The device at index `index` of bus `bus` (`bus->devices[index]`).
pub(crate) fn device_at(bus: u32, index: u8) -> Option<Arc<UsbDevice>> {
    let b = BUSES
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .find(|b| b.unit == bus)?;
    b.devices.lock().get(usize::from(index))?.clone()
}

/// Cancels what is queued on endpoint `address` of `pipe` from the
/// explore thread of bus `bus` (an open closed without stopping its
/// transfers, where nothing may sleep).
pub(crate) fn defer_abort(bus: u32, pipe: Arc<dyn Pipe>, address: u8) {
    let b = BUSES
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .find(|b| b.unit == bus);
    if let Some(b) = b {
        b.aborts.lock().push((pipe, address));
        b.explorer.kick();
    }
}

/// A bus.
pub struct Bus {
    /// `usbusN`'s unit.
    pub unit: u32,
    pub(crate) hc: Arc<dyn HostController>,
    pub(crate) explorer: Arc<Explorer>,
    pub(crate) hubs: SpinLock<Vec<Arc<Hub>>>,
    /// Device indices in use; 1 is the root hub's.
    indices: SpinLock<u128>,
    /// The devices, by index.
    devices: SpinLock<Vec<Option<Arc<UsbDevice>>>>,
    /// Endpoints to cancel ([`defer_abort`]).
    aborts: SpinLock<Vec<(Arc<dyn Pipe>, u8)>>,
    /// Its root hub has its driver, or it never will (the bus stopped):
    /// the next bus's may come.
    root_done: AtomicBool,
    /// Its first exploring (the boot's) is not over: devices it finds go
    /// in `ugen`'s boot order.
    pub(crate) booting: AtomicBool,
}

impl core::fmt::Debug for Bus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bus").field("unit", &self.unit).finish()
    }
}

impl Bus {
    /// A free device index.
    pub(crate) fn alloc_index(&self) -> Option<u8> {
        let mut m = self.indices.lock();
        let i = (2..=MAX_INDEX).find(|&i| *m & (1 << i) == 0)?;
        *m |= 1 << i;
        Some(i)
    }

    pub(crate) fn free_index(&self, i: u8) {
        if i <= MAX_INDEX {
            *self.indices.lock() &= !(1u128 << i);
        }
    }

    /// Puts `dev` at its index (or replaces what is there: a device
    /// enumerated again).
    pub(crate) fn set_device(&self, dev: &Arc<UsbDevice>) {
        if let Some(d) = self.devices.lock().get_mut(usize::from(dev.index)) {
            *d = Some(Arc::clone(dev));
        }
    }

    /// Takes the device at `index` away.
    pub(crate) fn clear_device(&self, index: u8) {
        if let Some(d) = self.devices.lock().get_mut(usize::from(index)) {
            *d = None;
        }
    }

    /// Lets the next bus's root hub come.
    fn root_done(&self) {
        self.root_done.store(true, Ordering::Release);
        ROOTS.wake_all();
    }

    /// One pass over every hub, root first.
    fn explore(self: &Arc<Self>, s: &Sleepable) {
        let aborts = core::mem::take(&mut *self.aborts.lock());
        for (pipe, address) in aborts {
            pipe.abort(s, address);
        }
        let hubs = self.hubs.lock().clone();
        for h in hubs {
            h.explore(s, self);
        }
    }

    fn thread(self: Arc<Self>, node: DeviceId, s: &Sleepable) {
        // As FreeBSD's usb_bus_attach, which runs on the USB process once
        // the boot's device probe is done: the root hub, then exploring.
        let m = ::bus::manager();
        // Root waits for the first exploring (FreeBSD's `bus_roothold`,
        // taken at `usb_attach`).
        let hold = ::bus::root_mount_hold(alloc::format!("usbus{}", self.unit));
        m.wait_idle(s);
        let e = Arc::clone(&self.explorer);
        // Root hubs come one bus after another, in unit order, so that
        // their `ugen` links, `uhub` units, and lines are always in the
        // same order. (FreeBSD's per-bus explore threads race for it; on
        // a uniprocessor boot their order is the scheduler's.) Only the
        // root hub waits: the ports are explored in parallel.
        ROOTS.wait_until(s, || {
            e.stop.load(Ordering::Acquire)
                || BUSES
                    .lock()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .filter(|b| b.unit < self.unit)
                    .all(|b| b.root_done.load(Ordering::Acquire))
        });
        if e.stop.load(Ordering::Acquire) {
            self.root_done();
            return;
        }
        let root = Arc::new(self.root_hub());
        m.log(format_args!(
            "{}: <{} {}> at usbus{}",
            root.ugen(),
            root.manufacturer,
            root.product,
            self.unit
        ));
        self.set_device(&root);
        crate::ugen::device_added(&root, true);
        crate::ugen::endpoints_added(&root);
        let iface = UsbInterface {
            desc: root.config.interfaces[0].clone(),
            device: Arc::clone(&root),
        };
        let child = m.add_child(
            s,
            node,
            Ident::Usb(iface.ident(), iface.place()),
            Some(Arc::new(iface)),
            Vec::new(),
            &[],
        );
        if let Some(c) = root.ifaces.lock().get_mut(0) {
            *c = Some(child);
        }
        m.wait_settled(child, s);
        self.root_done();
        // The first pass, whatever has changed yet; then root may mount
        // (`usb_root_mount_rel` after `usb_bus_explore`).
        // The root hub's driver first: until it attaches there is nothing
        // to explore. A hub found in a pass is explored in the next, until
        // none is new (FreeBSD's `uhub_explore` goes down into them at
        // once).
        m.wait_settled(node, s);
        // Every bus's root hub before any bus's devices, as FreeBSD's
        // threads make them at boot.
        ROOTS.wait_until(s, || {
            e.stop.load(Ordering::Acquire)
                || BUSES
                    .lock()
                    .iter()
                    .filter_map(Weak::upgrade)
                    .all(|b| b.root_done.load(Ordering::Acquire))
        });
        loop {
            e.pending.store(false, Ordering::Release);
            let hubs = self.hubs.lock().len();
            self.explore(s);
            if self.hubs.lock().len() == hubs {
                break;
            }
        }
        self.booting.store(false, Ordering::Release);
        drop(hold);
        loop {
            e.queue.wait_until(s, || {
                e.pending.load(Ordering::Acquire) || e.stop.load(Ordering::Acquire)
            });
            if e.stop.load(Ordering::Acquire) {
                return;
            }
            e.pending.store(false, Ordering::Release);
            self.explore(s);
        }
    }

    /// The root hub as a device: FreeBSD's `ugenB.1`, with the xHCI
    /// root hub's descriptors (`xhci_devd`, `xhci_confd`).
    fn root_hub(self: &Arc<Self>) -> UsbDevice {
        let raw_desc = usb::ugen::ROOT_HUB_DEVICE;
        let desc = DeviceDescriptor::parse(&raw_desc).unwrap_or_default();
        let raw_config = usb::ugen::ROOT_HUB_CONFIG.to_vec();
        let config = Configuration::parse(&raw_config).unwrap_or(Configuration {
            value: 1,
            string: 0,
            attributes: 0x40,
            max_power: 0,
            interfaces_declared: 0,
            interfaces: Vec::new(),
        });
        UsbDevice::new(DeviceInit {
            hc: Arc::clone(&self.hc),
            bus_ref: Arc::downgrade(self),
            tt: None,
            slot: None,
            bus: self.unit,
            index: 1,
            address: 1,
            speed: if self.hc.bus_speed().contains("Super") {
                Speed::Super
            } else {
                Speed::High
            },
            hub_index: 0,
            port: 0,
            route: 0,
            root_port: 0,
            depth: 0,
            desc,
            config,
            raw_config,
            manufacturer: String::from(self.hc.root_hub_vendor()),
            product: String::from("XHCI root HUB"),
            serial: String::new(),
            raw_desc,
            plugtime: crate::device::ticks(),
            session: new_session(),
            power: 0,
            self_powered: true,
            nports: self.hc.root_ports(),
            hub_address: 0,
            port_path: Vec::new(),
            power_mode: abi::usb::power_mode::SAVE,
        })
    }
}

/// The `usbus` driver.
pub struct Usbus {
    bus: Arc<Bus>,
    thread: SpinLock<Option<Arc<Thread>>>,
}

impl core::fmt::Debug for Usbus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Usbus")
            .field("unit", &self.bus.unit)
            .finish()
    }
}

impl Driver for Usbus {
    const NAME: &'static str = "usbus";
    const IDS: Ids = Ids::Nexus(&["usbus"]);

    fn describe(_: &Ident) -> String {
        String::new()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let data = cx.bus_data_arc::<UsbusData>().ok_or(AttachError::Decline)?;
        let hc = Arc::clone(&data.hc);
        Ok(Self {
            bus: Arc::new(Bus {
                unit: cx.unit(),
                hc,
                explorer: Arc::new(Explorer::default()),
                hubs: SpinLock::new(Vec::new()),
                indices: SpinLock::new(1 << 1),
                devices: SpinLock::new(alloc::vec![None; usize::from(MAX_INDEX) + 1]),
                aborts: SpinLock::new(Vec::new()),
                root_done: AtomicBool::new(false),
                booting: AtomicBool::new(true),
            }),
            thread: SpinLock::new(None),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let bus = &self.bus;
        {
            let mut all = BUSES.lock();
            all.retain(|b| b.strong_count() > 0);
            all.push(Arc::downgrade(bus));
        }
        ::bus::manager().log(format_args!("usbus{}: {}", bus.unit, bus.hc.bus_speed()));
        bus.hc.set_explorer(Arc::clone(&bus.explorer));
        let (b, node) = (Arc::clone(bus), cx.node());
        let t = sched::Builder::new("usb")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| b.thread(node, s))
            .map_err(|_| AttachError::NoMemory)?;
        *self.thread.lock() = Some(t);
        bus.explorer.kick();
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let e = &self.bus.explorer;
        e.stop.store(true, Ordering::Release);
        e.queue.wake_all();
        ROOTS.wake_all();
        let t = self.thread.lock().take();
        if let Some(t) = t {
            t.join(cx.sleepable());
        }
        // The root hub's nodes, once its driver (a child) is gone.
        let s = cx.sleepable();
        if let Some(root) = device_at(self.bus.unit, 1) {
            crate::ugen::endpoints_removed(s, &root);
            crate::ugen::device_removed(s, &root);
            root.set_gone();
        }
        self.bus.clear_device(1);
        BUSES
            .lock()
            .retain(|b| b.upgrade().is_some_and(|b| !Arc::ptr_eq(&b, &self.bus)));
    }
}

/// The declaration for the build's driver table.
pub const USBUS: DriverDecl = DriverDecl::of::<Usbus>();
