// SPDX-License-Identifier: BSD-2-Clause
//! The display processor as the Sky1's firmware describes it (`CIXH5010`)
//! and the kernel's device model sees it: its registers, interrupt,
//! clocks and memory, and the card built over them (clean-room
//! specification `linlon-dpu` 01, 08 §8.1 to §8.5).
//!
//! Firmware may have left a display running (the board's boot display is
//! unit 4). Until adoption exists such a unit is left alone: this driver
//! declines it, so the firmware's console stays.
//!
//! The unit's output is the transmitter of the same number
//! (`CIXH502F`, `sky1-dp`): this driver waits for it, builds the card
//! with its encoder and connector, and registers the card when it
//! starts. Pipeline 0 only; the second pipeline's stream is not built.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use bus::provider::{ClockHandle, Hz};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, Need, StartCx,
    StopCx, StopReason,
};
use devtree::ids::AcpiId;
use drm::gem::Gem;
use irq::Filter;
use regs::Bytes;
use sched::{IrqCx, Sleepable};
use sync::SpinLock;
use vm::dma::{Coherency, DmaConstraints, DmaTag, IoMapping};

use crate::card::{Card, ConsoleBuffer, Mapped, Output, Platform, create};
use crate::discover::{self, Delay};
use crate::hw::Dpu;
use crate::regs::{Words, bs};

/// The firmware's hardware ID.
pub const HID: &str = "CIXH5010";

/// The window is 128 KiB (section 08 §8.1).
const WINDOW: usize = 0x2_0000;

struct Regs(MmioGrant<Bytes>);

impl Words for Regs {
    fn read(&self, offset: usize) -> u32 {
        regs::Mmio::read32(&self.0, offset)
    }
    fn write(&self, offset: usize, v: u32) {
        regs::Mmio::write32(&self.0, offset, v);
    }
}

struct KernelDelay;

impl Delay for KernelDelay {
    fn delay_us(&self, us: u64) {
        bus::manager().delay_us(us);
    }
}

/// A mapping in the unit's translation domain, undone on drop.
struct DomainMapping {
    domain: Arc<dyn vm::dma::IoDomain>,
    mapping: IoMapping,
}

impl Drop for DomainMapping {
    fn drop(&mut self) {
        self.domain.unmap(&self.mapping);
    }
}

impl core::fmt::Debug for DomainMapping {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DomainMapping({:#x})", self.mapping.addr)
    }
}

/// Runs of consecutive frames as (address, length) pairs.
fn runs(frames: &[vm::Pfn]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for f in frames {
        let pa = f.addr().get();
        match out.last_mut() {
            Some((start, len)) if *start + *len == pa => *len += addr::PAGE_SIZE,
            _ => out.push((pa, addr::PAGE_SIZE)),
        }
    }
    out
}

/// What the card gets from the machine: memory for the display to read
/// and the clocks of its pipelines.
struct Machine {
    tag: DmaTag,
    aclk_khz: u64,
    pixel: [SpinLock<Option<ClockHandle>>; 2],
}

impl Platform for Machine {
    fn map(&self, gem: &Arc<Gem>) -> Result<Mapped, Errno> {
        let frames = gem.frames()?;
        let ranges = runs(&frames);
        match self.tag.domain() {
            // Through the unit's translation: any pages will do.
            Some(d) => {
                let mapping = d
                    .map(&ranges, false, self.tag.constraints())
                    .map_err(|_| Errno::ENOMEM)?;
                Ok(Mapped {
                    addr: mapping.addr,
                    // The addresses are the unit's, not the display
                    // processor's own translation (section 01 §1.5).
                    translated: false,
                    keep: Box::new(DomainMapping {
                        domain: Arc::clone(d),
                        mapping,
                    }),
                })
            }
            // Without one, the buffer must be one run of memory.
            None => match ranges.as_slice() {
                [(pa, _)] => Ok(Mapped {
                    addr: *pa,
                    translated: false,
                    keep: Box::new(()),
                }),
                _ => Err(Errno::EINVAL),
            },
        }
    }

    fn clean(&self, gem: &Gem, range: core::ops::Range<u64>) {
        let Ok(frames) = gem.frames() else { return };
        let mem = self.tag.memory();
        let page = addr::PAGE_SIZE;
        for (i, f) in (0u64..).zip(frames) {
            let (lo, hi) = (i * page, (i + 1) * page);
            if hi <= range.start || lo >= range.end {
                continue;
            }
            mem.clean(
                mem.virt(f.addr().get()).as_ptr(),
                usize::try_from(page).unwrap_or(0),
            );
        }
    }

    fn pixel_clock_on(&self, s: &Sleepable, p: usize, khz: u32) -> Result<(), Errno> {
        let mut slot = self.pixel.get(p).ok_or(Errno::EINVAL)?.lock().take();
        let r = match slot.as_mut() {
            Some(c) => {
                let hz = Hz(u64::from(khz) * 1000);
                // A rate at least the request is taken (section 08 §8.3).
                let got = c
                    .round_rate(s, hz)
                    .map_err(|_| Errno::EINVAL)
                    .and_then(|g| {
                        if g.0 >= hz.0 {
                            Ok(())
                        } else {
                            Err(Errno::EINVAL)
                        }
                    });
                got.and_then(|()| c.set_rate(s, hz).map_err(|_| Errno::EINVAL))
                    .and_then(|()| c.enable(s).map_err(|_| Errno::EINVAL))
            }
            None => Err(Errno::EINVAL),
        };
        *self.pixel[p].lock() = slot;
        r
    }

    fn pixel_clock_off(&self, s: &Sleepable, p: usize) {
        let Some(cell) = self.pixel.get(p) else {
            return;
        };
        let mut slot = cell.lock().take();
        if let Some(c) = slot.as_mut() {
            let _ = c.disable(s);
        }
        *cell.lock() = slot;
    }

    fn pixel_clock_adopt(&self, s: &Sleepable, p: usize) -> Option<u32> {
        let mut slot = self.pixel.get(p)?.lock().take();
        let khz = slot.as_mut().and_then(|c| {
            let hz = c.rate(s).ok()?;
            c.enable(s).ok()?;
            u32::try_from(hz.0 / 1000).ok()
        });
        *self.pixel[p].lock() = slot;
        khz
    }

    fn aclk_khz(&self) -> u64 {
        self.aclk_khz
    }

    fn now_ns(&self) -> u64 {
        sched::now_ns()
    }

    fn console_buffer(&self, width: u32, height: u32) -> Result<ConsoleBuffer, Errno> {
        let pixels =
            usize::try_from(u64::from(width) * u64::from(height)).map_err(|_| Errno::EINVAL)?;
        // Through the display's translation, as clients' buffers are, and
        // zeroed and cleaned from the caches by the allocation: black.
        let mem = self
            .tag
            .coherent::<u32>(pixels)
            .map_err(|_| Errno::ENOMEM)?;
        let buf = ConsoleBuffer {
            addr: mem.bus_addr(),
            translated: false,
            phys: mem.phys_addr(),
            width,
            height,
        };
        // The console and a panic may show it at any time: never freed.
        core::mem::forget(mem);
        Ok(buf)
    }

    fn console_retarget(&self, s: &Sleepable, b: &ConsoleBuffer) -> bool {
        bus::manager().console_retarget(s, b.phys, b.width, b.height, b.width)
    }

    fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(args);
    }
}

/// `komedaN`.
pub struct Komeda {
    card: Arc<Card>,
    /// The device node, while registered.
    node: SpinLock<Option<Arc<drm::Card>>>,
    /// The output's connector, to say what it sees once registered.
    connector: Arc<dyn drm::kms::ConnectorHw>,
    unit: u32,
    irq: SpinLock<Option<IrqLine>>,
    _aclk: ClockHandle,
    /// Held so the clocks the card enables and disables outlive it.
    _machine: Arc<Machine>,
}

impl core::fmt::Debug for Komeda {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Komeda")
            .field("card", &self.card)
            .finish_non_exhaustive()
    }
}

fn intr(d: &Komeda, _: usize, _: &IrqCx) -> Filter {
    if d.card.interrupt() {
        Filter::Handled
    } else {
        Filter::Stray
    }
}

/// A clock the unit may lack.
fn optional(r: Result<ClockHandle, bus::ResourceError>) -> Option<ClockHandle> {
    r.ok()
}

impl Driver for Komeda {
    const NAME: &'static str = "komeda";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new(HID)]);

    fn describe(_: &Ident) -> String {
        "Arm China Linlon display controller (CIX Sky1)".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let s = cx.sleepable();
        let unit: u32 = match cx.ident() {
            Ident::Acpi(i, _) => i.uid.as_deref().and_then(|u| u.parse().ok()),
            _ => None,
        }
        .ok_or(AttachError::Device("no unit number"))?;
        let mut aclk = cx.clock(Some("aclk"))?;
        // The main clock is pinned (section 08 §8.3); its register access
        // needs it running.
        let fixed = acpi::properties::integer(cx, "aclk_freq_fixed").unwrap_or(800_000_000);
        aclk.set_rate(s, Hz(fixed))
            .map_err(|_| AttachError::Device("aclk rate"))?;
        aclk.enable(s)
            .map_err(|_| AttachError::Device("aclk enable"))?;
        let (px0, px1) = (
            optional(cx.clock(Some("pipeline@0"))),
            optional(cx.clock(Some("pipeline@1"))),
        );
        // Firmware releases only the unit it uses for the boot display; the
        // others are held in reset, and the interconnect's monitor reports
        // any access to a unit in reset (observed on the board, 2026-11-23).
        // Release the subsystem's control block first, then the processor
        // (sky1-soc 02 §2.2 and §2.4). Releasing a line already released
        // changes nothing, so a unit firmware lit keeps scanning.
        let mut rcsu = cx.reset(Some("rcsu_reset"))?;
        let mut ip = cx.reset(Some("ip_reset"))?;
        rcsu.deassert(s)
            .map_err(|_| AttachError::Device("rcsu_reset"))?;
        ip.deassert(s)
            .map_err(|_| AttachError::Device("ip_reset"))?;
        let regs = cx.mmio_bytes(0)?;
        if regs.len() < WINDOW {
            return Err(AttachError::Device("register window too small"));
        }
        let irq = cx.irq(0)?;
        let words = Arc::new(Regs(regs));
        let delay = Arc::new(KernelDelay);
        let layout = discover::walk(&*words, WINDOW)
            .map_err(|_| AttachError::Device("not a Linlon display processor"))?;
        // Pipelines the firmware left scanning out are taken over as they
        // are: no reset, and their memory path stays as it is.
        let lit: Vec<bool> = layout
            .pipes
            .iter()
            .map(|p| words.read(p.bs.offset + bs::CONTROL) & bs::CTRL_EN != 0)
            .collect();
        let any_lit = lit.iter().any(|l| *l);
        let constraints = DmaConstraints {
            addr_bits: 40,
            align: 1,
            boundary: None,
            max_segment: 0xFFFF_FFFF,
            max_segments: u16::MAX,
            // `_CCA` is 0 on this board and the unit does not snoop.
            coherency: Coherency::NonCoherent,
        };
        // The unit's streams get one translated context; the firmware's
        // framebuffer (an RMR range) is mapped to itself in it before the
        // streams switch from bypass (docs/IOMMU.md §4.9), so a display
        // scanning it out keeps scanning.
        let tag = cx.dma_tag(constraints)?;
        // The transmitter of the same number gives the output; until it
        // has attached there is nothing to build the card for.
        let Some(out) = sky1_dp::outputs::lookup(unit) else {
            return Err(AttachError::Wait(Need::Firmware(alloc::format!(
                "\\_SB_.DP0{unit}"
            ))));
        };
        if !any_lit {
            discover::soft_reset(&*words, &*delay)
                .map_err(|_| AttachError::Device("soft reset"))?;
        }
        let dpu = Dpu::new(words.clone(), delay, layout);
        dpu.init();
        for (p, is_lit) in lit.iter().enumerate() {
            if !is_lit {
                dpu.clear(p);
            }
        }
        let machine = Arc::new(Machine {
            tag,
            aclk_khz: fixed / 1000,
            pixel: [SpinLock::new(px0), SpinLock::new(px1)],
        });
        let connector = out.connector.clone();
        let outputs = alloc::vec![Output {
            pipe: 0,
            encoder: out.encoder,
            connector: out.connector,
        }];
        let card =
            create(dpu, machine.clone(), outputs).map_err(|_| AttachError::Device("card"))?;
        if any_lit {
            card.adopt(s);
        }
        Ok(Self {
            card,
            node: SpinLock::new(None),
            connector,
            unit,
            irq: SpinLock::new(Some(irq)),
            _aclk: aclk,
            _machine: machine,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let line = self.irq.lock().take();
        if let Some(l) = line {
            cx.interrupt(l, Some(intr), None, 0)?;
        }
        // The card exists for userland from here.
        match drm::register(self.card.clone()) {
            Ok(n) => {
                crate::dri::publish(cx.sleepable(), n.unit(), false, "platform:komeda", true);
                *self.node.lock() = Some(n);
            }
            Err(_) => return Err(AttachError::Device("registering the card")),
        }
        // What the connector sees, for the console log, and the screen it
        // shows is the one the firmware's framebuffer card stands for: that
        // card goes (docs/GRAPHICS.md §7.5).
        let connected = self.connector.detect() == drm::kms::Connection::Connected;
        let modes = self.connector.modes();
        let first = modes
            .first()
            .map_or(alloc::string::String::from("none"), |m| {
                let hz = (u64::from(m.clock_khz) * 1000)
                    .checked_div(u64::from(m.htotal) * u64::from(m.vtotal))
                    .unwrap_or(0);
                alloc::format!("{}x{}@{hz}", m.hdisplay, m.vdisplay)
            });
        bus::manager().log(format_args!(
            "komeda{}: DisplayPort {}, {} mode(s), first {first}",
            self.unit,
            if connected {
                "connected"
            } else {
                "disconnected"
            },
            modes.len()
        ));
        if connected {
            let n = drm::retire_firmware_cards(cx.sleepable());
            if n > 0 {
                bus::manager().log(format_args!(
                    "komeda{}: took over from the firmware framebuffer card",
                    self.unit
                ));
            }
            // The console gets a framebuffer of its own at the display's
            // mode, in place of firmware's.
            self.card.console(cx.sleepable());
        }
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let node = self.node.lock().take();
        if let Some(n) = node {
            n.unregister(cx.sleepable());
        }
        self.card.dpu().mask();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Komeda>();

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeMap;
    use std::vec;

    use acpi_platform::PlatformSource;

    use super::*;

    /// The five display units as the Orange Pi 6 Plus firmware gives
    /// them: registers, clocks and resets by the names the driver asks
    /// for.
    #[test]
    fn the_firmware_gives_each_unit_the_lines_the_driver_asks_for() {
        let (mut i, mut h) = acpi_corpus::load("opi6p");
        let devices = acpi::deps::devices(&mut i, &mut h);
        let x = cix_sky1::PLATFORM.extract(&mut i, &mut h, &devices);
        let bases = [
            0x1401_0000u64,
            0x1408_0000,
            0x140F_0000,
            0x1416_0000,
            0x141D_0000,
        ];
        let mut seen = 0;
        for &d in &devices {
            if i.hid(d, &mut h).ok().flatten().as_deref() != Some(HID) {
                continue;
            }
            let Some(aml::Uid::Integer(uid)) = i.uid(d, &mut h).ok().flatten() else {
                panic!("a unit number");
            };
            let uid = usize::try_from(uid).unwrap();
            let crs = i.crs(d, &mut h).ok().flatten().unwrap_or_default();
            let mem = acpi::ranges::consumed(&crs);
            assert_eq!(mem.first().map(|r| r.base), Some(bases[uid]));
            let clocks: BTreeMap<_, _> = x
                .description
                .clocks
                .iter()
                .filter(|c| c.consumer == d)
                .map(|c| (c.name.clone().unwrap_or_default(), c.id))
                .collect();
            let n = u32::try_from(uid).unwrap();
            let want: BTreeMap<_, _> = vec![
                ("aclk".into(), 44 + n),
                ("pipeline@0".into(), 33 + 2 * n),
                ("pipeline@1".into(), 34 + 2 * n),
            ]
            .into_iter()
            .collect();
            assert_eq!(clocks, want, "unit {uid}");
            let resets: BTreeMap<_, _> = x
                .description
                .resets
                .iter()
                .filter(|r| r.consumer == d)
                .map(|r| (r.name.clone().unwrap_or_default(), r.index))
                .collect();
            assert_eq!(resets.get("ip_reset"), Some(&(19 + n)));
            assert_eq!(resets.get("rcsu_reset"), Some(&(110 + n)));
            seen += 1;
        }
        assert_eq!(seen, 5);
    }

    #[test]
    fn contiguous_frames_make_one_run() {
        let f = vm::Pfn;
        assert_eq!(
            runs(&[f(10), f(11), f(12), f(20), f(21)]),
            vec![(10 * 4096, 3 * 4096), (20 * 4096, 2 * 4096)]
        );
        assert!(runs(&[]).is_empty());
    }
}
