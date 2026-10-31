// SPDX-License-Identifier: BSD-2-Clause
//! Host tests: FreeBSD's UNIX-domain behavior, case by case.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::socket::{
    LOCAL_CREDS, MSG_EOR, MSG_TRUNC, SHUT_WR, SO_RCVBUF, SO_SNDBUF, SOL_LOCAL, SOL_SOCKET,
};

use crate::{Control, Cred, InFlight, Passable, Socket, Type, chain, chain_from, gc, inflight};

/// Held by the tests that put descriptors in flight: the count of them
/// and the collector are the whole process's.
static IN_FLIGHT: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cred() -> Cred {
    Cred {
        pid: 7,
        ruid: 1,
        euid: 2,
        rgid: 3,
        egid: 4,
        groups: vec![4, 5],
    }
}

fn pair(ty: Type) -> (Arc<Socket>, Arc<Socket>) {
    let a = Socket::new(ty);
    let b = Socket::new(ty);
    Socket::pair(&a, &b, &cred()).unwrap();
    (a, b)
}

/// Sends all of `data` it can: returns what went.
fn send(s: &Socket, data: &[u8], eor: bool) -> Result<usize, Errno> {
    let mut c: VecDeque<_> = chain(data, eor);
    s.send_stream(&mut c, &mut Vec::new(), &cred)
}

#[test]
fn stream_round_trip_and_eof() {
    let (a, b) = pair(Type::Stream);
    assert_eq!(send(&a, b"hello", false), Ok(5));
    let r = b.recv_stream(3, false, 0, false, false).unwrap();
    assert_eq!(r.data, b"hel");
    let r = b.recv_stream(10, true, 0, false, false).unwrap();
    assert_eq!(r.data, b"lo");
    let r = b.recv_stream(10, false, 0, false, false).unwrap();
    assert_eq!(r.data, b"lo");
    assert_eq!(
        b.recv_stream(10, false, 0, false, false).unwrap_err(),
        Errno::EAGAIN
    );
    a.close();
    assert!(
        b.recv_stream(10, false, 0, false, false)
            .unwrap()
            .data
            .is_empty()
    );
    assert_eq!(send(&b, b"x", false), Err(Errno::EPIPE));
}

#[test]
fn chain_from_fills_as_chain_does() {
    let bytes: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let mut at = 0;
    let c = chain_from(bytes.len(), true, |b| -> Result<usize, ()> {
        b.copy_from_slice(&bytes[at..at + b.len()]);
        at += b.len();
        Ok(b.len())
    })
    .unwrap();
    let want = chain(&bytes, true);
    assert_eq!(c.len(), want.len());
    for (a, b) in c.iter().zip(&want) {
        assert_eq!((&a.data, a.ext, a.eor), (&b.data, b.ext, b.eor));
    }
    // A short copy ends the chain, unmarked.
    let c = chain_from(5000, true, |b| -> Result<usize, ()> {
        Ok(b.len().min(100))
    })
    .unwrap();
    assert_eq!((c.len(), c[0].data.len(), c[0].eor), (1, 100, false));
}

#[test]
fn stream_fills_as_freebsd_counts() {
    let (a, b) = pair(Type::Stream);
    // 64 KiB of room, all taken in big writes.
    let big = vec![0u8; 100_000];
    assert_eq!(send(&a, &big, false), Ok(65536));
    assert_eq!(send(&a, b"x", false), Err(Errno::EAGAIN));
    let r = b.recv_stream(1000, false, 0, false, false).unwrap();
    assert_eq!(r.data.len(), 1000);
    assert_eq!(send(&a, &big, false), Ok(1000));
}

#[test]
fn small_writes_fill_by_memory() {
    let (a, _b) = pair(Type::Stream);
    // One-byte writes each take an mbuf: memory (64 KiB * 8 / 256 = 2048
    // of them) ends first.
    let mut n = 0;
    while send(&a, b"x", false) == Ok(1) {
        n += 1;
    }
    assert_eq!(n, 2048);
}

#[test]
fn seqpacket_records() {
    let (a, b) = pair(Type::Seqpacket);
    send(&a, b"one", true).unwrap();
    send(&a, b"two", true).unwrap();
    let r = b.recv_stream(100, false, 0, false, false).unwrap();
    assert_eq!(
        (r.data.as_slice(), r.flags & MSG_EOR),
        (&b"one"[..], MSG_EOR)
    );
    let r = b.recv_stream(2, false, 0, false, false).unwrap();
    assert_eq!((r.data.as_slice(), r.flags & MSG_EOR), (&b"tw"[..], 0));
    let r = b.recv_stream(2, false, 0, false, false).unwrap();
    assert_eq!((r.data.as_slice(), r.flags & MSG_EOR), (&b"o"[..], MSG_EOR));
}

#[test]
fn dgram_queues_and_enobufs() {
    let (a, b) = pair(Type::Dgram);
    a.send_dgram(b"abc", Vec::new(), None, &cred).unwrap();
    let r = b.recv_dgram(2, false, false).unwrap();
    assert_eq!(
        (r.data.as_slice(), r.flags & MSG_TRUNC),
        (&b"ab"[..], MSG_TRUNC)
    );
    assert_eq!(b.recv_dgram(8, false, false).unwrap_err(), Errno::EAGAIN);
    let max = vec![0u8; 8192];
    assert_eq!(
        a.send_dgram(&[0; 8193], Vec::new(), None, &cred),
        Err(Errno::EMSGSIZE)
    );
    // The sender's 16 KiB send buffer holds one 8 KiB datagram and its
    // name, not two.
    a.send_dgram(&max, Vec::new(), None, &cred).unwrap();
    assert_eq!(
        a.send_dgram(&max, Vec::new(), None, &cred),
        Err(Errno::ENOBUFS)
    );
    assert_eq!(b.recv_dgram(9000, true, false).unwrap().data.len(), 8192);
    assert_eq!(b.recv_dgram(9000, false, false).unwrap().data.len(), 8192);
}

#[test]
fn listen_connect_accept() {
    let l = Socket::new(Type::Stream);
    assert_eq!(l.listen(1, &cred()), Err(Errno::EDESTADDRREQ));
    l.bind_begin().unwrap();
    l.bind_end(Some(vec![3, 1, b'x']));
    l.listen(1, &cred()).unwrap();
    let c = Socket::new(Type::Stream);
    c.connect_begin().unwrap();
    c.connect_to(&l, &cred()).unwrap();
    assert!(c.is_connected());
    assert_eq!(c.connect_begin(), Err(Errno::EISCONN));
    let s = l.accept().unwrap();
    assert_eq!(l.accept().unwrap_err(), Errno::EAGAIN);
    send(&c, b"hi", false).unwrap();
    assert_eq!(
        s.recv_stream(9, false, 0, false, false).unwrap().data,
        b"hi"
    );
    assert_eq!(s.peercred().unwrap().cr_uid, 2);
    // Backlog 1: up to 3 * 1 / 2 + 1 = 2 queued, then refused.
    for _ in 0..2 {
        let x = Socket::new(Type::Stream);
        x.connect_begin().unwrap();
        x.connect_to(&l, &cred()).unwrap();
    }
    let x = Socket::new(Type::Stream);
    x.connect_begin().unwrap();
    assert_eq!(x.connect_to(&l, &cred()), Err(Errno::ECONNREFUSED));
    let d = Socket::new(Type::Dgram);
    d.connect_begin().unwrap();
    assert_eq!(d.connect_to(&l, &cred()), Err(Errno::EPROTOTYPE));
}

#[test]
fn shutdown_write_ends_the_peers_reads() {
    let (a, b) = pair(Type::Stream);
    a.shutdown(SHUT_WR).unwrap();
    assert!(
        b.recv_stream(1, false, 0, false, false)
            .unwrap()
            .data
            .is_empty()
    );
    assert_eq!(send(&a, b"x", false), Err(Errno::EPIPE));
}

#[test]
fn creds_with_first_send() {
    let (a, b) = pair(Type::Stream);
    b.setopt(SOL_LOCAL, LOCAL_CREDS, &1i32.to_ne_bytes())
        .unwrap();
    send(&a, b"1", false).unwrap();
    send(&a, b"2", false).unwrap();
    let r = b.recv_stream(1, false, 0, false, true).unwrap();
    assert_eq!(r.control.len(), 1);
    let r = b.recv_stream(1, false, 0, false, true).unwrap();
    assert!(r.control.is_empty());
}

#[test]
fn buffer_sizes() {
    let s = Socket::new(Type::Stream);
    s.setopt(SOL_SOCKET, SO_SNDBUF, &8192i32.to_ne_bytes())
        .unwrap();
    assert_eq!(
        s.getopt(SOL_SOCKET, SO_SNDBUF).unwrap(),
        8192i32.to_ne_bytes()
    );
    assert_eq!(
        s.setopt(SOL_SOCKET, SO_RCVBUF, &i32::MAX.to_ne_bytes()),
        Err(Errno::ENOBUFS)
    );
    assert_eq!(
        s.setopt(SOL_SOCKET, SO_RCVBUF, &0i32.to_ne_bytes()),
        Err(Errno::EINVAL)
    );
}

/// A file that is a socket.
struct File(Arc<Socket>);

impl Passable for File {
    fn unix(&self) -> Option<&Arc<Socket>> {
        Some(&self.0)
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
}

#[test]
fn collector_frees_a_socket_sent_to_itself() {
    let _g = IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (a, b) = pair(Type::Stream);
    let fb: Arc<dyn Passable> = Arc::new(File(Arc::clone(&b)));
    let before = inflight();
    // b's own descriptor in flight to b, and then b's last one closed.
    let mut c: VecDeque<_> = chain(b"x", false);
    a.send_stream(
        &mut c,
        &mut vec![Control::Rights(vec![InFlight::new(Arc::clone(&fb), 0)])],
        &cred,
    )
    .unwrap();
    assert_eq!(inflight(), before + 1);
    drop(fb);
    assert_eq!(gc(), 1);
    assert_eq!(inflight(), before);
}

#[test]
fn control_only_sends_are_received_alone() {
    let _g = IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (a, b) = pair(Type::Stream);
    let fb: Arc<dyn Passable> = Arc::new(File(Arc::clone(&a)));
    for _ in 0..2 {
        let mut c: VecDeque<_> = chain(b"", false);
        let rights = vec![Control::Rights(vec![InFlight::new(Arc::clone(&fb), 0)])];
        let mut ctl = rights;
        assert_eq!(a.send_stream(&mut c, &mut ctl, &cred), Ok(0));
        let r = b.recv_stream(0, false, 0, false, true).unwrap();
        assert_eq!((r.control.len(), r.data.len()), (1, 0));
    }
}
