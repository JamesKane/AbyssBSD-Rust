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
use drm::DrmDriver as _;

use crate::amd::{Amd, Bar5};

/// How long the watcher sleeps when no interrupt comes: it looks at the
/// lines anyway, so a lost interrupt costs a second, not a missed monitor.
const PERIOD_NS: u64 = 1_000_000_000;
/// How long a changed sense must hold before it counts (the filter's job in
/// the hardware too; this covers a connector that bounces as it is pushed in).
const SETTLE_NS: u64 = 500_000_000;

/// Starts the watcher for `amd`, if the line reads "connected" now: a
/// board whose line is not wired to the monitor reads 0 for ever, and then
/// the connector stays connected as it always was. The first line's
/// interrupt (client 4, source 9) wakes it at a change; every second it
/// looks anyway. The other lines are only read, and a change in one is
/// logged: no other connector is wired on the boards we have.
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
    let irq = Arc::clone(&amd.irq);
    let queue = irq.hpd_queue();
    // The interrupt on, for the change from what the line senses now.
    irq.hpd_arm(0);
    let spawned = sched::spawn("amd-hpd", sched::Priority::KERNEL, move |s| {
        let sleep = bus::manager();
        let mut state = true;
        let mut others = [false; hpd::LINES];
        loop {
            // An interrupt, or a second.
            let _ = sleep.wait_timeout(s, &queue, &mut || irq.hpd_pending(), PERIOD_NS);
            let events = irq.take_hpd_events();
            let Some(a) = me.upgrade() else {
                return;
            };
            if let Some(read) = a.hpd_sense()
                && read != state
            {
                // Hold: the change counts if the line still says so after a
                // moment.
                drop(a);
                let _ = sleep.wait_timeout(s, &queue, &mut || false, SETTLE_NS);
                let Some(a) = me.upgrade() else {
                    return;
                };
                if a.hpd_sense() == Some(read) {
                    state = read;
                    a.log(format_args!(
                        "hot-plug: the line changed ({})",
                        if events & 1 != 0 {
                            "interrupt"
                        } else {
                            "found by the poll"
                        }
                    ));
                    a.hotplug(s, read);
                }
                // An interrupt that came during the wait is not another event.
                let _ = irq.take_hpd_events();
            } else {
                drop(a);
            }
            // The other lines: read, and a change logged.
            let Some(a) = me.upgrade() else {
                return;
            };
            for (n, was) in others.iter_mut().enumerate().skip(1) {
                if let Some(now) = a.hpd_line_sense(n)
                    && now != *was
                {
                    *was = now;
                    a.log(format_args!(
                        "hot-plug: line {n} {}",
                        if now {
                            "senses a display"
                        } else {
                            "senses none"
                        }
                    ));
                }
            }
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

    /// Line `n`'s sense, `None` if the device does not answer.
    pub(crate) fn hpd_line_sense(&self, n: usize) -> Option<bool> {
        let gpu = self.gpu().ok()?;
        let raw = Bar5(&gpu).read32(hpd::int_status(n));
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
        // The notification drm-kmod sends (drm_sysfs_connector_hotplug_event):
        // libudev-devd turns it into a "change" event with HOTPLUG=1, which
        // is what wlroots listens for.
        let unit = self.dri_unit.load(Ordering::Acquire);
        let id = self
            .kms
            .lock()
            .as_ref()
            .and_then(|(card, _)| card.kms().and_then(|k| k.connector_ids().first().copied()));
        if let (true, Some(id)) = (unit >= 0, id) {
            let data = format!("cdev=dri/card{unit} connector={id}");
            bus::manager().notify("DRM", "CONNECTOR", "HOTPLUG", Some(&data));
        }
    }
}
