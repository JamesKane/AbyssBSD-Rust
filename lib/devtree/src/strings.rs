// SPDX-License-Identifier: BSD-2-Clause
//! The pnpinfo and location strings, byte for byte as FreeBSD formats
//! them: `devd.conf` rules and `devmatch(8)` match on them
//! (DRIVER-MODEL.md §11.2).
//!
//! Behavior reference: sys/dev/pci/pci.c (`pci_child_pnpinfo_method`,
//! `pci_child_location_method`), sys/dev/acpica/acpi.c (`acpi_pnpinfo`,
//! `acpi_child_location_method`), sys/dev/acpica/acpi_pci.c,
//! sys/dev/virtio/virtio.c (`virtio_child_pnpinfo`).

use core::fmt::{self, Write};

use crate::ids::{AcpiIdent, PciIdent, UsbIdent, UsbPlace, VirtioIdent};

/// A PCI function's position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciAddress {
    /// The segment (domain).
    pub domain: u32,
    /// Bus number.
    pub bus: u8,
    /// Slot (device) number.
    pub slot: u8,
    /// Function number.
    pub function: u8,
}

/// `vendor=0x8086 device=0x10d3 subvendor=… subdevice=… class=0x020000`.
pub fn pci_pnpinfo(out: &mut impl Write, i: &PciIdent) -> fmt::Result {
    write!(
        out,
        "vendor=0x{:04x} device=0x{:04x} subvendor=0x{:04x} subdevice=0x{:04x} class=0x{:06x}",
        i.vendor, i.device, i.subvendor, i.subdevice, i.class
    )
}

/// `slot=3 function=0 dbsf=pci0:0:3:0`, plus ` handle=… [_PXM=n]` when the
/// function has an ACPI namespace object (the ACPI PCI bus's addition).
pub fn pci_location(
    out: &mut impl Write,
    a: &PciAddress,
    acpi_handle: Option<&str>,
    pxm: Option<i32>,
) -> fmt::Result {
    write!(
        out,
        "slot={} function={} dbsf=pci{}:{}:{}:{}",
        a.slot, a.function, a.domain, a.bus, a.slot, a.function
    )?;
    if let Some(h) = acpi_handle {
        write!(out, " handle={h}")?;
        if let Some(p) = pxm {
            write!(out, " _PXM={p}")?;
        }
    }
    Ok(())
}

/// `vendor=0x00001af4 device=0x1001 subvendor=0x1af4 device_type=0x00000002`:
/// vendor and type widened to 32 bits so match tables are shared with
/// virtio-mmio. Virtio children have no location.
pub fn virtio_pnpinfo(out: &mut impl Write, i: &VirtioIdent) -> fmt::Result {
    write!(
        out,
        "vendor=0x{:08x} device=0x{:04x} subvendor=0x{:04x} device_type=0x{:08x}",
        i.vendor, i.device, i.subvendor, i.device_type
    )
}

/// `_HID=PNP0A08 _UID=0 _CID=PNP0A03`, with `none` for a missing ID. The
/// UID prints as an unsigned decimal parsed from the start of the UID
/// string, as `strtoul` does (a non-numeric UID prints as 0).
pub fn acpi_pnpinfo(out: &mut impl Write, i: &AcpiIdent) -> fmt::Result {
    write!(
        out,
        "_HID={} _UID={} _CID={}",
        i.hid.as_deref().unwrap_or("none"),
        i.uid.as_deref().map_or(0, strtoul10),
        i.cids.first().map_or("none", |c| c.as_str())
    )
}

/// `handle=\_SB_.PCI0 [_PXM=n]`.
pub fn acpi_location(out: &mut impl Write, handle: &str, pxm: Option<i32>) -> fmt::Result {
    write!(out, "handle={handle}")?;
    if let Some(p) = pxm {
        write!(out, " _PXM={p}")?;
    }
    Ok(())
}

/// An ACPI path as FreeBSD prints it: a leading `\`, then each 4-byte name
/// segment (underscore-padded, as stored) separated by dots.
pub fn acpi_path(out: &mut impl Write, segments: &[[u8; 4]]) -> fmt::Result {
    out.write_char('\\')?;
    for (i, s) in segments.iter().enumerate() {
        if i > 0 {
            out.write_char('.')?;
        }
        for &b in s {
            out.write_char(char::from(b))?;
        }
    }
    Ok(())
}

/// C's `strtoul(s, NULL, 10)` on a 64-bit `unsigned long`: leading space,
/// an optional sign, then decimal digits; stops at the first non-digit;
/// saturates on overflow; a minus sign negates modulo 2^64.
fn strtoul10(s: &str) -> u64 {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: u64 = 0;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            break;
        }
        match v
            .checked_mul(10)
            .and_then(|x| x.checked_add(u64::from(b - b'0')))
        {
            Some(x) => v = x,
            None => return u64::MAX,
        }
    }
    if neg { v.wrapping_neg() } else { v }
}

/// FreeBSD's USB interface pnpinfo (`uhub_child_pnpinfo`):
/// `vendor=0x0627 product=0x0001 devclass=0x00 devsubclass=0x00
/// devproto=0x00 sernum="42" release=0x0000 mode=host intclass=0x03
/// intsubclass=0x01 intprotocol=0x01`.
pub fn usb_pnpinfo(out: &mut impl Write, i: &UsbIdent, p: &UsbPlace) -> fmt::Result {
    write!(
        out,
        "vendor=0x{:04x} product=0x{:04x} devclass=0x{:02x} devsubclass=0x{:02x} \
         devproto=0x{:02x} sernum=\"{}\" release=0x{:04x} mode=host intclass=0x{:02x} \
         intsubclass=0x{:02x} intprotocol=0x{:02x}",
        i.vendor,
        i.product,
        i.class,
        i.subclass,
        i.protocol,
        p.serial,
        i.release,
        i.if_class,
        i.if_subclass,
        i.if_protocol
    )
}

/// FreeBSD's USB interface location (`uhub_child_location`):
/// `bus=0 hubaddr=1 port=1 devaddr=2 interface=0 ugen=ugen0.2`.
pub fn usb_location(out: &mut impl Write, i: &UsbIdent, p: &UsbPlace) -> fmt::Result {
    write!(
        out,
        "bus={} hubaddr={} port={} devaddr={} interface={} ugen={}",
        p.bus, p.hub_addr, p.port, p.dev_addr, i.interface, p.ugen
    )
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;

    use super::*;

    #[test]
    fn pci() {
        let i = PciIdent {
            vendor: 0x1af4,
            device: 0x1042,
            subvendor: 0x1af4,
            subdevice: 0x1100,
            class: 0x01_0000,
            revision: 1,
        };
        let mut s = String::new();
        pci_pnpinfo(&mut s, &i).unwrap();
        assert_eq!(
            s,
            "vendor=0x1af4 device=0x1042 subvendor=0x1af4 subdevice=0x1100 class=0x010000"
        );
        let a = PciAddress {
            domain: 0,
            bus: 0,
            slot: 3,
            function: 0,
        };
        let mut s = String::new();
        pci_location(&mut s, &a, None, None).unwrap();
        assert_eq!(s, "slot=3 function=0 dbsf=pci0:0:3:0");
        let mut s = String::new();
        pci_location(&mut s, &a, Some("\\_SB_.PCI0.S18_"), Some(1)).unwrap();
        assert_eq!(
            s,
            "slot=3 function=0 dbsf=pci0:0:3:0 handle=\\_SB_.PCI0.S18_ _PXM=1"
        );
    }

    #[test]
    fn acpi() {
        let i = AcpiIdent {
            hid: Some("PNP0A08".into()),
            cids: vec!["PNP0A03".into()],
            uid: Some("0".into()),
        };
        let mut s = String::new();
        acpi_pnpinfo(&mut s, &i).unwrap();
        assert_eq!(s, "_HID=PNP0A08 _UID=0 _CID=PNP0A03");
        let mut s = String::new();
        acpi_pnpinfo(&mut s, &AcpiIdent::default()).unwrap();
        assert_eq!(s, "_HID=none _UID=0 _CID=none");
        let mut s = String::new();
        acpi_path(&mut s, &[*b"_SB_", *b"PCI0"]).unwrap();
        acpi_location(&mut String::new(), &s, None).unwrap();
        assert_eq!(s, "\\_SB_.PCI0");
    }

    #[test]
    fn uid_parsing() {
        assert_eq!(strtoul10("12"), 12);
        assert_eq!(strtoul10("  7xyz"), 7);
        assert_eq!(strtoul10("CPU0"), 0);
        assert_eq!(strtoul10("-1"), u64::MAX);
        assert_eq!(strtoul10("99999999999999999999999"), u64::MAX);
    }
}
