// SPDX-License-Identifier: BSD-2-Clause
//! The CIX Sky1's DisplayPort transmitter as firmware describes it
//! (`CIXH502F`): its registers, interrupt, clocks and reset, the combo PHY
//! behind it, and the encoder and connector it gives the display card
//! (clean-room specification `sky1-dp-tx` 00, 02).
//!
//! The transmitter's logic is `sky1-dp-tx` and the PHY's is
//! `sky1-usbdp-phy`; this crate is what binds them to the device model.
//!
//! Firmware may have left a link running (the board's boot display is
//! unit 4). It is taken over as it is: no reset, no PHY preparation, no
//! training. The first encoder enable keeps the stream if it is the one
//! the mode asks for, and otherwise resets the block and starts afresh.
//!
//! Not built: the unit-2 eDP PHY and panel (declined), the video
//! adapter's second pipeline and YCbCr output, multi-stream, audio, panel
//! self refresh, and the hot-plug uevent that tells a compositor to look
//! again (a compositor that polls connectors sees the change).

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use abi::drm::{DRM_MODE_CONNECTOR_DISPLAYPORT, DRM_MODE_ENCODER_TMDS};
use bus::provider::ClockHandle;
use bus::provider::ResetHandle;
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, Need, StartCx,
    StopCx, StopReason,
};
use devtree::ids::AcpiId;
use drm::kms::{Connection, ConnectorHw, EncoderHw};
use irq::Filter;
use kms_check::Mode;
use regs::Bytes;
use sched::{IrqCx, Sleepable};
use sky1_dp_tx::dptx::{Config, Dptx};
use sky1_dp_tx::link::Phy;
use sky1_dp_tx::regs as dpr;
use sky1_usbdp_phy::phy::{UsbDpPhy, Words32};
use sync::SpinLock;

pub mod outputs;

/// The firmware's hardware ID.
pub const HID: &str = "CIXH502F";

/// The combo PHY serving DisplayPort controller `unit`, by its index: DP
/// 0, 1, 3 and 4 have PHYs 0 to 3 (section 3.1 of the combo PHY text); DP
/// 2 has the eDP PHY instead.
#[must_use]
pub fn phy_index(unit: u32) -> Option<u32> {
    match unit {
        0 => Some(0),
        1 => Some(1),
        3 => Some(2),
        4 => Some(3),
        _ => None,
    }
}

/// The firmware path of the PHY with index `n`.
#[must_use]
pub fn phy_path(n: u32) -> String {
    format!("\\_SB_.UCP{n}")
}

/// A register window the driver can give up while the encoder and the
/// connector, which the display card holds, still refer to it: once the
/// driver stops (before or after the card, as the order of detach falls),
/// the grant goes back and reads give 0 and writes nothing. Without it the
/// card's references kept the grants past the driver's detach (the ledger's
/// "detach left 3 grant(s) outstanding").
struct Window(SpinLock<Option<MmioGrant<Bytes>>>);

impl Window {
    fn new(g: MmioGrant<Bytes>) -> Self {
        Self(SpinLock::new(Some(g)))
    }

    fn read(&self, offset: usize) -> u32 {
        self.0
            .lock()
            .as_ref()
            .map_or(0, |g| regs::Mmio::read32(g, offset))
    }

    fn write(&self, offset: usize, v: u32) {
        if let Some(g) = self.0.lock().as_ref() {
            regs::Mmio::write32(g, offset, v);
        }
    }

    fn len(&self) -> usize {
        self.0.lock().as_ref().map_or(0, |g| g.len())
    }

    /// Gives the grant back.
    fn revoke(&self) {
        let g = self.0.lock().take();
        drop(g);
    }
}

struct Core(Window);

impl dpr::Words for Core {
    fn read(&self, offset: usize) -> u32 {
        self.0.read(offset)
    }
    fn write(&self, offset: usize, v: u32) {
        self.0.write(offset, v);
    }
}

struct PhyWindow(Window);

impl Words32 for PhyWindow {
    fn read(&self, offset: usize) -> u32 {
        self.0.read(offset)
    }
    fn write(&self, offset: usize, v: u32) {
        self.0.write(offset, v);
    }
}

struct KernelDelay;

impl sky1_dp_tx::Delay for KernelDelay {
    fn delay_us(&self, us: u64) {
        bus::manager().delay_us(us);
    }
}

/// The video adapter between the display processor and the core (section
/// 1.10): pipeline 0's register, for the units that map it.
struct Adapter(Window);

impl Adapter {
    /// RGB at one pixel per clock, or two (bit 29).
    fn set_pipeline0(&self, two_pixels: bool) {
        let at = dpr::rcsu_adapter(0);
        if at + 4 > self.0.len() {
            return;
        }
        let v = self.0.read(at);
        let v = v & !(1 << 29) & !(3 << 30) | u32::from(two_pixels) << 29;
        self.0.write(at, v);
    }
}

/// What the encoder, connector and driver share.
struct Link {
    dptx: Dptx,
    phy: Arc<UsbDpPhy>,
    adapter: Option<Adapter>,
    /// The core's and the PHY interface's windows, to give back at stop.
    core: Arc<Core>,
    phy_win: Arc<PhyWindow>,
    vid_clk0: SpinLock<Option<ClockHandle>>,
    reset: SpinLock<Option<ResetHandle>>,
    apb_mhz: u32,
    /// A link taken over from firmware, until the first enable decides.
    adopted: AtomicBool,
    /// The link was shut down: the next bring-up re-initialises the host
    /// side with a reset first (section 2.11.2, 2.1).
    reinit_next: AtomicBool,
    /// What the monitor said, when last read.
    edid: SpinLock<Option<(Vec<u8>, edid::Edid)>>,
    sink_bpc: SpinLock<u32>,
}

/// The thread an encoder operation runs on: a commit, which may sleep.
#[allow(unsafe_code)]
fn thread() -> Sleepable {
    // SAFETY: encoders are enabled and disabled from the commit path, on
    // the requesting thread; never from an interrupt.
    unsafe { Sleepable::assume() }
}

impl Link {
    /// Resets the block and the PHY interface and sets the transmitter up
    /// again (section 2.1), for a link that cannot be kept.
    fn reinit(&self) {
        let s = thread();
        let mut r = self.reset.lock().take();
        if let Some(h) = r.as_mut() {
            let _ = h.pulse(&s, bus::provider::Micros(20));
        }
        *self.reset.lock() = r;
        bus::manager().delay_us(100);
        self.dptx.forget_link();
        // The PHY core starts from nothing, with the table.
        self.phy.power_down();
        self.phy.prepare();
        self.dptx.host_init(self.apb_mhz);
    }

    /// Reads the sink: capabilities, then the monitor's data. Whether a
    /// usable sink is there.
    fn read_sink(&self) -> bool {
        if let Err(e) = self.dptx.probe() {
            bus::manager().log(format_args!("sky1_dp: no sink data: {e:?}"));
            *self.edid.lock() = None;
            return false;
        }
        let parsed = match self.dptx.edid() {
            Ok(raw) => match edid::parse(&raw) {
                Ok(e) => Some((raw, e)),
                Err(e) => {
                    bus::manager().log(format_args!("sky1_dp: EDID not usable: {e:?}"));
                    None
                }
            },
            Err(e) => {
                bus::manager().log(format_args!("sky1_dp: EDID not read: {e:?}"));
                None
            }
        };
        if let Some((_, e)) = &parsed {
            *self.sink_bpc.lock() = u32::from(e.bpc.unwrap_or(8));
        }
        *self.edid.lock() = parsed;
        true
    }
}

struct Encoder(Arc<Link>);

impl EncoderHw for Encoder {
    fn kind(&self) -> u32 {
        // DisplayPort has no encoder type of its own (the standard
        // interface names the transmitter's TMDS).
        DRM_MODE_ENCODER_TMDS
    }

    fn enable(&self, _crtc: usize, mode: &Mode) {
        let l = &*self.0;
        let bpc = *l.sink_bpc.lock();
        let m = *mode;
        if l.adopted.swap(false, Ordering::AcqRel) {
            if l.dptx.stream_matches(&m) {
                // The display is showing this already: join its video
                // clock and leave everything as it is.
                let s = thread();
                let mut c = l.vid_clk0.lock().take();
                if let Some(h) = c.as_mut() {
                    let _ = h.enable(&s);
                }
                *l.vid_clk0.lock() = c;
                l.dptx.keep_stream();
                return;
            }
            l.reinit();
        } else if l.reinit_next.swap(false, Ordering::AcqRel) {
            l.reinit();
        }
        // A sink just woken after the last link was shut down can refuse
        // (NACK) the first requests (seen on the board between two
        // modes): try again after a pause, and once more from a full
        // re-initialisation of the host side.
        let mut tried = l.dptx.link_up(&m, bpc);
        for attempt in 0..2 {
            let Err(e) = &tried else { break };
            let (hpd, status) = l.dptx.aux_snapshot();
            bus::manager().log(format_args!(
                "sky1_dp: link did not come up: {e:?} (hpd {hpd}, aux status {status:#x}), trying again"
            ));
            bus::manager().delay_us(100_000);
            if attempt == 1 {
                l.reinit();
            }
            tried = l.dptx.link_up(&m, bpc);
        }
        let up = match tried {
            Ok(up) => up,
            Err(e) => {
                let (hpd, status) = l.dptx.aux_snapshot();
                bus::manager().log(format_args!(
                    "sky1_dp: link did not come up: {e:?} (hpd {hpd}, aux status {status:#x})"
                ));
                return;
            }
        };
        bus::manager().log(format_args!(
            "sky1_dp: link up, {:?} x{}, {} bits a component",
            up.trained.link.rate, up.trained.link.lanes, up.bpc
        ));
        let s = thread();
        let mut c = l.vid_clk0.lock().take();
        if let Some(h) = c.as_mut() {
            let _ = h.enable(&s);
        }
        *l.vid_clk0.lock() = c;
        if let Some(a) = &l.adapter {
            a.set_pipeline0(sky1_dp_tx::stream::pixels_per_clock(m.clock_khz) == 2);
        }
        if let Err(e) = l.dptx.stream_on(&m) {
            bus::manager().log(format_args!("sky1_dp: stream did not start: {e:?}"));
        }
    }

    fn disable(&self) {
        let l = &*self.0;
        l.dptx.stream_off();
        let s = thread();
        let mut c = l.vid_clk0.lock().take();
        if let Some(h) = c.as_mut() {
            let _ = h.disable(&s);
        }
        *l.vid_clk0.lock() = c;
        l.dptx.link_down();
        l.phy.link_off();
        l.reinit_next.store(true, Ordering::Release);
    }
}

struct Connector(Arc<Link>);

impl ConnectorHw for Connector {
    fn kind(&self) -> u32 {
        DRM_MODE_CONNECTOR_DISPLAYPORT
    }

    fn detect(&self) -> Connection {
        let l = &*self.0;
        if !l.dptx.connected() {
            return Connection::Disconnected;
        }
        if l.dptx.caps().is_some() || l.read_sink() {
            Connection::Connected
        } else {
            Connection::Disconnected
        }
    }

    fn modes(&self) -> Vec<Mode> {
        let l = &*self.0;
        let from_monitor = l
            .edid
            .lock()
            .as_ref()
            .map(|(_, e)| e.modes.clone())
            .filter(|m| !m.is_empty());
        let modes = from_monitor.unwrap_or_else(edid::fallback_modes);
        let lanes = l.phy.lanes().min(4);
        modes
            .iter()
            .map(|m| l.dptx.offered(m))
            .filter(|m| fits(l, m, lanes))
            .collect()
    }

    fn edid(&self) -> Option<Vec<u8>> {
        self.0.edid.lock().as_ref().map(|(raw, _)| raw.clone())
    }

    fn size_mm(&self) -> (u32, u32) {
        self.0
            .edid
            .lock()
            .as_ref()
            .map_or((0, 0), |(_, e)| e.size_mm)
    }
}

/// Whether the link the sink and the port allow can carry `m` at 6 bits a
/// component at least.
fn fits(l: &Link, m: &Mode, lanes: u8) -> bool {
    let Some(caps) = l.dptx.caps() else {
        // Not read yet: the board's limit.
        return m.clock_khz <= 600_000;
    };
    let lim = sky1_dp_tx::link::Limits::of(&caps, sky1_dp_tx::dpcd::Rate::Hbr2, 4, lanes);
    m.clock_khz <= 600_000 && sky1_dp_tx::link::pick_rgb_bpc(m.clock_khz, 8, 8, lim).is_some()
}

/// `dpN`.
pub struct SkyDp {
    link: Arc<Link>,
    irq: SpinLock<Option<IrqLine>>,
    hpd: AtomicBool,
    attention: AtomicBool,
    _apb: ClockHandle,
    unit: u32,
}

impl core::fmt::Debug for SkyDp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SkyDp")
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

fn filter(d: &SkyDp, _: usize, _: &IrqCx) -> Filter {
    match d.link.dptx.interrupt() {
        None => Filter::Stray,
        Some(ev) => {
            if ev.hpd {
                d.hpd.store(true, Ordering::Release);
            }
            if ev.attention {
                d.attention.store(true, Ordering::Release);
            }
            Filter::Schedule
        }
    }
}

/// Plug, unplug and the sink's calls for attention (section 2.2). The
/// level is read again here: events bounce.
fn work(d: &SkyDp, _: usize, _: &Sleepable) {
    let l = &d.link;
    if d.hpd.swap(false, Ordering::AcqRel) {
        if l.dptx.connected() {
            l.read_sink();
        } else {
            // Down at once, even before the display stack disables it.
            l.dptx.stream_off();
            l.dptx.link_down();
            l.phy.link_off();
            *l.edid.lock() = None;
        }
    }
    // A short pulse wants the link checked; retraining on loss is not
    // built, so the pulse only has the sink read again when it is idle.
    let _ = d.attention.swap(false, Ordering::AcqRel);
}

impl Driver for SkyDp {
    const NAME: &'static str = "sky1_dptx";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new(HID)]);

    fn describe(_: &Ident) -> String {
        "Trilinear DisplayPort transmitter (CIX Sky1)".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let s = cx.sleepable();
        let unit: u32 = match cx.ident() {
            Ident::Acpi(i, _) => i.uid.as_deref().and_then(|u| u.parse().ok()),
            _ => None,
        }
        .ok_or(AttachError::Device("no unit number"))?;
        // Unit 2's PHY is the eDP one, which is not built.
        let Some(phy_n) = phy_index(unit) else {
            return Err(AttachError::Decline);
        };
        // The PHY's driver publishes it; wait for that.
        let Some(shared) = sky1_usbdp_phy::registry::lookup(unit) else {
            return Err(AttachError::Wait(Need::Firmware(phy_path(phy_n))));
        };
        let mut apb = cx.clock(Some("apb_clk"))?;
        let vid0 = cx.clock(Some("vid_clk0"))?;
        let mut reset = cx.reset(Some("dp_reset"))?;
        apb.enable(s).map_err(|_| AttachError::Device("apb_clk"))?;
        // The windows are the resources in firmware's order: the core, the
        // unused `dsc`, the PHY interface, the video adapter.
        let core = Arc::new(Core(Window::new(cx.mmio_bytes(0)?)));
        let phy_win = Arc::new(PhyWindow(Window::new(cx.mmio_bytes(2)?)));
        let adapter = cx.mmio_bytes(3).ok().map(|g| Adapter(Window::new(g)));
        if !UsbDpPhy::is_combo(&*phy_win) {
            return Err(AttachError::Decline);
        }
        // A link firmware left running is taken over as it is.
        let lit_reg = dpr::source(0) + dpr::VIDEO_STREAM_ENABLE;
        let lit = dpr::Words::read(&*core, lit_reg) & 1 != 0
            || dpr::Words::read(&*core, dpr::SOURCE_ENABLE) & 1 != 0;
        let apb_mhz = apb
            .rate(s)
            .map(|h| u32::try_from(h.0 / 1_000_000).unwrap_or(0))
            .unwrap_or(0);
        // Host initialisation (section 2.1): reset the block, then the
        // PHY interface, then the transmitter. Not for a link that is up.
        if !lit {
            reset
                .pulse(s, bus::provider::Micros(20))
                .map_err(|_| AttachError::Device("dp_reset"))?;
            bus::manager().delay_us(100);
        }
        let phy = Arc::new(UsbDpPhy::with_core(
            shared.core.clone(),
            shared.regs.clone(),
            phy_win.clone(),
            shared.plat.clone(),
        ));
        if !lit {
            phy.prepare();
        }
        let phy_dyn: Arc<dyn Phy> = phy.clone();
        let dptx = Dptx::new(
            core.clone(),
            Arc::new(KernelDelay),
            phy_dyn,
            // The boards cap at HBR2 (`cix,dp-max-rate` 540000).
            Config::default(),
        );
        if lit {
            // Whatever is half-running is left alone.
            if dptx.adopt().is_none() {
                return Err(AttachError::Decline);
            }
        } else {
            dptx.host_init(apb_mhz);
        }
        let link = Arc::new(Link {
            dptx,
            phy,
            adapter,
            core,
            phy_win,
            vid_clk0: SpinLock::new(Some(vid0)),
            reset: SpinLock::new(Some(reset)),
            apb_mhz,
            adopted: AtomicBool::new(lit),
            reinit_next: AtomicBool::new(false),
            edid: SpinLock::new(None),
            sink_bpc: SpinLock::new(8),
        });
        let irq = cx.irq(0)?;
        Ok(Self {
            link,
            irq: SpinLock::new(Some(irq)),
            hpd: AtomicBool::new(false),
            attention: AtomicBool::new(false),
            _apb: apb,
            unit,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        outputs::publish(
            self.unit,
            outputs::Output {
                encoder: Arc::new(Encoder(self.link.clone())),
                connector: Arc::new(Connector(self.link.clone())),
            },
        );
        let line = self.irq.lock().take();
        if let Some(l) = line {
            cx.interrupt(l, Some(filter), Some(work), 0)?;
        }
        // A sink already plugged in.
        if self.link.dptx.connected() {
            self.link.read_sink();
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        outputs::withdraw(self.unit);
        self.link.dptx.host_exit();
        self.link.phy.link_off();
        // The card may outlive this driver and hold the encoder: the
        // windows go back now.
        self.link.core.0.revoke();
        self.link.phy_win.0.revoke();
        if let Some(a) = &self.link.adapter {
            a.0.revoke();
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<SkyDp>();

#[cfg(test)]
mod tests;
