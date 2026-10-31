// SPDX-License-Identifier: BSD-2-Clause
//! The Berkeley Packet Filter (docs/NETWORKING.md §12): taps on
//! interfaces, and the descriptors `/dev/bpf` opens on them.
//!
//! Each interface has a [`Tap`] with its data-link type. A [`Desc`] set to
//! an interface sees each packet the interface sends or receives, runs
//! its filter over it, and copies what the filter takes, after a header,
//! into its store buffer. When the store buffer is full it becomes the
//! hold buffer, which `read(2)` returns whole, and the free buffer becomes
//! the store; a packet with no buffer to go to is dropped and counted.
//! A write goes out on the interface as a frame.
//!
//! A descriptor set to an interface but never given a filter only writes
//! (`bd_writer`, as `dhclient` uses one), so taps skip it until its first
//! filter that is not `pcap_open_live`'s snapshot length.
//!
//! Sleeping, signals, and select are the kernel's: these types say when
//! to wait and for how long, and call [`Notify::wake`] when a reader
//! should look again.
//!
//! Behavior reference: sys/net/bpf.c, sys/net/bpf_buffer.c,
//! sys/net/bpfdesc.h.

pub mod filter;

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use abi::bpf::{BpfInsn, MAXINSNS, bufmode, dir, dlt, ts, wordalign};
use abi::errno::Errno;
use abi::net::iff;
use mbuf::{Packet, flags};
use sync::{Once, SpinLock};

use crate::Vnet;
use filter::{Prefixed, Source};

/// `net.bpf.bufsize`, `net.bpf.maxbufsize` (`BPF_MAXBUFSIZE`), and
/// `BPF_MINBUFSIZE`.
pub const BUFSIZE: u32 = 4096;
pub const MAXBUFSIZE: u32 = 0x80000;
pub const MINBUFSIZE: u32 = 32;
/// Ticks per second, for the read timeout (`hz`).
pub const HZ: u64 = 1000;
/// `BPF_PRIO_MAX`.
const PRIO_MAX: u32 = 7;
/// `NSIG`.
const NSIG: u32 = 128;
/// `ETHER_HDR_LEN`.
const ETHER_HDR_LEN: usize = 14;

/// The clock stamps come from: nanoseconds since boot, and the time of
/// boot in nanoseconds since the epoch.
static CLOCK: Once<fn() -> (u64, u64)> = Once::new();

/// Sets the clock.
pub fn set_clock(f: fn() -> (u64, u64)) {
    let _ = CLOCK.set(f);
}

/// Something to tell when a descriptor has data to read, or was detached:
/// its sleeping readers, `select` and `kevent` waiters, and `FIOASYNC`
/// signals.
pub trait Notify: Send + Sync {
    fn wake(&self);
}

/// Where a packet goes past an interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    In,
    Out,
}

/// An interface's tap (`struct bpf_if`).
pub struct Tap {
    /// Its data-link type and link-header length (`bif_dlt`,
    /// `bif_hdrlen`).
    pub dlt: u32,
    pub hdrlen: u32,
    /// Its interface: the VNET it is in, and its index there.
    home: SpinLock<(Weak<Vnet>, u16)>,
    /// Descriptors set to it, readers and writers alike.
    descs: SpinLock<Vec<Arc<Desc>>>,
    /// How many of them read (`bif_dlist` not empty).
    readers: AtomicUsize,
}

impl core::fmt::Debug for Tap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tap")
            .field("dlt", &self.dlt)
            .finish_non_exhaustive()
    }
}

impl Tap {
    /// A tap for an Ethernet interface (`bpfattach(ifp, DLT_EN10MB,
    /// ETHER_HDR_LEN)`).
    #[must_use]
    pub fn ether() -> Arc<Self> {
        Self::new(dlt::EN10MB, 14)
    }

    /// A tap for a loopback interface (`DLT_NULL`, a 4-byte family).
    #[must_use]
    pub fn null() -> Arc<Self> {
        Self::new(dlt::NULL, 4)
    }

    fn new(dlt: u32, hdrlen: u32) -> Arc<Self> {
        Arc::new(Self {
            dlt,
            hdrlen,
            home: SpinLock::new((Weak::new(), 0)),
            descs: SpinLock::new(Vec::new()),
            readers: AtomicUsize::new(0),
        })
    }

    /// Records where its interface is.
    pub fn set_home(&self, vnet: Weak<Vnet>, index: u16) {
        *self.home.lock() = (vnet, index);
    }

    /// Its interface's VNET and index.
    #[must_use]
    pub fn home(&self) -> (Option<Arc<Vnet>>, u16) {
        let h = self.home.lock();
        (h.0.upgrade(), h.1)
    }

    /// Whether anyone reads (`bpf_peers_present`).
    #[must_use]
    pub fn peers(&self) -> bool {
        self.readers.load(Ordering::Relaxed) != 0
    }

    /// Offers `m`, going `way`, to every reader (`bpf_mtap`).
    pub fn mtap(&self, m: &Packet, way: Way) {
        if self.peers() {
            self.offer(&m.data, m.len(), way);
        }
    }

    /// Offers `m` behind link header `head` (`bpf_mtap2`).
    pub fn mtap2(&self, head: &[u8], m: &Packet, way: Way) {
        if self.peers() {
            let p = Prefixed {
                head,
                rest: &m.data,
            };
            self.offer(&p, head.len() + m.len(), way);
        }
    }

    fn offer<S: Source + ?Sized>(&self, pkt: &S, len: usize, way: Way) {
        let pktlen = u32::try_from(len).unwrap_or(u32::MAX);
        let mut now: Option<(u64, u64)> = None;
        let mut woke: Vec<Arc<Desc>> = Vec::new();
        {
            let descs = self.descs.lock();
            for d in descs.iter() {
                let mut st = d.st.lock();
                if st.writer != 0 {
                    continue;
                }
                // `BPF_CHECK_DIRECTION`.
                if (st.direction == dir::IN && way == Way::Out)
                    || (st.direction == dir::OUT && way == Way::In)
                {
                    continue;
                }
                st.rcount += 1;
                let slen = match &st.rfilter {
                    Some(f) => filter::run(f, pkt, pktlen),
                    None => u32::MAX,
                };
                if slen == 0 {
                    continue;
                }
                st.fcount += 1;
                // One reading of the clock serves every descriptor.
                let t = *now.get_or_insert_with(|| CLOCK.get().map_or((0, 0), |f| f()));
                if st.catch(self.hdrlen as usize, pkt, len, slen as usize, t) {
                    woke.push(Arc::clone(d));
                }
            }
        }
        for d in woke {
            d.notify.wake();
        }
    }

    /// Detaches every descriptor (`bpf_ifdetach`): the interface is going,
    /// or moving to another VNET. Their readers get `ENXIO`.
    pub fn detach_all(&self) {
        let descs = core::mem::take(&mut *self.descs.lock());
        self.readers.store(0, Ordering::Relaxed);
        for d in descs {
            {
                let mut st = d.st.lock();
                st.bif = None;
                // The interface keeps no promiscuous count of a gone tap.
                st.promisc = false;
                st.wake_timer();
            }
            d.notify.wake();
        }
    }
}

/// `bd_state`: the read timeout's progress for `select` and `kevent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    Idle,
    Waiting,
    TimedOut,
}

/// A descriptor's state, under its lock.
struct State {
    bufsize: usize,
    /// The store, hold, and free buffers (`bd_sbuf`, `bd_hbuf`,
    /// `bd_fbuf`) and the bytes in the first two.
    sbuf: Option<Vec<u8>>,
    slen: usize,
    hbuf: Option<Vec<u8>>,
    hlen: usize,
    fbuf: Option<Vec<u8>>,
    /// A reader is copying the hold buffer out (it is lent to it).
    hbuf_in_use: bool,
    bif: Option<Arc<Tap>>,
    rfilter: Option<Arc<[BpfInsn]>>,
    wfilter: Option<Arc<[BpfInsn]>>,
    /// Filter settings left before this writer reads (`bd_writer`).
    writer: u8,
    immediate: bool,
    hdrcmplt: bool,
    feedback: bool,
    locked: bool,
    promisc: bool,
    direction: u32,
    tstamp: u32,
    sig: u32,
    pcp: u32,
    bufmode: u32,
    /// The read timeout in ticks, 0 for none (`bd_rtout`).
    rtout: u64,
    timer: Timer,
    rcount: u64,
    dcount: u64,
    fcount: u64,
    wcount: u64,
    wfcount: u64,
    wdcount: u64,
}

impl State {
    /// `bpf_hdrlen`: the header plus padding that puts the network header
    /// after a link header of `link` bytes on a word boundary.
    fn hdrlen(&self, link: usize) -> usize {
        let h = if self.tstamp & ts::FORMAT_MASK == ts::MICROTIME || self.tstamp == ts::NONE {
            abi::bpf::SIZEOF_BPF_HDR
        } else {
            abi::bpf::SIZEOF_BPF_XHDR
        };
        wordalign(link + h) - link
    }

    /// `ROTATE_BUFFERS`.
    fn rotate(&mut self) {
        self.hbuf = self.sbuf.take();
        self.hlen = self.slen;
        self.sbuf = self.fbuf.take();
        self.slen = 0;
    }

    /// `catchpacket`: stores what the filter took of a `pktlen`-byte
    /// packet, `snaplen` bytes at most, stamped with `(uptime, boottime)`
    /// nanoseconds; returns whether a reader should be woken.
    fn catch<S: Source + ?Sized>(
        &mut self,
        link: usize,
        pkt: &S,
        pktlen: usize,
        snaplen: usize,
        t: (u64, u64),
    ) -> bool {
        if self.bif.is_none() {
            self.dcount += 1;
            return false;
        }
        let hdrlen = self.hdrlen(link);
        let totlen = (hdrlen + snaplen.min(pktlen)).min(self.bufsize);
        let mut curlen = wordalign(self.slen);
        let mut wake = false;
        if curlen + totlen > self.bufsize {
            if self.fbuf.is_none() {
                // No room, and none to come (`bpf_buffull`).
                self.dcount += 1;
                return false;
            }
            self.rotate();
            wake = true;
            curlen = 0;
        } else if self.immediate || self.timer == Timer::TimedOut {
            wake = true;
        }
        let caplen = totlen - hdrlen;
        let Some(buf) = self.sbuf.as_mut() else {
            self.dcount += 1;
            return false;
        };
        // Zero the padding after the previous packet, then the header.
        buf[self.slen..curlen + hdrlen].fill(0);
        let (sec, frac) = stamp(self.tstamp, t);
        let h = &mut buf[curlen..];
        h[0..8].copy_from_slice(&sec.to_ne_bytes());
        h[8..16].copy_from_slice(&frac.to_ne_bytes());
        h[16..20].copy_from_slice(&u32::try_from(caplen).unwrap_or(0).to_ne_bytes());
        h[20..24].copy_from_slice(&u32::try_from(pktlen).unwrap_or(u32::MAX).to_ne_bytes());
        h[24..26].copy_from_slice(&u16::try_from(hdrlen).unwrap_or(0).to_ne_bytes());
        let at = curlen + hdrlen;
        if !pkt.read(0, &mut buf[at..at + caplen]) {
            buf[at..at + caplen].fill(0);
        }
        self.slen = curlen + totlen;
        if wake {
            self.wake_timer();
        }
        wake
    }

    /// `bpf_wakeup`'s timer half: a waiting `select` stops waiting.
    fn wake_timer(&mut self) {
        if self.timer == Timer::Waiting {
            self.timer = Timer::Idle;
        }
    }

    /// `bpf_ready`.
    fn ready(&self) -> bool {
        (self.hbuf.is_some() && self.hlen != 0)
            || ((self.immediate || self.timer == Timer::TimedOut) && self.slen != 0)
    }

    /// `reset_d`: empties the buffers and zeroes the counts.
    fn reset(&mut self) {
        if self.hbuf.is_some() {
            self.fbuf = self.hbuf.take();
            self.hlen = 0;
        }
        self.slen = 0;
        self.rcount = 0;
        self.dcount = 0;
        self.fcount = 0;
        self.wcount = 0;
        self.wfcount = 0;
        self.wdcount = 0;
    }

    /// `bpf_buffer_ioctl_sblen`: buffers of `size` bytes, clamped.
    fn set_bufsize(&mut self, size: u32) -> Result<u32, Errno> {
        let size = size.clamp(MINBUFSIZE, MAXBUFSIZE);
        if self.bif.is_some() {
            return Err(Errno::EINVAL);
        }
        let n = size as usize;
        self.bufsize = n;
        self.fbuf = Some(alloc::vec![0; n]);
        self.sbuf = Some(alloc::vec![0; n]);
        self.hbuf = None;
        self.slen = 0;
        self.hlen = 0;
        Ok(size)
    }
}

/// The stamp a header carries for `(uptime, boottime)` in format
/// `tstamp` (`bpf_bintime2ts`): seconds and the fraction.
fn stamp(tstamp: u32, (up, boot): (u64, u64)) -> (i64, u64) {
    if tstamp == ts::NONE {
        return (0, 0);
    }
    let t = if tstamp & ts::MONOTONIC != 0 {
        up
    } else {
        up.saturating_add(boot)
    };
    let sec = i64::try_from(t / 1_000_000_000).unwrap_or(i64::MAX);
    let ns = t % 1_000_000_000;
    let frac = match tstamp & ts::FORMAT_MASK {
        ts::MICROTIME => ns / 1000,
        ts::NANOTIME => ns,
        // `nsec2bintime`'s scale: 2^64 / 10^9, truncated.
        _ => ns * 18_446_744_073,
    };
    (sec, frac)
}

/// The filter a `BIOCSETF`-like request sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    /// `BIOCSETF`: the read filter, then a flush.
    Read,
    /// `BIOCSETFNR`: the read filter, keeping what was captured.
    ReadKeep,
    /// `BIOCSETWF`: the write filter.
    Write,
}

/// What a reader does next.
#[derive(Debug)]
pub enum Step {
    /// Copy out this buffer's first bytes, then give it back with
    /// [`Desc::read_done`].
    Data(Vec<u8>, usize),
    /// Sleep until woken, or for this many ticks if not 0, then call
    /// [`Desc::read_step`] again (`timed_out` if the time ran out).
    Wait(u64),
    /// Return 0 bytes.
    Empty,
}

/// A frame a write sends: on which tap's interface, its bytes, and
/// whether a copy comes back in (`BIOCFEEDBACK`).
#[derive(Debug)]
pub struct Outgoing {
    pub tap: Arc<Tap>,
    pub frame: Packet,
    pub feedback: bool,
}

/// An open `/dev/bpf` (`struct bpf_d`).
pub struct Desc {
    st: SpinLock<State>,
    notify: Arc<dyn Notify>,
}

impl core::fmt::Debug for Desc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Desc").finish_non_exhaustive()
    }
}

impl Desc {
    /// A descriptor (`bpfopen`); `readable` is false for one opened
    /// write-only, which never reads.
    #[must_use]
    pub fn new(notify: Arc<dyn Notify>, readable: bool) -> Arc<Self> {
        Arc::new(Self {
            st: SpinLock::new(State {
                bufsize: BUFSIZE as usize,
                sbuf: None,
                slen: 0,
                hbuf: None,
                hlen: 0,
                fbuf: None,
                hbuf_in_use: false,
                bif: None,
                rfilter: None,
                wfilter: None,
                writer: if readable { 0 } else { 2 },
                immediate: false,
                hdrcmplt: false,
                feedback: false,
                locked: false,
                promisc: false,
                direction: dir::INOUT,
                tstamp: ts::MICROTIME,
                sig: 23, // SIGIO
                pcp: 0,
                bufmode: bufmode::BUFFER,
                rtout: 0,
                timer: Timer::Idle,
                rcount: 0,
                dcount: 0,
                fcount: 0,
                wcount: 0,
                wfcount: 0,
                wdcount: 0,
            }),
            notify,
        })
    }

    /// Detaches it (closing, `bpf_dtor`), and lets go of the interface's
    /// promiscuous mode if it took it.
    pub fn close(self: &Arc<Self>) {
        self.detach();
    }

    /// `bpf_detachd`.
    fn detach(self: &Arc<Self>) {
        let Some(tap) = self.st.lock().bif.clone() else {
            return;
        };
        let promisc = {
            let mut descs = tap.descs.lock();
            let mut st = self.st.lock();
            descs.retain(|d| !Arc::ptr_eq(d, self));
            if st.writer == 0 {
                tap.readers.fetch_sub(1, Ordering::Relaxed);
            }
            st.bif = None;
            core::mem::take(&mut st.promisc)
        };
        if promisc && let (Some(v), index) = tap.home() {
            let _ = v.net.lock().promisc(index, false);
        }
    }

    /// `bpf_attachd`: sets it to `tap`, leaving the one it was on.
    fn attach(self: &Arc<Self>, tap: &Arc<Tap>) {
        self.detach();
        let mut descs = tap.descs.lock();
        let mut st = self.st.lock();
        st.bif = Some(Arc::clone(tap));
        // With `net.bpf.optimize_writers` 0, only a write-only open
        // starts out as a writer.
        if st.writer != 0 {
            st.writer = 2;
        } else {
            tap.readers.fetch_add(1, Ordering::Relaxed);
        }
        descs.push(Arc::clone(self));
        st.reset();
        st.wake_timer();
    }

    /// Whether `BIOCLOCK` forbids request `cmd` now.
    #[must_use]
    pub fn forbids(&self, cmd: u32) -> bool {
        use abi::bpf::bioc;
        use abi::ioctl::FIONREAD;
        /// `TIOCGPGRP`.
        const TIOCGPGRP: u32 = 0x4004_7477;
        self.st.lock().locked
            && !matches!(
                cmd,
                bioc::GBLEN
                    | bioc::FLUSH
                    | bioc::GDLT
                    | bioc::GDLTLIST
                    | bioc::GETIF
                    | bioc::GRTIMEOUT
                    | bioc::GSTATS
                    | bioc::VERSION
                    | bioc::GRSIG
                    | bioc::GHDRCMPLT
                    | bioc::STSTAMP
                    | bioc::FEEDBACK
                    | FIONREAD
                    | bioc::LOCK
                    | bioc::SRTIMEOUT
                    | bioc::IMMEDIATE
                    | TIOCGPGRP
                    | bioc::ROTZBUF
            )
    }

    /// Every request stops a waiting `select`'s timer (`bpfioctl`'s
    /// prologue).
    pub fn idle(&self) {
        self.st.lock().timer = Timer::Idle;
    }

    /// `FIONREAD`.
    #[must_use]
    pub fn nread(&self) -> usize {
        let st = self.st.lock();
        st.slen + if st.hbuf.is_some() { st.hlen } else { 0 }
    }

    /// `BIOCGBLEN`.
    #[must_use]
    pub fn bufsize(&self) -> u32 {
        u32::try_from(self.st.lock().bufsize).unwrap_or(0)
    }

    /// `BIOCSBLEN`: the size set.
    ///
    /// # Errors
    ///
    /// `EINVAL` once set to an interface.
    pub fn set_bufsize(&self, size: u32) -> Result<u32, Errno> {
        self.st.lock().set_bufsize(size)
    }

    /// `BIOCSETF`, `BIOCSETFNR`, `BIOCSETWF` (`bpf_setf`): `None` clears
    /// the filter.
    ///
    /// # Errors
    ///
    /// `EINVAL` for more than `BPF_MAXINSNS` instructions or a program
    /// that fails validation.
    pub fn set_filter(&self, which: Which, prog: Option<Vec<BpfInsn>>) -> Result<(), Errno> {
        let prog = match prog {
            Some(p) if !p.is_empty() => {
                if p.len() > MAXINSNS as usize || !filter::validate(&p) {
                    return Err(Errno::EINVAL);
                }
                Some(Arc::<[BpfInsn]>::from(p))
            }
            _ => None,
        };
        let tap = self.st.lock().bif.clone();
        let _descs = tap.as_ref().map(|t| t.descs.lock());
        let mut st = self.st.lock();
        if which == Which::Write {
            st.wfilter = prog;
            return Ok(());
        }
        // `bpf_check_upgrade`: pcap_open_live sets a lone `ret #snaplen`
        // first; any other filter, or a second, makes a writer read.
        let upgrade = st.writer != 0
            && prog.as_ref().is_some_and(|p| {
                let snap = which == Which::Read
                    && p.len() == 1
                    && p[0].code == abi::bpf::op::RET | abi::bpf::op::K;
                if snap {
                    st.writer -= 1;
                    st.writer == 0
                } else {
                    true
                }
            });
        st.rfilter = prog;
        if which == Which::Read {
            st.reset();
        }
        if upgrade {
            st.writer = 0;
            if let Some(t) = &tap {
                t.readers.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// `BIOCFLUSH`.
    pub fn flush(&self) {
        self.st.lock().reset();
    }

    /// `BIOCSETIF` (`bpf_setif`): sets it to the interface named `name`
    /// in `vnet`, giving it buffers first if it has none.
    ///
    /// # Errors
    ///
    /// `ENXIO` for no such interface, or one without a tap.
    pub fn set_interface(self: &Arc<Self>, vnet: &Arc<Vnet>, name: &[u8]) -> Result<(), Errno> {
        {
            let mut st = self.st.lock();
            if st.bufmode == bufmode::BUFFER && st.sbuf.is_none() {
                let size = u32::try_from(st.bufsize).unwrap_or(BUFSIZE);
                // Not yet attached: it cannot fail.
                st.set_bufsize(size)?;
            }
        }
        let tap = {
            let n = vnet.net.lock();
            let ifp = n.by_name(name).ok_or(Errno::ENXIO)?;
            ifp.bpf.clone().ok_or(Errno::ENXIO)?
        };
        let same = self
            .st
            .lock()
            .bif
            .as_ref()
            .is_some_and(|b| Arc::ptr_eq(b, &tap));
        if same {
            self.st.lock().reset();
        } else {
            self.attach(&tap);
        }
        Ok(())
    }

    /// The interface it is set to: its VNET and index (`BIOCGETIF`).
    ///
    /// # Errors
    ///
    /// `EINVAL` when it is set to none.
    pub fn interface(&self) -> Result<(Option<Arc<Vnet>>, u16), Errno> {
        let tap = self.st.lock().bif.clone().ok_or(Errno::EINVAL)?;
        Ok(tap.home())
    }

    /// `BIOCGDLT`.
    ///
    /// # Errors
    ///
    /// `EINVAL` when it is set to no interface.
    pub fn dlt(&self) -> Result<u32, Errno> {
        self.st
            .lock()
            .bif
            .as_ref()
            .map(|b| b.dlt)
            .ok_or(Errno::EINVAL)
    }

    /// `BIOCSDLT`: each interface has one type.
    ///
    /// # Errors
    ///
    /// `EINVAL` when it is set to no interface, or for another type.
    pub fn set_dlt(&self, dlt: u32) -> Result<(), Errno> {
        if self.dlt()? == dlt {
            Ok(())
        } else {
            Err(Errno::EINVAL)
        }
    }

    /// `BIOCPROMISC`: the interface goes promiscuous while it is set to
    /// it.
    ///
    /// # Errors
    ///
    /// `EINVAL` when it is set to no interface.
    pub fn promisc(&self) -> Result<(), Errno> {
        let tap = self.st.lock().bif.clone().ok_or(Errno::EINVAL)?;
        if self.st.lock().promisc {
            return Ok(());
        }
        let (Some(v), index) = tap.home() else {
            return Err(Errno::EINVAL);
        };
        v.net.lock().promisc(index, true)?;
        self.st.lock().promisc = true;
        Ok(())
    }

    /// `BIOCSRTIMEOUT`, from seconds and microseconds.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a negative or malformed time (`itimerfix`).
    pub fn set_timeout(&self, sec: i64, usec: i64) -> Result<(), Errno> {
        if sec < 0 || !(0..1_000_000).contains(&usec) {
            return Err(Errno::EINVAL);
        }
        // `tvtohz` rounds up and adds a tick, less the one taken off
        // since this is not a one-shot timer.
        let us = u64::try_from(sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000)
            .saturating_add(u64::try_from(usec).unwrap_or(0));
        let tick = 1_000_000 / HZ;
        self.st.lock().rtout = us.div_ceil(tick);
        Ok(())
    }

    /// `BIOCGRTIMEOUT`: seconds and microseconds.
    #[must_use]
    pub fn timeout(&self) -> (i64, i64) {
        let t = self.st.lock().rtout;
        let sec = i64::try_from(t / HZ).unwrap_or(i64::MAX);
        let usec = i64::try_from((t % HZ) * (1_000_000 / HZ)).unwrap_or(0);
        (sec, usec)
    }

    /// `BIOCGSTATS`: received and dropped.
    #[must_use]
    pub fn stats(&self) -> (u32, u32) {
        let st = self.st.lock();
        #[allow(clippy::cast_possible_truncation, reason = "as FreeBSD's u_int")]
        (st.rcount as u32, st.dcount as u32)
    }

    /// The counters `net.bpf.stats` reports: received, dropped, filtered
    /// in, written, written past the filter, and write drops.
    #[must_use]
    pub fn counters(&self) -> [u64; 6] {
        let st = self.st.lock();
        [
            st.rcount, st.dcount, st.fcount, st.wcount, st.wfcount, st.wdcount,
        ]
    }

    pub fn set_immediate(&self, on: bool) {
        self.st.lock().immediate = on;
    }

    #[must_use]
    pub fn hdrcmplt(&self) -> bool {
        self.st.lock().hdrcmplt
    }

    pub fn set_hdrcmplt(&self, on: bool) {
        self.st.lock().hdrcmplt = on;
    }

    #[must_use]
    pub fn direction(&self) -> u32 {
        self.st.lock().direction
    }

    /// `BIOCSDIRECTION`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for an unknown direction.
    pub fn set_direction(&self, d: u32) -> Result<(), Errno> {
        if !matches!(d, dir::IN | dir::INOUT | dir::OUT) {
            return Err(Errno::EINVAL);
        }
        self.st.lock().direction = d;
        Ok(())
    }

    #[must_use]
    pub fn tstamp(&self) -> u32 {
        self.st.lock().tstamp
    }

    /// `BIOCSTSTAMP`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for an invalid kind (`BPF_T_VALID`).
    pub fn set_tstamp(&self, t: u32) -> Result<(), Errno> {
        if !ts::valid(t) {
            return Err(Errno::EINVAL);
        }
        self.st.lock().tstamp = t;
        Ok(())
    }

    pub fn set_feedback(&self, on: bool) {
        self.st.lock().feedback = on;
    }

    /// `BIOCLOCK`.
    pub fn lock(&self) {
        self.st.lock().locked = true;
    }

    #[must_use]
    pub fn sig(&self) -> u32 {
        self.st.lock().sig
    }

    /// `BIOCSRSIG`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a signal past `NSIG`.
    pub fn set_sig(&self, sig: u32) -> Result<(), Errno> {
        if sig >= NSIG {
            return Err(Errno::EINVAL);
        }
        self.st.lock().sig = sig;
        Ok(())
    }

    #[must_use]
    pub fn bufmode(&self) -> u32 {
        self.st.lock().bufmode
    }

    /// `BIOCSETBUFMODE`: zero-copy buffers are off
    /// (`net.bpf.zerocopy_enable` 0).
    ///
    /// # Errors
    ///
    /// `EINVAL` for any mode but `BPF_BUFMODE_BUFFER`, `EBUSY` once there
    /// are buffers or an interface.
    pub fn set_bufmode(&self, m: u32) -> Result<(), Errno> {
        if m != bufmode::BUFFER {
            return Err(Errno::EINVAL);
        }
        let mut st = self.st.lock();
        if st.sbuf.is_some() || st.hbuf.is_some() || st.fbuf.is_some() || st.bif.is_some() {
            return Err(Errno::EBUSY);
        }
        st.bufmode = m;
        Ok(())
    }

    /// `BIOCSETVLANPCP`.
    ///
    /// # Errors
    ///
    /// `EINVAL` past `BPF_PRIO_MAX`.
    pub fn set_pcp(&self, pcp: u32) -> Result<(), Errno> {
        if pcp > PRIO_MAX {
            return Err(Errno::EINVAL);
        }
        self.st.lock().pcp = pcp;
        Ok(())
    }

    /// Whether a read would return now, for `select` and `kevent`
    /// (`bpf_ready`); if not, whether to start the read timeout, and for
    /// how many ticks. Set to no interface, it is always ready (`poll`'s
    /// `POLLHUP`).
    pub fn poll(&self) -> (bool, Option<u64>) {
        let mut st = self.st.lock();
        if st.bif.is_none() {
            return (true, None);
        }
        if st.ready() {
            return (true, None);
        }
        if st.rtout > 0 && st.timer == Timer::Idle {
            st.timer = Timer::Waiting;
            return (false, Some(st.rtout));
        }
        (false, None)
    }

    /// The bytes `EVFILT_READ` reports.
    #[must_use]
    pub fn readable(&self) -> usize {
        let st = self.st.lock();
        st.slen
            + if st.hbuf.is_some() && !st.hbuf_in_use {
                st.hlen
            } else {
                0
            }
    }

    /// Whether it is set to an interface, and its MTU for
    /// `EVFILT_WRITE`.
    #[must_use]
    pub fn writable(&self) -> Option<u32> {
        let tap = self.st.lock().bif.clone()?;
        let (Some(v), index) = tap.home() else {
            return Some(0);
        };
        let n = v.net.lock();
        Some(n.by_index(index).map_or(0, |i| i.mtu))
    }

    /// The `select` timer ran out (`bpf_timed_out`): a reader with data
    /// should look.
    pub fn timed_out(&self) {
        let wake = {
            let mut st = self.st.lock();
            if st.timer != Timer::Waiting {
                return;
            }
            st.timer = Timer::TimedOut;
            st.slen != 0
        };
        if wake {
            self.notify.wake();
        }
    }

    /// Starts a read of `resid` bytes (`bpfread`'s checks): whether the
    /// `select` timer had run out, which lets the read take a partial
    /// store buffer.
    ///
    /// # Errors
    ///
    /// `EINVAL` unless `resid` is the buffer size; `EOPNOTSUPP` with
    /// zero-copy buffers.
    pub fn read_begin(&self, resid: usize) -> Result<bool, Errno> {
        let mut st = self.st.lock();
        if resid != st.bufsize {
            return Err(Errno::EINVAL);
        }
        if st.bufmode != bufmode::BUFFER {
            return Err(Errno::EOPNOTSUPP);
        }
        let timed_out = st.timer == Timer::TimedOut;
        st.timer = Timer::Idle;
        Ok(timed_out)
    }

    /// The read's next step: `timed_out` from [`Desc::read_begin`], or
    /// `true` once a timed wait ran out (`expired`).
    ///
    /// # Errors
    ///
    /// `ENXIO` with nothing buffered and no interface; `EAGAIN` with
    /// nothing to read and `nonblock`.
    pub fn read_step(&self, nonblock: bool, timed_out: bool, expired: bool) -> Result<Step, Errno> {
        let mut st = self.st.lock();
        if st.hbuf_in_use {
            return Ok(Step::Wait(0));
        }
        if st.hbuf.is_none() {
            if expired {
                if st.slen == 0 {
                    return Ok(Step::Empty);
                }
                st.rotate();
            } else if st.slen != 0 && (st.immediate || nonblock || timed_out) {
                st.rotate();
            } else if st.bif.is_none() {
                return Err(Errno::ENXIO);
            } else if nonblock {
                return Err(Errno::EAGAIN);
            } else {
                return Ok(Step::Wait(st.rtout));
            }
        }
        st.hbuf_in_use = true;
        let len = st.hlen;
        let buf = st.hbuf.take().unwrap_or_default();
        Ok(Step::Data(buf, len))
    }

    /// Gives back the hold buffer a read copied out; it becomes the free
    /// buffer.
    pub fn read_done(&self, buf: Vec<u8>) {
        {
            let mut st = self.st.lock();
            st.hbuf_in_use = false;
            st.hlen = 0;
            if buf.len() == st.bufsize && st.fbuf.is_none() && st.sbuf.is_some() {
                st.fbuf = Some(buf);
            }
        }
        // Readers waiting for the hold buffer.
        self.notify.wake();
    }

    /// Builds the frame a write of `data` sends (`bpfwrite`, `bpf_movein`):
    /// `None` for an empty write.
    ///
    /// # Errors
    ///
    /// `ENXIO` with no interface, `ENETDOWN` if it is down, `EMSGSIZE` for
    /// a frame shorter than the link header or longer than the MTU past
    /// it, `EPERM` if the write filter refuses it, `ENOBUFS` without
    /// buffers.
    pub fn write(&self, data: &[u8]) -> Result<Option<Outgoing>, Errno> {
        let r = self.movein(data);
        let mut st = self.st.lock();
        st.wcount += 1;
        match &r {
            Ok(Some(_)) => st.wfcount += 1,
            Ok(None) => {}
            Err(_) => st.wdcount += 1,
        }
        r
    }

    /// Counts a write the interface refused.
    pub fn write_failed(&self) {
        self.st.lock().wdcount += 1;
    }

    fn movein(&self, data: &[u8]) -> Result<Option<Outgoing>, Errno> {
        let (tap, wfilter, hdrcmplt, feedback) = {
            let st = self.st.lock();
            let tap = st.bif.clone().ok_or(Errno::ENXIO)?;
            (tap, st.wfilter.clone(), st.hdrcmplt, st.feedback)
        };
        let (Some(v), index) = tap.home() else {
            return Err(Errno::ENXIO);
        };
        let (flags, mtu, lladdr) = {
            let n = v.net.lock();
            let i = n.by_index(index).ok_or(Errno::ENXIO)?;
            (i.flags, i.mtu, i.lladdr)
        };
        if flags & iff::UP == 0 {
            return Err(Errno::ENETDOWN);
        }
        if data.is_empty() {
            return Ok(None);
        }
        let hlen = match tap.dlt {
            dlt::EN10MB => ETHER_HDR_LEN,
            dlt::NULL => 4,
            _ => return Err(Errno::EIO),
        };
        if data.len() < hlen || data.len() - hlen > mtu as usize {
            return Err(Errno::EMSGSIZE);
        }
        let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
        if let Some(f) = &wfilter
            && filter::run(f, data, len) == 0
        {
            return Err(Errno::EPERM);
        }
        let mut m = Packet::for_output(data.len())?;
        if tap.dlt == dlt::EN10MB {
            let mut hdr = [0u8; ETHER_HDR_LEN];
            hdr.copy_from_slice(&data[..ETHER_HDR_LEN]);
            if hdr[0] & 1 != 0 {
                m.hdr.flags |= if hdr[..6] == [0xff; 6] {
                    flags::BCAST
                } else {
                    flags::MCAST
                };
            }
            // The source is the interface's unless the writer completes
            // the header itself.
            if !hdrcmplt && let Some(ll) = lladdr {
                hdr[6..12].copy_from_slice(&ll);
            }
            m.data.extend_from_slice(&hdr)?;
            m.data.extend_from_slice(&data[ETHER_HDR_LEN..])?;
        } else {
            m.data.extend_from_slice(data)?;
        }
        Ok(Some(Outgoing {
            tap,
            frame: m,
            feedback,
        }))
    }
}

#[cfg(test)]
mod tests;
