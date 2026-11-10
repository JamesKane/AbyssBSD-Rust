// SPDX-License-Identifier: BSD-2-Clause
//! `gpiobus` and `gpioc`: a GPIO controller's bus, and `/dev/gpiocN`, which
//! `gpioctl(8)` and `libgpio` use.
//!
//! A controller's driver publishes its pins (`StartCx::provide_gpio`) and
//! adds a `gpiobus` child with [`GpioBusData`]; the bus adds `gpioc` at
//! every pin, and, for an ACPI controller, `gpio_aei` for its `_AEI`
//! (FreeBSD's `acpi_gpiobus`, whose description it takes). Firmware's own
//! consumers reach the pins through their supplier edges, not through
//! the bus.
//!
//! `gpioc` passes each request to the controller past any driver's claim on
//! the pin, as FreeBSD's `gpiobus` does. Pin names set with `GPIOSETNAME`
//! are the bus's, shared by its users. No pin interrupts reach userland:
//! a pin asked to interrupt is refused with `EOPNOTSUPP` (no controller
//! here offers it to `gpioc`), so `read` finds no event pin and answers
//! `ENXIO`, as FreeBSD's does without one.
//!
//! Behavior reference: sys/dev/gpio/gpiobus.c, sys/dev/gpio/acpi_gpiobus.c,
//! sys/dev/gpio/gpioc.c.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

#[cfg(test)]
#[allow(unsafe_code)]
mod tests;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::gpio::{self as g, GpioPin, GpioReq, intr, pin};
use bus::gpio::{GpioSource, PinMode, PinState, Pull, pin_list};
use bus::provider::ProviderError;
use bus::{
    AttachCx, AttachError, DeviceId, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason,
};
use sched::Sleepable;
use sync::SpinLock;
use vfs::{CharDevice, Uio};

/// What a controller gives its `gpiobus`.
pub struct GpioBusData {
    /// Its pins.
    pub source: Arc<GpioSource>,
    /// The controller, which serves the pins `gpio_aei` asks for.
    pub controller: DeviceId,
    /// Its ACPI namespace node, if firmware describes it there.
    pub acpi: Option<aml::NodeId>,
}

impl core::fmt::Debug for GpioBusData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpioBusData")
            .field("controller", &self.controller)
            .finish_non_exhaustive()
    }
}

/// What a `gpiobus` shares with its `gpioc`: the pins and their names.
struct Bus {
    source: Arc<GpioSource>,
    /// Names set with `GPIOSETNAME` (`gpiobus`'s `sc_pins[].name`).
    names: SpinLock<BTreeMap<u32, String>>,
}

/// `gpiobusN`.
pub struct GpioBus {
    bus: Arc<Bus>,
    controller: DeviceId,
    acpi: Option<aml::NodeId>,
}

impl core::fmt::Debug for GpioBus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpioBus").finish_non_exhaustive()
    }
}

impl Driver for GpioBus {
    const NAME: &'static str = "gpiobus";
    const IDS: Ids = Ids::Nexus(&["gpiobus"]);

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Child { desc, .. } => (*desc).into(),
            _ => "GPIO bus".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let d = cx.bus_data::<GpioBusData>().ok_or(AttachError::Decline)?;
        Ok(Self {
            bus: Arc::new(Bus {
                source: Arc::clone(&d.source),
                names: SpinLock::new(BTreeMap::new()),
            }),
            controller: d.controller,
            acpi: d.acpi,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let all: Vec<u32> = (0..self.bus.source.pins()).collect();
        cx.add_child(
            Ident::Pins {
                class: "gpioc",
                desc: "GPIO controller",
                pins: pin_list(&all),
                handle: self.acpi.map(|_| String::from("(unknown)")),
            },
            Some(Arc::clone(&self.bus) as Arc<dyn core::any::Any + Send + Sync>),
            Vec::new(),
        );
        if let Some(node) = self.acpi {
            acpi::aei::add(cx, node, self.controller);
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The description of the `gpiobus` a controller adds:
/// `acpi_gpiobus`'s for one firmware describes in ACPI.
#[must_use]
pub fn bus_ident(acpi: bool) -> Ident {
    Ident::Child {
        class: "gpiobus",
        desc: if acpi {
            "GPIO bus (ACPI-hinted)"
        } else {
            "GPIO bus"
        },
    }
}

/// `gpiocN`.
pub struct Gpioc {
    path: String,
}

impl core::fmt::Debug for Gpioc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gpioc").field("path", &self.path).finish()
    }
}

impl Driver for Gpioc {
    const NAME: &'static str = "gpioc";
    const IDS: Ids = Ids::Nexus(&["gpioc"]);

    fn describe(_: &Ident) -> String {
        "GPIO controller".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let bus = cx.bus_data_arc::<Bus>().ok_or(AttachError::Decline)?;
        let path = format!("gpioc{}", cx.unit());
        // `make_dev`: root's alone (`gpioc_attach`).
        vfs::devices::make_dev(path.as_bytes(), 0o600, 0, 0, Arc::new(Node { bus }))
            .ok_or(AttachError::Device("gpioc node"))?;
        Ok(Self { path })
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        vfs::devices::destroy_dev(cx.sleepable(), self.path.as_bytes());
    }
}

/// `/dev/gpiocN`.
struct Node {
    bus: Arc<Bus>,
}

/// An error from the controller as `gpiobus` passes it on: a pin past the
/// last is `EINVAL` (`gpiobus`'s own check), what the controller cannot do
/// `EOPNOTSUPP` (`gpio_default_nosupport`).
fn errno(e: ProviderError) -> Errno {
    match e {
        ProviderError::NoSuchLine | ProviderError::OutOfRange => Errno::EINVAL,
        ProviderError::Unsupported => Errno::EOPNOTSUPP,
        ProviderError::Revoked => Errno::ENXIO,
        _ => Errno::EIO,
    }
}

/// `GPIO_EVENT_REPORT_DETAIL`, `GPIO_EVENT_REPORT_SUMMARY`.
const REPORT_DETAIL: u32 = 0;
const REPORT_SUMMARY: u32 = 1;

/// `sizeof(struct gpio_event_detail)`: the least `read` takes.
const EVENT_DETAIL_LEN: usize = 16;

impl Node {
    /// The `gpio_pin` `GPIOGETCONFIG` answers for `p`.
    fn config(&self, s: &Sleepable, p: u32) -> Result<GpioPin, Errno> {
        let raw = self.bus.source.raw(p).map_err(errno)?;
        let flags = match raw.state(s).map_err(errno)? {
            PinState::Input => pin::INPUT,
            PinState::Output => pin::OUTPUT,
            PinState::Peripheral => 0,
        };
        let c = raw.caps();
        let caps = [
            (c.input, pin::INPUT),
            (c.output, pin::OUTPUT),
            (c.pull_up, pin::PULLUP),
            (c.pull_down, pin::PULLDOWN),
        ]
        .into_iter()
        .filter(|&(has, _)| has)
        .fold(0, |caps, (_, bit)| caps | bit);
        let name = self
            .bus
            .names
            .lock()
            .get(&p)
            .cloned()
            .unwrap_or_else(|| raw.name());
        let mut out = GpioPin {
            gp_pin: p,
            gp_name: [0; g::GPIOMAXNAME],
            gp_caps: caps,
            gp_flags: flags,
        };
        let n = name.len().min(g::GPIOMAXNAME - 1);
        out.gp_name[..n].copy_from_slice(&name.as_bytes()[..n]);
        Ok(out)
    }

    /// `GPIOSETCONFIG`: `gpio_check_flags`, the interrupt part refused
    /// (no pin offers one), then the rest to the controller, which takes
    /// only what it can do (`EINVAL` for the rest, as Cadence's does).
    fn set_config(&self, s: &Sleepable, p: &GpioPin) -> Result<(), Errno> {
        let current = self.config(s, p.gp_pin)?;
        let caps = current.gp_caps;
        if !g::flags_valid(caps, p.gp_flags) {
            return Err(Errno::EINVAL);
        }
        match p.gp_flags & intr::MASK {
            intr::NONE => {}
            // Only edges are offered, and only by a pin that can: none here.
            intr::EDGE_RISING | intr::EDGE_FALLING | intr::EDGE_BOTH => {
                return Err(Errno::EOPNOTSUPP);
            }
            _ => return Err(Errno::EINVAL),
        }
        let flags = p.gp_flags & !intr::MASK;
        if flags & !caps != 0 {
            return Err(Errno::EINVAL);
        }
        let pull = match (flags & pin::PULLUP != 0, flags & pin::PULLDOWN != 0) {
            (true, _) => Pull::Up,
            (_, true) => Pull::Down,
            _ => Pull::Default,
        };
        let raw = self.bus.source.raw(p.gp_pin).map_err(errno)?;
        let mode = if flags & pin::OUTPUT != 0 {
            // It starts driving what the pad shows, so it does not glitch
            // (Cadence's FreeBSD driver drives whatever its output register
            // last held).
            PinMode::Output {
                high: raw.get(s).map_err(errno)?,
            }
        } else if flags & pin::INPUT != 0 {
            PinMode::Input
        } else {
            // Neither direction: nothing to change.
            return Ok(());
        };
        raw.configure(s, mode, pull).map_err(errno)
    }
}

impl CharDevice for Node {
    fn read(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        if uio.resid() < EVENT_DETAIL_LEN {
            return Err(Errno::EINVAL);
        }
        // No pin of this file reports events.
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let src = &self.bus.source;
        match req {
            g::GPIOMAXPIN => {
                let max = src.pins().saturating_sub(1);
                arg.get_mut(..4)
                    .ok_or(Errno::EINVAL)?
                    .copy_from_slice(&max.to_ne_bytes());
                Ok(())
            }
            g::GPIOGETCONFIG => {
                let p = GpioPin::from_bytes(arg).ok_or(Errno::EINVAL)?;
                self.config(s, p.gp_pin)?.store(arg);
                Ok(())
            }
            g::GPIOSETCONFIG => {
                let p = GpioPin::from_bytes(arg).ok_or(Errno::EINVAL)?;
                self.set_config(s, &p)
            }
            g::GPIOGET => {
                let mut r = GpioReq::from_bytes(arg).ok_or(Errno::EINVAL)?;
                let high = src.raw(r.gp_pin).and_then(|p| p.get(s)).map_err(errno)?;
                r.gp_value = u32::from(high);
                r.store(arg);
                Ok(())
            }
            g::GPIOSET => {
                let r = GpioReq::from_bytes(arg).ok_or(Errno::EINVAL)?;
                src.raw(r.gp_pin)
                    .and_then(|p| p.set(s, r.gp_value != 0))
                    .map_err(errno)
            }
            g::GPIOTOGGLE => {
                let r = GpioReq::from_bytes(arg).ok_or(Errno::EINVAL)?;
                src.raw(r.gp_pin).and_then(|p| p.toggle(s)).map_err(errno)
            }
            g::GPIOSETNAME => {
                let p = GpioPin::from_bytes(arg).ok_or(Errno::EINVAL)?;
                src.raw(p.gp_pin).map_err(errno)?;
                let name = String::from_utf8_lossy(p.name()).into_owned();
                self.bus.names.lock().insert(p.gp_pin, name);
                Ok(())
            }
            // No controller here does 32 pins at once
            // (`gpio_default_nosupport`).
            g::GPIOACCESS32 | g::GPIOCONFIG32 => Err(Errno::EOPNOTSUPP),
            g::GPIOCONFIGEVENTS => {
                // `GPIO_EVENT_REPORT_DETAIL` or `_SUMMARY`; kept by no pin.
                let kind = arg.get(..4).ok_or(Errno::EINVAL)?;
                match u32::from_ne_bytes([kind[0], kind[1], kind[2], kind[3]]) {
                    REPORT_DETAIL | REPORT_SUMMARY => Ok(()),
                    _ => Err(Errno::EINVAL),
                }
            }
            _ => Err(Errno::ENOTTY),
        }
    }
}

/// The declarations for the build's driver table.
pub const GPIOBUS: DriverDecl = DriverDecl::of::<GpioBus>();
pub const GPIOC: DriverDecl = DriverDecl::of::<Gpioc>();
