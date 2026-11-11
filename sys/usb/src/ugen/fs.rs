// SPDX-License-Identifier: BSD-2-Clause
//! The FIFO interface libusb moves data with (`USB_FS_*`): an open of a
//! device's control node describes up to [`USB_FS_XFER_MAX`] transfers in
//! an array of `usb_fs_endpoint`s in its own memory (`USB_FS_INIT`), opens
//! each on an endpoint (`USB_FS_OPEN`), starts them (`USB_FS_START`: the
//! frames' lengths and, for a write, their data are read from user memory
//! then), and collects each as it completes (`USB_FS_COMPLETE`: a read's
//! data, the lengths moved, the frames done, and the status are written
//! back then). `poll` and kqueue report a completion waiting.
//!
//! A transfer's frames go to the endpoint one after another; a short
//! frame read ends the transfer unless `USB_FS_FLAG_MULTI_SHORT_OK`, and is
//! an error (`USB_ERR_SHORT_XFER`) unless one of the short flags is set. A
//! control transfer's first frame is the setup packet, checked as
//! `USB_DO_REQUEST` checks it; the rest are its data stage.
//!
//! Behavior reference: sys/dev/usb/usb_generic.c (`ugen_fs_init`,
//! `usb_fs_open`, `ugen_fs_copy_in`, `ugen_fs_copy_out`, `ugen_ioctl`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::usb::{
    USB_FS_FLAG_CLEAR_STALL, USB_FS_FLAG_FORCE_SHORT, USB_FS_FLAG_MULTI_SHORT_OK,
    USB_FS_FLAG_SINGLE_SHORT_OK, USB_FS_MAX_BUFSIZE, USB_FS_MAX_FRAMES,
    USB_FS_MAX_FRAMES_PRE_SCALE, UsbFsEndpoint, UsbFsOpen, err,
};
use sched::Sleepable;
use sync::SpinLock;
use usb::descriptor::{Endpoint, TransferType};
use usb::ugen::FsLimits;
use usb::{Setup, Speed};
use vm::object::IoBuf;

use super::Kernel;
use crate::UsbError;
use crate::transfer::{Rejected, Transfer};

/// `USB_FS_XFER_MAX`: the most transfers one open describes.
pub(crate) const USB_FS_XFER_MAX: u8 = 126;

/// Where a FIFO transfer's frames go: a device's endpoints.
pub(crate) trait Pipe: Send + Sync {
    /// Queues `t` on endpoint `endpoint` (with `setup`, the default
    /// control endpoint).
    fn submit(&self, endpoint: u8, setup: Option<Setup>, t: Transfer) -> Result<(), Rejected>;
    /// Cancels what is queued on endpoint `endpoint`.
    fn abort(&self, s: &Sleepable, endpoint: u8);
    /// Clears the endpoint's halt on the device and the controller.
    fn clear_halt(&self, s: &Sleepable, endpoint: u8) -> Result<(), UsbError>;
    /// Restarts the controller's side of a halted endpoint (its data
    /// toggle reset), for a `CLEAR_FEATURE(ENDPOINT_HALT)` sent from
    /// userland.
    fn reset_endpoint(&self, s: &Sleepable, endpoint: u8);
    /// The device's speed.
    fn speed(&self) -> Speed;
    /// The endpoint at `address` in the active configuration, or the
    /// default control endpoint for address 0.
    fn endpoint(&self, address: u8) -> Option<Endpoint>;
}

/// A transfer's status as `usb_fs_endpoint` reports it (`usb_error_t`).
#[must_use]
pub(crate) fn status_of(e: UsbError) -> i32 {
    match e {
        UsbError::Stalled => err::STALLED,
        UsbError::TimedOut => err::TIMEOUT,
        UsbError::Cancelled | UsbError::Gone => err::CANCELLED,
        UsbError::Busy => err::IN_USE,
        UsbError::NoMemory => err::NOMEM,
        UsbError::Invalid => err::INVAL,
        UsbError::Babble
        | UsbError::Transaction
        | UsbError::Protocol(_)
        | UsbError::Controller(_) => err::IOERROR,
    }
}

/// Endpoints whose transfers are to be cancelled.
pub(crate) type Aborts = Vec<(Arc<dyn Pipe>, u8)>;

/// One open's FIFO transfers.
pub(crate) struct Fs {
    inner: SpinLock<Inner>,
    /// Completions waiting, for `poll` and kqueue.
    pub(crate) notify: vfs::event::Notify,
}

impl core::fmt::Debug for Fs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fs").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Inner {
    /// The `usb_fs_endpoint` array (a user address); 0 before
    /// `USB_FS_INIT`.
    ep_ptr: u64,
    /// The transfers, `ep_index_max` of them once initialized.
    xfers: Vec<Option<Xfer>>,
    /// Completed transfers not yet collected, in completion order.
    done: VecDeque<u8>,
    /// Tells a transfer opened later at an index from one closed there.
    next_tag: u64,
}

struct Xfer {
    tag: u64,
    pipe: Arc<dyn Pipe>,
    address: u8,
    kind: TransferType,
    max_packet: u16,
    limits: FsLimits,
    stream: u16,
    run: Run,
}

enum Run {
    /// Not started, or collected.
    Idle,
    Running(Box<Running>),
    Done(Box<Outcome>),
}

struct Running {
    setup: Option<Setup>,
    is_in: bool,
    flags: u16,
    timeout_ms: u32,
    /// The data frames' lengths as asked.
    frames: Vec<u32>,
    /// A write's frames.
    out: Vec<Vec<u8>>,
    /// The lengths moved, and a read's data, of the frames done.
    lens: Vec<u32>,
    got: Vec<Vec<u8>>,
    /// The zero-length packet that ends a forced-short write is out.
    zlp: bool,
}

struct Outcome {
    status: i32,
    /// Frames done, the setup packet counted.
    aframes: u32,
    control: bool,
    is_in: bool,
    lens: Vec<u32>,
    data: Vec<Vec<u8>>,
}

/// A frame length as a size.
fn us(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

fn u32s(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_ne_bytes(*c))
        .collect()
}

fn u64s(b: &[u8]) -> Vec<u64> {
    b.as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_ne_bytes(*c))
        .collect()
}

/// `count` elements of `size` bytes from user address `at`.
fn read_array(k: &dyn Kernel, at: u64, count: usize, size: usize) -> Result<Vec<u8>, Errno> {
    let mut b = Vec::new();
    let n = count.checked_mul(size).ok_or(Errno::EFAULT)?;
    b.try_reserve_exact(n).map_err(|_| Errno::ENOMEM)?;
    b.resize(n, 0);
    k.copy_in(&mut b, at)?;
    Ok(b)
}

fn offset(base: u64, i: usize, size: usize) -> Result<u64, Errno> {
    let off =
        u64::try_from(i.checked_mul(size).ok_or(Errno::EFAULT)?).map_err(|_| Errno::EFAULT)?;
    base.checked_add(off).ok_or(Errno::EFAULT)
}

impl Fs {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(Inner::default()),
            notify: vfs::event::Notify::new(),
        })
    }

    /// Whether `USB_FS_INIT` set it up.
    pub(crate) fn is_active(&self) -> bool {
        !self.inner.lock().xfers.is_empty()
    }

    /// Whether a completion waits (`flag_iscomplete`).
    pub(crate) fn has_completion(&self) -> bool {
        !self.inner.lock().done.is_empty()
    }

    /// `USB_FS_INIT` on an open of endpoint `ep` (only the control node's
    /// opens have FIFO transfers).
    pub(crate) fn init(&self, ep: u8, ep_ptr: u64, max: u8) -> Result<(), Errno> {
        if ep_ptr == 0 || max > USB_FS_XFER_MAX {
            return Err(Errno::EINVAL);
        }
        let mut g = self.inner.lock();
        if !g.xfers.is_empty() {
            return Err(Errno::EBUSY);
        }
        if ep != 0 || max == 0 {
            return Err(Errno::EINVAL);
        }
        let mut v = Vec::new();
        v.try_reserve_exact(usize::from(max))
            .map_err(|_| Errno::ENOMEM)?;
        v.resize_with(usize::from(max), || None);
        g.xfers = v;
        g.ep_ptr = ep_ptr;
        g.done.clear();
        Ok(())
    }

    /// `ugen_fs_uninit`: every transfer stopped and closed. The
    /// endpoints whose transfers were running are returned, to be
    /// aborted (with `s`, now; by the caller otherwise).
    pub(crate) fn uninit(&self, s: Option<&Sleepable>) -> Result<Aborts, Errno> {
        let xfers = {
            let mut g = self.inner.lock();
            if g.xfers.is_empty() {
                return Err(Errno::EINVAL);
            }
            g.ep_ptr = 0;
            g.done.clear();
            core::mem::take(&mut g.xfers)
        };
        let mut running = Vec::new();
        for x in xfers.into_iter().flatten() {
            if matches!(x.run, Run::Running(_)) {
                running.push((x.pipe, x.address));
            }
        }
        if let Some(s) = s {
            for (p, a) in &running {
                p.abort(s, *a);
            }
            running.clear();
        }
        Ok(running)
    }

    /// `usb_fs_open` (`stream` 0 for `USB_FS_OPEN`): opens transfer
    /// `o.ep_index` on endpoint `o.ep_no` of `pipe`, writing the sizes it
    /// got back into `o`. `read` and `write` are the open's access.
    pub(crate) fn open(
        &self,
        pipe: Arc<dyn Pipe>,
        o: &mut UsbFsOpen,
        stream: u16,
        read: bool,
        write: bool,
    ) -> Result<(), Errno> {
        {
            let g = self.inner.lock();
            match g.xfers.get(usize::from(o.ep_index)) {
                None => return Err(Errno::EINVAL),
                Some(Some(_)) => return Err(Errno::EBUSY),
                Some(None) => {}
            }
        }
        o.max_bufsize = o.max_bufsize.min(USB_FS_MAX_BUFSIZE);
        // Isochronous frame scaling has nothing to scale here.
        let frames = (o.max_frames & !USB_FS_MAX_FRAMES_PRE_SCALE).min(USB_FS_MAX_FRAMES);
        if frames == 0 {
            return Err(Errno::EINVAL);
        }
        let ep = pipe.endpoint(o.ep_no).ok_or(Errno::EINVAL)?;
        let kind = ep.transfer_type();
        match kind {
            TransferType::Control => {}
            // The controller driver has no isochronous transfers.
            TransferType::Isochronous => return Err(Errno::EINVAL),
            _ => {
                let ok = if ep.is_in() { read } else { write };
                if !ok {
                    return Err(Errno::EPERM);
                }
            }
        }
        let limits =
            usb::ugen::fs_limits(pipe.speed(), &ep, o.max_bufsize, frames).ok_or(Errno::ENOMEM)?;
        let mut g = self.inner.lock();
        let tag = g.next_tag;
        g.next_tag += 1;
        let slot = g
            .xfers
            .get_mut(usize::from(o.ep_index))
            .ok_or(Errno::EINVAL)?;
        if slot.is_some() {
            return Err(Errno::EBUSY);
        }
        *slot = Some(Xfer {
            tag,
            pipe,
            address: ep.address,
            kind,
            max_packet: ep.max_packet(),
            limits,
            stream,
            run: Run::Idle,
        });
        o.max_packet_length = u16::try_from(limits.max_frame).unwrap_or(u16::MAX);
        o.max_bufsize = limits.max_data;
        o.max_frames = limits.frames;
        Ok(())
    }

    /// `usb_fs_close`: the transfer at `index` goes; a running one is
    /// cancelled (its completion is dropped).
    pub(crate) fn close(&self, s: &Sleepable, index: u8) -> Result<(), Errno> {
        let x = {
            let mut g = self.inner.lock();
            let x = g
                .xfers
                .get_mut(usize::from(index))
                .ok_or(Errno::EINVAL)?
                .take()
                .ok_or(Errno::EINVAL)?;
            g.done.retain(|&i| i != index);
            x
        };
        if matches!(x.run, Run::Running(_)) {
            x.pipe.abort(s, x.address);
        }
        Ok(())
    }

    /// `usb_fs_clear_stall_sync`.
    pub(crate) fn clear_stall(&self, s: &Sleepable, index: u8) -> Result<(), Errno> {
        let (pipe, address) = {
            let g = self.inner.lock();
            let x = g
                .xfers
                .get(usize::from(index))
                .ok_or(Errno::EINVAL)?
                .as_ref()
                .ok_or(Errno::EINVAL)?;
            if matches!(x.run, Run::Running(_)) {
                return Err(Errno::EBUSY);
            }
            (Arc::clone(&x.pipe), x.address)
        };
        pipe.clear_halt(s, address).map_err(|_| Errno::ENXIO)
    }

    /// `USB_FS_STOP`: a running transfer is cancelled; it completes with
    /// `USB_ERR_CANCELLED`.
    pub(crate) fn stop(&self, s: &Sleepable, index: u8) -> Result<(), Errno> {
        let target = {
            let g = self.inner.lock();
            if usize::from(index) >= g.xfers.len() {
                return Err(Errno::EINVAL);
            }
            match &g.xfers[usize::from(index)] {
                Some(x) if matches!(x.run, Run::Running(_)) => {
                    Some((Arc::clone(&x.pipe), x.address))
                }
                _ => None,
            }
        };
        if let Some((p, a)) = target {
            p.abort(s, a);
        }
        Ok(())
    }

    /// Records the outcome of transfer `index` and queues its completion.
    fn finish(self: &Arc<Self>, index: u8, tag: u64, o: Outcome) {
        {
            let mut g = self.inner.lock();
            let Some(Some(x)) = g.xfers.get_mut(usize::from(index)) else {
                return;
            };
            if x.tag != tag {
                return;
            }
            x.run = Run::Done(Box::new(o));
            g.done.push_back(index);
        }
        self.notify.notify(0);
    }

    /// `ugen_fs_copy_in` and `usbd_transfer_start`: reads transfer
    /// `index`'s description and starts it. A description FreeBSD finds
    /// invalid completes the transfer with `USB_ERR_INVAL`.
    pub(crate) fn start(
        self: &Arc<Self>,
        s: &Sleepable,
        k: &dyn Kernel,
        index: u8,
    ) -> Result<(), Errno> {
        let (tag, pipe, address, kind, limits, ep_ptr) = {
            let g = self.inner.lock();
            let x = g
                .xfers
                .get(usize::from(index))
                .ok_or(Errno::EINVAL)?
                .as_ref()
                .ok_or(Errno::EINVAL)?;
            if matches!(x.run, Run::Running(_)) {
                return Err(Errno::EBUSY);
            }
            (
                x.tag,
                Arc::clone(&x.pipe),
                x.address,
                x.kind,
                x.limits,
                g.ep_ptr,
            )
        };
        let invalid = |control| Outcome {
            status: err::INVAL,
            aframes: 0,
            control,
            is_in: false,
            lens: Vec::new(),
            data: Vec::new(),
        };
        let control = kind == TransferType::Control;
        let mut raw = [0u8; UsbFsEndpoint::LEN];
        k.copy_in(
            &mut raw,
            offset(ep_ptr, usize::from(index), UsbFsEndpoint::LEN)?,
        )?;
        let fe = UsbFsEndpoint::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        if fe.n_frames > limits.frames || fe.n_frames == 0 {
            self.finish(index, tag, invalid(control));
            return Ok(());
        }
        let n = usize::try_from(fe.n_frames).map_err(|_| Errno::EINVAL)?;
        let buffers = u64s(&read_array(k, fe.pp_buffer, n, 8)?);
        let lengths = u32s(&read_array(k, fe.p_length, n, 4)?);
        let (setup, is_in, first) = if control {
            if lengths[0] != 8 {
                self.finish(index, tag, invalid(true));
                return Ok(());
            }
            let mut b = [0u8; 8];
            k.copy_in(&mut b, buffers[0])?;
            let r = abi::usb::UsbDeviceRequest::from_bytes(&b).ok_or(Errno::EFAULT)?;
            let setup = Setup {
                request_type: r.bm_request_type,
                request: r.b_request,
                value: r.value(),
                index: r.index(),
                length: r.length(),
            };
            let c = usb::ugen::check_request(&setup);
            let denied = (c.needs_priv && k.priv_driver().is_err())
                || c.endpoint.is_some_and(|e| pipe.endpoint(e).is_none());
            if denied {
                self.finish(index, tag, invalid(true));
                return Ok(());
            }
            if c.clears_halt
                && let Some(e) = c.endpoint
            {
                pipe.reset_endpoint(s, e);
            }
            (Some(setup), setup.is_in(), 1)
        } else {
            (None, address & 0x80 != 0, 0)
        };
        let mut rem = limits.max_data;
        let mut frames = Vec::new();
        let mut out = Vec::new();
        for f in first..n {
            let len = lengths[f];
            if len > rem {
                self.finish(index, tag, invalid(control));
                return Ok(());
            }
            rem -= len;
            frames.push(len);
            if !is_in {
                out.push(read_array(
                    k,
                    buffers[f],
                    usize::try_from(len).map_err(|_| Errno::EINVAL)?,
                    1,
                )?);
            }
        }
        if fe.flags & USB_FS_FLAG_CLEAR_STALL != 0 {
            let _ = pipe.clear_halt(s, address);
        }
        let r = Running {
            setup,
            is_in,
            flags: fe.flags,
            timeout_ms: u32::from(fe.timeout),
            frames,
            out,
            lens: Vec::new(),
            got: Vec::new(),
            zlp: false,
        };
        let t = {
            let mut g = self.inner.lock();
            let Some(Some(x)) = g.xfers.get_mut(usize::from(index)) else {
                return Err(Errno::EINVAL);
            };
            if x.tag != tag || matches!(x.run, Run::Running(_)) {
                return Err(Errno::EBUSY);
            }
            x.run = Run::Running(Box::new(r));
            let Run::Running(r) = &mut x.run else {
                return Err(Errno::EINVAL);
            };
            self.first_transfer(index, tag, r, x.stream)
        };
        self.send(&*pipe, address, setup, t);
        Ok(())
    }

    /// The transfer that starts `r`: the whole control transfer, or the
    /// first frame.
    fn first_transfer(
        self: &Arc<Self>,
        index: u8,
        tag: u64,
        r: &mut Running,
        stream: u16,
    ) -> Transfer {
        let buf = if let Some(setup) = r.setup {
            let total: usize = r.frames.iter().map(|&l| us(l)).sum();
            let len = total.min(usize::from(setup.length));
            if r.is_in {
                vec![0; len]
            } else {
                let mut v: Vec<u8> = r.out.iter().flatten().copied().collect();
                v.truncate(len);
                v
            }
        } else {
            self.frame_buf(r, 0)
        };
        self.transfer(index, tag, buf, r.timeout_ms, stream)
    }

    fn frame_buf(&self, r: &mut Running, f: usize) -> Vec<u8> {
        if r.is_in {
            vec![0; r.frames.get(f).map_or(0, |&l| us(l))]
        } else {
            r.out.get_mut(f).map(core::mem::take).unwrap_or_default()
        }
    }

    fn transfer(
        self: &Arc<Self>,
        index: u8,
        tag: u64,
        buf: Vec<u8>,
        timeout_ms: u32,
        stream: u16,
    ) -> Transfer {
        let me = Arc::clone(self);
        Transfer::new(IoBuf::from(buf), timeout_ms, move |b, res| {
            me.frame_done(index, tag, b, res);
        })
        .on_stream(stream)
    }

    fn send(&self, pipe: &dyn Pipe, address: u8, setup: Option<Setup>, t: Transfer) {
        if let Err(r) = pipe.submit(address, setup, t) {
            r.transfer.complete(Err(r.error));
        }
    }

    /// A frame (or a control transfer) of transfer `index` completed.
    fn frame_done(self: &Arc<Self>, index: u8, tag: u64, buf: IoBuf, res: Result<usize, UsbError>) {
        let mut outcome = None;
        let mut next = None;
        {
            let mut g = self.inner.lock();
            let Some(Some(x)) = g.xfers.get_mut(usize::from(index)) else {
                return;
            };
            if x.tag != tag {
                return;
            }
            let (pipe, address, stream, max_packet) =
                (Arc::clone(&x.pipe), x.address, x.stream, x.max_packet);
            let Run::Running(r) = &mut x.run else {
                return;
            };
            let control = r.setup.is_some();
            let end = |r: &mut Running, status: i32, aframes: usize| Outcome {
                status,
                aframes: u32::try_from(aframes).unwrap_or(u32::MAX),
                control,
                is_in: r.is_in,
                lens: core::mem::take(&mut r.lens),
                data: core::mem::take(&mut r.got),
            };
            match res {
                Err(e) => {
                    let done = r.lens.len() + usize::from(control);
                    outcome = Some(end(r, status_of(e), done));
                }
                Ok(moved) if control => {
                    // The data stage's bytes, frame by frame.
                    let mut left = moved;
                    let data = match buf {
                        IoBuf::Owned(v) => v,
                        _ => Vec::new(),
                    };
                    let mut at = 0;
                    for &want in &r.frames {
                        let n = us(want).min(left);
                        r.lens.push(u32::try_from(n).unwrap_or(0));
                        if r.is_in {
                            r.got
                                .push(data.get(at..at + n).map(<[u8]>::to_vec).unwrap_or_default());
                        }
                        at += n;
                        left -= n;
                    }
                    let short = moved < r.frames.iter().map(|&l| us(l)).sum::<usize>();
                    let status = if short
                        && r.is_in
                        && r.flags & (USB_FS_FLAG_SINGLE_SHORT_OK | USB_FS_FLAG_MULTI_SHORT_OK) == 0
                    {
                        err::SHORT_XFER
                    } else {
                        err::NORMAL_COMPLETION
                    };
                    let frames = r.frames.len() + 1;
                    outcome = Some(end(r, status, frames));
                }
                Ok(_) if r.zlp => {
                    let frames = r.frames.len();
                    outcome = Some(end(r, err::NORMAL_COMPLETION, frames));
                }
                Ok(moved) => {
                    let f = r.lens.len();
                    let want = r.frames.get(f).map_or(0, |&l| us(l));
                    let moved = moved.min(want);
                    r.lens.push(u32::try_from(moved).unwrap_or(0));
                    if r.is_in {
                        let mut v = match buf {
                            IoBuf::Owned(v) => v,
                            _ => Vec::new(),
                        };
                        v.truncate(moved);
                        r.got.push(v);
                    }
                    let short = moved < want && r.is_in;
                    let more = f + 1 < r.frames.len();
                    if short && r.flags & USB_FS_FLAG_MULTI_SHORT_OK == 0 {
                        let status = if r.flags & USB_FS_FLAG_SINGLE_SHORT_OK != 0 {
                            err::NORMAL_COMPLETION
                        } else {
                            err::SHORT_XFER
                        };
                        outcome = Some(end(r, status, f + 1));
                    } else if more {
                        let b = self.frame_buf(r, f + 1);
                        next = Some((
                            pipe,
                            address,
                            self.transfer(index, tag, b, r.timeout_ms, stream),
                        ));
                    } else if !r.is_in
                        && r.flags & USB_FS_FLAG_FORCE_SHORT != 0
                        && want != 0
                        && max_packet != 0
                        && want.is_multiple_of(usize::from(max_packet))
                    {
                        r.zlp = true;
                        next = Some((
                            pipe,
                            address,
                            self.transfer(index, tag, Vec::new(), r.timeout_ms, stream),
                        ));
                    } else {
                        let frames = r.frames.len();
                        outcome = Some(end(r, err::NORMAL_COMPLETION, frames));
                    }
                }
            }
        }
        if let Some(o) = outcome {
            self.finish(index, tag, o);
        }
        if let Some((pipe, address, t)) = next {
            self.send(&*pipe, address, None, t);
        }
    }

    /// `USB_FS_COMPLETE` and `ugen_fs_copy_out`: the next completed
    /// transfer's index, its results written to user memory; `EBUSY` when
    /// none waits.
    pub(crate) fn complete(&self, k: &dyn Kernel) -> Result<u8, Errno> {
        let (index, outcome, ep_ptr, max_data) = {
            let mut g = self.inner.lock();
            let index = g.done.pop_front().ok_or(Errno::EBUSY)?;
            let ep_ptr = g.ep_ptr;
            let x = g
                .xfers
                .get_mut(usize::from(index))
                .and_then(Option::as_mut)
                .ok_or(Errno::EINVAL)?;
            let o = match core::mem::replace(&mut x.run, Run::Idle) {
                Run::Done(o) => Some(o),
                other => {
                    x.run = other;
                    None
                }
            };
            (index, o, ep_ptr, x.limits.max_data)
        };
        let at = offset(ep_ptr, usize::from(index), UsbFsEndpoint::LEN)?;
        let mut raw = [0u8; UsbFsEndpoint::LEN];
        k.copy_in(&mut raw, at)?;
        let fe = UsbFsEndpoint::from_bytes(&raw).ok_or(Errno::EFAULT)?;
        // Stopped before it started: a cancellation.
        let Some(o) = outcome else {
            write_status(k, at, err::CANCELLED, 0)?;
            return Ok(index);
        };
        let mut status = o.status;
        if status == err::NORMAL_COMPLETION {
            let first = usize::from(o.control);
            let n = usize::try_from(fe.n_frames).map_err(|_| Errno::EINVAL)?;
            let lengths = u32s(&read_array(k, fe.p_length, n, 4)?);
            let buffers = if o.is_in {
                u64s(&read_array(k, fe.pp_buffer, n, 8)?)
            } else {
                Vec::new()
            };
            let mut rem = max_data;
            for f in first..n {
                let asked = lengths[f];
                if asked > rem {
                    status = err::INVAL;
                    break;
                }
                rem -= asked;
                let i = f - first;
                let len = o.lens.get(i).copied().unwrap_or(0);
                if len > asked {
                    status = err::INVAL;
                    break;
                }
                if o.is_in
                    && let Some(d) = o.data.get(i)
                {
                    k.copy_out(buffers[f], &d[..d.len().min(us(len))])?;
                }
                k.copy_out(offset(fe.p_length, f, 4)?, &len.to_ne_bytes())?;
            }
        }
        write_status(k, at, status, o.aframes)?;
        Ok(index)
    }
}

/// `ugen_fs_update`: `aFrames`, `isoc_time_complete`, and `status` of the
/// `usb_fs_endpoint` at `at`.
fn write_status(k: &dyn Kernel, at: u64, status: i32, aframes: u32) -> Result<(), Errno> {
    k.copy_out(
        offset(at, UsbFsEndpoint::A_FRAMES_AT, 1)?,
        &aframes.to_ne_bytes(),
    )?;
    k.copy_out(
        offset(at, UsbFsEndpoint::ISOC_TIME_COMPLETE_AT, 1)?,
        &0u16.to_ne_bytes(),
    )?;
    k.copy_out(
        offset(at, UsbFsEndpoint::STATUS_AT, 1)?,
        &status.to_ne_bytes(),
    )
}

#[cfg(test)]
mod tests;
