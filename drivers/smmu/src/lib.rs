// SPDX-License-Identifier: BSD-2-Clause
//! `smmu`: the Arm SMMUv3 (Arm IHI 0070) as the IORT describes it, a
//! child of `acpi0` (docs/IOMMU.md).
//!
//! The driver probes, claims its registers and its event, sync, and gerror
//! interrupts (not PRI, as FreeBSD), and reads the identification
//! registers. Then, with DMA translation on (`hw.iommu.dma`, IOMMU.md
//! §4.12) and the features it needs, it runs the SMMU (stage 2 of IOMMU.md
//! §8; [`unit`]):
//!
//! - Every stream starts invalid: DMA from a device nobody gave a context
//!   aborts, with a `C_BAD_STE` event.
//! - The streams with reserved memory (IORT RMR nodes) are bypassed before
//!   the unit is enabled, and FreeBSD's `N streams bypassed (reserved
//!   memory)` reports them; so are named components' streams with
//!   `hw.smmu.bypass_named=1` (IOMMU.md §4.9).
//! - It publishes a [`bus::iommu::Iommu`] provider. The broker asks it for
//!   a device's context when the device first needs DMA (its tag or MSI),
//!   and it makes one per stream (stage 3): a translated context is a
//!   `sys/iommu` domain with its own ASID and context descriptor, the
//!   stream's STE pointing at it (stage 1); a pass-through one (FreeBSD's
//!   disabled requesters, a virtio device using physical addresses) a
//!   bypass STE. Domains reach the unit for their invalidations
//!   ([`iommu_framework::domain::Invalidate`]). `dev.smmu.N.dma` says
//!   whether it translates: `hw.iommu.dma`, while message-signalled
//!   interrupts are off (until stage 4 maps the ITS doorbell, IOMMU.md
//!   §16).
//! - Its event lines name the device whose context the stream is
//!   (IOMMU.md §4.10).
//! - Events are drained in the event interrupt's thread and printed as
//!   FreeBSD's `smmu_print_event` does, rate-limited per stream; global
//!   errors are acknowledged, a refused command skipped.
//! - At shutdown (after every device it translates has stopped) it turns
//!   the SMMU off and leaves `GBPA` as firmware had it (IOMMU.md §4.11).
//!
//! With `hw.iommu.dma=0` the unit attaches, prints the board port's `left
//! disabled: DMA translation is off (hw.iommu.dma)`, and leaves the
//! hardware as firmware left it. An SMMU without a feature the driver
//! needs attaches the same way, with FreeBSD's messages: what it would
//! translate runs untranslated, as on FreeBSD when its attach fails.
//!
//! - Probe takes the generic SMMUv3 only (IORT model 0), as FreeBSD's
//!   `smmu_acpi_probe`.
//! - It refuses a voluntary detach: the devices it translates depend on
//!   it.
//! - `hw.smmu.bypass_named` (`CTLFLAG_RDTUN`, 0): what becomes of the
//!   streams of named components without a driver (IOMMU.md §4.9).
//!
//! Behavior reference: sys/arm64/iommu/smmu.c, smmu_acpi.c (and the
//! user's branch orangepi-6-plus of them).

// The translated-DMA tests' shared memory implements unsafe traits; the
// crate itself has no unsafe code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]
#![no_std]

extern crate alloc;

pub mod hw;
pub mod unit;

#[cfg(test)]
mod tests;
#[cfg(test)]
#[allow(unsafe_code)]
mod tests_dma;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::sysctl::ctlflag;
use acpi::iort::{SMMU_DESC, SmmuNode};
use acpi_tables::iort::SmmuV3;
use bus::{
    AttachCx, AttachError, Busy, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, StartCx,
    StopCx, StopReason,
};
use iommu::cd::ContextDescriptor;
use iommu::event::{EVENT_WORDS, Event, code};
use iommu::regs::{Features, Missing, gerror, off};
use iommu::ste::{Geometry, STE_WORDS, Ste};
use iommu::{Asid, PhysAddr};
use iommu_framework::domain::{Domain, Invalidate, Params, Unconfirmed};
use irq::Filter;
use regs::Bytes;
use sched::{IrqCx, Sleepable};
use sync::SpinLock;
use sysctl::{Number, Tree, Value};
use vm::dma::{Coherency, DmaCoherent, DmaConstraints, DmaMemory, DmaTag};

use crate::hw::Hw;
use crate::unit::{Notice, Unit};

/// `hw.smmu.bypass_named`.
static BYPASS_NAMED: AtomicI32 = AtomicI32::new(0);

/// Takes the boot-time tunables (`TUNABLE_INT_FETCH`): `get` gives a
/// name's integer value, if set. The kernel calls it before drivers
/// attach.
pub fn set_tunables(get: &dyn Fn(&str) -> Option<i32>) {
    if let Some(v) = get("hw.smmu.bypass_named") {
        // Relaxed: a tunable, read at attach.
        BYPASS_NAMED.store(v, Ordering::Relaxed);
    }
}

/// Whether named components without a driver have their streams
/// bypassed rather than aborted.
#[must_use]
pub fn bypass_named() -> bool {
    BYPASS_NAMED.load(Ordering::Relaxed) != 0
}

/// Adds `hw.smmu` and its knob to `t` (`smmu_acpi.c`'s static sysctls).
pub fn sysctl_init(t: &mut Tree) {
    let root = t.root();
    let Some(hw) = t.child(root, "hw") else {
        return;
    };
    let Some(node) = t
        .child(hw, "smmu")
        .or_else(|| t.add_node(hw, Number::Auto, "smmu", "SMMU").ok())
    else {
        return;
    };
    if let Ok(id) = t.add_value(
        node,
        Number::Auto,
        "bypass_named",
        "ACPI devices' streams bypass the SMMU (no translation)",
        Value::Int(Box::new(|| BYPASS_NAMED.load(Ordering::Relaxed))),
    ) {
        t.set_flags(id, ctlflag::TUN);
    }
}

/// The identification registers, read once at attach: IDR0, IDR1, IDR3,
/// IDR5, IIDR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Idr(pub [u32; 5]);

impl Idr {
    /// Reads them with `read32`.
    pub fn read(read32: impl Fn(u32) -> u32) -> Self {
        Self([off::IDR0, off::IDR1, off::IDR3, off::IDR5, off::IIDR].map(read32))
    }

    /// What they say, with the IORT's coherency override (`COHACC`).
    #[must_use]
    pub fn features(&self, firmware: &SmmuV3) -> Features {
        let [i0, i1, i3, i5, ii] = self.0;
        Features::decode(i0, i1, i3, i5, ii, firmware.coherent())
    }

    /// The `bootverbose` line with the raw registers.
    pub fn verbose(&self, mut line: impl FnMut(core::fmt::Arguments<'_>)) {
        let [i0, i1, i3, i5, ii] = self.0;
        line(format_args!(
            "IDR0 {i0:#x} IDR1 {i1:#x} IDR3 {i3:#x} IDR5 {i5:#x} IIDR {ii:#x}"
        ));
    }
}

/// The streams that bypass the SMMU from the start (IOMMU.md §4.9): every
/// RMR stream on it, and with `named` every named component's, without
/// duplicates, in table order. The `bootverbose` line for each.
#[must_use]
pub fn bypassed_streams(node: &SmmuNode, named: bool) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = Vec::new();
    let mut add = |sid: u32, why: String| {
        if !out.iter().any(|(s, _)| *s == sid) {
            out.push((sid, why));
        }
    };
    for r in &node.reserved {
        for sid in r.streams.iter().cloned().flatten() {
            for &(base, len) in &r.ranges {
                add(
                    sid,
                    alloc::format!(
                        "stream {sid:#x}: reserved {base:#x}-{:#x}{}",
                        base.saturating_add(len).saturating_sub(1),
                        if r.remap { "" } else { " (no remapping)" }
                    ),
                );
            }
        }
    }
    if named {
        for n in &node.named {
            for sid in n.streams.iter().cloned().flatten() {
                add(sid, alloc::format!("stream {sid:#x}: {}", n.name));
            }
        }
    }
    out
}

/// The kernel's [`Hw`]: the granted registers, and memory from the unit's
/// own direct tag (the SMMU's accesses are not translated).
pub struct Kernel {
    regs: MmioGrant<Bytes>,
    tag: DmaTag,
}

impl core::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Kernel").field("tag", &self.tag).finish()
    }
}

fn at(off: u32) -> usize {
    usize::try_from(off).unwrap_or(usize::MAX)
}

impl Hw for Kernel {
    type Mem = DmaCoherent<u64>;

    fn read32(&self, off: u32) -> u32 {
        self.regs.read32(at(off))
    }

    fn write32(&self, off: u32, v: u32) {
        self.regs.write32(at(off), v);
    }

    fn alloc(&self, bytes: u64, align: u64) -> Option<DmaCoherent<u64>> {
        let c = DmaConstraints {
            align,
            ..DmaConstraints::UNLIMITED
        };
        // The backend does not matter for coherent memory, which is
        // allocated within the tag's reach either way.
        let n = usize::try_from(bytes.div_ceil(8)).ok()?;
        self.tag.derive(&c, 0).coherent(n).ok()
    }

    fn pa(&self, m: &DmaCoherent<u64>) -> u64 {
        m.bus_addr()
    }

    fn load(&self, m: &DmaCoherent<u64>, word: usize) -> u64 {
        u64::from_le(m.read(word))
    }

    fn store(&self, m: &DmaCoherent<u64>, word: usize, v: u64) {
        m.write(word, v.to_le());
    }

    fn load32(&self, m: &DmaCoherent<u64>, off: usize) -> u32 {
        u32::from_le(m.read_at::<u32>(off))
    }

    fn write_barrier(&self) {
        hal::dma::write_barrier();
    }

    fn doorbell_barrier(&self) {
        hal::dma::doorbell_barrier();
    }

    fn read_barrier(&self) {
        hal::dma::read_barrier();
    }

    fn now_ns(&self) -> u64 {
        sched::now_ns()
    }

    fn relax(&self) {
        core::hint::spin_loop();
    }
}

/// The events of one stream, for the log's rate limit and for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamEvents {
    /// Events seen.
    pub total: u64,
    /// The last one.
    pub last: Option<Event>,
    /// When the current one-second window began.
    window: u64,
    /// Logged in it.
    logged: u32,
    /// Not logged in it.
    suppressed: u64,
}

/// How many events of one stream are printed per second; the rest are
/// counted (IOMMU.md §4.10).
pub const EVENTS_PER_SECOND: u32 = 5;

/// A stream's context: translated through a domain, or pass-through.
struct Ctx {
    domain: Option<Arc<Domain>>,
    /// The device, for event lines: `nvme0 (rid 0xc100)`.
    label: String,
}

/// The §4.10 line for an event of a device: what happened, where, and
/// which way (`nvme0 (rid 0xc100): translation fault at 0x1234000
/// (write)`).
#[must_use]
pub fn event_line(label: &str, e: &Event) -> String {
    match e.record.input() {
        Some(i) => alloc::format!(
            "{label}: {} at {:#x} ({})",
            e.record.describe(),
            i.addr.get(),
            if i.access.read { "read" } else { "write" }
        ),
        None => alloc::format!("{label}: {}", e.record.describe()),
    }
}

/// What a unit shares with its interrupt threads, its provider, and the
/// domains it made.
pub(crate) struct Shared<H: Hw> {
    unit_no: u32,
    unit: SpinLock<Option<Unit<H>>>,
    streams: SpinLock<BTreeMap<u32, StreamEvents>>,
    /// The contexts made, by stream.
    contexts: SpinLock<BTreeMap<u32, Ctx>>,
    /// Memory for the domains' page tables: `Some` when the unit
    /// translates.
    tables: Option<&'static dyn DmaMemory>,
    /// Itself, for the domains' way back to the unit.
    me: Weak<Self>,
}

impl<H: Hw + Send + Sync + 'static> Shared<H>
where
    H::Mem: Send,
{
    pub(crate) fn new(
        unit_no: u32,
        unit: Option<Unit<H>>,
        tables: Option<&'static dyn DmaMemory>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            unit_no,
            unit: SpinLock::new(unit),
            streams: SpinLock::new(BTreeMap::new()),
            contexts: SpinLock::new(BTreeMap::new()),
            tables,
            me: me.clone(),
        })
    }

    /// Whether contexts translate.
    fn translates(&self) -> bool {
        self.tables.is_some()
    }

    fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("smmu{}: {args}", self.unit_no));
    }

    /// Prints what the unit noted.
    fn report(&self, notices: Vec<Notice>) {
        for n in notices {
            match n {
                Notice::WasEnabled => {
                    self.log(format_args!("smmu_reset: Warning: SMMU is enabled"))
                }
                Notice::SyncTimeout => self.log(format_args!("Failed to sync")),
                Notice::EventOverflow => {
                    self.log(format_args!("event queue overflowed: events lost"));
                }
                Notice::CommandError {
                    error,
                    index,
                    words,
                } => {
                    self.log(format_args!("SMMU Global Error"));
                    let name = iommu::cmd::Command::decode(&words).map_or("unknown", |c| c.name());
                    self.log(format_args!(
                        "CMDQ_ERR: {error:?} on command {index} ({name}: {:#x} {:#x}), skipped",
                        words[0], words[1]
                    ));
                }
                Notice::GlobalError(bits) => {
                    self.log(format_args!("SMMU Global Error"));
                    let mut b = bits;
                    while b != 0 {
                        let bit = b.isolate_lowest_one();
                        self.log(format_args!("{}", gerror::name(bit)));
                        b &= !bit;
                    }
                }
            }
        }
    }

    /// Runs `f` on the unit while it is enabled, and reports what it
    /// noted; `None` if it is not running.
    fn with_unit<R>(&self, f: impl FnOnce(&mut Unit<H>) -> R) -> Option<R> {
        let (r, notices) = {
            let mut g = self.unit.lock();
            let u = g.as_mut()?;
            if u.state() != unit::State::Enabled {
                return None;
            }
            let r = f(u);
            (r, u.take_notices())
        };
        if !notices.is_empty() {
            self.report(notices);
        }
        Some(r)
    }

    /// The event interrupt's work: drains the queue and prints each event
    /// as FreeBSD's `smmu_print_event` does (the record's 32-bit words,
    /// then the stream's STE), at most [`EVENTS_PER_SECOND`] per stream,
    /// each led by the line naming the device whose context the stream is
    /// (IOMMU.md §4.10).
    fn drain_events(&self) {
        type Dump = (Option<u64>, Option<[u64; STE_WORDS]>);
        let mut seen: Vec<(Event, [u64; EVENT_WORDS], Dump)> = Vec::new();
        let notices = {
            let mut g = self.unit.lock();
            let Some(u) = g.as_mut() else {
                return;
            };
            let mut raw = Vec::new();
            u.drain_events(|e, w| raw.push((e, w)));
            for (e, w) in raw {
                let dump = u.ste_words(e.sid.0);
                seen.push((e, w, dump));
            }
            u.take_notices()
        };
        self.report(notices);
        let now = sched::now_ns();
        for (e, w, (l1, ste)) in seen {
            let (show, more) = {
                let mut s = self.streams.lock();
                let st = s.entry(e.sid.0).or_default();
                st.total += 1;
                st.last = Some(e);
                let mut more = 0;
                if now.wrapping_sub(st.window) >= 1_000_000_000 {
                    more = core::mem::take(&mut st.suppressed);
                    st.window = now;
                    st.logged = 0;
                }
                let show = st.logged < EVENTS_PER_SECOND;
                if show {
                    st.logged += 1;
                } else {
                    st.suppressed += 1;
                }
                (show, more)
            };
            if more != 0 {
                self.log(format_args!(
                    "SID {:x}: {more} more events not shown",
                    e.sid.0
                ));
            }
            if !show {
                continue;
            }
            let label = self.contexts.lock().get(&e.sid.0).map(|c| c.label.clone());
            if let Some(label) = label {
                self.log(format_args!("{}", event_line(&label, &e)));
            }
            let c = e.record.code();
            match code::freebsd(c) {
                Some((name, msg)) => self.log(format_args!("Event {name} ({msg}) received.")),
                None => self.log(format_args!("Event {c:#x} received")),
            }
            let words32: Vec<u32> = w
                .iter()
                .flat_map(|d| {
                    let [a, b, c, d2, e2, f, g, h] = d.to_le_bytes();
                    [
                        u32::from_le_bytes([a, b, c, d2]),
                        u32::from_le_bytes([e2, f, g, h]),
                    ]
                })
                .collect();
            self.log(format_args!("SID {:x}, Input Address: {:x}", e.sid.0, w[2]));
            for (i, x) in words32.iter().enumerate() {
                self.log(format_args!("evt[{i}] {x:x}"));
            }
            if let Some(l1) = l1 {
                self.log(format_args!("L1 ste == {l1:x}"));
            }
            if let Some(ste) = ste {
                for (i, x) in ste.iter().enumerate() {
                    self.log(format_args!("ste[{i}] == {x:x}"));
                }
            }
        }
    }

    /// The gerror interrupt's work.
    fn gerror(&self) {
        let notices = {
            let mut g = self.unit.lock();
            let Some(u) = g.as_mut() else {
                return;
            };
            u.handle_gerror();
            u.take_notices()
        };
        self.report(notices);
    }

    /// Gives `sid` the STE `ste`, reporting what the unit noted.
    fn set_stream(&self, sid: u32, ste: Ste) -> Result<(), unit::Error> {
        self.with_unit(|u| u.set_stream(sid, ste))
            .unwrap_or(Err(unit::Error::NoStream(sid)))
    }

    /// A domain for `stream`, its STE pointing at its descriptor
    /// (FreeBSD's `smmu_domain_alloc` and `smmu_ctx_init`): an ASID, an
    /// empty space with `req`'s ranges reserved, empty tables, and a CD.
    fn make_domain(
        &self,
        stream: u32,
        req: &bus::iommu::ContextRequest<'_>,
    ) -> Result<Arc<Domain>, unit::Error> {
        let mem = self.tables.ok_or(unit::Error::NoMemory)?;
        let (asid, features) = self
            .with_unit(|u| u.alloc_asid().map(|a| (a, *u.features())))
            .unwrap_or(Err(unit::Error::NoStream(stream)))?;
        let oas = features.idr5.oas;
        let me: Weak<dyn Invalidate> = self.me.clone();
        let made = Domain::new(
            mem,
            me,
            Params {
                asid: asid.0,
                reach: 1u64 << oas.bits(),
                walk_coherent: features.coherent,
                device_coherent: req.coherent,
            },
        )
        .map_err(|_| unit::Error::NoMemory)
        .and_then(|d| {
            for &(start, len) in req.reserved {
                d.reserve(start, start.saturating_add(len))
                    .map_err(|_| unit::Error::NoMemory)?;
            }
            let cd = ContextDescriptor::new(asid, PhysAddr::new(d.root()), oas, features.coherent);
            self.with_unit(|u| u.attach_stage1(stream, &cd))
                .unwrap_or(Err(unit::Error::NoStream(stream)))?;
            Ok(d)
        });
        if made.is_err() {
            let _ = self.with_unit(|u| u.free_asid(asid));
        }
        made
    }

    /// The context of `stream` (the provider's `context`).
    fn context(
        &self,
        stream: u32,
        req: &bus::iommu::ContextRequest<'_>,
    ) -> Result<bus::iommu::Context, unit::Error> {
        use bus::iommu::{Context, Want};
        let mut ctxs = self.contexts.lock();
        if let Some(c) = ctxs.get(&stream) {
            return Ok(match (&c.domain, req.want) {
                (Some(d), Want::Translate) => Context::Translated(Arc::clone(d) as _),
                _ => Context::PassThrough,
            });
        }
        // FreeBSD's disabled requesters (`hw.busdma.*=bounce`) and devices
        // that use physical addresses pass through.
        let translate = self.translates()
            && req.want == Want::Translate
            && !iommu_framework::requester_disabled(req.segment, req.rid);
        let domain = if translate {
            Some(self.make_domain(stream, req)?)
        } else {
            self.set_stream(stream, Ste::Bypass)?;
            None
        };
        ctxs.insert(
            stream,
            Ctx {
                domain: domain.clone(),
                label: alloc::format!("{} (rid {:#x})", req.name, req.rid),
            },
        );
        Ok(match domain {
            Some(d) => Context::Translated(d),
            None => Context::PassThrough,
        })
    }

    /// Ends `stream`'s context: its STE invalid (its DMA aborts again),
    /// its domain's ASID and descriptor freed. The domain itself goes when
    /// its last tag does.
    fn release(&self, stream: u32) -> Result<(), unit::Error> {
        let Some(c) = self.contexts.lock().remove(&stream) else {
            return Ok(());
        };
        self.set_stream(stream, Ste::Invalid)?;
        if let Some(d) = c.domain {
            self.with_unit(|u| u.free_asid(Asid(d.asid())))
                .unwrap_or(Ok(()))?;
        }
        Ok(())
    }
}

impl<H: Hw + Send + Sync + 'static> Invalidate for Shared<H>
where
    H::Mem: Send,
{
    fn invalidate(&self, asid: u16, iova: u64, len: u64) -> Result<(), Unconfirmed> {
        // A unit no longer running translates nothing.
        self.with_unit(|u| u.invalidate(Asid(asid), iova, len))
            .unwrap_or(Ok(()))
            .map_err(|_| Unconfirmed)
    }
}

/// The unit as the broker sees it.
pub(crate) struct Provider<H: Hw>(pub(crate) Arc<Shared<H>>);

impl<H: Hw + Send + Sync + 'static> bus::iommu::Iommu for Provider<H>
where
    H::Mem: Send,
{
    fn context(
        &self,
        stream: u32,
        req: &bus::iommu::ContextRequest<'_>,
    ) -> Result<bus::iommu::Context, bus::iommu::IommuError> {
        self.0.context(stream, req).map_err(unit::Error::for_bus)
    }
}

/// The running units, for [`set_stream`] and [`stream_events`].
static UNITS: SpinLock<Vec<Arc<Shared<Kernel>>>> = SpinLock::new(Vec::new());

fn running(unit: u32) -> Option<Arc<Shared<Kernel>>> {
    UNITS.lock().iter().find(|s| s.unit_no == unit).cloned()
}

/// What a stream's STE says, as [`set_stream`] sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamState {
    /// Invalid: traffic aborts with `C_BAD_STE`.
    Invalid,
    /// Config abort: traffic aborts without an event.
    Abort,
    /// Bypass: traffic reaches memory untranslated.
    Bypass,
    /// Stage 1: traffic is translated through a domain.
    Translated,
}

/// Whether `smmuN` is running (attached, translation on, enabled).
#[must_use]
pub fn is_running(unit: u32) -> bool {
    running(unit).is_some()
}

/// Whether `smmuN` gives translated contexts (`dev.smmu.N.dma`).
#[must_use]
pub fn translates(unit: u32) -> bool {
    running(unit).is_some_and(|s| s.translates())
}

/// Sets stream `sid` of `smmuN` (tests and diagnostics: the broker owns
/// streams of devices with contexts).
///
/// # Errors
///
/// [`unit::Error::NoStream`] if the unit is not running or the stream is
/// beyond its table, or the state is [`StreamState::Translated`] (only a
/// context translates); or the unit's.
pub fn set_stream(unit: u32, sid: u32, state: StreamState) -> Result<(), unit::Error> {
    let s = running(unit).ok_or(unit::Error::NoStream(sid))?;
    s.set_stream(
        sid,
        match state {
            StreamState::Invalid => Ste::Invalid,
            StreamState::Abort => Ste::Abort,
            StreamState::Bypass => Ste::Bypass,
            StreamState::Translated => return Err(unit::Error::NoStream(sid)),
        },
    )
}

/// What stream `sid` of `smmuN` does now; `None` if the unit is not
/// running.
#[must_use]
pub fn stream(unit: u32, sid: u32) -> Option<StreamState> {
    let s = running(unit)?;
    let g = s.unit.lock();
    Some(match g.as_ref()?.stream(sid) {
        Ste::Invalid => StreamState::Invalid,
        Ste::Abort => StreamState::Abort,
        Ste::Bypass => StreamState::Bypass,
        Ste::Stage1(_) => StreamState::Translated,
    })
}

/// The device whose context stream `sid` of `smmuN` is, as its event
/// lines name it (`nvme0 (rid 0x10)`).
#[must_use]
pub fn stream_device(unit: u32, sid: u32) -> Option<String> {
    running(unit)?
        .contexts
        .lock()
        .get(&sid)
        .map(|c| c.label.clone())
}

/// Ends stream `sid`'s context on `smmuN` (tests: a context otherwise
/// lives as long as its device's node).
///
/// # Errors
///
/// The unit's.
pub fn release_context(unit: u32, sid: u32) -> Result<(), unit::Error> {
    running(unit).map_or(Ok(()), |s| s.release(sid))
}

/// The events `smmuN` has drained for stream `sid`.
#[must_use]
pub fn stream_events(unit: u32, sid: u32) -> StreamEvents {
    running(unit)
        .and_then(|s| s.streams.lock().get(&sid).copied())
        .unwrap_or_default()
}
/// Why the unit does not translate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Off {
    /// It lacks a feature.
    Missing(Missing),
    /// `hw.iommu.dma` is 0.
    Disabled,
}

/// An `smmuN` instance.
pub struct Smmu {
    unit: u32,
    idr: Idr,
    features: Features,
    off: Option<Off>,
    /// The streams bypassed from the start, with their `bootverbose`
    /// lines; those beyond the table.
    bypassed: Vec<(u32, String)>,
    out_of_range: Vec<u32>,
    shared: Arc<Shared<Kernel>>,
    /// The registers when no unit holds them (translation off).
    _regs: Option<MmioGrant<Bytes>>,
    /// The event, sync, and gerror lines, until `start` installs the
    /// handlers; the sync line is kept without one, as on FreeBSD.
    lines: SpinLock<Vec<IrqLine>>,
}

impl core::fmt::Debug for Smmu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Smmu")
            .field("unit", &self.unit)
            .field("idr", &self.idr)
            .field("off", &self.off)
            .finish_non_exhaustive()
    }
}

fn filter(_: &Smmu, _: usize, _: &IrqCx) -> Filter {
    Filter::Schedule
}

fn event_thread(d: &Smmu, _: usize, _: &Sleepable) {
    d.shared.drain_events();
}

fn gerror_thread(d: &Smmu, _: usize, _: &Sleepable) {
    d.shared.gerror();
}

impl Driver for Smmu {
    const NAME: &'static str = "smmu";
    const IDS: Ids = Ids::Nexus(&["smmu"]);

    fn describe(_: &Ident) -> String {
        SMMU_DESC.into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let node = cx
            .bus_data::<SmmuNode>()
            .ok_or(AttachError::Decline)?
            .clone();
        // FreeBSD's probe: the generic model only.
        if node.smmu.model != SmmuV3::MODEL_GENERIC {
            return Err(AttachError::Decline);
        }
        let regs = cx.mmio_bytes(0)?;
        // Interrupt lines are event, PRI, sync, gerror; PRI is not used.
        let lines = alloc::vec![cx.irq(0)?, cx.irq(2)?, cx.irq(3)?];
        let idr = Idr::read(|o| regs.read32(at(o)));
        let features = idr.features(&node.smmu);
        let bypassed = bypassed_streams(&node, bypass_named());
        let off = match features.check() {
            Err(m) => Some(Off::Missing(m)),
            Ok(()) if !iommu_framework::dma() => Some(Off::Disabled),
            Ok(()) => None,
        };
        let mut out_of_range = Vec::new();
        let mut tables = None;
        let (unit, idle) = if off.is_some() {
            (None, Some(regs))
        } else {
            // The unit's structures: within its output size, cacheable
            // when it is coherent.
            let oas = u8::try_from(features.idr5.oas.bits()).unwrap_or(48);
            let tag = cx.dma_tag(DmaConstraints {
                addr_bits: oas,
                coherency: if features.coherent {
                    Coherency::Coherent
                } else {
                    Coherency::NonCoherent
                },
                ..DmaConstraints::UNLIMITED
            })?;
            // Page tables come from the same memory, within the unit's
            // reach; contexts translate under `hw.iommu.dma`, their MSIs
            // too (IOMMU.md §4.8).
            if iommu_framework::dma() {
                tables = Some(tag.memory());
            }
            let hw = Kernel { regs, tag };
            let mut u = Unit::new(hw, features).map_err(|_| AttachError::NoMemory)?;
            for &(sid, _) in &bypassed {
                match u.set_stream(sid, Ste::Bypass) {
                    Ok(()) => {}
                    Err(unit::Error::NoStream(_)) => out_of_range.push(sid),
                    Err(_) => return Err(AttachError::NoMemory),
                }
            }
            (Some(u), None)
        };
        Ok(Self {
            unit: cx.unit(),
            idr,
            features,
            off,
            bypassed,
            out_of_range,
            shared: Shared::new(cx.unit(), unit, tables),
            _regs: idle,
            lines: SpinLock::new(lines),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let sh = &self.shared;
        let verbose = bus::bootverbose();
        // The board port prints its count before the unit is reset.
        if !self.bypassed.is_empty() {
            sh.log(format_args!(
                "{} streams bypassed (reserved memory{})",
                self.bypassed.len(),
                if bypass_named() { ", ACPI devices" } else { "" }
            ));
        }
        if verbose {
            for (_, line) in &self.bypassed {
                sh.log(format_args!("{line}"));
            }
            self.idr.verbose(|l| sh.log(l));
            self.features.verbose(|t, v| match v {
                Some(v) => sh.log(format_args!("{t} {v}")),
                None => sh.log(format_args!("{t}")),
            });
        }
        for sid in &self.out_of_range {
            sh.log(format_args!("reserved stream {sid:#x} out of range"));
        }
        match self.off {
            Some(Off::Missing(m)) => {
                sh.log(format_args!("{}", m.describe()));
                sh.log(format_args!(
                    "Some features are required but not supported by hardware."
                ));
            }
            Some(Off::Disabled) => {
                sh.log(format_args!(
                    "left disabled: DMA translation is off (hw.iommu.dma)"
                ));
            }
            None => self.run(cx)?,
        }
        // `dev.smmu.N.dma`: whether this unit translates DMA.
        let unit = self.unit;
        let dma = i32::from(sh.translates());
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            if let Some(n) = bus::device_node(t, Self::NAME, unit) {
                let _ = t.add_value(n, Number::Auto, "dma", "DMA ops enabled", Value::int(dma));
            }
        });
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        UNITS.lock().retain(|s| !Arc::ptr_eq(s, &self.shared));
        let r = {
            let mut g = self.shared.unit.lock();
            g.as_mut().map(Unit::quiesce)
        };
        if let Some(Err(e)) = r {
            self.shared
                .log(format_args!("Could not disable SMMU ({e:?})."));
        }
    }

    fn may_detach(&self) -> Result<(), Busy> {
        Err(Busy)
    }
}

impl Smmu {
    /// Installs the handlers, resets and enables the unit, and publishes
    /// it.
    fn run(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let sh = &self.shared;
        if bus::bootverbose()
            && let Some(u) = sh.unit.lock().as_ref()
        {
            let (c, e) = u.queue_sizes();
            let g = u.geometry();
            let form = match g {
                Geometry::Linear { log2size } => alloc::format!("linear, {log2size} SID bits"),
                Geometry::TwoLevel { log2size, split } => {
                    alloc::format!("2-level, {log2size} SID bits, split {split}")
                }
            };
            sh.log(format_args!(
                "stream table {form}; CMD queue {c} bits, EVENT queue {e} bits; sync {:?}",
                u.sync_mode()
            ));
        }
        let mut lines = core::mem::take(&mut *self.lines.lock());
        // Event, sync, gerror, as claimed.
        if lines.len() == 3 {
            let gerr = lines.pop().ok_or(AttachError::Device("smmu: lines"))?;
            let sync = lines.pop().ok_or(AttachError::Device("smmu: lines"))?;
            let event = lines.pop().ok_or(AttachError::Device("smmu: lines"))?;
            cx.interrupt(event, Some(filter), Some(event_thread), 0)?;
            cx.interrupt(gerr, Some(filter), Some(gerror_thread), 0)?;
            self.lines.lock().push(sync);
        }
        let (r, notices) = {
            let mut g = sh.unit.lock();
            match g.as_mut() {
                Some(u) => (u.enable(), u.take_notices()),
                None => return Err(AttachError::Device("smmu: no unit")),
            }
        };
        sh.report(notices);
        if let Err(e) = r {
            sh.log(format_args!("Couldn't reset SMMU ({e:?})."));
            return Err(AttachError::Device("smmu: reset failed"));
        }
        cx.provide_iommu(Arc::new(Provider(Arc::clone(sh))) as Arc<dyn bus::iommu::Iommu>);

        UNITS.lock().push(Arc::clone(sh));
        // Anything raised before the handlers were in.
        sh.gerror();
        sh.drain_events();
        Ok(())
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Smmu>();
