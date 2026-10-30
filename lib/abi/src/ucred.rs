// SPDX-License-Identifier: BSD-2-Clause
//! Credentials (`sys/ucred.h`): `setcred(2)`'s argument.

/// `struct setcred`: what `setcred` sets, as its flags select.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Setcred {
    pub sc_uid: u32,
    pub sc_ruid: u32,
    pub sc_svuid: u32,
    pub sc_gid: u32,
    pub sc_rgid: u32,
    pub sc_svgid: u32,
    pub sc_pad: u32,
    pub sc_supp_groups_nb: u32,
    /// The supplementary groups and the MAC label (user addresses).
    pub sc_supp_groups: u64,
    pub sc_label: u64,
}
