# ADR-0022: The SMMUv3 in Phase 9, in FreeBSD's split: `smmu` units under `acpi0`, a framework and a third DMA backend, a domain per requester with strict invalidation, and a settle edge

- Status: Accepted
- Date: 2026-11-10

## Context

DRIVER-MODEL.md put every IOMMU in P11 (§3.1, §16) and ROADMAP's P11 listed "VT-d, AMD-Vi, SMMUv3
with IORT RMR identity maps". On the Orange Pi 6 Plus, PCIe DMA to the RAM at `0x80_0000_0000`
lands somewhere else and hangs the board (2026-11-10); the board runs with `hw.physmem=29G` until
that is fixed. FreeBSD on the same board does not hit it: its DMA goes through the SMMUv3s, which
translate there. The user decided against a board-specific DMA limit ("nothing special"): the fix
is the SMMU, FreeBSD's design, in Phase 9.

The design is [IOMMU.md](../IOMMU.md); its §13 records the user's decisions of 2026-11-10.

## Decision

1. **The SMMUv3 moves into P9.** x86 DMAR and AMD-Vi, interrupt remapping, the SMMUv2 (SC8280XP),
   stage 2, ATS, PRI, PASIDs, and IOMMU groups stay later (P11 or after; IOMMU.md §9). amd64
   FreeBSD leaves its IOMMUs off by default, so P10's X870E runs untranslated, as FreeBSD does.
2. **FreeBSD's split, with this project's types.** `acpi0` adds an `smmu` child per SMMUv3 node of
   the IORT, in table order, with FreeBSD's resources (`smmu_acpi_identify`). The hardware driver
   (`drivers/smmu`) is a driver crate like any other, `#![forbid(unsafe_code)]`. The framework
   (`sys/iommu`: units, domains, contexts, IOVA spaces, and the I/O page tables, which are its
   `unsafe`) is FreeBSD's sys/dev/iommu and arm64/iommu/iommu.c. Pure logic (the IOVA allocator,
   the SMMUv3's formats, a host model of the SMMU) is `lib/iommu`. `vm::dma` gains the `Iommu`
   backend beside direct and bounce; drivers do not change.
3. **Ordering by a fifth edge kind, `Settle`.** An SMMU → each PCI host bridge whose root complex
   maps to it, and → each named component it translates, from the IORT. The consumer waits until
   the supplier has *settled* (attached, failed, or matched by no driver), then attaches whatever
   the outcome; translated if the unit translates, untranslated otherwise, as FreeBSD runs when its
   SMMU does not attach. Shutdown orders consumers first, as for `Required`. Cycle breaking treats
   it as soft.
4. **One domain per requester** (FreeBSD's arm64 `iommu_get_ctx`), requesters found by FreeBSD's
   `iommu_get_requester` walk. `%iommu` reads `rid=…` for every PCI function on both ISAs, as on
   both reference kernels (which have `options IOMMU`).
5. **IOVAs from a FreeBSD-style `gas`**: address-ordered first fit, lowest first, a guard page on
   each side, reserved and identity regions; a PCI domain reserves its host bridge's windows.
6. **Strict invalidation**: an unmap clears the PTEs, invalidates, and waits before the buffer goes
   back to the CPU, with one sync for all of a mapping's pages. No deferred (lazy) invalidation.
7. **Defaults**: `hw.iommu.dma=1` on arm64 when an SMMU attaches (FreeBSD: 0), and
   `hw.smmu.bypass_named=0`, as the board's working FreeBSD runs; both in compat-deviations.
8. **What stays untranslated**: IORT RMR streams are bypassed before a unit is enabled (the
   firmware framebuffer's scanout). Named components without a driver abort (`bypass_named=0`);
   with one, they get contexts in M-G. `smmu1` is enabled in P9 as FreeBSD does.
9. **DMA-visible memory rule 2**: with an IOMMU, buffers from the general heap are bounced unless
   they fill whole pages that hold nothing else; buffers from payload-only zones (mbuf clusters,
   buffer-cache pages) are mapped in place.
10. **The performance baseline on the board** is FreeBSD `GENERIC-HMP-IOMMU` translating
    (`hw.iommu.dma=1`): like for like.

## Consequences

- DRIVER-MODEL.md §3.1 (`sys/iommu`, `lib/iommu`), §6.1 (the IORT row and the `Settle` kind),
  §8.4 (the backend row, rules 2 and 3), and §16 (a P9 row; P11 keeps x86 and the SMMUv2) are
  amended. ROADMAP's P11 IOMMU bullet now means x86 and the SMMUv2. HARDWARE.md's IOMMU rows say
  the SMMUv3 is P9's.
- The work is staged (IOMMU.md §8): tables and tree (stage 0), the pure libraries, the SMMU
  running with every stream bypassed, translated DMA, MSI through the doorbell window, the
  board, and performance. `hw.physmem=29G` comes out of the board's `loader.conf` at stage 5.
- `lib/devtree`'s graph and the bus manager learn `Settle`: a supplier that fails or that no driver
  matches now releases its consumers, which a `Required` edge never needed.
- Tests: the IORT against the board's table and QEMU's `virt,iommu=smmuv3`; an `aarch64-smmu`
  boot case; a host model of the SMMU for the driver and the DMA layer.
- Translation costs a page-table update, an invalidation, and a sync per map and unmap; pools,
  one sync per unmap, per-device domains, and spinning syncs keep it within the 2% gate, measured
  against a translating FreeBSD.
