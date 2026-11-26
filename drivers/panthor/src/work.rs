// SPDX-License-Identifier: BSD-2-Clause
//! The GPU's work: per-file address spaces (spec 04, 10 §10.4), groups and their queues (07),
//! the jobs written into the queues' rings (07 §7.4), their completion fences, the firmware's
//! events (06 §6.3.7, 07 §7.9), and tiler heaps (08).

#![allow(
    clippy::cast_possible_truncation,
    reason = "slot, queue and heap indices are bounded by the firmware interface (at most 16), and \
              ring and chunk sizes are checked against 32 bits where userspace gives them"
)]
//!
//! The scheduling is the simplest the firmware allows: a group takes a free CSG slot (and its
//! address space a free MMU slot) when it first has work, and keeps them until it is destroyed.
//! There are 8 CSG slots and 7 address spaces for groups on the Sky1; a group that finds none
//! free is refused (`EBUSY`) rather than time-multiplexed (07 §7.6 is not built).

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::errno::Errno;
use drm::gem::Gem;
use panthor::csf::{self, Blocks, Shared};
use panthor::mmu::{self, Map};
use panthor::regs::Regs;
use panthor::ring;
use sched::Sleepable;

use crate::mcu::{Booted, SharedMem};
use crate::mem::{self, Kbo, PageTables, VaRange};
use crate::{Dev, Spin};

/// Kernel buffers in the MCU's address space: no-exec; cached or not as 07 §7.2.1 says.
const MCU_NC: Map = Map {
    read_only: false,
    no_exec: true,
    cacheable: false,
};
const MCU_CACHED: Map = Map {
    read_only: false,
    no_exec: true,
    cacheable: true,
};
/// Ring buffers and sync objects in a group's address space: no-exec, uncached (07 §7.2.1).
const VM_NC: Map = MCU_NC;
/// Heap memory: no-exec, cacheable (08 §8.2.3).
const VM_CACHED: Map = MCU_CACHED;

/// Most VMs and groups per file (10 §10.4.1, §10.6.1).
const MAX_VMS: u32 = 32;
const MAX_GROUPS: u32 = 128;
const MAX_HEAPS: usize = 128;
/// The kernel's job sequence: 16 instructions, 128 bytes (07 §7.4.4).
const JOB_BYTES: u64 = 128;

/// What the core lock protects.
pub(crate) struct Core {
    pub(crate) fw: Option<Booted>,
    pub(crate) files: BTreeMap<u64, File>,
    /// CSG slots: the group on each (file, group handle).
    csg: Vec<Option<(u64, u32)>>,
    /// MMU slots (index 0 is the MCU's): the VM on each (file, VM id).
    as_slots: Vec<Option<(u64, u32)>>,
}

impl Core {
    pub(crate) fn new(as_present: u32) -> Self {
        let n = (32 - as_present.leading_zeros()) as usize;
        Self {
            fw: None,
            files: BTreeMap::new(),
            csg: Vec::new(),
            as_slots: alloc::vec![None; n.max(1)],
        }
    }
}

/// One open file's objects.
#[derive(Default)]
pub(crate) struct File {
    pub(crate) vms: BTreeMap<u32, Vm>,
    pub(crate) groups: BTreeMap<u32, Group>,
    /// Creation flags of the buffers made here, by handle.
    pub(crate) bo_flags: BTreeMap<u32, u32>,
    pub(crate) user_mmio: u64,
}

struct Mapping {
    va: u64,
    len: u64,
    _gem: Option<Arc<Gem>>,
}

/// A tiler heap (08).
struct Heap {
    chunks: Vec<Kbo>,
    chunk_size: u64,
    max_chunks: u32,
    target_in_flight: u32,
}

/// A VM's heap pool: 128 contexts of 64 bytes (08 §8.2.3).
struct HeapPool {
    pool: Kbo,
    heaps: Vec<Option<Heap>>,
}

/// A GPU address space of one file.
pub(crate) struct Vm {
    pt: PageTables,
    va_bits: u8,
    /// Userspace owns `[0, user_end)`; the kernel's buffers sit above.
    user_end: u64,
    krange: VaRange,
    pub(crate) as_slot: Option<usize>,
    mappings: Vec<Mapping>,
    pub(crate) unusable: bool,
    heaps: Option<HeapPool>,
    /// Kernel buffers no longer used, kept until the VM goes (the GPU may have read them).
    retired: Vec<Kbo>,
    /// Groups using it.
    users: u32,
    /// Its handle was destroyed; it goes when its last group does.
    destroyed: bool,
    /// Its slot was disabled by a fault: enabled again before its next use (04 §4.9.2).
    faulted: bool,
}

/// Group states (07 §7.3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GState {
    Created,
    Active,
    Terminated,
}

struct Queue {
    ring: Kbo,
    /// The ring's input page (offset 0) and output page (offset 4096), in the MCU's space.
    io: Kbo,
    priority: u8,
    insert: u64,
    seq: u64,
    last: Option<Arc<dyn drm::Fence>>,
}

/// A group of queues (07).
pub(crate) struct Group {
    vm: u32,
    queues: Vec<Queue>,
    slot: Option<u32>,
    state: GState,
    compute: u64,
    fragment: u64,
    tiler: u64,
    max: [u8; 3],
    suspend: Kbo,
    protm: Kbo,
    /// One 64-bit sync object per queue (07 §7.2.3), in the group's VM.
    syncs: Arc<Kbo>,
    pub(crate) fatal_queues: u32,
    pub(crate) timedout: bool,
}

/// A job's completion: its queue's sync object reaching its number (07 §7.4.6).
pub(crate) struct JobFence {
    syncs: Arc<Kbo>,
    off: usize,
    seq: u64,
    dev: Weak<Dev>,
}

impl drm::Fence for JobFence {
    fn signaled(&self) -> bool {
        self.syncs.mem.read_at::<u64>(self.off) >= self.seq
    }

    fn wait(&self, s: &Sleepable, ns: u64) -> bool {
        match self.dev.upgrade() {
            Some(d) => bus::manager().wait_timeout(s, &d.done, &mut || self.signaled(), ns),
            None => self.signaled(),
        }
    }

    fn register(&self, w: &Arc<dyn vfs::event::Wake>) {
        if let Some(d) = self.dev.upgrade() {
            d.wakers.lock().push(Arc::clone(w));
        }
    }
}

fn shared(b: &Booted) -> SharedMem<'_> {
    SharedMem(&b.sections[b.shared])
}

impl Booted {
    /// A kernel buffer in the MCU's address space.
    pub(crate) fn kbo(&mut self, d: &Dev, size: u64, m: Map) -> Result<Kbo, Errno> {
        let mut pt = PageTables {
            pages: core::mem::take(&mut self.tables),
            root: self.root,
        };
        let mut range = VaRange {
            next: self.next_va,
            end: crate::mcu::MCU_HOST_END,
            free: core::mem::take(&mut self.free_va),
        };
        let r = mem::kbo(
            &d.regs,
            &d.tag,
            &mut pt,
            Some(0),
            d.info.va_bits(),
            &mut range,
            size,
            m,
        );
        self.tables = pt.pages;
        self.next_va = range.next;
        self.free_va = range.free;
        r.map_err(|why| {
            d.log(format_args!("MCU buffer: {why}"));
            Errno::ENOMEM
        })
    }

    /// Unmaps and frees a kernel buffer of the MCU's space (its user has left the firmware).
    pub(crate) fn free(&mut self, d: &Dev, k: Kbo) {
        let mut pt = PageTables {
            pages: core::mem::take(&mut self.tables),
            root: self.root,
        };
        let (va, len) = (k.va, k.len());
        let ok = mem::update(&d.regs, Some(0), va, va + len, || {
            pt.unmap(&d.tag, d.info.va_bits(), va, len)
        })
        .is_ok();
        self.tables = pt.pages;
        if ok {
            self.free_va.push((va, len));
            drop(k);
        } else {
            // The firmware may still see it: kept.
            core::mem::forget(k);
        }
    }
}

/// The default user range (10 §10.4.1): the processes' 48-bit address space, below the top.
fn user_range(va_bits: u8, asked: u64) -> Result<(u64, u64), Errno> {
    let full = 1u64 << va_bits;
    if full < 0x1000_0000 {
        return Err(Errno::EINVAL);
    }
    let cap = full - 0x1000_0000;
    let user = if asked != 0 {
        if asked > cap {
            return Err(Errno::EINVAL);
        }
        asked
    } else if va_bits > 32 {
        full / 2
    } else {
        cap
    };
    // The kernel range: the largest power of two fitting above, at the top.
    let room = full - user;
    let kernel = 1u64 << (63 - room.leading_zeros());
    Ok((full - kernel, full))
}

impl Dev {
    pub(crate) fn file(core: &mut Core, file: u64) -> &mut File {
        core.files.entry(file).or_insert_with(|| File {
            user_mmio: crate::uapi::USER_MMIO_64,
            ..File::default()
        })
    }

    pub(crate) fn vm_create(
        &self,
        core: &mut Core,
        file: u64,
        asked: u64,
    ) -> Result<(u32, u64), Errno> {
        let va_bits = self.info.va_bits();
        let (user_end, top) = user_range(va_bits, asked)?;
        let pt = PageTables::new(&self.tag).ok_or(Errno::ENOMEM)?;
        let f = Self::file(core, file);
        let id = (1..=MAX_VMS)
            .find(|i| !f.vms.contains_key(i))
            .ok_or(Errno::EBUSY)?;
        f.vms.insert(
            id,
            Vm {
                pt,
                va_bits,
                user_end,
                krange: VaRange {
                    next: user_end,
                    end: top,
                    free: Vec::new(),
                },
                as_slot: None,
                mappings: Vec::new(),
                unusable: false,
                heaps: None,
                retired: Vec::new(),
                users: 0,
                destroyed: false,
                faulted: false,
            },
        );
        Ok((id, user_end))
    }

    pub(crate) fn vm_destroy(&self, core: &mut Core, file: u64, id: u32) -> Result<(), Errno> {
        let f = core.files.get_mut(&file).ok_or(Errno::EINVAL)?;
        let vm = f
            .vms
            .get_mut(&id)
            .filter(|v| !v.destroyed)
            .ok_or(Errno::EINVAL)?;
        vm.destroyed = true;
        // Its heaps stay mapped while a group may still use them (08 §8.5): they go with it.
        if vm.users == 0 {
            self.drop_vm(core, file, id);
        }
        Ok(())
    }

    /// Frees a VM nothing uses: its MMU slot first.
    fn drop_vm(&self, core: &mut Core, file: u64, id: u32) {
        let Some(f) = core.files.get_mut(&file) else {
            return;
        };
        let Some(vm) = f.vms.remove(&id) else {
            return;
        };
        if let Some(slot) = vm.as_slot {
            let _ = panthor::control::flush(
                &self.regs,
                &mut Spin,
                panthor::control::Flush::PAGE_TABLES,
            );
            if mmu::disable(&self.regs, &mut Spin, slot).is_err() {
                core::mem::forget(vm);
                return;
            }
            core.as_slots[slot] = None;
        }
    }

    /// VM_BIND's ops, run synchronously (10 §10.4.3: an asynchronous bind waits for its fences
    /// first, then runs, so its signals are ready when the request returns).
    #[allow(clippy::too_many_arguments, reason = "the request's own fields")]
    pub(crate) fn vm_map(
        &self,
        core: &mut Core,
        file: u64,
        id: u32,
        flags: u32,
        va: u64,
        size: u64,
        gem: Option<(Arc<Gem>, u64)>,
    ) -> Result<(), Errno> {
        let vm = core
            .files
            .get_mut(&file)
            .and_then(|f| f.vms.get_mut(&id))
            .filter(|v| !v.destroyed)
            .ok_or(Errno::EINVAL)?;
        if (va | size) & 0xFFF != 0 || va.checked_add(size).is_none_or(|e| e > vm.user_end) {
            return Err(Errno::EINVAL);
        }
        let (start, end) = (va, va + size);
        let slot = vm.as_slot;
        match flags & crate::uapi::OP_TYPE_MASK {
            crate::uapi::OP_MAP => {
                if vm.unusable {
                    return Err(Errno::EINVAL);
                }
                let (gem, off) = gem.ok_or(Errno::EINVAL)?;
                if off & 0xFFF != 0 || off.checked_add(size).is_none_or(|e| e > gem.size()) {
                    return Err(Errno::EINVAL);
                }
                let pas = gem.phys_pages()?;
                let first = (off / 0x1000) as usize;
                let pas = pas
                    .get(first..first + (size / 0x1000) as usize)
                    .ok_or(Errno::EINVAL)?
                    .to_vec();
                let m = Map {
                    read_only: flags & crate::uapi::MAP_READONLY != 0,
                    no_exec: flags & crate::uapi::MAP_NOEXEC != 0,
                    cacheable: flags & crate::uapi::MAP_UNCACHED == 0,
                };
                let (tag, bits) = (&self.tag, vm.va_bits);
                let pt = &mut vm.pt;
                let r = mem::update(&self.regs, slot, start, end, || {
                    pt.unmap(tag, bits, start, size);
                    pt.map_pages(tag, bits, start, &pas, m)
                })
                .map_err(|_| Errno::EIO)?;
                // A mapping wholly replaced lets its buffer go; one only partly replaced keeps its
                // reference, since some of its pages are still mapped.
                vm.mappings
                    .retain(|x| !(x.va >= start && x.va + x.len <= end));
                if r.is_err() {
                    vm.unusable = true;
                    return Err(Errno::ENOMEM);
                }
                vm.mappings.push(Mapping {
                    va,
                    len: size,
                    _gem: Some(gem),
                });
                Ok(())
            }
            crate::uapi::OP_UNMAP => {
                let (tag, bits) = (&self.tag, vm.va_bits);
                let pt = &mut vm.pt;
                mem::update(&self.regs, slot, start, end, || {
                    pt.unmap(tag, bits, start, size)
                })
                .map_err(|_| Errno::EIO)?;
                vm.mappings
                    .retain(|x| !(x.va >= start && x.va + x.len <= end));
                Ok(())
            }
            _ => Err(Errno::EINVAL),
        }
    }

    /// GROUP_CREATE (10 §10.6.1, 07 §7.3.2): `queues` as (priority, ring size).
    pub(crate) fn group_create(
        &self,
        core: &mut Core,
        file: u64,
        vm_id: u32,
        queues: &[(u8, u32)],
        masks: (u64, u64, u64),
        max: [u8; 3],
    ) -> Result<u32, Errno> {
        if !core
            .files
            .get(&file)
            .and_then(|f| f.vms.get(&vm_id))
            .is_some_and(|v| !v.destroyed)
        {
            return Err(Errno::EINVAL);
        }
        if core
            .files
            .get(&file)
            .is_some_and(|f| f.groups.len() >= MAX_GROUPS as usize)
        {
            return Err(Errno::EBUSY);
        }
        let fw = core.fw.as_mut().ok_or(Errno::ENODEV)?;
        let iface = fw.iface;
        if queues.is_empty() || queues.len() > iface.streams as usize {
            return Err(Errno::EINVAL);
        }
        // The MCU's buffers; on a failure the ones made are unmapped and freed (nothing ran).
        let mut mcu: Vec<Kbo> = Vec::new();
        let sizes = [
            u64::from(iface.suspend_size.max(1)),
            u64::from(iface.protm_suspend_size.max(1)),
        ]
        .into_iter()
        .chain(queues.iter().map(|_| 0x2000));
        for (k, size) in sizes.enumerate() {
            match fw.kbo(self, size, if k < 2 { MCU_CACHED } else { MCU_NC }) {
                Ok(b) => mcu.push(b),
                Err(e) => {
                    for b in mcu {
                        fw.free(self, b);
                    }
                    return Err(e);
                }
            }
        }
        let mut mcu = mcu.into_iter();
        let (Some(suspend), Some(protm)) = (mcu.next(), mcu.next()) else {
            return Err(Errno::ENOMEM);
        };
        let ios: Vec<Kbo> = mcu.collect();
        let vm = core
            .files
            .get_mut(&file)
            .and_then(|f| f.vms.get_mut(&vm_id))
            .filter(|v| !v.destroyed)
            .ok_or(Errno::EINVAL)?;
        let syncs = mem::kbo(
            &self.regs,
            &self.tag,
            &mut vm.pt,
            vm.as_slot,
            vm.va_bits,
            &mut vm.krange,
            16 * queues.len() as u64,
            VM_NC,
        );
        let mut rings = Vec::new();
        for &(_, size) in queues {
            match mem::kbo(
                &self.regs,
                &self.tag,
                &mut vm.pt,
                vm.as_slot,
                vm.va_bits,
                &mut vm.krange,
                u64::from(size),
                VM_NC,
            ) {
                Ok(r) => rings.push(r),
                Err(_) => break,
            }
        }
        let syncs = match syncs {
            Ok(s) if rings.len() == queues.len() => s,
            other => {
                // Mapped in the VM: retired with it, not freed here.
                vm.retired.extend(rings);
                if let Ok(s) = other {
                    vm.retired.push(s);
                }
                let fw = core.fw.as_mut().ok_or(Errno::ENODEV)?;
                fw.free(self, suspend);
                fw.free(self, protm);
                for io in ios {
                    fw.free(self, io);
                }
                return Err(Errno::ENOMEM);
            }
        };
        let mut qs = Vec::new();
        for ((&(priority, _), io), ring) in queues.iter().zip(ios).zip(rings) {
            qs.push(Queue {
                ring,
                io,
                priority,
                insert: 0,
                seq: 0,
                last: None,
            });
        }
        vm.users += 1;
        let f = core.files.get_mut(&file).ok_or(Errno::EINVAL)?;
        let handle = (1..=MAX_GROUPS)
            .find(|h| !f.groups.contains_key(h))
            .ok_or(Errno::EBUSY)?;
        f.groups.insert(
            handle,
            Group {
                vm: vm_id,
                queues: qs,
                slot: None,
                state: GState::Created,
                compute: masks.0,
                fragment: masks.1,
                tiler: masks.2,
                max,
                suspend,
                protm,
                syncs: Arc::new(syncs),
                fatal_queues: 0,
                timedout: false,
            },
        );
        Ok(handle)
    }

    /// GROUP_DESTROY, and a closing file's groups: off the firmware first (TERMINATE), then
    /// every queue's sync object set to its last job so waiters are released (07 §7.10.5), then
    /// the memory.
    pub(crate) fn group_destroy(
        &self,
        core: &mut Core,
        file: u64,
        handle: u32,
    ) -> Result<(), Errno> {
        let mut g = core
            .files
            .get_mut(&file)
            .and_then(|f| f.groups.remove(&handle))
            .ok_or(Errno::EINVAL)?;
        let mut off_firmware = true;
        if let Some(slot) = g.slot.take() {
            off_firmware = self.terminate(core, slot, &g);
            core.csg[slot as usize] = None;
        }
        for (k, q) in g.queues.iter().enumerate() {
            g.syncs.mem.write_at::<u64>(16 * k, q.seq);
            g.syncs.mem.write_at::<u32>(16 * k + 8, u32::MAX);
        }
        self.wake_all();
        let vm_id = g.vm;
        let Some(fw) = core.fw.as_mut() else {
            core::mem::forget(g);
            return Ok(());
        };
        if !off_firmware {
            core::mem::forget(g);
            return Ok(());
        }
        let Group {
            queues,
            suspend,
            protm,
            syncs,
            ..
        } = g;
        fw.free(self, suspend);
        fw.free(self, protm);
        let mut rings = Vec::new();
        for q in queues {
            fw.free(self, q.io);
            rings.push(q.ring);
        }
        let mut drop_vm = false;
        if let Some(vm) = core
            .files
            .get_mut(&file)
            .and_then(|f| f.vms.get_mut(&vm_id))
        {
            vm.retired.extend(rings);
            if let Ok(s) = Arc::try_unwrap(syncs) {
                vm.retired.push(s);
            }
            vm.users -= 1;
            if vm.users == 0 {
                if vm.destroyed {
                    drop_vm = true;
                } else if let Some(slot) = vm.as_slot.take() {
                    // No group uses its MMU slot any more: it goes back.
                    let _ = panthor::control::flush(
                        &self.regs,
                        &mut Spin,
                        panthor::control::Flush::PAGE_TABLES,
                    );
                    if mmu::disable(&self.regs, &mut Spin, slot).is_ok() {
                        core.as_slots[slot] = None;
                    } else {
                        vm.as_slot = Some(slot);
                    }
                }
            }
        }
        if drop_vm {
            self.drop_vm(core, file, vm_id);
        }
        Ok(())
    }

    /// Sends TERMINATE to CSG slot `slot` and waits; `false` if the firmware did not answer.
    fn terminate(&self, core: &mut Core, slot: u32, g: &Group) -> bool {
        let Some(fw) = core.fw.as_ref() else {
            return true;
        };
        let sh = shared(fw);
        let Ok(b) = csf::csg(&sh, &fw.iface, slot) else {
            return true;
        };
        csf::stop_group(&sh, &b, csf::CSG_TERMINATE);
        let ok = self.ring_and_wait(&sh, fw, slot, &b, 0);
        for j in 0..g.queues.len() as u32 {
            if let Ok(c) = csf::cs(&sh, &fw.iface, slot, j) {
                csf::stop_stream(&sh, &c);
            }
        }
        if !ok {
            self.log(format_args!("group on slot {slot} did not terminate"));
        }
        ok
    }

    /// Rings CSG slot `slot` (its GLB doorbell bit and doorbell 0) and waits up to 100 ms for its
    /// state and the toggles in `mask` to be acknowledged, ringing again (a real edge only) when a
    /// request seems missed (06 §6.3.8).
    fn ring_and_wait(
        &self,
        sh: &SharedMem<'_>,
        fw: &Booted,
        slot: u32,
        b: &Blocks,
        mask: u32,
    ) -> bool {
        let i = &fw.iface;
        csf::ring_group(sh, i, slot);
        crate::mcu::ring(&self.regs);
        for n in 0..10_000u32 {
            if csf::csg_acked(sh, b, mask) {
                return true;
            }
            if n % 200 == 199
                && csf::read64(sh, i.glb_input + 0x08) as u32 == sh.read32(i.glb_output + 0x08)
            {
                csf::ring_group(sh, i, slot);
                crate::mcu::ring(&self.regs);
            }
            bus::manager().delay_us(10);
        }
        csf::csg_acked(sh, b, mask)
    }

    /// Gives group `handle` of `file` a CSG slot (and its VM an MMU slot) and starts it
    /// (07 §7.3.3).
    fn make_resident(&self, core: &mut Core, file: u64, handle: u32) -> Result<u32, Errno> {
        let (vm_id, slot) = {
            let g = core
                .files
                .get(&file)
                .and_then(|f| f.groups.get(&handle))
                .ok_or(Errno::EINVAL)?;
            if let Some(s) = g.slot {
                return Ok(s);
            }
            (g.vm, g.slot)
        };
        debug_assert!(slot.is_none());
        let fw = core.fw.as_ref().ok_or(Errno::ENODEV)?;
        if core.csg.is_empty() {
            core.csg = alloc::vec![None; fw.iface.groups.min(16) as usize];
        }
        let csg_slot = core
            .csg
            .iter()
            .position(Option::is_none)
            .ok_or(Errno::EBUSY)? as u32;
        // The VM's MMU slot.
        let as_slot = {
            let vm = core
                .files
                .get(&file)
                .and_then(|f| f.vms.get(&vm_id))
                .ok_or(Errno::EINVAL)?;
            match vm.as_slot {
                Some(s) if !vm.faulted => s,
                Some(s) => {
                    let (root, bits) = (vm.pt.root, vm.va_bits);
                    mmu::enable(
                        &self.regs,
                        &mut Spin,
                        s,
                        root,
                        mmu::memattr(false),
                        mmu::transcfg(bits, false),
                    )
                    .map_err(|_| Errno::EIO)?;
                    self.mmu_mask
                        .fetch_or(1 << s, core::sync::atomic::Ordering::AcqRel);
                    if let Some(vm) = core
                        .files
                        .get_mut(&file)
                        .and_then(|f| f.vms.get_mut(&vm_id))
                    {
                        vm.faulted = false;
                    }
                    s
                }
                None => {
                    let present = self.info.as_present;
                    let s = (1..core.as_slots.len())
                        .find(|&s| present & (1 << s) != 0 && core.as_slots[s].is_none())
                        .ok_or(Errno::EBUSY)?;
                    let (root, bits) = (vm.pt.root, vm.va_bits);
                    mmu::enable(
                        &self.regs,
                        &mut Spin,
                        s,
                        root,
                        mmu::memattr(false),
                        mmu::transcfg(bits, false),
                    )
                    .map_err(|_| Errno::EIO)?;
                    self.mmu_mask
                        .fetch_or(1 << s, core::sync::atomic::Ordering::AcqRel);
                    core.as_slots[s] = Some((file, vm_id));
                    if let Some(vm) = core
                        .files
                        .get_mut(&file)
                        .and_then(|f| f.vms.get_mut(&vm_id))
                    {
                        vm.as_slot = Some(s);
                    }
                    s
                }
            }
        };
        let fw = core.fw.as_ref().ok_or(Errno::ENODEV)?;
        let sh = shared(fw);
        let g = core
            .files
            .get(&file)
            .and_then(|f| f.groups.get(&handle))
            .ok_or(Errno::EINVAL)?;
        let b = csf::csg(&sh, &fw.iface, csg_slot).map_err(|_| Errno::EIO)?;
        let mut streams = 0u32;
        for (j, q) in g.queues.iter().enumerate() {
            let c = csf::cs(&sh, &fw.iface, csg_slot, j as u32).map_err(|_| Errno::EIO)?;
            // The input page's EXTRACT from the output page's (06 §6.6.6).
            let out_extract = q.io.mem.read_at::<u64>(0x1000);
            q.io.mem.write_at::<u64>(8, out_extract);
            csf::program_stream(
                &sh,
                &c,
                &csf::Stream {
                    ring: q.ring.va,
                    ring_size: q.ring.len() as u32,
                    user_input: q.io.va,
                    user_output: q.io.va + 0x1000,
                    priority: q.priority,
                    doorbell: (csg_slot + 1) as u8,
                },
            );
            streams |= 1 << j;
        }
        let cfg = csf::Group {
            compute: g.compute,
            fragment: g.fragment,
            tiler: (g.tiler & 0xFFFF_FFFF) as u32,
            max_compute: g.max[0],
            max_fragment: g.max[1],
            max_tiler: g.max[2],
            priority: (15 - csg_slot.min(15)) as u8,
            as_slot: as_slot as u32,
            suspend: g.suspend.va,
            protm_suspend: g.protm.va,
        };
        csf::start_group(&sh, &fw.iface, &b, &cfg, streams, csf::CSG_START);
        if !self.ring_and_wait(&sh, fw, csg_slot, &b, csf::CSG_ENDPOINT_CONFIG) {
            self.log(format_args!("group did not start on CSG slot {csg_slot}"));
            return Err(Errno::EIO);
        }
        core.csg[csg_slot as usize] = Some((file, handle));
        if let Some(g) = core
            .files
            .get_mut(&file)
            .and_then(|f| f.groups.get_mut(&handle))
        {
            g.slot = Some(csg_slot);
            g.state = GState::Active;
        }
        Ok(csg_slot)
    }

    /// Writes one job into queue `q` of group `handle` (07 §7.4.3) and rings its doorbell.
    /// Returns its fence. An empty job's fence is the queue's last one.
    #[allow(clippy::too_many_arguments, reason = "the request's own fields")]
    pub(crate) fn submit(
        self: &Arc<Self>,
        core: &mut Core,
        file: u64,
        handle: u32,
        q: usize,
        stream: u64,
        size: u32,
        flush_id: u32,
    ) -> Result<Arc<dyn drm::Fence>, Errno> {
        {
            let g = core
                .files
                .get(&file)
                .and_then(|f| f.groups.get(&handle))
                .ok_or(Errno::EINVAL)?;
            if g.state == GState::Terminated || g.timedout || g.fatal_queues != 0 {
                return Err(Errno::EINVAL);
            }
            if q >= g.queues.len() {
                return Err(Errno::EINVAL);
            }
            if size == 0 {
                return Ok(g.queues[q]
                    .last
                    .clone()
                    .unwrap_or_else(|| Arc::new(drm::syncobj::Done)));
            }
        }
        let slot = self.make_resident(core, file, handle)?;
        let fw = core.fw.as_ref().ok_or(Errno::ENODEV)?;
        let (regs_n, sb) = (fw.iface.work_registers(), fw.iface.cs_features >> 8 & 0xFF);
        let g = core
            .files
            .get_mut(&file)
            .and_then(|f| f.groups.get_mut(&handle))
            .ok_or(Errno::EINVAL)?;
        let syncs = Arc::clone(&g.syncs);
        let qu = &mut g.queues[q];
        let ring_size = qu.ring.len();
        let extract = qu.io.mem.read_at::<u64>(0x1000);
        if qu.insert + JOB_BYTES - extract > ring_size {
            return Err(Errno::EAGAIN);
        }
        let words = ring::job(regs_n, sb, stream, size, syncs.va + 16 * q as u64, flush_id);
        let at = (qu.insert % ring_size) as usize;
        for (k, w) in words.iter().enumerate() {
            qu.ring.mem.write_at::<u64>(at + 8 * k, *w);
        }
        hal::dma::write_barrier();
        qu.io.mem.write_at::<u64>(8, extract);
        qu.insert += JOB_BYTES;
        qu.io.mem.write_at::<u64>(0, qu.insert);
        qu.seq += 1;
        let fence: Arc<dyn drm::Fence> = Arc::new(JobFence {
            syncs,
            off: 16 * q,
            seq: qu.seq,
            dev: Arc::downgrade(self),
        });
        qu.last = Some(Arc::clone(&fence));
        hal::dma::doorbell_barrier();
        self.regs
            .write32(0x8_0000 + (slot as usize + 1) * 0x1_0000, 1);
        Ok(fence)
    }

    pub(crate) fn group_state(
        &self,
        core: &Core,
        file: u64,
        handle: u32,
    ) -> Result<(u32, u32), Errno> {
        let g = core
            .files
            .get(&file)
            .and_then(|f| f.groups.get(&handle))
            .ok_or(Errno::EINVAL)?;
        let mut state = 0;
        if g.timedout {
            state |= crate::uapi::STATE_TIMEDOUT;
        }
        if g.fatal_queues != 0 {
            state |= crate::uapi::STATE_FATAL_FAULT;
        }
        Ok((state, g.fatal_queues))
    }

    /// TILER_HEAP_CREATE (10 §10.7.1, 08 §8.3.4): the context's and the first chunk's addresses.
    #[allow(clippy::too_many_arguments, reason = "the request's own fields")]
    pub(crate) fn heap_create(
        &self,
        core: &mut Core,
        file: u64,
        vm_id: u32,
        initial: u32,
        chunk_size: u32,
        max_chunks: u32,
        target: u32,
    ) -> Result<(u32, u64, u64), Errno> {
        if initial == 0 || initial > max_chunks {
            return Err(Errno::EINVAL);
        }
        if !chunk_size.is_multiple_of(0x1000) || !(0x2_0000..=0x80_0000).contains(&chunk_size) {
            return Err(Errno::EINVAL);
        }
        let vm = core
            .files
            .get_mut(&file)
            .and_then(|f| f.vms.get_mut(&vm_id))
            .filter(|v| !v.destroyed)
            .ok_or(Errno::EINVAL)?;
        let stride = 32u64.max(1 << self.info.l2_line_log2());
        if vm.heaps.is_none() {
            let pool = mem::kbo(
                &self.regs,
                &self.tag,
                &mut vm.pt,
                vm.as_slot,
                vm.va_bits,
                &mut vm.krange,
                MAX_HEAPS as u64 * stride,
                VM_CACHED,
            )
            .map_err(|_| Errno::ENOMEM)?;
            vm.heaps = Some(HeapPool {
                pool,
                heaps: (0..MAX_HEAPS).map(|_| None).collect(),
            });
        }
        let idx = vm
            .heaps
            .as_ref()
            .and_then(|p| p.heaps.iter().position(Option::is_none))
            .ok_or(Errno::EBUSY)?;
        let mut chunks: Vec<Kbo> = Vec::new();
        for _ in 0..initial {
            let c = match mem::kbo(
                &self.regs,
                &self.tag,
                &mut vm.pt,
                vm.as_slot,
                vm.va_bits,
                &mut vm.krange,
                u64::from(chunk_size),
                VM_CACHED,
            ) {
                Ok(c) => c,
                Err(_) => {
                    vm.retired.extend(chunks);
                    return Err(Errno::ENOMEM);
                }
            };
            // NEXT points at the chunk made before (08 §8.3.4).
            if let Some(prev) = chunks.last() {
                c.mem
                    .write_at::<u64>(0, prev.va | u64::from(chunk_size >> 12));
            }
            chunks.push(c);
        }
        let first = chunks.last().map_or(0, |c| c.va);
        let pool = vm.heaps.as_mut().ok_or(Errno::EINVAL)?;
        let ctx = pool.pool.va + idx as u64 * stride;
        for k in 0..stride as usize / 4 {
            pool.pool
                .mem
                .write_at::<u32>(idx * stride as usize + 4 * k, 0);
        }
        pool.heaps[idx] = Some(Heap {
            chunks,
            chunk_size: u64::from(chunk_size),
            max_chunks,
            target_in_flight: target,
        });
        Ok((vm_id << 16 | idx as u32, ctx, first))
    }

    pub(crate) fn heap_destroy(
        &self,
        core: &mut Core,
        file: u64,
        handle: u32,
    ) -> Result<(), Errno> {
        let vm = core
            .files
            .get_mut(&file)
            .and_then(|f| f.vms.get_mut(&(handle >> 16)))
            .ok_or(Errno::EINVAL)?;
        let pool = vm.heaps.as_mut().ok_or(Errno::ENOENT)?;
        let heap = pool
            .heaps
            .get_mut((handle & 0xFFFF) as usize)
            .and_then(Option::take)
            .ok_or(Errno::EINVAL)?;
        vm.retired.extend(heap.chunks);
        Ok(())
    }

    /// A file closed: its groups, then its VMs.
    pub(crate) fn file_closed(&self, core: &mut Core, file: u64) {
        let groups: Vec<u32> = core
            .files
            .get(&file)
            .map(|f| f.groups.keys().copied().collect())
            .unwrap_or_default();
        for g in groups {
            let _ = self.group_destroy(core, file, g);
        }
        let vms: Vec<u32> = core
            .files
            .get(&file)
            .map(|f| f.vms.keys().copied().collect())
            .unwrap_or_default();
        for v in vms {
            let _ = self.vm_destroy(core, file, v);
        }
        if core.files.get(&file).is_some_and(|f| f.vms.is_empty()) {
            core.files.remove(&file);
        }
    }

    /// Wakes everything waiting on a job.
    pub(crate) fn wake_all(&self) {
        self.done.wake_all();
        let w: Vec<_> = core::mem::take(&mut *self.wakers.lock());
        for x in w {
            x.wake();
        }
    }

    /// The JOB interrupt's events (06 §6.3.7, 07 §7.9), after the block's bits were cleared.
    pub(crate) fn job_events(&self, core: &mut Core, bits: u32) {
        let Some(fw) = core.fw.as_ref() else {
            return;
        };
        let sh = shared(fw);
        let iface = fw.iface;
        if bits & crate::mcu::JOB_GLOBAL != 0 {
            csf::ack_global_idle(&sh, &iface);
        }
        let mut sync = false;
        let mut faulted: Vec<(u32, u32)> = Vec::new();
        let mut oom: Vec<(u32, u32)> = Vec::new();
        for slot in 0..iface.groups.min(31) {
            if bits & 1 << slot == 0 {
                continue;
            }
            let Ok(b) = csf::csg(&sh, &iface, slot) else {
                continue;
            };
            let irq_req = sh.read32(b.output + csf::CSG_CS_IRQ_REQ);
            let irq_ack = sh.read32(b.input + csf::CSG_CS_IRQ_ACK);
            sh.write32(b.input + csf::CSG_CS_IRQ_ACK, irq_req);
            let ev = csf::events(
                &sh,
                b.input + csf::CSG_REQ,
                b.output + csf::CSG_ACK,
                csf::CSG_EVENTS,
            );
            csf::ack_events(&sh, b.input + csf::CSG_REQ, b.output + csf::CSG_ACK, ev);
            if ev & csf::CSG_SYNC_UPDATE != 0 {
                sync = true;
            }
            if ev & csf::CSG_PROGRESS_TIMER != 0 {
                self.log(format_args!("CSG slot {slot}: progress timeout"));
                faulted.push((slot, u32::MAX));
            }
            let mut db = 0u32;
            for j in 0..iface.streams.min(32) {
                if (irq_req ^ irq_ack) & 1 << j == 0 {
                    continue;
                }
                let Ok(c) = csf::cs(&sh, &iface, slot, j) else {
                    continue;
                };
                let e = csf::events(
                    &sh,
                    c.input + csf::CS_REQ,
                    c.output + csf::CS_ACK,
                    csf::CS_FATAL_EVENT | csf::CS_FAULT_EVENT | csf::CS_TILER_OOM,
                );
                if e & csf::CS_FATAL_EVENT != 0 {
                    self.log(format_args!(
                        "CSG {slot} stream {j}: fatal {:#x} info {:#x}",
                        sh.read32(c.output + csf::CS_FATAL),
                        csf::read64(&sh, c.output + csf::CS_FATAL_INFO)
                    ));
                    faulted.push((slot, 1 << j));
                }
                if e & csf::CS_FAULT_EVENT != 0 {
                    self.log(format_args!(
                        "CSG {slot} stream {j}: fault {:#x} info {:#x}",
                        sh.read32(c.output + csf::CS_FAULT),
                        csf::read64(&sh, c.output + csf::CS_FAULT_INFO)
                    ));
                    db |= 1 << j;
                }
                csf::ack_events(
                    &sh,
                    c.input + csf::CS_REQ,
                    c.output + csf::CS_ACK,
                    e & (csf::CS_FATAL_EVENT | csf::CS_FAULT_EVENT),
                );
                if e & csf::CS_TILER_OOM != 0 {
                    oom.push((slot, j));
                }
            }
            if db != 0 {
                csf::toggle(
                    &sh,
                    b.input + csf::CSG_DB_REQ,
                    b.output + csf::CSG_DB_ACK,
                    db,
                );
            }
            csf::ring_group(&sh, &iface, slot);
        }
        crate::mcu::ring(&self.regs);
        let failed = !faulted.is_empty();
        for (slot, mask) in faulted {
            if let Some((file, h)) = core.csg.get(slot as usize).copied().flatten()
                && let Some(g) = core.files.get_mut(&file).and_then(|f| f.groups.get_mut(&h))
            {
                if mask == u32::MAX {
                    g.timedout = true;
                }
                fail_queues(g, mask);
            }
        }
        for (slot, j) in oom {
            self.tiler_oom(core, slot, j);
        }
        if sync || failed {
            self.wake_all();
        }
    }

    /// A stream's tiler heap ran out (08 §8.4.2): a chunk if the policy allows, else 0, then the
    /// event acknowledged.
    fn tiler_oom(&self, core: &mut Core, slot: u32, j: u32) {
        let Some((file, h)) = core.csg.get(slot as usize).copied().flatten() else {
            return;
        };
        let Some(fw) = core.fw.as_ref() else {
            return;
        };
        let sh = shared(fw);
        let iface = fw.iface;
        let (Ok(b), Ok(c)) = (csf::csg(&sh, &iface, slot), csf::cs(&sh, &iface, slot, j)) else {
            return;
        };
        let addr = csf::read64(&sh, c.output + csf::CS_HEAP_ADDRESS);
        let vt_start = sh.read32(c.output + csf::CS_HEAP_VT_START);
        let vt_end = sh.read32(c.output + csf::CS_HEAP_VT_END);
        let frag_end = sh.read32(c.output + csf::CS_HEAP_FRAG_END);
        let valid = frag_end <= vt_end && vt_end < vt_start;
        let vm_id = core
            .files
            .get(&file)
            .and_then(|f| f.groups.get(&h))
            .map(|g| g.vm);
        let stride = 32u64.max(1 << self.info.l2_line_log2());
        let mut reply = 0u64;
        let mut ok = valid;
        if let Some(vm) =
            vm_id.and_then(|v| core.files.get_mut(&file).and_then(|f| f.vms.get_mut(&v)))
        {
            let (regs, tag) = (&self.regs, &self.tag);
            let (pt, slot_as, bits, krange) = (&mut vm.pt, vm.as_slot, vm.va_bits, &mut vm.krange);
            match vm.heaps.as_mut() {
                Some(pool) if addr >= pool.pool.va => {
                    let idx = ((addr - pool.pool.va) / stride) as usize;
                    match pool.heaps.get_mut(idx).and_then(Option::as_mut) {
                        Some(heap) if valid => {
                            let in_flight = vt_start - frag_end;
                            if in_flight <= heap.target_in_flight
                                && (heap.chunks.len() as u32) < heap.max_chunks
                                && let Ok(k) = mem::kbo(
                                    regs,
                                    tag,
                                    pt,
                                    slot_as,
                                    bits,
                                    krange,
                                    heap.chunk_size,
                                    VM_CACHED,
                                )
                            {
                                reply = k.va | heap.chunk_size >> 12;
                                heap.chunks.push(k);
                            }
                        }
                        _ => ok = false,
                    }
                }
                _ => ok = false,
            }
        }
        if !ok {
            self.log(format_args!(
                "CSG {slot} stream {j}: bad tiler out-of-memory report"
            ));
            if let Some(g) = core.files.get_mut(&file).and_then(|f| f.groups.get_mut(&h)) {
                fail_queues(g, 1 << j);
            }
            self.wake_all();
            return;
        }
        let Some(fw) = core.fw.as_ref() else {
            return;
        };
        let sh = shared(fw);
        csf::write_u64(&sh, c.input + csf::CS_HEAP_START, reply);
        csf::write_u64(&sh, c.input + csf::CS_HEAP_END, reply);
        csf::ack_events(
            &sh,
            c.input + csf::CS_REQ,
            c.output + csf::CS_ACK,
            csf::CS_TILER_OOM,
        );
        csf::toggle(
            &sh,
            b.input + csf::CSG_DB_REQ,
            b.output + csf::CSG_DB_ACK,
            1 << j,
        );
        csf::ring_group(&sh, &iface, slot);
        crate::mcu::ring(&self.regs);
    }

    /// The faulting slots' hardware half (04 §4.9.1 steps 1 to 6), without the core lock: the
    /// fault logged, then the slot switched to "unmapped", so every access of the work on it
    /// faults and the work stops. The slot stays the VM's.
    pub(crate) fn stop_faulted(&self, bits: u32) {
        for slot in 0..16usize {
            if bits & 1 << slot == 0 {
                continue;
            }
            let st = self.regs.read32(mmu::as_reg(slot, mmu::AS_FAULTSTATUS));
            let addr = self.regs.read64(mmu::as_reg(slot, mmu::AS_FAULTADDRESS));
            self.log(format_args!(
                "MMU fault on address space {slot}: status {st:#x} at {addr:#x}"
            ));
            if slot != 0 {
                let _ = mmu::disable(&self.regs, &mut Spin, slot);
            }
        }
    }

    /// The faults' bookkeeping: the VM on each slot marked faulted, and every queue of its groups
    /// fatal (04 §4.9.2: no specific fault reported is assumed).
    pub(crate) fn mmu_events(&self, core: &mut Core, bits: u32) {
        for slot in 1..16usize {
            if bits & 1 << slot == 0 {
                continue;
            }
            if let Some((file, vm)) = core.as_slots.get(slot).copied().flatten()
                && let Some(f) = core.files.get_mut(&file)
            {
                if let Some(v) = f.vms.get_mut(&vm) {
                    v.faulted = true;
                }
                for g in f.groups.values_mut().filter(|g| g.vm == vm) {
                    fail_queues(g, u32::MAX);
                }
            }
        }
        self.wake_all();
    }
}

/// Marks the queues of `mask` fatal and completes every job they were given, with an error
/// status: a job that cannot finish still signals its fence, so no waiter is left waiting on a
/// dead context (07 §7.10: the jobs of a faulted group fail).
fn fail_queues(g: &mut Group, mask: u32) {
    let mask = mask & ((1u32 << g.queues.len()) - 1);
    g.fatal_queues |= mask;
    for (k, q) in g.queues.iter().enumerate() {
        if mask & 1 << k != 0 {
            g.syncs.mem.write_at::<u32>(16 * k + 8, u32::MAX);
            g.syncs.mem.write_at::<u64>(16 * k, q.seq);
        }
    }
}
