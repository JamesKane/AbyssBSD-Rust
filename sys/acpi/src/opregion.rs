// SPDX-License-Identifier: BSD-2-Clause
//! Address-space handlers: the drivers that serve operation regions in
//! the spaces only they can reach (`GenericSerialBus` through an I2C
//! controller, `GeneralPurposeIo` through a GPIO controller).
//!
//! A bus installs its handler on its controller's namespace node
//! ([`Service::install_handler`]), as FreeBSD's `acpi_iicbus` and
//! `acpi_gpiobus` call `AcpiInstallAddressSpaceHandler` on the
//! controller's handle. An access to a region in that space at or below
//! the node, with no nearer handler, reaches the handler (the interpreter
//! names the node, [`aml::BufferTransfer::handler`]); one with no handler
//! fails, as ACPICA's `AE_NOT_EXIST` does. Installing runs the `_REG(space,
//! 1)` of each device declaring such a region, so firmware that waits for
//! `_REG` before touching it (ACPI 6.5 §6.5.4) starts using it; removing
//! runs `_REG(space, 0)` first.
//!
//! Handlers run on the AML thread, with its sleepable context: an I2C
//! transfer may sleep on its controller's interrupt.
//!
//! Behavior reference: sys/contrib/dev/acpica/components/events/evhandler.c,
//! evregion.c (handler search and `_REG`), sys/dev/iicbus/acpi_iicbus.c,
//! sys/dev/gpio/acpi_gpiobus.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use aml::{BufferTransfer, GpioAccess, HostError, NodeId, RegionSpace};
use sched::Sleepable;
use sync::SpinLock;

use crate::service::Service;

/// A driver's handler for one address space.
pub trait RegionHandler: Send + Sync {
    /// A buffer-protocol field access (`GenericSerialBus`): returns the
    /// buffer the field operation yields, status and length first.
    ///
    /// # Errors
    ///
    /// [`HostError::Unsupported`] by default; the AML operation fails.
    fn transfer(&self, s: &Sleepable, req: &BufferTransfer<'_>) -> Result<Vec<u8>, HostError> {
        let _ = (s, req);
        Err(HostError::Unsupported)
    }

    /// Reads GPIO pins: bit `i` is the connection's pin `access.pin + i`.
    ///
    /// # Errors
    ///
    /// [`HostError::Unsupported`] by default.
    fn gpio_read(&self, s: &Sleepable, access: &GpioAccess<'_>) -> Result<u64, HostError> {
        let _ = (s, access);
        Err(HostError::Unsupported)
    }

    /// Writes GPIO pins, bit `i` of `value` to pin `access.pin + i`.
    ///
    /// # Errors
    ///
    /// [`HostError::Unsupported`] by default.
    fn gpio_write(
        &self,
        s: &Sleepable,
        access: &GpioAccess<'_>,
        value: u64,
    ) -> Result<(), HostError> {
        let _ = (s, access, value);
        Err(HostError::Unsupported)
    }
}

type Entry = (NodeId, RegionSpace, Arc<dyn RegionHandler>);

/// The installed handlers, by node and space.
pub(crate) struct Handlers(SpinLock<Vec<Entry>>);

impl Handlers {
    pub(crate) const fn new() -> Self {
        Self(SpinLock::new(Vec::new()))
    }

    /// The handler on `node` for `space`.
    pub(crate) fn get(
        &self,
        node: Option<NodeId>,
        space: RegionSpace,
    ) -> Result<Arc<dyn RegionHandler>, HostError> {
        let node = node.ok_or(HostError::Unsupported)?;
        self.0
            .lock()
            .iter()
            .find(|(n, sp, _)| *n == node && *sp == space)
            .map(|(_, _, h)| Arc::clone(h))
            .ok_or(HostError::Unsupported)
    }

    fn add(&self, node: NodeId, space: RegionSpace, h: Arc<dyn RegionHandler>) -> bool {
        let mut v = self.0.lock();
        if v.iter().any(|(n, sp, _)| *n == node && *sp == space) {
            return false;
        }
        v.push((node, space, h));
        true
    }

    fn remove(&self, node: NodeId, space: RegionSpace) {
        self.0
            .lock()
            .retain(|(n, sp, _)| !(*n == node && *sp == space));
    }
}

/// What installing or removing a handler ran.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RegOutcome {
    /// `_REG` methods that ran.
    pub ran: usize,
    /// Those that failed, by their device's path.
    pub failed: Vec<(alloc::string::String, aml::Error)>,
}

/// Why a handler was not installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// One is already installed there for that space
    /// (`AE_ALREADY_EXISTS`).
    Exists,
}

impl Service {
    /// Installs `handler` for `space` on `node` (FreeBSD's
    /// `AcpiInstallAddressSpaceHandler`) and runs the `_REG(space, 1)`
    /// methods it makes connected. Must not be called from the AML
    /// thread.
    ///
    /// # Errors
    ///
    /// [`InstallError::Exists`] if `node` has a handler for `space`.
    pub fn install_handler(
        &self,
        s: &Sleepable,
        node: NodeId,
        space: RegionSpace,
        handler: Arc<dyn RegionHandler>,
    ) -> Result<RegOutcome, InstallError> {
        // In the table first: `_REG`'s AML may use the region at once.
        if !self.handlers().add(node, space, handler) {
            return Err(InstallError::Exists);
        }
        let (ran, failed) = self.run(s, move |interp, host| {
            let (ran, failed) = interp.install_handler(node, space, host);
            (ran, paths(interp, failed))
        });
        Ok(RegOutcome { ran, failed })
    }

    /// Removes the handler for `space` on `node` after running the
    /// `_REG(space, 0)` methods of the regions it served (FreeBSD's
    /// `AcpiRemoveAddressSpaceHandler`). Must not be called from the AML
    /// thread.
    pub fn remove_handler(&self, s: &Sleepable, node: NodeId, space: RegionSpace) -> RegOutcome {
        let (ran, failed) = self.run(s, move |interp, host| {
            let (ran, failed) = interp.remove_handler(node, space, host);
            (ran, paths(interp, failed))
        });
        self.handlers().remove(node, space);
        RegOutcome { ran, failed }
    }
}

fn paths(
    interp: &aml::Interpreter,
    failed: Vec<(NodeId, aml::Error)>,
) -> Vec<(alloc::string::String, aml::Error)> {
    failed
        .into_iter()
        .map(|(n, e)| (interp.path(n).to_text(), e))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;
    impl RegionHandler for Nothing {}

    #[test]
    fn handlers_are_found_by_node_and_space() {
        let i = aml::Interpreter::new(aml::Config::default());
        let (root, sb) = (i.root(), i.child(i.root(), "_SB").unwrap());
        let h = Handlers::new();
        let gsb = RegionSpace::GenericSerialBus;
        assert!(h.add(sb, gsb, Arc::new(Nothing)));
        assert!(!h.add(sb, gsb, Arc::new(Nothing)), "one per node and space");
        assert!(h.get(Some(sb), gsb).is_ok());
        assert!(h.get(Some(sb), RegionSpace::GeneralPurposeIo).is_err());
        assert!(h.get(Some(root), gsb).is_err());
        assert!(h.get(None, gsb).is_err());
        // The default methods refuse.
        // SAFETY: host tests run on threads that may block.
        let s = unsafe { Sleepable::assume() };
        let conn = [0x79, 0];
        let a = GpioAccess {
            region: sb,
            connection: &conn,
            pin: 0,
            count: 1,
            handler: Some(sb),
        };
        assert_eq!(Nothing.gpio_read(&s, &a), Err(HostError::Unsupported));
        h.remove(sb, gsb);
        assert!(h.get(Some(sb), gsb).is_err());
    }
}
