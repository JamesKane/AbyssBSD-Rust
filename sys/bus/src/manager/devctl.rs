// SPDX-License-Identifier: BSD-2-Clause
//! devctl's side of the manager: the event queue `/dev/devctl` serves
//! `devd(8)`, and the operations behind `devctl(8)`'s requests on
//! `/dev/devctl2`.
//!
//! - Events queue whether or not anyone reads them, so `devd` started
//!   late still sees the boot's, up to `hw.bus.devctl_queue` lines (the
//!   oldest go first); setting it to 0 turns the queue off for good and
//!   drops what it held. A device's nomatch event goes out once.
//! - Each request answers with the error FreeBSD's `devctl2_ioctl` gives,
//!   in its order. What the driver model has no operation for (suspend,
//!   resume, rescan, reset) is answered as FreeBSD's bus methods answer
//!   for a bus without one.
//!
//! Behavior reference: sys/kern/kern_devctl.c (the queue,
//! `sysctl_devctl_queue`), sys/kern/subr_bus.c (`devctl2_ioctl`,
//! `find_device`, `device_get_path`), sys/kern/subr_kobj.c
//! (`kobj_error_method`), sys/dev/pci/pci.c (`pci_lookup`),
//! sys/dev/acpica/acpi.c (`acpi_lookup`).

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use abi::errno::Errno;
use sched::Sleepable;

use super::{Manager, Topology};
use crate::types::{AttachError, DetachReason, DeviceId, Ident, Ids, NodeState};

/// `hw.bus.devctl_queue`'s default (`DEVCTL_DEFAULT_QUEUE_LEN`).
pub const DEFAULT_QUEUE: usize = 1000;

/// The queued event lines and the knobs that govern them.
pub(super) struct Events {
    lines: VecDeque<String>,
    /// Lines kept at most; 0 once the queue is off.
    limit: usize,
    /// `hw.bus.devctl_nomatch_enabled`.
    nomatch: bool,
    /// `DEV_FREEZE` is in force.
    frozen: bool,
}

impl Events {
    pub(super) fn new(limit: usize, nomatch: bool) -> Self {
        Self {
            lines: VecDeque::new(),
            limit,
            nomatch,
            frozen: false,
        }
    }
}

/// Whether drivers with match table `ids` attach to the bus `ident` is
/// on (FreeBSD's `driver_exists` on the parent's devclass).
fn same_bus(ids: &Ids, ident: &Ident) -> bool {
    matches!(
        (ids, ident),
        (
            Ids::Nexus(_),
            Ident::Nexus(_) | Ident::Channel { .. } | Ident::Child { .. }
        ) | (Ids::PciBus, Ident::PciBus { .. })
            | (Ids::Acpi(_), Ident::Acpi(..))
            | (Ids::Pci(_), Ident::Pci(..))
            | (Ids::Virtio(_), Ident::Virtio(_))
            | (Ids::Usb(_), Ident::Usb(..))
            | (Ids::Hid(_), Ident::Hid(..))
            | (Ids::Test(_), Ident::Test(_))
    )
}

/// An ACPI path with each segment padded to four characters with `_`,
/// as `AcpiGetHandle` reads `\_SB.PCI0`.
fn acpi_normal(path: &str) -> String {
    let mut out = String::new();
    let (root, rest) = path.split_at(path.len().min(1));
    out.push_str(root);
    for (i, seg) in rest.split('.').enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(seg);
        for _ in seg.len()..4 {
            out.push('_');
        }
    }
    out
}

/// The error a failed attach reports (`device_attach`'s, which drivers
/// give as `ENXIO` mostly).
fn attach_errno(e: &AttachError) -> Errno {
    match e {
        AttachError::NoMemory => Errno::ENOMEM,
        _ => Errno::ENXIO,
    }
}

impl Manager {
    /// Queues an event line (`devctl_queue`), unless the queue is off or
    /// the line is longer than FreeBSD's buffer holds; the oldest line
    /// goes when the queue is full.
    pub(super) fn event(&self, line: String) {
        if line.len() > devtree::devctl::LINE_MAX {
            return;
        }
        {
            let mut e = self.events.lock();
            if e.limit == 0 {
                return;
            }
            while e.lines.len() >= e.limit {
                e.lines.pop_front();
            }
            e.lines.push_back(line);
        }
        if let Some(f) = self.on_event.get() {
            f();
        }
    }

    /// `id`'s nomatch event, the first time it has no driver
    /// (`device_handle_nomatch`, `DF_DONENOMATCH`). Topology lock held.
    pub(super) fn nomatch(&self, t: &mut Topology, id: DeviceId) {
        let Some(n) = t.node_mut(id) else {
            return;
        };
        if core::mem::replace(&mut n.nomatch_sent, true) {
            return;
        }
        if self.events.lock().nomatch {
            let line = self.device_line(t, '?', "", id.index);
            self.event(line);
        }
    }

    /// Has `f` called after each event is queued (once; later calls are
    /// ignored). It runs with the topology lock held, maybe, so it only
    /// wakes.
    pub fn set_event_hook(&self, f: fn()) {
        let _ = self.on_event.set(f);
    }

    /// Queues a notification (`devctl_notify`): `!system=… subsystem=…
    /// type=… data`.
    pub fn notify(&self, system: &str, subsystem: &str, kind: &str, data: Option<&str>) {
        let mut line = String::new();
        let _ = devtree::devctl::notify(&mut line, system, subsystem, kind, data);
        self.event(line);
    }

    /// The oldest queued event, taken (`devread`).
    pub fn take_event(&self) -> Option<String> {
        self.events.lock().lines.pop_front()
    }

    /// The devctl events so far (oldest first), consuming them.
    pub fn take_events(&self) -> Vec<String> {
        self.events.lock().lines.drain(..).collect()
    }

    /// Events queued (`devsoftc.queued`).
    #[must_use]
    pub fn events_queued(&self) -> usize {
        self.events.lock().lines.len()
    }

    /// `hw.bus.devctl_queue`.
    #[must_use]
    pub fn devctl_queue(&self) -> usize {
        self.events.lock().limit
    }

    /// Sets `hw.bus.devctl_queue` (`sysctl_devctl_queue`): only turning
    /// the queue off is allowed once the system is up, which drops what
    /// is queued.
    ///
    /// # Errors
    ///
    /// `EINVAL` for anything but 0.
    pub fn set_devctl_queue(&self, q: i32) -> Result<(), Errno> {
        if q != 0 {
            return Err(Errno::EINVAL);
        }
        let mut e = self.events.lock();
        e.limit = 0;
        e.lines.clear();
        Ok(())
    }

    /// `hw.bus.devctl_nomatch_enabled`.
    #[must_use]
    pub fn devctl_nomatch(&self) -> bool {
        self.events.lock().nomatch
    }

    /// Sets `hw.bus.devctl_nomatch_enabled`.
    pub fn set_devctl_nomatch(&self, on: bool) {
        self.events.lock().nomatch = on;
    }

    /// The device `name` names (`find_device`): an exact name with unit,
    /// else a bus's locator, a PCI selector (`pci0:0:3:0`) or an absolute
    /// ACPI path (`\_SB_.PCI0`).
    #[must_use]
    pub fn lookup(&self, name: &str, s: &Sleepable) -> Option<DeviceId> {
        let t = self.topo.read(s);
        let all = || (0..u32::try_from(t.nodes.len()).unwrap_or(u32::MAX)).filter_map(|i| t.id(i));
        if let Some(id) =
            all().find(|id| !name.is_empty() && t.nameunit(self.drivers, id.index) == name)
        {
            return Some(id);
        }
        if let Some(a) = devtree::devctl::pci_selector(name) {
            return all().find(|&id| matches!(t.node(id), Some(n) if matches!(&n.ident, Ident::Pci(_, at, _) if *at == a)));
        }
        if name.starts_with('\\') {
            let want = acpi_normal(name);
            return all().find(|&id| {
                t.node(id)
                    .and_then(|n| n.ident.acpi_path())
                    .is_some_and(|p| acpi_normal(p) == want)
            });
        }
        None
    }

    /// Probes and attaches `id` and waits for the outcome
    /// (`device_probe_and_attach`): a device already given a driver, or
    /// disabled, is left as it is.
    fn probe_and_attach_now(&self, id: DeviceId, s: &Sleepable) -> Result<(), Errno> {
        if self.is_disabled(id, s) || self.probe_and_attach(id, s).is_err() {
            return Ok(());
        }
        self.wait_settled(id, s);
        match self.state(id, s) {
            Some(NodeState::Attached | NodeState::Waiting(_)) => Ok(()),
            Some(NodeState::Failed(e)) => Err(attach_errno(&e)),
            _ => Err(Errno::ENXIO),
        }
    }

    /// What `id` is now: (state, class, unit, kernel-owned or root).
    fn devctl_view(
        &self,
        id: DeviceId,
        s: &Sleepable,
    ) -> Option<(NodeState, Option<&'static str>, Option<u32>, bool)> {
        let t = self.topo.read(s);
        let n = t.node(id)?;
        Some((
            n.state.clone(),
            n.class(self.drivers),
            n.unit,
            n.adopted.is_some() || n.parent.is_none(),
        ))
    }

    /// Detaches `id` for a devctl request: a kernel-owned device has no
    /// detach method (`kobj_error_method`'s `ENXIO`), and one whose driver
    /// refuses is busy.
    fn devctl_detach_as(
        &self,
        id: DeviceId,
        why: DetachReason,
        s: &Sleepable,
    ) -> Result<(), Errno> {
        match self.devctl_view(id, s) {
            Some((_, _, _, true)) => Err(Errno::ENXIO),
            Some(_) => self.detach(id, why, s).map_err(|e| match e {
                super::DetachError::Busy => Errno::EBUSY,
                _ => Errno::ENXIO,
            }),
            None => Err(Errno::ENOENT),
        }
    }

    /// `DEV_ATTACH`.
    ///
    /// # Errors
    ///
    /// `EBUSY` if attached, `ENXIO` if disabled or no driver attaches.
    pub fn devctl_attach(&self, id: DeviceId, s: &Sleepable) -> Result<(), Errno> {
        match self.state(id, s) {
            None => Err(Errno::ENOENT),
            Some(NodeState::Attached | NodeState::Detaching(_)) => Err(Errno::EBUSY),
            Some(_) if self.is_disabled(id, s) => Err(Errno::ENXIO),
            Some(_) => self.probe_and_attach_now(id, s),
        }
    }

    /// `DEV_DETACH`; `force` skips asking the driver (`device_quiesce`).
    ///
    /// # Errors
    ///
    /// `ENXIO` if not attached, `EBUSY` if the driver refuses.
    pub fn devctl_detach(&self, id: DeviceId, force: bool, s: &Sleepable) -> Result<(), Errno> {
        if !matches!(self.state(id, s), Some(NodeState::Attached)) {
            return Err(Errno::ENXIO);
        }
        let why = if force {
            DetachReason::Forced
        } else {
            DetachReason::Requested
        };
        self.devctl_detach_as(id, why, s)
    }

    /// `DEV_ENABLE`: the device may attach again, and is offered to every
    /// driver.
    ///
    /// # Errors
    ///
    /// `EBUSY` if it is enabled; what its attach reports.
    pub fn devctl_enable(&self, id: DeviceId, s: &Sleepable) -> Result<(), Errno> {
        {
            let mut t = self.topo.write(s);
            let Some(n) = t.node_mut(id) else {
                return Err(Errno::ENOENT);
            };
            let Some(class) = n.disabled.take() else {
                return Err(Errno::EBUSY);
            };
            // Out of its devclass, so drivers rebid (unless fixed).
            if let Some(u) = n.unit.take() {
                t.units.release(class, u);
            }
        }
        self.bump();
        self.probe_and_attach_now(id, s)
    }

    /// `DEV_DISABLE`: detaches it, keeping its name, and keeps it from
    /// attaching.
    ///
    /// # Errors
    ///
    /// `ENXIO` if it is disabled; `EBUSY` while it attaches or detaches,
    /// or if the driver refuses.
    pub fn devctl_disable(&self, id: DeviceId, force: bool, s: &Sleepable) -> Result<(), Errno> {
        let Some((state, class, unit, _)) = self.devctl_view(id, s) else {
            return Err(Errno::ENOENT);
        };
        if self.is_disabled(id, s) {
            return Err(Errno::ENXIO);
        }
        match state {
            NodeState::Attaching | NodeState::Detaching(_) => return Err(Errno::EBUSY),
            NodeState::Attached => {
                let why = if force {
                    DetachReason::Forced
                } else {
                    DetachReason::Requested
                };
                self.devctl_detach_as(id, why, s)?;
            }
            _ => {}
        }
        let mut t = self.topo.write(s);
        let class = class.unwrap_or("");
        // The name stays (FreeBSD forces `DF_FIXEDCLASS` around the
        // detach): the unit is taken again if detach gave it back.
        let unit = match (state, unit) {
            (NodeState::Attached, Some(u)) => t.units.alloc(class, Some(u)).ok(),
            (_, u) => u,
        };
        if let Some(n) = t.node_mut(id) {
            n.disabled = Some(class);
            n.unit = unit;
            n.driver = None;
            n.state = NodeState::NotPresent;
        }
        drop(t);
        self.bump();
        Ok(())
    }

    /// `DEV_SUSPEND`: the driver model has no suspend, which is answered
    /// as a bus method that is not there answers.
    ///
    /// # Errors
    ///
    /// `EINVAL` for the root, else `ENXIO`.
    pub fn devctl_suspend(&self, id: DeviceId, s: &Sleepable) -> Result<(), Errno> {
        match self.devctl_view(id, s) {
            None => Err(Errno::ENOENT),
            Some(_) if id == self.root() => Err(Errno::EINVAL),
            Some(_) => Err(Errno::ENXIO),
        }
    }

    /// `DEV_RESUME`: nothing is ever suspended.
    ///
    /// # Errors
    ///
    /// `EINVAL`.
    pub fn devctl_resume(&self, id: DeviceId, s: &Sleepable) -> Result<(), Errno> {
        match self.devctl_view(id, s) {
            None => Err(Errno::ENOENT),
            Some(_) => Err(Errno::EINVAL),
        }
    }

    /// `DEV_SET_DRIVER`: only `driver` may attach to it from now on;
    /// `detach` detaches the driver it has.
    ///
    /// # Errors
    ///
    /// `EINVAL` for an empty name or the root, `ENOENT` if no driver of
    /// that name serves its bus, `EBUSY` if attached and not `detach`;
    /// what its attach reports.
    pub fn devctl_set_driver(
        &self,
        id: DeviceId,
        driver: &str,
        detach: bool,
        s: &Sleepable,
    ) -> Result<(), Errno> {
        if driver.is_empty() {
            return Err(Errno::EINVAL);
        }
        let d = {
            let t = self.topo.read(s);
            let n = t.node(id).ok_or(Errno::ENOENT)?;
            if n.class(self.drivers) == Some(driver) {
                return Ok(());
            }
            if n.parent.is_none() {
                return Err(Errno::EINVAL);
            }
            self.drivers
                .iter()
                .position(|d| d.name == driver && same_bus(&d.ids, &n.ident))
                .ok_or(Errno::ENOENT)?
        };
        self.devctl_unbind(id, detach, s)?;
        if let Some(n) = self.topo.write(s).node_mut(id) {
            n.fixed = Some(d);
        }
        self.probe_and_attach_now(id, s)
    }

    /// `DEV_CLEAR_DRIVER`: any driver may attach to it again; `detach`
    /// detaches the one it has. No driver attaching is not an error.
    ///
    /// # Errors
    ///
    /// `EBUSY` if attached and not `detach`; what its attach reports.
    pub fn devctl_clear_driver(
        &self,
        id: DeviceId,
        detach: bool,
        s: &Sleepable,
    ) -> Result<(), Errno> {
        match self.topo.read(s).node(id) {
            None => return Err(Errno::ENOENT),
            Some(n) if n.fixed.is_none() => return Ok(()),
            Some(_) => {}
        }
        self.devctl_unbind(id, detach, s)?;
        if let Some(n) = self.topo.write(s).node_mut(id) {
            n.fixed = None;
        }
        match self.probe_and_attach_now(id, s) {
            Err(Errno::ENXIO) => Ok(()),
            r => r,
        }
    }

    /// Takes `id`'s driver away for a new choice: detached if `detach`
    /// (else `EBUSY`), or its chosen-but-waiting driver dropped. Its unit
    /// goes back to its devclass, and drivers that declined it may bid
    /// again.
    fn devctl_unbind(&self, id: DeviceId, detach: bool, s: &Sleepable) -> Result<(), Errno> {
        match self.devctl_view(id, s) {
            None => return Err(Errno::ENOENT),
            Some((NodeState::Attaching | NodeState::Detaching(_), ..)) => {
                return Err(Errno::EBUSY);
            }
            Some((NodeState::Attached, ..)) if !detach => return Err(Errno::EBUSY),
            Some((NodeState::Attached, ..)) => {
                self.devctl_detach_as(id, DetachReason::DriverChange, s)?;
            }
            Some(_) => {}
        }
        let mut t = self.topo.write(s);
        let Some(n) = t.node_mut(id) else {
            return Err(Errno::ENOENT);
        };
        let class = n.driver.take().map(|d| self.drivers[d].name);
        let unit = n.unit.take();
        n.declined.clear();
        if matches!(n.state, NodeState::Waiting(_) | NodeState::Failed(_)) {
            n.state = NodeState::NotPresent;
        }
        if let (Some(c), Some(u)) = (class, unit) {
            t.units.release(c, u);
        }
        Ok(())
    }

    /// `DEV_RESCAN`: no bus here rescans (`bus_null_rescan`).
    ///
    /// # Errors
    ///
    /// `ENXIO` if not attached, else `ENODEV`.
    pub fn devctl_rescan(&self, id: DeviceId, s: &Sleepable) -> Result<(), Errno> {
        match self.state(id, s) {
            Some(NodeState::Attached) => Err(Errno::ENODEV),
            _ => Err(Errno::ENXIO),
        }
    }

    /// `DEV_DELETE`: takes the device (detached first) out of the tree.
    /// Every bus here reports its children present (`root_child_present`),
    /// so only a forced delete goes ahead.
    ///
    /// # Errors
    ///
    /// `EINVAL` for the root, `EBUSY` without `force` or while it attaches
    /// or detaches, `ENXIO` for a device the kernel owns.
    pub fn devctl_delete(&self, id: DeviceId, force: bool, s: &Sleepable) -> Result<(), Errno> {
        let Some((state, ..)) = self.devctl_view(id, s) else {
            return Err(Errno::ENOENT);
        };
        if id == self.root() {
            return Err(Errno::EINVAL);
        }
        if !force {
            return Err(Errno::EBUSY);
        }
        match state {
            NodeState::Attaching | NodeState::Detaching(_) => return Err(Errno::EBUSY),
            NodeState::Attached => self.devctl_detach_as(id, DetachReason::Forced, s)?,
            _ => {}
        }
        self.delete(id, s);
        Ok(())
    }

    /// `DEV_FREEZE`: drivers added meanwhile would wait to probe (none
    /// are: every driver is built in).
    ///
    /// # Errors
    ///
    /// `EBUSY` if frozen already.
    pub fn devctl_freeze(&self) -> Result<(), Errno> {
        let mut e = self.events.lock();
        if core::mem::replace(&mut e.frozen, true) {
            return Err(Errno::EBUSY);
        }
        Ok(())
    }

    /// `DEV_THAW`.
    ///
    /// # Errors
    ///
    /// `EBUSY` if not frozen.
    pub fn devctl_thaw(&self) -> Result<(), Errno> {
        if !core::mem::replace(&mut self.events.lock().frozen, false) {
            return Err(Errno::EBUSY);
        }
        self.bump();
        Ok(())
    }

    /// `DEV_RESET`: no bus here resets a child (`BUS_RESET_CHILD` has no
    /// default).
    ///
    /// # Errors
    ///
    /// `EINVAL` for unknown flags or the root, else `ENXIO`.
    pub fn devctl_reset(&self, id: DeviceId, flags: i32, s: &Sleepable) -> Result<(), Errno> {
        if self.devctl_view(id, s).is_none() {
            return Err(Errno::ENOENT);
        }
        if flags & !abi::bus::DEVF_RESET_DETACH != 0 || id == self.root() {
            return Err(Errno::EINVAL);
        }
        Err(Errno::ENXIO)
    }

    /// `DEV_GET_PATH`'s path for `id` by `locator` (`device_get_path`):
    /// `/` for the root; `FreeBSD`'s is the names from the root down
    /// (`/nexus0/pci0/vtnet0`), `ACPI`'s the device's namespace path.
    ///
    /// # Errors
    ///
    /// `EIO` when the locator gives it no path.
    pub fn device_path(&self, id: DeviceId, locator: &str, s: &Sleepable) -> Result<String, Errno> {
        let t = self.topo.read(s);
        let n = t.node(id).ok_or(Errno::ENOENT)?;
        if n.parent.is_none() {
            return Ok(String::from("/"));
        }
        let path = match locator {
            abi::bus::BUS_LOCATOR_FREEBSD => {
                let mut names = Vec::new();
                let mut at = Some(id.index);
                while let Some(i) = at {
                    let Some(n) = t.nodes.get(i as usize).and_then(Option::as_ref) else {
                        break;
                    };
                    if n.parent.is_none() {
                        break;
                    }
                    names.push(t.nameunit(self.drivers, i));
                    at = n.parent;
                }
                names.iter().rev().fold(String::new(), |mut p, n| {
                    p.push('/');
                    p.push_str(n);
                    p
                })
            }
            abi::bus::BUS_LOCATOR_ACPI => String::from(n.ident.acpi_path().unwrap_or("")),
            _ => String::new(),
        };
        if path.len() <= 1 {
            return Err(Errno::EIO);
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::acpi_normal;

    #[test]
    fn acpi_paths_pad_their_segments() {
        assert_eq!(acpi_normal("\\_SB.PCI0"), "\\_SB_.PCI0");
        assert_eq!(acpi_normal("\\_SB_.PCI0.S1"), "\\_SB_.PCI0.S1__");
        assert_eq!(acpi_normal("\\"), "\\____");
    }
}
