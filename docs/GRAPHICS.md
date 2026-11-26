# Graphics

- Status: **Draft**. A forward design for M-G. Decisions here become an ADR at the start of M-G,
  together with ROADMAP open decisions 6 (graphics ABI) and 7 (drivers ported from Linux)
- Date: 2026-10-27
- Scope: the kernel side of graphics and display: buffer memory, synchronization, job submission,
  modesetting, and the user interface to them. Video codecs, NPUs, and audio are out of scope (§13)
- Inputs: [DRIVER-MODEL.md](DRIVER-MODEL.md) (§5.3, §8.4–§8.6, §9, §13.3, §17 question 6), the
  clean-room specifications in `~/Projects/OS/chinese-wall/specs/` (`panthor`, `linlon-dpu`,
  `sky1-dp-tx`, `adreno-a690`, `qcom-mdss-dpu`), and FreeBSD with `drm-kmod` as userland sees it

The short version: **the DRM user interface as FreeBSD's ports see it, implemented natively in Rust,
with no LinuxKPI.** Userland (libdrm, Mesa, Wayland compositors, Xorg) runs unmodified from FreeBSD
packages. Inside, buffers are VM objects with a device pager, device address spaces are owned by
the DMA layer, fences are a small crate whose signalling paths cannot block on memory, and display
hardware is an aggregate of drivers behind a KMS class.

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal | Why |
|---|---|---|
| G1 | **FreeBSD's graphics userland runs unmodified**: `pkg install mesa-dri mesa-libs libdrm wayland sway` works as on FreeBSD with `drm-kmod` | The ABI target is FreeBSD userland ([ADR-0004](adr/0004-abi-target.md)). Vulkan and GL drivers are far beyond what we can write; Mesa already has panvk, Turnip, RADV, and Venus |
| G2 | **Drivers are safe Rust** and follow the driver model: framework-owned lifecycle, gates, grants | [DRIVER-MODEL.md](DRIVER-MODEL.md) G2, G4 |
| G3 | **No fence deadlock by construction**: the paths that signal fences cannot wait on memory reclaim | The defining bug class of GPU drivers (panthor 08 §8.4.2) |
| G4 | **A device can go away**: unbind, reset, or hang recovery never leaves userland holding memory the device can still reach, and never crashes the kernel | DRIVER-MODEL.md G2, G7 |
| G5 | **Performance parity** with FreeBSD and `drm-kmod` on the same machine, where FreeBSD has the driver | [ADR-0011](adr/0011-performance-parity.md), [ADR-0014](adr/0014-performance-parity-per-component.md) |
| G6 | **Both ISAs from the first driver**: nothing assumes x86 PAT, snooping, or a discrete GPU | Sky1 and the Q8B are non-coherent integrated GPUs; the X870E's are discrete and integrated |

### 1.2 Non-goals

- **LinuxKPI and `drm-kmod`.** Open decision 7 leans against them. This design assumes that, and
  needs only the user interface `drm-kmod` defines, not its internals.
- **Linux interfaces outside DRM**: sysfs, udev, uevents, fdinfo, debugfs. Where Mesa or a
  compositor needs what they give on Linux, we provide FreeBSD's equivalent (sysctl, devd).
- **fbdev** (`/dev/fb*`) for userland. The kernel console keeps its own path (§7.6).
- **New userland graphics APIs.** M-D's compositor (Scoria's `scoriad`, on libdrm, GBM, and
  Vulkan) uses the same interfaces as everything else.

## 2. The ABI decision

**Proposal: DRM-compatible, as FreeBSD userland sees it.**

- Userland reaches GPUs through `/dev/dri/card<N>` (primary, modesetting) and
  `/dev/dri/renderD<128+N>` (render-only) device nodes, with the DRM ioctls, `read()` for events,
  `mmap()` at offsets the driver hands out, and `poll`/kqueue.
- **Request numbers are FreeBSD's.** libdrm on FreeBSD is built with `<sys/ioccom.h>`, whose
  direction bits and 13-bit size field differ from Linux's `_IOC`. The dispatcher decodes
  FreeBSD's encoding. It never assumes Linux numbering.
- **Structure rules are DRM's.** A caller may pass a structure shorter or longer than the
  kernel's. Shorter is zero-extended; longer must have zero tail bytes (panthor 10 §10.1.2).
  Object arrays carry a stride (panthor 10 §10.1.3).
- **Driver identity is Linux's driver name** in `DRM_IOCTL_VERSION` (`panthor`, `msm`, `amdgpu`,
  `virtio_gpu`), because Mesa's loader selects its driver by it. These names go in
  `compat-deviations.md` only where FreeBSD's `drm-kmod` would report something else.
- **Discovery is FreeBSD's.** How libdrm on FreeBSD finds devices and bus information (devfs names,
  `hw.dri.*` and `dev.drm.*` sysctls) is **to record** from FreeBSD 15.1 with `drm-kmod` on the
  build server, and reproduced byte for byte, as `devinfo` is.

**Why not a new interface.** It would mean new Vulkan and GL drivers, or a translation layer in
userland, for every GPU. That is several person-decades of Mesa. M-D's compositor runs on libdrm,
GBM, and Mesa's Vulkan drivers from ports ([DESKTOP.md](DESKTOP.md) §6) and gains nothing from a new kernel
interface that DRM/KMS lacks.

**Sources.** We may use DRM's user interface headers (`include/uapi/drm/*.h`, MIT), libdrm (MIT),
Mesa (MIT), and the clean-room specifications. Kernel sources are used only where the file is
under MIT or BSD terms, checked per file (amdgpu's are largely MIT; NVIDIA's open kernel modules
are dual MIT/GPL). GPL-only DRM core and driver files are never read
([ADR-0007](adr/0007-license-and-clean-implementation.md)).

## 3. Overview

```
 userland    Mesa (panvk, Turnip, RADV, Venus)   libdrm   compositor / Xorg   kmscube   igt
──────────────────────┬──────────────────────────────────────────┬───────────────────────────────
 files                │ /dev/dri/cardN  /dev/dri/renderD128+N    │ dma-buf fds, sync-file fds
                      │ (DRM file: handle tables, events, master)│ (file types: SCM_RIGHTS, kqueue)
──────────────────────┴──────────────────────────────────────────┴───────────────────────────────
 sys/drm      ioctl dispatch · GEM handles · PRIME · syncobj · KMS atomic core · vblank · leases
              render class (RenderDevice)             │ kms class (KmsDevice; CrtcHw, PlaneHw, …)
──────────────────────────────────────────────────────┴──────────────────────────────────────────
 sys/fence    Fence · Timeline · Reservation · SyncFile · FenceSignalCx
 sys/vm       BufferObject (VM object + device pager) · DmaBuf · reclaim hooks
 sys/vm::dma  IoAddressSpace · IoPageTable<F> · BusAddr · ReservedRegion       (+ sys/iommu, P11)
──────────────────────────────────────────────────────────────────────────────────────────────────
 drivers      gop-kms   virtio-gpu   panthor   linlondp + sky1-dp + phy + panel   amdgpu   msm …
──────────────────────────────────────────────────────────────────────────────────────────────────
 libs         lib/edid · lib/dp (AUX, link training) · lib/drm-fourcc · lib/kms-check (host-tested)
```

| Crate | Contents | `unsafe` |
|---|---|---|
| `sys/drm` | DRM files and ioctls, GEM handle tables, PRIME, syncobj, the KMS object model and atomic core, vblank, master and leases, the render and KMS class traits | none beyond the char class |
| `sys/fence` | Fences, timelines, reservations, sync files, the signalling context | small (callback lists) |
| `sys/vm` (extended) | `BufferObject`, the device pager, `DmaBuf`, reclaim integration | yes (it is the VM) |
| `sys/vm::dma` (extended) | `IoAddressSpace`, `IoPageTable<F>`, `BusAddr` | yes |
| `lib/edid`, `lib/dp`, `lib/drm-fourcc`, `lib/kms-check` | Pure logic: EDID and DisplayID, DP link training state machines, pixel formats and modifiers, atomic-state validation | forbidden |
| `drivers/*` | GPU and display drivers | **forbidden** |

## 4. Memory

### 4.1 Buffer objects

A **`BufferObject`** is a VM object (FreeBSD's `vm_object_t` role) with a device pager. It is the
single type behind GEM handles, dma-bufs, scanout buffers, and GPU allocations.

| Backing | Used for | Pages |
|---|---|---|
| `System` | Most buffers on integrated GPUs (Sky1, Q8B, Radeon iGPU) | Ordinary pages, allocated at first fault or first pin, swappable while unpinned |
| `Contiguous` | Scanout without an IOMMU, firmware buffers | From a `ReservedRegion` or a contiguous allocator |
| `DeviceLocal` | VRAM on discrete GPUs | A range of a BAR, managed by the driver's allocator; evicted to `System` |
| `Imported` | A dma-buf from another device | The exporter's object, attached (§4.4) |

- **Unified and discrete memory.** The same buffer type serves both kinds of machine. On a
  system on a chip whose display and graphics engines share the processor's memory (Sky1, the Q8B,
  the Radeon iGPU), a buffer is `System` pages and moving it between devices is a matter of mapping
  the same pages, not copying. On a discrete card it may be `DeviceLocal`, and sharing it makes it
  `System`. Two more properties are chosen **per buffer by the driver**, not assumed from the
  architecture: the **cache mode** of its CPU mappings (write-back, write-combined, uncached) and
  whether devices see the processor's writes without cache maintenance (**coherent**). The Sky1 and
  Q8B display and graphics engines do not snoop, so their buffers are `System` and not coherent:
  the core calls the driver's `sync_for_device` before a commit shows one, and `sync_for_cpu`
  before the processor reads what a device wrote. A snooping device (any x86 machine's) is
  coherent and costs nothing. `Placement` (backing, cache mode, coherent) is data the driver gives
  when a buffer is made; only cached `System` pages are built so far, and the others refuse with
  `EOPNOTSUPP` until the virtual memory support they need exists.
- **Lifetime.** A buffer is counted, held by GEM handles, dma-buf files, CPU mappings, device
  mappings, and in-flight jobs. It is **not** binding memory: it may outlive the driver that made
  it (DRIVER-MODEL.md §4.1). After the binding goes, its device mappings are gone (§4.3) and CPU
  faults on `DeviceLocal` pages deliver `SIGBUS`, as FreeBSD does for a destroyed device's pager.
- **Pinning.** A buffer the device is using right now (scanout, an active job) is pinned. Unpinned
  buffers may be evicted. Pin counts belong to the binding's ledger.
- **Labels and accounting.** Per-file and per-device totals are exported through sysctl
  (`kern.abyss.drm.*`), in place of fdinfo.

### 4.2 CPU mappings

- `mmap` on a DRM file at an offset the driver returned (`*_MMAP_OFFSET`) resolves, through the
  file's offset table, to the buffer. This is FreeBSD's `d_mmap_single` returning a VM object, and
  [DRIVER-MODEL.md §9.1](DRIVER-MODEL.md#91-classes)'s `CdevOps::mmap` should be specified the
  same way when the char class is built.
- Each mapping has a **cache attribute** chosen by the driver per buffer: write-back, write-combined,
  or uncached. It is fixed while the buffer has any CPU mapping.
- **Population is at fault time**, under the object lock, never under a fence wait.
- **Revocation.** `BufferObject::revoke_cpu_mappings()` tears down every user mapping, for eviction,
  for a moving `DeviceLocal` buffer, and for power transitions. The next access faults it back in.
- **Explicit cache maintenance** for non-coherent devices: `sync_for_cpu(range)` and
  `sync_for_device(range)`, behind the driver's `BO_SYNC` and dma-buf's `DMA_BUF_IOCTL_SYNC`.
- **User-mapped device pages.** A read-only page of a granted register window (panthor's
  `LATEST_FLUSH_ID`) is a `UserMmioPage` grant served by the same pager. It is swapped for a page of
  RAM holding a fixed value while the device is powered down, and its mappings are revoked on every
  power transition.

### 4.3 Device address spaces

GPUs translate through their own page tables, or through a system IOMMU context the driver
switches. Both are owned by the DMA layer, not the driver.

```rust
/// An address space a device translates through: a GPU VM, an SMMU context bank, a firmware window.
pub struct IoAddressSpace<F: PteFormat> { /* tables, IOVA allocator, ASID */ }

/// A page-table format: levels, granule, block sizes, and how one entry encodes an address.
pub trait PteFormat {
    const LEVELS: u8;
    const GRANULE: PageSize;
    fn encode(addr: BusAddr, attrs: PteAttrs) -> u64;   // safe: BusAddr is opaque
}

impl<F: PteFormat> IoAddressSpace<F> {
    pub fn map(&self, va: IoVa, bo: &BufferObject, range: PageRange, prot: Prot) -> Result<(), DmaError>;
    pub fn unmap(&self, va: IoVa, len: ByteCount) -> Unmapped;      // the TLB flush is a fence-able token
    pub fn root(&self) -> DeviceTableRef;                           // opaque; written into a register or ring
}
```

- **`BusAddr`** is opaque. It comes only from a mapping, and can be written only into a `PteFormat`
  entry or a register field typed as an address. Drivers still never see a physical address
  (DRIVER-MODEL.md §8). A user-visible device address (the NPU returns one) requires an IOMMU
  behind the device.
- **Formats** are data the driver supplies: Mali's AArch64-style tables (panthor 04), Adreno
  through per-process SMMU TTBR switching (`CP_SMMU_TABLE_UPDATE`, adreno-a690 03 §4), AMD's GPUVM.
  The P11 SMMUv3 stage-1 backend uses the same AArch64 format, so it is one implementation.
- **`VM_BIND`.** Binding and unbinding are asynchronous operations with in-fences and out-fences
  (panthor 10 §10.4.3, msm). Page-table memory for a bind is allocated **before** the operation
  enters the fence-signalling path (§5.3).
- **Fixed windows and holes** (the NPU's 4 GiB windows less their top GiB, the Q8B VPU's TZ-reserved
  low 600 MiB, the GMU's fixed IOVAs) are constraints on the address space's IOVA allocator.

### 4.4 Sharing: dma-buf

- A **dma-buf** is a file (`DTYPE_DMABUF`, an AbyssBSD addition; FreeBSD's `drm-kmod` implements it
  inside LinuxKPI), so it passes over `SCM_RIGHTS`, can be `mmap`ed, and works with kqueue.
- **PRIME** (`PRIME_HANDLE_TO_FD`, `PRIME_FD_TO_HANDLE`) exports a GEM handle as a dma-buf and
  imports one as a handle. Importing a buffer the file already has returns the same handle.
- **Attachments.** Each importing device attaches, and gets its own device mapping of the
  exporter's buffer. The exporter decides placement: a `DeviceLocal` buffer shared with another
  device migrates to `System` and is pinned while attached.
- **Coherency between devices.** The producer's fence is the hand-off point. If the consumer does
  not snoop (Sky1's and the Q8B's display controllers), the producer's completion includes the
  cache clean. This is a property of the attachment, not a driver decision.
- **Implicit sync.** Each buffer has a reservation (§5.2). Userland that does not use explicit
  sync gets correct ordering from it, as on Linux and FreeBSD.

### 4.5 Reclaim

- Evictable buffers register with the VM's pageout as a **reclaim source**: under pressure,
  idle unpinned buffers are evicted (VRAM) or their pages swapped out (`System`).
- Reclaim **may wait on fences**. So nothing that signals a fence may wait on reclaim. §5.3 makes
  that a type rule.

## 5. Synchronization

### 5.1 Objects

| Object | What it is | User-visible as |
|---|---|---|
| `Fence` | One-shot completion with an error, a timeline, and a sequence number. Callbacks run in the signaller's context | Inside syncobjs, sync files, and reservations |
| `Timeline` | Ordered sequence of fences from one ring, group, or context | — |
| `SyncObj` | A container for one fence (binary) or a timeline of points | A per-file handle: the DRM syncobj ioctls, including `TIMELINE_WAIT`, `TRANSFER`, `QUERY` |
| `SyncFile` | A file holding one fence (possibly merged) | An fd: `poll`/kqueue `EVFILT_READ` signal on completion, `SYNC_IOC_MERGE`, `SYNC_IOC_FILE_INFO` |
| `Reservation` | Per-buffer list of fences by usage (kernel, write, read, bookkeeping) | Implicit sync; `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` and `IMPORT_SYNC_FILE` |

- Waits take absolute `CLOCK_MONOTONIC` deadlines, as the DRM ioctls do.
- **Deadline hints** from a waiter (a compositor waiting for the next vblank) propagate to the
  fence's driver, which may raise its frequency (`PerfDomain`, DRIVER-MODEL.md §9.3).
- A syncobj can be waited on through an eventfd (FreeBSD has eventfd), for compositors' event loops.

### 5.2 Locking several buffers

A submission locks the reservations of every buffer it uses. That needs a **wound-wait mutex**
with an acquire context: when two submitters lock overlapping sets in different orders, the
younger backs off, drops what it holds, and retries. `sys/sched` gains a `WwMutex` with an
`AcquireCx`, checked by the witness. FreeBSD has no counterpart outside LinuxKPI.

### 5.3 The signalling rule, as a type

Between the point a fence is published (another thread can wait on it) and the point it signals,
the code must not:

- allocate memory with a call that may sleep for reclaim;
- take a lock that is ever held while allocating that way;
- wait on another fence that could depend on this one.

In Linux this is a lockdep annotation. Here it is a context token: **`FenceSignalCx`**, created when
a job is armed and consumed when its fence signals. It cannot produce `Sleepable`, and the
allocator's sleeping entry points require `Sleepable`. Allocation on this path uses pre-reserved
pools (`KBox::try_new_in(pool)`). The scheduler's run, timeout, and completion paths take
`FenceSignalCx`. The witness additionally treats "held in fence-signalling context" as a lock class,
so a lock taken both there and around a sleeping allocation is a witness error.

## 6. Job submission

GPUs differ in who schedules: the kernel feeds rings (virtio-gpu, older AMD), or firmware
schedules groups the kernel binds to slots (panthor's CSF, Adreno's ringbuffers with GMU
assistance, AMD's MES, NVIDIA's GSP). `sys/drm` provides the common part once:

- **An entity per queue** (a panthor group queue, a ring), with priority from the DRM rules
  (`PANTHOR_GROUP_PRIORITY_*`, `capable` checks mapped to FreeBSD privileges).
- **Dependency resolution.** A job is held until its in-fences signal. Waiting is a fence
  callback, not a sleeping thread.
- **Run.** When ready, the job is handed to the driver's `RenderDevice::run(job, &mut
  FenceSignalCx) -> Fence`. For firmware schedulers, `run` writes to the ring and rings the doorbell.
- **Timeouts.** Each job has a deadline (panthor's default is in 07 §7.11). A timeout calls the
  driver's `timed_out`, which recovers in place (reset the engine, reload firmware, re-bind slots,
  DRIVER-MODEL.md §13.3) and reports which context was guilty. Guilty contexts are banned and
  their later submissions fail with `ECANCELED`, as on Linux.
- **No thread hop on the submit path.** Submission runs in the ioctl's thread up to the doorbell,
  unless a dependency is pending. Completion runs in the interrupt's filter or ithread
  (DRIVER-MODEL.md §6.5).
- Render drivers are `Concurrency::Concurrent`. Under `Serialized`, every ioctl would serialize
  with the interrupt threads.

## 7. Display (KMS)

### 7.1 Object model

The atomic KMS model: **CRTCs**, **planes** (primary, overlay, cursor), **encoders**,
**connectors**, **bridges** and **panels**, each with typed properties. Userland sees DRM's object
IDs, property IDs, and blobs. Legacy (non-atomic) ioctls are implemented on top of atomic commits,
as on Linux.

### 7.2 Hardware traits and aggregates

Display hardware is usually several devices (DRIVER-MODEL.md §5.3): on Sky1, the DPU (`CIXH5010`),
the DP transmitter (`CIXH502F`), the USB-DP combo PHY, an eDP panel (`CIXH5040`), and a backlight
(`CIXH5041`). Each member is a driver that publishes a narrow trait:

| Trait | Implemented by | Main operations |
|---|---|---|
| `CrtcHw` | DPU pipelines | `check(&CrtcState)`, `commit(&CrtcState, &mut CommitCx)`, vblank enable, scanout position |
| `PlaneHw` | DPU layers | formats and modifiers, `check`, `commit` |
| `EncoderHw` | DP-TX, HDMI, DSI | `mode_valid`, `enable(mode)`, `disable`, link training (with `lib/dp`) |
| `ConnectorHw` | DP-TX, panels | `detect`, EDID read, hot-plug interrupts, HPD-IRQ |
| `PanelHw`, `BacklightHw` | panel and backlight drivers | power sequencing, brightness |

A **synthetic aggregate node** (DRIVER-MODEL.md §5.5) attaches when its members have. It owns the
KMS publication, builds the object graph from firmware (`_DSD` graph endpoints, or the platform
description), and runs commits. A member that detaches takes the aggregate down first (graph
order), and the KMS device is revoked through its gate.

### 7.3 Atomic commits

- `check` is pure: `lib/kms-check` validates the generic rules (plane-CRTC compatibility, format
  and modifier support, scaling limits, bandwidth when the driver gives a model) on the host. The
  driver's `check` adds hardware limits.
- Commits may be **nonblocking**. They wait on their buffers' fences (`IN_FENCE_FD`), program the
  hardware at the next vblank, and signal `OUT_FENCE_PTR` and page-flip events.
- **Events** (vblank, flip complete, `CRTC_SEQUENCE`) are read from the DRM file, as on Linux.

### 7.4 Connectors, hotplug, and EDID

- Hot-plug interrupts and DP HPD-IRQs are handled in the connector driver's thread handler. They
  produce a devd notification (the exact `system`/`subsystem`/`type` strings are **to record**
  from FreeBSD's `drm-kmod`) and the DRM hotplug event that compositors listen for.
- EDID and DisplayID are parsed by `lib/edid`, which is host-tested and fuzzed. Monitor data is
  untrusted input.
- DP over USB-C needs the Type-C event provider and PHY lane leasing (DRIVER-MODEL.md §9.3).

### 7.5 Firmware handover

UEFI leaves a display running (GOP). The design keeps it lit:

1. Boot: the kernel console draws on the GOP framebuffer (`fb-console`, as today).
2. **`gop-kms`**, a minimal KMS driver over the GOP framebuffer (one CRTC, one plane, one
   connector, no modes but the current one), is the first KMS device. It gives compositors a
   `/dev/dri/card0` on any UEFI machine and in QEMU, like Linux's simpledrm.
3. When a native display driver attaches for the same output, it **adopts** the running state
   (attach input `FirmwareState::LeftRunning`: no reset, read the current mode and scanout
   address), and `gop-kms` detaches. On the Q8B the SMMU's boot identity mapping for the display,
   and on Sky1 the IORT reserved-memory region for the GOP splash, stay until the first native
   modeset (DRIVER-MODEL.md §8.4).
4. The console moves with it (§7.6).

### 7.6 Kernel console

FreeBSD's `vt` picks among framebuffer back ends by priority (`vt_allocate`/`vt_deallocate` in
`sys/dev/vt/vt_core.c`): `vt_efifb` at boot, then `vt_fb` when a DRM driver registers a
framebuffer. AbyssBSD's `sys/vt` does the same: a KMS device publishes a console framebuffer, `vt`
switches to it, and switches back to the GOP buffer if the device detaches. When a DRM master holds
the display, `vt` stops drawing (`KDSETMODE` `KD_GRAPHICS`) as on FreeBSD, and panics take the
display back.

### 7.7 Backlight

Backlights are FreeBSD's `backlight(9)` devices (`/dev/backlight/backlight<N>`,
`<sys/backlight.h>`), so `backlight(8)` works. The DRM connector also exposes them as a property.

## 8. Device files and policy

- **Primary nodes** (`card<N>`) allow modesetting by the **DRM master** only. The first opener
  becomes master. `SET_MASTER`/`DROP_MASTER` need the same privilege checks as FreeBSD's
  `drm-kmod` (to record). Leases (`MODE_CREATE_LEASE`) let a compositor give a VR headset or
  another client its own CRTCs.
- **Render nodes** (`renderD<128+N>`) never modeset and never authenticate. Any process with access
  may submit work.
- **Permissions** come from devfs (`devfs.rules`; the `video` group, as FreeBSD's ports expect).
- **Capsicum.** An open render node, dma-buf, and sync file work in capability mode, so a sandboxed
  client can render. That needs `cap_rights` for their ioctls.
- **Jails.** Device nodes are hidden from jails unless devfs rules expose them, as on FreeBSD.

## 9. Drivers and order

| Order | Driver | Hardware | Why here |
|---|---|---|---|
| 1 | `gop-kms` | Any UEFI machine, QEMU, bhyve | Exercises the DRM file, GEM, KMS core, events, and console handover with no GPU |
| 2 | `virtio-gpu` 2D, then 3D (Venus, virgl) | QEMU | The test vehicle for fences, syncobj, dma-buf, and submission. Venus needs host-visible blob memory |
| 3 | Sky1 display: `linlondp` + DP-TX + combo PHY + eDP panel + backlight | Orange Pi 6 Plus | Specifications exist; the Tier 1 arm64 board's only display path |
| 4 | `panthor` (Immortalis-G720) | Orange Pi 6 Plus | Specifications exist; the hardest test of firmware loading, device MMUs, and the scheduler |
| 5 | amd64 native: RDNA2 (Radeon 610M iGPU, Navi 22) | X870E, Z690-A | §13 question 1; the desktop's metal target (DESKTOP.md §13) |
| Tier 2 | `msm` (Adreno 690 + DPU1) | Radxa Dragon Q8B | Specifications exist; depends on the PEP providers and the SMMUv2 backend |

The ROADMAP's M-G entry names RDNA2 as the first native GPU. The X870E has no Intel GPU: its boot
GPU is an NVIDIA RTX 4070, and its CPU has an RDNA2 iGPU.

## 10. Prerequisites from earlier phases

| Need | Where |
|---|---|
| Firmware store (GPU firmware by GPU ID) | DRIVER-MODEL.md §8.6 (P8) |
| Clock, reset, power, and SCMI providers; the platform description | DRIVER-MODEL.md §5.5, §8.5, §9.3 (P8) |
| Runtime PM with driver-held `PowerRef` | DRIVER-MODEL.md §8.5 (P11) |
| `PerfDomain` (GPU DVFS) and thermal capping | DRIVER-MODEL.md §9.3 (P11) |
| IOMMU backends (SMMUv3, SMMUv2) and device address spaces | DRIVER-MODEL.md §8.4 (P11) |
| `SCM_RIGHTS`, kqueue, eventfd, shared memory | P6 |
| evdev (compositors read input from `/dev/input/event*`) | `sys/evdev` (USB track) |
| VM objects with pagers, swap | P5, P11 |

## 11. Testing

- **Host**: `lib/edid` and `lib/dp` against captured EDIDs and DP traces (fuzzed); `lib/kms-check`
  against recorded atomic states; fence, timeline, and wound-wait algorithms model-checked with the
  in-house loom-style checker.
- **Differential**: the DRM ioctl results from libdrm's test programs (`modetest`, `drmdevice`) and
  `kmscube` on AbyssBSD and on FreeBSD with `drm-kmod`, on the same VM or machine.
- **Conformance**: igt-gpu-tools (MIT) for the DRM core and KMS, Vulkan CTS and dEQP for Mesa
  drivers, run the way kyua is: per test case against FreeBSD's result, or Linux's where FreeBSD
  has no driver, with known failures listed.
- **Detach stress**: unbinding a GPU under load leaves no mapping the device can reach, and userland
  sees `ENODEV` and `SIGBUS`, never a kernel fault (G4).

## 12. Performance

Components (ADR-0014), each against FreeBSD with `drm-kmod` on the same machine:

- `drm.submit`: submission rate and latency of empty and small jobs (virtio-gpu, amdgpu);
- `drm.bo`: buffer create, map, fault, and free;
- `drm.prime`: export and import, and cross-device scanout of a rendered buffer;
- `drm.flip`: page-flip latency to the vblank, and compositor frame time under `sway`;
- `drm.fence`: wait latency for an already-signalled and a just-signalled fence;
- application benchmarks (`vkmark`, `glmark2`, including the jellyfish scene of the acceptance
  capture, §14) as diagnostics.

Where FreeBSD has no driver (panthor on Sky1, msm on the Q8B), there is no baseline, and
[PERFORMANCE.md](PERFORMANCE.md) has no rule for that case yet (§13 question 7). Until it does,
those results are recorded and do not gate.

## 13. Open questions

1. **The amd64 native GPU.** Decided in [ADR-0018](adr/0018-desktop-architecture.md), which
   [ADR-0026](adr/0026-desktop-scoria.md) keeps: RDNA2 (amdgpu sources largely MIT, FreeBSD has
   `drm-kmod` amdgpu for a baseline). The Z690-A has a Navi 22 card, and the X870E and B650E have
   RDNA2 iGPUs. The
   RTX 4070 (NVIDIA's open kernel modules are dual MIT/GPL, GSP firmware does most of the work,
   and FreeBSD's baseline is NVIDIA's own driver) comes later.

   **2026-11-24:** both X870E GPUs were brought up far enough to read them, and the owner has chosen to
   carry the **Radeon first, on the iGPU**, to completion (§16), matching the ADRs. The RTX 4070
   (`drivers/drmn`'s NVIDIA half, `lib/nvgpu`, the clean-room `nvidia-nova` specification) stays where it
   stopped: identified, its Video BIOS and firmware files read, and the firmware security microcode's FRTS
   command run once on request (§9 of HARDWARE.md); the GSP boot waits.
   On FreeBSD a native GPU driver is a child of the PCI function's `vgapci` (`drmn0` under `vgapci0`
   in the QEMU corpus); the native drivers attach the same way.

2. **FreeBSD's discovery and policy details** (§2, §7.4, §8): sysctl names, devd strings, master
   privileges, device node modes. Record them from FreeBSD 15.1 with `drm-kmod` before writing
   `sys/drm`.
3. **Video codecs and NPUs.** The Linlon VPU and Iris speak V4L2 memory-to-memory; FreeBSD has no
   V4L2 in the kernel. The Zhouyi NPU has its own character device. They share §4 and §5 but need
   their own user-interface decision. A separate media document.
4. **`DTYPE_DMABUF` and `DTYPE_SYNCFILE`** are AbyssBSD additions. Does any FreeBSD tool (`fstat`,
   `procstat -f`) need to show them a particular way?
5. **Display Stream Compression, MST, VRR, HDR, and color management.** Not needed for the first
   drivers; the property model leaves room.
6. **Scheduler priority and privilege.** Which FreeBSD privilege stands for Linux's
   `CAP_SYS_NICE` in high-priority GPU contexts.
7. **Parity without a FreeBSD baseline.** ADR-0014 assumes FreeBSD runs the component. For drivers
   FreeBSD lacks, either the component does not gate, or it gates against another FreeBSD
   measurement of the same work (software rendering, or the same Mesa workload on a GPU FreeBSD
   does drive). Decided with the M-G ADR.

## 14. Phasing within M-G

M-G is accepted on **stock Wayland and Sway**: FreeBSD 15.1 packages (`pkg install sway foot
fastfetch glmark2 grim`), unmodified, with no AbyssBSD patches or settings beyond the renderer
choice in `WLR_RENDERER`. Scoria's `scoriad` is M-D's acceptance, not M-G's (DESKTOP.md §15).

Each step that puts a display up exits with the same **acceptance capture**: a `grim` screenshot of
a Sway session, taken on the machine under test and kept with the run's results, showing two
windows in their Sway frames:

1. a `foot` terminal running `fastfetch`, which names the OS (as FreeBSD 15.1-RELEASE: the compatibility contract keeps `uname -s` and `os-release` FreeBSD, docs/compat-deviations.md), the CPU, and the GPU;
2. glmark2's 3D jellyfish scene (`glmark2-es2-wayland -b jellyfish:show-fps=true --run-forever`),
   with its frame rate on screen.

The run's log records glmark2's `GL_RENDERER` and Sway's renderer, so the capture says what drew
it. Frame rates are diagnostics (§12), not gates.

| Step | Work |
|---|---|
| **M-G.0** | This document finalized, the ADR for open decisions 6 and 7, and FreeBSD `drm-kmod` behavior recorded (§13 question 2) |
| **M-G.1** | `sys/drm` core (files, GEM, KMS atomic core, events), `gop-kms`, `vt` handover. Exit: the acceptance capture on `gop-kms` in QEMU and on both reference machines, with software rendering (Sway on `pixman`, jellyfish on llvmpipe) |
| **M-G.2** | `sys/fence`, syncobj, sync files, dma-buf, `BufferObject` and its pager, `virtio-gpu` 2D. Exit: igt's core tests pass in QEMU, and the acceptance capture on `virtio-gpu` 2D with software rendering |
| **M-G.3** | `virtio-gpu` 3D (Venus), with dma-buf import and export through DRM format modifiers. Exit: `vkcube` and Vulkan CTS subsets in QEMU, and the acceptance capture accelerated: Sway on wlroots' Vulkan renderer over Venus (`WLR_RENDERER=vulkan`, with explicit sync), jellyfish on Zink over Venus |
| **M-G.4** | Sky1 display aggregate. Exit: native modesetting on the Orange Pi 6 Plus, hotplug, backlight, and the acceptance capture on the Sky1 display with software rendering |
| **M-G.5** | `panthor`. Exit: panvk runs `vkcube` and the CTS subset on the Orange Pi 6 Plus, and the acceptance capture there accelerated: Sway on Vulkan over panvk, jellyfish on panfrost |
| **M-G.6** | amd64 native GPU (RDNA2). Exit: parity with FreeBSD `drm-kmod` on the X870E and the Z690-A, and the acceptance capture on each accelerated: Sway on Vulkan over RADV, jellyfish on radeonsi. The desktop running accelerated is M-D.4, which follows |

## 15. Sky1 display: state and what the board must answer

`drivers/linlondp` (the Linlon-D6 display processor) is built and host tested against a simulated
device written from the same clean-room specification, so the tests show the driver agrees with the
specification and with itself, not that the silicon does. Built: register model, block discovery,
mode to timing registers (modes that overflow a field are refused, not truncated), one display's data
path with the two-phase stop, interrupt decode, and the card (one display controller per pipeline, one
plane per layer, linear buffers one to one, non-coherent dumb buffers). Not built: the kernel glue
(registration as an ACPI driver for `CIXH5010`, clocks, interrupt, memory mapping through the SMMU),
the firmware-adoption path, scalers, rotation, compressed buffers, colour management, write-back,
side-by-side composition, and every encoder (DP-TX, combo PHY, eDP panel, backlight).

From the board's ACPI dump (`tests/corpus/opi6p/freebsd/acpidump-dt.txt`): pipeline clocks are named
`pipeline@0` and `pipeline@1` in `CLKT`, the main clock `aclk` is pinned at 800 MHz
(`aclk_freq_fixed`), `enabled_by_gop` is 0, `_CCA` is 0 (not coherent), and power is an ACPI power
resource that the ACPI bus switches on before attach. The firmware leaves DPU 4 running 1920x1080
(DisplayPort 4), so adoption matters on this board.

Open questions for the board session (the specification leaves them open):

1. Whether a second config-valid write before the first latches is safe (03 §3.4); the card avoids it.
2. Whether the config-valid readback (1 until latched) holds on the D6 as the vendor notes say; the
   two-phase stop and the first enable poll it.
3. Where `BS_PROG_LINE` counts from, and whether the programmed-line interrupt falls after the latch
   (the card completes commits on it).
4. Whether the display takes the 5 DPU stream IDs with one translation (08 §8.2.1) and how a buffer's
   pages are mapped for it (the card asks the platform for a device address per buffer).
5. The driver name Mesa pairs with: `komeda` is the FreeBSD port's and the specification's; not yet
   checked against the Mesa in the 15.1 packages (the freedesktop host refuses automated fetches).
6. A buffer's pages must stay resident while mapped; the core has no pin yet.

### 15.1 DisplayPort transmitter (`drivers/sky1-dp-tx`)

Built and host tested against a simulated core and sink (same caveat as above): register model, the
AUX channel (chunking, defer and timeout retries, I2C over AUX, EDID), sink capability parsing, rate,
lane and colour-depth selection, link training (clock recovery, equalisation with TPS2/3/4, rate
fallback, a clean end), the video stream registers, and bring-up and teardown of one SST link. The PHY
is a trait (`link::Phy`) for the combo-PHY (USB-C) and eDP-PHY drivers, which are not written.

Not built: the kernel glue (ACPI `CIXH502F`, clocks, reset, interrupt thread, HPD work), the `Phy`
implementations and Type-C lane muxing, the RCSU video adapter, `EncoderHw`/`ConnectorHw` over it,
firmware adoption, the eDP panel and backlight, multi-stream, audio, secondary data packets, panel
self refresh, variable refresh, HDCP, fast retraining, YCbCr output.

Questions for the board: whether the sync inversion (`Config::invert_sync`, specification 02 §2.6.1)
is right for a sink that honours the polarity bits; the DATA_CONTROL and TU_CONFIG field meanings;
whether the interrupt cause is clear on read; whether the PHY wants lane 0's drive for every lane; the
exact `CIXH502F` `_DSD`, `CLKT` and `RSTL` names (needs the DSDT; the corpus has them).

### 15.2 USB-C/DisplayPort combo PHY (`drivers/sky1-usbdp-phy`)

Built and host tested (simulated registers and interface window): the initialisation tables for DP only,
USB plus DP in both orientations, and USB only; powering the shared core in the specified order with
orientation first and the clock retried; the controller-side interface (prepare, init, power-state
handshakes); the rate change (PLL1 bracket, per-rate values, lane selection by mode and orientation);
drive levels with the table's filler cells; and `sky1_dp_tx::link::Phy` over it.

Not built: the USB 3 side's use of the same core (shared-user accounting), the Type-C controller that
supplies mode and orientation (`rts5453` exists for the PD side), the S5 orientation write and reset and
clock handles (a `Platform` trait stands for them), the eDP PHY of unit 2, and the firmware-handover byte.

Board questions: the 8.1 Gb/s `SS_CTRL3`/`SS_CTRL4` writes (taken as `0x0062` to control 3 and `0x0004`
to control 4; the vendor's text writes control 3 twice); whether PLL1's lock bit (`0xE001` bit 7) is
valid (recorded, not required); the PMA lane map encoding on a flipped cable in DP-only mode; whether
the orientation reaches the S5 bit after a hot plug under ACPI; whether lane 0's drive suits all lanes.
The board's boot display (DP 4) is PHY 3 in DP-only mode.

### 15.3 Kernel glue: state and what the firmware tables showed

Built: `linlondp::komeda`, the ACPI driver for `CIXH5010`, registered in the arm64 driver table. It pins
`aclk` from the firmware property, takes the pipeline clocks by their `CLKT` names, maps the registers
and interrupt, takes a DMA tag (non-coherent, 40 bits), discovers and initialises the blocks, and builds
the card. Scanout memory maps through the unit's translation domain, or as one contiguous run if the
stream bypasses translation (`Gem::frames`). A unit the firmware left scanning out is declined, so the
boot console stays (adoption is not built). The card is not registered until an output joins it. A
corpus test checks all five units' registers, clocks and resets against the board's firmware.

From the DSDT (`tests/corpus/opi6p/freebsd/acpidump-dt.txt`), for the next drivers:

- `CIXH502F` (DP 4): memory 0 `dp` `0x14224000`, 1 `dsc`, 2 `dp_phy` `0x1422FF00` (256 B), 3 `dp_rcsu`
  `0x14210000` (`0x304` B); clocks `vid_clk0`, `vid_clk1`, `apb_clk` (SCMI 57, 58, 63); reset
  `dp_reset` (index 28); `_DSD` `dp_phy` references `\_SB.UCP3.UDPP`, and graph ports 0 and 1 lead to
  `DPU4` `pipeline@0` and `pipeline@1`. The graph builder already makes `dp_phy` a required edge.
- `CIXH2033` (PHY 3): `_DSD` `id` 3, `default_conf` 2 (DP only), **`cix,usbphy_syscon` references
  `CRU0`** (the specification says ACPI has no syscon reference: it does), `phy-status` `"usb"`;
  clock `pclk` (119); resets `preset` (index 54) and `reset` (58); children `USBP` and `UDPP`.

Not built, in the order they are needed: a lease on the system controller's orientation register
(`sky1-syscon` has none yet); the PHY and DP-TX ACPI drivers and the hand-over of the PHY object between
them (a registry keyed by unit stands in until the M-G PHY provider is designed); the RCSU video
adapter; encoder and connector objects over the transmitter; the hot-plug interrupt thread; the
aggregate that registers the card when its output attaches; and adoption of the firmware's running
display, which the boot display (DP 4) needs before any of it can drive that connector.

### 15.4 The transmitter's driver and the chain to a card

Built (host tested; not run on hardware): `lib/edid` (base block: detailed timings, name, size, depth;
malformed data is an error, not a panic); `drivers/sky1-dp`, the ACPI driver for `CIXH502F`, with the
hot-plug interrupt thread, an encoder (link up, video clock, video adapter, stream on) and a connector
(detect, modes from the monitor or the standard fallback list, EDID), published by unit number; and the
display driver (`komeda`) now waits for the transmitter of its number, builds the card with that output
and registers it. A mode is offered to userland already adjusted for the core (back porch of 12 pixels
at least) and, as the vendor does, with its sync polarities inverted (`Config::invert_sync`), so the
display processor and the transmitter are given the same timing without a fix-up hook in the core.

Declined or waiting by design: a unit the firmware left running (display or link), so the boot display
(unit 4) keeps its console; unit 2 (eDP: its PHY and its panel `CIXH5040` have no driver; the driver
table's ledger test records the wait).

Not built, in order: adoption of the running display (what unit 4 needs); the hot-plug uevent to
compositors; retraining on a sink's attention pulse; the second pipeline's stream and YCbCr output;
bandwidth checks beyond the mode list's; the eDP PHY, panel and backlight; the Type-C controller feeding
the PHY's mode and orientation; then the board session, which answers the open questions in §15 to
§15.3 (two needing a sink: the sync inversion and the interrupt cause's clear-on-read).

### 15.5 Adoption of the firmware's display

Built (host tested against the simulators): a display unit or transmitter link that firmware left running
is taken over instead of declined.

- **Display processor.** A pipeline whose timing generator runs is marked running and not reset; its pixel
  clock is joined unchanged. The first mode set keeps the display (no stop, no clock change) if the mode
  asked for has exactly the timing the pipeline runs and a pixel clock within 1 % of firmware's; the
  layers it does not use are then hidden in the same latch. Any other mode takes it down by the ordinary
  two-phase stop and starts again. Later mode sets are ordinary.
- **Transmitter.** A running stream is adopted without a reset, PHY preparation or training; only the
  interrupt mask is written. The first encoder enable keeps the stream if the registers hold exactly what
  the mode would write (timing, polarities, colour depth), and otherwise resets the block and the PHY and
  trains afresh.
- **If nothing could be shown** (no layer's buffer could be mapped), the firmware's picture is left as it
  is rather than replaced by a blank one.

**The memory path.** Firmware's scanout streams bypass the SMMU (IORT reserved-memory nodes), so the unit
gets one translated context for all five of its streams, and the reserved range (`0x84800000`, 8 MiB) is
mapped to itself in that domain *before* any stream leaves bypass (docs/IOMMU.md §4.9): a display scanning
the firmware framebuffer keeps scanning, and buffers mapped for client use are scattered pages through the
domain, never on the reserved addresses. Built: `Domain::identity_map`; `Iommu::group_context`; the
SMMU's group context (one domain and ASID for many streams, released once); the broker's named-component
path; the reserved ranges a named component may remap (`SmmuNode::identity_for`). Tested on the host
against the SMMU model (two streams starting in bypass, five ending translated, the framebuffer reading as
itself on each, release) and on the corpus (each display unit gets the 8 MiB range, the other named
components none); the kernel's SMMU boot cases still pass. Not exercised by any test: the broker's
named-component branch itself (QEMU has no named component behind an SMMU); the board is its first run.

Also unconfirmed until the board: that the firmware's sync polarities and porches are the ones the offered
modes carry (a mismatch only costs a visible blank, by the fallback above), and that the pixel clock reads
back as the SCMI rate it was set to.

### 15.6 What the Orange Pi 6 Plus showed

Sway 1.12 runs stably on the display processor's card at 1920x1080, 60 Hz, on the HDMI output (unit 4) and
on a DisplayPort monitor at up to 3840x2160 (`Hbr2 x4`); the atomic test commit and the real commit both
succeed. The hardware facts the driver now follows, each learned on the board:

1. **Resets.** Firmware releases only the unit it uses for the boot display. The driver releases
   `rcsu_reset`, then `ip_reset`, before the first register access (sky1-soc 02 §2.2, §2.4); otherwise the
   interconnect's monitor reports a non-secure access error for each unit.
2. **`MAX_LINE_SIZE`.** Reading a block's own `MAX_LINE_SIZE` (offset 8 of the first block) is refused on
   this part. The walk uses the global line size for every block and never reads it.
3. **SMMU.** A bypassed stream is switched to a translated one with one 64-bit store of word 0 after the other
   words are written, with no invalid window (an invalid entry faults the display, `C_BAD_STE`).
4. **Shared register windows.** The USB-C PHY's control window (`UCRU`) lies inside the FCH reset controller's
   window, and the audio one (`ACRU`) is shared with three audio devices. Both are declared, as the S5 one is.
5. **One card.** The native driver retires the firmware framebuffer card (`drm::retire_firmware_cards`) once
   its connector sees a monitor, so only `card1` remains; the console keeps drawing on the framebuffer memory
   the display scans.
6. **Link shutdown.** The bring-up after a link shutdown re-initialises the host side with a reset first
   (sky1-dp-tx 02 §2.11.2); a flag set at shutdown makes the next enable do it. Without it a mode switch
   leaves the link dead.
7. **The channel's reply codes.** The sink answers an I2C-over-AUX read with the channel's own reply field
   DEFER (`AUX_REPLY_CODE` = 2) while the I2C field reads ACK. The native field applies first
   (sky1-dp-tx 01 §1.4.3), and a DEFER is retried (32 tries for an I2C request). Short I2C replies are continued.
8. **EDID.** A sink that NACKs the E-DDC segment pointer (address `0x30`) ends the read, and what was read
   stands (the base block carries the modes). A failed I2C-over-AUX transaction is closed with a bare address
   packet, as the standard's helpers do; a transaction left open stops the sink answering. A failed link
   bring-up (`Aux(Nack)` right after a shutdown) is retried after a pause, then once from a full re-initialisation.
9. **Buffer release.** A flip can take effect a frame after its event is sent, so a buffer is released once
   three newer commits have replaced it (`retire` in `drivers/linlondp/src/card.rs`); earlier releases showed
   as `F_TRANSLATION` faults from the unit's stream. If the latch can be later than one frame, release should
   follow the interrupt that confirms the latch (not seen).
10. **Detach.** The transmitter's register windows are revocable (`Window` in `drivers/sky1-dp/src/lib.rs`):
    the driver gives them back at stop whichever of encoder and display unit detaches first; later reads give
    0 and writes do nothing.
11. **Mode list.** `lib/edid` reads CTA-861 extension blocks (`lib/edid/src/cta.rs`): the video data block's
    standard video codes, looked up in a table of the progressive 480p to 2160p timings (each entry carries the
    rate it is meant to give and a test checks it to 0.1 %), and the detailed timings after the data blocks.
    A size at a rate already given is not added twice; only the base block's first timing is preferred; a
    malformed extension gives no modes. The 4K monitor's connector offers 15 modes (2160p at 60, 50, 30, 25, 24
    Hz; 1080p at 120, 60, 50, 30, 24 Hz; 720p60/50; 576p, 480p, 640x480).

Verified on hardware: EDID read on three monitors; the first mode firmware never ran (1680x1050, an older
Acer) is stable; the 4K monitor steps through 2160p60 (`Hbr2 x4`), 1080p60 (`Rbr x4`), 1080p120 (`Hbr x4`),
720p60 (`Rbr x2`) and back with every link bring-up taking and no `F_TRANSLATION` faults; a Sway restart takes
the link down and up again cleanly.

Open: the sync polarities (a monitor that locks regardless of polarity shows the same picture); DisplayID
extensions, YCbCr 4:2:0-only modes, interlaced modes and the audio blocks are not read; the connector's
readout does not say the link rate and lanes.

## 16. Radeon on the X870E (Raphael, then Navi 22)

Specification: `~/Projects/OS/chinese-wall/specs/amd-rdna2` (sections 01 to 12; every source MIT).
Code: `lib/amdgpu` (MIT, with AMD's notices), `drivers/drmn` (the AMD half; "drmn" is the AbyssBSD kernel's
display and render driver name), `drivers/amd-kms` (the display card), `vgapci`.

Status: the Radeon 610M (Raphael, 1002:164e) drives the display, takes modes from the monitor's EDID, runs
Mesa's `radeonsi` on a render node, and runs Sway with hardware rendering. The Navi 22 (Z690-A) is not started.

### 16.1 Hardware facts

- **Attach.** `vgapci` maps the register window (BAR5, `Bar5` in the code), the doorbell aperture (BAR2) and the
  video memory window (BAR0), and adds `drmn`, which reads VRAM size (512 MiB) and the security processor's
  ready bit and declines a device that answers all ones. Nothing is written at attach.
- **IP blocks** (the discovery table in the last 64 KiB of video memory, read through the index registers):
  GC 10.3.6, SDMA 5.2.6, DMU (display) 3.1.5, PSP/SMU 13.0.5, interrupt handler 5.2.1, host data path 5.2.0,
  bus interface 7.3.0; 30 blocks, identical to Fedora's.
- **Video memory is carved from system memory.** `DCN_VM_FB_OFFSET` is `0x1840`, so video memory is system
  physical `0x1840000000` (ACPI-reserved), GPU address `0xF400000000`. BAR0 is a write-combining window onto the
  first 256 MiB of it. The loader's GOP framebuffer is the first 8,294,400 bytes of it, which is the test for
  "this GOP picture is this device's": compare the loader's base with BAR0's.
- **What firmware owns at handover:** video memory from 0 to the end of the GOP surface (9 MiB used) and the
  top 2 MiB (`0x1fe00000` on; the discovery table and the platform display firmware's windows). Everything
  else is the driver's.
- **The firmware's display:** pipe 0 only (plane, blend, output processor, timing generator and encoder 0),
  1920x1080 in a 2200x1125 total, linear 32-bit ARGB, pitch 7,680 bytes, no compression, at the first byte of
  video memory; the plane's system aperture covers all of video memory, so a surface anywhere in it scans out
  with no page tables. The encoder is in DVI mode. The display microcontroller (DMCUB) runs the platform's
  firmware (`SCRATCH0` `0x42`).
- **Page tables are off at handover.** Both hubs' contexts read `ENABLE_CONTEXT` clear; the display, the
  security processor and the interrupt handler use the system aperture alone. The capture is
  `tests/corpus/x870e/abyssbsd/raphael-drmn1-memory.txt`.

**Video memory layout** (BAR0 offsets; the host test `layout` in `gcring.rs` checks that nothing overlaps):

| At | What |
|---|---|
| 0 | the firmware's surface (the console's) |
| 9 MiB to 128 MiB | scan-out surfaces (the card's two), mode-set scratch |
| 128 MiB | interrupt handler ring (64 KiB) and its write-back slot |
| 144 MiB | GART table (2 MiB, physical `0x1849000000`) |
| 160 / 161 MiB | the security processor's ring, command and fence buffers / its staging buffer |
| 161 to 165 MiB | display firmware image and windows (the staging buffer's space, used one after the other) |
| 170 MiB | graphics ring, report words, clear-state buffer |
| 171 MiB | kernel interface queue |
| 172, 173 MiB | test page tables |
| 174 MiB | DMA ring |
| 175 MiB | eight compute rings (128 KiB each) |
| 176 MiB | page-table pools |
| 192 MiB | per-file fence-packet pages (1 MiB a file) |
| 200 MiB | metrics table (`smu_metrics`) |
| `0x1e000000` | the security processor's trusted memory zone (10 MiB) |
| `0x1fe00000` | firmware's (top 2 MiB) |

### 16.2 Display scan-out and the KMS card

- **Adoption.** The card (`drivers/amd-kms`, over `lib/amdgpu/src/flip.rs`) registers only if the firmware's
  display is running (timing generator on, surface at the start of video memory) and the console's framebuffer
  is the start of this device's BAR0; with another GPU as the boot display it does nothing. `hw.drmn.kms` is on
  unless set to 0. The device registers as `dri/card1` and retires the firmware framebuffer card.
- **CPU path.** BAR0, granted through `AttachCx::mmio_bytes_console`: a prefetchable resource the console's
  framebuffer lies wholly inside, with no other sensitive range overlapping, mapped write-combining. Writes
  need the host-data-path flush before the display reads them.
- **A flip** writes the surface address (high half, then the low half that latches it,
  `HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH` `0xeb2c` and `_ADDRESS` `0xeb28`) with the flip-type bit, and
  polls the flip-pending bit; it takes 3 to 10 ms and never blanks the panel. A commit **copies** the client's
  pixels (forced opaque) into the back one of two video-memory surfaces and flips to it, **unless the buffer
  is the firmware's surface exactly** (the plane at the corner, 1920x1080, pitch 7,680, offset 0): then it is
  shown in place (§16.14). A plane smaller than the screen is refused.
- **Vertical blanking** comes from the display's frame-start interrupt (§16.3); the core's timer is the fallback
  while the interrupts are off. Measured 59.998 Hz against the mode's 60.000 Hz.
- **The console comes back.** Turning the controller off, or the last client on the card's own node closing
  (`DrmDriver::last_close`; render-node files do not count), scans the console's surface again. `stop` does so
  for every detach reason except `Shutdown` and `Reset`, across which the last frame stays.
- **The panic path.** `DrmDriver::panic_restore` (default: nothing) runs for every registered card from
  `drm::panic_restore()`, which the panic handler and fatal-trap report call after the other processors are
  stopped and before anything is printed. The Radeon's takes no lock and waits for nothing: two register writes
  pointing the display at the firmware's surface. Host and QEMU tests cover it; on the board
  `dev.drmn.1.panic_restore` showed the console after a client held the screen.
- **Modes and the monitor.** The connector reads the monitor's EDID over the DDC engine at attach
  (`Amd::read_sink`, `sink.rs`) and offers those modes of its table of seven CTA-861 modes (codes 16, 31, 4, 19,
  3, 18, 1: 1080p60/50, 720p60/50, 480p, 576p, 640x480) that the EDID lists, the native one preferred when the
  card can set it, else 1080p60; no usable EDID gives the whole table. The matching includes the pixel clock
  (1080p25 has the totals of 1080p50). `dev.drmn.<unit>.sink` shows the result. The plane takes a size equal to one of the
  modes' at the corner and copies exactly that size into the surface, whose pitch stays 1920 pixels; a mode-set
  commit calls `Display::set_mode` and turning the controller off sets the firmware's mode again.
- **Hot-plug** (`drivers/drmn/src/hotplug.rs`, `lib/amdgpu/src/hpd.rs`; 08 section 4.3). A kernel thread
  (`amd-hpd`) reads the line's sense (`HPD0_DC_HPD_INT_STATUS` `0x14f50` bit 1; `0x12` with the Dell connected)
  every 250 ms and needs four agreeing reads (one second) to call a change. **Unplugged:** the EDID is
  forgotten and `Connector::detect` says disconnected (`Display::connected`). **Plugged in:** the EDID is read
  again, so the offered modes follow the new monitor (the connector and the controller's check ask the display
  for the current list each time), and **the link is trained again**: `Amd::relink_now` runs the way off and on of
  a clock change (transmitter off, pixel clock, stream setup, front end reconnected, transmitter on) at the
  mode running (`current_vic`), loading the display firmware first if it is not up. Without that, a replugged
  monitor shows the right text through green noise, as photographed on the X870E. A line that reads
  disconnected when the watcher starts (not wired) is left alone and the connector stays connected. User space
  is told with the notification drm-kmod sends (`drm_sysfs_connector_hotplug_event` in `AbyssBSD/kmod/drm`):
  `!system=DRM subsystem=CONNECTOR type=HOTPLUG cdev=dri/card<N> connector=<connector id>`, sent after the link
  is trained again. libudev-devd turns it into a "change" event, which wlroots takes as a hot-plug (FreeBSD's
  linuxkpi makes `kobject_uevent_env` do nothing, so this is the only path). The line's interrupt (IH client 4, source 9, the line in
  the entry's first data word; `INT_CONTROL` at `0x14f54`, enable bit 16, acknowledge bit 0, polarity bit 8)
  wakes the watcher at once: the handler acknowledges and flips the polarity to the opposite of the filtered
  sense (`INT_STATUS` bit 4), the interrupt being level-type, and re-arms after a chip reset (`Irq::restart`).
  The watcher also looks every second, so a lost interrupt costs a second; a changed sense must hold for half a
  second. The other four lines are read once a second and a change is logged (no other connector is wired on
  the boards we have).
  Verified on the X870E: unplug and replug of the HDMI cable (disconnect and connect logged, EDID read, link
  retrained, the console normal afterwards), and with Sway 1.12 (gles2, render node) running: wlroots logs both
  "udev event for card1 (change)" and its hot-plug scans, turns the output off on unplug, sets a mode on replug,
  and Sway's picture comes back normal.
- **Verified:** `tests/graphics/kmsflip.c` (raw ioctls) sets each mode and flips between two patterns; Sway 1.12
  with the pixman renderer, `foot` and glmark2 run on it (`tests/graphics/accept-x86_64-amd-display.png`); a
  ten-minute Sway soak ran 42,048 frames with no overflow.

### 16.3 Interrupts, translation hubs, doorbells

- **Interrupts.** `vgapci` enables one MSI message (`hw.vgapci.msi`, on by default), turns bus mastering on and
  calls the sink the GPU driver registers (`vgapci::InterruptSink`). The interrupt handler's ring (64 KiB,
  2,048 entries, write-back slot after it) is in video memory at BAR0 offset 128 MiB, addressed by its GPU
  address and not snooped; this works with both hubs' contexts off. `lib/amdgpu/src/ih.rs` has the bring-up,
  the entry decoder and the drain (host tested: wraps, overflow restarts at the oldest entry, hostile pointers
  handle nothing). Entries: client 4 source `0x3c` is the display's frame start (`OTG0_OTG_GLOBAL_SYNC_STATUS`
  bit 0 enables, bit 4 clears); client `0x14` source 181 is the graphics fence; client 8 source 224 is the DMA
  trap. `dev.drmn.<unit>.irq` counts them. The read pointer is published through its doorbell (BAR2 byte
  `0xbc0`, `IH_DOORBELL_RPTR`).
- **Translation hubs** (`lib/amdgpu/src/gmc.rs`; 05 section 5.1 order): both hubs get the GART's flat table,
  AGP off, a dummy page, the L1 buffer and a read-modify-write of the L2 so the firmware's ECO bits survive,
  context 0 on (`0x007ffe01`, L2 `0x00080e01`, L1 `0x00001d59`), the fifteen user contexts and the invalidation
  engines. The graphics hub (GFXHUB) is set up before the memory hub (MMHUB), which the display shares, and
  each step is checked by the frame counter still counting. The GART table is 2 MiB (1 GiB of GPU address
  space), every entry invalid, plus one dummy system page from a 44-bit DMA tag. Invalidation goes through
  engine 17 with the memory hub's semaphore taken around it.
- **Doorbells** (`lib/amdgpu/src/doorbell.rs`): `RCC_DOORBELL_APER_EN`, the interrupt handler's range (dword
  `0x2f0`, two dwords) and the DMA engine's (RSMU range `0x500ef0`) programmed behind the NBIF index/data pair
  (`RSMU_INDEX` `0x50`, `RSMU_DATA` `0x54`, the index put back as found).
- **Differences from Linux's settings** that are left as the firmware set them: L1 TLB control `0x501`, fault
  control 2 (`0x20000`, `0xa0000`) and the AGP window registers.

### 16.4 The security processor and the engines' firmware

- **PSP** (`lib/amdgpu/src/psp.rs`, `drivers/drmn/src/psp.rs`): the secure OS is already running at handover
  (`C2PMSG_81` nonzero; version register `0x420016`). The driver creates the ring (MP0 `C2PMSG_64`, `_67`
  write pointer, `_69` to `_71` address and size; 64-byte frames), loads the table of contents
  (`psp_13_0_5_toc.bin` from offset `0x100`, 1,536 bytes, command `0x20`) and sets up the trusted memory zone
  (`0x05`, flags 2): `0xa00000` bytes at `0xf41e000000` (physical `0x185e000000`). Every buffer is in video
  memory. Firmware files are read by `kernel/src/firmware_files.rs` from `/lib/firmware/amdgpu/` (decompressed
  from linux-firmware 20261019, checked against 02 section 11) on a kernel thread, because a sysctl writer
  cannot sleep. **Teardown** (03 section 9) destroys the zone (command `0x07`) then the ring
  (`DESTROY_RINGS` `0x00030000`); a chip reset needs it because the security processor survives a mode-2 reset.
- **Engines' firmware** (`lib/amdgpu/src/gcfw.rs`, `drivers/drmn/src/gcload.rs`): the vendor containers give
  twelve images in the security processor's order (SDMA0, CE, PFP, ME, MEC1, MEC2, the restore lists CNTL, GPM,
  SRM, IRAM, DRAM, RLC_G), each loaded through the staging buffer, then the start-autoload command. Size
  vectors (SDMA 33,792; CE 263,040; PFP and ME 263,168; MEC 267,440; RLC_G 25,088) are checked on the vendor's
  files (`AMDGPU_FIRMWARE_DIR`). The autoload finishes with `RLC_RLCS_BOOTLOAD_STATUS` `0xc0000001` and
  `CP_STAT` 0; the command processors stay halted until released. At handover the graphics engines are not
  started (`RLC_GPM_STAT` `0x00f40016`, `RLC_CNTL` 0, all processors halted).
- **Power firmware (SMU)** (`lib/amdgpu/src/smu.rs`): mailbox through `PCIE_INDEX2`/`DATA2` (BAR5 `0x38`/`0x3c`),
  byte addresses flags `0x3b10024` (bit 0: running), message `0x3b10508`, response `0x3b10984`, argument
  `0x3b10988`; messages used: 2 version, 25 driver interface (5), 10 graphics device reset, 15 graphics clock,
  16 enabled features, 11/12/13 driver table address and transfer (the metrics table, id 7). No message is
  needed for graphics to run at the firmware's clocks (600 MHz idle, 2200 MHz loaded); GFXOFF is never allowed.

### 16.5 The display firmware, HDMI and mode setting

- **Display microcontroller** (`lib/amdgpu/src/dmcub.rs`, `drivers/drmn/src/dmcub.rs`; 08 section 3.2): the
  platform's firmware is asked to stop, the unit is held in reset, the security processor loads the driver's image
  (type 51, 239,392 bytes), windows 2 to 6 (region 5), mailboxes and boot options are programmed, the unit
  released; `DMCUB_SCRATCH0` goes `0x42` to `0x43` (display firmware, mailbox ready). The picture never blanks
  through it. The security processor writes windows 0 and 1 itself at the platform layout's `0x0n00_0000` bases
  (not the `0x6000_0000` ones 08 section 3.2 lists). Commands go through inbox 1 (64-byte slots, the read
  pointer waited for); version `0x5003700`.
- **HDMI** (`lib/amdgpu/src/hdmi.rs`): the audio encoder setup, endpoint configuration bracketed by the clock
  gating disable, the audio clock, the encoder's HDMI mode, the AVI info frame (`hdmi::avi_for`, checksums
  checked against the specification), packet controls, enable and unmute. The endpoint is written from the
  EDID's short audio descriptors, speaker allocation and basic-audio flag. **Sound works on the X870E** (a
  1 kHz tone through the Dell ST2421L's audio-out, heard clean; the monitor has no speakers of its own, its
  audio-out is a line-out): `drivers/hda` runs an output stream on the Radeon's controller (`hdac1`, 1002:1640)
  into its codec (`hdacc1`, ATI `0xaa01`; `hdaa1`: pins 3, 5, 7, 9 and 11, all digital outputs, each
  converter behind one pin; pin 3 and converter 2 are endpoint 0 and DIG0) as `dev.hdaa.1.tone` (§16.11). The
  order that works after a boot: `hdmi_audio`, `edid`, `hdmi` (display side), then the tone. Until `hdmi`, the pin
  senses no display; after it, pin 3 reads presence and a valid ELD. **Measured, closing two open questions of
  the specification:** the audio infoframe the codec writes through the pin's data island packet verbs lands in
  `AFMT_AUDIO_INFO0` (`0x170`: checksum `0x70`, channel count minus one in bit 8), so the display driver does
  not write it; and with the stream running `AFMT_STATUS` reads `0x40000010` (bit 4, audio enabled),
  `AFMT_AUDIO_PACKET_CONTROL` bit 0 (sample send) set, `HDMI_INFOFRAME_CONTROL0` bit 4 set and
  `AFMT_INFOFRAME_CONTROL0` 0. Which of the three clock-regeneration pairs the hardware picks for a 48 kHz
  stream is still not established (all three are programmed; 48 kHz works). **Audio follows the pixel
  clock:** a mode set redoes the audio clock module (the pixel clock in 100 Hz units) and the three clock
  regeneration pairs (`hdmi::acr_values`: the HDMI table's N and CTS for 25.175, 25.2, 27, 27.027, 74.25 and
  148.5 MHz, the formula N 4096/6272/6144 with CTS = clock x N / (128 x rate) for any other) when the audio
  side is set up; tones were heard clean at 1080p60, 720p60 and 640x480 on the X870E.
- **EDID over DDC** (`lib/amdgpu/src/ddc.rs`): the display core's hardware I2C engine (08 section 4.7):
  arbitration, pin hand-over (all restored on every path), up to four transactions per run (segment pointer,
  offset write, read). `MICROSECOND_TIME_BASE_DIV` reads `0x00120218` rather than the `0x00120264` the
  specification's prescale of 500 expects; the driver tries 500, then the platform's value, and 500 works. The
  real EDID of the X870E's Dell ST2421L (`tests/corpus/x870e/abyssbsd/edid-hdmi-a-1.txt`) is the test vector in
  `lib/edid`. The clock actually produced, the engine with the display firmware running, and SCDC are not measured.
- **Mode setting** (`lib/amdgpu/src/timing.rs`, `modeset.rs`; sequence in `Amd::set_mode`): the timing
  generator's eight registers from 08 section 2.2 (they reproduce the GOP's 1080p60 exactly); `preflight`
  (live registers must be as 08 section 3.7 says), `plan`/`plan_retimed`, `stop`, `apply`, `start`;
  `snapshot`/`plan_restore` put the firmware's own values back exactly. With no pixel-clock change (1080p50,
  576p) only timing registers and the request fields change. With a clock change the sequence is the reference's:
  packets stopped, plane blanked, transmitter off (firmware command), generator stopped, new pixel clock
  (`dmcub::set_pixel_clock`, header `0x10000280`, window 3 holding the video BIOS copy), timing and request
  registers, generator started with the plane still blanked, encoder in HDMI mode, encoder stream setup, stream
  controls, AVI frame, packet controls, transmitter on, **the encoder's front end reconnected after the
  transmitter-on command**, unblank. Findings the specification lacked: both transmitter commands clear the
  encoder's front-end connection (`DIG_FE_CNTL` to 0 and `DIG_BE_CNTL` bit 8), without the reconnect the monitor
  shows no signal while every other register looks right, and a `DIG_FE_CNTL` write reads back 0 until the
  generator runs. A pixel-clock change without the transmitter pair loses the signal and only a reboot (the
  platform firmware retrains the link) recovers it.
- **Verified on the X870E** (Dell ST2421L, user-observed): 1080p60, 1080p50 (50.332 Hz), 720p60 and 720p50
  (74.25 MHz), 480p (27 MHz), 576p50, 640x480 (25.175 MHz), each stable, each returning to 1080p60. Not done:
  modes above 148.5 MHz (§16.14) or interlaced (§16.14); a different active size beyond the plane's top-left crop; in-driver
  recovery of a lost signal; hot-plug.
- **A process rule from this work:** a display-changing command runs only after the user has agreed to it, one
  command at a time; never bundle one with its harmless pre-check.

### 16.6 The graphics, DMA and compute engines

- **Graphics block** (`lib/amdgpu/src/gc.rs`, `pm4.rs`, `mqd.rs`, `kiq.rs`; 06): golden settings of GC 10.3.6
  (22 entries), per-VMID shader memory configuration, the clear-state buffer (949 dwords), the command processors
  released (a released processor is busy running its firmware; `CP_STAT` is not waited to idle). **The graphics
  ring runs only through the kernel interface queue (KIQ):** the KIQ is programmed directly on MEC2 pipe 1, then
  `SET_RESOURCES` and `MAP_QUEUES` map the graphics queue with its queue descriptor; with only the ring registers
  programmed the firmware reads a zero descriptor address and faults at address 0. Doorbell writes are 64-bit.
  The ring's write pointer is a **count that never wraps** (doorbell and poll word get the 64-bit count; the ring
  offset is the count modulo the size); a wrapped pointer stalls the ring at its first wrap.
- **User address spaces** (`lib/amdgpu/src/vm.rs`: four-level tables in video memory): the ring switches a VMID's
  page table and flushes through packets (`gmc::ring_vm_flush`: root written low then high, request, wait on
  the acknowledgement, `PFP_SYNC_ME`). A page an indirect buffer is fetched from must be mapped **executable**
  (a fault leaves the fetcher stuck until reboot). Addresses with the top bit set stay sign-extended in packets
  and are masked for the page tables.
- **The job** is the reference's frame (06 section 10.4): pipeline sync, address-space flush, state loads
  (`CONTEXT_CONTROL`), frame control, indirect buffers, fences (`RELEASE_MEM` with the interrupt select),
  `SWITCH_BUFFER`, padded to 256 dwords. A client's own fence word is written by a small indirect buffer run in
  the job's address space (a ring-level write runs in VMID 0 and faults); kernel-reserved address space is the
  top 4 MiB, and the fence packets live at its top 1 MiB.
- **DMA engine 0** (`lib/amdgpu/src/sdma.rs`, 07): reset pulse, ring registers (64-bit byte-offset pointers),
  doorbell range, translation settings, a self-test at start. Its job: previous fence, address-space switch (two
  register writes and an invalidation poll on engine 13), buffers each ending on an 8-dword boundary, fence and
  trap.
- **Compute:** eight kernel queues on the first micro engine (pipe `n mod 4`, queue `n div 4`, doorbell dword
  `6 + 2n`) mapped in one KIQ submission (`SET_RESOURCES` mask `0x03030303`); each answers a test write at start.
  The interface offers four slots (09 section 3.2); each context's slot is bound at its first submission to the
  less loaded of queues `s` and `s + 4` and stays there, so a context's fence values keep the order of one ring.
  A compute job's handle carries its queue in the high half. The job is the graphics frame without the
  parser-only items, with the address-space switch on the ring's own invalidation engine (`4 + n`).
- **Fence interrupts:** `CP_INT_CNTL_RING0` time-stamp enable (bit 26) is set when the ring starts; the handler
  wakes a wait queue; `WAIT_CS`, `GEM_WAIT_IDLE` and the syncobj wait sleep on it with a 50 ms (render) or 20 ms
  (syncobj) timeout as fallback, so a lost interrupt costs latency, not a hang.

### 16.7 The render node and Mesa

The card has a render node (`dri/renderD<128+N>`, device `drm/<128+N>`; `drivers/drmn/src/render.rs`) that takes
rendering requests only: amdgpu interface 3.64 with `INFO`, `GEM_CREATE/MMAP/VA/OP/METADATA/WAIT_IDLE`, contexts,
buffer lists, `CS` (IB, fence and syncobj-out chunks), `WAIT_CS`, and synchronisation objects in `sys/drm`
(create, destroy, wait, reset, signal, the timeline requests, descriptors, `SYNCOBJ_EVENTFD`, and sync file export and import: §16.12 and §16.13). Mesa's `radeonsi` (ACO) initialises
through it and `glmark2-es2-gbm --off-screen` completes the whole suite on the Radeon 610M (score about 11,900
at the first run, terrain about 430 FPS); Sway's `gles2` renderer and its clients (jellyfish about 7,400 FPS)
draw on it, with the compositor importing every client's shared buffer
(`tests/graphics/accept-x86_64-amd-radeonsi-sway.png`; `tests/graphics/run-sway-gles.sh`).

Details a client depends on: libdrm on FreeBSD finds the device by `devname` (nodes `drm/<minor>` with
`dri/card<N>` and `dri/renderD<M>` as links, plus `hw.dri.<N>.busid` `pci:0000:7a:00.0` and `.modesetting`);
the context answer's id is its first dword; context query is operation 4; sharing is by PRIME (see §16.8).
Mesa does not use the compute and DMA rings by default; the interface offers them to what asks
(`tests/graphics/amdrender.c` runs a command buffer on graphics, DMA write and copy, and compute rings 0 and 3).

### 16.8 Reset, preemption and buffer sharing

- **Ring reset** (10 sections 1 and 4). A job with no progress for two seconds (noticed by whoever waits for
  it, or by a submission that finds its ring full) gets its ring reset and its jobs completed by writing the
  ring's fence word. Graphics: `CP_VMID_RESET` (descriptor inactive, request done, new descriptor, remap, the
  ring's start sequence). Compute: `UNMAP_QUEUES` with the reset action (queue seen inactive, remap). DMA: the
  engine's own reset (ring stopped, translation cache off, soft reset pulse, restart; the freeze step is left
  out because its registers are not in the specification). The context that made the last job reads as
  guilty; every context reads that a reset happened (`QUERY2` flags bit 0 and bit 2, hangs 1).
  `tests/graphics/amdhang.c` hangs each engine and checks the wait returns after 2.0 s, the state says so, and
  the engine runs a job afterwards.
- **Full chip reset** (`chip_reset`, `drivers/drmn/src/amd/reset.rs`; 10 sections 3, 5 and 8): the flag is
  `BIOS_SCRATCH_3` (BAR5 byte `0x13c`) bit 29; the security processor's zone and ring are destroyed; PCI
  configuration is saved (`PciChild::save_state`) and bus mastering cleared; SMU message 10 with argument 2
  (mode 2) is sent; configuration is restored; `RCC_CONFIG_MEMSIZE` (dword `0xde3`) is polled; the **AtomBIOS
  ASIC-init table** is run by the interpreter (`lib/amdgpu/src/atom.rs`; the video BIOS image comes from the
  ACPI VFCT table, `lib/amdgpu/src/vbios.rs`, `Manager::acpi_table`; command table 0 with the boot engine and
  memory clocks from firmware-info data table 4, done when scratch 7 bit 9 is set); then interrupts, hubs,
  security processor, firmware, graphics, compute, DMA and the render fences are re-established (display
  last). The display keeps scanning out through it. The vendor video BIOS is never committed
  (`tests/corpus/x870e/vbios.bin` is git-ignored).
  Verified: six runs on the X870E, `amdrender` and glmark2 pass afterwards. **Open: the graphics runs at
  half speed after a reset** (terrain 202 FPS against 430). The processor is unaffected (`cpuspin.c`); the
  firmware reports the same clocks (2200 MHz graphics, 3300 MHz memory) and 100 % activity, but graphics power
  is 6.5 W against 64 W. It is not the video BIOS initialisation (`chip_reset_no_init` is the same), not GFXOFF
  (`smu_gfxoff_disallow`), and the graphics register dump matches a clean boot. Suspect a configuration the
  firmware sets only at power-on (shader array or work-group-processor enables).
- **Preemption.** The DMA engine's works (`sdma_preempt`). `UNMAP_QUEUES` is built for all four actions, but
  the firmware did not write the fence of "preempt without unmapping" on an idle compute queue with a video
  memory fence; the specification (10 section 4.3) says the reference uses that action only on the graphics ring,
  with preemptible buffers and a GART-mapped fence. Compute and graphics preemption are unverified and nothing
  preempts a job.
- **Buffer sharing.** A buffer made on the render node is exported as a descriptor and imported in a second
  file (same pages, writes cross both ways, a second import gives the same handle, the GPU writes through the
  importer's address space, the card's own node makes a framebuffer of it, the buffer outlives the exporter's
  handle): `tests/graphics/amdprime.c`.
- **User-mode queues** need a firmware scheduler this hardware does not have (09 section 1: user-queue mask 0);
  their requests (`0x16` to `0x18`) are refused as not supported.

### 16.9 Debug triggers

One-shot sysctls in `drivers/drmn/src/amd/debug.rs` (`dev.drmn.<unit>.<name>`, written with 1; all left out by
`hw.drmn.debug=0`, on by default): reads (`discover`, `display`, `memory`, `vram`, `gc_probe`, `smu_probe`, `smu_metrics`, `fence_words`, `afmt_probe`, `ddc_probe`,
`mode_probe`, `vbios_dump`; and `sink` and `irq`, which are always present); bring-up pieces (`psp`, `gmc`,
`gmc_mm`, `interrupts`, `doorbells`, `dmcub`, `gc_fw`, `gc_ring`, `gc_vm`, `asic_init`); display (`flip`,
`mode=<code>`, `hdmi`, `hdmi_off`, `hdmi_audio`, `edid`, `link_reconnect`, `refresh`, `panic_restore`); and tests
that disturb the engines (`gc_hang`, `gc_fault`, `gc_preempt`, `sdma_preempt`, `chip_reset`,
`chip_reset_no_init`, `smu_gfxoff_allow`, `smu_gfxoff_disallow`). Anything that can hang the machine is run only after the user agrees; a hang is cleared by a
reboot (always set `BootNext` first so the board does not boot Fedora).

### 16.10 Not built

- Interlaced modes and pixel clocks above 148.5 MHz (§16.14); a clock action for deadline hints.
- Hot-plug events for connectors other than HDMI-A-1 (no other is wired on the X870E; their lines' senses are
  logged).
- Compute and graphics preemption verification; the wave-kill soft recovery; the DMA reset's freeze step.
- Scan-out in place for buffers that are not exactly the firmware's surface (other sizes, pitches, modes
  other than 1080p60); they are copied.
- Sound over HDMI through the sound engine (`/dev/dsp`, docs/AUDIO.md); the Navi 22 (Z690-A) bring-up; the half-speed-after-reset cause (§16.8).
- Render targets in video memory, the compute rings' dependency wait through system memory, buffers made whole at
  creation so a mapping does not fault page by page, (all §16.15).

### 16.11 HDMI audio: the stream half

`drivers/hda` plays through any HD Audio codec: `lib/hda/src/stream.rs` has the stream descriptor registers, the
buffer descriptor list entry, the stream format word (`format(48_000, 16, 2)` is `0x0011`), the verbs that point a
converter at a stream (format, stream and channel, digital converter enable) and set a pin (output enable,
connection select, amplifiers, EAPD, the HDMI channel slots and the data island packet writes for the audio
infoframe), the infoframe's bytes and the tone. `drivers/hda/src/controller.rs` runs output stream 0 (descriptor
index = the input streams + 0, stream number 1) over a two-entry buffer descriptor list, polled with no
interrupt (`INTCTL` stays 0): `Controller::play` returns a `Playback` whose drop stops the stream.
`drivers/hda/src/tone.rs` chooses the output pin (a present digital pin first, with its capabilities and sense
asked again at that moment, since the display driver enables the endpoint after attach), follows the first
connection of each node to the converter, powers the path and opens its amplifiers, starts the stream, sets
the converter and (for HDMI) the pin, and logs a ladder: the codec's readbacks, the link position (19,200 bytes
in 100 ms is right at 48 kHz stereo), the wraps of the 30,720-byte looped buffer, the pin sense. Behavior
reference: FreeBSD 15.1 `hdac.c` (`hdac_stream_start`) and `hdaa.c` (`hdaa_audio_setup`).

`dev.hdaa.<unit>.tone` (write seconds, 1 to 20) runs it on a kernel thread. The kernel's audio test
(`hda_controller_codec_and_function_group`) plays it on QEMU's `hda-duplex` on both architectures and asserts
the position moves and the buffer wraps. `dev.drmn.<unit>.afmt_probe` logs the display's audio registers (reads
only) to run during a tone. This is the driver a `PcmHw` will wrap (docs/AUDIO.md §7.2, row 4); the sound
engine, `/dev/dsp` and mixer are not built.

### 16.12 Sync files

A **sync file** is a descriptor that holds fences and becomes readable once every one has finished
(`sys/drm/src/sync_file.rs`; `FileKind::SyncFile` in the kernel, `DTYPE` 18, `KF_TYPE_SYNCFILE`). It answers
`poll` (`POLLIN`) and `kqueue` (`EVFILT_READ`, data 1 when finished), has no `read` or `write`, and takes the
two `linux/sync_file.h` requests: `SYNC_IOC_MERGE` (a third descriptor for the fences of two, a fence held by
both counted once) and `SYNC_IOC_FILE_INFO` (the name, the status, the number of fences, and, on a second call
with room, one 72-byte record each). `SYNCOBJ_HANDLE_TO_FD` with `EXPORT_SYNC_FILE` gives one for the fence in a
syncobj (an empty one is `EINVAL`); `FD_TO_HANDLE` with `IMPORT_SYNC_FILE` puts a sync file's fence in the
named syncobj, or a new one for handle 0.

The wake-up is a new `Fence::register(&Arc<dyn Wake>)`: a fence that can say when it may have finished takes the
waker and wakes it from thread context; the sync file looks again and either tells its pollers and knotes or
asks again. The Radeon's `JobFence` hands the waker to the `Render`, whose thread `amd-fence` (started on the
first one, gone after a second idle) sleeps until the fence interrupt bumps a counter, with a 50 ms timeout as
the fallback, wakes the wakers, and looks for a hung ring (`hang_check`) on every engine, so a job that never
finishes is reset even when only a sync file waits for it. Tests: `sys/drm` host tests (five: poll and wake,
merge, export and import, merge and info requests), and `tests/graphics/amdsync.c` on the X870E: a 3,000-packet
job's sync file is pending at once and readable within 2 s, kqueue reports it, two merge into one with two
fences that becomes readable and describes itself with status 1, an import is waited for as a syncobj, and
the refusals (a plain syncobj descriptor, an empty syncobj, a bad descriptor) are the errors above.

### 16.13 Timeline objects, object descriptors, eventfds and dma-buf fences

- **Timeline objects** (`sys/drm/src/syncobj.rs`). A syncobj is binary (one fence, point 0) until a point above 0
  is put in: then it is a timeline, a map from 64-bit point to fence whose points only grow (`EINVAL` for one that
  does not). A point is finished when its fence and every earlier one is, as a fence chain is. Requests:
  `TIMELINE_SIGNAL` (0xcd), `TIMELINE_WAIT` (0xca, with `WAIT_ALL`, `WAIT_FOR_SUBMIT`, `WAIT_AVAILABLE`), `QUERY`
  (0xcb: the last finished point, or with `LAST_SUBMITTED` the last put in; a binary object is at 0), `TRANSFER`
  (0xcc: the fence at a source point to a destination point, waiting up to 10 s for a submission with
  `WAIT_FOR_SUBMIT`), and `HANDLE_TO_FD`/`FD_TO_HANDLE` with `TIMELINE` for a sync file at a point. The
  capability `DRM_CAP_SYNCOBJ_TIMELINE` is 1. A point past the last waits for submission only with the flag.
- **A descriptor for the object itself** (`FileKind::SyncObj`, `DTYPE` 19, `KF_TYPE_SYNCOBJ`): `HANDLE_TO_FD`
  without flags exports the object, and `FD_TO_HANDLE` in another file (or another process) names the same one,
  so a signal through either handle is seen through the other. It has no `read`, `write` or `poll`.
- **`SYNCOBJ_EVENTFD`** (0xcf, allowed on the render node): when the fence at a point has finished (or is
  there, with `WAIT_AVAILABLE`, even on an object that has none yet) one is added to the eventfd. It rides on
  `Fence::register` for the fence and on a list of waiters for the next fence put in the object
  (`drm::Signal`, which the kernel's `Eventfd` implements).
- **dma-buf fences.** Every buffer has a `Reservation` (`sys/drm/src/resv.rs`): fences by use (write or read) and
  whether they came from outside. `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (`_IOWR('b', 2)`) gives a sync file of the
  fences someone who will read (the writers') or write (all) has to wait for; `IMPORT_SYNC_FILE` (`_IOW('b', 3)`)
  adds a sync file's fence as a reader's or a writer's, marked external. The Radeon's `CS` keeps the buffer
  handles of each buffer list (`BO_LIST` create and update read them), puts the job's fence on every buffer of its
  list as a write fence, and before it runs waits for the fences of the objects it names and for the external
  fences on its buffers.
- **Command submission chunks:** `SYNCOBJ_IN` (4) and `SYNCOBJ_TIMELINE_WAIT` (8) are waited for before the job
  runs (an object or point with no fence is `EINVAL`, as for the reference); `SYNCOBJ_OUT` (5) and
  `SYNCOBJ_TIMELINE_SIGNAL` (9) take the job's fence, and a point that does not follow the last is refused
  before the job runs.
- **The fence notifier** looks for a hung ring only on engines that a registered fence is on, so an idle
  engine's last job is never taken for a hang (the first version looked at all and reset idle rings).
- **Tested:** 11 `sys/drm` host tests for these (timeline signal, wait, query, transfer, point export and import,
  object descriptors, the eventfd's three paths, the dma-buf requests), and `tests/graphics/amdsync.c` on the
  X870E, which adds to §16.12's checks: an object descriptor (a signal seen through the other handle), a
  timeline job (a point signalled by a job, waited for, queried, a point that does not grow refused,
  transferred to a binary object), an eventfd signalled when a job finishes and when a fence arrives, and a
  dma-buf whose job fence is exported as a sync file, imported back and exported again; three runs in a row
  with `amdrender`, glmark2 (terrain 425 FPS) and `amdhang` unchanged.

### 16.14 Scan-out in place, ordering, deadlines, modes and link health

- **Scan-out in place** (`lib/amdgpu/src/scanout.rs`, `drivers/drmn/src/kms.rs`, `drivers/amd-kms`; 08 section
  2.3.1). The display hub's context 0 is programmed as a copy of the GART (`DCN_VM_CONTEXT0_CNTL` `0xe864` 0,
  start 0, end `0x3ffff`, base `0xe868`/`0xe86c` the table's physical address with VALID, the low half last and
  polled; `HUBPREQ0_DCN_VM_MX_L1_TLB_CNTL` `0xec0c` = `0x19`), once, after the hubs are set up. A buffer that is
  exactly the firmware's surface (`in_place_fits`) has its pages mapped into one of two 16 MiB slots of the GART
  (at `0x2000_0000` and `0x2100_0000`; entries valid, system, readable, writeable, executable, **uncached and
  not snooped**), written back from the processor's caches (`DmaMemory::clean`, which is now `clflush` on x86),
  the hubs' VMID 0 invalidated, and the surface address flipped to the slot's GART address. The slot not on
  show is the one written. The `Gem` is held (its pages with it) until the display has moved off it; the console
  restore and the panic path write the firmware's surface address as before, and the copy path takes every
  buffer that does not fit. Verified on the X870E: a gradient from 8 MiB of system memory
  (`dev.drmn.1.scanout_test`, no hub fault, 180 frames in 3 s), then Sway 1.12 with the glmark2 jellyfish
  (13,661 pictures shown in place by `dev.drmn.1.scanout`), drawn correctly, console back on exit. Open: the
  specification's start of the display's rIOMMU and the programming of context 15 are not needed on this board;
  buffers of other sizes or pitches, and modes other than 1080p60, are still copied.
- **Cross-ring ordering.** A reservation entry carries the ring that owns the fence (or `EXTERNAL`); a job waits
  for the unfinished fences on its buffers except those of its own ring (which orders its own jobs). The buffers
  come from the `BO_LIST` entries or, as Mesa sends them, the inline `BO_HANDLES` chunk (`render` sysctl: 1,722
  of 1,811 submissions in a glmark2 run). `tests/graphics/amdsync.c`: a long graphics job and then a compute job
  on the same buffer, 20 of 20 times the first job is finished when the second submission returns (0 of 20 with
  no list).
- **Deadline hints.** `WAIT_DEADLINE` (flag 8, the `deadline_nsec` after the padding of both wait layouts;
  other flags are `EINVAL`) tells each unfinished fence the deadline once (`Fence::set_deadline`). The Radeon
  counts them (`render` sysctl) and has no clock to raise: the power firmware runs the engine at its top clock
  (2,200 MHz) under load.
- **Hot-plug** is by interrupt (§16.2), and the watcher also keeps a link's health.
- **Lost-link recovery.** Once a second, if the link was healthy since the last mode set, the encoder's FIFO
  (`DIG_FIFO_STATUS` `0x15544`: bit 29 calibrated, bit 0 level error; `0x22099000` healthy) is read; three bad
  looks in a row train the link again (`relink_now`), up to three times, then it is left alone and logged.
  `dev.drmn.<unit>.link_break` (the transmitter off through the display firmware) makes the test: the driver
  found the link down within 3 s and brought the picture back. The recovery uses the working link's saved front
  end connection (a link already taken down has none; the first version restored an empty one and stayed blank).
- **Interlaced modes: not supported.** DCN 3.1.5 has no progressive-to-interlace converter and the HUBP no field
  fetch (08 section 3.7.9, which also records that the reference never drives interlace and leaves the field
  line values, the second field's half line and the per-field fetch open); the panel's 1080i, 480i and 576i
  modes would need the surface address switched every field by software with the field timing of an unverified
  register path. The card does not offer them.
- **Pixel clocks above 148.5 MHz: not built, unverified.** The sink on the X870E (a Dell ST2421L) takes at most
  1080p60, and no other display is available; a 4K mode would also need larger surfaces than the card makes
  and, above 340 MHz, the SCDC scrambling handshake (08 section 3.7.8). Nothing was run for it.

### 16.15 Performance against FreeBSD `drm-kmod` (the X870E, Radeon 610M)

Measured on the same board against stock FreeBSD 15.1 with `drm-612-kmod` 6.12 and Mesa 26.2.2 (`radeonsi`,
glmark2 2023.01), the same glmark2 and `tests/perf/graphics/drmbench.c` binaries on both
(`tests/perf/graphics/README.md` says how; the raw runs are in `results/`: `abyss-start-*` before, `abyss-gl-*`
after the changes below but the write-combined buffers, `abyss-wc-*` after them, the table's "now"). `drm-kmod` is the baseline of G5
and of M-G.6.

| | start | now | FreeBSD |
|---|---|---|---|
| glmark2 score, three full runs | 9,913 / 9,934 / 9,931 | 11,879 / 11,872 / 11,873 | 10,796 / 10,729 / 10,938 |
| geometric mean of the 32 tests' frames per second against FreeBSD | 0.928 | **1.082** | 1 |
| `drm.submit.gfx`, submit and wait (ns) | 13,077 | 7,580 | 7,570 |
| `drm.submit.gfx.batch`, per job of 1,000 queued (ns) | 7,900 | 3,430 | 7,700 |
| `drm.submit.compute` (ns) | 50,029,499 | 8,096 | 7,575 |
| `drm.submit.compute.batch` (ns) | 50,070 | 33,240 | 7,570 |
| `drm.fence.signalled`, a wait on a finished job (ns) | 888 | 119 | 139 |
| `drm.bo.create4k` / `create1m` (ns) | 1,341 / 81,115 | 1,256 / 82,555 | 2,196 / 37,147 |

What the gap was, and what closed it. The cost of reading across the bus into video memory (about 0.7 us for
one word, more when the engine is busy) was behind most of it:

- **The compute rings had no fence interrupt.** Only graphics ring 0's end-of-pipe interrupt was enabled; the
  four pipes of the first micro engine (`CP_ME1_PIPE0_INT_CNTL` at `0xc214`, bit 26, from the vendor's MIT
  register header) were not, so a compute job's waiter was woken by the 50 ms poll: 50 ms per compute job.
- **The fence words are in system memory** (`drivers/drmn/src/gart.rs`, `Fences` in `gcring.rs`): one page,
  entered in the GART at `0x3000_0000` (snooped), each word 64 bytes apart. A wait read the word four or five
  times, each a read of video memory. (The spacing matters: a hung graphics job's fence packet, run when its
  queue is reset, writes its value 16 bytes past its address; with the words 4 bytes apart that landed in a
  compute ring's word, which then looked finished. `amdhang` followed by `amdrender` found it.)
- **The interrupt handler's ring and its write-back word are in system memory** (GART `0x3100_0000`): the
  handler read the write pointer and eight dwords of the entry from video memory, about 6 us per fence
  interrupt before the waiter woke.
- **A submission** no longer reads and flushes video memory more than it must: one flush for the packets and the
  poll word instead of two, the read pointer looked at only when the cached bound (`Room`) says the ring may be
  full, the user fence's read back dropped (the ring's flush completes it), a full ring polled every 20 us
  rather than every millisecond, and a compute job's wait for the previous job's fence left out when the
  processor already sees that job finished.

- **Write-combined buffers, read without snooping.** Mesa asks for `AMDGPU_GEM_CREATE_CPU_GTT_USWC` on most
  buffers (346 of 479 requests in a glmark2 run, 794 MiB): the processor only writes them. Such a buffer's
  pages are now mapped write-combined for the processor (`Placement` write-combine; `Object::set_write_combining`,
  and each page is written back out of the caches when it is made, `PageOps::uncache`, because the direct map
  alias is write-back) and read by the engines with the PTE's snoop bit clear, as `drm-kmod` does. `bump
  high-poly` went from 6,904 to 9,080 frames per second (FreeBSD 8,717). A first attempt that cleared the
  snoop bit of every large buffer with the processor's mappings still write-back made the engines read stale
  command buffers: the two sides have to change together.
- **A pool of video memory for buffers** (`drivers/drmn/src/vram.rs`: 248 MiB, from 224 to 472 MiB of the 512,
  first fit; `Object::device_at`, `Gem::new_device`): a buffer asked for in video memory *and* marked local
  (`AMDGPU_GEM_CREATE_VM_ALWAYS_VALID`, not shared) lives there, mapped write-combined for the processor,
  cleared when made, addressed by the engines as video memory, not exportable (`EOPNOTSUPP`) and not scanned
  out. `tests/graphics/amdvram.c` checks it on the board. Mesa 26 on this card sends the local flag for 2
  requests of 479, so a glmark2 run uses the pool for nothing; 241 requests ask for video memory unmarked.
  Creating one costs 453 us (the clearing, a word at a time across the bus).

- **Address-space switch and translation flush only when needed.** Every job wrote its address space's page-table
  base and flushed every translation cache level (`0xf << 19 | 1 << 23`) before it ran; the job after it then walked
  the page tables again. A job now does neither when the engine's last job was in the same address space and no
  mapping was changed, removed or reset since (`Render::vm_check`; the `render` sysctl counts both: 34 switches
  and 357,264 skipped jobs in a run). This was the largest single gap: `shadow` 8,114 to 8,942 fps, `desktop
  shadow` 5,980 to 6,962, the glmark2 mean 10,971 to 11,852. The lead came from the specification's account of
  when the reference flushes (05 section 5.4: a new or reassigned address space, a mapping removed or moved, a
  reset; a new mapping into unused space needs none). Measured by comparing against the FreeBSD run, with the
  clocks, memory placement and page-table setup ruled out first.

- **Large pages for the translation cache.** A buffer's pages were scattered single frames (268 of 37,251 mapped
  pages happened to be aligned runs), each page table entry its own cache entry. A buffer's pages are now made, when
  the client maps it, as the largest aligned runs of consecutive frames up to 2 MiB (`Object::populate_runs`,
  `PageOps::alloc_block`, `PhysAllocator::alloc_block_as_pages`, each page still freed alone), and the page-table
  builder gives each run that is aligned in the address space and in memory the entry field `FRAGMENT` = log2 of
  its pages (`Space::map`; the specification, 05 section 3.3: the cache holds the run as one entry and assumes the
  frames consecutive, so only a physically contiguous, aligned run may have it). 37,222 of 37,251 pages are in
  fragments in a glmark2 run. `refract` 882 to 943 fps, `shadow` 8,942 to 9,074 (FreeBSD 947, 9,186), the glmark2
  mean 11,852 to 11,875. (The 2 MiB entry that needs no page table, `PDE_IS_PTE`, is not used.) Tried and dropped:
  not waiting for the previous job before the next on the graphics ring (no change); the specification says the
  reference does not, and relies on the end-of-job cache write-back and the client's own packets.

Where AbyssBSD is still behind (ratio of frames per second): `buffer` (map update) 0.97 and `shadow` 0.99; every
other test is at or above FreeBSD. Open, in this order:

1. **Render targets in video memory** (`drm-kmod` places the buffers Mesa asks for in video memory in it): the
   pool takes every request for video memory, which needs a shared buffer in video memory to be usable: export
   as a dma-buf (the importer maps it as video memory too, works on the same device), mapping by the processor
   (write-combined, works), and scan-out straight from video memory (the display reads it as its own memory; the
   card's flip takes a video memory address) or a copy by reading it back (`Gem::with_page` refuses a device
   buffer now).
2. `drm.bo.create1m` is 2.2 times slower: a mapping's pages are made one write fault at a time (FreeBSD makes the
   buffer whole at creation and maps neighbors).
3. `drm.submit.compute.batch`: 1,000 jobs queued behind each other wait for the previous fence, which the engine
   polls in system memory (33 us a job against 8 us when the word was in video memory); a job queue that is
   shallow is unaffected. A compute job could wait on a copy of the word in video memory.
4. **Sway with the `gles2` renderer did not start**, and now does: wlroots commits with an explicit-sync fence now
   that the render node advertises native fence sync (§16.12), and the planes had no `IN_FENCE_FD` property
   (`Plane 2 is missing the IN_FENCE_FD property`, then `Backend commit failed`). Each plane now has it
   (`sys/drm/src/atomic.rs`: a signed range, the atomic call turns the descriptor into the sync file's fence and
   waits for it, up to 10 s, before the commit; it is never held in the state and reads back -1). Verified on the
   X870E: Sway 1.12 with `gles2` and the glmark2 jellyfish, drawn correctly (watched), 1,759 frames scanned out
   in place in 30 s. Not built: `OUT_FENCE_PTR` on the display controller (wlroots goes without).

The regression programs (`amdrender`, `amdsync`, `amdprime`, `amdhang`) pass on the final build, and the host
tests, `cargo xtask check` and the x86_64 and aarch64 boot cases do. The hot-plug interrupt, whose ring is now in
system memory too, was not exercised again (frame-start and fence entries do come through it).

