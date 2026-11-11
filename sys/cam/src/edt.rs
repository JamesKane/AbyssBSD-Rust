// SPDX-License-Identifier: BSD-2-Clause
//! The existing device table: CAM's buses, their targets and LUNs, and
//! the peripherals on each, as drivers register them.
//!
//! - A bus is a SIM's: its name and unit (`ahcich0`, `nvme0`,
//!   `umass-sim0`), its path ID (the `scbusN` number), and the [`Sim`]
//!   that answers for it. The transport layer's own bus (`xpt0`, path ID
//!   `CAM_XPT_PATH_ID`) is always there, last.
//! - A device is a target and LUN on a bus, with what the probe learned
//!   (inquiry or identify data, the serial number, NVMe's identify
//!   pages) and its peripherals: the disk driver's (`da0`, `ada0`,
//!   `nda0`) and `pass`'s, in FreeBSD's order.
//! - A SIM that keeps a wildcard path (`ahcich`, `nvme`) has a wildcard
//!   device (target -1, LUN 0xffffffff), unconfigured, as on FreeBSD.
//! - Each list has a generation, bumped as it changes, which a paged
//!   `XPT_DEV_MATCH` checks.
//!
//! Behavior reference: sys/cam/cam_xpt.c (`xpt_bus_register`,
//! `xpt_alloc_device`, `xpt_add_periph`), sys/cam/cam_periph.c
//! (`camperiphnextunit`).

use alloc::boxed::Box;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::cam::{CAM_LUN_WILDCARD, CAM_TARGET_WILDCARD, CAM_XPT_PATH_ID, proto};
use sched::Sleepable;
use sync::SpinLock;

use crate::ccb::{PathInq, TranSettings};
use crate::io::{AtaIo, NvmeIo, ScsiIo};

/// What a SIM does for its bus: the transport-specific requests a pass
/// device sends it. Each I/O runs to completion before it returns.
pub trait Sim: Send + Sync {
    /// `XPT_PATH_INQ` for a path to `lun`.
    fn path_inq(&self, lun: u64) -> PathInq;

    /// `XPT_GET_TRAN_SETTINGS` for `target` (current settings, or the
    /// user's).
    fn trans_settings(&self, s: &Sleepable, target: u32, user: bool) -> TranSettings;

    /// `XPT_SCSI_IO`: the CAM status.
    fn scsi_io(&self, s: &Sleepable, target: u32, lun: u64, io: &mut ScsiIo) -> u32 {
        let _ = (s, target, lun, io);
        abi::cam::status::REQ_INVALID
    }

    /// `XPT_ATA_IO`: the CAM status.
    fn ata_io(&self, s: &Sleepable, target: u32, io: &mut AtaIo) -> u32 {
        let _ = (s, target, io);
        abi::cam::status::REQ_INVALID
    }

    /// `XPT_NVME_IO` (`admin` false) and `XPT_NVME_ADMIN`: the CAM status.
    fn nvme_io(&self, s: &Sleepable, lun: u64, admin: bool, io: &mut NvmeIo) -> u32 {
        let _ = (s, lun, admin, io);
        abi::cam::status::REQ_INVALID
    }

    /// `XPT_RESET_DEV`: the CAM status.
    fn reset_dev(&self, s: &Sleepable) -> u32 {
        let _ = s;
        abi::cam::status::REQ_CMP
    }
}

/// A SIM's identity on its bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimName {
    /// `sim_name`: `ahcich`, `nvme`, `umass-sim`.
    pub name: &'static str,
    /// `unit_number`.
    pub unit: u32,
    /// `bus_id`: the bus on the SIM (0 for all of these).
    pub bus_id: u32,
}

/// What a probe learned of a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Its target.
    pub target: u32,
    /// Its LUN (an NVMe namespace's ID).
    pub lun: u64,
    /// `PROTO_*`.
    pub protocol: u32,
    /// Its protocol's version.
    pub protocol_version: u32,
    /// `XPORT_*`.
    pub transport: u32,
    /// Its transport's version.
    pub transport_version: u32,
    /// `struct scsi_inquiry_data` (zeros for ATA and NVMe).
    pub inq_data: Box<[u8; 256]>,
    /// `struct ata_params`, as CAM keeps it (`ata_param_fixup`), or
    /// zeros.
    pub ident_data: Box<[u8; 512]>,
    /// `inq_flags` (`SID_CmdQue` for a disk that queues).
    pub inq_flags: u8,
    /// Command openings (tags), for `XPT_GDEV_STATS`.
    pub openings: u32,
    /// The serial number, as CAM keeps it.
    pub serial: Vec<u8>,
    /// The device identification VPD page (`CDAI_TYPE_SCSI_DEVID`).
    pub devid: Vec<u8>,
    /// NVMe's identify controller and namespace data.
    pub nvme: Option<Box<NvmeData>>,
    /// The disk driver's peripheral: `da`, `ada`, `nda`, and its unit.
    pub periph: (&'static str, u32),
}

/// An NVMe namespace's identify pages, for `XPT_DEV_ADVINFO`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NvmeData {
    /// Identify controller.
    pub cdata: [u8; 4096],
    /// Identify namespace.
    pub nsdata: [u8; 4096],
}

impl DeviceInfo {
    /// A device with no probe data yet.
    #[must_use]
    pub fn new(target: u32, lun: u64, protocol: u32, periph: (&'static str, u32)) -> Self {
        Self {
            target,
            lun,
            protocol,
            protocol_version: abi::cam::VERSION_UNSPECIFIED,
            transport: abi::cam::xport::UNSPECIFIED,
            transport_version: abi::cam::VERSION_UNSPECIFIED,
            inq_data: Box::new([0; 256]),
            ident_data: Box::new([0; 512]),
            inq_flags: 0,
            openings: 1,
            serial: Vec::new(),
            devid: Vec::new(),
            nvme: None,
            periph,
        }
    }
}

/// A peripheral: its driver's name and its unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Periph {
    pub name: &'static str,
    pub unit: u32,
}

/// A device in the table.
#[derive(Debug)]
pub(crate) struct DeviceNode {
    pub(crate) lun: u64,
    /// Which device this is, of all made since boot.
    pub(crate) id: u64,
    pub(crate) info: Option<DeviceInfo>,
    pub(crate) unconfigured: bool,
    pub(crate) generation: u32,
    pub(crate) periphs: Vec<Periph>,
}

/// A target on a bus.
#[derive(Debug)]
pub(crate) struct TargetNode {
    pub(crate) id: u32,
    pub(crate) generation: u32,
    /// By LUN.
    pub(crate) devices: Vec<DeviceNode>,
}

/// A bus.
pub(crate) struct BusNode {
    pub(crate) path_id: u32,
    pub(crate) sim: SimName,
    pub(crate) generation: u32,
    /// By target ID.
    pub(crate) targets: Vec<TargetNode>,
    pub(crate) ops: Option<Arc<dyn Sim>>,
}

impl core::fmt::Debug for BusNode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BusNode")
            .field("path_id", &self.path_id)
            .field("sim", &self.sim)
            .finish_non_exhaustive()
    }
}

/// The table.
#[derive(Debug)]
pub(crate) struct Edt {
    pub(crate) bus_generation: u32,
    /// By path ID, so the transport layer's bus is last.
    pub(crate) buses: Vec<BusNode>,
    pub(crate) next_id: u64,
}

impl Edt {
    /// A table with only the transport layer's bus and its `xpt0`.
    pub(crate) fn new() -> Self {
        let mut e = Self {
            bus_generation: 0,
            buses: Vec::new(),
            next_id: 1,
        };
        e.add_bus(
            CAM_XPT_PATH_ID,
            SimName {
                name: "xpt",
                unit: 0,
                bus_id: 0,
            },
            None,
        );
        let id = e.add_device(CAM_XPT_PATH_ID, CAM_TARGET_WILDCARD, CAM_LUN_WILDCARD, None);
        if let Some(d) = e.device_mut(id) {
            d.periphs.push(Periph {
                name: "xpt",
                unit: 0,
            });
            d.generation += 1;
        }
        e
    }

    pub(crate) fn add_bus(&mut self, path_id: u32, sim: SimName, ops: Option<Arc<dyn Sim>>) {
        let at = self
            .buses
            .iter()
            .position(|b| b.path_id > path_id)
            .unwrap_or(self.buses.len());
        self.buses.insert(
            at,
            BusNode {
                path_id,
                sim,
                generation: 0,
                targets: Vec::new(),
                ops,
            },
        );
        self.bus_generation = self.bus_generation.wrapping_add(1);
    }

    pub(crate) fn remove_bus(&mut self, path_id: u32) {
        self.buses.retain(|b| b.path_id != path_id);
        self.bus_generation = self.bus_generation.wrapping_add(1);
    }

    pub(crate) fn bus(&self, path_id: u32) -> Option<&BusNode> {
        self.buses.iter().find(|b| b.path_id == path_id)
    }

    /// Adds a device (unconfigured until it has info); returns its ID.
    pub(crate) fn add_device(
        &mut self,
        path_id: u32,
        target: u32,
        lun: u64,
        info: Option<DeviceInfo>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let Some(bus) = self.buses.iter_mut().find(|b| b.path_id == path_id) else {
            return id;
        };
        let ti = match bus.targets.iter().position(|t| t.id >= target) {
            Some(i) if bus.targets[i].id == target => i,
            at => {
                let i = at.unwrap_or(bus.targets.len());
                bus.targets.insert(
                    i,
                    TargetNode {
                        id: target,
                        generation: 0,
                        devices: Vec::new(),
                    },
                );
                bus.generation = bus.generation.wrapping_add(1);
                i
            }
        };
        let t = &mut bus.targets[ti];
        let at = t
            .devices
            .iter()
            .position(|d| d.lun > lun)
            .unwrap_or(t.devices.len());
        // A probe adds and removes its own peripheral first: two changes.
        let configured = info.is_some();
        t.devices.insert(
            at,
            DeviceNode {
                lun,
                id,
                unconfigured: !configured,
                info,
                generation: if configured { 2 } else { 0 },
                periphs: Vec::new(),
            },
        );
        t.generation = t.generation.wrapping_add(1);
        id
    }

    pub(crate) fn remove_device(&mut self, id: u64) {
        for bus in &mut self.buses {
            for t in &mut bus.targets {
                let n = t.devices.len();
                t.devices.retain(|d| d.id != id);
                if t.devices.len() != n {
                    t.generation = t.generation.wrapping_add(1);
                }
            }
            let n = bus.targets.len();
            bus.targets.retain(|t| !t.devices.is_empty());
            if bus.targets.len() != n {
                bus.generation = bus.generation.wrapping_add(1);
            }
        }
    }

    pub(crate) fn device_mut(&mut self, id: u64) -> Option<&mut DeviceNode> {
        self.buses
            .iter_mut()
            .flat_map(|b| b.targets.iter_mut())
            .flat_map(|t| t.devices.iter_mut())
            .find(|d| d.id == id)
    }

    /// The device `id`, with its bus and target.
    pub(crate) fn device(&self, id: u64) -> Option<(&BusNode, u32, &DeviceNode)> {
        for b in &self.buses {
            for t in &b.targets {
                if let Some(d) = t.devices.iter().find(|d| d.id == id) {
                    return Some((b, t.id, d));
                }
            }
        }
        None
    }

    /// The device at a path.
    pub(crate) fn device_at(&self, path_id: u32, target: u32, lun: u64) -> Option<&DeviceNode> {
        self.bus(path_id)?
            .targets
            .iter()
            .find(|t| t.id == target)?
            .devices
            .iter()
            .find(|d| d.lun == lun)
    }

    /// The lowest unit of peripheral driver `name` not in use.
    pub(crate) fn next_unit(&self, name: &str) -> u32 {
        let used = |u: u32| {
            self.periphs()
                .any(|(_, _, _, p)| p.name == name && p.unit == u)
        };
        (0..).find(|&u| !used(u)).unwrap_or(0)
    }

    /// Every peripheral, with its bus, target, and device.
    pub(crate) fn periphs(&self) -> impl Iterator<Item = (&BusNode, u32, &DeviceNode, &Periph)> {
        self.buses.iter().flat_map(|b| {
            b.targets.iter().flat_map(move |t| {
                t.devices
                    .iter()
                    .flat_map(move |d| d.periphs.iter().map(move |p| (b, t.id, d, p)))
            })
        })
    }
}

static EDT: SpinLock<Option<Edt>> = SpinLock::new(None);

/// Runs `f` on the table.
pub(crate) fn with<R>(f: impl FnOnce(&mut Edt) -> R) -> R {
    let mut g = EDT.lock();
    f(g.get_or_insert_with(Edt::new))
}

/// A registered bus. [`Bus::deregister`] takes it away, with its
/// devices.
#[derive(Debug)]
pub struct Bus {
    path_id: u32,
    wildcard: Option<u64>,
}

impl Bus {
    /// Registers a bus for `sim` (`xpt_bus_register`): it takes the
    /// lowest free `scbus` number. With `wildcard`, the SIM keeps a
    /// wildcard path, which shows as an unconfigured device.
    pub fn register(sim: SimName, ops: Arc<dyn Sim>, wildcard: bool) -> Self {
        let path_id = storage::alloc_bus();
        with(|e| {
            e.add_bus(path_id, sim, Some(ops));
            let wildcard = wildcard
                .then(|| e.add_device(path_id, CAM_TARGET_WILDCARD, CAM_LUN_WILDCARD, None));
            Self { path_id, wildcard }
        })
    }

    /// Its path ID: the `scbusN` number.
    #[must_use]
    pub fn path_id(&self) -> u32 {
        self.path_id
    }

    /// Adds a probed device, with its disk peripheral and a `pass`
    /// peripheral (`/dev/passN`, `root:operator` 0600). SCSI devices list
    /// the disk first (`da0,pass1`); ATA and NVMe devices list `pass`
    /// first (`pass0,ada0`), as FreeBSD's peripheral drivers attach.
    pub fn add_device(&self, info: DeviceInfo) -> Device {
        let path_id = self.path_id;
        let (target, lun, periph, protocol) = (info.target, info.lun, info.periph, info.protocol);
        let (id, pass) = with(|e| {
            // Unconfigured devices at the same path give way.
            let stale: Vec<u64> = e
                .device_at(path_id, target, lun)
                .map(|d| d.id)
                .into_iter()
                .collect();
            for id in stale {
                e.remove_device(id);
            }
            let id = e.add_device(path_id, target, lun, Some(info));
            let pass = e.next_unit("pass");
            if let Some(d) = e.device_mut(id) {
                let disk = Periph {
                    name: periph.0,
                    unit: periph.1,
                };
                let pass = Periph {
                    name: "pass",
                    unit: pass,
                };
                d.periphs = if protocol == proto::SCSI {
                    alloc::vec![disk, pass]
                } else {
                    alloc::vec![pass, disk]
                };
                d.generation += 2;
            }
            (id, pass)
        });
        let path = format!("pass{pass}");
        let node = Arc::new(crate::xpt::PassDev { id });
        let _ = vfs::devices::make_dev(path.as_bytes(), 0o600, 0, crate::GID_OPERATOR, node);
        Device { id, pass }
    }

    /// Takes the bus away (`xpt_bus_deregister`), and every device left
    /// on it.
    pub fn deregister(self, s: &Sleepable) {
        let path_id = self.path_id;
        let passes: Vec<u32> = with(|e| {
            let units = e
                .periphs()
                .filter(|(b, _, _, p)| b.path_id == path_id && p.name == "pass")
                .map(|(_, _, _, p)| p.unit)
                .collect();
            e.remove_bus(path_id);
            units
        });
        for u in passes {
            vfs::devices::destroy_dev(s, format!("pass{u}").as_bytes());
        }
        let _ = self.wildcard;
        storage::free_bus(path_id);
    }
}

/// A device a driver added. [`Device::remove`] takes it away.
#[derive(Debug)]
pub struct Device {
    id: u64,
    pass: u32,
}

impl Device {
    /// Its `pass` unit.
    #[must_use]
    pub fn pass_unit(&self) -> u32 {
        self.pass
    }

    /// Takes it away (`AC_LOST_DEVICE`): its pass node goes; an open of
    /// it fails what it is asked from then on with `ENXIO`.
    pub fn remove(self, s: &Sleepable) {
        with(|e| e.remove_device(self.id));
        vfs::devices::destroy_dev(s, format!("pass{}", self.pass).as_bytes());
    }
}
