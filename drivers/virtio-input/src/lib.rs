// SPDX-License-Identifier: BSD-2-Clause
//! Virtio input devices (`vtinput`): keyboards, mice, and tablets.
//!
//! FreeBSD has no driver for these; the specification (virtio 1.2 §5.8)
//! is the reference, and the user interface is FreeBSD's, as `hkbd` and
//! `hms` give it. The name follows FreeBSD's other virtio drivers
//! (`vtblk`, `vtrnd`).
//!
//! The device describes itself in its configuration space, one selected
//! page at a time: its name, serial, `input_id`, properties, event bits,
//! and the range of each absolute axis. That becomes the evdev device as
//! it stands. Its events are already evdev events: eight bytes each on the
//! event queue, which the driver keeps full of buffers.
//!
//! A device with keyboard keys (any key that has a HID usage) is also a
//! `kbd` under `kbdmux`, as `hkbd` is: its keys go through the input
//! class, which types them at the console and reports them to evdev under
//! `kern.evdev.rcpt_mask`'s keyboard bit, and repeats a held key itself
//! (so the device's own repeats are dropped). The lock LEDs go back to it
//! on the status queue. Any other device reports straight to evdev, as
//! `hms` does.
//!
//! Behavior references: virtio 1.2 §5.8; sys/dev/hid/hkbd.c and hms.c
//! for what the input class sees.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::evdev::{
    BTN_MISC, EV_ABS, EV_CNT, EV_KEY, EV_LED, EV_MSC, EV_REL, EV_REP, EV_SND, EV_SW, EV_SYN,
    InputId, LED_CAPSL, LED_NUML, LED_SCROLLL, SYN_REPORT,
};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, StartCx, StopCx, StopReason,
};
use devtree::ids::VirtioId;
use input::{Keyboard, Leds};
use irq::Filter;
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable};
use sync::SpinLock;
use virtio::{InterruptMask, VirtioDevice, Virtqueue, device_type};
use vm::dma::{DmaCoherent, DmaConstraints};

/// The configuration space (§5.8.4): what to select, then the selected
/// page's size and contents.
mod cfg {
    pub(super) const SELECT: u16 = 0;
    pub(super) const SUBSEL: u16 = 1;
    pub(super) const SIZE: u16 = 2;
    pub(super) const DATA: u16 = 8;

    pub(super) const ID_NAME: u8 = 0x01;
    pub(super) const ID_SERIAL: u8 = 0x02;
    pub(super) const ID_DEVIDS: u8 = 0x03;
    pub(super) const PROP_BITS: u8 = 0x10;
    pub(super) const EV_BITS: u8 = 0x11;
    pub(super) const ABS_INFO: u8 = 0x12;
}

/// One event, as the queues carry it: type, code, value.
const EVENT: usize = 8;

/// Entries wanted in each queue.
const QUEUE: u16 = 64;

/// The status queue's buffers: an LED update is three LEDs and a
/// `SYN_REPORT`.
const STATUS_EVENTS: usize = 4;

static QUEUE_LOCK: LockClass = LockClass::new("vtinput events", 500);

/// The event queue, its buffers, and which buffer each ring entry holds.
#[derive(Debug)]
struct Events {
    vq: Virtqueue,
    buf: DmaCoherent<u8>,
    slots: Vec<Option<usize>>,
}

impl Events {
    /// Hands buffer `i` to the device.
    fn post(&mut self, i: usize) {
        let Ok(off) = u64::try_from(i * EVENT) else {
            return;
        };
        if let Ok(head) = self.vq.push(&[], &[(self.buf.bus_addr() + off, 8)])
            && let Some(slot) = self.slots.get_mut(usize::from(head))
        {
            *slot = Some(i);
        }
    }
}

/// The status queue, for the lock LEDs; filled and reclaimed under its
/// lock, never waited on.
#[derive(Debug)]
struct Status {
    vq: Virtqueue,
    buf: DmaCoherent<u8>,
    /// The next buffer to fill, round robin.
    next: usize,
}

impl Status {
    /// Sends the lock LEDs: one buffer of `EV_LED` events and a
    /// `SYN_REPORT`, dropped if the device holds every buffer.
    fn leds(&mut self, leds: Leds) {
        while let Ok(Some(_)) = self.vq.pop() {}
        let slots = self.buf.len() / (EVENT * STATUS_EVENTS);
        if slots == 0 || self.vq.ring().free() == 0 {
            return;
        }
        let at = self.next * EVENT * STATUS_EVENTS;
        self.next = (self.next + 1) % slots;
        let events = [
            (EV_LED, LED_NUML, leds.num),
            (EV_LED, LED_CAPSL, leds.caps),
            (EV_LED, LED_SCROLLL, leds.scroll),
            (EV_SYN, SYN_REPORT, false),
        ];
        for (i, (kind, code, on)) in events.into_iter().enumerate() {
            let off = at + i * EVENT;
            self.buf.write_at::<u16>(off, kind);
            self.buf.write_at::<u16>(off + 2, code);
            self.buf.write_at::<u32>(off + 4, u32::from(on));
        }
        let (Ok(off), Ok(len)) = (u64::try_from(at), u32::try_from(EVENT * STATUS_EVENTS)) else {
            return;
        };
        if self
            .vq
            .push(&[(self.buf.bus_addr() + off, len)], &[])
            .is_ok()
        {
            self.vq.kick();
        }
    }
}

/// What the device describes, read at attach.
#[derive(Debug, Default)]
struct Config {
    name: String,
    serial: String,
    id: InputId,
    props: Vec<u16>,
    /// Each event type's codes.
    bits: Vec<(u16, Vec<u16>)>,
    /// Each absolute axis: code, minimum, maximum, fuzz, flat, resolution.
    abs: Vec<(u16, [i32; 5])>,
}

impl Config {
    fn codes(&self, kind: u16) -> &[u16] {
        self.bits
            .iter()
            .find(|(k, _)| *k == kind)
            .map_or(&[], |(_, c)| c)
    }

    /// Whether it has keyboard keys: keys below the buttons that a HID
    /// keyboard can name.
    fn keyboard(&self) -> bool {
        self.codes(EV_KEY)
            .iter()
            .any(|&c| c < BTN_MISC && evdev::keys::key2hid(c).is_some())
    }
}

/// Selects page `select`/`subsel`; its size.
fn select(dev: &VirtioDevice, select: u8, subsel: u8) -> usize {
    dev.set_config8(cfg::SELECT, select);
    dev.set_config8(cfg::SUBSEL, subsel);
    usize::from(dev.config8(cfg::SIZE).min(128))
}

/// The selected page's first `n` bytes.
fn bytes(dev: &VirtioDevice, n: usize) -> Vec<u8> {
    (0..n)
        .filter_map(|i| u16::try_from(i).ok())
        .map(|i| dev.config8(cfg::DATA + i))
        .collect()
}

/// A string page, up to its NUL, with anything but printable ASCII
/// replaced.
fn string(dev: &VirtioDevice, sel: u8) -> String {
    let n = select(dev, sel, 0);
    bytes(dev, n)
        .into_iter()
        .take_while(|&b| b != 0)
        .map(|b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

/// A bitmap page's set bits.
fn bitmap(dev: &VirtioDevice, sel: u8, subsel: u8) -> Vec<u16> {
    let n = select(dev, sel, subsel);
    let mut codes = Vec::new();
    for (i, b) in bytes(dev, n).into_iter().enumerate() {
        for bit in 0..8 {
            if b & (1 << bit) != 0
                && let Ok(code) = u16::try_from(i * 8 + bit)
            {
                codes.push(code);
            }
        }
    }
    codes
}

impl Config {
    fn read(dev: &VirtioDevice) -> Self {
        let mut c = Self {
            name: string(dev, cfg::ID_NAME),
            serial: string(dev, cfg::ID_SERIAL),
            ..Self::default()
        };
        if select(dev, cfg::ID_DEVIDS, 0) >= 8 {
            c.id = InputId {
                bustype: dev.config16(cfg::DATA),
                vendor: dev.config16(cfg::DATA + 2),
                product: dev.config16(cfg::DATA + 4),
                version: dev.config16(cfg::DATA + 6),
            };
        }
        c.props = bitmap(dev, cfg::PROP_BITS, 0);
        for kind in 1..EV_CNT {
            let Ok(sub) = u8::try_from(kind) else { break };
            let codes = bitmap(dev, cfg::EV_BITS, sub);
            if !codes.is_empty() {
                c.bits.push((kind, codes));
            }
        }
        for code in c.codes(EV_ABS).to_vec() {
            let Ok(sub) = u8::try_from(code) else {
                continue;
            };
            if select(dev, cfg::ABS_INFO, sub) < 20 {
                continue;
            }
            let field = |i: u16| dev.config32(cfg::DATA + 4 * i).cast_signed();
            c.abs
                .push((code, [field(0), field(1), field(2), field(3), field(4)]));
        }
        c
    }
}

/// Where the device's events go.
#[derive(Debug)]
struct Sink {
    evdev: Arc<evdev::Device>,
    kbd: Option<Arc<Keyboard>>,
}

impl Sink {
    /// One event from the device.
    fn event(&self, kind: u16, code: u16, value: i32) {
        let Some(k) = &self.kbd else {
            if kind == EV_SYN {
                if code == SYN_REPORT {
                    self.evdev.sync();
                }
            } else {
                let _ = self.evdev.push(kind, code, value);
            }
            return;
        };
        match kind {
            EV_SYN if code == SYN_REPORT => k.sync(),
            // The input class repeats held keys itself.
            EV_KEY if value > 1 => {}
            EV_KEY => match evdev::keys::key2hid(code) {
                Some(u) => k.key(u16::from(u), value != 0),
                None if evdev::receives(evdev::RCPT_HW_KBD) => {
                    self.evdev.push_key(code, value != 0);
                }
                None => {}
            },
            // The LEDs are the class's to set.
            EV_LED => {}
            _ if evdev::receives(evdev::RCPT_HW_KBD) => {
                let _ = self.evdev.push(kind, code, value);
            }
            _ => {}
        }
    }
}

/// A `vtinputN` instance.
pub struct VtInput {
    dev: Arc<VirtioDevice>,
    unit: u32,
    config: Config,
    events: Mutex<Events>,
    status: Arc<SpinLock<Status>>,
    intr: InterruptMask,
    line: SpinLock<Option<IrqLine>>,
    sink: SpinLock<Option<Arc<Sink>>>,
}

impl core::fmt::Debug for VtInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VtInput")
            .field("unit", &self.unit)
            .field("name", &self.config.name)
            .finish_non_exhaustive()
    }
}

fn filter(d: &VtInput, _: usize, _: &IrqCx) -> Filter {
    if d.dev.shared_line() && d.dev.isr() & 1 == 0 {
        return Filter::Stray;
    }
    d.intr.suppress();
    Filter::Schedule
}

/// Takes the events the device posted and gives their buffers back.
fn drain(d: &VtInput, _: usize, s: &Sleepable) {
    let sink = d.sink.lock().clone();
    let mut q = d.events.lock(s);
    q.vq.set_interrupts(false);
    let mut posted = false;
    loop {
        let used = match q.vq.pop() {
            Ok(Some(u)) => u,
            Ok(None) if q.vq.rearm() => break,
            Ok(None) => {
                q.vq.set_interrupts(false);
                continue;
            }
            Err(e) => {
                bus::manager().log(format_args!(
                    "vtinput{}: bad completion from the device: {e:?}",
                    d.unit
                ));
                q.vq.set_interrupts(true);
                break;
            }
        };
        let Some(i) = q
            .slots
            .get_mut(usize::from(used.head))
            .and_then(Option::take)
        else {
            continue;
        };
        if used.len >= 8
            && let Some(sink) = &sink
        {
            let off = i * EVENT;
            let kind = q.buf.read_at::<u16>(off);
            let code = q.buf.read_at::<u16>(off + 2);
            let value = q.buf.read_at::<u32>(off + 4).cast_signed();
            sink.event(kind, code, value);
        }
        q.post(i);
        posted = true;
    }
    if posted {
        q.vq.kick();
    }
}

impl Driver for VtInput {
    const NAME: &'static str = "vtinput";
    const IDS: Ids = Ids::Virtio(&[VirtioId::new(device_type::INPUT)]);

    fn describe(_: &Ident) -> String {
        "VirtIO Input Adapter".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let dev = cx
            .bus_data_arc::<VirtioDevice>()
            .ok_or(AttachError::Decline)?;
        dev.negotiate(0)
            .map_err(|_| AttachError::Device("feature negotiation failed"))?;
        let config = Config::read(&dev);
        // Once a queue is set up the device may DMA: any failure from here
        // resets it before the memory goes.
        let built = (|| {
            let tag = cx.dma_tag(DmaConstraints {
                addr_bits: if dev.is_modern() { 64 } else { 44 },
                ..DmaConstraints::UNLIMITED
            })?;
            let vq = dev
                .queue(&tag, 0, QUEUE)
                .map_err(|_| AttachError::Device("event queue"))?;
            let size = usize::from(vq.ring().size());
            let buf = tag
                .coherent::<u8>(size * EVENT)
                .map_err(|_| AttachError::NoMemory)?;
            let intr = vq.interrupt_mask();
            let svq = dev
                .queue(&tag, 1, QUEUE)
                .map_err(|_| AttachError::Device("status queue"))?;
            // Its buffers are reclaimed when the next LEDs go out.
            svq.set_interrupts(false);
            let ssize = usize::from(svq.ring().size());
            let sbuf = tag
                .coherent::<u8>(ssize * EVENT * STATUS_EVENTS)
                .map_err(|_| AttachError::NoMemory)?;
            let line = dev.queue_irq(0).map(|i| cx.irq(i)).transpose()?;
            let events = Events {
                vq,
                buf,
                slots: (0..size).map(|_| None).collect(),
            };
            let status = Status {
                vq: svq,
                buf: sbuf,
                next: 0,
            };
            Ok::<_, AttachError>((events, status, intr, line))
        })();
        let (events, status, intr, line) = built.inspect_err(|_| dev.reset())?;
        if line.is_none() {
            dev.reset();
            return Err(AttachError::Device("no interrupt"));
        }
        Ok(Self {
            dev,
            unit: cx.unit(),
            config,
            events: Mutex::new(&QUEUE_LOCK, events),
            status: Arc::new(SpinLock::new(status)),
            intr,
            line: SpinLock::new(line),
            sink: SpinLock::new(None),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let me = format!("vtinput{}", self.unit);
        let c = &self.config;
        let keyboard = c.keyboard();
        let mut b = evdev::Builder::new(&c.name);
        b.phys(&me).uniq(&c.serial).id(c.id);
        for &p in &c.props {
            b.prop(p);
        }
        for (kind, codes) in &c.bits {
            b.event(*kind);
            for &code in codes {
                match *kind {
                    EV_KEY => b.key(code),
                    EV_REL => b.rel(code),
                    EV_MSC => b.msc(code),
                    EV_LED => b.led(code),
                    EV_SND => b.snd(code),
                    EV_SW => b.sw(code),
                    _ => &mut b,
                };
            }
        }
        for &(code, [min, max, fuzz, flat, res]) in &c.abs {
            b.abs(code, min, max, fuzz, flat, res);
        }
        if keyboard {
            // The input class's repeats, as `hkbd` reports them.
            b.event(EV_REP);
        }
        let ev = b
            .register(cx.sleepable())
            .map_err(|_| AttachError::Device("no evdev device"))?;
        let kbd = keyboard.then(|| {
            let status = Arc::clone(&self.status);
            input::attach(
                &me,
                move |leds| status.lock().leds(leds),
                Some(Arc::clone(&ev)),
            )
        });
        *self.sink.lock() = Some(Arc::new(Sink { evdev: ev, kbd }));
        // Taken in its own statement: installing may sleep.
        let line = self.line.lock().take();
        if let Some(line) = line {
            cx.interrupt(line, Some(filter), Some(drain), 0)?;
        }
        self.dev.ready();
        let mut q = self.events.lock(cx.sleepable());
        for i in 0..q.slots.len() {
            q.post(i);
        }
        q.vq.kick();
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        // The device lets go of every buffer before the sink goes.
        self.dev.reset();
        let sink = self.sink.lock().take();
        if let Some(sink) = sink {
            if let Some(k) = &sink.kbd {
                k.detach();
            }
            sink.evdev.unregister(cx.sleepable());
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<VtInput>();

#[cfg(test)]
mod tests {
    use super::*;
    use abi::evdev::BTN_MOUSE;

    /// `KEY_A`.
    const KEY_A: u16 = 30;

    #[test]
    fn keyboards_have_keys_a_hid_keyboard_names() {
        let mut c = Config::default();
        c.bits.push((EV_KEY, alloc::vec![BTN_MOUSE]));
        assert!(!c.keyboard());
        c.bits[0].1.push(KEY_A);
        assert!(c.keyboard());
    }
}
