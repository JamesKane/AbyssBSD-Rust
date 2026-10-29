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

/// The longest event line: FreeBSD builds each in a fixed buffer of
/// `DEVCTL_BUFFER` (1024 bytes less a pointer) with its NUL, and drops one
/// that overflows it.
pub const LINE_MAX: usize = 1024 - 8 - 1;

/// The PCI function a `devctl(8)` name selects, `pciD:B:S:F` or `pciB:S:F`
/// (domain 0), as `pciconf` writes them (FreeBSD's `pci_lookup`). The
/// numbers are read as `strtol` reads them, so `pci:0:3:0` is domain 0.
#[must_use]
pub fn pci_selector(name: &str) -> Option<crate::strings::PciAddress> {
    // `strtol(s, &end, 10)`: optional spaces and sign, then digits; with
    // no digits, the value is 0 and `end` is `s`.
    fn strtol(s: &[u8]) -> (i64, &[u8]) {
        let t = s.trim_ascii_start();
        let (neg, t) = match t.first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        let digits = t.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return (0, s);
        }
        let v = t[..digits].iter().fold(0i64, |v, &d| {
            v.saturating_mul(10).saturating_add(i64::from(d - b'0'))
        });
        (if neg { -v } else { v }, &t[digits..])
    }
    // A number from 0 to `INT_MAX`.
    fn field(s: &[u8]) -> Option<(u32, &[u8])> {
        let (v, end) = strtol(s);
        let v = u32::try_from(v)
            .ok()
            .filter(|&v| v <= i32::MAX.cast_unsigned())?;
        Some((v, end))
    }
    let rest = name.as_bytes().strip_prefix(b"pci")?;
    let (domain, end) = field(rest)?;
    let end = end.strip_prefix(b":")?;
    let (bus, end) = field(end)?;
    let end = end.strip_prefix(b":")?;
    let (slot, end) = field(end)?;
    let (domain, bus, slot, func) = match end {
        [] => (0, domain, bus, slot),
        [b':', more @ ..] => match field(more)? {
            (f, []) => (domain, bus, slot, f),
            _ => return None,
        },
        _ => return None,
    };
    // PCI_DOMAINMAX, PCI_BUSMAX, PCI_SLOTMAX, PCIE_ARI_FUNCMAX, PCI_FUNCMAX.
    if domain > 65535 || bus > 255 || slot > 31 || func > 255 || (slot != 0 && func > 7) {
        return None;
    }
    Some(crate::strings::PciAddress {
        domain,
        bus: u8::try_from(bus).ok()?,
        slot: u8::try_from(slot).ok()?,
        function: u8::try_from(func).ok()?,
    })
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

    #[test]
    fn pci_selectors() {
        use crate::strings::PciAddress;
        let a = |domain, bus, slot, function| {
            Some(PciAddress {
                domain,
                bus,
                slot,
                function,
            })
        };
        assert_eq!(pci_selector("pci0:0:3:0"), a(0, 0, 3, 0));
        assert_eq!(pci_selector("pci1:2:3"), a(0, 1, 2, 3));
        assert_eq!(pci_selector("pci0:0:0:200"), a(0, 0, 0, 200));
        // strtol: no digits reads as 0 there, and a sign is taken.
        assert_eq!(pci_selector("pci:0:3:0"), a(0, 0, 3, 0));
        assert_eq!(pci_selector("pci+0:0:3"), a(0, 0, 0, 3));
        assert_eq!(pci_selector("pci-1:0:3"), None);
        assert_eq!(pci_selector("pci0:0:1:8"), None);
        assert_eq!(pci_selector("pci0:0:32:0"), None);
        assert_eq!(pci_selector("pci0:256:0:0"), None);
        assert_eq!(pci_selector("pci0:0:3:0x"), None);
        assert_eq!(pci_selector("pci0:0"), None);
        assert_eq!(pci_selector("pci0"), None);
        assert_eq!(pci_selector("vtnet0"), None);
    }
}
