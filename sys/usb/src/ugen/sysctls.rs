// SPDX-License-Identifier: BSD-2-Clause
//! `hw.usb`: FreeBSD's USB knobs, in the order GENERIC lists them, for
//! the parts of its USB stack this one has (the core, `ugen`, `uhub`,
//! `usbhid`, `umass`, and `xhci`). The debug levels are kept and only
//! `hw.usb.ugen.debug` changes anything (it logs each `ugen` request);
//! the other values describe what this stack does, read-only where
//! FreeBSD's are boot-time tunables.
//!
//! Behavior reference: sys/dev/usb/usb_debug.c, usb_device.c,
//! usb_hub.c, usb_generic.c, usb_dev.c, usb_process.c, usb_request.c,
//! usb_transfer.c, input/usbhid.c, storage/umass.c,
//! controller/xhci.c.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicI32, Ordering};

use sched::Sleepable;
use sysctl::{Number, Tree, Value, int_writer};

use super::{TEMPLATE, UGEN_DEBUG, USB_DEBUG};

/// The debug levels besides `usb_debug` and `ugen_debug`.
static DEV_DEBUG: AtomicI32 = AtomicI32::new(0);
static UHUB_DEBUG: AtomicI32 = AtomicI32::new(0);
static PROC_DEBUG: AtomicI32 = AtomicI32::new(0);
static CTRL_DEBUG: AtomicI32 = AtomicI32::new(0);
static USBHID_DEBUG: AtomicI32 = AtomicI32::new(0);
static UMASS_DEBUG: AtomicI32 = AtomicI32::new(0);
static XHCI_DEBUG: AtomicI32 = AtomicI32::new(0);
/// `hw.usb.power_timeout`, seconds (`usb_power_timeout`).
static POWER_TIMEOUT: AtomicI32 = AtomicI32::new(30);

/// A read-write `int` over `v`.
fn rw(t: &mut Tree, parent: sysctl::NodeId, name: &str, desc: &str, v: &'static AtomicI32) {
    let _ = t.add_writable(
        parent,
        Number::Auto,
        name,
        desc,
        0,
        // Plain knobs: no ordering with anything else.
        Value::Int(Box::new(move || v.load(Ordering::Relaxed))),
        int_writer(move |n| {
            v.store(n, Ordering::Relaxed);
            Ok(())
        }),
    );
}

/// A read-only `int`.
fn ro(t: &mut Tree, parent: sysctl::NodeId, name: &str, desc: &str, v: i32) {
    let _ = t.add_value(parent, Number::Auto, name, desc, Value::int(v));
}

fn node(t: &mut Tree, parent: sysctl::NodeId, name: &str, desc: &str) -> Option<sysctl::NodeId> {
    t.add_node(parent, Number::Auto, name, desc).ok()
}

pub(super) fn init(s: &Sleepable) {
    ::bus::manager().sysctl(s, &mut |t| {
        let root = t.root();
        let Some(hw) = t.child(root, "hw") else {
            return;
        };
        let Some(usb) = node(t, hw, "usb", "USB debugging") else {
            return;
        };
        if let Some(n) = node(t, usb, "usbhid", "USB usbhid") {
            rw(t, n, "debug", "Debug level", &USBHID_DEBUG);
            ro(t, n, "enable", "Enable usbhid and prefer it to other USB HID drivers", 1);
        }
        ro(t, usb, "full_ddesc", "USB always read complete device descriptor, if set", 0);
        ro(t, usb, "no_cs_fail", "USB clear stall failures are ignored, if set", 0);
        if let Some(n) = node(t, usb, "proc", "USB process") {
            rw(t, n, "debug", "Debug level", &PROC_DEBUG);
        }
        ro(t, usb, "disable_port_power", "Set to disable all USB port power.", 0);
        ro(t, usb, "disable_enumeration", "Set to disable all USB device enumeration. This can secure against USB devices turning evil, for example a USB memory stick becoming a USB keyboard.", 0);
        rw(t, usb, "power_timeout", "USB power timeout", &POWER_TIMEOUT);
        if let Some(n) = node(t, usb, "uhub", "USB HUB") {
            rw(t, n, "debug", "Debug level", &UHUB_DEBUG);
        }
        if let Some(n) = node(t, usb, "ugen", "USB generic") {
            rw(t, n, "debug", "Debug level", &UGEN_DEBUG);
        }
        ro(t, usb, "usb_lang_mask", "Preferred USB language mask", 255);
        ro(t, usb, "usb_lang_id", "Preferred USB language ID", 9);
        rw(t, usb, "template", "Selected USB device side template", &TEMPLATE);
        if let Some(n) = node(t, usb, "dev", "USB device") {
            rw(t, n, "debug", "Debug Level", &DEV_DEBUG);
        }
        if let Some(n) = node(t, usb, "timings", "Timings") {
            for (name, desc, v) in [
                ("enum_nice_time", "Enumeration thread nice time", 16u32),
                ("extra_power_up_time", "Extra PowerUp Time", 20),
                ("resume_recovery", "Resume Recovery", 50),
                ("resume_wait", "Resume Wait", 50),
                ("resume_delay", "Resume Delay", 250),
                ("set_address_settle", "Set Address Settle", 10),
                ("port_resume_delay", "Port Resume Delay", 40),
                ("port_powerup_delay", "Port PowerUp Delay", 300),
                ("port_reset_recovery", "Port Reset Recovery", 20),
                ("port_root_reset_delay", "Root Port Reset Delay", 200),
                ("port_reset_delay", "Port Reset Delay", 50),
            ] {
                // `CTLTYPE_UINT`, format `IU`.
                let _ = t.add_value(n, Number::Auto, name, desc, Value::Uint(Box::new(move || v)));
            }
        }
        rw(t, usb, "debug", "Debug level", &USB_DEBUG);
        if let Some(n) = node(t, usb, "umass", "USB umass") {
            ro(t, n, "throttle", "Forced delay between commands in milliseconds", 0);
            rw(t, n, "debug", "umass debug level", &UMASS_DEBUG);
        }
        ro(t, usb, "no_shutdown_wait", "No USB device waiting at system shutdown.", 0);
        ro(t, usb, "no_suspend_wait", "No USB device waiting at system suspend.", 0);
        ro(t, usb, "no_boot_wait", "No USB device enumerate waiting at boot.", 0);
        if let Some(n) = node(t, usb, "ctrl", "USB controller") {
            rw(t, n, "debug", "Debug level", &CTRL_DEBUG);
        }
        if let Some(n) = node(t, usb, "xhci", "USB XHCI") {
            ro(t, n, "ctlstep", "Set to enable control endpoint status stage stepping", 0);
            ro(t, n, "dma32", "Set to only use 32-bit DMA for the XHCI controller", 0);
            ro(t, n, "use_polling", "Set to enable software interrupt polling for the XHCI controller", 0);
            ro(t, n, "xhci_port_route", "Routing bitmap for switching EHCI ports to the XHCI controller", 0);
            rw(t, n, "debug", "Debug level", &XHCI_DEBUG);
            ro(t, n, "dcepquirk", "Set to disable endpoint deconfigure command", 0);
            ro(t, n, "ctlquirk", "Set to enable control endpoint quirk", 1);
            // UAS gets streams here (USB.md §4.4).
            ro(t, n, "streams", "Set to enable streams mode support", 1);
        }
    });
}
