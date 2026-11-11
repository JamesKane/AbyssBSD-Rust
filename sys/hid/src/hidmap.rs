// SPDX-License-Identifier: BSD-2-Clause
//! `hidmap`'s kernel half: a `hidbus` child whose collection a driver's
//! tables map to evdev events (`hms`, `hcons`, `hsctrl`). The mapping
//! itself is `lib/hid`'s [`hid::hidmap`]; this builds the evdev device
//! from it, runs the reports through it while the device is open, and
//! adds the `dev.<driver>.<unit>.debug` knob FreeBSD's `HID_DEBUG`
//! kernels (15.1's GENERIC among them) give every `hidmap` driver.
//!
//! A driver's `attach` calls [`Mapper::probe`] (`hidmap_probe`, the
//! collection's usage already matched by the bus) and may add more tables
//! ([`Mapper::add_map`]); its `start` calls [`Mapper::attach`]
//! (`hidmap_attach`) and its `stop` [`Mapper::detach`].
//!
//! Behavior reference: sys/dev/hid/hidmap.c.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

use core::sync::atomic::{AtomicI32, Ordering};

use abi::errno::Errno;
use abi::evdev::InputId;
use abi::sysctl::ctlflag;
use bus::{AttachCx, AttachError};
use devtree::ids::HidIdent;
use hid::hidmap::{AddError, Caps, Events, Hidmap, MapItem, Support};
use sched::Sleepable;
use sync::SpinLock;
use sysctl::{Number, Value};

use crate::HidChild;

/// An evdev device being built, as `hidmap` tells it what it reports.
struct Building<'a>(&'a mut evdev::Builder);

impl Support for Building<'_> {
    fn event(&mut self, kind: u16) {
        self.0.event(kind);
    }

    fn key(&mut self, code: u16) {
        self.0.key(code);
    }

    fn rel(&mut self, code: u16) {
        self.0.rel(code);
    }

    fn abs(&mut self, code: u16, min: i32, max: i32, fuzz: i32, flat: i32, resolution: i32) {
        self.0.abs(code, min, max, fuzz, flat, resolution);
    }

    fn sw(&mut self, code: u16) {
        self.0.sw(code);
    }
}

/// A registered evdev device, as `hidmap` reports to it.
struct Reporting<'a>(&'a evdev::Device);

impl Events for Reporting<'_> {
    fn push(&mut self, kind: u16, code: u16, value: i32) {
        // Codes the device does not report are refused (EINVAL), as
        // `evdev_push_event` refuses them.
        let _ = self.0.push(kind, code, value);
    }

    fn sync(&mut self) {
        self.0.sync();
    }
}

/// Starts and stops the reports as evdev clients come and go
/// (`hidmap_ev_open`, `hidmap_ev_close`).
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

/// A `hidmap` driver's state (`struct hidmap`).
pub struct Mapper {
    child: Arc<HidChild>,
    map: Arc<SpinLock<Hidmap>>,
    ident: HidIdent,
    evdev: SpinLock<Option<Arc<evdev::Device>>>,
    debug: Arc<AtomicI32>,
}

impl core::fmt::Debug for Mapper {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mapper")
            .field("index", &self.child.index)
            .finish_non_exhaustive()
    }
}

impl Mapper {
    /// `hidmap_probe`: the child's collection, if `map` maps something in
    /// it, with the entries that matched.
    ///
    /// # Errors
    ///
    /// [`AttachError::Decline`] if the bus data is not a `hidbus` child's
    /// or the table maps nothing (FreeBSD's `ENXIO`).
    pub fn probe(
        cx: &mut AttachCx<'_>,
        map: &'static [MapItem],
    ) -> Result<(Self, Caps), AttachError> {
        let child = cx.bus_data_arc::<HidChild>().ok_or(AttachError::Decline)?;
        let bus::Ident::Hid(ident, _) = cx.ident().clone() else {
            return Err(AttachError::Decline);
        };
        let mut m = Hidmap::new();
        let caps = m
            .add_map(child.descriptor(), child.index, map)
            .map_err(|_| AttachError::Decline)?;
        Ok((
            Self {
                child,
                map: Arc::new(SpinLock::new(m)),
                ident,
                evdev: SpinLock::new(None),
                debug: Arc::new(AtomicI32::new(0)),
            },
            caps,
        ))
    }

    /// `hidmap_add_map`: another table, before [`Mapper::attach`].
    ///
    /// # Errors
    ///
    /// If it maps nothing, or the device has its tables already.
    pub fn add_map(&self, map: &'static [MapItem]) -> Result<Caps, AddError> {
        let c = &self.child;
        self.map.lock().add_map(c.descriptor(), c.index, map)
    }

    /// The child.
    #[must_use]
    pub fn child(&self) -> &Arc<HidChild> {
        &self.child
    }

    /// The mapped values, for what a driver says about them at attach.
    pub fn with_map<R>(&self, f: impl FnOnce(&Hidmap) -> R) -> R {
        f(&self.map.lock())
    }

    /// `hidmap_attach`: the evdev device for driver instance `driver`
    /// `unit`, named `desc`, reporting what the tables map; `finish` adds
    /// what the driver reports besides (FreeBSD's finalizing callbacks at
    /// attach). Reports run from the first open to the last close.
    ///
    /// # Errors
    ///
    /// If the evdev device cannot be registered.
    pub fn attach(
        &self,
        s: &Sleepable,
        driver: &'static str,
        unit: u32,
        desc: &str,
        finish: impl FnOnce(&mut evdev::Builder),
    ) -> Result<Arc<evdev::Device>, AttachError> {
        let me = format!("{driver}{unit}");
        let mut b = evdev::Builder::new(desc);
        b.phys(&me).uniq(self.child.serial()).id(InputId {
            bustype: self.ident.bus,
            vendor: self.ident.vendor,
            product: self.ident.product,
            version: self.ident.version,
        });
        {
            let c = &self.child;
            self.map
                .lock()
                .attach(c.descriptor(), c.index, &mut Building(&mut b));
        }
        finish(&mut b);
        b.methods(Arc::new(Methods(Arc::clone(&self.child))));
        let map = Arc::clone(&self.map);
        let ev = b
            .register(s)
            .map_err(|_| AttachError::Device("no evdev device"))?;
        let e = Arc::clone(&ev);
        self.child.listen(move |id, report| {
            map.lock().intr(id, report, &mut Reporting(&e));
        });
        *self.evdev.lock() = Some(Arc::clone(&ev));
        self.add_debug(s, driver, unit);
        Ok(ev)
    }

    /// `dev.<driver>.<unit>.debug`, as `hidmap_attach` adds it under
    /// `HID_DEBUG` (it starts at 0: `hw.hid.<driver>.debug` is not read).
    fn add_debug(&self, s: &Sleepable, driver: &'static str, unit: u32) {
        let (r, w) = (Arc::clone(&self.debug), Arc::clone(&self.debug));
        bus::manager().sysctl(s, &mut |t| {
            let Some(node) = bus::device_node(t, driver, unit) else {
                return;
            };
            let (r, w) = (Arc::clone(&r), Arc::clone(&w));
            let _ = t.add_writable(
                node,
                Number::Auto,
                "debug",
                "Verbosity level",
                ctlflag::TUN | ctlflag::MPSAFE,
                // A knob nothing orders against.
                Value::Int(alloc::boxed::Box::new(move || r.load(Ordering::Relaxed))),
                sysctl::int_writer(move |v| {
                    w.store(v, Ordering::Relaxed);
                    Ok(())
                }),
            );
        });
    }

    /// `hidmap_detach`: the reports stopped, the callbacks told, and the
    /// evdev device gone (its clients closed).
    pub fn detach(&self, s: &Sleepable) {
        self.child.unlisten();
        self.map.lock().detach();
        let ev = self.evdev.lock().take();
        if let Some(e) = ev {
            e.unregister(s);
        }
    }
}

/// `hidbus_set_desc`: the device's name, with `suffix` unless the name
/// has it already (any case) or the device has only this collection.
#[must_use]
pub fn set_desc(name: &str, suffix: &str, collections: usize) -> String {
    let has = name
        .to_ascii_lowercase()
        .contains(&suffix.to_ascii_lowercase());
    if has || collections <= 1 {
        String::from(name)
    } else {
        format!("{name} {suffix}")
    }
}

/// The description of a `hidbus` child with `suffix`, for a driver's
/// `describe_with`.
#[must_use]
pub fn describe(
    ident: &bus::Ident,
    bus_data: Option<&(dyn core::any::Any + Send + Sync)>,
    suffix: &str,
) -> String {
    match (ident, bus_data.and_then(|d| d.downcast_ref::<HidChild>())) {
        (bus::Ident::Hid(_, name), Some(c)) => {
            set_desc(name, suffix, c.descriptor().collections.len())
        }
        (bus::Ident::Hid(_, name), None) => name.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::set_desc;

    #[test]
    fn suffix_only_for_several_collections() {
        assert_eq!(
            set_desc("QEMU QEMU USB Tablet", "Tablet", 1),
            "QEMU QEMU USB Tablet"
        );
        assert_eq!(set_desc("Acme Combo", "Mouse", 1), "Acme Combo");
        assert_eq!(set_desc("Acme Combo", "Mouse", 2), "Acme Combo Mouse");
        assert_eq!(set_desc("Acme MOUSE", "Mouse", 2), "Acme MOUSE");
        assert_eq!(
            set_desc("vendor 0x0c45 product 0x0133", "Consumer Control", 2),
            "vendor 0x0c45 product 0x0133 Consumer Control"
        );
    }
}
