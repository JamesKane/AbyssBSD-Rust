# Driver Model

- Status: **Accepted** ([ADR-0012](adr/0012-driver-model.md))
- Date: 2026-10-18. Amended 2026-10-27 after a review of the clean-room specifications
  (`~/Projects/OS/chinese-wall/specs/`): firmware images (§8.6), platform description and overlays
  (§5.5), dependency edge kinds and sources (§6.1), concrete platform providers (§8.5, §9.3), and
  in-driver recovery (§13.3)
- Scope: the device framework in `sys/bus` and its neighbors (P3 onward)
- Evidence: [DRIVER-MODEL-SURVEY.md](DRIVER-MODEL-SURVEY.md)

This document describes how AbyssBSD finds devices, chooses drivers for them, gives drivers their
hardware, runs them, and takes them away again. It supersedes the earlier sketch in
[ARCHITECTURE.md §6](ARCHITECTURE.md#6-device-model-sysbus). Section numbers in the form *S§n* refer
to the survey.

The short version: **FreeBSD's newbus as userland sees it, with internals that would be at home in
WDF and Rust for Linux.** The framework owns the lifecycle. Matching is data. A dependency graph
replaces bus passes. The device node, the binding, and the user-visible handle are three separate
lifetimes in the types. Every asynchronous way into a driver is created through its binding, so
detach can drain all of them. Driver crates contain no `unsafe`.

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal | Why (survey) |
|---|---|---|
| G1 | **Exact FreeBSD userland view**: device names and units, `devinfo`, `devctl`, `devd` events, `sysctl dev.*`, `pciconf`, and `/dev` nodes behave as on FreeBSD 15.1 | [COMPATIBILITY.md](COMPATIBILITY.md), S§2.1 |
| G2 | **Detach is always safe**: no use of a driver or its hardware after detach, whatever triggered it (devctl, surprise removal, shutdown, attach failure) | S§13.2 classes 1, 2, 4 |
| G3 | **Ordering is derived, not configured**: attach, suspend, resume, and shutdown order come from one dependency graph | S§2.3.3, S§5.4.2 |
| G4 | **Drivers are safe Rust**: `#![forbid(unsafe_code)]` in every `drivers/*` crate | S§9 (Asterinas), [ENGINEERING.md §4](ENGINEERING.md#4-unsafe-policy) |
| G5 | **No performance cost against FreeBSD** on interrupt, I/O submission, completion, and DMA paths | [ADR-0011](adr/0011-performance-parity.md), S§13.4 |
| G6 | **Wrong execution context is a compile error**: no sleeping in interrupt context, no blocking in a filter | S§7.2, S§13.2 class 5 |
| G7 | **Hostile or broken devices do not crash the kernel**: device-written data is untrusted input, and DMA is confined once the IOMMU lands | S§12 (Thunderclap), [ENGINEERING.md §3.4](ENGINEERING.md#34-errors-are-values) |
| G8 | **Host-testable logic**: matching, dependency resolution, naming, and the userland encodings run under `cargo test` | [ENGINEERING.md](ENGINEERING.md) |
| G9 | **Both ISAs from the first driver**: nothing in the model assumes x86 port I/O, TSO, or 4 KiB IOMMU pages | [ARCHITECTURE.md §3](ARCHITECTURE.md#3-the-architecture-seam-hal) |

### 1.2 Non-goals (Day 1)

- **A stable or binary driver ABI.** The kernel is static ([ADR-0005](adr/0005-monolithic-static-kernel.md)),
  and the internal API is free to change. If loadable modules come after v1.0, their interface is
  designed then (S§6.2: a frozen ABI is costly).
- **Drivers in user space or in isolated domains.** The data-path cost is far beyond 2% (S§13.4).
  §14 keeps a seam open for low-rate drivers later.
- **Driver restart after a panic.** A panic is a kernel bug and halts the system
  ([ENGINEERING.md §3.4](ENGINEERING.md#34-errors-are-values)). *Device* reset and rebind are in
  scope (§13.3).
- **Device tree (FDT) enumeration.** ACPI only ([ADR-0006](adr/0006-uefi-acpi-only-own-loader-and-acpi.md)).
  The bus layer would accept an FDT bus later without change.
- **Linux interfaces**: sysfs, uevents, modalias, devres, LinuxKPI ([VISION](VISION.md) goal 11).

## 2. Design principles

Each principle comes from a failure that a surveyed system documented.

1. **The framework owns the state machine.** Drivers supply callbacks. The framework decides when
   they run, and in what order. This is WDF's lesson (S§7.2).
2. **Every way into a driver goes through its binding.** Interrupt handlers, callouts, tasks, class
   calls, cdev operations, and child-bus calls are all created through the binding and registered
   with it. Detach drains a known, finite set. This answers FreeBSD's manual drain and the Q8B
   callout hang (S§2.3.2).
3. **Three lifetimes, three types.** `DeviceRef` (the node), `Binding` (attach to detach), and
   `Gate`-protected handles (user-visible objects) are different types. Memory that lives for the
   binding can never back a user-visible object. This is the devm lesson (S§5.4.1).
4. **Matching is data, ranked with FreeBSD's values.** Match tables are `const` data, checked for
   ambiguity at build time and tested on the host (S§13.3).
5. **Dependencies are explicit and named.** A device waits for *named* suppliers, never for a blind
   retry (S§5.4.2).
6. **Concurrency is declared.** Drivers are serialized by default and concurrent by opt-in, and
   nothing on the data path hops threads (S§6.2, S§8).
7. **Drivers get grants, not hardware.** A platform resource broker hands each driver typed handles
   to its own MMIO, interrupts, and DMA, and nothing else (S§9, S§10).
8. **Device memory is plain old data.** Drivers see device-written memory only through copy-in and
   copy-out of POD types, and they validate what they read (S§12).
9. **Classes own protocol logic.** Leaf drivers implement a narrow class trait. The storage, network,
   and tty classes own naming, queueing, and the user interface (S§6.2, S§10).
10. **No state spill.** A driver has no globals. All its state lives in the instance the framework
    holds, so detach and re-attach are exact (S§9, Theseus).

## 3. Overview

```
 userland     devinfo  devctl  devd  pciconf  sysctl dev.*   /dev/nvme0 /dev/nda0 …   ifconfig
──────────────────────┬──────────────────────────────────────────┬──────────────────────────────
 projections          │ hw.bus.* sysctls, dev.* tree,            │ devfs cdevs, GEOM providers,
 (§11)                │ /dev/devctl events, /dev/devctl2 ioctls  │ ifnets: all behind Gates
──────────────────────┴──────────────────────────────────────────┴──────────────────────────────
 classes (§9)         storage  net  tty/uart  console  input  │ platform providers:
                      (queue-pair class traits)               │ irq, timer, clock, power, gpio, i2c, pcc
──────────────────────────────────────────────────────────────┴───────────────────────────────
 device manager (§4–§6)   device tree · matcher · dependency graph · lifecycle state machine
                          · attach workers · topology lock
──────────────────────────────────────────────────────────────────────────────────────────────
 drivers (§7)         nvme  ahci  virtio-blk  xhci  rge  uart-16550  gic-its … (safe Rust)
──────────────────────────────────────────────────────────────────────────────────────────────
 buses (§5)           acpi ── pci ── virtio ── usb ── (platform: early devices)
──────────────────────────────────────────────────────────────────────────────────────────────
 platform broker (§8) resource spaces · irq domains · dma tags / iommu · power resources
                      (the only layer that maps MMIO, programs MSI, or touches page tables)
──────────────────────────────────────────────────────────────────────────────────────────────
 HAL / arch           mmu, interrupt entry, barriers, cache maintenance
```

### 3.1 Crates

| Crate | Contents | `unsafe` |
|---|---|---|
| `lib/devtree` (new) | Pure logic: match ranking, ID table types, dependency graph and topological order, unit allocation, pnpinfo and location formatting, `u_device`/`u_rman` encoding, devctl event grammar. Host-tested and fuzzed | forbidden |
| `sys/bus` | Device manager, node table, lifecycle state machine, `Binding`, `Gate`, contexts, attach workers, `Driver` and `Bus` traits, devctl/devctl2, `hw.bus.*` and `dev.*` export | small, audited (binding storage) |
| `sys/irq` (new, split out of the arch crates) | `IrqDomain` hierarchy, filter and ithread dispatch, MSI/MSI-X allocation, affinity | small (dispatch tables) |
| `sys/vm::dma` | `DmaTag`, backends (direct, bounce, IOMMU), `DmaCoherent`, `Mapped`, pools | yes (it is the DMA layer) |
| `sys/acpi`, `sys/pci`, `sys/virtio`, `sys/usb` | Bus drivers: enumeration, per-bus device views, child resources | `sys/pci`: config-space MMIO only |
| `sys/iommu` (P9) | VT-d, AMD-Vi, SMMUv3 backends for `vm::dma` and interrupt remapping | yes |
| `lib/regs` | Typed MMIO register blocks (extended with field-level definitions) | yes (already) |
| `drivers/*` | Leaf drivers | **forbidden** |
| `kernel` / platform config | The driver table for each build (§7.1) | — |

The layering rules of [ARCHITECTURE.md §2](ARCHITECTURE.md#layering-rules) hold. Drivers depend on
`sys/bus`, `sys/irq`, `sys/vm` (DMA only), `sys/sync`, `sys/sched` (locks), class crates, and
`lib/regs`. They never depend on an `arch/<isa>` crate.

## 4. Objects and lifetimes

### 4.1 The three lifetimes

| Object | Type | Lives | Holds | Who may hold it |
|---|---|---|---|---|
| **Device node** | `DeviceRef` (refcounted handle to a `Node`) | From discovery by a bus to deletion (`device_delete_child` / `DEV_DELETE` / surprise removal and last reference) | Name, unit, parent, bus-specific identity, firmware handle, resource list, state, flags, sysctl node | Anyone: the manager, the parent bus, devctl, open files (for naming) |
| **Binding** | `Binding<D>` (owned by the manager, never shared) | From the start of `attach` to the end of detach | The driver instance `D`, its granted resources, and its registry of async sources and publications | Only the manager. Drivers see `&D` inside callbacks |
| **Handle** | `Gate<T>`-protected objects: cdev `si_drv`, GEOM provider, ifnet, console, class objects | From publication until its last user lets go, possibly long after the binding | A `Gate` to the binding, plus class-owned state | Subsystems and open files |

The rules:

- **A binding never outlives its node.** A node can outlive any number of bindings: detach,
  re-attach, `DEV_SET_DRIVER`.
- **A handle may outlive the binding.** Once the binding goes, the handle's operations fail with
  `ENXIO` (FreeBSD's `destroy_dev` semantics). Class-owned state (a disk's name, an ifnet's
  counters) stays valid until the last reference.
- **Driver state cannot escape the binding.** Callbacks receive `&D` for the duration of the call.
  `D` is not `Clone`, and the framework never hands out an `Arc<D>`, so nothing outside the binding
  can keep it alive. A driver can still move an owned resource into something long-lived. That is
  memory-safe, because resources release on drop, but it is a leak. The binding ledger (§13.1)
  reports it at detach.

**Why not a `'bound` lifetime brand?** Rust for Linux is moving driver data to a `'bound` lifetime
that resources carry (S§5.2). That brand is needed there because C code owns the binding and can
free it under Rust. Here the manager owns `Binding<D>` and `D` owns its resources by value, so a
resource cannot outlive the binding unless the driver moves it out, and the ledger catches that.
Owned values give the same guarantee with less type machinery, and they keep driver signatures
simple. If a case arises that ownership cannot express, the brand can be added without changing
the lifecycle.

### 4.2 The device node

```rust
pub struct DeviceId(Generational<u32>);          // index + generation; stale ids fail lookup

pub enum NodeState {                              // exported as FreeBSD DS_* (§11.1)
    NotPresent,                                   // DS_NOTPRESENT: discovered, no driver chosen
    Waiting(WaitReason),                          // DS_NOTPRESENT: matched, suppliers not ready
    Attaching,                                    // DS_ATTACHING
    Attached(Power),                              // DS_ATTACHED (Power: D0 | Suspended(DState))
    Detaching(DetachReason),                      // DS_ATTACHED until the binding is gone
    Failed(AttachFailure),                        // DS_NOTPRESENT, and DF_* reflects disable policy
    Gone,                                         // surprise-removed; awaiting deletion
}

pub enum WaitReason {
    Supplier(DeviceId),                           // a known supplier node is not attached yet
    Unresolved(FirmwareRef),                      // a firmware reference no node provides (yet)
    Frozen,                                       // DEV_FREEZE in effect
}
```

`Node` also carries the FreeBSD-visible fields: devclass, unit, description, `DF_*` flags (stored
as a typed set, encoded to bits only at export), and the bus's typed per-child data (§5.2), which
replaces ivars.

### 4.3 Names and units

- **The devclass is the driver's name** (`nvme`, `ahci`, `vtblk`, `pci`, `acpi`, `uart`), as in
  FreeBSD. The FreeBSD driver name is the compatibility contract, so we use FreeBSD's names
  wherever FreeBSD has a driver for the device. A driver for hardware FreeBSD has no driver for
  needs a name recorded in `docs/compat-deviations.md`.
- **Units are allocated deterministically.** Discovery and matching run in one thread in bus
  enumeration order (ACPI namespace order, PCI depth-first by bus, slot, function), and the unit is
  chosen when the driver is chosen, before attach. Attaches then run in parallel (§6.4), but units
  never depend on attach timing. Differential tests against FreeBSD on the same VM configuration
  check that the names match (`nvme0`, `nvme1`, `vtblk0`, …).
- **Hints**: `hint.<driver>.<unit>.disabled=1` from the loader environment is honored. Wiring hints
  (`at`, resource hints) are a legacy ISA mechanism and are not supported.
- Class objects have their own names, chosen by the class, as in FreeBSD: an `nvme0` controller
  publishes the disks `nda0` and `nda1`, and a `uart0` publishes `ttyu0`.

### 4.4 Topology lock

FreeBSD's topology lock is still Giant (S§2.3.1). Ours is a real lock with a narrow job.

- A single `SxLock` at a declared witness level protects the tree's **structure**: adding and
  removing nodes, parent/child links, and the devclass and unit tables. It is held only for
  structural edits and for consistent snapshots (`hw.bus.devices` iteration takes it shared).
- **It is never held across a driver callback.** The node's state is the exclusion mechanism: a
  node in `Attaching` or `Detaching` is owned by the worker running that transition, and any
  conflicting operation (devctl detach, rescan of its parent, set-driver) fails with `EBUSY` or
  waits on the node's transition `WaitQueue`, depending on the operation's FreeBSD semantics.
- Lock order, checked by the witness: topology → node state → binding serializer (§6.5) → driver
  locks. A driver callback can create children (structural, so it takes the topology lock
  briefly) but cannot hold it. That rules out the probe-under-bus-lock deadlock seen on the Q8B
  (S§13.2 class 8).

## 5. Buses

A **bus** is a driver whose binding also implements `Bus`, so it can enumerate children and serve
their requests. ACPI, PCI, virtio-pci, and USB hubs are buses. FreeBSD's root `nexus` is replaced
by the platform root (§5.4).

### 5.1 The `Bus` trait

```rust
pub trait Bus: Send + Sync + 'static {
    /// Typed identity of a child: what drivers match on.
    type Ident: Sync;                             // PciIdent, AcpiIdent, VirtioIdent, UsbIdent
    /// The view of a child that a driver gets during attach and probe.
    type Dev<'a>;                                 // PciFunction<'a>, AcpiDevice<'a>, …
    /// One match-table entry for drivers on this bus.
    type Id: Matches<Self::Ident> + 'static;      // PciId, AcpiId, …

    /// Enumerates children (on attach, and again on DEV_RESCAN).
    fn enumerate(&self, cx: &mut EnumerateCx<'_, Self>) -> Result<(), BusError>;

    /// Resources a child wants, from BARs, _CRS, virtio config, or USB descriptors.
    fn child_resources(&self, child: &ChildInfo<Self>) -> ResourceList;

    /// Grants a child resource (the broker does the mapping; the bus checks ownership).
    fn grant(&self, child: &ChildInfo<Self>, req: ResourceRequest) -> Result<Grant, ResourceError>;

    /// FreeBSD pnpinfo and location strings (byte-exact; §11.2).
    fn pnpinfo(&self, child: &ChildInfo<Self>, out: &mut impl fmt::Write) -> fmt::Result;
    fn location(&self, child: &ChildInfo<Self>, out: &mut impl fmt::Write) -> fmt::Result;

    /// Firmware references a child depends on (§6.1).
    fn dependencies(&self, child: &ChildInfo<Self>, out: &mut DepSink);

    /// Bus-level reset of one child, if the bus supports it (FLR, hub port reset).
    fn reset_child(&self, child: &ChildInfo<Self>, kind: ResetKind) -> Result<(), BusError> {
        Err(BusError::Unsupported)
    }
}
```

The traits are used through static dispatch inside each bus's code, and through `&dyn` object-safe
facades where the manager handles buses generically. None of this is on a hot path: buses are
involved in enumeration, attach, and detach, not in I/O.

### 5.2 Typed child views instead of ivars

Each bus defines the view a driver sees. For PCI:

```rust
pub struct PciFunction<'a> { /* node, config-space accessor, capability list, BAR table */ }

impl PciFunction<'_> {
    pub fn ident(&self) -> &PciIdent;                 // vendor, device, subsystem, class, revision
    pub fn config(&self) -> ConfigSpace<'_>;          // typed reads; writes limited to driver-owned fields
    pub fn capability<C: Capability>(&self) -> Option<C::View<'_>>;   // MSI-X, PCIe, PM, …
    pub fn bar(&self, n: BarIndex) -> Option<BarInfo>;               // size, kind, prefetchable
}
```

ACPI devices expose `_HID`/`_CID`/`_UID`, `_ADR`, `_DSD` properties as typed values
(`DeviceProperty`, mirroring FreeBSD's `device_get_property()` kinds), `_DSM` evaluation, and the
AML handle for notify registration. These views replace FreeBSD's `BUS_READ_IVAR` (S§2.3.5).

A PCI function that firmware also describes in the namespace has an **ACPI companion**, as in
FreeBSD's `acpi_pci`: `PciFunction::acpi_companion() -> Option<AcpiDevice<'_>>`. Through it a PCI
driver reads `_DSD` (USB4 `IMR_VALID` and `WAKE_SUPPORTED`), evaluates `_DSM` (retimers), and the
framework finds `_PRW` and `_S0W`. The companion's dependency edges (§6.1) apply to the PCI node.

### 5.3 Multi-function devices

- **Transport buses.** virtio-pci is a bus: it attaches to the PCI function and enumerates a virtio
  child (`vtblk0` on `virtio_pci0`), as FreeBSD does. The same holds for a USB hub.
- **Function children.** A driver that splits one device into parts (a GPU's audio function, a
  NIC's PTP clock) registers a small child bus with its own typed child view. This is FreeBSD's
  `device_add_child` pattern, with typed parent interfaces. It covers the case Linux's auxiliary
  bus does (S§5.1).
- **Aggregates.** A device that needs several others (a display pipeline, an ACPI device that needs
  an I2C controller, a GPIO controller, and a power resource) is not a special mechanism. It is a
  node with several suppliers in the dependency graph (§6). It attaches when all of them have.
  Some aggregates have no firmware node of their own (a display card built from a DPU, an
  encoder, a PHY, and a panel). The platform description (§5.5) creates a synthetic node for them,
  whose suppliers are the members. The members publish narrow hardware traits to it through
  gates, and the aggregate owns the class publication. The design of those traits is M-G work.
- **Several PCI functions, one chip.** When one function's BAR holds registers its siblings need
  (the TC956x's chip-wide clock and reset block is decoded only by function 0), function 0
  publishes a provider (§9.3) wrapping those registers, and the sibling depends on it with
  `Need::Device`. The functions form a **reset group**: a reset of one detaches every function
  in the group first (§13.3).

### 5.4 The platform root and early devices

Some devices must work before the tree exists: the early console (P1), interrupt controllers and
timers (P2), and the watchdog (P3, as early as possible). The kernel initializes them directly in
the order [ARCHITECTURE.md §4](ARCHITECTURE.md#4-boot-flow) gives.

- Each early device is a value of its driver type in an `Early<D>` wrapper, created by init code
  from ACPI static tables (SPCR/DBG2, MADT, GTDT, WDRT).
- When the tree is built, the **platform root** creates nodes for them under `acpi0` (or under the
  root, for devices that have no namespace object) and **adopts** them. Each `Early<D>` becomes a
  `Binding<D>` in the `Attached` state, without running `attach` again. The adopted driver then
  receives its sysctl node, publications, and normal lifecycle.
- The ACPI bus recognizes the namespace objects for adopted devices (a `PNP0501` matching the SPCR
  address) and attaches no second driver to them.
- An adopted device is never detached by `devctl` (`EBUSY`), as a kernel console or interrupt
  controller cannot go away.

This replaces FreeBSD's ROOT, CPU, RESOURCE, INTERRUPT, TIMER, and SCHEDULER bus passes: everything
those passes order is either initialized before the tree, or ordered by the graph.

### 5.5 Platform description and overlays

ACPI tables on arm64 SoCs are often incomplete or wrong, and some vendors describe clocks, resets,
and power outside the standard objects. Rather than spreading fixes through drivers, each
supported platform may have a **platform description**: `const` data in `lib/devtree`, selected by
the firmware's identity (FADT/MCFG OEM ID, a namespace object such as Qualcomm's `\_SB.SOID` or
CIX's `CIXA1019`), host-tested against the captured tables. It is FreeBSD's `acpi_quirks`, made
data and widened. It may:

| Entry | Example |
|---|---|
| **Fix a resource**: add, correct, split, or suppress a `_CRS` entry, keyed by `_HID`/`_UID` | Q8B: GMU interrupts missing from `\_SB.GPU0`; `GPU0` resource 3 encloses resource 6; SPCR's wrong GSI |
| **Override an attribute** | Sky1: `_CCA = 1` on the GPU (`CIXH5000`), which must be treated as non-coherent |
| **Split a node into function children** (§5.3) | Q8B: `\_SB.GPU0` covers the GPU, the display subsystem, and the video codec, after Windows' single-miniport model |
| **Create a synthetic node** for a block with no namespace object | Q8B clock controllers (GPUCC, DISPCC, VIDEOCC), cmd-db, SMEM; aggregates (§5.3) |
| **Add dependency edges** (§6.1) and provider bindings (§9.3) | Q8B: which power domain, clocks, and interconnect paths each `PEP0` consumer needs |
| **Describe reserved memory** (§8.4) | Carveouts, firmware shared memory |

An extractor for vendor tables that live in AML (CIX's `CLKT`, `RSTL`, `RSNL`, `DLKL`, `RSVL`)
also belongs here: it evaluates them at graph build and produces the same typed entries.

Rules:

- The description changes what firmware *says*, never driver behavior. Driver quirks stay on match
  entries (§7.2).
- Every entry names the firmware defect or omission it addresses, and the board corpus test
  fails if an entry no longer applies.
- This is not a board configuration knob. A platform description is selected by firmware identity,
  needs no user action, and is part of the kernel ("nothing special").

## 6. Dependencies and ordering

### 6.1 The dependency graph

The manager keeps a directed graph of **supplier → consumer** edges between nodes. Edges come from
firmware, before any driver runs:

| Source | Edge |
|---|---|
| Tree | parent → child (always) |
| ACPI `_DEP` | listed object → device |
| ACPI `_CRS` resource sources | GPIO controller (`GpioIo`/`GpioInt`), I2C/SPI/UART controller (`*SerialBus`), interrupt combiner (extended IRQ with a source) → device |
| ACPI power resources (`_PR0`, `_PR3`) | power resource provider → device |
| PCCT / PCC operation regions | PCC mailbox → device whose AML uses that channel |
| IORT (P9) | SMMU → each device it translates. ITS group → each device whose MSIs it receives |
| DMAR / IVRS (P9) | IOMMU unit → each device in its scope |
| MADT / GSI routing | interrupt controller → each device using its GSIs (mostly satisfied by adoption) |
| `_PRT` link devices | PCI interrupt link → device |
| `_CRS` `FixedDMA` | DMA engine → device (with `_DSD` `dma-names` for the channel's role) |
| `_CRS` `PinFunction`, `PinConfig`, `PinGroup*` | Pin controller → device |
| `GenericSerialBus` and `GeneralPurposeIo` operation regions | I2C/SPI controller or GPIO controller → device whose AML uses the region (a `_PS0` that talks to a PMIC) |
| `_DSD` reference properties, from a table of known names (`mboxes`, `shmem`, `power-domains`, `phys`, `dp_phy`, `usb4-host-interface`, `phy-handle`, `backlight`, …) | Referenced object → device, with the kind the table gives |
| `_DSD` graph ports and endpoints | Upstream → downstream in a media pipeline (a DPU → its DP transmitter) |
| Platform description (§5.5) | Whatever it declares: vendor clock and reset tables, Qualcomm `PEP0` consumers, synthetic nodes |
| Driver-declared (rare) | `cx.require(Need::…)` during attach, naming a firmware reference |

Edges carry a kind:

| Kind | Attach | Shutdown, suspend, resume, runtime PM | Example |
|---|---|---|---|
| **Required** | The consumer waits for the supplier | Ordered | `_DEP`, `GpioInt` |
| **Optional** | The consumer attaches without it, and is told when it appears | Ordered once present | An I2S port the audio card can do without |
| **Order** | Not gated | Ordered: the supplier suspends after and resumes before the consumer | A USB4 port's `usb4-host-interface` reference to its NHI |
| **Power** | The consumer waits | Ordered, and the supplier stays powered while the consumer is | A DP transmitter in the display controller's power domain |

Keeping **Order** and **Power** apart from **Required** matters for cycles: a DP transmitter needs
the DPU's power domain while the DPU uses the transmitter as its encoder. Those are a Power edge one
way and a pipeline edge the other, and only the second is a candidate to break (§6.2).

A supplier is not always a device. An ACPI object with `_STA = 0` that exists only to be
referenced (the SCMI protocol nodes on Sky1), or a data-only child (a PHY's per-port `UDPP`
child), resolves to a **sub-handle** of the provider that owns it, `ProviderRef { node, sub }`,
rather than to a node that would never get a driver and would emit a `?` event.

An unresolved reference (a
`_DEP` naming an object no node provides) is recorded as `WaitReason::Unresolved`. For a
required edge, that is a diagnosable state, never a hang: the Sky1 DSDT's reference to a
nonexistent `I2C0.UXC0` becomes a logged, typed error ([HARDWARE.md §4](HARDWARE.md)).

The graph logic (insertion, cycle detection, topological order, and the "what is blocking whom"
report) lives in `lib/devtree` and is tested on the host against the ACPI corpus.

### 6.2 Attach order

1. The manager matches a node as soon as its bus creates it (§7.2) and allocates the unit.
2. If every required supplier is `Attached`, the node is **ready** and goes to the attach workers.
   Otherwise it enters `Waiting(Supplier(id))`.
3. When a node attaches, the manager re-examines **only its consumers**. There is no global retry
   (S§5.4.2).
4. A driver that finds during attach that it needs something not described by firmware returns
   `AttachError::Wait(Need)`. `Need` names the reference, so the same targeted wakeup applies. A
   `Wait` without a resolvable name is not representable:
   - `Need::AcpiPath(path)`: the node with that ACPI path, woken when it attaches;
   - `Need::Device(id)`: a node the driver already knows, such as a PCI sibling (§5.3);
   - `Need::Image(name)`: a firmware image not yet available, woken when root is mounted (§8.6).
5. At the end of boot (the point where FreeBSD runs `config_intrhook` and mounts root), the manager
   logs every node still in `Waiting`, with its chain of missing suppliers, and exports it under
   `kern.abyss.bus.waiting` ([COMPATIBILITY.md](COMPATIBILITY.md) puts extensions under
   `kern.abyss.*`).

**Cycles.** Firmware cycles are reported at graph build time. They are broken by dropping
optional and order edges first, then non-tree edges whose consumer is the cycle's ancestor in the
tree. Tree edges and power edges are never dropped. A cycle that cannot be broken leaves its nodes in `Failed`, with
the cycle in the log.

### 6.3 Shutdown, suspend, and resume order

The same graph orders every system transition:

- **Shutdown, reboot, kexec, poweroff**: consumers before suppliers (reverse topological order).
  Each binding's `quiesce` stops DMA and masks the device's interrupts before firmware takes over
  (ACPI S5 or PSCI `SYSTEM_OFF`). This matters for warm reset, where devices left running can DMA
  into the next boot's memory.
- **Suspend** (stretch, P9): reverse topological order. **Resume**: topological order.
- Independent subtrees run their transitions in parallel, as attach does.

Ordering by the graph rather than by sibling order (S§2.3.4) means an IOMMU, a power resource, or
a GPIO controller always stays up until its last consumer is down.

### 6.4 Parallel attach

Attach runs on a small pool of manager worker threads at kernel priority. Independent ready
nodes attach concurrently. Boot time is a P6 benchmark ([PERFORMANCE.md §5](PERFORMANCE.md)), and
NVMe or xHCI controller resets take tens to hundreds of milliseconds. Determinism is kept where
userland can see it: names and units are fixed before attach (§4.3), and devctl `+` events are
emitted in the order attaches complete, as in FreeBSD.

A boot option `abyss.bus.serial=1` forces one worker, for debugging.

### 6.5 Execution model

Each driver declares how it may be entered:

```rust
pub enum Concurrency {
    /// The default. The framework holds the binding's serializer (a Mutex) around every
    /// thread-context entry point: start, stop, callouts, tasks, ithreads, and class calls.
    /// Filters never take it. Like IOKit's work loop, WDF's device sync scope, or Fuchsia's
    /// synchronized dispatcher.
    Serialized,
    /// The driver is entered concurrently from any thread. Its state must be Sync and use its own
    /// locks or per-queue ownership. Required for multi-queue storage and network drivers.
    Concurrent,
}
```

- **Nothing on the data path hops threads.** A class call (submit an I/O, transmit a packet) runs
  the driver's code in the calling thread. A completion runs in the filter or ithread that
  observed it, and the class's completion callback runs there too. There are no per-driver threads
  and no queues between the class and the driver (S§8: +50–150% CPU).
- **Re-entry.** A serialized driver that is called back while its serializer is held, for example
  when a completion callback calls submit, would deadlock. Classes therefore never call into a
  driver from inside a completion callback that driver made. A completion that wants to resubmit
  pushes the work onto the class's per-CPU deferred list, which runs after the callback returns.
  The witness catches violations (recursive acquisition of a binding serializer).

### 6.6 Contexts

Every callback receives a context token. Tokens are zero-sized or small, are not `Send`, and cannot
be built by drivers:

| Token | Where | Allows | Forbids |
|---|---|---|---|
| `IrqCx` | Interrupt filters | Spin locks, MMIO, completing to classes, scheduling ithreads and tasks | Sleeping, sleepable locks, allocation that may sleep |
| `ThreadCx` | ithreads, callouts, tasks, class calls in thread context | Everything in `IrqCx`, plus `Mutex` and `RwLock`, and `Sleepable` for bounded sleeps | — |
| `AttachCx<'_, B>` | `attach` | Resource grants (§8), dependency declarations, sleeping (device resets) | Publishing (no instance yet) |
| `StartCx<'_, D>` | `start` | Installing handlers, creating callouts and tasks, publishing, adding children, sysctls | — |
| `StopCx<'_>` | `stop` / `quiesce` | MMIO, bounded sleeps | Creating new async sources |

The sleepable locks in `sys/sched` take a `&Sleepable` proof, which only `ThreadCx` and the attach,
start, and stop contexts can produce. This is the context typing planned in
[ARCHITECTURE.md §5.1](ARCHITECTURE.md#51-synchronization-syssync), applied to drivers. A
filter that blocks is then a type error, not a Driver Verifier finding.

## 7. Drivers

### 7.1 Registration

There is no linker-section magic and no global constructor. Each driver crate exports a `const`
declaration, and the platform configuration lists the drivers a build includes, behind Cargo
features ([ADR-0005](adr/0005-monolithic-static-kernel.md)):

```rust
// drivers/nvme/src/lib.rs
pub const DRIVER: DriverDecl = DriverDecl::pci::<Nvme>();

// kernel/src/platform/drivers.rs (generated by xtask from Cargo features, or written by hand)
pub static DRIVERS: &[DriverDecl] = &[
    acpi::DRIVER, pci::DRIVER, pci::HOST_ECAM, virtio::PCI_TRANSPORT,
    #[cfg(feature = "nvme")] nvme::DRIVER,
    #[cfg(feature = "ahci")] ahci::DRIVER,
    // …
];
```

The table is ordinary, reviewable code. Its order is the tie-break of last resort (§7.2), and the
table's contents are exactly what `kldstat` and `modfind` report as built in (ADR-0005).

### 7.2 Matching

```rust
pub enum Rank {                         // FreeBSD BUS_PROBE_* values, exported unchanged
    Specific    = 0,                    // BUS_PROBE_SPECIFIC: only this driver can drive it
    Vendor      = -10,                  // BUS_PROBE_VENDOR: vendor-supplied driver
    Default     = -20,                  // BUS_PROBE_DEFAULT
    LowPriority = -40,                  // BUS_PROBE_LOW_PRIORITY
    Generic     = -100,                 // BUS_PROBE_GENERIC: class-code drivers
    Hoover      = -1_000_000,           // BUS_PROBE_HOOVER: catches everything left over
}

pub const NVME_IDS: &[PciId] = &[
    PciId::class(0x01_08_02).rank(Rank::Generic),                   // any NVMe controller
    PciId::device(0x144d, 0xa80a).rank(Rank::Default).quirks(Q_SAMSUNG_980),
];
```

1. **Tables are data.** Each driver's `IDS` is a `const` slice of the bus's `Id` type. The manager
   evaluates every table against a new node's `Ident` without running driver code. The best rank
   wins. Matches are host-tested like Fuchsia's bind rules: `lib/devtree` tests check that each
   corpus device (QEMU, bhyve, and the captured reference-machine `pciconf`/`acpidump` data) gets
   the expected driver.
2. **Ambiguity is a build error.** `cargo xtask check` evaluates all tables in a build's driver set
   and rejects two drivers matching some identity at equal rank, unless one declares it yields to
   the other. FreeBSD resolves such ties by registration order, silently.
3. **Optional refinement.** A driver may provide `fn refine(dev: &B::Dev<'_>, id: &B::Id) ->
   Refine`, which can only read (config space, `_DSD`, `_STA`) and returns `Accept`, `Decline`, or
   `Rank(r)`. It runs once, for the winning candidates only, and cannot allocate resources. This
   covers the probes in FreeBSD that check a register or revision. FreeBSD's double probe call
   (S§2.3.6) does not exist.
4. **Quirks are data on the match entry**, not code paths keyed on IDs inside the driver.
5. **No match** emits the FreeBSD `?` nomatch event (§11.3), so devd rules and `devmatch(8)` keep
   working. devmatch finds nothing to load, because everything is built in.
6. **`DEV_SET_DRIVER`** (devctl `set driver`) pins a node to a named driver, skipping ranking but not
   `refine`. `DEV_CLEAR_DRIVER` removes the pin. Both match FreeBSD's semantics, including the
   `*_DETACH` flags.

### 7.3 The driver traits

Each bus has a driver trait with the same shape. This is the PCI one:

```rust
pub trait PciDriver: Sized + Send + Sync + 'static {
    /// The devclass: "nvme".
    const NAME: &'static str;
    const IDS: &'static [PciId];
    const CONCURRENCY: Concurrency = Concurrency::Serialized;
    /// Shown as %desc.
    fn describe(dev: &PciFunction<'_>, id: &PciId) -> Description;

    fn refine(_dev: &PciFunction<'_>, _id: &PciId) -> Refine { Refine::Accept }

    /// Phase 1: claim resources, reset and identify the hardware, build the driver instance.
    /// Nothing outside the driver can call it yet. On error, everything claimed is dropped.
    fn attach(cx: &mut AttachCx<'_, Pci>) -> Result<Self, AttachError>;

    /// Phase 2: the instance has its final address. Install interrupt handlers, start
    /// callouts and tasks, publish to classes, add children, create sysctls.
    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError>;

    /// Stop the hardware: no more DMA, interrupts masked at the device. Called by detach,
    /// shutdown, and reset. Must not fail; it is told why (DetachReason) and how long it has.
    fn stop(&self, cx: &mut StopCx<'_>, why: StopReason);

    /// Asked first on a voluntary detach (devctl detach without -f). Gone, shutdown, and
    /// forced detach do not ask.
    fn may_detach(&self) -> Result<(), Busy> { Ok(()) }

    fn suspend(&self, _cx: &mut StopCx<'_>, _to: DState) -> Result<(), PmError> { Ok(()) }
    fn resume(&self, _cx: &mut ThreadCx<'_>) -> Result<(), PmError> { Ok(()) }
}
```

`attach` corresponds to WDF's PrepareHardware, `start` to D0Entry plus self-managed I/O, and `stop`
to D0Exit. They are split into two phases because handlers and publications need `&Self` at a
stable address, and `Self` does not exist until `attach` returns. The framework stores `Self` in
its binding (pinned in a kernel box) between the phases.

There is no `detach` callback. Detach is `stop`, followed by the framework's teardown, followed by
`Drop` (§10.2).

```rust
pub enum AttachError {
    Decline,                    // not mine after all: try the next-ranked driver
    Wait(Need),                 // a named supplier is missing (§6.2)
    Device(DeviceError),        // hardware failed to reset or identify: node -> Failed
    Resource(ResourceError),    // grant refused or exhausted
    NoMemory,                   // allocation failed (allocation is fallible: ENGINEERING §3.6)
}
```

### 7.4 An example

```rust
pub struct Nvme {
    regs: Mmio<NvmeRegs>,                 // BAR 0, typed register block
    admin: Mutex<AdminQueue>,
    io: KBox<[IoQueue]>,                  // one per vector, each owned by its vector
    vectors: MsixVectors,
}

impl PciDriver for Nvme {
    const NAME: &'static str = "nvme";
    const IDS: &'static [PciId] = NVME_IDS;
    const CONCURRENCY: Concurrency = Concurrency::Concurrent;

    fn describe(_: &PciFunction<'_>, _: &PciId) -> Description { Description::lit("Generic NVMe Device") }

    fn attach(cx: &mut AttachCx<'_, Pci>) -> Result<Self, AttachError> {
        cx.enable_bus_master();
        let regs = cx.mmio::<NvmeRegs>(BarIndex(0))?;
        let ctrl = Controller::reset(&regs, cx.sleepable())?;   // typestate: Reset -> Ready
        let dma = cx.dma_tag(DmaConstraints::nvme(&ctrl))?;
        let vectors = cx.msix(cx.cpus().count().min(ctrl.max_queues()))?;
        let admin = AdminQueue::new(&dma, &ctrl)?;
        let io = IoQueue::create_all(&dma, &ctrl, &admin, &vectors)?;
        Ok(Nvme { regs, admin: Mutex::new(&NVME_ADMIN, admin), io, vectors })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        for (i, v) in self.vectors.iter().enumerate() {
            cx.interrupt(v).filter(Self::intr, i).cpu(v.cpu()).install()?;
        }
        for ns in self.admin.lock(cx.sleepable()).identify_namespaces()? {
            cx.publish(storage::Disk::<Self>::new(ns, &self.io))?;  // becomes nda0, nda1 …
        }
        cx.cdev(Nvme::ctrl_ops(), format_args!("nvme{}", cx.unit()))?;   // /dev/nvme0
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _why: StopReason) {
        Controller::shutdown(&self.regs, cx);              // CC.SHN, bounded wait
    }
}

impl Nvme {
    fn intr(&self, queue: usize, cx: &IrqCx) -> Filter {
        self.io[queue].reap(cx)                            // completes bios to the storage class
    }
}
```

Notes:

- Every `?` in `attach` unwinds by dropping what was claimed so far. There is no `goto fail` and no
  devres: ownership does it (S§7.2: WDF's object hierarchy, made free).
- The handler is a `fn(&Self, usize, &IrqCx)`, not a closure. It cannot capture state, so it cannot
  smuggle anything past detach.
- `storage::Disk::<Self>` is the class's publication type. The class calls `Nvme` through the
  binding's gate (§9.2).

## 8. The platform resource broker

All hardware access a driver gets comes from **grants**: typed, owned handles that the broker
creates and the binding's ledger records. Drivers never see a physical address and never build an
`Mmio` themselves (`Mmio::new` is `unsafe` and used only by the broker).

### 8.1 Resource spaces

Each bus that hands out ranges keeps a **resource space** (the `rman` equivalent):

| Kind | Examples | Notes |
|---|---|---|
| `Memory` | PCI BARs, ACPI `_CRS` memory ranges, host-bridge windows | Mapped Device-nGnRE (arm64) or UC (x86). Prefetchable BARs may request Normal-NC / WC |
| `IoPort` | Legacy PCI I/O BARs, 16550 at `0x3f8` | **x86_64 only**, gated by `cfg`. The `RegisterIo` trait in `lib/regs` lets one driver serve both forms |
| `BusNumber` | PCI bus ranges | Validated, not reassigned on Day 1 (firmware assignment is kept) |
| `Irq` | GSIs, SPIs, MSI vectors (§8.3) | Through IRQ domains |
| `DmaWindow` | IOMMU address space (P9) | Through DMA tags (§8.4) |

- **Grants are vetted.** A driver can only claim resources in its own node's resource list. The
  broker additionally refuses **sensitive ranges** to leaf drivers: interrupt controller, IOMMU, and
  ECAM registers, and the kernel's own consoles (Asterinas, S§9). A request outside these rules is a
  `ResourceError`, logged with the driver's name.
- **Firmware-reserved ranges** (`PNP0C01`/`PNP0C02` system resources, the UEFI memory map's MMIO
  entries) are claimed at boot by the platform root, as FreeBSD's `sysres` does.
- **Ranges AML also touches.** The ACPI core records every `SystemMemory` and `SystemIO`
  operation region in the resource spaces as *firmware-shared*. A grant that overlaps one
  succeeds as `Grant::Shared { with: Aml }`, and the driver must treat its accesses as racing with
  firmware: it declines, or restricts itself to reads, unless the platform description (§5.5)
  says otherwise. A Super I/O whose environment-controller ports a `SystemIO` region covers is
  the common case.
- **Ranges two nodes share.** Firmware sometimes gives two devices overlapping ranges on purpose
  (Sky1's SCMI shared memory inside its mailbox's 64 KiB window). Overlaps are refused unless the
  platform description declares them, and then each side gets a `Grant::Shared { with: node }`.
  A register block several drivers program (a "syscon": Sky1's audio CRU, the reset
  controller's PHY orientation bits) is never granted to them directly. Its owner publishes a
  `RegisterRegion` provider (§9.3) that leases field subsets and does the read-modify-write under
  its own lock.
- **Splitting a claim.** `Mmio<R>::split` turns one granted window into typed sub-blocks, for a
  window that holds several register blocks (a GPU window that also holds its management unit).
- The resource spaces are exported as `hw.bus.rman` for `devinfo -r` and `-u` (§11.1).

### 8.2 Typed register blocks

`lib/regs` grows from byte-register access to typed register blocks: a declarative macro defines a
block's registers (offset, width, access mode) and fields (bit ranges and enums). `Mmio<R>` then
gives `regs.read(R::CSTS).field(Csts::RDY)` and similar, with access width and offset fixed by the
type. Writes to read-only registers do not compile. Barriers and ordering (`Mmio` accesses are
device-ordered; explicit `wmb`-style fences for DMA hand-off) come from the HAL. Drivers do not
write inline assembly.

PCI configuration space uses the same mechanism. Fields the bus owns (BARs, the command register's
memory and I/O enables, bus numbers, the MSI capability) can be written only through bus APIs.

### 8.3 Interrupts

Interrupts form a hierarchy of **IRQ domains**, as in FreeBSD's INTRNG and Linux's irq_domain
(S§2.1, S§5.1). The same model serves both ISAs:

```
 amd64:  device MSI-X entry → PCI MSI domain → (remapping, P9) → LAPIC vector domain → CPU
         ACPI GSI           → I/O APIC domain                    → LAPIC vector domain → CPU
 arm64:  device MSI-X entry → PCI MSI domain → ITS domain (DeviceID/EventID → LPI) → GIC redist → CPU
         ACPI GSI (SPI)     → GIC distributor domain                                 → CPU
 both:   GPIO interrupt     → GPIO controller domain (threaded; may need bus I/O to ack)
         ACPI GED / GPE     → ACPI event domain (threaded)
```

- **Allocation** returns owned `IrqLine` or `MsixVectors` grants. A domain allocates its own level
  (a vector, an LPI, an ITS translation entry) and asks its parent for the rest.
- **Handlers** are installed in `start`, through the binding:
  - `filter(fn(&D, usize, &IrqCx) -> Filter)` runs in interrupt context and returns `Stray`,
    `Handled`, or `Schedule`, the FreeBSD filter results.
  - `thread(fn(&D, usize, &mut ThreadCx))` runs in the vector's ithread at interrupt priority. A
    driver may provide either or both.
  - Interrupts from **threaded-only domains** (GPIO behind I2C, GED) accept only `thread` handlers.
    This is WDF's passive-level interrupt (S§7.1), and it will be common on ACPI arm64 machines.
- **Dispatch is direct.** A vector's handler slot holds the binding pointer, the function, and
  the cookie. Dispatch is one indirect call, with no lookup, lock, or allocation. Removing a handler
  clears the slot and then waits for any CPU still running it (an epoch on the dispatch path),
  so teardown is synchronous.
- **Affinity**: MSI-X vectors are placed on CPUs by the topology, one queue per CPU or per core
  cluster, preferring higher-capacity cores on asymmetric parts.
  `cpuset -x` (`cpuset_setaffinity` on `CPU_WHICH_IRQ`) is supported from P6.
- **Storm detection**: a line that fires faster than a threshold with `Stray` results is masked and
  logged, as FreeBSD's `hw.intr_storm_threshold` does.
- **Driver-published domains.** A leaf driver whose device aggregates interrupts for its function
  children (a display subsystem's interrupt status register, a MAC's MSI generator carrying a
  PHY's interrupt, Qualcomm's IPCC) publishes a child domain with `StartCx::irq_domain(...)`. It
  supplies safe mask, unmask, and status callbacks, and its children allocate from it as from any
  domain. The domain is part of the binding and is torn down after its children have detached.
- **Wake.** An interrupt firmware marks as a wake source (`GpioInt` with `…AndWake`, `_PRW`)
  carries that flag on its `IrqLine`. `IrqLine::set_wake(bool)` arms it for suspend, through the
  owning domain (on Sky1, a firmware call; §8.7). This is used from P9.

### 8.4 DMA

DMA follows FreeBSD's busdma model (constraint-inheriting tags with a pluggable backend), with
ownership made explicit in the types.

```rust
pub struct DmaConstraints {
    pub addr_bits: u8,          // 32, 48, 64 …
    pub window: Option<DmaWindow>, // bus address = CPU address - cpu_base + bus_base, within len
    pub align: Align,
    pub boundary: Option<Boundary>,
    pub max_segment: ByteCount,
    pub max_segments: u16,
    pub coherent: Coherency,    // from _CCA / IORT, the platform description (§5.5), or the arch default
}

impl AttachCx<'_, B> {
    /// Inherits the parent bus's restrictions (bridge windows, the IOMMU domain) and adds these.
    pub fn dma_tag(&mut self, c: DmaConstraints) -> Result<DmaTag, ResourceError>;
}
```

**Backends**, chosen per device by the broker:

| Backend | When | Map cost |
|---|---|---|
| `Direct` | No IOMMU and the device reaches all memory | Address arithmetic only |
| `Bounce` | The device cannot reach some memory (`addr_bits` below the top of RAM) | Copy, for out-of-range pages only |
| `Iommu` (P9) | VT-d, AMD-Vi, SMMUv3. One DMA domain per device, or per isolation group | Page-table updates. IOTLB invalidation is batched and deferred |

**Types.**

- `DmaCoherent<T: DeviceShared>` is for queues, rings, and descriptors that the CPU and device share
  for the binding's whole lifetime. It is allocated from the tag, mapped once, and accessed only
  through volatile reads and writes of whole `DeviceShared` fields. `DeviceShared` is implemented by
  a declarative macro for `#[repr(C)]` types made only of integers, arrays, and other
  `DeviceShared` types. They contain no references, no pointers, and no `bool` or `enum`s that a
  device could set to an invalid value.
- I/O buffers (bio pages, packet buffers) are mapped by the tag, as FreeBSD's
  `bus_dmamap_load` is:
  - `tag.map::<Dir, B>(buf) -> Result<Mapped<B, Dir>, (B, DmaError)>` does the PRE sync and yields
    the bus segments.
  - `Mapped::unmap(self) -> B` does the POST sync and gives the buffer back.

  The mapping refers to the tag's decisions rather than carrying the tag. A coherent,
  single-segment, unbounced mapping is just the buffer and one segment. Bounce pages, extra
  segments, and non-coherent syncs go in a boxed record. Moving a value larger than 64 bytes costs
  a `rep movs` in a kernel built without SSE, and this keeps the common path below that size.

  While the device owns a buffer, the CPU has no reference to it, because the value moved (the
  RedLeaf `RRef` idea, S§9). A forgotten sync cannot compile. On coherent devices the syncs are
  inline no-ops.
- **Pools.** `DmaPool` gives fixed-size, pre-mapped buffers (receive buffers, PRP lists, command
  buffers), so steady-state I/O does no mapping at all. This is the IOMMU lesson from Asterinas
  (S§9), and it also helps the direct backend on non-coherent arm64 devices.

**Rules for DMA-visible memory** (S§12, Thunderclap):

1. DMA-visible pages hold only device payload and `DeviceShared` structures. No kernel pointers,
   no list links, no class metadata. `Packet` and `Bio` metadata live outside the buffers they
   describe.
2. With an IOMMU, a mapping covers whole pages that hold nothing else. Small buffers that would
   share a page with other data are bounced.
3. Unmapping completes before the CPU reuses a buffer, including the IOTLB invalidation. Deferred
   invalidation batches across buffers, but never lets the CPU reuse a page the device can still
   reach.
4. Interrupt remapping is enabled together with DMA remapping (P9).

**Windows and offsets.** Some masters see memory at an offset, or only through a window: Sky1's
audio DMA sees DRAM at the CPU address minus `0x9000_0000` inside 2 GiB, and the TC956x adds
`0x10_0000_0000` to every bus address. `DmaConstraints::window` describes this, from `_DMA`,
`_DSD`, or the platform description, so drivers never do the arithmetic themselves.

**Reserved memory.** Memory that firmware sets aside for a device (UEFI-reserved carveouts listed by
the platform description, CIX's `RSVL`, Qualcomm's remote-processor regions, cmd-db and SMEM) is a
`ReservedRegion` grant with its memory attributes (Normal-NC, WC, or none). A DMA tag can be built
over it with `DmaTag::from_reserved(region)`, which allocates within it and still hides its physical
address. A region handed to a secure monitor for authentication moves to a state in which the CPU
mapping is revoked, because touching it afterwards faults (§8.7).

**Device-walked page tables and per-context address spaces** (GPUs, video codecs, NPUs, and
SMMUv2 context banks) are outside what `DmaCoherent` and `Mapped` express: the driver must write
bus addresses into memory the device walks. The plan is an opaque `BusAddr`, obtainable only from a
mapping and writable only into a page-table format type or an address-typed register field, and
an `IoPageTable<F: PteFormat>` whose table pages and cache maintenance the DMA layer owns. Buffers
mapped by the CPU and several devices at once (dma-buf, GEM objects) and fences belong with it.
These are designed with the IOMMU backends (P9) and the graphics milestone, in a separate
document, and are listed in §17.

**Untrusted device data.** Anything read from a `DmaCoherent` or a register is untrusted input:
completion queue entries, descriptor indices, lengths, and status words. Drivers bounds-check
before using any of it, and a value out of range is a `DeviceError` (logged, and possibly a reset),
never a panic or an out-of-bounds index. The PCIe surprise-removal pattern (all-ones reads) is a
case of this.

### 8.5 Power resources and clocks

- **The framework guarantees power while a driver runs.** Before `attach`, the framework brings up
  every power supplier the device has: its ACPI power resources (`_PR0`, through the ACPI
  power-resource provider), and any `PowerDomain` the platform description or a `_DSD` reference
  names (§6.1). Then it sets D0 (`_PS0`). They stay on while the binding exists, unless the
  device is suspended through the framework. No driver code runs against an unpowered device. On
  the Q8B, a read of a clock controller whose power domain was off reset the SoC (S§13.2 class 7).
- **Clocks and resets the driver sequences itself.** Many SoC blocks need a sequence the
  framework cannot know (power on, clocks on, hold reset 10–20 µs, release, then a vendor
  register), repeated on every resume. The driver gets handles in `attach`, resolved through its
  supplier edges:

  ```rust
  let core = cx.clock("core")?;            // Option-returning variants exist for optional ones
  let rst  = cx.reset("gpu")?;
  core.set_rate(Hz(800_000_000))?;          // sleeps: ThreadCx and attach/start/stop only
  core.enable()?;
  rst.pulse(Micros(20))?;
  ```

  Names come from firmware (`_DSD` `clock-names`, CIX's `CLKT`/`RSTL`) or the platform
  description. Handles are counted: a reset line shared by four GPIO controllers is asserted only
  when every holder asks, and `pulse` on a shared line is refused unless the provider declares the
  group safe to reset.
- **Firmware owns what it can.** Where firmware provides the control (ACPI power resources and
  `_PSx`, SCMI, RPMI on RISC-V), drivers use it, and no OS driver programs a clock tree firmware
  owns. Where firmware does not (Qualcomm's GDSCs, RPMh votes, and branch clocks behind `PEP0`),
  an OS provider implements the same traits, driven by the platform description.
- **Runtime power management** is needed from the first GPU, video codec, and NPU drivers, not only
  later: they idle in tens of milliseconds, and suspend is a firmware handshake the driver performs.
  A driver holds a `PowerRef` guard while the device must stay up (a class may hold one too), and
  supplies `runtime_suspend` and `runtime_resume`. The framework calls them when the last guard
  drops and turns the domain off only after `runtime_suspend` returns. Until a driver opts in, its
  device stays powered while bound (§14).

### 8.6 Firmware images

Devices that need an image (GPU microcontroller firmware, DSP and modem images, video codec
firmware, PHY patches) get it from a **firmware store**, FreeBSD's `firmware(9)` with the same
image names, which follow Linux's `linux-firmware` layout so the files can be shared:

```rust
let fw = cx.firmware(&format_args!("arm/mali/arch{}.{}/mali_csffw.bin", id.arch_major, id.arch_minor))?;
```

- **Sources**, in order: images built into the kernel (small patches, as FreeBSD's `rge` embeds
  its microcode), images the loader preloads from the ESP (`/boot/firmware/…`, FreeBSD's
  preloaded `firmware(9)` images), then the root file system once it is mounted.
- An image not yet available returns `AttachError::Wait(Need::Image(name))`. The manager wakes the
  node at mountroot and again whenever the store gains images, and logs the name of an image still
  missing at the end of boot (§6.2 step 5). `ThreadCx::firmware` serves lazy loads (a video codec
  loads one image per codec when a session opens).
- An `FwImage` is an immutable, counted byte slice. The driver may keep it for reloads after a
  hang reset. Its contents are untrusted input: parsers are host-tested and fuzzed.
- Names that depend on the board (Qualcomm's `qcom/sc8280xp/<vendor>/<model>/…`) are resolved
  from firmware (`_SUB`, `PSUB`) or the platform description, never from configuration.
- Signed images are not checked by the kernel. The device or the secure monitor authenticates them
  (§8.7).

### 8.7 Firmware calls

Some operations go to platform firmware through an SMC or HVC: SCMI over SMC (Sky1's power domains),
vendor SiP calls (Sky1's GPIO wake mask and display handover flag), and the Qualcomm secure
monitor (peripheral authentication, `INIT_IMAGE`, `AUTH_AND_RESET`). Drivers cannot issue them
directly (no `unsafe`, no inline assembly). The broker provides:

- **`FirmwareCall` grants**, each covering one function ID with a typed argument shape. They are
  listed on the driver's match entry or in the platform description, and logged.
- A **secure-monitor provider** in the arch crate for calls that take memory. Physical addresses
  travel only as opaque `ReservedRegion` or `TzBuffer` tokens the provider resolves. It handles the
  protocol's retry and wait-for-interrupt returns, so drivers see one call that may sleep.

## 9. Classes and publications

### 9.1 Classes

A **class** owns the protocol logic for a kind of device and its user interface. Leaf drivers
implement a narrow class trait. This is the IOKit family, the sDDF device class, FreeBSD's GEOM
disk and iflib (S§13.3).

| Class (crate) | Driver trait | Published as | User-visible names |
|---|---|---|---|
| storage (`sys/storage`) | `DiskQueues`: submit a batch of bios to a hardware queue, completion via the class's callback | GEOM-lite disk provider | `nda*`, `ada*`, `vtbd*`, plus partitions |
| net (`sys/net`, P8) | `EtherQueues`: tx/rx queue pairs, offload capabilities, link state, filter programming (iflib-shaped) | `ifnet` | `vtnet*`, `re*`, `igc*`, … |
| tty (`sys/tty`) | `UartHw`: line settings, tx/rx FIFOs, modem lines | tty | `ttyu*`, `cuau*` |
| console | `ConsoleOut`, `ConsoleIn` | Kernel console | `/dev/console`, `ttyv*` (with fb) |
| input (USB track) | `KbdSource`, `PointerSource` | kbd / evdev (FreeBSD's evdev ABI) | `/dev/kbd*`, `/dev/input/event*` |
| entropy | `EntropySource` | Random pool | — |
| watchdog | `Watchdog` | watchdog(4) | `/dev/fido` |
| rtc | `RealTimeClock` | clock | — |
| char | `CdevOps` (open, close, read, write, ioctl, mmap, poll, kqueue) | devfs node | any |

Class traits are **batch-first and queue-shaped**. `DiskQueues::submit` takes a batch of requests
for one hardware queue, and a completion hands back a batch. This matches NVMe, virtio, and AHCI
hardware, and it is the shape every high-performance system in the survey converged on (S§12).

**Requests are completed exactly once.** A class request (`Bio`, a tx `Packet`) is an owned value
handed to the driver. Completing it consumes it (`bio.complete(status)`). Dropping an uncompleted
request is a bug: in verifier mode it is reported with the driver's name and completed with `EIO`,
and in release builds it is completed with `EIO` and counted. Double completion cannot be
expressed. This removes WDM's whole class of IRP bugs (S§7.2).

**Cancellation is the class's job.** Classes that support cancellation (aborting a queued bio) do it
in their own queues, before submission. A driver gets an `abort` callback for requests already on
hardware and may decline it. Drivers never write cancellation races.

### 9.2 Publications and gates

`StartCx::publish(class_object)` returns nothing to the driver. The binding records the
publication, and the class keeps a **`Gate`** to the binding.

```rust
/// A revocable reference from a long-lived object to a binding.
pub struct Gate<T: ?Sized> { /* binding pointer, revoked flag, per-CPU in-flight counters */ }

impl<T: ?Sized> Gate<T> {
    /// Enters the driver for one call. Fails once the binding is detaching.
    pub fn enter(&self) -> Result<GateGuard<'_, T>, Revoked>;
}
```

- **Entry is cheap.** `enter` increments a per-CPU counter (a single uncontended relaxed atomic
  on the current CPU's cache line) and checks the revoked flag. `GateGuard::drop` decrements the
  counter on whatever CPU it runs on. Counters are signed, so migration between CPUs is fine. There
  is no shared cache line on the I/O path, unlike a refcount, and the cost is comparable to FreeBSD's
  `si_threadcount` increment in `devfs`. The P3 benchmarks measure it.
- **Revocation** sets the flag and waits until the sum of the counters is zero. Calls that entered
  before the flag was set run to completion. Later calls get `Revoked`, which classes turn into
  `ENXIO` for user operations, or fail with `ENXIO` for queued I/O.
- **Guards may sleep** (a cdev `ioctl` that waits for the hardware). Revocation waits for them.
  That is FreeBSD's `destroy_dev` drain. A driver that blocks forever inside a gated call stalls its
  own detach, and the verifier's detach timeout reports it (§13.1).

Open files hold the class object (a cdev, an ifnet reference), not the binding. After revocation
they keep working as objects (close works, `fstat` works) while device operations fail, exactly as
on FreeBSD after `destroy_dev`.

### 9.3 Platform providers

Some drivers serve other drivers rather than userland: interrupt controllers, GPIO controllers,
I2C and SPI controllers, PCC mailboxes, ACPI power resources, SCMI clocks and power domains, and
IOMMUs. They publish **provider** objects, keyed by their firmware identity (an ACPI path, an IORT
node), and consumers reach them through the dependency graph rather than by global name:

```rust
// In a consumer's attach, for a GpioInt in its _CRS:
let irq = cx.gpio_interrupt(GpioRef::from_crs(0))?;    // resolves the supplier edge from §6.1
```

The provider traits, all in `sys/bus` so consumers depend on no particular provider crate:

| Trait | Operations | Providers |
|---|---|---|
| `Clock` | enable, disable, `round_rate`, `set_rate`, `rate` | SCMI clock protocol; Qualcomm GCC/GPUCC/DISPCC/VIDEOCC; AML methods |
| `Reset` | assert, deassert, `pulse` (counted; group-aware) | SCMI reset; CIX S5 reset controller; Qualcomm block resets |
| `PowerDomain` | vote a level (`PerfLevel`, with on/off as the two-level case); votes from all consumers are combined by maximum | ACPI power resources; SCMI power; Qualcomm GDSCs and RPMh ARC levels; RPMI |
| `PerfDomain` | `levels()`, `request(level)`; the provider rate-limits and deduplicates, because firmware channels are shared | SCMI performance (Sky1 GPU, VPU, NPU); Qualcomm GMU and EPSS |
| `Interconnect` | vote average and peak bandwidth on a path | Qualcomm BCM through RPMh |
| `Phy` | counted `power_on`/`off`; `configure(mode)` leases lanes, and a reconfiguration first tells each lessee its lanes are revoked | USB/DP combo PHYs on Sky1 and SC8280XP |
| `RegisterRegion` | leases a field subset of a shared register block (§8.1) | Sky1 audio CRU; S5 reset controller's PHY bits; TC956x function 0 |
| `DmaEngine` | slave and cyclic channels by request line | Sky1 DMA-350 |
| `Mailbox` | typed channels (doorbell, FIFO, shared memory) | CIX mailbox; PCC; Qualcomm IPCC |
| `Gpio`, `I2c`, `Spi`, `Pinctrl` | as their FreeBSD counterparts (`gpiobus`, `iicbus`, `spibus`) | SoC controllers |

Providers are reached through supplier edges (§6.1), and a handle names a sub-handle where the
supplier is a `_STA = 0` stub or data-only child. Operations that talk to firmware sleep, so they
take a `ThreadCx` or an attach, start, or stop context, never `IrqCx`.

Provider handles held by a consumer go through the same `Gate` mechanism. A supplier cannot detach
while it has attached consumers, because the graph detaches consumers first (§10.1). The gate makes
a mistake in that ordering fail safely instead of touching freed state.

## 10. Lifecycle

### 10.1 State machine

```
             bus enumerates
                   │
                   ▼
            ┌─────────────┐   no driver matches: "?" event
            │ NotPresent  │───────────────────────────────┐
            └─────────────┘                               │
                   │ match + unit                         │
                   ▼                                      │
            ┌─────────────┐  supplier attached            │
            │  Waiting    │──────────────┐                │
            └─────────────┘              ▼                │
                   │ ready        ┌─────────────┐         │
                   └─────────────►│  Attaching  │ attach, start
                                  └─────────────┘         │
                       Decline: next rank │ error │ ok    │
                     ┌────────────────────┘   │   │       │
                     ▼                        ▼   ▼       │
               (re-match)             ┌────────┐ ┌──────────┐ "+" event
                                      │ Failed │ │ Attached │◄──── resume
                                      └────────┘ └──────────┘────► suspend
                                                      │ devctl detach / disable / set-driver,
                                                      │ parent detach, Gone, shutdown, reset
                                                      ▼
                                               ┌─────────────┐
                                               │  Detaching  │ "-" event
                                               └─────────────┘
                                                      │
                                     NotPresent (node kept) or deleted (Gone / DEV_DELETE)
```

All transitions run on manager workers, never in a driver callback or interrupt context. A request
to detach a node in `Attaching` waits for the attach to finish and then detaches. A node can never
be detached half-attached (S§2.3.2, D19375).

### 10.2 Detach protocol

Detach runs the same sequence whatever the reason (`DetachReason`: `Requested`, `Forced`, `Parent`,
`Gone`, `Shutdown`, `Reset`, `DriverChange`):

1. **Ask** (only for `Requested`). Check `may_detach()` and the FreeBSD busy count (`device_busy()`,
   which open cdevs hold). If either refuses, return `EBUSY`, as FreeBSD does without `-f`.
2. **Detach children**, depth-first. Consumers in the dependency graph are detached first,
   with reason `Parent`.
3. **Revoke publications.** Close every gate in reverse order of publication, and wait for
   in-flight calls to drain. From here no class, file, or consumer can enter the driver. Pending
   class requests are failed with `ENXIO` by their class, or held for the class to fail.
4. **Stop the hardware.** Call `stop(why)`. For `Gone`, `stop` is told the device has gone, and its
   MMIO reads may return all-ones.
5. **Tear down async sources**, in reverse order of creation. Interrupt handlers are removed
   synchronously (§8.3). Callouts are cancelled and drained. Tasks are drained. Sysctl nodes are
   removed. The binding's registry is the complete list, because a driver has no other way to
   create them (principle 2).
6. **Drop the driver instance.** Its fields drop in declaration order. MMIO grants unmap. DMA
   buffers unmap and free. IRQ grants return to their domains. A driver's own `Drop` may still touch
   registers, because its grants are fields of the instance and are still alive.
7. **Check the ledger.** Every grant the binding issued must have come back. A leak (a grant moved
   somewhere long-lived) is reported with the driver name and grant kind. In verifier mode it is a
   panic in CI.
8. **Release.** Power resources are released, the node moves to `NotPresent` (or is deleted), and
   the `-` devctl event is emitted.

Steps 3 to 5 are the part every surveyed system gets wrong when drivers do it by hand (S§13.2
classes 1 and 2). Here the driver writes only `stop`.

### 10.3 Surprise removal

A hot-unplugged device (USB, PCIe hotplug later) is reported by its bus. The node moves to `Gone`
and detach runs with `DetachReason::Gone`. Until `stop` is called, the driver may observe the
removal as all-ones reads, failed transfers, or timeouts. Drivers must treat these as errors
(§8.4), and the class fails outstanding requests. The node stays in the tree, in state `Gone`,
until the last `DeviceRef` is dropped.

All-ones reads are what x86 and most arm64 root complexes return. Some do not: on the SC8280XP a
completer abort or unsupported request is a fatal SError. The PCI bus records, per root complex,
whether such errors are contained, and hotplug, USB4 tunnels, and `DEV_RESET` are refused below a
complex where they are not, until RAS containment exists for it.

### 10.4 Attach failure

An attach that returns an error drops everything it claimed (ownership), and the node moves to:

- the next-ranked candidate, for `Decline`;
- `Waiting`, for `Wait(need)`;
- `Failed`, otherwise. `hw.bus.disable_failed_devices` behaves as in FreeBSD.

If `start` fails, the framework runs the detach protocol from step 3, so a half-started driver is
torn down exactly like a fully started one.

## 11. Userland projections

These are compatibility surfaces. Each is generated from the manager's state, and each is checked
against FreeBSD 15.1 by `cargo xtask abi-check` (layouts) and by differential tests on the build
server (strings and behavior, from the same bhyve and QEMU configurations).

### 11.1 sysctl

| Node | Source | Notes |
|---|---|---|
| `hw.bus.info` | `u_businfo { ub_version: 2, ub_generation }` | The generation increments on every structural change and every state change visible in `u_device` |
| `hw.bus.devices.<n>` | One `u_device` per node, in tree order | `dv_fields` packs `name\0desc\0driver\0pnpinfo\0location\0`. Encoding lives in `lib/devtree` and is host-tested |
| `hw.bus.rman.<n>`, `hw.bus.rman.<n>.<m>` | Resource spaces and their allocations | For `devinfo -r` and `-u` |
| `dev.<class>.<unit>.%desc`, `%driver`, `%location`, `%pnpinfo`, `%parent`, `%iommu`, `%domain` | Node fields | Created when a driver is chosen, removed when the node loses it |
| `dev.<class>.%parent` | Parent bus class | |
| `dev.<class>.<unit>.*` | Driver sysctls | Created through `StartCx`, part of the binding, removed at detach step 5 |
| `hw.bus.disable_failed_devices`, `hw.bus.devctl_queue` | Tunables | FreeBSD semantics |
| `kern.abyss.bus.*` | Extensions: waiting nodes, the dependency graph, ledger counters | Additive only |

### 11.2 pnpinfo and location strings

They are produced by each bus's `pnpinfo` and `location` (§5.1), **byte for byte** in FreeBSD's
formats, because `devd.conf` rules and `devmatch` match on them:

| Bus | pnpinfo | location |
|---|---|---|
| PCI | `vendor=0x%04x device=0x%04x subvendor=0x%04x subdevice=0x%04x class=0x%02x%02x%02x` | `slot=%d function=%d dbsf=pci%d:%d:%d:%d` |
| ACPI | `_HID=%s _UID=%lu _CID=%s` | `handle=%s` |
| virtio, USB | FreeBSD's formats for those buses | FreeBSD's formats |

### 11.3 `/dev/devctl` and devd

- **A single reader**, as in FreeBSD. Records are newline-terminated:
  - `+<name> at <location> <pnpinfo> on <parent>` on attach;
  - `-<name> at …` on detach;
  - `? at <location> <pnpinfo> on <parent>` on no match;
  - `!system=<sys> subsystem=<sub> type=<type> [key=value …]` for notifications.
- The queue is bounded by `hw.bus.devctl_queue` (0 disables it). Old events are dropped when it is
  full, and quoting follows `devctl_safe_quote_sb()`.
- Notifications that devd's stock rules expect come from their owning subsystems: `ACPI` (power
  button, lid, AC adapter, thermal, and `notify=0x..` for ACPI `Notify`), `kernel power resume`,
  `IFNET` link state (P8), `GEOM` (`DEV` create and destroy), `USB`, and `DEVFS` (`CDEV` create and
  destroy).
- devctl events also feed the netlink `nlsysevent` hook when netlink lands (P8), as in FreeBSD.

### 11.4 `/dev/devctl2`

The `devreq` ioctls map onto manager operations:

| ioctl | Manager operation |
|---|---|
| `DEV_ATTACH` | Match and attach a `NotPresent` or `Failed` node |
| `DEV_DETACH` | Detach (`Requested`, or `Forced` with `DEVF_FORCE_DETACH`) |
| `DEV_ENABLE`, `DEV_DISABLE` | Clear or set `DF_ENABLED`; disable detaches |
| `DEV_SUSPEND`, `DEV_RESUME` | Per-device suspend and resume (P9; `EOPNOTSUPP` before) |
| `DEV_SET_DRIVER`, `DEV_CLEAR_DRIVER` | Driver pin (§7.2) |
| `DEV_RESCAN` | `Bus::enumerate` again on a bus node |
| `DEV_DELETE` | Detach and delete a node, if its bus allows it |
| `DEV_FREEZE`, `DEV_THAW` | Hold new attaches (`WaitReason::Frozen`) |
| `DEV_RESET` | Device reset (§13.3), with `DEVF_RESET_DETACH` semantics |
| `DEV_GET_PATH` | Path by locator kind |

Names resolve as in FreeBSD: by nameunit, by `pciD:B:S:F`, or by ACPI path, through a lookup that
each bus provides. All operations need root privileges and are refused inside jails.

### 11.5 Other surfaces

- **`/dev/pci`** (`sys/pci`): `PCIOCGETCONF` (with `pd_name` and `pd_unit` from the tree),
  `PCIOCREAD`/`WRITE` (writes limited as in FreeBSD and gated by securelevel), `PCIOCATTACHED`,
  `PCIOCGETBAR`, `PCIOCLISTVPD`, `PCIOCBARMMAP`, `PCIOCBARIO`.
- **devfs**: cdevs are created through `StartCx::cdev` (gated), with FreeBSD names, modes, and
  owners. `devfs.rules` applies unchanged.
- **`kldstat`, `modfind`, `kldfind`**: report the driver table as built-in modules
  ([ADR-0005](adr/0005-monolithic-static-kernel.md)).

## 12. Performance

The bound is 2% against FreeBSD on every benchmark ([ADR-0011](adr/0011-performance-parity.md)).
The design keeps the framework off every hot path:

| Path | FreeBSD | AbyssBSD | Expected difference |
|---|---|---|---|
| Interrupt dispatch | `intr_event` → filter function pointer | Handler slot → one indirect call to `fn(&D, usize, &IrqCx)` | None. No lock or lookup. The epoch enter on the dispatch path is a per-CPU increment |
| I/O submission | GEOM → `d_strategy` pointer → driver | Class → gate enter (per-CPU) → `&dyn DiskQueues` call | Gate enter is comparable to `si_threadcount`. Batching amortizes it |
| Completion | Filter or ithread → `biodone` | Filter or ithread → class completion, batched | None |
| DMA map (no IOMMU) | `_bus_dmamap_load_*` with callbacks | `DmaTag::map`: address arithmetic, inline no-op syncs when coherent | Expected faster (no callback, no deferred-load machinery) |
| DMA map (IOMMU, P9) | DMAR or SMMU page-table updates, deferred invalidation | Pools for steady-state buffers, batched invalidation for the rest | Same or better, and measured |
| Serialized driver | Driver's own mutex | Framework mutex around the entry point | None. Same lock, taken in a different place |

Rules for hot paths:

- No topology lock, no allocation (pre-allocated request pools, `DmaPool`), no shared atomics, no
  dynamic dispatch through more than one level, and no thread hops.
- Generic code is monomorphized only where it pays off (the per-driver handler shims). Bus and
  manager code uses `&dyn` ([ENGINEERING.md §6](ENGINEERING.md#6-style-and-lints)).
- MSI-X vectors per CPU or per cluster, so completions run where submissions did.
- Polling mode (busy-poll and interrupt moderation) is a class feature for net (P8), not a driver
  rewrite.

The P3 suite already measures interrupt latency and block IOPS and latency through virtio-blk and
NVMe ([PERFORMANCE.md §5](PERFORMANCE.md#5-benchmark-suites-by-phase)). This design adds four
benchmarks to P3:

- gate enter/exit cost, uncontended and with 8 CPUs;
- device-tree attach time on the QEMU and bhyve configurations (it feeds P6 boot time);
- DMA map/unmap throughput through the direct and bounce backends;
- devctl detach/attach cycle time for virtio-blk under I/O load (a correctness benchmark that also
  catches drain slowness).

## 13. Verification and robustness

### 13.1 Verifier mode

Debug builds, and release builds booted with `abyss.verifier=1`, enable runtime checks in the spirit
of Driver Verifier (S§7.1):

- **The binding ledger**: every grant, async source, and publication is recorded with the creating
  driver. Detach checks that all came back (§10.2 step 7).
- **Request accounting**: dropped uncompleted requests, and requests still outstanding when a gate
  closes.
- **DMA checking**: CPU access to a `DeviceOwned` buffer is impossible by type, but verifier mode
  also poisons unmapped bounce pages and checks `DeviceShared` reads against the coherency
  configuration.
- **Fault injection**: probabilistic failure of allocations, grants, DMA maps, and command
  timeouts, seeded and reproducible, so attach error paths actually run.
- **Detach timeouts**: a gate or a `stop` that does not drain within a bound is reported with a
  backtrace of the threads inside the driver.
- **The witness** (P2) covers the binding serializers, the topology lock, and the node state locks.

CI runs the boot and stress tiers with the verifier on. P3 adds a **detach stress test**: repeated
`devctl detach` and `attach` of virtio-blk and NVMe in QEMU under random I/O, with ledger and
allocator accounting checked after each cycle.

### 13.2 Host testing

`lib/devtree` holds everything that does not touch hardware: matching, ranking, ambiguity checks,
graph construction, cycle breaking, order computation, unit allocation, and all the userland
encodings. It is tested against:

- the ACPI corpus (`tests/corpus/*`), so every device in every captured DSDT gets the expected
  driver and dependency set;
- recorded FreeBSD outputs (`devinfo -rv`, `sysctl dev`, devd event logs, `pciconf -lv`) from the
  build server on the same VM configurations, for string-exact comparison;
- fuzzing of the graph builder with random `_DEP` and `_CRS` structures.

Drivers are written so their protocol logic (queue state machines, descriptor parsing, register
programming sequences) is testable on the host against a fake register block. That is the same
`RegisterIo` pattern the 16550 driver already uses.

### 13.3 Device reset and rebind

Most driver recovery is a device reset, not a driver restart (S§7.1, TDR):

- A driver that detects a wedged device (a command timeout, a fatal status bit) asks the
  framework for a reset through `ThreadCx::request_reset(kind)`.
- The framework runs the detach protocol with `DetachReason::Reset`, asks the bus to reset the
  device (PCIe function-level reset, hot reset of the port, USB port reset), and attaches again.
- Classes keep their published objects across the reset when the new binding republishes the same
  identity (the same namespace on the same controller). Queued requests are retried, if the class
  allows it, instead of failing. Open files survive.
- `DEV_RESET` from devctl triggers the same path.
- `ResetKind` includes `DriverDefined` for devices whose reset is not a bus operation: a remote
  processor is stopped (with its stop handshake) and its image re-authenticated and restarted.
- **Recovery inside the driver is allowed** when the device can recover without losing state that
  userland holds. A GPU hang is reset by reloading its firmware while VMs, buffers, and open files
  persist, and that is frequent enough that a rebind would be wrong. The framework reset above is
  for the case the driver cannot recover from, and a device that cannot be brought back at all is
  moved to `Gone`.

## 14. Future seams

These are not Day-1 work. The design keeps room for them so they do not need a new model:

- **Runtime power management (P9).** The driver interface is in §8.5. What remains is
  power-managed class queues that resume the device before submission (WDF, S§7.1), and idle
  policy for storage and network. The dependency graph already gives the order.
- **Hotplug (USB track, then PCIe).** Gone handling (§10.3) is part of the base design, and USB
  exercises it first. PCIe hotplug (and USB4 PCIe tunnels behind it) needs a port-service
  driver on bridges, `_OSC` control requests per feature as FreeBSD's
  `acpi_pcib_request_feature` makes them, and bus numbers and windows sub-allocated from the
  parent bridge, including firmware's hotplug padding. Devices behind an `ExternalFacingPort`
  get an IOMMU or bounce DMA tag, never `Direct`, so tunnels stay off until P9.
- **IOMMU groups (P9).** The DMA domain is per group, and the broker reports the group as
  `%iommu`. bhyve passthrough (`ppt`) would attach to a group, not to a function.
- **Low-rate drivers outside the kernel** (after v1.0, if ever). Because drivers see hardware only
  through grants (MMIO, IRQ, DMA) and classes only through traits, a grant proxy could host a
  driver in a user process for HID, serial, or printers, like DriverKit or UMDF. That needs no
  change to the in-kernel model. It would never be used for storage or network.
- **Loadable modules** (ADR-0005, after v1.0). A module would be another source of `DriverDecl`s,
  and the matcher and graph already handle drivers arriving late.

## 15. Rejected alternatives

| Alternative | Why not |
|---|---|
| kobj method tables (FreeBSD) | Untyped, a cache probe on every call, hand-written interfaces. Rust traits do the same work with type checking and static dispatch (S§2.3.5) |
| Numeric bus passes | Coarse, global, and silent about why a device waits. The graph subsumes them (S§2.3.3) |
| Linux-style deferred probe | A blind retry that cannot diagnose a missing supplier (S§5.4.2) |
| devres-style binding-scoped allocation | Backs user-visible objects with binding-lifetime memory, the source of the devm bug class (S§5.4.1). Ownership and gates do the job |
| Driver-owned detach (FreeBSD, WDM) | Every system that left drain to drivers got it wrong (S§13.2). WDF moved it into the framework |
| Generic filter driver stacks (WDM) | Flexible, but the main source of WDM complexity. Filters live at class-defined points: GEOM classes, ifnet taps (S§7.2) |
| Per-driver threads or queues (DriverKit, Fuchsia DFv1) | Measured at +50–150% CPU and about 12% throughput (S§13.4) |
| Drivers in user space or in isolated domains | Too slow for a 2% bound on the data path (S§13.4). The seam in §14 keeps it possible for low-rate classes |
| A `'bound` lifetime brand on all resources (Rust for Linux, 2026) | Needed where C owns the binding. Owned values with a ledger give the same guarantee here more simply (§4.1) |
| Linker-section driver registration | Hides the driver set. An explicit table in the platform configuration is reviewable and ordered (§7.1) |
| LinuxKPI | Imports Linux's model and lifetimes into the kernel. Rejected for Day 1 ([ROADMAP](ROADMAP.md) open decision 7) |
| A stable driver ABI | Freezes mistakes (IOKit's vtable padding, Banjo, illumos DDI). Pointless while the kernel is static |

## 16. Phasing

| Phase | Driver-model work |
|---|---|
| **P3** | `lib/devtree`. `sys/bus` manager, lifecycle, ledger, gates, contexts, parallel attach. `sys/irq` domains (LAPIC, I/O APIC, GIC distributor, ITS, PCI MSI/MSI-X, and the threaded-only ACPI GED domain, which the arm64 power button needs). The kernel sysctl tree that `hw.bus.*` and `dev.*` hang from. Early-device adoption. ACPI bus (including the dependency sources in §6.1 that AML provides) and PCI bus (ECAM, BARs, capabilities). `vm::dma` direct and bounce backends, `DmaCoherent`, `DmaStream`, `DmaPool`. virtio-pci transport. virtio-blk, NVMe, AHCI on the storage class. Watchdogs, RTC, entropy. Shutdown ordering. `hw.bus.*` and `dev.*` sysctls. Verifier mode. The detach stress test |
| **P4** | cdev publication through devfs (gated). The tty class on the serial console. `/dev/devctl` queue (readable once userland exists) |
| **P5** | The storage class grows GEOM-lite partition providers and FreeBSD disk naming |
| **USB track** | USB bus, hubs, surprise removal, threaded-only GPIO interrupt domains on arm64 (GED's arrives in P3), input class |
| **P6** | `/dev/devctl2`, `/dev/pci`, devd running on stock rules, `devinfo`/`devctl`/`pciconf` differential tests. Device reset and rebind. IRQ affinity through `cpuset` |
| **P7** | Board bring-up: the platform description (§5.5) with the Sky1 entries (the Q8B's are Tier 2, best effort), the Order and Power edge kinds and the new edge sources (§6.1), the `Clock`, `Reset`, and `PowerDomain` provider traits with their first providers (§8.5, §9.3), `PciFunction::acpi_companion`, firmware-shared and declared-shared ranges (§8.1), and the firmware store (§8.6) if a P7 driver needs an image |
| **P8** | Net class (queue pairs, offloads), virtio-net, e1000e, igc, rge. `miibus` and PHY drivers as a child bus of the MAC. Driver-published IRQ domains (§8.3) |
| **P9** | IOMMU backends (VT-d, AMD-Vi, SMMUv3, and SMMUv2 for the SC8280XP) and interrupt remapping. IORT, DMAR, and IVRS dependency edges. Suspend and resume (stretch), wake interrupts. Runtime PM (§8.5). `PerfDomain` and `Interconnect` providers. Firmware calls and the secure-monitor provider (§8.7). PCIe hotplug |
| **M-G** | Display and GPU classes. Function children and aggregates in anger: synthetic aggregate nodes, `Phy` lane leasing, Type-C events. Device-walked page tables, shared buffers, and fences (§8.4, §17) |

## 17. Open questions

1. **`Concurrency::Serialized` for interrupt threads.** Should ithreads of a serialized driver take
   the serializer (simpler drivers, the IOKit default), or run concurrently with class calls (the
   FreeBSD default, with lower latency)? This design takes the serializer, to be revisited if the
   P3 interrupt-latency benchmark shows a cost.
2. **Unit numbering on attach failure.** Resolved (P3.2, from `device_attach` in
   sys/kern/subr_bus.c): a failed attach removes the device from its devclass, freeing the unit,
   unless the devclass was fixed at creation or `hw.bus.disable_failed_devices` is set (then the
   device stays, disabled, keeping its unit). A driver that later claims the device takes the
   lowest free unit of its own devclass. A device disabled by a hint keeps its unit.
   `lib/devtree::units` implements this.
3. **Where class traits live.** One crate per class (`sys/storage`, `sys/net`), or a single
   `sys/class` with features? This doc assumes one crate per class, matching the existing plan.
4. **Typed register macro.** `lib/regs` needs a register-block macro without external crates. A
   declarative `macro_rules!` design should be prototyped against NVMe's and the GIC's register
   maps before P3 drivers are written.
5. **`DeviceShared` soundness.** A declarative macro cannot easily prove that a struct has no
   padding. Either require explicit padding fields and check `size_of` against the sum of the
   field sizes in a `const` assertion, or write an in-workspace `proc-macro` crate (allowed by
   ADR-0002, since it is not external). The first option is simpler.
6. **Graphics memory.** Device-walked page tables (`BusAddr`, `IoPageTable<F>`), buffers mapped by
   the CPU and several devices at once (a VM object with a device pager, FreeBSD's
   `d_mmap_single`; a dma-buf equivalent; user-page pinning), user-mapped register pages, and fences
   whose signalling context cannot block on allocation. The panthor, Adreno, Linlon, Iris, and
   Zhouyi specifications all need them. The forward design is [GRAPHICS.md](GRAPHICS.md) (draft),
   finalized with the graphics ABI decision (ROADMAP open decision 6) at the start of M-G.
7. **Sky1 firmware transport.** [HARDWARE.md §4](HARDWARE.md) records Sky1 clocks as AML methods
   that call SCMI over PCC. The clean-room `sky1-soc` specification describes an SCMI agent over
   the CIX mailbox (`_DSD` `mboxes`) and a separate SCMI-over-SMC agent for power domains. Both may
   be true at once. The Orange Pi 6 Plus DSDT, once captured into `tests/corpus/`, settles which
   providers §9.3 needs first.
8. **Platform description format.** §5.5 tables are `const` Rust data. Whether CIX's in-AML tables
   are extracted at boot or transcribed at build time from the corpus is open. Extraction follows
   firmware updates, and transcription is simpler to review.
