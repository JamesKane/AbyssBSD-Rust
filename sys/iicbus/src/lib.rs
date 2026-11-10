// SPDX-License-Identifier: BSD-2-Clause
//! `iicbus` and `iic`: the bus under an I2C controller, and `/dev/iicN`
//! on it for `i2c(8)`.
//!
//! - A controller's driver adds `iicbus` once it has published its
//!   provider (`StartCx::provide_i2c`, then `StartCx::add_iicbus`), as a
//!   FreeBSD controller's attach adds its `iicbus` child.
//! - When firmware describes the controller, the bus is FreeBSD's
//!   `acpi_iicbus` ("ACPI-hinted"): its children are the devices whose
//!   first `I2cSerialBus` names the controller ([`acpi::iicbus`]). `acpi0`
//!   already added each as its own child; the bus takes over those that
//!   have not attached (`StartCx::take_child`), which FreeBSD does by
//!   deleting `acpi0`'s child and adding its own. They keep their ACPI
//!   bus data, lines, and dependencies, and are matched again as
//!   `iicbus` children (`Ids::IicAcpi`), at `addr=0x32 handle=...`.
//! - The bus runs at the slowest such device's speed, or 100 kHz
//!   (`dev.iicbus.N.frequency`).
//! - Then `iic` is added at address 0, as its identify routine does after
//!   the bus's enumeration, and makes `/dev/iicN` (`root:wheel`, 0600).
//!
//! The kernel installs its user-memory access ([`set_user_memory`])
//! before devices attach; `iic`'s requests are in [`iic`].
//!
//! Behavior reference: sys/dev/iicbus/iicbus.c, acpi_iicbus.c, iic.c.

#![no_std]
// The tests make a `Sleepable` for the host; the crate itself has no
// unsafe code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;

pub mod iic;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype};
use bus::i2c::{I2cSource, IicbusOf};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IicIdent, StartCx, StopCx, StopReason,
};
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use sync::Once;
use vfs::{CharDevice, Uio};

use crate::iic::{OpenState, UserMemory};

static USER: Once<&'static dyn UserMemory> = Once::new();

/// Installs the kernel's access to user memory, once, before devices
/// attach.
pub fn set_user_memory(m: &'static dyn UserMemory) {
    let _ = USER.set(m);
}

/// `iicbusN`.
#[derive(Debug)]
pub struct Iicbus {
    source: Arc<I2cSource>,
    acpi: Option<String>,
    unit: u32,
    /// `dev.iicbus.N.frequency`.
    frequency: Arc<AtomicU32>,
}

impl Driver for Iicbus {
    const NAME: &'static str = "iicbus";
    const IDS: Ids = Ids::Nexus(&["iicbus"]);

    fn describe(_: &Ident) -> String {
        "Philips I2C bus".into()
    }

    fn unit(_: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> Option<u32> {
        bus_data?.downcast_ref::<IicbusOf>().map(|o| o.unit)
    }

    fn describe_with(ident: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        let hinted = bus_data
            .and_then(|d| d.downcast_ref::<IicbusOf>())
            .is_some_and(|o| o.acpi.is_some());
        if hinted {
            "Philips I2C bus (ACPI-hinted)".into()
        } else {
            Self::describe(ident)
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let source = cx.parent_i2c()?;
        let acpi = cx.bus_data::<IicbusOf>().and_then(|o| o.acpi.clone());
        Ok(Self {
            source,
            acpi,
            unit: cx.unit(),
            frequency: Arc::new(AtomicU32::new(acpi::iicbus::DEFAULT_FREQUENCY)),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        if let Some(path) = &self.acpi {
            self.adopt_acpi_children(cx, path);
        }
        let freq = Arc::clone(&self.frequency);
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            let Some(node) = bus::device_node(t, Self::NAME, self.unit) else {
                return;
            };
            let f = Arc::clone(&freq);
            let _ = t.add_query(
                node,
                sysctl::Number::Auto,
                "frequency",
                "Bus frequency in Hz",
                ctltype::UINT | ctlflag::RW | ctlflag::TUN | ctlflag::MPSAFE,
                "IU",
                Arc::new(move |written, out| {
                    if let Some(w) = written {
                        let b: [u8; 4] = w.try_into().map_err(|_| Errno::EINVAL)?;
                        f.store(u32::from_ne_bytes(b), Ordering::Relaxed);
                    }
                    out.extend_from_slice(&f.load(Ordering::Relaxed).to_ne_bytes());
                    Ok(())
                }),
            );
        });
        // `iic`, after the bus's own children (its identify routine).
        cx.add_child(
            Ident::Iic(IicIdent {
                addr: 0,
                acpi: None,
            }),
            Some(Arc::new(IicOf {
                source: Arc::clone(&self.source),
                unit: self.unit,
            }) as Arc<dyn Any + Send + Sync>),
            Vec::new(),
        );
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

impl Iicbus {
    /// `acpi_iicbus_enumerate_children`: the devices on the controller at
    /// `path`, taken from `acpi0`, and the bus clock they allow.
    fn adopt_acpi_children(&self, cx: &mut StartCx<'_, Self>, path: &str) {
        let Some(svc) = acpi::service() else { return };
        let Ok(parsed) = aml::AmlPath::parse(path) else {
            return;
        };
        let found = svc.run(cx.sleepable(), move |interp, host| {
            interp
                .lookup(&parsed)
                .map(|ctl| acpi::iicbus::children(interp, host, ctl))
                .unwrap_or_default()
        });
        self.frequency
            .store(acpi::iicbus::bus_frequency(&found), Ordering::Relaxed);
        for c in &found {
            if bus::bootverbose() {
                svc.ops().log(format_args!(
                    "iicbus{}: found ACPI child {} at {:#x}, {} Hz, {}-bit",
                    self.unit,
                    c.path,
                    c.address,
                    c.speed,
                    if c.ten_bit { 10 } else { 7 }
                ));
            }
            let addr = c.address;
            let make = move |old: &Ident| match old {
                Ident::Acpi(i, p) => Some(Ident::Iic(IicIdent {
                    addr,
                    acpi: Some((i.clone(), p.clone())),
                })),
                _ => None,
            };
            if cx.take_child(&c.path, "acpi", &make).is_none() && bus::bootverbose() {
                svc.ops().log(format_args!(
                    "iicbus{}: {} stays on acpi0 (attached or absent)",
                    self.unit, c.path
                ));
            }
        }
    }
}

/// What `iicN` and its device node share.
#[derive(Debug)]
struct Softc {
    source: Arc<I2cSource>,
    /// Detached: the node's opens fail from now on.
    gone: AtomicBool,
}

/// What `iicbus` gives its `iic`: the controller, and the bus's unit,
/// which `iic` takes (`iicN` is `iicbusN`'s, as FreeBSD's serial attach
/// numbers them).
struct IicOf {
    source: Arc<I2cSource>,
    unit: u32,
}

/// `iicN`.
#[derive(Debug)]
pub struct Iic {
    sc: Arc<Softc>,
    unit: u32,
}

impl Driver for Iic {
    const NAME: &'static str = "iic";
    const IDS: Ids = Ids::IicGeneric;

    fn describe(_: &Ident) -> String {
        "I2C generic I/O".into()
    }

    fn unit(_: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> Option<u32> {
        bus_data?.downcast_ref::<IicOf>().map(|o| o.unit)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let source = cx
            .bus_data::<IicOf>()
            .map(|o| Arc::clone(&o.source))
            .ok_or(AttachError::Decline)?;
        Ok(Self {
            sc: Arc::new(Softc {
                source,
                gone: AtomicBool::new(false),
            }),
            unit: cx.unit(),
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let node = Arc::new(Node(Arc::clone(&self.sc)));
        let path = format!("iic{}", self.unit);
        // UID_ROOT, GID_WHEEL, 0600, as `iic_attach` makes it.
        vfs::devices::make_dev(path.as_bytes(), 0o600, 0, 0, node)
            .map(|_| ())
            .ok_or(AttachError::Device("failed to create character device"))
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.sc.gone.store(true, Ordering::Release);
        let path = format!("iic{}", self.unit);
        vfs::devices::destroy_dev(cx.sleepable(), path.as_bytes());
    }
}

/// `/dev/iicN`.
struct Node(Arc<Softc>);

static OPEN: LockClass = LockClass::new("iic open", 250);

/// One open of `/dev/iicN` (`iic_cdevpriv`), its requests serialized.
struct Open {
    sc: Arc<Softc>,
    state: Mutex<OpenState>,
}

impl CharDevice for Node {
    fn instance(&self, _: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if self.0.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        Ok(Some(Arc::new(Open {
            sc: Arc::clone(&self.0),
            state: Mutex::new(&OPEN, OpenState::default()),
        })))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }
}

impl Open {
    fn live(&self) -> Result<(), Errno> {
        if self.sc.gone.load(Ordering::Acquire) {
            Err(Errno::ENXIO)
        } else {
            Ok(())
        }
    }

    fn uio(&self, s: &Sleepable, nonblock: bool) -> Result<usize, Errno> {
        self.live()?;
        let st = self.state.lock(s);
        iic::uio(&st, &self.sc.source, s, nonblock).map(|()| 0)
    }
}

impl CharDevice for Open {
    fn read(&self, s: &Sleepable, _: &mut dyn Uio, _: u64, nonblock: bool) -> Result<usize, Errno> {
        self.uio(s, nonblock)
    }

    fn write(
        &self,
        s: &Sleepable,
        _: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        self.uio(s, nonblock)
    }

    fn ioctl_file(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &mut [u8],
        fflag: u32,
    ) -> Result<usize, Errno> {
        self.live()?;
        let mem = USER.get().ok_or(Errno::ENXIO)?;
        let nonblock = fflag & abi::fcntl::O_NONBLOCK != 0;
        let mut st = self.state.lock(s);
        iic::ioctl(&mut st, &self.sc.source, s, *mem, req, arg, nonblock).map(|()| 0)
    }
}

/// The bus driver, for the build's driver table.
pub const IICBUS: DriverDecl = DriverDecl::of::<Iicbus>();
/// `iic`, likewise.
pub const IIC: DriverDecl = DriverDecl::of::<Iic>();
