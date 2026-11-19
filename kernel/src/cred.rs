// SPDX-License-Identifier: BSD-2-Clause
//! Process credentials (FreeBSD's `struct ucred`): the `security` crate's
//! [`Cred`], which holds the ids, the groups, the jail, and the privilege
//! policies. A process shares its credential by reference and replaces it
//! whole when one changes, so a call in progress keeps the one it started
//! with.
//!
//! Behavior reference: sys/kern/kern_prot.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

pub(crate) use security::{Cred as Ucred, Ids, NGROUPS_MAX, Priv, SetCred};

/// The superuser in prison0, made once.
static ROOT: sync::Once<Arc<Ucred>> = sync::Once::new();

/// The superuser in prison0, which kernel threads run as: one credential
/// for the life of the kernel, borrowed with no count.
pub(crate) fn root() -> &'static Arc<Ucred> {
    loop {
        if let Some(r) = ROOT.get() {
            return r;
        }
        // proc0's: one supplementary group, 0 (`proc0_init`'s
        // `cr_ngroups = 1`), which init and what the kernel starts
        // inherit. Losing the race to set it means another CPU's is on
        // its way.
        let mut c = Ucred::root();
        c.groups = alloc::vec![0];
        if ROOT.set(Arc::new(c)).is_err() {
            core::hint::spin_loop();
        }
    }
}

/// The superuser in prison0, shared.
pub(crate) fn root_shared() -> Arc<Ucred> {
    Arc::clone(root())
}

/// Supplementary groups from user memory, for `setgroups`.
pub(crate) fn groups_from(raw: &[u8]) -> Vec<u32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|&b| u32::from_ne_bytes(b))
        .collect()
}
