# Driver Models: Survey and Analysis

This survey looks at how current operating systems structure device drivers, and what each design
gets right and wrong. It is the evidence base for the AbyssBSD driver model in
[DRIVER-MODEL.md](DRIVER-MODEL.md). It was written in October 2026.

**Method.** We used public documentation, papers, conference talks, mailing-list prose, and LWN
coverage. FreeBSD facts were checked against `../freebsd-src` (16.0-CURRENT). No GPL source code
was read for any system: Linux is described only from prose, per
[ADR-0007](adr/0007-license-and-clean-implementation.md). Claims we could not confirm against a
primary source are marked *(unverified)*. Benchmark numbers are the cited authors' own unless
stated otherwise.

## 1. Comparison axes

Every driver model answers the same questions. We compare along these axes:

| Axis | Question |
|---|---|
| Topology | What is a device, and how are devices related? |
| Matching | How is a driver chosen for a device? Code or data? Ranked? |
| Ordering | How are dependencies between devices (clocks, IOMMU, power, GPIO) expressed? |
| Lifecycle | Who owns the attach/detach/suspend/removal state machine: the driver or the framework? |
| Teardown | How does detach find and drain every asynchronous way into a driver? |
| Resources | How are MMIO, interrupts, DMA, and bus numbers granted and released? |
| Concurrency | What may run concurrently inside a driver, and in which context? |
| Interrupts | How are stacked interrupt controllers and MSI modeled? |
| DMA | How are constraints, bounce buffers, coherency, and the IOMMU hidden from drivers? |
| Power | How are device power states and suspend order derived? |
| Isolation | What contains a faulty driver, and at what cost? |
| Interface stability | Is the driver interface a stable ABI? |
| User interface | How does userland see devices and events? |
| Verification | What catches driver bugs before or during deployment? |

## 2. FreeBSD

FreeBSD's model is the behavioral reference for AbyssBSD, so it gets the most detail.

### 2.1 Mechanisms

- **newbus tree** (`sys/kern/subr_bus.c`, `sys/sys/bus.h`). Each `device_t` has a parent, ordered
  children, a `devclass` (the name and unit namespace, such as `em`), a driver, a unit, a softc, and
  bus-private *ivars*. Its state is one of `DS_NOTPRESENT`, `DS_ALIVE`, `DS_ATTACHING`, or
  `DS_ATTACHED`. Its `DF_*` flags include `DF_ENABLED`, `DF_FIXEDCLASS`, `DF_WILDCARD`, and
  `DF_SUSPENDED`. A busy count (`device_busy()`, propagated up the tree) makes detaching an open
  device fail with `EBUSY`.
- **kobj dispatch.** A `driver_t` is a kobj class with a method table, built from `DEVMETHOD`
  entries. Interfaces (`device_if.m`, `bus_if.m`, `pci_if.m`, `pcib_if.m`, `acpi_if.m`) are
  compiled to C stubs. Each call goes through a 256-entry per-class method cache and falls back to
  a walk of the class hierarchy. Inheritance (`DEFINE_CLASSN`) is how `acpi_pcib` reuses `pcib`.
- **Registration and ordering.** `DRIVER_MODULE` / `EARLY_DRIVER_MODULE` register a driver with a
  parent bus class at a **bus pass**: ROOT 0, BUS 10, CPU 20, RESOURCE 30, INTERRUPT 40, TIMER 50,
  SCHEDULER 60, SUPPORTDEV 100000, DEFAULT `INT_MAX`. Sub-orders FIRST through LAST refine a pass.
  `bus_set_pass()` raises the level monotonically and reprobes unattached children at each level.
- **Bidding probe.** `probe` returns a priority: `BUS_PROBE_SPECIFIC` 0, `VENDOR` −10, `DEFAULT`
  −20, `LOW_PRIORITY` −40, `GENERIC` −100, `HOOVER` −1000000, `NOWILDCARD` −2000000000. A positive
  value is an errno. The highest bid wins, and the winner's probe is run a second time, so probe
  must be idempotent.
- **Lifecycle methods**: `identify`, `probe`, `attach`, `detach`, `shutdown`, `suspend`, `resume`,
  `quiesce`. The generic helpers detach, shut down, and suspend children in reverse order and
  resume them forward. `config_intrhook` defers attach work until interrupts work.
- **Resources.** `rman` range allocators sit on buses. `bus_alloc_resource()` climbs the tree until
  a bus can satisfy the request. Each child has a `resource_list` (BARs, `_CRS`, hints). Activation
  yields `bus_space` tag/handle pairs.
- **Interrupts.** `bus_setup_intr()` takes a **filter**, which runs in interrupt context and returns
  `FILTER_STRAY`, `FILTER_HANDLED`, or `FILTER_SCHEDULE_THREAD`, and an **ithread handler**. arm64
  uses INTRNG (`subr_intr.c`: `intr_pic_register`, `intr_isrc`, typed map data for ACPI, FDT, and
  MSI). amd64 has its own `intr_machdep.c`. PCI MSI/MSI-X requests climb through `PCIB_ALLOC_MSI(X)`
  and `PCIB_MAP_MSI` to the MSI controller.
- **busdma** (derived from NetBSD's `bus_dma`). Tags inherit the most restrictive constraints from
  their parents: alignment, boundary, address limits, segment count and size. Map loads have
  callbacks, `bus_dmamap_sync` takes PRE/POST READ/WRITE operations, and there are bounce pages. An
  IOMMU (DMAR, AMD-Vi, SMMUv3) replaces the backend transparently.
- **Subsystems drivers plug into**: devfs (`make_dev_s`/`cdevsw`; `destroy_dev` drains threads
  still inside the driver), GEOM (`disk_create`), ifnet/iflib, CAM, and the USB stack.
- **LinuxKPI** hosts Linux-API drivers (drm-kmod, iwlwifi) over newbus. It shows newbus is rich
  enough to host a foreign model, and how much glue that takes.

**Userland ABI that depends on the model's shape.** AbyssBSD must reproduce all of this exactly:

| Interface | Shape | Consumers |
|---|---|---|
| `hw.bus.info` | `struct u_businfo { ub_version = 2, ub_generation }` | libdevinfo |
| `hw.bus.devices.<n>` | `struct u_device`: handle, parent, devflags, `DF_*` flags, state, `dv_fields[3072]` = `name\0desc\0driver\0pnpinfo\0location\0`. Iterate until `ENOENT`; restart if the generation changes | devinfo(8), devctl(8), devmatch(8), pciconf |
| `hw.bus.rman.<n>[.<m>]` | `struct u_rman`, `struct u_resource` | `devinfo -r`, `devinfo -u` |
| `dev.<class>.<unit>.%{desc,driver,location,pnpinfo,parent,iommu,domain}`, `dev.<class>.%parent` | Strings, plus driver sysctls under the node | sysctl(8), scripts |
| `/dev/devctl` | One reader (devd). Lines: `+name at <loc> <pnp> on <parent>`, `-name …`, `? at <loc> <pnp> on <parent>` (no match), `!system=X subsystem=Y type=Z [k=v…]` | devd(8), which re-broadcasts on `/var/run/devd.pipe` and `devd.seqpacket.pipe` |
| `/dev/devctl2` | `struct devreq` ioctls `DEV_ATTACH` (1) through `DEV_GET_PATH` (14): detach, enable, disable, suspend, resume, set/clear driver, rescan, delete, freeze, thaw, reset. Names resolve via nameunit, `pciD:B:S:F`, or an ACPI path | devctl(8), libdevctl |
| pnpinfo / location strings | PCI: `vendor=0x%04x device=0x%04x subvendor=… subdevice=… class=0x%06x` / `slot=%d function=%d dbsf=pci%d:%d:%d:%d`. ACPI: `_HID=%s _UID=%lu _CID=%s` / `handle=%s` | devd.conf rules, devmatch |
| `/dev/pci` | `PCIOCGETCONF`, `PCIOCREAD/WRITE`, `PCIOCATTACHED`, `PCIOCGETBAR`, `PCIOCLISTVPD`, `PCIOCBARMMAP`, `PCIOCBARIO`. `struct pci_conf` carries the attached driver's name and unit | pciconf(8) |

### 2.2 Strengths

- **The bus-recursive design is excellent.** Resources, interrupts, DMA tags, and IOMMU contexts
  all flow up the parent chain. A driver cannot tell whether its PCI function sits behind ACPI,
  FDT, or a virtio transport.
- **Bidding probe** gives clean vendor overrides and generic fallbacks.
- **One tree feeds the whole userland toolchain.** Stable names (`em0`), `dev.*`, devctl events,
  and `devinfo` all hang off it.
- **busdma constraint inheritance with a transparent IOMMU backend** means drivers do not change
  when the IOMMU is enabled.

### 2.3 Weaknesses (with evidence)

1. **The topology lock is Giant.** `bus_topo_mtx()` returns `&Giant`. Giant is dropped across
   sleeps, and detach sleeps (`destroy_dev` drains), so concurrent rescans or resets can
   double-detach. `bus_topo_lock()` arrived in 2021 ([D31831](https://reviews.freebsd.org/D31831)).
   A proper sleepable topology lock ([D53386](https://reviews.freebsd.org/D53386), 2025–26) is still
   in review. A 2009 attempt was reverted *(seen only in commit-history summaries)*.
2. **There is no general drain.** Detach must find every callout, taskqueue item, ithread, and cdev
   thread by hand. [D19375](https://reviews.freebsd.org/D19375) had to make detach return `EBUSY`
   while attach was still running, after `kldunload` crashed mid-attach. Our own Q8B bring-up hit
   the same class of bug: a callout that nothing drained at unload hard-hung the board (see
   `../AbyssBSD/docs/boards/radxa-dragon-q8b/lessons.md`).
3. **There is no dependency graph.** Ordering is a global numeric pass. A consumer whose supplier
   (clock, GPIO, IOMMU, PHY, power resource) attaches later must either fail or force the supplier
   into an earlier pass. `BUS_PASS_SUPPORTDEV` is the catch-all.
4. **Suspend and resume follow tree order only.** Cross-tree dependencies, such as an ACPI power
   resource or the IOMMU, are not expressed.
5. **Typing is weak.** Ivars are untyped `uintptr_t` side channels, a softc is a `void *`, and kobj
   signatures are checked only by generated stubs. Every method call costs an indirect call and a
   cache probe, and global hit/miss counters bounce cache lines *(the performance impact is our
   inference)*.
6. **Probe runs twice** and must be side-effect free, a rule that is often violated.
7. **The userland ABI is a raw struct dump**: a fixed 3 KiB string field, a generation-retry
   protocol, and `/dev/devctl` limited to a single reader. It is fixed now, and we must match it.

### 2.4 Takeaway

Keep the concepts userland sees: one tree, devclass and unit names, `DS_*`/`DF_*`, bidding ranks,
bus-recursive resources, busdma, and the filter+ithread split. Replace the internals: kobj, Giant,
manual drain, numeric passes, and untyped ivars.

## 3. NetBSD and OpenBSD

- **autoconf(9)** dates from 4.4BSD. Each driver has a `cfdriver` and a `cfattach`
  (`match`/`attach`/`detach`/`activate`). config(8) generates a static `cfdata` table from the
  kernel configuration, with **locators** (`at pci? dev ? function ?`) that can pin devices to
  positions. Parents call `config_found()`, `config_match()` returns a priority, and
  `config_defer()`/`config_interrupts()` defer attach work. NetBSD adds `pmf(9)` power hooks,
  `drvctl(4)`, and devpubd. OpenBSD has one `activate()` hook taking `DVACT_QUIESCE`, `SUSPEND`,
  `RESUME`, and `POWERDOWN`.
- **bus_space(9)** and **bus_dma(9)** (Thorpe, [USENIX 1998](https://www.netbsd.org/docs/kernel/bus_dma.pdf))
  are the origin of FreeBSD's equivalents.
- **Strengths.** Plain function pointers with no dispatch layer. Portable, well-documented
  register and DMA abstractions. Deterministic naming. OpenBSD's single phased `activate()` makes
  suspend ordering simple.
- **Weaknesses.** The static configuration table makes part of the tree shape a build artifact.
  Detach and hotplug support was retrofitted late *(unverified detail)*. autoconf ran under
  `KERNEL_LOCK` for a long time. Each bus has its own ad-hoc `*_attach_args` with no shared typing.
- **Takeaway.** A static Rust kernel is close to config(8) in spirit: the driver set is fixed at
  build time. Matching must still happen at runtime from ID tables, because FreeBSD userland
  expects wildcard units and nomatch events. OpenBSD's phased `activate()` is a good template for a
  typed power-transition enum.

## 4. illumos / Solaris

- **DDI/DKI**: documented, versioned entry points (section 9E) and services (9F). `dev_ops` has
  `attach`/`detach` taking `DDI_ATTACH`/`DDI_RESUME` and `DDI_DETACH`/`DDI_SUSPEND`, plus `power`
  and `quiesce`. Nexus drivers implement `bus_ops`, and leaf requests become calls to the parent
  nexus. Nodes carry typed properties (`ddi_prop_*`) from firmware, `driver.conf`, or the driver.
- **Binding** goes through `/etc/driver_aliases` with `compatible`-style names. Instance numbers
  persist across boots in `/etc/path_to_instance`. The kernel publishes physical nodes under
  `/devices`, and the devfsadm daemon builds `/dev` names from link-generator modules.
- **Strengths.** A stable binary DDI that kept third-party drivers working across releases.
  Typed properties. A clean split between the kernel's physical tree and userland naming policy.
  Integration with fault management (FMA) for device retire.
- **Weaknesses.** Many moving parts: devfsadm, `path_to_instance`, and `devlink.tab`, where a stale
  `path_to_instance` is a classic admin problem. The stable ABI froze old designs (the
  `ddi_dma_setup` era).
- **Takeaway.** Typed properties beat untyped ivars. FreeBSD's newer
  `device_get_property()` (`DEVICE_PROP_BUFFER/UINT32/UINT64/HANDLE`) moves the same way. The stable
  ABI that matters to AbyssBSD is the userland one, not a driver ABI.

## 5. Linux

Linux is described from prose only (LWN, docs.kernel.org concept pages, talks). Its internal
designs are informative. Its user interfaces (sysfs, uevents, modalias, udev) are Linuxisms we do
not adopt ([VISION](VISION.md) goal 11).

### 5.1 Mechanisms

- **Unified device model.** Each `struct device` is refcounted through a kobject. Each bus type
  defines matching and probing. Drivers carry declarative **ID tables** (PCI, USB, ACPI
  `_HID`/`_CID`, DT `compatible`). Classes group devices by function. sysfs exports the tree.
  [Overview](https://docs.kernel.org/driver-api/driver-model/overview.html).
- **Two lifetimes.** The *device object* lives while anyone holds a reference. The *driver
  binding* lasts from `probe` to `remove`. Much of the trouble below comes from confusing them.
- **Deferred probe.** When a supplier is not ready, probe returns `-EPROBE_DEFER`. The core retries
  every deferred device after each successful probe.
- **Device links and fw_devlink.** Supplier→consumer links
  ([docs](https://docs.kernel.org/driver-api/device_link.html)) order probe, runtime PM, suspend,
  and shutdown. fw_devlink creates them from firmware references (DT phandles), so a consumer is
  not probed until its suppliers have bound.
- **Managed resources (devres, `devm_*`)** are released in reverse order on probe failure or after
  `remove()`. [docs](https://docs.kernel.org/driver-api/driver-model/devres.html).
- **Auxiliary bus** ([LWN](https://lwn.net/Articles/840416/)): one function split into sub-devices
  for other drivers. **Component framework**: an aggregate driver waits for N components.
- **Hierarchical irq_domain** (device → MSI → remapping → vector or GIC), and threaded IRQs.
  [docs](https://docs.kernel.org/core-api/irq/irq-domain.html).
- **DMA API**: streaming maps with explicit `sync_for_cpu`/`sync_for_device` ownership hand-off,
  coherent allocations, and swiotlb or IOMMU backends. IOMMU groups are the unit of isolation.
- **Power**: system-sleep phases plus usage-counted runtime PM. Suspend order is registration order,
  adjusted by device links.
- **No stable in-kernel API**
  ([stable-api-nonsense](https://docs.kernel.org/process/stable-api-nonsense.html)). Internal APIs
  change freely, and every in-tree driver is fixed along with them.

### 5.2 Rust for Linux driver abstractions

These matter most to us, because they are retrofits of invariants a Rust kernel can have from day
one.

- **Device context typestates.** `Device` (refcounted only) → `Device<Bound>` (guaranteed bound for
  the reference's lifetime) → `Device<Core>` (inside bus callbacks, device lock held). APIs that
  need a bound device, such as I/O mapping and coherent DMA, demand `&Device<Bound>`.
  ([patch prose](https://patchew.org/linux/20250331202805.338468-1-dakr@kernel.org),
  [LWN](https://lwn.net/Articles/1059348/)).
- **`Revocable<T>` / `Devres<T>`.** A resource is revoked at unbind, and anything that outlives the
  binding (a work item, an open file) finds it gone instead of touching freed MMIO. A race in
  `Devres` drop was found and fixed in 2025.
- **Binding-lifetime ("higher-ranked lifetime") series, 2026.** Driver data had to be `'static`, so
  drivers stored refcounted devices and `Arc<Devres<T>>` everywhere and used fallible access even in
  destructors. The series makes driver data generic over a `'bound` lifetime that I/O mappings
  carry ([LWN](https://lwn.net/Articles/1074047/)). It is under review, and its merge status is
  *unverified*.
- **pin-init**: fallible, in-place construction for objects that must not move (locks, lists, wait
  queues).
- **Experience reports.** The Asahi AGX GPU driver reported no kernel oopses apart from firmware
  crashes ([Asahi](https://asahilinux.org/2022/11/tales-of-the-m1-gpu/)). Its author found that the
  DRM scheduler's unwritten lifetime rules (fences outliving the scheduler) could not be wrapped
  safely, and wrote a separate scheduler ([LWN](https://lwn.net/Articles/988438)). Rust was declared
  no longer experimental at the 2025 Maintainers Summit ([LWN](https://lwn.net/Articles/1050174)).

### 5.3 Strengths

- One refcounted object model with a uniform bind/unbind/PM lifecycle.
- **Matching is data.** An ACPI-enumerated and a DT-enumerated device differ only in the ID table.
- **One dependency graph** (device links) drives probe order, runtime PM, suspend, and shutdown.
- Composable answers for "one device, many drivers" (auxiliary bus) and "many devices, one driver"
  (components).
- A hierarchical irq_domain that matches the stacked controllers on real ACPI machines (MSI →
  ITS → GIC, MSI → remapping → vector).

### 5.4 Weaknesses (with evidence)

1. **The devm lifetime trap.** devres frees at *unbind*, but user-visible objects live until the
   *last reference*. Laurent Pinchart's LPC 2022 talk counted more than 6000 `devm_kzalloc` call
   sites, many of them latent use-after-free bugs
   ([slides](https://linuxplumbersconf.org/event/16/contributions/1227/contribution.pdf)). Ordering
   bugs (an IRQ released by devres after `remove()` freed its handler's data) are a recurring CVE
   class *(the CVE examples came from secondary aggregators)*.
2. **Deferred probe is a blind busy-retry.** It cannot tell "the supplier comes later" from "the
   supplier never will". That led to timeout hacks, and the diagnostic lists are close to useless
   ([LKML](https://lkml.iu.edu/hypermail/linux/kernel/1510.2/04968.html)). fw_devlink reduced
   deferrals but added new failure modes: cycles, optional suppliers, and firmware nodes with no
   device.
3. **Unbind and hot-unplug races are structural.** Every subsystem reinvents a "device is gone"
   flag. A C "revocable" mechanism was proposed, reverted, and resubmitted in 2026, and maintainers
   still disagree about where the fix belongs
   ([LKML](https://lkml.iu.edu/2601.3/07837.html)).
4. **device_lock is coarse.** Holding it across probe makes nested probes and sysfs callbacks
   deadlock-prone *(background knowledge)*.
5. **C APIs with implicit lifetime contracts resist safe wrapping** (the DRM scheduler, and the
   placement fight over the DMA bindings).
6. **sysfs became an accidental ABI** with thousands of ad-hoc attributes. Policy moved to udev.

### 5.5 Takeaway

Adopt the internal ideas: data matching, a supplier/consumer graph used for every ordering
decision, an hierarchical interrupt domain, typed DMA ownership, the three lifetimes, and a typed
"bound" context. Reject blind deferred probe, binding-scoped allocation of user-visible objects,
and every Linux user interface.

## 6. macOS (IOKit and DriverKit)

### 6.1 Mechanisms

- **IOKit** ([IOKit Fundamentals](https://developer.apple.com/library/archive/documentation/DeviceDrivers/Conceptual/IOKitFundamentals/Introduction/Introduction.html))
  is a restricted Embedded C++ object model (`OSObject`, `OSMetaClass`, no exceptions or RTTI).
  - The **I/O Registry** is a live graph with several *planes* (service, device tree, power) and
    typed properties that userland can browse (`ioreg`).
  - **Matching** is driven by data. Info.plist **personalities** name a provider class and
    family-specific keys (PCI IDs, USB class triples), plus an `IOProbeScore`. Passive matching on
    properties comes first. Then candidates' `probe()` is called in score order, and the best one
    gets `start()`.
  - **Nubs**: a driver attaches to its provider's nub and publishes nubs for its children (AHCI →
    port nubs → block storage → `IOMedia` → partitions). **Families** such as `IOPCIFamily` and
    `IOStorageFamily` own the class logic, so leaf drivers stay small.
  - **Termination** is a multi-phase protocol (`terminate` → `willTerminate` / `didTerminate` → wait
    for clients to close → `stop` → `detach`). That makes surprise removal well defined.
  - **`IOWorkLoop`**: a per-driver thread plus gate. Interrupt, timer, and command-gate event
    sources run serialized on it. Drivers are single-threaded by default.
  - **DMA**: `IOMemoryDescriptor` plus `IODMACommand` (constraints, segments, bouncing, IOMMU).
    On Apple Silicon, nearly every DMA agent sits behind its own **DART** IOMMU.
  - **Power**: a separate power tree. Drivers register an `IOPMPowerState` table, and transitions
    run children-first on the way down and parents-first on the way up.
  - **`IOUserClient::externalMethod`** is the user interface, and historically the largest
    kernel attack surface *(well documented by jailbreak research; no single citation)*.
- **DriverKit** ([WWDC19 702](https://developer.apple.com/videos/play/wwdc2019/702/),
  [WWDC20 10210](https://developer.apple.com/videos/play/wwdc2020/10210/)) runs drivers as
  user-space *dexts* with the same matching, registry, and object model. Interfaces are written in
  IIG and calls travel as Mach messages. Serial `IODispatchQueue`s replace the workloop. Kexts for
  covered families were deprecated from 2019.
- **Kernel collections.** Since Big Sur, the kernel and Apple's kexts are prelinked into one boot
  collection. In practice macOS has converged on a mostly static kernel image.

### 6.2 Strengths and weaknesses

- **Strengths.** Match data is separate from code, and probe scores are deterministic. Families
  keep leaf drivers small. A termination protocol makes surprise removal well defined.
  Single-threaded drivers by default. A table-driven power tree. Moving to DriverKit was
  evolutionary because the model stayed the same.
- **Weaknesses.** The C++ vtable ABI is fragile: reserved padding slots are a finite, manual
  resource. Termination and power-ack protocols are hard to get right. DriverKit costs throughput:
  developers report about 645 vs 565 MB/s for a storage driver, blamed on serial-queue submission
  ([forum](https://developer.apple.com/forums/thread/132431)), and about 60% of a USB audio dext's
  time in `mach_msg` ([forum](https://developer.apple.com/forums/thread/692248)). Both are anecdotes,
  not Apple benchmarks.
- **Takeaway.** Use families (classes) as the main abstraction. Make termination a framework
  protocol. Serialize drivers by default, but make parallel queues an explicit option, because a
  serial queue on the data path is exactly where DriverKit loses.

## 7. Windows NT (WDM and WDF)

### 7.1 Mechanisms

- **WDM.** One `DRIVER_OBJECT` per driver, and one `DEVICE_OBJECT` per device per layer. Device
  stacks are made of a PDO (created by the bus driver), an FDO (created by the function driver),
  and filter DOs. **IRPs** carry a stack location per layer, are passed down with `IoCallDriver`,
  and unwind through completion routines. The PnP manager sends start, query-remove, remove,
  surprise-removal, and stop (for rebalancing). Power IRPs flow through the power-policy owner.
  **IRQL** levels (PASSIVE, APC, DISPATCH, DIRQL) decide what may block. An ISR queues a DPC.
- **WDF (KMDF, UMDF 2)**, [open source since 2015](https://github.com/microsoft/Windows-Driver-Frameworks):
  - an **object hierarchy** in which deleting a parent deletes its children, with typed per-object
    context and cleanup callbacks;
  - `WDFQUEUE` dispatch types (Sequential, Parallel, Manual), power-managed by default, with
    framework-owned cancellation;
  - **a built-in PnP/power/power-policy state machine**, so a driver supplies only callbacks
    (`PrepareHardware`, `D0Entry`, `D0Exit`, `ReleaseHardware`);
  - **synchronization scopes** (device or queue) and **execution levels**;
  - I/O targets that track whether the lower device is removed;
  - DMA enablers and transactions;
  - **passive-level interrupts** for devices behind I2C/GPIO, where acknowledging the interrupt
    needs a bus transaction ([docs](https://learn.microsoft.com/windows-hardware/drivers/wdf/supporting-passive-level-interrupts)).
  - UMDF runs the same API in a host process behind a kernel reflector, and a crashing UMDF driver
    is restarted.
- **Verification**: Driver Verifier (special pool, forced IRQL checking, I/O and DMA verification,
  deadlock detection, fault injection), the WDF verifier, Static Driver Verifier (SLAM model
  checking), SAL annotations, the HLK certification kit, and mandatory signing.
- **Ranking**: INF hardware IDs from most to least specific (`VEN&DEV&SUBSYS&REV` → `VEN&DEV` →
  class code as a compatible ID), then signature, then date
  ([docs](https://learn.microsoft.com/windows-hardware/drivers/install/how-windows-ranks-drivers--windows-vista-and-later-)).
- **Moving work out of the kernel.** WDDM splits graphics into a large user-mode driver and a small
  kernel miniport, and **TDR** resets a hung GPU instead of crashing
  ([docs](https://learn.microsoft.com/windows-hardware/drivers/display/timeout-detection-and-recovery)).
  After the CrowdStrike incident (July 2024, about 8.5 million machines), the Windows Resiliency
  Initiative moved security products out of kernel mode
  ([Windows blog](https://blogs.windows.com/windowsexperience/2024/11/19/windows-security-and-resiliency-protecting-your-business/)).
- **Rust**: [windows-drivers-rs](https://github.com/microsoft/windows-drivers-rs) wraps WDM, KMDF,
  and UMDF. It still needs `unsafe` for most OS interaction, because it wraps C APIs.

### 7.2 Strengths and weaknesses

- **Evidence.** "Bugs in kernel-level device drivers cause 85% of the system crashes in Windows XP"
  ([Ball et al., EuroSys 2006](https://cs.uwaterloo.ca/~brecht/courses/epfl/Possible-Readings/device-drivers/static-anal-device-drivers-eurosys-2006.pdf)).
  NVIDIA drivers caused 28.8% of logged Vista crashes in 2007
  ([The Register](https://www.theregister.com/2008/03/28/nvidia_vista_drivers/)).
- **WDM is notoriously hard to get right.** The pending/completion rules, cancellation racing
  completion under a global spinlock (which is why cancel-safe queues exist), remove locks, the
  power IRP dance, and IRQL rules checked only at runtime all work against correctness. WDF exists
  because most WDM drivers got PnP and power wrong.
- **WDF is the strongest lesson in the survey.** It wrote the hardest code (PnP, power, removal, and
  cancellation interactions) once, as one tested state machine, and turned drivers into sets of
  callbacks. Its parent-hierarchy cleanup removed whole classes of leaks.
- **Generic filter stacks** are composable, but they cause much of WDM's complexity.
- **Takeaway.** The framework owns the lifecycle state machine. Requests are completed exactly once.
  The execution context is in the type. Ranking is data. Build a verifier mode. Use filters only at
  class-defined points (GEOM classes, ifnet taps), never as a generic stack.

## 8. Fuchsia (Driver Framework v2)

- **Mechanisms** ([overview](https://fuchsia.dev/fuchsia-src/concepts/drivers/driver_framework)):
  - A **driver manager** keeps the node topology and asks the **driver index** to match nodes.
  - Drivers run in **driver host** processes, optionally colocated with their parent.
  - **Bind rules** are a small declarative language over typed, namespaced properties. They are
    compiled to bytecode and come with match tests
    ([docs](https://fuchsia.dev/fuchsia-src/development/drivers/concepts/device_driver_model/driver-binding)).
  - **Composite node specs** declare multi-parent devices with one primary parent and optional
    parents ([RFC-0197](https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0197_node_groups)).
- **Driver runtime** ([RFC-0126](https://fuchsia.dev/docs/contribute/governance/rfcs/0126_driver_runtime)):
  - in-process channels with arena-backed zero-copy messages;
  - *synchronized* dispatchers (never two threads in a driver at once) or *unsynchronized* ones,
    with blocking declared up front;
  - a call to a colocated driver runs **inline in the caller's stack frame** when that driver's
    lock is free, and is queued otherwise;
  - re-entry into a driver already on the stack is queued, so drivers never need recursive locks.
- **Banjo**, the in-process C-ABI vtable layer, was deprecated: it could not evolve and gave no
  threading guarantees. **devfs was deprecated** in favor of explicit services
  ([RFC-0263](https://fuchsia.dev/docs/contribute/governance/rfcs/0263_migrate_driver_communication_to_services)).
  Topological paths leaked implementation details and hid dependencies from the component manager.
- **Evidence.** RFC-0126's benchmarks of putting channels and thread hops under the block and
  network stacks found throughput down, tail latency up, and **CPU use up 50–150%**, worst at queue
  depth 1–2. That result is why the runtime calls inline.
- **Weaknesses.** The DFv1→v2 migration needed a compatibility shim and a long playbook. The ABI is
  not yet stable.
- **Takeaway.** Matching is declarative and host-tested. Composites are explicit. A
  synchronized/unsynchronized contract with no thread hop on the data path. Make `/dev` a
  *projection* of typed class services, not the binding mechanism.

## 9. Rust kernels

| System | Model | Evidence | Lesson for us |
|---|---|---|---|
| **Asterinas** ([paper](https://arxiv.org/html/2506.03876v1)) | "Framekernel": all `unsafe` sits in OSTD, and drivers are safe Rust. `IoMem` is only constructible over ranges labeled *insensitive*, so a driver cannot map the LAPIC or the IOMMU. DMA is default-deny through the IOMMU and covers only *untyped* frames, with POD-only access to device-writable memory. Interrupt remapping is on | The TCB is about 14% of the code. Normalized to Linux: LMbench 1.08, Nginx 1.17, SQLite 0.85. Dynamic IOMMU map/unmap is expensive, so DMA regions are pooled. Incomplete pooling cost up to 8.3% on SQLite | Closest to our model. A small audited `unsafe` hardware layer, vetted MMIO ranges, POD-only device memory, pooled long-lived IOMMU mappings |
| **RedLeaf** ([OSDI 2020](https://www.usenix.org/conference/osdi20/presentation/narayanan-vikram)) | Language-based domains, an exchange heap, `RRef<T>` ownership, generated proxies, shadow-driver restart | A cross-domain call costs 124 cycles. ixgbe 6.5 Mpps vs DPDK 6.7. NVMe 457K vs SPDK 452K IOPS. **Statically linked nullnet: 29.5 Mpps; with two domain crossings: 5.3 Mpps** | Keep crossings off the data path. Move semantics on I/O buffers is worth having on its own |
| **Theseus** ([OSDI 2020](https://usenix.org/conference/osdi20/presentation/boos)) | Single address space, runtime-loaded cells, invariants pushed into types, no "state spill" | Recovery of injected faults reported at 69% *(press figure, unverified)* | No hidden global registrations: driver state lives in the driver instance, so detach and re-probe are clean |
| **Tock** | Safe-Rust capsules, HIL traits, no kernel heap, grants | No liveness isolation | Cross-platform bus traits (I2C, SPI, GPIO, UART). Static instances |
| **Hubris** ([reference](https://hubris.oxide.computer/reference/)) | Tasks fixed at build time, drivers as tasks, synchronous IPC with *leases* | Microcontroller scale | Build-time device and driver manifests. A lease is an IPC borrow |
| **Redox** ([book](https://doc.redox-os.org/book/drivers.html)) | User-space scheme daemons, `pcid` spawning drivers from a config | No driver benchmarks found. Drivers still use `unsafe` freely | Too costly for a 2% bound |

## 10. Microkernel and component systems

- **seL4 Device Driver Framework (sDDF)** ([project](https://trustworthy.systems/projects/drivers/),
  [design](https://trustworthy.systems/projects/drivers/sddf-design.pdf)). A driver's only job is
  to translate a device interface into a device-class interface. It is single-threaded and talks
  over bounded **single-producer/single-consumer rings** (free and used queues) with
  notifications as doorbells. *Virtualisers* multiplex the device between clients outside the
  driver. Reported results: the 1 GbE link saturated on one core, while Linux maxes the CPU at about
  500 Mb/s, and the Ethernet driver is under 600 lines against about 5000. The project page's
  figures have been revised over time, so treat them as qualitative.
- **Genode**: typed session interfaces (Block, Nic, Platform). A **platform driver** owns PCI, ACPI,
  and the IOMMU, and hands each driver only its own MMIO, IRQs, and DMA buffers. Since 23.11 the
  IOMMU driver runs in user space ([release notes](https://genode.org/documentation/release-notes/23.11)).
- **QNX**: drivers are resource-manager processes serving POSIX messages. They can be restarted
  independently, at the cost of a message pass per operation *(from memory)*.
- **MINIX 3**: the reincarnation server restarts failed drivers. Fault-injection campaigns survived
  thousands of driver crashes ([;login:](https://www.usenix.net/system/files/login/articles/618-herder.pdf)).
  Recovery is easy for stateless drivers and hard for stateful ones (in-flight block writes).
- **Takeaway.** The *interface* is the lesson, not the address space. Queue pairs of buffer
  descriptors between driver and class, multiplexing in the class, and a single resource broker
  that grants each driver typed access to only its own hardware. In a monolithic kernel those
  queues are plain memory with no crossings.

## 11. Haiku and Plan 9

- **Haiku**: kernel add-ons. The newer device manager keeps a node tree with attributes, and
  drivers report a support score. Legacy and new driver APIs coexist, which is the same migration
  cost Fuchsia paid *(from memory)*.
- **Plan 9**: devices are small file servers (`ctl` plus `data`) bound into per-process namespaces.
  The lesson is to keep in-kernel class interfaces small and uniform. FreeBSD fixes `/dev` plus
  `ioctl` as the user interface.

## 12. Research results

- **Driver bug density.** Chou et al. (SOSP 2001) found error rates in drivers up to 7× those of
  the rest of the kernel. Palix et al. ([ASPLOS 2011](https://faultlinux.lip6.fr)) found drivers
  still had the most faults by count, but their fault *rate* had fallen below arch and fs. Drivers
  dominate bug counts because they dominate code.
- **CVEs.** A 2024 SoK on driver isolation ([arXiv 2412.16754](https://arxiv.org/pdf/2412.16754))
  puts drivers at 16.9% of Linux kernel CVEs in 2014, a 59.1% peak in 2017, and 28.5% in 2023. It
  also finds an average of **33 compartment-interface vulnerabilities per isolated driver**:
  isolation boundaries leak through badly designed interfaces.
- **Nooks** (SOSP 2003) recovered from 99% of crashing driver faults, at significant CPU cost on
  high-rate network workloads *(the size of the overhead is from memory)*. **Shadow drivers**
  (OSDI 2004) replay a driver's state after restart. **SUD** (ATC 2010) runs drivers in user space
  confined by the IOMMU. **Decaf** (ATC 2009) keeps a small kernel fast path and moves the rest out.
  **Dingo and Termite** (2009) show that an explicit driver protocol state machine removes most
  concurrency bugs.
- **Thunderclap** ([NDSS 2019](https://www.csl.sri.com/~neumann/ndss-iommu.pdf)): a malicious
  peripheral defeated IOMMU protection on macOS, FreeBSD, and Linux. It read sub-page data that
  shared a mapped page, used stale mappings, and found kernel pointers in DMA-visible structures.
  An IOMMU alone is not enough. What a page *contains* matters.
- **Queue-pair interfaces.** NVMe SQ/CQ, virtio virtqueues, io_uring, and SPDK/DPDK poll-mode
  drivers all use batched descriptor rings with doorbells. That is the de facto model, and RedLeaf
  and sDDF match kernel-bypass performance with it.

## 13. Cross-cutting analysis

### 13.1 Comparison matrix

| | FreeBSD | Linux | IOKit | WDF | Fuchsia | Asterinas | sDDF |
|---|---|---|---|---|---|---|---|
| Matching | Code (bidding probe) | Data (ID tables) | Data + scored probe | Data (INF ranks) | Data (bind rules) | Code | Static config |
| Ordering | Numeric passes | Deferred probe + links | Async matching | PnP manager | Composite specs | Init order | Static system |
| Lifecycle owner | Driver | Driver (devres helps) | Framework protocol | **Framework state machine** | Framework | Driver | Static |
| Teardown drain | Manual | devres (unbind-scoped) | Termination phases | Object hierarchy | Dispatcher shutdown | Drop | n/a |
| Concurrency | Driver's own locks | Driver's own locks | Workloop (serial) | Sync scope (declared) | Dispatcher (declared) | Driver's own locks | Single thread |
| DMA | busdma tags | DMA API | IODMACommand | DMA enabler | Platform protocols | Default-deny IOMMU | Virtualiser |
| Isolation | None | None | Optional user space | Optional user space | Processes | Types | Address spaces |
| Driver ABI | Unstable KLD | None (by policy) | Fragile C++ | Stable | Unstable | n/a | n/a |

### 13.2 Recurring failure classes

The same bugs show up in every system that leaves these jobs to the driver:

1. **Asynchronous entry points that outlive teardown**: callouts, tasks, interrupt handlers, and
   open files. Seen in FreeBSD (manual drain, the Q8B hang), Linux (devm ordering), IOKit
   (termination), and WDM (remove locks).
2. **Confusing the device's lifetime with the binding's lifetime with the user-visible object's
   lifetime.** Linux devm is the clearest case, and Rust for Linux spent three years retrofitting
   the distinction.
3. **Ordering by convention**: numeric passes, blind retry, or registration order, instead of a
   dependency graph.
4. **Cancellation and completion races**: WDM IRPs, until WDF moved cancellation into queues.
5. **Wrong execution context**: sleeping or touching paged memory at raised IRQL, or with a spin
   lock held. Windows needs Driver Verifier's forced IRQL checks to find these.
6. **DMA ownership errors**: a missing sync, the CPU touching a buffer the device owns, metadata in
   DMA-visible pages (Thunderclap).
7. **Touching a device whose power domain is off.** On the Q8B this resets the SoC (`lessons.md`:
   "vote the domain on before touching a clock controller").
8. **Locks held across probe**: Linux `device_lock`, FreeBSD Giant, and a LinuxKPI IRQ lookup under
   a shared bus lock that self-deadlocked during boot on the Q8B.

### 13.3 Convergent evolution

Independently, the mature systems moved toward the same things:

- **Declarative matching with ranking**: Linux ID tables, IOKit personalities and scores, INF
  ranks, Fuchsia bind rules. FreeBSD's bidding probe gives ranking but in code.
- **A framework-owned lifecycle**: WDF, the IOKit termination protocol, Fuchsia's driver manager.
- **Explicit dependency graphs**: Linux device links and fw_devlink, Fuchsia composites, WDF's
  PnP manager.
- **Declared concurrency contracts**: the IOKit workloop, WDF sync scopes, Fuchsia dispatchers.
- **Families or classes** that own protocol logic and keep leaf drivers small: IOKit, sDDF, Genode
  sessions, FreeBSD's GEOM and iflib.
- **Mandatory DMA abstraction with the IOMMU behind it**: everywhere, with Apple Silicon's per-agent
  DART as the end state.

### 13.4 What isolation costs

| Mechanism | Measured cost | Source |
|---|---|---|
| Thread hop or channel per driver call | +50–150% CPU, worst at low queue depth | Fuchsia RFC-0126 |
| Domain crossing per I/O, unbatched | 29.5 → 5.3 Mpps (5.6×) | RedLeaf |
| User-space driver with serial queue | about 12% throughput (one report) | Apple developer forums |
| Dynamic IOMMU map/unmap per I/O | up to 8.3% (SQLite), more without pooling | Asterinas |
| Language-based safety, no crossings | about parity with C and Linux | Asterinas, RedLeaf (statically linked) |

Against a bound of 2% slower than FreeBSD ([ADR-0011](adr/0011-performance-parity.md)), the
evidence is clear. Address-space or thread-hop isolation on the data path is out. Type-based
safety with no crossings is affordable. IOMMU use must be built around pooled, long-lived mappings.

### 13.5 What the evidence says about safety

- Memory safety removes the largest bug class. The Asahi GPU driver and Asterinas are the strongest
  evidence that drivers in safe Rust work in practice.
- Memory safety does not fix lifecycle, ordering, liveness, or DMA-content bugs. The SoK's
  interface-vulnerability counts and the Q8B hangs (power domains, undrained callouts) are not
  memory-safety bugs. They need a framework that owns the protocol.
- A safe wrapper over an API with unwritten lifetime rules is not safe (the DRM scheduler,
  windows-drivers-rs). We control both sides of every interface, so the rules can be written into
  the types.

## 14. Implications for AbyssBSD

These carry directly into [DRIVER-MODEL.md](DRIVER-MODEL.md):

1. **Keep FreeBSD's user-visible model byte for byte**: the tree, devclass and unit names,
   `DS_*`/`DF_*`, `hw.bus.*`, `dev.*`, `/dev/devctl`, `/dev/devctl2`, `/dev/pci`, and the pnpinfo and
   location strings.
2. **Replace newbus internals.** Trait dispatch instead of kobj. A real sleepable topology lock. A
   framework-owned lifecycle state machine. Typed per-bus device views instead of ivars.
3. **Matching is data**, using FreeBSD's rank values and checked at build time for ambiguity.
4. **One dependency graph**, derived from ACPI before attach, governs attach order, suspend,
   resume, and shutdown. Deferral always names what it waits for.
5. **Keep the three lifetimes distinct in the types**: device node, binding, and user-visible
   handle. User-visible handles reach the driver only through a revocation gate.
6. **Every asynchronous way into a driver is created through its binding**, so detach can find and
   drain them all.
7. **A declared concurrency contract** (serialized by default, concurrent by opt-in), with no thread
   hop on the data path.
8. **Context tokens** for interrupt and sleepable context, enforced at compile time.
9. **A platform resource broker.** Drivers get typed, vetted grants (MMIO, IRQ, DMA) and never raw
   hardware. Driver crates forbid `unsafe`.
10. **Typed DMA ownership** with move semantics. POD-only device memory. Pooled IOMMU mappings.
    No kernel pointers in DMA-visible pages.
11. **Classes with queue-pair interfaces** own protocol logic. Leaf drivers stay small.
12. **A verifier mode** from the first driver, plus differential tests of every userland-visible
    string and struct against FreeBSD 15.1.
13. **No stable driver ABI** while the kernel is static ([ADR-0005](adr/0005-monolithic-static-kernel.md)).
    If modules ever come, their interface is designed then, not frozen now.
