// SPDX-License-Identifier: BSD-2-Clause
//! The virtio entropy device (`vtrnd`).
//!
//! One request queue: the driver offers a buffer, the device fills it
//! with random bytes. Unlike FreeBSD's driver, which polls, completions
//! arrive by interrupt (MSI-X, or the shared INTx line with the ISR byte
//! saying whether it was this device), and the reader sleeps meanwhile.
//!
//! The device is published as an entropy source behind a [`Gate`]: detach
//! revokes the gate, waits for reads inside to finish, and only then
//! resets the device.
//!
//! Behavior reference: sys/dev/virtio/random/virtio_random.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Gate, Ident, Ids, IrqLine, StartCx, StopCx,
    StopReason,
};
use devtree::ids::VirtioId;
use irq::Filter;
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable, WaitQueue};
use sync::SpinLock;
use virtio::{VirtioDevice, Virtqueue, device_type};
use vm::dma::{DmaCoherent, DmaConstraints};

/// Bytes asked for per request.
const REQUEST: usize = 64;

/// Entries wanted in the request queue (one request is ever outstanding).
const QUEUE: u16 = 8;

static QUEUE_LOCK: LockClass = LockClass::new("vtrnd queue", 500);

/// The published sources, each with the gate that guards it.
static SOURCES: SpinLock<Vec<(Arc<Gate>, Arc<Source>)>> = SpinLock::new(Vec::new());

/// The request queue and its buffer.
#[derive(Debug)]
struct Queue {
    vq: Virtqueue,
    buf: DmaCoherent<u8>,
}

/// One device as an entropy source.
#[derive(Debug)]
pub struct Source {
    queue: Mutex<Queue>,
    done: WaitQueue,
    /// No interrupt: readers poll.
    polled: bool,
    /// The interrupt is a shared INTx line rather than a message.
    shared: bool,
    interrupts: AtomicU64,
}

impl Source {
    fn read(&self, s: &Sleepable, out: &mut [u8]) -> usize {
        let mut q = self.queue.lock(s);
        let want = out.len().min(REQUEST);
        let addr = q.buf.bus_addr();
        let len = u32::try_from(want).unwrap_or(0);
        if q.vq.push(&[], &[(addr, len)]).is_err() {
            return 0;
        }
        q.vq.kick();
        if self.polled {
            while !q.vq.has_used() {
                sched::yield_now();
            }
        } else {
            self.done.wait_until(s, || q.vq.has_used());
        }
        match q.vq.pop() {
            Ok(Some(used)) => {
                let n = (used.len as usize).min(want);
                for (i, b) in out[..n].iter_mut().enumerate() {
                    *b = q.buf.read(i);
                }
                n
            }
            _ => 0,
        }
    }

    /// Interrupts taken.
    #[must_use]
    pub fn interrupts(&self) -> u64 {
        self.interrupts.load(Ordering::Relaxed)
    }
}

/// Fills `out` (up to 64 bytes) from the first entropy device, returning
/// how many bytes it gave, or `None` with no device attached.
pub fn read(s: &Sleepable, out: &mut [u8]) -> Option<usize> {
    let gate;
    let inside;
    let src;
    {
        let sources = SOURCES.lock();
        let (g, source) = sources.first()?;
        gate = Arc::clone(g);
        // Entered under the registry lock: once detach has revoked the
        // gate, no new reader gets the source.
        inside = gate.enter().ok()?;
        src = Arc::clone(source);
    }
    let n = src.read(s, out);
    drop(src);
    drop(inside);
    Some(n)
}

/// Interrupts the first entropy device has taken, if one is attached.
#[must_use]
pub fn interrupts() -> Option<u64> {
    SOURCES.lock().first().map(|(_, s)| s.interrupts())
}

/// How the first entropy device interrupts: `Some(true)` on a shared INTx
/// line, `Some(false)` by message (MSI-X), `None` if it polls or there is
/// no device.
#[must_use]
pub fn shared_line() -> Option<bool> {
    SOURCES
        .lock()
        .first()
        .filter(|(_, s)| !s.polled)
        .map(|(_, s)| s.shared)
}

/// A `vtrndN` instance.
#[derive(Debug)]
pub struct VtRnd {
    dev: Arc<VirtioDevice>,
    src: Arc<Source>,
    gate: Arc<Gate>,
    line: SpinLock<Option<IrqLine>>,
}

fn interrupt(d: &VtRnd, _: usize, _: &IrqCx) -> Filter {
    // On a shared line, reading the ISR byte says whether it was us, and
    // acknowledges it.
    if d.dev.shared_line() && d.dev.isr() == 0 {
        return Filter::Stray;
    }
    d.src.interrupts.fetch_add(1, Ordering::Relaxed);
    d.src.done.wake_all();
    Filter::Handled
}

impl Driver for VtRnd {
    const NAME: &'static str = "vtrnd";
    const IDS: Ids = Ids::Virtio(&[VirtioId::new(device_type::ENTROPY)]);

    fn describe(_: &Ident) -> String {
        "VirtIO Entropy Adapter".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let dev = cx
            .bus_data_arc::<VirtioDevice>()
            .ok_or(AttachError::Decline)?;
        dev.negotiate(0)
            .map_err(|_| AttachError::Device("feature negotiation failed"))?;
        // Once a queue is set up the device may DMA: any failure from here
        // resets it before the memory goes.
        let built = (|| {
            // The legacy interface takes a 32-bit page number.
            let addr_bits = if dev.is_modern() { 64 } else { 44 };
            let tag = cx.dma_tag(DmaConstraints {
                addr_bits,
                ..DmaConstraints::UNLIMITED
            })?;
            let buf = tag
                .coherent::<u8>(REQUEST)
                .map_err(|_| AttachError::NoMemory)?;
            let vq = dev
                .queue(&tag, 0, QUEUE)
                .map_err(|_| AttachError::Device("request queue"))?;
            let line = dev.queue_irq(0).map(|i| cx.irq(i)).transpose()?;
            Ok::<_, AttachError>((Queue { vq, buf }, line))
        })();
        let (queue, line) = built.inspect_err(|_| dev.reset())?;
        Ok(Self {
            src: Arc::new(Source {
                queue: Mutex::new(&QUEUE_LOCK, queue),
                done: WaitQueue::new(),
                polled: line.is_none(),
                shared: dev.shared_line(),
                interrupts: AtomicU64::new(0),
            }),
            dev,
            gate: Arc::new(Gate::new()),
            line: SpinLock::new(line),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        if let Some(line) = self.line.lock().take() {
            cx.interrupt(line, Some(interrupt), None, 0)?;
        }
        self.dev.ready();
        SOURCES
            .lock()
            .push((Arc::clone(&self.gate), Arc::clone(&self.src)));
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.gate.revoke(cx.sleepable());
        SOURCES.lock().retain(|(g, _)| !Arc::ptr_eq(g, &self.gate));
        self.dev.reset();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<VtRnd>();
