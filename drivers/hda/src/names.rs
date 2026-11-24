// SPDX-License-Identifier: BSD-2-Clause
//! The names FreeBSD gives controllers and codecs in their descriptions
//! (`hdac_devices` and `hdacc_codecs`), for the hardware this tree has
//! met; everything else gets the vendor's name with the device in
//! parentheses, or `Generic`, as FreeBSD does.

use alloc::format;
use alloc::string::String;

/// The controller's description (`hdac_probe`), without ` HDA Controller`.
#[must_use]
pub(crate) fn controller(vendor: u16, device: u16) -> String {
    let exact = match (vendor, device) {
        (0x8086, 0x293e) => Some("Intel 82801I"),
        (0x8086, 0x7ad0) => Some("Intel Alder Lake"),
        (0x1022, 0x15e3) => Some("AMD Ryzen"),
        _ => None,
    };
    if let Some(n) = exact {
        return n.into();
    }
    let vendor_wide = match vendor {
        0x8086 => Some("Intel"),
        0x1022 => Some("AMD"),
        0x1002 => Some("ATI"),
        0x10de => Some("NVIDIA"),
        _ => None,
    };
    match vendor_wide {
        Some(v) => format!("{v} ({device:#06x})"),
        None => format!(
            "Generic ({:#010x})",
            u32::from(device) << 16 | u32::from(vendor)
        ),
    }
}

/// The codec's description (`hdacc_probe`), without ` HDA CODEC`.
#[must_use]
pub(crate) fn codec(vendor: u16, device: u16) -> String {
    let exact = match (vendor, device) {
        (0x10ec, 0x0897) => Some("Realtek ALC897"),
        (0x10ec, 0x1168 | 0x1220) => Some("Realtek ALC1220"),
        (0x10ec, 0x0269) => Some("Realtek ALC269"),
        (0x8086, 0x2882) => Some("Intel Alder Lake"),
        _ => None,
    };
    if let Some(n) = exact {
        return n.into();
    }
    let vendor_wide = match vendor {
        0x1013 => Some("Cirrus Logic"),
        0x10ec => Some("Realtek"),
        0x1057 => Some("Motorola"),
        0x1102 => Some("Creative"),
        0x11d4 => Some("Analog Devices"),
        0x13f6 | 0x434d => Some("CMedia"),
        0x8384 => Some("Sigmatel"),
        0x111d => Some("IDT"),
        0x1095 => Some("Silicon Image"),
        0x11c1 => Some("Lucent/Agere Systems"),
        0x14f1 => Some("Conexant"),
        0x17e8 => Some("Chrontel"),
        0x1106 => Some("VIA"),
        0x15ad => Some("VMware"),
        0x1002 => Some("ATI"),
        0x10de => Some("NVIDIA"),
        0x8086 => Some("Intel"),
        _ => None,
    };
    match vendor_wide {
        Some(v) => format!("{v} ({device:#06x})"),
        None => format!(
            "Generic ({:#06x})",
            u32::from(vendor) << 16 | u32::from(device)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_names() {
        assert_eq!(controller(0x8086, 0x293e), "Intel 82801I");
        assert_eq!(controller(0x1002, 0x1640), "ATI (0x1640)");
        assert_eq!(controller(0x1234, 0x5678), "Generic (0x56781234)");
        assert_eq!(codec(0x10ec, 0x1220), "Realtek ALC1220");
        assert_eq!(codec(0x10ec, 0x9999), "Realtek (0x9999)");
        assert_eq!(codec(0x1af4, 0x0022), "Generic (0x1af40022)");
    }
}
