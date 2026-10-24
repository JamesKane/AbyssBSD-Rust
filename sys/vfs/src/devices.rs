// SPDX-License-Identifier: BSD-2-Clause
//! Device nodes made outside the kernel crate (FreeBSD's `make_dev` and
//! `destroy_dev`): drivers and device classes name a node under `/dev`
//! and the [`CharDevice`] behind it, and devfs, which registers itself
//! here at boot, serves it.

use alloc::sync::Arc;

use sched::Sleepable;
use sync::Once;

use crate::CharDevice;

/// What devfs does for [`make_dev`] and [`destroy_dev`].
pub trait Registry: Send + Sync {
    /// Adds `ops` at `path`; returns its device number, or `None` if the
    /// name is taken.
    fn make(
        &self,
        path: &[u8],
        mode: u16,
        uid: u32,
        gid: u32,
        ops: Arc<dyn CharDevice>,
    ) -> Option<u64>;
    /// Adds a symbolic link at `path` to `target`; `false` if the name
    /// is taken.
    fn alias(&self, path: &[u8], target: &[u8]) -> bool;
    /// Takes the node at `path` away.
    fn destroy(&self, s: &Sleepable, path: &[u8]);
}

static REGISTRY: Once<&'static dyn Registry> = Once::new();

/// Installs devfs, once, before any device attaches.
pub fn set_registry(r: &'static dyn Registry) {
    let _ = REGISTRY.set(r);
}

/// Adds the device `ops` at `path` (under `/dev`), with permissions
/// `mode`, owned by `uid` and `gid`. Returns its device number, or
/// `None` if the name is taken or there is no devfs.
pub fn make_dev(
    path: &[u8],
    mode: u16,
    uid: u32,
    gid: u32,
    ops: Arc<dyn CharDevice>,
) -> Option<u64> {
    REGISTRY.get()?.make(path, mode, uid, gid, ops)
}

/// Adds a symbolic link at `path` to `target`, as FreeBSD's
/// `make_dev_alias` (`kbd0 -> kbdmux0`); `false` if the name is taken
/// or there is no devfs. [`destroy_dev`] takes it away.
pub fn make_dev_alias(path: &[u8], target: &[u8]) -> bool {
    REGISTRY.get().is_some_and(|r| r.alias(path, target))
}

/// Takes the node at `path` away; an open file keeps its device until it
/// closes, as FreeBSD's `destroy_dev` leaves threads already inside it.
pub fn destroy_dev(s: &Sleepable, path: &[u8]) {
    if let Some(r) = REGISTRY.get() {
        r.destroy(s, path);
    }
}
