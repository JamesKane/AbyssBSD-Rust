// SPDX-License-Identifier: BSD-2-Clause
//! `hidraw`: a HID device's reports, raw, at `/dev/hidrawN` (hidapi's and
//! libfido2's FreeBSD backend).
//!
//! As FreeBSD's `hidraw.ko`, it is not part of the stock kernel's
//! drivers: it attaches once loaded (`kldload hidraw`, or `hidraw_load=YES`
//! at boot), one per `hidbus` (its index 255 child, "Raw HID Device"),
//! alongside the drivers of the bus's collections, and hears every input
//! report whole, its ID byte included.
//!
//! - `/dev/hidrawN` is `root:operator` 0600 and opens once at a time
//!   (`EBUSY`); an open starts the device's reports and queues up to 63
//!   of them.
//! - `read(2)` returns one report (hidraw mode), or, once
//!   `HIDRAW_GET_REPORT_DESC` switched to uhid mode, as many
//!   input-report-sized chunks as fit; `HIDRAW_SET_IMMED` makes it read a
//!   report with `GET_REPORT` instead. `write(2)` sends an output report
//!   on the interrupt OUT pipe, or by `SET_REPORT` without one.
//! - The uhid-compatible requests (`HIDRAW_GET_REPORT_DESC`,
//!   `HIDRAW_GET_REPORT`, `HIDRAW_SET_REPORT`, `HIDRAW_SET_IMMED`,
//!   `HIDRAW_GET_REPORT_ID`, `HIDRAW_GET_DEVICEINFO`) and the
//!   Linux-compatible ones (`HIDIOCGRDESCSIZE`, `HIDIOCGRDESC`,
//!   `HIDIOCGRAWINFO`, `HIDIOCGRAWNAME`, `HIDIOCGRAWPHYS`,
//!   `HIDIOCGRAWUNIQ`, and the feature, input, and output report gets
//!   and sets); anything else is `EINVAL`, as FreeBSD's.
//! - `poll` and kqueue: readable with a report queued; writable always.
//!
//! Behavior reference: sys/dev/hid/hidraw.c, sys/dev/hid/hidbus.c
//! (`hidbus_fill_rdesc_info`, `hidbus_write`), sys/dev/hid/hid.c
//! (`hid_report_size_max`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use abi::errno::Errno;
use abi::event::{EV_EOF, EVFILT_READ, POLLHUP, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM};
use abi::fcntl::{FREAD, FWRITE};
use abi::hidraw::*;
use abi::usb::UsbGenDescriptor;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::HidId;
use hid::{Descriptor, Kind};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vfs::{CharDevice, Uio};

use crate::{HidChild, RAW_INDEX, TransportError};

/// `GID_OPERATOR`.
const GID_OPERATOR: u32 = 5;
/// `HID_RSIZE_MAX`: the largest report.
const HID_RSIZE_MAX: usize = 1024;

/// The sizes and IDs of a descriptor's reports (`struct hid_rdesc_info`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RdescInfo {
    pub isize: usize,
    pub osize: usize,
    pub fsize: usize,
    pub iid: u8,
    pub oid: u8,
    pub fid: u8,
}

/// `hid_report_size_max`: the bytes from the first item of `kind` to the
/// end of the furthest, a byte more if any is numbered, and the first
/// report ID met.
fn size_max(d: &Descriptor, kind: Kind) -> (usize, u8) {
    let mut lo = u32::MAX;
    let mut hi = 0u32;
    let mut id = 0;
    for f in d.fields.iter().filter(|f| f.kind == kind) {
        if f.report_id != 0 && id == 0 {
            id = f.report_id;
        }
        lo = lo.min(f.bit);
        hi = hi.max(f.bit + u32::from(f.size) * u32::from(f.count));
    }
    let mut bits = hi.saturating_sub(lo);
    if id != 0 {
        bits += 8;
    }
    let n = usize::try_from(bits.div_ceil(8)).unwrap_or(HID_RSIZE_MAX);
    (n.min(HID_RSIZE_MAX), id)
}

impl RdescInfo {
    /// `hidbus_fill_rdesc_info` of a parsed descriptor; without one, the
    /// largest sizes, so that reports still pass.
    #[must_use]
    pub fn of(d: &Descriptor, raw_len: usize) -> Self {
        if raw_len == 0 {
            return Self {
                isize: HID_RSIZE_MAX,
                osize: HID_RSIZE_MAX,
                fsize: HID_RSIZE_MAX,
                ..Self::default()
            };
        }
        let (isize, iid) = size_max(d, Kind::Input);
        let (osize, oid) = size_max(d, Kind::Output);
        let (fsize, fid) = size_max(d, Kind::Feature);
        Self {
            isize,
            osize,
            fsize,
            iid,
            oid,
            fid,
        }
    }

    /// The size and ID of a report type (`HID_*_REPORT`).
    fn of_kind(&self, kind: u8) -> Option<(usize, u8)> {
        match kind {
            HID_INPUT_REPORT => Some((self.isize, self.iid)),
            HID_OUTPUT_REPORT => Some((self.osize, self.oid)),
            HID_FEATURE_REPORT => Some((self.fsize, self.fid)),
            _ => None,
        }
    }
}

/// What the caller's memory and privilege are, for requests that carry
/// pointers (`ugen`'s hook).
fn kernel() -> Result<&'static dyn usbus::ugen::Kernel, Errno> {
    usbus::ugen::kernel()
}

/// An open's state (`struct hidraw_softc`'s per-open part).
#[derive(Default)]
struct State {
    open: bool,
    read: bool,
    write: bool,
    queue: VecDeque<Vec<u8>>,
    immed: bool,
    uhid: bool,
}

/// One `hidraw` device.
struct Raw {
    child: Arc<HidChild>,
    info: RdescInfo,
    /// `hidrawN`.
    name: String,
    state: SpinLock<State>,
    wait: WaitQueue,
    notify: vfs::event::Notify,
    gone: AtomicBool,
}

impl Raw {
    /// `hidraw_intr`: a report queued unless the queue is full.
    fn intr(&self, report: &[u8]) {
        {
            let mut st = self.state.lock();
            if !st.open || st.queue.len() >= HIDRAW_BUFFER_SIZE - 1 {
                return;
            }
            st.queue.push_back(report.to_vec());
        }
        self.wait.wake_all();
        self.notify.notify(0);
    }

    fn largest(&self) -> usize {
        self.info.isize.max(self.info.osize).max(self.info.fsize)
    }

    /// `hid_get_report` of type `kind` and `id`, `len` bytes.
    fn get_report(&self, s: &Sleepable, kind: u8, id: u8, len: usize) -> Result<Vec<u8>, Errno> {
        let len = u16::try_from(len).map_err(|_| Errno::ENOBUFS)?;
        self.child
            .transport()
            .get_report(s, kind, id, len)
            .map_err(|TransportError| Errno::EIO)
    }

    fn set_report(&self, s: &Sleepable, kind: u8, id: u8, data: &[u8]) -> Result<(), Errno> {
        self.child
            .transport()
            .set_report(s, kind, id, data)
            .map_err(|TransportError| Errno::EIO)
    }

    /// `hidbus_write`: the interrupt OUT pipe, or `SET_REPORT` without
    /// one.
    fn write_report(&self, s: &Sleepable, data: &[u8]) -> Result<(), Errno> {
        match self.child.transport().write(s, data) {
            Some(r) => r.map_err(|TransportError| Errno::EIO),
            None => {
                let id = if self.info.oid != 0 {
                    data.first().copied().unwrap_or(0)
                } else {
                    0
                };
                self.set_report(s, HID_OUTPUT_REPORT, id, data)
            }
        }
    }
}

/// `/dev/hidrawN`.
struct Node(Arc<Raw>);

impl CharDevice for Node {
    /// `hidraw_open`: once at a time; reports start.
    fn instance_open(
        &self,
        _s: &Sleepable,
        _nonblock: bool,
        read: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        let raw = &self.0;
        if raw.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        {
            let mut st = raw.state.lock();
            if st.open {
                return Err(Errno::EBUSY);
            }
            *st = State {
                open: true,
                read,
                write,
                ..State::default()
            };
        }
        raw.child.intr_start();
        Ok(Some(Arc::new(Open(Arc::clone(raw)))))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::EIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::EIO)
    }
}

/// An open of `/dev/hidrawN`.
struct Open(Arc<Raw>);

impl Drop for Open {
    /// `hidraw_dtor`.
    fn drop(&mut self) {
        let raw = &self.0;
        raw.child.intr_stop();
        *raw.state.lock() = State::default();
        raw.notify.drain();
    }
}

fn fill(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

/// `strlcpy` of `s` into `arg`; the call returns the bytes it took, its
/// NUL included.
fn string_out(arg: &mut [u8], s: &str) -> usize {
    arg.fill(0);
    fill(arg, s);
    (s.len() + 1).min(arg.len())
}

impl Open {
    fn gone(&self) -> Result<(), Errno> {
        if self.0.gone.load(Ordering::Acquire) {
            Err(Errno::EIO)
        } else {
            Ok(())
        }
    }

    #[allow(clippy::too_many_lines, reason = "hidraw_ioctl's switch")]
    fn request(&self, s: &Sleepable, cmd: u32, arg: &mut [u8], fflag: u32) -> Result<usize, Errno> {
        self.gone()?;
        let raw = &self.0;
        let readable = fflag & FREAD != 0;
        let writable = fflag & FWRITE != 0;
        let data = raw.child.bus_data();
        match cmd {
            abi::ioctl::FIONBIO => return Ok(0),
            HIDRAW_GET_REPORT_DESC => {
                if data.raw.is_empty() {
                    return Err(Errno::EOPNOTSUPP);
                }
                raw.state.lock().uhid = true;
                let mut g = UsbGenDescriptor::from_bytes(arg).ok_or(Errno::EINVAL)?;
                let size = data.raw.len().min(usize::from(g.ugd_maxlen));
                g.ugd_actlen = u16::try_from(size).unwrap_or(0);
                arg.copy_from_slice(&g.to_bytes());
                if g.ugd_data != 0 {
                    kernel()?.copy_out(g.ugd_data, &data.raw[..size])?;
                }
                return Ok(0);
            }
            HIDRAW_SET_REPORT_DESC => {
                if !writable {
                    return Err(Errno::EPERM);
                }
                kernel()?.priv_driver()?;
                // The bus's children would be enumerated again from the
                // new descriptor; not done here (compat-deviations).
                return Err(Errno::EOPNOTSUPP);
            }
            HIDRAW_SET_IMMED => {
                if !readable {
                    return Err(Errno::EPERM);
                }
                let on = arg
                    .first_chunk::<4>()
                    .is_some_and(|c| i32::from_ne_bytes(*c) != 0);
                if on {
                    raw.get_report(s, HID_INPUT_REPORT, raw.info.iid, raw.info.isize)
                        .map_err(|_| Errno::EOPNOTSUPP)?;
                }
                raw.state.lock().immed = on;
                return Ok(0);
            }
            HIDRAW_GET_REPORT => {
                if !readable {
                    return Err(Errno::EPERM);
                }
                let mut g = UsbGenDescriptor::from_bytes(arg).ok_or(Errno::EINVAL)?;
                let (size, mut id) = raw.info.of_kind(g.ugd_report_type).ok_or(Errno::EINVAL)?;
                let k = kernel()?;
                if id != 0 {
                    let mut b = [0u8];
                    k.copy_in(&mut b, g.ugd_data)?;
                    id = b[0];
                }
                let size = size.min(usize::from(g.ugd_maxlen));
                let r = raw.get_report(s, g.ugd_report_type, id, size);
                let actlen = match &r {
                    Ok(v) => {
                        k.copy_out(g.ugd_data, v)?;
                        v.len()
                    }
                    Err(_) => 0,
                };
                g.ugd_actlen = u16::try_from(actlen).unwrap_or(0);
                arg.copy_from_slice(&g.to_bytes());
                return r.map(|_| 0);
            }
            HIDRAW_SET_REPORT => {
                if !writable {
                    return Err(Errno::EPERM);
                }
                let g = UsbGenDescriptor::from_bytes(arg).ok_or(Errno::EINVAL)?;
                let (size, id) = raw.info.of_kind(g.ugd_report_type).ok_or(Errno::EINVAL)?;
                let size = size.min(usize::from(g.ugd_maxlen));
                let mut b = vec![0; size];
                kernel()?.copy_in(&mut b, g.ugd_data)?;
                let id = if id != 0 {
                    b.first().copied().unwrap_or(0)
                } else {
                    0
                };
                raw.set_report(s, g.ugd_report_type, id, &b)?;
                return Ok(0);
            }
            HIDRAW_GET_REPORT_ID => {
                arg.fill(0);
                return Ok(0);
            }
            HIDRAW_GET_DEVICEINFO => {
                let id = data.ident;
                let mut d = HidrawDeviceInfo {
                    hdi_product: id.product,
                    hdi_vendor: id.vendor,
                    hdi_version: id.version,
                    hdi_bustype: id.bus,
                    ..HidrawDeviceInfo::default()
                };
                fill(&mut d.hdi_name, &data.name);
                fill(&mut d.hdi_phys, &raw.name);
                fill(&mut d.hdi_uniq, &data.serial);
                fill(
                    &mut d.hdi_release,
                    &format!("{:x}.{:02x}", id.version >> 8, id.version & 0xff),
                );
                arg.copy_from_slice(&d.to_bytes());
                return Ok(0);
            }
            HIDIOCGRDESCSIZE => {
                arg.copy_from_slice(&i32::from(data.rdesc_size).to_ne_bytes());
                return Ok(0);
            }
            HIDIOCGRDESC => {
                // The argument is the user's pointer itself.
                let at = arg
                    .first_chunk::<8>()
                    .map(|c| u64::from_ne_bytes(*c))
                    .ok_or(Errno::EINVAL)?;
                let k = kernel()?;
                let mut size = [0u8; 4];
                k.copy_in(&mut size, at)?;
                let size = usize::try_from(u32::from_ne_bytes(size)).unwrap_or(usize::MAX);
                if size >= HID_MAX_DESCRIPTOR_SIZE {
                    return Err(Errno::EINVAL);
                }
                raw.state.lock().uhid = false;
                let n = size.min(data.raw.len());
                k.copy_out(at + HIDRAW_REPORT_DESCRIPTOR_VALUE_AT, &data.raw[..n])?;
                return Ok(0);
            }
            HIDIOCGRAWINFO => {
                let id = data.ident;
                let d = HidrawDevinfo {
                    bustype: u32::from(id.bus),
                    vendor: id.vendor.cast_signed(),
                    product: id.product.cast_signed(),
                };
                arg.copy_from_slice(&d.to_bytes());
                return Ok(0);
            }
            _ => {}
        }
        let len = arg.len();
        match base(cmd) {
            HIDIOCGRAWNAME_BASE => Ok(string_out(arg, &data.name)),
            HIDIOCGRAWPHYS_BASE => Ok(string_out(arg, &raw.name)),
            HIDIOCGRAWUNIQ_BASE => Ok(string_out(arg, &data.serial)),
            b @ (HIDIOCSFEATURE_BASE | HIDIOCSINPUT_BASE | HIDIOCSOUTPUT_BASE) => {
                if !writable {
                    return Err(Errno::EPERM);
                }
                if len < 2 {
                    return Err(Errno::EINVAL);
                }
                let id = arg[0];
                let body = if id == 0 { &arg[1..] } else { &arg[..] };
                let kind = match b {
                    HIDIOCSFEATURE_BASE => HID_FEATURE_REPORT,
                    HIDIOCSINPUT_BASE => HID_INPUT_REPORT,
                    _ => HID_OUTPUT_REPORT,
                };
                raw.set_report(s, kind, id, body)?;
                Ok(len)
            }
            b @ (HIDIOCGFEATURE_BASE | HIDIOCGINPUT_BASE | HIDIOCGOUTPUT_BASE) => {
                if !readable {
                    return Err(Errno::EPERM);
                }
                if len < 2 {
                    return Err(Errno::EINVAL);
                }
                let id = arg[0];
                let at = usize::from(id == 0);
                let kind = match b {
                    HIDIOCGFEATURE_BASE => HID_FEATURE_REPORT,
                    HIDIOCGINPUT_BASE => HID_INPUT_REPORT,
                    _ => HID_OUTPUT_REPORT,
                };
                let v = raw.get_report(s, kind, id, len - at)?;
                let n = v.len().min(len - at);
                arg[at..at + n].copy_from_slice(&v[..n]);
                Ok(n + at)
            }
            _ => Err(Errno::EINVAL),
        }
    }
}

impl CharDevice for Open {
    /// `hidraw_read`.
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        self.gone()?;
        let raw = &self.0;
        let (immed, uhid) = {
            let st = raw.state.lock();
            (st.immed, st.uhid)
        };
        if immed {
            let v = raw.get_report(s, HID_INPUT_REPORT, raw.info.iid, raw.info.isize)?;
            let n = v.len().min(uio.resid());
            return uio.copy_out(&v[..n]);
        }
        if raw.state.lock().queue.is_empty() {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            raw.wait
                .wait_until_interruptible(s, || {
                    !raw.state.lock().queue.is_empty() || raw.gone.load(Ordering::Acquire)
                })
                .map_err(|_| Errno::EINTR)?;
            self.gone()?;
        }
        let mut done = 0;
        loop {
            if uio.resid() == 0 {
                break;
            }
            let Some(mut r) = raw.state.lock().queue.pop_front() else {
                break;
            };
            if uhid {
                // Whole input reports, zero padded.
                r.resize(raw.info.isize, 0);
            }
            let n = r.len().min(uio.resid());
            done += uio.copy_out(&r[..n])?;
            // Reports go one by one in hidraw mode, their lengths
            // differing.
            if !uhid {
                break;
            }
        }
        Ok(done)
    }

    /// `hidraw_write`.
    fn write(&self, s: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        self.gone()?;
        let raw = &self.0;
        if raw.info.osize == 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let total = uio.resid();
        let uhid = raw.state.lock().uhid;
        let buf = if uhid {
            if total != raw.info.osize {
                return Err(Errno::EINVAL);
            }
            let mut b = vec![0; total];
            uio.copy_in(&mut b)?;
            b
        } else {
            if total < 2 {
                return Err(Errno::EINVAL);
            }
            let mut id = [0u8];
            uio.copy_in(&mut id)?;
            // A leading 0 is no report ID: dropped.
            let size = if id[0] != 0 { total } else { total - 1 };
            let max = raw
                .child
                .transport()
                .write_max(raw.info.osize, raw.largest());
            if size > max {
                return Err(Errno::ENOBUFS);
            }
            let mut b = vec![0; size];
            let off = usize::from(id[0] != 0);
            if off == 1 {
                b[0] = id[0];
            }
            uio.copy_in(&mut b[off..])?;
            b
        };
        raw.write_report(s, &buf)?;
        Ok(total)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let st = self.0.state.lock();
        let fflag = if st.read { FREAD } else { 0 } | if st.write { FWRITE } else { 0 };
        drop(st);
        self.request(s, req, arg, fflag).map(|_| ())
    }

    fn ioctl_file(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &mut [u8],
        fflag: u32,
    ) -> Result<usize, Errno> {
        self.request(s, req, arg, fflag)
    }

    /// `hidraw_poll`.
    fn poll(&self, events: i16, w: Option<&Arc<dyn vfs::event::Wake>>) -> i16 {
        let raw = &self.0;
        if raw.gone.load(Ordering::Acquire) {
            return POLLHUP;
        }
        let (read, write) = {
            let st = raw.state.lock();
            (st.read, st.write)
        };
        let mut r = 0;
        if write {
            r |= events & (POLLOUT | POLLWRNORM);
        }
        if read && events & (POLLIN | POLLRDNORM) != 0 {
            if let Some(w) = w {
                raw.notify.record(w);
            }
            if !raw.state.lock().queue.is_empty() {
                r |= events & (POLLIN | POLLRDNORM);
            }
        }
        r
    }

    /// `hidraw_kqfilter`: reads only.
    fn kqfilter(&self, kev: &abi::event::Kevent) -> Result<Box<dyn vfs::event::Filter>, Errno> {
        if self.0.gone.load(Ordering::Acquire) {
            return Err(Errno::ENXIO);
        }
        if kev.filter != EVFILT_READ || !self.0.state.lock().read {
            return Err(Errno::EINVAL);
        }
        Ok(Box::new(Filter(Arc::clone(&self.0))))
    }
}

/// `hidraw_filterops_read`.
struct Filter(Arc<Raw>);

impl vfs::event::Filter for Filter {
    fn attach(&self, kn: &Arc<vfs::event::Knote>) -> Result<(), Errno> {
        self.0.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &vfs::event::Knote) {
        self.0.notify.remove(kn);
    }

    /// `hidraw_kqread`.
    fn event(&self, _kn: &vfs::event::Knote, note: &mut vfs::event::Note, _hint: u64) -> bool {
        if self.0.gone.load(Ordering::Acquire) {
            note.kev.flags |= EV_EOF;
            return true;
        }
        !self.0.state.lock().queue.is_empty()
    }
}

/// The `hidraw` driver.
pub struct Hidraw {
    raw: Arc<Raw>,
}

impl core::fmt::Debug for Hidraw {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hidraw")
            .field("name", &self.raw.name)
            .finish()
    }
}

impl Driver for Hidraw {
    const NAME: &'static str = "hidraw";
    const IDS: Ids = Ids::Hid(&[HidId::index(255)]);

    /// `hidbus_set_desc(self, "Raw HID Device")`: the device's name and
    /// the suffix, unless the name has it.
    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Hid(_, name) if name.to_ascii_lowercase().contains("raw hid device") => {
                name.clone()
            }
            Ident::Hid(_, name) => format!("{name} Raw HID Device"),
            _ => String::new(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<HidChild>().ok_or(AttachError::Decline)?;
        if child.index != RAW_INDEX {
            return Err(AttachError::Decline);
        }
        let data = child.bus_data();
        let info = RdescInfo::of(child.descriptor(), data.raw.len());
        Ok(Self {
            raw: Arc::new(Raw {
                child: Arc::clone(&child),
                info,
                name: format!("hidraw{}", cx.unit()),
                state: SpinLock::new(State::default()),
                wait: WaitQueue::new(),
                notify: vfs::event::Notify::new(),
                gone: AtomicBool::new(false),
            }),
        })
    }

    fn start(&self, _cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let raw = &self.raw;
        if raw.child.bus_data().raw.is_empty() {
            ::bus::manager().log(format_args!("{}: no report descriptor", raw.name));
        }
        let weak: Weak<Raw> = Arc::downgrade(raw);
        raw.child.listen_raw(Some(Arc::new(move |r: &[u8]| {
            if let Some(raw) = weak.upgrade() {
                raw.intr(r);
            }
        })));
        vfs::devices::make_dev(
            raw.name.as_bytes(),
            0o600,
            0,
            GID_OPERATOR,
            Arc::new(Node(Arc::clone(raw))),
        )
        .ok_or(AttachError::Device("Can not create character device"))?;
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let raw = &self.raw;
        raw.gone.store(true, Ordering::Release);
        raw.child.listen_raw(None);
        raw.wait.wake_all();
        raw.notify.notify(0);
        vfs::devices::destroy_dev(cx.sleepable(), raw.name.as_bytes());
    }
}

/// The declaration for the build's driver table.
pub const HIDRAW: DriverDecl = DriverDecl::of::<Hidraw>();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_sizes_are_freebsds() {
        // The boot mouse: a 3-byte input report, no output or feature.
        let d = Descriptor::parse(hid::BOOT_MOUSE).unwrap();
        let i = RdescInfo::of(&d, hid::BOOT_MOUSE.len());
        assert_eq!((i.isize, i.osize, i.fsize, i.iid), (3, 0, 0, 0));
        // The boot keyboard: 8 bytes in, 1 out.
        let d = Descriptor::parse(hid::BOOT_KEYBOARD).unwrap();
        let i = RdescInfo::of(&d, hid::BOOT_KEYBOARD.len());
        assert_eq!((i.isize, i.osize), (8, 1));
        // A numbered report: the ID byte counts.
        let numbered = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x02, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02,
            0xc0,
        ];
        let d = Descriptor::parse(&numbered).unwrap();
        let i = RdescInfo::of(&d, numbered.len());
        assert_eq!((i.isize, i.iid), (3, 2));
        // No descriptor: the largest sizes.
        let i = RdescInfo::of(&Descriptor::default(), 0);
        assert_eq!(i.isize, HID_RSIZE_MAX);
        assert_eq!(i.of_kind(HID_OUTPUT_REPORT), Some((HID_RSIZE_MAX, 0)));
        assert_eq!(i.of_kind(9), None);
    }

    #[test]
    fn strings_are_cut_as_strlcpy_cuts_them() {
        let mut b = [0xffu8; 6];
        assert_eq!(string_out(&mut b, "hidraw0"), 6);
        assert_eq!(&b, b"hidra\0");
        let mut b = [0xffu8; 16];
        assert_eq!(string_out(&mut b, "QEMU"), 5);
        assert_eq!(&b[..5], b"QEMU\0");
    }
}
