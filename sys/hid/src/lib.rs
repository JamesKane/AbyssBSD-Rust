// SPDX-License-Identifier: BSD-2-Clause
//! HID as a bus (USB.md §6.1): `hidbus`, with one child per top-level
//! collection of the report descriptor, over a [`HidTransport`]; and
//! `usbhid`, the USB transport.
//!
//! Children (`hkbd`, `hms`) get a [`HidChild`] as bus data: the parsed
//! descriptor, their collection's index, and [`HidChild::listen`] for
//! the input reports `hidbus` routes to them by report ID.
//!
//! Behavior reference: sys/dev/hid/hidbus.c, sys/dev/usb/input/usbhid.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod usbhid;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::HidIdent;
use hid::{Descriptor, Kind};
use sync::SpinLock;

pub use usbhid::USBHID;

/// Where a transport delivers input reports.
pub type Sink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// What a transport does for `hidbus`.
pub trait HidTransport: Send + Sync {
    /// Starts delivering input reports (with their ID byte, if the device
    /// numbers them) to `sink`, until [`HidTransport::stop`].
    fn start(&self, sink: Sink);
    /// Stops delivering reports.
    fn stop(&self, s: &sched::Sleepable);
    /// Sends an output report without waiting (`data` starts with the ID
    /// byte if `id` is not 0).
    fn output(&self, id: u8, data: Vec<u8>);
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
}

impl core::fmt::Debug for HidbusData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HidbusData")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

type Handler = Box<dyn Fn(u8, &[u8]) + Send + Sync>;

/// Shared by a bus and its children: the transport and who listens.
struct Shared {
    transport: Arc<dyn HidTransport>,
    desc: Arc<Descriptor>,
    /// Per collection index: its handler.
    handlers: SpinLock<Vec<Option<Arc<Handler>>>>,
}

impl Shared {
    /// Routes an input report to the collection that declares its ID.
    fn deliver(&self, report: &[u8]) {
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
                .filter_map(|&c| h.get(c).and_then(Clone::clone))
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
            *slot = Some(Arc::new(Box::new(f)));
        }
    }

    /// Stops listening.
    pub fn unlisten(&self) {
        if let Some(slot) = self.shared.handlers.lock().get_mut(self.index) {
            *slot = None;
        }
    }

    /// Sends an output report without waiting.
    pub fn output(&self, id: u8, data: Vec<u8>) {
        self.shared.transport.output(id, data);
    }
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
                handlers: SpinLock::new((0..n).map(|_| None).collect()),
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
        let shared = Arc::clone(&self.shared);
        self.shared
            .transport
            .start(Arc::new(move |r: &[u8]| shared.deliver(r)));
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.shared.transport.stop(cx.sleepable());
    }
}

/// The declaration for the build's driver table.
pub const HIDBUS: DriverDecl = DriverDecl::of::<Hidbus>();
