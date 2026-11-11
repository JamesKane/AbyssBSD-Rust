// SPDX-License-Identifier: BSD-2-Clause
//! HID mice and tablets (`hms`) on `hidbus`, reported through evdev by
//! `hidmap` ([`hidbus::hidmap`]).
//!
//! `hms_map` maps relative X, Y, and Z to `REL_*`, absolute ones to
//! `ABS_*` (with the field's range and resolution), AC Pan to
//! `REL_HWHEEL`, and buttons 1 to 16 to `BTN_MOUSE` on; the wheel's table
//! (`REL_WHEEL`) is added at attach. A collection with no X or Y axis is
//! not a mouse. One with an absolute axis is a tablet: its evdev device is
//! direct (`INPUT_PROP_DIRECT`), a mouse's a pointer.
//!
//! Behavior reference: sys/dev/hid/hms.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use core::any::Any;

use abi::evdev::{
    ABS_X, ABS_Y, ABS_Z, BTN_MOUSE, EV_KEY, INPUT_PROP_DIRECT, INPUT_PROP_POINTER, REL_HWHEEL,
    REL_WHEEL, REL_X, REL_Y, REL_Z,
};
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::HidId;
use hid::hidmap::{Caps, ItemKind, MapItem};
use hid::usage::{BUTTON, CONSUMER, GENERIC_DESKTOP, MOUSE, POINTER};
use hidbus::HidChild;
use hidbus::hidmap::Mapper;

/// Generic Desktop usages (`HUG_*`), and Consumer's AC Pan.
const HUG_X: u16 = 0x30;
const HUG_Y: u16 = 0x31;
const HUG_Z: u16 = 0x32;
const HUG_WHEEL: u16 = 0x38;
const HUC_AC_PAN: u16 = 0x238;

/// `hms_map`'s entries, by index (`enum hms_map_index`).
const REL_X_CAP: usize = 0;
const REL_Y_CAP: usize = 1;
const REL_Z_CAP: usize = 2;
const ABS_X_CAP: usize = 3;
const ABS_Y_CAP: usize = 4;
const ABS_Z_CAP: usize = 5;
const HWHEEL_CAP: usize = 6;

/// `hms_map`. FreeBSD's finalizing callback, which makes the device
/// direct or a pointer, is [`Hms::start`]'s.
static MAP: [MapItem; 8] = [
    MapItem::rel(GENERIC_DESKTOP, HUG_X, REL_X),
    MapItem::rel(GENERIC_DESKTOP, HUG_Y, REL_Y),
    MapItem::rel(GENERIC_DESKTOP, HUG_Z, REL_Z),
    MapItem::abs(GENERIC_DESKTOP, HUG_X, ABS_X),
    MapItem::abs(GENERIC_DESKTOP, HUG_Y, ABS_Y),
    MapItem::abs(GENERIC_DESKTOP, HUG_Z, ABS_Z),
    MapItem::rel(CONSUMER, HUC_AC_PAN, REL_HWHEEL),
    MapItem::key_range(BUTTON, 1, 16, BTN_MOUSE),
];

/// `hms_map_wheel`.
static WHEEL: [MapItem; 1] = [MapItem::rel(GENERIC_DESKTOP, HUG_WHEEL, REL_WHEEL)];

/// Whether `caps` has an absolute X or Y axis.
fn absolute(caps: &Caps) -> bool {
    caps.test(ABS_X_CAP) || caps.test(ABS_Y_CAP)
}

/// The `hms` driver.
pub struct Hms {
    map: Mapper,
    caps: Caps,
    wheel: Caps,
    unit: u32,
    desc: String,
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
        hidbus::hidmap::describe(ident, None, "")
    }

    fn describe_with(ident: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        let Some(child) = bus_data.and_then(|d| d.downcast_ref::<HidChild>()) else {
            return Self::describe(ident);
        };
        let (_, caps) = hid::hidmap::probe(child.descriptor(), child.index, &MAP);
        let suffix = if absolute(&caps) { "Tablet" } else { "Mouse" };
        hidbus::hidmap::describe(ident, bus_data, suffix)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let (map, caps) = Mapper::probe(cx, &MAP)?;
        // There should be at least one X or Y axis.
        if ![REL_X_CAP, REL_Y_CAP, ABS_X_CAP, ABS_Y_CAP]
            .iter()
            .any(|&c| caps.test(c))
        {
            return Err(AttachError::Decline);
        }
        let child = map.child();
        let desc = Self::describe_with(cx.ident(), Some(&**child as &(dyn Any + Send + Sync)));
        // `hms_attach`: the wheel's table, if the mouse has one.
        let wheel = map.add_map(&WHEEL).unwrap_or_default();
        Ok(Self {
            map,
            caps,
            wheel,
            unit: cx.unit(),
            desc,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let direct = absolute(&self.caps);
        self.map
            .attach(cx.sleepable(), Self::NAME, self.unit, &self.desc, |b| {
                // `hms_final_cb`.
                b.prop(if direct {
                    INPUT_PROP_DIRECT
                } else {
                    INPUT_PROP_POINTER
                });
            })?;
        // Variable values mapped to buttons, and the first item's report.
        let (buttons, id) = self.map.with_map(|m| {
            let buttons = m
                .items()
                .iter()
                .filter(|i| {
                    matches!(
                        i.kind,
                        ItemKind::Variable {
                            kind: EV_KEY,
                            null_state: false,
                            ..
                        }
                    )
                })
                .count();
            (buttons, m.items().first().map_or(0, |i| i.id))
        });
        let c = &self.caps;
        bus::manager().log(format_args!(
            "{}{}: {buttons} buttons and [{}{}{}{}{}] coordinates ID={id}",
            Self::NAME,
            self.unit,
            if c.test(REL_X_CAP) || c.test(ABS_X_CAP) {
                "X"
            } else {
                ""
            },
            if c.test(REL_Y_CAP) || c.test(ABS_Y_CAP) {
                "Y"
            } else {
                ""
            },
            if c.test(REL_Z_CAP) || c.test(ABS_Z_CAP) {
                "Z"
            } else {
                ""
            },
            if self.wheel.test(0) { "W" } else { "" },
            if c.test(HWHEEL_CAP) { "H" } else { "" },
        ));
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.map.detach(cx.sleepable());
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
        let (items, caps) = hid::hidmap::probe(&d, 0, &MAP);
        // Three buttons and two axes.
        assert_eq!(items, 5);
        assert!(caps.test(REL_X_CAP) && caps.test(REL_Y_CAP) && caps.test(7));
        assert!(!absolute(&caps));
        let (wheel, _) = hid::hidmap::probe(&d, 0, &WHEEL);
        assert_eq!(wheel, 0);
    }
}
