// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/devctl` event lines, as devd(8) parses them
//! (DRIVER-MODEL.md §11.3).
//!
//! Behavior reference: sys/kern/kern_devctl.c (`devaddq`,
//! `devctl_notify`, `devctl_safe_quote_sb`).

use core::fmt::{self, Write};

/// What happened to a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceEvent<'a> {
    /// `+name at …`: a driver attached.
    Attach(&'a str),
    /// `-name at …`: a driver detached.
    Detach(&'a str),
    /// `? at …`: no driver matched.
    NoMatch,
}

/// A device event line: `+nvme0 at <location> <pnpinfo> on pci0\n`. A
/// device without a parent is reported `on .`.
pub fn device_event(
    out: &mut impl Write,
    ev: DeviceEvent<'_>,
    location: &str,
    pnpinfo: &str,
    parent: Option<&str>,
) -> fmt::Result {
    let (sigil, what) = match ev {
        DeviceEvent::Attach(n) => ('+', n),
        DeviceEvent::Detach(n) => ('-', n),
        DeviceEvent::NoMatch => ('?', ""),
    };
    writeln!(
        out,
        "{sigil}{what} at {location} {pnpinfo} on {}",
        parent.unwrap_or(".")
    )
}

/// A notification line: `!system=ACPI subsystem=PowerButton type=… data\n`.
pub fn notify(
    out: &mut impl Write,
    system: &str,
    subsystem: &str,
    kind: &str,
    data: Option<&str>,
) -> fmt::Result {
    write!(out, "!system={system} subsystem={subsystem} type={kind}")?;
    if let Some(d) = data {
        write!(out, " {d}")?;
    }
    out.write_char('\n')
}

/// Escapes `"` and `\` with a backslash, for values placed inside quotes.
pub fn quote(out: &mut impl Write, src: &str) -> fmt::Result {
    for c in src.chars() {
        if c == '"' || c == '\\' {
            out.write_char('\\')?;
        }
        out.write_char(c)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::*;

    #[test]
    fn lines() {
        let mut s = String::new();
        device_event(
            &mut s,
            DeviceEvent::Attach("vtblk0"),
            "",
            "vendor=0x1af4 device=0x1001",
            Some("virtio_pci0"),
        )
        .unwrap();
        assert_eq!(
            s,
            "+vtblk0 at  vendor=0x1af4 device=0x1001 on virtio_pci0\n"
        );
        let mut s = String::new();
        device_event(&mut s, DeviceEvent::NoMatch, "slot=1", "x=1", Some("pci0")).unwrap();
        assert_eq!(s, "? at slot=1 x=1 on pci0\n");
        let mut s = String::new();
        device_event(&mut s, DeviceEvent::Detach("root0"), "", "", None).unwrap();
        assert_eq!(s, "-root0 at   on .\n");
        let mut s = String::new();
        notify(
            &mut s,
            "ACPI",
            "PowerButton",
            "\\_SB_.PWRB",
            Some("notify=0x80"),
        )
        .unwrap();
        assert_eq!(
            s,
            "!system=ACPI subsystem=PowerButton type=\\_SB_.PWRB notify=0x80\n"
        );
        let mut s = String::new();
        quote(&mut s, r#"a "b" \c"#).unwrap();
        assert_eq!(s, r#"a \"b\" \\c"#);
    }
}
