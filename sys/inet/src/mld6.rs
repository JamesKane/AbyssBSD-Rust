// SPDX-License-Identifier: BSD-2-Clause
//! MLD (docs/NETWORKING.md §7): the IPv6 multicast groups each interface
//! has joined (`struct in6_multi`), with their link-layer memberships,
//! and the MLDv2 reports a host sends as it joins and leaves them and as
//! queries ask (RFC 3810), falling back to MLDv1 while an MLDv1 querier
//! is heard. Timers run in MLD's fast ticks, five a second.
//!
//! Behavior reference: sys/netinet6/mld6.c (`mld_initial_join`,
//! `mld_final_leave`, `mld_fasttimo_vnet`, `mld_v2_process_group_timers`,
//! `mld_v2_enqueue_group_record`, `mld_v2_encap_report`,
//! `mld_dispatch_packet`, `mld_v1_input_query`, `mld_v2_input_query`,
//! `mld_is_addr_reported`, `mld_set_version`), sys/netinet6/in6_mcast.c
//! (`in6_joingroup_locked`, `in6_leavegroup_locked`).

use alloc::vec::Vec;

use abi::net::iff;
use mbuf::Packet;
use net::Vnet;
use net::addr::{In6, is_unspecified};
use net::ifnet::{Net, icmp6_ifs, in6_iff};

use crate::icmp6::ty;
use crate::in6::{self, mc_scope, scope};
use crate::ip6::{self, Mopts6, Opts6, out};
use crate::state::{State, on};

/// `MLD_FASTHZ`: fast ticks a second; one tick in nanoseconds.
const FASTHZ: u64 = 5;
const TICK: u64 = 1_000_000_000 / FASTHZ;
/// `MLD_TIMER_SCALE`: queries' delays are in milliseconds.
const TIMER_SCALE: u32 = 1000;
/// `MLD_RV_INIT`, `MLD_QI_INIT`, `MLD_QRI_INIT` (seconds),
/// `MLD_URI_INIT`.
const RV_INIT: u32 = 2;
const QI_INIT: u32 = 125;
const QRI_INIT: u32 = 10;
const URI_INIT: u32 = 3;

/// A group record's type (`MLD_*_MODE`).
pub mod rec {
    pub const MODE_IS_INCLUDE: u8 = 1;
    pub const MODE_IS_EXCLUDE: u8 = 2;
    pub const CHANGE_TO_INCLUDE_MODE: u8 = 3;
    pub const CHANGE_TO_EXCLUDE_MODE: u8 = 4;
    pub const BLOCK_OLD_SOURCES: u8 = 6;
}

/// A group's state (`MLD_*_MEMBER`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Member {
    /// Not reported (loopback, all-nodes, interface-local).
    Silent,
    /// MLDv2: reporting; MLDv1: the next report is ours to send.
    Reporting,
    /// MLDv1: we reported last.
    Idle,
    /// MLDv2: a query for the group waits for its answer.
    GQueryPending,
    /// MLDv2: leaving, its last reports still to go.
    Leaving,
}

/// A group an interface joined (`struct in6_multi`).
#[derive(Clone, Copy, Debug)]
struct Group {
    ifindex: u16,
    addr: In6,
    refs: u32,
    state: Member,
    /// Fast ticks to a query's answer (`in6m_timer`), and to the next
    /// state-change report (`in6m_sctimer`) with those left to send
    /// (`in6m_scrv`).
    timer: u32,
    sctimer: u32,
    scrv: u32,
    /// The state-change record waiting to go.
    pending: u8,
}

/// An interface's MLD state (`struct mld_ifsoftc`).
#[derive(Clone, Copy, Debug)]
struct MldIf {
    ifindex: u16,
    version: u8,
    /// Robustness, query interval, query response interval (seconds),
    /// unsolicited report interval.
    rv: u32,
    qi: u32,
    qri: u32,
    uri: u32,
    /// Fast ticks to the answer of a general query (`mli_v2_timer`).
    v2_timer: u32,
    /// When MLDv1 compatibility ends, nanoseconds (`mli_v1_timer`).
    v1_until: u64,
}

/// A VNET's MLD state.
#[derive(Debug, Default)]
pub struct Mld {
    groups: Vec<Group>,
    ifs: Vec<MldIf>,
    /// Each address's memberships (`ia6_memberships`): its interface, the
    /// address, the group.
    memberships: Vec<(u16, In6, In6)>,
    /// When the next fast tick is due, 0 while no timer runs.
    tick_at: u64,
}

impl Mld {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            groups: Vec::new(),
            ifs: Vec::new(),
            memberships: Vec::new(),
            tick_at: 0,
        }
    }

    /// The next fast tick, `u64::MAX` for none.
    pub(crate) fn next_tick(&self) -> u64 {
        if self.tick_at == 0 {
            u64::MAX
        } else {
            self.tick_at
        }
    }

    fn mli(&mut self, ifindex: u16) -> &mut MldIf {
        if let Some(i) = self.ifs.iter().position(|m| m.ifindex == ifindex) {
            return &mut self.ifs[i];
        }
        self.ifs.push(MldIf {
            ifindex,
            version: 2,
            rv: RV_INIT,
            qi: QI_INIT,
            qri: QRI_INIT,
            uri: URI_INIT,
            v2_timer: 0,
            v1_until: 0,
        });
        let last = self.ifs.len() - 1;
        &mut self.ifs[last]
    }

    /// Starts the fast ticks if they are not running.
    fn run(&mut self) {
        if self.tick_at == 0 {
            self.tick_at = crate::now() + TICK;
            crate::nd6::arm(self.tick_at);
        }
    }
}

/// `mld_is_addr_reported`: interface-local groups and link-local
/// all-nodes are never reported.
#[must_use]
pub fn is_reported(a: &In6) -> bool {
    mc_scope(a) != scope::INTFACELOCAL && *a != in6::ALLNODES
}

/// `MLD_RANDOM_DELAY`: 1 to `x` ticks.
fn random_delay(x: u32) -> u32 {
    crate::random() % x.max(1) + 1
}

/// `ETHER_MAP_IPV6_MULTICAST`.
fn ether_group(g: &In6) -> [u8; 6] {
    [0x33, 0x33, g[12], g[13], g[14], g[15]]
}

/// Whether interface `ifindex` joined group `a` (`in6m_lookup`).
pub(crate) fn member(st: &State, ifindex: u16, a: &In6) -> bool {
    st.nd6
        .lock()
        .mld
        .groups
        .iter()
        .any(|g| g.ifindex == ifindex && g.addr == *a && g.refs > 0)
}

/// Records that address `addr` of `ifindex` holds a membership of
/// `group` (`ia6_memberships`).
pub(crate) fn add_membership(st: &State, ifindex: u16, addr: &In6, group: &In6) {
    st.nd6.lock().mld.memberships.push((ifindex, *addr, *group));
}

/// Takes the memberships address `addr` of `ifindex` holds.
pub(crate) fn take_memberships(st: &State, ifindex: u16, addr: &In6) -> Vec<In6> {
    let mut nd = st.nd6.lock();
    let mut out = Vec::new();
    nd.mld.memberships.retain(|(i, a, g)| {
        if *i == ifindex && a == addr {
            out.push(*g);
            false
        } else {
            true
        }
    });
    out
}

/// `in6_joingroup_locked` and `mld_initial_join`: interface `ifindex`
/// joins `group` (another reference if it has); a reported group's
/// MLDv2 report goes `delay` ticks from now (one tick for none).
pub(crate) fn join(n: &mut Net, st: &State, ifindex: u16, group: &In6, delay: u32) {
    let Some(ifp) = n.by_index(ifindex) else {
        return;
    };
    let silent = ifp.flags & iff::LOOPBACK != 0 || !is_reported(group);
    {
        let mut nd = st.nd6.lock();
        if let Some(g) = nd
            .mld
            .groups
            .iter_mut()
            .find(|g| g.ifindex == ifindex && g.addr == *group && g.state != Member::Leaving)
        {
            g.refs += 1;
            return;
        }
        // A group still saying its goodbyes is joined again.
        nd.mld
            .groups
            .retain(|g| !(g.ifindex == ifindex && g.addr == *group));
    }
    let _ = n.add_multi(ifindex, ether_group(group), false);
    let mut nd = st.nd6.lock();
    let version = nd.mld.mli(ifindex).version;
    let rv = nd.mld.mli(ifindex).rv;
    let mut g = Group {
        ifindex,
        addr: *group,
        refs: 1,
        state: Member::Silent,
        timer: 0,
        sctimer: 0,
        scrv: 0,
        pending: 0,
    };
    if !silent {
        g.state = Member::Reporting;
        if version == 1 {
            // MLDv1: a report now (or after the delay), and its repeat.
            g.timer = if delay > 0 {
                delay.max(random_delay(10 * u32::try_from(FASTHZ).unwrap_or(5)))
            } else {
                1
            };
        } else {
            g.pending = rec::CHANGE_TO_EXCLUDE_MODE;
            g.scrv = rv;
            g.sctimer = if delay > 0 { delay } else { 1 };
        }
        nd.mld.run();
    }
    nd.mld.groups.push(g);
}

/// `in6_leavegroup_locked` and `mld_final_leave`: one reference to
/// `group` on `ifindex` goes; with the last, a reported group says so
/// (MLDv2: its retransmissions, then it goes).
pub(crate) fn leave(n: &mut Net, st: &State, ifindex: u16, group: &In6) {
    let mut v1_done = None;
    let gone =
        {
            let mut nd = st.nd6.lock();
            let version = nd.mld.mli(ifindex).version;
            let rv = nd.mld.mli(ifindex).rv;
            let Some(i) = nd.mld.groups.iter().position(|g| {
                g.ifindex == ifindex && g.addr == *group && g.state != Member::Leaving
            }) else {
                return;
            };
            let g = &mut nd.mld.groups[i];
            g.refs -= 1;
            if g.refs > 0 {
                return;
            }
            match g.state {
                Member::Reporting | Member::Idle | Member::GQueryPending if version == 2 => {
                    g.timer = 0;
                    g.scrv = rv;
                    // `mld_v2_enqueue_group_record`: with
                    // `MLIF_USEALLOW`, a leave blocks the (no) sources.
                    g.pending = if on(&st.conf6.mld_use_allow) {
                        rec::BLOCK_OLD_SOURCES
                    } else {
                        rec::CHANGE_TO_INCLUDE_MODE
                    };
                    g.state = Member::Leaving;
                    g.sctimer = 1;
                    nd.mld.run();
                    false
                }
                Member::Reporting | Member::Idle => {
                    v1_done = Some(*group);
                    nd.mld.groups.remove(i);
                    true
                }
                _ => {
                    nd.mld.groups.remove(i);
                    true
                }
            }
        };
    if let Some(g) = v1_done {
        send_v1(n, st, ifindex, ty::MLD_LISTENER_DONE, &g);
    }
    if gone {
        let _ = n.del_multi(ifindex, ether_group(group));
    }
}

/// Every group of interface `ifindex` goes, silently (`in6_purgemaddrs`).
pub(crate) fn purge(n: &mut Net, st: &State, ifindex: u16) {
    let gone: Vec<In6> = {
        let mut nd = st.nd6.lock();
        let gone = nd
            .mld
            .groups
            .iter()
            .filter(|g| g.ifindex == ifindex)
            .map(|g| g.addr)
            .collect();
        nd.mld.groups.retain(|g| g.ifindex != ifindex);
        nd.mld.ifs.retain(|m| m.ifindex != ifindex);
        nd.mld.memberships.retain(|(i, _, _)| *i != ifindex);
        gone
    };
    for g in gone {
        let _ = n.del_multi(ifindex, ether_group(&g));
    }
}

/// `mld_fasttimo_vnet`: one fast tick, if due; the reports whose timers
/// ran out go, one packet per interface for each kind.
pub(crate) fn fasttimo(n: &mut Net, st: &State, now: u64) {
    let mut sends: Vec<(u16, Vec<(u8, In6)>)> = Vec::new();
    let mut v1: Vec<(u16, In6)> = Vec::new();
    let mut gone: Vec<(u16, In6)> = Vec::new();
    {
        let mut nd = st.nd6.lock();
        let mld = &mut nd.mld;
        if mld.tick_at == 0 || mld.tick_at > now {
            return;
        }
        let mut running = false;
        let ifs: Vec<u16> = mld.ifs.iter().map(|m| m.ifindex).collect();
        for ifindex in ifs {
            // The MLDv1 querier's time ends.
            let mli = mld.mli(ifindex);
            if mli.version == 1 && mli.v1_until != 0 && mli.v1_until <= now {
                mli.version = 2;
                mli.v1_until = 0;
            }
            let version = mli.version;
            let uri = random_delay(mli.uri * u32::try_from(FASTHZ).unwrap_or(5));
            let mut general = false;
            if mli.v2_timer > 0 {
                mli.v2_timer -= 1;
                if mli.v2_timer == 0 {
                    general = true;
                } else {
                    running = true;
                }
            }
            let mut qrq: Vec<(u8, In6)> = Vec::new();
            let mut scq: Vec<(u8, In6)> = Vec::new();
            if general {
                // `mld_v2_dispatch_general_query`: every reported group.
                for g in mld.groups.iter().filter(|g| {
                    g.ifindex == ifindex
                        && matches!(
                            g.state,
                            Member::Reporting | Member::Idle | Member::GQueryPending
                        )
                }) {
                    qrq.push((rec::MODE_IS_EXCLUDE, g.addr));
                }
            }
            let mut i = 0;
            while i < mld.groups.len() {
                let g = &mut mld.groups[i];
                if g.ifindex != ifindex {
                    i += 1;
                    continue;
                }
                let mut qexp = false;
                let mut scexp = false;
                if g.timer > 0 {
                    g.timer -= 1;
                    if g.timer == 0 {
                        qexp = true;
                    } else {
                        running = true;
                    }
                }
                if g.sctimer > 0 {
                    g.sctimer -= 1;
                    if g.sctimer == 0 {
                        scexp = true;
                    } else {
                        running = true;
                    }
                }
                if version == 1 {
                    if qexp && g.state == Member::Reporting {
                        v1.push((ifindex, g.addr));
                        g.state = Member::Idle;
                    }
                    i += 1;
                    continue;
                }
                if qexp && g.state == Member::GQueryPending {
                    qrq.push((rec::MODE_IS_EXCLUDE, g.addr));
                    g.state = Member::Reporting;
                }
                if scexp
                    && matches!(
                        g.state,
                        Member::Reporting | Member::Leaving | Member::GQueryPending
                    )
                {
                    g.scrv = g.scrv.saturating_sub(1);
                    if g.scrv > 0 {
                        g.sctimer = uri;
                        running = true;
                    }
                    if g.pending != 0 {
                        scq.push((g.pending, g.addr));
                    }
                    if g.state == Member::Leaving && g.scrv == 0 {
                        gone.push((ifindex, g.addr));
                        mld.groups.remove(i);
                        continue;
                    }
                }
                i += 1;
            }
            if !qrq.is_empty() {
                sends.push((ifindex, qrq));
            }
            if !scq.is_empty() {
                sends.push((ifindex, scq));
            }
        }
        mld.tick_at = if running { now + TICK } else { 0 };
        if running {
            crate::nd6::arm(mld.tick_at);
        }
    }
    for (ifindex, g) in v1 {
        send_v1(n, st, ifindex, ty::MLD_LISTENER_REPORT, &g);
    }
    for (ifindex, recs) in sends {
        send_v2(n, st, ifindex, &recs);
    }
    for (ifindex, g) in gone {
        let _ = n.del_multi(ifindex, ether_group(&g));
    }
}

/// The options MLD's packets go with (`mld_po`, `mld_dispatch_packet`):
/// hop limit 1 out of the interface, the Router Alert, `::` allowed,
/// never fragmented.
fn mld_opts(ifindex: u16) -> Opts6 {
    Opts6 {
        flags: out::UNSPECSRC | out::DONTFRAG,
        mcast: Some(Mopts6 {
            ifindex,
            hlim: 1,
            loop_: false,
        }),
        hlim: Some(1),
        zone: ifindex,
        src_zone: ifindex,
        router_alert: true,
        minmtu: ip6::minmtu::MCASTONLY,
        ..Opts6::default()
    }
}

/// `mld_v2_encap_report`'s source: the interface's link-local address if
/// it is ready, else `::`.
fn report_src(n: &Net, ifindex: u16) -> In6 {
    n.by_index(ifindex)
        .and_then(|i| in6::linklocal_of(i, in6_iff::NOTREADY | in6_iff::ANYCAST))
        .unwrap_or([0; 16])
}

/// An MLDv2 report of `recs` (record type, group) out of `ifindex`, to
/// all MLDv2 routers.
fn send_v2(n: &mut Net, st: &State, ifindex: u16, recs: &[(u8, In6)]) {
    let src = report_src(n, ifindex);
    let mut body = alloc::vec![ty::MLDV2_LISTENER_REPORT, 0, 0, 0, 0, 0];
    body.extend_from_slice(&u16::try_from(recs.len()).unwrap_or(0).to_be_bytes());
    for (t, g) in recs {
        body.extend_from_slice(&[*t, 0, 0, 0]);
        body.extend_from_slice(g);
    }
    dispatch(n, st, ifindex, &src, &in6::ALLV2ROUTERS, &body);
}

/// An MLDv1 report or done for `group` out of `ifindex` (`mld_v1_transmit_report`).
fn send_v1(n: &mut Net, st: &State, ifindex: u16, t: u8, group: &In6) {
    let src = report_src(n, ifindex);
    let dst = if t == ty::MLD_LISTENER_DONE {
        in6::ALLROUTERS
    } else {
        *group
    };
    let mut body = alloc::vec![t, 0, 0, 0, 0, 0, 0, 0];
    body.extend_from_slice(group);
    dispatch(n, st, ifindex, &src, &dst, &body);
}

/// `mld_dispatch_packet`.
fn dispatch(n: &mut Net, st: &State, ifindex: u16, src: &In6, dst: &In6, body: &[u8]) {
    let Some(m) = crate::nd6::icmp6_packet(src, dst, 1, body) else {
        return;
    };
    let t = body[0];
    if let Ok(outif) = ip6::output_if(n, st, m, &mld_opts(ifindex)) {
        ip6::count_out(st, t);
        if let Some(i) = n.by_index(outif) {
            i.stats.icmp6_inc(icmp6_ifs::OUT_MSG);
            match t {
                ty::MLD_LISTENER_REPORT | ty::MLDV2_LISTENER_REPORT => {
                    i.stats.icmp6_inc(icmp6_ifs::OUT_MLDREPORT);
                }
                ty::MLD_LISTENER_DONE => i.stats.icmp6_inc(icmp6_ifs::OUT_MLDDONE),
                _ => {}
            }
        }
    }
}

/// `mld_input`: a query is answered in time (a host ignores the other
/// messages). The message goes on to the raw sockets either way.
pub(crate) fn input(vnet: &Vnet, st: &State, m: &Packet, off: usize, len: usize) -> bool {
    let b = crate::nd6::message(m, off, len);
    let ifindex = m.hdr.rcvif;
    let mut h = [0u8; 40];
    m.data.copy_out(0, &mut h);
    let src: In6 = crate::nd6::bytes_at(&h, 8);
    let dst: In6 = crate::nd6::bytes_at(&h, 24);
    if b[0] != ty::MLD_LISTENER_QUERY {
        return true;
    }
    // From a link-local querier only.
    if !in6::is_linklocal(&src) {
        return true;
    }
    let group: In6 = crate::nd6::bytes_at(&b, 8);
    let maxdelay = u32::from(u16::from_be_bytes([b[4], b[5]]));
    let v2 = len >= 28;
    let _ = vnet;
    let mut nd = st.nd6.lock();
    if !v2 {
        // MLDv1 (RFC 3810 8.2): an MLDv1 querier is heard.
        if !on(&st.conf6.mld_v1enable) {
            return true;
        }
        if is_unspecified(&group) && dst != in6::ALLNODES {
            return true;
        }
        let mli = nd.mld.mli(ifindex);
        let span = u64::from(mli.rv * mli.qi + mli.qri) * 1_000_000_000;
        mli.version = 1;
        mli.v1_until = crate::now() + span;
        mli.v2_timer = 0;
        let timer = (maxdelay * u32::try_from(FASTHZ).unwrap_or(5) / TIMER_SCALE).max(1);
        for g in nd
            .mld
            .groups
            .iter_mut()
            .filter(|g| g.ifindex == ifindex && (is_unspecified(&group) || g.addr == group))
        {
            g.sctimer = 0;
            g.scrv = 0;
            if g.state == Member::Silent || g.state == Member::Leaving {
                continue;
            }
            if g.timer == 0 || timer < g.timer {
                g.timer = random_delay(timer);
            }
            g.state = Member::Reporting;
        }
        nd.mld.run();
        return true;
    }
    if !on(&st.conf6.mld_v2enable) {
        return true;
    }
    let mut maxdelay = maxdelay;
    if maxdelay >= 32768 {
        maxdelay = ((maxdelay & 0x0fff) | 0x1000) << (((maxdelay >> 12) & 7) + 3);
    }
    let timer = (maxdelay * u32::try_from(FASTHZ).unwrap_or(5) / TIMER_SCALE).max(1);
    let mut qrv = u32::from(b[24] & 7);
    if qrv < 2 {
        qrv = RV_INIT;
    }
    let mut qqi = u32::from(b[25]);
    if qqi >= 128 {
        qqi = ((qqi & 0x0f) | 0x10) << (((qqi >> 4) & 7) + 3);
    }
    let nsrc = usize::from(u16::from_be_bytes([b[26], b[27]]));
    if nsrc > 256 || len < 28 + nsrc * 16 {
        return true;
    }
    if is_unspecified(&group) && nsrc > 0 {
        return true;
    }
    let mli = nd.mld.mli(ifindex);
    if mli.version != 2 {
        return true;
    }
    mli.rv = qrv;
    mli.qi = qqi;
    mli.qri = maxdelay;
    if is_unspecified(&group) {
        if mli.v2_timer == 0 || mli.v2_timer >= timer {
            mli.v2_timer = random_delay(timer);
            nd.mld.run();
        }
    } else {
        let general = nd.mld.mli(ifindex).v2_timer;
        if general != 0 && general < timer {
            return true;
        }
        if let Some(g) = nd.mld.groups.iter_mut().find(|g| {
            g.ifindex == ifindex
                && g.addr == group
                && matches!(
                    g.state,
                    Member::Reporting | Member::Idle | Member::GQueryPending
                )
        }) {
            if g.state == Member::GQueryPending && g.timer != 0 && g.timer < timer {
                return true;
            }
            g.state = Member::GQueryPending;
            g.timer = random_delay(timer);
            nd.mld.run();
        }
    }
    true
}

/// Every group each interface joined (for `NET_RT_IFMALIST`).
#[must_use]
pub fn groups(vnet: &Vnet) -> Vec<(u16, In6)> {
    crate::state::of(vnet)
        .nd6
        .lock()
        .mld
        .groups
        .iter()
        .map(|g| (g.ifindex, g.addr))
        .collect()
}

/// `net.inet6.mld.ifinfo.<ifindex>`: the interface's `struct
/// mld_ifinfo` (version, timers, flags, robustness, intervals); `None`
/// for an interface without IPv6.
#[must_use]
pub fn ifinfo(vnet: &Vnet, ifindex: u16) -> Option<[u32; 8]> {
    let st = crate::state::of(vnet);
    vnet.net.lock().by_index(ifindex)?;
    let mut nd = st.nd6.lock();
    let m = nd.mld.mli(ifindex);
    let v1 = if m.v1_until == 0 {
        0
    } else {
        u32::try_from(m.v1_until.saturating_sub(crate::now()) / 1_000_000_000).unwrap_or(0)
    };
    Some([
        u32::from(m.version),
        v1,
        m.v2_timer,
        0,
        m.rv,
        m.qi,
        m.qri,
        m.uri,
    ])
}

/// `net.inet6.ip6.mcast.filters.<ifindex>.<group>`: a listed group's
/// filter mode: exclude with no sources while joined (every join here is
/// any-source), undefined while it is being left; `None` for a group
/// not listed.
#[must_use]
pub fn filter_mode(vnet: &Vnet, ifindex: u16, group: &In6) -> Option<u32> {
    let st = crate::state::of(vnet);
    let listed = st
        .nd6
        .lock()
        .mld
        .groups
        .iter()
        .any(|g| g.ifindex == ifindex && g.addr == *group);
    listed.then(|| if member(&st, ifindex, group) { 2 } else { 0 })
}
