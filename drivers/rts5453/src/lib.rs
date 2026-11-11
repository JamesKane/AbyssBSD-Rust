// SPDX-License-Identifier: BSD-2-Clause
//! `rts5453`: the Realtek RTS5453 USB Type-C power-delivery controllers,
//! as the CIX Sky1 boards' firmware describes them (`CIXH200D`, two on
//! I2C7 at 0x30 and 0x31 on the Orange Pi 6 Plus).
//!
//! The controller runs its port itself (CC detection, PD negotiation,
//! DisplayPort alternate mode); this only watches. Each unit polls the
//! six-byte status at 0x5f every `dev.rts5453.N.poll_ms` (1000), keeps
//! the last, and logs it when it changes ("USB, reverse, data host, power
//! sink"); a change after the first is acknowledged (0x50: the interrupt,
//! and a DisplayPort HPD IRQ when one is pending), which releases the
//! chip's shared interrupt line. The sysctls decode the last status:
//! `mode`, `orientation`, `data_role`, `power_role`, `hpd`, and the raw
//! `status` bytes 1 to 5; "unknown" until it is first read.
//!
//! Polled, as the sibling port's driver is: nothing here takes the
//! `GpioInt` the firmware gives the pair (S5 GPIO 0 line 7, level low).
//! The only writes are those acknowledgements; nothing changes a role
//! (the chip takes no role swap from the host).
//!
//! Behavior reference: the AbyssBSD Project's FreeBSD driver,
//! ../AbyssBSD/src/sys/dev/iicbus/rts5453.c (BSD-2-Clause, by this
//! project's author); the clean-room spec
//! chinese-wall/specs/sky1-peripherals/04-usb-pd-rts5453.md.

#![no_std]
// The tests make a `Sleepable` for the host; the driver has no unsafe
// code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use abi::sysctl::ctlflag;
use bus::i2c::I2cDevice;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::AcpiId;
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use sysctl::{Number, Value};

/// The interrupt and HPD acknowledge register, and its bits (in the
/// payload's second and third bytes).
const DATA_CTRL: u8 = 0x50;
const INT_ACK: u8 = 1 << 2;
const HPD_IRQ_ACK: u8 = 1 << 5;
/// The status register: a length byte, then the status.
const DATA_STATUS: u8 = 0x5f;
const STATUS_LEN: usize = 6;

/// Status byte 1.
const ORIENTATION: u8 = 1 << 1;
const USB2_CONN: u8 = 1 << 4;
const USB3_CONN: u8 = 1 << 5;
const DATA_ROLE: u8 = 1 << 7;
/// Status byte 2.
const DP_CONN: u8 = 1 << 0;
const DEBUG_ACC: u8 = 1 << 4;
const HPD_IRQ: u8 = 1 << 6;
const HPD_STATE: u8 = 1 << 7;
/// Status byte 3.
const PWR_SOURCE: u8 = 1 << 0;
const PWR_SINK: u8 = 1 << 1;

/// The default `poll_ms`.
const POLL_MS: i32 = 1000;

/// One status as read: the length byte, then five bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status(pub [u8; STATUS_LEN]);

impl Status {
    /// The partner's mode: none, USB, DP, USB+DP, or a debug accessory.
    #[must_use]
    pub fn mode(&self) -> &'static str {
        let st = &self.0;
        if st[2] & DEBUG_ACC != 0 {
            "debug accessory"
        } else if st[1] & USB3_CONN != 0 && st[2] & DP_CONN != 0 {
            "USB+DP"
        } else if st[2] & DP_CONN != 0 {
            "DP"
        } else if st[1] & (USB3_CONN | USB2_CONN) != 0 {
            "USB"
        } else {
            "none"
        }
    }

    /// The plug's orientation, where the SuperSpeed lanes are in use.
    #[must_use]
    pub fn orientation(&self) -> &'static str {
        let st = &self.0;
        if st[1] & USB3_CONN == 0 && st[2] & DP_CONN == 0 {
            "none"
        } else if st[1] & ORIENTATION != 0 {
            "reverse"
        } else {
            "normal"
        }
    }

    /// The USB data role.
    #[must_use]
    pub fn data_role(&self) -> &'static str {
        let st = &self.0;
        if st[1] & (USB3_CONN | USB2_CONN) == 0 {
            "none"
        } else if st[1] & DATA_ROLE != 0 {
            "device"
        } else {
            "host"
        }
    }

    /// Whether the port sources or sinks VBUS.
    #[must_use]
    pub fn power_role(&self) -> &'static str {
        match self.0[3] & (PWR_SOURCE | PWR_SINK) {
            PWR_SOURCE => "source",
            PWR_SINK => "sink",
            _ => "none",
        }
    }

    /// DisplayPort's hot-plug level.
    #[must_use]
    pub fn hpd(&self) -> &'static str {
        let st = &self.0;
        if st[2] & DP_CONN == 0 {
            "none"
        } else if st[2] & HPD_STATE != 0 {
            "high"
        } else {
            "low"
        }
    }

    /// Bytes 1 to 5 in hex.
    #[must_use]
    pub fn raw(&self) -> String {
        let st = &self.0;
        alloc::format!(
            "{:02x} {:02x} {:02x} {:02x} {:02x}",
            st[1],
            st[2],
            st[3],
            st[4],
            st[5]
        )
    }

    /// The line logged when it changes.
    #[must_use]
    pub fn line(&self) -> String {
        let hpd = if self.0[2] & DP_CONN == 0 {
            ""
        } else if self.0[2] & HPD_STATE != 0 {
            ", HPD high"
        } else {
            ", HPD low"
        };
        alloc::format!(
            "{}, {}, data {}, power {}{hpd}",
            self.mode(),
            self.orientation(),
            self.data_role(),
            self.power_role()
        )
    }
}

/// What a poll last saw.
#[derive(Debug, Default)]
struct Seen {
    /// A status has been read.
    valid: bool,
    /// The last read failed (logged once until one succeeds).
    read_failed: bool,
    status: Status,
}

/// One controller's state, shared by its poll thread and its sysctls.
pub struct Softc {
    name: String,
    dev: I2cDevice,
    log: fn(core::fmt::Arguments<'_>),
    poll_ms: AtomicI32,
    seen: SpinLock<Seen>,
    detaching: AtomicBool,
    wake: WaitQueue,
}

impl core::fmt::Debug for Softc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Softc")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Softc {
    /// The controller `name` behind `dev`, logging through `log`.
    #[must_use]
    pub fn new(name: String, dev: I2cDevice, log: fn(core::fmt::Arguments<'_>)) -> Self {
        Self {
            name,
            dev,
            log,
            poll_ms: AtomicI32::new(POLL_MS),
            seen: SpinLock::new(Seen::default()),
            detaching: AtomicBool::new(false),
            wake: WaitQueue::new(),
        }
    }

    /// `rts5453_ack`: acknowledges the interrupt, and the HPD IRQ if
    /// `hpd`. A failure is not reported, as the sibling's is not.
    fn ack(&self, s: &Sleepable, hpd: bool) {
        let out = [DATA_CTRL, 2, INT_ACK, if hpd { HPD_IRQ_ACK } else { 0 }];
        let _ = self.dev.write(s, &out);
    }

    /// `rts5453_poll` without its rescheduling: reads the status, and logs
    /// and acknowledges a change.
    pub fn poll(&self, s: &Sleepable) {
        let mut st = [0u8; STATUS_LEN];
        let r = self.dev.write_read(s, &[DATA_STATUS], &mut st);
        let (changed, ack) = {
            let mut seen = self.seen.lock();
            match r {
                Err(e) => {
                    if !seen.read_failed {
                        (self.log)(format_args!(
                            "{}: cannot read its status: {}",
                            self.name,
                            iicbus::acpi_space::status_of(e)
                        ));
                        seen.read_failed = true;
                    }
                    (None, None)
                }
                Ok(()) => {
                    seen.read_failed = false;
                    let new = Status(st);
                    if seen.valid && new.0[1..] == seen.status.0[1..] {
                        (None, None)
                    } else {
                        let ack = seen.valid.then_some(new.0[2] & HPD_IRQ != 0);
                        seen.status = new;
                        seen.valid = true;
                        (Some(new), ack)
                    }
                }
            }
        };
        if let Some(new) = changed {
            (self.log)(format_args!("{}: {}", self.name, new.line()));
        }
        if let Some(hpd) = ack {
            self.ack(s, hpd);
        }
    }

    /// The last status, if one has been read.
    #[must_use]
    pub fn status(&self) -> Option<Status> {
        let seen = self.seen.lock();
        seen.valid.then_some(seen.status)
    }

    /// A field of the last status for its sysctl, "unknown" before one.
    fn field(&self, f: fn(&Status) -> String) -> String {
        self.status()
            .map_or_else(|| String::from("unknown"), |st| f(&st))
    }

    /// The poll thread: a poll at once, then one every `poll_ms` (at
    /// least a millisecond) until detach.
    fn run(&self, s: &Sleepable) {
        while !self.detaching.load(Ordering::Acquire) {
            self.poll(s);
            let ms = u64::try_from(self.poll_ms.load(Ordering::Relaxed).max(1)).unwrap_or(1);
            let _ = bus::manager().wait_timeout(
                s,
                &self.wake,
                &mut || self.detaching.load(Ordering::Acquire),
                ms * 1_000_000,
            );
        }
    }
}

/// A sysctl over the last status: its name, description, and value.
type Field = (&'static str, &'static str, fn(&Status) -> String);

/// An `rts5453N` instance.
#[derive(Debug)]
pub struct Rts5453 {
    sc: Arc<Softc>,
    unit: u32,
    thread: SpinLock<Option<Arc<sched::Thread>>>,
}

fn log(args: core::fmt::Arguments<'_>) {
    bus::manager().log(args);
}

impl Driver for Rts5453 {
    const NAME: &'static str = "rts5453";
    const IDS: Ids = Ids::IicAcpi(&[AcpiId::new("CIXH200D")]);

    fn describe(_: &Ident) -> String {
        "Realtek RTS5453 USB Type-C PD controller".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let dev = cx.i2c(0)?;
        Ok(Self {
            sc: Arc::new(Softc::new(
                alloc::format!("{}{}", Self::NAME, cx.unit()),
                dev,
                log,
            )),
            unit: cx.unit(),
            thread: SpinLock::new(None),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let fields: [Field; 6] = [
            ("mode", "Partner's mode: none, USB, DP, USB+DP", |s| {
                s.mode().into()
            }),
            ("orientation", "Plug orientation", |s| {
                s.orientation().into()
            }),
            ("data_role", "Data role: host, device or none", |s| {
                s.data_role().into()
            }),
            ("power_role", "Power role: source, sink or none", |s| {
                s.power_role().into()
            }),
            ("hpd", "DisplayPort hot plug: high, low or none", |s| {
                s.hpd().into()
            }),
            ("status", "Raw status bytes", Status::raw),
        ];
        let sc = Arc::clone(&self.sc);
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            let Some(node) = bus::device_node(t, Self::NAME, self.unit) else {
                return;
            };
            for (name, desc, f) in fields {
                let sc = Arc::clone(&sc);
                if let Ok(id) = t.add_value(
                    node,
                    Number::Auto,
                    name,
                    desc,
                    Value::Str(Box::new(move |out| out.push_str(&sc.field(f)))),
                ) {
                    t.set_flags(id, ctlflag::MPSAFE);
                }
            }
            let (rd, wr) = (Arc::clone(&sc), Arc::clone(&sc));
            let _ = t.add_writable(
                node,
                Number::Auto,
                "poll_ms",
                "Polling interval (ms)",
                0,
                Value::Int(Box::new(move || rd.poll_ms.load(Ordering::Relaxed))),
                sysctl::int_writer(move |v| {
                    wr.poll_ms.store(v, Ordering::Relaxed);
                    Ok(())
                }),
            );
        });
        let sc = Arc::clone(&self.sc);
        let th = sched::spawn(&self.sc.name, sched::Priority::KERNEL, move |s| sc.run(s))
            .map_err(|_| AttachError::Device("cannot start its poll thread"))?;
        *self.thread.lock() = Some(th);
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.sc.detaching.store(true, Ordering::Release);
        self.sc.wake.wake_all();
        let th = self.thread.lock().take();
        if let Some(th) = th {
            th.join(cx.sleepable());
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Rts5453>();

#[cfg(test)]
mod tests;
