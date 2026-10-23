// SPDX-License-Identifier: BSD-2-Clause
//! `usbhid`: a USB HID interface as a `hidbus` transport (FreeBSD 15's
//! default, `hw.usb.usbhid.enable=1`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use bus::{
    Announce, AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use devtree::ids::{HidIdent, UsbId};
use hid::{BOOT_KEYBOARD, BOOT_MOUSE, Descriptor};
use sched::Sleepable;
use sync::SpinLock;
use usb::Setup;
use usb::descriptor::{TransferType, class, kind};
use usb::request::rt;
use usbus::{Transfer, UsbDevice, UsbInterface};
use vm::object::IoBuf;

use crate::{HidTransport, HidbusData, Sink};

/// HID class requests (HID 1.11 §7.2).
const SET_REPORT: u8 = 0x09;
const SET_IDLE: u8 = 0x0a;
const SET_PROTOCOL: u8 = 0x0b;
/// The report type of an output report.
const OUTPUT: u16 = 2;
/// FreeBSD's `BUS_USB`.
const BUS_USB: u16 = 0x03;
/// Interrupt transfers kept queued.
const IN_FLIGHT: usize = 2;

fn class_request(request: u8, value: u16, iface: u8, length: u16) -> Setup {
    Setup {
        request_type: rt::CLASS | rt::INTERFACE,
        request,
        value,
        index: u16::from(iface),
        length,
    }
}

/// The interface as a transport.
struct Transport {
    dev: Arc<UsbDevice>,
    iface: u8,
    ep: u8,
    len: u16,
    sink: SpinLock<Option<Sink>>,
    running: AtomicBool,
}

impl Transport {
    fn arm(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let t = Transfer::new(
            IoBuf::from(vec![0; usize::from(self.len)]),
            0,
            move |buf, r| {
                if let (Ok(n), IoBuf::Owned(b)) = (r, &buf) {
                    let sink = me.sink.lock().clone();
                    if let Some(f) = sink {
                        f(&b[..n.min(b.len())]);
                    }
                }
                let again = me.running.load(Ordering::Acquire)
                    && !me.dev.is_gone()
                    && matches!(r, Ok(_) | Err(usbus::UsbError::Transaction));
                if again {
                    me.arm();
                }
            },
        );
        let _ = self.dev.submit(self.ep, t);
    }
}

impl HidTransport for Arc<Transport> {
    fn start(&self, sink: Sink) {
        *self.sink.lock() = Some(sink);
        if !self.running.swap(true, Ordering::AcqRel) {
            for _ in 0..IN_FLIGHT {
                self.arm();
            }
        }
    }

    fn stop(&self, s: &Sleepable) {
        self.running.store(false, Ordering::Release);
        self.dev.abort(s, self.ep);
        *self.sink.lock() = None;
    }

    fn output(&self, id: u8, data: Vec<u8>) {
        let len = u16::try_from(data.len()).unwrap_or(u16::MAX);
        let setup = class_request(SET_REPORT, (OUTPUT << 8) | u16::from(id), self.iface, len);
        let t = Transfer::new(IoBuf::from(data), 1000, |_, _| {});
        let _ = self.dev.control_async(setup, t);
    }
}

/// The `usbhid` driver.
pub struct Usbhid {
    data: Arc<HidbusData>,
    dev: Arc<UsbDevice>,
    unit: u32,
}

impl core::fmt::Debug for Usbhid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Usbhid")
            .field("name", &self.data.name)
            .finish()
    }
}

impl Driver for Usbhid {
    const NAME: &'static str = "usbhid";
    const IDS: Ids = Ids::Usb(&[UsbId::interface(class::HID)]);
    const ANNOUNCE: Announce = Announce::Bare;

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
        let s = cx.sleepable();
        let dev = Arc::clone(&iface.device);
        let number = iface.desc.number;
        let ep = iface
            .desc
            .endpoints
            .iter()
            .find(|e| e.is_in() && e.transfer_type() == TransferType::Interrupt)
            .ok_or(AttachError::Device("no interrupt endpoint"))?;
        let (ep, len) = (ep.address, ep.max_packet().max(1));
        // The HID descriptor names the report descriptor's length.
        let report_len = iface
            .class_descriptors()
            .find(|(t, _)| *t == kind::HID)
            .and_then(|(_, d)| hid_report_len(d));
        // Idle: report only on change (some devices stall it; harmless).
        let _ = dev.control(s, class_request(SET_IDLE, 0, number, 0), None);
        let boot = iface.desc.subclass == 1;
        let raw = report_len.and_then(|n| {
            dev.control(
                s,
                Setup::get_interface_descriptor(kind::REPORT, 0, number, n),
                None,
            )
            .ok()
        });
        let desc = match raw.as_deref().map(Descriptor::parse) {
            Some(Ok(d)) if !d.collections.is_empty() => d,
            // Unusable: fall back to the boot protocol, if the interface
            // has it.
            _ if boot => {
                let _ = dev.control(s, class_request(SET_PROTOCOL, 0, number, 0), None);
                let fallback = if iface.desc.protocol == 2 {
                    BOOT_MOUSE
                } else {
                    BOOT_KEYBOARD
                };
                Descriptor::parse(fallback).map_err(|_| AttachError::Device("boot descriptor"))?
            }
            _ => return Err(AttachError::Device("no usable report descriptor")),
        };
        let name = alloc::format!("{} {}", dev.manufacturer, dev.product);
        let transport = Arc::new(Transport {
            dev: Arc::clone(&dev),
            iface: number,
            ep,
            len,
            sink: SpinLock::new(None),
            running: AtomicBool::new(false),
        });
        Ok(Self {
            data: Arc::new(HidbusData {
                transport: Arc::new(transport),
                desc: Arc::new(desc),
                ident: HidIdent {
                    usage: 0,
                    bus: BUS_USB,
                    vendor: dev.desc.vendor,
                    product: dev.desc.product,
                    version: dev.desc.release,
                    index: 0,
                },
                name,
            }),
            dev,
            unit: cx.unit(),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        self.dev.announce(Self::NAME, self.unit);
        cx.add_child(
            Ident::Nexus("hidbus"),
            Some(Arc::clone(&self.data) as Arc<dyn core::any::Any + Send + Sync>),
            Vec::new(),
        );
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.data.transport.stop(cx.sleepable());
    }
}

/// The report descriptor's length, from a HID descriptor (HID 1.11
/// §6.2.1).
fn hid_report_len(d: &[u8]) -> Option<u16> {
    let n = usize::from(*d.get(5)?);
    (0..n).find_map(|i| {
        let at = 6 + 3 * i;
        (*d.get(at)? == kind::REPORT)
            .then(|| Some(u16::from_le_bytes([*d.get(at + 1)?, *d.get(at + 2)?])))
            .flatten()
    })
}

/// The declaration for the build's driver table.
pub const USBHID: DriverDecl = DriverDecl::of::<Usbhid>();
