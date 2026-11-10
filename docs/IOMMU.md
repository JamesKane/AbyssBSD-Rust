# IOMMU: DMA translation through the SMMUv3

- Status: **Accepted** ([ADR-0022](adr/0022-smmuv3-in-p9.md)). The design for Phase 9's SMMUv3 item
  ([ROADMAP.md](ROADMAP.md#phase-9-sky1-hardening-l)). Stage 0 (§8) landed 2026-11-10; what it
  decided where this design was silent or wrong is in §14, stage 2's in §15, and stage 3's in
  §16
- Date: 2026-11-10
- Scope: the arm64 SMMUv3 as IORT describes it, the IOMMU framework it plugs into (units, domains,
  contexts, IOVA allocation), the `Iommu` backend of `vm::dma`, and MSI through a translated
  path. x86 DMAR and AMD-Vi, SMMUv2, stage 2, ATS, and PRI are deferred (§9)
- Inputs: [DRIVER-MODEL.md](DRIVER-MODEL.md) §6, §8.3, §8.4, §9.3, §14, which this document
  makes concrete for the SMMUv3 and moves from P11 to P9; [ARCHITECTURE.md](ARCHITECTURE.md) §5.2,
  §6; the Orange Pi 6 Plus ground truth in `tests/corpus/opi6p/` and `tests/corpus/acpi/opi6p/`

The short version: **FreeBSD's split, with the types this project uses everywhere else.** An `smmu`
driver attaches under `acpi0` for each SMMUv3 the IORT lists, as FreeBSD's `smmu_acpi` identify
adds them. It publishes an IOMMU provider. A new dependency edge holds back every device it
translates until it has settled. The broker then gives those devices `vm::dma` tags with a third
backend, beside direct and bounce, whose mappings are IOVAs from a FreeBSD-style address-ordered
allocator (`iommu_gas`). Each requester gets its own domain, mapped with strict invalidation. The
ITS doorbell is mapped into each domain, so MSIs keep working. Firmware's reserved memory is
bypassed. Drivers do not change: they already ask for tags and map buffers through them.

Behavior reference: sys/arm64/iommu (smmu.c, smmu_acpi.c, smmureg.h, smmuvar.h, iommu.c,
iommu_pmap.c, iommu_pte.h), sys/dev/iommu (busdma_iommu.c, iommu_gas.c, iommu.h, iommu_msi.h),
sys/arm64/acpica/acpi_iort.c, sys/arm64/arm64/gicv3_its.c (`gicv3_iommu_init`), and the
`%iommu` handler in sys/kern/subr_bus.c. On top of that is the user's own FreeBSD port that the
board runs (`../AbyssBSD/src`, branch `orangepi-6-plus`, BSD-2-Clause, kernel
`GENERIC-HMP-IOMMU`), whose IOMMU changes are summarized in §3.3. The hardware references are the
Arm SMMUv3 architecture specification (IHI 0070), the IORT specification (DEN 0049, revision E.e,
which the board's revision-5 table follows), and the GICv3 specification (IHI 0069) for the
doorbell. FreeBSD is the specification of behavior, not the source of the code
([ADR-0007](adr/0007-license-and-clean-implementation.md)).

## 1. Why, and what for

### 1.1 The defect

On the Orange Pi 6 Plus (CIX Sky1), RAM sits below `0x8_0000_0000`, plus a 2 GiB region at
`0x80_0000_0000`. AbyssBSD gives PCIe devices (NVMe, `rge`) their buffers' physical addresses.
DMA to the high region does not reach it: it lands somewhere else, corrupts memory silently,
and hangs the board. This was shown on 2026-11-10. With that RAM left unused (`hw.physmem=29G`,
commit `060a614`, `kernel/src/mm/mod.rs`), gigabytes of writes ran clean. Without it, 512 MiB
wedged the board. Firmware gives no warning. The IORT's root-complex node has a memory address
size limit of 0, and the host bridges have no `_DMA`, so neither the direct backend nor the
bounce backend has a limit to honor.

FreeBSD on the same board does not hit this: its DMA goes through the SMMUv3s, which translate by
default there (`hw.iommu.dma=1`). The IOVAs it hands out are small, since its allocator is
lowest-first, and the SMMUs output 40-bit physical addresses (`oas 40-bit` in the Linux ground
truth), which cover the high region. The user has decided against a workaround such as a
board-specific DMA limit: the fix is the SMMU, FreeBSD's design, in Phase 9. Until it lands, the
board runs with `hw.physmem=29G`.

### 1.2 Goals

- **DMA from PCIe devices on the Sky1 is translated**, as on the FreeBSD the board runs, so all RAM
  is usable and `hw.physmem` comes out of the board's `loader.conf`.
- **FreeBSD's device tree and knobs.** `smmu0` and `smmu1` sit under `acpi0` with FreeBSD's
  resources, descriptions, and sysctls. Translated functions print what FreeBSD prints. `%iommu`
  reads as on FreeBSD on both ISAs (§4.2).
- **Ordering is derived.** No device the SMMU translates starts DMA before the SMMU is ready. The
  dependency graph says so; no bus pass does (§4.3).
- **Drivers do not change.** Translation lives in the broker and in `vm::dma`. Driver crates
  stay `#![forbid(unsafe_code)]`, the SMMU driver included (§4.1).
- **Confinement (DRIVER-MODEL G7).** A translated device reaches only what is mapped for it, and
  the mapping is gone before the CPU takes a buffer back (§4.5).
- **Performance parity** on the board, against the same FreeBSD (§10, and §13 question 3).

### 1.3 Non-goals for Phase 9

- Translation for named components (ACPI devices behind `smmu1`: the display controllers, NPU,
  ISP). AbyssBSD has no driver for any of them until M-G. Their streams are handled (§4.9), and
  the design is written here, but building it waits for a consumer.
- x86 DMAR and AMD-Vi, interrupt remapping, the SMMUv2 (SC8280XP), stage 2 and nesting, ATS and
  PRI, PASIDs, hotplug, and IOMMU groups for passthrough (§9).

## 2. FreeBSD's design

### 2.1 The pieces

| Piece | Where | Role |
|---|---|---|
| `smmu` (ACPI attachment) | arm64/iommu/smmu_acpi.c | `identify` walks the IORT and adds one `smmu` child to `acpi0` per SMMUv3 node, with resources built from the node: memory `0x20000` at its base and IRQs 0–3 = event, PRI, sync, gerror GSIVs. `probe` accepts only the generic model (`BUS_PROBE_NOWILDCARD`). `attach` allocates the memory and the event, sync, and gerror IRQs (not PRI). It records COHACC from the node's flags, runs `smmu_attach`, and registers the unit, with its base address as its xref |
| `smmu` (hardware) | arm64/iommu/smmu.c, smmureg.h, smmuvar.h | Reads the IDRs. Builds a linear or two-level stream table, the command and event queues, and ASIDs. Builds context descriptors (one per domain) and stream table entries. Runs the command queue with `CMD_SYNC` completion, either polled or by an MSI written to memory when coherent. Handles event and gerror interrupts, and an errata table (MMU-600). Implements `IOMMU_MAP`, `IOMMU_UNMAP` (TLBI per page, then one sync), domain and context alloc/init/free, and `IOMMU_FIND` |
| IOMMU framework (arm64 glue) | arm64/iommu/iommu.c, iommu_if.m | The registry of units. `iommu_get_ctx`: **one domain per context** ("in our current configuration we have a domain per each ctx"). Domain and context lifetimes, with each context counted |
| I/O page tables | arm64/iommu/iommu_pmap.c, iommu_pte.h | A small pmap for stage 1: a VMSAv8-64 4 KiB granule, 48-bit input (`MAXADDR_48BIT`) |
| busdma backend | dev/iommu/busdma_iommu.c | The third `bus_dma_impl` beside direct and bounce. `iommu_get_requester` walks PCI bridges to the requester ID the IOMMU sees. `iommu_instantiate_ctx`. The `hw.busdma.default` and `hw.busdma.pciD.B.S.F` = `bounce`/`iommu` knobs: a disabled requester gets a pass-through context. Loads that cannot sleep are delayed to a taskqueue. `hw.iommu.dma` (default 0) turns DMA translation on |
| IOVA allocator | dev/iommu/iommu_gas.c | "Guest address space": an address-ordered **first-fit** red-black tree, augmented with the largest free gap below each node. Allocations are lowest-first under the tag's `lowaddr`, with **guard pages on both sides** of each one. Reserved regions and identity (RMRR) regions. The MSI page (`iommu_map_msi`, `iommu_translate_msi`) |
| IORT | arm64/acpica/acpi_iort.c | Maps a PCI RID through the root complex to an SMMU stream ID and to an ITS device ID |
| MSI | arm64/arm64/gicv3_its.c | `gicv3_iommu_init`: if the function has a context, map the page holding `GITS_TRANSLATER` into its domain. The composed message address is then translated into that domain |

How a PCI device gets translated, in order: `smmu` attaches in an early pass
(`BUS_PASS_INTERRUPT + BUS_PASS_ORDER_LATE` since upstream commit `0bcd0616ff8`: after the
interrupt controllers, before every default-pass device). A driver's first `bus_get_dma_tag`
reaches `pci_get_dma_tag`, which asks `iommu_get_dma_tag`. That finds the unit (`IOMMU_FIND`:
the IORT lookup of the requester's stream), instantiates the context and its domain, writes the
stream table entry, and returns the context's tag. `bus_dmamap_load` allocates IOVA space from
`gas` and maps the pages. `bus_dmamap_unload` unmaps them (TLBI, sync) and frees the space. MSI
allocation maps the doorbell page and translates the message address. Faults arrive on the event
queue and are printed from the interrupt filter.

### 2.2 Defaults

Stock FreeBSD arm64 `GENERIC` has no `options IOMMU`: every file above is `optional iommu`, so the
board's stock FreeBSD 15.1 neither attaches `smmu` nor translates. amd64 `GENERIC` has
`options IOMMU`, but `hw.dmar.enable` and `hw.amdiommu.enable` default to 0, so x86 machines run
untranslated too. That is why P10 needs no IOMMU (§9). The FreeBSD the corpus records is the
user's `GENERIC-HMP-IOMMU` (= `GENERIC` + `options IOMMU`), set up on the board with
`hw.iommu.dma=1` and `hw.smmu.bypass_named=0` in `loader.conf`.

### 2.3 What the board's port adds

The sibling branch's IOMMU commits (2026-09-29 to 2026-10-08, by the user) are the board's lessons.
Each one lands somewhere in this design:

| Commit | What it found | Here |
|---|---|---|
| `1b89d7594db` command queue ordering | Nothing ordered the store of a queue entry before the `PROD` write that publishes it. A sync could pick its completion slot before taking the lock and then wait on an older sync. Sleeping in sync waits (`DELAY(100)`) held a translated NIC to a few MB/s | §4.7: a store barrier before `PROD`; the slot is chosen, written, and published under the lock; sync waits spin |
| `76b8fc65b72` context references | arm64 did not count context references, so the second lookup (MSI after DMA tag) freed a live context | §4.4: contexts are reference-counted objects |
| `01b65ffd609` host bridge windows | A PCI device's request to an address inside its host bridge's windows goes to the device there (peer to peer), not to memory. The board's PCIe 0 32-bit window is `0x6000_0000–0x7fff_ffff`, within reach of a lowest-first allocator | §4.6: each PCI domain reserves its host bridge's `_CRS` memory windows |
| `1cc7e77320b` RMR bypass, stay off when disabled | Without parsing IORT RMRs, the display controllers' scanout of the firmware framebuffer faulted once the SMMU was enabled. With `hw.iommu.dma=0`, an enabled SMMU aborted all DMA | §4.9: RMR streams get bypass STEs before enable, and the message `smmu1: 10 streams bypassed (reserved memory)`. §4.12: translation off leaves the SMMU as firmware left it |
| `cf75a5f809b` reserved regions on free | Freeing a domain that still held reserved and identity regions failed an assertion | §4.6: a domain drops its reservations as it goes |
| `00f4c20e278`, `eebfc167ba9`, `036ff2d8f16`, `8931fe320eb` IORT named components | Named-component stream IDs, counted in 64 bits | §4.2 (`lib/acpi-tables`) |
| `446348afb11` translate ACPI devices | Named components get contexts with several stream IDs (a display controller has five). RMR memory is identity-mapped before their streams are translated, and the streams go back to bypass when freed | §4.9, built in M-G |
| `0dd7d31e2e6` non-coherent caches | busdma_iommu came from x86 and did no cache maintenance | Already structural here: `vm::dma` syncs by the tag's coherency whatever the backend (§4.5) |
| `db2f0538c8b` relaxed ordering | With PCIe relaxed ordering, an NVMe completion overtook its data. The buffer was unmapped on completion, and the late data faulted and was lost (a Crucial P3, a gigabyte read back wrong within an hour). With RO off, four hours ran clean, twice | §4.4: clear `DEVCTL.RO` on every translated PCIe function, and print `nvme0: relaxed ordering disabled (IOMMU translation)`, `hw.iommu.relaxed_ordering` |
| `2427574f670` Normal memory | Mapping DMA pages as Device-nGnRnE made the NPU ten times slower | §4.7: Normal write-back inner-shareable for coherent devices, Normal non-cacheable outer-shareable for non-coherent ones |
| `67d6727be10` cacheable walks | Context descriptors left the table walk non-cacheable on a coherent SMMU | §4.7: TCR walk attributes write-back inner-shareable when the SMMU is coherent |
| `1464aadaf77` reserve from 0 | Reserving a region that starts at IOVA 0 looped forever (the Qualcomm video codec's firmware-protected bottom) | §4.6: a host test reserves from 0 |

Upstream FreeBSD (Andrew Turner, 2026-09) also added the MMU-600 errata check (erratum 1076982:
no SEV before variant 1), polled sync completion, and MSI-to-memory sync completion only on a
coherent SMMU. All three are in §4.7.

## 3. The board

### 3.1 What the IORT says

`tests/corpus/acpi/opi6p/iort.dat` (revision 5, 19 nodes), decoded:

| Node | Facts |
|---|---|
| ITS group | One ITS, id 0 (`its0` at `0xe050000`, so the doorbell page is `0xe060000`) |
| SMMUv3, id 0 | Base `0xb010000`. Flags `0x1` (COHACC override: coherent). Model 0 (generic). GSIVs: event 107, PRI 116, gerror 112, sync 108. One ID mapping: `0x0–0xffff` → ITS, identity (its MSIs and its clients' device IDs) |
| Root complex | Segment 0. Memory access properties: CCA 1, CPM and DACS set (coherent). ATS supported. **Memory address size limit 0.** One mapping: RIDs `0x0–0xffff` → SMMU 0, stream ID = RID |
| SMMUv3, id 1 | Base `0xb1b0000`. Flags `0x1`. Model 0. GSIVs: event 141, PRI 150, gerror 146, sync 142. No ID mappings (no MSIs) |
| 14 named components | `\_SB.DPU0`–`DPU4` (5 streams each), `AEU0`–`AEU4` (1 each), `NPU0`, `ISPM`, `CBD0`, `CBD2`. All map to SMMU 1 with single mappings, stream IDs `0x0–0x22`. Coherent, 40-bit address limit |
| RMR | Revision 3. Flags `0x11`: remapping permitted, access attributes Normal inner/outer non-cacheable. One range: `0x8480_0000` + 8 MiB (the firmware framebuffer). 10 streams on SMMU 1: `0x0, 0x1, 0x6, 0x7, 0xc, 0xd, 0x12, 0x13, 0x18, 0x19` (two per display controller) |

The three PCIe host bridges (`pcib0`, `pcib2`, `pcib4`) are all segment 0, on disjoint bus ranges
(`pciconf`: buses 48, 96, 144, 192). So one RC node serves them all, and a function's RID is
unique across them. xHCI, HDA, the DMA-350, and the audio subsystem are not in the IORT: no SMMU
translates them, and they stay on the direct backend (risk in §12).

### 3.2 What FreeBSD and Linux show

- FreeBSD (`dmesg.boot.txt`):
  - `smmu0: <ARM System Memory Management Unit> iomem 0xb010000-0xb02ffff irq 136,137,138,139 on acpi0`
  - the same for `smmu1` at `0xb1b0000-0xb1cffff`
  - `smmu1: 10 streams bypassed (reserved memory)`
  - `nvme0`, `nvme1`, `rge0`, `rge1`: `relaxed ordering disabled (IOMMU translation)`
- FreeBSD (`devinfo-rv.txt`): `smmu0` and `smmu1` are children of `acpi0`, after `gic0` and before
  `generic_timer0`, with `pnpinfo unknown`. Each has three active interrupts (event, sync,
  gerror, but not PRI), e.g. `0x88`, `0x8a`, `0x8b`.
- FreeBSD (`sysctl-a.txt`):
  - `dev.smmu.N.dma: 1`
  - `hw.iommu.check_free: 0`, `hw.iommu.relaxed_ordering: 0`, `hw.smmu.bypass_named: 0`
  - `dev.<pci device>.%iommu: rid=0xc100` (and so on) for every PCI device, root ports included.
    The bhyve corpus shows the same `rid=` strings on amd64 `GENERIC`.
- Linux (`linux/dmesg.txt`, `interrupts.txt`, `iomem.txt`; ground-truth output only):
  - both SMMUs: `oas 40-bit`, a two-level stream table ("covers 25/32 bits of SID", so SID size
    32 and a linear table is out of the question), and wired interrupts ("msi_domain absent")
  - the interrupts are edge-triggered GIC SPIs
  - Linux's TLB invalidation policy on this board is strict
  - HTTU is present but overridden by firmware

## 4. The design in AbyssBSD

### 4.1 Crates and layering

| Crate | Contents | `unsafe` |
|---|---|---|
| `lib/acpi-tables` (`iort`) | Grows from the ITS walk to the whole table: SMMUv3, root complex, named component, and RMR nodes. Single mappings. Stream lookups (PCI RID → SMMU and stream, named-component path → SMMU and streams), the existing ITS lookup, and RMR iteration | forbidden |
| `lib/iommu` (new) | Pure, ISA-neutral logic: the IOVA allocator (`gas` semantics, §4.6); the SMMUv3 formats: STE, CD, commands, events, gerror, queue index and wrap arithmetic, IDR decoding; and a host **SMMUv3 model** that consumes a command queue, walks stream table → CD → page tables, and produces events, for tests (§7) | forbidden |
| `lib/pt` | A stage-1 SMMU `Format` beside the CPU's aarch64 one: the same VMSAv8-64 descriptors, with the CD's MAIR indexes, AF always set (no HTTU), and read/write permissions from the mapping's direction | forbidden |
| `sys/vm::dma` | `Backend::Iommu` and the `IoDomain` trait it maps through (§4.5) | yes (already) |
| `sys/bus` | The `Iommu` provider trait (§9.3's table gains its row). The broker's translation record per node, and `dma_tag` choosing the backend. `EdgeKind::Settle` (§4.3, in `lib/devtree`) | small (as now) |
| `sys/iommu` (planned in DRIVER-MODEL §3.1; built now) | The framework, FreeBSD's sys/dev/iommu plus arm64/iommu/iommu.c: units, domains, contexts, requester resolution, IOVA spaces, and **the I/O page tables** (`lib/pt` over pages from `DmaMemory`, cleaned when the unit is not coherent). It implements `vm::dma::IoDomain` | yes: it writes page tables the hardware walks, as `sys/vm` does |
| `drivers/smmu` (new) | The SMMUv3 hardware driver: registers (`lib/regs`), queues, stream table and CDs in `DmaCoherent` memory from its own direct tag, commands, sync, events, and errata. It implements the `sys/iommu` unit interface (map and unmap commands, STE install, TLBI) and publishes the provider | **forbidden**, as for every driver |
| `kernel/src/its.rs` | Composes MSI messages through the function's context (§4.8) | as now |

The split follows FreeBSD's: hardware in the driver, policy in the framework. It also follows the
project's rule that driver crates contain no `unsafe`. The one thing the driver cannot do safely
is write page tables that a device walks, and DRIVER-MODEL §8.4 already put that with the DMA
layer. GRAPHICS.md's `IoPageTable<F: PteFormat>` (device-walked tables for GPUs) can later share
the same `lib/pt` format code.

### 4.2 The device tree

- **Children.** The ACPI bus adds one `smmu` child per SMMUv3 node in IORT order, as
  `smmu_acpi_identify` does, after the namespace's devices (FreeBSD's identify children follow
  them; §14). They are `Ident::AcpiChild { class: "smmu", desc: "ARM System Memory Management
  Unit" }` (pnpinfo `unknown`, as `acpi_pnpinfo` says for no handle) with bus data from the IORT
  node: base, flags, model, GSIVs, and node offset. Resources: memory `0x20000` at the base, and four IRQs in
  FreeBSD's order (event, PRI, sync, gerror). Each IRQ is edge-triggered, active-high: ACPI carries
  no trigger for them, and FreeBSD forces this (`smmu_configure_intr`).
- **Placement.** They sit in `acpi0`'s child list where FreeBSD's order puts them (after the
  namespace's devices, `gic0` among the identify children before them, `generic_timer0` after, in
  the corpus; AbyssBSD has neither under `acpi0`). Units come from IORT order, so `smmu0` is
  `0xb010000`.
- **Attach.** Attach claims the memory and the event, sync, and gerror lines. The sync line is
  granted and left without a handler, as FreeBSD's is. That keeps `devinfo -rv` at three active
  interrupts. Probe accepts model 0 only, like FreeBSD (`BUS_PROBE_NOWILDCARD`, so only this child
  is matched). Messages:
  - `smmuN: <ARM System Memory Management Unit> on acpi0`. FreeBSD's line also lists the
    resources (`iomem … irq …`), which no AbyssBSD announce line does yet (§14)
  - `smmu1: 10 streams bypassed (reserved memory)` when RMR streams exist
  - under `bootverbose`, the features and each bypassed stream
- **Sysctls.**
  - `dev.smmu.N.dma` (read-only, whether this unit translates DMA)
  - `hw.iommu.check_free` (0; kept for parity)
  - `hw.iommu.relaxed_ordering` (tunable, default 0)
  - `hw.smmu.bypass_named` (tunable)
  - The tunables `hw.iommu.dma`, `hw.busdma.default`, and `hw.busdma.pciD.B.S.F` (§4.12)
- **`%iommu`.** FreeBSD's handler prints `rid=%#x` (from `iommu_get_requester`) for every PCI
  device whenever the kernel has `options IOMMU`, translated or not. Today AbyssBSD prints an empty
  string. Both reference kernels have the option (amd64 `GENERIC`, the board's
  `GENERIC-HMP-IOMMU`), so the requester walk (§4.4) fills it on both ISAs from stage 0. Before
  then it is a quiet G1 difference.
- Nothing becomes a child of an `smmu`. Translation is an attribute of a node, not a place in the
  tree.

### 4.3 Ordering: IORT edges and the `Settle` kind

The ACPI bus already builds the dependency graph from firmware. The IORT adds edges with
`Origin::Iort`, which `lib/devtree` already names:

- `smmuN` → each PCI host bridge (`PNP0A08`/`PNP0A03`) whose segment (`_SEG`, default 0) matches
  a root-complex node that maps to `smmuN`. PCI functions are the bridge's descendants, so tree
  edges carry the order down. A host bridge does no DMA itself, so holding it back is earlier than
  strictly needed. It is exact, though, and it is what FreeBSD's pass order does.
- `smmuN` → each named component the IORT maps to it, by namespace path.

These edges need a semantic the four kinds do not have. **Required** would strand the whole PCI
tree, and with it the root disk, if the SMMU failed to attach or no driver matched it. FreeBSD
then just runs untranslated. **Optional** and **Order** do not gate attach at all. So this adds a
fifth kind:

| Kind | Attach | Shutdown, suspend, resume |
|---|---|---|
| **Settle** | The consumer waits until the supplier has *settled*: attached, failed, or matched by no driver. Then it attaches whatever the outcome | Ordered while the supplier is attached, as Required |

Cycle breaking treats `Settle` as soft, like Optional, though no cycle through an IORT edge is
expected. The broker learns the outcome from the supplier's state. If the SMMU is attached and
translating, the consumer's tag is translated. Otherwise the consumer gets the backend it would
have had without an IOMMU, and the boot log says so once per unit. That keeps G3 ("ordering is
derived") and FreeBSD's failure behavior.

The ITS is set up before the bus (`its::init`) and is not in the graph. The SMMU's wired
interrupts go to the GIC, which is adopted early. So an `smmu` has no supplier of its own besides
`acpi0`.

### 4.4 Units, domains, and contexts

- **Unit.** An attached `smmu` publishes `sys/bus::Iommu`, keyed by its IORT node.
  `dev.smmu.N.dma` records whether it translates (§4.12).
- **Translation record.** As the ACPI bus records `_CCA` with `set_dma_constraints`, it records
  translation with a new `StartCx::set_dma_translation(consumer, Translation)`. Descendants inherit
  it. For a host bridge the record names the root-complex node and its SMMU. For a named component
  it names the SMMU and the stream IDs.
- **Requester.** For a PCI function, the requester and its RID come from FreeBSD's
  `iommu_get_requester` walk:
  - a PCIe function is its own requester;
  - a conventional function behind a PCIe-to-PCI bridge is seen as (secondary bus, 0, 0);
  - one behind a conventional bridge is seen as the bridge.

  The stream ID is the RID through the root-complex mapping (the identity on the board and in
  QEMU). Devices that share a requester share its context, as on FreeBSD. Phantom functions and
  DMA-alias quirks are deferred.
- **Domain policy: one domain per context**, as FreeBSD's arm64 `iommu_get_ctx`. Each domain has
  its own ASID, CD, page tables, and IOVA space. Linux groups a root port with its device (no ACS),
  but FreeBSD does not, and the device tree and isolation are FreeBSD's. Groups (DRIVER-MODEL §14)
  come with passthrough.
- **Context lifetime.** A context is created on its node's first need (`dma_tag` or MSI
  allocation, whichever comes first, as `iommu_get_dev_ctx` does). It is reference-counted:
  tags, coherent allocations, pools, and the MSI window each hold a reference. It belongs to the
  device node, not the binding, so a detach and re-attach keep it. It goes when the node is
  deleted and its last reference drops: the STE is cleared, then the domain's reservations and
  tables are freed.
- **Disabled requesters.** `hw.busdma.default=bounce` or `hw.busdma.pciD.B.S.F=bounce` gives the
  requester a pass-through context (STE bypass) and the backend it would have had untranslated.
  This is FreeBSD's `IOMMU_CTX_DISABLED`. The same path serves virtio devices that do not offer
  `VIRTIO_F_ACCESS_PLATFORM`. By the virtio specification such a device uses physical addresses
  whatever the platform does, and QEMU's do bypass its vIOMMU. The transport therefore asks the
  broker for a pass-through context when the feature is not negotiated (it already accepts the
  feature when offered, `sys/virtio/src/transport.rs`).
- **Relaxed ordering.** When a translated (not pass-through) PCIe function gets its context, its
  Device Control `Enable Relaxed Ordering` bit is cleared, in the saved configuration too, so a
  reset or rebind keeps it cleared. If the bit was set, the device prints
  `<dev>: relaxed ordering disabled (IOMMU translation)`. `hw.iommu.relaxed_ordering=1` keeps RO
  on. This is the board port's rule (§2.3); upstream FreeBSD lacks it, but the corpus has the
  lines.

### 4.5 The `Iommu` backend of `vm::dma`

DRIVER-MODEL §8.4 already lists it. Concretely:

```rust
/// A translation domain, as vm::dma sees it (implemented by sys/iommu).
pub trait IoDomain: Send + Sync {
    /// Maps physical ranges at one IOVA range for `dir`, below `limit`, with `align` and
    /// `boundary`; returns the bus segments. Never sleeps.
    fn map(&self, ranges: &[PhysRange], dir: Access, c: &DmaConstraints) -> Result<IoMapping, DmaError>;
    /// Unmaps, invalidates, and waits for the invalidation, then frees the IOVA range.
    fn unmap(&self, m: IoMapping);
    /// The bus address at which the MSI doorbell page `pa` is mapped in this domain (§4.8).
    fn doorbell(&self, pa: u64) -> Result<u64, DmaError>;
}
```

- **The tag.** `DmaTag` gains `Backend::Iommu` and a reference to its context's domain (an `Arc`),
  so it becomes `Clone` rather than `Copy`. Its constraints apply to **bus** addresses. `addr_bits`
  bounds the IOVA, not the physical page, so a translated tag never bounces for reach. Memory
  comes from anywhere in RAM, the high region included: that is the fix.
- **`map`.** The buffer's physical pages are found as now. One IOVA range covers the buffer, page
  aligned, at the buffer's page offset, so a physically scattered buffer becomes one bus segment
  when the constraints allow it, as FreeBSD's `gas` load gives. Cache syncs for non-coherent
  devices run as they do for the other backends. The tag already does them by its coherency
  (the sibling's commit `0dd7d31e2e6` is structural here). The mapping record holds the domain
  and the IOVA range in the boxed spill record. Mappings stay under the 64-byte move budget,
  since a translated mapping is never the one-segment inline case.
- **`unmap` is strict.** The PTEs are cleared, the TLB entries invalidated, and the sync awaited
  before `Mapped::unmap` returns the buffer. That is rule 3 of DRIVER-MODEL §8.4, and it is
  FreeBSD's arm64 behavior (`smmu_unmap` syncs). Linux on this board is strict too. Batching is
  within one unmap: one `CMD_SYNC` for all of a mapping's pages. Deferred (lazy) invalidation is
  not done: it would hand the CPU a page the device can still reach. DRIVER-MODEL's phrase
  "batched and deferred" means this batching, and §13 question 4 asks to reword it.
- **`DmaCoherent`** is mapped once at allocation and unmapped in `Drop`. **`DmaPool`** buffers are
  mapped once for the pool's life, so drivers whose steady state uses pools (receive buffers, PRP
  lists) do no IOMMU work per I/O.
- **Context.** `map` and `unmap` never sleep. They run in submission paths and in completion
  filters (`IrqCx`). IOVA space and page-table pages are taken under spin locks from the physical
  allocator without sleeping. Running out is `DmaError::NoMemory`, which drivers already handle
  for the bounce backend. FreeBSD's delayed-load taskqueue is not needed.
- **Rule 2** (DRIVER-MODEL §8.4: a mapping covers whole pages holding nothing else; smaller
  buffers are bounced) is where this design is unsure. FreeBSD maps the page and exposes its
  neighbors. Bouncing every buffer smaller than a page would copy every 2 KiB mbuf cluster.
  §13 question 5; decision 5 settled it, as §16 note 5 builds it.

### 4.6 IOVA allocation

`lib/iommu::gas` follows `iommu_gas.c`'s behavior:

- address-ordered **first fit**, lowest addresses first, under the tag's limit;
- a free-gap augmentation per node, so a search is logarithmic;
- **a guard page on each side** of every allocation, so IOVA 0 is never handed out (a device that
  DMAs to bus address 0, a zeroed descriptor, faults instead of hitting a live buffer) and an
  overrun faults at the next page;
- alignment and boundary per request;
- **reserved regions** that are never allocated, and **identity regions** mapped at their own
  address (RMR, §4.9). Reserving from 0 works (sibling `1464aadaf77`), and freeing a domain
  releases both kinds (`cf75a5f809b`).

The domain's space is 48 bits (FreeBSD's `MAXADDR_48BIT`, 4 KiB granule, four levels). A PCI
domain reserves its host bridge's `_CRS` memory windows at their bus addresses (sibling
`01b65ffd609`), which `sys/pci` already parses. Lowest-first keeps real IOVAs small: a domain
never maps more than RAM, so its addresses stay far below 32 GiB. That matters on a root complex
whose own reach is unknown (memory address size limit 0).

Stage 1 notes (where `lib/iommu::gas` decides what FreeBSD leaves open):
- The tree is an AVL tree in an arena of nodes, with FreeBSD's `free_down` augmentation. It grows
  with `try_reserve` and recycles freed nodes, so running out is an error, not an abort, and
  `reserve_nodes` grows it ahead of a path that must not allocate.
- A boundary must be at least a page. With a smaller one, FreeBSD's `iommu_gas_match_one` could
  place an entry off a page boundary.
- An entry may end at the top of the space. FreeBSD refuses one there (`EINVAL`), which only
  mattered for a reservation that runs to the end.
- An identity region is clipped where it overlaps identity regions at its ends, as FreeBSD's RMRR
  path does. One overlapping an identity region strictly inside it is `Busy`, where FreeBSD would
  corrupt its tree (its `XXXKIB` case).
- `lib/pt`'s `map` refuses one range that crosses bit 47, the CPU formats' half boundary, though
  the SMMU's TTB0 covers all 48 bits. Lowest-first allocation does not get near it. If it ever
  does, stage 3 maps such a range in two calls.

### 4.7 The hardware

- **Probe of features.** IDR0–IDR5 and IIDR: stage-1 support and AArch64 tables, the stream-table
  forms, ASID16, COHACC, MSI, SEV, OAS, the queue sizes, and SID bits. A missing required feature
  is a logged attach with translation off, not a failed attach (§4.3). Errata: MMU-600 below
  variant 1 loses SEV (Arm erratum 1076982), as upstream does. Coherency comes from IORT COHACC
  override or IDR0.COHACC. The board's SMMUs are coherent.
- **Memory.** The stream table, CDs, and queues are `DmaCoherent` from the driver's own direct tag:
  the SMMU's own accesses are not translated. With a coherent SMMU they are cacheable (`*BASE`
  RA/WA, inner-shareable), and nothing is cleaned. Otherwise every write is cleaned, as
  `DmaCoherent` already does for a non-coherent device.
- **Stream table.** Linear when it fits, two-level otherwise (the board's 32-bit SID space needs
  two levels), with level-1 descriptors and level-2 spans allocated on demand for SIDs in use. STEs
  are written invalid, then complete, then `V`, with `CFGI_STE` and a sync after each change.
- **Initial state.** Every STE is invalid (abort), except the RMR streams (bypass, §4.9) and
  pass-through contexts. The SMMU is disabled and the `GBPA` that firmware left is recorded, then
  the queues and table are programmed, the caches invalidated (`CFGI_ALL`, `TLBI_NSNH_ALL`), and
  translation enabled. A device given no context cannot DMA at all. Abort, not bypass, is the
  default for a stream nobody claimed (confinement).
- **Contexts.** Stage 1 only. The STE has `S1ContextPtr` to a single CD (no substreams) and stage 2
  bypassed. The CD gets the domain's ASID, TTB0 with a 48-bit T0SZ, a 4 KiB granule, and IPS from
  OAS. On a coherent SMMU the table walk is write-back inner-shareable (sibling `67d6727be10`).
  TTB1 is disabled (EPD1). The MAIR has entries for Normal write-back, Normal non-cacheable, and
  Device-nGnRE.
- **Mapping attributes.** A coherent domain maps Normal write-back inner-shareable, a
  non-coherent one Normal non-cacheable outer-shareable (sibling `2427574f670`; FreeBSD upstream
  maps Device-nGnRnE, which cost the board's NPU a factor of ten). The doorbell page is
  Device-nGnRE, write-only. Permissions follow the mapping's direction: `ToDevice` is read-only to
  the device, `FromDevice` write-only. AF is always set, since HTTU is not used.
  - Stage 1 note: stage 1 descriptors cannot express write-only, since AP grants read-write or
    read-only. So `FromDevice` mappings and the doorbell are read-write, as FreeBSD's busdma maps
    them, and only `ToDevice` is narrowed, to read-only (`lib/pt::smmu`).
  - Every leaf has AP[1] set, since PCIe requests are unprivileged. Every leaf is also
    execute-never and not global (tagged with the domain's ASID), as FreeBSD's `iommu_pmap` makes
    them.
  - STEs leave EATS at 0 (§9). FreeBSD writes full ATS.
- **Commands.** One queue under a spin lock, as FreeBSD. An entry is written, a store barrier
  orders it before the `PROD` update, and `PROD` publishes it. A sync's slot is chosen, written,
  and published under the lock. The wait is a spin on that slot, never a sleep (sibling
  `1b89d7594db`). Sync completion is an MSI write to memory on a coherent SMMU with IDR0.MSI,
  otherwise polling of `CONS` (upstream `50e01b6351b`). Map issues `TLBI_NH_VA` per page and one
  sync, as FreeBSD does. Whether a map needs any invalidation is a stage-7 question (§8). Unmap
  issues `TLBI_NH_VA` per page, or `TLBI_NH_ASID` past a threshold, and one sync. Empty
  intermediate tables are kept until the domain is freed, which avoids walk-cache invalidation of
  table entries.
- **Interrupts.** The event and gerror lines get filters that acknowledge and schedule the
  thread. The thread drains the event queue (§4.10). PRI is not enabled. The SMMU's own MSIs are
  not used (wired, as on FreeBSD and Linux).

### 4.8 MSI through the SMMU

A translated function's MSI write goes through its domain like any DMA. Composing the message
therefore takes one more step, FreeBSD's `iommu_map_msi` and `iommu_translate_msi`:

1. `its.rs` composes the message as now: `GITS_TRANSLATER`'s physical address, the event, and the
   LPI. The device ID is the RID through the IORT, SMMU hop included, which `its_for_pci` already
   walks.
2. The MSI path asks the broker for the function's context (creating it if MSI comes first). If
   translated, it calls `IoDomain::doorbell(page of GITS_TRANSLATER)`. That maps the page once per
   domain (Device-nGnRE, write-only, an IOVA from `gas`) and returns its bus address. The message
   address becomes that plus the offset in the page.
3. The window goes with the domain. A pass-through context uses the physical address as now.

`pci::msi::MsiPlatform::compose` takes the translation as a parameter (or the PCI layer applies it
after compose). The choice is left to stage 4. Either way `sys/pci` learns nothing about SMMUs.

### 4.9 Reserved memory and named components

- **RMR (P9).** Before enabling a unit, each RMR stream on it gets a bypass STE, so the display
  controllers keep scanning out the firmware framebuffer, which AbyssBSD's `vt` console draws
  into. The unit then prints FreeBSD's count line. An RMR without "remapping permitted" keeps its
  bypass even when the device gets a driver.
- **Named components without a driver (P9).** `hw.smmu.bypass_named` decides their streams:
  - 0 leaves them invalid, so their DMA aborts. This is the board's FreeBSD configuration, under
    which the console and boot ran with every non-RMR display stream aborted until `komeda`
    loaded.
  - 1 bypasses them, as the sibling's compiled-in default does.

  AbyssBSD defaults to 0 (§13 question 2).
- **Named components with a driver (M-G).** They get contexts with all their stream IDs (the
  sibling's `446348afb11`). The coherency comes from `_CCA` (non-coherent unless `_CCA` says
  otherwise, as the sibling decides). Their remappable RMR ranges are identity-mapped in the
  domain before the streams switch from bypass to translated. On free, the streams go back to
  bypass. This needs the panthor, Linlon, and NPU drivers to exist. GRAPHICS.md's `BusAddr` and
  device-walked tables build on the same domains.

### 4.10 Faults and errors

- **Events** (translation, permission, and access faults, `C_BAD_STE`, `C_BAD_CD`, and the rest)
  are drained in the event thread. Each is decoded (`lib/iommu`) and logged with the device it
  belongs to (by stream), the IOVA, and the access: `smmu0: nvme0 (rid 0xc100): translation fault
  at 0x1234000 (write)`. A per-stream counter is exported, and logging is rate-limited per
  stream.
- **Storms.** Linux has seen an event-queue interrupt storm on this board. Rate-limited logging,
  draining in a thread, and the IRQ layer's storm detection (DRIVER-MODEL §8.3) bound the cost.
  Queue overflow (`Q_OVF`) is counted and logged once.
- **Quirks.** FreeBSD silences RTL8168 translation faults at IOVA 0 (`smmu_quirks.c`). The table
  is carried as data. Whether the RTL8126 (`rge`) needs an entry is found on the board, not
  assumed.
- **Gerror.** Gerror is logged and acknowledged (`GERRORN`). A `CMDQ_ERR` skips the bad command
  (`CMD_SYNC` in its place), logs it, and lets the queue go on.
- Faulting devices are not reset by the SMMU driver. The device's own driver sees its I/O fail or
  time out, as on FreeBSD.

### 4.11 Shutdown, warm reset, and kexec

The graph quiesces every consumer before its SMMU (DRIVER-MODEL §6.3). The SMMU's `quiesce` then
disables translation and restores the `GBPA` that firmware left. Whatever boots next, the firmware
with its UEFI NVMe driver or the next AbyssBSD, finds the SMMU as it was found. At attach, a unit
found enabled (a warm reset that did not reset it, or a panic) is disabled before it is
programmed, with FreeBSD's warning. A panic does not quiesce. Whether PSCI `SYSTEM_RESET` resets
the SMMUs on the Sky1 is checked on the board (§12).

### 4.12 Knobs

| Name | Default | Meaning |
|---|---|---|
| `hw.iommu.dma` | **1** on arm64 when an SMMU attaches (FreeBSD: 0) | DMA translation. With 0, units attach, report `dev.smmu.N.dma: 0`, and leave the hardware as firmware left it (the sibling's rule), and devices get untranslated tags |
| `hw.smmu.bypass_named` | 0 (the sibling's compiled default: 1; the board runs 0) | Named components without a driver: abort (0) or bypass (1) |
| `hw.iommu.relaxed_ordering` | 0 | Keep PCIe relaxed ordering on translated functions |
| `hw.busdma.default`, `hw.busdma.pciD.B.S.F` | unset | `bounce` gives a pass-through context; `iommu` translates |
| `hw.iommu.check_free` | 0 | Kept for parity (FreeBSD's `gas` self-check under `INVARIANTS`) |

The `hw.iommu.dma` default differs from FreeBSD's. Translation is what makes the board's DMA
correct, and a board setting would be a driver bug (CLAUDE.md, "nothing special"). It goes in
compat-deviations with that reason (§13 question 2).

## 5. Interaction with existing code

- **`AttachCx::dma_tag`** (`sys/bus/src/driver.rs`) intersects the node's constraints as now,
  then reads the node's translation record. If the unit is attached and translating, and the
  requester is not disabled, it gets the context and returns an `Iommu` tag. Otherwise the tag is
  direct or bounce, as today.
- **`kernel/src/its.rs`** keeps its direct tags: the ITS's own tables are not behind an SMMU.
  `compose` gains §4.8's step.
- **`sys/acpi/src/bus.rs`** adds the `smmu` children and the IORT edges and records. `deps.rs`'s
  corpus graph tests learn the board's IORT edges.
- **`kernel/src/mm`**: `hw.physmem` stays as a FreeBSD tunable. The board stops needing it.
- **Drivers** do not change. The relaxed-ordering message is printed by the framework under the
  device's name.

## 6. Testing in QEMU

QEMU's `virt` machine builds the IORT with an SMMUv3 in front of its PCIe root complex when given
`-machine virt,gic-version=3,acpi=on,iommu=smmuv3` (QEMU 10.2 locally). That is exactly the
board's PCIe shape, minus named components and RMRs.

- **`xtask/src/qemu.rs`** gains an `iommu` option. It appends `,iommu=smmuv3` to the aarch64
  `-machine` string (both the EL1 and EL2 forms). It also adds `iommu_platform=on` to every
  `virtio-*-pci` device, without which QEMU's virtio devices bypass the vIOMMU, and
  `disable-legacy=on`, since QEMU refuses `iommu_platform` on a transitional device. `nvme` needs
  nothing, since QEMU's NVMe always honors the IOMMU. `cargo xtask run --arch aarch64 --iommu` for
  interactive work. `-trace 'smmuv3_*'` is the debugging aid.
- **A boot case `aarch64-smmu`**: NVMe as the disk, virtio-net, virtio-rng, and the input devices,
  all translated, with MSI-X. A second case runs with `hw.pci.enable_msix=0` (INTx under
  translation); it could be the existing `aarch64-el2` case with the IOMMU added, rather than a new
  one. The plain `aarch64` case stays untranslated, so the direct path stays tested.
- **ktests under translation** (in the `aarch64-smmu` case):
  - storage and network transfers verified against their data;
  - a virtio device without `iommu_platform` gets a pass-through context and still works;
  - a deliberate fault: a request whose segment points at an unmapped IOVA must produce one
    translation-fault event naming the right stream and address, fail that request, and corrupt
    nothing;
  - unmap before reuse: after `Mapped::unmap`, the device's access to the old IOVA faults;
  - `devinfo` shows `smmu0` with QEMU's resources, and `%iommu` reads `rid=`.
- The x86 cases are unchanged (no IOMMU). Per the project's practice, a stage runs its own cases
  (`--case`), and the full tiers run at the phase's exit.

## 7. Host testing

- `lib/acpi-tables::iort`, against `tests/corpus/acpi/opi6p/iort.dat` and a captured QEMU
  `iommu=smmuv3` IORT (added to the corpus in stage 0):
  - the node counts;
  - both SMMUs' bases, flags, and GSIVs;
  - RID → (SMMU, stream) and → ITS device ID;
  - every named component's stream list;
  - the RMR's range, attributes, and ten streams;
  - malformed tables, looping references, and truncated nodes fail cleanly.
- `lib/iommu::gas`, against a model (a sorted interval list) under randomized
  allocate/free/reserve sequences:
  - lowest-first;
  - guard pages;
  - alignment and boundary;
  - limits;
  - reserve from 0;
  - identity regions;
  - free-on-fini.
- `lib/iommu` formats: STE, CD, every command, every event against the specification's field
  tables, and queue wrap arithmetic at the boundaries.
- **The SMMU model**: a host implementation of the parts of IHI 0070 the driver uses. Registers,
  command consumption (CFGI, TLBI, SYNC with both completion forms), two-level stream table and CD
  lookup, a stage-1 walk with the `lib/pt` SMMU format, a TLB that honors invalidation (so a
  missing TLBI is caught as a stale translation), and event production.
  - `drivers/smmu` runs against it through `lib/regs`'s test backing, as other drivers' host tests
    do.
  - Stage 1 notes on `lib/iommu::model`:
    - It owns the memory, a `PhysMemory`. `SparseMemory` is also a `pt::Memory`, so tables built
      with `pt::PageTable<Smmu>` are the ones it walks. Its registers are offsets
      (`read32`/`write32`/`read64`/`write64`), so the driver's register trait wraps it.
    - It caches level-1 descriptors, STEs, and CDs per StreamID until a CFGI. It caches leaves per
      ASID, plus intermediate walks, until a TLBI (a leaf TLBI keeps the walks). CFGI_STE does not
      drop the CD.
    - Two knobs cover what IHI 0070 leaves open. `cache_invalid_config` (default on) also caches
      invalid structures, so a driver must invalidate after making one valid. `cache_faults`
      (default off: the Arm rule is that faulting walks are not cached) lets stage 6 test its "no
      TLBI on map" candidate under either reading.
    - Commands it does not implement (ATS, PRI, stall, stage 2) stop the queue as illegal and are
      logged as violations. So are writes the architecture forbids, such as a base register
      while enabled.
  - `sys/iommu` and `vm::dma`'s `Iommu` backend run against it for map, unmap, coherent, pool,
    doorbell, and the strict-unmap property.
- `lib/devtree`: `Settle` edges. A failed, unmatched, or attached supplier releases its consumers.
  Shutdown order.

## 8. Implementation plan

Each stage is one or more commits that build on both ISAs. A stage's tests are the targeted ones
listed. Nothing touches the board before stage 5.

| Stage | Work | Tests |
|---|---|---|
| **0. Tables and tree** | The IORT parser grows (§4.1). The `smmu` children of `acpi0` with FreeBSD's resources and order. A minimal `drivers/smmu` that probes, maps registers, reads the IDRs, prints FreeBSD's line, and leaves the hardware alone. `EdgeKind::Settle` and the IORT edges and records. `%iommu` as `rid=` on both ISAs (the requester walk). The `hw.iommu.*`, `hw.smmu.*` sysctls | IORT host tests on both corpora. `devtree` tests. `deps.rs` corpus graph tests (the board's edges). QEMU `aarch64-smmu` boots untranslated with `smmu0` in `devinfo`. The x86 boot case's `%iommu` strings against the bhyve corpus |
| **1. Pure libraries** | `lib/iommu`: `gas`, the SMMUv3 formats, the model. The `lib/pt` SMMU format | Host tests (§7) |
| **2. The SMMU running** | Queues, two-level and linear stream tables, sync (both forms), events and gerror, errata, the initial all-abort state with RMR bypass, quiesce and restore. Every PCI context pass-through for now. `hw.iommu.dma=0` | Model tests. QEMU `aarch64-smmu`: boots with the SMMU enabled and every stream bypassed. A ktest sets one idle device's stream to abort, provokes its DMA, and checks the logged `C_BAD_STE` event. The warm-reset case with the SMMU on |
| **3. Translated DMA** | `sys/iommu` (units, domains, contexts, page tables), `vm::dma`'s `Iommu` backend, the broker's selection, host-bridge window reservations, relaxed ordering, the virtio pass-through rule. `DmaTag` becomes `Clone` | `vm::dma` host tests against the model. QEMU `aarch64-smmu` with `hw.pci.enable_msix=0`: storage and network ktests, the deliberate fault, unmap-before-reuse |
| **4. MSI** | The doorbell window and message translation (§4.8) | QEMU `aarch64-smmu` with MSI-X (the default). The multi-user scenario with `--iommu` once |
| **5. The board** | Deploy with translation on and `hw.physmem` still set. Then without it | Boot. `devinfo -rv` and `sysctl dev.smmu hw.iommu hw.smmu` against the corpus. No events. The high-memory write test that wedged it (512 MiB, then 2 GiB writes, several rounds). fsx with `fsck_ffs` on the NVMe. A USB disk with all RAM in use (§12). `rge` under the P7 suite's load. Warm reboots through `efibootmgr -n`. Then a day of looped scenarios with the watchdog armed (Phase 9's exit). `hw.physmem=29G` comes out of the board's `loader.conf` and the roadmap |
| **6. Performance** | Measure P5's storage suite and P7's network suite on the board against the FreeBSD baseline (§13 question 3). Audit first: the per-I/O path against FreeBSD's (map, unmap, syncs, lock hold times). Only then consider the candidates: no TLBI on map (if IHI 0070 forbids caching invalid entries), range or ASID invalidation, block mappings for large coherent allocations, a three-level 39-bit space, and per-queue command batching | `cargo xtask perf --platform opi6p`, suite geometric means within 2% |
| **M-G (later)** | Named-component contexts, RMR identity maps, non-coherent domains (§4.9) | With the first display or NPU driver |

## 9. Deferred

- **x86 DMAR and AMD-Vi**, and **interrupt remapping**: amd64 `GENERIC` leaves them off by default
  (§2.2), so P10's X870E runs untranslated as FreeBSD does there. The framework (`lib/iommu::gas`,
  `sys/iommu`, `IoDomain`) is ISA-neutral, so they plug in later as units. ROADMAP's P11 IOMMU
  bullet now means them.
- **SMMUv2** (the SC8280XP's, Tier 2), and the Qualcomm apps-SMMU arrangement the sibling has.
- **Stage 2, nesting, bhyve passthrough (`ppt`), IOMMU groups.** The IORT root complex advertises
  ATS, but **ATS and PRI** are off (STE `EATS` 0, no PRI queue), as on FreeBSD. So are **PASIDs**
  and substreams, **HTTU**, and the SMMU's own MSIs.
- **Hotplug** and devices behind an `ExternalFacingPort` (DRIVER-MODEL §14). With an IOMMU,
  their tags are translated from the first DMA. Hotplug itself stays P11.
- **PMCG** (SMMU performance counters): no such node on the board.

## 10. Performance

Translation costs a page-table update, a TLB invalidation, and a sync per map and unmap. It also
costs IOTLB misses during transfers. Under PERFORMANCE.md's gate, the components P9 measures on
the board (storage over NVMe, network over `rge`) must stay within 2% of FreeBSD. The comparison is
fair only if FreeBSD translates too, that is, the board's `GENERIC-HMP-IOMMU` with
`hw.iommu.dma=1`. Against stock `GENERIC`, which does not translate (and which has the same
high-memory exposure), the IOMMU's cost would count against AbyssBSD (§13 question 3).

What keeps the cost down by design:
- pools for steady-state buffers;
- one sync per unmap, not per page;
- per-device domains, so there is no cross-device lock beyond each unit's command queue;
- spinning, not sleeping, for syncs.

What stage 6 may add is listed there, each only after the audit (the project's rule: map the path
against FreeBSD's design before tuning).

## 11. Hardware notes for stage 5

- The Samsung 990 PRO (`nda1`) is AbyssBSD's disk; FreeBSD stays the default boot. Booting AbyssBSD
  stays `Boot0007` by `BootNext`. Nothing here changes that.
- If the SMMU leaves the board unbootable, FreeBSD is still the default boot. Removing
  `hw.physmem` is a separate step after translation has run clean with it.
- Read before experimenting: the board notes in `../AbyssBSD/docs/boards/orangepi-6-plus/` (the
  IOMMU row, the branch notes) and HARDWARE.md §4. HARDWARE.md's storage row lost its "SMMUv3 +
  IORT RMRs come in P11" at stage 0 (ADR-0022), and says the board translates when stage 5
  lands.

## 12. Risks

- **The premise.** Translation fixes the board's DMA because IOVAs stay low and the SMMU's 40-bit
  output reaches the high region. FreeBSD's behavior on the board supports this, but it is not
  proven for AbyssBSD until stage 5's write test passes with `hw.physmem` removed.
- **Untranslated masters.** xHCI ×10, HDA, the DMA-350, and audio are not in the IORT, so they keep
  physical addresses. If any of them shares the PCIe path's problem with the high region, the SMMU
  does not help, and the RAM there needs another answer. FreeBSD uses all RAM with these devices
  working, which argues against it. Stage 5 checks a USB disk with all RAM in use before
  `hw.physmem` comes off.
- **The event-queue storm** Linux saw on this board (§4.10).
- **Warm reset with the SMMU enabled.** If PSCI reset leaves the SMMU on, firmware's UEFI drivers
  would DMA through stale tables. The quiesce in §4.11 covers clean reboots, not panics.
- **Command-queue contention.** One lock per unit, held across a spin for sync completion, on every
  map and unmap from every CPU. FreeBSD has the same shape; if the board's NVMe suite shows it,
  stage 6 deals with it.
- **Relaxed ordering and unmap timing** (the sibling's lost-data case) bite any driver that unmaps
  on completion. Clearing RO is the fix, and the board's NVMe fsx is the check.
- **Spec subtleties** the model may encode wrongly: ordering rules, what the SMMU may cache, and
  two-level table updates. The model is written from IHI 0070, and QEMU's implementation is a
  second, independent reading.

## 13. Decisions (the user, 2026-11-10)

The questions this design raised, settled before stage 0:

1. **An ADR:** yes, ADR-0022, written at stage 0, which also amends DRIVER-MODEL §3.1 and §16,
   ROADMAP's P11 IOMMU bullet, and HARDWARE.md's IOMMU rows (the SMMUv3 is P9's now).
2. **Defaults:** `hw.iommu.dma=1` on arm64 when an SMMU attaches, and `hw.smmu.bypass_named=0`,
   as the board's working FreeBSD runs; both recorded in compat-deviations.
3. **Performance baseline on the board:** FreeBSD `GENERIC-HMP-IOMMU`, translating (like for
   like).
4. **Invalidation:** strict per unmap, batched within it (§4.5); DRIVER-MODEL §8.4's "batched and
   deferred" is reworded to match.
5. **Rule 2:** bounce only buffers from the general heap; map payload-only zones (mbuf clusters,
   buffer-cache pages) in place.
6. **`smmu1`:** enabled in P9 as FreeBSD does: RMR streams bypassed, the rest aborted.
7. **The edge:** `Settle`, a fifth `EdgeKind`.

## 14. Stage 0 as built (2026-11-10)

Stage 0 (§8) built what §8's first row lists. Where this design was silent or wrong, it made the
smallest decision that follows FreeBSD:

1. **Placement.** §4.2 said the `smmu` children come before the namespace walk; the corpus
   (`devinfo-rv.txt`) has them after the namespace's devices, with FreeBSD's other identify
   children (`psci0`, `gic0`, `generic_timer0`). They are added after the namespace's devices.
   Because each host bridge waits for its SMMU to settle, `smmu0` announces before `pcib0`, as on
   FreeBSD, where it attaches in an earlier pass.
2. **Identity.** `Ident::Child` gives an empty pnpinfo; FreeBSD's `acpi0` says `unknown` for a child
   without a handle. A new `Ident::AcpiChild` does. (`acpi_timer` on x86 is such a child too and
   still shows an empty pnpinfo; that is outside this work.)
3. **The announce line** is the framework's, `smmu0: <ARM System Memory Management Unit> on
   acpi0`, without FreeBSD's `iomem … irq …`: no `acpi0` child's line lists resources yet, which
   goes with the resource managers (compat-deviations, "Resource managers").
4. **Interrupts.** The four IRQ resources are added only if all four GSIVs route (an SMMU that
   signals by MSI has none); the driver then fails to claim them, and its consumers go on
   untranslated, as FreeBSD's attach fails.
5. **Single mappings.** The old ITS walk skipped single mappings; lookups now follow FreeBSD's
   `iort_entry_lookup` (a single mapping outputs its base for every ID it covers), and pass over
   an SMMUv3's own MSI mapping only when the SMMU uses MSIs.
6. **The settle edge in the manager.** A node is *settled* when it is attached, failed, gone,
   disabled, or has no driver after matching; a failed attach, a nomatch, and a disabling hint now
   re-examine the node's consumers, as an attach always did. A detach cascades to settle
   consumers as to required ones (§4.3's "ordered while the supplier is attached"), and the
   `smmu` driver refuses a voluntary detach (`EBUSY`), so `devctl detach smmu0` cannot take the
   PCI tree with it.
7. **The requester walk** is FreeBSD's, but carried down: each PCI bus records what the walk does
   above it (`pci::requester::Upstream`), so a function's RID needs only its own bus's record. The
   PCI bus records it on the node (`StartCx::set_requester_id`), and `%iommu` is fixed at attach
   (FreeBSD computes it at read; the tree above an attached device does not change). Only the RID
   is recorded, not the requester device: devices sharing a requester share its RID, so the
   stream, and stage 3 can key contexts by (unit, stream).
8. **The translation record** (`bus::iommu::Translation`) holds, for a host bridge, the root
   complexes' mappings that go to the unit (QEMU maps only buses 0 and 1 through its SMMU, the
   rest straight to the ITS); a RID no mapping covers has no stream and is untranslated. A device
   two SMMUs claim keeps the first.
9. **Crates.** `lib/iommu` (package `iommu`) starts with the IDR decoding stage 1 would have put
   there; `sys/iommu` (package `iommu-framework`) starts with the knobs; `drivers/smmu` is in
   aarch64's driver table only (`files.arm64`'s `optional iommu`).
10. **Knobs.** `hw.iommu.dma` is a tunable only (FreeBSD has no such sysctl): 1 by default on
    arm64. `hw.iommu.relaxed_ordering` (`RDTUN`), `hw.iommu.check_free` (`RWTUN`), and
    `hw.smmu.bypass_named` (`RDTUN`) exist on arm64, as on the board's FreeBSD; amd64 has no
    `hw.iommu` until DMAR (compat-deviations). `dev.smmu.N.dma` reads 0 until stage 3, since
    nothing is translated. With `hw.iommu.dma=0` the driver prints the board port's
    `left disabled: DMA translation is off (hw.iommu.dma)`.
11. **The `aarch64-smmu` boot case** keeps virtio-blk as its boot disk for now (the case's UFS
    tests already run an NVMe controller behind the SMMU); §6's NVMe boot disk comes with stage 3,
    when translation makes the difference.

For stage 1: QEMU's SMMUv3 identifies as `IDR0 0xd44101b IDR1 0x2730010 IDR3 0x1414 IDR5 0x74`
(two-level stream tables, 16-bit stream IDs, terminate model, coherent, 44-bit output, no MSI),
which the `bootverbose` attach prints and `lib/iommu`'s tests use.


## 15. Stage 2 as built (2026-11-10)

Stage 2 (§8) runs the SMMU: `drivers/smmu` over a register and memory trait (`smmu::hw::Hw`),
host-tested against `iommu::model` (`drivers/smmu/src/tests.rs`), with the kernel's
implementation over the granted registers and `DmaCoherent` memory. Where this design was silent
or wrong, it made these decisions:

1. **Contexts come from the broker, at first DMA.** `bus::iommu::Iommu` is the unit's provider
   (`StartCx::provide_iommu`), with one method so far, `pass_through(stream)`.
   `AttachCx::dma_tag` and the PCI MSI and MSI-X setup (`AttachCx::iommu_context`) ask for the
   device's context, as FreeBSD's `iommu_get_dev_ctx` runs at the first tag. The stream comes
   from the node's translation record and the nearest requester ID up the tree, so a virtio
   device's is its transport function's and a USB device's its controller's. "Every PCI context
   pass-through" therefore means every function whose driver asked for DMA or MSI: each gets a
   bypass STE. A function no driver takes (QEMU's `edu`) keeps an invalid STE and its DMA
   aborts, as on FreeBSD with `hw.iommu.dma=1`. Stage 3 makes the same call return translated
   contexts.
2. **Level-2 tables on demand.** As FreeBSD's `smmu_init_l1_entry`, a level-2 table is made when
   a stream of its span first gets an STE. A stream in a span without one is
   `C_BAD_STREAMID`, not `C_BAD_STE` (IHI 0070, and QEMU): both abort with an event. The new
   level-1 descriptor is invalidated with the STE that made it (`CMD_CFGI_STE`, not leaf).
3. **Geometry** is FreeBSD's: two levels whenever offered and the SMMU has more StreamID bits
   than `STRTAB_SPLIT` (8), so QEMU's 16-bit space is two-level as on FreeBSD (the model tests
   the linear form); the level-1 table is capped at 2^17 descriptors (`STRTAB_L1_SZ_SHIFT`,
   25 StreamID bits on the board, as its Linux log says). A linear table is capped at 2^16
   STEs where FreeBSD would size it for every StreamID (`ste::Geometry::for_smmu`).
4. **Queue sizes** are capped at 2^12 commands and 2^11 events (64 KiB each). FreeBSD takes
   IDR1's maximum, 2^19 on QEMU and the board: 24 MiB of contiguous memory per unit
   (compat-deviations).
5. **Sync.** MSI completion (data 0 to the sync's own slot) on a coherent SMMU with IDR0.MSI;
   otherwise CONS is polled, with `CS=SIG_SEV` only when the SMMU has SEV after errata (FreeBSD
   sends SEV whenever it does not use MSI). The wait spins (`spin_loop`), not WFE: the kernel
   runs no event stream, so a WFE might not wake before the timeout. The unit's spin lock is held
   from choosing the slot to the completion (§12's shape), which removes the board port's need
   for a copy of the indices and the wrap race of an MSI slot. One second without completion is
   FreeBSD's `Failed to sync` and an error to the caller.
6. **Reset** is FreeBSD's `smmu_reset` order with four changes: CR0 is written 0, not just
   SMMUEN cleared, since the queue bases may not be written while their queues are enabled;
   global errors firmware left are acknowledged before the command queue starts (a stale
   `CMDQ_ERR` would stop it); CR2.E2H is set only with IDR0.HYP (RES0 otherwise); ATSCHK as
   FreeBSD when ATS is offered. Firmware's CR0 and GBPA are recorded first, and a unit found
   enabled prints `smmu_reset: Warning: SMMU is enabled`. The reset runs in `start`, after the
   event and gerror handlers are installed (one drain follows, for anything raised before),
   and the provider is published before the node counts as attached, so no device it
   translates attaches first.
7. **GBPA** is never written while the unit runs: it governs traffic only while SMMUEN is clear,
   which happens at quiesce. Quiesce writes CR0 and IRQ_CTRL to 0 and writes GBPA back only if it
   differs from firmware's, which nothing in the kernel causes. A failed reset leaves the unit
   disabled, its consumers untranslated.
8. **STE updates** break before make: a valid entry is first made invalid (`CMD_CFGI_STE`,
   sync), then words 1–7 are written, a barrier, word 0, `CMD_CFGI_STE`, sync. Giving a stream
   its current STE does nothing. Before the reset, entries (the RMR streams) are written
   directly, as the board port's `smmu_init_bypass_sids`, and the reset's `CMD_CFGI_ALL` covers
   them. FreeBSD's `CMD_PREFETCH_CONFIG` after each change is left out.
9. **Events** print FreeBSD's lines: `Event C_BAD_STE (Used STE invalid.) received.`,
   `SID %x, Input Address: %jx`, the eight `evt[i]`, and the stream's level-1 descriptor and
   STE. At most five per stream per second; the rest are counted and summarized
   (`SID %x: N more events not shown`). QEMU reports a 64-byte transfer from an invalid stream
   as sixteen events, so the limit matters. Per-stream counts and the last event are kept
   (`smmu::stream_events`) but not yet exported as sysctls. §4.10's `nvme0 (rid …)` line, which
   needs stream-to-device lookup, and FreeBSD's quirk table, which applies to translation
   faults only, come with stage 3. An overflow is acknowledged and logged once per overflow.
10. **Global errors** print FreeBSD's `SMMU Global Error`, then each bit's name, and are
    acknowledged. A refused command (`CMDQ_ERR`) is replaced in its slot with a `CMD_SYNC` that
    signals nothing, logged with its words, and acknowledged. The check also runs inside every
    sync and queue-space wait, which would otherwise wait out the timeout on a stopped queue.
11. **The count line** is the board port's: `N streams bypassed (reserved memory)` (with
    `, ACPI devices` under `hw.smmu.bypass_named=1`), the streams deduplicated, printed before
    the reset and also with `hw.iommu.dma=0`, as the port prints it before its `hw.iommu.dma`
    check. RMR streams come from `Iort::node_streams` over `rmrs()` (`acpi::iort::SmmuNode`'s
    `reserved`), named components' from every named-component node mapping to the unit
    (`named`), present in the namespace or not. A stream beyond the table prints
    `reserved stream %#x out of range`.
12. **Tests in QEMU.** The SMMU cases add QEMU's `edu` device (`dma_mask` widened to reach
    `virt`'s RAM) as the idle device: no driver takes it, and the ktest
    `smmu_running::smmu_streams_bypass_and_abort` drives its DMA engine bypassed (memory to the
    device and back), invalid (`C_BAD_STE` for its stream, nothing written), and config abort
    (nothing written, no event: IHI 0070 gives the abort configuration no event). The new
    `aarch64-smmu-warm-reset` case shuts the first boot's devices down before the reset
    (`abyss.test.orderly_reset=1`), so the SMMU is quiesced; QEMU's system reset resets its SMMU
    anyway, so whether the Sky1's PSCI reset does stays stage 5's question (§12).

For stage 3: `Provider::pass_through` is where translated contexts plug in (the broker already
calls it at the right moments); `Unit::set_stream` already does the break-before-make sequence
a `Ste::Stage1` needs, and `CMD_CFGI_CD` and the TLBI commands go through the same queue
(`Unit::push` and `Unit::sync`, private now). The event path has the stream, the place to name
the device.

## 16. Stage 3 as built (2026-11-10)

Stage 3 (§8) translates DMA: `sys/iommu::domain::Domain` (an IOVA space and the I/O page tables
over `pt::smmu`, implementing `vm::dma::IoDomain`), `vm::dma`'s `Backend::Iommu` (`DmaTag` is
`Clone` now), the broker's backend choice, and the unit's contexts in `drivers/smmu`. Where this
design was silent or wrong, it made these decisions:

1. **MSI in stage 3 (what §8 missed).** §8's stage 3 row tests with `hw.pci.enable_msix=0`, but
   says nothing of the default configuration, where a translated function's MSI write to the ITS
   doorbell would fault and its interrupts never arrive. So until stage 4, a unit translates only
   while message-signalled interrupts are off: the kernel tells the framework
   (`iommu_framework::set_messages_untranslated`, from `pci::msi::available()` once `pci::init` has
   or has not installed the ITS), `translating()` is `hw.iommu.dma` and not that, and a unit that
   would translate otherwise gives every context pass-through (stage 2's behavior), prints
   `smmu0: message-signalled interrupts are not translated: devices pass through`, and reads
   `dev.smmu.N.dma: 0`. In AbyssBSD `hw.pci.enable_msix=0` (or `enable_msi=0`) turns off both
   MSI and MSI-X (the ITS is not installed), so "INTx under translation" needs that one knob.
   `aarch64-smmu` now runs with it (translated); `aarch64-smmu-warm-reset` keeps MSI-X and so
   covers the pass-through fallback. Stage 4 removes the rule.
2. **Interfaces.** `IoDomain` is `reach()` (the first physical address the unit cannot output,
   so memory for a translated device comes from below it), `map(ranges, write, constraints)`
   (physical ranges in order, each joining the next at a page boundary, at one IOVA range;
   `write` false is §4.7's read-only `ToDevice`), `unmap(&IoMapping)`, and `doorbell(pa)`.
   `bus::iommu::Iommu` has one method, `context(stream, &ContextRequest)`, replacing stage 2's
   `pass_through`; the request carries what is wanted (`Want::Translate` or
   `Want::PassThrough`), the requester's name and RID, its PCI segment, the device's coherency
   (`dma_limits`), and the host bridge's windows. The unit keeps its contexts by stream (FreeBSD
   keys them by requester; §14 note 7 made that the stream). A domain reaches its unit through
   `iommu_framework::domain::Invalidate` (a `Weak` back-reference, so the unit's contexts and
   the domains do not keep each other alive).
3. **What a request gets.** The first request for a stream decides its context: translated when
   the unit translates, the request wants it, and `hw.busdma.pciD.B.S.F` (else
   `hw.busdma.default`) is not `bounce` (FreeBSD's `iommu_bus_dma_is_dev_disabled`, read from the
   kernel environment); pass-through otherwise. Later requests get that context, except that a
   request for physical addresses always gets `Context::PassThrough` (its tag direct), the stream
   left as it is. That serves the virtio rule (§4.4): `VirtioDevice::dma_tag` asks for a
   pass-through tag when `ACCESS_PLATFORM` was not negotiated (the four virtio drivers call it
   instead of `AttachCx::dma_tag`); such a device uses physical addresses whatever the STE says,
   by the virtio specification and in QEMU.
4. **Context lifetime.** Domains are counted (`Arc`): tags, `DmaCoherent` memory, pools, and the
   unit's context table hold them. A context lives as long as its unit, which covers a detach and
   re-attach (§4.4). Ending one when its node is deleted needs node deletion, which only hotplug
   brings (P11); `smmu::release_context` does it for tests: the STE invalid, then the ASID's TLB
   entries invalidated and its descriptor freed; the domain's tables go with its last reference.
5. **Rule 2** (§13 decision 5) is `DmaBuf::payload_only`, false by default. `PageRun` and
   `IoBuf`'s lent and fresh pages (buffer-cache pages) say true. A buffer that does not is mapped
   in place only if every range of it is whole pages; otherwise it is bounced, and the bounce
   pages are mapped through the domain like any buffer. No mbuf cluster reaches DMA yet: `rge`
   and `vtnet` copy each frame they send into a heap buffer, which under translation is bounced
   too, a second copy per frame. Receive buffers are coherent memory, mapped once. Stage 6
   should give transmit a payload-only buffer (a cluster zone or a pool) before it measures.
6. **Mapping in place** also needs the buffer's physical pieces to meet at page boundaries (one
   contiguous IOVA range), its pages below the unit's reach, and its start aligned as the tag
   asks (beyond a page, a page-aligned start, which the domain then aligns); otherwise it is
   bounced. A tag's address limit and alignment become the allocation's (`gas` `limit`,
   `align`); its boundary is given to `gas` only when the buffer fits in one boundary unit, and
   otherwise the segments are split at boundaries as on the direct backend.
7. **Invalidation.** Map enters the pages under the domain's table lock, then issues
   `CMD_TLBI_NH_VA` (leaf) per page and one sync, as FreeBSD's `smmu_map`; a failure there is
   ignored, since the entries are in place either way. Unmap clears the entries, then
   `CMD_TLBI_NH_VA` per page, or one `CMD_TLBI_NH_ASID` past 512 pages
   (`smmu::unit::TLBI_ASID_PAGES`), and one sync, and only then frees the IOVA range. If the sync
   is not confirmed, the range is never reused (`Domain::leaked_pages`); FreeBSD frees it anyway.
   Range invalidation (IDR3.RIL) is stage 6's.
8. **Locks.** A domain has a spin lock for its tables and one for its space; the unit has its
   command queue's. None is held while another is taken, so CPUs mapping for different devices
   meet only at the unit's queue, as on FreeBSD. Making a context holds the unit's context table
   while it takes the unit's lock; the event thread takes them one after the other.
9. **Tables.** Frames come from the memory of the unit's own direct tag, below `1 << OAS`, zeroed,
   and through the direct map; on a unit whose walks are not coherent each frame a change touched
   is cleaned before the unit is told. A range that crosses bit 47 is entered in two calls (§4.6's
   stage 1 note). A domain's tables are freed when it goes. The attributes follow the device's
   coherency as the broker computes it from `_CCA` (§4.7).
10. **ASIDs and descriptors.** ASIDs come from a bitmap of IDR0's 8 or 16 bits, 0 never given out.
    One CD per domain, keyed by its ASID, in its own page of the unit's coherent memory, written
    words 1–7 then word 0 with V. The STE's `S1STALLD` is set when the SMMU's stall model is
    stall-and-terminate (FreeBSD sets it unless stalls are forced). `CMD_CFGI_CD` goes with the
    final `CMD_CFGI_STE`, before its sync.
11. **Windows.** The ACPI bus records each host bridge's produced memory windows (`_CRS` address
    descriptors that are not consumers, at `_MIN`, as the board port reads `Minimum`) in
    `Translation::Pci`; each domain reserves them with `reserve_extend`. QEMU's are
    `0x1000_0000` + `0x2eff_0000` and `0x80_0000_0000` + 512 GiB.
12. **Relaxed ordering.** The PCI bus gives each function with a requester ID a hook
    (`bus::iommu::RequesterHook`), which the broker runs when the stream's first translated
    context is made: it clears `DEVCTL.RO` if set (unless `hw.iommu.relaxed_ordering=1`) and the
    broker prints `<dev>: relaxed ordering disabled (IOMMU translation)` under the function's
    name. The bit is cleared on the function itself; FreeBSD's port clears it on the requester,
    which differs only for a conventional function behind a PCIe-to-PCI bridge, which has no PCIe
    capability. There is no saved configuration yet for a reset to restore over it. QEMU's
    devices do not set RO, so the line never shows there; the board's should.
13. **Event lines** (§4.10): for a stream with a context, `smmu0: nvme0 (rid 0x10): translation
    fault at 0x1234000 (write)` comes before FreeBSD's lines, under the same per-stream rate
    limit. The name is the requester's node's (a virtio device's is its transport's,
    `virtio_pciN`), or, for a function no driver has, its selector (`pci0:0:6:0`). FreeBSD's
    quirk table (RTL8168 faults at IOVA 0) is not carried: whether the board's RTL8126 needs an
    entry is stage 5's finding (§4.10).
14. **The doorbell** (§4.8) is built and host-tested, `IoDomain::doorbell`: one page per domain,
    Device-nGnRE, read-write, mapped on first use. Nothing calls it until stage 4.
15. **The boot disk** of `aarch64-smmu` stays virtio-blk (§14 note 11 put NVMe as the boot disk in
    stage 3): the storage ktests already read and write two NVMe namespaces under translation,
    and an NVMe boot disk would renumber the `nda` disks they name.
16. **Tests.** Host: `drivers/smmu/src/tests_dma.rs` runs `vm::dma` tags over domains over the
    driver's unit over `iommu::model`, in one memory that the CPU side reaches as `DmaMemory` and
    the model as `PhysMemory` (so the model walks the tables the kernel's code wrote): strict
    unmap and reuse, read-only `ToDevice`, scattered pages as one segment, rule 2's bounces,
    constraints on bus addresses, coherent memory and pools mapped for life, windows, contexts per
    stream and pass-through requests, the doorbell, event lines, and stage 1's randomized
    strict-unmap test redone through tags (it fails if unmap skips its invalidation).
    `sys/acpi` checks QEMU's windows; `sys/iommu` the `hw.busdma` knobs. QEMU `aarch64-smmu`
    (`hw.pci.enable_msix=0`): every storage, network, and file-system ktest under translation;
    `smmu_translates_dma` checks each NVMe controller's translated STE, its event name, and
    relaxed ordering off, then gives `edu` a context through the broker and moves memory through
    IOVAs both ways, checks that an unmapped buffer's old IOVA faults with nothing written
    (unmap before reuse), and that a read of an IOVA never mapped faults with the event the
    device's line names.

For stage 4: `IoDomain::doorbell` is ready; the MSI path (`pci::bus::msix_lines`, `msi_line`,
and virtio's own MSI-X setup, which does not ask for a context at all) gets the context from
`AttachCx::iommu_context` (which now returns it) and composes the message address through it;
then `iommu_framework::set_messages_untranslated` and the `enable_msix=0` in `aarch64-smmu` go.
For stage 5: the relaxed-ordering lines, the event names (`nvme0 (rid 0xc100)`), and whether
`rge` faults at IOVA 0 are the board's to show; `hw.iommu.dma=1` translates there only once
stage 4 has landed (the board uses MSI-X).
