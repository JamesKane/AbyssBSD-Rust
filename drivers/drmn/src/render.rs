// SPDX-License-Identifier: BSD-2-Clause
//! The `amdgpu` render interface (stage R4.5 to R4.7 of docs/GRAPHICS.md
//! §16, specification 09): what `libdrm_amdgpu` and Mesa's `radeonsi` ask of
//! the kernel. Each open file has its own GPU address space (a VMID and
//! four-level page tables in video memory); buffers are system pages
//! (on this APU the "video memory" is carved from system memory, so every
//! buffer is a system buffer the GPU reaches through snooped entries);
//! command submission puts a job on the graphics ring and a fence word says
//! how far the ring got.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::errno::Errno;
use amd_kms::Render as RenderApi;
use amdgpu::info::{self, Device, Live};
use amdgpu::pm4::Packets;
use amdgpu::uapi::{self, InfoRequest, cmd};
use amdgpu::vm::{Flags, MemoryType, Space, Tables};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use drm::IoctlCx;
use sync::{PreemptLock, SpinLock};
use vgapci::Aperture;

use crate::amd::Amd;
use crate::gcring::VramTables;

/// The first video memory offset of the page-table pools: one pool of 1 MiB
/// (256 tables) a VMID, VMIDs 1 to 7 (06 section 5).
pub(crate) const POOLS_AT: u64 = 176 << 20;
const POOL_TABLES: usize = 256;
/// A file's pool of tables, in bytes.
pub(crate) const POOL_BYTES: u64 = 1 << 20;
/// The VMIDs a graphics job may use.
const VMIDS: usize = 7;
/// The largest buffer: 1 GiB.
const MAX_BO: u64 = 1 << 30;
/// How long a submission waits for room in the ring.
const RING_WAIT_MS: u32 = 200;
/// `AMDGPU_VM_PAGE_*` of a mapping.
const VM_READABLE: u32 = 1 << 1;
const VM_WRITEABLE: u32 = 1 << 2;
const VM_EXECUTABLE: u32 = 1 << 3;
/// The system memory the GPU may map, as the device reports it (the
/// reference's figure for this machine is about 47 GiB; this is a floor).
const GTT_BYTES: u64 = 16 << 30;
/// The kernel's own VA reservation at the bottom of every space.
const VA_MIN: u64 = 0x10000;
const VA_MAX: u64 = 1 << 48;
/// The start of the top 4 MiB of the address space, which the device
/// information leaves to the kernel.
const KERNEL_VA: u64 = VA_MAX - (4 << 20);
/// The top 1 MiB of it: the pages of small buffers the kernel writes for a
/// job (the packet that writes a client's fence word), mapped from video
/// memory. The engine writes a client's fence word with the job's own address
/// space, so the packet runs from a buffer there, not in the ring (whose
/// packets use the kernel's).
const FENCE_IB_VA: u64 = VA_MAX - (1 << 20);
/// Pages of those a file has: a set for each engine (a job's page in its set
/// is its number modulo `FENCE_IB_SLOTS`).
const FENCE_IB_PAGES: u64 = 160;
const FENCE_IB_SLOTS: u64 = 16;
const FENCE_IB_SLOTS_U32: u32 = 16;
// The fence words' buffers come first, then the fence packet pages, inside
// the top 4 MiB the device information leaves to the kernel; a file's fence
// pages fit in its share of video memory and in the mapped range.
const _: () = assert!(KERNEL_VA < FENCE_IB_VA);
const _: () = assert!(FENCE_IB_VA + (1 << 20) == VA_MAX);
const _: () = assert!(KERNEL_VA + (4 << 20) == VA_MAX);
const _: () = assert!(FENCE_IB_PAGES * 4096 <= FENCE_IB_FILE_BYTES);
const _: () = assert!(FENCE_IB_PAGES * 4096 <= 1 << 20);
/// Where they are in video memory: 1 MiB for each file, from here.
pub(crate) const FENCE_IB_AT: u64 = 192 << 20;
/// What each file takes of it.
pub(crate) const FENCE_IB_FILE_BYTES: u64 = 1 << 20;

/// The engines a job runs on, each with its own ring and sequence numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    Gfx,
    Dma,
    /// Kernel compute queue `n` (0 to 7: the hardware queues). A submission
    /// names one of four slots; each context's slot is bound to one of the
    /// two queues behind it (slot `s`: queue `s` or `s + 4`).
    Compute(u8),
}

/// How long a job may go without the engine finishing anything (10 section
/// 1: two seconds) before the ring is reset.
const HANG_NS: u64 = 2_000_000_000;
/// How many engines have state of their own (graphics, DMA, the compute
/// queues).
const ENGINES: usize = 2 + COMPUTE_QUEUES;

/// How many compute slots are offered (09 section 3.2: the limit is four).
const COMPUTE_RINGS: u32 = 4;
/// The hardware queues behind them.
const COMPUTE_QUEUES: usize = 8;

/// The queue a context's slot is bound to when it first submits: of the two
/// behind the slot (`slot` and `slot + 4`) the one with fewer contexts bound,
/// the first on a tie. A context keeps its queue, so its fences stay in the
/// order of one ring. `queues` is how many answered their start-up test.
fn choose_queue(slot: u8, bound: &[u32; COMPUTE_QUEUES], queues: usize) -> u8 {
    let (a, b) = (slot, slot + 4);
    if usize::from(b) < queues && bound[usize::from(b)] < bound[usize::from(a)] {
        b
    } else {
        a
    }
}

/// A compute job's handle: the queue in the high half, the job's number on
/// that queue in the low half. A context's jobs are all on one queue, so
/// the handles a client compares (its fence word) only grow.
fn compute_handle(queue: u8, seq: u32) -> u64 {
    (u64::from(queue) << 32) | u64::from(seq)
}

impl Engine {
    /// The engine an indirect buffer or a wait names: the hardware IP type
    /// and ring.
    fn new(ip: u32, ring: u32) -> Option<Self> {
        match (ip, ring) {
            (uapi::ip::GFX, 0) => Some(Self::Gfx),
            (uapi::ip::DMA, 0) => Some(Self::Dma),
            (uapi::ip::COMPUTE, r) if r < COMPUTE_RINGS => u8::try_from(r).ok().map(Self::Compute),
            _ => None,
        }
    }

    /// Every engine the interface offers.
    fn all() -> impl Iterator<Item = Self> {
        [Self::Gfx, Self::Dma]
            .into_iter()
            .chain((0..COMPUTE_RINGS).filter_map(|r| u8::try_from(r).ok().map(Self::Compute)))
    }

    /// The engine's place in the tables of per-engine state: graphics, DMA,
    /// then the compute queues.
    fn index(self) -> usize {
        match self {
            Self::Gfx => 0,
            Self::Dma => 1,
            Self::Compute(q) => 2 + usize::from(q),
        }
    }

    /// Which set of fence-packet pages the engine's jobs use.
    fn page_set(self) -> u64 {
        match self {
            Self::Gfx => 0,
            Self::Dma => 1,
            Self::Compute(r) => 2 + u64::from(r),
        }
    }
}

/// Whether a driver request number is one of the user-mode queue requests.
fn is_user_queue(nr: u8) -> bool {
    matches!(nr, cmd::USERQ | cmd::USERQ_SIGNAL | cmd::USERQ_WAIT)
}

/// The page-table flags of a mapping request's `AMDGPU_VM_PAGE_*` bits: every
/// buffer is system memory, snooped.
fn gpu_flags(request: u32) -> Flags {
    Flags {
        readable: request & VM_READABLE != 0,
        writeable: request & VM_WRITEABLE != 0,
        executable: request & VM_EXECUTABLE != 0,
        system: true,
        snooped: true,
        memory_type: MemoryType::Nc,
    }
}

/// The address as the engine's packets carry it: the high half of the 48-bit
/// space (bit 47 set) has the top 16 bits set too; the hardware refuses an
/// address that does not repeat bit 47 there.
fn extended(va: u64) -> u64 {
    if va & (1 << 47) != 0 {
        va | (0xffff << 48)
    } else {
        va
    }
}

/// The 48-bit address a client's address means: the translation uses the
/// low 48 bits, and the top 16 must repeat bit 47 (the high half of the space
/// is written with them set).
fn canonical(va: u64) -> Option<u64> {
    match va >> 47 {
        0 | 0x1_ffff => Some(va & (VA_MAX - 1)),
        _ => None,
    }
}

/// A buffer mapped into a file's address space.
struct Mapping {
    va: u64,
    pages: u64,
    gem: Arc<drm::gem::Gem>,
}

/// What one open file has.
struct FileState {
    /// Which pool (and VMID `slot + 1`).
    slot: usize,
    space: Space<VramTables>,
    maps: Vec<Mapping>,
    /// The contexts, with how many ring resets had happened when each was
    /// made.
    contexts: BTreeMap<u32, u32>,
    /// The compute queue each (context, slot) is bound to.
    queues: BTreeMap<(u32, u8), u8>,
    /// The buffer lists: an id and the buffer handles in it.
    lists: BTreeMap<u32, Vec<u32>>,
    /// The metadata a client keeps with a buffer, by handle.
    metadata: BTreeMap<u32, Vec<u8>>,
    /// Where the next buffer the kernel maps for itself (a fence word) goes,
    /// in the top 4 MiB the device information reserves.
    kernel_next: u64,
    /// Whether its fence-packet pages are mapped.
    fence_pages: bool,
}

/// Numbers the interface hands out and counts.
struct Counters {
    /// The next context or list id.
    next_id: u32,
    /// Bytes of buffers made and not freed.
    bytes: u64,
    /// How many contexts are bound to each compute queue.
    bound: [u32; COMPUTE_QUEUES],
}

struct State {
    files: BTreeMap<u64, FileState>,
    slots: [bool; VMIDS],
    count: Counters,
}

/// The render interface of one card.
pub(crate) struct Render {
    amd: Weak<Amd>,
    state: PreemptLock<State>,
    /// The last job's sequence number.
    seq: AtomicU32,
    /// The same for the DMA engine's jobs.
    dma_seq: AtomicU32,
    /// And the compute rings'.
    comp_seq: [AtomicU32; COMPUTE_QUEUES],
    /// The context of the last job (a job from another one loads the
    /// context's state).
    last_ctx: AtomicU32,
    /// Where waiters for a job sleep; the fence interrupt wakes it.
    queue: Arc<sched::WaitQueue>,
    /// Counts the fence interrupts' wakes (the notifier thread sleeps until
    /// it moves).
    kick: Arc<AtomicU32>,
    /// Sync files waiting for a job to finish, and whether the thread that
    /// wakes them is running.
    fence_wakers: SpinLock<Vec<Arc<dyn vfs::event::Wake>>>,
    notifier_running: core::sync::atomic::AtomicBool,
    /// Submissions, how many named a buffer list, and how many carried the
    /// handles inline (`BO_HANDLES`).
    cs_count: AtomicU32,
    cs_with_list: AtomicU32,
    cs_with_handles: AtomicU32,
    /// Deadline hints from waiters, and the last one's time.
    deadline_hints: AtomicU32,
    latest_deadline: core::sync::atomic::AtomicU64,
    /// The engines (bit per [`Engine::index`]) that a waiting fence is on:
    /// the only ones the notifier looks for a hung ring on.
    waited_engines: AtomicU32,
    /// Itself, for the fences it hands out.
    me: Weak<Render>,
    /// For each engine, the number of the last job seen finished and when it
    /// was first seen so (the time a job has gone without progress).
    watch: SpinLock<[Option<(u32, u64)>; ENGINES]>,
    /// The context that made the last job of each engine.
    last_ctx_of: SpinLock<[u32; ENGINES]>,
    /// How many times a ring was reset, and the contexts that were on a ring
    /// when it was.
    resets: AtomicU32,
    guilty: SpinLock<Vec<u32>>,
    /// The pool of video memory for buffers, and how many were made there,
    /// how many asked for it, and how many of those could be shared.
    vram: Arc<crate::vram::VramPool>,
    vram_made: AtomicU64,
    vram_asked: AtomicU64,
    vram_asked_shared: AtomicU64,
    /// What buffer requests asked: all, with the write-combined flag, local
    /// ones (the always-valid flag), and bytes of those.
    asks: AtomicU64,
    asks_wc: AtomicU64,
    asks_local: AtomicU64,
    asks_wc_bytes: AtomicU64,
}

impl core::fmt::Debug for Render {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Render")
    }
}

impl Render {
    pub(crate) fn new(amd: Weak<Amd>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            watch: SpinLock::new([None; ENGINES]),
            last_ctx_of: SpinLock::new([0; ENGINES]),
            resets: AtomicU32::new(0),
            guilty: SpinLock::new(Vec::new()),
            amd,
            seq: AtomicU32::new(0),
            dma_seq: AtomicU32::new(0),
            comp_seq: [const { AtomicU32::new(0) }; COMPUTE_QUEUES],
            last_ctx: AtomicU32::new(0),
            queue: Arc::new(sched::WaitQueue::new()),
            kick: Arc::new(AtomicU32::new(0)),
            fence_wakers: SpinLock::new(Vec::new()),
            notifier_running: core::sync::atomic::AtomicBool::new(false),
            waited_engines: AtomicU32::new(0),
            cs_count: AtomicU32::new(0),
            cs_with_list: AtomicU32::new(0),
            cs_with_handles: AtomicU32::new(0),
            deadline_hints: AtomicU32::new(0),
            latest_deadline: core::sync::atomic::AtomicU64::new(0),
            state: PreemptLock::new(State {
                files: BTreeMap::new(),
                slots: [false; VMIDS],
                count: Counters {
                    next_id: 1,
                    bytes: 0,
                    bound: [0; COMPUTE_QUEUES],
                },
            }),
            vram: crate::vram::VramPool::new(crate::vram::POOL_START, crate::vram::POOL_END),
            vram_made: AtomicU64::new(0),
            vram_asked: AtomicU64::new(0),
            vram_asked_shared: AtomicU64::new(0),
            asks: AtomicU64::new(0),
            asks_wc: AtomicU64::new(0),
            asks_local: AtomicU64::new(0),
            asks_wc_bytes: AtomicU64::new(0),
        })
    }

    /// What the submissions looked like, for the `render` sysctl.
    pub(crate) fn summary(&self) -> alloc::string::String {
        alloc::format!(
            "{} submissions, {} with a buffer list, {} with inline buffer handles, {} ring resets, {} deadline hints; \
             buffer requests: {} ({} write-combined, {} MiB of them; {} local); {} asked for video memory ({} that could be shared), {} made there, {} KiB of {} KiB in use",
            self.cs_count.load(Ordering::Relaxed),
            self.cs_with_list.load(Ordering::Relaxed),
            self.cs_with_handles.load(Ordering::Relaxed),
            self.resets.load(Ordering::Relaxed),
            self.deadline_hints.load(Ordering::Relaxed),
            self.asks.load(Ordering::Relaxed),
            self.asks_wc.load(Ordering::Relaxed),
            self.asks_wc_bytes.load(Ordering::Relaxed) >> 20,
            self.asks_local.load(Ordering::Relaxed),
            self.vram_asked.load(Ordering::Relaxed),
            self.vram_asked_shared.load(Ordering::Relaxed),
            self.vram_made.load(Ordering::Relaxed),
            self.vram.used() >> 10,
            self.vram.total() >> 10,
        )
    }

    /// The counter the fence interrupt bumps.
    pub(crate) fn kick(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.kick)
    }

    /// Takes `w` to be woken when a job may have finished (a sync file
    /// waits this way), and starts the thread that does it if it is not
    /// running. A job's fence interrupt wakes the thread at once; every 50
    /// ms it also wakes them anyway and looks for a hung ring, so a lost
    /// interrupt costs latency, not a wait for ever.
    fn add_waker(&self, w: &Arc<dyn vfs::event::Wake>, engine: Engine) {
        use core::sync::atomic::Ordering::{AcqRel, Acquire, Release};
        self.waited_engines
            .fetch_or(1 << engine.index(), Ordering::AcqRel);
        self.fence_wakers.lock().push(Arc::clone(w));
        if self.notifier_running.swap(true, AcqRel) {
            return;
        }
        let me = self.me.clone();
        let queue = Arc::clone(&self.queue);
        let kick = Arc::clone(&self.kick);
        let spawned = sched::spawn("amd-fence", sched::Priority::KERNEL, move |s| {
            let mut seen = kick.load(Acquire);
            let mut idle = 0u32;
            loop {
                let _ = bus::manager().wait_timeout(
                    s,
                    &queue,
                    &mut || kick.load(Acquire) != seen,
                    50_000_000,
                );
                seen = kick.load(Acquire);
                let Some(r) = me.upgrade() else {
                    return;
                };
                let woken: Vec<_> = core::mem::take(&mut *r.fence_wakers.lock());
                if woken.is_empty() {
                    idle += 1;
                    if idle >= 20 {
                        // Idle for a second: stop, unless one came in as we
                        // decided to.
                        r.notifier_running.store(false, Release);
                        if r.fence_wakers.lock().is_empty() || r.notifier_running.swap(true, AcqRel)
                        {
                            return;
                        }
                        idle = 0;
                    }
                    continue;
                }
                idle = 0;
                if let Some(amd) = r.amd.upgrade() {
                    // A job that never finishes is found here too, on the
                    // engines something waits for (an idle engine's last job
                    // is not a hang).
                    let mask = r.waited_engines.swap(0, AcqRel);
                    for e in Engine::all().filter(|e| mask & (1 << e.index()) != 0) {
                        r.hang_check(&amd, e);
                    }
                }
                for w in woken {
                    w.wake();
                }
            }
        });
        if spawned.is_err() {
            self.notifier_running.store(false, Release);
        }
    }

    /// The queue the fence interrupt wakes.
    pub(crate) fn queue(&self) -> Arc<sched::WaitQueue> {
        Arc::clone(&self.queue)
    }

    /// After a chip reset built the rings again: every job up to the last of
    /// each engine is completed (its fence word is written), every context
    /// reads that a reset happened, and waiters wake.
    pub(crate) fn after_reset(&self, amd: &Amd) {
        let Ok(gpu) = amd.gpu_arc() else {
            return;
        };
        if let Some(g) = amd.gfx_lock().as_ref() {
            g.complete_gfx(&gpu, self.last_seq(Engine::Gfx));
            for q in 0..COMPUTE_QUEUES {
                g.complete_compute(
                    &gpu,
                    q,
                    self.last_seq(Engine::Compute(u8::try_from(q).unwrap_or(0))),
                );
            }
        }
        if let Some(d) = amd.sdma_lock().as_ref() {
            d.complete(&gpu, self.last_seq(Engine::Dma));
        }
        self.resets.fetch_add(1, Ordering::AcqRel);
        *self.watch.lock() = [None; ENGINES];
        self.queue.wake_all();
    }

    /// Whether `engine` has a job that has made no progress for two seconds;
    /// if so the ring is reset (10 section 4: the queue's own reset) and the
    /// jobs it held are completed. Returns whether it reset one. Called by
    /// whoever waits for a job, and by a submission that finds its ring full.
    fn hang_check(&self, amd: &Amd, engine: Engine) -> bool {
        let Ok(gpu) = amd.gpu_arc() else {
            return false;
        };
        let last = self.last_seq(engine);
        let now = sched::now_ns();
        let finished = match engine {
            Engine::Gfx => amd.gfx_lock().as_ref().map(|g| g.finished(&gpu)),
            Engine::Dma => amd.sdma_lock().as_ref().map(|d| d.finished(&gpu)),
            Engine::Compute(q) => amd
                .gfx_lock()
                .as_ref()
                .map(|g| g.compute_finished(&gpu, usize::from(q))),
        };
        let Some(finished) = finished else {
            return false;
        };
        let i = engine.index();
        {
            let mut w = self.watch.lock();
            // Idle, or progressing: start the clock again from now.
            if finished == last || w[i].is_none_or(|(f, _)| f != finished) {
                w[i] = Some((finished, now));
                return false;
            }
            if w[i].is_some_and(|(_, since)| now.saturating_sub(since) < HANG_NS) {
                return false;
            }
            w[i] = None;
        }
        self.recover(amd, &gpu, engine, last)
    }

    /// Resets `engine`'s ring and completes its jobs up to `last`.
    fn recover(&self, amd: &Amd, gpu: &Arc<Aperture>, engine: Engine, last: u32) -> bool {
        let log = |args: core::fmt::Arguments<'_>| amd.log(args);
        log(format_args!(
            "render: {engine:?}: job {last} did not finish in two seconds; resetting the ring"
        ));
        let done = match engine {
            Engine::Gfx => {
                let mut g = amd.gfx_lock();
                g.as_mut().is_some_and(|g| {
                    let ok = g.reset_gfx(gpu, &log).is_ok();
                    g.complete_gfx(gpu, last);
                    ok
                })
            }
            Engine::Dma => {
                let mut g = amd.sdma_lock();
                g.as_mut().is_some_and(|d| {
                    let ok = d.reset(gpu, &log).is_ok();
                    d.complete(gpu, last);
                    ok
                })
            }
            Engine::Compute(q) => {
                let mut g = amd.gfx_lock();
                g.as_mut().is_some_and(|g| {
                    let ok = g.reset_compute(gpu, usize::from(q), &log).is_ok();
                    g.complete_compute(gpu, usize::from(q), last);
                    ok
                })
            }
        };
        // The context that made the last job is the guilty one; every
        // context sees that a reset happened.
        let ctx = self.last_ctx_of.lock()[engine.index()];
        self.guilty.lock().push(ctx);
        self.resets.fetch_add(1, Ordering::AcqRel);
        self.queue.wake_all();
        log(format_args!(
            "render: {engine:?}: ring {}; jobs up to {last} completed",
            if done { "reset" } else { "reset FAILED" }
        ));
        done
    }

    /// Sleeps until job `seq` has finished or `ns` nanoseconds have passed
    /// (the fence interrupt ends it early; without interrupts it is a poll
    /// at this period).
    fn sleep_for(&self, s: &sched::Sleepable, amd: &Amd, engine: Engine, seq: u32, ns: u64) {
        let _ = bus::manager().wait_timeout(
            s,
            &self.queue,
            &mut || self.done(amd, engine, seq).unwrap_or(true),
            ns,
        );
    }

    fn amd(&self) -> Result<Arc<Amd>, Errno> {
        self.amd.upgrade().ok_or(Errno::ENODEV)
    }

    /// The file's state, made on first use: a free VMID and an address space
    /// in its pool.
    fn with_file<R>(
        &self,
        amd: &Amd,
        file: u64,
        f: impl FnOnce(&mut FileState, &mut Counters) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        let gpu = amd.gpu_arc()?;
        let phys = amd.vram_phys()?;
        let mut st = self.state.lock();
        if !st.files.contains_key(&file) {
            let slot = st.slots.iter().position(|u| !*u).ok_or(Errno::EMFILE)?;
            let tables = VramTables::new(
                Arc::clone(&gpu),
                phys,
                POOLS_AT + slot as u64 * POOL_BYTES,
                POOL_TABLES,
            );
            let space = Space::new(tables).map_err(|_| Errno::ENOMEM)?;
            st.slots[slot] = true;
            st.files.insert(
                file,
                FileState {
                    slot,
                    space,
                    maps: Vec::new(),
                    contexts: BTreeMap::new(),
                    queues: BTreeMap::new(),
                    lists: BTreeMap::new(),
                    metadata: BTreeMap::new(),
                    kernel_next: KERNEL_VA,
                    fence_pages: false,
                },
            );
        }
        let State { files, count, .. } = &mut *st;
        let fs = files.get_mut(&file).ok_or(Errno::ENODEV)?;
        f(fs, count)
    }

    fn info(&self, amd: &Amd, cx: &IoctlCx<'_>, arg: &[u8]) -> Result<usize, Errno> {
        let req = InfoRequest::decode(arg).map_err(|_| Errno::EINVAL)?;
        let gpu = amd.gpu_arc()?;
        let bytes = self.state.lock().count.bytes;
        let vram_used = self.vram.used();
        let live = LiveState {
            gpu,
            vram_used,
            gtt_used: bytes.saturating_sub(vram_used),
        };
        // The graphics ring always; the DMA engine when it started; compute
        // is not driven yet.
        let device = Device::raphael(self.vram.total(), GTT_BYTES);
        let device = match (amd.sdma_ready(), amd.compute_rings() > 0) {
            (true, true) => device,
            (true, false) => device.without_compute(),
            (false, true) => device.without_dma(),
            (false, false) => device.graphics_only(),
        };
        let answer = info::answer(&device, &live, &req).map_err(|_| Errno::EINVAL)?;
        let out = uapi::truncated(&answer, req.return_size);
        cx.copy_out(req.return_pointer, out)?;
        Ok(0)
    }

    fn gem_create(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let r = uapi::GemCreate::decode(arg).map_err(|_| Errno::EINVAL)?;
        if r.size == 0 || r.size > MAX_BO {
            return Err(Errno::EINVAL);
        }
        let size = r.size.checked_next_multiple_of(4096).ok_or(Errno::EINVAL)?;
        let amd = self.amd()?;
        // A buffer that is not shared may live in the device's own memory,
        // which the engines read faster than system memory behind the GART:
        // when the client asks for it and the pool has a run that long;
        // otherwise (and for every shareable buffer) it is system memory.
        self.asks.fetch_add(1, Ordering::Relaxed);
        if r.flags & 4 != 0 {
            self.asks_wc.fetch_add(1, Ordering::Relaxed);
            self.asks_wc_bytes.fetch_add(size, Ordering::Relaxed);
        }
        if r.flags & uapi::gem_flag::VM_ALWAYS_VALID != 0 {
            self.asks_local.fetch_add(1, Ordering::Relaxed);
        }
        let asked = r.domains & uapi::domain::VRAM != 0;
        let local = r.flags & uapi::gem_flag::VM_ALWAYS_VALID != 0;
        if asked {
            self.vram_asked.fetch_add(1, Ordering::Relaxed);
            if !local {
                self.vram_asked_shared.fetch_add(1, Ordering::Relaxed);
            }
        }
        // (Only where the processor's window onto video memory reaches.)
        let window = amd.gpu_arc()?.vram_len().unwrap_or(0) as u64;
        let block = if asked && local && size <= self.vram.total() / 4 {
            self.vram
                .alloc(size)
                .filter(|b| b.offset() + size <= window)
        } else {
            None
        };
        let (handle, gem) = match block {
            Some(block) => {
                let gpu = amd.gpu_arc()?;
                let bar = gpu.vram_phys().ok_or(Errno::ENXIO)?;
                // What an earlier buffer left there must not be read.
                for at in (block.offset()..block.offset() + size).step_by(8) {
                    gpu.vram_write64(usize::try_from(at).unwrap_or(0), 0);
                }
                let made = cx.new_device_gem(size, vm::Pfn((bar + block.offset()) >> 12), block)?;
                self.vram_made.fetch_add(1, Ordering::Relaxed);
                made
            }
            // Write-combined when the client says it only writes the buffer
            // from the processor (what Mesa does for most): the engines read
            // such memory without snooping the processor's caches.
            None if r.flags & uapi::gem_flag::CPU_GTT_USWC != 0 => cx.new_gem(
                size,
                drm::gem::Placement {
                    cache: drm::gem::CacheMode::WriteCombine,
                    ..drm::gem::Placement::SYSTEM_COHERENT
                },
            )?,
            None => cx.new_gem(size, drm::gem::Placement::SYSTEM_COHERENT)?,
        };
        self.with_file(&amd, cx.file_id(), |_, c| {
            c.bytes += gem.size();
            Ok(())
        })?;
        uapi::GemCreate::answer(arg, handle);
        Ok(0)
    }

    fn gem_va(&self, cx: &IoctlCx<'_>, arg: &[u8]) -> Result<usize, Errno> {
        let r = uapi::GemVa::decode(arg).map_err(|_| Errno::EINVAL)?;
        let amd = self.amd()?;
        let page = 4096u64;
        if r.va_address % page != 0 || r.map_size % page != 0 || r.map_size == 0 {
            return Err(Errno::EINVAL);
        }
        let mut r = r;
        r.va_address = canonical(r.va_address).ok_or(Errno::EINVAL)?;
        let end = r.va_address.checked_add(r.map_size).ok_or(Errno::EINVAL)?;
        if r.va_address < VA_MIN || end > VA_MAX {
            return Err(Errno::EINVAL);
        }
        let pages = r.map_size / page;
        match r.operation {
            uapi::va_op::MAP => {
                if r.offset_in_bo % page != 0 {
                    return Err(Errno::EINVAL);
                }
                let gem = cx.gem(r.handle).ok_or(Errno::ENOENT)?;
                let off_end = r
                    .offset_in_bo
                    .checked_add(r.map_size)
                    .ok_or(Errno::EINVAL)?;
                if off_end > gem.size() {
                    return Err(Errno::EINVAL);
                }
                // The pages, populated now (before any lock is held).
                let frames = gem.frames()?;
                let first = usize::try_from(r.offset_in_bo / page).map_err(|_| Errno::EINVAL)?;
                let mut flags = gpu_flags(r.flags);
                // A buffer in the device's own memory: the engines address
                // it as video memory (the processor's window onto it, taken
                // away and the start of video memory in the GPU's space
                // put in its place).
                // Write-combined system memory is read without snooping.
                if gem.placement().cache == drm::gem::CacheMode::WriteCombine {
                    flags.snooped = false;
                }
                let base = if gem.is_device() {
                    let gpu = amd.gpu_arc()?;
                    flags.system = false;
                    flags.snooped = false;
                    let window = gpu.vram_phys().ok_or(Errno::ENXIO)?;
                    Some((amd.vram_phys()?, window))
                } else {
                    None
                };
                self.with_file(&amd, cx.file_id(), |fs, _| {
                    fs.space
                        .map(r.va_address, pages, flags, |n| {
                            let at = frames
                                .get(first + usize::try_from(n).unwrap_or(0))
                                .map_or(0, |p| p.0 * page);
                            match base {
                                Some((gpu_base, window)) => gpu_base + (at - window),
                                None => at,
                            }
                        })
                        .map_err(|e| match e {
                            amdgpu::vm::Error::Busy => Errno::EBUSY,
                            amdgpu::vm::Error::NoMemory => Errno::ENOMEM,
                            amdgpu::vm::Error::Range => Errno::EINVAL,
                        })?;
                    fs.maps.push(Mapping {
                        va: r.va_address,
                        pages,
                        gem,
                    });
                    Ok(())
                })?;
                Ok(0)
            }
            uapi::va_op::UNMAP => self.with_file(&amd, cx.file_id(), |fs, _| {
                let at = fs
                    .maps
                    .iter()
                    .position(|m| m.va == r.va_address && m.pages == pages)
                    .ok_or(Errno::EINVAL)?;
                fs.space
                    .unmap(r.va_address, pages)
                    .map_err(|_| Errno::EINVAL)?;
                fs.maps.remove(at);
                Ok(0)
            }),
            _ => Err(Errno::EINVAL),
        }
    }

    /// A submission; a refusal is logged with what was asked.
    fn cs(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let asked = uapi::CsRequest::decode(arg).ok();
        let r = self.cs_run(cx, arg);
        if let (Err(e), Some(q), Ok(amd)) = (&r, asked, self.amd()) {
            amd.log(format_args!(
                "render: submission refused ({e:?}): context {}, {} chunks, flags {:#x}",
                q.ctx_id, q.num_chunks, q.flags
            ));
            for i in 0..u64::from(q.num_chunks.min(8)) {
                let mut p = [0u8; 8];
                let mut h = [0u8; 16];
                if cx.copy_in(&mut p, q.chunks + 8 * i).is_ok()
                    && cx.copy_in(&mut h, u64::from_le_bytes(p)).is_ok()
                {
                    let mut body = [0u8; 40];
                    let n = (u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize * 4).min(40);
                    let _ = cx.copy_in(
                        &mut body[..n],
                        u64::from_le_bytes(h[8..16].try_into().unwrap_or([0; 8])),
                    );
                    amd.log(format_args!(
                        "render:   chunk {i}: id {:#x} length {} dwords, data {:02x?}",
                        u32::from_le_bytes([h[0], h[1], h[2], h[3]]),
                        n / 4,
                        &body[..n]
                    ));
                }
            }
        }
        r
    }

    fn cs_run(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let r = uapi::CsRequest::decode(arg).map_err(|_| Errno::EINVAL)?;
        if r.num_chunks == 0 || r.num_chunks > 64 {
            return Err(Errno::EINVAL);
        }
        // The chunks, read from the caller's memory before any lock.
        let ctx_id = r.ctx_id;
        self.cs_count.fetch_add(1, Ordering::Relaxed);
        if r.bo_list != 0 {
            self.cs_with_list.fetch_add(1, Ordering::Relaxed);
        }
        let mut ibs = Vec::new();
        let mut engine: Option<Engine> = None;
        let mut fence: Option<(u32, u32)> = None;
        let mut inline_bos: Option<Vec<u32>> = None;
        let mut out_syncobjs: Vec<(u32, u64)> = Vec::new();
        // What the job waits for before it runs: objects (at points).
        let mut in_syncobjs: Vec<(u32, u64)> = Vec::new();
        for i in 0..u64::from(r.num_chunks) {
            let mut p = [0u8; 8];
            cx.copy_in(&mut p, r.chunks + 8 * i)?;
            let mut h = [0u8; 16];
            cx.copy_in(&mut h, u64::from_le_bytes(p))?;
            let c = uapi::Chunk::decode(&h).map_err(|_| Errno::EINVAL)?;
            // The lists of buffers and the like are not needed: only the
            // chunks read below are fetched.
            if c.id == uapi::chunk::BO_HANDLES {
                self.cs_with_handles.fetch_add(1, Ordering::Relaxed);
            }
            if !matches!(
                c.id,
                uapi::chunk::IB
                    | uapi::chunk::BO_HANDLES
                    | uapi::chunk::FENCE
                    | uapi::chunk::SYNCOBJ_OUT
                    | uapi::chunk::SYNCOBJ_IN
                    | uapi::chunk::SYNCOBJ_TIMELINE_WAIT
                    | uapi::chunk::SYNCOBJ_TIMELINE_SIGNAL
            ) {
                continue;
            }
            let len = usize::try_from(c.length_dw).map_err(|_| Errno::EINVAL)? * 4;
            if len > 4096 {
                return Err(Errno::EINVAL);
            }
            let mut data = alloc::vec![0u8; len];
            cx.copy_in(&mut data, c.data)?;
            match c.id {
                uapi::chunk::IB => {
                    let mut ib = uapi::IbChunk::decode(&data).map_err(|_| Errno::EINVAL)?;
                    ib.va_start = canonical(ib.va_start).ok_or(Errno::EINVAL)?;
                    // One engine a submission.
                    let this = Engine::new(ib.ip_type, ib.ring).ok_or(Errno::EINVAL)?;
                    if ib.ip_instance != 0 || engine.is_some_and(|e| e != this) {
                        return Err(Errno::EINVAL);
                    }
                    engine = Some(this);
                    ibs.push(ib);
                }
                uapi::chunk::FENCE if len >= 8 => {
                    let handle = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    let offset = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                    fence = Some((handle, offset));
                }
                // The synchronisation objects to give this job's fence to.
                uapi::chunk::SYNCOBJ_OUT => {
                    out_syncobjs.extend(
                        data.as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| (u32::from_le_bytes(*c), 0)),
                    );
                }
                // The buffers inline (the same request as a list's creation):
                // used for this job instead of a list's.
                uapi::chunk::BO_HANDLES => {
                    let req = uapi::BoListRequest::decode(&data).map_err(|_| Errno::EINVAL)?;
                    inline_bos = Some(bo_list_handles(cx, &req)?);
                }
                // Objects waited on, and objects (with points) to signal.
                uapi::chunk::SYNCOBJ_IN => {
                    in_syncobjs.extend(
                        data.as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| (u32::from_le_bytes(*c), 0)),
                    );
                }
                uapi::chunk::SYNCOBJ_TIMELINE_WAIT => in_syncobjs.extend(timeline_entries(&data)),
                uapi::chunk::SYNCOBJ_TIMELINE_SIGNAL => {
                    out_syncobjs.extend(timeline_entries(&data))
                }
                // Dependencies on other jobs of this driver: every job is on
                // one ring, in order; the buffer handles list is not needed
                // (every mapping stays resident).
                _ => {}
            }
        }
        let mut engine = engine.ok_or(Errno::EINVAL)?;
        if ibs.is_empty() {
            return Err(Errno::EINVAL);
        }
        let user_fence_gem = fence.and_then(|(h, o)| cx.gem(h).map(|g| (g, u64::from(o))));
        // The buffers of the job's list: what it waits for from outside,
        // and what its fence goes on.
        let list_amd = self.amd()?;
        let bos: Vec<Arc<drm::gem::Gem>> = match inline_bos {
            Some(handles) => handles,
            None => self.with_file(&list_amd, cx.file_id(), |fs, _| {
                Ok(fs.lists.get(&r.bo_list).cloned().unwrap_or_default())
            })?,
        }
        .into_iter()
        .filter_map(|h| cx.gem(h))
        .collect();
        // A point that does not follow the last is refused before the job
        // runs, not after.
        for (h, point) in &out_syncobjs {
            let slot = cx.syncobj(*h).ok_or(Errno::ENOENT)?;
            if *point != 0 && slot.query(true) >= *point {
                return Err(Errno::EINVAL);
            }
        }
        let amd = self.amd()?;
        match engine {
            Engine::Dma if !amd.sdma_ready() => return Err(Errno::ENODEV),
            Engine::Compute(r) if usize::from(r) >= amd.compute_rings() => {
                return Err(Errno::ENODEV);
            }
            _ => {}
        }
        // A compute slot: the queue this context uses for it.
        if let Engine::Compute(slot) = engine {
            let queues = amd.compute_rings();
            let hw = self.with_file(&amd, cx.file_id(), |fs, c| {
                if let Some(q) = fs.queues.get(&(ctx_id, slot)) {
                    return Ok(*q);
                }
                let q = choose_queue(slot, &c.bound, queues);
                c.bound[usize::from(q)] += 1;
                fs.queues.insert((ctx_id, slot), q);
                Ok(q)
            })?;
            engine = Engine::Compute(hw);
        }
        // What the job waits for before it runs: its objects, and the fences
        // on its buffers from other rings and from outside (its own ring
        // orders its own jobs).
        wait_dependencies(cx, &in_syncobjs, &bos, engine)?;
        let gpu = amd.gpu_arc()?;
        // The address space's root and the user fence's address, under the
        // state lock; the job itself under the ring's.
        let fence_frames = match &user_fence_gem {
            // The fence word's buffer is written by the engine through the
            // job's address space as system memory.
            Some((gem, _)) if gem.is_device() => return Err(Errno::EINVAL),
            Some((gem, _)) => Some(gem.frames()?),
            None => None,
        };
        let phys = amd.vram_phys()?;
        let (vmid, slot, root, user_fence) = self.with_file(&amd, cx.file_id(), |fs, _| {
            let vmid = u32::try_from(fs.slot).unwrap_or(0) + 1;
            let root = fs.space.tables().address(fs.space.root()).0;
            if !fs.fence_pages {
                let at = phys + FENCE_IB_AT + (fs.slot as u64 * FENCE_IB_FILE_BYTES);
                fs.space
                    .map(
                        FENCE_IB_VA,
                        FENCE_IB_PAGES,
                        Flags {
                            executable: true,
                            ..Flags::VRAM_RW
                        },
                        |n| at + n * 4096,
                    )
                    .map_err(|_| Errno::ENOMEM)?;
                fs.fence_pages = true;
            }
            // A fence word's buffer is named by handle, not by address: it is
            // mapped here, once, if the client did not map it itself.
            let user_fence = match (&user_fence_gem, &fence_frames) {
                (Some((gem, off)), Some(frames)) => {
                    let found = fs.maps.iter().find(|m| Arc::ptr_eq(&m.gem, gem));
                    let va = match found {
                        Some(m) => m.va,
                        None => {
                            let pages = frames.len() as u64;
                            let va = fs.kernel_next;
                            if va + pages * 4096 > FENCE_IB_VA {
                                return Err(Errno::ENOMEM);
                            }
                            fs.space
                                .map(va, pages, Flags::SYSTEM_RW, |n| {
                                    frames
                                        .get(usize::try_from(n).unwrap_or(0))
                                        .map_or(0, |p| p.0 * 4096)
                                })
                                .map_err(|_| Errno::ENOMEM)?;
                            fs.kernel_next += pages * 4096;
                            fs.maps.push(Mapping {
                                va,
                                pages,
                                gem: Arc::clone(gem),
                            });
                            va
                        }
                    };
                    Some(va + off)
                }
                _ => None,
            };
            Ok((vmid, fs.slot, root, user_fence))
        })?;
        self.last_ctx_of.lock()[engine.index()] = ctx_id;
        let seq = match engine {
            Engine::Gfx => loop {
                let mut g = amd.gfx_lock();
                let gfx = g.as_mut().ok_or(Errno::ENXIO)?;
                let seq = self.seq.load(Ordering::Acquire).wrapping_add(1).max(1);
                // The page for this job's fence packet is free once the job that
                // used it before has finished.
                let page = engine.page_set() * FENCE_IB_SLOTS + u64::from(seq) % FENCE_IB_SLOTS;
                let before = seq.wrapping_sub(FENCE_IB_SLOTS_U32);
                if user_fence.is_some() && gfx.finished(&gpu).wrapping_sub(before) >= 0x8000_0000 {
                    drop(g);
                    if sched::current().is_interrupted() {
                        return Err(Errno::EINTR);
                    }
                    bus::manager().sleep_ms(cx.sleepable(), 1);
                    continue;
                }
                // The reference's frame (06 section 10.4): the previous job's
                // fence, the address space, the state loads (all of them when the
                // context changed), the frame, its fences, the switch.
                let mut p = Packets::new();
                p.wait_mem(
                    3,
                    gfx.seq_address(),
                    self.seq.load(Ordering::Acquire),
                    0xffff_ffff,
                    4,
                    true,
                );
                p.0.extend(amdgpu::gmc::ring_vm_flush(vmid, root, false, 0).0);
                let switched = self.last_ctx.swap(ctx_id, Ordering::AcqRel) != ctx_id;
                p.context_control(if switched { 0x0101_8003 } else { 0 });
                p.frame_control(false);
                for ib in &ibs {
                    p.indirect_buffer(extended(ib.va_start), ib.ib_bytes / 4, vmid, false);
                }
                p.frame_control(true);
                if let Some(va) = user_fence {
                    let mut f = Packets::new();
                    f.fence(extended(va), u64::from(seq), true, false);
                    let off = FENCE_IB_AT + (slot as u64 * FENCE_IB_FILE_BYTES) + page * 4096;
                    for (i, d) in f.0.iter().enumerate() {
                        gpu.vram_write32(usize::try_from(off).unwrap_or(0) + 4 * i, *d);
                    }
                    // No read back here: the ring's flush after its packets
                    // (a read across the bus) completes these writes too.
                    p.indirect_buffer(
                        extended(FENCE_IB_VA + page * 4096),
                        u32::try_from(f.0.len()).unwrap_or(0),
                        vmid,
                        false,
                    );
                }
                p.fence(gfx.seq_address(), u64::from(seq), false, true);
                p.switch_buffer();
                // The reference pads every commit to 256 dwords.
                p.pad(256);
                if let Err(e) = gfx.run(&gpu, &p.0, RING_WAIT_MS) {
                    drop(g);
                    if e == Errno::EAGAIN && self.hang_check(&amd, engine) {
                        continue;
                    }
                    return Err(e);
                }
                self.seq.store(seq, Ordering::Release);
                break seq;
            },
            Engine::Dma => loop {
                let mut g = amd.sdma_lock();
                let sdma = g.as_mut().ok_or(Errno::ENXIO)?;
                let seq = self.dma_seq.load(Ordering::Acquire).wrapping_add(1).max(1);
                // The page for this job's fence packets, in the DMA set.
                let page = engine.page_set() * FENCE_IB_SLOTS + u64::from(seq) % FENCE_IB_SLOTS;
                let before = seq.wrapping_sub(FENCE_IB_SLOTS_U32);
                if user_fence.is_some() && sdma.finished(&gpu).wrapping_sub(before) >= 0x8000_0000 {
                    drop(g);
                    if sched::current().is_interrupted() {
                        return Err(Errno::EINTR);
                    }
                    bus::manager().sleep_ms(cx.sleepable(), 1);
                    continue;
                }
                // The frame of 07 section 7.5: the previous job's fence, the
                // address space, the buffers, the fences.
                let start = sdma.position();
                let mut p = amdgpu::sdma::Packets::new();
                p.poll_mem(
                    sdma.seq_address(),
                    self.dma_seq.load(Ordering::Acquire),
                    0xffff_ffff,
                    4,
                    0xfff,
                );
                let [(lo_reg, hi_reg), _, _] = amdgpu::gmc::Hub::Gfx.context_table(vmid as usize);
                p.vm_flush(
                    vmid,
                    u32::try_from(lo_reg / 4).unwrap_or(0),
                    u32::try_from(hi_reg / 4).unwrap_or(0),
                    amdgpu::vm::pde(root, false),
                    amdgpu::gmc::invalidate_request(vmid),
                    amdgpu::sdma::INVALIDATION_ENGINE,
                );
                for ib in &ibs {
                    p.pad_for_indirect(start);
                    p.indirect(extended(ib.va_start), ib.ib_bytes / 4, vmid);
                }
                if let Some(va) = user_fence {
                    // As for the graphics ring: the client's fence word is
                    // written by a buffer run in the job's address space.
                    let mut f = amdgpu::sdma::Packets::new();
                    f.fence64(extended(va), u64::from(seq));
                    f.pad8();
                    let off = FENCE_IB_AT + (slot as u64 * FENCE_IB_FILE_BYTES) + page * 4096;
                    for (i, d) in f.0.iter().enumerate() {
                        gpu.vram_write32(usize::try_from(off).unwrap_or(0) + 4 * i, *d);
                    }
                    // No read back here: the ring's flush after its packets
                    // (a read across the bus) completes these writes too.
                    p.pad_for_indirect(start);
                    p.indirect(
                        extended(FENCE_IB_VA + page * 4096),
                        u32::try_from(f.0.len()).unwrap_or(0),
                        vmid,
                    );
                }
                p.fence(sdma.seq_address(), seq);
                p.trap(0);
                if let Err(e) = sdma.run(&gpu, &p.0, RING_WAIT_MS) {
                    drop(g);
                    if e == Errno::EAGAIN && self.hang_check(&amd, engine) {
                        continue;
                    }
                    return Err(e);
                }
                self.dma_seq.store(seq, Ordering::Release);
                break seq;
            },
            Engine::Compute(ring) => loop {
                let r = usize::from(ring);
                let mut g = amd.gfx_lock();
                let gfx = g.as_mut().ok_or(Errno::ENXIO)?;
                let last = self.comp_seq[r].load(Ordering::Acquire);
                let seq = last.wrapping_add(1).max(1);
                let page = engine.page_set() * FENCE_IB_SLOTS + u64::from(seq) % FENCE_IB_SLOTS;
                let before = seq.wrapping_sub(FENCE_IB_SLOTS_U32);
                if user_fence.is_some()
                    && gfx.compute_finished(&gpu, r).wrapping_sub(before) >= 0x8000_0000
                {
                    drop(g);
                    if sched::current().is_interrupted() {
                        return Err(Errno::EINTR);
                    }
                    bus::manager().sleep_ms(cx.sleepable(), 1);
                    continue;
                }
                // The graphics frame without its parser-only items (06
                // section 10.4): the previous fence, the address space (on
                // the ring's own invalidation engine), the buffers (with the
                // valid bit), the fences.
                let mut p = Packets::new();
                // Only if the previous job is still running (the processor
                // sees its fence word in a few nanoseconds; the engine polls
                // it across the bus).
                if gfx.compute_finished(&gpu, r).wrapping_sub(last) >= 0x8000_0000 {
                    p.wait_mem(3, gfx.compute_seq_address(r), last, 0xffff_ffff, 4, false);
                }
                p.0.extend(amdgpu::gmc::ring_vm_flush_on(vmid, root, false, 4 + r, false).0);
                for ib in &ibs {
                    p.indirect_buffer(extended(ib.va_start), ib.ib_bytes / 4, vmid, true);
                }
                if let Some(va) = user_fence {
                    let mut f = Packets::new();
                    f.fence(extended(va), compute_handle(ring, seq), true, false);
                    f.mark_compute();
                    let off = FENCE_IB_AT + (slot as u64 * FENCE_IB_FILE_BYTES) + page * 4096;
                    for (i, d) in f.0.iter().enumerate() {
                        gpu.vram_write32(usize::try_from(off).unwrap_or(0) + 4 * i, *d);
                    }
                    // No read back here: the ring's flush after its packets
                    // (a read across the bus) completes these writes too.
                    p.indirect_buffer(
                        extended(FENCE_IB_VA + page * 4096),
                        u32::try_from(f.0.len()).unwrap_or(0),
                        vmid,
                        true,
                    );
                }
                p.fence(gfx.compute_seq_address(r), u64::from(seq), false, true);
                if let Err(e) = gfx.compute_run(&gpu, r, &p.0, RING_WAIT_MS) {
                    drop(g);
                    if e == Errno::EAGAIN && self.hang_check(&amd, engine) {
                        continue;
                    }
                    return Err(e);
                }
                self.comp_seq[r].store(seq, Ordering::Release);
                break seq;
            },
        };
        let job: Arc<dyn drm::Fence> = Arc::new(JobFence {
            amd: self.amd.clone(),
            render: self.me.clone(),
            queue: Arc::clone(&self.queue),
            engine,
            seq,
        });
        for (h, point) in out_syncobjs {
            let slot = cx.syncobj(h).ok_or(Errno::ENOENT)?;
            // The point was checked; the handle may have been destroyed or
            // another job taken the point since, which is the caller's race.
            let _ = slot.add_point(point, Arc::clone(&job));
        }
        // Whoever uses these buffers next waits for this job (implicit
        // synchronisation, which sync files export and import).
        for g in &bos {
            g.reservation().lock().add(
                Arc::clone(&job),
                true,
                u32::try_from(engine.index()).unwrap_or(0),
            );
        }
        let handle = match engine {
            Engine::Compute(q) => compute_handle(q, seq),
            _ => u64::from(seq),
        };
        uapi::CsRequest::answer(arg, handle);
        Ok(0)
    }

    /// The last job's number on `engine`.
    fn last_seq(&self, engine: Engine) -> u32 {
        match engine {
            Engine::Gfx => self.seq.load(Ordering::Acquire),
            Engine::Dma => self.dma_seq.load(Ordering::Acquire),
            Engine::Compute(r) => self.comp_seq[usize::from(r)].load(Ordering::Acquire),
        }
    }

    /// Whether job `seq` of `engine` has finished (its fence word has
    /// reached it).
    fn done(&self, amd: &Amd, engine: Engine, seq: u32) -> Result<bool, Errno> {
        let gpu = amd.gpu_arc()?;
        Ok(finished_at(amd, &gpu, engine, seq))
    }

    fn wait_cs(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let r = uapi::WaitCs::decode(arg).map_err(|_| Errno::EINVAL)?;
        let amd = self.amd()?;
        let seq = u32::try_from(r.handle & 0xffff_ffff).map_err(|_| Errno::EINVAL)?;
        let engine = match Engine::new(r.ip_type, r.ring).ok_or(Errno::EINVAL)? {
            // A compute handle names its queue in the high half.
            Engine::Compute(_) => {
                let q = u8::try_from(r.handle >> 32).map_err(|_| Errno::EINVAL)?;
                if usize::from(q) >= COMPUTE_QUEUES {
                    return Err(Errno::EINVAL);
                }
                Engine::Compute(q)
            }
            e => e,
        };
        let start = sched::now_ns();
        let mut timed_out = false;
        loop {
            if self.done(&amd, engine, seq)? {
                break;
            }
            if sched::now_ns().saturating_sub(start) >= r.timeout_ns {
                timed_out = true;
                break;
            }
            // A signal ends the wait (a client may wait without limit).
            if sched::current().is_interrupted() {
                return Err(Errno::EINTR);
            }
            self.hang_check(&amd, engine);
            let left = r
                .timeout_ns
                .saturating_sub(sched::now_ns().saturating_sub(start));
            self.sleep_for(cx.sleepable(), &amd, engine, seq, left.min(50_000_000));
        }
        uapi::WaitCs::answer(arg, timed_out);
        Ok(0)
    }

    fn gem_metadata(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        if arg.len() < 288 {
            return Err(Errno::EINVAL);
        }
        let handle = u32::from_le_bytes([arg[0], arg[1], arg[2], arg[3]]);
        let op = u32::from_le_bytes([arg[4], arg[5], arg[6], arg[7]]);
        cx.gem(handle).ok_or(Errno::ENOENT)?;
        let amd = self.amd()?;
        self.with_file(&amd, cx.file_id(), |fs, _| {
            match op {
                // SET_METADATA
                1 => {
                    fs.metadata.insert(handle, arg[8..288].to_vec());
                }
                // GET_METADATA
                2 => {
                    if let Some(m) = fs.metadata.get(&handle) {
                        arg[8..288].copy_from_slice(m);
                    }
                }
                _ => return Err(Errno::EINVAL),
            }
            Ok(0)
        })
    }
}

impl Render {
    fn ioctl_inner(&self, cx: &IoctlCx<'_>, nr: u8, arg: &mut [u8]) -> Result<usize, Errno> {
        let amd = self.amd()?;
        // The graphics block starts with the first request.
        amd.ensure_gfx(cx.sleepable())?;
        match nr - abi::drm::DRM_COMMAND_BASE {
            cmd::INFO => self.info(&amd, cx, arg),
            cmd::GEM_CREATE => self.gem_create(cx, arg),
            cmd::GEM_MMAP => {
                let h = uapi::gem_mmap_handle(arg).map_err(|_| Errno::EINVAL)?;
                let gem = cx.gem(h).ok_or(Errno::ENOENT)?;
                uapi::gem_mmap_answer(arg, gem.mmap_offset());
                Ok(0)
            }
            cmd::CTX => {
                let r = uapi::Ctx::decode(arg).map_err(|_| Errno::EINVAL)?;
                match r.op {
                    uapi::ctx_op::ALLOC => {
                        let made_at = self.resets.load(Ordering::Acquire);
                        let id = self.with_file(&amd, cx.file_id(), |fs, c| {
                            let id = c.next_id;
                            c.next_id += 1;
                            fs.contexts.insert(id, made_at);
                            Ok(id)
                        })?;
                        uapi::Ctx::answer(arg, id, 0);
                    }
                    uapi::ctx_op::FREE => {
                        self.with_file(&amd, cx.file_id(), |fs, c| {
                            fs.contexts.remove(&r.id);
                            unbind(fs, c, Some(r.id));
                            Ok(())
                        })?;
                        uapi::Ctx::answer(arg, 0, 0);
                    }
                    // The state: whether a ring was reset since the context
                    // was made, and whether it was on it (10 section 9). Video
                    // memory is never lost here. The older query reports 3
                    // ("unknown") for a reset.
                    uapi::ctx_op::QUERY | uapi::ctx_op::QUERY2 => {
                        let made_at = self.with_file(&amd, cx.file_id(), |fs, _| {
                            Ok(fs.contexts.get(&r.id).copied())
                        })?;
                        let made_at = made_at.ok_or(Errno::EINVAL)?;
                        let resets = self.resets.load(Ordering::Acquire);
                        let reset = resets > made_at;
                        let guilty = reset && self.guilty.lock().contains(&r.id);
                        if r.op == uapi::ctx_op::QUERY2 {
                            let mut flags = 0;
                            if reset {
                                flags |= uapi::ctx_op::STATE_RESET;
                            }
                            if guilty {
                                flags |= uapi::ctx_op::STATE_GUILTY;
                            }
                            uapi::Ctx::answer_state(arg, flags, u32::from(reset), 0);
                        } else {
                            uapi::Ctx::answer_state(
                                arg,
                                0,
                                u32::from(reset),
                                if reset { 3 } else { 0 },
                            );
                        }
                    }
                    uapi::ctx_op::GET_STABLE_PSTATE | uapi::ctx_op::SET_STABLE_PSTATE => {
                        uapi::Ctx::answer(arg, 0, 0);
                    }
                    _ => return Err(Errno::EINVAL),
                }
                Ok(0)
            }
            cmd::BO_LIST => {
                let r = uapi::BoListRequest::decode(arg).map_err(|_| Errno::EINVAL)?;
                match r.operation {
                    uapi::bo_list_op::CREATE => {
                        let bos = bo_list_handles(cx, &r)?;
                        let id = self.with_file(&amd, cx.file_id(), |fs, c| {
                            let id = c.next_id;
                            c.next_id += 1;
                            fs.lists.insert(id, bos);
                            Ok(id)
                        })?;
                        uapi::BoListRequest::answer(arg, id);
                    }
                    uapi::bo_list_op::DESTROY => {
                        self.with_file(&amd, cx.file_id(), |fs, _| {
                            fs.lists.remove(&r.list_handle);
                            Ok(())
                        })?;
                        uapi::BoListRequest::answer(arg, 0);
                    }
                    uapi::bo_list_op::UPDATE => {
                        let bos = bo_list_handles(cx, &r)?;
                        self.with_file(&amd, cx.file_id(), |fs, _| {
                            fs.lists.insert(r.list_handle, bos);
                            Ok(())
                        })?;
                        uapi::BoListRequest::answer(arg, r.list_handle);
                    }
                    _ => return Err(Errno::EINVAL),
                }
                Ok(0)
            }
            cmd::GEM_VA => self.gem_va(cx, arg),
            cmd::CS => self.cs(cx, arg),
            cmd::WAIT_CS => self.wait_cs(cx, arg),
            cmd::GEM_WAIT_IDLE => {
                let (_, timeout) = uapi::gem_wait_idle(arg).map_err(|_| Errno::EINVAL)?;
                let start = sched::now_ns();
                let mut busy = false;
                // Every job on every engine.
                for engine in [
                    Engine::Gfx,
                    Engine::Dma,
                    Engine::Compute(0),
                    Engine::Compute(1),
                    Engine::Compute(2),
                    Engine::Compute(3),
                    Engine::Compute(4),
                    Engine::Compute(5),
                    Engine::Compute(6),
                    Engine::Compute(7),
                ] {
                    let seq = self.last_seq(engine);
                    while seq != 0 && !self.done(&amd, engine, seq)? {
                        if sched::now_ns().saturating_sub(start) >= timeout {
                            busy = true;
                            break;
                        }
                        if sched::current().is_interrupted() {
                            return Err(Errno::EINTR);
                        }
                        self.hang_check(&amd, engine);
                        let left = timeout.saturating_sub(sched::now_ns().saturating_sub(start));
                        self.sleep_for(cx.sleepable(), &amd, engine, seq, left.min(50_000_000));
                    }
                }
                uapi::gem_wait_idle_answer(arg, busy);
                Ok(0)
            }
            cmd::GEM_METADATA => self.gem_metadata(cx, arg),
            // GEM_OP: the creation parameters (0), the preferred domains
            // (1: every buffer is system memory, so nothing changes), and
            // the list of mappings (2, not built).
            cmd::GEM_OP => {
                if arg.len() < 16 {
                    return Err(Errno::EINVAL);
                }
                let handle = u32::from_le_bytes([arg[0], arg[1], arg[2], arg[3]]);
                let op = u32::from_le_bytes([arg[4], arg[5], arg[6], arg[7]]);
                let user = u64::from_le_bytes([
                    arg[8], arg[9], arg[10], arg[11], arg[12], arg[13], arg[14], arg[15],
                ]);
                let gem = cx.gem(handle).ok_or(Errno::ENOENT)?;
                match op {
                    0 => {
                        let mut info = [0u8; 32];
                        info[0..8].copy_from_slice(&gem.size().to_le_bytes());
                        info[8..16].copy_from_slice(&4096u64.to_le_bytes());
                        info[16..24].copy_from_slice(&uapi::domain::GTT.to_le_bytes());
                        cx.copy_out(user, &info)?;
                        Ok(0)
                    }
                    1 => Ok(0),
                    _ => Err(Errno::EOPNOTSUPP),
                }
            }
            // Reserving a VMID for the compute runtime: nothing to do.
            cmd::VM => {
                arg.iter_mut().for_each(|b| *b = 0);
                Ok(0)
            }
            // User-mode queues need a firmware scheduler this hardware does not
            // have; the device information says so (a user-queue mask of 0), and
            // the requests are refused as not supported.
            n if is_user_queue(n) => Err(Errno::EOPNOTSUPP),
            _ => Err(Errno::ENOTTY),
        }
    }
}

impl RenderApi for Render {
    fn ioctl(&self, cx: &IoctlCx<'_>, nr: u8, arg: &mut [u8]) -> Result<usize, Errno> {
        let r = self.ioctl_inner(cx, nr, arg);
        if let (Err(e), Ok(amd)) = (&r, self.amd()) {
            // A refusal of a request that is not a submission or a wait
            // (those say their own).
            let n = nr - abi::drm::DRM_COMMAND_BASE;
            if n != cmd::CS && n != cmd::WAIT_CS {
                amd.log(format_args!("render: request {n:#x} refused: {e:?}"));
            }
        }
        r
    }

    fn file_closed(&self, file: u64) {
        let mut st = self.state.lock();
        if let Some(mut fs) = st.files.remove(&file) {
            st.slots[fs.slot] = false;
            unbind(&mut fs, &mut st.count, None);
        }
    }
}

/// The completion of a job: the ring's fence word reaching its number.
struct JobFence {
    amd: Weak<Amd>,
    render: Weak<Render>,
    queue: Arc<sched::WaitQueue>,
    engine: Engine,
    seq: u32,
}

impl drm::Fence for JobFence {
    fn signaled(&self) -> bool {
        let Some(amd) = self.amd.upgrade() else {
            return true;
        };
        let Ok(gpu) = amd.gpu_arc() else {
            return true;
        };
        finished_at(&amd, &gpu, self.engine, self.seq)
    }

    fn wait(&self, s: &sched::Sleepable, ns: u64) -> bool {
        // A job that never finishes is found here too: the ring is reset.
        if let (Some(r), Some(amd)) = (self.render.upgrade(), self.amd.upgrade()) {
            r.hang_check(&amd, self.engine);
        }
        bus::manager().wait_timeout(s, &self.queue, &mut || self.signaled(), ns)
    }

    fn register(&self, w: &Arc<dyn vfs::event::Wake>) {
        if let Some(r) = self.render.upgrade() {
            r.add_waker(w, self.engine);
        }
    }

    /// A waiter needs this job by a time. The engine runs at the power
    /// firmware's automatic clock, which is its top clock under load, so
    /// there is no clock to raise; the hint is counted (`render` sysctl).
    fn set_deadline(&self, ns: u64) {
        if let Some(r) = self.render.upgrade() {
            r.deadline_hints.fetch_add(1, Ordering::Relaxed);
            r.latest_deadline
                .store(ns, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Whether job `seq` of `engine` has finished: its fence word has reached it.
fn finished_at(amd: &Amd, gpu: &Arc<Aperture>, engine: Engine, seq: u32) -> bool {
    match engine {
        Engine::Gfx => amd
            .gfx_lock()
            .as_ref()
            .is_none_or(|g| g.finished(gpu).wrapping_sub(seq) < 0x8000_0000),
        Engine::Dma => amd
            .sdma_lock()
            .as_ref()
            .is_none_or(|d| d.finished(gpu).wrapping_sub(seq) < 0x8000_0000),
        Engine::Compute(r) => amd.gfx_lock().as_ref().is_none_or(|g| {
            g.compute_finished(gpu, usize::from(r)).wrapping_sub(seq) < 0x8000_0000
        }),
    }
}

/// Gives up the compute queues a file's contexts were bound to: those of
/// context `ctx`, or all of them.
fn unbind(fs: &mut FileState, c: &mut Counters, ctx: Option<u32>) {
    let gone: Vec<(u32, u8)> = fs
        .queues
        .keys()
        .filter(|(id, _)| ctx.is_none_or(|x| x == *id))
        .copied()
        .collect();
    for k in gone {
        if let Some(q) = fs.queues.remove(&k) {
            let n = &mut c.bound[usize::from(q)];
            *n = n.saturating_sub(1);
        }
    }
}

/// What the interface reads while it runs.
struct LiveState {
    gpu: Arc<Aperture>,
    vram_used: u64,
    gtt_used: u64,
}

impl Live for LiveState {
    fn vram_usage(&self) -> u64 {
        self.vram_used
    }

    fn gtt_usage(&self) -> u64 {
        self.gtt_used
    }

    fn timestamp(&self) -> u64 {
        // The GPU's counter ticks at 100 MHz: a tick every 10 ns.
        sched::now_ns() / 10
    }

    fn read_register(&self, index: u32) -> Option<u32> {
        use amdgpu::regs::Window;
        Some(crate::amd::Bar5(&self.gpu).read32(usize::try_from(index).ok()? * 4))
    }
}

/// The buffer handles a buffer list request names (entries of at least 8
/// bytes: the handle, then the priority).
fn bo_list_handles(cx: &IoctlCx<'_>, r: &uapi::BoListRequest) -> Result<Vec<u32>, Errno> {
    if r.count == 0 {
        return Ok(Vec::new());
    }
    if r.count > 4096 || r.entry_size < 8 || r.entry_size > 64 {
        return Err(Errno::EINVAL);
    }
    let size = r.entry_size as usize;
    let mut raw = alloc::vec![0u8; r.count as usize * size];
    cx.copy_in(&mut raw, r.entries)?;
    Ok(raw
        .chunks_exact(size)
        .filter_map(|e| uapi::bo_list_entry(e, 0).map(|(h, _)| h))
        .collect())
}

/// The (handle, point) pairs of a timeline chunk: 16 bytes each (the handle,
/// flags, the 64-bit point).
fn timeline_entries(data: &[u8]) -> impl Iterator<Item = (u32, u64)> + '_ {
    data.as_chunks::<16>().0.iter().map(|e| {
        (
            u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
            u64::from_le_bytes([e[8], e[9], e[10], e[11], e[12], e[13], e[14], e[15]]),
        )
    })
}

/// How long a submission waits for the fences it depends on, in
/// nanoseconds.
const DEPENDENCY_WAIT_NS: u64 = 10_000_000_000;

/// Waits for the fences a job depends on that are not this driver's own
/// ordering: the objects it waits on, and the fences other programs put on
/// its buffers (imported sync files).
fn wait_dependencies(
    cx: &IoctlCx<'_>,
    objects: &[(u32, u64)],
    bos: &[Arc<drm::gem::Gem>],
    engine: Engine,
) -> Result<(), Errno> {
    let mut fences: Vec<Arc<dyn drm::Fence>> = Vec::new();
    for (h, point) in objects {
        let slot = cx.syncobj(*h).ok_or(Errno::ENOENT)?;
        // An object with nothing at the point is an error, as for the
        // reference: a dependency that was never submitted cannot be waited
        // for.
        fences.push(slot.find(*point)?.ok_or(Errno::EINVAL)?);
    }
    for g in bos {
        fences.extend(
            g.reservation()
                .lock()
                .blocking(true, Some(u32::try_from(engine.index()).unwrap_or(0))),
        );
    }
    let start = sched::now_ns();
    for f in fences {
        while !f.signaled() {
            if sched::now_ns().saturating_sub(start) > DEPENDENCY_WAIT_NS {
                return Err(Errno::ETIMEDOUT);
            }
            if sched::current().is_interrupted() {
                return Err(Errno::EINTR);
            }
            let _ = f.wait(cx.sleepable(), 20_000_000);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_means_its_low_48_bits_when_the_top_16_repeat_bit_47() {
        // The low half is itself.
        assert_eq!(canonical(0x1000), Some(0x1000));
        assert_eq!(canonical(0x7fff_ffff_ffff), Some(0x7fff_ffff_ffff));
        // The high half, written with the top bits set, is the same address
        // without them; the packets carry the extended form.
        assert_eq!(canonical(0xffff_8001_0001_0000), Some(0x8001_0001_0000));
        assert_eq!(extended(0x8001_0001_0000), 0xffff_8001_0001_0000);
        assert_eq!(extended(0x1000), 0x1000);
        // The two forms round-trip.
        for va in [
            0x10000u64,
            0x7fff_ffff_f000,
            0x8000_0000_0000,
            0xffff_ffc0_0000,
        ] {
            assert_eq!(canonical(extended(va)), Some(va));
        }
        // An address whose top bits do not repeat bit 47 means nothing.
        assert_eq!(canonical(0x0001_0000_0000_0000), None);
        assert_eq!(canonical(0x8000_0000_0000_0000), None);
        assert_eq!(canonical(0xffff_0000_0000_0000), None);
    }

    #[test]
    fn the_kernels_reserved_top_of_the_address_space_is_ordered() {
        // The reservations are checked where the constants are; what is left
        // to see at run time is that the extended form of the reserved
        // addresses is one the hardware accepts.
        assert!(canonical(extended(KERNEL_VA)).is_some());
        assert!(canonical(extended(FENCE_IB_VA)).is_some());
        assert_eq!(FENCE_IB_PAGES, 10 * FENCE_IB_SLOTS);
    }

    #[test]
    fn a_submission_names_its_engine_by_ip_type_and_ring() {
        assert_eq!(Engine::new(uapi::ip::GFX, 0), Some(Engine::Gfx));
        assert_eq!(Engine::new(uapi::ip::DMA, 0), Some(Engine::Dma));
        assert_eq!(Engine::new(uapi::ip::COMPUTE, 3), Some(Engine::Compute(3)));
        // Rings that are not offered, and engines that do not exist.
        assert_eq!(Engine::new(uapi::ip::GFX, 1), None);
        assert_eq!(Engine::new(uapi::ip::DMA, 1), None);
        assert_eq!(Engine::new(uapi::ip::COMPUTE, 4), None);
        assert_eq!(Engine::new(7, 0), None);
    }

    #[test]
    fn each_engine_has_its_own_set_of_fence_pages() {
        let sets: Vec<u64> = [
            Engine::Gfx,
            Engine::Dma,
            Engine::Compute(0),
            Engine::Compute(1),
            Engine::Compute(2),
            Engine::Compute(3),
            Engine::Compute(4),
            Engine::Compute(5),
            Engine::Compute(6),
            Engine::Compute(7),
        ]
        .iter()
        .map(|e| e.page_set())
        .collect();
        assert_eq!(sets, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn mapping_bits_become_page_table_flags() {
        let f = gpu_flags(VM_READABLE | VM_EXECUTABLE);
        assert!(f.readable && !f.writeable && f.executable);
        // Always system memory, snooped.
        assert!(f.system && f.snooped);
        let none = gpu_flags(0);
        assert!(!none.readable && !none.writeable && !none.executable);
        assert!(gpu_flags(VM_WRITEABLE).writeable);
    }

    #[test]
    fn a_contexts_slot_goes_to_the_less_loaded_of_its_two_queues() {
        let mut bound = [0u32; COMPUTE_QUEUES];
        // Nothing bound: the first of the pair.
        assert_eq!(choose_queue(1, &bound, 8), 1);
        bound[1] = 1;
        // The first is loaded: the second, four above.
        assert_eq!(choose_queue(1, &bound, 8), 5);
        bound[5] = 1;
        assert_eq!(choose_queue(1, &bound, 8), 1);
        // Other slots are not affected by it.
        assert_eq!(choose_queue(2, &bound, 8), 2);
        // Only the queues that answered their test are used.
        assert_eq!(choose_queue(1, &bound, 4), 1);
        bound[1] = 5;
        assert_eq!(choose_queue(1, &bound, 4), 1);
    }

    #[test]
    fn a_compute_handle_keeps_its_queue_and_orders_by_number() {
        assert_eq!(compute_handle(0, 7), 7);
        assert_eq!(compute_handle(5, 7), (5 << 32) | 7);
        // One queue's handles only grow, which is what a client's fence word
        // needs.
        assert!(compute_handle(5, 8) > compute_handle(5, 7));
        assert_eq!(compute_handle(5, 7) >> 32, 5);
        assert_eq!(compute_handle(5, 7) & 0xffff_ffff, 7);
    }

    #[test]
    fn the_user_queue_requests_are_recognised_and_nothing_else_is() {
        assert!(is_user_queue(0x16) && is_user_queue(0x17) && is_user_queue(0x18));
        for nr in [0x00, 0x04, 0x13, 0x14, 0x15, 0x19, 0x40] {
            assert!(!is_user_queue(nr), "{nr:#x}");
        }
    }
}
