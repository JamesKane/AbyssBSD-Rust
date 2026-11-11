// SPDX-License-Identifier: BSD-2-Clause
//! `ugen` and `/dev/usbctl`: the generic USB device interface `usbconfig`
//! and libusb use ([USB.md §5.5]).
//!
//! - `/dev/usbctl` (`root:operator` 0644) answers `USB_READ_DIR`, the list
//!   of `ugenB.D` names libusb enumerates devices by, and the template
//!   requests (there is no device side: the template is only kept).
//! - Each device has `/dev/usb/B.D.0`, its default control endpoint, from
//!   the moment it is addressed until it leaves, and the link `/dev/ugenB.D`
//!   to it; each endpoint number its configuration uses has
//!   `/dev/usb/B.D.E` (read for IN, write for OUT) while it is configured.
//!   All are `root:operator` 0600, as FreeBSD makes them.
//! - Opens of the control node take requests about the device (its
//!   descriptors, strings, configuration, power, port path, drivers),
//!   control transfers (`USB_DO_REQUEST`, with FreeBSD's permission
//!   rules), and the FIFO transfers ([`fs`]) libusb moves data with.
//! - Re-enumeration, a new configuration, and powering a port off and on
//!   are done by the bus's explore thread, as FreeBSD's are; the request
//!   waits for it.
//!
//! Behavior reference: sys/dev/usb/usb_dev.c, sys/dev/usb/usb_generic.c,
//! sys/dev/usb/usb_device.c (`usb_cdev_create`, `usbd_fill_deviceinfo`).
//!
//! [USB.md §5.5]: ../../../../docs/USB.md

pub(crate) mod fs;
mod sysctls;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::errno::Errno;
use abi::event::{EVFILT_READ, EVFILT_WRITE, POLLHUP, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM};
use abi::fcntl::{FREAD, FWRITE};
use abi::usb::*;
use bus::{DetachReason, NodeState};
use sched::Sleepable;
use sync::{Once, SpinLock};
use usb::descriptor::{Endpoint, kind};
use usb::{Setup, Speed};
use vfs::{CharDevice, Uio};

use crate::UsbError;
use crate::device::{Pending, UsbDevice};
use fs::{Fs, Pipe};

/// What `ugen` needs of the kernel: the caller's memory and privilege.
pub trait Kernel: Sync {
    /// Copies `dst.len()` bytes from user address `src`.
    ///
    /// # Errors
    ///
    /// `EFAULT`.
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno>;
    /// Copies `src` to user address `dst`.
    ///
    /// # Errors
    ///
    /// `EFAULT`.
    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno>;
    /// The caller's `priv_check(PRIV_DRIVER)`.
    ///
    /// # Errors
    ///
    /// `EPERM` if it lacks it.
    fn priv_driver(&self) -> Result<(), Errno>;
}

static KERNEL: Once<&'static dyn Kernel> = Once::new();

/// Hands `ugen` the kernel's side, once, before devices attach.
pub fn set_kernel(k: &'static dyn Kernel) {
    let _ = KERNEL.set(k);
}

/// The kernel's side, for other drivers whose requests carry pointers
/// (`hidraw`).
///
/// # Errors
///
/// `EFAULT` before the kernel set it.
pub fn kernel() -> Result<&'static dyn Kernel, Errno> {
    KERNEL.get().copied().ok_or(Errno::EFAULT)
}

/// `GID_OPERATOR`.
const GID_OPERATOR: u32 = 5;

/// `hw.usb.template`, which `USB_GET_TEMPLATE` and `USB_SET_TEMPLATE`
/// read and write: -1, none.
pub(crate) static TEMPLATE: AtomicI32 = AtomicI32::new(-1);
/// `hw.usb.debug` (`usb_debug`), which `USB_SETDEBUG` sets.
pub(crate) static USB_DEBUG: AtomicI32 = AtomicI32::new(0);
/// `hw.usb.ugen.debug`: at 1 and up, each request to a `ugen` node is
/// logged, as FreeBSD's `DPRINTFN(6, "cmd=...")` is at 6.
pub(crate) static UGEN_DEBUG: AtomicI32 = AtomicI32::new(0);

/// The `ugen` links, in the order they were made: `USB_READ_DIR`'s
/// list (FreeBSD's `usb_sym_head`).
static LINKS: SpinLock<Vec<String>> = SpinLock::new(Vec::new());

/// The opens of a device's FIFOs (FreeBSD's `udev->fifo`): the control
/// node's (each its own pair of FIFOs, `USB_FIFO_MAX / 2` at most), and
/// the endpoint numbers open for reading and for writing.
#[derive(Debug, Default)]
struct FifoUse {
    control: u8,
    rx: u16,
    tx: u16,
}

/// FIFO pairs per device.
const CONTROL_OPENS: u8 = 64;

static FIFOS: SpinLock<BTreeMap<(u32, u8), FifoUse>> = SpinLock::new(BTreeMap::new());

/// Makes `hw.usb` and its knobs, and `/dev/usbctl`; once, at boot.
pub fn init(s: &Sleepable) {
    sysctls::init(s);
    let _ = vfs::devices::make_dev(USB_DEVICE_NAME, 0o644, 0, GID_OPERATOR, Arc::new(Ctl));
}

fn ep_path(bus: u32, index: u8, ep: u8) -> String {
    format!("usb/{bus}.{index}.{ep}")
}

/// A device was addressed (`usb_alloc_device`): its control node and link.
pub(crate) fn device_added(dev: &UsbDevice) {
    let (bus, index) = (dev.bus, dev.index);
    let node = Node {
        bus,
        index,
        ep: 0,
        read: true,
        write: true,
    };
    let path = ep_path(bus, index, 0);
    let _ = vfs::devices::make_dev(path.as_bytes(), 0o600, 0, GID_OPERATOR, Arc::new(node));
    let link = format!("ugen{bus}.{index}");
    let _ = vfs::devices::make_dev_alias(link.as_bytes(), path.as_bytes());
    LINKS.lock().push(link);
}

/// A device left (`usb_free_device`).
pub(crate) fn device_removed(s: &Sleepable, dev: &UsbDevice) {
    let (bus, index) = (dev.bus, dev.index);
    let link = format!("ugen{bus}.{index}");
    LINKS.lock().retain(|l| *l != link);
    vfs::devices::destroy_dev(s, link.as_bytes());
    vfs::devices::destroy_dev(s, ep_path(bus, index, 0).as_bytes());
}

/// The endpoint numbers of `dev`'s configuration, every alternate setting
/// included, with whether each has an IN and an OUT endpoint
/// (`usb_cdev_create`).
fn endpoint_modes(dev: &UsbDevice) -> Vec<(u8, bool, bool)> {
    let (mut inmask, mut outmask) = (0u16, 0u16);
    for e in dev.config.interfaces.iter().flat_map(|i| &i.endpoints) {
        if e.is_in() {
            inmask |= 1 << e.number();
        } else {
            outmask |= 1 << e.number();
        }
    }
    (1..16)
        .map(|ep| (ep, inmask & (1 << ep) != 0, outmask & (1 << ep) != 0))
        .filter(|&(_, r, w)| r || w)
        .collect()
}

/// `dev` was configured: a node per endpoint number.
pub(crate) fn endpoints_added(dev: &UsbDevice) {
    for (ep, read, write) in endpoint_modes(dev) {
        let node = Node {
            bus: dev.bus,
            index: dev.index,
            ep,
            read,
            write,
        };
        let path = ep_path(dev.bus, dev.index, ep);
        let _ = vfs::devices::make_dev(path.as_bytes(), 0o600, 0, GID_OPERATOR, Arc::new(node));
    }
}

/// `dev` is being unconfigured: its endpoint nodes go, the last made
/// first (`usb_cdev_free`).
pub(crate) fn endpoints_removed(s: &Sleepable, dev: &UsbDevice) {
    for (ep, _, _) in endpoint_modes(dev).into_iter().rev() {
        vfs::devices::destroy_dev(s, ep_path(dev.bus, dev.index, ep).as_bytes());
    }
}

/// `/dev/usbctl` (`usb_static_ioctl`).
struct Ctl;

impl CharDevice for Ctl {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, _s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            USB_READ_DIR => {
                let rd = UsbReadDir::from_bytes(arg).ok_or(Errno::EINVAL)?;
                let names = LINKS.lock().clone();
                let b = usb::ugen::read_dir(
                    names.iter().map(String::as_str),
                    rd.urd_startentry,
                    rd.urd_maxlen,
                );
                if !b.is_empty() {
                    kernel()?.copy_out(rd.urd_data, &b)?;
                }
                Ok(())
            }
            USB_GET_TEMPLATE => {
                put_int(arg, TEMPLATE.load(Ordering::Relaxed));
                Ok(())
            }
            USB_SET_TEMPLATE => {
                kernel()?.priv_driver()?;
                TEMPLATE.store(get_int(arg)?, Ordering::Relaxed);
                Ok(())
            }
            _ => Err(Errno::ENOTTY),
        }
    }
}

fn get_int(arg: &[u8]) -> Result<i32, Errno> {
    arg.first_chunk::<4>()
        .map(|c| i32::from_ne_bytes(*c))
        .ok_or(Errno::EINVAL)
}

fn put_int(arg: &mut [u8], v: i32) {
    if let Some(c) = arg.first_chunk_mut::<4>() {
        *c = v.to_ne_bytes();
    }
}

/// A `/dev/usb/B.D.E` node: its location, and the directions it opens
/// for. The device is found by location at each use, as FreeBSD's
/// `usb_ref_device` finds it.
struct Node {
    bus: u32,
    index: u8,
    ep: u8,
    read: bool,
    write: bool,
}

impl CharDevice for Node {
    /// `usb_open`: the access the node allows, the device there, and a
    /// free FIFO.
    fn instance_open(
        &self,
        _s: &Sleepable,
        _nonblock: bool,
        read: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if (read && !self.read) || (write && !self.write) {
            return Err(Errno::EPERM);
        }
        let dev = crate::bus::device_at(self.bus, self.index)
            .filter(|d| !d.is_gone())
            .ok_or(Errno::ENXIO)?;
        if self.ep != 0 && !dev.is_configured() {
            return Err(Errno::ENXIO);
        }
        {
            let mut f = FIFOS.lock();
            let u = f.entry((self.bus, self.index)).or_default();
            if self.ep == 0 {
                if u.control >= CONTROL_OPENS {
                    return Err(Errno::ENOMEM);
                }
                u.control += 1;
            } else {
                let bit = 1u16 << self.ep;
                if (read && u.rx & bit != 0) || (write && u.tx & bit != 0) {
                    return Err(Errno::EBUSY);
                }
                if read {
                    u.rx |= bit;
                }
                if write {
                    u.tx |= bit;
                }
            }
        }
        Ok(Some(Arc::new(Open {
            bus: self.bus,
            index: self.index,
            ep: self.ep,
            read,
            write,
            session: dev.session,
            at_open: Arc::downgrade(&dev),
            fs: Fs::new(),
        })))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }
}

/// An open of a `ugen` node (FreeBSD's `usb_cdev_privdata` with its
/// FIFOs).
struct Open {
    bus: u32,
    index: u8,
    ep: u8,
    read: bool,
    write: bool,
    /// The device's session when opened: a device plugged in later at the
    /// same place is another.
    session: u64,
    /// The device as configured when opened: an endpoint node's FIFOs go
    /// with that configuration.
    at_open: Weak<UsbDevice>,
    fs: Arc<Fs>,
}

impl Drop for Open {
    fn drop(&mut self) {
        let mut f = FIFOS.lock();
        if let Some(u) = f.get_mut(&(self.bus, self.index)) {
            if self.ep == 0 {
                u.control = u.control.saturating_sub(1);
            } else {
                let bit = 1u16 << self.ep;
                if self.read {
                    u.rx &= !bit;
                }
                if self.write {
                    u.tx &= !bit;
                }
            }
        }
        drop(f);
        // A close without `USB_FS_UNINIT` (the process died): the running
        // transfers are cancelled by the explore thread, which may sleep.
        if let Ok(running) = self.fs.uninit(None) {
            for (pipe, address) in running {
                crate::bus::defer_abort(self.bus, pipe, address);
            }
        }
    }
}

impl Pipe for UsbDevice {
    fn submit(
        &self,
        endpoint: u8,
        setup: Option<Setup>,
        t: crate::Transfer,
    ) -> Result<(), crate::Rejected> {
        match setup {
            Some(setup) => self.control_async(setup, t),
            None => UsbDevice::submit(self, endpoint, t),
        }
    }

    fn abort(&self, s: &Sleepable, endpoint: u8) {
        UsbDevice::abort(self, s, endpoint);
    }

    fn clear_halt(&self, s: &Sleepable, endpoint: u8) -> Result<(), UsbError> {
        UsbDevice::clear_halt(self, s, endpoint)
    }

    fn reset_endpoint(&self, s: &Sleepable, endpoint: u8) {
        if let Some(slot) = self.slot() {
            let _ = self.hc().reset_endpoint(s, slot, endpoint);
        }
    }

    fn speed(&self) -> Speed {
        self.speed
    }

    fn endpoint(&self, address: u8) -> Option<Endpoint> {
        if address & 0x7f == 0 {
            return Some(control_endpoint(self));
        }
        UsbDevice::endpoint(self, address & 0x8f).cloned()
    }
}

/// The default control endpoint, as a descriptor would give it.
fn control_endpoint(dev: &UsbDevice) -> Endpoint {
    let d = usb::descriptor::DeviceDescriptor::parse(&dev.raw_desc).unwrap_or(dev.desc);
    Endpoint {
        address: 0,
        attributes: 0,
        max_packet_raw: d.ep0_max_packet(),
        interval: 0,
        companion: None,
        extra: Vec::new(),
    }
}

/// `USB_SPEED_*` for `s`.
fn speed_code(s: Speed) -> u8 {
    match s {
        Speed::Low => speed::LOW,
        Speed::Full => speed::FULL,
        Speed::High => speed::HIGH,
        Speed::Super | Speed::SuperPlus => speed::SUPER,
    }
}

/// `strlcpy` into a fixed field.
fn fill(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
    dst[n..].fill(0);
}

/// `usbd_fill_deviceinfo`.
pub(crate) fn device_info(dev: &UsbDevice) -> UsbDeviceInfo {
    let d = &dev.desc;
    let mut di = UsbDeviceInfo {
        udi_product_no: d.product,
        udi_vendor_no: d.vendor,
        udi_release_no: d.release,
        udi_power: if dev.self_powered { 0 } else { dev.power },
        udi_bus: u8::try_from(dev.bus).unwrap_or(u8::MAX),
        udi_addr: dev.address,
        udi_index: dev.index,
        udi_class: d.class,
        udi_subclass: d.subclass,
        udi_protocol: d.protocol,
        udi_config_no: if dev.is_configured() {
            dev.config.value
        } else {
            0
        },
        udi_config_index: dev.config_index(),
        udi_speed: speed_code(dev.speed),
        udi_mode: mode::HOST,
        udi_nports: dev.nports,
        udi_power_mode: dev.power_mode(),
        ..UsbDeviceInfo::default()
    };
    fill(&mut di.udi_serial, &dev.serial);
    fill(&mut di.udi_vendor, &dev.manufacturer);
    fill(&mut di.udi_product, &dev.product);
    fill(&mut di.udi_release, &usb::ugen::printbcd(d.release));
    if dev.slot().is_some() {
        di.udi_hubaddr = dev.hub_address;
        di.udi_hubindex = dev.hub_index;
        di.udi_hubport = dev.port;
    }
    di
}

/// `usbd_req_get_string_desc` into FreeBSD's 255-byte scratch area: the
/// descriptor's length read first, then the whole of it.
fn string_desc(s: &Sleepable, dev: &UsbDevice, index: u8, lang: u16) -> Result<Vec<u8>, UsbError> {
    const MAX: u16 = 255;
    let get = |len| {
        dev.control(
            s,
            Setup::get_descriptor(kind::STRING, index, lang, len),
            None,
        )
    };
    let mut b = match get(2) {
        Ok(h) if h.len() == 2 => {
            let len = u16::from(h[0]).min(MAX);
            if len < 2 {
                return Err(UsbError::Invalid);
            }
            let mut b = get(len)?;
            if b.len() < usize::from(len) {
                return Err(UsbError::Protocol("short string"));
            }
            b[0] = b[0].min(u8::try_from(len).unwrap_or(u8::MAX));
            b
        }
        // As FreeBSD retries: the whole length, short allowed.
        _ => {
            let mut b = get(MAX)?;
            if b.len() < 2 || b[0] < 2 {
                return Err(UsbError::Invalid);
            }
            b.truncate(usize::from(b[0]));
            b
        }
    };
    b[1] = kind::STRING;
    let n = usize::from(b[0]).min(b.len());
    b.truncate(n);
    Ok(b)
}

/// The configuration descriptor at `index` read from the device
/// (`usbd_req_get_config_desc_full`).
fn read_config(s: &Sleepable, dev: &UsbDevice, index: u8) -> Result<Vec<u8>, UsbError> {
    let head_len = u16::try_from(usb::descriptor::CONFIGURATION_LEN).unwrap_or(9);
    let head = dev.control(
        s,
        Setup::get_descriptor(kind::CONFIGURATION, index, 0, head_len),
        None,
    )?;
    let total = usb::descriptor::configuration_total(&head)
        .map_err(|_| UsbError::Protocol("bad configuration"))?;
    let b = dev.control(
        s,
        Setup::get_descriptor(kind::CONFIGURATION, index, 0, total),
        None,
    )?;
    if b.len() < usize::from(total) {
        return Err(UsbError::Protocol("short configuration"));
    }
    Ok(b)
}

impl Open {
    /// The device this open is on (`usb_ref_device`): `ENXIO` once it
    /// left, or, for an endpoint node, was unconfigured.
    fn device(&self) -> Result<Arc<UsbDevice>, Errno> {
        let d = crate::bus::device_at(self.bus, self.index)
            .filter(|d| !d.is_gone() && d.session == self.session)
            .ok_or(Errno::ENXIO)?;
        if self.ep != 0 {
            let same = self.at_open.upgrade().is_some_and(|o| Arc::ptr_eq(&o, &d));
            if !same || !d.is_configured() {
                return Err(Errno::ENXIO);
            }
        }
        Ok(d)
    }

    fn fflag_ok(&self, fflag: u32) -> (bool, bool) {
        (
            fflag & FREAD != 0 && self.read,
            fflag & FWRITE != 0 && self.write,
        )
    }

    /// Waits for the explore thread to finish what this request asked of
    /// `dev` (`usb_ioctl`'s wait for `re_enumerate_wait`).
    fn wait_explored(s: &Sleepable, dev: &UsbDevice) {
        while dev.pending() != Pending::Done && !dev.is_gone() {
            ::bus::manager().sleep_ms(s, 8);
        }
    }

    /// Asks the explore thread for `p` and waits for it.
    fn explore(s: &Sleepable, dev: &UsbDevice, p: Pending) -> Result<(), Errno> {
        if !dev.ask(p) && dev.pending() != p {
            return Err(Errno::EIO);
        }
        if let Some(bus) = dev.bus_ref.upgrade() {
            bus.explorer.kick();
        }
        Self::wait_explored(s, dev);
        Ok(())
    }

    /// `ugen_fs_uninit`, ignoring its error, before the configuration
    /// changes under the FIFO transfers.
    fn fs_uninit(&self, s: &Sleepable) {
        let _ = self.fs.uninit(Some(s));
    }

    /// The interface at `index` of the configuration.
    fn interface_count(dev: &UsbDevice) -> usize {
        if dev.is_configured() {
            dev.config.defaults().count()
        } else {
            0
        }
    }

    #[allow(clippy::too_many_lines, reason = "ugen_ioctl_post's switch")]
    fn request(&self, s: &Sleepable, req: u32, arg: &mut [u8], fflag: u32) -> Result<(), Errno> {
        let dev = self.device()?;
        if UGEN_DEBUG.load(Ordering::Relaxed) > 0 {
            ::bus::manager().log(format_args!(
                "ugen{}.{}: cmd=0x{req:08x}",
                self.bus, self.index
            ));
        }
        let (readable, writable) = self.fflag_ok(fflag);
        let k = kernel()?;
        match req {
            // ugen_ioctl: the fast path.
            USB_FS_COMPLETE => {
                let i = self.fs.complete(k)?;
                *arg.first_mut().ok_or(Errno::EINVAL)? = i;
                Ok(())
            }
            USB_FS_START => self.fs.start(s, k, *arg.first().ok_or(Errno::EINVAL)?),
            USB_FS_STOP => self.fs.stop(s, *arg.first().ok_or(Errno::EINVAL)?),
            // ugen_ioctl_post.
            USB_DISCOVER => {
                if let Some(b) = dev.bus_ref.upgrade() {
                    b.explorer.kick();
                }
                Ok(())
            }
            USB_SETDEBUG => {
                if !writable {
                    return Err(Errno::EPERM);
                }
                USB_DEBUG.store(get_int(arg)?, Ordering::Relaxed);
                Ok(())
            }
            USB_GET_CONFIG => {
                put_int(arg, i32::from(dev.config_index()));
                Ok(())
            }
            USB_SET_CONFIG => {
                if !writable {
                    return Err(Errno::EPERM);
                }
                let index = u8::try_from(get_int(arg)? & 0xff).unwrap_or(0);
                self.fs_uninit(s);
                if index == dev.config_index() {
                    return Ok(());
                }
                dev.next_config.store(index, Ordering::Release);
                Self::explore(s, &dev, Pending::SetConfig)
            }
            USB_GET_ALTINTERFACE => {
                let i = usize::from(*arg.first().ok_or(Errno::EINVAL)?);
                if i >= Self::interface_count(&dev) {
                    return Err(Errno::EINVAL);
                }
                // Only the first alternate setting is ever selected.
                arg[1] = 0;
                Ok(())
            }
            USB_SET_ALTINTERFACE => {
                if !writable {
                    return Err(Errno::EPERM);
                }
                self.fs_uninit(s);
                let (i, alt) = (arg[0], arg[1]);
                if usize::from(i) >= Self::interface_count(&dev) {
                    return Err(Errno::EIO);
                }
                // The one selected: nothing to do (FreeBSD optimizes the
                // request away). One that does not exist is FreeBSD's
                // `EIO`; selecting another is not supported
                // (compat-deviations).
                if alt == 0 { Ok(()) } else { Err(Errno::EIO) }
            }
            USB_GET_DEVICE_DESC => {
                arg.copy_from_slice(&dev.raw_desc);
                Ok(())
            }
            USB_GET_CONFIG_DESC => {
                if !dev.is_configured() || dev.raw_config.len() < CONFIG_DESCRIPTOR_LEN {
                    return Err(Errno::EIO);
                }
                arg.copy_from_slice(&dev.raw_config[..CONFIG_DESCRIPTOR_LEN]);
                Ok(())
            }
            USB_GET_FULL_DESC => {
                let mut g = UsbGenDescriptor::from_bytes(arg).ok_or(Errno::EINVAL)?;
                if g.ugd_data == 0 {
                    return Err(Errno::EINVAL);
                }
                let current = g.ugd_config_index == USB_UNCONFIG_INDEX
                    || g.ugd_config_index == dev.config_index();
                let bytes = if current {
                    if !dev.is_configured() {
                        return Err(Errno::ENXIO);
                    }
                    dev.raw_config.clone()
                } else {
                    read_config(s, &dev, g.ugd_config_index).map_err(|_| Errno::ENXIO)?
                };
                let total = usb::descriptor::configuration_total(&bytes).unwrap_or(0);
                let len = total.min(g.ugd_maxlen);
                let n = usize::from(len).min(bytes.len());
                g.ugd_actlen = len;
                g.ugd_offset = 0;
                k.copy_out(g.ugd_data, &bytes[..n])?;
                arg.copy_from_slice(&g.to_bytes());
                Ok(())
            }
            USB_GET_STRING_DESC => {
                let mut g = UsbGenDescriptor::from_bytes(arg).ok_or(Errno::EINVAL)?;
                let b = string_desc(s, &dev, g.ugd_string_index, g.ugd_lang_id)
                    .map_err(|_| Errno::EINVAL)?;
                let size = b.len().min(usize::from(g.ugd_maxlen));
                g.ugd_actlen = u16::try_from(size).unwrap_or(0);
                g.ugd_offset = 0;
                k.copy_out(g.ugd_data, &b[..size])?;
                arg.copy_from_slice(&g.to_bytes());
                Ok(())
            }
            USB_GET_IFACE_DRIVER => {
                let mut g = UsbGenDescriptor::from_bytes(arg).ok_or(Errno::EINVAL)?;
                if g.ugd_data == 0 || g.ugd_maxlen == 0 {
                    return Err(Errno::EINVAL);
                }
                if usize::from(g.ugd_iface_index) >= Self::interface_count(&dev) {
                    return Err(Errno::EINVAL);
                }
                let m = ::bus::manager();
                let attached = dev
                    .interface_node(g.ugd_iface_index)
                    .filter(|&c| m.state(c, s) == Some(NodeState::Attached))
                    .and_then(|c| Some((m.nameunit(c, s)?, m.description(c, s)?)));
                match attached {
                    Some((name, desc)) => {
                        let text = format!("{name}: <{desc}>");
                        let max = usize::from(g.ugd_maxlen) - 1;
                        let n = text.len().min(max);
                        let mut b = Vec::from(&text.as_bytes()[..n]);
                        b.push(0);
                        g.ugd_actlen = u16::try_from(b.len()).unwrap_or(0);
                        k.copy_out(g.ugd_data, &b)?;
                    }
                    None => k.copy_out(g.ugd_data, &[0])?,
                }
                arg.copy_from_slice(&g.to_bytes());
                Ok(())
            }
            USB_REQUEST | USB_DO_REQUEST => {
                if !writable {
                    return Err(Errno::EPERM);
                }
                self.do_request(s, k, &dev, arg)
            }
            USB_DEVICEINFO | USB_GET_DEVICEINFO => {
                arg.copy_from_slice(&device_info(&dev).to_bytes());
                Ok(())
            }
            USB_DEVICESTATS => {
                let (ok, failed) = dev.stats();
                let st = UsbDeviceStats {
                    uds_requests_ok: ok,
                    uds_requests_fail: failed,
                };
                arg.copy_from_slice(&st.to_bytes());
                Ok(())
            }
            USB_DEVICEENUMERATE => {
                k.priv_driver()?;
                self.fs_uninit(s);
                // The root hub cannot be reset; FreeBSD unconfigures and
                // configures it again, which here would only detach and
                // attach everything below it.
                if dev.slot().is_none() {
                    return Ok(());
                }
                Self::explore(s, &dev, Pending::Start)
            }
            USB_GET_PLUGTIME => {
                arg.copy_from_slice(&dev.plugtime.to_ne_bytes());
                Ok(())
            }
            USB_CLAIM_INTERFACE | USB_RELEASE_INTERFACE => Ok(()),
            USB_IFACE_DRIVER_ACTIVE => {
                let n = u8::try_from(get_int(arg)? & 0xff).unwrap_or(0);
                let m = ::bus::manager();
                let alive = dev
                    .interface_node(n)
                    .is_some_and(|c| m.state(c, s) == Some(NodeState::Attached));
                if alive { Ok(()) } else { Err(Errno::ENXIO) }
            }
            USB_IFACE_DRIVER_DETACH => {
                k.priv_driver()?;
                let n = u8::try_from(get_int(arg)? & 0xff).unwrap_or(0);
                if n == USB_IFACE_INDEX_ANY {
                    return Err(Errno::EINVAL);
                }
                detach_interface(s, &dev, n);
                Ok(())
            }
            USB_SET_POWER_MODE => self.set_power_mode(s, k, &dev, get_int(arg)?),
            USB_GET_POWER_MODE => {
                put_int(arg, i32::from(dev.power_mode()));
                Ok(())
            }
            USB_GET_DEV_PORT_PATH => {
                if dev.port_path.len() > USB_DEVICE_PORT_PATH_MAX {
                    return Err(Errno::EINVAL);
                }
                let mut p = UsbDevicePortPath {
                    udp_bus: u8::try_from(dev.bus).unwrap_or(u8::MAX),
                    udp_index: dev.index,
                    udp_port_level: u8::try_from(dev.port_path.len()).unwrap_or(0),
                    ..UsbDevicePortPath::default()
                };
                p.udp_port_no[..dev.port_path.len()].copy_from_slice(&dev.port_path);
                arg.copy_from_slice(&p.to_bytes());
                Ok(())
            }
            USB_GET_POWER_USAGE => {
                put_int(arg, i32::from(dev.power));
                Ok(())
            }
            USB_SET_PORT_ENABLE | USB_SET_PORT_DISABLE => {
                k.priv_driver()?;
                let port = u8::try_from(get_int(arg)?).map_err(|_| Errno::EINVAL)?;
                if port == 0 || dev.desc.class != usb::descriptor::class::HUB || port > dev.nports {
                    return Err(Errno::EINVAL);
                }
                let set = req == USB_SET_PORT_ENABLE;
                match dev.slot() {
                    // The root hub: enabling is a no-op on xHCI.
                    None => {
                        if !set {
                            dev.hc().root_port_disable(port);
                        }
                        Ok(())
                    }
                    Some(_) => {
                        let setup = if set {
                            usb::hub::set_port_feature(port, usb::hub::feature::PORT_ENABLE)
                        } else {
                            usb::hub::clear_port_feature(port, usb::hub::feature::PORT_ENABLE)
                        };
                        dev.control(s, setup, None)
                            .map(|_| ())
                            .map_err(|_| Errno::ENXIO)
                    }
                }
            }
            USB_FS_INIT => {
                let i = UsbFsInit::from_bytes(arg).ok_or(Errno::EINVAL)?;
                self.fs.init(self.ep, i.p_endpoints, i.ep_index_max)
            }
            USB_FS_UNINIT => {
                if arg.first().copied() != Some(0) {
                    return Err(Errno::EINVAL);
                }
                self.fs.uninit(Some(s)).map(|_| ())
            }
            USB_FS_OPEN | USB_FS_OPEN_STREAM => {
                let mut o = UsbFsOpen::from_bytes(arg).ok_or(Errno::EINVAL)?;
                let stream = if req == USB_FS_OPEN_STREAM {
                    u16::from_ne_bytes([arg[16], arg[17]])
                } else {
                    0
                };
                let pipe: Arc<dyn Pipe> = dev;
                self.fs.open(pipe, &mut o, stream, readable, writable)?;
                o.write_to(arg);
                Ok(())
            }
            USB_FS_CLOSE => self.fs.close(s, *arg.first().ok_or(Errno::EINVAL)?),
            USB_FS_CLEAR_STALL_SYNC => self.fs.clear_stall(s, *arg.first().ok_or(Errno::EINVAL)?),
            // ugen_iface_ioctl: the endpoint FIFOs' settings.
            USB_GET_RX_INTERFACE_DESC | USB_GET_TX_INTERFACE_DESC => {
                let ok = if req == USB_GET_RX_INTERFACE_DESC {
                    readable
                } else {
                    writable
                };
                if !ok {
                    return Err(Errno::EINVAL);
                }
                let iface = dev
                    .config
                    .interfaces
                    .iter()
                    .find(|i| {
                        i.alternate == 0
                            && (self.ep == 0 || i.endpoints.iter().any(|e| e.number() == self.ep))
                    })
                    .filter(|_| dev.is_configured())
                    .ok_or(Errno::EIO)?;
                arg.copy_from_slice(&[
                    9,
                    kind::INTERFACE,
                    iface.number,
                    iface.alternate,
                    u8::try_from(iface.endpoints.len()).unwrap_or(0),
                    iface.class,
                    iface.subclass,
                    iface.protocol,
                    iface.string,
                ]);
                Ok(())
            }
            USB_GET_RX_ENDPOINT_DESC | USB_GET_TX_ENDPOINT_DESC => {
                let rx = req == USB_GET_RX_ENDPOINT_DESC;
                if !(if rx { readable } else { writable }) {
                    return Err(Errno::EINVAL);
                }
                let address = if self.ep == 0 {
                    0
                } else {
                    self.ep | if rx { 0x80 } else { 0 }
                };
                let e = Pipe::endpoint(&*dev, address).ok_or(Errno::EINVAL)?;
                let mps = e.max_packet_raw.to_le_bytes();
                arg.copy_from_slice(&[
                    7,
                    kind::ENDPOINT,
                    e.address,
                    e.attributes,
                    mps[0],
                    mps[1],
                    e.interval,
                ]);
                Ok(())
            }
            USB_SET_RX_SHORT_XFER
            | USB_SET_RX_TIMEOUT
            | USB_SET_RX_BUFFER_SIZE
            | USB_SET_RX_STALL_FLAG => {
                if readable {
                    Ok(())
                } else {
                    Err(Errno::EINVAL)
                }
            }
            USB_SET_TX_FORCE_SHORT
            | USB_SET_TX_TIMEOUT
            | USB_SET_TX_BUFFER_SIZE
            | USB_SET_TX_STALL_FLAG => {
                if writable {
                    Ok(())
                } else {
                    Err(Errno::EINVAL)
                }
            }
            // No transfer is set up on the FIFOs here (no read(2) or
            // write(2) has run).
            USB_GET_RX_FRAME_SIZE | USB_GET_TX_FRAME_SIZE => Err(Errno::EINVAL),
            USB_GET_RX_BUFFER_SIZE | USB_GET_TX_BUFFER_SIZE => {
                let ok = if req == USB_GET_RX_BUFFER_SIZE {
                    readable
                } else {
                    writable
                };
                if !ok {
                    return Err(Errno::EINVAL);
                }
                // `ugen_open`'s default for the device's speed.
                let size = match dev.speed {
                    Speed::Low | Speed::Full => 64 * 32,
                    _ => 1024 * 32,
                };
                put_int(arg, size);
                Ok(())
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    /// `ugen_do_request`.
    fn do_request(
        &self,
        s: &Sleepable,
        k: &dyn Kernel,
        dev: &UsbDevice,
        arg: &mut [u8],
    ) -> Result<(), Errno> {
        let mut r = UsbCtlRequest::from_bytes(arg).ok_or(Errno::EINVAL)?;
        let q = r.ucr_request;
        let setup = Setup {
            request_type: q.bm_request_type,
            request: q.b_request,
            value: q.value(),
            index: q.index(),
            length: q.length(),
        };
        let c = usb::ugen::check_request(&setup);
        if c.needs_priv && k.priv_driver().is_err() {
            return Err(Errno::EPERM);
        }
        if let Some(e) = c.endpoint {
            if Pipe::endpoint(dev, e).is_none() {
                return Err(Errno::EPERM);
            }
            if c.clears_halt {
                Pipe::reset_endpoint(dev, s, e);
            }
        }
        let len = usize::from(setup.length);
        if len != 0 && r.ucr_data == 0 {
            return Err(Errno::EFAULT);
        }
        let out = if !setup.is_in() && len != 0 {
            let mut b = alloc::vec![0; len];
            if k.copy_in(&mut b, r.ucr_data).is_err() {
                return Err(Errno::EIO);
            }
            Some(b)
        } else {
            None
        };
        let result = dev.control(s, setup, out.as_deref());
        let (actlen, error) = match result {
            Ok(data) if setup.is_in() => {
                let short = data.len() < len && r.ucr_flags & USB_SHORT_XFER_OK == 0;
                let copied = data.is_empty() || k.copy_out(r.ucr_data, &data).is_ok();
                (data.len(), short || !copied)
            }
            Ok(_) => (len, false),
            Err(_) => (0, true),
        };
        r.ucr_actlen = u16::try_from(actlen).unwrap_or(0);
        arg.copy_from_slice(&r.to_bytes());
        if error { Err(Errno::EIO) } else { Ok(()) }
    }

    /// `ugen_set_power_mode`.
    fn set_power_mode(
        &self,
        s: &Sleepable,
        k: &dyn Kernel,
        dev: &UsbDevice,
        mode: i32,
    ) -> Result<(), Errno> {
        // The root hub has no parent hub to power it.
        if dev.slot().is_none() {
            return Err(Errno::EINVAL);
        }
        k.priv_driver()?;
        let old = dev.power_mode();
        let Ok(mut mode) = u8::try_from(mode) else {
            return Err(Errno::EINVAL);
        };
        if old == mode {
            return Ok(());
        }
        let mut job = None;
        match mode {
            power_mode::OFF => job = Some(Pending::PowerOff),
            power_mode::ON | power_mode::SAVE => {}
            // There is no selective suspend here: resume and suspend
            // leave the device running, in the automatic mode.
            power_mode::RESUME | power_mode::SUSPEND => mode = power_mode::SAVE,
            _ => return Err(Errno::EINVAL),
        }
        if old == power_mode::OFF {
            job = Some(Pending::Start);
        }
        if mode == old {
            return Ok(());
        }
        dev.set_power_mode(mode);
        if let Some(j) = job {
            Self::explore(s, dev, j)?;
        }
        Ok(())
    }
}

/// `USB_IFACE_DRIVER_DETACH` (`usb_detach_device` on one interface): its
/// driver goes, with FreeBSD's line, and none attaches to it again until
/// the device is configured anew.
fn detach_interface(s: &Sleepable, dev: &UsbDevice, n: u8) {
    let node = {
        let mut v = dev.ifaces.lock();
        v.get_mut(usize::from(n)).and_then(Option::take)
    };
    let Some(node) = node else {
        return;
    };
    let m = ::bus::manager();
    if m.state(node, s) == Some(NodeState::Attached) {
        if let Some(name) = m.nameunit(node, s) {
            let hub = m
                .parent(node, s)
                .and_then(|p| m.nameunit(p, s))
                .unwrap_or_default();
            m.log(format_args!(
                "{name}: at {hub}, port {}, addr {} (disconnected)",
                dev.port, dev.address
            ));
        }
        let _ = m.detach(node, DetachReason::Forced, s);
    }
}

impl CharDevice for Open {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        // No transfers are set up for read(2) on the endpoint FIFOs.
        self.device()?;
        Err(Errno::EIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        self.device()?;
        Err(Errno::EIO)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        self.request(s, req, arg, FREAD | FWRITE)
    }

    fn ioctl_file(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &mut [u8],
        fflag: u32,
    ) -> Result<usize, Errno> {
        self.request(s, req, arg, fflag).map(|()| 0)
    }

    /// `usb_poll`: with FIFO transfers, a completion waiting; without,
    /// the FIFOs are in error (no transfer is set up) and so ready.
    fn poll(&self, events: i16, w: Option<&Arc<dyn vfs::event::Wake>>) -> i16 {
        let all = POLLHUP | POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM;
        if self.device().is_err() {
            return events & all;
        }
        let rd = POLLIN | POLLRDNORM;
        let wr = POLLOUT | POLLWRNORM;
        if !self.fs.is_active() {
            let mut r = 0;
            if self.read {
                r |= rd;
            }
            if self.write {
                r |= wr;
            }
            return events & r;
        }
        // The transfers belong to the read FIFO if there is one.
        let mine = if self.read { rd } else { wr };
        if events & mine == 0 {
            return 0;
        }
        if let Some(w) = w {
            self.fs.notify.record(w);
        }
        if self.fs.has_completion() {
            events & mine
        } else {
            0
        }
    }

    /// `usb_kqfilter`.
    fn kqfilter(&self, kev: &abi::event::Kevent) -> Result<Box<dyn vfs::event::Filter>, Errno> {
        self.device()?;
        let ok = match kev.filter {
            EVFILT_READ => self.read,
            EVFILT_WRITE => self.write,
            _ => return Err(Errno::EOPNOTSUPP),
        };
        if !ok {
            return Err(Errno::EINVAL);
        }
        Ok(Box::new(Filter {
            fs: Arc::clone(&self.fs),
            mine: (kev.filter == EVFILT_READ) == self.read,
        }))
    }
}

/// `usb_filtops_read` and `usb_filtops_write`: active with a completion
/// waiting (or, without FIFO transfers, at once).
struct Filter {
    fs: Arc<Fs>,
    /// The transfers' FIFO is this filter's direction.
    mine: bool,
}

impl vfs::event::Filter for Filter {
    fn attach(&self, kn: &Arc<vfs::event::Knote>) -> Result<(), Errno> {
        self.fs.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &vfs::event::Knote) {
        self.fs.notify.remove(kn);
    }

    fn event(&self, _kn: &vfs::event::Knote, _note: &mut vfs::event::Note, _hint: u64) -> bool {
        if !self.fs.is_active() {
            return true;
        }
        self.mine && self.fs.has_completion()
    }
}
