// SPDX-License-Identifier: BSD-2-Clause
//! HID keyboards (`hkbd`) on `hidbus`: input reports become key presses
//! and releases for the input class; the lock LEDs go back as an output
//! report.
//!
//! It registers an evdev device with every key a HID keyboard can name
//! and its lock LEDs, and starts its reports at once (a keyboard types
//! at the console whether or not anyone reads its evdev device).
//!
//! A report's keys are the set of usages its fields name: the modifier
//! bits, any bitmap (n-key rollover) fields, and the key array. Each
//! report is compared with the last; what appeared is pressed (modifiers
//! first), what vanished released. A report of `ErrorRollOver` keys is
//! ignored, as HID 1.11 Appendix C says.
//!
//! Behavior reference: sys/dev/hid/hkbd.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use abi::evdev::{EV_KEY, EV_LED, EV_REP, InputId, LED_CAPSL, LED_NUML, LED_SCROLLL};
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::{HidId, HidIdent};
use hid::usage::{self, KEY_ROLLOVER, KEYBOARD, KEYBOARD_APP, LED};
use hid::{Field, Kind};
use hidbus::HidChild;
use input::{Keyboard, Leds};
use sync::SpinLock;

/// Keys down, as a bitmap of keyboard-page usages.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Keys([u64; 4]);

impl Keys {
    fn set(&mut self, u: u16) {
        if let Some(w) = self.0.get_mut(usize::from(u >> 6)) {
            *w |= 1 << (u & 63);
        }
    }

    fn has(&self, u: u16) -> bool {
        self.0
            .get(usize::from(u >> 6))
            .is_some_and(|w| w & (1 << (u & 63)) != 0)
    }
}

/// The keyboard's input fields, and its LED field.
struct Layout {
    keys: Vec<Field>,
    leds: Option<Field>,
}

impl Layout {
    fn of(child: &HidChild) -> Self {
        let d = child.descriptor();
        let page_of = |f: &Field| f.usage(0).map(usage::page);
        Self {
            keys: d
                .fields_of(child.index)
                .filter(|f| f.kind == Kind::Input && !f.is_constant())
                .filter(|f| page_of(f) == Some(KEYBOARD) || matches!(&f.usages, hid::Usages::Range(lo, _) if usage::page(*lo) == KEYBOARD))
                .cloned()
                .collect(),
            leds: d
                .fields_of(child.index)
                .find(|f| f.kind == Kind::Output && page_of(f) == Some(LED))
                .cloned(),
        }
    }

    /// The keys a report holds down, or `None` for a rollover report.
    fn decode(&self, id: u8, report: &[u8]) -> Option<Keys> {
        let mut k = Keys::default();
        for f in self.keys.iter().filter(|f| f.report_id == id) {
            for i in 0..f.count {
                let Some(v) = f.value(report, i) else { break };
                if f.is_variable() {
                    if v != 0
                        && let Some(u) = f.usage(i)
                    {
                        k.set(usage::id(u));
                    }
                } else {
                    if v < f.logical_min || v > f.logical_max {
                        continue;
                    }
                    let index = u32::try_from(v - f.logical_min).unwrap_or(0);
                    let Some(u) = f.usages.get(index).map(usage::id) else {
                        continue;
                    };
                    if u == KEY_ROLLOVER {
                        return None;
                    }
                    if u != 0 {
                        k.set(u);
                    }
                }
            }
        }
        Some(k)
    }

    /// Whether the LED field has `led`.
    fn has_led(&self, led: u32) -> bool {
        self.leds
            .as_ref()
            .is_some_and(|f| (0..f.count).any(|i| f.usage(i) == Some(led)))
    }

    /// The output report for `leds`.
    fn led_report(&self, leds: Leds) -> Option<(u8, Vec<u8>)> {
        let f = self.leds.as_ref()?;
        let bits = usize::try_from(f.bit + u32::from(f.size) * u32::from(f.count)).ok()?;
        let mut data = vec![0u8; bits.div_ceil(8)];
        for i in 0..f.count {
            let on = match f.usage(i) {
                Some(usage::LED_NUM_LOCK) => leds.num,
                Some(usage::LED_CAPS_LOCK) => leds.caps,
                Some(usage::LED_SCROLL_LOCK) => leds.scroll,
                _ => false,
            };
            if on {
                let bit = f.bit + u32::from(i) * u32::from(f.size);
                let byte = usize::try_from(bit / 8).ok()?;
                *data.get_mut(byte)? |= 1 << (bit % 8);
            }
        }
        if f.report_id != 0 {
            data.insert(0, f.report_id);
        }
        Some((f.report_id, data))
    }
}

/// The `hkbd` driver.
pub struct Hkbd {
    child: Arc<HidChild>,
    layout: Arc<Layout>,
    unit: u32,
    ident: HidIdent,
    name: String,
    kbd: SpinLock<Option<Arc<Keyboard>>>,
    evdev: SpinLock<Option<Arc<evdev::Device>>>,
}

impl core::fmt::Debug for Hkbd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hkbd").field("unit", &self.unit).finish()
    }
}

impl Driver for Hkbd {
    const NAME: &'static str = "hkbd";
    const IDS: Ids = Ids::Hid(&[HidId::usage(KEYBOARD_APP)]);

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Hid(_, name) => name.clone(),
            _ => String::new(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<HidChild>().ok_or(AttachError::Decline)?;
        let layout = Layout::of(&child);
        if layout.keys.is_empty() {
            return Err(AttachError::Device("no key fields"));
        }
        let Ident::Hid(ident, name) = cx.ident().clone() else {
            return Err(AttachError::Decline);
        };
        Ok(Self {
            child,
            layout: Arc::new(layout),
            unit: cx.unit(),
            ident,
            name,
            kbd: SpinLock::new(None),
            evdev: SpinLock::new(None),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let me = alloc::format!("hkbd{}", self.unit);
        let mut b = evdev::Builder::new(&self.name);
        b.phys(&me).uniq(self.child.serial()).id(InputId {
            bustype: self.ident.bus,
            vendor: self.ident.vendor,
            product: self.ident.product,
            version: self.ident.version,
        });
        b.event(EV_KEY);
        let leds = [
            (usage::LED_NUM_LOCK, LED_NUML),
            (usage::LED_CAPS_LOCK, LED_CAPSL),
            (usage::LED_SCROLL_LOCK, LED_SCROLLL),
        ];
        if leds.iter().any(|&(u, _)| self.layout.has_led(u)) {
            b.event(EV_LED);
        }
        b.event(EV_REP);
        for u in 0..=255u8 {
            b.key(evdev::keys::hid2key(u));
        }
        for (u, code) in leds {
            if self.layout.has_led(u) {
                b.led(code);
            }
        }
        let ev = b.register(cx.sleepable()).ok();
        let (child, layout) = (Arc::clone(&self.child), Arc::clone(&self.layout));
        let kbd = input::attach(
            &me,
            move |leds| {
                if let Some((id, data)) = layout.led_report(leds) {
                    child.output(id, data);
                }
            },
            ev.clone(),
        );
        *self.evdev.lock() = ev;
        let last = Arc::new(SpinLock::new(Keys::default()));
        let (k, layout) = (Arc::clone(&kbd), Arc::clone(&self.layout));
        self.child.listen(move |id, report| {
            let Some(now) = layout.decode(id, report) else {
                return;
            };
            let before = core::mem::replace(&mut *last.lock(), now);
            for u in 0..=255u16 {
                if before.has(u) && !now.has(u) {
                    k.key(u, false);
                }
            }
            // Modifiers (0xe0 to 0xe7) before the keys they modify.
            for u in (0..=255u16).rev() {
                if now.has(u) && !before.has(u) {
                    k.key(u, true);
                }
            }
            k.sync();
        });
        *self.kbd.lock() = Some(kbd);
        self.child.intr_start();
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.child.intr_stop();
        self.child.unlisten();
        let kbd = self.kbd.lock().take();
        if let Some(k) = kbd {
            k.detach(cx.sleepable());
        }
        let ev = self.evdev.lock().take();
        if let Some(e) = ev {
            e.unregister(cx.sleepable());
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Hkbd>();
