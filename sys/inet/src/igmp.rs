// SPDX-License-Identifier: BSD-2-Clause
//! IGMP, as far as group joins need: a group's first join on an
//! interface and its last leave are reported to the network's routers,
//! IGMPv3 state-change reports by default and IGMPv2 reports and leaves
//! with `net.inet.igmp.default_version=2`, with TTL 1 and the Router
//! Alert option. A received message goes to the raw sockets that want it
//! (a multicast router's); queries are not answered, and reports are not
//! repeated (robustness), which a later phase brings.
//!
//! Behavior reference: sys/netinet/igmp.c (`igmp_input`,
//! `igmp_initial_join`, `igmp_final_leave`, `igmp_v1v2_queue_report`,
//! `igmp_v3_encap_report`, `igmp_intr`, `igmp_isgroupreported`),
//! sys/netinet/in_mcast.c (`in_joingroup`, `in_leavegroup`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::net::iff;
use mbuf::Packet;
use net::Vnet;

use crate::cksum;
use crate::ip::{self, HDR_LEN, Ip};
use crate::state::{self, on};

/// `IGMP_v2_HOST_MEMBERSHIP_REPORT`, `IGMP_HOST_LEAVE_MESSAGE`,
/// `IGMP_v3_HOST_MEMBERSHIP_REPORT`.
const V2_REPORT: u8 = 0x16;
const V2_LEAVE: u8 = 0x17;
const V3_REPORT: u8 = 0x22;
/// `CHANGE_TO_INCLUDE_MODE`, `CHANGE_TO_EXCLUDE_MODE`.
const TO_IN: u8 = 3;
const TO_EX: u8 = 4;
/// `INADDR_ALLHOSTS_GROUP`, `INADDR_ALLRTRS_GROUP`,
/// `INADDR_ALLRPTS_GROUP`.
const ALLHOSTS: [u8; 4] = [224, 0, 0, 1];
const ALLRTRS: [u8; 4] = [224, 0, 0, 2];
const ALLRPTS: [u8; 4] = [224, 0, 0, 22];
/// `IPTOS_PREC_INTERNETCONTROL`.
const TOS_INTERNETCONTROL: u8 = 0xc0;

/// A received IGMP message (`igmp_input`), to the raw sockets.
pub(crate) fn input(vnet: &Arc<Vnet>, m: Packet, ip: &Ip) {
    crate::raw::input4(vnet, m, ip, true);
}

/// `ETHER_MAP_IP_MULTICAST`: the link-layer group IPv4 group `g` maps to.
#[must_use]
pub const fn ether_group(g: [u8; 4]) -> [u8; 6] {
    [1, 0, 0x5e, g[1] & 0x7f, g[2], g[3]]
}

/// `igmp_isgroupreported`: never the all-hosts group; the link-local
/// ones only with `net.inet.igmp.sendlocal`.
fn reported(st: &state::State, g: [u8; 4]) -> bool {
    g != ALLHOSTS && (on(&st.conf.igmp_sendlocal) || g[..3] != [224, 0, 0])
}

/// A socket joined `group` on interface `ifindex` (`in_joingroup`): the
/// interface's first member reports it.
pub(crate) fn join(vnet: &Arc<Vnet>, ifindex: u16, group: [u8; 4]) {
    let st = state::of(vnet);
    let first = {
        let mut g = st.groups.lock();
        match g.iter_mut().find(|(i, a, _)| *i == ifindex && *a == group) {
            Some(e) => {
                e.2 += 1;
                false
            }
            None => {
                g.push((ifindex, group, 1));
                true
            }
        }
    };
    if first && reported(&st, group) {
        report(vnet, &st, ifindex, group, true);
    }
}

/// A socket left it (`in_leavegroup`): the last member's leave is
/// reported.
pub(crate) fn leave(vnet: &Arc<Vnet>, ifindex: u16, group: [u8; 4]) {
    let st = state::of(vnet);
    let last = {
        let mut g = st.groups.lock();
        let Some(at) = g.iter().position(|(i, a, _)| *i == ifindex && *a == group) else {
            return;
        };
        g[at].2 -= 1;
        if g[at].2 == 0 {
            g.remove(at);
            true
        } else {
            false
        }
    };
    if last && reported(&st, group) {
        report(vnet, &st, ifindex, group, false);
    }
}

/// The report of a join (or leave) of `group` on `ifindex`, in the
/// interface's IGMP version, out of it (`igmp_intr`'s `ip_output` with
/// TTL 1, no loopback, and the Router Alert option). A loopback
/// interface is silent (`IGIF_SILENT`).
fn report(vnet: &Arc<Vnet>, st: &state::State, ifindex: u16, group: [u8; 4], join: bool) {
    let silent = vnet
        .net
        .lock()
        .by_index(ifindex)
        .is_none_or(|i| i.flags & iff::LOOPBACK != 0);
    if silent {
        return;
    }
    let v3 = st.conf.igmp_version.load(Ordering::Relaxed) >= 3;
    let (msg, dst, tos, off): (Vec<u8>, [u8; 4], u8, u16) = if v3 {
        // `struct igmp_report` and one `igmp_grouprec` with no sources.
        let mut b = alloc::vec![V3_REPORT, 0, 0, 0, 0, 0, 0, 1];
        b.extend_from_slice(&[if join { TO_EX } else { TO_IN }, 0, 0, 0]);
        b.extend_from_slice(&group);
        (b, ALLRPTS, TOS_INTERNETCONTROL, ip::DF)
    } else if join {
        let mut b = alloc::vec![V2_REPORT, 0, 0, 0];
        b.extend_from_slice(&group);
        (b, group, 0, 0)
    } else {
        let mut b = alloc::vec![V2_LEAVE, 0, 0, 0];
        b.extend_from_slice(&group);
        (b, ALLRTRS, 0, 0)
    };
    let mut msg = msg;
    let c = cksum::of(&msg);
    msg[2..4].copy_from_slice(&c.to_be_bytes());
    let Ok(mut m) = Packet::for_output(msg.len()) else {
        return;
    };
    if m.data.extend_from_slice(&msg).is_err() {
        return;
    }
    let Ok(len) = u16::try_from(HDR_LEN + msg.len()) else {
        return;
    };
    let mut h = Ip::new(ip::proto::IGMP, [0; 4], dst, len, 1, tos);
    h.off = off;
    let Ok(b) = m.prepend(HDR_LEN) else {
        return;
    };
    b.copy_from_slice(&h.bytes());
    let o = ip::Opts {
        flags: 0,
        mcast: Some(ip::Mopts {
            ifindex,
            ttl: 1,
            loop_: false,
        }),
        fib: 0,
        pcp: None,
        router_alert: on(&st.conf.igmp_sendra),
        flowid: 0,
    };
    let _ = ip::output(vnet, m, &o);
}
