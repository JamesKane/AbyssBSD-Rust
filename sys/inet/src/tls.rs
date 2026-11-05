// SPDX-License-Identifier: BSD-2-Clause
//! A socket's kernel TLS (`sys/ktls` has the records).
//!
//! Sending seals records as they are queued, in the sender's context:
//! [`Socket::tls_tx`]'s lock, taken around each record and its place in the
//! send buffer, keeps sequence numbers in the order of the stream. The
//! lock comes before the socket's; a cipher never runs under the latter.
//!
//! Receiving keeps what TCP delivers apart ([`Rx::mtls`], `sb_mtls`) until
//! a whole record is there, then a `ktls` thread ([`next`],
//! [`Socket::ktls_decrypt`]) opens it and moves its payload to the stream,
//! where [`Rx::records`] holds where each record ends. `rcv.cc` counts only
//! what can be read, as `sbavail` does, so `poll` and `kevent` look at it
//! as for any socket; TCP's window counts [`Inner::tls_pending`] too.
//!
//! Behavior reference: sys/kern/uipc_ktls.c (`ktls_enable_tx`,
//! `ktls_enable_rx`, `ktls_check_rx`, `ktls_decrypt`, `ktls_drop`),
//! sys/kern/uipc_socket.c (`sosend_generic`, `soreceive_generic`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;

use abi::errno::Errno;
use abi::inet::{ss, tcp};
use abi::ktls::TLS_RLTYPE_ALERT;
use abi::socket::MSG_EOR;
use ktls::{Direction, Enable, Layout, MAX_FRAME, Session, Tx, stats};
use mbuf::Chain;
use sync::SpinLock;

use crate::{Inner, Received, Socket};

/// `struct tls_get_record`, as a control message carries it.
pub type RecordInfo = [u8; 6];

/// A record's payload in the stream.
#[derive(Debug)]
struct Record {
    /// Its `TLS_GET_RECORD`, until read.
    ctl: Option<RecordInfo>,
    /// Bytes not yet read.
    left: usize,
}

/// A socket's receiving side.
#[derive(Debug)]
pub(crate) struct Rx {
    session: Arc<Session>,
    /// `sb_tls_seqno`.
    seqno: u64,
    /// Received and not yet opened (`sb_mtls`).
    mtls: Chain,
    /// Being opened (`sb_tlsdcc`); 0 after a flush took it away.
    dcc: u32,
    /// `SB_TLS_RX_RUNNING`: on the queue, or a thread has it.
    running: bool,
    records: VecDeque<Record>,
}

/// Sockets with whole records (`ktls_wq`'s `so_head`).
static QUEUE: SpinLock<VecDeque<Arc<Socket>>> = SpinLock::new(VecDeque::new());
static KICK: SpinLock<Option<fn()>> = SpinLock::new(None);

/// Sets what is called when the queue gets work.
pub fn set_kick(f: fn()) {
    *KICK.lock() = Some(f);
}

/// The next socket with records to open.
pub fn next() -> Option<Arc<Socket>> {
    let so = QUEUE.lock().pop_front();
    if so.is_some() {
        stats::SW_RX_INQUEUE.fetch_sub(1, Relaxed);
    }
    so
}

/// Whether [`next`] has one.
pub fn pending() -> bool {
    !QUEUE.lock().is_empty()
}

pub(crate) fn enqueue(so: &Arc<Socket>) {
    QUEUE.lock().push_back(Arc::clone(so));
    stats::SW_RX_INQUEUE.fetch_add(1, Relaxed);
    let kick = *KICK.lock();
    if let Some(f) = kick {
        f();
    }
}

fn fill_random(b: &mut [u8]) {
    for c in b.chunks_mut(4) {
        c.copy_from_slice(&crate::random().to_ne_bytes()[..c.len()]);
    }
}

impl Inner {
    /// `sb_tlscc` and `sb_tlsdcc`: bytes held that cannot be read yet.
    pub(crate) fn tls_pending(&self) -> u32 {
        self.tls_rx.as_ref().map_or(0, |r| {
            u32::try_from(r.mtls.len())
                .unwrap_or(u32::MAX)
                .saturating_add(r.dcc)
        })
    }

    /// `sbflush` of the receive buffer.
    pub(crate) fn rcv_flush(&mut self) {
        self.stream = Chain::default();
        self.rcv.cc = 0;
        self.rcv.mbcnt = 0;
        if let Some(r) = self.tls_rx.as_mut() {
            r.mtls = Chain::default();
            r.dcc = 0;
            r.records.clear();
        }
    }

    fn rcv_recount(&mut self) {
        let held = self.tls_rx.as_ref().map_or(0, |r| r.mtls.cost());
        self.rcv.mbcnt = self.stream.cost().saturating_add(held);
    }

    /// `sbappend_ktls_rx`: `data` from TCP, ciphertext. `true` if the
    /// socket goes on the queue.
    pub(crate) fn tls_append(&mut self, data: Chain) -> bool {
        if let Some(r) = self.tls_rx.as_mut() {
            r.mtls.append(data);
        }
        self.rcv_recount();
        self.tls_check_rx()
    }

    /// `ktls_check_rx`: whether a whole record is there to open; at the
    /// end of the stream, a part of one is an error.
    pub(crate) fn tls_check_rx(&mut self) -> bool {
        let cant = self.rcv.cant;
        let Some(r) = self.tls_rx.as_mut() else {
            return false;
        };
        if r.running {
            return false;
        }
        let cc = r.mtls.len();
        if cc < ktls::HDR {
            if cant && cc != 0 {
                self.error = Some(Errno::EMSGSIZE);
            }
            return false;
        }
        let mut h = [0; ktls::HDR];
        r.mtls.copy_out(0, &mut h);
        if cc < ktls::HDR + usize::from(u16::from_be_bytes([h[3], h[4]])) {
            if cant {
                self.error = Some(Errno::EMSGSIZE);
            }
            return false;
        }
        r.running = true;
        true
    }

    /// A thread is done with the socket, in the same hold of the lock as
    /// its last look: what comes after that puts it on the queue again.
    fn tls_idle(&mut self) {
        let cant = self.rcv.cant;
        if let Some(r) = self.tls_rx.as_mut() {
            r.running = false;
            if cant && !r.mtls.is_empty() {
                self.error = Some(Errno::EMSGSIZE);
            }
        }
    }

    /// Whether a record waits to be read, even one with no bytes.
    pub(crate) fn tls_record_ready(&self) -> bool {
        self.tls_rx.as_ref().is_some_and(|r| !r.records.is_empty())
    }
}

impl Socket {
    fn tls_checks(&self, en: &Enable<'_>) -> Result<(), Errno> {
        if !stats::ENABLE.load(Relaxed) {
            return Err(Errno::ENOTSUP);
        }
        stats::ENABLE_CALLS.fetch_add(1, Relaxed);
        if Session::is_cbc(en) && !stats::CBC_ENABLE.load(Relaxed) {
            return Err(Errno::ENOTSUP);
        }
        Ok(())
    }

    /// `TCP_TXTLS_ENABLE` (`ktls_enable_tx`).
    ///
    /// # Errors
    ///
    /// `ENOTSUP` turned off by sysctl; `EALREADY` enabled before; `EINVAL`
    /// or `EPROTONOSUPPORT` for the suite; `ENOTCONN` listening; `EINVAL`
    /// spliced; `ECONNRESET` dropped.
    pub fn tls_enable_tx(&self, en: &Enable<'_>) -> Result<(), Errno> {
        self.tls_checks(en)?;
        if self.inner.lock().tls_tx.is_some() {
            return Err(Errno::EALREADY);
        }
        let tx = Tx::new(en, &mut fill_random)?;
        let layout = tx.layout();
        let mut slot = self.tls_tx.lock();
        let mut i = self.inner.lock();
        check_usable(&i)?;
        if i.tls_tx.is_some() {
            return Err(Errno::EALREADY);
        }
        if i.splice_back.is_some() {
            return Err(Errno::EINVAL);
        }
        i.tls_tx = Some(layout);
        *slot = Some(tx);
        stats::OFFLOAD_TOTAL.fetch_add(1, Relaxed);
        Ok(())
    }

    /// `TCP_RXTLS_ENABLE` (`ktls_enable_rx`). What is already in the
    /// buffer is taken to be records.
    ///
    /// # Errors
    ///
    /// As [`Socket::tls_enable_tx`], but `EALREADY` spliced.
    pub fn tls_enable_rx(self: &Arc<Self>, en: &Enable<'_>) -> Result<(), Errno> {
        self.tls_checks(en)?;
        if self.inner.lock().tls_rx.is_some() {
            return Err(Errno::EALREADY);
        }
        let session = Session::new(en, Direction::Rx)?;
        let queue = {
            let mut i = self.inner.lock();
            check_usable(&i)?;
            if i.tls_rx.is_some() || i.splice.is_some() {
                return Err(Errno::EALREADY);
            }
            // `sb_mark_notready`.
            let mtls = core::mem::take(&mut i.stream);
            i.rcv.cc = 0;
            i.tls_rx = Some(Box::new(Rx {
                session: Arc::new(session),
                seqno: u64::from_be_bytes(en.rec_seq),
                mtls,
                dcc: 0,
                running: false,
                records: VecDeque::new(),
            }));
            i.tls_check_rx()
        };
        if queue {
            enqueue(self);
        }
        stats::OFFLOAD_TOTAL.fetch_add(1, Relaxed);
        Ok(())
    }

    /// `ktls_get_tx_mode` and `ktls_get_rx_mode`.
    pub(crate) fn tls_mode(i: &Inner, name: i32) -> Result<i32, Errno> {
        if i.tcp.as_ref().is_some_and(|t| t.listen.is_some()) {
            return Err(Errno::EINVAL);
        }
        let on = if name == tcp::TCP_TXTLS_MODE {
            i.tls_tx.is_some()
        } else {
            i.tls_rx.is_some()
        };
        Ok(if on {
            tcp::TCP_TLS_MODE_SW
        } else {
            tcp::TCP_TLS_MODE_NONE
        })
    }

    /// `ktls_set_tx_mode`: every session here is in software, and no
    /// interface takes one.
    pub(crate) fn tls_set_tx_mode(i: &Inner, mode: i32) -> Result<(), Errno> {
        if i.tcp.as_ref().is_some_and(|t| t.listen.is_some()) {
            return Err(Errno::EINVAL);
        }
        match mode {
            tcp::TCP_TLS_MODE_SW => Ok(()),
            tcp::TCP_TLS_MODE_IFNET if i.tls_tx.is_none() => Ok(()),
            tcp::TCP_TLS_MODE_IFNET => Err(Errno::ENXIO),
            _ => Err(Errno::EINVAL),
        }
    }

    /// A sending session's record sizes.
    pub fn tls_tx_layout(&self) -> Option<Layout> {
        self.inner.lock().tls_tx
    }

    /// `ktls_frame` and the encryption of `data`, in records of at most
    /// [`MAX_FRAME`], onto the send buffer, whatever room it has; nothing
    /// at all makes one empty record.
    pub(crate) fn tls_frame(&self, data: &[u8], rtype: u8, layout: Layout) -> Result<(), Errno> {
        let mut off = 0;
        loop {
            let n = (data.len() - off).min(MAX_FRAME);
            let mut rec = vec![0; layout.record_len(n)];
            rec[layout.hlen..layout.hlen + n].copy_from_slice(&data[off..off + n]);
            let mut slot = self.tls_tx.lock();
            let tx = slot.as_mut().ok_or(Errno::EINVAL)?;
            let mark = tx.mark();
            tx.seal(rtype, &mut rec, n, &mut fill_random);
            let Ok(c) = Chain::from_slice(&rec) else {
                // Nothing went: the next try uses the same number.
                tx.reset(mark);
                return Err(Errno::ENOBUFS);
            };
            let mut i = self.inner.lock();
            if i.tcp.as_ref().is_some_and(|t| t.dropped) {
                return Err(Errno::ECONNRESET);
            }
            i.snd.cc = i
                .snd
                .cc
                .saturating_add(u32::try_from(c.len()).unwrap_or(u32::MAX));
            i.sndq.append(c);
            i.snd.mbcnt = i.sndq.cost();
            drop(i);
            drop(slot);
            off += n;
            if off == data.len() {
                return Ok(());
            }
        }
    }

    /// `ktls_decrypt`: opens the whole records there are.
    pub fn ktls_decrypt(self: &Arc<Self>) {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            // The next whole record, out of the buffer.
            let (session, seq) = {
                let mut i = self.inner.lock();
                let Some(r) = i.tls_rx.as_mut() else {
                    return;
                };
                let hlen = r.session.layout().hlen;
                if r.mtls.len() < hlen {
                    i.tls_idle();
                    break;
                }
                let mut h = [0; 32];
                r.mtls.copy_out(0, &mut h[..hlen]);
                let len = match r.session.check_header(&h[..hlen]) {
                    Ok(len) => len,
                    Err(e) => {
                        // Out of step for good: `running` stays, nothing
                        // more is opened.
                        stats::CORRUPTED_RECORDS.fetch_add(1, Relaxed);
                        drop(i);
                        self.tls_drop(e);
                        return;
                    }
                };
                if r.mtls.len() < len {
                    i.tls_idle();
                    break;
                }
                buf.resize(len, 0);
                r.mtls.copy_out(0, &mut buf);
                r.mtls.trim_front(len);
                r.dcc = u32::try_from(len).unwrap_or(u32::MAX);
                let seq = r.seqno;
                r.seqno = seq.wrapping_add(1);
                let session = Arc::clone(&r.session);
                i.rcv_recount();
                (session, seq)
            };
            let opened = session.open(&mut buf, seq);
            let plain = opened.and_then(|o| {
                let c = Chain::from_slice(&buf[o.start..o.start + o.len]);
                c.map(|c| (o, c)).map_err(|_| Errno::ENOBUFS)
            });
            {
                let mut i = self.inner.lock();
                let Some(r) = i.tls_rx.as_mut() else {
                    return;
                };
                if r.dcc == 0 {
                    // A flush took it while it was out.
                    i.tls_idle();
                    break;
                }
                r.dcc = 0;
                match plain {
                    Ok((o, c)) => {
                        let n = u16::try_from(o.len).unwrap_or(u16::MAX).to_be_bytes();
                        r.records.push_back(Record {
                            ctl: Some([o.rtype, buf[1], buf[2], 0, n[0], n[1]]),
                            left: o.len,
                        });
                        i.stream.append(c);
                        i.rcv.cc += u32::try_from(o.len).unwrap_or(u32::MAX);
                        i.rcv_recount();
                    }
                    Err(e) => {
                        // This record is lost; later ones may be read.
                        stats::FAILED_CRYPTO.fetch_add(1, Relaxed);
                        i.error = Some(if matches!(e, Errno::EMSGSIZE | Errno::ENOBUFS) {
                            e
                        } else {
                            Errno::EBADMSG
                        });
                    }
                }
            }
            self.wake();
        }
        self.wake();
    }

    /// `soreceive_generic` on a socket with a receiving session, one look:
    /// up to `max` bytes of the next record, its `TLS_GET_RECORD` if not
    /// taken yet, and `MSG_EOR` at its end. With `appdata`
    /// (`MSG_TLSAPPDATA`), `ENXIO` before an alert, which stays.
    ///
    /// # Errors
    ///
    /// As `recv_stream`, a pending error coming after what was opened.
    pub(crate) fn tls_recv(
        i: &mut Inner,
        max: usize,
        peek: bool,
        appdata: bool,
    ) -> Result<Received, Errno> {
        let mut out = Received::default();
        let held = i.tls_pending();
        let cant = i.rcv.cant;
        let error = i.error;
        let unconnected = i.state
            & (ss::ISCONNECTING | ss::ISCONNECTED | ss::ISDISCONNECTING | ss::ISDISCONNECTED)
            == 0;
        let Some(r) = i.tls_rx.as_mut() else {
            return Err(Errno::EINVAL);
        };
        let Some(rec) = r.records.front_mut() else {
            if let Some(e) = error {
                if !peek {
                    i.error = None;
                }
                return Err(e);
            }
            if cant && held == 0 {
                return Ok(out);
            }
            if unconnected {
                return Err(Errno::ENOTCONN);
            }
            if max == 0 {
                return Ok(out);
            }
            return Err(Errno::EAGAIN);
        };
        if appdata && rec.ctl.is_some_and(|c| c[0] == TLS_RLTYPE_ALERT) {
            return Err(Errno::ENXIO);
        }
        out.tls = rec.ctl;
        let n = max.min(rec.left);
        if n > 0 && n == rec.left {
            out.flags |= MSG_EOR;
        }
        if !peek {
            rec.ctl = None;
            rec.left -= n;
            if rec.left == 0 {
                r.records.pop_front();
            }
        }
        out.data = vec![0; n];
        i.stream.copy_out(0, &mut out.data);
        if !peek {
            i.stream.trim_front(n);
            i.rcv.cc -= u32::try_from(n).unwrap_or(0);
            i.rcv_recount();
        }
        Ok(out)
    }

    /// Whether a receive of this socket goes by records.
    pub(crate) fn tls_rx_on(i: &Inner) -> bool {
        i.tls_rx.is_some()
    }

    /// `ktls_drop`: the connection cannot go on, for `e`.
    fn tls_drop(self: &Arc<Self>, e: Errno) {
        let live = self.inner.lock().tcp.as_ref().is_some_and(|t| !t.dropped);
        if live {
            self.tcp_drop_for(e);
        } else {
            self.inner.lock().error = Some(e);
            self.wake();
        }
    }
}

/// What `SOCK_IO_SEND_LOCK` and `tcp_ctloutput` refuse.
fn check_usable(i: &Inner) -> Result<(), Errno> {
    match i.tcp.as_ref() {
        Some(t) if t.listen.is_some() => Err(Errno::ENOTCONN),
        Some(t) if t.dropped => Err(Errno::ECONNRESET),
        Some(_) => Ok(()),
        None => Err(Errno::EINVAL),
    }
}
