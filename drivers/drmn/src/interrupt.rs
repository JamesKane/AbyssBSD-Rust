// SPDX-License-Identifier: BSD-2-Clause
//! The Radeon's interrupts (stage R2.1 of docs/GRAPHICS.md §16.6): the
//! interrupt handler's ring in video memory, the vector `vgapci` owns, and
//! the display's frame-start event, which is the vertical blanking
//! interrupt.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use amdgpu::doorbell;
use amdgpu::flip;
use amdgpu::ih;
use amdgpu::regs::Window;
use irq::Filter;
use sched::IrqCx;
use vgapci::{Aperture, InterruptSink};

use crate::amd::Bar5;

/// Where the interrupt handler's ring is in video memory (BAR0 offset): past
/// the display surfaces (`amd-kms`), well below the top 2 MiB.
pub(crate) const RING_AT: usize = 128 << 20;
/// Its size: 2,048 entries.
pub(crate) const RING_BYTES: u32 = 64 << 10;
/// Its write-back slot, right after it.
pub(crate) const WRITEBACK_AT: usize = RING_AT + RING_BYTES as usize;

/// `OTG0_OTG_GLOBAL_SYNC_STATUS`: bit 0 enables the frame-start interrupt,
/// writing bit 4 clears its event (08 section 2.5).
const OTG0_GLOBAL_SYNC_STATUS: usize = 0x14128;
const FRAME_START_ENABLE: u32 = 1 << 0;
const FRAME_START_CLEAR: u32 = 1 << 4;

/// The display's client id in an interrupt entry, and the frame-start
/// source of the first timing generator.
const CLIENT_DISPLAY: u8 = 4;
const SOURCE_FRAME_START_0: u8 = 0x3c;
/// The graphics command processor's client id, and its end-of-pipe source
/// (a fence with the interrupt bit; 06 section 11.1).
const CLIENT_GRBM_CP: u8 = 0x14;
const SOURCE_EOP: u8 = 181;
/// The system DMA engine's client and its trap source (07 section 6).
const CLIENT_SDMA0: u8 = amdgpu::sdma::IH_CLIENT;
const SOURCE_SDMA_TRAP: u8 = amdgpu::sdma::IH_SOURCE_TRAP;

/// What an interrupt entry announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    /// The first timing generator began a frame.
    FrameStart,
    /// A job's fence: the graphics command processor's end-of-pipe event or
    /// the system DMA engine's trap.
    Fence,
    /// Anything else.
    Other,
}

/// Which event the entry from `client` with `source` is.
fn classify(client: u8, source: u8) -> Event {
    match (client, source) {
        (CLIENT_DISPLAY, SOURCE_FRAME_START_0) => Event::FrameStart,
        (CLIENT_GRBM_CP, SOURCE_EOP) | (CLIENT_SDMA0, SOURCE_SDMA_TRAP) => Event::Fence,
        _ => Event::Other,
    }
}

/// The interrupts of one GPU, and what they were.
pub(crate) struct Irq {
    gpu: Weak<Aperture>,
    /// Whether the ring is made and the sink should act.
    active: AtomicBool,
    /// The read pointer, a byte offset, as last given to the hardware.
    rptr: AtomicU32,
    /// Whether the read pointer goes to the hardware through the doorbell
    /// (stage R2.4) rather than the register.
    doorbell: AtomicBool,
    /// Called for each frame-start event, in interrupt context.
    on_frame: sync::Once<alloc::boxed::Box<dyn Fn(u64) + Send + Sync>>,
    /// Called when a fence interrupt came, in interrupt context.
    on_eop: sync::Once<alloc::boxed::Box<dyn Fn() + Send + Sync>>,
    eops: AtomicU64,
    interrupts: AtomicU64,
    entries: AtomicU64,
    frames: AtomicU64,
    other: AtomicU64,
    overflows: AtomicU64,
    /// The client and source of the last entry that was not a frame event.
    last_other: AtomicU32,
}

impl core::fmt::Debug for Irq {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Irq").finish_non_exhaustive()
    }
}

impl Irq {
    pub(crate) fn new(gpu: Weak<Aperture>) -> Self {
        Self {
            gpu,
            active: AtomicBool::new(false),
            rptr: AtomicU32::new(0),
            doorbell: AtomicBool::new(false),
            on_frame: sync::Once::new(),
            on_eop: sync::Once::new(),
            eops: AtomicU64::new(0),
            interrupts: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            other: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            last_other: AtomicU32::new(0),
        }
    }

    /// Sets what each frame-start event does (once): `f` gets the time in
    /// nanoseconds, in interrupt context.
    #[allow(
        dead_code,
        reason = "wired to the card once the board shows the interrupts arrive"
    )]
    pub(crate) fn set_on_eop(&self, f: alloc::boxed::Box<dyn Fn() + Send + Sync>) -> bool {
        self.on_eop.set(f).is_ok()
    }

    pub(crate) fn set_on_frame(&self, f: alloc::boxed::Box<dyn Fn(u64) + Send + Sync>) -> bool {
        self.on_frame.set(f).is_ok()
    }

    /// Whether the interrupts are running.
    pub(crate) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// The counts, for a sysctl.
    pub(crate) fn summary(&self) -> String {
        let mut s = String::new();
        let other = self.last_other.load(Ordering::Relaxed);
        let _ = write!(
            s,
            "{}: {} interrupts, {} entries, {} frame starts, {} fence interrupts, {} others (last client {} source {}), {} overflows",
            if self.active() { "on" } else { "off" },
            self.interrupts.load(Ordering::Relaxed),
            self.entries.load(Ordering::Relaxed),
            self.frames.load(Ordering::Relaxed),
            self.eops.load(Ordering::Relaxed),
            self.other.load(Ordering::Relaxed),
            other >> 8,
            other & 0xff,
            self.overflows.load(Ordering::Relaxed),
        );
        s
    }

    /// Makes the ring, registers `sink` for the vector, and turns the
    /// handler and the display's frame-start interrupt on.
    ///
    /// # Errors
    ///
    /// `ENXIO` with no aperture or video memory window, `ENODEV` with no
    /// vector, `EBUSY` if a sink is registered already, `EINVAL` for a ring
    /// the hardware does not take.
    pub(crate) fn start(self: &Arc<Self>) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu.upgrade().ok_or(Errno::ENXIO)?;
        if gpu
            .vram_len()
            .is_none_or(|len| len < RING_AT + (RING_BYTES as usize) + 16)
        {
            return Err(Errno::ENXIO);
        }
        if !gpu.has_interrupt() {
            return Err(Errno::ENODEV);
        }
        if self.active() {
            return Err(Errno::EBUSY);
        }
        let w = Bar5(&gpu);
        // Where video memory is, in the GPU's address space.
        let base = u64::from(w.read32(flip::FB_LOCATION_BASE) & 0xffff) << 24;
        // The ring and its slot start out zero.
        for at in (0..RING_BYTES as usize + 16).step_by(4) {
            gpu.vram_write32(RING_AT + at, 0);
        }
        self.rptr.store(0, Ordering::Release);
        let sink: Arc<dyn InterruptSink> = Arc::new(Sink(Arc::clone(self)));
        if !gpu.set_sink(sink) {
            return Err(Errno::EBUSY);
        }
        let ring = ih::Ring {
            address: base + RING_AT as u64,
            bytes: RING_BYTES,
            writeback: base + WRITEBACK_AT as u64,
            snoop: false,
        };
        // The ring first, then the display's event, so nothing is raised
        // into a ring that is not there.
        self.active.store(true, Ordering::Release);
        if ih::bring_up(&w, &ring, true).is_err() {
            self.active.store(false, Ordering::Release);
            return Err(Errno::EINVAL);
        }
        w.write32(
            OTG0_GLOBAL_SYNC_STATUS,
            (w.read32(OTG0_GLOBAL_SYNC_STATUS) & FRAME_START_ENABLE) | FRAME_START_ENABLE,
        );
        Ok(())
    }

    /// Turns the display's frame-start event back on after the timing
    /// generator was stopped and started (a mode change), if the handler runs.
    pub(crate) fn rearm_frame_start(&self) {
        if !self.active() {
            return;
        }
        if let Some(gpu) = self.gpu.upgrade() {
            let w = Bar5(&gpu);
            w.write32(
                OTG0_GLOBAL_SYNC_STATUS,
                (w.read32(OTG0_GLOBAL_SYNC_STATUS) & FRAME_START_ENABLE) | FRAME_START_ENABLE,
            );
        }
    }

    /// Moves the read pointer to the doorbell (stage R2.4): the aperture
    /// enabled, the bus interface's range for the handler, and the handler's
    /// doorbell register. Returns the read pointer the driver holds and the
    /// one the register shows after a moment, which agree if the hardware
    /// took the doorbell.
    ///
    /// # Errors
    ///
    /// `ENXIO` with no doorbell aperture, `EINVAL` if the interrupts are
    /// not running.
    pub(crate) fn use_doorbell(&self) -> Result<(u32, u32), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu.upgrade().ok_or(Errno::ENXIO)?;
        if gpu
            .doorbell_len()
            .is_none_or(|l| l < (doorbell::IH_DOORBELL_DWORD as usize + 2) * 4)
        {
            return Err(Errno::ENXIO);
        }
        if !self.active() {
            return Err(Errno::EINVAL);
        }
        let w = Bar5(&gpu);
        doorbell::program_ih_range(&w);
        w.write32(
            ih::DOORBELL_RPTR,
            doorbell::ih_doorbell_rptr(doorbell::IH_DOORBELL_DWORD),
        );
        self.doorbell.store(true, Ordering::Release);
        // Publish the current pointer once through the doorbell, then give
        // the hardware a moment (it is an inbound write) and read the
        // register.
        let held = self.rptr.load(Ordering::Acquire);
        gpu.doorbell_write32((doorbell::IH_DOORBELL_DWORD * 4) as usize, held);
        bus::manager().delay_us(2000);
        let shown = w.read32(ih::RB_RPTR);
        Ok((self.rptr.load(Ordering::Acquire), shown))
    }

    /// Turns the display's event and the handler off.
    pub(crate) fn stop(&self) {
        if !self.active.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some(gpu) = self.gpu.upgrade() {
            let w = Bar5(&gpu);
            w.write32(OTG0_GLOBAL_SYNC_STATUS, 0);
            ih::disable(&w);
        }
    }

    /// Reads the ring and handles what is in it.
    fn handle(&self) -> Filter {
        self.interrupts.fetch_add(1, Ordering::Relaxed);
        if !self.active() {
            return Filter::Stray;
        }
        let Some(gpu) = self.gpu.upgrade() else {
            return Filter::Stray;
        };
        let w = Bar5(&gpu);
        let wptr = gpu.vram_read32(WRITEBACK_AT).unwrap_or(0);
        let rptr = self.rptr.load(Ordering::Acquire);
        let mut frame = false;
        let mut eop = false;
        let d = ih::drain(
            RING_BYTES,
            rptr,
            wptr,
            &|o| gpu.vram_read32(RING_AT + o as usize).unwrap_or(0),
            &mut |e| {
                self.entries.fetch_add(1, Ordering::Relaxed);
                match classify(e.client, e.source) {
                    Event::FrameStart => frame = true,
                    Event::Fence => {
                        eop = true;
                        self.eops.fetch_add(1, Ordering::Relaxed);
                    }
                    Event::Other => {
                        self.other.fetch_add(1, Ordering::Relaxed);
                        self.last_other.store(
                            u32::from(e.client) << 8 | u32::from(e.source),
                            Ordering::Relaxed,
                        );
                    }
                }
            },
        );
        self.rptr.store(d.rptr, Ordering::Release);
        if d.overflowed {
            self.overflows.fetch_add(1, Ordering::Relaxed);
            // Clear the overflow: write 1 to the clear bit, then 0 (05 8.3).
            let c = w.read32(ih::RB_CNTL);
            w.write32(ih::RB_CNTL, c | ih::cntl::WPTR_OVERFLOW_CLEAR);
            w.write32(ih::RB_CNTL, c & !ih::cntl::WPTR_OVERFLOW_CLEAR);
        }
        if self.doorbell.load(Ordering::Acquire) {
            gpu.doorbell_write32((doorbell::IH_DOORBELL_DWORD * 4) as usize, d.rptr);
        } else {
            w.write32(ih::RB_RPTR, d.rptr);
        }
        if eop && let Some(f) = self.on_eop.get() {
            f();
        }
        if frame {
            // Acknowledge the event, keeping the interrupt enabled.
            w.write32(
                OTG0_GLOBAL_SYNC_STATUS,
                (w.read32(OTG0_GLOBAL_SYNC_STATUS) & FRAME_START_ENABLE) | FRAME_START_CLEAR,
            );
            self.frames.fetch_add(1, Ordering::Relaxed);
            if let Some(f) = self.on_frame.get() {
                f(sched::now_ns());
            }
        }
        Filter::Handled
    }
}

/// What `vgapci` calls from the vector's filter.
struct Sink(Arc<Irq>);

impl InterruptSink for Sink {
    fn interrupt(&self, _: &IrqCx) -> Filter {
        self.0.handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_told_apart_by_client_and_source() {
        assert_eq!(classify(4, 0x3c), Event::FrameStart);
        // The graphics command processor's end-of-pipe event and the DMA
        // engine's trap are both a finished job.
        assert_eq!(classify(0x14, 181), Event::Fence);
        assert_eq!(classify(0x08, 224), Event::Fence);
        // The same source from another client, or another source from a
        // known client, is neither.
        assert_eq!(classify(0x14, 224), Event::Other);
        assert_eq!(classify(0x08, 181), Event::Other);
        assert_eq!(classify(4, 0x3d), Event::Other);
        assert_eq!(classify(0, 0), Event::Other);
    }
}
