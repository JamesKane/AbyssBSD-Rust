// SPDX-License-Identifier: BSD-2-Clause
//! Encoding `hw.bus.devices.<n>` records (`struct u_device`) for
//! `devinfo(8)`.
//!
//! Behavior reference: sys/kern/subr_bus.c (`sysctl_devices`).

use abi::bus::{BUS_USER_BUFFER, UDevice};

/// A device's text fields, in record order.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fields<'a> {
    /// `name` + unit (`nvme0`), empty without a unit.
    pub nameunit: &'a str,
    /// The description.
    pub desc: &'a str,
    /// The driver name, empty without one.
    pub driver: &'a str,
    /// The pnpinfo string.
    pub pnpinfo: &'a str,
    /// The location string.
    pub location: &'a str,
}

/// The fields do not fit in `BUS_USER_BUFFER` bytes; FreeBSD fails the
/// sysctl rather than truncate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow;

/// Builds a record: `dv_fields` is each field followed by a NUL, then
/// zeros.
pub fn encode(
    handle: usize,
    parent: usize,
    devflags: u32,
    flags: u16,
    state: i32,
    f: &Fields<'_>,
) -> Result<UDevice, Overflow> {
    let mut d = UDevice {
        dv_handle: handle,
        dv_parent: parent,
        dv_devflags: devflags,
        dv_flags: flags,
        dv_state: state,
        dv_fields: [0; BUS_USER_BUFFER],
    };
    let mut at = 0;
    for s in [f.nameunit, f.desc, f.driver, f.pnpinfo, f.location] {
        let end = at + s.len();
        // The field, its NUL, and sbuf's own terminating NUL must fit.
        if end + 1 >= BUS_USER_BUFFER {
            return Err(Overflow);
        }
        d.dv_fields[at..end].copy_from_slice(s.as_bytes());
        at = end + 1;
    }
    Ok(d)
}

/// The text fields of a record (for tests and diagnostics).
#[must_use]
pub fn fields(d: &UDevice) -> [&[u8]; 5] {
    let mut out: [&[u8]; 5] = [&[]; 5];
    let mut rest: &[u8] = &d.dv_fields;
    for slot in &mut out {
        let n = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        *slot = &rest[..n];
        rest = rest.get(n + 1..).unwrap_or(&[]);
    }
    out
}

/// The record's bytes as `struct u_device` lays them out, padding zeroed.
pub fn to_bytes(d: &UDevice, out: &mut alloc::vec::Vec<u8>) {
    use core::mem::{offset_of, size_of};
    let base = out.len();
    out.resize(base + size_of::<UDevice>(), 0);
    let o = &mut out[base..];
    let mut put = |at: usize, b: &[u8]| o[at..at + b.len()].copy_from_slice(b);
    put(offset_of!(UDevice, dv_handle), &d.dv_handle.to_ne_bytes());
    put(offset_of!(UDevice, dv_parent), &d.dv_parent.to_ne_bytes());
    put(
        offset_of!(UDevice, dv_devflags),
        &d.dv_devflags.to_ne_bytes(),
    );
    put(offset_of!(UDevice, dv_flags), &d.dv_flags.to_ne_bytes());
    put(offset_of!(UDevice, dv_state), &d.dv_state.to_ne_bytes());
    put(offset_of!(UDevice, dv_fields), &d.dv_fields);
}

#[cfg(test)]
mod tests {
    use abi::bus::{df, ds};

    use super::*;

    #[test]
    fn round_trip() {
        let f = Fields {
            nameunit: "nvme0",
            desc: "Generic NVMe Device",
            driver: "nvme",
            pnpinfo: "vendor=0x1b36 device=0x0010 subvendor=0x1af4 subdevice=0x1100 class=0x010802",
            location: "slot=4 function=0 dbsf=pci0:0:4:0",
        };
        let d = encode(0x10, 0x8, 0, df::ENABLED, ds::ATTACHED, &f).unwrap();
        let got = fields(&d);
        assert_eq!(got[0], b"nvme0");
        assert_eq!(got[2], b"nvme");
        assert_eq!(got[4], f.location.as_bytes());
        // Exactly the fields and their NULs, then zeros.
        let used: usize = got.iter().map(|g| g.len() + 1).sum();
        assert!(d.dv_fields[used..].iter().all(|&b| b == 0));
        assert_eq!(d.dv_fields[f.nameunit.len() + 1 + f.desc.len()], 0);
    }

    #[test]
    fn bytes_follow_the_layout() {
        let f = Fields {
            nameunit: "nvme0",
            ..Fields::default()
        };
        let d = encode(0x1122, 7, 0, 1, 30, &f).unwrap();
        let mut out = alloc::vec![0xee];
        to_bytes(&d, &mut out);
        assert_eq!(out.len(), 1 + size_of::<UDevice>());
        let b = &out[1..];
        assert_eq!(b[..8], 0x1122usize.to_ne_bytes());
        assert_eq!(b[8..16], 7usize.to_ne_bytes());
        assert_eq!(b[20..22], 1u16.to_ne_bytes());
        // dv_flags is followed by two padding bytes, zeroed.
        assert_eq!(b[22..24], [0, 0]);
        assert_eq!(b[24..28], 30i32.to_ne_bytes());
        assert_eq!(&b[28..34], b"nvme0\0");
    }

    #[test]
    fn overflow_fails() {
        let long = alloc::string::String::from_utf8(alloc::vec![b'x'; BUS_USER_BUFFER]).unwrap();
        let f = Fields {
            pnpinfo: &long,
            ..Fields::default()
        };
        assert_eq!(encode(1, 0, 0, 0, ds::ALIVE, &f).unwrap_err(), Overflow);
    }
}
