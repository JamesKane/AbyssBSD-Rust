// SPDX-License-Identifier: BSD-2-Clause
//! Packet buffers (docs/NETWORKING.md §4): FreeBSD's mbufs, as owned Rust
//! types.
//!
//! An [`Mbuf`] is one `MSIZE` buffer holding up to `MLEN` bytes inline, or
//! a cluster from one of the zones, or caller-owned pages (`M_EXTPG`). A
//! [`Chain`] links them (`m_next`), counting its bytes and what a socket
//! buffer charges for it. A [`Packet`] is a chain with FreeBSD's packet
//! header. Clusters are reference-counted, so a copy by reference shares
//! them (`m_copym`); shared data is read-only.
//!
//! Buffers come from [`zone`]s with FreeBSD's limits (`kern.ipc.nmbufs`,
//! `kern.ipc.nmbclusters`, ...). Every operation that allocates returns
//! [`NoBufs`] at a limit; nothing panics on a packet's contents.
//!
//! Behavior reference: sys/sys/mbuf.h, sys/kern/kern_mbuf.c,
//! sys/kern/uipc_mbuf.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod chain;
mod mbuf;
mod packet;
#[cfg(test)]
mod tests;
pub mod zone;

pub use chain::{Chain, Segments, carve_cost, carve_next};
pub use mbuf::{Cluster, Mbuf, Pages};
pub use packet::{Header, Packet, PktHdr, Pullup, csum, flags, hash};

/// An mbuf's size, and the header before its inline data (`MHSIZE`).
pub const MSIZE: u32 = 256;
pub const MHSIZE: u32 = 32;
/// Inline data in an mbuf, and in the first of a packet, which also holds
/// the 64-byte packet header on FreeBSD.
pub const MLEN: u32 = MSIZE - MHSIZE;
pub const MHLEN: u32 = MLEN - 64;
/// The least data put in a cluster rather than inline.
pub const MINCLSIZE: u32 = MHLEN + 1;
/// Cluster sizes.
pub const MCLBYTES: u32 = 2048;
pub const MJUMPAGESIZE: u32 = 4096;
pub const MJUM9BYTES: u32 = 9 * 1024;
pub const MJUM16BYTES: u32 = 16 * 1024;
/// Room kept before a packet's data for its headers: the largest link
/// header (`max_linkhdr`) and protocol headers (`max_protohdr`, IPv6 and
/// TCP).
pub const MAX_LINKHDR: u32 = 16;
pub const MAX_PROTOHDR: u32 = 60;
pub const MAX_HDR: u32 = MAX_LINKHDR + MAX_PROTOHDR;

/// No buffer could be had (`ENOBUFS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoBufs;

impl From<NoBufs> for abi::errno::Errno {
    fn from(_: NoBufs) -> Self {
        Self::ENOBUFS
    }
}
