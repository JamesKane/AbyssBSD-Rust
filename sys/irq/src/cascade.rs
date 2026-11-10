// SPDX-License-Identifier: BSD-2-Clause
//! Cascaded domains: interrupts a device multiplexes onto one input of its
//! own (DRIVER-MODEL.md §8.3).
//!
//! A GPIO controller raises one interrupt for all its pins. Its driver
//! implements [`Cascade`] for the pins and publishes a [`CascadeDomain`];
//! consumers install handlers on the domain's inputs like on any other
//! domain's. Each connected input gets a root id from the range
//! [`crate::Config::cascade_ids`] reserves, which no controller reports:
//! the driver's own filter, on the parent interrupt, finds the pending
//! inputs and runs their handlers with [`CascadeDomain::dispatch`]. This is
//! INTRNG's `intr_isrc_dispatch` from a parent's filter.
//!
//! Masking, the interrupt threads of level-triggered inputs, shared level
//! inputs, storm detection, and synchronous removal all work as for a
//! controller's inputs, through the driver's [`Cascade::mask`] and
//! [`Cascade::unmask`].
//!
//! Devices' interrupt descriptions name a domain as
//! `&'static dyn IrqDomain`, but a driver's domain lives only while it is
//! attached. So the domain consumers see is one of a fixed set of static
//! slots, which forwards to the driver's [`Cascade`] while the
//! [`CascadeDomain`] lives. Once that is dropped (the driver detached, its
//! consumers first), the slot ignores calls, and it is reused when its last
//! input is disconnected.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use hal::CpuId;
use sched::IrqCx;
use sync::SpinLock;

use crate::{IrqDomain, IrqError, Polarity, Trigger};

/// What a driver whose device multiplexes interrupts implements, for its
/// inputs by the device's own numbering (a GPIO pin).
pub trait Cascade: Send + Sync {
    /// A name for reports.
    fn name(&self) -> &'static str;
    /// How many inputs the device has.
    fn inputs(&self) -> u32;
    /// Sets input `hw` to signal by `trigger` and `polarity`, leaving it
    /// masked. [`IrqError::Unsupported`] if the device cannot.
    fn configure(&self, hw: u32, trigger: Trigger, polarity: Polarity) -> Result<(), IrqError>;
    /// The input's last handler is gone; it is masked already.
    fn release(&self, hw: u32) {
        let _ = hw;
    }
    /// Masks input `hw`. Called in interrupt context.
    fn mask(&self, hw: u32);
    /// Unmasks input `hw`. Called in interrupt context.
    fn unmask(&self, hw: u32);
    /// Whether handlers must run in a thread: the device needs bus I/O
    /// (I2C) to mask or acknowledge an input.
    fn threaded_only(&self) -> bool {
        false
    }
}

/// No root id.
const NONE: u32 = u32::MAX;

/// A live domain: the driver's inputs and each connected one's root id.
struct Shared {
    imp: Arc<dyn Cascade>,
    roots: Box<[AtomicU32]>,
    /// The driver dropped its [`CascadeDomain`]: calls no longer reach it.
    revoked: AtomicBool,
}

#[derive(Default)]
struct SlotState {
    shared: Option<Arc<Shared>>,
    /// Inputs connected through the slot.
    connected: u32,
}

/// One static domain consumers' descriptions can name.
struct Slot {
    state: SpinLock<SlotState>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: SpinLock::new(SlotState {
                shared: None,
                connected: 0,
            }),
        }
    }

    /// The driver's domain, if it is still live.
    fn live(&self) -> Option<Arc<Shared>> {
        let st = self.state.lock();
        let sh = st.shared.as_ref()?;
        (!sh.revoked.load(Ordering::Acquire)).then(|| Arc::clone(sh))
    }
}

/// How many cascaded domains may exist at once: one per GPIO controller,
/// with room (the Sky1 has seven).
const SLOTS: usize = 64;
static SLOT: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];

impl IrqDomain for Slot {
    fn name(&self) -> &'static str {
        self.live().map_or("cascade", |s| s.imp.name())
    }

    fn connect(
        &self,
        hw: u32,
        trigger: Trigger,
        polarity: Polarity,
        _cpu: CpuId,
    ) -> Result<u32, IrqError> {
        let sh = self.live().ok_or(IrqError::NoSuchInput)?;
        let r = sh.roots.get(hw as usize).ok_or(IrqError::NoSuchInput)?;
        if r.load(Ordering::Acquire) != NONE {
            return Err(IrqError::Busy);
        }
        sh.imp.configure(hw, trigger, polarity)?;
        let root = crate::alloc_cascade_id().ok_or(IrqError::NoVector)?;
        r.store(root, Ordering::Release);
        self.state.lock().connected += 1;
        Ok(root)
    }

    fn disconnect(&self, hw: u32, _cpu: CpuId, root: u32) {
        let mut st = self.state.lock();
        if let Some(sh) = st.shared.clone() {
            if let Some(r) = sh.roots.get(hw as usize) {
                r.store(NONE, Ordering::Release);
            }
            if !sh.revoked.load(Ordering::Acquire) {
                sh.imp.release(hw);
            }
        }
        crate::free_cascade_id(root);
        st.connected = st.connected.saturating_sub(1);
        let free = st.connected == 0
            && st
                .shared
                .as_ref()
                .is_some_and(|s| s.revoked.load(Ordering::Acquire));
        if free {
            st.shared = None;
        }
    }

    fn mask(&self, hw: u32) {
        if let Some(sh) = self.live() {
            sh.imp.mask(hw);
        }
    }

    fn unmask(&self, hw: u32) {
        if let Some(sh) = self.live() {
            sh.imp.unmask(hw);
        }
    }

    fn threaded_only(&self) -> bool {
        self.live().is_some_and(|s| s.imp.threaded_only())
    }

    fn pin_numbered(&self) -> bool {
        false
    }
}

/// A driver's cascaded domain. Dropping it (at detach, after its consumers
/// have removed their handlers) cuts the domain off from the driver.
pub struct CascadeDomain {
    slot: &'static Slot,
    shared: Arc<Shared>,
}

impl core::fmt::Debug for CascadeDomain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CascadeDomain")
            .field("name", &self.shared.imp.name())
            .field("inputs", &self.shared.roots.len())
            .finish()
    }
}

impl CascadeDomain {
    /// Publishes `imp`'s inputs as a domain. [`IrqError::NoVector`] if
    /// every slot is taken.
    pub fn new(imp: Arc<dyn Cascade>) -> Result<Self, IrqError> {
        let n = imp.inputs() as usize;
        let shared = Arc::new(Shared {
            imp,
            roots: (0..n).map(|_| AtomicU32::new(NONE)).collect(),
            revoked: AtomicBool::new(false),
        });
        for slot in &SLOT {
            let mut st = slot.state.lock();
            if st.shared.is_none() {
                st.shared = Some(Arc::clone(&shared));
                st.connected = 0;
                drop(st);
                return Ok(Self { slot, shared });
            }
        }
        Err(IrqError::NoVector)
    }

    /// The domain, for consumers' interrupt descriptions.
    #[must_use]
    pub fn domain(&self) -> &'static dyn IrqDomain {
        self.slot
    }

    /// Whether input `hw` has a handler.
    #[must_use]
    pub fn connected(&self, hw: u32) -> bool {
        self.shared
            .roots
            .get(hw as usize)
            .is_some_and(|r| r.load(Ordering::Acquire) != NONE)
    }

    /// Runs input `hw`'s handlers, from the driver's filter on the parent
    /// interrupt. False if it has none: the driver should mask it, as
    /// nothing will service it.
    pub fn dispatch(&self, hw: u32, cx: &IrqCx) -> bool {
        match self.shared.roots.get(hw as usize) {
            Some(r) => match r.load(Ordering::Acquire) {
                NONE => false,
                root => crate::dispatch(root, cx),
            },
            None => false,
        }
    }
}

impl Drop for CascadeDomain {
    fn drop(&mut self) {
        self.shared.revoked.store(true, Ordering::Release);
        let mut st = self.slot.state.lock();
        if st.connected == 0 {
            st.shared = None;
        }
    }
}
