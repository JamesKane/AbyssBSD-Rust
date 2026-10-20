// SPDX-License-Identifier: BSD-2-Clause
//! UNIX-domain sockets, the first part: `socket(AF_UNIX, ...)` and
//! `bind`, which makes the socket's node in the file system (FreeBSD's
//! `unp_bind`). Nothing listens yet, so a `connect` to a bound name is
//! refused; listening, accepting, and moving data come with the rest of
//! the sockets in Phase 6.
//!
//! Behavior reference: sys/kern/uipc_usrreq.c, sys/kern/sys_socket.c
//! (`soo_stat`).

use abi::stat::{S_IFSOCK, Stat};
use sync::SpinLock;
use vfs::VnodeUse;

/// A UNIX-domain socket (of any type: until they carry data, they
/// differ in nothing).
pub(crate) struct UnixSocket {
    /// The node `bind` made, held while the socket is open.
    bound: SpinLock<Option<VnodeUse>>,
}

impl UnixSocket {
    pub(crate) const fn new() -> Self {
        Self {
            bound: SpinLock::new(None),
        }
    }

    /// Whether `bind` has given it a name.
    pub(crate) fn is_bound(&self) -> bool {
        self.bound.lock().is_some()
    }

    /// Records the node `bind` made.
    pub(crate) fn set_bound(&self, node: VnodeUse) {
        *self.bound.lock() = Some(node);
    }

    /// `fstat`: a socket, nothing more (FreeBSD's `soo_stat`).
    pub(crate) fn stat(&self) -> Stat {
        Stat {
            st_mode: S_IFSOCK,
            ..Stat::default()
        }
    }
}
