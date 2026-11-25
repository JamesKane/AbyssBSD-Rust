// SPDX-License-Identifier: BSD-2-Clause
//! Hot-plug on the first connector (docs/GRAPHICS.md §16.2): a kernel thread
//! reads the hot-plug line's sense (`amdgpu::hpd`) every quarter second, and
//! when it has changed and stayed changed, reads the new monitor's EDID (or
//! forgets the old one), updates what the connector reports, and tells user
//! space through the device control channel.

use alloc::format;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::Ordering;

use amdgpu::hpd;
use amdgpu::regs::Window;

use crate::amd::{Amd, Bar5};

/// How often the line is read.
const PERIOD_NS: u64 = 250_000_000;
/// Reads that must agree before a change counts (one second).
const SETTLE: u8 = 4;

/// Starts the watcher for `amd`, if the line reads "connected" now: a
/// board whose line is not wired to the monitor reads 0 for ever, and then
/// the connector stays connected as it always was.
pub(crate) fn start(amd: &Arc<Amd>) {
    let Some(sensed) = amd.hpd_sense() else {
        return;
    };
    if !sensed {
        amd.log(format_args!(
            "hot-plug: the line reads disconnected with a display running; not watched"
        ));
        return;
    }
    let me: Weak<Amd> = Arc::downgrade(amd);
    let spawned = sched::spawn("amd-hpd", sched::Priority::KERNEL, move |s| {
        let queue = sched::WaitQueue::new();
        let mut settle = hpd::Debounce::new(true, SETTLE);
        loop {
            {
                let Some(a) = me.upgrade() else {
                    return;
                };
                if let Some(now) = a.hpd_sense().and_then(|read| settle.update(read)) {
                    a.hotplug(s, now);
                }
            }
            let _ = bus::manager().wait_timeout(s, &queue, &mut || false, PERIOD_NS);
        }
    });
    if spawned.is_err() {
        amd.log(format_args!("hot-plug: the watcher thread did not start"));
    }
}

impl Amd {
    /// The hot-plug line's sense, `None` if the device does not answer.
    pub(crate) fn hpd_sense(&self) -> Option<bool> {
        let gpu = self.gpu().ok()?;
        let raw = Bar5(&gpu).read32(hpd::HPD0_INT_STATUS);
        (raw != u32::MAX).then(|| hpd::sensed(raw))
    }

    /// Whether the connector reports a monitor.
    pub(crate) fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// The line settled at `now`: a monitor came or went.
    fn hotplug(&self, s: &sched::Sleepable, now: bool) {
        // The old monitor's EDID is forgotten either way; a new one is read
        // (a monitor with no EDID gets the whole table).
        *self.sink.lock() = None;
        if now {
            self.read_sink();
            // The link is trained again for the monitor now there.
            let r = self.relink_now(s);
            self.log(format_args!("hot-plug: link trained again: {r:?}"));
        }
        self.connected.store(now, Ordering::Release);
        let status = if now { "connected" } else { "disconnected" };
        self.log(format_args!("hot-plug: monitor {status}"));
        let unit = self.dri_unit.load(Ordering::Acquire);
        if unit >= 0 {
            let data = format!("connector=HDMI-A-1 status={status}");
            bus::manager().notify("DRM", &format!("card{unit}"), "HOTPLUG", Some(&data));
        }
    }
}
