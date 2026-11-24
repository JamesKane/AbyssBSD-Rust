// SPDX-License-Identifier: BSD-2-Clause
//! `vgapci`: the generic driver for a VGA-compatible display function on PCI
//! (class 03h subclass 00h, or the pre-class-code VGA, class 00h subclass
//! 01h).
//!
//! It changes nothing: the firmware's decoding and the boot framebuffer it
//! set up stay as they are, which is how the boot display (the UEFI GOP
//! framebuffer, `gop-kms`) keeps working. (The bus already lists the
//! function's BARs as held, as FreeBSD's `devinfo -rv` shows them under
//! `vgapciN`.) It gives the function's place in the tree, as `devinfo`
//! shows it on FreeBSD.
//!
//! A native display driver (M-G) attaches as its child, not in its place.
//! The child is the `drmn` node, as on FreeBSD: `drmn` is the name FreeBSD's
//! `drm-kmod` gives a graphics device's node (`drmn0`), the device a native GPU
//! driver attaches as, under the display function's `vgapci` node. "DRM" is
//! the Direct Rendering Manager, the kernel's graphics interface.
//!
//! FreeBSD lets any display driver claim a `vgapci` child by its PCI IDs;
//! here `vgapci` adds the `drmn` child itself for the vendors that have a
//! native driver (NVIDIA's and AMD's), maps the register aperture for it
//! (BAR0 on NVIDIA, BAR5 on AMD), and hands the child a weak reference so
//! that this driver's detach is never held up.
//!
//! Behavior reference: sys/dev/pci/vga_pci.c (`vga_pci_probe`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, Resource, StartCx,
    StopCx, StopReason,
};
use devtree::ids::PciId;
use irq::Filter;
use pci::BarKind;
use pci::bus::{Messages, PciChild};
use regs::Bytes;
use sched::IrqCx;
use sync::{Once, SpinLock};
use vm::dma::{DmaConstraints, DmaTag};

/// PCI class 03h, subclass 00h: a VGA-compatible controller.
const DISPLAY_VGA: PciId = PciId::subclass(0x03, 0x00);
/// PCI class 00h, subclass 01h: a VGA-compatible device from before class
/// codes were defined.
const OLD_VGA: PciId = PciId::subclass(0x00, 0x01);

/// NVIDIA's PCI vendor ID.
const VENDOR_NVIDIA: u16 = 0x10de;
/// The register aperture's size on the NVIDIA GPUs (BAR0: 16 MiB).
const NVIDIA_BAR0_LEN: u64 = 0x100_0000;
/// AMD's PCI vendor ID.
const VENDOR_AMD: u16 = 0x1002;
/// The register window (BAR5) of the AMD GPUs is 512 KiB (Raphael) or
/// 1 MiB (Navi 22); anything smaller is not one.
const AMD_BAR5_MIN: u64 = 0x8_0000;

/// What `vgapci` holds of a GPU for its native driver: the register
/// aperture, the PCI function (bus mastering is the driver's to turn on),
/// and a DMA tag for what the GPU reads and writes in host memory.
pub struct Aperture {
    mmio: MmioGrant<Bytes>,
    len: usize,
    /// An AMD GPU's VRAM aperture (BAR0), when it could be mapped.
    vram: Option<(MmioGrant<Bytes>, usize)>,
    /// Its doorbell aperture (BAR2), when it could be mapped.
    doorbells: Option<(MmioGrant<Bytes>, usize)>,
    pci: Arc<PciChild>,
    tag: Option<DmaTag>,
    /// What the GPU driver does with the function's interrupt, set once.
    sink: Once<Arc<dyn InterruptSink>>,
    /// Whether a message-signalled vector was set up for it.
    has_irq: AtomicBool,
}

/// Whether an AMD GPU gets a message-signalled interrupt (`hw.vgapci.msi`):
/// off until the board has shown it works.
static MSI: AtomicBool = AtomicBool::new(false);

/// Takes the boot-time tunables (`TUNABLE_INT_FETCH`): `get` gives a name's
/// integer value, if set. The kernel calls it before drivers attach.
pub fn set_tunables(get: &dyn Fn(&str) -> Option<i32>) {
    if let Some(v) = get("hw.vgapci.msi") {
        // Relaxed: a tunable, read at attach.
        MSI.store(v != 0, Ordering::Relaxed);
    }
}

/// What a GPU driver does with its function's interrupt: `vgapci` owns the
/// message-signalled vector (the function's), and calls this from the
/// interrupt filter, in interrupt context.
pub trait InterruptSink: Send + Sync {
    /// Handles the interrupt in the filter: no sleeping, no sleepable lock.
    fn interrupt(&self, cx: &IrqCx) -> Filter;
}

impl core::fmt::Debug for Aperture {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Aperture").field("len", &self.len).finish()
    }
}

impl Aperture {
    /// Writes the 32-bit register at byte `offset`; `false` (and no write)
    /// if it is not aligned or lies outside the aperture.
    pub fn write32(&self, offset: usize, value: u32) -> bool {
        if !offset.is_multiple_of(4) || offset.checked_add(4).is_none_or(|e| e > self.len) {
            return false;
        }
        self.mmio.write32(offset, value);
        true
    }

    /// The 32-bit word at byte `offset` of the VRAM aperture (BAR0 on an
    /// AMD GPU), or `None` if there is no aperture or the offset is outside
    /// or not aligned.
    #[must_use]
    pub fn vram_read32(&self, offset: usize) -> Option<u32> {
        let (map, len) = self.vram.as_ref()?;
        if !offset.is_multiple_of(4) || offset.checked_add(4)? > *len {
            return None;
        }
        Some(map.read32(offset))
    }

    /// Writes the 32-bit word at byte `offset` of the VRAM aperture;
    /// `false` (and no write) if there is no aperture, or the offset is
    /// unaligned or outside it.
    pub fn vram_write32(&self, offset: usize, value: u32) -> bool {
        let Some((map, len)) = self.vram.as_ref() else {
            return false;
        };
        if !offset.is_multiple_of(4) || offset.checked_add(4).is_none_or(|e| e > *len) {
            return false;
        }
        map.write32(offset, value);
        true
    }

    /// The physical address of the start of the VRAM aperture, if there is
    /// one.
    #[must_use]
    pub fn vram_phys(&self) -> Option<u64> {
        self.vram.as_ref().map(|(m, _)| m.phys())
    }

    /// Registers the driver's interrupt handler. Only the first registration
    /// takes; `false` for a later one. The handler runs for the life of
    /// the function: a driver that goes first makes its sink do nothing.
    pub fn set_sink(&self, sink: Arc<dyn InterruptSink>) -> bool {
        self.sink.set(sink).is_ok()
    }

    /// Whether the function has a message-signalled interrupt for a sink.
    #[must_use]
    pub fn has_interrupt(&self) -> bool {
        // Acquire: set once at attach, before a driver is added.
        self.has_irq.load(Ordering::Acquire)
    }

    /// Writes the 32-bit doorbell at byte `offset` of the doorbell aperture
    /// (BAR2); `false` (and no write) if there is none or the offset is
    /// unaligned or outside it.
    pub fn doorbell_write32(&self, offset: usize, value: u32) -> bool {
        let Some((map, len)) = self.doorbells.as_ref() else {
            return false;
        };
        if !offset.is_multiple_of(4) || offset.checked_add(4).is_none_or(|e| e > *len) {
            return false;
        }
        map.write32(offset, value);
        true
    }

    /// The size of the doorbell aperture, if there is one.
    #[must_use]
    pub fn doorbell_len(&self) -> Option<usize> {
        self.doorbells.as_ref().map(|(_, l)| *l)
    }

    /// The size of the VRAM aperture, if there is one.
    #[must_use]
    pub fn vram_len(&self) -> Option<usize> {
        self.vram.as_ref().map(|(_, l)| *l)
    }

    /// The PCI function.
    #[must_use]
    pub fn pci(&self) -> &PciChild {
        &self.pci
    }

    /// The DMA tag (47-bit addresses, 256-byte alignment), for the vendors
    /// whose driver loads firmware by DMA; `None` for the others.
    #[must_use]
    pub fn dma_tag(&self) -> Option<&DmaTag> {
        self.tag.as_ref()
    }

    /// The 32-bit register at byte `offset`, or `None` if it is not
    /// aligned or lies outside the aperture.
    #[must_use]
    pub fn read32(&self, offset: usize) -> Option<u32> {
        if !offset.is_multiple_of(4) || offset.checked_add(4)? > self.len {
            return None;
        }
        Some(self.mmio.read32(offset))
    }
}

/// What the `drmn` child (the graphics device node of FreeBSD's `drm-kmod`) is given: its parent's aperture, weakly, and the
/// PCI vendor, which says which kind of GPU it is.
#[derive(Debug)]
pub struct Registers(pub Weak<Aperture>, pub u16);

/// An AMD GPU's register window (BAR5), found by its BAR number among the
/// memory resources, or `None` (and a log line saying why).
fn amd_aperture(cx: &mut AttachCx<'_>, child: &Arc<PciChild>) -> Option<Arc<Aperture>> {
    let log = |msg: &str| bus::manager().log(format_args!("vgapci: AMD GPU: {msg}"));
    let Some(index) = child
        .function
        .bars
        .iter()
        .filter(|b| b.kind != BarKind::Io)
        .position(|b| b.index == 5)
    else {
        log("no register window (BAR5)");
        return None;
    };
    // `mmio_bytes` counts the memory resources only; the list also holds
    // the I/O BAR and the interrupt.
    let len = cx
        .resources()
        .iter()
        .filter_map(|r| match r {
            Resource::Memory { len, .. } => Some(*len),
            _ => None,
        })
        .nth(index)
        .unwrap_or(0);
    if len < AMD_BAR5_MIN {
        log(&alloc::format!(
            "the register window is too small: memory resource {index} is {len:#x} bytes"
        ));
        return None;
    }
    // The VRAM aperture is BAR0, the first memory resource; a failure to
    // map it leaves the register window usable.
    let vram = child
        .function
        .bars
        .iter()
        .filter(|b| b.kind != BarKind::Io)
        .position(|b| b.index == 0)
        .and_then(|i| {
            let size = child.function.bars.iter().find(|b| b.index == 0)?.size;
            match cx.mmio_bytes_console(i) {
                Ok(m) => Some((m, usize::try_from(size).ok()?)),
                Err(e) => {
                    log(&alloc::format!(
                        "the VRAM aperture (BAR0) was refused: {e:?}"
                    ));
                    None
                }
            }
        });
    // The doorbell aperture is BAR2, a plain mapping (writes only).
    let doorbells = child
        .function
        .bars
        .iter()
        .filter(|b| b.kind != BarKind::Io)
        .position(|b| b.index == 2)
        .and_then(|i| {
            let size = child.function.bars.iter().find(|b| b.index == 2)?.size;
            match cx.mmio_bytes(i) {
                Ok(m) => Some((m, usize::try_from(size).ok()?)),
                Err(e) => {
                    log(&alloc::format!(
                        "the doorbell aperture (BAR2) was refused: {e:?}"
                    ));
                    None
                }
            }
        });
    // What the GPU reaches in system memory: 44 bits on an APU (the
    // reference's DMA mask), pages aligned. A tag the broker will not make
    // leaves the driver without system memory for the GPU, as before.
    let tag = cx
        .dma_tag(DmaConstraints {
            addr_bits: 44,
            align: 4096,
            ..DmaConstraints::UNLIMITED
        })
        .ok();
    match cx.mmio_bytes(index) {
        Ok(mmio) => Some(Arc::new(Aperture {
            mmio,
            len: usize::try_from(len).unwrap_or(0),
            vram,
            doorbells,
            pci: Arc::clone(child),
            tag,
            sink: Once::new(),
            has_irq: AtomicBool::new(false),
        })),
        Err(e) => {
            log(&alloc::format!("the register window was refused: {e:?}"));
            None
        }
    }
}

/// `vgapciN`.
#[derive(Debug)]
pub struct VgaPci {
    /// The register aperture, for a function a native driver is wanted for.
    aperture: Option<Arc<Aperture>>,
    vendor: u16,
    /// The message-signalled vector's line, until `start` installs the
    /// handler.
    line: SpinLock<Option<IrqLine>>,
    /// The vector's owner: dropping it turns the function's MSI off.
    _messages: Option<Messages>,
}

/// The interrupt filter: whatever the GPU driver registered handles it; with
/// none, the interrupt is nobody's.
fn interrupt_filter(d: &VgaPci, _: usize, cx: &IrqCx) -> Filter {
    match d.aperture.as_ref().and_then(|a| a.sink.get()) {
        Some(sink) => sink.interrupt(cx),
        None => Filter::Stray,
    }
}

impl Driver for VgaPci {
    const NAME: &'static str = "vgapci";
    const IDS: Ids = Ids::Pci(&[DISPLAY_VGA, OLD_VGA]);

    fn describe(_: &Ident) -> String {
        "VGA-compatible display".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let mut aperture = None;
        let vendor = child.function.header.ident.vendor;
        if vendor == VENDOR_NVIDIA
            && matches!(
                cx.resources().first(),
                Some(Resource::Memory {
                    len: NVIDIA_BAR0_LEN,
                    ..
                })
            )
        {
            // Memory decoding is on already (firmware drives this display);
            // nothing is written to the function.
            let tag = cx.dma_tag(DmaConstraints {
                addr_bits: 47,
                align: 256,
                ..DmaConstraints::UNLIMITED
            });
            if let (Ok(mmio), Ok(tag)) = (cx.mmio_bytes(0), tag) {
                aperture = Some(Arc::new(Aperture {
                    mmio,
                    len: usize::try_from(NVIDIA_BAR0_LEN).unwrap_or(0),
                    vram: None,
                    doorbells: None,
                    pci: Arc::clone(&child),
                    tag: Some(tag),
                    sink: Once::new(),
                    has_irq: AtomicBool::new(false),
                }));
            }
        } else if vendor == VENDOR_AMD {
            aperture = amd_aperture(cx, &child);
        }
        // One message-signalled interrupt for an AMD GPU the driver is
        // wanted for (05 section 9: the reference asks for exactly one). The
        // function must be able to write the message.
        let mut line = None;
        let mut messages = None;
        if let (Some(a), true) = (
            &aperture,
            vendor == VENDOR_AMD && MSI.load(Ordering::Relaxed),
        ) {
            match pci::bus::msi_line(cx) {
                Ok((l, m)) => {
                    child.set_bus_master(true);
                    a.has_irq.store(true, Ordering::Release);
                    line = Some(l);
                    messages = Some(m);
                }
                Err(e) => bus::manager().log(format_args!(
                    "vgapci: AMD GPU: no message-signalled interrupt ({e:?})"
                )),
            }
        }
        Ok(Self {
            aperture,
            vendor,
            line: SpinLock::new(line),
            _messages: messages,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        // Lock released before the handler is installed.
        let line = self.line.lock().take();
        if let Some(line) = line
            && let Err(e) = cx.interrupt(line, Some(interrupt_filter), None, 0)
        {
            bus::manager().log(format_args!("vgapci: interrupt not installed ({e:?})"));
        }
        if let Some(a) = &self.aperture {
            let ident = Ident::Channel {
                class: "drmn",
                channel: 0,
            };
            let link = Registers(Arc::downgrade(a), self.vendor);
            cx.add_child(ident, Some(Arc::new(link) as _), Vec::<Resource>::new());
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<VgaPci>();

#[cfg(test)]
mod tests {
    use super::*;
    use devtree::ids::PciIdent;
    use devtree::matching::Matches;

    fn ident(class: u32) -> PciIdent {
        PciIdent {
            vendor: 0x10de,
            device: 0x2786,
            subvendor: 0,
            subdevice: 0,
            class,
            revision: 0xa1,
        }
    }

    #[test]
    fn claims_vga_functions_only() {
        // The RTX 4070 and the Radeon 610M: 0300h.
        assert!(DISPLAY_VGA.rank_for(&ident(0x03_00_00)).is_some());
        assert!(OLD_VGA.rank_for(&ident(0x00_01_00)).is_some());
        // A 3D controller (0302h) and an HDA function (0403h) are not VGA.
        assert!(DISPLAY_VGA.rank_for(&ident(0x03_02_00)).is_none());
        assert!(DISPLAY_VGA.rank_for(&ident(0x04_03_00)).is_none());
    }
}
