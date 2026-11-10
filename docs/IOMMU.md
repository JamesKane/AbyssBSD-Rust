# IOMMU: DMA translation through the SMMUv3

- Status: **Draft**. The design for Phase 9's SMMUv3 item ([ROADMAP.md](ROADMAP.md#phase-9-sky1-hardening-l)).
  It becomes an ADR when implementation starts (§13 question 1)
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
  `smmu_acpi_identify` does, before it walks the namespace. They are `Ident::Child { class: "smmu",
  desc: "ARM System Memory Management Unit" }` with bus data from the IORT node: base, flags,
  model, GSIVs, and node offset. Resources: memory `0x20000` at the base, and four IRQs in
  FreeBSD's order (event, PRI, sync, gerror). Each IRQ is edge-triggered, active-high: ACPI carries
  no trigger for them, and FreeBSD forces this (`smmu_configure_intr`).
- **Placement.** They sit in `acpi0`'s child list where FreeBSD's order puts them (after `gic0`,
  before `generic_timer0`, in the corpus). Units come from IORT order, so `smmu0` is `0xb010000`.
- **Attach.** Attach claims the memory and the event, sync, and gerror lines. The sync line is
  granted and left without a handler, as FreeBSD's is. That keeps `devinfo -rv` at three active
  interrupts. Probe accepts model 0 only, like FreeBSD (`BUS_PROBE_NOWILDCARD`, so only this child
  is matched). Messages:
  - `smmuN: ... on acpi0`, built from the resources
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
  §13 question 5.

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
  `virtio-*-pci` device, without which QEMU's virtio devices bypass the vIOMMU. `nvme` needs
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
  IOMMU row, the branch notes) and HARDWARE.md §4. HARDWARE.md's storage row ("SMMUv3 + IORT RMRs
  come in P11") is updated when stage 5 lands.

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

## 13. Open questions

1. **An ADR?** DRIVER-MODEL (ADR-0012) planned IOMMUs for P11; this moves the SMMUv3 into P9 and
   adds `EdgeKind::Settle`, `sys/iommu`, and `drivers/smmu`. Record it as ADR-0022 when stage 0
   starts? Should DRIVER-MODEL §3.1 and §16 (whose P11 rows name the SMMUv3), ROADMAP's P11 bullet,
   and HARDWARE.md's IOMMU rows be amended then?
2. **Defaults.** `hw.iommu.dma=1` on arm64 when an SMMU attaches, against FreeBSD's 0; and
   `hw.smmu.bypass_named=0`, against the sibling's compiled-in 1. Both follow the board's working
   FreeBSD configuration and "nothing special". Confirm, and confirm that they are recorded in
   compat-deviations.
3. **The performance baseline on the board**: FreeBSD `GENERIC-HMP-IOMMU` with translation (like
   for like), or stock `GENERIC` (which does not translate)? The design assumes the former.
4. **DRIVER-MODEL's "batched and deferred" invalidation.** This design is strict per unmap,
   batched within it (§4.5), as FreeBSD's arm64 and Linux on this board are. Reword §8.4's table to
   match?
5. **Rule 2 (whole pages, small buffers bounced).** Options:
   - follow FreeBSD (map the page, neighbors exposed);
   - bounce only buffers from the general heap, and map payload-only zones (mbuf clusters,
     buffer-cache pages) in place;
   - enforce rule 2 as written and measure the copy.

   The second keeps rule 1's guarantee (no kernel pointers reachable) at little cost. Which?
6. **Enabling `smmu1` in P9** when AbyssBSD has no driver behind it. Following FreeBSD, it is
   enabled: RMR streams bypassed, the rest aborted. The alternative leaves it as firmware left it
   until M-G, which is safer for the console but gives no isolation. The design takes FreeBSD's
   way.
7. **The edge kind's name and placement.** `Settle` as a fifth `EdgeKind`, or a flag on
   Required ("released if the supplier fails")?
