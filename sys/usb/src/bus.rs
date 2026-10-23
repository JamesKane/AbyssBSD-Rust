// SPDX-License-Identifier: BSD-2-Clause
//! `usbus`: one per controller. It owns the bus's device indices and its
//! explore thread, and adds the root hub.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use sched::{Sleepable, Thread, WaitQueue};
use sync::SpinLock;
use usb::Speed;
use usb::descriptor::{Configuration, DeviceDescriptor, Interface, class};

use crate::device::{DeviceInit, UsbDevice, UsbInterface};
use crate::hc::HostController;
use crate::hub::Hub;

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

/// A bus.
pub struct Bus {
    /// `usbusN`'s unit.
    pub unit: u32,
    pub(crate) hc: Arc<dyn HostController>,
    pub(crate) explorer: Arc<Explorer>,
    pub(crate) hubs: SpinLock<Vec<Arc<Hub>>>,
    /// Device indices in use; 1 is the root hub's.
    indices: SpinLock<u128>,
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

    /// One pass over every hub, root first.
    fn explore(self: &Arc<Self>, s: &Sleepable) {
        let hubs = self.hubs.lock().clone();
        for h in hubs {
            h.explore(s, self);
        }
    }

    fn thread(self: Arc<Self>, s: &Sleepable) {
        let e = Arc::clone(&self.explorer);
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

    /// The root hub as a device: FreeBSD's `ugenB.1`.
    fn root_hub(self: &Arc<Self>) -> UsbDevice {
        // FreeBSD's xHCI root hub descriptor: USB 3.0, IDs 0, a
        // SuperSpeed hub.
        let usb3 = self.hc.bus_speed().contains("Super");
        let iface = Interface {
            number: 0,
            alternate: 0,
            class: class::HUB,
            subclass: 0,
            protocol: 0,
            string: 0,
            endpoints: Vec::new(),
            extra: Vec::new(),
        };
        UsbDevice::new(DeviceInit {
            hc: Arc::clone(&self.hc),
            bus_ref: Arc::downgrade(self),
            tt: None,
            slot: None,
            bus: self.unit,
            index: 1,
            speed: if usb3 { Speed::Super } else { Speed::High },
            hub_index: 0,
            port: 0,
            route: 0,
            root_port: 0,
            depth: 0,
            desc: DeviceDescriptor {
                usb: if usb3 { 0x0300 } else { 0x0200 },
                class: class::HUB,
                protocol: if usb3 { 3 } else { 1 },
                release: 0x0100,
                configurations: 1,
                ..DeviceDescriptor::default()
            },
            config: Configuration {
                value: 1,
                string: 0,
                attributes: 0x40,
                max_power: 0,
                interfaces_declared: 1,
                interfaces: alloc::vec![iface],
            },
            raw_config: Vec::new(),
            manufacturer: String::from(self.hc.root_hub_vendor()),
            product: String::from("XHCI root HUB"),
            serial: String::new(),
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
        ::bus::manager().log(format_args!("usbus{}: {}", cx.unit(), hc.bus_speed()));
        Ok(Self {
            bus: Arc::new(Bus {
                unit: cx.unit(),
                hc,
                explorer: Arc::new(Explorer::default()),
                hubs: SpinLock::new(Vec::new()),
                indices: SpinLock::new(1 << 1),
            }),
            thread: SpinLock::new(None),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let bus = &self.bus;
        bus.hc.set_explorer(Arc::clone(&bus.explorer));
        let root = Arc::new(bus.root_hub());
        ::bus::manager().log(format_args!(
            "{}: <{} {}> at usbus{}",
            root.ugen(),
            root.manufacturer,
            root.product,
            bus.unit
        ));
        let iface = UsbInterface {
            desc: root.config.interfaces[0].clone(),
            device: root,
        };
        cx.add_child(
            Ident::Usb(iface.ident(), iface.place()),
            Some(Arc::new(iface)),
            Vec::new(),
        );
        let b = Arc::clone(bus);
        let t = sched::Builder::new("usb")
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| b.thread(s))
            .map_err(|_| AttachError::NoMemory)?;
        *self.thread.lock() = Some(t);
        bus.explorer.kick();
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let e = &self.bus.explorer;
        e.stop.store(true, Ordering::Release);
        e.queue.wake_all();
        let t = self.thread.lock().take();
        if let Some(t) = t {
            t.join(cx.sleepable());
        }
    }
}

/// The declaration for the build's driver table.
pub const USBUS: DriverDecl = DriverDecl::of::<Usbus>();
