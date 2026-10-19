# Roadmap

The roadmap is a sequence of **phases with hard exit criteria**. A phase is done when its exit
criteria pass in CI on **both AMD64 and ARM64**, not when the code is written. Sizes are relative
(S < M < L < XL) rather than calendar estimates.

**Performance parity** ([ADR-0011](adr/0011-performance-parity.md)). From P2 on, every exit
criterion includes the phase's benchmark suite: on each benchmark, AbyssBSD is **no more than 2%
slower** than stock FreeBSD 15.1 on the same machine and configuration, and the goal is to be
faster. Suites are cumulative and run nightly after the phase that introduces them. Method,
fairness rules, and the full benchmark list: [PERFORMANCE.md](PERFORMANCE.md). Before P6 the
gating platform is bhyve (amd64). aarch64 numbers are recorded, but only gate from P6, when real
ARM64 hardware is in the loop.

**QEMU first** ([ADR-0010](adr/0010-qemu-first-development.md)). Through P5, development and every
exit criterion run under QEMU (plus bhyve for amd64). **No physical hardware is touched until
FreeBSD userland runs end to end in QEMU**: no smoke checks, no table captures. At P6 the physical
reference machines are brought up and become **gating**.

```
P0 Foundations ─► P1 Boot ─► P2 Core kernel ─► P3 Devices & ACPI ─► P4 First userspace
                                                      │                    │
                                                      │                    ▼
                                                      │             P5 VFS & file systems
                                                      │                    │
                                                      ├─(parallel)─► USB/HID track
                                                      ▼                    ▼
                                                 P6 FreeBSD multi-user ◄──┘
                                                      │
                                                      ▼
                                                 P7 Networking ─► P8 Self-hosting & hardening = v1.0 (Day 1)
                                                                                │
                                                                                ▼
                                                                         P9 RV64 (Day 2)
                                                                                │
                                              ┌─────────────────────────────────┼──────────────────┐
                                              ▼                                 ▼                  ▼
                                    M-U Userland milestone        M-G Graphics milestone   M-D Desktop milestone
```

---

## Phase 0: Foundations (S) — done 2026-10-17

**Goal:** an empty but complete engineering loop.

- Cargo workspace, `rust-toolchain.toml` (pinned stable, edition 2024, the four targets), lint
  configuration, `rustfmt.toml`, and CI policy checks (no external deps, `SAFETY:` comments).
- `xtask` skeleton: `build`, `run`, `test`, `image` (FAT32 ESP via our own `lib/fat` writer),
  and `--remote` execution on the build server.
- `tools/syscallgen` producing `lib/abi` syscall tables from `../freebsd-src/sys/kern/syscalls.master`.
- `cargo xtask abi-check` pipeline proven on a handful of structs (`timespec`, `stat`, `sigaction`).
- Build server provisioned (rustup, qemu, edk2, bare git repo, CI hook).
- In-house test harness for kernel tests (`#[ktest]` collection via linker section).

**Exit:** `cargo xtask test --tier unit` passes on both hosts. `cargo xtask run --arch {x86_64,aarch64}`
boots a stub UEFI app that prints "AbyssBSD" on QEMU.

## Phase 1: Boot to Rust on both architectures (M) — done 2026-10-18

**Goal:** our loader hands off to our kernel, which owns the machine.

- `loader-efi`: file loading from the ESP, ELF loading with KASLR, memory map, GOP, RSDP, SMBIOS,
  runtime services table, `bootproto::BootInfo` v1, initial page tables, `ExitBootServices`.
- Kernel entry per arch: stack, BSS, per-CPU block, trap/exception vectors (with full register
  dumps), early serial (16550 / PL011 from SPCR, or DBG2 when there is no SPCR) and framebuffer
  text console. On arm64, EL2 entry with an EL2 stub and a drop to EL1.
- Panic handler with a symbolized backtrace. Kernel `log!` facility, mirrored into a persistent RAM ring
  that the loader saves to the ESP on the next boot (the X870E has no serial port).
- Physical memory map ingestion. Kernel's own page tables with the DMAP. W^X kernel mappings.
- `#[ktest]` runner, and QEMU exit-code signalling.

**Exit:** both arches boot in QEMU to a kernel banner on serial and framebuffer, run kernel unit
tests, deliberately take and report a page fault, and exit cleanly. Boot also works on bhyve.

## Phase 2: Core kernel (L) — done 2026-10-18

**Goal:** SMP, preemptive, timed, and memory-managed.

- Physical allocator (buddy, NUMA-aware data structures even if single-node). Slab/zone allocator
  with per-CPU caches, used as `GlobalAlloc`.
- Kernel virtual memory: `kmem` arena, guard-paged kernel stacks.
- ACPI static tables: MADT, FADT, MCFG, SPCR, HPET, GTDT, SRAT, SLIT, PPTT.
- Interrupt controllers: x2APIC/IOAPIC; GICv3 distributor and redistributors.
- Timekeeping: timecounters (TSC / generic counter), event timers, callouts, tickless idle.
- SMP bring-up (INIT-SIPI / PSCI), per-CPU data, IPIs, TLB shootdown.
- Synchronization: spin, mutex (turnstiles with priority propagation), rw, sx, epoch, wait
  queues. Lock-level types and runtime witness.
- Scheduler v1 (ULE-like, kernel threads only), preemption, CPU topology including asymmetric
  capacities (the Orange Pi 6 Plus has three core classes).
- CPU errata framework (MIDR/CPUID-keyed). Investigate GIC erratum 2941627 for Sky1.
- FP/SIMD context switching including SVE.

**Exit:** a stress test of 64 kernel threads doing lock ping-pong, allocator churn, and sleeps with
timeouts runs for 10 minutes on `-smp 8` on both arches, with no witness violations and no leaks
(allocator accounting balances). The P2 performance suite (context switch, locks, zone and page
allocators, callouts, IPIs and TLB shootdown) is within 2% of FreeBSD's `kbench` module on bhyve.

**As delivered.** `cargo xtask test --tier stress` runs the exit test on both architectures
(10 minutes, 8 CPUs, 64 threads). The threads do lock ping-pong, ordered multi-lock
contention, sx locking, allocator churn, timed sleeps, and FP/SIMD register checks. The run
passes with no witness violations and with heap, page, and kernel-stack accounting balanced.
Where Phase 2 departs from the plan above:

- **Lock order:** levels are declared per lock class and enforced by the runtime witness on
  every acquisition, in all builds. Compile-time level tokens are not built yet.
- **NUMA:** the allocator keeps per-domain free lists, but domains from the SRAT are not wired
  in yet. Every machine so far is one node.
- **Interrupt controllers:** the GICv3 ITS and I/O APIC routing come with MSI and devices in
  Phase 3. HPET is parsed but unused; the TSC is calibrated against the ACPI PM timer.
- **CPU capacities:** set by core type until CPPC (`_CPC`, Phase 3) supplies real performance
  levels.
- **GIC erratum 2941627:** the errata framework and the GICD_IIDR boot log are in place. The
  erratum itself is to be assessed against Arm's published errata notice at Sky1 bring-up
  (P6), since only that hardware can confirm it.
- **Performance (measured in P3.0).** `cargo xtask perf` compares the P2 suite with FreeBSD
  15.1's `kbench` module in identical pinned bhyve VMs, 10 runs per side. No benchmark exceeds
  the bound: 16 are faster with a confidence interval below 1.0, 2 tie, and the geometric mean
  ratio is 0.63. 18 contended or latency benchmarks vary by more than 1% from run to run on the
  FreeBSD side (up to 11%), so by PERFORMANCE.md's rule they cannot resolve 2%. One of those,
  `rw.rd.t4`, has a median ratio above 1.02 (1.036, with a confidence interval spanning 1.0).
  Getting there took a round of fixes:
  - critical sections in place of interrupt masking;
  - depot magazines in the heap and per-CPU page caches;
  - caller-owned timers with no allocation;
  - lazy timer reprogramming;
  - lock-word rw/sx locks and FreeBSD-style mutex spinning;
  - ULE's idle spin.
  The aarch64 numbers are not yet recorded.

## Phase 3: Device framework, PCIe, ACPI AML, storage (L/XL)

**Goal:** discover and drive real hardware through the driver model of
[DRIVER-MODEL.md](DRIVER-MODEL.md) ([ADR-0012](adr/0012-driver-model.md)).

**Scope.** Everything in DRIVER-MODEL.md §16's P3 row, plus the AML interpreter, platform services,
and the storage drivers below. Two items move earlier than DRIVER-MODEL.md §16 placed them: the
threaded-only interrupt domain for ACPI GED (the aarch64 power button needs it), and the kernel
sysctl tree (`hw.bus.*` and `dev.*` need a home before the `__sysctl` syscall arrives in P4).

**Reference machines.** Every step is tested on the same device configurations:
- **x86_64:** QEMU `q35`.
- **aarch64:** QEMU `virt`.
- **bhyve:** on the build server.

Each configuration has virtio-blk, two NVMe controllers (one with two namespaces), an AHCI disk, and
virtio-rng. FreeBSD 15.1 guests on the build server, in bhyve and QEMU with the same configurations,
supply two things: `kbench` baselines, and recorded `devinfo -rv`, `sysctl dev`, `pciconf -lv`, and
devd event logs for differential tests.

### Milestones

Each milestone ends green on both architectures, with CI passing.

```
P3.0 ─► P3.1 ─┬─► P3.2 ─────────┐
              └─► P3.4 ─► P3.5 ─┴─► P3.6 ─┬─► P3.7 ─┐
P3.3 (from the start, pure crate) ────────┘   P3.8 ─┴─► P3.9
```

**P3.0 Benchmark harness, and closing Phase 2.**
- `tests/perf/kbench`: a FreeBSD kernel module, built on the server against the 15.1 sources.
- An in-kernel harness that prints the same format.
- `cargo xtask perf`: runs both in identical bhyve VMs and compares the results.
- The P2 suite at ≤2% on bhyve (aarch64 numbers recorded under TCG).
- The FreeBSD reference VMs, and the first recordings from them.

**P3.1 Groundwork for the model.** Done 2026-10-18.
- The sched locks take a `Sleepable` proof, and the context tokens (`IrqCx`, `ThreadCx`) exist.
- A typed register-block macro in `lib/regs`, prototyped against the NVMe and GICv3 register maps
  (DRIVER-MODEL.md open question 4).
- `DeviceShared`, with explicit padding fields and a `const` size assertion (open question 5).
- Per-CPU counters, for gates and for dispatch epochs.
- The kernel sysctl tree: typed nodes with FreeBSD's OID numbering.
- As delivered: `regs::register_block!` with `Mmio<B>` (the GICv3 driver uses it);
  `device_shared!`; `sched::Sleepable` and `sched::IrqCx` (the binding contexts follow in P3.5);
  `sync::PerCpuCounter`; and the `sys/sysctl` tree with the identity and `hw.*` boot nodes.
  `abi-check` now also verifies the sysctl constants against FreeBSD's headers.

**P3.2 `lib/devtree`** (pure, host-tested). Done 2026-10-18.
- Match ranking, and the ambiguity check, wired into `cargo xtask check`.
- The dependency graph: cycle breaking, topological order, and the "what is blocking whom" report.
- Unit allocation.
- pnpinfo and location strings, `u_businfo`/`u_device` encoding, and the devctl event grammar, all
  compared byte for byte with the P3.0 recordings.
- Resolves open question 2 (unit numbering after a failed attach) from FreeBSD's source.
- As delivered: matching with PCI, ACPI, and virtio IDs; the dependency graph (fuzzed); units;
  pnpinfo, location, `u_device`, and devctl formats, checked byte for byte against FreeBSD 15.1's
  `devinfo -rv` and `pciconf -lv` recorded in bhyve (`cargo xtask --remote freebsd-record`). Open
  question 2's answer: a failed attach frees the unit unless the devclass was fixed or
  `hw.bus.disable_failed_devices` is set, and a hint-disabled device keeps its unit. Running the
  ambiguity check over each build's driver table in `cargo xtask check` waits for that table
  (now P3.6).

**P3.3 `lib/aml`** (pure, host-tested, fuzzed; runs in parallel with P3.0–P3.2). Done 2026-10-18.
- Parser, namespace, and interpreter.
- Operation regions behind a trait: SystemMemory, SystemIO, PCI_Config, and PCC. EmbeddedControl,
  GeneralPurposeIo, and GenericSerialBus are hooks.
- `_OSI` answered as FreeBSD answers it; `Notify`, `Mutex`, and `Event`.
- Resource-template decoding for `_CRS` and `_PRS`.
- Malformed AML is an error, never a panic, and loops are bounded.
- Corpus: the DSDT and SSDTs of QEMU `q35`, `pc`, and `virt`, and of bhyve, plus the existing Radxa
  Q8B capture. Physical-machine captures wait for P6.
- **As delivered.** `lib/aml` executes AML straight from the table bytes: loading runs module-level
  code, method bodies are parsed when invoked, and a failure inside a scope body during loading
  becomes a `LoadWarning`. Every region space goes through the `Host` trait (integer accesses,
  buffer transfers for SMBus/IPMI/GenericSerialBus, GPIO hooks, PCI addresses derived through
  bridges), with access widths, update rules, `IndexField`/`BankField` sequencing, and the lock
  rule. `_OSI` answers FreeBSD's list (the Windows strings, not `Linux`), `_REV` is 2, and slack
  mode is on, as in FreeBSD. Resource templates decode to typed descriptors (pin descriptors are
  kept as `Unknown`); device queries cover `_HID`, `_CID`, `_UID`, `_ADR`, `_STA`, `_CRS`, `_PRT`,
  and `_DEP`. All five corpus DSDTs load with no warnings, and every Q8B device answers `_STA` and
  `_CRS`. Nesting is bounded by measured stack use: a level costs ~0.5 KiB, so the default 10 KiB
  budget (for a 16 KiB kernel stack) runs the QEMU and bhyve firmware, but the Q8B's AML needs
  ~13 KiB and should run on a dedicated thread with a 32–64 KiB stack (`Limits::for_stack`).
  Not done: `Load`/`LoadTable`/`Unload` (return `Unsupported`), real concurrency for `Wait` and
  `Acquire` timeouts (the interpreter is single-threaded), and SSDTs in the corpus (none
  captured yet).

**P3.4 Interrupts and DMA.**
- `sys/irq`, split out of the arch crates.
  - Domains: LAPIC vectors, I/O APIC, GIC distributor, GICv3 ITS, PCI MSI/MSI-X, and the
    threaded-only domain for GED.
  - Filters and ithreads; direct dispatch, drained by epoch; storm detection; affinity from the
    topology.
- `sys/vm::dma`.
  - Tags that inherit constraints; Direct and Bounce backends.
  - `DmaCoherent`, the `DmaStream` typestate, and `DmaPool`.
  - Cache maintenance for non-coherent devices (`_CCA`).
- As delivered (2026-10-18): `sys/irq` with the I/O APIC and GIC SPI domains, filters, interrupt
  threads, synchronous removal, and storm detection, proven on the console UART's interrupt on
  both architectures; `vm::dma` with direct and bounce backends, `DmaCoherent`, the `DmaStream`
  typestate, and `DmaPool`, host-tested and run on the kernel's memory (including a forced
  bounce). The GICv3 ITS and the PCI MSI/MSI-X domain move to P3.6, where PCI devices first exist
  to exercise them; the threaded-only GED domain comes with the ACPI bus in P3.6.

**P3.5 Device manager (`sys/bus`).** Done 2026-10-18.
- Node table with generational IDs, the lifecycle state machine, and the topology lock at a witness
  level.
- Parallel attach workers (`abyss.bus.serial=1` forces one).
- `Binding`, its ledger, `Gate`, the binding serializer, and the contexts.
- The detach protocol and the attach-failure paths.
- The platform root, adopting the early console, interrupt controllers, and timers.
- The resource broker: resource spaces, claims of firmware-reserved ranges, and refusal of
  sensitive ranges.
- `hw.bus.*`, `dev.*`, and `kern.abyss.bus.*`.
- Shutdown ordering.
- Verifier mode with seeded fault injection.
- Logic tested on the host against a fake bus.
- As delivered: the node table, matching with decline and fallback, units chosen at match time,
  dependency-graph gating (a named `Need` from attach becomes a graph edge), parallel attach
  workers, the `Driver` trait with `DriverDecl` erasure for the build's driver table, bindings with
  the grant ledger, `MmioGrant`/`IrqLine`/`DmaTag` grants, `Gate`, the binding serializer, the
  detach protocol, and devctl `+`/`-`/`?` events. `hw.bus.info`, `hw.bus.devices` (read through
  a new sysctl handler node, called outside the tree lock), `dev.<driver>.<unit>.%…`, and
  `kern.abyss.bus.*`. The platform root is `root0 → nexus0` (plus `apic0` on amd64), as FreeBSD
  shows it, through `Manager::adopt`. Interrupt controller and early-console ranges are refused to
  drivers. Shutdown detaches consumers before suppliers; kernel-owned nodes stay. Verifier mode
  (`abyss.verifier`, or `Manager::inject_faults`) fails grants from a seed. The manager needs the
  scheduler, so instead of a host fake bus it is tested in ktests with a test bus and drivers that
  decline, fail, wait on a slower sibling, and run under injected faults, with `abyss.bus.serial=1`
  in the aarch64-el2 boot case. The ambiguity check over the driver table moves to P3.6, with the
  first drivers that carry real PCI and ACPI IDs. Adopting the console UART and the timers as
  devices comes with their drivers (P3.8).

**P3.6 Buses.** Done 2026-10-18.
- `sys/acpi`.
  - Namespace load at boot, the `_INI`/`_STA` walk, and `_OSC`.
  - Nodes from `_HID`/`_CID`/`_UID`/`_ADR`, and `_CRS` decoded into resource lists.
  - Dependency edges from `_DEP` and resource sources; power resources (`_PR0`, `_PS0`).
  - `_PRT`; SCI and GPEs on x86, GED on aarch64.
  - `_CPC` for CPU capacities.
- `sys/pci`.
  - Host bridges from MCFG and `PNP0A08`; ECAM; depth-first enumeration.
  - BAR validation, capabilities, and bus-owned config-space fields.
  - MSI/MSI-X, with INTx through `_PRT` as the fallback.
- The virtio-pci modern transport, as a bus.
- As delivered:
  - **ACPI.** `acpi0` has one child per `_HID`/`_CID` device, in namespace order, matching FreeBSD
    15.1's `devinfo` on bhyve exactly. Every child is added first and the edges after, so a
    `_DEP` or resource source (interrupt, GPIO, or serial bus controller) may name a later device;
    on the Radxa Q8B's DSDT that is 32 of 107 edges. A dependency cycle is broken and logged.
    `_PR0` power resources (shared, counted) and `_PS0` bring each device to D0 first.
  - **ACPI host bridges.** `_OSC` is asked exactly as FreeBSD asks: support bits, no control.
    `_PRT` routes INTx, directly or through link devices.
  - **ACPI events.**
    - amd64: `acpi0` takes the SCI, enters ACPI mode through `SMI_CMD` if needed, and handles the
      fixed power button and every runtime GPE (`_Lxx`/`_Exx`).
    - arm64: `acpi_ged` runs `_EVT` from threaded handlers.
    - `Notify` reaches handlers after each AML job. `acpi_button` powers off on a press.
    - Tested end to end: xtask presses the button over QMP. On q35 it arrives as the fixed event;
      on arm64 virt through GED → `_EVT` → `Notify` → `acpi_button`.
  - **`_CPC`.** Levels from integers or memory/I/O registers set the CPU capacities.
  - **PCI.**
    - `pcib`/`pci` pairs for host bridges and PCI-PCI bridges: the generic bridge swizzles INTx,
      and the ACPI bridge uses its `_PRT`. QEMU's virtio-rng now sits behind a root port on both
      architectures.
    - Drivers see configuration space through `PciChild`. They may write device-specific
      registers only; the header, capability links, and the MSI and MSI-X capabilities belong to
      the bus.
  - **MSI/MSI-X.** One `PCI_MSI` domain. The message comes from the local APIC on amd64 and from
      the GICv3 ITS on arm64; the ITS maps devices, events, and LPIs, with device IDs from IORT.
      `hw.pci.enable_msix=0` keeps INTx, and the arm64 EL2 boot case uses it so INTx stays tested.
      MSI-X tables are unmapped when their last vector goes.
  - **virtio.** Two `virtio_pci` drivers, as in FreeBSD 16: modern at the default rank, legacy as
    the low-priority fallback for bhyve's legacy-only devices. Each adds one child that gets a
    `VirtioDevice` and split virtqueues hardened against a misbehaving device. `vtrnd` (the
    entropy device) is the first child driver: interrupt-driven, published behind a gate. It is
    tested by reads by MSI-X on q35, arm64, and bhyve, and by a detach/re-attach cycle that checks
    the mappings are returned. That cycle also found grants a bus's deleted children still held.
  - **Driver table.** It is its own host-buildable crate. `cargo xtask check` fails on any
    unresolved tie over recorded and per-entry witness identities.
  - **Not yet.**
    - bhyve's power button (SIGTERM) is not pressed by the harness.
    - `_CPC` levels in FFH or PCC are skipped.
    - Power resources are never switched off.
    - GED is a driver, not an interrupt domain other devices' interrupts route through. No
      firmware seen needs that.
    - If the GIC will not snoop its tables, MSI stays off (cache maintenance for that waits for
      hardware, P6).

**P3.7 Storage.** Done 2026-10-18.
- `sys/storage`.
  - The batch `DiskQueues` trait, and owned `Bio`s completed exactly once.
  - The per-CPU deferred resubmit list.
  - The GEOM-lite disk provider, and FreeBSD names (`nda`, `ada`, `vtbd`).
- Drivers:
  - `vtblk`.
  - `nvme`: an admin queue, and per-CPU I/O queues on MSI-X.
  - `ahci`, whose `ahcich` children publish `ada`, matching FreeBSD's names.
- The GPT read and partition dump, through `lib/gpt`.
- As delivered:
  - **`sys/storage`.**
    - `Bio`s are completed exactly once; one dropped uncompleted is completed with `EIO` and counted.
    - `DiskQueues` takes batches per hardware queue. What does not fit waits, in order, on a
      per-queue list until the driver reports room; with a queue per CPU, that is the per-CPU
      resubmit list.
    - Disks are published behind a gate with FreeBSD's names and announcement. Requests are
      checked for alignment, range, and size.
    - Every disk is tasted for a GPT, and each partition is a disk of its own (`vtbd0p1`).
  - **Drivers.**
    - `vtblk`.
    - `nvme`: polled admin commands, I/O queue pairs per CPU on MSI-X (one on INTx), PRP lists,
      and namespaces as `nda`.
    - `ahci`/`ahcich`: channels named as FreeBSD names them, disabled where not implemented. NCQ
      when the drive supports it; disks as `ada`.
  - **Determinism.** Units are now deterministic under parallel attach: attaches run in parallel,
    and `start` (where buses add children) runs in depth-first tree order.
  - **Shared interrupts.** Level-triggered interrupt lines can be shared (INTx), which the arm64
    EL2 case needs for AHCI.
  - **Tests.** Every boot case has blank virtio-blk, NVMe, and AHCI disks. Each is written and
    read back, including 256 concurrent requests. The runs cover MSI-X on q35, arm64 virt, and
    bhyve, and shared INTx in the EL2 case.
  - **FreeBSD match.** On bhyve these match FreeBSD 15.1's `devinfo` line for line: `vtblk0`,
    `vtblk1`, `nvme0`, `ahci0`, `ahcich0`, and the 31 disabled channels.
  - **Not yet.**
    - A dropped `Bio` is counted but not yet reported with its driver's name in verifier mode.
    - Abort callbacks for requests on hardware.
    - 4Kn GPT (lib/gpt reads 512-byte sectors only).

**P3.8 Platform services.** Done 2026-10-18.
- Entropy: RDRAND, RNDR, and virtio-rng feeding an fxrng-style CSPRNG.
- RTC: `atrtc` on x86; PL031 or `efirtc` on aarch64.
- UEFI runtime services.
- ACPI reset, and `_S5` or PSCI `SYSTEM_OFF`.
- The power button, triggering an ordered shutdown and then power-off.
- Watchdogs.
  - `ichwd` runs in QEMU on `q35`.
  - The SBSA generic watchdog (with Sky1's `WOR` quirk) and the AMD FCH watchdog are tested on the
    host against fake register blocks. The SBSA watchdog also boots on QEMU `sbsa-ref` if that
    firmware is available; otherwise it is validated at P6.
- As delivered:
  - **Entropy.** `lib/crypto` has ChaCha20 and BLAKE2b, checked against the RFC vectors.
    `sys/random` follows fxrng: a fast-key-erasure root, reseeds through BLAKE2b, and per-CPU
    generators.
    - It is seeded at boot from the loader's EFI_RNG bytes and RDSEED/RDRAND or RNDR.
    - A harvester thread keeps feeding it from the CPU's RNG, vtrnd, and timing.
  - **Clocks.** `sys/clock` holds the real-time clock class. `atrtc` and `efirtc` (UEFI
    `GetTime`) set the time of day, and the ktest checks they agree. QEMU's `virt` describes no
    PL031 in ACPI, so arm64 uses `efirtc`.
  - **Power.** Power-off and reset go through ACPI S5 or the reset register on amd64, PSCI on
    arm64, then UEFI as the fallback.
    - Each is tested: two boot cases end with a real power-off, and the warm-reset cases reboot
      through the reset register or PSCI.
    - An orderly power-off (the power button) runs `_PTS(5)`, then the device shutdown.
  - **Watchdogs.** `sys/watchdog` holds the watchdog class.
    - `isab`/`isa` add `ichwd` on Intel and `amdsbwd` on AMD, as FreeBSD's identify routines do.
    - `ichwd` counts down and is patted on q35. A boot case lets it expire and checks the VM
      reset.
    - `sbsa_gwdt` (with Sky1's refresh-through-`WOR` detection) and `amdsbwd` are host-tested
      against fakes. There is no `sbsa-ref` firmware here, so both wait for P6.
  - **Adopted devices.** `uart`, `attimer`, `hpet`, and `acpi_timer` attach under FreeBSD's names
    and claim nothing yet. On bhyve every attached ACPI child matches FreeBSD's `devinfo` line for
    line.
  - **Not yet.** PL031; the console UART's hand-over to a tty driver (P4); `/dev/fido` and
    `/dev/random` (devfs, P4).

**P3.9 Verification and performance.** Done 2026-10-19.
- The detach stress test: virtio-blk and NVMe detached and re-attached under random I/O, with
  ledger and allocator checks after every cycle.
- The verifier on in the CI boot and stress tiers.
- Differential name tests against the FreeBSD recordings.
- The P3 suite: interrupt latency; IOPS at queue depths 1 and 32 with p50/p99 latency; sequential
  bandwidth; AML namespace load and `_STA`/`_CRS` evaluation; gate cost at 1 and 8 CPUs; attach
  time; DMA map throughput; and detach-cycle time.
- As delivered: the detach stress test (vtblk1 and nvme0, 8 cycles each, under I/O) and the
  verifier run in every boot case. The perf suite `p3` (tests/perf/README.md) has 17 benchmarks:
  block QD1 p50/p99, QD32, and sequential reads on vtbd and nda; AML namespace load (q35's DSDT as
  an SSDT) and `_STA`/`_CRS`; HPET-to-handler interrupt latency; DMA map; gate cost at 1 and 8
  CPUs; and vtblk attach and detach-cycle time. An audit of the order-of-magnitude wins fixed
  harness bugs that handicapped FreeBSD (no GEOM direct dispatch, interrupts spread across CPUs,
  vtbd1 left mounted, console output in attach) and one that favored AbyssBSD (it idle-spun where
  FreeBSD halted; both now read CPUID leaf 0x1F and neither spins on SMT siblings). Five runs on
  bhyve: every benchmark is faster than FreeBSD (geometric mean 0.34). The largest leads are
  design differences, each traced on FreeBSD with DTrace: newbus zeroes every candidate driver's
  softc (about 9 MB per PCI attach); ACPICA spends about 70% of a table load in its OS mutex
  layer; `dev_refthread` takes a per-cdev mutex.
- Known issues, accepted for now: FreeBSD's p99 tails (QD1 reads, interrupt latency) are
  unexplained beyond lying in its idle wake-up path, possibly the event timer's idle mode; and
  FreeBSD's `blk.vtbd.seq` varies 100% run to run.

**Exit:** on both arches:
- The kernel enumerates PCIe through the device manager and attaches NVMe, AHCI, and virtio-blk
  with FreeBSD's device names and units, matching the FreeBSD recordings.
- It reads the GPT and dumps partition tables.
- It sustains a multi-queue random-read stress test.
- It passes the detach stress test with balanced ledgers.
- It powers off via ACPI or PSCI when the power button is pressed (QEMU `system_powerdown`), after
  quiescing devices in reverse dependency order.

Also required:
- The AML corpus and `lib/devtree` pass on the host.
- CI runs with the verifier on.
- The P3 performance suite is within 2% of FreeBSD on bhyve.


## Phase 4: First userspace: `/rescue/sh` (L)

**Goal:** an unmodified, statically linked FreeBSD binary runs.

- VM: `VmMap`/`VmObject`/COW shadow chains, page-fault handler, `mmap`/`munmap`/`mprotect`/`madvise`,
  user-copy primitives (`UserPtr<T>`), and ASLR.
- Processes and threads: `fork` (COW), `execve` (ELF64, FreeBSD branding, auxv, shared page,
  ps_strings), `exit`/`wait4`, sessions and process groups.
- Syscall entry per arch with generated dispatch. Tier A syscall set
  ([COMPATIBILITY.md §2.1](COMPATIBILITY.md#21-system-calls)).
- Signals: delivery, frames, `sigreturn`, masks, default actions.
- Minimal VFS over a **memory disk** (preloaded image) with a read-only in-memory FS, plus devfs
  with `console`, `null`, `zero`, and `random`.
- TTY layer v1 on the serial console (line discipline, termios, job control signals).

**Exit:** both arches boot to an interactive `/rescue/sh` (from FreeBSD 15.1 `base.txz`) on the
serial console. Pipelines, job control (`^Z`, `fg`), `ls`, `cat`, `sysctl kern.ostype`, and
`/rescue/ps` work. A boot-to-shell expect test is in CI. The P4 performance suite (syscall, process,
VM-fault, signal, and pipe microbenchmarks, run as the same static binaries on both kernels) is
within 2% of FreeBSD.

## Phase 5: VFS and file systems (L)

**Goal:** real persistent storage.

- Full VFS: vnode lifecycle, namecache (with negative entries), mount table, `nmount`, permissions
  and ACL hooks, `stat`/`statfs`/`getdirentries` fidelity, and file locking (`flock`, `fcntl`
  advisory locks).
- Unified buffer/page cache, write-back with a syncer thread, and `fsync` semantics.
- Storage stack: `BlockDevice`, GPT providers, FreeBSD device naming (`nda*`, `ada*`, `vtbd*`).
- **tmpfs** and **devfs** (complete).
- **msdosfs** read/write (FAT12/16/32, LFN). Mount the ESP at `/boot/efi`.
- **UFS2** read, then write (without soft updates; clean unmount required), then **soft updates**.
  **SU+J** follows in Phase 8.
- Validation: images written by AbyssBSD must pass FreeBSD `fsck_ffs -f` and `fsck_msdosfs` on the
  build server with zero errors, under a randomized-workload stress test.

**Exit:** both arches boot with the root on UFS2 on NVMe and `/rescue/sh` as init. A file-system
stress workload (fsx-style, run unmodified from FreeBSD's tests) completes, and the image passes
`fsck_ffs` afterwards. The loader can read the kernel from UFS2. The P5 performance suite (namecache,
directory, UFS2 and tmpfs I/O, `fsync`, `base.txz` extraction) is within 2% of FreeBSD.

## Track: USB and input (parallel, starts after P3) (M)

- xHCI (rings, contexts, MSI-X), hub driver, device enumeration, HID boot protocol, then report
  parser (keyboard, mouse), USB mass storage (BBB, then UAS), and virtio-input.
- Framebuffer virtual terminals (`ttyv*`) with a `vt(4)`-compatible keyboard layer.

**Exit (merges into P6):** a physical machine without a serial port can be driven entirely from the
GOP console and a USB keyboard.

## Phase 6: FreeBSD multi-user (XL)

**Goal:** stock FreeBSD `/sbin/init` → `rc` → multi-user `login:`.

- Dynamic linking: `rtld` (`ld-elf.so.1`) runs. `mmap` semantics for shared objects, PT_INTERP,
  RELRO, and TLS (`sysarch` FSBASE, `TPIDR_EL0`).
- `libthr`: `thr_new`/`thr_exit`/`thr_kill`, and the full `_umtx_op` set including robust and
  shared-memory mutexes.
- Tier B syscalls, sysctl tree (boot-critical and tooling sets), `kqueue` (all filters used by base),
  pipes, UNIX domain sockets with `SCM_RIGHTS`/`SCM_CREDS`, ptys (`posix_openpt`, `/dev/pts`), and
  `devctl`/`devd` events.
- Credentials (setuid exec, `issetugid`), resource limits, `procctl`, **Capsicum** (capability mode
  and rights), **jails** (basic: `jail_set`/`attach`, path and hostname isolation), and securelevel.
- Shared page `vdso_timehands` (fast `clock_gettime`). `ktrace` (so `kdump` works). `reboot`/
  `shutdown` paths.
- `kinfo_proc`/`kinfo_file`/`procstat` sysctls so `ps`, `top`, `fstat`, and `procstat` work.
- `abi-check` covers every struct crossing the boundary.
- kyua test suite runs nightly, with the baseline recorded.

**Exit:** unmodified FreeBSD 15.1 `base.txz` boots **multi-user** from UFS2 on both arches (QEMU,
bhyve, and **both physical reference machines**, which gate from this phase on). Login works on the serial console and on `ttyv0`.
`syslogd`, `cron`, and `devd` run. The kyua pass rate meets the agreed target for kern/, lib/libc,
bin/, and usr.bin/. The P6 performance suite (boot time, `libthr`/umtx, `kqueue`, UNIX sockets, ptys,
dynamic exec) is within 2% of FreeBSD, and every earlier suite now also holds on both physical
machines against FreeBSD on bare metal.

## Phase 7: Networking (XL)

**Goal:** a networked FreeBSD system.

- `ifnet`, packet buffers, Ethernet, ARP, IPv4, ICMP, IPv6 + ND + SLAAC, UDP, TCP (RFC 9293
  baseline, SACK, window scaling, timestamps, NewReno + CUBIC), loopback, `bpf` (for `dhclient`
  and `tcpdump`).
- Socket API completeness for INET/INET6, `sendfile`, multicast basics.
- Control plane: routing tables (FIB), routing socket, **netlink** (`NETLINK_ROUTE` +
  `NETLINK_GENERIC` as used by 15.x `ifconfig`/`route`/`netstat`), and interface ioctls.
- Drivers: virtio-net (multi-queue, checksum/TSO offload), e1000e, igc, and Realtek RTL8126 (the
  Orange Pi 6 Plus has no other NIC).
- Differential tests against the build server for TCP behavior (packet captures).

**Exit:** both arches obtain an address with `dhclient` and SLAAC. `sshd` accepts logins.
`fetch`/`pkg update && pkg install <curated set>` succeed. Sustained `iperf3` runs complete without
stalls or leaks. The P7 performance suite (TCP/UDP throughput, request/response latency, connection
rate, `sendfile`, CPU per gigabit) is within 2% of FreeBSD over loopback, virtio-net, and the physical
NICs.

## Phase 8: Self-hosting and hardening = **v1.0 / Day 1 complete** (L)

- **Self-hosting:** `pkg install rust` on AbyssBSD, then build AbyssBSD on AbyssBSD (amd64 and
  arm64) and boot the result.
- UFS2 **SU+J**, `fsck` on unclean boot, and swap (swap pager on a GPT partition).
- IOMMU (VT-d, AMD-Vi, SMMUv3 with IORT RMR identity maps) with busdma integration. Watch for the
  SMMUv3 event-queue interrupt storm reported on Sky1 under Linux.
- CPU idle and frequency: `_LPI` (with PSCI power-down states and a broadcast event timer, e.g.
  Sky1's CIX GPT), CPPC (SystemMemory + AMU counters, per `_PSD` domain), `_CST`, HWP. ACPI thermal
  zones with per-domain passive cooling. SCMI clients over PCC.
- Security hardening: SMEP/SMAP/PAN enforced, KASLR, stack canaries in the loader, kernel W^X
  audit, speculative-execution mitigations, and arm64 PAC/BTI if stable-toolchain support allows.
- Suspend/resume (S3 / s2idle) is a **stretch goal**.
- Performance pass: lock contention profiling and scheduler tuning on hybrid cores, aimed at
  beating FreeBSD rather than matching it. The P8 suite adds self-hosted build time, SU+J
  metadata workloads, and swap throughput.
- Documentation: installation guide, supported-hardware list, compat-deviations list.

**Exit (v1.0):** the success criteria in [VISION.md](VISION.md#success-criteria-for-day-1-v10) are
met.

## Phase 9: RV64, Day 2 (L)

- `arch/riscv64`: S-mode entry from UEFI, Sv48 paging, traps, SBI (timer, IPI, HSM, SRST), and the
  AIA interrupt controllers (IMSIC/APLIC), all from ACPI (RHCT, MADT RINTC/IMSIC/APLIC).
- `abi` for FreeBSD riscv64 (struct layouts, signal frames, and the syscall convention:
  `ecall`, `t0` number, `a0..a7` arguments).
- Boot FreeBSD riscv64 `base.txz` multi-user on QEMU virt (`acpi=on`). Real hardware follows as
  UEFI+ACPI RV64 boards become available.

**Exit:** the P6 exit criteria for RV64 on QEMU. Every HAL change the port needed is recorded as
an ADR or follow-up, which tells us how well the seams held.

---

## Later milestones (post Day 1)

### M-U: Userland

- Decide which base components to reimplement in Rust: init/rc replacement, libc (FreeBSD-ABI
  compatible), core utilities, and the pkg toolchain. Keep binary compatibility as the contract so
  stock and new components can mix.
- Rust `std` support for an AbyssBSD target (or keep `x86_64-unknown-freebsd` as the userland
  target, since we are ABI-compatible).

### M-G: Graphics

- Kernel modesetting interface (KMS-equivalent: CRTCs, planes, connectors, atomic commits, dma-buf
  style buffer sharing, sync files). Decide between FreeBSD/Linux DRM ioctl compatibility and a
  new interface. The decision drives which userland GPU stacks can be reused.
- Drivers: GOP-backed simple KMS, then virtio-gpu (2D, then 3D/venus), then a native GPU (Intel Xe
  or AMD RDNA; one first, chosen by reference hardware).
- evdev-compatible input interface (likely needed by a Wayland stack).

### M-D: Desktop

- An original Wayland compositor with dynamic tiling, workspaces, animations, IPC, and declarative
  configuration (in the spirit of Hyprland, with new code and design).
- Wayland protocol library, renderer (software → Vulkan), panel/launcher/notifications,
  terminal emulator, and a minimal toolkit.
- Built for BSD, not ported to it ([VISION](VISION.md) goal 11 applies here most of all). Most
  existing desktop stacks assume Linux: systemd/logind for seats and sessions, udev for hotplug,
  epoll/timerfd/signalfd event loops, D-Bus as the system bus. Ours is designed for AbyssBSD's
  interfaces from the start: kqueue event loops, devd for hotplug, Capsicum for sandboxing
  clients, sysctl and rc for configuration and services. Portability to other systems is a
  non-goal.
- Prerequisites: M-G, plus audio (HDA/USB audio, later) and power management maturity.

### M-Z: OpenZFS

- Host the upstream **OpenZFS** C code on our kernel rather than rewriting it. A Rust ZFS would
  be a project of its own, and OpenZFS's on-disk format and pool semantics are defined by that
  codebase in practice.
- The work is the plumbing: a Solaris Porting Layer equivalent (memory, locks, condvars, taskqs,
  threads, time, kstats, the `vnode`/`uio` shapes ZPL expects), a VFS adapter so ZPL datasets
  mount like any other file system, a GEOM-style vdev provider over our disk stack, and the
  `/dev/zfs` ioctl interface so stock `zpool`/`zfs` binaries work unmodified.
- Needs its own ADR before work starts: it breaks the no-C rule (ADR-0002/0003 build policy),
  brings CDDL code into a BSD-2-Clause tree (kept in a separate, clearly licensed directory, as
  FreeBSD does), and must fit the static-kernel model (ADR-0005), so it is linked in at build time
  rather than loaded as `zfs.ko`.
- Prerequisites: VFS and buffer/page cache maturity (P5), GEOM-equivalent disk providers (P3),
  and SMP-safe sleep locks.

---

## Key risks

| Risk | Impact | Mitigation |
|---|---|---|
| AML interpreter correctness on real firmware | Devices missing, no power off, hangs | Large `acpidump` corpus, fuzzing, ACPICA-compatible `_OSI` answers, early real-hardware testing |
| Breadth of the FreeBSD ABI (sysctl/ioctl/netlink long tail) | P6/P7 stall on small issues | Syscall/ioctl usage telemetry (log the first use of anything unimplemented), kyua and differential tests to prioritize |
| VM + UFS2 + buffer cache interaction (data corruption) | Lost data, loss of trust | Host-tested on-disk crates, `fsck_ffs` validation on every CI image, crash-consistency tests (kill the VM mid-write) |
| Weak memory ordering bugs only visible on ARM64 | Heisenbugs | Both arches gate every merge, real ARM64 hardware early, Miri/loom-style in-house model checks for lock-free code |
| No external crates means everything is built here | Scope creep | Keep `lib/*` crates minimal and purpose-built, and favor the simplest correct design |
| Rust stable-toolchain gaps (e.g. arm64 branch protection) | Hardening delayed | Track upstream. ADR required for any nightly opt-in |
| Performance below FreeBSD's | AbyssBSD is not a credible replacement | The 2% bound gates every phase from P2 ([ADR-0011](adr/0011-performance-parity.md)), so losses are caught in the layer that causes them. Nightly cumulative suites. Hot-path designs weighed against ARM64 costs before P6 makes arm64 gating |
| Physical hardware variety | Slow v1.0 | A small set of Tier 1 machines, SystemReady-only on ARM64 |
| The X870E holds user data (Fedora NVMe, RAID array) | Catastrophic data loss | Non-designated disks attach read-only until P8. `xtask deploy` identifies the media NVMe by serial number. AbyssBSD never writes the Fedora ESP or boot entries |
| No serial console on the X870E | Blind debugging of early hangs | Framebuffer console, persistent RAM log saved by the loader, FCH watchdog, an optional PCIe serial card or polled network log |
| Bring-up on the Orange Pi 6 Plus costs power cycles | Slow iteration | QEMU first. Serial console from day one, the watchdog in P3. Capture the Linux/FreeBSD ground truth (ACPI dumps, register state) before the board is reimaged, and read it before experimenting |

## Open decisions

Decided items are struck through. Each decision is recorded as an ADR.

1. ~~Physical reference machines~~: decided. ARM64 is the Orange Pi 6 Plus
   ([ADR-0008](adr/0008-arm64-reference-board-orange-pi-6-plus.md)). AMD64 is the Ryzen 9 7900X /
   X870E ([ADR-0009](adr/0009-amd64-reference-machine-ryzen-x870e.md)). Remaining: install the
   AMD64 machine's second NVMe, and decide whether to add a PCIe serial card to it.
2. ~~`alloc` in the kernel, `std` in host tooling~~: accepted ([ADR-0002](adr/0002-dependency-policy.md)).
3. ~~Primary ABI target~~: accepted, FreeBSD 15.x until 16.0-RELEASE ([ADR-0004](adr/0004-abi-target.md)).
4. ~~Disk space on the Orange Pi 6 Plus~~: decided. The board is reimaged for AbyssBSD when its
   hardware phase arrives.
5. **Hosting / forge**: self-hosted on the build server initially, with public hosting later.
6. **Graphics ABI** (DRM-compatible or new), decided at the start of M-G.
7. **Drivers ported from Linux**: leaning against adopting FreeBSD's LinuxKPI compatibility layer
   (and so against porting `drm-kmod`, `iwlwifi`, and similar). Instead, rebuild each driver in
   Rust for our kernel's semantics. Vendor firmware blobs are treated as part of the hardware:
   loaded and talked to, never reimplemented. Drivers are written from published specifications,
   with clean-room reverse engineering where none exist (observing the device and documenting it,
   never reading GPL driver source, per [ADR-0007](adr/0007-license-and-clean-implementation.md)).
   Support a limited set of hardware and expand organically, driven by the reference machines.
   Decided by ADR at the start of M-G, the first milestone that would need it.
