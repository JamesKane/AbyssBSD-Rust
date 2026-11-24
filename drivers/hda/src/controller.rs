// SPDX-License-Identifier: BSD-2-Clause
//! `hdac`: a High Definition Audio controller. It resets the link, runs
//! the command and response rings, finds the codecs that came out of reset,
//! and sends their verbs one at a time, polled as FreeBSD's
//! `hdac_send_command` does.
//!
//! Streams, the DMA position buffer, and interrupts (unsolicited responses,
//! the response-count interrupt) wait for the sound engine; with
//! `INTCTL` left at zero the controller raises none.
//!
//! Behavior reference: sys/dev/sound/pci/hda/hdac.c (15.1).

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::ids::PciId;
use hda::reg::{self, Caps};
use hda::verb;
use pci::bus::PciChild;
use regs::Bytes;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use vm::dma::{DmaCoherent, DmaConstraints};

use crate::names;

static RING_LOCK: LockClass = LockClass::new("hdac ring", 500);

/// A command that gets no response within this many 10 us polls timed out
/// (`hdac_send_command`: 100 ms).
const POLLS: u32 = 10_000;

/// The command and response rings, and where each stands.
struct Rings {
    corb: DmaCoherent<u32>,
    rirb: DmaCoherent<u32>,
    corb_size: u16,
    rirb_size: u16,
    corb_wp: u16,
    rirb_rp: u16,
    /// Unsolicited responses that arrived while a command waited. They
    /// are counted and dropped until the sound engine can take them.
    unsolicited: u32,
}

/// One controller: its registers and rings. The codecs' drivers reach it
/// through [`Link`]s that hold it weakly, so detaching the controller
/// never waits on them.
pub(crate) struct Controller {
    mmio: bus::MmioGrant<Bytes>,
    caps: Caps,
    rings: Mutex<Rings>,
}

impl core::fmt::Debug for Controller {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Controller")
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}

/// The low 32 bits of an address (a ring's base register takes it in halves).
fn low(a: u64) -> u32 {
    u32::try_from(a & 0xffff_ffff).unwrap_or_default()
}

fn delay_us(us: u64) {
    bus::manager().delay_us(us);
}

impl Controller {
    /// Resets the link (`hdac_reset`): stops every DMA engine, puts the
    /// controller in reset for at least 100 us, and, if `wakeup`, takes it
    /// out again and waits for the codecs' own reset (521 us at least).
    fn reset(&self, wakeup: bool) -> Result<(), &'static str> {
        let m = &*self.mmio;
        for i in 0..self.caps.streams() {
            m.write32(reg::SD_BASE + i * reg::SD_STEP, 0);
        }
        m.write8(reg::CORBCTL, 0);
        m.write8(reg::RIRBCTL, 0);
        m.write32(reg::DPLBASE, 0);
        m.write32(reg::DPUBASE, 0);
        m.write32(reg::GCTL, m.read32(reg::GCTL) & !reg::GCTL_CRST);
        if !self.wait_crst(false) {
            return Err("unable to put the controller in reset");
        }
        if !wakeup {
            return Ok(());
        }
        delay_us(100);
        m.write32(reg::GCTL, m.read32(reg::GCTL) | reg::GCTL_CRST);
        if !self.wait_crst(true) {
            return Err("the controller is stuck in reset");
        }
        delay_us(1000);
        Ok(())
    }

    fn wait_crst(&self, want: bool) -> bool {
        for _ in 0..POLLS {
            if (self.mmio.read32(reg::GCTL) & reg::GCTL_CRST != 0) == want {
                return true;
            }
            delay_us(10);
        }
        false
    }

    /// Sends `verb` to codec `cad` and waits for its response (`None` if
    /// none came in 100 ms).
    pub(crate) fn command(&self, s: &Sleepable, cad: u8, verb: u32) -> Option<u32> {
        if usize::from(cad) >= reg::CODEC_MAX {
            return None;
        }
        let mut r = self.rings.lock(s);
        let m = &*self.mmio;
        let verb = verb & 0x0fff_ffff | u32::from(cad) << 28;
        r.corb_wp = (r.corb_wp + 1) % r.corb_size;
        let wp = r.corb_wp;
        r.corb.write(usize::from(wp), verb.to_le());
        m.write16(reg::CORBWP, wp);
        for _ in 0..POLLS {
            // The write pointer is eight bits; a ring has at most 256
            // entries.
            let rirbwp = u16::from(m.read8(reg::RIRBWP));
            let mut got = None;
            while r.rirb_rp != rirbwp % r.rirb_size {
                r.rirb_rp = (r.rirb_rp + 1) % r.rirb_size;
                let at = usize::from(r.rirb_rp) * 2;
                let resp = u32::from_le(r.rirb.read(at));
                let ex = u32::from_le(r.rirb.read(at + 1));
                if ex & 0x10 != 0 {
                    r.unsolicited = r.unsolicited.wrapping_add(1);
                } else if ex & 0xf == u32::from(cad) {
                    got = Some(resp);
                }
            }
            if got.is_some() {
                return got;
            }
            delay_us(10);
        }
        None
    }
}

/// How a codec's driver reaches its controller, and who the codec is.
#[derive(Debug)]
pub(crate) struct Link {
    ctlr: Weak<Controller>,
    /// The codec's address.
    pub(crate) cad: u8,
    /// Vendor ID.
    pub(crate) vendor: u16,
    /// Device ID.
    pub(crate) device: u16,
}

impl Link {
    /// Sends `verb` to this codec.
    pub(crate) fn command(&self, s: &Sleepable, verb: u32) -> Option<u32> {
        self.ctlr.upgrade()?.command(s, self.cad, verb)
    }
}

/// `hdacN`.
pub(crate) struct Hdac {
    ctlr: Arc<Controller>,
    /// Codec addresses that came out of reset, from `STATESTS`.
    present: u16,
}

impl core::fmt::Debug for Hdac {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hdac")
            .field("present", &self.present)
            .finish_non_exhaustive()
    }
}

impl Driver for Hdac {
    const NAME: &'static str = "hdac";
    // Multimedia, HD audio.
    const IDS: Ids = Ids::Pci(&[PciId::subclass(0x04, 0x03)]);

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Pci(i, _, _) => {
                alloc::format!("{} HDA Controller", names::controller(i.vendor, i.device))
            }
            _ => "HDA Controller".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let mmio = cx.mmio_bytes(0)?;
        child.enable_memory();
        child.set_bus_master(true);
        let caps = Caps::of(mmio.read16(reg::GCAP));
        let corb_size = reg::ring_entries(mmio.read8(reg::CORBSIZE)).unwrap_or(256);
        let rirb_size = reg::ring_entries(mmio.read8(reg::RIRBSIZE)).unwrap_or(256);
        let tag = cx.dma_tag(DmaConstraints {
            addr_bits: if caps.addr64 { 64 } else { 32 },
            // The rings are 128-byte aligned (`HDA_DMA_ALIGNMENT`).
            align: 128,
            ..DmaConstraints::UNLIMITED
        })?;
        let corb = tag
            .coherent::<u32>(usize::from(corb_size))
            .map_err(|_| AttachError::NoMemory)?;
        let rirb = tag
            .coherent::<u32>(usize::from(rirb_size) * 2)
            .map_err(|_| AttachError::NoMemory)?;
        let ctlr = Controller {
            mmio,
            caps,
            rings: Mutex::new(
                &RING_LOCK,
                Rings {
                    corb,
                    rirb,
                    corb_size,
                    rirb_size,
                    corb_wp: 0,
                    rirb_rp: 0,
                    unsolicited: 0,
                },
            ),
        };
        // Quiesce, then bring the link up: FreeBSD resets twice, once to
        // read the capabilities and once with the rings allocated.
        ctlr.reset(true).map_err(AttachError::Device)?;
        ctlr.reset(true).map_err(AttachError::Device)?;
        let m = &*ctlr.mmio;
        let s = cx.sleepable();
        let r = ctlr.rings.lock(s);
        // The command ring.
        m.write8(reg::CORBSIZE, reg::ring_size_select(corb_size));
        let corb_pa = r.corb.bus_addr();
        m.write32(reg::CORBLBASE, low(corb_pa));
        m.write32(reg::CORBUBASE, low(corb_pa >> 32));
        m.write16(reg::CORBWP, 0);
        m.write16(reg::CORBRP, reg::CORBRP_RST);
        m.write16(reg::CORBRP, 0);
        // The response ring, with its interrupt off (`INTCTL` is zero).
        m.write8(reg::RIRBSIZE, reg::ring_size_select(rirb_size));
        let rirb_pa = r.rirb.bus_addr();
        m.write32(reg::RIRBLBASE, low(rirb_pa));
        m.write32(reg::RIRBUBASE, low(rirb_pa >> 32));
        m.write16(reg::RIRBWP, reg::RIRBWP_RST);
        m.write16(reg::RINTCNT, rirb_size / 2);
        m.write8(reg::RIRBCTL, 0);
        drop(r);
        // Start both engines, take no wake events, and read which codecs
        // came out of reset.
        m.write8(reg::CORBCTL, m.read8(reg::CORBCTL) | reg::CORBCTL_RUN);
        m.write8(reg::RIRBCTL, m.read8(reg::RIRBCTL) | reg::RIRBCTL_DMAEN);
        m.write16(reg::WAKEEN, 0);
        let present = m.read16(reg::STATESTS);
        m.write16(reg::STATESTS, present);
        m.write32(reg::GCTL, m.read32(reg::GCTL) | reg::GCTL_UNSOL);
        delay_us(1000);
        Ok(Self {
            ctlr: Arc::new(ctlr),
            present,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let s = cx.sleepable();
        let mut codecs: Vec<Link> = Vec::new();
        for cad in 0..15u8 {
            if !reg::codec_present(self.present, cad) {
                continue;
            }
            let vendor =
                self.ctlr
                    .command(s, cad, verb::get_parameter(0, 0, verb::PARAM_VENDOR_ID));
            let revision =
                self.ctlr
                    .command(s, cad, verb::get_parameter(0, 0, verb::PARAM_REVISION_ID));
            let (Some(v), Some(r)) = (vendor, revision) else {
                continue;
            };
            if v == verb::INVALID && r == verb::INVALID {
                continue;
            }
            codecs.push(Link {
                ctlr: Arc::downgrade(&self.ctlr),
                cad,
                vendor: verb::vendor_of(v),
                device: verb::device_of(v),
            });
        }
        for link in codecs {
            let ident = Ident::Channel {
                class: "hdacc",
                channel: u32::from(link.cad),
            };
            cx.add_child(ident, Some(Arc::new(link) as _), Vec::<Resource>::new());
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        // Leave the link in reset (`hdac_detach`'s `hdac_reset(sc, false)`).
        let _ = self.ctlr.reset(false);
    }
}

/// The declaration for the build's driver table.
pub const HDAC: DriverDecl = DriverDecl::of::<Hdac>();
