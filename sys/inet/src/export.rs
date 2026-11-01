// SPDX-License-Identifier: BSD-2-Clause
//! What `netstat` and `sockstat` read: `net.inet.udp.pcblist` (an
//! `xinpgen`, an `xinpcb` per PCB, another `xinpgen`) and
//! `net.inet.tcp.pcblist` (with `xtcpcb`s), newest PCB first.
//!
//! Behavior reference: sys/netinet/in_pcb.c (`in_pcbtoxinpcb`),
//! sys/netinet/udp_usrreq.c (`udp_pcblist`), sys/netinet/tcp_subr.c
//! (`tcp_pcblist`, `tcp_inptoxtp`), sys/kern/uipc_socket.c
//! (`sotoxsocket`), sys/kern/uipc_sockbuf.c (`sbtoxsockbuf`).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::inet::{
    INC_ISIPV6, INP_IPV6, InConninfo, InEndpoints, Xinpcb, Xinpgen, Xsockbuf, Xsocket, Xtcpcb,
};

use crate::addr::In6;
use crate::pcb::table;
use crate::{Proto, Sb, Socket};

/// A PCB's fields, copied out under the table's lock.
#[derive(Clone, Debug)]
struct Snap {
    so: Weak<Socket>,
    family: crate::Family,
    vflag: u8,
    flags: i32,
    flags2: i32,
    laddr: In6,
    faddr: In6,
    lport: u16,
    fport: u16,
    zone: u32,
    uid: u32,
    gencnt: u64,
    ip_p: u8,
}

/// The kernel address a record names a socket by (`xso_so`), and its
/// PCB (`so_pcb`).
#[must_use]
pub fn so_addr(so: &Socket) -> u64 {
    core::ptr::from_ref(so).addr() as u64
}

/// See [`so_addr`].
#[must_use]
pub fn pcb_addr(so: &Socket) -> u64 {
    core::ptr::from_ref(&so.inner).addr() as u64
}

fn xsockbuf(sb: &Sb) -> Xsockbuf {
    Xsockbuf {
        sb_cc: sb.cc,
        sb_hiwat: sb.hiwat,
        sb_mbcnt: sb.mbcnt,
        sb_spare0: 0,
        sb_spare1: 0,
        sb_mbmax: sb.mbmax,
        sb_lowat: sb.lowat.cast_signed(),
        // Ticks (`hz` 1000).
        sb_timeo: i32::try_from(sb.timeo / 1_000_000).unwrap_or(i32::MAX),
        sb_flags: 0,
        _pad: 0,
    }
}

/// `sotoxsocket`.
fn xsocket(so: &Socket, uid: u32) -> Xsocket {
    let i = so.inner.lock();
    let (qlen, qlimit) = i
        .tcp
        .as_ref()
        .and_then(|t| t.listen.as_ref())
        .map_or((0, 0), |l| {
            (u32::try_from(l.comp.len()).unwrap_or(0), l.qlimit)
        });
    #[allow(
        clippy::cast_possible_truncation,
        reason = "so_options is a short here"
    )]
    let options = so.so_options() as i16;
    Xsocket {
        xso_len: size_of::<Xsocket>() as u64,
        xso_so: so_addr(so),
        so_pcb: pcb_addr(so),
        so_oobmark: 0,
        so_splice_so: 0,
        so_spare64: [0; 7],
        xso_protocol: so.proto.number(),
        xso_family: i32::from(so.family.af()),
        so_qlen: qlen,
        so_incqlen: 0,
        so_qlimit: qlimit,
        so_pgid: 0,
        so_uid: uid,
        so_fibnum: 0,
        so_spare32: [0; 7],
        so_type: i16::try_from(so.proto.sock_type()).unwrap_or(0),
        so_options: options,
        so_linger: i16::try_from(i.linger).unwrap_or(i16::MAX),
        so_state: i.state,
        so_timeo: 0,
        so_error: i.error.map_or(0, |e| u16::try_from(e.get()).unwrap_or(0)),
        so_rcv: xsockbuf(&i.rcv),
        so_snd: xsockbuf(&i.snd),
    }
}

/// `in_pcbtoxinpcb`.
fn xinpcb(c: &Snap, so: Option<&Arc<Socket>>) -> Xinpcb {
    let mut x = Xinpcb {
        xi_len: size_of::<Xinpcb>() as u64,
        ..Xinpcb::default()
    };
    match so {
        Some(s) => x.xi_socket = xsocket(s, c.uid),
        None => {
            // A connection in `TIME_WAIT` after its socket went.
            x.xi_socket.xso_len = size_of::<Xsocket>() as u64;
            x.xi_socket.xso_protocol = abi::inet::IPPROTO_TCP;
            x.xi_socket.xso_family = i32::from(c.family.af());
            x.xi_socket.so_type = i16::try_from(abi::socket::SOCK_STREAM).unwrap_or(0);
            x.xi_socket.so_uid = c.uid;
        }
    }
    let v6 = c.vflag & INP_IPV6 != 0;
    x.inp_inc = InConninfo {
        inc_flags: if v6 { INC_ISIPV6 } else { 0 },
        inc_len: 0,
        inc_fibnum: 0,
        inc_ie: InEndpoints {
            ie_fport: c.fport.to_be_bytes(),
            ie_lport: c.lport.to_be_bytes(),
            ie_dependfaddr: c.faddr,
            ie_dependladdr: c.laddr,
            ie6_zoneid: c.zone,
        },
    };
    x.inp_gencnt = c.gencnt;
    x.inp_flags = c.flags;
    x.inp_flags2 = c.flags2;
    x.inp_vflag = c.vflag;
    x.inp_ip_p = c.ip_p;
    if let Some(s) = so {
        let i = s.inner.lock();
        x.inp_ip_ttl = i.ip.ttl;
        x.inp_ip_tos = i.ip.tos;
        x.inp_ip_minttl = i.ip.minttl;
        x.in6p_hops = u16::from_ne_bytes(i.ip.hops.to_ne_bytes());
    } else {
        x.inp_ip_ttl = 64;
        x.in6p_hops = u16::MAX;
    }
    x.in6p_cksum = -1;
    x
}

fn copies(proto: Proto, vnet: u32) -> (u64, Vec<Snap>) {
    let mut t = table(proto).lock();
    let mut v = Vec::new();
    for (_, p) in t.iter() {
        if p.vnet.id != vnet {
            continue;
        }
        v.push(Snap {
            so: p.so.clone(),
            family: p.family,
            vflag: p.vflag,
            flags: p.flags,
            flags2: p.flags2,
            laddr: p.laddr,
            faddr: p.faddr,
            lport: p.lport,
            fport: p.fport,
            zone: p.zone,
            uid: p.uid,
            gencnt: p.gencnt,
            ip_p: p.ip_p,
        });
    }
    (t.gencnt, v)
}

fn xinpgen(count: usize, gen_: u64) -> Xinpgen {
    Xinpgen {
        xig_len: size_of::<Xinpgen>() as u64,
        xig_count: u32::try_from(count).unwrap_or(u32::MAX),
        _xig_spare32: 0,
        xig_gen: gen_,
        xig_sogen: gen_,
        _xig_spare64: [0; 4],
    }
}

fn push<T: Copy>(out: &mut Vec<u8>, v: &T) {
    // SAFETY: `T` is one of this module's `#[repr(C)]` records of
    // integers and byte arrays, every field (padding included, by the
    // explicit spare fields) initialized; viewing it as bytes reads
    // initialized memory of its exact size.
    let b =
        unsafe { core::slice::from_raw_parts(core::ptr::from_ref(v).cast::<u8>(), size_of::<T>()) };
    out.extend_from_slice(b);
}

/// `udp_pcblist`: the records for the PCBs of VNET `vnet` that
/// `visible` lets the caller see (by uid, `cr_canseeinpcb`).
#[must_use]
pub fn udp_pcblist(vnet: u32, visible: &dyn Fn(u32) -> bool) -> Vec<u8> {
    dgram_pcblist(Proto::Udp, vnet, visible)
}

/// `rip_pcblist`: the raw sockets', as [`udp_pcblist`].
#[must_use]
pub fn raw_pcblist(vnet: u32, visible: &dyn Fn(u32) -> bool) -> Vec<u8> {
    dgram_pcblist(Proto::Raw(0), vnet, visible)
}

fn dgram_pcblist(proto: Proto, vnet: u32, visible: &dyn Fn(u32) -> bool) -> Vec<u8> {
    let (gen_, list) = copies(proto, vnet);
    let list: Vec<_> = list.into_iter().filter(|c| visible(c.uid)).collect();
    let mut out = Vec::new();
    push(&mut out, &xinpgen(list.len(), gen_));
    for c in &list {
        let so = c.so.upgrade();
        let x = xinpcb(c, so.as_ref());
        push(&mut out, &x);
    }
    push(&mut out, &xinpgen(list.len(), gen_));
    out
}

/// `tcp_pcblist`, with `tcp_inptoxtp`'s fields.
#[must_use]
pub fn tcp_pcblist(vnet: u32, visible: &dyn Fn(u32) -> bool) -> Vec<u8> {
    let (gen_, list) = copies(Proto::Tcp, vnet);
    let list: Vec<_> = list.into_iter().filter(|c| visible(c.uid)).collect();
    let mut out = Vec::new();
    push(&mut out, &xinpgen(list.len(), gen_));
    for c in &list {
        let so = c.so.upgrade();
        let mut x = Xtcpcb {
            xt_len: size_of::<Xtcpcb>() as u64,
            ..Xtcpcb::default()
        };
        x.xt_inp = xinpcb(c, so.as_ref());
        x.xt_stack[..7].copy_from_slice(b"freebsd");
        let snap = so
            .as_ref()
            .map(|s| crate::tcp::snapshot(s))
            .unwrap_or_default();
        x.t_state = snap.state;
        x.t_flags = snap.flags;
        x.t_maxseg = snap.maxseg;
        x.t_rcv_wnd = snap.rcv_wnd;
        x.t_snd_wnd = snap.snd_wnd;
        x.t_snd_cwnd = snap.snd_cwnd;
        x.t_snd_ssthresh = snap.snd_ssthresh;
        x.t_sndzerowin = snap.sndzerowin.cast_signed();
        x.t_sndrexmitpack = snap.sndrexmitpack.cast_signed();
        x.t_rcvoopack = snap.rcvoopack.cast_signed();
        // Ticks (`hz` 1000) since it last received, and the timers' left.
        x.t_rcvtime = i32::try_from(snap.idle).unwrap_or(i32::MAX);
        let tick = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
        x.tt_rexmt = tick(snap.timers[0]);
        x.tt_persist = tick(snap.timers[1]);
        x.tt_keep = tick(snap.timers[2]);
        x.tt_2msl = tick(snap.timers[3]);
        x.tt_delack = tick(snap.timers[4]);
        x.xt_ecn = snap.ecn;
        x.t_dsack_bytes = snap.dsack_bytes;
        x.t_dsack_pack = snap.dsack_pack;
        x.xt_cc = [0; abi::inet::tcp::TCP_CA_NAME_MAX];
        let cc = if snap.cc.is_empty() { "cubic" } else { snap.cc };
        x.xt_cc[..cc.len()].copy_from_slice(cc.as_bytes());
        push(&mut out, &x);
    }
    push(&mut out, &xinpgen(list.len(), gen_));
    out
}

/// The number of PCBs of `proto` in VNET `vnet` (`net.inet.*.pcbcount`'s
/// value).
#[must_use]
pub fn count(proto: Proto, vnet: u32) -> usize {
    table(proto)
        .lock()
        .iter()
        .filter(|(_, p)| p.vnet.id == vnet)
        .count()
}
