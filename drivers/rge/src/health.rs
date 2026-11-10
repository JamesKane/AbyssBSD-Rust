// SPDX-License-Identifier: BSD-2-Clause
//! What the once-a-second tick (`rge_tick`) decides: the transmit
//! watchdog's countdown (`sc_watchdog`), a receive ring that has frames
//! waiting with nothing taking them, and a controller that no longer
//! answers.
//!
//! - The watchdog arms when a frame is queued on an idle ring, rearms
//!   whenever a completion is reaped with frames still out, and disarms
//!   when the ring empties. A tick that finds it armed counts it down; at
//!   zero, [`TX_TIMEOUT_TICKS`] seconds have passed with frames on the
//!   controller and none completed, and the driver reinitializes the
//!   chip (`rge_init_locked`).
//! - A returned receive descriptor that two ticks in a row find at the
//!   head of the ring, with no frame taken in between, waited a second
//!   for an interrupt that never came.
//! - Register reads of all ones mean the device has left the bus (a PCIe
//!   link down, a surprise removal): the registers never read so.
//!
//! Behavior reference: sys/dev/rge/if_rge.c (`rge_tick`, `rge_txeof`,
//! `rge_tx_task`).

/// Ticks (seconds) without a transmit completion before the watchdog
/// fires: `rge_txeof`'s and `rge_tx_task`'s `sc_watchdog = 5`.
pub const TX_TIMEOUT_TICKS: u8 = 5;

/// What the watchdog prints when it fires, after the device name
/// (`RGE_PRINT_ERROR(sc, "TX timeout (watchdog)\n")`).
pub const MSG_TX_TIMEOUT: &str = "[ERR] TX timeout (watchdog)";
/// What a system error interrupt prints before the reinitialization.
pub const MSG_SYSTEM_ERR: &str = "[ERR] system error, reinitializing";
/// What a receive stall prints (with the count so far).
pub const MSG_RX_STALL: &str = "[ERR] RX stall: frames waiting with no interrupt";
/// What a device that left the bus prints, once.
pub const MSG_GONE: &str = "[ERR] device not responding (registers read all ones), stopped";
/// What a failed reinitialization prints.
pub const MSG_INIT_FAILED: &str = "[ERR] reinitialization failed";

/// The transmit watchdog (`sc_watchdog`): seconds left before a stalled
/// transmitter is declared, or 0 when nothing is out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxWatchdog {
    timer: u8,
}

impl TxWatchdog {
    /// A frame was handed to the controller: the countdown starts unless
    /// it runs already, so a stream of new frames cannot hold off a
    /// stalled ring.
    pub fn queued(&mut self) {
        if self.timer == 0 {
            self.timer = TX_TIMEOUT_TICKS;
        }
    }

    /// Completions were reaped (`progress`: at least one frame), and
    /// `pending` frames remain on the controller (`rge_txeof`'s end).
    pub fn reaped(&mut self, progress: bool, pending: bool) {
        if !pending {
            self.timer = 0;
        } else if progress {
            self.timer = TX_TIMEOUT_TICKS;
        }
    }

    /// One second passed (`rge_tick`); whether the watchdog fired. It
    /// disarms when it fires.
    pub fn tick(&mut self) -> bool {
        if self.timer == 0 {
            return false;
        }
        self.timer -= 1;
        self.timer == 0
    }

    /// Disarms (`rge_stop_locked`).
    pub fn clear(&mut self) {
        self.timer = 0;
    }

    /// Whether frames are out and the countdown runs.
    #[must_use]
    pub fn armed(&self) -> bool {
        self.timer != 0
    }
}

/// The receive stall check: the head of the ring and the frames taken
/// so far, as the last tick saw them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxWatch {
    last: Option<(usize, u64)>,
    stalls: u64,
}

impl RxWatch {
    /// One second passed: `waiting` is the head descriptor if the
    /// controller has returned it, `taken` the descriptors taken so far.
    /// Some(the stall count) if it is the same returned descriptor as at
    /// the last tick, with nothing taken since.
    pub fn check(&mut self, waiting: Option<usize>, taken: u64) -> Option<u64> {
        let now = waiting.map(|d| (d, taken));
        let stalled = now.is_some() && now == self.last;
        self.last = now;
        if !stalled {
            return None;
        }
        // Taken now (by the caller), so the next tick starts afresh.
        self.last = None;
        self.stalls += 1;
        Some(self.stalls)
    }

    /// Forgets the last tick's view (the rings were reset).
    pub fn clear(&mut self) {
        self.last = None;
    }
}

/// Whether the `n`th occurrence of something is reported: the 1st, 2nd,
/// 4th, 8th, ..., so a recurring fault is seen without flooding.
#[must_use]
pub const fn report(n: u64) -> bool {
    n.is_power_of_two()
}

/// Whether a 32-bit register read came from the device: one that has
/// left the bus completes reads with all ones.
#[must_use]
pub const fn responding(v: u32) -> bool {
    v != u32::MAX
}
