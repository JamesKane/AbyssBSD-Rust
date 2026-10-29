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
| G1 | **FreeBSD's graphics userland runs unmodified**: `pkg install mesa-dri mesa-libs libdrm wayland` and a compositor from ports work as on FreeBSD with `drm-kmod` | The ABI target is FreeBSD userland ([ADR-0004](adr/0004-abi-target.md)). Vulkan and GL drivers are far beyond what we can write; Mesa already has panvk, Turnip, RADV, and Venus |
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
- **New userland graphics APIs.** M-D's compositor (the desktop's `undertow`, over wlroots) uses
  the same DRM/KMS, GBM/EGL, and Vulkan interfaces as everything else.

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
userland, for every GPU. That is several person-decades of Mesa. M-D's compositor runs on wlroots,
libdrm, and Mesa from ports ([DESKTOP.md](DESKTOP.md) §3.3) and gains nothing from a new kernel
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
 sys/vm::dma  IoAddressSpace · IoPageTable<F> · BusAddr · ReservedRegion       (+ sys/iommu, P9)
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
  The P9 SMMUv3 stage-1 backend uses the same AArch64 format, so it is one implementation.
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
| 5 | amd64 native: RDNA2 (Radeon 610M iGPU, Navi 22) | X870E, Z690-A | §13 question 1; the desktop's metal target |
| Tier 2 | `msm` (Adreno 690 + DPU1) | Radxa Dragon Q8B | Specifications exist; depends on the PEP providers and the SMMUv2 backend |

The ROADMAP's M-G entry names RDNA2 as the first native GPU. The X870E has no Intel GPU: its boot
GPU is an NVIDIA RTX 4070, and its CPU has an RDNA2 iGPU.

## 10. Prerequisites from earlier phases

| Need | Where |
|---|---|
| Firmware store (GPU firmware by GPU ID) | DRIVER-MODEL.md §8.6 (P7) |
| Clock, reset, power, and SCMI providers; the platform description | DRIVER-MODEL.md §5.5, §8.5, §9.3 (P7) |
| Runtime PM with driver-held `PowerRef` | DRIVER-MODEL.md §8.5 (P9) |
| `PerfDomain` (GPU DVFS) and thermal capping | DRIVER-MODEL.md §9.3 (P9) |
| IOMMU backends (SMMUv3, SMMUv2) and device address spaces | DRIVER-MODEL.md §8.4 (P9) |
| `SCM_RIGHTS`, kqueue, eventfd, shared memory | P6 |
| evdev (compositors read input from `/dev/input/event*`) | `sys/evdev` (USB track) |
| VM objects with pagers, swap | P5, P9 |

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
- `drm.flip`: page-flip latency to the vblank, and compositor frame time under `weston` or `sway`;
- `drm.fence`: wait latency for an already-signalled and a just-signalled fence;
- application benchmarks (`vkmark`, `glmark2`) as diagnostics.

Where FreeBSD has no driver (panthor on Sky1, msm on the Q8B), there is no baseline, and
[PERFORMANCE.md](PERFORMANCE.md) has no rule for that case yet (§13 question 7). Until it does,
those results are recorded and do not gate.

## 13. Open questions

1. **The amd64 native GPU.** Decided in [ADR-0018](adr/0018-desktop-architecture.md): RDNA2
   (amdgpu sources largely MIT, FreeBSD has `drm-kmod` amdgpu for a baseline). The desktop's metal
   target is a Navi 22 card, the Z690-A has one, and the X870E and B650E have RDNA2 iGPUs. The
   RTX 4070 (NVIDIA's open kernel modules are dual MIT/GPL, GSP firmware does most of the work,
   and FreeBSD's baseline is NVIDIA's own driver) comes later.
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

| Step | Work |
|---|---|
| **M-G.0** | This document finalized, the ADR for open decisions 6 and 7, and FreeBSD `drm-kmod` behavior recorded (§13 question 2) |
| **M-G.1** | `sys/drm` core (files, GEM, KMS atomic core, events), `gop-kms`, `vt` handover. Exit: a compositor from ports runs on `gop-kms` in QEMU and on both reference machines, with software rendering, and the desktop's `undertow` does too (DESKTOP.md M-D.2) |
| **M-G.2** | `sys/fence`, syncobj, sync files, dma-buf, `BufferObject` and its pager, `virtio-gpu` 2D. Exit: igt's core tests pass in QEMU |
| **M-G.3** | `virtio-gpu` 3D (Venus), with dma-buf import and export through DRM format modifiers. Exit: `vkcube` and Vulkan CTS subsets in QEMU, and a wlroots compositor on its Vulkan renderer (the desktop's `undertow`, DESKTOP.md §3.3) |
| **M-G.4** | Sky1 display aggregate. Exit: native modesetting on the Orange Pi 6 Plus, hotplug, backlight |
| **M-G.5** | `panthor`. Exit: panvk runs `vkcube` and the CTS subset on the Orange Pi 6 Plus |
| **M-G.6** | amd64 native GPU (RDNA2). Exit: parity with FreeBSD `drm-kmod` on the X870E and the Z690-A, with the desktop running accelerated (DESKTOP.md M-D.4) |
