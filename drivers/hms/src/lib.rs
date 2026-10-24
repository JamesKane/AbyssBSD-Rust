// SPDX-License-Identifier: BSD-2-Clause
//! HID mice and tablets (`hms`) on `hidbus`, reported through evdev.
//!
//! A pointer or mouse collection's input fields are mapped value by value
//! to evdev codes: relative X, Y, and Z to `REL_*`, absolute ones to
//! `ABS_*` (with the field's range and resolution), the wheel and AC Pan
//! to `REL_WHEEL` and `REL_HWHEEL`, and buttons 1 to 16 to `BTN_MOUSE`
//! on. A collection with no X or Y axis is not a mouse. One with an
//! absolute axis is a tablet: its evdev device is direct (`INPUT_PROP_DIRECT`),
//! a mouse's a pointer.
//!
//! Reports run only while the evdev device is open, and each one becomes
//! the events whose values changed (a relative axis reports any motion,
//! an absolute one or a button only a new value), then `SYN_REPORT`.
//!
//! Behavior reference: sys/dev/hid/hms.c, sys/dev/hid/hidmap.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::errno::Errno;
use abi::evdev::{
    ABS_X, ABS_Y, ABS_Z, BTN_MOUSE, EV_ABS, EV_KEY, EV_REL, INPUT_PROP_DIRECT, INPUT_PROP_POINTER,
    InputId, KEY_RESERVED, REL_HWHEEL, REL_WHEEL, REL_X, REL_Y, REL_Z,
};
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::{HidId, HidIdent};
use hid::usage::{self, AC_PAN, BUTTON, MOUSE, POINTER, WHEEL, X, Y, Z};
use hid::{Descriptor, Field, Kind, flags};
use hidbus::HidChild;
use sync::SpinLock;

/// What a mapped value reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Map {
    /// One variable value: an event of `kind` and `code`.
    Variable { kind: u16, code: u16 },
    /// An array value: the button its usage names.
    Array,
}

/// FreeBSD's `hms_map` entries, and the wheel's map: the capabilities
/// `hmsN: … coordinates` reports.
#[derive(Clone, Copy, Debug, Default)]
struct Caps {
    rel_x: bool,
    rel_y: bool,
    rel_z: bool,
    abs_x: bool,
    abs_y: bool,
    abs_z: bool,
    hwheel: bool,
    wheel: bool,
}

impl Caps {
    fn absolute(&self) -> bool {
        self.abs_x || self.abs_y
    }

    fn pointer(&self) -> bool {
        self.rel_x || self.rel_y || self.abs_x || self.abs_y
    }
}

/// The evdev code a button usage maps to (buttons 1 to 16).
fn button(u: u32) -> Option<u16> {
    let id = usage::id(u);
    (usage::page(u) == BUTTON && (1..=16).contains(&id)).then(|| BTN_MOUSE + id - 1)
}

/// What a variable value with usage `u` maps to, noting it in `caps`
/// (`can_map_variable` over `hms_map`, then the wheel's map).
fn map_variable(u: u32, relative: bool, caps: &mut Caps) -> Option<(u16, u16)> {
    let (kind, code, cap) = match (u, relative) {
        (X, true) => (EV_REL, REL_X, &mut caps.rel_x),
        (Y, true) => (EV_REL, REL_Y, &mut caps.rel_y),
        (Z, true) => (EV_REL, REL_Z, &mut caps.rel_z),
        (X, false) => (EV_ABS, ABS_X, &mut caps.abs_x),
        (Y, false) => (EV_ABS, ABS_Y, &mut caps.abs_y),
        (Z, false) => (EV_ABS, ABS_Z, &mut caps.abs_z),
        (AC_PAN, true) => (EV_REL, REL_HWHEEL, &mut caps.hwheel),
        (WHEEL, true) => (EV_REL, REL_WHEEL, &mut caps.wheel),
        _ => return button(u).map(|code| (EV_KEY, code)),
    };
    *cap = true;
    Some((kind, code))
}

/// One mapped value of one input field (`struct hidmap_hid_item`).
#[derive(Clone, Debug)]
struct Item {
    field: Field,
    /// Which of the field's values.
    index: u16,
    map: Map,
    /// The last value reported (variable), or the last button (array).
    last: i32,
    last_key: u16,
}

/// A collection's mapped values, and what they add up to.
struct Mapping {
    items: Vec<Item>,
    caps: Caps,
}

impl Mapping {
    /// `hidmap_parse_hid_descr` over the collection's input fields.
    fn of(desc: &Descriptor, index: usize) -> Self {
        let mut caps = Caps::default();
        let mut items = Vec::new();
        for f in desc
            .fields_of(index)
            .filter(|f| f.kind == Kind::Input && !f.is_constant())
        {
            for i in 0..f.count {
                let map = if f.is_variable() {
                    let Some(u) = f.usage(i) else { continue };
                    match map_variable(u, f.is_relative(), &mut caps) {
                        Some((kind, code)) => Map::Variable { kind, code },
                        None => continue,
                    }
                } else {
                    // An array of buttons: mapped if any of its usages is
                    // one.
                    let n = u32::try_from(i64::from(f.logical_max) - i64::from(f.logical_min))
                        .unwrap_or(0);
                    if !(0..=n).any(|k| f.usages.get(k).and_then(button).is_some()) {
                        continue;
                    }
                    Map::Array
                };
                items.push(Item {
                    field: f.clone(),
                    index: i,
                    map,
                    last: 0,
                    last_key: KEY_RESERVED,
                });
            }
        }
        Self { items, caps }
    }

    /// The buttons: variable values mapped to keys.
    fn buttons(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.map, Map::Variable { kind: EV_KEY, .. }))
            .count()
    }
}

/// `hidbus_set_desc`: the device's name, with `suffix` unless the name
/// has it already or the device has only this collection.
fn describe(name: &str, suffix: &str, collections: usize) -> String {
    let has = name
        .to_ascii_lowercase()
        .contains(&suffix.to_ascii_lowercase());
    if has || collections <= 1 {
        String::from(name)
    } else {
        format!("{name} {suffix}")
    }
}

/// Starts and stops the reports as evdev clients come and go.
struct Methods(Arc<HidChild>);

impl evdev::Methods for Methods {
    fn open(&self) -> Result<(), Errno> {
        self.0.intr_start();
        Ok(())
    }

    fn close(&self) {
        self.0.intr_stop();
    }
}

/// The `hms` driver.
pub struct Hms {
    child: Arc<HidChild>,
    mapping: Arc<SpinLock<Mapping>>,
    unit: u32,
    ident: HidIdent,
    desc: String,
    evdev: SpinLock<Option<Arc<evdev::Device>>>,
}

impl core::fmt::Debug for Hms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hms").field("unit", &self.unit).finish()
    }
}

impl Driver for Hms {
    const NAME: &'static str = "hms";
    const IDS: Ids = Ids::Hid(&[HidId::usage(POINTER), HidId::usage(MOUSE)]);

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Hid(_, name) => name.clone(),
            _ => String::new(),
        }
    }

    fn describe_with(ident: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        let (Ident::Hid(_, name), Some(child)) =
            (ident, bus_data.and_then(|d| d.downcast_ref::<HidChild>()))
        else {
            return Self::describe(ident);
        };
        let d = child.descriptor();
        let caps = Mapping::of(d, child.index).caps;
        let suffix = if caps.absolute() { "Tablet" } else { "Mouse" };
        describe(name, suffix, d.collections.len())
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<HidChild>().ok_or(AttachError::Decline)?;
        let mapping = Mapping::of(child.descriptor(), child.index);
        // There should be at least one X or Y axis.
        if !mapping.caps.pointer() {
            return Err(AttachError::Decline);
        }
        let Ident::Hid(ident, _) = cx.ident().clone() else {
            return Err(AttachError::Decline);
        };
        let desc = Self::describe_with(cx.ident(), Some(&*child as &(dyn Any + Send + Sync)));
        Ok(Self {
            child,
            mapping: Arc::new(SpinLock::new(mapping)),
            unit: cx.unit(),
            ident,
            desc,
            evdev: SpinLock::new(None),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let me = format!("hms{}", self.unit);
        let mut b = evdev::Builder::new(&self.desc);
        b.phys(&me).uniq(self.child.serial()).id(InputId {
            bustype: self.ident.bus,
            vendor: self.ident.vendor,
            product: self.ident.product,
            version: self.ident.version,
        });
        let (caps, buttons, id) = {
            let m = self.mapping.lock();
            for item in &m.items {
                let f = &item.field;
                match item.map {
                    Map::Variable { kind: EV_KEY, code } => {
                        b.event(EV_KEY).key(code);
                    }
                    Map::Variable { kind: EV_REL, code } => {
                        b.event(EV_REL).rel(code);
                    }
                    Map::Variable { kind, code } => {
                        if kind == EV_ABS {
                            b.event(EV_ABS).abs(
                                code,
                                f.logical_min,
                                f.logical_max,
                                0,
                                0,
                                f.resolution(),
                            );
                        }
                    }
                    Map::Array => {
                        b.event(EV_KEY);
                        let n = u32::try_from(i64::from(f.logical_max) - i64::from(f.logical_min))
                            .unwrap_or(0);
                        for k in 0..=n {
                            if let Some(code) = f.usages.get(k).and_then(button) {
                                b.key(code);
                            }
                        }
                    }
                }
            }
            (
                m.caps,
                m.buttons(),
                m.items.first().map_or(0, |i| i.field.report_id),
            )
        };
        // `hms_final_cb`.
        b.prop(if caps.absolute() {
            INPUT_PROP_DIRECT
        } else {
            INPUT_PROP_POINTER
        });
        b.methods(Arc::new(Methods(Arc::clone(&self.child))));
        let ev = b
            .register(cx.sleepable())
            .map_err(|_| AttachError::Device("no evdev device"))?;
        let mapping = Arc::clone(&self.mapping);
        let e = Arc::clone(&ev);
        self.child
            .listen(move |id, report| intr(&mut mapping.lock(), &e, id, report));
        *self.evdev.lock() = Some(ev);
        bus::manager().log(format_args!(
            "{me}: {buttons} buttons and [{}{}{}{}{}] coordinates ID={id}",
            if caps.rel_x || caps.abs_x { "X" } else { "" },
            if caps.rel_y || caps.abs_y { "Y" } else { "" },
            if caps.rel_z || caps.abs_z { "Z" } else { "" },
            if caps.wheel { "W" } else { "" },
            if caps.hwheel { "H" } else { "" },
        ));
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.child.unlisten();
        let ev = self.evdev.lock().take();
        if let Some(e) = ev {
            // Closes the reports if clients had them open.
            e.unregister(cx.sleepable());
        }
    }
}

/// `hidmap_intr`: one input report.
fn intr(m: &mut Mapping, ev: &evdev::Device, id: u8, report: &[u8]) {
    let mut sync = false;
    for item in m.items.iter_mut().filter(|i| i.field.report_id == id) {
        let f = &item.field;
        let Some(data) = f.value(report, item.index) else {
            continue;
        };
        match item.map {
            Map::Variable { kind, code } => {
                // HID 1.11 §5.10: an out-of-range value leaves a control
                // with a null state as it was.
                if f.flags & flags::NULL_STATE != 0
                    && (data < f.logical_min || data > f.logical_max)
                {
                    continue;
                }
                if data == if kind == EV_REL { 0 } else { item.last } {
                    continue;
                }
                if kind == EV_KEY {
                    ev.push_key(code, data != 0);
                } else {
                    let _ = ev.push(kind, code, data);
                }
                item.last = data;
            }
            Map::Array => {
                // §6.2.2.5: an out-of-range value asserts no control.
                let key = if data < f.logical_min || data > f.logical_max {
                    KEY_RESERVED
                } else {
                    u32::try_from(i64::from(data) - i64::from(f.logical_min))
                        .ok()
                        .and_then(|k| f.usages.get(k))
                        .and_then(button)
                        .unwrap_or(KEY_RESERVED)
                };
                if key == item.last_key {
                    continue;
                }
                if item.last_key != KEY_RESERVED {
                    ev.push_key(item.last_key, false);
                }
                if key != KEY_RESERVED {
                    ev.push_key(key, true);
                }
                item.last_key = key;
            }
        }
        sync = true;
    }
    if sync {
        ev.sync();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Hms>();

#[cfg(test)]
mod tests {
    use super::*;
    use hid::{BOOT_MOUSE, Descriptor};

    #[test]
    fn boot_mouse_maps_three_buttons_and_xy() {
        let d = Descriptor::parse(BOOT_MOUSE).unwrap();
        let m = Mapping::of(&d, 0);
        assert_eq!(m.buttons(), 3);
        assert!(m.caps.rel_x && m.caps.rel_y && !m.caps.wheel);
        assert!(!m.caps.absolute());
    }

    #[test]
    fn suffix_only_for_several_collections() {
        assert_eq!(
            describe("QEMU QEMU USB Tablet", "Tablet", 1),
            "QEMU QEMU USB Tablet"
        );
        assert_eq!(describe("Acme Combo", "Mouse", 1), "Acme Combo");
        assert_eq!(describe("Acme Combo", "Mouse", 2), "Acme Combo Mouse");
        assert_eq!(describe("Acme MOUSE", "Mouse", 2), "Acme MOUSE");
    }
}
