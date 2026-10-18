# Architecture

This document describes how the AbyssBSD kernel is structured. FreeBSD is the behavioral
specification. The internal design is ours, and it is shaped by Rust's ownership model.

## 1. Overall shape

- **Monolithic, statically linked kernel.** It is a single ELF image. Drivers and file systems are
  compiled in and chosen by Cargo features and per-platform configuration crates. A Rust-native
  module system may come later ([ADR-0005](adr/0005-monolithic-static-kernel.md)).
- **SMP and fully preemptible** from Phase 2. There is no "giant lock" stage.
- **Higher-half kernel with a physical direct map (DMAP)** on all architectures, as FreeBSD does.
  User space occupies the lower half.
- **Workspace of many small crates.** Layering is enforced by the crate dependency graph, not by
  convention. Pure-logic crates build for the host so they can use `cargo test` and Miri.

```
            ┌──────────────────────────────────────────────────────────────┐
 user       │  FreeBSD userland (libc, rtld, libthr, base utilities)       │
────────────┼──────────────────────────────────────────────────────────────┤
 syscall    │  abi::<arch>  →  syscall dispatch (generated from            │
 boundary   │  syscalls.master)  →  copyin/copyout (UserPtr<T>)            │
            ├──────────────────────────────────────────────────────────────┤
 kernel     │ proc/thread  signals  exec  sysctl  kqueue  tty  umtx  jail   │
 services   │ vfs(namecache, vnode) ── ufs2  msdosfs  tmpfs  devfs          │
            │ net(ifnet, ether, ip/ip6, tcp/udp, unix, rtsock, netlink)     │
            │ storage(disk, gpt, bio)                                        │
            ├──────────────────────────────────────────────────────────────┤
 core       │ vm(map, object, page, fault, pager) ── slab allocator         │
            │ sched(ULE-like)  sync(spin, mutex, rw, sx, epoch)  time       │
            ├──────────────────────────────────────────────────────────────┤
 device     │ bus model ── acpi(tables, AML) ── pci(ECAM, MSI-X) ── iommu   │
 framework  │ drivers: nvme ahci virtio xhci uart fb nic …                  │
            ├──────────────────────────────────────────────────────────────┤
 HAL        │ arch::x86_64 │ arch::aarch64 │ (arch::riscv64, Day 2)        │
            │ cpu, mmu/pmap, traps, irq ctrl, timers, smp, context switch    │
            └──────────────────────────────────────────────────────────────┘
 firmware      UEFI (boot services → runtime services), ACPI tables
```

## 2. Repository / crate layout (target)

```
AbyssBSD-Rust/
├── Cargo.toml                 # workspace
├── rust-toolchain.toml        # pinned stable toolchain + targets
├── xtask/                     # host build orchestration (std allowed)
├── boot/
│   ├── bootproto/             # loader→kernel handoff types (no_std, versioned)
│   └── loader-efi/            # our UEFI application (PE/COFF)
├── kernel/                    # the kernel binary crate: glue + init order only
├── arch/
│   ├── hal/                   # arch-neutral CPU primitives, bound at link time
│   ├── x86_64/
│   ├── aarch64/
│   └── riscv64/               # Day 2
├── lib/                       # pure, host-testable no_std libraries
│   ├── addr/                  # PhysAddr, VirtAddr, ranges, alignment
│   ├── pt/                    # 4-level page tables, x86_64 and AArch64 encodings
│   ├── logring/               # persistent RAM log ring format
│   ├── demangle/              # Rust v0 and legacy symbol demangling
│   ├── font/                  # the console font (original 8x16)
│   ├── abi/                   # FreeBSD ABI: errno, syscall nums, structs (per arch)
│   ├── elf/                   # ELF parsing (loader + exec + linker)
│   ├── collections/           # intrusive lists, radix trees, rb-trees, bitmaps
│   ├── regs/                  # typed MMIO / sysreg register framework
│   ├── acpi-tables/           # static table parsing
│   ├── aml/                   # AML interpreter
│   ├── gpt/  ufs2/  fat/      # on-disk formats (parsing/serialization only)
│   ├── nettypes/              # packet header types, checksums
│   └── devtree/               # driver matching, dependency graph, devinfo/devctl encodings
├── sys/                       # kernel subsystems (no_std, kernel-only)
│   ├── sync/  vm/  sched/  time/  proc/  signal/  exec/  syscall/
│   ├── sysctl/  kqueue/  tty/  umtx/  cred/  jail/  capsicum/
│   ├── vfs/   fs-ufs/  fs-msdos/  fs-tmpfs/  fs-devfs/
│   ├── storage/  net/
│   └── bus/   irq/   acpi/  pci/  virtio/  usb/  iommu/
├── drivers/
│   ├── nvme/  ahci/  virtio/  xhci/  usb-hid/  usb-msc/
│   ├── uart-16550/  uart-pl011/  fb-console/
│   ├── intc-apic/  intc-gic/  timer-hpet/  timer-generic/  rtc/
│   └── nic-virtio/  nic-e1000e/  nic-igc/
├── tools/
│   ├── syscallgen/            # syscalls.master → Rust tables
│   └── (abi-check lives in xtask; layouts come from lib/abi::layout)
├── tests/                     # QEMU-driven integration and compatibility tests
└── docs/
```

### Layering rules

1. `lib/*` must not depend on `sys/*`, `drivers/*`, or `arch/*`. These crates are pure and run on
   the host for tests.
2. `drivers/*` depend on `sys/bus`, `sys/sync`, `sys/vm` (DMA), and `arch/hal` traits. They never
   depend on a concrete `arch/<isa>` crate.
3. Only `arch/<isa>` crates may contain inline assembly. Only `arch/*`, `lib/regs`, and the DMA/MMIO
   parts of `sys/vm` may perform raw pointer MMIO.
4. `kernel/` is the only crate that names a concrete architecture. It wires the HAL implementation
   into everything else.

## 3. The architecture seam (HAL)

AMD64 and ARM64 differ in almost every privileged mechanism. The seam is defined in `arch/hal` as
traits and associated types, with one implementation per ISA. Selection is compile-time (static
dispatch) and costs nothing at runtime.

**As built (Phase 2).** `arch/hal` declares `extern "Rust"` hooks that the one linked
`arch/<isa>` crate defines (each definition is checked against a function-pointer type in
`hal`). They cover what arch-neutral crates need today: interrupt masking, the CPU number, the
kernel's per-CPU pointer, context creation and switching (`hal::Context`, 16 words),
waiting for an interrupt, and FP/SIMD save areas (`hal::fpu`). On the host the hooks are inert,
so `sys/*` crates still run their unit tests. The kernel crate reaches everything else (page
tables, trap frames, interrupt controllers, timers, SMP start-up) through the concrete arch
crate's modules, which `arch-x86_64` and `arch-aarch64` mirror name for name. The trait sketch
below remains the shape the seam grows into as more of it is shared.

```rust
pub trait Arch: 'static {
    type Pmap: Pmap;                 // page-table management for one address space
    type TrapFrame: TrapFrame;       // saved user/kernel register state
    type FpuState: FpuState;         // lazily or eagerly saved FP/SIMD
    type IrqController: IrqController;
    type CpuLocal;                   // per-CPU data reachable via GS / TPIDR_EL1 / tp

    const PAGE_SIZES: &'static [PageSize];
    const USER_VA: Range<VirtAddr>;
    const KERNEL_BASE: VirtAddr;

    fn current_cpu() -> CpuId;
    fn interrupts() -> &'static dyn InterruptState;   // enable/disable/save/restore
    fn halt_until_interrupt();
    unsafe fn switch_context(from: &mut Context, to: &Context);
    fn enter_user(frame: &Self::TrapFrame) -> !;
}
```

| Concern | AMD64 | ARM64 | RV64 (Day 2) |
|---|---|---|---|
| Firmware handoff | UEFI → our loader | UEFI → our loader. Entry may be at **EL2** (Sky1: VHE-capable): install an EL2 stub, drop to EL1 | UEFI → our loader |
| Per-CPU pointer | `GS_BASE` / `swapgs` | `TPIDR_EL1` | `tp` + `sscratch` |
| Paging | 4-level (LA57 later), PCID | 4 KiB granule, 48-bit VA, TTBR0/TTBR1, ASID | Sv48 (Sv57 later), ASID |
| Syscall entry | `syscall`/`sysret` | `svc #0` → EL1 sync vector | `ecall` → S-mode trap |
| Interrupt controller | x2APIC/xAPIC + IOAPIC (MADT) | GICv3 + ITS (MADT GICC/GICD/GICR/ITS) | AIA: IMSIC + APLIC (MADT) |
| Timer | TSC-deadline / LAPIC; HPET for calibration | Generic Timer (GTDT) | SBI timer / Sstc |
| SMP bring-up | INIT-SIPI-SIPI (MADT) | PSCI `CPU_ON` (FADT/MADT) | SBI HSM |
| Power off / reset | ACPI S5 via AML, FADT reset reg | PSCI `SYSTEM_OFF`/`RESET` | SBI SRST |
| Early console | 16550 (SPCR) + GOP fb | PL011 / SBSA UART (SPCR, else DBG2) + GOP fb | 16550 (SPCR) + GOP fb |
| FP/SIMD state | x87/SSE/AVX/AVX-512 via XSAVE | FP/NEON + **SVE** (and SME later), vector length restored after power-down | V extension |
| Errata | CPUID-keyed table | MIDR-keyed table (e.g. GIC erratum 2941627 on Sky1) | `mvendorid`/`marchid`-keyed table |
| IOMMU (later) | VT-d (DMAR) / AMD-Vi (IVRS) | SMMUv3 (IORT) | RISC-V IOMMU (RIMT) |
| Kernel hardening | SMEP, SMAP, NX, CET-IBT (later) | PAN, PXN/UXN, PAC, BTI (later) | Smepmp, SUM discipline |

Exercising both ISAs from the start keeps x86 assumptions out of the HAL. Examples of what must not
leak: port I/O, `cli`/`sti` semantics, a strongly ordered memory model, and fixed 4 KiB pages.

## 4. Boot flow

1. **`loader-efi`** (Rust UEFI app, `x86_64-unknown-uefi` / `aarch64-unknown-uefi`):
   - reads the kernel ELF from `\EFI\ABYSS\KERNEL` and the command line from
     `\EFI\ABYSS\CMDLINE` on the volume it was loaded from (FAT32 via UEFI SimpleFileSystem; 8.3
     names until `lib/fat` writes long names). Later it can read the kernel from UFS2 directly,
     using `lib/ufs2`.
   - places the kernel (a static PIE linked at `0xffff_ffff_8000_0000`) at a random 2 MiB-aligned
     slide, seeded from `EFI_RNG_PROTOCOL`, else `RDRAND`, else timer jitter, and applies its
     RELATIVE relocations. On aarch64 it cleans the image to the point of coherency.
   - gathers the memory map, RSDP (ACPI 2.0+ only), GOP framebuffer, SMBIOS, UEFI runtime services
     table, command line, the kernel's ELF symbol table (for backtraces), and entropy. It finds
     the serial console through SPCR, then DBG2 (COM1 on x86 when neither exists).
   - sets up the persistent log ring. Its address is kept in an NV variable; if the ring survived
     a reset, the previous boot's log is saved to `\EFI\ABYSS\PREVLOG.TXT`.
   - builds the initial page tables: every RAM range identity-mapped and in the DMAP, the kernel
     segments W^X at their slid addresses, and the console devices in the device window.
   - `ExitBootServices`, then calls the kernel's entry point at its physical address with
     `(BootInfo, kernel_root, identity_root)`.
2. **Kernel early init** (single CPU, no allocator): the entry stub enables NX and the PAT (x86)
   or leaves EL2 through a resident EL2 stub, then programs MAIR/TCR/TTBRn (arm64). It switches
   to the loader's tables and jumps to the linked address. Rust code then installs the GDT, TSS
   and IDT (x86) or the EL1 vectors (arm64) and the per-CPU pointer, validates the handoff,
   starts the serial and framebuffer consoles, ingests the memory map, and builds and switches to
   the kernel's own tables. Those have the DMAP, a W^X image with an unmapped boot-stack guard
   page, and no identity map. UEFI runtime services are called in physical mode under a separate
   table that maps the runtime regions 1:1 and shares the kernel half (as FreeBSD's `efirt`
   does).
3. **Core init**: slab allocator, ACPI static tables (MADT, MCFG, SPCR, FADT, HPET/GTDT, SRAT/SLIT),
   interrupt controller, timers, SMP bring-up, scheduler start.
4. **Device init**: AML namespace load (DSDT/SSDTs), `_OSC`, bus enumeration (ACPI + PCIe), driver
   attach.
5. **Mount root** (UFS2 by GPT partition UUID/label, or a memory disk). Then `exec("/sbin/init")`.

The kernel virtual layout is the same on both architectures (`bootproto::layout`): DMAP at
`0xffff_8000_0000_0000` (64 TiB), kernel virtual memory from `0xffff_c000_0000_0000`, the device
window at `0xffff_f000_0000_0000`, and the kernel image at `0xffff_ffff_8000_0000` plus the slide.

`bootproto` is a separate crate with explicit versioning, so the loader and kernel can evolve
independently and the kernel can reject handoffs it does not understand.

## 5. Core subsystems

### 5.1 Synchronization (`sys/sync`)

Modeled on FreeBSD's lock classes, with Rust guards and lock ordering checked in the type system:

| Primitive | FreeBSD analogue | Notes |
|---|---|---|
| `SpinLock<T>` | `mtx` (spin) | Disables interrupts/preemption; for IRQ-shared state |
| `Mutex<T>` | `mtx` (default) | Adaptive spin, then turnstile with priority propagation |
| `RwLock<T>` | `rwlock` | Turnstile-based, non-sleepable holders |
| `SxLock<T>` | `sx` | Sleepable shared/exclusive |
| `Epoch` / `Rcu<T>` | `epoch(9)` | Read-mostly structures (ifnet lists, routing, namecache) |
| `Atomic*`, `SeqLock` | `atomic(9)`, `seqc` | Timekeeping, stats |
| `WaitQueue`, `Condvar` | `sleepqueue`, `cv` | Sleep/wakeup with signal interruptibility as a typed result |

**As built (Phase 2).** `SpinLock` (interrupt-masking ticket lock) and `Once` live in
`sys/sync`. The blocking locks (`Mutex`, `RwLock`, `SxLock`, `Epoch`) and `WaitQueue` live in
`sys/sched` (`sched::lock`), because they are built on the scheduler. Each lock belongs to a
`LockClass` with a declared level and sleep rule. The witness checks every acquisition and every
sleep at runtime, in all builds. The compile-time level tokens and context tokens below are not
built yet.

- **Lock levels as types.** Each lock is declared with a level (`Mutex<L3, T>`). Acquiring a lock
  requires a token proving that only lower levels are held. Most ordering bugs fail at compile time.
- **Runtime witness** (`WITNESS` equivalent) in debug builds catches what the types cannot, such as
  dynamically chosen locks and sleeping with a spinlock held.
- **Context typing.** Functions that may sleep take a `&Sleepable` context token. Interrupt
  handlers receive an `IrqContext` that cannot produce one.

### 5.2 Memory (`sys/vm`)

We follow FreeBSD's Mach-derived design, adapted to Rust ownership:

- **Physical**: buddy allocator over per-NUMA-domain free lists (`vm_phys`). Page metadata lives
  in a `VmPage` array. `Pfn`, `PhysAddr`, and `VirtAddr` are distinct newtypes.
- **Kernel allocation**: slab/zone allocator (UMA-equivalent) with per-CPU caches, which also backs
  `alloc::GlobalAlloc`. Typed zones (`Zone<T>`) return `ZBox<T>`.
- **Address spaces**: `VmMap` (an ordered map of `VmEntry`), `VmObject` (anonymous, vnode,
  device, physical), and shadow chains for COW. `Pmap` is the per-arch page-table backend behind
  the HAL trait.
- **Fault handling**: `vm_fault` resolves through the map and object chain, using `Result` types
  that separate "retry", "signal SIGSEGV/SIGBUS", and "kernel bug".
- **User memory access**: only through `UserPtr<T>` / `UserSlice<T>`, which can be consumed solely
  by fallible `copyin`/`copyout` using arch fault-recovery tables. Kernel code cannot dereference
  user pointers by construction.
- **Unified buffer/page cache** for file data (FreeBSD's VMIO model). Metadata is cached through a
  small buffer cache API used by UFS2 and msdosfs.
- **Paging out / swap**: Phase 6+. The design leaves room for it from the start (pager trait on
  `VmObject`).

### 5.3 Scheduling (`sys/sched`)

**As built (Phase 2):**

- **Run queues:** one per CPU, 64 FIFO levels with a bitmap. Idle CPUs steal from the busiest
  CPU. A woken thread goes to its last CPU, or to the least-loaded CPU, preferring higher
  capacity.
- **Priority classes:** fixed priorities below 120 (interrupt, real-time, kernel).
  Time-sharing threads, from 120 up, take their priority from decayed CPU usage plus a waiting
  bonus, which bounds how long any of them can starve. The idle class is at the bottom.
- **Preemption:** time slices are 10 ms on the per-CPU one-shot timer, and an idle CPU stops
  its timer (tickless).
- **Switching rule:** a thread is queued only once it has left its CPU, so a switching CPU
  never waits on another.
- **Topology:** read from the PPTT or CPUID, with capacities set by core type.

- A ULE-inspired design: per-CPU run queues, interactivity scoring, timeshare/realtime/idle classes,
  and topology-aware load balancing.
- **Heterogeneous cores are first-class.** The topology (from ACPI PPTT, CPPC/`_CPC`, and on x86
  CPUID hybrid leaves/HFI) feeds capacity-aware placement.
- Tickless idle and high-resolution event timers (FreeBSD `eventtimers` + `callout` semantics).
- Threads are kernel objects. Processes are containers (address space, file table, credentials,
  signal state), as in FreeBSD.

### 5.4 Time (`sys/time`)

- A `timecounter` abstraction (TSC, ARM generic counter, HPET fallback), driven by NTP-disciplined
  `timehands`.
- The **shared page** exports `vdso_timehands` so FreeBSD libc's userspace `clock_gettime` fast
  path works.

## 6. Device model (`sys/bus`)

The driver model is specified in [DRIVER-MODEL.md](DRIVER-MODEL.md)
([ADR-0012](adr/0012-driver-model.md)), and the survey behind it is
[DRIVER-MODEL-SURVEY.md](DRIVER-MODEL-SURVEY.md). In summary:

- **FreeBSD's userland view, byte for byte.** Device names and units, `hw.bus.*`, `dev.*`,
  `/dev/devctl` events, `/dev/devctl2` ioctls, `/dev/pci`, and the pnpinfo and location strings
  that devd rules match on.
- **The framework owns the lifecycle.** Drivers implement `attach` (claim resources, build the
  instance), `start` (install handlers, publish), and `stop` (quiesce the hardware). Detach is a
  fixed framework sequence: revoke publications, stop, tear down async sources, drop, check the
  ledger. Every way into a driver is created through its binding, so detach can drain all of them.
- **Three lifetimes**: the device node (`DeviceRef`), the binding (owned by the manager), and
  user-visible handles (cdevs, disks, ifnets), which reach the driver only through a revocation
  `Gate`.
- **Matching is `const` data** with FreeBSD's `BUS_PROBE_*` ranks. A build fails if two drivers
  match the same device at equal rank. Each bus gives drivers a typed child view in place of ivars.
- **One dependency graph** built from ACPI (`_DEP`, `_CRS` resource sources, power resources, PCC,
  IORT) orders attach, shutdown, suspend, and resume, replacing bus passes. Early devices
  (consoles, interrupt controllers, timers) are initialized before the tree and adopted into it.
- **Declared concurrency**: drivers are serialized by default and concurrent by opt-in. Context
  tokens (`IrqCx`, `ThreadCx`) separate filters from sleepable code. Nothing on the data path hops
  threads.
- **Resources are typed grants** from a platform broker: `Mmio<R>` register blocks, `IrqLine` and
  MSI-X vectors from an `IrqDomain` hierarchy (MSI → ITS → GIC, MSI → LAPIC), and DMA tags with
  direct, bounce, and IOMMU backends. `DmaCoherent<T: DeviceShared>`, a `DmaStream` ownership
  typestate, and pre-mapped pools keep DMA safe and fast. Driver crates are
  `#![forbid(unsafe_code)]`.
- **Classes** (storage, net, tty, console, input) own protocol logic and user interfaces behind
  batch-first, queue-shaped driver traits. Requests are completed exactly once, by move.

### ACPI

We write our own ACPI implementation (no ACPICA):

- `lib/acpi-tables`: zero-copy, validated parsing of RSDP/XSDT, MADT, FADT, MCFG, SPCR, DBG2, HPET,
  GTDT, SRAT, SLIT, PPTT, PCCT, IORT, DMAR, IVRS, BGRT, and later RHCT/RIMT for RISC-V.
- `lib/aml`: an AML interpreter (parser, namespace, method execution, operation regions, `Notify`,
  mutexes, events). It is host-testable against DSDT/SSDT dumps collected from real machines
  (`acpidump` on FreeBSD). Operation-region spaces are a trait implemented by the kernel:
  SystemMemory, SystemIO, PCI_Config, EmbeddedControl, GenericSerialBus, and
  **PlatformCommChannel (PCC)**. PCC matters because on Sky1 the clock methods talk to the SCMI
  firmware through PCC mailboxes. Firmware bugs such as references to nonexistent objects
  produce typed, logged errors, never panics.
- `sys/acpi`: OS services glue (`_OSC`, `_OSI`, power button via GPE/SCI or GED, S5 shutdown,
  power resources `_PRx`/`_PSx`, EC, PCC channels from PCCT). Later: `_LPI`, CPPC, and thermal
  zones.

The AML interpreter is one of the highest-risk components, so it gets its own fuzzing and corpus.

## 7. Process model and syscalls

- `syscalls.master` is the source of truth. `tools/syscallgen` generates the syscall number enum,
  argument structs, and a dispatch table. Unimplemented entries return `ENOSYS`, and the first
  use of each is logged.
- Syscall handlers have the signature `fn(&mut ThreadCtx, Args) -> SysResult<Ret>`, where `Errno` is
  an enum with FreeBSD's numeric values. The arch layer encodes success and failure into the native
  convention (carry flag + `rax`/`x0`).
- `exec`: an ELF64 image activator with FreeBSD branding checks (`NT_FREEBSD_ABI_TAG`, `ELFOSABI`),
  PT_INTERP (`/libexec/ld-elf.so.1`), auxv, ps_strings, the shared page, and ASLR.
- Signals: FreeBSD `sigframe`/`ucontext_t` layouts per arch, `sigreturn`, alternate stacks, and
  `SA_RESTART` semantics.
- Credentials, jails, Capsicum, and resource limits are first-class from Phase 6. Many base
  utilities enter capability mode.

## 8. Storage and file systems

- `sys/storage`: a small GEOM replacement. A `BlockDevice` trait provides async `Bio` requests,
  GPT partition providers, and `/dev` nodes named FreeBSD-style (`nda0p2`, `ada0p1`, `vtbd0p3`) so
  stock `fstab` conventions and tools work.
- `sys/vfs`: vnodes as refcounted `Arc<Vnode>` with a typed `VnodeKind` enum, a namecache with
  negative entries, mount points, and a FreeBSD-compatible `struct stat`, `statfs`, and `dirent`.
- File systems: **devfs**, **tmpfs**, **msdosfs** (FAT12/16/32 + VFAT LFN), and **UFS2** (read →
  write → soft updates → SU+J). On-disk formats live in host-testable `lib/` crates. For UFS2 we
  check against FreeBSD's `fsck_ffs` on images round-tripped through the build server.

## 9. Networking

- `ifnet` with an epoch-protected interface list, mbuf-equivalent packet buffers (`Packet` with
  typed headroom), and Ethernet, ARP, IPv4, IPv6/ND, ICMP, UDP, TCP (NewReno + CUBIC), and loopback.
- Sockets: UNIX domain (stream/dgram/seqpacket, SCM_RIGHTS, SCM_CREDS) early, because base daemons
  need them. INET/INET6 come later.
- Control plane: routing socket (`PF_ROUTE`) and **netlink** (FreeBSD 14+ `ifconfig`/`route` use
  it), plus the `ioctl` set used by `ifconfig`.
- Drivers: virtio-net first, then Intel e1000e (I219), igc (I225/I226), and later ixgbe/ice or
  Realtek, depending on target hardware.

## 10. Debugging and observability

- Serial console from the first instruction after the loader. Panic dumps include a symbolized
  backtrace (embedded symbol table generated by `xtask`).
- A GDB remote stub over serial, plus the QEMU gdbstub during bring-up.
- An in-kernel debugger (DDB-like) with commands for threads, locks, VM maps, and the device tree.
- `ktrace`-compatible syscall tracing (FreeBSD `kdump` works unmodified), and later a DTrace-like
  facility.
- Kernel statistics exported via sysctl so `top`, `vmstat`, `systat`, and `netstat` work.
