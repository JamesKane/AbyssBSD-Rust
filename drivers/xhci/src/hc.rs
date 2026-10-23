// SPDX-License-Identifier: BSD-2-Clause
//! The controller: bring-up, commands, slots and endpoints, transfers,
//! events, and root ports (USB.md §4).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use bus::{AttachError, MmioGrant};
use irq::Filter;
use regs::Bytes;
use sched::Sleepable;
use sync::SpinLock;
use usb::descriptor::{Endpoint, TransferType};
use usb::{Setup, Speed};
use usbus::bus::Explorer;
use usbus::hc::{HubInfo, Tt};
use usbus::{HostController, NewDevice, PortState, Rejected, SlotId, Transfer, UsbError};
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag, Mapped, dir};
use vm::object::IoBuf;
use xhci::context::{
    self, ContextSize, Dci, EndpointContext, EndpointType, SlotContext, input_control,
};
use xhci::event::Event;
use xhci::regs::{self as r, Params, Protocol, cap, op, portsc, rt, xcap};
use xhci::ring::{EventRing, ProducerRing, TrbMemory, erst_entry};
use xhci::trb::{self, CompletionCode, SetupPacket, Trb, command, flag, transfer as tt};

/// TRBs per ring: one page each.
const RING_TRBS: usize = 256;
/// Command ring TRBs.
const COMMAND_TRBS: usize = 64;
/// How long a command may take (FreeBSD's `XHCI_CMD_TIMEOUT` is 5 s).
const COMMAND_TIMEOUT_MS: u32 = 5000;
/// The watchdog's period.
pub(crate) const TICK_MS: u32 = 100;
/// Events handled per batch before `ERDP` moves.
const EVENT_BATCH: usize = 64;
/// Interrupt moderation, in 250 ns units: 125 µs.
const IMOD: u32 = 500;

/// A ring's TRBs in DMA memory, four words each.
pub(crate) struct Trbs(DmaCoherent<u32>);

impl TrbMemory for Trbs {
    fn slots(&self) -> usize {
        self.0.len() / 4
    }
    fn bus_addr(&self) -> u64 {
        self.0.bus_addr()
    }
    fn read(&self, i: usize) -> Trb {
        Trb([
            self.0.read(4 * i),
            self.0.read(4 * i + 1),
            self.0.read(4 * i + 2),
            self.0.read(4 * i + 3),
        ])
    }
    fn write_body(&self, i: usize, b: [u32; 3]) {
        for (k, w) in b.into_iter().enumerate() {
            self.0.write(4 * i + k, w);
        }
    }
    fn write_control(&self, i: usize, c: u32) {
        self.0.write(4 * i + 3, c);
    }
}

fn trbs(tag: &DmaTag, n: usize) -> Result<Trbs, UsbError> {
    tag.coherent::<u32>(4 * n)
        .map(Trbs)
        .map_err(|_| UsbError::NoMemory)
}

/// The register windows.
pub(crate) struct Regs {
    pub(crate) mmio: MmioGrant<Bytes>,
    op: usize,
    rt: usize,
    db: usize,
}

impl Regs {
    fn op32(&self, off: usize) -> u32 {
        self.mmio.read32(self.op + off)
    }
    fn set_op32(&self, off: usize, v: u32) {
        self.mmio.write32(self.op + off, v);
    }
    fn write64(&self, at: usize, v: u64) {
        let b = v.to_le_bytes();
        self.mmio
            .write32(at, u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        self.mmio
            .write32(at + 4, u32::from_le_bytes([b[4], b[5], b[6], b[7]]));
    }
    fn interrupter(&self, off: usize) -> usize {
        self.rt + rt::interrupter(0) + off
    }
    fn portsc(&self, port: u8) -> usize {
        self.op + op::portsc(port)
    }
    fn doorbell(&self, slot: u8, value: u32) {
        self.mmio.write32(self.db + 4 * usize::from(slot), value);
    }
}

/// Where a transfer's data is mapped.
enum Data {
    None,
    In(Mapped<IoBuf, dir::FromDevice>),
    Out(Mapped<IoBuf, dir::ToDevice>),
}

/// A transfer on a ring.
struct InFlight {
    t: Transfer,
    data: Data,
    /// Each TRB's data length.
    lens: Vec<u32>,
    /// A control transfer's data stage came up short: the bytes moved.
    short: Option<usize>,
    /// Milliseconds left, if it has a timeout.
    left_ms: Option<u32>,
    expired: bool,
}

impl InFlight {
    /// Bytes moved if TRB `k` completed with `residue` bytes left.
    fn actual_at(&self, k: usize, residue: u32) -> usize {
        let before: u64 = self.lens.iter().take(k).map(|&l| u64::from(l)).sum();
        let this = self
            .lens
            .get(k)
            .map_or(0, |&l| u64::from(l.saturating_sub(residue)));
        usize::try_from(before + this).unwrap_or(usize::MAX)
    }

    fn finish(self, r: Result<usize, UsbError>) {
        let Self { mut t, data, .. } = self;
        match data {
            Data::None => {}
            Data::In(m) => t.buf = m.unmap(),
            Data::Out(m) => t.buf = m.unmap(),
        }
        t.complete(r);
    }
}

struct Ep {
    ring: ProducerRing<Trbs, InFlight>,
    control: bool,
    /// Its max packet size, for TD sizes.
    max_packet: u16,
}

struct Slot {
    /// The device (output) context.
    out: DmaCoherent<u32>,
    input: DmaCoherent<u32>,
    speed: Speed,
    /// By device context index.
    eps: Vec<Option<Ep>>,
}

/// A command's completion.
struct CmdWait {
    done: AtomicBool,
    result: SpinLock<Option<(CompletionCode, u8)>>,
}

impl CmdWait {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: AtomicBool::new(false),
            result: SpinLock::new(None),
        })
    }
}

/// The controller.
pub(crate) struct Hc {
    name: String,
    vendor: String,
    regs: Regs,
    params: Params,
    csz: ContextSize,
    protocols: Vec<Protocol>,
    /// Contexts and rings.
    tag: DmaTag,
    /// Transfer data: segments that never cross 64 KiB, so one TRB each.
    data_tag: DmaTag,
    dcbaa: DmaCoherent<u64>,
    _scratch: Option<(DmaCoherent<u64>, Vec<DmaCoherent<u8>>)>,
    _erst: DmaCoherent<u32>,
    cmd: SpinLock<ProducerRing<Trbs, Arc<CmdWait>>>,
    events: SpinLock<EventRing<Trbs>>,
    slots: Vec<SpinLock<Option<Slot>>>,
    explorer: SpinLock<Option<Arc<Explorer>>>,
    stopping: AtomicBool,
}

fn xspeed(s: Speed) -> context::Speed {
    match s {
        Speed::Low => context::Speed::Low,
        Speed::Full => context::Speed::Full,
        Speed::High => context::Speed::High,
        Speed::Super => context::Speed::Super,
        Speed::SuperPlus => context::Speed::SuperPlus,
    }
}

fn speed_of(psiv: u8) -> Option<Speed> {
    Some(match context::Speed::from_psiv(psiv)? {
        context::Speed::Low => Speed::Low,
        context::Speed::Full => Speed::Full,
        context::Speed::High => Speed::High,
        context::Speed::Super => Speed::Super,
        context::Speed::SuperPlus => Speed::SuperPlus,
    })
}

fn error(c: CompletionCode) -> UsbError {
    match c {
        CompletionCode::Stall => UsbError::Stalled,
        CompletionCode::Babble => UsbError::Babble,
        CompletionCode::UsbTransaction | CompletionCode::SplitTransaction => UsbError::Transaction,
        CompletionCode::Stopped
        | CompletionCode::StoppedLengthInvalid
        | CompletionCode::StoppedShortPacket
        | CompletionCode::CommandAborted => UsbError::Cancelled,
        CompletionCode::NoSlots | CompletionCode::Resource | CompletionCode::Bandwidth => {
            UsbError::NoMemory
        }
        CompletionCode::Other(v) => UsbError::Controller(v),
        other => UsbError::Controller(raw(other)),
    }
}

fn raw(c: CompletionCode) -> u8 {
    use CompletionCode::*;
    match c {
        Invalid => 0,
        Success => 1,
        DataBuffer => 2,
        Babble => 3,
        UsbTransaction => 4,
        Trb => 5,
        Stall => 6,
        Resource => 7,
        Bandwidth => 8,
        NoSlots => 9,
        SlotNotEnabled => 11,
        EndpointNotEnabled => 12,
        ShortPacket => 13,
        Parameter => 17,
        ContextState => 19,
        EventRingFull => 21,
        IncompatibleDevice => 22,
        CommandRingStopped => 24,
        CommandAborted => 25,
        Stopped => 26,
        StoppedLengthInvalid => 27,
        StoppedShortPacket => 28,
        SplitTransaction => 36,
        Other(v) => v,
    }
}

fn sleep_ms(s: &Sleepable, ms: u32) {
    bus::manager().sleep_ms(s, u64::from(ms));
}

/// Waits up to `ms` for `done`, polling every millisecond.
fn poll(s: &Sleepable, ms: u32, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..ms {
        if done() {
            return true;
        }
        sleep_ms(s, 1);
    }
    done()
}

impl Hc {
    /// Takes the controller from firmware, resets it, and starts it
    /// (xHCI 1.2 §4.2).
    pub(crate) fn new(
        s: &Sleepable,
        name: String,
        vendor: String,
        mmio: MmioGrant<Bytes>,
        tag: DmaTag,
        data_tag: DmaTag,
    ) -> Result<Self, AttachError> {
        let len = mmio.len();
        if len < 0x40 {
            return Err(AttachError::Device("register window too small"));
        }
        let caplength = usize::from(mmio.read32(cap::CAPLENGTH).to_le_bytes()[0]);
        let params = Params::decode(
            mmio.read32(cap::HCSPARAMS1),
            mmio.read32(cap::HCSPARAMS2),
            mmio.read32(cap::HCCPARAMS1),
        );
        let db = usize::try_from(mmio.read32(cap::DBOFF) & !3).unwrap_or(usize::MAX);
        let rtb = usize::try_from(mmio.read32(cap::RTSOFF) & !0x1f).unwrap_or(usize::MAX);
        let fits = |at: usize, n: usize| at.checked_add(n).is_some_and(|e| e <= len);
        if params.max_slots == 0
            || params.max_ports == 0
            || !fits(caplength, op::portsc(params.max_ports) + 0x10)
            || !fits(db, 4 * (usize::from(params.max_slots) + 1))
            || !fits(rtb, rt::interrupter(1))
        {
            return Err(AttachError::Device("registers out of range"));
        }
        if caplength == 0 || mmio.read32(caplength + op::PAGESIZE) & 1 == 0 {
            return Err(AttachError::Device("4 KiB pages not supported"));
        }
        let regs = Regs {
            mmio,
            op: caplength,
            rt: rtb,
            db,
        };

        // Ownership from firmware, and the protocols' ports.
        let mut protocols = Vec::new();
        let caps: Vec<(usize, u8)> =
            r::ext_caps(params.xecp, len, |at| regs.mmio.read32(at)).collect();
        for (at, id) in caps {
            match id {
                xcap::LEGACY => handoff(s, &regs, at),
                xcap::PROTOCOL if fits(at, 16) => protocols.push(Protocol::decode([
                    regs.mmio.read32(at),
                    regs.mmio.read32(at + 4),
                    regs.mmio.read32(at + 8),
                    regs.mmio.read32(at + 12),
                ])),
                _ => {}
            }
        }

        // Halt, then reset.
        let cmd = regs.op32(op::USBCMD);
        if cmd & op::cmd::RUN != 0 {
            regs.set_op32(op::USBCMD, cmd & !op::cmd::RUN);
        }
        if !poll(s, 100, || regs.op32(op::USBSTS) & op::sts::HCH != 0) {
            return Err(AttachError::Device("controller did not halt"));
        }
        regs.set_op32(op::USBCMD, op::cmd::HCRST);
        // Some controllers must not be read for a millisecond after.
        sleep_ms(s, 1);
        let reset = poll(s, 1000, || {
            regs.op32(op::USBCMD) & op::cmd::HCRST == 0 && regs.op32(op::USBSTS) & op::sts::CNR == 0
        });
        if !reset {
            return Err(AttachError::Device("controller reset timed out"));
        }

        let nomem = |_| AttachError::NoMemory;
        let slots = usize::from(params.max_slots);
        regs.set_op32(op::CONFIG, u32::from(params.max_slots));
        let dcbaa = tag.coherent::<u64>(slots + 1).map_err(nomem)?;
        let scratch = if params.scratchpads > 0 {
            let n = usize::from(params.scratchpads);
            let array = tag.coherent::<u64>(n).map_err(nomem)?;
            let mut pages = Vec::new();
            for i in 0..n {
                let p = tag.coherent::<u8>(4096).map_err(nomem)?;
                array.write(i, p.bus_addr());
                pages.push(p);
            }
            dcbaa.write(0, array.bus_addr());
            Some((array, pages))
        } else {
            None
        };
        regs.write64(regs.op + op::DCBAAP, dcbaa.bus_addr());

        let cmd_ring =
            ProducerRing::new(trbs(&tag, COMMAND_TRBS).map_err(|_| AttachError::NoMemory)?);
        let (deq, cycle) = cmd_ring.dequeue();
        regs.write64(regs.op + op::CRCR, deq | u64::from(cycle));

        let events = EventRing::new(trbs(&tag, RING_TRBS).map_err(|_| AttachError::NoMemory)?);
        let erst = tag.coherent::<u32>(4).map_err(nomem)?;
        let base = events.memory().bus_addr();
        let n = u16::try_from(RING_TRBS).unwrap_or(u16::MAX);
        for (i, w) in erst_entry(base, n).into_iter().enumerate() {
            erst.write(i, w);
        }
        regs.mmio.write32(regs.interrupter(rt::ERSTSZ), 1);
        regs.write64(regs.interrupter(rt::ERDP), events.erdp());
        regs.write64(regs.interrupter(rt::ERSTBA), erst.bus_addr());
        regs.mmio.write32(regs.interrupter(rt::IMOD), IMOD);
        regs.mmio
            .write32(regs.interrupter(rt::IMAN), rt::IMAN_IP | rt::IMAN_IE);

        regs.set_op32(op::USBCMD, op::cmd::RUN | op::cmd::INTE | op::cmd::HSEE);
        if !poll(s, 100, || regs.op32(op::USBSTS) & op::sts::HCH == 0) {
            return Err(AttachError::Device("controller did not start"));
        }
        if params.ppc {
            for p in 1..=params.max_ports {
                let at = regs.portsc(p);
                let v = regs.mmio.read32(at);
                regs.mmio.write32(at, portsc::write(v, portsc::PP));
            }
        }
        Ok(Self {
            name,
            vendor,
            csz: if params.csz {
                ContextSize::Bytes64
            } else {
                ContextSize::Bytes32
            },
            params,
            protocols,
            regs,
            tag,
            data_tag,
            dcbaa,
            _scratch: scratch,
            _erst: erst,
            cmd: SpinLock::new(cmd_ring),
            events: SpinLock::new(events),
            slots: (0..=slots).map(|_| SpinLock::new(None)).collect(),
            explorer: SpinLock::new(None),
            stopping: AtomicBool::new(false),
        })
    }

    /// The interrupt filter: acknowledges the interrupter.
    pub(crate) fn filter(&self) -> Filter {
        let at = self.regs.interrupter(rt::IMAN);
        let iman = self.regs.mmio.read32(at);
        if iman == u32::MAX || iman & rt::IMAN_IP == 0 {
            return Filter::Stray;
        }
        self.regs.mmio.write32(at, iman | rt::IMAN_IP);
        self.regs.set_op32(op::USBSTS, op::sts::EINT);
        Filter::Schedule
    }

    /// Drains the event ring and acts on each event.
    pub(crate) fn process_events(&self) {
        loop {
            let mut batch = Vec::new();
            {
                let mut ev = self.events.lock();
                while batch.len() < EVENT_BATCH {
                    match ev.pop() {
                        Some(t) => batch.push(t),
                        None => break,
                    }
                }
                if !batch.is_empty()
                    || self.regs.mmio.read32(self.regs.interrupter(rt::ERDP)) & 8 != 0
                {
                    self.regs
                        .write64(self.regs.interrupter(rt::ERDP), ev.erdp() | rt::ERDP_EHB);
                }
            }
            if batch.is_empty() {
                return;
            }
            let mut done = Vec::new();
            for t in &batch {
                self.event(Event::decode(t), &mut done);
            }
            for (f, r) in done {
                f.finish(r);
            }
        }
    }

    fn event(&self, e: Event, done: &mut Vec<(InFlight, Result<usize, UsbError>)>) {
        match e {
            Event::Command {
                pointer,
                code,
                slot,
                ..
            } => {
                let m = self.cmd.lock().complete(pointer);
                if let Some(m) = m {
                    *m.tag.result.lock() = Some((code, slot));
                    m.tag.done.store(true, Ordering::Release);
                }
            }
            Event::Transfer {
                pointer,
                residue,
                code,
                event_data,
                slot,
                endpoint,
            } => {
                if event_data {
                    return;
                }
                let Some(cell) = self.slots.get(usize::from(slot)) else {
                    return;
                };
                let mut g = cell.lock();
                let Some(ep) = g
                    .as_mut()
                    .and_then(|sl| sl.eps.get_mut(usize::from(endpoint)))
                    .and_then(Option::as_mut)
                else {
                    return;
                };
                if ep.control && code == CompletionCode::ShortPacket {
                    // A short data stage: the status stage still runs.
                    if let Some((f, k, len)) = ep.ring.find_mut(pointer)
                        && k + 1 < len
                    {
                        f.short = Some(f.actual_at(k, residue));
                        return;
                    }
                }
                if matches!(
                    code,
                    CompletionCode::Stopped
                        | CompletionCode::StoppedLengthInvalid
                        | CompletionCode::StoppedShortPacket
                ) {
                    // Whoever stopped the endpoint completes what is left.
                    return;
                }
                if let Some(m) = ep.ring.complete(pointer) {
                    let f = m.tag;
                    let r = if code.is_ok() {
                        Ok(f.short.unwrap_or_else(|| f.actual_at(m.trb, residue)))
                    } else {
                        Err(error(code))
                    };
                    done.push((f, r));
                }
            }
            Event::PortStatusChange { .. } => {
                if let Some(e) = self.explorer.lock().as_ref() {
                    e.kick();
                }
            }
            Event::HostController { code } => {
                bus::manager().log(format_args!(
                    "{}: host controller event {code:?}",
                    self.name
                ));
            }
            Event::MfindexWrap | Event::Other { .. } => {}
        }
    }

    /// Runs a command and waits for its completion: the code and the
    /// slot field.
    fn exec(&self, s: &Sleepable, t: Trb) -> Result<(CompletionCode, u8), UsbError> {
        let w = CmdWait::new();
        self.cmd
            .lock()
            .push(&[t], Arc::clone(&w))
            .map_err(|_| UsbError::Busy)?;
        self.regs.doorbell(0, 0);
        let finished = |w: &CmdWait| w.done.load(Ordering::Acquire);
        let ok = poll(s, COMMAND_TIMEOUT_MS, || {
            self.process_events();
            finished(&w)
        });
        if !ok {
            // Abort the ring; the command completes as aborted (or the
            // ring reports itself stopped).
            let crcr = self.regs.op + op::CRCR;
            self.regs.write64(crcr, op::crcr::CA);
            poll(s, 1000, || {
                self.process_events();
                finished(&w)
            });
            if !finished(&w) {
                let _ = self.cmd.lock().drain().count();
                let (deq, cycle) = self.cmd.lock().dequeue();
                self.regs.write64(crcr, deq | u64::from(cycle));
                return Err(UsbError::TimedOut);
            }
        }
        let r = w.result.lock().take().ok_or(UsbError::TimedOut)?;
        Ok(r)
    }

    /// Runs a command that must succeed.
    fn exec_ok(&self, s: &Sleepable, t: Trb) -> Result<u8, UsbError> {
        match self.exec(s, t)? {
            (CompletionCode::Success, slot) => Ok(slot),
            (c, _) => Err(error(c)),
        }
    }

    fn protocol(&self, port: u8) -> Option<&Protocol> {
        self.protocols.iter().find(|p| p.has(port))
    }

    fn write_ctx(mem: &DmaCoherent<u32>, byte: usize, w: [u32; 8]) {
        for (i, v) in w.into_iter().enumerate() {
            mem.write(byte / 4 + i, v);
        }
    }

    fn read_ctx(mem: &DmaCoherent<u32>, byte: usize) -> [u32; 8] {
        core::array::from_fn(|i| mem.read(byte / 4 + i))
    }

    fn clear(mem: &DmaCoherent<u32>) {
        for i in 0..mem.len() {
            mem.write(i, 0);
        }
    }

    /// Endpoint `address`'s context index on a slot.
    fn dci(address: u8) -> Option<Dci> {
        Dci::from_address(address, false)
    }

    /// Cancels everything on an endpoint: stops it (unless it is halted
    /// or stopped already), completes what is queued (`TimedOut` for
    /// what expired, `why` for the rest), and moves its dequeue pointer
    /// past them.
    fn cancel(&self, s: &Sleepable, slot: u8, dci: Dci, why: UsbError, stop: bool) {
        if stop {
            let _ = self.exec(s, command::stop_endpoint(slot, dci.get()));
        }
        let mut dropped = Vec::new();
        let deq = {
            let mut g = match self.slots.get(usize::from(slot)) {
                Some(c) => c.lock(),
                None => return,
            };
            let Some(ep) = g
                .as_mut()
                .and_then(|sl| sl.eps.get_mut(usize::from(dci.get())))
                .and_then(Option::as_mut)
            else {
                return;
            };
            dropped.extend(ep.ring.drain());
            ep.ring.dequeue()
        };
        for f in dropped {
            let r = if f.expired { UsbError::TimedOut } else { why };
            f.finish(Err(r));
        }
        let _ = self.exec(s, command::set_tr_dequeue(slot, dci.get(), deq.0, deq.1, 0));
    }

    /// One watchdog tick: times out transfers whose time ran out.
    pub(crate) fn tick(&self, s: &Sleepable) {
        let mut expired = Vec::new();
        for (i, cell) in self.slots.iter().enumerate() {
            let mut g = cell.lock();
            let Some(sl) = g.as_mut() else { continue };
            for (d, ep) in sl.eps.iter_mut().enumerate() {
                let Some(ep) = ep else { continue };
                let mut hit = false;
                for f in ep.ring.outstanding_mut() {
                    if let Some(left) = f.left_ms.as_mut() {
                        *left = left.saturating_sub(TICK_MS);
                        if *left == 0 && !f.expired {
                            f.expired = true;
                            hit = true;
                        }
                    }
                }
                if hit
                    && let (Ok(slot), Some(dci)) =
                        (u8::try_from(i), u8::try_from(d).ok().and_then(Dci::new))
                {
                    expired.push((slot, dci));
                }
            }
        }
        for (slot, dci) in expired {
            self.cancel(s, slot, dci, UsbError::Cancelled, true);
        }
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// Stops the controller: no more DMA; everything outstanding
    /// completes with `Gone`.
    pub(crate) fn shutdown(&self, s: &Sleepable) {
        self.stopping.store(true, Ordering::Release);
        let cmd = self.regs.op32(op::USBCMD);
        self.regs
            .set_op32(op::USBCMD, cmd & !(op::cmd::RUN | op::cmd::INTE));
        poll(s, 100, || self.regs.op32(op::USBSTS) & op::sts::HCH != 0);
        self.regs
            .mmio
            .write32(self.regs.interrupter(rt::IMAN), rt::IMAN_IP);
        let mut gone = Vec::new();
        for cell in &self.slots {
            if let Some(mut sl) = cell.lock().take() {
                for ep in sl.eps.iter_mut().flatten() {
                    gone.extend(ep.ring.drain());
                }
            }
        }
        for f in gone {
            f.finish(Err(UsbError::Gone));
        }
        let waits: Vec<Arc<CmdWait>> = self.cmd.lock().drain().collect();
        for w in waits {
            w.done.store(true, Ordering::Release);
        }
    }

    fn endpoint_context(
        &self,
        speed: Speed,
        e: &Endpoint,
        ring: &ProducerRing<Trbs, InFlight>,
    ) -> EndpointContext {
        let ep_type = EndpointType::from_descriptor(e.attributes, e.is_in());
        let isoch = e.transfer_type() == TransferType::Isochronous;
        let (burst, mult) = match (speed.is_super(), e.companion) {
            (true, Some(c)) => (c.max_burst, if isoch { c.attributes & 3 } else { 0 }),
            (false, _) if speed == Speed::High && ep_type.is_periodic() => {
                (e.extra_transactions(), 0)
            }
            _ => (0, 0),
        };
        let mp = e.max_packet();
        let esit = if ep_type.is_periodic() {
            match e.companion {
                Some(c) if speed.is_super() => u32::from(c.bytes_per_interval),
                _ => u32::from(mp) * (u32::from(burst) + 1) * (u32::from(mult) + 1),
            }
        } else {
            0
        };
        let (deq, dcs) = ring.dequeue();
        EndpointContext {
            state: 0,
            mult,
            max_primary_streams: 0,
            lsa: false,
            interval: context::interval(xspeed(speed), ep_type, e.interval),
            error_count: if isoch { 0 } else { 3 },
            ep_type: Some(ep_type),
            max_burst: burst,
            max_packet: mp,
            dequeue: deq,
            dcs,
            average_trb_length: ep_type.average_trb_length(),
            max_esit_payload: esit,
        }
    }
}

/// The controller's DMA tags: one for contexts and rings, and one for
/// transfer data whose segments never cross 64 KiB, so each is one TRB.
pub(crate) fn tags(
    cx: &mut bus::AttachCx<'_>,
    mmio: &MmioGrant<Bytes>,
) -> Result<(DmaTag, DmaTag), AttachError> {
    let ac64 = mmio.read32(cap::HCCPARAMS1) & 1 != 0;
    let addr_bits = if ac64 { 64 } else { 32 };
    let tag = cx.dma_tag(DmaConstraints {
        addr_bits,
        align: 64,
        ..DmaConstraints::UNLIMITED
    })?;
    let data = cx.dma_tag(DmaConstraints {
        addr_bits,
        boundary: Some(u64::from(trb::MAX_TRB_BYTES)),
        max_segment: u64::from(trb::MAX_TRB_BYTES),
        ..DmaConstraints::UNLIMITED
    })?;
    Ok((tag, data))
}

/// Takes the controller from firmware (xHCI 1.2 §4.22.1).
fn handoff(s: &Sleepable, regs: &Regs, at: usize) {
    let v = regs.mmio.read32(at);
    if v & xcap::BIOS_OWNED != 0 {
        regs.mmio.write32(at, v | xcap::OS_OWNED);
        let freed = poll(s, 1000, || regs.mmio.read32(at) & xcap::BIOS_OWNED == 0);
        if !freed {
            // Firmware never let go: take it anyway, as FreeBSD does.
            regs.mmio.write32(
                at,
                (regs.mmio.read32(at) & !xcap::BIOS_OWNED) | xcap::OS_OWNED,
            );
        }
    }
    let ctl = regs.mmio.read32(at + 4);
    regs.mmio
        .write32(at + 4, (ctl & !xcap::SMI_ENABLES) | xcap::SMI_STATUS);
}

impl HostController for Hc {
    fn name(&self) -> &str {
        &self.name
    }

    fn bus_speed(&self) -> &str {
        if self.protocols.iter().any(|p| p.major >= 3) {
            "5.0Gbps Super Speed USB v3.0"
        } else {
            "480Mbps High Speed USB v2.0"
        }
    }

    fn root_hub_vendor(&self) -> &str {
        &self.vendor
    }

    fn root_ports(&self) -> u8 {
        self.params.max_ports
    }

    fn root_port_state(&self, port: u8) -> PortState {
        if port == 0 || port > self.params.max_ports {
            return PortState::default();
        }
        let v = self.regs.mmio.read32(self.regs.portsc(port));
        if v == u32::MAX {
            return PortState::default();
        }
        PortState {
            connected: v & portsc::CCS != 0,
            enabled: v & portsc::PED != 0,
            speed: speed_of(portsc::speed(v)),
            changed: v & portsc::CHANGES != 0,
            connect_changed: v & portsc::CSC != 0,
            over_current: v & portsc::OCA != 0,
        }
    }

    fn root_port_clear(&self, port: u8) {
        if port == 0 || port > self.params.max_ports {
            return;
        }
        let at = self.regs.portsc(port);
        let v = self.regs.mmio.read32(at);
        self.regs
            .mmio
            .write32(at, portsc::write(v, v & portsc::CHANGES));
    }

    fn root_port_reset(&self, s: &Sleepable, port: u8) -> Result<PortState, UsbError> {
        if port == 0 || port > self.params.max_ports {
            return Err(UsbError::Invalid);
        }
        let at = self.regs.portsc(port);
        let v = self.regs.mmio.read32(at);
        if v & portsc::CCS == 0 {
            return Err(UsbError::Gone);
        }
        self.regs.mmio.write32(at, portsc::write(v, portsc::PR));
        for _ in 0..50 {
            sleep_ms(s, 10);
            let v = self.regs.mmio.read32(at);
            if v & portsc::CCS == 0 {
                return Err(UsbError::Gone);
            }
            if v & portsc::PR == 0 && v & portsc::PED != 0 {
                self.regs
                    .mmio
                    .write32(at, portsc::write(v, portsc::PRC | portsc::WRC));
                // Reset recovery (USB 2.0 §7.1.7.5).
                sleep_ms(s, 10);
                return Ok(self.root_port_state(port));
            }
        }
        Err(UsbError::TimedOut)
    }

    fn root_port_disable(&self, port: u8) {
        if port == 0 || port > self.params.max_ports {
            return;
        }
        let at = self.regs.portsc(port);
        let v = self.regs.mmio.read32(at);
        // PED is write-1-to-disable: the one place it is written.
        self.regs
            .mmio
            .write32(at, (v & portsc::PRESERVE) | portsc::PED);
    }

    fn set_explorer(&self, e: Arc<Explorer>) {
        *self.explorer.lock() = Some(e);
    }

    fn enable_device(&self, s: &Sleepable, d: &NewDevice) -> Result<SlotId, UsbError> {
        if self.is_stopping() {
            return Err(UsbError::Gone);
        }
        let slot_type = self.protocol(d.root_port).map_or(0, |p| p.slot_type);
        let slot = self.exec_ok(s, command::enable_slot(slot_type))?;
        if slot == 0 || slot > self.params.max_slots {
            return Err(UsbError::Protocol("bad slot ID"));
        }
        let r = self.address(s, slot, d);
        if r.is_err() {
            let _ = self.exec(s, command::disable_slot(slot));
            if let Some(c) = self.slots.get(usize::from(slot)) {
                *c.lock() = None;
            }
            self.dcbaa.write(usize::from(slot), 0);
        }
        r.map(|()| SlotId(slot))
    }

    fn set_max_packet0(
        &self,
        s: &Sleepable,
        slot: SlotId,
        max_packet: u16,
    ) -> Result<(), UsbError> {
        let input_addr = {
            let g = self
                .slots
                .get(usize::from(slot.0))
                .ok_or(UsbError::Invalid)?
                .lock();
            let sl = g.as_ref().ok_or(UsbError::Invalid)?;
            Self::clear(&sl.input);
            Self::write_ctx(&sl.input, 0, input_control(0, 1 << 1, 0, 0, 0));
            let mut ep0 = EndpointContext::decode(&Self::read_ctx(
                &sl.out,
                self.csz.device_endpoint(Dci::CONTROL),
            ));
            ep0.state = 0;
            ep0.max_packet = max_packet;
            Self::write_ctx(
                &sl.input,
                self.csz.input_endpoint(Dci::CONTROL),
                ep0.encode(),
            );
            sl.input.bus_addr()
        };
        self.exec_ok(s, command::evaluate_context(slot.0, input_addr))?;
        if let Some(c) = self.slots.get(usize::from(slot.0)) {
            let mut g = c.lock();
            if let Some(ep) = g
                .as_mut()
                .and_then(|sl| sl.eps.get_mut(1))
                .and_then(Option::as_mut)
            {
                ep.max_packet = max_packet;
            }
        }
        Ok(())
    }

    fn configure(
        &self,
        s: &Sleepable,
        slot: SlotId,
        endpoints: &[Endpoint],
        hub: Option<HubInfo>,
    ) -> Result<(), UsbError> {
        let mut rings = Vec::new();
        let input_addr = {
            let g = self
                .slots
                .get(usize::from(slot.0))
                .ok_or(UsbError::Invalid)?
                .lock();
            let sl = g.as_ref().ok_or(UsbError::Invalid)?;
            Self::clear(&sl.input);
            let mut add = 1u32;
            let mut last = 1u8;
            for e in endpoints {
                let Some(dci) = Self::dci(e.address) else {
                    continue;
                };
                let ring = ProducerRing::new(trbs(&self.tag, RING_TRBS)?);
                let ctx = self.endpoint_context(sl.speed, e, &ring);
                Self::write_ctx(&sl.input, self.csz.input_endpoint(dci), ctx.encode());
                add |= 1 << dci.get();
                last = last.max(dci.get());
                rings.push((dci, ring));
            }
            let mut sc = SlotContext::decode(&Self::read_ctx(&sl.out, self.csz.device_slot()));
            sc.entries = last;
            sc.state = 0;
            sc.address = 0;
            if let Some(h) = hub {
                sc.hub = true;
                sc.ports = h.ports;
                sc.ttt = h.think_time;
                sc.mtt = h.multi_tt;
            }
            Self::write_ctx(&sl.input, 0, input_control(0, add, 0, 0, 0));
            Self::write_ctx(&sl.input, self.csz.input_slot(), sc.encode());
            sl.input.bus_addr()
        };
        self.exec_ok(s, command::configure_endpoint(slot.0, input_addr))?;
        let mut g = self
            .slots
            .get(usize::from(slot.0))
            .ok_or(UsbError::Invalid)?
            .lock();
        let sl = g.as_mut().ok_or(UsbError::Invalid)?;
        for (dci, ring) in rings {
            if let Some(e) = sl.eps.get_mut(usize::from(dci.get())) {
                let mp = endpoints
                    .iter()
                    .find(|e| Self::dci(e.address) == Some(dci))
                    .map_or(0, Endpoint::max_packet);
                *e = Some(Ep {
                    ring,
                    control: false,
                    max_packet: mp,
                });
            }
        }
        Ok(())
    }

    fn disable_device(&self, s: &Sleepable, slot: SlotId) {
        let _ = self.exec(s, command::disable_slot(slot.0));
        let sl = self
            .slots
            .get(usize::from(slot.0))
            .and_then(|c| c.lock().take());
        self.dcbaa.write(usize::from(slot.0), 0);
        if let Some(mut sl) = sl {
            let mut gone = Vec::new();
            for ep in sl.eps.iter_mut().flatten() {
                gone.extend(ep.ring.drain());
            }
            for f in gone {
                f.finish(Err(UsbError::Gone));
            }
        }
    }

    fn submit(
        &self,
        slot: SlotId,
        endpoint: u8,
        setup: Option<Setup>,
        mut t: Transfer,
    ) -> Result<(), Rejected> {
        let reject = |t: Transfer, error| Err(Rejected { transfer: t, error });
        let Some(dci) = Self::dci(endpoint) else {
            return reject(t, UsbError::Invalid);
        };
        if self.is_stopping() {
            return reject(t, UsbError::Gone);
        }
        let is_in = match setup {
            Some(p) => p.is_in(),
            None => endpoint & 0x80 != 0,
        };
        let len = t.len();
        let buf = core::mem::take(&mut t.buf);
        let (data, segs): (Data, Vec<(u64, u32)>) = if len == 0 {
            (Data::None, Vec::new())
        } else if is_in {
            match self.data_tag.map::<dir::FromDevice, _>(buf) {
                Ok(m) => {
                    let segs = segments(m.segments());
                    (Data::In(m), segs)
                }
                Err((b, _)) => {
                    t.buf = b;
                    return reject(t, UsbError::NoMemory);
                }
            }
        } else {
            match self.data_tag.map::<dir::ToDevice, _>(buf) {
                Ok(m) => {
                    let segs = segments(m.segments());
                    (Data::Out(m), segs)
                }
                Err((b, _)) => {
                    t.buf = b;
                    return reject(t, UsbError::NoMemory);
                }
            }
        };

        let mut g = match self.slots.get(usize::from(slot.0)) {
            Some(c) => c.lock(),
            None => return reject(unmapped(t, data), UsbError::Invalid),
        };
        let Some(sl) = g.as_mut() else {
            return reject(unmapped(t, data), UsbError::Gone);
        };
        let Some(ep) = sl
            .eps
            .get_mut(usize::from(dci.get()))
            .and_then(Option::as_mut)
        else {
            return reject(unmapped(t, data), UsbError::Invalid);
        };
        let mp = ep.max_packet;
        let mut chain = Vec::new();
        let mut lens = Vec::new();
        let total: u32 = segs.iter().map(|&(_, l)| l).sum();
        let mut left = total;
        if let Some(p) = setup {
            let sp = SetupPacket {
                request_type: p.request_type,
                request: p.request,
                value: p.value,
                index: p.index,
                length: p.length,
            };
            chain.push(with_chain(tt::setup(&sp, 0)));
            lens.push(0);
            for (i, &(addr, l)) in segs.iter().enumerate() {
                left -= l;
                let last = i + 1 == segs.len();
                let size = tt::td_size(left, mp, last);
                let f = flag::CHAIN | flag::ISP;
                let trb = if i == 0 {
                    tt::data(addr, l, size, is_in, f, 0)
                } else {
                    tt::normal(addr, l, size, f, 0)
                };
                chain.push(trb);
                lens.push(l);
            }
            let data_in = (len > 0).then_some(is_in);
            chain.push(tt::status_stage(data_in, 0));
            lens.push(0);
        } else if segs.is_empty() {
            chain.push(tt::normal(0, 0, 0, flag::IOC, 0));
            lens.push(0);
        } else {
            for (i, &(addr, l)) in segs.iter().enumerate() {
                left -= l;
                let last = i + 1 == segs.len();
                let size = tt::td_size(left, mp, last);
                let f = if last {
                    flag::IOC | flag::ISP
                } else {
                    flag::CHAIN | flag::ISP
                };
                chain.push(tt::normal(addr, l, size, f, 0));
                lens.push(l);
            }
        }
        let f = InFlight {
            left_ms: (t.timeout_ms > 0).then_some(t.timeout_ms),
            t,
            data,
            lens,
            short: None,
            expired: false,
        };
        // On a full ring the chain is not taken: hand the transfer back.
        if chain.len() >= RING_TRBS {
            drop(g);
            let InFlight { t, data, .. } = f;
            return reject(unmapped(t, data), UsbError::Invalid);
        }
        if ep.ring.free() < chain.len() {
            drop(g);
            let InFlight { t, data, .. } = f;
            return reject(unmapped(t, data), UsbError::Busy);
        }
        // Checked above: the push cannot fail (and a failure would
        // complete the dropped transfer as cancelled).
        let _ = ep.ring.push(&chain, f);
        drop(g);
        self.regs.doorbell(slot.0, r::doorbell(dci.get(), 0));
        Ok(())
    }

    fn reset_endpoint(&self, s: &Sleepable, slot: SlotId, endpoint: u8) -> Result<(), UsbError> {
        let dci = Self::dci(endpoint).ok_or(UsbError::Invalid)?;
        match self.exec(s, command::reset_endpoint(slot.0, dci.get()))? {
            // Not halted: nothing to reset.
            (CompletionCode::Success | CompletionCode::ContextState, _) => {}
            (c, _) => return Err(error(c)),
        }
        self.cancel(s, slot.0, dci, UsbError::Cancelled, false);
        Ok(())
    }

    fn abort(&self, s: &Sleepable, slot: SlotId, endpoint: u8) {
        if let Some(dci) = Self::dci(endpoint) {
            self.cancel(s, slot.0, dci, UsbError::Cancelled, true);
        }
    }
}

impl Hc {
    /// Address Device for a new slot, with its contexts and default
    /// control endpoint ring.
    fn address(&self, s: &Sleepable, slot: u8, d: &NewDevice) -> Result<(), UsbError> {
        let out = self
            .tag
            .coherent::<u32>(self.csz.device_bytes() / 4)
            .map_err(|_| UsbError::NoMemory)?;
        let input = self
            .tag
            .coherent::<u32>(self.csz.input_bytes() / 4)
            .map_err(|_| UsbError::NoMemory)?;
        Self::clear(&out);
        Self::clear(&input);
        let ring = ProducerRing::new(trbs(&self.tag, RING_TRBS)?);
        let (deq, dcs) = ring.dequeue();
        let (tt_slot, tt_port, mtt) = match d.tt {
            Some(Tt { hub, port, multi }) => (hub.0, port, multi),
            None => (0, 0, false),
        };
        let sc = SlotContext {
            route: d.route,
            speed: xspeed(d.speed).psiv(),
            mtt,
            entries: 1,
            root_port: d.root_port,
            tt_slot,
            tt_port,
            ..SlotContext::default()
        };
        let ep0 = EndpointContext {
            state: 0,
            mult: 0,
            max_primary_streams: 0,
            lsa: false,
            interval: 0,
            error_count: 3,
            ep_type: Some(EndpointType::Control),
            max_burst: 0,
            max_packet: d.speed.default_max_packet(),
            dequeue: deq,
            dcs,
            average_trb_length: 8,
            max_esit_payload: 0,
        };
        Self::write_ctx(&input, 0, input_control(0, 0b11, 0, 0, 0));
        Self::write_ctx(&input, self.csz.input_slot(), sc.encode());
        Self::write_ctx(&input, self.csz.input_endpoint(Dci::CONTROL), ep0.encode());
        self.dcbaa.write(usize::from(slot), out.bus_addr());
        let input_addr = input.bus_addr();
        let mut eps: Vec<Option<Ep>> = (0..32).map(|_| None).collect();
        eps[1] = Some(Ep {
            ring,
            control: true,
            max_packet: d.speed.default_max_packet(),
        });
        *self
            .slots
            .get(usize::from(slot))
            .ok_or(UsbError::Invalid)?
            .lock() = Some(Slot {
            out,
            input,
            speed: d.speed,
            eps,
        });
        self.exec_ok(s, command::address_device(slot, input_addr, false))
            .map(|_| ())
    }
}

fn segments(segs: &[vm::dma::Segment]) -> Vec<(u64, u32)> {
    segs.iter()
        .map(|g| (g.bus, u32::try_from(g.len).unwrap_or(trb::MAX_TRB_BYTES)))
        .collect()
}

fn with_chain(t: Trb) -> Trb {
    Trb([t.0[0], t.0[1], t.0[2], t.0[3] | flag::CHAIN])
}

/// The transfer with its buffer unmapped, for handing back.
fn unmapped(mut t: Transfer, data: Data) -> Transfer {
    match data {
        Data::None => {}
        Data::In(m) => t.buf = m.unmap(),
        Data::Out(m) => t.buf = m.unmap(),
    }
    t
}
