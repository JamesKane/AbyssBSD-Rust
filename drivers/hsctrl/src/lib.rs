// SPDX-License-Identifier: BSD-2-Clause
//! System Control collections (`hsctrl`) on `hidbus`: a keyboard's power,
//! sleep, and wake keys and the system menu controls, reported through
//! evdev by `hidmap` ([`hidbus::hidmap`]) with FreeBSD's usage-to-key
//! table.
//!
//! A collection of usage System Control (Generic Desktop page, usage
//! 0x80) is `hsctrl`'s if the table maps one of its input values. As on
//! FreeBSD, the keys are evdev events and nothing else: Power Down is
//! `KEY_POWER` for whatever reads the device (a desktop's session manager),
//! and does not shut the system down or reach `acpi_button`'s power
//! button.
//!
//! Behavior reference: sys/dev/hid/hsctrl.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use core::any::Any;

use abi::evdev as ev;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::HidId;
use hid::hidmap::MapItem;
use hid::usage::GENERIC_DESKTOP;
use hidbus::hidmap::Mapper;

/// Generic Desktop: System Control (`HUG_SYSTEM_CONTROL`), the
/// collection's usage.
pub const SYSTEM_CONTROL: u32 = 0x0001_0080;

/// What [`hidbus::hidmap::set_desc`] adds to the device's name.
const SUFFIX: &str = "System Control";

/// The System Control usages (`HUG_SYSTEM_*`).
pub mod usage {
    /// System Power Down.
    pub const POWER_DOWN: u16 = 0x81;
    /// System Sleep.
    pub const SLEEP: u16 = 0x82;
    /// System Wake Up.
    pub const WAKEUP: u16 = 0x83;
    /// System Context Menu.
    pub const CONTEXT_MENU: u16 = 0x84;
    /// System Main Menu.
    pub const MAIN_MENU: u16 = 0x85;
    /// System App Menu.
    pub const APP_MENU: u16 = 0x86;
    /// System Menu Help.
    pub const MENU_HELP: u16 = 0x87;
    /// System Menu Exit.
    pub const MENU_EXIT: u16 = 0x88;
    /// System Menu Select.
    pub const MENU_SELECT: u16 = 0x89;
    /// System Menu Right.
    pub const MENU_RIGHT: u16 = 0x8a;
    /// System Menu Left.
    pub const MENU_LEFT: u16 = 0x8b;
    /// System Menu Up.
    pub const MENU_UP: u16 = 0x8c;
    /// System Menu Down.
    pub const MENU_DOWN: u16 = 0x8d;
    /// System Cold Restart (`HUG_SYSTEM_POWER_UP`).
    pub const POWER_UP: u16 = 0x8e;
    /// System Warm Restart (`HUG_SYSTEM_RESTART`).
    pub const RESTART: u16 = 0x8f;
}

/// `hsctrl_map`.
pub static MAP: [MapItem; 15] = [
    MapItem::key(GENERIC_DESKTOP, usage::POWER_DOWN, ev::KEY_POWER),
    MapItem::key(GENERIC_DESKTOP, usage::SLEEP, ev::KEY_SLEEP),
    MapItem::key(GENERIC_DESKTOP, usage::WAKEUP, ev::KEY_WAKEUP),
    MapItem::key(GENERIC_DESKTOP, usage::CONTEXT_MENU, ev::KEY_CONTEXT_MENU),
    MapItem::key(GENERIC_DESKTOP, usage::MAIN_MENU, ev::KEY_MENU),
    MapItem::key(GENERIC_DESKTOP, usage::APP_MENU, ev::KEY_PROG1),
    MapItem::key(GENERIC_DESKTOP, usage::MENU_HELP, ev::KEY_HELP),
    MapItem::key(GENERIC_DESKTOP, usage::MENU_EXIT, ev::KEY_EXIT),
    MapItem::key(GENERIC_DESKTOP, usage::MENU_SELECT, ev::KEY_SELECT),
    MapItem::key(GENERIC_DESKTOP, usage::MENU_RIGHT, ev::KEY_RIGHT),
    MapItem::key(GENERIC_DESKTOP, usage::MENU_LEFT, ev::KEY_LEFT),
    MapItem::key(GENERIC_DESKTOP, usage::MENU_UP, ev::KEY_UP),
    MapItem::key(GENERIC_DESKTOP, usage::MENU_DOWN, ev::KEY_DOWN),
    MapItem::key(GENERIC_DESKTOP, usage::POWER_UP, ev::KEY_POWER2),
    MapItem::key(GENERIC_DESKTOP, usage::RESTART, ev::KEY_RESTART),
];

/// The `hsctrl` driver.
pub struct Hsctrl {
    map: Mapper,
    unit: u32,
    desc: String,
}

impl core::fmt::Debug for Hsctrl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hsctrl").field("unit", &self.unit).finish()
    }
}

impl Driver for Hsctrl {
    const NAME: &'static str = "hsctrl";
    const IDS: Ids = Ids::Hid(&[HidId::usage(SYSTEM_CONTROL)]);

    fn describe(ident: &Ident) -> String {
        hidbus::hidmap::describe(ident, None, SUFFIX)
    }

    fn describe_with(ident: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        hidbus::hidmap::describe(ident, bus_data, SUFFIX)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let (map, _) = Mapper::probe(cx, &MAP)?;
        let child = map.child();
        let desc = Self::describe_with(cx.ident(), Some(&**child as &(dyn Any + Send + Sync)));
        Ok(Self {
            map,
            unit: cx.unit(),
            desc,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        self.map
            .attach(cx.sleepable(), Self::NAME, self.unit, &self.desc, |_| {})?;
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.map.detach(cx.sleepable());
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Hsctrl>();

#[cfg(test)]
mod tests;
