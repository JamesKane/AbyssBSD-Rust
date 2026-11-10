// SPDX-License-Identifier: BSD-2-Clause
//! `gpio_aei`: ACPI events signalled by GPIO pins (ACPI 6.5 §5.6.5).
//!
//! A GPIO controller's `_AEI` lists `GpioInt` pins whose interrupts are
//! ACPI events: each runs the controller's `_Exx` (edge) or `_Lxx`
//! (level) method for its pin, or `_EVT` with the pin number when there is
//! no such method or the pin is above 255. They are how firmware hears an
//! embedded controller or a lid on GPIO-signalled platforms (the Sky1's
//! `GPI4` takes the EC's line; the X870E's `GPIO` one too).
//!
//! As in FreeBSD, the controller's driver adds a `gpio_aei` child for its
//! `_AEI` once it serves pins ([`add`]); here the child's lines are those
//! pins, and the [`GpioAei`] driver asks for them like any `GpioInt`
//! consumer. The methods are AML, so the handlers are threaded: a level
//! pin stays masked until its method has run.
//!
//! An event whose method fails is disabled and logged: its filter calls
//! the pin's interrupts strays from then on, so storm detection masks a
//! pin that keeps firing. A level event whose method cannot clear its
//! source would otherwise run forever. The Sky1's `_L06` reads the EC
//! through a `GenericSerialBus` region on `I2C6`, a bus firmware hides,
//! so until that region has a handler it fails, and FreeBSD (which lets
//! it run) would loop.
//!
//! Behavior reference: sys/dev/gpio/gpioaei.c, sys/dev/gpio/acpi_gpiobus.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use aml::resource::{GpioKind, Resource as AmlResource};
use bus::gpio::{GpioMode, GpioUse};
use bus::{
    AttachCx, AttachError, DeviceId, Driver, DriverDecl, Ident, Ids, IrqLine, Line, LineDetail,
    LineKind, StartCx, StopCx, StopReason,
};
use sched::Sleepable;

use crate::service::service;

/// One event: a pin and the method its interrupt runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AeiEvent {
    /// The pin, by the controller's numbering, and how it signals.
    pub pin: GpioUse,
    /// `_Exx`, `_Lxx`, or `_EVT` (which takes the pin number).
    pub method: String,
}

/// What [`add`] hands the `gpio_aei` child: the controller's namespace
/// node and its events, in `_AEI` order.
#[derive(Debug)]
pub struct AeiEvents {
    /// The controller, whose methods the events run.
    pub controller: aml::NodeId,
    /// The events; line `i` of the child is event `i`'s pin.
    pub events: Vec<AeiEvent>,
}

/// The events a controller's `_AEI` describes, and notes on pins it
/// leaves out (no method to run), as FreeBSD's `gpio_aei_enumerate`
/// finds them: each `GpioInt` pin is one event.
pub fn events(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    controller: aml::NodeId,
) -> (Vec<AeiEvent>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let aei = interp
        .resources(controller, "_AEI", host)
        .ok()
        .flatten()
        .unwrap_or_default();
    for r in aei {
        let AmlResource::Gpio(g) = r else { continue };
        let GpioKind::Interrupt {
            trigger,
            polarity,
            shared,
            wake,
        } = g.kind
        else {
            continue;
        };
        let pull = crate::bus::gpio_pull(g.pin_config);
        let edge = matches!(trigger, aml::resource::Trigger::Edge);
        for &pin in &g.pins {
            let named = alloc::format!("_{}{:02X}", if edge { 'E' } else { 'L' }, pin);
            let method = if pin <= 255 && interp.child(controller, &named).is_some() {
                named
            } else if interp.child(controller, "_EVT").is_some() {
                String::from("_EVT")
            } else {
                notes.push(alloc::format!(
                    "{}: _AEI pin {pin} has no {named} or _EVT",
                    interp.path(controller)
                ));
                continue;
            };
            out.push(AeiEvent {
                pin: GpioUse {
                    pins: alloc::vec![u32::from(pin)],
                    mode: GpioMode::Interrupt {
                        trigger: crate::bus::trigger(trigger),
                        polarity: crate::bus::gpio_polarity(polarity),
                        wake,
                    },
                    shared,
                    pull,
                    debounce_us: u32::from(g.debounce) * 10,
                },
                method,
            });
        }
    }
    (out, notes)
}

/// Adds the `gpio_aei` child for the `_AEI` of `controller`, whose pins
/// `supplier` serves, under the device `cx` is starting (the controller's
/// `gpiobus`, as FreeBSD's `acpi_gpiobus_attach_aei` adds it), if it has
/// events. Call it once the pins are published (`StartCx::provide_gpio`).
pub fn add<D: Driver>(cx: &mut StartCx<'_, D>, controller: aml::NodeId, supplier: DeviceId) {
    let Some(svc) = service() else { return };
    let (events, notes, handle) = svc.run(cx.sleepable(), move |interp, host| {
        let (e, n) = events(interp, host, controller);
        (e, n, alloc::format!("{}._AEI", interp.path(controller)))
    });
    if bus::bootverbose() {
        for n in &notes {
            svc.ops().log(format_args!("acpi0: {n}"));
        }
    }
    if events.is_empty() {
        return;
    }
    let me = supplier;
    let lines = events
        .iter()
        .map(|e| Line {
            kind: LineKind::Gpio,
            name: None,
            supplier: me,
            id: e.pin.pins[0],
            detail: LineDetail::Gpio(e.pin.clone()),
        })
        .collect();
    let pins: Vec<u32> = events.iter().map(|e| e.pin.pins[0]).collect();
    let child = cx.add_child(
        Ident::Pins {
            class: "gpio_aei",
            desc: "ACPI Event Information Device",
            pins: bus::gpio::pin_list(&pins),
            handle: Some(handle),
        },
        Some(Arc::new(AeiEvents { controller, events })),
        Vec::new(),
    );
    cx.set_lines(child, lines);
}

/// `gpio_aeiN`.
#[derive(Debug)]
pub struct GpioAei {
    data: Arc<AeiEvents>,
    /// Each event's line, until `start` installs them.
    lines: sync::SpinLock<Vec<(usize, IrqLine)>>,
    /// Events whose method failed.
    failed: Vec<AtomicBool>,
    unit: u32,
}

fn filter(d: &GpioAei, i: usize, _: &sched::IrqCx) -> irq::Filter {
    match d.failed.get(i) {
        Some(f) if !f.load(Ordering::Relaxed) => irq::Filter::Schedule,
        _ => irq::Filter::Stray,
    }
}

fn event(d: &GpioAei, i: usize, s: &Sleepable) {
    let (Some(svc), Some(e)) = (service(), d.data.events.get(i)) else {
        return;
    };
    let node = d.data.controller;
    let method = e.method.clone();
    let args = if method == "_EVT" {
        alloc::vec![aml::Value::Integer(u64::from(e.pin.pins[0]))]
    } else {
        Vec::new()
    };
    let r = svc.run(s, move |interp, host| {
        interp.evaluate_child(node, &method, args, host).map(|_| ())
    });
    if let Err(err) = r
        && let Some(f) = d.failed.get(i)
        && !f.swap(true, Ordering::Relaxed)
    {
        svc.ops().log(format_args!(
            "gpio_aei{}: pin {} {}: {err:?}; event disabled",
            d.unit, e.pin.pins[0], e.method
        ));
    }
}

impl Driver for GpioAei {
    const NAME: &'static str = "gpio_aei";
    const IDS: Ids = Ids::Nexus(&["gpio_aei"]);

    fn describe(_: &Ident) -> String {
        "ACPI Event Information Device".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let data = cx.bus_data_arc::<AeiEvents>().ok_or(AttachError::Decline)?;
        let mut lines = Vec::new();
        for (i, e) in data.events.iter().enumerate() {
            // A pin that cannot be had is left out, as FreeBSD does.
            match cx.gpio_interrupt(i) {
                Ok(l) => lines.push((i, l)),
                Err(err) => {
                    if let Some(svc) = service() {
                        svc.ops().log(format_args!(
                            "gpio_aei{}: pin {}: {err:?}",
                            cx.unit(),
                            e.pin.pins[0]
                        ));
                    }
                }
            }
        }
        let failed = data.events.iter().map(|_| AtomicBool::new(false)).collect();
        Ok(Self {
            data,
            lines: sync::SpinLock::new(lines),
            failed,
            unit: cx.unit(),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let lines = core::mem::take(&mut *self.lines.lock());
        for (i, line) in lines {
            cx.interrupt(line, Some(filter), Some(event), i)?;
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const GPIO_AEI: DriverDecl = DriverDecl::of::<GpioAei>();
