// SPDX-License-Identifier: BSD-2-Clause
//! TCP on the simulated network.

use alloc::vec::Vec;

use crate::hdr::{Th, th};
use crate::sim::Net;
use crate::tcb::tf;
use crate::{State, tcps};

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn a_handshake_negotiates_freebsd_s_options() {
    let mut n = Net::new();
    n.connect();
    n.run(100);
    assert_eq!(n.state(0), State::Established);
    assert_eq!(n.state(1), State::Established);
    assert!(n.hosts[0].so.connected && n.hosts[1].so.connected);
    // The SYN: mss 1460, wscale 8 (sb_max 8 MiB), SACK, timestamps.
    let (_, syn, _) = n.log[0];
    assert_eq!(syn.flags, th::SYN);
    assert_eq!(syn.off, 40);
    for h in 0..2 {
        let tp = n.tp(h);
        assert_eq!(tp.snd_scale, 8);
        assert_eq!(tp.rcv_scale, 8);
        assert_ne!(tp.flags & tf::RCVD_TSTMP, 0);
        assert_ne!(tp.flags & tf::SACK_PERMIT, 0);
        // 1500 - 40 of headers - 12 of timestamps.
        assert_eq!(tp.maxseg(), 1448);
    }
    assert_eq!(n.hosts[1].stats.get(tcps::SC_ADDED), 1);
    assert_eq!(n.hosts[1].stats.get(tcps::SC_COMPLETED), 1);
}

#[test]
fn data_crosses_and_the_connection_closes() {
    let mut n = Net::new();
    n.connect();
    n.run(100);
    let data = pattern(300_000);
    let got = n.transfer(&data, 20);
    assert_eq!(got, data);
    // The client closes first: TIME_WAIT for it, LAST_ACK then CLOSED
    // for the server once it closes too.
    n.shutdown(0);
    n.run(100);
    assert_eq!(n.state(0), State::FinWait2);
    assert_eq!(n.state(1), State::CloseWait);
    assert!(n.hosts[1].so.cantrcvmore);
    n.shutdown(1);
    n.run(100);
    assert_eq!(n.state(0), State::TimeWait);
    assert!(n.hosts[1].closed);
    // 2 MSL later the TIME_WAIT is over.
    n.run(61_000);
    assert!(n.hosts[0].closed);
}

#[test]
fn a_lost_syn_ack_is_retransmitted_after_a_second() {
    let mut n = Net::new();
    n.lose = |from, i, _| from == 1 && i == 0;
    n.connect();
    n.run(500);
    assert_eq!(n.state(0), State::SynSent);
    n.run(1500);
    assert_eq!(n.state(0), State::Established);
    assert_eq!(n.state(1), State::Established);
    // Whichever came first answered: the client's own SYN retransmission
    // (a duplicate SYN), or the server's retransmission timer.
    let s = &n.hosts[1].stats;
    assert_eq!(s.get(tcps::SC_DUPSYN) + s.get(tcps::SC_RETRANSMITTED), 1);
}

#[test]
fn a_lost_segment_is_recovered_with_sack() {
    let mut n = Net::new();
    n.rate = 1000;
    // The 20th segment the client sends is lost.
    n.lose = |from, i, t: &Th| from == 0 && i == 20 && t.flags & th::SYN == 0;
    n.connect();
    n.run(100);
    let data = pattern(500_000);
    let got = n.transfer(&data, 10);
    assert_eq!(got, data);
    let s = &n.hosts[0].stats;
    assert_eq!(s.get(tcps::SACK_RECOVERY_EPISODE), 1);
    assert!(s.get(tcps::SACK_REXMITS) >= 1);
    assert_eq!(s.get(tcps::REXMTTIMEO), 0, "recovered without a timeout");
}

#[test]
fn losses_without_sack_use_newreno() {
    let mut n = Net::new();
    n.conf
        .sack_enable
        .store(0, core::sync::atomic::Ordering::Relaxed);
    n.rate = 1000;
    n.lose = |from, i, t: &Th| from == 0 && (i == 30 || i == 31) && t.flags & th::SYN == 0;
    n.connect();
    n.run(100);
    assert_eq!(n.tp(0).flags & tf::SACK_PERMIT, 0);
    let data = pattern(400_000);
    let got = n.transfer(&data, 10);
    assert_eq!(got, data);
    assert!(n.hosts[0].stats.get(tcps::SNDREXMITPACK) >= 2);
}

#[test]
fn a_zero_window_is_probed_until_it_opens() {
    let mut n = Net::new();
    n.connect();
    n.run(100);
    n.hosts[1].so.rcv_auto = false;
    // The server does not read: its 64 KiB fill and the window closes.
    let data = pattern(100_000);
    n.write(0, &data[..32768]);
    for _ in 0..20 {
        n.run(50);
        let room = n.hosts[0].so.snd_hiwat as usize - n.hosts[0].so.snd.len();
        let sent = n.hosts[0].so.snd.len();
        let _ = (room, sent);
    }
    let mut off = 32768;
    while off < data.len() {
        let room = n.hosts[0].so.snd_hiwat as usize - n.hosts[0].so.snd.len();
        if room == 0 {
            break;
        }
        let k = room.min(data.len() - off);
        n.write(0, &data[off..off + k]);
        off += k;
        n.run(50);
    }
    n.run(20_000);
    assert!(n.hosts[1].so.rcv.len() <= n.hosts[1].so.rcv_hiwat as usize);
    assert!(n.hosts[0].stats.get(tcps::PERSISTTIMEO) >= 1);
    assert_eq!(n.state(0), State::Established);
    // Reading opens the window; the rest follows.
    let mut got = n.read_all(1);
    while off < data.len() || got.len() < off {
        let room = n.hosts[0].so.snd_hiwat as usize - n.hosts[0].so.snd.len();
        let k = room.min(data.len() - off);
        if k > 0 {
            n.write(0, &data[off..off + k]);
            off += k;
        }
        n.run(100);
        got.extend(n.read_all(1));
    }
    assert_eq!(got, data);
}

#[test]
fn a_dead_peer_times_out_after_the_retries() {
    let mut n = Net::new();
    n.connect();
    n.run(100);
    n.lose = |from, _, _| from == 0;
    n.write(0, b"hello");
    n.run(30 * 60 * 1000);
    assert!(n.hosts[0].closed);
    assert_eq!(n.hosts[0].so.error, Some(crate::Errno::ETIMEDOUT));
    assert_eq!(n.hosts[0].stats.get(tcps::TIMEOUTDROP), 1);
}

#[test]
fn a_reset_answers_a_segment_for_no_connection() {
    let syn = Th {
        sport: 1,
        dport: 2,
        seq: 100,
        ack: 0,
        off: 20,
        flags: th::SYN,
        ..Th::default()
    };
    let r = crate::reset_for(&syn, 0).unwrap();
    assert_eq!((r.flags, r.seq, r.ack), (th::RST | th::ACK, 0, 101));
    let ack = Th {
        flags: th::ACK,
        ack: 555,
        ..syn
    };
    let r = crate::reset_for(&ack, 10).unwrap();
    assert_eq!((r.flags, r.seq), (th::RST, 555));
    assert!(
        crate::reset_for(
            &Th {
                flags: th::RST,
                ..syn
            },
            0
        )
        .is_none()
    );
}

#[test]
fn urgent_data_is_taken_out_of_line() {
    let mut n = Net::new();
    n.connect();
    n.run(100);
    n.hosts[0]
        .so
        .snd
        .append(mbuf::Chain::from_slice(b"abc!").unwrap());
    let _ = n.with(0, |tp, so, cx| tp.send_oob(so, cx));
    n.run(100);
    assert_eq!(n.hosts[1].so.oob, Some(b'!'));
    assert_eq!(n.read_all(1), b"abc");
}

#[test]
fn cubic_grows_the_window_past_slow_start() {
    let mut n = Net::new();
    n.rate = 2000;
    n.delay = 20;
    n.lose = |from, i, t: &Th| from == 0 && i == 200 && t.flags & th::SYN == 0;
    n.connect();
    n.run(200);
    let data = pattern(3_000_000);
    let got = n.transfer(&data, 20);
    assert_eq!(got, data);
    let tp = n.tp(0);
    assert!(tp.snd_ssthresh < 1 << 30, "a loss set ssthresh");
    assert!(
        tp.snd_cwnd > 10 * 1448,
        "the window grew again: {}",
        tp.snd_cwnd
    );
}

#[test]
fn reassembly_fills_gaps_in_any_order() {
    use crate::cc::Algo;
    use crate::sim::SimSock;
    use crate::tcb::{Inc, Tcpcb};
    let conf = crate::Conf::new();
    let stats = crate::Stats::new();
    let hc = crate::hostcache::HostCache::new();
    let mut out = Vec::new();
    let cx = crate::Ctx {
        now: 0,
        conf: &conf,
        stats: &stats,
        random: || 1,
        out: &mut out,
        hc: &hc,
        path: &|_| crate::tcb::Path::default(),
        secrets: &crate::isn::Secrets::default(),
    };
    let data = pattern(10_000);
    // Pieces of 1000 bytes (some overlapping their neighbors), out of
    // order; the one at the start comes last.
    let pieces: [(usize, usize); 9] = [
        (3000, 4000),
        (5000, 6500),
        (1000, 2000),
        (6000, 7000),
        (2000, 3000),
        (8000, 10_000),
        (4000, 5000),
        (6500, 8000),
        (0, 1000),
    ];
    let mut tp = Tcpcb::new(Inc::default(), Algo::NewReno, &conf, 0);
    tp.state = crate::State::Established;
    tp.rcv_nxt = 100;
    let mut so = SimSock::new();
    for (a, b) in pieces {
        let c = mbuf::Chain::from_slice(&data[a..b]).unwrap();
        let _ = tp.reass(&mut so, &cx, Some((100 + a as u32, 0, (b - a) as u32, c)));
    }
    assert!(tp.segq.is_empty());
    assert_eq!(tp.rcv_nxt, 10_100);
    assert_eq!(so.read(20_000), data);
}
