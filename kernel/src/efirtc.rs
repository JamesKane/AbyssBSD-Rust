// SPDX-License-Identifier: BSD-2-Clause
//! `efirtc`: the time of day from UEFI's `GetTime`, a child of `nexus0`
//! wherever runtime services are available, as on FreeBSD.
//!
//! It registers with the resolution `GetTime`'s capabilities give and
//! takes the time to set unadjusted (`CLOCKF_SETTIME_NO_ADJ`): it adds
//! half its resolution itself, unless the firmware zeroes the part of the
//! second below the resolution when it sets the time, in which case the
//! writes come just before the top of a second instead.
//!
//! Behavior reference: sys/dev/efidev/efirtc.c.

use alloc::string::String;
use alloc::sync::Arc;

use abi::errno::Errno;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use clock::{DateTime, RealTimeClock, Timespec};
use sched::Sleepable;

/// Microseconds per second.
const US_PER_S: u32 = 1_000_000;

/// The firmware's clock.
#[derive(Debug)]
pub(crate) struct EfiClock {
    name: String,
    /// Half its resolution, in nanoseconds (`efirtc_resadj`).
    resadj_ns: u32,
    /// `SetTime` zeroes the part of the second below the resolution
    /// (`efirtc_zeroes_subseconds`).
    zeroes_subseconds: bool,
}

impl RealTimeClock for EfiClock {
    fn name(&self) -> &str {
        &self.name
    }

    fn gettime(&self, _: &Sleepable) -> Result<DateTime, Errno> {
        let t = crate::firmware::get_time()?;
        // Local time (2047) is taken as UTC, as FreeBSD's efirtc does
        // without a timezone hint; an offset is applied.
        let mut d = DateTime {
            year: t.year,
            month: t.month,
            day: t.day,
            hour: t.hour,
            minute: t.minute,
            second: t.second,
            nanosecond: t.nanosecond,
        };
        clock::dbgprint(&self.name, clock::rtc::DBG_READ, &d);
        if t.time_zone != 2047 && t.time_zone != 0 {
            let secs = d
                .unix_seconds()
                .and_then(|s| s.checked_add_signed(i64::from(t.time_zone) * 60))
                .ok_or(Errno::EINVAL)?;
            d = DateTime::from_unix(secs, t.nanosecond);
        }
        if d.valid() { Ok(d) } else { Err(Errno::EINVAL) }
    }

    fn settime(&self, _: &Sleepable, ts: Timespec) -> Result<(), Errno> {
        let mut ts = ts;
        ts.sec = ts.sec.saturating_sub(clock::utc_offset());
        if !self.zeroes_subseconds {
            ts = ts.plus_ns(self.resadj_ns);
        }
        let d = ts.to_datetime().ok_or(Errno::EINVAL)?;
        clock::dbgprint(&self.name, clock::rtc::DBG_WRITE, &d);
        // The rest zeroed, as `efirtc_settime` clears its `efi_tm`: a time
        // zone of 0 (UTC) and no daylight saving.
        let t = efi::raw::Time {
            year: d.year,
            month: d.month,
            day: d.day,
            hour: d.hour,
            minute: d.minute,
            second: d.second,
            nanosecond: d.nanosecond,
            ..efi::raw::Time::default()
        };
        crate::firmware::set_time(&t)
    }
}

/// An `efirtcN` instance.
#[derive(Debug)]
pub(crate) struct EfiRtc {
    clock: Arc<EfiClock>,
    resolution: u32,
}

impl Driver for EfiRtc {
    const NAME: &'static str = "efirtc";
    const IDS: Ids = Ids::Nexus(&["efirtc"]);

    fn describe(_: &Ident) -> String {
        "EFI Realtime Clock".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let cap = crate::firmware::get_time_capabilities()
            .map_err(|_| AttachError::Device("cannot get EFI time capabilities"))?;
        // The resolution in Hz as a tick in microseconds: 0 is taken as
        // 1 Hz, more than a million as 1 us.
        let resolution = match cap.resolution {
            0 => US_PER_S,
            r if r > US_PER_S => 1,
            r => US_PER_S / r,
        };
        Ok(Self {
            clock: Arc::new(EfiClock {
                name: alloc::format!("efirtc{}", cx.unit()),
                resadj_ns: resolution * 1000 / 2,
                zeroes_subseconds: cap.sets_to_zero != 0,
            }),
            resolution,
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let c: Arc<dyn RealTimeClock> = Arc::clone(&self.clock) as _;
        clock::register(
            Arc::clone(&c),
            self.resolution,
            clock::flags::SETTIME_NO_ADJ,
        );
        if self.clock.zeroes_subseconds {
            // Just before the top of a second.
            clock::schedule(&c, 1_000_000_000 - 1000);
        }
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let c: Arc<dyn RealTimeClock> = Arc::clone(&self.clock) as _;
        clock::unregister(cx.sleepable(), &c);
    }
}

pub(crate) const DRIVER: DriverDecl = DriverDecl::of::<EfiRtc>();
