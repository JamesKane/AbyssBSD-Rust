// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/hidrawN` (`dev/hid/hidraw.h`): the uhid-compatible requests and
//! the Linux-compatible `HIDIOC*` ones hidapi and libfido2 make, and the
//! structures they pass.
//!
//! `struct hidraw_gen_descriptor` is `struct usb_gen_descriptor` and
//! `struct hidraw_device_info` has `struct usb_device_info`'s layout, so
//! their byte conversions are [`crate::usb`]'s.
//!
//! Behavior reference: sys/dev/hid/hidraw.h, sys/dev/hid/hidraw.c.

use crate::ioctl::{IOC_IN, IOC_OUT, IOC_VOID, IOCPARM_MASK};
use crate::layout::{Arches, ConstValue};

/// `HIDRAW_BUFFER_SIZE`: input reports queued per open.
pub const HIDRAW_BUFFER_SIZE: usize = 64;
/// `HID_MAX_DESCRIPTOR_SIZE`.
pub const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;

/// `HID_INPUT_REPORT`, `HID_OUTPUT_REPORT`, `HID_FEATURE_REPORT`
/// (`dev/hid/hid.h`).
pub const HID_INPUT_REPORT: u8 = 1;
pub const HID_OUTPUT_REPORT: u8 = 2;
pub const HID_FEATURE_REPORT: u8 = 3;

/// `struct hidraw_gen_descriptor`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HidrawGenDescriptor {
    pub hgd_data: u64,
    pub hgd_lang_id: u16,
    pub hgd_maxlen: u16,
    pub hgd_actlen: u16,
    pub hgd_offset: u16,
    pub hgd_config_index: u8,
    pub hgd_string_index: u8,
    pub hgd_iface_index: u8,
    pub hgd_altif_index: u8,
    pub hgd_endpt_index: u8,
    pub hgd_report_type: u8,
    pub reserved: [u8; 8],
    pub _pad: [u8; 2],
}

/// `struct hidraw_device_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidrawDeviceInfo {
    pub hdi_product: u16,
    pub hdi_vendor: u16,
    pub hdi_version: u16,
    pub occupied: [u8; 18],
    pub hdi_bustype: u16,
    pub reserved: [u8; 14],
    pub hdi_name: [u8; 128],
    pub hdi_phys: [u8; 128],
    pub hdi_uniq: [u8; 64],
    pub hdi_release: [u8; 8],
}

impl Default for HidrawDeviceInfo {
    fn default() -> Self {
        Self {
            hdi_product: 0,
            hdi_vendor: 0,
            hdi_version: 0,
            occupied: [0; 18],
            hdi_bustype: 0,
            reserved: [0; 14],
            hdi_name: [0; 128],
            hdi_phys: [0; 128],
            hdi_uniq: [0; 64],
            hdi_release: [0; 8],
        }
    }
}

impl HidrawDeviceInfo {
    /// Its size.
    pub const LEN: usize = 368;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0; Self::LEN];
        b[0..2].copy_from_slice(&self.hdi_product.to_ne_bytes());
        b[2..4].copy_from_slice(&self.hdi_vendor.to_ne_bytes());
        b[4..6].copy_from_slice(&self.hdi_version.to_ne_bytes());
        b[6..24].copy_from_slice(&self.occupied);
        b[24..26].copy_from_slice(&self.hdi_bustype.to_ne_bytes());
        b[26..40].copy_from_slice(&self.reserved);
        b[40..168].copy_from_slice(&self.hdi_name);
        b[168..296].copy_from_slice(&self.hdi_phys);
        b[296..360].copy_from_slice(&self.hdi_uniq);
        b[360..368].copy_from_slice(&self.hdi_release);
        b
    }
}

/// `struct hidraw_report_descriptor`: `HIDIOCGRDESC`'s argument (by
/// pointer).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidrawReportDescriptor {
    pub size: u32,
    pub value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

/// Where `value` is in [`HidrawReportDescriptor`].
pub const HIDRAW_REPORT_DESCRIPTOR_VALUE_AT: u64 = 4;

/// `struct hidraw_devinfo`: `HIDIOCGRAWINFO`'s answer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HidrawDevinfo {
    pub bustype: u32,
    pub vendor: i16,
    pub product: i16,
}

impl HidrawDevinfo {
    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut b = [0; 8];
        b[0..4].copy_from_slice(&self.bustype.to_ne_bytes());
        b[4..6].copy_from_slice(&self.vendor.to_ne_bytes());
        b[6..8].copy_from_slice(&self.product.to_ne_bytes());
        b
    }
}

// Every length is within `IOCPARM_MASK`.
#[allow(clippy::cast_possible_truncation)]
const fn ioc(dir: u32, num: u32, len: usize) -> u32 {
    dir | (((len as u32) & IOCPARM_MASK) << 16) | (b'U' as u32) << 8 | num
}

const GEN: usize = 32;
const INOUT: u32 = IOC_IN | IOC_OUT;

pub const HIDRAW_GET_REPORT_DESC: u32 = ioc(INOUT, 21, GEN);
pub const HIDRAW_SET_IMMED: u32 = ioc(IOC_IN, 22, 4);
pub const HIDRAW_GET_REPORT: u32 = ioc(INOUT, 23, GEN);
pub const HIDRAW_SET_REPORT: u32 = ioc(IOC_IN, 24, GEN);
pub const HIDRAW_GET_REPORT_ID: u32 = ioc(IOC_OUT, 25, 4);
pub const HIDRAW_SET_REPORT_DESC: u32 = ioc(IOC_IN, 26, GEN);
pub const HIDRAW_GET_DEVICEINFO: u32 = ioc(IOC_OUT, 112, HidrawDeviceInfo::LEN);
pub const HIDIOCGRDESCSIZE: u32 = ioc(IOC_OUT, 30, 4);
pub const HIDIOCGRDESC: u32 = ioc(IOC_VOID, 31, 0);
pub const HIDIOCGRAWINFO: u32 = ioc(IOC_OUT, 32, 8);

/// The variable-length requests, by their base (`IOCBASECMD`: the length
/// bits clear); the length is the caller's buffer.
pub const HIDIOCGRAWNAME_BASE: u32 = ioc(IOC_OUT, 33, 0);
pub const HIDIOCGRAWPHYS_BASE: u32 = ioc(IOC_OUT, 34, 0);
pub const HIDIOCSFEATURE_BASE: u32 = ioc(IOC_IN, 35, 0);
pub const HIDIOCGFEATURE_BASE: u32 = ioc(INOUT, 36, 0);
pub const HIDIOCGRAWUNIQ_BASE: u32 = ioc(IOC_OUT, 37, 0);
pub const HIDIOCSINPUT_BASE: u32 = ioc(IOC_IN, 38, 0);
pub const HIDIOCGINPUT_BASE: u32 = ioc(INOUT, 39, 0);
pub const HIDIOCSOUTPUT_BASE: u32 = ioc(IOC_IN, 40, 0);
pub const HIDIOCGOUTPUT_BASE: u32 = ioc(INOUT, 41, 0);

/// `IOCBASECMD(cmd)`: the request without its length.
#[must_use]
pub const fn base(cmd: u32) -> u32 {
    cmd & !(IOCPARM_MASK << 16)
}

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "dev/hid/hidraw.h",
        value,
        arches: Arches::All,
    }
}

const fn h(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "dev/hid/hid.h",
        value,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("HIDRAW_BUFFER_SIZE", HIDRAW_BUFFER_SIZE as i64),
    c("HID_MAX_DESCRIPTOR_SIZE", HID_MAX_DESCRIPTOR_SIZE as i64),
    c("HIDRAW_GET_REPORT_DESC", HIDRAW_GET_REPORT_DESC as i64),
    c("HIDRAW_SET_IMMED", HIDRAW_SET_IMMED as i64),
    c("HIDRAW_GET_REPORT", HIDRAW_GET_REPORT as i64),
    c("HIDRAW_SET_REPORT", HIDRAW_SET_REPORT as i64),
    c("HIDRAW_GET_REPORT_ID", HIDRAW_GET_REPORT_ID as i64),
    c("HIDRAW_SET_REPORT_DESC", HIDRAW_SET_REPORT_DESC as i64),
    c("HIDRAW_GET_DEVICEINFO", HIDRAW_GET_DEVICEINFO as i64),
    c("HIDIOCGRDESCSIZE", HIDIOCGRDESCSIZE as i64),
    c("HIDIOCGRDESC", HIDIOCGRDESC as i64),
    c("HIDIOCGRAWINFO", HIDIOCGRAWINFO as i64),
    c("HIDIOCGRAWNAME(0)", HIDIOCGRAWNAME_BASE as i64),
    c("HIDIOCGRAWPHYS(0)", HIDIOCGRAWPHYS_BASE as i64),
    c("HIDIOCSFEATURE(0)", HIDIOCSFEATURE_BASE as i64),
    c("HIDIOCGFEATURE(0)", HIDIOCGFEATURE_BASE as i64),
    c("HIDIOCGRAWUNIQ(0)", HIDIOCGRAWUNIQ_BASE as i64),
    c("HIDIOCSINPUT(0)", HIDIOCSINPUT_BASE as i64),
    c("HIDIOCGINPUT(0)", HIDIOCGINPUT_BASE as i64),
    c("HIDIOCSOUTPUT(0)", HIDIOCSOUTPUT_BASE as i64),
    c("HIDIOCGOUTPUT(0)", HIDIOCGOUTPUT_BASE as i64),
    h("HID_INPUT_REPORT", HID_INPUT_REPORT as i64),
    h("HID_OUTPUT_REPORT", HID_OUTPUT_REPORT as i64),
    h("HID_FEATURE_REPORT", HID_FEATURE_REPORT as i64),
];

const _: () = {
    assert!(size_of::<HidrawGenDescriptor>() == GEN);
    assert!(size_of::<HidrawDeviceInfo>() == HidrawDeviceInfo::LEN);
    assert!(size_of::<HidrawReportDescriptor>() == 4100);
    assert!(size_of::<HidrawDevinfo>() == 8);
    // The uhid-compatible requests are usb_ioctl.h's.
    assert!(HIDRAW_GET_REPORT_DESC == crate::usb::USB_GET_REPORT_DESC);
    assert!(HIDRAW_GET_DEVICEINFO == crate::usb::USB_GET_DEVICEINFO);
};
