// SPDX-License-Identifier: BSD-2-Clause
//! Socket buffers as FreeBSD counts them: data in mbuf chains
//! (`sys/mbuf`), each mbuf charged its header and cluster against the
//! buffer's memory limit (`sb_mbmax`), so a buffer fills when FreeBSD's
//! would.
//!
//! Behavior reference: sys/kern/kern_mbuf.c (`mc_get`),
//! sys/kern/uipc_usrreq.c (`uipc_stream_sbspace`).

use alloc::collections::VecDeque;

use mbuf::Chain;

use crate::control::Control;

pub(crate) use mbuf::{MCLBYTES, MLEN, MSIZE};

/// What a stream or seqpacket socket's receive buffer holds, in order.
#[derive(Debug)]
pub(crate) enum Item {
    /// A control message, with the length it is counted as (`m_len`).
    Control(Control, u32),
    /// One send's data; only its last mbuf may end a record.
    Data(Chain),
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
    pub(crate) fn push_data(&mut self, c: Chain) {
        if c.is_empty() {
            return;
        }
        self.cc += len32(c.len());
        self.mbcnt += c.cost();
        self.items.push_back(Item::Data(c));
    }

    /// Empties it, handing back what it held (`unp_dispose`).
    pub(crate) fn take_all(&mut self) -> VecDeque<Item> {
        self.cc = 0;
        self.ctl = 0;
        self.mbcnt = 0;
        core::mem::take(&mut self.items)
    }
}

/// A buffer's byte count as its `u32` fields hold it.
pub(crate) fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
