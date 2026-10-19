// SPDX-License-Identifier: BSD-2-Clause
//! The two PCI register interfaces behind one [`VirtioDevice`].
//!
//! Modern (virtio 1.2 §4.1.4): structures in memory BARs located by
//! vendor capabilities: common configuration, notifications, the ISR byte,
//! and the device configuration. Legacy (§4.1.4.8): one I/O BAR with a
//! fixed layout, whose device configuration moves when MSI-X is on.

use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{IoGrant, MmioGrant};
use regs::Bytes;
use vm::dma::DmaTag;

use crate::ring::{Ring, Virtqueue};
use crate::{feature, status};

/// "No MSI-X vector" in the vector registers.
pub(crate) const NO_VECTOR: u16 = 0xffff;

/// Modern common configuration (virtio 1.2 §4.1.4.3).
mod common {
    pub(crate) const DEVICE_FEATURE_SELECT: usize = 0x00;
    pub(crate) const DEVICE_FEATURE: usize = 0x04;
    pub(crate) const DRIVER_FEATURE_SELECT: usize = 0x08;
    pub(crate) const DRIVER_FEATURE: usize = 0x0c;
    pub(crate) const CONFIG_MSIX_VECTOR: usize = 0x10;
    pub(crate) const NUM_QUEUES: usize = 0x12;
    pub(crate) const DEVICE_STATUS: usize = 0x14;
    pub(crate) const CONFIG_GENERATION: usize = 0x15;
    pub(crate) const QUEUE_SELECT: usize = 0x16;
    pub(crate) const QUEUE_SIZE: usize = 0x18;
    pub(crate) const QUEUE_MSIX_VECTOR: usize = 0x1a;
    pub(crate) const QUEUE_ENABLE: usize = 0x1c;
    pub(crate) const QUEUE_NOTIFY_OFF: usize = 0x1e;
    pub(crate) const QUEUE_DESC: usize = 0x20;
    pub(crate) const QUEUE_DRIVER: usize = 0x28;
    pub(crate) const QUEUE_DEVICE: usize = 0x30;
    /// The structure's size.
    pub(crate) const SIZE: usize = 0x38;
}

/// Legacy I/O registers (virtio 1.2 §4.1.4.8).
mod legacy {
    pub(crate) const DEVICE_FEATURES: u16 = 0x00;
    pub(crate) const GUEST_FEATURES: u16 = 0x04;
    pub(crate) const QUEUE_PFN: u16 = 0x08;
    pub(crate) const QUEUE_SIZE: u16 = 0x0c;
    pub(crate) const QUEUE_SELECT: u16 = 0x0e;
    pub(crate) const QUEUE_NOTIFY: u16 = 0x10;
    pub(crate) const STATUS: u16 = 0x12;
    pub(crate) const ISR: u16 = 0x13;
    pub(crate) const CONFIG_VECTOR: u16 = 0x14;
    pub(crate) const QUEUE_VECTOR: u16 = 0x16;
    /// Device configuration without MSI-X, and with it.
    pub(crate) const CONFIG: u16 = 0x14;
    pub(crate) const CONFIG_MSIX: u16 = 0x18;
}

/// A structure inside one of the mapped BARs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Window {
    /// Index into the transport's grants.
    pub(crate) grant: usize,
    pub(crate) offset: usize,
    pub(crate) len: usize,
}

/// The register interface.
#[derive(Debug)]
pub(crate) enum Regs {
    Modern {
        grants: Vec<MmioGrant<Bytes>>,
        common: Window,
        notify: Window,
        notify_multiplier: u32,
        isr: Window,
        device: Option<Window>,
    },
    Legacy {
        io: IoGrant,
        msix: bool,
    },
}

/// How the device's interrupts reach the child's handlers. The child's
/// interrupt resources are numbered as the MSI-X vectors are; with INTx
/// there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Vectors {
    /// MSI-X with this many vectors: 0 for configuration changes, then
    /// one per queue, or one shared by all queues, or (with a single
    /// vector) everything on 0.
    MsiX { count: u16, queues: u16 },
    /// One shared line; the ISR byte says why.
    Intx,
    /// No interrupt at all.
    None,
}

/// Why feature negotiation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NegotiateError {
    /// The device refused the features (it cleared `FEATURES_OK`).
    Refused,
    /// A modern device that does not offer `VERSION_1`.
    NotModern,
}

/// A virtio device, through its transport: what a virtio child's driver
/// gets as bus data.
#[derive(Debug)]
pub struct VirtioDevice {
    pub(crate) regs: Regs,
    pub(crate) device_type: u32,
    pub(crate) vectors: Vectors,
    pub(crate) queues: u16,
    /// `RING_EVENT_IDX` was negotiated: rings use the event fields.
    pub(crate) event_idx: core::sync::atomic::AtomicBool,
}

impl VirtioDevice {
    /// The grant holding window `w`, if `width` bytes at `off` lie inside
    /// it. Fixed registers were checked at attach; offsets the device
    /// reports, or a driver asks for, are checked here.
    fn mmio(&self, w: Window, off: usize, width: usize) -> Option<&MmioGrant<Bytes>> {
        let Regs::Modern { grants, .. } = &self.regs else {
            return None;
        };
        (off.checked_add(width)? <= w.len).then(|| &grants[w.grant])
    }

    fn common(&self, off: usize, width: usize) -> (&MmioGrant<Bytes>, usize) {
        let Regs::Modern { common, .. } = self.regs else {
            unreachable!("common configuration of a legacy device")
        };
        let g = self
            .mmio(common, off, width)
            .expect("common configuration checked at attach");
        (g, common.offset + off)
    }

    fn common_read8(&self, off: usize) -> u8 {
        let (g, at) = self.common(off, 1);
        g.read8(at)
    }

    fn common_write8(&self, off: usize, v: u8) {
        let (g, at) = self.common(off, 1);
        g.write8(at, v);
    }

    fn common_read16(&self, off: usize) -> u16 {
        let (g, at) = self.common(off, 2);
        g.read16(at)
    }

    fn common_write16(&self, off: usize, v: u16) {
        let (g, at) = self.common(off, 2);
        g.write16(at, v);
    }

    fn common_read32(&self, off: usize) -> u32 {
        let (g, at) = self.common(off, 4);
        g.read32(at)
    }

    fn common_write32(&self, off: usize, v: u32) {
        let (g, at) = self.common(off, 4);
        g.write32(at, v);
    }

    fn common_write64(&self, off: usize, v: u64) {
        self.common_write32(off, v as u32);
        self.common_write32(off + 4, (v >> 32) as u32);
    }

    /// The virtio device type (`device_type`).
    #[must_use]
    pub fn device_type(&self) -> u32 {
        self.device_type
    }

    /// Whether the modern interface is in use.
    #[must_use]
    pub fn is_modern(&self) -> bool {
        matches!(self.regs, Regs::Modern { .. })
    }

    /// The device status.
    #[must_use]
    pub fn status(&self) -> u8 {
        match &self.regs {
            Regs::Modern { .. } => self.common_read8(common::DEVICE_STATUS),
            Regs::Legacy { io, .. } => io.read8(legacy::STATUS),
        }
    }

    fn set_status(&self, s: u8) {
        match &self.regs {
            Regs::Modern { .. } => self.common_write8(common::DEVICE_STATUS, s),
            Regs::Legacy { io, .. } => io.write8(legacy::STATUS, s),
        }
    }

    /// Resets the device: it stops using every queue and forgets the
    /// features and vectors set.
    pub fn reset(&self) {
        self.set_status(0);
        // A modern device may take a while; reading 0 says it is done
        // (§4.1.4.3.2). A legacy device resets at once.
        if self.is_modern() {
            for _ in 0..1_000_000 {
                if self.status() == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
    }

    /// Marks the device failed (the driver gave up on it).
    pub fn fail(&self) {
        self.set_status(self.status() | status::FAILED);
    }

    fn device_features(&self) -> u64 {
        match &self.regs {
            Regs::Modern { .. } => {
                self.common_write32(common::DEVICE_FEATURE_SELECT, 0);
                let lo = self.common_read32(common::DEVICE_FEATURE);
                self.common_write32(common::DEVICE_FEATURE_SELECT, 1);
                let hi = self.common_read32(common::DEVICE_FEATURE);
                u64::from(hi) << 32 | u64::from(lo)
            }
            Regs::Legacy { io, .. } => u64::from(io.read32(legacy::DEVICE_FEATURES)),
        }
    }

    fn set_driver_features(&self, f: u64) {
        match &self.regs {
            Regs::Modern { .. } => {
                self.common_write32(common::DRIVER_FEATURE_SELECT, 0);
                self.common_write32(common::DRIVER_FEATURE, f as u32);
                self.common_write32(common::DRIVER_FEATURE_SELECT, 1);
                self.common_write32(common::DRIVER_FEATURE, (f >> 32) as u32);
            }
            Regs::Legacy { io, .. } => io.write32(legacy::GUEST_FEATURES, f as u32),
        }
    }

    /// Resets the device, acknowledges it, and negotiates features: the
    /// device-specific ones in `wanted` that it offers, plus the transport
    /// features this layer needs. Returns the features in effect.
    pub fn negotiate(&self, wanted: u64) -> Result<u64, NegotiateError> {
        self.reset();
        self.set_status(status::ACKNOWLEDGE);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER);
        if let Vectors::MsiX { .. } = self.vectors {
            self.set_config_vector(0);
        }
        let offered = self.device_features();
        // Device-specific bits are 0-23 and 50-127; the transport keeps
        // the rest for itself.
        let device_bits = ((1u64 << 24) - 1) | !((1u64 << 50) - 1);
        // Of the transport's bits, the driver may ask for event indices.
        let mut want = wanted & (device_bits | feature::RING_EVENT_IDX) & offered;
        let modern = self.is_modern();
        if modern {
            if offered & feature::VERSION_1 == 0 {
                self.fail();
                return Err(NegotiateError::NotModern);
            }
            // A device that needs platform translation refuses a driver
            // that does not accept it; bus addresses are what we give it.
            want |= feature::VERSION_1 | offered & feature::ACCESS_PLATFORM;
        }
        self.set_driver_features(want);
        self.event_idx.store(
            want & feature::RING_EVENT_IDX != 0,
            core::sync::atomic::Ordering::Relaxed,
        );
        if modern {
            self.set_status(status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK);
            if self.status() & status::FEATURES_OK == 0 {
                self.fail();
                return Err(NegotiateError::Refused);
            }
        }
        Ok(want)
    }

    /// Tells the device the driver is ready: queues set up, handlers
    /// installed.
    pub fn ready(&self) {
        self.set_status(self.status() | status::DRIVER_OK);
    }

    fn set_config_vector(&self, v: u16) {
        match &self.regs {
            Regs::Modern { .. } => self.common_write16(common::CONFIG_MSIX_VECTOR, v),
            Regs::Legacy { io, .. } => io.write16(legacy::CONFIG_VECTOR, v),
        }
    }

    /// Queues the device has (legacy devices: up to the first that is
    /// absent, at most 64).
    #[must_use]
    pub fn queues(&self) -> u16 {
        self.queues
    }

    /// The largest size the device allows for queue `q` (0: no such
    /// queue).
    #[must_use]
    pub fn queue_max(&self, q: u16) -> u16 {
        match &self.regs {
            Regs::Modern { .. } => {
                if q >= self.queues {
                    return 0;
                }
                self.common_write16(common::QUEUE_SELECT, q);
                self.common_read16(common::QUEUE_SIZE)
            }
            Regs::Legacy { io, .. } => {
                io.write16(legacy::QUEUE_SELECT, q);
                io.read16(legacy::QUEUE_SIZE)
            }
        }
    }

    /// The MSI-X vector for queue `q`, or [`NO_VECTOR`].
    fn queue_vector(&self, q: u16) -> u16 {
        match self.vectors {
            Vectors::MsiX { count, queues } if count > queues => q + 1,
            Vectors::MsiX { count, .. } if count >= 2 => 1,
            Vectors::MsiX { .. } => 0,
            Vectors::Intx | Vectors::None => NO_VECTOR,
        }
    }

    /// Which of the child's interrupt resources fires for queue `q`, if
    /// any.
    #[must_use]
    pub fn queue_irq(&self, q: u16) -> Option<usize> {
        match self.vectors {
            Vectors::MsiX { .. } => Some(usize::from(self.queue_vector(q))),
            Vectors::Intx => Some(0),
            Vectors::None => None,
        }
    }

    /// Which of the child's interrupt resources fires for configuration
    /// changes, if any.
    #[must_use]
    pub fn config_irq(&self) -> Option<usize> {
        match self.vectors {
            Vectors::MsiX { .. } | Vectors::Intx => Some(0),
            Vectors::None => None,
        }
    }

    /// Whether the interrupt is a shared line: handlers read
    /// [`VirtioDevice::isr`] to learn whether (and why) it was this device.
    #[must_use]
    pub fn shared_line(&self) -> bool {
        self.vectors == Vectors::Intx
    }

    /// Reads and clears the ISR byte: bit 0 a queue, bit 1 a configuration
    /// change. Only meaningful without MSI-X.
    #[must_use]
    pub fn isr(&self) -> u8 {
        match &self.regs {
            Regs::Modern { isr, .. } => self.mmio(*isr, 0, 1).map_or(0, |g| g.read8(isr.offset)),
            Regs::Legacy { io, .. } => io.read8(legacy::ISR),
        }
    }

    /// Sets up queue `q` with a new ring of at most `max` entries (legacy
    /// devices: exactly the device's size) and its interrupt vector.
    pub fn queue(
        self: &Arc<Self>,
        tag: &DmaTag,
        q: u16,
        max: u16,
    ) -> Result<Virtqueue, QueueError> {
        let dev_max = self.queue_max(q);
        if dev_max == 0 {
            return Err(QueueError::NoQueue);
        }
        let size = match &self.regs {
            Regs::Modern { .. } => {
                let mut size = dev_max.min(max.max(1));
                if !size.is_power_of_two() {
                    size = size.next_power_of_two() / 2;
                }
                size
            }
            Regs::Legacy { .. } => dev_max,
        };
        let mut ring = Ring::new(tag, size).map_err(|_| QueueError::NoMemory)?;
        ring.set_event_idx(self.event_idx.load(core::sync::atomic::Ordering::Relaxed));
        let vector = self.queue_vector(q);
        match &self.regs {
            Regs::Modern { .. } => {
                // Selected by `queue_max`.
                self.common_write16(common::QUEUE_SIZE, size);
                if vector != NO_VECTOR {
                    self.common_write16(common::QUEUE_MSIX_VECTOR, vector);
                    if self.common_read16(common::QUEUE_MSIX_VECTOR) != vector {
                        return Err(QueueError::Vector);
                    }
                }
                self.common_write64(common::QUEUE_DESC, ring.desc_addr());
                self.common_write64(common::QUEUE_DRIVER, ring.avail_addr());
                self.common_write64(common::QUEUE_DEVICE, ring.used_addr());
                self.common_write16(common::QUEUE_ENABLE, 1);
            }
            Regs::Legacy { io, msix } => {
                if *msix && vector != NO_VECTOR {
                    io.write16(legacy::QUEUE_VECTOR, vector);
                    if io.read16(legacy::QUEUE_VECTOR) != vector {
                        return Err(QueueError::Vector);
                    }
                }
                let pfn =
                    u32::try_from(ring.desc_addr() >> 12).map_err(|_| QueueError::NoMemory)?;
                io.write32(legacy::QUEUE_PFN, pfn);
            }
        }
        Ok(Virtqueue::new(ring, Arc::clone(self), q))
    }

    /// Tells the device queue `q` has new buffers.
    pub fn notify(&self, q: u16) {
        match &self.regs {
            Regs::Modern {
                notify,
                notify_multiplier,
                ..
            } => {
                self.common_write16(common::QUEUE_SELECT, q);
                let off = usize::from(self.common_read16(common::QUEUE_NOTIFY_OFF))
                    * *notify_multiplier as usize;
                // An offset outside the structure is the device's bug; it
                // gets no notification.
                if let Some(g) = self.mmio(*notify, off, 2) {
                    g.write16(notify.offset + off, q);
                }
            }
            Regs::Legacy { io, .. } => io.write16(legacy::QUEUE_NOTIFY, q),
        }
    }

    /// Where field `off..off + width` of the device configuration is:
    /// a modern window and offset, or a legacy port offset. `None` if it
    /// is outside what the device provides.
    fn config_at(&self, off: u16, width: u16) -> Option<ConfigAt<'_>> {
        match &self.regs {
            Regs::Modern { device, .. } => {
                let d = (*device)?;
                let g = self.mmio(d, off.into(), width.into())?;
                Some(ConfigAt::Mmio(g, d.offset + usize::from(off)))
            }
            Regs::Legacy { io, msix } => {
                let base = if *msix {
                    legacy::CONFIG_MSIX
                } else {
                    legacy::CONFIG
                };
                let at = base.checked_add(off)?;
                (u32::from(at) + u32::from(width) <= u32::from(io.len()))
                    .then_some(ConfigAt::Io(io, at))
            }
        }
    }

    /// Reads the byte at `off` in the device configuration (all ones
    /// outside it).
    #[must_use]
    pub fn config8(&self, off: u16) -> u8 {
        match self.config_at(off, 1) {
            Some(ConfigAt::Mmio(g, at)) => g.read8(at),
            Some(ConfigAt::Io(io, at)) => io.read8(at),
            None => u8::MAX,
        }
    }

    /// Reads the 16-bit field at `off` in the device configuration.
    #[must_use]
    pub fn config16(&self, off: u16) -> u16 {
        match self.config_at(off, 2) {
            Some(ConfigAt::Mmio(g, at)) => g.read16(at),
            Some(ConfigAt::Io(io, at)) => io.read16(at),
            None => u16::MAX,
        }
    }

    /// Reads the 32-bit field at `off` in the device configuration.
    #[must_use]
    pub fn config32(&self, off: u16) -> u32 {
        match self.config_at(off, 4) {
            Some(ConfigAt::Mmio(g, at)) => g.read32(at),
            Some(ConfigAt::Io(io, at)) => io.read32(at),
            None => u32::MAX,
        }
    }

    /// Reads the 64-bit field at `off` in the device configuration, as one
    /// consistent value.
    #[must_use]
    pub fn config64(&self, off: u16) -> u64 {
        loop {
            let before = self.generation();
            let v = u64::from(self.config32(off + 4)) << 32 | u64::from(self.config32(off));
            if self.generation() == before {
                return v;
            }
        }
    }

    /// Writes the 32-bit field at `off` in the device configuration
    /// (ignored outside it).
    pub fn set_config32(&self, off: u16, v: u32) {
        match self.config_at(off, 4) {
            Some(ConfigAt::Mmio(g, at)) => g.write32(at, v),
            Some(ConfigAt::Io(io, at)) => io.write32(at, v),
            None => {}
        }
    }

    /// The configuration generation (legacy devices have none: 0).
    fn generation(&self) -> u8 {
        match &self.regs {
            Regs::Modern { .. } => self.common_read8(common::CONFIG_GENERATION),
            Regs::Legacy { .. } => 0,
        }
    }

    /// Reads the number of queues of a modern device.
    pub(crate) fn modern_queues(&self) -> u16 {
        self.common_read16(common::NUM_QUEUES)
    }
}

/// A device configuration field's location.
enum ConfigAt<'a> {
    Mmio(&'a MmioGrant<Bytes>, usize),
    Io(&'a IoGrant, u16),
}

/// Why a queue could not be set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// The device has no such queue.
    NoQueue,
    /// No DMA memory for the ring.
    NoMemory,
    /// The device refused the MSI-X vector.
    Vector,
}

/// The common configuration's size, for validating its capability.
pub(crate) const COMMON_SIZE: usize = common::SIZE;
