// SPDX-License-Identifier: BSD-2-Clause
//! Socket buffers as FreeBSD counts them: data in mbufs, each charged its
//! header and cluster against the buffer's memory limit (`sb_mbmax`), so
//! a buffer fills when FreeBSD's would.
//!
//! Behavior reference: sys/kern/kern_mbuf.c (`mc_get`),
//! sys/kern/uipc_usrreq.c (`uipc_stream_sbspace`).

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::control::Control;

/// `MSIZE`, `MCLBYTES`, `MJUMPAGESIZE`, `MLEN`, `MINCLSIZE`.
pub(crate) const MSIZE: u32 = 256;
pub(crate) const MCLBYTES: u32 = 2048;
pub(crate) const MJUMPAGESIZE: u32 = 4096;
pub(crate) const MLEN: u32 = 224;
pub(crate) const MINCLSIZE: u32 = 169;

/// One mbuf of data.
#[derive(Debug)]
pub struct Mbuf {
    pub(crate) data: Vec<u8>,
    /// Its cluster's size, 0 for none.
    pub(crate) ext: u32,
    /// It ends a record (`M_EOR`).
    pub(crate) eor: bool,
}

impl Mbuf {
    /// What it costs the buffer's memory count.
    pub(crate) fn cost(&self) -> u32 {
        MSIZE + self.ext
    }

    pub(crate) fn len(&self) -> u32 {
        u32::try_from(self.data.len()).unwrap_or(u32::MAX)
    }
}

/// `mc_get`: the sizes of the mbufs that hold `len` bytes (each with its
/// cluster size).
pub(crate) fn carve(len: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut left = len;
    loop {
        let (room, ext) = if left > MCLBYTES {
            (MJUMPAGESIZE, MJUMPAGESIZE)
        } else if left >= MINCLSIZE {
            (MCLBYTES, MCLBYTES)
        } else {
            (MLEN, 0)
        };
        let take = left.min(room);
        out.push((take, ext));
        left -= take;
        if left == 0 {
            return out;
        }
    }
}

/// `bytes` in mbufs as `mc_uiotomc` would put them, the last marked to end
/// a record if `eor`.
pub fn chain(bytes: &[u8], eor: bool) -> VecDeque<Mbuf> {
    let mut out = VecDeque::new();
    // Nothing for nothing (`mc_uiotomc`).
    if bytes.is_empty() {
        return out;
    }
    let mut at = 0usize;
    for (n, ext) in carve(u32::try_from(bytes.len()).unwrap_or(u32::MAX)) {
        let n = n as usize;
        out.push_back(Mbuf {
            data: bytes[at..at + n].to_vec(),
            ext,
            eor: false,
        });
        at += n;
    }
    if eor && let Some(last) = out.back_mut() {
        last.eor = true;
    }
    out
}

/// [`chain`] for `len` bytes that `fill` copies straight into each mbuf
/// (`mc_uiotomc`'s `uiomove`), rather than from a buffer of their own.
/// `fill` returns how much it copied; a short copy ends the chain there.
///
/// # Errors
///
/// What `fill` returns.
pub fn chain_from<E>(
    len: usize,
    eor: bool,
    mut fill: impl FnMut(&mut [u8]) -> Result<usize, E>,
) -> Result<VecDeque<Mbuf>, E> {
    let mut out = VecDeque::new();
    if len == 0 {
        return Ok(out);
    }
    for (n, ext) in carve(u32::try_from(len).unwrap_or(u32::MAX)) {
        let mut data = alloc::vec![0u8; n as usize];
        let got = fill(&mut data)?;
        let short = got < data.len();
        data.truncate(got);
        if !data.is_empty() {
            out.push_back(Mbuf {
                data,
                ext,
                eor: false,
            });
        }
        if short {
            return Ok(out);
        }
    }
    if eor && let Some(last) = out.back_mut() {
        last.eor = true;
    }
    Ok(out)
}

/// Splits `c` after `len` bytes (`mc_split`): the rest is returned. An
/// mbuf cut in two keeps its cluster in both halves (both charged for it);
/// a plain one's tail is copied to a new mbuf.
pub(crate) fn split(c: &mut VecDeque<Mbuf>, len: u32) -> VecDeque<Mbuf> {
    let mut have = 0u32;
    let mut i = 0;
    while i < c.len() && have + c[i].len() <= len {
        have += c[i].len();
        i += 1;
    }
    let mut rest = c.split_off(i);
    if have < len
        && let Some(m) = rest.front_mut()
    {
        let cut = (len - have) as usize;
        let tail = m.data.split_off(cut);
        let head = Mbuf {
            data: core::mem::replace(&mut m.data, tail),
            ext: m.ext,
            eor: false,
        };
        c.push_back(head);
    }
    rest
}

/// What a stream or seqpacket socket's receive buffer holds, in order.
#[derive(Debug)]
pub(crate) enum Item {
    /// A control message, with the length it is counted as (`m_len`).
    Control(Control, u32),
    Data(Mbuf),
}

/// `sb_state` bits (`SBS_*`).
pub(crate) const SBS_CANTSENDMORE: u16 = 0x0010;
pub(crate) const SBS_CANTRCVMORE: u16 = 0x0020;

/// A socket buffer's limits and counts (FreeBSD's `struct sockbuf`
/// fields).
#[derive(Debug)]
pub(crate) struct Sockbuf {
    pub(crate) hiwat: u32,
    pub(crate) lowat: u32,
    pub(crate) mbmax: u32,
    /// Data bytes (`sb_ccc`, `sb_acc`), control bytes (`sb_ctl`), memory
    /// (`sb_mbcnt`).
    pub(crate) cc: u32,
    pub(crate) ctl: u32,
    pub(crate) mbcnt: u32,
    /// `SBS_CANTRCVMORE` (receive buffer), `SBS_CANTSENDMORE` (send).
    pub(crate) cant: bool,
    /// A stream receive buffer's records.
    pub(crate) items: VecDeque<Item>,
}

/// `sb_efficiency`, `sb_max` (`kern.ipc.maxsockbuf`).
pub(crate) const SB_EFFICIENCY: u32 = 8;
pub(crate) const SB_MAX: u32 = 8 * 1024 * 1024;
/// `sb_max_adj`: the most a buffer may be reserved.
/// (8 MiB * 2048 / 2304, which fits.)
#[allow(clippy::cast_possible_truncation, reason = "the quotient fits")]
pub(crate) const SB_MAX_ADJ: u32 =
    (SB_MAX as u64 * MCLBYTES as u64 / (MSIZE + MCLBYTES) as u64) as u32;

impl Sockbuf {
    pub(crate) const fn new() -> Self {
        Self {
            hiwat: 0,
            lowat: 0,
            mbmax: 0,
            cc: 0,
            ctl: 0,
            mbcnt: 0,
            cant: false,
            items: VecDeque::new(),
        }
    }

    /// `sbreserve_locked`: false if `cc` is more than may be had.
    pub(crate) fn reserve(&mut self, cc: u32) -> bool {
        if cc > SB_MAX_ADJ {
            return false;
        }
        self.hiwat = cc;
        self.mbmax = cc.saturating_mul(SB_EFFICIENCY).min(SB_MAX);
        if self.lowat > self.hiwat {
            self.lowat = self.hiwat;
        }
        true
    }

    /// `uipc_stream_sbspace`: room by bytes or memory, whichever ends
    /// first.
    pub(crate) fn stream_space(&self) -> u32 {
        let Some(space) = self.hiwat.checked_sub(self.cc + self.ctl) else {
            return 0;
        };
        let Some(mbspace) = self.mbmax.checked_sub(self.mbcnt) else {
            return 0;
        };
        space.min(mbspace)
    }

    /// Appends a control message.
    pub(crate) fn push_control(&mut self, c: Control) {
        let len = c.len();
        self.ctl += len;
        self.mbcnt += c.cost();
        self.items.push_back(Item::Control(c, len));
    }

    /// Appends data.
    pub(crate) fn push_data(&mut self, c: VecDeque<Mbuf>) {
        for m in c {
            self.cc += m.len();
            self.mbcnt += m.cost();
            self.items.push_back(Item::Data(m));
        }
    }

    /// Empties it, handing back what it held (`unp_dispose`).
    pub(crate) fn take_all(&mut self) -> VecDeque<Item> {
        self.cc = 0;
        self.ctl = 0;
        self.mbcnt = 0;
        core::mem::take(&mut self.items)
    }
}
