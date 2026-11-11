// SPDX-License-Identifier: BSD-2-Clause
//! What `ugen` and `/dev/usbctl` compute from data alone: the checks on a
//! control request from userland, the `USB_READ_DIR` listing, the xHCI
//! root hub's descriptors and standard requests, string descriptors made
//! from text, and the frame and buffer sizes a FIFO transfer gets.
//!
//! Behavior reference: sys/dev/usb/usb_util.c (`usb_check_request`,
//! `usb_make_str_desc`, `usb_printbcd`), sys/dev/usb/usb_dev.c
//! (`usb_read_symlink`), sys/dev/usb/controller/xhci.c (`xhci_devd`,
//! `xhci_confd`, `xhci_roothub_exec`), sys/dev/usb/usb_transfer.c
//! (`usbd_transfer_setup_sub`, `usbd_get_std_packet_size`).

use alloc::string::String;
use alloc::vec::Vec;

use crate::descriptor::{Endpoint, TransferType, kind};
use crate::request::{code, rt};
use crate::{Setup, Speed};

/// What [`check_request`] asks of a request from userland.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestCheck {
    /// It needs `PRIV_DRIVER` (`SET_ADDRESS`, `SET_CONFIGURATION`,
    /// `SET_INTERFACE`, which could break the bus's view of the device).
    pub needs_priv: bool,
    /// A standard request to an endpoint: that endpoint (`wIndex`'s low
    /// byte) must exist.
    pub endpoint: Option<u8>,
    /// It clears the endpoint's halt, so its data toggle is reset too.
    pub clears_halt: bool,
}

/// `usb_check_request`: what userland's request `s` must pass.
#[must_use]
pub fn check_request(s: &Setup) -> RequestCheck {
    let write_device = rt::STANDARD | rt::DEVICE;
    let write_interface = rt::STANDARD | rt::INTERFACE;
    let write_endpoint = rt::STANDARD | rt::ENDPOINT;
    let needs_priv = (s.request_type == write_device
        && matches!(s.request, code::SET_ADDRESS | code::SET_CONFIGURATION))
        || (s.request_type == write_interface && s.request == code::SET_INTERFACE);
    let to_endpoint = s.request_type == write_endpoint;
    RequestCheck {
        needs_priv,
        endpoint: to_endpoint.then(|| s.index.to_le_bytes()[0]),
        clears_halt: to_endpoint
            && s.request == code::CLEAR_FEATURE
            && s.value == crate::request::ENDPOINT_HALT,
    }
}

/// `usb_read_symlink`: the `USB_READ_DIR` listing of `names` (FreeBSD's
/// list of `ugen` links, each its own source and destination), from entry
/// `start`, in at most `max` bytes. Each entry is its length (a byte),
/// the source, a NUL, the destination, a NUL; a zero length ends the
/// list when there is room for it. Entries whose length does not fit a
/// byte are skipped, as they are not counted either.
#[must_use]
pub fn read_dir<'a>(names: impl IntoIterator<Item = &'a str>, start: u32, max: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut skip = start;
    let mut left = max;
    for name in names {
        let n = u32::try_from(name.len()).unwrap_or(u32::MAX);
        let total = n.saturating_mul(2).saturating_add(3);
        let Ok(len) = u8::try_from(total) else {
            continue;
        };
        if skip != 0 {
            skip -= 1;
            continue;
        }
        if total > left {
            break;
        }
        out.push(len);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        left -= total;
    }
    if left != 0 {
        out.push(0);
    }
    out
}

/// `usb_make_str_desc`: a string descriptor of `s` (its bytes as UTF-16
/// code units, at most 126 of them), in at most `max` bytes.
#[must_use]
pub fn string_descriptor(s: &str, max: usize) -> Vec<u8> {
    if max < 2 {
        return Vec::new();
    }
    let n = s.len().min(126).min(max / 2 - 1);
    let mut out = Vec::with_capacity(2 + 2 * n);
    // At most 127 units: fits a byte.
    out.push(u8::try_from((n + 1) * 2).unwrap_or(u8::MAX));
    out.push(kind::STRING);
    for &b in &s.as_bytes()[..n] {
        out.extend_from_slice(&[b, 0]);
    }
    out
}

/// `usb_printbcd`: `bcdDevice` as FreeBSD prints a release (`1.00`).
#[must_use]
pub fn printbcd(bcd: u16) -> String {
    alloc::format!("{:x}.{:02x}", bcd >> 8, bcd & 0xff)
}

/// `xhci_devd`: the xHCI root hub's device descriptor, a USB 3.0 hub.
pub const ROOT_HUB_DEVICE: [u8; 18] = [
    18, 1, 0x00, 0x03, 9, 0, 3, 9, 0, 0, 0, 0, 0x00, 0x01, 1, 2, 0, 1,
];

/// `xhci_confd`: its configuration descriptor: one hub interface with the
/// status change endpoint (`0x81`, interrupt, two bytes, interval 255)
/// and its SuperSpeed companion.
pub const ROOT_HUB_CONFIG: [u8; 31] = [
    9, 2, 31, 0, 1, 1, 0, 0x40, 0, // configuration, self-powered
    9, 4, 0, 0, 1, 9, 0, 0, 0, // interface: hub
    7, 5, 0x81, 3, 2, 0, 255, // endpoint
    6, 0x30, 0, 0, 0, 0, // SuperSpeed companion
];

/// `xhci_roothub_exec`'s standard requests: the answer to `s` from the
/// root hub of a controller whose vendor string is `vendor`, with
/// configuration `config` set; `None` is FreeBSD's `USB_ERR_IOERROR`.
/// Hub class requests are not served here.
#[must_use]
pub fn root_hub_request(s: &Setup, vendor: &str, config: u8) -> Option<Vec<u8>> {
    let read = |r: u8| rt::IN | rt::STANDARD | r;
    let write = |r: u8| rt::STANDARD | r;
    let index = s.value.to_le_bytes()[0];
    let ty = s.value.to_le_bytes()[1];
    match (s.request, s.request_type) {
        (code::CLEAR_FEATURE, t)
            if t == write(rt::DEVICE) || t == write(rt::INTERFACE) || t == write(rt::ENDPOINT) =>
        {
            Some(Vec::new())
        }
        (code::GET_CONFIGURATION, t) if t == read(rt::DEVICE) => Some(alloc::vec![config]),
        (code::GET_DESCRIPTOR, t) if t == read(rt::DEVICE) => match ty {
            kind::DEVICE if index == 0 => Some(ROOT_HUB_DEVICE.to_vec()),
            kind::CONFIGURATION if index == 0 => Some(ROOT_HUB_CONFIG.to_vec()),
            kind::STRING => {
                let text = match index {
                    // The language table, as FreeBSD's root hubs give it.
                    0 => "\u{1}",
                    1 => vendor,
                    2 => "XHCI root HUB",
                    _ => "",
                };
                Some(string_descriptor(text, 64))
            }
            _ => None,
        },
        (code::GET_INTERFACE, t) if t == read(rt::INTERFACE) => Some(alloc::vec![0]),
        // Self-powered.
        (code::GET_STATUS, t) if t == read(rt::DEVICE) => Some(alloc::vec![1, 0]),
        (code::GET_STATUS, t) if t == read(rt::INTERFACE) || t == read(rt::ENDPOINT) => {
            Some(alloc::vec![0, 0])
        }
        (code::SET_ADDRESS, t) if t == write(rt::DEVICE) => (s.value < 128).then(Vec::new),
        (code::SET_CONFIGURATION, t) if t == write(rt::DEVICE) => (s.value <= 1).then(Vec::new),
        // SET_DESCRIPTOR, SET_INTERFACE, SYNCH_FRAME: accepted.
        (7, t) if t == write(rt::DEVICE) => Some(Vec::new()),
        (code::SET_INTERFACE, t) if t == write(rt::INTERFACE) => Some(Vec::new()),
        (12, t) if t == write(rt::ENDPOINT) => Some(Vec::new()),
        _ => None,
    }
}

/// `usbd_get_std_packet_size`: the max packet sizes FreeBSD allows an
/// endpoint of a type at a speed: a range (interrupt, isochronous) or four
/// fixed sizes (bulk, control).
fn std_packet(t: TransferType, speed: Speed) -> (Option<u16>, [u16; 4]) {
    let hi = matches!(speed, Speed::High);
    let ss = speed.is_super();
    match t {
        TransferType::Interrupt => (
            Some(match speed {
                Speed::Low => 8,
                Speed::Full => 64,
                _ => 1024,
            }),
            [0; 4],
        ),
        TransferType::Isochronous => (
            Some(match speed {
                Speed::Low => 0,
                Speed::Full => 1023,
                _ => 1024,
            }),
            [0; 4],
        ),
        TransferType::Bulk | TransferType::Control => {
            let bulk = t == TransferType::Bulk;
            let base = match (bulk, hi, ss) {
                (true, _, true) => 1024,
                (true, true, _) | (false, _, true) => 512,
                (false, true, _) => 64,
                _ => 8,
            };
            if speed == Speed::Full {
                (None, [base, 16, 32, 64])
            } else {
                (None, [base; 4])
            }
        }
    }
}

/// xHCI's limits on a transfer (`xhci_xfer_setup`): its largest packet
/// and packets per frame.
const HC_MAX_PACKET: u16 = 0x400;
const HC_MAX_PACKET_COUNT: u16 = 16 * 3;
/// A control transfer's setup packet, which its buffer holds too.
const REQ_SIZE: u32 = 8;

/// What a FIFO transfer on an endpoint gets (`usbd_transfer_setup` with
/// `proxy_buffer`, as `usb_fs_open` asks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsLimits {
    /// `max_frame_size`: the most bytes a frame moves (packet size times
    /// packets per frame); `usb_fs_open`'s `max_packet_length`.
    pub max_frame: u32,
    /// `max_data_length`: the buffer, whole frames (plus the setup packet
    /// for control); `max_bufsize`.
    pub max_data: u32,
    /// The frames.
    pub frames: u32,
}

/// The limits of a FIFO transfer on endpoint `ep` (its address 0 and type
/// control for the default endpoint, whose max packet is `ep0_max`) at
/// `speed`, asking for `bufsize` bytes and `frames` frames (both already
/// clamped to `USB_FS_MAX_*`). `None` is FreeBSD's `USB_ERR_INVAL` (a
/// zero max packet).
#[must_use]
pub fn fs_limits(speed: Speed, ep: &Endpoint, bufsize: u32, frames: u32) -> Option<FsLimits> {
    let t = ep.transfer_type();
    let mut packet = ep.max_packet_raw;
    let mut count: u16 = 1;
    match speed {
        Speed::High => {
            if matches!(t, TransferType::Isochronous | TransferType::Interrupt) {
                count = (count + ((packet >> 11) & 3)).min(3);
            }
            packet &= 0x7ff;
        }
        Speed::Super | Speed::SuperPlus => {
            count += (packet >> 11) & 3;
            if let Some(c) = ep.companion {
                count += u16::from(c.max_burst);
            }
            if count == 0 || count > 16 {
                count = 16;
            }
            match t {
                TransferType::Control => count = 1,
                TransferType::Isochronous => {
                    let mult = ep.companion.map_or(1, |c| u16::from(c.attributes & 3) + 1);
                    count *= mult.min(3);
                }
                _ => {}
            }
            packet &= 0x7ff;
        }
        _ => {}
    }
    count = count.min(HC_MAX_PACKET_COUNT);
    if packet > HC_MAX_PACKET || packet == 0 {
        packet = HC_MAX_PACKET;
    }
    let (range, fixed) = std_packet(t, speed);
    packet = match range {
        Some(max) => packet.min(max),
        None => {
            if packet >= fixed[3] {
                fixed[3]
            } else if packet >= fixed[2] {
                fixed[2]
            } else if packet >= fixed[1] {
                fixed[1]
            } else {
                fixed[0]
            }
        }
    };
    let max_frame = u32::from(packet) * u32::from(count);
    if max_frame == 0 {
        return None;
    }
    let mut buf = if bufsize == 0 {
        if t == TransferType::Isochronous {
            max_frame.saturating_mul(frames)
        } else {
            max_frame
        }
    } else {
        bufsize
    };
    // Whole frames.
    buf = buf.checked_add(max_frame - 1)?;
    buf -= buf % max_frame;
    if t == TransferType::Control {
        buf += REQ_SIZE;
    }
    Some(FsLimits {
        max_frame,
        max_data: buf,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::Companion;

    fn ep(address: u8, attributes: u8, mps: u16, burst: Option<u8>) -> Endpoint {
        Endpoint {
            address,
            attributes,
            max_packet_raw: mps,
            interval: 0,
            companion: burst.map(|b| Companion {
                max_burst: b,
                attributes: 0,
                bytes_per_interval: 0,
            }),
            extra: Vec::new(),
        }
    }

    #[test]
    fn requests_that_need_privilege_or_an_endpoint() {
        let set_config = Setup::set_configuration(1);
        assert!(check_request(&set_config).needs_priv);
        assert!(check_request(&Setup::set_interface(0, 1)).needs_priv);
        let status = Setup::get_status();
        assert_eq!(check_request(&status), RequestCheck::default());
        let clear = Setup::clear_halt(0x81);
        let c = check_request(&clear);
        assert_eq!(
            (c.endpoint, c.clears_halt, c.needs_priv),
            (Some(0x81), true, false)
        );
        // A class request to an endpoint is not checked.
        let class = Setup {
            request_type: rt::CLASS | rt::ENDPOINT,
            ..clear
        };
        assert_eq!(check_request(&class).endpoint, None);
    }

    #[test]
    fn read_dir_is_freebsds_listing() {
        let names = ["ugen0.1", "ugen0.2", "ugen0.3"];
        let all = read_dir(names, 0, 256);
        assert_eq!(all[0], 17);
        assert_eq!(&all[1..9], b"ugen0.1\0");
        assert_eq!(&all[9..17], b"ugen0.1\0");
        assert_eq!(all.len(), 3 * 17 + 1);
        assert_eq!(*all.last().unwrap(), 0);
        // From the second, in room for one: no terminator, as FreeBSD's
        // buffer ends exactly.
        let one = read_dir(names, 1, 17);
        assert_eq!(one.len(), 17);
        assert_eq!(&one[1..9], b"ugen0.2\0");
        // Too small for any: just the end.
        assert_eq!(read_dir(names, 0, 10), [0]);
        assert_eq!(read_dir(names, 3, 10), [0]);
        assert!(read_dir(names, 0, 0).is_empty());
    }

    #[test]
    fn string_descriptors_and_releases() {
        assert_eq!(string_descriptor("\u{1}", 64), [4, 3, 1, 0]);
        let d = string_descriptor("XHCI root HUB", 64);
        assert_eq!(d[0], 28);
        assert_eq!(crate::descriptor::string(&d).unwrap(), "XHCI root HUB");
        assert_eq!(string_descriptor("abcdef", 6), [6, 3, b'a', 0, b'b', 0]);
        assert_eq!(printbcd(0x0100), "1.00");
        assert_eq!(printbcd(0x0111), "1.11");
        assert_eq!(printbcd(0), "0.00");
    }

    #[test]
    fn the_root_hub_answers_as_freebsds() {
        let cfg = crate::descriptor::Configuration::parse(&ROOT_HUB_CONFIG).unwrap();
        assert_eq!(cfg.interfaces[0].endpoints[0].address, 0x81);
        let d = crate::descriptor::DeviceDescriptor::parse(&ROOT_HUB_DEVICE).unwrap();
        assert_eq!(
            (d.usb, d.class, d.protocol, d.max_packet0),
            (0x0300, 9, 3, 9)
        );
        let get = |kind: u8, index: u8| {
            root_hub_request(&Setup::get_descriptor(kind, index, 0, 255), "(0x1b36)", 1)
        };
        assert_eq!(get(kind::DEVICE, 0).unwrap(), ROOT_HUB_DEVICE);
        assert_eq!(get(kind::DEVICE, 1), None);
        let s = get(kind::STRING, 1).unwrap();
        assert_eq!(crate::descriptor::string(&s).unwrap(), "(0x1b36)");
        assert_eq!(get(kind::STRING, 9).unwrap(), [2, 3]);
        assert_eq!(get(kind::BOS + 1, 0), None);
        assert_eq!(
            root_hub_request(&Setup::get_status(), "", 1).unwrap(),
            [1, 0]
        );
        let set = Setup::set_configuration(2);
        assert_eq!(root_hub_request(&set, "", 1), None);
    }

    #[test]
    fn fifo_limits_follow_freebsds_rules() {
        // QEMU's SuperSpeed BBB disk: 1024-byte packets, bursts of 16.
        let bulk = ep(0x81, 2, 1024, Some(15));
        let l = fs_limits(Speed::Super, &bulk, 65536, 1).unwrap();
        assert_eq!((l.max_frame, l.max_data), (16384, 65536));
        let l = fs_limits(Speed::Super, &bulk, 1000, 1).unwrap();
        assert_eq!(l.max_data, 16384);
        // High-speed bulk is 512 whatever it says.
        let l = fs_limits(Speed::High, &ep(0x02, 2, 64, None), 0, 1).unwrap();
        assert_eq!((l.max_frame, l.max_data), (512, 512));
        // A full-speed interrupt endpoint of 8 bytes, default buffer.
        let l = fs_limits(Speed::Full, &ep(0x81, 3, 8, None), 0, 1).unwrap();
        assert_eq!((l.max_frame, l.max_data), (8, 8));
        // Full-speed bulk snaps down to a standard size.
        let l = fs_limits(Speed::Full, &ep(0x81, 2, 48, None), 0, 1).unwrap();
        assert_eq!(l.max_frame, 32);
        // The default control endpoint of a high-speed device: 64 and the
        // setup packet.
        let l = fs_limits(Speed::High, &ep(0, 0, 64, None), 256, 2).unwrap();
        assert_eq!((l.max_frame, l.max_data), (64, 264));
    }
}
