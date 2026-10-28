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

/// The superuser in prison0, shared.
pub(crate) fn root_shared() -> Arc<Ucred> {
    Arc::new(Ucred::root())
}

/// Supplementary groups from user memory, for `setgroups`.
pub(crate) fn groups_from(raw: &[u8]) -> Vec<u32> {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|&b| u32::from_ne_bytes(b))
        .collect()
}
