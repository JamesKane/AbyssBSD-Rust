// SPDX-License-Identifier: BSD-2-Clause
//! Panthor's Direct Rendering Manager device: a render node (`dri/renderD<N>`), named `panthor`
//! as Mesa's loader looks for it, with the requests of spec 10.
//!
//! The firmware boots on the first request (the root file system holds its image), on the
//! requester's thread.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use drm::gem::{Backing, CacheMode, Placement};
use drm::{DriverInfo, DrmDriver, IoctlCx};

use panthor::Regs;

use crate::Dev;
use crate::uapi::{self as u, ObjArray, SyncOp};

/// The CPU mapping of a buffer: write-combined, as on a non-coherent GPU (09 §9.3.2, 11 §11.7).
const WC: Placement = Placement {
    backing: Backing::System,
    cache: CacheMode::WriteCombine,
    coherent: true,
};

/// The card's driver side.
pub(crate) struct Render(pub(crate) Arc<Dev>);

impl DrmDriver for Render {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "panthor",
            desc: "Panthor DRM driver",
            date: "20230801",
            major: u::MAJOR,
            minor: u::MINOR,
            patch: 0,
        }
    }

    fn render(&self) -> bool {
        true
    }

    fn ioctl_cx(&self, cx: &IoctlCx<'_>, nr: u8, arg: &mut [u8]) -> Result<usize, Errno> {
        let d = &self.0;
        d.ensure_booted(cx.sleepable())?;
        let file = cx.file_id();
        match nr {
            u::DEV_QUERY => d.dev_query(cx, arg),
            u::VM_CREATE => {
                if u::u32_at(arg, 0) != 0 {
                    return Err(Errno::EINVAL);
                }
                let mut core = d.core.lock(cx.sleepable());
                let (id, range) = d.vm_create(&mut core, file, u::u64_at(arg, 8))?;
                u::put_u32(arg, 4, id);
                u::put_u64(arg, 8, range);
                Ok(0)
            }
            u::VM_DESTROY => {
                if u::u32_at(arg, 4) != 0 {
                    return Err(Errno::EINVAL);
                }
                let mut core = d.core.lock(cx.sleepable());
                d.vm_destroy(&mut core, file, u::u32_at(arg, 0))?;
                Ok(0)
            }
            u::VM_BIND => self.vm_bind(cx, arg),
            u::VM_GET_STATE => {
                let core = d.core.lock(cx.sleepable());
                let vm = core
                    .files
                    .get(&file)
                    .and_then(|f| f.vms.get(&u::u32_at(arg, 0)))
                    .ok_or(Errno::EINVAL)?;
                u::put_u32(arg, 4, u32::from(vm.unusable));
                Ok(0)
            }
            u::BO_CREATE => {
                let (size, flags, excl) =
                    (u::u64_at(arg, 0), u::u32_at(arg, 8), u::u32_at(arg, 12));
                if size == 0
                    || u::u32_at(arg, 20) != 0
                    || flags & !(u::BO_NO_MMAP | u::BO_WB_MMAP) != 0
                {
                    return Err(Errno::EINVAL);
                }
                // WB_MMAP is a 1.7 flag: not offered.
                if flags & u::BO_WB_MMAP != 0 {
                    return Err(Errno::EINVAL);
                }
                let mut core = d.core.lock(cx.sleepable());
                if excl != 0
                    && !core
                        .files
                        .get(&file)
                        .is_some_and(|f| f.vms.contains_key(&excl))
                {
                    return Err(Errno::EINVAL);
                }
                let (handle, gem) = cx.new_gem(size, WC)?;
                Dev::file(&mut core, file).bo_flags.insert(handle, flags);
                u::put_u64(arg, 0, gem.size());
                u::put_u32(arg, 16, handle);
                Ok(0)
            }
            u::BO_MMAP_OFFSET => {
                if u::u32_at(arg, 4) != 0 {
                    return Err(Errno::EINVAL);
                }
                let h = u::u32_at(arg, 0);
                let gem = cx.gem(h).ok_or(Errno::ENOENT)?;
                let core = d.core.lock(cx.sleepable());
                let flags = core
                    .files
                    .get(&file)
                    .and_then(|f| f.bo_flags.get(&h))
                    .copied()
                    .unwrap_or(0);
                if flags & u::BO_NO_MMAP != 0 {
                    return Err(Errno::EPERM);
                }
                u::put_u64(arg, 8, gem.mmap_offset());
                Ok(0)
            }
            u::GROUP_CREATE => self.group_create(cx, arg),
            u::GROUP_DESTROY => {
                if u::u32_at(arg, 4) != 0 {
                    return Err(Errno::EINVAL);
                }
                let mut core = d.core.lock(cx.sleepable());
                d.group_destroy(&mut core, file, u::u32_at(arg, 0))?;
                Ok(0)
            }
            u::GROUP_SUBMIT => self.group_submit(cx, arg),
            u::GROUP_GET_STATE => {
                if u::u32_at(arg, 12) != 0 {
                    return Err(Errno::EINVAL);
                }
                let core = d.core.lock(cx.sleepable());
                let (state, fatal) = d.group_state(&core, file, u::u32_at(arg, 0))?;
                arg[..16].fill(0);
                u::put_u32(arg, 4, state);
                u::put_u32(arg, 8, fatal);
                Ok(0)
            }
            u::TILER_HEAP_CREATE => {
                let mut core = d.core.lock(cx.sleepable());
                let (h, ctx, first) = d.heap_create(
                    &mut core,
                    file,
                    u::u32_at(arg, 0),
                    u::u32_at(arg, 4),
                    u::u32_at(arg, 8),
                    u::u32_at(arg, 12),
                    u::u32_at(arg, 16),
                )?;
                u::put_u32(arg, 20, h);
                u::put_u64(arg, 24, ctx);
                u::put_u64(arg, 32, first);
                Ok(0)
            }
            u::TILER_HEAP_DESTROY => {
                if u::u32_at(arg, 4) != 0 {
                    return Err(Errno::EINVAL);
                }
                let mut core = d.core.lock(cx.sleepable());
                d.heap_destroy(&mut core, file, u::u32_at(arg, 0))?;
                Ok(0)
            }
            u::BO_SET_LABEL => {
                if u::u32_at(arg, 4) != 0 {
                    return Err(Errno::EINVAL);
                }
                cx.gem(u::u32_at(arg, 0)).ok_or(Errno::ENOENT)?;
                Ok(0)
            }
            u::SET_USER_MMIO_OFFSET => {
                let off = u::u64_at(arg, 0);
                if off != u::USER_MMIO_64 && off != u::USER_MMIO_32 {
                    return Err(Errno::EINVAL);
                }
                let mut core = d.core.lock(cx.sleepable());
                Dev::file(&mut core, file).user_mmio = off;
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn file_closed(&self, file: u64) {
        self.0.close_file(file);
    }

    /// The `LATEST_FLUSH_ID` page at the user MMIO offset (10 §10.9). It is a page of memory
    /// holding 1, the value 09 §9.5.2 gives a powered-down GPU's: a flush ID that is always old.
    /// The kernel's job prologue flushes whatever token a job carries, so nothing is skipped.
    /// Both of the offsets a file may choose are taken; any other offset in that range is not.
    fn mmap_special(
        &self,
        _file: u64,
        page: u64,
        pages: u64,
    ) -> Option<Result<Arc<vm::object::Object>, Errno>> {
        let off = page.checked_mul(0x1000)?;
        if off < u::USER_MMIO_32 {
            return None;
        }
        if (off != u::USER_MMIO_64 && off != u::USER_MMIO_32) || pages != 1 {
            return Some(Err(Errno::EINVAL));
        }
        if self.0.flush_page.get().is_none() {
            match drm::page_object(&1u32.to_le_bytes()) {
                Ok(p) => {
                    let _ = self.0.flush_page.set(p);
                }
                Err(e) => return Some(Err(e)),
            }
        }
        Some(self.0.flush_page.get().cloned().ok_or(Errno::ENOMEM))
    }
}

impl Render {
    fn vm_bind(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let d = &self.0;
        let file = cx.file_id();
        let (vm_id, flags) = (u::u32_at(arg, 0), u::u32_at(arg, 4));
        let ops = ObjArray::at(arg, 8);
        let elems = ops.read(48, 48, 4096, &|b, a| cx.copy_in(b, a))?;
        let asynchronous = flags & u::BIND_ASYNC != 0;
        // Validate everything before anything runs, then wait for every op's in-fences.
        let mut all = Vec::new();
        for e in &elems {
            let syncs = ObjArray::at(e, 32);
            let sops: Vec<SyncOp> = syncs
                .read(16, 16, 256, &|b, a| cx.copy_in(b, a))?
                .iter()
                .map(|s| SyncOp::parse(s))
                .collect::<Result<_, _>>()?;
            if !asynchronous && !sops.is_empty() {
                return Err(Errno::EINVAL);
            }
            all.push(sops);
        }
        for sops in &all {
            d.wait_syncs(cx, sops)?;
        }
        for (k, e) in elems.iter().enumerate() {
            let (op, handle, off, va, size) = (
                u::u32_at(e, 0),
                u::u32_at(e, 4),
                u::u64_at(e, 8),
                u::u64_at(e, 16),
                u::u64_at(e, 24),
            );
            let r = match op & u::OP_TYPE_MASK {
                u::OP_SYNC_ONLY => {
                    if op & !u::OP_TYPE_MASK != 0 || handle != 0 || off != 0 || va != 0 || size != 0
                    {
                        Err(Errno::EINVAL)
                    } else {
                        Ok(())
                    }
                }
                _ if size == 0 && !asynchronous => Ok(()),
                u::OP_MAP => {
                    if op & !(u::MAP_READONLY | u::MAP_NOEXEC | u::MAP_UNCACHED) != 0 {
                        Err(Errno::EINVAL)
                    } else {
                        let gem = cx.gem(handle).ok_or(Errno::EINVAL)?;
                        let mut core = d.core.lock(cx.sleepable());
                        d.vm_map(&mut core, file, vm_id, op, va, size, Some((gem, off)))
                    }
                }
                u::OP_UNMAP => {
                    if op & !u::OP_TYPE_MASK != 0 || handle != 0 || off != 0 {
                        Err(Errno::EINVAL)
                    } else {
                        let mut core = d.core.lock(cx.sleepable());
                        d.vm_map(&mut core, file, vm_id, op, va, size, None)
                    }
                }
                _ => Err(Errno::EINVAL),
            };
            if let Err(e) = r {
                if !asynchronous {
                    u::put_u32(arg, 12, u32::try_from(k).unwrap_or(u32::MAX));
                }
                return Err(e);
            }
        }
        // The binds are done: their signals carry finished fences.
        for sops in &all {
            d.signal_syncs(
                cx,
                sops,
                &(Arc::new(drm::syncobj::Done) as Arc<dyn drm::Fence>),
            )?;
        }
        Ok(0)
    }

    fn group_create(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let d = &self.0;
        let qa = ObjArray::at(arg, 0);
        if qa.count == 0 || qa.count > 32 {
            return Err(Errno::EINVAL);
        }
        let qs = qa.read(8, 8, 32, &|b, a| cx.copy_in(b, a))?;
        let (maxc, maxf, maxt, prio) = (
            u::u8_at(arg, 16),
            u::u8_at(arg, 17),
            u::u8_at(arg, 18),
            u::u8_at(arg, 19),
        );
        if prio > 3 {
            return Err(Errno::EINVAL);
        }
        if prio >= 2 {
            return Err(Errno::EACCES);
        }
        if u::u32_at(arg, 20) != 0 {
            return Err(Errno::EINVAL);
        }
        let (cm, fm, tm) = (u::u64_at(arg, 24), u::u64_at(arg, 32), u::u64_at(arg, 40));
        let info = &d.info;
        if cm & !info.shader_present != 0
            || fm & !info.shader_present != 0
            || tm & !info.tiler_present != 0
        {
            return Err(Errno::EINVAL);
        }
        if cm.count_ones() < u32::from(maxc)
            || fm.count_ones() < u32::from(maxf)
            || tm.count_ones() < u32::from(maxt)
        {
            return Err(Errno::EINVAL);
        }
        let mut queues = Vec::new();
        for q in &qs {
            let (p, size) = (u::u8_at(q, 0), u::u32_at(q, 4));
            if q[1..4].iter().any(|&b| b != 0)
                || !size.is_power_of_two()
                || !(0x1000..=0x1_0000).contains(&size)
                || p > 15
            {
                return Err(Errno::EINVAL);
            }
            queues.push((p, size));
        }
        let mut core = d.core.lock(cx.sleepable());
        let h = d.group_create(
            &mut core,
            cx.file_id(),
            u::u32_at(arg, 48),
            &queues,
            (cm, fm, tm),
            [maxc, maxf, maxt],
        )?;
        u::put_u32(arg, 52, h);
        Ok(0)
    }

    fn group_submit(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let d = &self.0;
        if u::u32_at(arg, 4) != 0 {
            return Err(Errno::EINVAL);
        }
        let handle = u::u32_at(arg, 0) & 0xFFFF;
        let subs = ObjArray::at(arg, 8).read(40, 40, 256, &|b, a| cx.copy_in(b, a))?;
        let mut jobs = Vec::new();
        for s in &subs {
            let (q, size, addr, flush) = (
                u::u32_at(s, 0),
                u::u32_at(s, 4),
                u::u64_at(s, 8),
                u::u32_at(s, 16),
            );
            if u::u32_at(s, 20) != 0
                || (size == 0) != (addr == 0)
                || addr % 64 != 0
                || size % 8 != 0
                || flush & 0x7F00_0000 != 0
            {
                return Err(Errno::EINVAL);
            }
            let sops: Vec<SyncOp> = ObjArray::at(s, 24)
                .read(16, 16, 256, &|b, a| cx.copy_in(b, a))?
                .iter()
                .map(|x| SyncOp::parse(x))
                .collect::<Result<_, _>>()?;
            jobs.push((q as usize, addr, size, flush, sops));
        }
        for (q, addr, size, flush, sops) in &jobs {
            d.wait_syncs(cx, sops)?;
            let mut tries = 0;
            let fence = loop {
                let mut core = d.core.lock(cx.sleepable());
                match d.submit(&mut core, cx.file_id(), handle, *q, *addr, *size, *flush) {
                    Err(Errno::EAGAIN) if tries < 5000 => {
                        drop(core);
                        tries += 1;
                        bus::manager().sleep_ms(cx.sleepable(), 1);
                    }
                    Err(Errno::EAGAIN) => return Err(Errno::ETIMEDOUT),
                    r => break r?,
                }
            };
            d.signal_syncs(cx, sops, &fence)?;
        }
        Ok(0)
    }
}

impl Dev {
    /// Waits, on the requester's thread, for every wait in `sops` (10 §10.5: a fence that is not
    /// there is `EINVAL`).
    fn wait_syncs(&self, cx: &IoctlCx<'_>, sops: &[SyncOp]) -> Result<(), Errno> {
        for op in sops.iter().filter(|o| !o.signal) {
            let slot = cx.syncobj(op.handle).ok_or(Errno::ENOENT)?;
            let fence = if op.timeline && op.point != 0 {
                slot.find(op.point)?
            } else {
                slot.fence()
            };
            let fence = fence.ok_or(Errno::EINVAL)?;
            // Bounded: a job runs at most a few seconds (07 §7.10.3).
            let mut waited = 0;
            while !fence.signaled() {
                if !fence.wait(cx.sleepable(), 1_000_000_000) {
                    waited += 1;
                    if waited >= 30 {
                        return Err(Errno::ETIMEDOUT);
                    }
                }
            }
        }
        Ok(())
    }

    fn signal_syncs(
        &self,
        cx: &IoctlCx<'_>,
        sops: &[SyncOp],
        fence: &Arc<dyn drm::Fence>,
    ) -> Result<(), Errno> {
        for op in sops.iter().filter(|o| o.signal) {
            let slot = cx.syncobj(op.handle).ok_or(Errno::EINVAL)?;
            if op.timeline && op.point != 0 {
                slot.add_point(op.point, Arc::clone(fence))?;
            } else {
                slot.set(Some(Arc::clone(fence)));
            }
        }
        Ok(())
    }

    fn dev_query(&self, cx: &IoctlCx<'_>, arg: &mut [u8]) -> Result<usize, Errno> {
        let (ty, size, ptr) = (
            u::u32_at(arg, 0),
            u::u32_at(arg, 4) as usize,
            u::u64_at(arg, 8),
        );
        let (len, min) = match ty {
            u::QUERY_GPU_INFO => (u::GPU_INFO_SIZE, u::GPU_INFO_MIN),
            u::QUERY_CSIF_INFO => (u::CSIF_INFO_SIZE, u::CSIF_INFO_SIZE),
            u::QUERY_TIMESTAMP_INFO => (u::TIMESTAMP_INFO_SIZE, u::TIMESTAMP_INFO_MIN),
            u::QUERY_GROUP_PRIORITIES_INFO => (u::PRIORITIES_INFO_SIZE, u::PRIORITIES_INFO_SIZE),
            _ => return Err(Errno::EINVAL),
        };
        if ptr == 0 {
            u::put_u32(arg, 4, u32::try_from(len).map_err(|_| Errno::EINVAL)?);
            return Ok(0);
        }
        if size < min {
            return Err(Errno::EINVAL);
        }
        let mut out = alloc::vec![0u8; len.max(size)];
        match ty {
            u::QUERY_GPU_INFO => self.gpu_info(&mut out),
            u::QUERY_CSIF_INFO => {
                let core = self.core.lock(cx.sleepable());
                let i = core.fw.as_ref().ok_or(Errno::ENODEV)?.iface;
                u::put_u32(&mut out, 0, i.groups.min(16));
                u::put_u32(&mut out, 4, i.streams);
                u::put_u32(&mut out, 8, i.work_registers());
                u::put_u32(&mut out, 12, i.cs_features >> 8 & 0xFF);
                u::put_u32(&mut out, 16, 4);
            }
            u::QUERY_TIMESTAMP_INFO => {
                let mut input = alloc::vec![0u8; size.min(len)];
                cx.copy_in(&mut input, ptr)?;
                let flags = u::u32_at(&input, 24);
                let flags = if flags == 0 {
                    1 | 1 << 4 | 1 << 6
                } else {
                    flags
                };
                if flags & !0xFF != 0 || (flags >> 1 & 7) > 2 {
                    return Err(Errno::EINVAL);
                }
                let t0 = sched::now_ns();
                if flags & 1 != 0 {
                    u::put_u64(
                        &mut out,
                        8,
                        self.regs.read_counter(panthor::regs::TIMESTAMP),
                    );
                }
                if flags >> 1 & 7 != 0 {
                    let ns = sched::now_ns();
                    u::put_u64(&mut out, 40, ns / 1_000_000_000);
                    u::put_u64(&mut out, 48, ns % 1_000_000_000);
                }
                if flags & 1 << 5 != 0 {
                    u::put_u64(
                        &mut out,
                        32,
                        self.regs.read_counter(panthor::regs::CYCLE_COUNT),
                    );
                }
                if flags & 1 << 4 != 0 {
                    u::put_u64(
                        &mut out,
                        16,
                        self.regs.read64(panthor::regs::TIMESTAMP_OFFSET),
                    );
                }
                if flags & 1 << 6 != 0 {
                    u::put_u64(&mut out, 0, self.timer_hz);
                }
                u::put_u32(&mut out, 24, flags);
                if flags & 1 << 7 != 0 {
                    u::put_u32(
                        &mut out,
                        28,
                        u32::try_from(sched::now_ns() - t0).unwrap_or(u32::MAX),
                    );
                }
            }
            _ => out[0] = 0b0011,
        }
        cx.copy_out(ptr, &out[..size])?;
        Ok(0)
    }

    fn gpu_info(&self, out: &mut [u8]) {
        let i = &self.info;
        for (off, v) in [
            (0, i.gpu_id.0),
            (4, i.revid),
            (8, i.csf_id),
            (12, i.l2_features),
            (16, i.tiler_features),
            (20, i.mem_features),
            (24, i.mmu_features),
            (28, i.thread_features),
            (32, i.thread_max_threads),
            (36, i.thread_max_workgroup_size),
            (40, i.thread_max_barrier_size),
            (44, i.coherency_features),
            (64, i.as_present),
            (96, i.core_features),
        ] {
            u::put_u32(out, off, v);
        }
        for (k, v) in i.texture_features.iter().enumerate() {
            u::put_u32(out, 48 + 4 * k, *v);
        }
        u::put_u64(out, 72, i.shader_present);
        u::put_u64(out, 80, i.l2_present);
        u::put_u64(out, 88, i.tiler_present);
        u::put_u64(out, 104, i.gpu_features);
    }
}
