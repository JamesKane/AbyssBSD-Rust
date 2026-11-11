// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/usbctl` and the `ugen` nodes (`dev/usb/usb_ioctl.h`): the
//! requests `usbconfig(8)` and libusb's `ugen20` backend make, the
//! structures they pass, and the `dev/usb/usb.h` and `dev/usb/usbdi.h`
//! values that cross with them.
//!
//! Every structure is spelled out with its padding (`_pad` fields), so it
//! is plain bytes; `from_bytes` and `to_bytes` convert an ioctl argument
//! (native byte order, as the kernel copies it). Pointers are user
//! addresses, kept as `u64`.
//!
//! Behavior reference: sys/dev/usb/usb_ioctl.h, sys/dev/usb/usb.h,
//! sys/dev/usb/usbdi.h.

use crate::ioctl::{IOC_IN, IOC_OUT, IOC_VOID};
use crate::layout::{Arches, ConstValue};

/// `USB_DEVICE_NAME`: the bus's own node, under `/dev`.
pub const USB_DEVICE_NAME: &[u8] = b"usbctl";
/// `USB_DEVICE_DIR`: where the endpoint nodes live, under `/dev`.
pub const USB_DEVICE_DIR: &[u8] = b"usb";
/// `USB_GENERIC_NAME`: the prefix of the `ugenB.D` links.
pub const USB_GENERIC_NAME: &[u8] = b"ugen";

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(v)
}

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    b[at..at + v.len()].copy_from_slice(v);
}

/// `struct usb_read_dir`: `USB_READ_DIR`'s argument.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbReadDir {
    /// The buffer (a user address).
    pub urd_data: u64,
    /// Entries to skip.
    pub urd_startentry: u32,
    /// The buffer's length.
    pub urd_maxlen: u32,
}

impl UsbReadDir {
    /// Its size.
    pub const LEN: usize = 16;

    /// The structure in an ioctl argument.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() == Self::LEN).then(|| Self {
            urd_data: u64_at(b, 0),
            urd_startentry: u32_at(b, 8),
            urd_maxlen: u32_at(b, 12),
        })
    }
}

/// `struct usb_device_request`: a setup packet, its words little-endian
/// as on the wire.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbDeviceRequest {
    pub bm_request_type: u8,
    pub b_request: u8,
    pub w_value: [u8; 2],
    pub w_index: [u8; 2],
    pub w_length: [u8; 2],
}

impl UsbDeviceRequest {
    /// Its size.
    pub const LEN: usize = 8;

    /// The packet in `b` (eight bytes).
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() == Self::LEN).then(|| Self {
            bm_request_type: b[0],
            b_request: b[1],
            w_value: [b[2], b[3]],
            w_index: [b[4], b[5]],
            w_length: [b[6], b[7]],
        })
    }

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::LEN] {
        [
            self.bm_request_type,
            self.b_request,
            self.w_value[0],
            self.w_value[1],
            self.w_index[0],
            self.w_index[1],
            self.w_length[0],
            self.w_length[1],
        ]
    }

    /// `wValue`.
    #[must_use]
    pub fn value(&self) -> u16 {
        u16::from_le_bytes(self.w_value)
    }

    /// `wIndex`.
    #[must_use]
    pub fn index(&self) -> u16 {
        u16::from_le_bytes(self.w_index)
    }

    /// `wLength`.
    #[must_use]
    pub fn length(&self) -> u16 {
        u16::from_le_bytes(self.w_length)
    }
}

/// `struct usb_ctl_request`: `USB_DO_REQUEST`'s argument.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbCtlRequest {
    /// The data stage's buffer (a user address).
    pub ucr_data: u64,
    /// [`USB_SHORT_XFER_OK`] or 0.
    pub ucr_flags: u16,
    /// The bytes moved, set on return.
    pub ucr_actlen: u16,
    /// Unused (zero).
    pub ucr_addr: u8,
    pub ucr_request: UsbDeviceRequest,
    pub _pad: [u8; 3],
}

impl UsbCtlRequest {
    /// Its size.
    pub const LEN: usize = 24;

    /// The structure in an ioctl argument.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN {
            return None;
        }
        Some(Self {
            ucr_data: u64_at(b, 0),
            ucr_flags: u16_at(b, 8),
            ucr_actlen: u16_at(b, 10),
            ucr_addr: b[12],
            ucr_request: UsbDeviceRequest::from_bytes(&b[13..21])?,
            _pad: [0; 3],
        })
    }

    /// Its bytes, padding zeroed.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0; Self::LEN];
        put(&mut b, 0, &self.ucr_data.to_ne_bytes());
        put(&mut b, 8, &self.ucr_flags.to_ne_bytes());
        put(&mut b, 10, &self.ucr_actlen.to_ne_bytes());
        b[12] = self.ucr_addr;
        put(&mut b, 13, &self.ucr_request.to_bytes());
        b
    }
}

/// `struct usb_alt_interface`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbAltInterface {
    pub uai_interface_index: u8,
    pub uai_alt_index: u8,
}

/// `struct usb_gen_descriptor`: the argument of the descriptor and
/// string requests.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbGenDescriptor {
    /// The buffer (a user address).
    pub ugd_data: u64,
    pub ugd_lang_id: u16,
    pub ugd_maxlen: u16,
    pub ugd_actlen: u16,
    pub ugd_offset: u16,
    pub ugd_config_index: u8,
    pub ugd_string_index: u8,
    pub ugd_iface_index: u8,
    pub ugd_altif_index: u8,
    pub ugd_endpt_index: u8,
    pub ugd_report_type: u8,
    pub reserved: [u8; 8],
    pub _pad: [u8; 2],
}

impl UsbGenDescriptor {
    /// Its size.
    pub const LEN: usize = 32;

    /// The structure in an ioctl argument.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN {
            return None;
        }
        let mut reserved = [0; 8];
        reserved.copy_from_slice(&b[22..30]);
        Some(Self {
            ugd_data: u64_at(b, 0),
            ugd_lang_id: u16_at(b, 8),
            ugd_maxlen: u16_at(b, 10),
            ugd_actlen: u16_at(b, 12),
            ugd_offset: u16_at(b, 14),
            ugd_config_index: b[16],
            ugd_string_index: b[17],
            ugd_iface_index: b[18],
            ugd_altif_index: b[19],
            ugd_endpt_index: b[20],
            ugd_report_type: b[21],
            reserved,
            _pad: [0; 2],
        })
    }

    /// Its bytes, padding zeroed.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0; Self::LEN];
        put(&mut b, 0, &self.ugd_data.to_ne_bytes());
        put(&mut b, 8, &self.ugd_lang_id.to_ne_bytes());
        put(&mut b, 10, &self.ugd_maxlen.to_ne_bytes());
        put(&mut b, 12, &self.ugd_actlen.to_ne_bytes());
        put(&mut b, 14, &self.ugd_offset.to_ne_bytes());
        b[16] = self.ugd_config_index;
        b[17] = self.ugd_string_index;
        b[18] = self.ugd_iface_index;
        b[19] = self.ugd_altif_index;
        b[20] = self.ugd_endpt_index;
        b[21] = self.ugd_report_type;
        put(&mut b, 22, &self.reserved);
        b
    }
}

/// `struct usb_device_info`: `USB_GET_DEVICEINFO`'s answer.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsbDeviceInfo {
    pub udi_product_no: u16,
    pub udi_vendor_no: u16,
    pub udi_release_no: u16,
    /// mA, 0 if self-powered.
    pub udi_power: u16,
    pub udi_bus: u8,
    pub udi_addr: u8,
    pub udi_index: u8,
    pub udi_class: u8,
    pub udi_subclass: u8,
    pub udi_protocol: u8,
    pub udi_config_no: u8,
    pub udi_config_index: u8,
    /// `USB_SPEED_*`.
    pub udi_speed: u8,
    /// `USB_MODE_*`.
    pub udi_mode: u8,
    pub udi_nports: u8,
    pub udi_hubaddr: u8,
    pub udi_hubindex: u8,
    pub udi_hubport: u8,
    /// `USB_POWER_MODE_*`.
    pub udi_power_mode: u8,
    pub udi_suspended: u8,
    pub udi_bustype_no: u16,
    pub udi_reserved: [u8; 14],
    pub udi_product: [u8; 128],
    pub udi_vendor: [u8; 128],
    pub udi_serial: [u8; 64],
    pub udi_release: [u8; 8],
}

impl Default for UsbDeviceInfo {
    fn default() -> Self {
        Self {
            udi_product_no: 0,
            udi_vendor_no: 0,
            udi_release_no: 0,
            udi_power: 0,
            udi_bus: 0,
            udi_addr: 0,
            udi_index: 0,
            udi_class: 0,
            udi_subclass: 0,
            udi_protocol: 0,
            udi_config_no: 0,
            udi_config_index: 0,
            udi_speed: 0,
            udi_mode: 0,
            udi_nports: 0,
            udi_hubaddr: 0,
            udi_hubindex: 0,
            udi_hubport: 0,
            udi_power_mode: 0,
            udi_suspended: 0,
            udi_bustype_no: 0,
            udi_reserved: [0; 14],
            udi_product: [0; 128],
            udi_vendor: [0; 128],
            udi_serial: [0; 64],
            udi_release: [0; 8],
        }
    }
}

impl UsbDeviceInfo {
    /// Its size.
    pub const LEN: usize = 368;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0; Self::LEN];
        put(&mut b, 0, &self.udi_product_no.to_ne_bytes());
        put(&mut b, 2, &self.udi_vendor_no.to_ne_bytes());
        put(&mut b, 4, &self.udi_release_no.to_ne_bytes());
        put(&mut b, 6, &self.udi_power.to_ne_bytes());
        put(
            &mut b,
            8,
            &[
                self.udi_bus,
                self.udi_addr,
                self.udi_index,
                self.udi_class,
                self.udi_subclass,
                self.udi_protocol,
                self.udi_config_no,
                self.udi_config_index,
                self.udi_speed,
                self.udi_mode,
                self.udi_nports,
                self.udi_hubaddr,
                self.udi_hubindex,
                self.udi_hubport,
                self.udi_power_mode,
                self.udi_suspended,
            ],
        );
        put(&mut b, 24, &self.udi_bustype_no.to_ne_bytes());
        put(&mut b, 26, &self.udi_reserved);
        put(&mut b, 40, &self.udi_product);
        put(&mut b, 168, &self.udi_vendor);
        put(&mut b, 296, &self.udi_serial);
        put(&mut b, 360, &self.udi_release);
        b
    }
}

/// `USB_DEVICE_PORT_PATH_MAX`.
pub const USB_DEVICE_PORT_PATH_MAX: usize = 32;

/// `struct usb_device_port_path`: the ports from the root hub down.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbDevicePortPath {
    pub udp_bus: u8,
    pub udp_index: u8,
    pub udp_port_level: u8,
    pub udp_port_no: [u8; USB_DEVICE_PORT_PATH_MAX],
}

impl UsbDevicePortPath {
    /// Its size.
    pub const LEN: usize = 35;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0; Self::LEN];
        b[0] = self.udp_bus;
        b[1] = self.udp_index;
        b[2] = self.udp_port_level;
        put(&mut b, 3, &self.udp_port_no);
        b
    }
}

/// `struct usb_device_stats`: requests by transfer type (`UE_*`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbDeviceStats {
    pub uds_requests_ok: [u32; 4],
    pub uds_requests_fail: [u32; 4],
}

impl UsbDeviceStats {
    /// Its size.
    pub const LEN: usize = 32;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0; Self::LEN];
        for (i, v) in self
            .uds_requests_ok
            .iter()
            .chain(&self.uds_requests_fail)
            .enumerate()
        {
            put(&mut b, i * 4, &v.to_ne_bytes());
        }
        b
    }
}

/// `struct usb_fs_start`, and `usb_fs_stop`, `usb_fs_complete`,
/// `usb_fs_close`, `usb_fs_clear_stall_sync`, all one endpoint index.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsStart {
    pub ep_index: u8,
}

/// `struct usb_fs_stop`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsStop {
    pub ep_index: u8,
}

/// `struct usb_fs_complete`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsComplete {
    pub ep_index: u8,
}

/// `struct usb_fs_close`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsClose {
    pub ep_index: u8,
}

/// `struct usb_fs_clear_stall_sync`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsClearStallSync {
    pub ep_index: u8,
}

/// `struct usb_fs_uninit`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsUninit {
    /// Must be zero.
    pub dummy: u8,
}

/// A short frame ends the transfer without an error.
pub const USB_FS_FLAG_SINGLE_SHORT_OK: u16 = 0x0001;
/// Short frames do not end it.
pub const USB_FS_FLAG_MULTI_SHORT_OK: u16 = 0x0002;
/// Every frame sent ends short (a zero-length packet after a full one).
pub const USB_FS_FLAG_FORCE_SHORT: u16 = 0x0004;
/// Clear the endpoint's stall first.
pub const USB_FS_FLAG_CLEAR_STALL: u16 = 0x0008;
/// `USB_FS_TIMEOUT_NONE`.
pub const USB_FS_TIMEOUT_NONE: u16 = 0;
/// `USB_FS_MAX_BUFSIZE`: 32 MiB.
pub const USB_FS_MAX_BUFSIZE: u32 = 1 << 25;
/// `USB_FS_MAX_FRAMES`.
pub const USB_FS_MAX_FRAMES: u32 = 1 << 12;
/// `USB_FS_MAX_FRAMES_PRE_SCALE`, a flag in `max_frames`.
pub const USB_FS_MAX_FRAMES_PRE_SCALE: u32 = 1 << 31;

/// `struct usb_fs_endpoint`: one transfer's description, in the array
/// `USB_FS_INIT` names, read at `USB_FS_START` and updated at
/// `USB_FS_COMPLETE`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsEndpoint {
    /// The frames' buffers (a user address of `nFrames` pointers).
    pub pp_buffer: u64,
    /// The frames' lengths (a user address of `nFrames` `uint32_t`s),
    /// updated to the lengths moved.
    pub p_length: u64,
    pub n_frames: u32,
    /// Frames moved, set on completion.
    pub a_frames: u32,
    /// `USB_FS_FLAG_*`.
    pub flags: u16,
    /// Milliseconds; 0 for none.
    pub timeout: u16,
    pub isoc_time_complete: u16,
    pub _pad0: u16,
    /// `USB_ERR_*`, set on completion.
    pub status: i32,
    pub _pad1: u32,
}

impl UsbFsEndpoint {
    /// Its size.
    pub const LEN: usize = 40;
    /// Where `aFrames` is in it.
    pub const A_FRAMES_AT: usize = 20;
    /// Where `isoc_time_complete` is.
    pub const ISOC_TIME_COMPLETE_AT: usize = 28;
    /// Where `status` is.
    pub const STATUS_AT: usize = 32;

    /// The structure in `b`.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() == Self::LEN).then(|| Self {
            pp_buffer: u64_at(b, 0),
            p_length: u64_at(b, 8),
            n_frames: u32_at(b, 16),
            a_frames: u32_at(b, 20),
            flags: u16_at(b, 24),
            timeout: u16_at(b, 26),
            isoc_time_complete: u16_at(b, 28),
            _pad0: 0,
            status: i32::from_ne_bytes([b[32], b[33], b[34], b[35]]),
            _pad1: 0,
        })
    }
}

/// `struct usb_fs_init`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsInit {
    /// The endpoint array (a user address of [`UsbFsEndpoint`]s).
    pub p_endpoints: u64,
    /// Its length.
    pub ep_index_max: u8,
    pub _pad: [u8; 7],
}

impl UsbFsInit {
    /// Its size.
    pub const LEN: usize = 16;

    /// The structure in an ioctl argument.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() == Self::LEN).then(|| Self {
            p_endpoints: u64_at(b, 0),
            ep_index_max: b[8],
            _pad: [0; 7],
        })
    }
}

/// `struct usb_fs_open`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsOpen {
    pub max_bufsize: u32,
    pub max_frames: u32,
    /// Set on return.
    pub max_packet_length: u16,
    /// Unused.
    pub dev_index: u8,
    pub ep_index: u8,
    /// `bEndpointAddress`.
    pub ep_no: u8,
    pub _pad: [u8; 3],
}

impl UsbFsOpen {
    /// Its size.
    pub const LEN: usize = 16;

    /// The structure in an ioctl argument (the first [`Self::LEN`] bytes
    /// of a `usb_fs_open_stream`'s too).
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        (b.len() >= Self::LEN).then(|| Self {
            max_bufsize: u32_at(b, 0),
            max_frames: u32_at(b, 4),
            max_packet_length: u16_at(b, 8),
            dev_index: b[10],
            ep_index: b[11],
            ep_no: b[12],
            _pad: [0; 3],
        })
    }

    /// Writes it over the first [`Self::LEN`] bytes of `b`.
    pub fn write_to(&self, b: &mut [u8]) {
        put(b, 0, &self.max_bufsize.to_ne_bytes());
        put(b, 4, &self.max_frames.to_ne_bytes());
        put(b, 8, &self.max_packet_length.to_ne_bytes());
        b[10] = self.dev_index;
        b[11] = self.ep_index;
        b[12] = self.ep_no;
    }
}

/// `struct usb_fs_open_stream`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbFsOpenStream {
    pub fs_open: UsbFsOpen,
    pub stream_id: u16,
    pub _pad: [u8; 2],
}

impl UsbFsOpenStream {
    /// Its size.
    pub const LEN: usize = 20;
}

/// `struct usb_device_descriptor` (18 bytes, packed).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbDeviceDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub bcd_usb: [u8; 2],
    pub b_device_class: u8,
    pub b_device_sub_class: u8,
    pub b_device_protocol: u8,
    pub b_max_packet_size: u8,
    pub id_vendor: [u8; 2],
    pub id_product: [u8; 2],
    pub bcd_device: [u8; 2],
    pub i_manufacturer: u8,
    pub i_product: u8,
    pub i_serial_number: u8,
    pub b_num_configurations: u8,
}

/// `struct usb_config_descriptor` (9 bytes, packed).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbConfigDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub w_total_length: [u8; 2],
    pub b_num_interface: u8,
    pub b_configuration_value: u8,
    pub i_configuration: u8,
    pub bm_attributes: u8,
    pub b_max_power: u8,
}

/// `struct usb_interface_descriptor` (9 bytes, packed).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbInterfaceDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub b_interface_number: u8,
    pub b_alternate_setting: u8,
    pub b_num_endpoints: u8,
    pub b_interface_class: u8,
    pub b_interface_sub_class: u8,
    pub b_interface_protocol: u8,
    pub i_interface: u8,
}

/// `struct usb_endpoint_descriptor` (7 bytes, packed).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsbEndpointDescriptor {
    pub b_length: u8,
    pub b_descriptor_type: u8,
    pub b_endpoint_address: u8,
    pub bm_attributes: u8,
    pub w_max_packet_size: [u8; 2],
    pub b_interval: u8,
}

/// The sizes of the descriptors above.
pub const DEVICE_DESCRIPTOR_LEN: usize = 18;
pub const CONFIG_DESCRIPTOR_LEN: usize = 9;
pub const INTERFACE_DESCRIPTOR_LEN: usize = 9;
pub const ENDPOINT_DESCRIPTOR_LEN: usize = 7;

/// `usb_error_t` (`usbdi.h`): a transfer's status, as
/// [`UsbFsEndpoint::status`] reports it.
pub mod err {
    pub const NORMAL_COMPLETION: i32 = 0;
    pub const PENDING_REQUESTS: i32 = 1;
    pub const NOT_STARTED: i32 = 2;
    pub const INVAL: i32 = 3;
    pub const NOMEM: i32 = 4;
    pub const CANCELLED: i32 = 5;
    pub const BAD_ADDRESS: i32 = 6;
    pub const BAD_BUFSIZE: i32 = 7;
    pub const BAD_FLAG: i32 = 8;
    pub const NO_CALLBACK: i32 = 9;
    pub const IN_USE: i32 = 10;
    pub const NO_ADDR: i32 = 11;
    pub const NO_PIPE: i32 = 12;
    pub const ZERO_NFRAMES: i32 = 13;
    pub const ZERO_MAXP: i32 = 14;
    pub const SET_ADDR_FAILED: i32 = 15;
    pub const NO_POWER: i32 = 16;
    pub const TOO_DEEP: i32 = 17;
    pub const IOERROR: i32 = 18;
    pub const NOT_CONFIGURED: i32 = 19;
    pub const TIMEOUT: i32 = 20;
    pub const SHORT_XFER: i32 = 21;
    pub const STALLED: i32 = 22;
    pub const INTERRUPTED: i32 = 23;
    pub const DMA_LOAD_FAILED: i32 = 24;
    pub const BAD_CONTEXT: i32 = 25;
    pub const NO_ROOT_HUB: i32 = 26;
    pub const NO_INTR_THREAD: i32 = 27;
    pub const NOT_LOCKED: i32 = 28;
}

/// `USB_SHORT_XFER_OK` (`usbdi.h`): a short read is not an error.
pub const USB_SHORT_XFER_OK: u16 = 0x0004;
/// `USB_DEFAULT_TIMEOUT`, ms.
pub const USB_DEFAULT_TIMEOUT: u32 = 5000;

/// `USB_POWER_MODE_*`.
pub mod power_mode {
    pub const OFF: u8 = 0;
    pub const ON: u8 = 1;
    pub const SAVE: u8 = 2;
    pub const SUSPEND: u8 = 3;
    pub const RESUME: u8 = 4;
}

/// `enum usb_dev_speed`.
pub mod speed {
    pub const VARIABLE: u8 = 0;
    pub const LOW: u8 = 1;
    pub const FULL: u8 = 2;
    pub const HIGH: u8 = 3;
    pub const SUPER: u8 = 4;
}

/// `enum usb_hc_mode`.
pub mod mode {
    pub const HOST: u8 = 0;
    pub const DEVICE: u8 = 1;
    pub const DUAL: u8 = 2;
}

/// `USB_UNCONFIG_INDEX`: no configuration selected.
pub const USB_UNCONFIG_INDEX: u8 = 0xff;
/// `USB_IFACE_INDEX_ANY`.
pub const USB_IFACE_INDEX_ANY: u8 = 0xff;
/// `USB_TEMP_MAX`: the device-side templates' count.
pub const USB_TEMP_MAX: i32 = 12;

// Every length is a few hundred bytes, well within `IOCPARM_MASK`.
#[allow(clippy::cast_possible_truncation)]
const fn ioc(dir: u32, num: u32, len: usize) -> u32 {
    dir | ((len as u32) << 16) | (b'U' as u32) << 8 | num
}

const IOWR: u32 = IOC_IN | IOC_OUT;
const INT: usize = 4;

pub const USB_REQUEST: u32 = ioc(IOWR, 1, UsbCtlRequest::LEN);
pub const USB_SETDEBUG: u32 = ioc(IOC_IN, 2, INT);
pub const USB_DISCOVER: u32 = ioc(IOC_VOID, 3, 0);
pub const USB_DEVICEINFO: u32 = ioc(IOWR, 4, UsbDeviceInfo::LEN);
pub const USB_DEVICESTATS: u32 = ioc(IOC_OUT, 5, UsbDeviceStats::LEN);
pub const USB_DEVICEENUMERATE: u32 = ioc(IOC_IN, 6, INT);
pub const USB_GET_REPORT_DESC: u32 = ioc(IOWR, 21, UsbGenDescriptor::LEN);
pub const USB_GET_CONFIG: u32 = ioc(IOC_OUT, 100, INT);
pub const USB_SET_CONFIG: u32 = ioc(IOC_IN, 101, INT);
pub const USB_GET_ALTINTERFACE: u32 = ioc(IOWR, 102, 2);
pub const USB_SET_ALTINTERFACE: u32 = ioc(IOWR, 103, 2);
pub const USB_GET_DEVICE_DESC: u32 = ioc(IOC_OUT, 105, DEVICE_DESCRIPTOR_LEN);
pub const USB_GET_CONFIG_DESC: u32 = ioc(IOC_OUT, 106, CONFIG_DESCRIPTOR_LEN);
pub const USB_GET_RX_INTERFACE_DESC: u32 = ioc(IOC_OUT, 107, INTERFACE_DESCRIPTOR_LEN);
pub const USB_GET_RX_ENDPOINT_DESC: u32 = ioc(IOC_OUT, 108, ENDPOINT_DESCRIPTOR_LEN);
pub const USB_GET_FULL_DESC: u32 = ioc(IOWR, 109, UsbGenDescriptor::LEN);
pub const USB_GET_STRING_DESC: u32 = ioc(IOWR, 110, UsbGenDescriptor::LEN);
pub const USB_DO_REQUEST: u32 = ioc(IOWR, 111, UsbCtlRequest::LEN);
pub const USB_GET_DEVICEINFO: u32 = ioc(IOC_OUT, 112, UsbDeviceInfo::LEN);
pub const USB_SET_RX_SHORT_XFER: u32 = ioc(IOC_IN, 113, INT);
pub const USB_SET_RX_TIMEOUT: u32 = ioc(IOC_IN, 114, INT);
pub const USB_GET_RX_FRAME_SIZE: u32 = ioc(IOC_OUT, 115, INT);
pub const USB_GET_RX_BUFFER_SIZE: u32 = ioc(IOC_OUT, 117, INT);
pub const USB_SET_RX_BUFFER_SIZE: u32 = ioc(IOC_IN, 118, INT);
pub const USB_SET_RX_STALL_FLAG: u32 = ioc(IOC_IN, 119, INT);
pub const USB_SET_TX_STALL_FLAG: u32 = ioc(IOC_IN, 120, INT);
pub const USB_GET_IFACE_DRIVER: u32 = ioc(IOWR, 121, UsbGenDescriptor::LEN);
pub const USB_CLAIM_INTERFACE: u32 = ioc(IOC_IN, 122, INT);
pub const USB_RELEASE_INTERFACE: u32 = ioc(IOC_IN, 123, INT);
pub const USB_IFACE_DRIVER_ACTIVE: u32 = ioc(IOC_IN, 124, INT);
pub const USB_IFACE_DRIVER_DETACH: u32 = ioc(IOC_IN, 125, INT);
pub const USB_GET_PLUGTIME: u32 = ioc(IOC_OUT, 126, 4);
pub const USB_READ_DIR: u32 = ioc(IOC_IN, 127, UsbReadDir::LEN);
pub const USB_GET_DEV_PORT_PATH: u32 = ioc(IOC_OUT, 134, UsbDevicePortPath::LEN);
pub const USB_GET_POWER_USAGE: u32 = ioc(IOC_OUT, 135, INT);
pub const USB_SET_TX_FORCE_SHORT: u32 = ioc(IOC_IN, 136, INT);
pub const USB_SET_TX_TIMEOUT: u32 = ioc(IOC_IN, 137, INT);
pub const USB_GET_TX_FRAME_SIZE: u32 = ioc(IOC_OUT, 138, INT);
pub const USB_GET_TX_BUFFER_SIZE: u32 = ioc(IOC_OUT, 139, INT);
pub const USB_SET_TX_BUFFER_SIZE: u32 = ioc(IOC_IN, 140, INT);
pub const USB_GET_TX_INTERFACE_DESC: u32 = ioc(IOC_OUT, 141, INTERFACE_DESCRIPTOR_LEN);
pub const USB_GET_TX_ENDPOINT_DESC: u32 = ioc(IOC_OUT, 142, ENDPOINT_DESCRIPTOR_LEN);
pub const USB_SET_PORT_ENABLE: u32 = ioc(IOC_IN, 143, INT);
pub const USB_SET_PORT_DISABLE: u32 = ioc(IOC_IN, 144, INT);
pub const USB_SET_POWER_MODE: u32 = ioc(IOC_IN, 145, INT);
pub const USB_GET_POWER_MODE: u32 = ioc(IOC_OUT, 146, INT);
pub const USB_SET_TEMPLATE: u32 = ioc(IOC_IN, 147, INT);
pub const USB_GET_TEMPLATE: u32 = ioc(IOC_OUT, 148, INT);
pub const USB_FS_START: u32 = ioc(IOC_IN, 192, 1);
pub const USB_FS_STOP: u32 = ioc(IOC_IN, 193, 1);
pub const USB_FS_COMPLETE: u32 = ioc(IOC_OUT, 194, 1);
pub const USB_FS_INIT: u32 = ioc(IOC_IN, 195, UsbFsInit::LEN);
pub const USB_FS_UNINIT: u32 = ioc(IOC_IN, 196, 1);
pub const USB_FS_OPEN: u32 = ioc(IOWR, 197, UsbFsOpen::LEN);
pub const USB_FS_CLOSE: u32 = ioc(IOC_IN, 198, 1);
pub const USB_FS_CLEAR_STALL_SYNC: u32 = ioc(IOC_IN, 199, 1);
pub const USB_FS_OPEN_STREAM: u32 = ioc(IOWR, 200, UsbFsOpenStream::LEN);

/// The quirk requests (`'Q'`), which `/dev/usbctl` answers only when
/// FreeBSD's `usb_quirk` is present.
pub const USB_DEV_QUIRK_GET: u32 = IOWR | (64 << 16) | (b'Q' as u32) << 8;
pub const USB_QUIRK_NAME_GET: u32 = USB_DEV_QUIRK_GET | 1;
pub const USB_DEV_QUIRK_ADD: u32 = IOC_IN | (64 << 16) | (b'Q' as u32) << 8 | 2;
pub const USB_DEV_QUIRK_REMOVE: u32 = IOC_IN | (64 << 16) | (b'Q' as u32) << 8 | 3;

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "dev/usb/usb_ioctl.h",
        value,
        arches: Arches::All,
    }
}

const fn d(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "dev/usb/usbdi.h",
        value,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("USB_REQUEST", USB_REQUEST as i64),
    c("USB_SETDEBUG", USB_SETDEBUG as i64),
    c("USB_DISCOVER", USB_DISCOVER as i64),
    c("USB_DEVICEINFO", USB_DEVICEINFO as i64),
    c("USB_DEVICESTATS", USB_DEVICESTATS as i64),
    c("USB_DEVICEENUMERATE", USB_DEVICEENUMERATE as i64),
    c("USB_GET_REPORT_DESC", USB_GET_REPORT_DESC as i64),
    c("USB_GET_CONFIG", USB_GET_CONFIG as i64),
    c("USB_SET_CONFIG", USB_SET_CONFIG as i64),
    c("USB_GET_ALTINTERFACE", USB_GET_ALTINTERFACE as i64),
    c("USB_SET_ALTINTERFACE", USB_SET_ALTINTERFACE as i64),
    c("USB_GET_DEVICE_DESC", USB_GET_DEVICE_DESC as i64),
    c("USB_GET_CONFIG_DESC", USB_GET_CONFIG_DESC as i64),
    c(
        "USB_GET_RX_INTERFACE_DESC",
        USB_GET_RX_INTERFACE_DESC as i64,
    ),
    c("USB_GET_RX_ENDPOINT_DESC", USB_GET_RX_ENDPOINT_DESC as i64),
    c("USB_GET_FULL_DESC", USB_GET_FULL_DESC as i64),
    c("USB_GET_STRING_DESC", USB_GET_STRING_DESC as i64),
    c("USB_DO_REQUEST", USB_DO_REQUEST as i64),
    c("USB_GET_DEVICEINFO", USB_GET_DEVICEINFO as i64),
    c("USB_SET_RX_SHORT_XFER", USB_SET_RX_SHORT_XFER as i64),
    c("USB_SET_RX_TIMEOUT", USB_SET_RX_TIMEOUT as i64),
    c("USB_GET_RX_FRAME_SIZE", USB_GET_RX_FRAME_SIZE as i64),
    c("USB_GET_RX_BUFFER_SIZE", USB_GET_RX_BUFFER_SIZE as i64),
    c("USB_SET_RX_BUFFER_SIZE", USB_SET_RX_BUFFER_SIZE as i64),
    c("USB_SET_RX_STALL_FLAG", USB_SET_RX_STALL_FLAG as i64),
    c("USB_SET_TX_STALL_FLAG", USB_SET_TX_STALL_FLAG as i64),
    c("USB_GET_IFACE_DRIVER", USB_GET_IFACE_DRIVER as i64),
    c("USB_CLAIM_INTERFACE", USB_CLAIM_INTERFACE as i64),
    c("USB_RELEASE_INTERFACE", USB_RELEASE_INTERFACE as i64),
    c("USB_IFACE_DRIVER_ACTIVE", USB_IFACE_DRIVER_ACTIVE as i64),
    c("USB_IFACE_DRIVER_DETACH", USB_IFACE_DRIVER_DETACH as i64),
    c("USB_GET_PLUGTIME", USB_GET_PLUGTIME as i64),
    c("USB_READ_DIR", USB_READ_DIR as i64),
    c("USB_GET_DEV_PORT_PATH", USB_GET_DEV_PORT_PATH as i64),
    c("USB_GET_POWER_USAGE", USB_GET_POWER_USAGE as i64),
    c("USB_SET_TX_FORCE_SHORT", USB_SET_TX_FORCE_SHORT as i64),
    c("USB_SET_TX_TIMEOUT", USB_SET_TX_TIMEOUT as i64),
    c("USB_GET_TX_FRAME_SIZE", USB_GET_TX_FRAME_SIZE as i64),
    c("USB_GET_TX_BUFFER_SIZE", USB_GET_TX_BUFFER_SIZE as i64),
    c("USB_SET_TX_BUFFER_SIZE", USB_SET_TX_BUFFER_SIZE as i64),
    c(
        "USB_GET_TX_INTERFACE_DESC",
        USB_GET_TX_INTERFACE_DESC as i64,
    ),
    c("USB_GET_TX_ENDPOINT_DESC", USB_GET_TX_ENDPOINT_DESC as i64),
    c("USB_SET_PORT_ENABLE", USB_SET_PORT_ENABLE as i64),
    c("USB_SET_PORT_DISABLE", USB_SET_PORT_DISABLE as i64),
    c("USB_SET_POWER_MODE", USB_SET_POWER_MODE as i64),
    c("USB_GET_POWER_MODE", USB_GET_POWER_MODE as i64),
    c("USB_SET_TEMPLATE", USB_SET_TEMPLATE as i64),
    c("USB_GET_TEMPLATE", USB_GET_TEMPLATE as i64),
    c("USB_FS_START", USB_FS_START as i64),
    c("USB_FS_STOP", USB_FS_STOP as i64),
    c("USB_FS_COMPLETE", USB_FS_COMPLETE as i64),
    c("USB_FS_INIT", USB_FS_INIT as i64),
    c("USB_FS_UNINIT", USB_FS_UNINIT as i64),
    c("USB_FS_OPEN", USB_FS_OPEN as i64),
    c("USB_FS_CLOSE", USB_FS_CLOSE as i64),
    c("USB_FS_CLEAR_STALL_SYNC", USB_FS_CLEAR_STALL_SYNC as i64),
    c("USB_FS_OPEN_STREAM", USB_FS_OPEN_STREAM as i64),
    c("USB_DEV_QUIRK_GET", USB_DEV_QUIRK_GET as i64),
    c("USB_QUIRK_NAME_GET", USB_QUIRK_NAME_GET as i64),
    c("USB_DEV_QUIRK_ADD", USB_DEV_QUIRK_ADD as i64),
    c("USB_DEV_QUIRK_REMOVE", USB_DEV_QUIRK_REMOVE as i64),
    c(
        "USB_FS_FLAG_SINGLE_SHORT_OK",
        USB_FS_FLAG_SINGLE_SHORT_OK as i64,
    ),
    c(
        "USB_FS_FLAG_MULTI_SHORT_OK",
        USB_FS_FLAG_MULTI_SHORT_OK as i64,
    ),
    c("USB_FS_FLAG_FORCE_SHORT", USB_FS_FLAG_FORCE_SHORT as i64),
    c("USB_FS_FLAG_CLEAR_STALL", USB_FS_FLAG_CLEAR_STALL as i64),
    c("USB_FS_TIMEOUT_NONE", USB_FS_TIMEOUT_NONE as i64),
    c("USB_FS_MAX_BUFSIZE", USB_FS_MAX_BUFSIZE as i64),
    c("USB_FS_MAX_FRAMES", USB_FS_MAX_FRAMES as i64),
    c(
        "USB_FS_MAX_FRAMES_PRE_SCALE",
        USB_FS_MAX_FRAMES_PRE_SCALE as i64,
    ),
    c("USB_DEVICE_PORT_PATH_MAX", USB_DEVICE_PORT_PATH_MAX as i64),
    c("USB_TEMP_MAX", USB_TEMP_MAX as i64),
    c("USB_POWER_MODE_OFF", power_mode::OFF as i64),
    c("USB_POWER_MODE_ON", power_mode::ON as i64),
    c("USB_POWER_MODE_SAVE", power_mode::SAVE as i64),
    c("USB_POWER_MODE_SUSPEND", power_mode::SUSPEND as i64),
    c("USB_POWER_MODE_RESUME", power_mode::RESUME as i64),
    c("USB_SPEED_VARIABLE", speed::VARIABLE as i64),
    c("USB_SPEED_LOW", speed::LOW as i64),
    c("USB_SPEED_FULL", speed::FULL as i64),
    c("USB_SPEED_HIGH", speed::HIGH as i64),
    c("USB_SPEED_SUPER", speed::SUPER as i64),
    c("USB_MODE_HOST", mode::HOST as i64),
    c("USB_MODE_DEVICE", mode::DEVICE as i64),
    c("USB_MODE_DUAL", mode::DUAL as i64),
    c("USB_UNCONFIG_INDEX", USB_UNCONFIG_INDEX as i64),
    c("USB_IFACE_INDEX_ANY", USB_IFACE_INDEX_ANY as i64),
    d("USB_SHORT_XFER_OK", USB_SHORT_XFER_OK as i64),
    d("USB_DEFAULT_TIMEOUT", USB_DEFAULT_TIMEOUT as i64),
    d("USB_ERR_NORMAL_COMPLETION", err::NORMAL_COMPLETION as i64),
    d("USB_ERR_PENDING_REQUESTS", err::PENDING_REQUESTS as i64),
    d("USB_ERR_NOT_STARTED", err::NOT_STARTED as i64),
    d("USB_ERR_INVAL", err::INVAL as i64),
    d("USB_ERR_NOMEM", err::NOMEM as i64),
    d("USB_ERR_CANCELLED", err::CANCELLED as i64),
    d("USB_ERR_BAD_ADDRESS", err::BAD_ADDRESS as i64),
    d("USB_ERR_BAD_BUFSIZE", err::BAD_BUFSIZE as i64),
    d("USB_ERR_BAD_FLAG", err::BAD_FLAG as i64),
    d("USB_ERR_NO_CALLBACK", err::NO_CALLBACK as i64),
    d("USB_ERR_IN_USE", err::IN_USE as i64),
    d("USB_ERR_NO_ADDR", err::NO_ADDR as i64),
    d("USB_ERR_NO_PIPE", err::NO_PIPE as i64),
    d("USB_ERR_ZERO_NFRAMES", err::ZERO_NFRAMES as i64),
    d("USB_ERR_ZERO_MAXP", err::ZERO_MAXP as i64),
    d("USB_ERR_SET_ADDR_FAILED", err::SET_ADDR_FAILED as i64),
    d("USB_ERR_NO_POWER", err::NO_POWER as i64),
    d("USB_ERR_TOO_DEEP", err::TOO_DEEP as i64),
    d("USB_ERR_IOERROR", err::IOERROR as i64),
    d("USB_ERR_NOT_CONFIGURED", err::NOT_CONFIGURED as i64),
    d("USB_ERR_TIMEOUT", err::TIMEOUT as i64),
    d("USB_ERR_SHORT_XFER", err::SHORT_XFER as i64),
    d("USB_ERR_STALLED", err::STALLED as i64),
    d("USB_ERR_INTERRUPTED", err::INTERRUPTED as i64),
    d("USB_ERR_DMA_LOAD_FAILED", err::DMA_LOAD_FAILED as i64),
    d("USB_ERR_BAD_CONTEXT", err::BAD_CONTEXT as i64),
    d("USB_ERR_NO_ROOT_HUB", err::NO_ROOT_HUB as i64),
    d("USB_ERR_NO_INTR_THREAD", err::NO_INTR_THREAD as i64),
    d("USB_ERR_NOT_LOCKED", err::NOT_LOCKED as i64),
];

const _: () = {
    assert!(size_of::<UsbReadDir>() == UsbReadDir::LEN);
    assert!(size_of::<UsbDeviceRequest>() == UsbDeviceRequest::LEN);
    assert!(size_of::<UsbCtlRequest>() == UsbCtlRequest::LEN);
    assert!(size_of::<UsbGenDescriptor>() == UsbGenDescriptor::LEN);
    assert!(size_of::<UsbDeviceInfo>() == UsbDeviceInfo::LEN);
    assert!(size_of::<UsbDevicePortPath>() == UsbDevicePortPath::LEN);
    assert!(size_of::<UsbDeviceStats>() == UsbDeviceStats::LEN);
    assert!(size_of::<UsbFsEndpoint>() == UsbFsEndpoint::LEN);
    assert!(size_of::<UsbFsInit>() == UsbFsInit::LEN);
    assert!(size_of::<UsbFsOpen>() == UsbFsOpen::LEN);
    assert!(size_of::<UsbFsOpenStream>() == UsbFsOpenStream::LEN);
    assert!(size_of::<UsbDeviceDescriptor>() == DEVICE_DESCRIPTOR_LEN);
    assert!(size_of::<UsbConfigDescriptor>() == CONFIG_DESCRIPTOR_LEN);
    assert!(size_of::<UsbInterfaceDescriptor>() == INTERFACE_DESCRIPTOR_LEN);
    assert!(size_of::<UsbEndpointDescriptor>() == ENDPOINT_DESCRIPTOR_LEN);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let r = UsbCtlRequest {
            ucr_data: 0x1234_5678_9abc,
            ucr_flags: USB_SHORT_XFER_OK,
            ucr_actlen: 7,
            ucr_addr: 0,
            ucr_request: UsbDeviceRequest {
                bm_request_type: 0x80,
                b_request: 0,
                w_value: [0, 0],
                w_index: [0, 0],
                w_length: [2, 0],
            },
            _pad: [0; 3],
        };
        assert_eq!(UsbCtlRequest::from_bytes(&r.to_bytes()), Some(r));
        assert_eq!(r.ucr_request.length(), 2);
        let g = UsbGenDescriptor {
            ugd_data: 0xdead_beef,
            ugd_lang_id: 0x409,
            ugd_maxlen: 255,
            ugd_string_index: 2,
            reserved: [1, 2, 3, 4, 5, 6, 7, 8],
            ..UsbGenDescriptor::default()
        };
        assert_eq!(UsbGenDescriptor::from_bytes(&g.to_bytes()), Some(g));
        assert_eq!(UsbGenDescriptor::from_bytes(&[0; 31]), None);
    }

    #[test]
    fn request_numbers_are_freebsds() {
        // From FreeBSD 15.1's headers, as `abi-check` holds them.
        assert_eq!(USB_READ_DIR, 0x8010_557f);
        assert_eq!(USB_GET_DEVICEINFO, 0x4170_5570);
        assert_eq!(USB_DO_REQUEST, 0xc018_556f);
        assert_eq!(USB_FS_INIT, 0x8010_55c3);
        assert_eq!(USB_FS_OPEN, 0xc010_55c5);
        assert_eq!(USB_FS_COMPLETE, 0x4001_55c2);
    }

    #[test]
    fn fs_open_keeps_its_place_in_a_stream_open() {
        let mut b = [0u8; UsbFsOpenStream::LEN];
        let o = UsbFsOpen {
            max_bufsize: 65536,
            max_frames: 2,
            max_packet_length: 512,
            ep_index: 3,
            ep_no: 0x81,
            ..UsbFsOpen::default()
        };
        o.write_to(&mut b);
        b[16] = 5;
        assert_eq!(UsbFsOpen::from_bytes(&b), Some(o));
        assert_eq!(b[16], 5);
    }
}
