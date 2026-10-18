// SPDX-License-Identifier: BSD-2-Clause
//! `acpi_button`: the power and sleep buttons as namespace devices
//! (PNP0C0C, PNP0C0E), which firmware reports by `Notify(…, 0x80)`.
//! A power button press asks the kernel to power off; there is no sleep
//! state to enter yet, so a sleep button press is only logged.
//!
//! Fixed-feature buttons are PM1 events, handled with the SCI
//! ([`crate::events`]).
//!
//! Behavior reference: sys/dev/acpica/acpi_button.c.

use alloc::string::String;
use alloc::sync::Arc;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StopCx, StopReason};
use devtree::ids::AcpiId;

use crate::bus::AcpiDevice;
use crate::service::{NotifyId, service};

/// "Pressed" (ACPI 6.5 §5.6.6, table 5.169).
const PRESSED: u64 = 0x80;

/// `acpi_buttonN`.
#[derive(Debug)]
pub struct AcpiButton {
    notify: NotifyId,
}

fn is_power(ident: &Ident) -> bool {
    match ident {
        Ident::Acpi(a, _) => {
            a.hid.as_deref() == Some("PNP0C0C") || a.cids.iter().any(|c| c == "PNP0C0C")
        }
        _ => false,
    }
}

impl Driver for AcpiButton {
    const NAME: &'static str = "acpi_button";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("PNP0C0C"), AcpiId::new("PNP0C0E")]);

    fn describe(ident: &Ident) -> String {
        if is_power(ident) {
            "Power Button"
        } else {
            "Sleep Button"
        }
        .into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let svc = service().ok_or(AttachError::Decline)?;
        let dev = cx.bus_data::<AcpiDevice>().ok_or(AttachError::Decline)?;
        let power = is_power(cx.ident());
        let ops = svc.ops();
        let unit = cx.unit();
        let notify = svc.on_notify(
            dev.node,
            Arc::new(move |value| {
                if value != PRESSED {
                    return;
                }
                if power {
                    ops.power_button();
                } else {
                    ops.log(format_args!(
                        "acpi_button{unit}: sleep button (no sleep states yet)"
                    ));
                }
            }),
        );
        Ok(Self { notify })
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        if let Some(svc) = service() {
            svc.remove_notify(self.notify);
        }
    }
}

/// The declaration for the build's driver table.
pub const BUTTON: DriverDecl = DriverDecl::of::<AcpiButton>();
