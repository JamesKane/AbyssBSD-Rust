// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/bpf` (docs/NETWORKING.md §12): each open is a [`net::bpf::Desc`];
//! this is its device side. Reads sleep until the descriptor has a
//! buffer to return, or its read timeout passes; writes go out of the
//! interface it is set to; `select` and `kevent` see whether a read
//! would return, starting the read timeout as FreeBSD does; `FIOASYNC`
//! sends the signal `BIOCSRSIG` chose.
//!
//! Behavior reference: sys/net/bpf.c (`bpfopen`, `bpfread`, `bpfwrite`,
//! `bpfioctl`, `bpfpoll`, `bpfkqfilter`, `bpf_drvinit`).

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use abi::bpf::{BpfDltlist, BpfInsn, BpfProgram, MAXINSNS, bioc};
use abi::errno::Errno;
use abi::event::{
    EVFILT_READ, EVFILT_WRITE, Kevent, POLLHUP, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM,
};
use abi::ioctl::{FIOASYNC, FIOGETOWN, FIONBIO, FIONREAD, FIOSETOWN};
use abi::net::IFNAMSIZ;
use abi::signal::SIGIO;
use event::{Filter, Knote, Note, Notify, Wake};
use net::bpf::{Desc, Step, Which};
use net::{Family, Vnet};
use sched::{Sleepable, WaitQueue};
use vfs::{CharDevice, Uio};

use crate::signal::sigio::Sigio;
use crate::softclock::Callout;
use crate::uaccess;

/// `TIOCSPGRP`, `TIOCGPGRP`: the old owner requests.
const TIOCSPGRP: u32 = 0x8004_7476;
const TIOCGPGRP: u32 = 0x4004_7477;
/// The largest write (`m_get3`'s `MJUM16BYTES`).
const MAX_WRITE: usize = 16 * 1024;

/// What a descriptor wakes: readers, pollers and knotes, and the owner of
/// `FIOASYNC`'s signal.
struct Waker {
    wq: WaitQueue,
    notify: Notify,
    sigio: Sigio,
    async_on: AtomicBool,
    sig: AtomicU32,
    /// Bumped by each wakeup, for sleepers to see it.
    wakes: AtomicU64,
}

impl net::bpf::Notify for Waker {
    fn wake(&self) {
        self.wakes.fetch_add(1, Ordering::Release);
        self.wq.wake_all();
        self.notify.notify(0);
        if self.async_on.load(Ordering::Acquire) {
            self.sigio.signal_with(self.sig.load(Ordering::Relaxed));
        }
    }
}

/// The node: each open makes a descriptor.
struct Bpf;

impl CharDevice for Bpf {
    /// `bpfopen`.
    fn instance(&self, _s: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        let w = Arc::new(Waker {
            wq: WaitQueue::new(),
            notify: Notify::new(),
            sigio: Sigio::new(),
            async_on: AtomicBool::new(false),
            sig: AtomicU32::new(SIGIO),
            wakes: AtomicU64::new(0),
        });
        // Opens for writing alone cannot be told from read-write ones
        // here; either reads once it has a filter.
        let d = Desc::new(Arc::clone(&w) as Arc<dyn net::bpf::Notify>, true);
        Ok(Some(Arc::new(BpfOpen::new(d, w))))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }
}

/// An open of `/dev/bpf`; dropped on its last close (`bpf_dtor`).
struct BpfOpen {
    d: Arc<Desc>,
    w: Arc<Waker>,
    /// The read timeout `select` starts (`bd_callout`).
    timer: Pin<Box<Callout>>,
    /// The descriptor for the timer's handler.
    me: Box<Weak<Desc>>,
}

impl BpfOpen {
    fn new(d: Arc<Desc>, w: Arc<Waker>) -> Self {
        Self {
            me: Box::new(Arc::downgrade(&d)),
            d,
            w,
            timer: Callout::new(),
        }
    }

    /// Starts the read timeout a `select` asked for.
    fn arm(&self, ticks: Option<u64>) {
        if let Some(t) = ticks {
            let arg = core::ptr::from_ref::<Weak<Desc>>(&self.me).expose_provenance();
            self.timer.as_ref().reset(
                t.saturating_mul(1_000_000_000 / net::bpf::HZ),
                timed_out,
                arg,
            );
        }
    }
}

/// `bpf_timed_out`.
fn timed_out(_: &Sleepable, arg: usize) {
    // SAFETY: `arg` is the boxed `Weak` of the open that armed the timer,
    // which outlives the timer (dropping the callout drains it).
    let me = unsafe { &*core::ptr::with_exposed_provenance::<Weak<Desc>>(arg) };
    if let Some(d) = me.upgrade() {
        d.timed_out();
    }
}

impl Drop for BpfOpen {
    fn drop(&mut self) {
        self.timer.stop();
        self.d.close();
        self.w.sigio.clear();
    }
}

fn int(arg: &[u8]) -> u32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_ne_bytes)
}

fn put_u32(arg: &mut [u8], v: u32) {
    if let Some(a) = arg.get_mut(..4) {
        a.copy_from_slice(&v.to_ne_bytes());
    }
}

/// A `struct bpf_program`'s instructions, copied in.
fn program(arg: &[u8]) -> Result<Option<Vec<BpfInsn>>, Errno> {
    let mut p = BpfProgram::default();
    let n = size_of::<BpfProgram>().min(arg.len());
    // SAFETY: `BpfProgram` is integers; `arg` holds `n` of its bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(arg.as_ptr(), core::ptr::from_mut(&mut p).cast::<u8>(), n);
    }
    if p.bf_len > MAXINSNS || (p.bf_insns == 0 && p.bf_len != 0) {
        return Err(Errno::EINVAL);
    }
    if p.bf_len == 0 {
        return Ok(None);
    }
    let len = p.bf_len as usize;
    let mut raw = alloc::vec![0u8; len * size_of::<BpfInsn>()];
    uaccess::copy_in(&mut raw, p.bf_insns).map_err(|_| Errno::EINVAL)?;
    Ok(Some(
        raw.as_chunks::<8>()
            .0
            .iter()
            .map(|c| BpfInsn {
                code: u16::from_ne_bytes([c[0], c[1]]),
                jt: c[2],
                jf: c[3],
                k: u32::from_ne_bytes([c[4], c[5], c[6], c[7]]),
            })
            .collect(),
    ))
}

impl BpfOpen {
    fn request(&self, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let d = &self.d;
        d.idle();
        self.timer.stop();
        if d.forbids(req) {
            return Err(Errno::EPERM);
        }
        match req {
            FIONREAD => put_u32(arg, u32::try_from(d.nread()).unwrap_or(u32::MAX)),
            bioc::GBLEN => put_u32(arg, d.bufsize()),
            bioc::SBLEN => {
                let v = d.set_bufsize(int(arg))?;
                put_u32(arg, v);
            }
            bioc::SETF => d.set_filter(Which::Read, program(arg)?)?,
            bioc::SETFNR => d.set_filter(Which::ReadKeep, program(arg)?)?,
            bioc::SETWF => d.set_filter(Which::Write, program(arg)?)?,
            bioc::FLUSH => d.flush(),
            bioc::PROMISC => d.promisc()?,
            bioc::GDLT => put_u32(arg, d.dlt()?),
            bioc::GDLTLIST => {
                let dlt = d.dlt()?;
                let mut l = BpfDltlist::default();
                let n = size_of::<BpfDltlist>().min(arg.len());
                // SAFETY: `BpfDltlist` is integers; `arg` holds `n` of its
                // bytes.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        arg.as_ptr(),
                        core::ptr::from_mut(&mut l).cast::<u8>(),
                        n,
                    );
                }
                // `bpf_getdltlist`: one type per interface.
                if l.bfl_list != 0 {
                    if l.bfl_len < 1 {
                        return Err(Errno::ENOMEM);
                    }
                    uaccess::copy_out(l.bfl_list, &dlt.to_ne_bytes()).map_err(|_| Errno::EFAULT)?;
                }
                l.bfl_len = 1;
                put_u32(arg, l.bfl_len);
            }
            bioc::SDLT => d.set_dlt(int(arg))?,
            bioc::GETIF => {
                let (v, index) = d.interface()?;
                let name = v
                    .and_then(|v| v.net.lock().by_index(index).map(|i| i.name.clone()))
                    .unwrap_or_default();
                arg[..IFNAMSIZ].fill(0);
                let n = name.len().min(IFNAMSIZ - 1);
                arg[..n].copy_from_slice(&name.as_bytes()[..n]);
            }
            bioc::SETIF => {
                let name = &arg[..IFNAMSIZ.min(arg.len())];
                let name = &name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())];
                d.set_interface(&super::curvnet(), name)?;
            }
            bioc::SRTIMEOUT => {
                let sec = i64::from_ne_bytes(arg[..8].try_into().unwrap_or_default());
                let usec = i64::from_ne_bytes(arg[8..16].try_into().unwrap_or_default());
                d.set_timeout(sec, usec)?;
            }
            bioc::GRTIMEOUT => {
                let (sec, usec) = d.timeout();
                arg[..8].copy_from_slice(&sec.to_ne_bytes());
                arg[8..16].copy_from_slice(&usec.to_ne_bytes());
            }
            bioc::GSTATS => {
                let (recv, drop) = d.stats();
                arg[..4].copy_from_slice(&recv.to_ne_bytes());
                arg[4..8].copy_from_slice(&drop.to_ne_bytes());
            }
            bioc::IMMEDIATE => d.set_immediate(int(arg) != 0),
            bioc::VERSION => {
                arg[..2].copy_from_slice(&abi::bpf::MAJOR_VERSION.to_ne_bytes());
                arg[2..4].copy_from_slice(&abi::bpf::MINOR_VERSION.to_ne_bytes());
            }
            bioc::GHDRCMPLT => put_u32(arg, u32::from(d.hdrcmplt())),
            bioc::SHDRCMPLT => d.set_hdrcmplt(int(arg) != 0),
            bioc::GDIRECTION => put_u32(arg, d.direction()),
            bioc::SDIRECTION => d.set_direction(int(arg))?,
            bioc::GTSTAMP => put_u32(arg, d.tstamp()),
            bioc::STSTAMP => d.set_tstamp(int(arg))?,
            bioc::FEEDBACK => d.set_feedback(int(arg) != 0),
            bioc::LOCK => d.lock(),
            bioc::GRSIG => put_u32(arg, d.sig()),
            bioc::SRSIG => {
                d.set_sig(int(arg))?;
                self.w.sig.store(int(arg), Ordering::Relaxed);
            }
            bioc::GETBUFMODE => put_u32(arg, d.bufmode()),
            bioc::SETBUFMODE => d.set_bufmode(int(arg))?,
            bioc::SETVLANPCP => d.set_pcp(int(arg))?,
            // Zero-copy buffers are off (`net.bpf.zerocopy_enable` 0).
            bioc::GETZMAX | bioc::ROTZBUF | bioc::SETZBUF => return Err(Errno::EINVAL),
            FIONBIO => {}
            FIOASYNC => self.w.async_on.store(int(arg) != 0, Ordering::Release),
            FIOSETOWN => self.w.sigio.set(int(arg).cast_signed())?,
            FIOGETOWN => put_u32(arg, self.w.sigio.get().cast_unsigned()),
            TIOCSPGRP => self
                .w
                .sigio
                .set(int(arg).cast_signed().checked_neg().ok_or(Errno::EINVAL)?)?,
            TIOCGPGRP => put_u32(arg, self.w.sigio.get().wrapping_neg().cast_unsigned()),
            _ => return Err(Errno::EINVAL),
        }
        Ok(())
    }
}

/// Sends a write's frame out of its interface (`bpfwrite`'s
/// `if_output`), and with `BIOCFEEDBACK` back in.
fn send(out: net::bpf::Outgoing) -> Result<(), Errno> {
    let (Some(v), index) = out.tap.home() else {
        return Err(Errno::ENXIO);
    };
    let (link, flags) = {
        let n = v.net.lock();
        let i = n.by_index(index).ok_or(Errno::ENXIO)?;
        (i.link.0.clone().ok_or(Errno::ENXIO)?, i.flags)
    };
    let back = if out.feedback {
        let mut c = out.frame.copy()?;
        c.hdr.rcvif = index;
        Some(c)
    } else {
        None
    };
    match out.tap.dlt {
        abi::bpf::dlt::NULL => {
            // `looutput` with `AF_UNSPEC`: the family is the header's.
            let mut m = out.frame;
            let mut h = [0u8; 4];
            m.data.copy_out(0, &mut h);
            m.data.trim_front(4);
            let fam = Family::from_af(u8::try_from(u32::from_ne_bytes(h)).unwrap_or(0))
                .ok_or(Errno::EAFNOSUPPORT)?;
            net::clone::output(&v, index, fam, m)?;
        }
        _ => net::ether::output_frame(&v, index, &link, flags, out.frame)?,
    }
    if let Some(c) = back {
        feed(&v, c);
    }
    Ok(())
}

/// `if_input` for a feedback copy.
fn feed(v: &Arc<Vnet>, m: mbuf::Packet) {
    net::ether::input(v, m);
}

impl CharDevice for BpfOpen {
    /// `bpfread`.
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let timed_out = self.d.read_begin(uio.resid())?;
        let mut expired = false;
        loop {
            let g = self.w.wakes.load(Ordering::Acquire);
            match self.d.read_step(nonblock, timed_out, expired)? {
                Step::Data(buf, len) => {
                    let r = uio.copy_out(&buf[..len]);
                    self.d.read_done(buf);
                    return r;
                }
                Step::Empty => return Ok(0),
                Step::Wait(ticks) => {
                    let ns =
                        (ticks > 0).then(|| ticks.saturating_mul(1_000_000_000 / net::bpf::HZ));
                    let _w = sched::wait_message("bpf");
                    let w = &self.w;
                    let r = crate::signal::sleep(
                        s,
                        &w.wq,
                        || w.wakes.load(Ordering::Acquire) != g,
                        ns,
                    )?;
                    expired = r == crate::signal::Woke::TimedOut;
                }
            }
        }
    }

    /// `bpfwrite`.
    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        let len = uio.resid();
        if len > MAX_WRITE {
            return Err(Errno::EMSGSIZE);
        }
        let mut buf = alloc::vec![0u8; len];
        let got = uio.copy_in(&mut buf)?;
        buf.truncate(got);
        let Some(out) = self.d.write(&buf)? else {
            return Ok(0);
        };
        send(out).inspect_err(|_| self.d.write_failed())?;
        Ok(got)
    }

    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        self.request(req, arg)
    }

    fn set_async(&self, on: bool) {
        self.w.async_on.store(on, Ordering::Release);
    }

    /// `bpfpoll`.
    fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        let rw = POLLIN | POLLRDNORM | POLLOUT | POLLWRNORM;
        if self.d.interface().is_err() {
            return events & (POLLHUP | rw);
        }
        let mut revents = events & (POLLOUT | POLLWRNORM);
        if events & (POLLIN | POLLRDNORM) != 0 {
            if let Some(w) = w {
                self.w.notify.record(w);
            }
            let (ready, arm) = self.d.poll();
            if ready {
                revents |= events & (POLLIN | POLLRDNORM);
            } else {
                self.arm(arm);
            }
        }
        revents
    }

    /// `bpfkqfilter`.
    fn kqfilter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        let write = match kev.filter {
            EVFILT_READ => false,
            EVFILT_WRITE => true,
            _ => return Err(Errno::EINVAL),
        };
        Ok(Box::new(BpfFilter {
            d: Arc::clone(&self.d),
            w: Arc::clone(&self.w),
            write,
        }))
    }
}

/// `bpfread_filtops` and `bpfwrite_filtops`.
struct BpfFilter {
    d: Arc<Desc>,
    w: Arc<Waker>,
    write: bool,
}

impl Filter for BpfFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.w.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.w.notify.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        if self.write {
            let mtu = self.d.writable();
            note.kev.data = i64::from(mtu.unwrap_or(0));
            return mtu.is_some();
        }
        // The read timeout a waiting `kevent` starts is the one `poll`
        // would; its expiry wakes the knote.
        let (ready, _) = self.d.poll();
        if ready {
            note.kev.data = i64::try_from(self.d.readable()).unwrap_or(i64::MAX);
        }
        ready
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self {
            d: Arc::clone(&self.d),
            w: Arc::clone(&self.w),
            write: self.write,
        }))
    }
}

/// The time stamps' clock: uptime, and the time of day at boot.
fn clock() -> (u64, u64) {
    (crate::time::now_ns(), crate::time::realtime_base_ns())
}

/// Adds `/dev/bpf` (`bpf_drvinit`; its `bpf0` alias is not made, devfs
/// having no aliases).
pub(crate) fn init() {
    net::bpf::set_clock(clock);
    crate::fs::devfs::add(b"bpf", 0o600, 0, 0, Arc::new(Bpf));
}
