# Driver Selection and Loadable Modules

Status: **accepted high-level design** (2026-11-10). Decision record:
[ADR-0021](adr/0021-driver-selection-and-modules.md) (Accepted). It amends
[ADR-0005](adr/0005-monolithic-static-kernel.md) and refines
[ADR-0020](adr/0020-platform-crates.md) decision 6 and [DRIVER-MODEL.md](DRIVER-MODEL.md) §7.1 and §14.

This document covers two related problems:

1. **Build-time selection.** A kernel should contain only the drivers that can run on its
   architecture. Today every `DriverDecl` and every platform is in both kernels: the aarch64
   kernel carries `isa`, `atrtc`, `ichwd`, `amdsbwd`, and the i8254/HPET timers, and the x86_64
   kernel carries the Sky1 clock, reset, pinctrl, syscon, mailbox, GPIO, and I²C drivers and the
   `cix-sky1` platform.
2. **Loadable modules.** Large drivers that most machines never use (the native GPU drivers of
   [GRAPHICS.md](GRAPHICS.md) §9: `panthor`, `amdgpu`, later `msm` and NVIDIA) should not be in
   every kernel. They are loaded at run time with `kldload`, as FreeBSD's `drm-kmod` is, and can be
   unloaded.

Part 1 is small and can land in P9 or P10. Part 2 is a new kernel subsystem and is scheduled
before the first native GPU driver (§10).

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal |
|---|---|
| G1 | **One kernel per architecture boots every supported machine of that architecture** with no board option ("nothing special", ADR-0020 decision 6). Selection is by ISA, never by board |
| G2 | **The driver set stays reviewable code**: a table, not linker sections (DRIVER-MODEL.md §7.1) |
| G3 | **Host checks see every driver**: the match-tie check and the corpus graph checks cover the union of every architecture's table and every module, on any host |
| G4 | **FreeBSD's userland view of modules**: `kldload`, `kldunload`, `kldstat`, `kldfind`, `modfind`, `kern.module_path`, `kld_list` in `rc.conf`, `loader.conf` `*_load`, `devmatch(8)` and `linker.hints`, behaving as on FreeBSD 15.1 |
| G5 | **Unload is safe by construction**: no code or data of an unloaded module is reachable, using the framework-owned detach (DRIVER-MODEL.md §10.2) rather than per-driver discipline |
| G6 | **A driver crate is the same source built in or as a module.** Which one is a build decision, not a code change |
| G7 | **No performance cost** on the static kernel's paths, and none on a module's data path beyond one indirect call per kernel service call (ADR-0014) |
| G8 | Within the hard rules: stable Rust, built-in targets, no external crates, both ISAs |

### 1.2 Non-goals

- **Loading FreeBSD `.ko` files.** FreeBSD KLDs are C against FreeBSD's kernel internals. Not
  possible and not wanted.
- **A stable driver ABI.** A module loads only into a kernel from the same build (§6.3). The
  internal API stays free to change (DRIVER-MODEL.md §1.2).
- **Third-party or out-of-tree modules.** Every module is a workspace crate built by
  `cargo xtask`.
- **Per-board kernels.** No `GENERIC-SKY1`. A machine that needs a driver removed to boot has a
  driver bug.
- **Modules for small drivers.** Storage, network, input, USB, buses, timers, consoles, and
  everything needed to mount root stay built in. Modules are for drivers whose size or rarity
  earns the boundary (§5.1).

## 2. Where the bytes are

The release kernels are about 5.4 MB of text on x86_64 (2026-11-10). With fat LTO and
`--gc-sections`, a driver that no table names is already removed from the image. So "not compiled
in" is decided by the table, and Cargo dependency gating only saves build time. The drivers
mis-placed today are small. The real pressure is ahead:

- Linux's amdgpu is several million lines, much of it generated register headers; a native RDNA2
  driver will be larger than today's whole kernel, plus tens of MB of firmware (already outside
  the image, DRIVER-MODEL.md §8.6).
- `panthor`, the Linlon display aggregate, the VPU and NPU drivers are Sky1-only and useless on
  every QEMU, bhyve, and x86 machine.

## 3. Part 1: build-time selection by architecture

### 3.1 The rule

A driver is built into an architecture's kernel when FreeBSD builds it for that architecture: the
file is `standard` or `optional` in `sys/conf/files.<arch>` (or `files.x86` for amd64) and the
device appears in that architecture's `GENERIC`. FreeBSD is the reference, so the rule is cited,
not invented:

| Driver | FreeBSD | AbyssBSD x86_64 | AbyssBSD aarch64 |
|---|---|---|---|
| `isa`, `atrtc`, `attimer`, `hpet`, `acpi_timer` | `files.x86` / `files.amd64` | yes | no |
| `ichwd`, `amdsbwd` | `files.x86` | yes | no |
| `sbsa_gwdt`, xHCI described in ACPI (`generic_xhci_acpi`) | `files.arm64` | no | yes |
| Sky1 drivers (`sky1-*`, `cix-mbox`, `cdns-gpio`, `cdnc-i2c`) | `cdnc_i2c` in `files.arm64`; the others have no counterpart | no | yes |
| PCI and USB function drivers (`nvme`, `ahci`, `rge`, `xhci` on PCI, `virtio-*`, HID, `umass`) | `files` | yes | yes |
| ACPI devices (`acpi_button`, `acpi_thermal`, `acpi_ged`, `acpi_gpiobus`, CPPC, `uart`) | `files` | yes | yes |

One driver crate may have entries in different tables: the `xhci` crate's PCI attachment is
common and its ACPI one is aarch64 only.

A driver for a device on a plug-in card (PCIe, USB) is in both, whatever machine it was written
for. A driver for an SoC block or a chipset is in its ISA only. The same rule applies to
`PLATFORMS`: `cix-sky1` is aarch64 only, and every aarch64 kernel has it (ADR-0020 decision 6
is unchanged within an ISA).

### 3.2 The mechanism

`kernel/table` holds one table per architecture, all of them compiled on the host:

```rust
// kernel/table/src/lib.rs (as built, 2026-11-10)
macro_rules! drivers { ($($isa:expr),*) => { &[acpi::DRIVER, /* … every common entry … */ $($isa),*] } }

#[cfg(any(target_arch = "x86_64", not(target_os = "none")))]
pub static X86_64: &[DriverDecl] = drivers![isa::ISAB, isa::ISA, atrtc::DRIVER, /* … */];
#[cfg(any(target_arch = "aarch64", not(target_os = "none")))]
pub static AARCH64: &[DriverDecl] = drivers![sbsa_gwdt::DRIVER, xhci_driver::ACPI, /* … */];
// PLATFORMS_X86_64 (empty) and PLATFORMS_AARCH64 (cix-sky1) likewise.

/// The table this kernel uses.
#[cfg(target_arch = "x86_64")]  pub use X86_64 as DRIVERS;
#[cfg(target_arch = "aarch64")] pub use AARCH64 as DRIVERS;
```

- Each ISA's table is one `static`: the common entries, written once in the `drivers!` macro,
  then the ISA's own. Order breaks no ties (the check forbids them), so the ISA's entries
  following the common ones changes nothing.
- The kernel itself names one arch-only driver crate, `sbsa-gwdt` (the GTDT watchdog nexus
  children in `kernel/src/devices.rs`). That dependency and code are aarch64 only.
- **Dependencies are gated so the host sees everything and a kernel target sees its own ISA.**
  Arch-only driver crates move to target-scoped dependencies:

  ```toml
  [target.'cfg(any(target_arch = "aarch64", not(target_os = "none")))'.dependencies]
  sky1-clk = { path = "../../drivers/sky1-clk" }
  [target.'cfg(any(target_arch = "x86_64", not(target_os = "none")))'.dependencies]
  ichwd = { path = "../../drivers/ichwd" }
  ```

  On the host (`target_os = "linux"`, `"freebsd"`) every crate is present, so the tie check and
  the whole-machine corpus checks (ADR-0020 consequences) run over every table. On
  `*-unknown-none` only the ISA's crates are compiled.
- The host checks become per table: no unresolved tie **within** each architecture's table, and
  each corpus machine is checked against its own architecture's table and platforms (QEMU and
  bhyve x86 corpora against `X86_64`; QEMU aarch64 and the Orange Pi 6 Plus against `AARCH64`).
  A new check fails if an ACPI device of a corpus machine matches no driver in its ISA's table
  but does match one in another's: that is a driver placed in the wrong table. Another pins the
  difference between the tables to the ISA lists, so the common part cannot drift.
- The `kldstat`/`modfind` built-in list (`kernel/src/syscall/misc.rs` `MODULES`) stays as it is.
  It is a hand-written list of FreeBSD module names (file systems, GEOM classes, `bus/driver`
  pairs) and names no arch-only driver, so there is nothing to derive from the table yet.
- Driver crates themselves stay ISA-neutral and host-testable (DRIVER-MODEL.md G8, G9). No
  `cfg(target_arch)` appears in `drivers/*`.

No Cargo features are introduced. There is exactly one configuration per ISA, so a feature matrix
would be untested surface. (The `#[cfg(feature = "nvme")]` sketch in DRIVER-MODEL.md §7.1 is
replaced by this.)

### 3.3 What Part 1 is not

There are no named kernel configurations (FreeBSD's `GENERIC`/`MINIMAL`). FreeBSD's `MINIMAL`
exists because its module system makes a small kernel plus modules practical; Part 2 gives the
same result for the drivers where it matters, without a second static configuration to test.

## 4. Part 2: loadable modules, overview

```
 build (xtask)                                 run time
 ─────────────                                 ────────
 drivers/amdgpu  ──┐                           kldload amdgpu   (rc kld_list, devmatch, loader)
 sys/drm/kapi    ──┼─► amdgpu.ko  ─────────►   kern_linker: verify build id, layout table
 lib/kapi        ──┘   (ET_DYN, PIC,              map RX/RW/RO, apply RELATIVE relocs
                        one entry symbol,         call entry(&KernelServices) -> &ModuleDecl
                        no undefined symbols)     register DriverDecls; manager reprobes
                                               kldunload: detach bindings (§10.2), drain,
                                                  wait epoch, unmap
```

The central choice: **a module imports no kernel symbols.** It is a self-contained,
position-independent ELF image (a static PIE, like the kernel) whose entry point is its only
way in, with no dynamic symbols at all. Everything it needs
from the kernel arrives at load time as a table of function pointers, and everything the kernel
needs from it is returned as a `ModuleDecl`. Consequences:

- The kernel can keep fat LTO and full internalization. It exports nothing, so no symbol must
  survive optimization and no kernel symbol table becomes ABI.
- The in-kernel linker is small: verify, map, apply `R_*_RELATIVE` relocations (which `lib/elf`
  already decodes for the PIE kernel and the vDSO), call `e_entry`. No symbol lookup, no
  PLT, no aarch64 branch-range veneers, no x86 `-mcmodel=kernel` placement constraint.
- Rust's unstable ABI is contained to one boundary whose types are checked at load (§6.3).

## 5. Module boundary

### 5.1 Which drivers are modules

A driver is built as a module when all hold:

- it is large (its text would be a significant fraction of the kernel), or most machines of its
  ISA lack its hardware;
- nothing needed to reach mountroot depends on it (the console falls back to the EFI framebuffer
  through `fb-console`, and later `gop-kms`);
- FreeBSD ships it as a module (`drm-kmod`'s `amdgpu.ko`, `i915kms.ko`, NVIDIA's `nvidia.ko`).

Initial list: `amdgpu`, `panthor`, the Sky1 display aggregate (`linlondp` and its DP-TX and PHY
children), later `msm`, NVIDIA, and the VPU and NPU drivers. `gop-kms` and `virtio-gpu` stay built
in: they are small, they are the test vehicles, and `gop-kms` is the console's handover target.

`sys/drm` itself (the class: files, GEM, KMS core, fences) is **built in**. FreeBSD splits it
into `drm.ko`; we do not need to, and a built-in class keeps every module dependency one level
deep (§8.3).

### 5.2 Crate layout

```
lib/kapi/           the boundary: KernelServices, ModuleDecl, BuildId, the layout table
sys/<class>/kapi/   a class's module-facing surface (sys/drm/kapi), types and traits only
drivers/amdgpu/     the driver, unchanged in shape: forbid(unsafe_code), DriverDecl
modules/amdgpu/     a generated shim crate: a no_main bin, `kapi::module!(amdgpu::MODULE)`
```

- **The `kapi` crates hold types, traits, and inline code only, never `static` items or code that
  touches kernel state.** A module links its own copy of every crate it uses. A copied pure crate
  (`lib/regs`, `lib/edid`, `core`, `alloc`) is harmless; a copied `static` would be a second,
  disconnected instance of kernel state. `cargo xtask check`'s policy check enforces it: the
  dependency closure of a `modules/*` crate may contain only `lib/*` crates without mutable or
  interior-mutable statics, `kapi` crates, and the driver.
- A driver that can be a module is written against `kapi` interfaces, never against `sys/bus` or
  `sys/vm` directly. The built-in kernel implements the same traits, so the same driver crate
  links into the static kernel unchanged when it is listed in `DRIVERS` (G6), and the built-in
  path is a static call.
- This is the expensive part of Part 2: the services a GPU driver uses (`bus` contexts, MMIO,
  interrupt, and DMA grants, the firmware store, `sys/drm`'s class traits, sysctl nodes, timers,
  task queues, `sys/vm` buffer objects) must be expressible as traits in `kapi` crates. `sys/drm`
  does not exist yet, so it is designed with its `kapi` split from the start (GRAPHICS.md M-G.0).
  The existing `sys/bus` driver-facing API is already traits and handles (DRIVER-MODEL.md §7.3,
  §8), so its split is mostly moving declarations.

### 5.3 `KernelServices` and `ModuleDecl`

```rust
// lib/kapi: sketch
#[repr(C)]
pub struct KernelServices {
    pub build: BuildId,                   // must equal the module's (§6.3)
    pub layout: &'static LayoutTable,     // the kernel's view of every boundary type
    pub alloc: &'static dyn RawAlloc,     // (size, align); the module's #[global_allocator] forwards here
    pub log: &'static dyn Log,            // takes &str
    pub abort: fn(&str) -> !,             // the module's panic handler formats, then calls this
    pub bus: &'static dyn bus_kapi::Services,     // contexts, grants, publications
    pub drm: &'static dyn drm_kapi::Services,     // present when the class is built in
    // ... one entry per kapi crate, by name, so adding a class does not move the others
}

#[repr(C)]
pub struct ModuleDecl {
    pub build: BuildId,
    pub layout: &'static LayoutTable,     // the module's view of the same types
    pub name: &'static str,               // "amdgpu", FreeBSD's module name
    pub version: i32,                     // MODULE_VERSION
    pub drivers: &'static [DriverDecl],   // offered to the matcher like DRIVERS
    pub depends: &'static [&'static str], // MODULE_DEPEND names; built-in ones are satisfied
    pub pnp: &'static [PnpRecord],        // for linker.hints (§8.2)
}

// modules/amdgpu/src/main.rs (generated): kapi::module! expands to the forwarding allocator,
// the panic handler, and the entry point, which is e_entry:
#[unsafe(no_mangle)]
pub extern "C" fn _start(k: &'static KernelServices) -> Option<&'static ModuleDecl>;
```

No core type with private fields crosses the boundary (`fmt::Arguments`, `PanicInfo`,
`alloc::Layout`): the layout table (§6.3) cannot name their fields, so a difference would go
unseen. Hence `&str` for the log, `(size, align)` for allocation, and `abort(&str)` for panics,
with the module formatting its message into a fixed buffer (a panic may be an allocation
failure). `KernelServices` is `#[repr(C)]` with `build` first, and `_start` compares it before
storing the pointer or touching anything else (spike finding 5).

The `'static` lifetimes here are a lie that the linker makes true: the module's memory lives until
`kldunload` has proved nothing refers to it (§7). Every route by which the kernel can hold a
module reference is enumerated in §7.1.

## 6. Building and verifying a module

### 6.1 Build

`cargo xtask build` builds the kernel and every module listed in the architecture's module
table (`kernel/table`'s `MODULES_<ARCH>`, beside `DRIVERS`) in **one Cargo invocation**, same
profile, same target (`x86_64-unknown-none`, `aarch64-unknown-none-softfloat`), and the same
`RUSTFLAGS`, so every shared crate is compiled identically:

- The shim is a `no_main` binary linked as a static PIE, exactly as the kernel is
  (`-C relocation-model=pie -C link-arg=-pie -C link-arg=-zmax-page-size=4096`), plus
  `-z separate-loadable-segments` so that `PT_GNU_RELRO`, which holds the vtables in
  `.data.rel.ro`, starts on a page. Its `_start` is `e_entry`. The built-in `*-unknown-none`
  targets refuse `cdylib` (no dynamic linking in the target spec); a `staticlib` linked by
  `rust-lld -shared` also works but adds a step and a symbol table nobody reads
  (spike (branch `spike/modules`, `spikes/kmod/README.md`), finding 1).
- xtask checks the result: `ET_DYN`, the dynamic relocations are only `R_X86_64_RELATIVE` /
  `R_AARCH64_RELATIVE` (and `NONE`), no `DT_NEEDED`, no dynamic symbols, no `PT_INTERP`, no
  TLS, no writable-and-executable segment, a page-aligned `PT_GNU_RELRO`, and the number of
  relocations that patch text (allowed, since the image is written before it is executable,
  but reported; the aarch64 kernel needs `-z notext` for the prebuilt `core`).
- The file is `<name>.ko` in `/boot/kernel/` beside the kernel, which is where FreeBSD's
  `kern.module_path` looks first. `xtask image` installs it.

### 6.2 Build identity

`BuildId` is a 32-byte hash, computed by xtask and passed to both the kernel and each module
through an environment variable read with `env!`:

- the `rustc -vV` output (version, commit hash, LLVM version), the target, the profile and
  `RUSTFLAGS`;
- the source hash of every `kapi` crate and every crate in the closure of a boundary type.

The kernel refuses a module whose `BuildId` differs, with `ENOEXEC` and a log line naming both
hashes. A kernel and its modules are one release unit, as FreeBSD's kernel and `/boot/kernel/*.ko`
are (`kldload` of a mismatched `.ko` fails there too, by `__FreeBSD_version` and `MODULE_DEPEND`).

### 6.3 Layout check

Identical toolchains and sources give identical layouts in practice, but Rust does not promise it
for `repr(Rust)` types. So the boundary is checked, not trusted:

- `lib/kapi` declares every type that crosses the boundary in a `boundary_types!` list. For each,
  both sides emit `(name, size, align, [offset_of!(field) ...])` into a `LayoutTable`, computed at
  compile time with `core::mem::{size_of, align_of, offset_of}` (all stable).
- Trait objects cross the boundary as `&'static dyn Trait`. Their vtables are emitted by whichever
  side creates the object, so caller and vtable always agree; what must agree across sides is the
  method signature's argument types, which are in the table.
- At load the kernel compares the two tables entry by entry before calling anything but the entry
  function. A mismatch is `ENOEXEC`, logged with the first differing type and field.
- Host tests build a test module and run the comparison, and include a deliberately mismatched
  table to prove the check fires.

`BuildId` makes mismatches nearly impossible; the layout table makes the remaining case a clean
refusal instead of memory corruption.

## 7. Loading and unloading (`sys/kld`)

A new crate `sys/kld` (FreeBSD's `kern_linker.c` and `kern_module.c` behavior) owns linker files
and modules. The syscalls in `kernel/src/syscall/misc.rs` move onto it.

### 7.1 Load

1. **Policy** (FreeBSD's `kern_kldload`): securelevel ≤ 0, `PRIV_KLD_LOAD`, the file resolved
   along `kern.module_path`, `EEXIST` if a module of that name is loaded or built in (today's
   behavior for built-ins, kept).
2. **Read and parse** with `lib/elf`. The file is untrusted input until verified: parse errors are
   `ENOEXEC`, never a panic, and the parser is fuzzed.
3. **Map** into a kernel VA region reserved for modules: text `RX`, read-only and RELRO data `R`,
   data and BSS `RW`, guard pages between modules. W^X holds at every instant: segments are
   written while mapped `RW`, then remapped.
4. **Relocate**: `base + addend` for each `RELATIVE` entry (decoded by `lib/elf`'s `Relocation`), then
   RELRO goes read-only.
5. **Enter**: call `e_entry(&KERNEL_SERVICES)`. Compare `BuildId` and the layout table.
   Then check `depends`. Any failure unmaps and returns `ENOEXEC` or `ENOENT` (missing
   dependency, as FreeBSD).
6. **Register**: the `DriverDecl`s join the matcher as a new source (DRIVER-MODEL.md §14). The
   host tie check already covers them (§9). The device manager offers every unbound node to the
   new drivers, FreeBSD's `devclass_driver_added`, and attaches through the normal graph order.
   A node a module driver outranks while it is bound to a built-in driver is **not** rebound,
   as on FreeBSD; `devctl set driver` does that explicitly.
7. `kldstat` now shows the file with its modules.

The loader can do steps 2–4 before the kernel runs (`amdgpu_load="YES"` in `loader.conf`), handing
the mapped, unrelocated image over in `bootproto` as FreeBSD's preloaded modules are; the kernel
then runs steps 5–7 at the same point it starts the device manager. This is a later step (§10);
`kld_list` in `rc.conf` is enough for GPU drivers, since nothing before multi-user needs them.

### 7.2 What can point into a module

The kernel reaches module code and data only through these, all created through a binding or the
module record:

| Reference | Created by | Released by |
|---|---|---|
| A `DriverDecl` in the matcher | step 6 | removal from the matcher, under the topology lock |
| A binding (the driver's state, its vtables) | attach | detach (DRIVER-MODEL.md §10.2) |
| Interrupt handlers, callouts, task-queue entries, threads | the binding's context | detach's async teardown step |
| Class publications (`/dev/dri/*` cdevs, sysctl nodes, KMS objects) | the binding | detach's revoke step (gates) |
| `&'static dyn` objects in user-visible handles | a publication | the gate: after revocation no call reaches the object; the handle may outlive the module but holds no module pointer (§7.4) |
| `&'static str` and other borrowed module data | registration | must be copied into kernel memory at registration if it can outlive the module (driver names, sysctl descriptions) |
| Fences and dma-bufs exported to other devices | the binding | GRAPHICS.md §5: a fence's ops vtable lives in the class, not the driver; the driver's state is reached through the binding (§11 question 3) |

Rules enforced by types:

- `kapi` APIs that would retain a module pointer beyond a binding take a `Binding`-scoped token,
  so no other API can capture one. In particular there is no `kapi` function that stores an
  arbitrary `&'static dyn` in a kernel-global structure.
- Module data that a kernel structure must keep past detach (a driver's name in a devclass that
  outlives it, as FreeBSD keeps devclasses) is copied on registration.

### 7.3 Unload

`kldunload` (FreeBSD's `MOD_UNLOAD`, `devclass_delete_driver`):

1. Remove the module's `DriverDecl`s from the matcher so nothing new binds.
2. Detach every binding of each of its drivers, children first, through the framework's fixed
   sequence: revoke publications, stop, tear down async sources, drop, check the ledger
   (DRIVER-MODEL.md §10.2). Detach cannot fail in this model, but a class can decline
   unload while its devices are open. `sys/drm` returns `EBUSY` for an open DRM file, as
   `drm-kmod` does, unless `kldunload -f`, which revokes them (users see `ENODEV`/`SIGBUS`,
   GRAPHICS.md §11).
3. Wait one grace period of every epoch and for the module's **code reference count** to reach
   zero. Each entry from the kernel into module code through a `kapi` vtable (in practice the
   per-binding gate already counts these) holds it; a thread blocked inside the module (a
   sleeping driver call) holds it, and detach's stop step is what returns it.
4. Unmap. Leftover counts after a bounded wait are a framework bug, reported by verifier mode
   (DRIVER-MODEL.md §13.1) with the holder named; the module stays mapped and `kldunload` returns
   `EBUSY` rather than unmapping live code.

The ledger and gates already exist for detach; §7 adds the module code count and the epoch wait.
This is the part of the design that FreeBSD lacks (its KLD unload relies on each driver's detach
being correct) and that the driver model makes tractable.

### 7.4 After unload

User handles that outlive the module (an open `/dev/dri/card0` under `-f`) are gated: calls
return `ENODEV`, mappings fault to `SIGBUS`, and close runs class code only. Reloading the module
rebinds the device as a new attach; old handles stay dead.

## 8. Userland

### 8.1 Syscalls and sysctls

`kldload`, `kldunload`, `kldunloadf`, `kldfind`, `kldnext`, `kldstat`, `kldfirstmod`, `kldsym`
(answers `ENOENT`: modules export no symbols), `modnext`, `modfnext`, `modstat`, `modfind`, and
`kern.module_path`, as FreeBSD 15.1, checked by differential tests of `kldstat -v`,
`kldload`/`kldunload` exit codes and messages, and `kldxref`. Built-in modules keep their current
answers (the kernel file, id 1).

### 8.2 Automatic loading: `devmatch`

FreeBSD loads drivers for unclaimed devices with `devmatch(8)`, run from `devd` on `nomatch`
events and at boot (`rc.d/devmatch`), reading the PNP tables that `kldxref` writes to
`/boot/kernel/linker.hints` from each `.ko`'s `MODULE_PNP_INFO`.

- xtask generates `linker.hints` in FreeBSD's format (`usr.sbin/kldxref/kldxref.c`,
  `sys/sys/module.h` `MODULE_PNP_INFO`) from each module's `pnp` records, which are derived from
  its `const` match table, so the two cannot drift. `kldxref` run on the installed system gives
  the same file, because each `.ko` also carries the records in the section `kldxref` reads
  (§11 question 4).
- Stock `devd` and `devmatch` then load `amdgpu.ko` for an unclaimed RDNA2 function with no
  configuration, as FreeBSD 15.1 does. `devinfo` and `/dev/devctl` already give `devmatch` what it
  reads (DRIVER-MODEL.md §11).
- `kld_list` in `rc.conf` and `*_load` in `loader.conf` work as FreeBSD's
  ([DESKTOP.md](DESKTOP.md) §2 expects `kld_list="amdgpu"`).

### 8.3 Dependencies

`MODULE_DEPEND` names resolve against built-in modules first (always satisfied, as today) and
then loaded ones. Since classes are built in (§5.1), driver modules depend only on the kernel.
Module-to-module dependencies (a module exporting services to another) are not in this design;
they would need a second `kapi` surface published by a module and are deferred until a driver
needs one.

## 9. Testing

- **Host**: the per-architecture tie checks and corpus checks of §3.2; the tie check over each
  table plus every module of that architecture; `lib/elf` relocation and verification against
  generated and hostile objects (fuzzed); the layout comparison including a mismatch; the
  `linker.hints` encoder against files `kldxref` produced on the build server.
- **Boot tier (QEMU)**: a test module (`modules/ktest-dummy`, a driver for a QEMU `pci-testdev` or
  `edu` device) is loaded, attaches, serves I/O, and is unloaded, both ISAs. A load–unload stress
  loop under I/O and with an open device node, with verifier mode on, before any GPU module
  exists.
- **Negative**: a module from another build (`ENOEXEC`), a tampered relocation (`ENOEXEC`), an
  dynamic symbol, an unaligned RELRO, a missing dependency (`ENOENT`), load above securelevel 0 (`EPERM`).
- **Differential**: `kldload`/`kldstat`/`kldunload`/`devmatch -p` transcripts against FreeBSD
  15.1 in bhyve, with a FreeBSD module of the same name where one exists.

## 10. Phasing

| Step | When | Work |
|---|---|---|
| **S1** Per-arch tables | P9 or P10 (before the X870E adds x86-only drivers) | §3: split `kernel/table`, target-scoped dependencies, per-arch host checks, `MODULES` derived from the table. Small; no new subsystem |
| **S2** Boundary design | M-G.0 | `lib/kapi`, the `boundary_types!` layout table, `BuildId`; `sys/drm` designed with `sys/drm/kapi` from the start; `sys/bus` driver-facing API moved into `sys/bus/kapi`. This ADR accepted |
| **S3** Linker and loader | Before M-G.4 | `sys/kld`, the build path of §6, the syscalls of §8.1, the dummy test module, load–unload stress |
| **S4** Unload | With S3 | §7.3 code counts and epoch wait; verifier reporting |
| **S5** devmatch | Before M-G.6 | `linker.hints`, `MODULE_PNP_INFO` records, `devmatch` end to end |
| **S6** First real modules | M-G.4, M-G.5, M-G.6 | Sky1 display aggregate, `panthor`, `amdgpu` built as modules; each also linkable built in for debugging (G6) |
| **S7** Loader preload | When something before multi-user needs a module | `loader.conf` `*_load`, `bootproto` handover |

S2–S7 fall after v1.0, which matches ADR-0005's "revisit loadable modules after v1.0".

## 11. Open questions

1. **`cdylib` on `*-unknown-none`.** Answered by the spike (branch `spike/modules`, `spikes/kmod/README.md`)
   (2026-11-10, Rust 1.99): both built-in targets refuse `cdylib`. A `no_main` bin linked as a
   static PIE works on both ISAs on stable, with `_start` as `e_entry` and no dynamic symbols;
   §6.1 now specifies it. The spike loads, runs, unloads, and refuses modules on x86_64 and
   aarch64 with a `no_std` harness built for the same targets.
2. **Allocator and panic forwarding.** Forwarding works (spike). A per-module allocation ledger
   (for leak reports at unload) cannot count at the forwarding layer, because ownership crosses
   the boundary: the module allocates its `Box<dyn Binding>` and the kernel frees it on detach.
   It must tag allocations by origin. Is that worth a header per allocation? Proposed: yes in
   verifier mode only.
3. **Cross-device objects.** A dma-buf or fence exported by a module driver and imported by a
   built-in one (scanout of a `panthor` buffer on the Linlon display, if they are separate
   modules) must not keep module code alive. GRAPHICS.md §4.4 and §5 should place every such
   object's ops in `sys/drm` with the driver reached only through its binding. To confirm when
   GRAPHICS.md is finalized.
4. **`linker.hints` from `kldxref`.** Must our `.ko` files carry FreeBSD's `set_modmetadata_set`
   sections so that the stock `kldxref` on an installed system regenerates the same hints, or is
   an xtask-generated file that `kldxref` leaves alone enough? FreeBSD's `rc.d` does not run
   `kldxref` at boot, but `make installkernel` and `pkg` scripts do.
5. **KVA placement.** No placement constraint is needed for correctness (no kernel symbols are
   imported), but should modules sit near the kernel for TLB and branch-predictor locality, and
   does that matter at GPU-driver call rates? Measure in S3, don't assume.
6. **Firmware with modules.** A module and its firmware images both live under `/boot`. Should
   `xtask image` install firmware only for modules installed, or always? Probably always; firmware
   is selected by GPU ID at attach (DRIVER-MODEL.md §8.6).
7. **Do any non-GPU drivers want to be modules?** Candidates: Wi-Fi (the X870E's WIFI7), Bluetooth,
   the VPU and NPU. Decide per driver against §5.1, not by default.
