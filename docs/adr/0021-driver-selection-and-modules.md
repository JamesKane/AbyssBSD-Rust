# ADR-0021: Drivers selected per ISA at build time; large drivers as same-build loadable modules that import no kernel symbols

- Status: Proposed
- Date: 2026-11-10

## Context

Every `DriverDecl` and every platform in `kernel/table` is in both kernels: the aarch64 kernel
carries `isa`, `atrtc`, `ichwd`, `amdsbwd`, and the x86 timers, and the x86_64 kernel carries the
Sky1 drivers and `cix-sky1`. Fat LTO already drops code no table names, so the waste is the table's,
and it is small today. It will not stay small: the native GPU drivers of GRAPHICS.md §9 (`amdgpu`,
`panthor`, the Sky1 display aggregate, later `msm` and NVIDIA) are each comparable to or larger
than today's whole kernel, and most machines of either ISA lack their hardware.

ADR-0005 made the kernel static and deferred loadable modules until after v1.0, "built by the
identical toolchain against an exported kernel interface crate, with versioned symbol checks".
ADR-0020 decision 6 builds every platform into every kernel so that no board needs a setting.
DESKTOP.md §2 expects `kld_list="amdgpu"` to work.

The full design is [MODULES.md](../MODULES.md).

## Decision

1. **Build-time selection is by ISA only.** `kernel/table` holds a common table and one per
   architecture; a kernel uses its own architecture's. A driver is in an architecture's table when
   FreeBSD builds it for that architecture (`sys/conf/files.<arch>`, `GENERIC`). Plug-in device
   drivers (PCIe, USB) are in both. Platforms follow the same rule; within an ISA, ADR-0020
   decision 6 is unchanged. No Cargo features and no named kernel configurations. Arch-only
   driver crates are target-scoped dependencies that every host build also sees, so host checks
   cover every table.
2. **Large, rarely present drivers are loadable modules**, chosen per driver by MODULES.md §5.1.
   Classes (`sys/drm`) and everything needed to reach mountroot stay built in.
3. **A module imports no kernel symbols.** It is a PIC ELF shared object with only `RELATIVE`
   relocations and one exported entry symbol. The kernel passes it a `KernelServices` table of
   trait objects; it returns a `ModuleDecl` (its drivers, FreeBSD module name, version,
   dependencies, PNP records). The kernel exports nothing and keeps fat LTO.
4. **A module loads only into the kernel of its own build.** A `BuildId` (toolchain, target,
   flags, boundary crate sources) must match, and a `LayoutTable` of every boundary type's size,
   alignment, and field offsets, emitted by both sides, must match. Otherwise `ENOEXEC`. There is
   no stable driver ABI.
5. **The boundary is `kapi` crates of types and traits with no statics.** A driver that can be a
   module is written against them, and the same crate links into the static kernel unchanged.
6. **Unload uses the framework-owned detach.** The kernel reaches module code only through
   bindings, publications, and the matcher; unload removes the drivers, detaches every binding,
   waits for an epoch grace period and a zero code count, then unmaps. A class may refuse with
   `EBUSY` while its devices are open, as `drm-kmod` does.
7. **FreeBSD's userland view of modules**: the `kld*`/`mod*` syscalls, `kern.module_path`,
   `/boot/kernel/<name>.ko`, `kld_list`, `loader.conf` `*_load`, and `devmatch(8)` reading a
   `linker.hints` that xtask generates in FreeBSD's format from each module's `const` match table.

## Consequences

- ADR-0005 is amended: the kernel stays monolithic and static for everything but the drivers named
  under decision 2, and `kldload`'s answer for built-ins is unchanged.
- DRIVER-MODEL.md §7.1's `#[cfg(feature = ...)]` sketch is replaced by the per-ISA tables, and §14's
  "Loadable modules" seam points to MODULES.md.
- Part 1 (decision 1) is small and lands in P9 or P10. Modules (decisions 2–7) land between M-G.0 and
  the first native GPU driver, after v1.0, as ADR-0005 planned.
- `sys/drm` is designed with its `kapi` split from the start, and `sys/bus`'s driver-facing API moves
  into `sys/bus/kapi`. This is the main cost.
- A module carries its own copies of `core`, `alloc`, and the pure `lib/*` crates it uses. A policy
  check forbids statics in a module's dependency closure outside the module itself.
- Kernel service calls from a module are indirect calls. That is negligible at GPU-driver call
  rates and absent from built-in drivers.
- Kernels and modules are one release unit; mixing builds fails cleanly at `kldload`.
