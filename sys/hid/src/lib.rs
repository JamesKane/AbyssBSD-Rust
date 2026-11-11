// SPDX-License-Identifier: BSD-2-Clause
//! HID as a bus (USB.md §6.1): `hidbus`, with one child per top-level
//! collection of the report descriptor, over a [`HidTransport`]; and
//! `usbhid`, the USB transport.
//!
//! Children (`hkbd`, `hms`) get a [`HidChild`] as bus data: the parsed
//! descriptor, their collection's index, [`HidChild::listen`] for the
//! input reports `hidbus` routes to them by report ID, and
//! [`HidChild::intr_start`] and [`HidChild::intr_stop`]: the transport
//! runs while any child is active, and only active children get reports.
//!
//! `hidraw` ([`hidraw`]), once loaded (`kldload hidraw`), adds a child
//! of its own to every bus, index 255, which gets every input report
//! whole, as FreeBSD's `hidraw_identify` adds one.
//!
//! Behavior reference: sys/dev/hid/hidbus.c, sys/dev/usb/input/usbhid.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod hidmap;
pub mod hidraw;
pub mod usbhid;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use alloc::sync::Weak;
use core::sync::atomic::{AtomicBool, Ordering};

use bus::{
    AttachCx, AttachError, DetachReason, DeviceId, Driver, DriverDecl, Ident, Ids, StartCx, StopCx,
    StopReason,
};
use devtree::ids::HidIdent;
use hid::{Descriptor, Kind};
use sched::Sleepable;
use sync::SpinLock;

pub use usbhid::USBHID;

/// Where a transport delivers input reports.
pub type Sink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// A transport request the device refused, or one the transport cannot
/// make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportError;

/// What a transport does for `hidbus`.
pub trait HidTransport: Send + Sync {
    /// Starts delivering input reports (with their ID byte, if the device
    /// numbers them) to `sink`, until [`HidTransport::pause`] or
    /// [`HidTransport::stop`]; starting it again is harmless. Must not
    /// sleep.
    fn start(&self, sink: Sink);
    /// Stops delivering reports without waiting for I/O in flight, whose
    /// reports are dropped. Must not sleep.
    fn pause(&self);
    /// Stops delivering reports and waits for I/O in flight (detach).
    fn stop(&self, s: &sched::Sleepable);
    /// Sends an output report without waiting (`data` starts with the ID
    /// byte if `id` is not 0).
    fn output(&self, id: u8, data: Vec<u8>);
    /// `GET_REPORT` of report `id` of type `kind` (`HID_*_REPORT`), at
    /// most `len` bytes (`hid_get_report`).
    ///
    /// # Errors
    ///
    /// If the device refused or the transport cannot.
    fn get_report(
        &self,
        s: &Sleepable,
        kind: u8,
        id: u8,
        len: u16,
    ) -> Result<Vec<u8>, TransportError> {
        let _ = (s, kind, id, len);
        Err(TransportError)
    }
    /// `SET_REPORT` of report `id` of type `kind` (`hid_set_report`).
    ///
    /// # Errors
    ///
    /// If the device refused or the transport cannot.
    fn set_report(
        &self,
        s: &Sleepable,
        kind: u8,
        id: u8,
        data: &[u8],
    ) -> Result<(), TransportError> {
        let _ = (s, kind, id, data);
        Err(TransportError)
    }
    /// Writes an output report on the interrupt OUT pipe and waits
    /// (`hid_write`); `None` if the device has none (FreeBSD's
    /// `HQ_NOWRITE`: reports go by `SET_REPORT` then).
    fn write(&self, s: &Sleepable, data: &[u8]) -> Option<Result<(), TransportError>> {
        let _ = (s, data);
        None
    }
    /// The longest output report a write takes (`rdesc.wrsize`), given the
    /// output and largest reports' sizes.
    fn write_max(&self, osize: usize, largest: usize) -> usize {
        let _ = osize;
        largest
    }
}

/// What a transport gives its `hidbus` child.
pub struct HidbusData {
    /// The transport.
    pub transport: Arc<dyn HidTransport>,
    /// The report descriptor, parsed.
    pub desc: Arc<Descriptor>,
    /// The identity every child shares (its usage and index aside).
    pub ident: HidIdent,
    /// The device's name, for the children's descriptions.
    pub name: String,
    /// Its serial number, if it has one.
    pub serial: String,
    /// The report descriptor as the device gave it (empty if it gave
    /// none).
    pub raw: Vec<u8>,
    /// The report descriptor's length as the HID descriptor says
    /// (`hid_device_info.rdescsize`).
    pub rdesc_size: u16,
}

impl core::fmt::Debug for HidbusData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HidbusData")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

type Handler = Box<dyn Fn(u8, &[u8]) + Send + Sync>;

/// What a bus's whole-device child (`hidraw`) hears: every report as
/// the transport delivered it, ID byte included.
pub type RawHandler = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Shared by a bus and its children: the transport and who listens.
struct Shared {
    transport: Arc<dyn HidTransport>,
    desc: Arc<Descriptor>,
    /// Per collection index: its handler, and whether it is active.
    handlers: SpinLock<Vec<(Option<Arc<Handler>>, bool)>>,
    /// Where the transport delivers, once the bus has started.
    sink: SpinLock<Option<Sink>>,
    serial: String,
    /// The whole-device child's handler, and whether it is active.
    raw: SpinLock<(Option<RawHandler>, bool)>,
    /// The bus's data, for its whole-device child.
    data: Arc<HidbusData>,
}

impl Shared {
    /// Routes an input report to the collection that declares its ID.
    fn deliver(&self, report: &[u8]) {
        let raw = {
            let r = self.raw.lock();
            if r.1 { r.0.clone() } else { None }
        };
        if let Some(f) = raw {
            f(report);
        }
        let (id, body) = if self.desc.uses_ids {
            match report.split_first() {
                Some((&id, body)) => (id, body),
                None => return,
            }
        } else {
            (0, report)
        };
        let targets: Vec<Arc<Handler>> = {
            let h = self.handlers.lock();
            let mut t: Vec<usize> = self
                .desc
                .fields
                .iter()
                .filter(|f| f.kind == Kind::Input && f.report_id == id)
                .map(|f| f.collection)
                .collect();
            t.dedup();
            t.iter()
                .filter_map(|&c| match h.get(c) {
                    Some((Some(f), true)) => Some(Arc::clone(f)),
                    _ => None,
                })
                .collect()
        };
        for h in targets {
            h(id, body);
        }
    }
}

/// What a `hidbus` child gets as bus data.
pub struct HidChild {
    shared: Arc<Shared>,
    /// Its collection's index.
    pub index: usize,
}

impl core::fmt::Debug for HidChild {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HidChild")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl HidChild {
    /// The report descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &Descriptor {
        &self.shared.desc
    }

    /// Calls `f` with each input report for this collection: its ID (0
    /// if the device numbers none) and the report without the ID byte.
    /// From completion context: it must not sleep.
    pub fn listen(&self, f: impl Fn(u8, &[u8]) + Send + Sync + 'static) {
        let mut h = self.shared.handlers.lock();
        if let Some(slot) = h.get_mut(self.index) {
            slot.0 = Some(Arc::new(Box::new(f)));
        }
    }

    /// Stops listening.
    pub fn unlisten(&self) {
        if let Some(slot) = self.shared.handlers.lock().get_mut(self.index) {
            slot.0 = None;
        }
    }

    /// Starts this child's reports, and the transport if it is the first
    /// active child (`hid_intr_start`). Must not sleep.
    pub fn intr_start(&self) {
        if self.index == RAW_INDEX {
            self.shared.raw.lock().1 = true;
        }
        let mut h = self.shared.handlers.lock();
        if let Some(slot) = h.get_mut(self.index) {
            slot.1 = true;
        }
        drop(h);
        let sink = self.shared.sink.lock().clone();
        if let Some(sink) = sink {
            self.shared.transport.start(sink);
        }
    }

    /// Stops this child's reports, and the transport if no child is
    /// active (`hid_intr_stop`). Must not sleep.
    pub fn intr_stop(&self) {
        if self.index == RAW_INDEX {
            self.shared.raw.lock().1 = false;
        }
        let raw = self.shared.raw.lock().1;
        let mut h = self.shared.handlers.lock();
        if let Some(slot) = h.get_mut(self.index) {
            slot.1 = false;
        }
        let active = raw || h.iter().any(|s| s.1);
        drop(h);
        if !active {
            self.shared.transport.pause();
        }
    }

    /// The device's serial number (empty if it has none).
    #[must_use]
    pub fn serial(&self) -> &str {
        &self.shared.serial
    }

    /// Sends an output report without waiting.
    pub fn output(&self, id: u8, data: Vec<u8>) {
        self.shared.transport.output(id, data);
    }

    /// The bus's data: the transport, the raw descriptor, the identity.
    #[must_use]
    pub fn bus_data(&self) -> &HidbusData {
        &self.shared.data
    }

    /// The transport.
    #[must_use]
    pub fn transport(&self) -> &Arc<dyn HidTransport> {
        &self.shared.transport
    }

    /// For the whole-device child: hears every report whole (`f` runs in
    /// completion context and must not sleep); `None` stops it.
    pub fn listen_raw(&self, f: Option<RawHandler>) {
        self.shared.raw.lock().0 = f;
    }
}

/// The index of a bus's whole-device child (`HIDRAW_INDEX`).
pub const RAW_INDEX: usize = 255;

/// Whether `hidraw` is loaded: every bus then has its child.
static HIDRAW: AtomicBool = AtomicBool::new(false);

/// A bus, for adding and taking away its `hidraw` child.
struct Registered {
    node: DeviceId,
    shared: Weak<Shared>,
    ident: HidIdent,
    name: String,
    child: Option<DeviceId>,
}

static BUSES: SpinLock<Vec<Registered>> = SpinLock::new(Vec::new());

/// The whole-device child of bus `r`: index 255, usage 0.
fn raw_child(r: &Registered) -> Option<(Ident, Arc<HidChild>)> {
    let shared = r.shared.upgrade()?;
    let ident = HidIdent {
        usage: 0,
        index: u8::try_from(RAW_INDEX).unwrap_or(u8::MAX),
        ..r.ident
    };
    Some((
        Ident::Hid(ident, r.name.clone()),
        Arc::new(HidChild {
            shared,
            index: RAW_INDEX,
        }),
    ))
}

/// The work [`set_hidraw`] does on one bus: its child, if it had one
/// already, or the one to add.
type Change = (DeviceId, Option<DeviceId>, Option<(Ident, Arc<HidChild>)>);

/// Loads or unloads `hidraw` (`kldload hidraw`, `kldunload hidraw`): each
/// bus, in unit order, gets its child attached or detached, and the call
/// returns once they are (as the module's load and unload do). A child is
/// kept, unattached, for the next load. Returns whether it was loaded
/// before.
pub fn set_hidraw(s: &Sleepable, on: bool) -> bool {
    let was = HIDRAW.swap(on, Ordering::AcqRel);
    if was == on {
        return was;
    }
    let m = bus::manager();
    let mut work: Vec<Change> = {
        let mut b = BUSES.lock();
        b.retain(|r| r.shared.strong_count() > 0);
        b.iter()
            .map(|r| {
                let add = if on && r.child.is_none() {
                    raw_child(r)
                } else {
                    None
                };
                (r.node, r.child, add)
            })
            .collect()
    };
    work.sort_by_key(|(n, _, _)| m.unit(*n, s).unwrap_or(u32::MAX));
    for (node, child, add) in work {
        match (on, child, add) {
            (false, Some(c), _) => {
                let name = m.nameunit(c, s);
                if m.detach(c, DetachReason::Requested, s).is_ok()
                    && let Some(n) = name
                {
                    m.log(format_args!("{n}: detached"));
                }
            }
            (true, Some(c), _) => {
                let _ = m.probe_and_attach(c, s);
                m.wait_settled(c, s);
            }
            (true, None, Some((ident, data))) => {
                let c = m.add_child(s, node, ident, Some(data), Vec::new(), &[]);
                if let Some(r) = BUSES.lock().iter_mut().find(|r| r.node == node) {
                    r.child = Some(c);
                }
                m.wait_settled(c, s);
            }
            _ => {}
        }
    }
    was
}

/// Whether `hidraw` is loaded.
#[must_use]
pub fn hidraw_loaded() -> bool {
    HIDRAW.load(Ordering::Acquire)
}

/// The `hidbus` driver.
pub struct Hidbus {
    shared: Arc<Shared>,
    ident: HidIdent,
    name: String,
}

impl core::fmt::Debug for Hidbus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hidbus").field("name", &self.name).finish()
    }
}

impl Driver for Hidbus {
    const NAME: &'static str = "hidbus";
    const IDS: Ids = Ids::Nexus(&["hidbus"]);

    fn describe(_: &Ident) -> String {
        String::from("HID bus")
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let d = cx
            .bus_data_arc::<HidbusData>()
            .ok_or(AttachError::Decline)?;
        let n = d.desc.collections.len();
        Ok(Self {
            shared: Arc::new(Shared {
                transport: Arc::clone(&d.transport),
                desc: Arc::clone(&d.desc),
                handlers: SpinLock::new((0..n).map(|_| (None, false)).collect()),
                sink: SpinLock::new(None),
                serial: d.serial.clone(),
                raw: SpinLock::new((None, false)),
                data: Arc::clone(&d),
            }),
            ident: d.ident,
            name: d.name.clone(),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        for (i, c) in self.shared.desc.collections.iter().enumerate() {
            let ident = HidIdent {
                usage: c.usage,
                index: u8::try_from(i).unwrap_or(u8::MAX),
                ..self.ident
            };
            cx.add_child(
                Ident::Hid(ident, self.name.clone()),
                Some(Arc::new(HidChild {
                    shared: Arc::clone(&self.shared),
                    index: i,
                })),
                Vec::new(),
            );
        }
        let mut r = Registered {
            node: cx.node(),
            shared: Arc::downgrade(&self.shared),
            ident: self.ident,
            name: self.name.clone(),
            child: None,
        };
        if hidraw_loaded()
            && let Some((ident, data)) = raw_child(&r)
        {
            r.child = Some(cx.add_child(ident, Some(data), Vec::new()));
        }
        BUSES.lock().push(r);
        // Children start the transport as they become active. The sink
        // holds the bus weakly: the transport outlives no bus.
        let shared = Arc::downgrade(&self.shared);
        *self.shared.sink.lock() = Some(Arc::new(move |r: &[u8]| {
            if let Some(s) = shared.upgrade() {
                s.deliver(r);
            }
        }));
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let me = Arc::as_ptr(&self.shared);
        BUSES
            .lock()
            .retain(|r| !core::ptr::eq(r.shared.as_ptr(), me));
        *self.shared.sink.lock() = None;
        self.shared.transport.stop(cx.sleepable());
    }
}

/// The declaration for the build's driver table.
pub const HIDBUS: DriverDecl = DriverDecl::of::<Hidbus>();
