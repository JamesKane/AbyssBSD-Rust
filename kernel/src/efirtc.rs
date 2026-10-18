// SPDX-License-Identifier: BSD-2-Clause
//! `efirtc`: the time of day from UEFI's `GetTime`, a child of `nexus0`
//! wherever runtime services are available, as on FreeBSD.
//!
//! Behavior reference: sys/dev/efidev/efirtc.c.

use alloc::string::String;
use alloc::sync::Arc;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use clock::{DateTime, RealTimeClock};

/// The firmware's clock.
#[derive(Debug)]
pub(crate) struct EfiClock {
    name: String,
}

impl RealTimeClock for EfiClock {
    fn name(&self) -> &str {
        &self.name
    }

    fn read(&self) -> Option<DateTime> {
        let t = crate::firmware::time()?;
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
        if t.time_zone != 2047 && t.time_zone != 0 {
            let secs = d
                .unix_seconds()?
                .checked_add_signed(i64::from(t.time_zone) * 60)?;
            d = DateTime::from_unix(secs, t.nanosecond);
        }
        d.valid().then_some(d)
    }
}

/// An `efirtcN` instance.
#[derive(Debug)]
pub(crate) struct EfiRtc {
    clock: Arc<EfiClock>,
}

impl Driver for EfiRtc {
    const NAME: &'static str = "efirtc";
    const IDS: Ids = Ids::Nexus(&["efirtc"]);

    fn describe(_: &Ident) -> String {
        "EFI Realtime Clock".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        crate::firmware::time().ok_or(AttachError::Device("GetTime failed"))?;
        Ok(Self {
            clock: Arc::new(EfiClock {
                name: alloc::format!("efirtc{}", cx.unit()),
            }),
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        clock::register(Arc::clone(&self.clock) as _);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        let c: Arc<dyn RealTimeClock> = Arc::clone(&self.clock) as _;
        clock::unregister(&c);
    }
}

pub(crate) const DRIVER: DriverDecl = DriverDecl::of::<EfiRtc>();
