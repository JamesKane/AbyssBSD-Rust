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
use amdgpu::pm4::acquire;
use amdgpu::uapi::{self, InfoRequest, cmd};
use amdgpu::vm::{Flags, MemoryType, Space, Tables};
use core::sync::atomic::{AtomicU32, Ordering};
use drm::IoctlCx;
use sync::PreemptLock;
use vgapci::Aperture;

use crate::amd::Amd;
use crate::gcring::VramTables;

/// The first video memory offset of the page-table pools: one pool of 1 MiB
/// (256 tables) a VMID, VMIDs 1 to 7 (06 section 5).
const POOLS_AT: u64 = 176 << 20;
const POOL_TABLES: usize = 256;
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
    contexts: Vec<u32>,
    lists: Vec<u32>,
    /// The metadata a client keeps with a buffer, by handle.
    metadata: BTreeMap<u32, Vec<u8>>,
}

/// Numbers the interface hands out and counts.
struct Counters {
    /// The next context or list id.
    next_id: u32,
    /// Bytes of buffers made and not freed.
    bytes: u64,
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
    device: Device,
}

impl core::fmt::Debug for Render {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Render")
    }
}

impl Render {
    pub(crate) fn new(amd: Weak<Amd>, vram_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            amd,
            seq: AtomicU32::new(0),
            state: PreemptLock::new(State {
                files: BTreeMap::new(),
                slots: [false; VMIDS],
                count: Counters {
                    next_id: 1,
                    bytes: 0,
                },
            }),
            device: Device::raphael(vram_bytes, GTT_BYTES),
        })
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
                POOLS_AT + ((slot as u64) << 20),
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
                    contexts: Vec::new(),
                    lists: Vec::new(),
                    metadata: BTreeMap::new(),
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
        let live = LiveState { gpu, bytes };
        let answer = info::answer(&self.device, &live, &req).map_err(|_| Errno::EINVAL)?;
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
        let (handle, gem) = cx.new_gem(size, drm::gem::Placement::SYSTEM_COHERENT)?;
        let amd = self.amd()?;
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
                let flags = Flags {
                    readable: r.flags & VM_READABLE != 0,
                    writeable: r.flags & VM_WRITEABLE != 0,
                    executable: r.flags & VM_EXECUTABLE != 0,
                    system: true,
                    snooped: true,
                    memory_type: MemoryType::Nc,
                };
                self.with_file(&amd, cx.file_id(), |fs, _| {
                    fs.space
                        .map(r.va_address, pages, flags, |n| {
                            frames
                                .get(first + usize::try_from(n).unwrap_or(0))
                                .map_or(0, |p| p.0 * page)
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

    fn cs(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let r = uapi::CsRequest::decode(arg).map_err(|_| Errno::EINVAL)?;
        if r.num_chunks == 0 || r.num_chunks > 64 {
            return Err(Errno::EINVAL);
        }
        // The chunks, read from the caller's memory before any lock.
        let mut ibs = Vec::new();
        let mut fence: Option<(u32, u32)> = None;
        for i in 0..u64::from(r.num_chunks) {
            let mut p = [0u8; 8];
            cx.copy_in(&mut p, r.chunks + 8 * i)?;
            let mut h = [0u8; 16];
            cx.copy_in(&mut h, u64::from_le_bytes(p))?;
            let c = uapi::Chunk::decode(&h).map_err(|_| Errno::EINVAL)?;
            let len = usize::try_from(c.length_dw).map_err(|_| Errno::EINVAL)? * 4;
            if len > 4096 {
                return Err(Errno::EINVAL);
            }
            let mut data = alloc::vec![0u8; len];
            cx.copy_in(&mut data, c.data)?;
            match c.id {
                uapi::chunk::IB => {
                    let ib = uapi::IbChunk::decode(&data).map_err(|_| Errno::EINVAL)?;
                    if ib.ip_type != uapi::ip::GFX || ib.ip_instance != 0 || ib.ring != 0 {
                        return Err(Errno::EINVAL);
                    }
                    ibs.push(ib);
                }
                uapi::chunk::FENCE if len >= 8 => {
                    let handle = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    let offset = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                    fence = Some((handle, offset));
                }
                // Dependencies and synchronisation objects: every job is on
                // one ring, in order; the buffer handles list is not needed
                // (every mapping stays resident).
                _ => {}
            }
        }
        if ibs.is_empty() {
            return Err(Errno::EINVAL);
        }
        let user_fence_gem = fence.and_then(|(h, o)| cx.gem(h).map(|g| (g, u64::from(o))));
        let amd = self.amd()?;
        let gpu = amd.gpu_arc()?;
        // The address space's root and the user fence's address, under the
        // state lock; the job itself under the ring's.
        let (vmid, root, user_fence) = self.with_file(&amd, cx.file_id(), |fs, _| {
            let vmid = u32::try_from(fs.slot).unwrap_or(0) + 1;
            let root = fs.space.tables().address(fs.space.root()).0;
            let user_fence = match &user_fence_gem {
                Some((gem, off)) => Some(
                    fs.maps
                        .iter()
                        .find(|m| Arc::ptr_eq(&m.gem, gem))
                        .map(|m| m.va + off)
                        .ok_or(Errno::EINVAL)?,
                ),
                None => None,
            };
            Ok((vmid, root, user_fence))
        })?;
        let seq = {
            let mut g = amd.gfx_lock();
            let gfx = g.as_mut().ok_or(Errno::ENXIO)?;
            let seq = self.seq.load(Ordering::Acquire).wrapping_add(1).max(1);
            let mut p = amdgpu::gmc::ring_vm_flush(vmid, root, false, 0);
            p.acquire_mem(acquire::FULL, false);
            p.context_control(0);
            p.frame_control(false);
            for ib in &ibs {
                p.indirect_buffer(ib.va_start, ib.ib_bytes / 4, vmid, false);
            }
            p.frame_control(true);
            if let Some(va) = user_fence {
                p.fence(va, u64::from(seq), true, false);
            }
            p.fence(gfx.seq_address(), u64::from(seq), false, false);
            gfx.run(&gpu, &p.0, RING_WAIT_MS)?;
            self.seq.store(seq, Ordering::Release);
            seq
        };
        uapi::CsRequest::answer(arg, u64::from(seq));
        Ok(0)
    }

    fn state_seq(&self) -> u32 {
        self.seq.load(Ordering::Acquire)
    }

    /// Whether job `seq` has finished (the fence word has reached it).
    fn done(&self, amd: &Amd, seq: u32) -> Result<bool, Errno> {
        let gpu = amd.gpu_arc()?;
        let g = amd.gfx_lock();
        let gfx = g.as_ref().ok_or(Errno::ENXIO)?;
        Ok(gfx.finished(&gpu).wrapping_sub(seq) < 0x8000_0000)
    }

    fn wait_cs(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let r = uapi::WaitCs::decode(arg).map_err(|_| Errno::EINVAL)?;
        let amd = self.amd()?;
        let seq = u32::try_from(r.handle & 0xffff_ffff).map_err(|_| Errno::EINVAL)?;
        let start = sched::now_ns();
        let _ = cx;
        let mut timed_out = false;
        loop {
            if self.done(&amd, seq)? {
                break;
            }
            if sched::now_ns().saturating_sub(start) >= r.timeout_ns {
                timed_out = true;
                break;
            }
            bus::manager().delay_us(50);
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

impl RenderApi for Render {
    fn ioctl(&self, cx: &IoctlCx<'_>, nr: u8, arg: &mut [u8]) -> Result<usize, Errno> {
        let amd = self.amd()?;
        // The graphics ring must be started (`gc_fw` and `gc_ring`).
        if !amd.gfx_ready() {
            return Err(Errno::ENXIO);
        }
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
                        let id = self.with_file(&amd, cx.file_id(), |fs, c| {
                            let id = c.next_id;
                            c.next_id += 1;
                            fs.contexts.push(id);
                            Ok(id)
                        })?;
                        uapi::Ctx::answer(arg, id, 0);
                    }
                    uapi::ctx_op::FREE => {
                        self.with_file(&amd, cx.file_id(), |fs, _| {
                            fs.contexts.retain(|c| *c != r.id);
                            Ok(())
                        })?;
                        uapi::Ctx::answer(arg, 0, 0);
                    }
                    // Query: no reset ever happened; the stable power state
                    // and the rest are not supported.
                    uapi::ctx_op::QUERY | uapi::ctx_op::QUERY2 => uapi::Ctx::answer(arg, 0, 0),
                    _ => return Err(Errno::EINVAL),
                }
                Ok(0)
            }
            cmd::BO_LIST => {
                let r = uapi::BoListRequest::decode(arg).map_err(|_| Errno::EINVAL)?;
                match r.operation {
                    uapi::bo_list_op::CREATE => {
                        let id = self.with_file(&amd, cx.file_id(), |fs, c| {
                            let id = c.next_id;
                            c.next_id += 1;
                            fs.lists.push(id);
                            Ok(id)
                        })?;
                        uapi::BoListRequest::answer(arg, id);
                    }
                    uapi::bo_list_op::DESTROY => {
                        self.with_file(&amd, cx.file_id(), |fs, _| {
                            fs.lists.retain(|l| *l != r.list_handle);
                            Ok(())
                        })?;
                        uapi::BoListRequest::answer(arg, 0);
                    }
                    uapi::bo_list_op::UPDATE => uapi::BoListRequest::answer(arg, r.list_handle),
                    _ => return Err(Errno::EINVAL),
                }
                Ok(0)
            }
            cmd::GEM_VA => self.gem_va(cx, arg),
            cmd::CS => self.cs(cx, arg),
            cmd::WAIT_CS => self.wait_cs(cx, arg),
            cmd::GEM_WAIT_IDLE => {
                let (_, timeout) = uapi::gem_wait_idle(arg).map_err(|_| Errno::EINVAL)?;
                let seq = self.state_seq();
                let start = sched::now_ns();
                let mut busy = false;
                while seq != 0 && !self.done(&amd, seq)? {
                    if sched::now_ns().saturating_sub(start) >= timeout {
                        busy = true;
                        break;
                    }
                    bus::manager().delay_us(50);
                }
                uapi::gem_wait_idle_answer(arg, busy);
                Ok(0)
            }
            cmd::GEM_METADATA => self.gem_metadata(cx, arg),
            // Reserving a VMID for the compute runtime: nothing to do.
            cmd::VM => {
                arg.iter_mut().for_each(|b| *b = 0);
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn file_closed(&self, file: u64) {
        let mut st = self.state.lock();
        if let Some(fs) = st.files.remove(&file) {
            st.slots[fs.slot] = false;
        }
    }
}

/// What the interface reads while it runs.
struct LiveState {
    gpu: Arc<Aperture>,
    bytes: u64,
}

impl Live for LiveState {
    fn vram_usage(&self) -> u64 {
        self.bytes
    }

    fn gtt_usage(&self) -> u64 {
        0
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
