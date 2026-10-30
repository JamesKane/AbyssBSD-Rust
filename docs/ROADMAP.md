# Roadmap

The roadmap is a sequence of **phases with hard exit criteria**. A phase is done when its exit
criteria pass in CI on **both AMD64 and ARM64**, not when the code is written. Sizes are relative
(S < M < L < XL) rather than calendar estimates.

**Performance parity** ([ADR-0011](adr/0011-performance-parity.md),
[ADR-0014](adr/0014-performance-parity-per-component.md)). From P2 on, every exit criterion includes
the phase's benchmark suite: each component it measures is, overall, **no more than 2% slower**
than stock FreeBSD 15.1 on the same machine and configuration, and the goal is to be faster. Suites are cumulative and run nightly after the phase that introduces them. Method,
fairness rules, and the full benchmark list: [PERFORMANCE.md](PERFORMANCE.md). Before P8 the
gating platform is bhyve (amd64). aarch64 numbers are recorded, but only gate from P8, when real
ARM64 hardware is in the loop.

**QEMU first** ([ADR-0010](adr/0010-qemu-first-development.md)). Through P5, development and every
exit criterion run under QEMU (plus bhyve for amd64). **No physical hardware is touched until
FreeBSD userland runs end to end in QEMU**: no smoke checks, no table captures. That is P6's
exit. P7 adds networking, still in QEMU and bhyve, so the machines come up with `ssh` and `scp`;
P8 brings up the physical reference machines, and they are **gating** from then on.

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
                                                 P7 Networking
                                                      │
                                                      ▼
                                                 P8 Board bring-up ─► P9 Self-hosting & hardening = v1.0 (Day 1)
                                                                                │
                                                                                ▼
                                                                         P10 RV64 (Day 2)
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

## Phase 3: Device framework, PCIe, ACPI AML, storage (L/XL) — done 2026-10-19

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

**As delivered.** Every exit criterion runs in CI on both architectures (`cargo xtask test --tier
boot`, local QEMU and the build server's bhyve, verifier on in every case):
- PCIe enumeration attaches `nvme`, `ahci`, and `vtblk` under FreeBSD's names and units, and the
  differential tests match the bhyve recordings line for line.
- GPT tables are read and dumped for every disk.
- Every disk takes 256 concurrent requests across the per-CPU queues, and the detach stress test
  (vtblk1 and nvme0 detached under random reads) passes with balanced ledgers.
- `system_powerdown` runs `_PTS(5)`, quiesces devices in reverse dependency order, and powers off
  through ACPI S5 or PSCI.
- The AML corpus and `lib/devtree` pass on the host.
- The P3 suite: all 17 benchmarks faster than FreeBSD on bhyve (geometric mean 0.34, P3.9).

Carried into P4.0: the aarch64 P2 and P3 numbers (recorded under TCG, not gating) are still not
recorded, and the two known issues of P3.9 stay open.


## Phase 4: First userspace: `/rescue/sh` (L) — done 2026-10-19

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

**Reference configuration.** The P3 machines (QEMU `q35` and `virt`, bhyve). Userland is
FreeBSD 15.1's `base.txz` for each architecture, fetched and unpacked on the build server; its
`/rescue` (one static, hard-linked binary) goes into a memory-disk image that the loader preloads.

### Milestones

Each milestone ends green on both architectures, with CI passing.

```
P4.0 ─► P4.1 ─► P4.2 ─► P4.3 ─┬─► P4.4 ─┐
                              └─► P4.5 ─┴─► P4.6 ─► P4.7
```

**P4.0 Userland harness, and closing Phase 3.**
- `tests/perf/ubench`: the P4 suite as static FreeBSD C programs, built on the server, printing the
  `kbench` line format. `cargo xtask perf --suite p4` runs them on FreeBSD now and on AbyssBSD
  once P4.7's syscalls are in.
- `cargo xtask` fetches `base.txz` (amd64 and arm64) on the server, checks its digest, and builds
  the memory-disk image holding `/rescue`.
- The expect harness for boot-to-shell tests (a scripted serial session with timeouts), proven
  against FreeBSD 15.1 booted in bhyve and QEMU.
- The aarch64 P2 and P3 numbers recorded under TCG.
- As delivered (2026-10-19):
  - **`ubench`** (`tests/perf/ubench/ubench.c`, spec in `tests/perf/README.md`): 12 benchmarks,
    one static binary bound to CPU 1. Unbound, FreeBSD put each exec'd child on an idle CPU, and
    `proc.vfork_exec` measured that CPU's wake-up.
  - **`perf --suite p4 --freebsd-only`** records FreeBSD's side. First baseline on bhyve (3
    runs): `getpid` 240 ns, `fork` 24.7 µs, `exec` 137 µs, an anonymous page fault 6.0 µs, pipe
    ping-pong 17.9 µs.
  - **`fetch-base`** pins `base.txz`'s SHA-256 per architecture and builds the `/rescue` ustar
    image (151 names, one binary).
  - **`freebsd-shell`** runs the boot-to-shell script (`xtask/src/expect.rs`) against FreeBSD's
    `/rescue/sh` in single-user mode, where init gives the shell the console, as AbyssBSD's boot
    will. All 12 steps pass, `^Z`/`jobs`/`fg`/`^C` included.
  - **Settled in P4.7:** `getpid` cost 240 ns in the guest against 67 ns on the host because the
    guest ran with PTI, which bare-metal FreeBSD does not use on this CPU.
  - **Not done:** the aarch64 TCG numbers. FreeBSD's arm64 VM image is now downloaded and its
    checksum verified. What remains is building `kbench.ko` for arm64 and giving the perf harness
    a QEMU TCG path for both kernels.

**P4.1 Address spaces** (`sys/vm`, pure parts host-tested). Done 2026-10-19.
- `VmMap` (an ordered map of `VmEntry` with protections, inheritance, and `MAP_*` flags) and
  `VmObject` (anonymous, physical) with shadow chains for COW, collapse included.
- `vm_fault` returning a typed outcome: mapped, retry, `SIGSEGV`, `SIGBUS`, or kernel bug.
- The per-arch `Pmap` behind the HAL: user page tables in the lower half, PCID on amd64 and ASIDs
  on arm64, and shootdowns limited to the CPUs an address space is active on.
- `UserPtr<T>`/`UserSlice<T>` with `copyin`/`copyout`/`copyinstr` recovered through the arch fixup
  tables; SMAP/PAN on where the CPU has it.
- Tested in kernel by mapping, faulting, forking and tearing down address spaces from kernel
  threads, with page accounting balanced after each.
- As delivered:
  - `sys/vm` holds the pure core, host-tested against a simulated MMU with page accounting:
    `map` (entries, clipping, first fit, `munmap`, `mprotect`, `minherit`, `fork` by
    inheritance), `object` (anonymous objects, shadow chains, collapse, FreeBSD's one-mapping
    rule for freeing on `munmap`), and `fault::resolve`.
  - `kernel/src/mm/space.rs` pairs a map with page tables, under a sleepable map lock.
  - Each CPU keeps the last user space loaded across kernel threads (lazy switching, as
    FreeBSD does); `sched::unload_address_space` lets it go.
  - Page faults reach the VM from user mode or from the copy routines only, then fall back to
    the fixups.
  - SMEP and SMAP on amd64, PAN on arm64. The copy routines use `rep movsb` (with `stac`/`clac`)
    and `ldtr`/`sttr`.
  - Kernel tests on both architectures cover faults through the copies, `EFAULT` at the
    bounds, copy-on-write between a parent and a child space, concurrent faults from every
    CPU, and a cross-CPU unmap that must leave no stale TLB entry.
  - Not yet: PCID on amd64 (every switch between user spaces flushes the non-global TLB), and
    16-bit ASIDs (8-bit, with ASID 0 shared and flushed when they run out).

**P4.2 User mode and syscalls.** Done 2026-10-19.
- Per-arch entry and exit: amd64 `syscall`/`sysret` (with the non-canonical `rcx` guard) and
  traps from ring 3; arm64 `svc #0` and EL0 exceptions. User FP/SIMD state, including SVE.
- Dispatch generated by `tools/syscallgen` (handlers `fn(&mut ThreadCtx, Args) -> SysResult<Ret>`),
  errno in the native carry convention, `syscall(2)`/`__syscall(2)` indirection, and `ENOSYS`
  logged on first use.
- `sysarch` TLS (amd64 `fsbase`/`gsbase`, arm64 `tpidr_el0`).
- First user code: a test binary built here (no libc) that calls `write` and `exit`.
- As delivered:
  - **amd64.** `syscall`/`sysret`, with `iretq` whenever `sysret` could not restore the frame.
    `swapgs` on traps from ring 3, and a `GS_BASE` check for NMI, double fault, and machine
    check, which can arrive anywhere.
  - **arm64.** `svc` and EL0 exceptions, with `SP_EL0` in the trap frame.
  - **sched.** A user thread's kernel stack (the TSS `rsp0`) and its thread pointer (FS base
    via `rdfsbase`/`wrfsbase` where present, else the MSR; `TPIDR_EL0`) are switched with it.
    Kernel threads pay one branch.
  - **Dispatch.** A match on the generated `Sysno`, the carry convention, and
    `syscall(2)`/`__syscall(2)`. Unimplemented calls return `ENOSYS` and log their first use
    (FreeBSD also sends `SIGSYS`; that comes with signals).
  - **Calls so far.** `exit`, `write` (fds 1 and 2 to the console until P4.5), and `sysarch`'s
    FS and TLS bases. `errno` values and the `sysarch` operations are checked by `abi-check`.
  - **Tests.** Machine code in `arch/*` runs in user mode on both architectures: `write`, an
    unknown call, and `exit` with the expected status. An amd64 program sets its TLS base and
    reads it through `fs:`.
  - **I-cache.** On arm64, executable pages are synced to the instruction cache of every CPU
    before they are mapped.
  - **Not yet.** User GS bases on amd64; arm64's own `sysarch` operations; a test of user
    FP/SIMD state across switches, which waits for real binaries (P4.3).

**P4.3 Processes and exec.** Done 2026-10-19, apart from three items that belong to later work (see Left below).
- `Proc` and `Thread` as in ARCHITECTURE.md §5.3: pid allocation, process groups, sessions, the
  process tree, and credentials (root only until P6).
- `execve`: ELF64 with FreeBSD branding (`NT_FREEBSD_ABI_TAG`, `ELFOSABI_FREEBSD`), auxv,
  `ps_strings`, the shared page (signal trampoline and `vdso_timehands`), stack and mmap ASLR.
- `fork` (COW), `pdfork`, `vfork`, `exit`, `wait4`, zombies and reparenting to init.
- The rest of Tier A that needs no files: `getpid`, the `getuid` family, `issetugid`, `getrandom`,
  `clock_gettime`, `nanosleep`, `getrlimit`, `thr_self`, `_umtx_op` (wait and wake), `minherit`,
  `madvise`, `mmap`/`munmap`/`mprotect`, and `__sysctl`/`__sysctlbyname` over the P3.1 tree.
- Progress (2026-10-19): FreeBSD 15.1's own `/rescue` runs on both architectures. The ktests run
  `/rescue/echo`, and `/rescue/sh -c` forking and exec'ing `/rescue/echo` and `/rescue/sysctl
  kern.ostype hw.ncpu`, waiting for each, and exiting with the status it was given.
  - **Memory disk** (moved up from P4.5, since exec needs files): the loader preloads
    `EFI/ABYSS/MD`, the `/rescue` ustar image, read by `lib/ustar`.
  - **Pager objects**: a file's pages fill on first touch and stay cached, shared by every
    process running it (FreeBSD's vnode object).
  - **Done**: the process tree, groups and sessions, `fork`, `vfork`, `execve`, `wait4`, the ELF
    activator and FreeBSD's initial stack, and the system calls listed above. The kernel tests
    found two bugs. First, `STAR` lacked RPL 3: on AMD, user code then ran with SS 0x18, and its
    next trap faulted on `iretq` with the user's GS live. Second, aarch64 rejected user addresses
    with bit 47 set.
  - **Shared page**: mapped read-only at `USRSTACK`. It holds `vdso_timekeep`, which a timer
    updates at least four times per wrap of the 32-bit count (on amd64, the TSC shifted under
    2^31 Hz). libc reads it through `AT_TIMEKEEP` without a system call, and the kernel's clock
    calls read it the same way, so the two always agree. `tests/user/vdso.c` checks this from
    user mode on both architectures: each libc reading falls between two system-call readings,
    and readings stay monotonic across updates. Under QEMU/KVM, a libc read costs about 18 ns.
    Test programs from `tests/user/` are now built into the memory disk.
  - **Timed waits**: `_umtx_op` waits take a relative or absolute timeout on any clock and fail
    with `ETIMEDOUT` (`tests/user/umtx.c`). The kernel test runs every `tests/user` program.
  - **ASLR** follows FreeBSD 15's 64-bit defaults (`kern.elf64.aslr.*`).
    - `anon_loc` is random between `vm_daddr + RLIMIT_DATA` and the middle of user space.
    - The stack and the shared page are placed above the data limit, with random gaps of up to
      4096 pages; the stack's top also moves within its page.
    - The top page of user space is a guard.
    - `RLIMIT_DATA` defaults to `MAXDSIZ` (32 GiB on amd64, 1 GiB on arm64), as on FreeBSD.
    - `tests/user/aslr.c` checks that the auxiliary vector matches the real layout.
  - **Left**: the signal trampoline in the shared page (done in P4.4), PIE (with the run-time
    linker, which is the only thing that loads `ET_DYN`), and `pdfork` (done in P4.5).

**P4.4 Signals.** Done 2026-10-19.
- `sigaction`, `sigprocmask`, `sigfastblock`, `kill`, pending sets, and default actions (including
  stop and continue).
- Delivery on return to user mode, with FreeBSD's `sigframe`, `ucontext_t`, and `mcontext_t` per
  arch; `sigreturn` validating the restored context; `SA_RESTART` and `EINTR`; alternate stacks.
- Faults become `SIGSEGV`, `SIGBUS`, `SIGILL`, and `SIGFPE` with FreeBSD's `si_code`s.
- As delivered:
  - **Return path.** A new `user_return` hook runs on every way back to user mode (system calls,
    traps, and interrupts from user mode, but not NMIs or machine checks). It costs one load when
    nothing is pending. The trampoline is in the shared page.
  - **Interruptible sleeps.** Each thread records the queue it sleeps on, and a signal unqueues it
    under a per-thread abort lock, so the queue cannot vanish mid-wakeup. A stop happens inside the
    sleep, as FreeBSD's `sleepq_catch_signals` does. `nanosleep`, `wait4`, `_umtx_op`,
    `sigsuspend`, and the `sigwait` family can be interrupted.
  - **System calls.** `sigreturn`, `getcontext`, `setcontext`, `swapcontext`, `sigaltstack`,
    `sigpending`, `sigsuspend`, `sigtimedwait`, `sigwaitinfo`, `sigwait`, `sigqueue` (real-time
    signals queue, with their values), `kill` (including process groups and -1), `thr_kill`, and
    `thr_kill2`. SIGSYS goes to numbers that are not system calls.
  - **Job control.** `wait4` reports stops (`WUNTRACED`) and continues (`WCONTINUED`). SIGCHLD
    carries `CLD_*` codes, honoring `SA_NOCLDSTOP` and `SA_NOCLDWAIT`. Stops from the terminal do
    not stop an orphaned process group.
  - **ABI.** `abi-check` now covers `siginfo_t`, `stack_t`, both architectures' `mcontext_t`,
    `ucontext_t`, and `struct sigframe`, and every signal constant. It caught one difference:
    `mc_esr` exists only in 16-CURRENT, while 15.1 has `mc_spare[7]`.
  - **Tests.** `tests/user/signals.c` passes on both architectures. It covers handlers and their
    arguments, blocking, `SA_RESETHAND` and `SA_NODEFER`, the alternate stack, a caught `SIGSEGV`,
    default kills and stop/continue seen through `wait4`, `EINTR` against `SA_RESTART`,
    `sigsuspend`, `sigwait`/`sigtimedwait`/`sigqueue`, the context calls, `thr_kill`, and SIGSYS.
  - **Bugs found on the way:**
    - XSAVE leaves most of its header unwritten, so stale heap bytes in `XCOMP_BV` made
      `sigreturn` reject about one context in three.
    - `sigreturn`'s RFLAGS check lacked RF, which a frame saved on the way back from a fault
      carries. (The fix itself did not land with P4.4; it did with P4.5, after CI under TCG
      showed the failure.)
    - A signal that arrived while blocked did not end a later `sigsuspend`. Lowering the mask now
      marks the thread interrupted.
  - **Deviations** (no SVE signal context, no core dumps, process-wide signal state) are listed in
    [compat-deviations.md](compat-deviations.md).

**P4.5 Files.** Done 2026-10-19.
- File descriptor tables (`dup2`, `fcntl`, close-on-exec), shared by `fork`.
- A minimal VFS: vnodes, path lookup, `chdir`/`fchdir`/`__getcwd`, `openat`, `fstat`/`fstatat`,
  `lseek`, `getdirentries`, over a read-only in-memory file system on the preloaded memory disk.
- devfs with `console`, `null`, `zero`, and `random` (the last on `sys/random`).
- Pipes (`pipe2`), with FreeBSD's buffer sizes and the direct-write path deferred.
- As delivered:
  - **`sys/vfs`** (host-tested). A vnode's kind is an enum (regular file, directory, symlink,
    device), so only a directory can be looked in. Path lookup crosses mounts both ways, follows
    symlinks (at most `MAXSYMLINKS`), and supports `O_RESOLVE_BENEATH`. `__getcwd` walks back up
    through the parents. A builder makes fixed trees: tarfs (the memory disk, mounted read-only as
    `/`) and devfs (on `/dev`). File system type numbers and ids match FreeBSD's (`lsvfs` agrees:
    devfs 0x71, tarfs 0x31).
  - **Descriptors.** Open files share their offset and status flags across `dup` and `fork`.
    Tables are copied by `fork` (dropping close-on-fork), trimmed by `execve` (close-on-exec), and
    emptied at exit before the parent hears. Kernel-started processes get the console on 0 to 2.
  - **System calls.** `open`/`openat` (with `O_PATH`, `O_EMPTY_PATH`, and FreeBSD's error
    order), `read`/`write` with their vector and positioned forms, `lseek`, `close`,
    `close_range`, `dup`, `dup2`, `fcntl` (the dup commands, descriptor and status flags,
    `F_DUP3FD`, `F_ISUNIONSTACK`), `ioctl` (`FIOCLEX`, `FIONCLEX`, `FIONBIO`, `FIONREAD`,
    devices), `pipe2`, `fstat`, `fstatat`, `fstatfs`, `statfs`, `getfsstat`, `getdirentries`,
    `chdir`, `fchdir`, `__getcwd`, `access`/`eaccess`/`faccessat`, `readlink`/`readlinkat`,
    `umask`, and `copy_file_range`'s checks (no regular file is writable yet, so `cat` falls back
    to `read` and `write`, as FreeBSD's does on a pipe).
  - **Pipes.** Both ends read and write, as FreeBSD's do. Buffers size themselves as FreeBSD's
    do: 16 KiB for the forward direction and a page for the reverse, doubling to 64 KiB for what
    is queued plus the write. Writes up to `PIPE_BUF` bytes are atomic. Closing an end gives the
    other end EOF, or `EPIPE` with `SIGPIPE`.
  - **`mmap` of files.** Private mappings of the memory disk's files share its cached pages until
    written. `/dev/zero` maps as anonymous memory,
    and FreeBSD's rights checks apply.
  - **`pdfork`, `pdgetpid`, `pdkill`** (from P4.3). A child with an open descriptor sends no
    `SIGCHLD`. `wait4(-1)` skips it, though its pid or its group still collects it. Closing the last
    descriptor reaps it if it has exited, and otherwise kills it unless `PD_DAEMON` is set.
    FreeBSD 15.1 answered these questions: 16-CURRENT's process-descriptor code has moved on.
  - **Tests.** `tests/user/files.c`, `pipes.c`, and `pdfork.c` pass on both architectures and on
    FreeBSD 15.1 natively, which caught one wrong assumption (a pipe holds 64 KiB, not 16). A
    kernel test runs `/rescue/sh` over pipelines, `cd`, `ls -l`, `sed`, `head`, `tail`, `md5`,
    and `realpath`.
  - **Bugs found on the way:**
    - On arm64, a successful system call left `x1` as it was; FreeBSD writes the second result
      (0) there. libc's `vfork` computes `x0 & (x1 - 1)`, so the parent's pid came back garbled,
      sometimes 0, and the parent then ran the child's code. The arm64 machine-code test now
      checks `x1`.
    - P4.4's RF fix had not landed: a signal delivered on the way out of a page fault saved RF,
      `sigreturn` refused the context, and the trampoline's `hlt` became `SIGBUS`. CI under TCG
      hit it. `tests/user/signals.c` now returns from a fault handler, which fails every time
      without the fix, and amd64's `sigreturn` reports refusals on the console, as FreeBSD's does.
    - Running `signals.c` on FreeBSD showed the test itself was wrong: a `SIGCHLD` handler without
      `SA_RESTART` makes the parent's `wait4` fail with `EINTR`, because FreeBSD signals the
      parent before waking it, and a sleep a signal interrupted reports it even if what it
      waited for also came. The kernel now does both, and the test checks both cases.
  - **Deviations** (read-only files, the console's input, advisory locks, `SIGIO`) are in
    [compat-deviations.md](compat-deviations.md).

**P4.6 TTY.** Done 2026-10-19.
- The console UART handed over from the kernel log to a tty driver (`uart` attaches as `ttyu0`).
- TTY layer v1: line discipline, `termios`, the `ioctl` subset, the controlling terminal, and job
  control (`SIGINT`, `SIGTSTP`, `SIGTTIN`/`SIGTTOU`, foreground process groups).
- As delivered:
  - **Input.** The `uart` on the console's registers takes the port's interrupt. Its filter has
    the console move received bytes off the 16550 or PL011, and its interrupt thread hands them
    to the terminal. The console keeps writing the port, so kernel messages and terminal output
    share it. `/dev/console` and `/dev/ttyu0` are the terminal, and `/dev/tty` opens the caller's
    controlling terminal.
  - **`sys/tty`** (host-tested): FreeBSD's line discipline. Canonical editing (erase, kill, word
    erase, reprint, literal next, UTF-8 characters erased whole), echo with `^X`, the signal
    characters, `^S`/`^Q` and `IXANY`, CR/NL mapping, `VEOF`, `OPOST` output (`ONLCR`, `TAB3`,
    `OCRNL`, `ONOCR`, `ONLRET`, `ONOEOT`), and raw reads with `VMIN` and `VTIME`. `termios`,
    `winsize`, the defaults, and every terminal `ioctl` number are checked by `abi-check`.
  - **Job control.** Only a session leader takes a controlling terminal (`TIOCSCTTY`). The
    background gets `SIGTTIN` for reads, and `SIGTTOU` for writes under `TOSTOP` and for changes
    (`TIOCSETA`, `TIOCSPGRP`, `TIOCSTI`, and the rest FreeBSD lists), or `EIO` as FreeBSD decides.
    The read retried after such a stop fails with `EINTR`, as FreeBSD's does. When a session
    leader exits, its foreground group gets `SIGHUP`. A group an exit orphans with stopped
    members gets `SIGHUP` and `SIGCONT` (FreeBSD's `orphanpg`).
  - **Tests.** `tests/user/tty.c` runs a session on the terminal: lines, erase, end of file, raw
    reads and their timer, `^C`, `SIGWINCH`, and background reads and writes. It passes on both
    architectures, and on FreeBSD 15.1 over a pty. A kernel test runs an interactive
    `/rescue/sh` on the console and types at it: `^C` ends a `sleep 30`, `^Z` suspends one, and
    `jobs` and `fg` bring it back. Another loops a byte back through the UART and waits for it
    to arrive by interrupt at the terminal.
  - **Found on the way:** `TIOCSTI` and the other changes must wait from the background, which
    the first version missed. FreeBSD showed it: the test's session leader typed from the
    background and stopped. Programs run over ssh inherit `SIGTTIN` and `SIGTTOU` ignored, so the
    test sets them back. A memory-leak check in the kernel tests counted earlier tests' address
    spaces, which each CPU let go only when it next loaded another; it now has every CPU let go
    first.
  - **Deviations** (no ptys, no `^T` status line, a fixed console line) are in
    [compat-deviations.md](compat-deviations.md).

**P4.7 Verification and performance.** Done 2026-10-19.
- The boot-to-shell expect test in CI on both architectures: pipelines, `^Z` and `fg`, `ls`, `cat`,
  `sysctl kern.ostype`, and `/rescue/ps`.
- A syscall fuzzer pass over Tier A (random arguments and pointers from a user process), with no
  kernel panics and balanced accounting.
- The P4 suite against FreeBSD 15.1 on bhyve, the same static binaries on both kernels.
- Progress (2026-10-19):
  - **Boot to a shell.** A normal boot ends in FreeBSD's `/rescue/sh`, run as single-user init
    runs it: a login shell (`-sh`) leading its own session, with the console as its controlling
    terminal. When it exits the machine powers off. `cargo xtask test --tier boot` now boots each
    architecture to that shell (and bhyve on the server) and runs `xtask/src/expect.rs`'s script,
    the one `freebsd-shell` runs against FreeBSD: arithmetic, a pipeline, `ls`, `sysctl
    kern.ostype`, `ps`, `^Z`, `jobs`, `fg`, `^C`, and `exit`. It passes on both architectures.
  - **`kern.proc`** for `ps`: `struct kinfo_proc` (1088 bytes, checked by `abi-check`), every
    selection with its `_td` twin, `args`, and `pathname`, plus `kern.pid_max`. Processes record
    their command name, arguments, executable path, and start time.
  - **Bug found:** a process killed by a signal leaked. Signal delivery held a reference to it on
    a stack that never unwinds, so it lived on as a zombie with no parent, holding its page-table
    root. `ps` showed them. The `ps` kernel test now checks that no process outlives its test.
  - **Fuzzer** (`tests/user/fuzz.c`). Children make random calls, from every implemented call
    but those that would reach outside them: no fork, exec, or exit; signals only to themselves;
    no sysctl writes; files opened read-only. Arguments are small, huge, -1, pointers to random
    bytes, pointers running into an unmapped page, NULL, or kernel addresses. Memory calls work
    mostly in a scratch region, and calls that would wait for good are given timeouts. Each boot
    test makes about 120,000 calls on each architecture with no panic. A kernel test runs it
    twice with one seed: the second run leaves free memory exactly where the first did, and no
    process behind.
  - The expect script waits a second after `fg` before typing `^C`: `fg` prints the job before
    giving it the terminal, and emulated arm64 in CI lost that race (FreeBSD has the same
    window). `freebsd-shell` still passes it against FreeBSD.
  - **PTI settled.** bhyve does not pass `IA32_ARCH_CAPABILITIES` through, so a FreeBSD guest on
    the i7-12700KF cannot see `RDCL_NO` and turns PTI on, which bare-metal FreeBSD on that CPU does
    not do. That explained `getpid` at 240 ns in the guest against 67 ns on the host. The perf
    guest now boots with `vm.pmap.pti=0`, the posture `PERFORMANCE.md` asks for, and logs its
    mitigations. Every earlier P4 comparison flattered AbyssBSD.
  - **The P4 suite against FreeBSD 15.1** (bhyve, 10 runs each, revision 1662758): no benchmark
    over the bound, geometric mean ratio 0.674. `getpid` 0.96, `write` 0.91, `read` 0.87,
    `fork` 0.98, `exec` 0.83, `vfork`+`exec` 0.81, anonymous fault 0.17, COW fault 0.93,
    `mmap`/`munmap` 0.97, signal delivery 0.93, pipe ping-pong 0.20, pipe bandwidth 0.61.
  - **What it took** (FreeBSD was ahead on half the suite at first):
    - Fault-around: a read or execute fault also maps up to four resident pages each side, read
      only (FreeBSD's `vm_fault_prefault`).
    - Shadow chains grew a level with every fork whose parent wrote while its child lived. They
      now fold at fork, and during a fault's own lookup when nothing else reaches the backing
      object.
    - Freeing a kernel stack unmapped it, which sent every other CPU a TLB shootdown at each
      thread exit. Freed stacks of the usual size now stay mapped in a 32-entry cache.
    - Fork flushes only the pages it write-protects. Exec and exit free the old address space
      by dropping it, not by emptying it entry by entry first.
    - Map entries are kept in short sorted chunks rather than a `BTreeMap`, whose node split and
      merge at a size boundary cost 150 ns per `mmap`. An exact one-entry `munmap` takes one
      search, and `munmap` of untouched memory skips the page tables.
    - On the gating CPU a locked instruction costs about 9 ns. Syscalls now borrow the current
      process and address space instead of cloning `Arc`s. A one-thread process's `read` and
      `write` borrow the file from the table without its lock or a reference (with several
      threads, at P6, the file is referenced). The process layout needs no lock.
    - `compiler_builtins`' `memcpy` and `memmove` start three string instructions per copy, and
      copy overlapping data backwards a byte at a time. `arch/x86_64/mem` replaces them for the
      kernel.
  - `cargo xtask ubench [--bhyve]` runs the suite on AbyssBSD alone, locally or in bhyve on the
    server.
  - **Left for later:** `proc.fork`'s spread across runs is wide (95% CI up to 1.03), so it is
    the benchmark to watch. A one-thread process's descriptor fast path has to be revisited when
    threads arrive (`Process::sole_thread`).

**Exit:** both arches boot to an interactive `/rescue/sh` (from FreeBSD 15.1 `base.txz`) on the
serial console. Pipelines, job control (`^Z`, `fg`), `ls`, `cat`, `sysctl kern.ostype`, and
`/rescue/ps` work. A boot-to-shell expect test is in CI. The P4 performance suite (syscall, process,
VM-fault, signal, and pipe microbenchmarks, run as the same static binaries on both kernels) is
within 2% of FreeBSD.

## Phase 5: VFS and file systems (L) — done 2026-10-23

**Goal:** real persistent storage.

- Full VFS: vnode lifecycle, namecache (with negative entries), mount table, `nmount`, permissions
  and ACL hooks, `stat`/`statfs`/`getdirentries` fidelity, and file locking (`flock`, `fcntl`
  advisory locks).
- Unified buffer/page cache, write-back with a syncer thread, and `fsync` semantics.
- Storage stack: `BlockDevice`, GPT providers, FreeBSD device naming (`nda*`, `ada*`, `vtbd*`).
- **tmpfs** and **devfs** (complete).
- **msdosfs** read/write (FAT12/16/32, LFN). Mount the ESP at `/boot/efi`.
- **UFS2** read, then write (without soft updates; clean unmount required), then **soft updates**.
  **SU+J** follows in Phase 9.
- Validation: images written by AbyssBSD must pass FreeBSD `fsck_ffs -f` and `fsck_msdosfs` on the
  build server with zero errors, under a randomized-workload stress test.

**Exit:** both arches boot with the root on UFS2 on NVMe and `/rescue/sh` as init. A file-system
stress workload (fsx-style, run unmodified from FreeBSD's tests) completes, and the image passes
`fsck_ffs` afterwards. The loader can read the kernel from UFS2. The P5 performance suite (namecache,
directory, UFS2 and tmpfs I/O, `fsync`, `base.txz` extraction) is within 2% of FreeBSD for each file
system overall ([ADR-0014](adr/0014-performance-parity-per-component.md)).

**Design.** [VFS.md](VFS.md) ([ADR-0013](adr/0013-vfs.md)): typed memory and use references to
vnodes, the protocol in the generic layer, one page cache for file data, a buffer cache for
metadata, and the on-disk engines in host-tested `lib/` crates.

**Reference configuration.** The P4 machines (QEMU `q35` and `virt`, bhyve), each with an NVMe disk
holding a GPT with a FAT ESP and a UFS2 partition made on the build server with `makefs`/`newfs`.
The FreeBSD 15.1 guests use the same disks for the baselines.

### Milestones

Each milestone ends green on both architectures, with CI passing.

```
P5.0 ─► P5.1 ─┬─► P5.2 ──────────────────────────┐
              └─► P5.3 ─► P5.4 ─► P5.5 ─┬─► P5.7 ─┼─► P5.8 ─► P5.9
                          P5.6 (from P5.3) ─────────┘
```

**P5.0 File-system harness, and closing Phase 4.**
- The P5 suite in `ubench` (or a sibling program), with FreeBSD's baseline on bhyve, on a UFS2
  bench disk and on tmpfs.
- `cargo xtask fs-image` builds the reference GPT disks (ESP plus UFS2) on the server, and `cargo
  xtask fs-check` runs `fsck_ffs -f -n` / `fsck_msdosfs -n` on an image and reports.
- fsx and pjdfstest built on the server from the FreeBSD tree, and their runs recorded on FreeBSD.
- The aarch64 TCG numbers left from P4.0, recorded.

**P5.1 VFS core.** Done 2026-10-19.
- Vnodes as VFS.md §4 describes: the per-mount inode table, `VnodeUse`, `inactive` and `reclaim`,
  `Live`/`Doomed` states, the vnode LRU and `vnlru`, and per-vnode `SxLock`s with the §5 order.
- The name cache with negative entries and directory back-references (§6). `namei` rebased on it,
  with typed lookup modes.
- The operation traits (§7) and generic implementations: `open` with `O_CREAT`/`O_EXCL`/`O_TRUNC`,
  `link`, `unlink`, `rename`, `mkdir`, `rmdir`, `symlink`, `mknod`/`mkfifo`, `chmod`, `chown`,
  `utimensat`, `truncate`, `chflags`, `getdirentries` with cookies, `access` checks (§13), and
  advisory locks (§12).
- Mounts: `nmount`, `mount`, `unmount` (with `MNT_FORCE`), `statfs`/`fstatfs`/`getfsstat`, and
  busy counts. The memory disk and devfs move onto the new interfaces.
- Host tests against an in-memory test file system, including random operation sequences checked
  against a model.
- Done 2026-10-19:
  - `sys/vfs` rebuilt on VFS.md. A vnode holds its file system's node behind an `SxLock`, `Live`
    or `Doomed`. Each mount's table holds its vnodes, so one goes only on purpose: the `vnlru`
    thread reclaims the longest unused idle ones past `kern.maxvnodes` (one per 32 pages, as
    FreeBSD sizes it), or unmount does. `VnodeUse` is the use reference, and its last drop frees an
    unlinked file.
  - The name cache holds positive and negative entries, refers to vnodes weakly (it never decides
    what stays in core), and records directories' names for `getcwd`.
  - The generic layer has FreeBSD's rules for create, unlink and rmdir, link, rename (one vnode
    lock taken and the rest tried, with a per-mount lock across directories), setattr (`vaccess`,
    ownership, set-id and sticky bits, `chflags` flags), and `getdirentries` packing.
  - Advisory locks: `fcntl` record locks owned by the process (released when it closes any
    descriptor for the file) and `flock` locks owned by the open file, in one list per vnode, as
    on FreeBSD, with deadlock detection.
  - tmpfs, and the fixed trees of the memory disk and the boot devfs, moved onto the new traits.
  - System calls: `open` with `O_CREAT`/`O_EXCL`/`O_TRUNC` and permission checks, writable
    files, `copy_file_range`, `mkdir`, `unlink`, `rmdir`, `rename`, `link`, `symlink`, `mknod`,
    `mkfifo`, the attribute calls, `truncate`, `fsync`, `sync`, `flock`, and `nmount` (tmpfs) and
    `unmount`, with their `*at` and descriptor forms.
  - 25 host tests (5000 random operations against a model among them), and `tests/user/vfs.c`,
    which passes natively on FreeBSD 15.1 too. It settled several `errno`s against FreeBSD:
    `unlink` of `.` gives `EPERM`, `O_CREAT` on a directory `EISDIR`, and a new name with a
    trailing slash `ENOENT`.
  - `sched` locks now work in host tests, and the witness lets vnode locks nest (`dup_ok`).

**P5.2 tmpfs and devfs.** Done 2026-10-19, except pjdfstest (see below).
- tmpfs (§10.2) mounted on `/tmp`. The first writable file system: every P5.1 syscall works on it,
  and pjdfstest passes on it as FreeBSD's tmpfs does.
- devfs made dynamic (§10.1): disk and partition nodes with raw block access, and names, modes, and
  `stat` matching FreeBSD.
- `fsx` on tmpfs.
- As delivered:
  - Boot mounts a tmpfs on `/tmp`. FreeBSD's fsx (built from the FreeBSD tree beside this one,
    never copied here) runs 20,000 operations on it in a kernel test, without mapped I/O until
    P5.3. Another test runs `/rescue`'s `mkdir`, `cp`, `ln`, `mv`, `chmod`, `tar`, and `rm` over a
    tree there.
  - What those tools needed: `pathconf` and its relatives, `vfs.conflist` (for `getvfsbyname`),
    `lseek` with `SEEK_DATA`/`SEEK_HOLE`, and `EOPNOTSUPP` from the ACL and extattr calls.
  - devfs is a file system over a registry. Disks and partitions appear and go as `sys/storage`
    publishes and withdraws them (`crw-r----- root:operator`, as on FreeBSD), and a gone device's
    vnode is doomed. A disk node reads and writes whole sectors and answers the `DIOCG*` requests.
    `chmod`, `chown`, `touch`, and symbolic links work on devfs. `tests/user/disks.c` checks a disk
    node read-only, and passes on FreeBSD 15.1 too.
  - `CharDevice` reads and writes now take the file offset, which advances for devices as on
    FreeBSD.
  - **Moved to P5.9:** pjdfstest. Most of it switches credentials (`-u`/`-g`), and every process is
    root until Phase 6, so it needs a minimal credential model (`setuid`, `setgid`, `setgroups`,
    per-process `Cred`) first.

**P5.3 Page cache, buffer cache, write-back.** — done 2026-10-19
- The vnode pager with fill and write-back (§8.1), dirty tracking by write fault for shared
  mappings, `msync`, and reclaiming clean pages under pressure.
- `sys/bufcache` (§8.2) over `sys/storage`, with synchronous wrappers that sleep on `Bio`
  completion and `BIO_FLUSH`.
- The syncer (§9.1), `fsync`/`fdatasync`/`sync`, and dirty throttling (§9.3).
- Clustering and read-ahead (§8.3).
- Proven first on a raw disk node, and by tmpfs moving its file data to the same paths.
- As delivered:
  - VM objects track dirty pages for pagers that write back. Pagers are fallible, and a fault
    never fills under the map lock: it returns the page to fill, the kernel fills with the map
    unlocked, then resolves again. `MAP_SHARED` file mappings map the file's object itself. A
    clean page maps read-only so its first write marks it dirty; a page mapped writable stays
    dirty, since there are no reverse maps to take the right back after writing it.
  - Pagers read clusters (`max_run`, `fill_run`). An object grows its read-ahead window by
    doubling while fills come in order and drops back to one page otherwise.
  - When the free lists run dry, clean pages of file objects nothing maps are reclaimed. Page-cache
    transfers and write-back hold an object's I/O guard, so no frame in use is freed.
  - `sys/vfs` `pagecache` reads, writes, and resizes through an object's pages. A writer that finds
    more than a tenth of memory dirty writes its own file back first.
  - tmpfs keeps file data in anonymous VM objects, so its files map, share, and execute. `exec`
    reads only the ELF head (`Elf::parse_head`).
  - `msync` with FreeBSD's checks. `fsync` writes the file's pages, then a per-node hook for
    metadata. The syncer thread asks every writable mount, once a second, to write what has been
    dirty for 30 seconds.
  - `sys/bufcache` caches metadata blocks by disk offset. A closure runs on each held buffer.
    Writes are synchronous or delayed, and delayed ones are stamped on the monotonic clock for the
    syncer. Dirty data is throttled between high and low watermarks, and idle clean buffers are
    evicted LRU first. The kernel puts it over `sys/storage` disks, whose transfers sleep on bio
    completion. A boot test drives it on the scratch disk.
  - fsx on tmpfs now passes with mapped reads and writes. `vfs.c` checks shared and private
    mappings against `read` and `write`, and passes on FreeBSD 15.1 too.
  - **Left to its users:** the vnode pager over a file system's block map comes with UFS (P5.4),
    which is its first pager that writes back. Write clustering of adjacent dirty buffers comes
    with UFS writes (P5.5).

**P5.4 UFS2 read.** — done 2026-10-19
- `lib/ufs` read side (§10.3 stage 1), host-tested on images `makefs` and `newfs` made from
  `base.txz`, comparing every file's contents and `stat` with the source tree.
- The kernel mounts UFS2 read-only from NVMe, AHCI, and virtio-blk. `find`, `ls -lR`, and
  `cat` over the whole tree match FreeBSD's output on the same image.
- The loader reads the kernel and its modules from UFS2.
- As delivered:
  - `lib/ufs` covers the superblock (FreeBSD's `validate_sblock` checks and the check-hash),
    inodes (with their check-hash), block maps through triple indirect blocks, `bmap_run`, holes,
    fragment tails, directories, symbolic links, and `statfs`. It works over a `BlockIo` that takes
    a context: `Sleepable` in the kernel, `()` elsewhere. CRC-32C moved into `lib/crc`.
  - A host test reads `makefs` images at two geometries. `cargo xtask --remote ufs-verify` checks
    the whole of `base.txz` (30,468 entries) twice: imaged by `makefs`, and copied through
    FreeBSD's UFS onto `newfs -U` with a sparse file added. Every mode, owner, size, mtime,
    content, and link target matches.
  - The kernel mounts UFS2 read-only (`nmount` type `ufs`). Metadata goes through a buffer cache
    over the disk. Each file's pages come from a pager that reads contiguous block runs straight
    from the disk. `unmount` takes `MNT_BYFSID`, as `umount(8)` uses.
  - The boot tests attach a UFS2 disk `makefs` made from `/rescue`, `/lib`, and `/usr/include`, on
    virtio-blk, NVMe, and AHCI. Over each, `/rescue`'s `mount`, `ls -lRnT`, and `md5 -r` print what
    FreeBSD printed over the same image: all 3,321 files' digests from virtio-blk, and the listing
    plus `/rescue/rescue`'s digest from the others. FreeBSD's bhyve device recording was redone
    with those disks.
  - The loader looks for `/boot/kernel/kernel` (and `/boot/abyss/md`) on `freebsd-ufs`
    partitions through firmware Block I/O, reading runs straight into place, and falls back to the
    ESP. `<arch>-ufs-shell` boots that way to `/rescue/sh` on both architectures, on the server.
    The loader has no modules apart from the memory disk yet.

**P5.5 UFS2 write.** — done 2026-10-20
- Stage 2 of §10.3: allocation with FreeBSD's policies, `balloc`, truncation, directory
  operations, inode allocation, check-hashes, write ordering, and the clean flag.
- Host random workloads on images, each checked by `fsck_ffs -f -n` on the server with no errors.
- In the kernel: a read-write UFS2 mount, pjdfstest and fsx on it, then `fsck_ffs -f` clean on the
  server. Large-directory lookups use a directory hash (FreeBSD's `dirhash`).
- As delivered:
  - `lib/ufs` follows FreeBSD's `ffs_alloc.c`, `ffs_balloc.c`, `ffs_inode.c`, and `ufs_vnops.c`
    without soft updates:
    - cylinder groups with exact accounting (fragment summaries, cluster maps, rotors, lazy
      inode-block initialization) and check-hashes;
    - allocation with FreeBSD's policies;
    - `balloc` and `truncate`: no file ends in a hole, and new space reads as zeros;
    - directory operations, and create, link, unlink, `rmdir`, and rename with link counts and
      FreeBSD's write order. Inodes, directory blocks, and new indirect blocks are written
      synchronously, and a truncated inode before its blocks are freed.
  - The directory hash hangs off a large directory's in-core inode (2,560 bytes or more, FreeBSD's
    `dirhash_minsize`). It indexes names and each chunk's free space, and every directory operation
    keeps it exact.
  - The host test runs a 3,000-operation random workload against a model on `makefs` images
    (three geometries, one filling up) and on `newfs` images, which carry the superblock, group,
    and inode check-hashes. Half the operations go through the directory index, which is checked
    against one rebuilt from the disk. A 3,000-entry directory test covers lookups, removals, and
    slot reuse through the index. Every image reads back as the model says, and FreeBSD's
    `fsck_ffs -f -n` finds it clean.
  - The kernel mounts UFS2 read-write:
    - in-core inodes are shared by node and pager under a sleepable lock;
    - `write` allocates block by block before copying through the page cache;
    - the pager writes pages back, allocating holes that mappings wrote;
    - inodes are written lazily;
    - the clean flag is cleared on mount and set on unmount, and an unclean file system is refused
      read-write unless forced.
  - A boot test on its own disk runs FreeBSD's `newfs`, `/rescue`'s tools, and fsx read-write. Then
    FreeBSD's `fsck_ffs`, running on AbyssBSD, finds the file system clean, and on the server the
    host's `fsck_ffs` checks the disk image again.
  - On the way:
    - VM truncation zeroes the pages it must keep, so cut-off data never reads back;
    - `bufcache` gains `discard`;
    - integer casts in `lib/ufs` go through explicit helpers.
  - **Moved to P5.9:** pjdfstest needs a minimal credential model first (see P5.2).

**P5.6 msdosfs.** — done 2026-10-20
- `lib/fat` (§10.4) read and write, host-tested against `newfs_msdos` images and `fsck_msdosfs`.
- The ESP mounted on `/boot/efi`. fsx on it, and `fsck_msdosfs -n` clean afterwards.
- As delivered:
  - `lib/fat`'s engine (`fat::fs`, beside the image builder) follows FreeBSD's msdosfs:
    - geometry with `mountmsdosfs`'s checks;
    - FAT12/16/32 entries, the in-use map, `clusteralloc1`'s allocation, chains, the clean bit,
      and FSInfo;
    - long-name directories with lookup as `msdosfs_lookup_ino` matches;
    - unique short names, slot placement, `.` and `..`, rename, and FAT times;
    - name conversion as `msdosfs_conv.c` does without kiconv.
  - Host tests read `makefs` FAT12/16/32 images of a tree of awkward names: long names, lookups
    ignoring case, contents, and times. A 2,000-operation workload, with renames across
    directories, reads back as its model on `newfs_msdos` FAT12/16/32 volumes, one filling up, and
    `fsck_msdosfs -n` finds each clean.
  - `fetch-base` makes a FAT32 test disk with names written through FreeBSD's own msdosfs. All 27
    short names FreeBSD chose, generation numbers included, are what `lib/fat` predicts. The boot
    tests mount it read-only, and AbyssBSD's `ls -aliRnT` (file numbers included) and `md5 -r`
    match FreeBSD's. FreeBSD's bhyve device recording was redone with the disk.
  - The kernel's msdosfs:
    - numbers files with FreeBSD's file ids; a renamed or removed in-use file keeps its vnode's
      number, so a new file gets another;
    - takes modes, owners, and masks from the mount options, as `mount_msdosfs` passes them;
    - maps chflags to DOS attributes, and refuses links and symbolic links as FreeBSD does;
    - keeps file data in the page cache over a cluster-mapping pager.
  - Boot mounts the first ESP read-write on `/boot/efi`. A boot test runs `/rescue`'s tools, a
    file removed while open, and fsx over it. FreeBSD's `fsck_msdosfs -n`, running on AbyssBSD,
    then finds it clean, and it remounts with the data intact.
  - `stat` can report a number other than the vnode's (`Attr::fileid`), which FreeBSD's FAT32 root
    needs: it shares its number with the first entry in it.

**P5.7 Soft updates.** Done 2026-10-20.
- Stage 3 of §10.3: dependency tracking and rollback, `fsync` that writes only what the file needs,
  and mounting with `-o softdep` / honoring `FS_DOSOFTDEP` as `tunefs -n enable` sets it.
- Crash tests: the VM killed at random points during workloads, then `fsck_ffs -p` and `-f -n` on
  the server, with nothing but leaked blocks and inodes.
- Done 2026-10-20:
  - `lib/ufs`'s tracker (`softdep.rs`, VFS.md §10.3) with FreeBSD's rules. Pointers, directory
    entries (number and type), and new inodes are rolled back in the copy of a buffer written too
    early. Frees and link counts dropping wait as deferred work. `Inode::effnlink` is the count
    users see meanwhile.
  - Three rules go beyond FreeBSD's, so a second fsck finds nothing either:
    - no pointer to an indirect block that is empty on the disk goes out;
    - partial truncation disconnects the kept tree before cutting it;
    - a failed `balloc` gives back the indirect blocks it made.
  - Two other changes:
    - The buffer cache gained a write hook, and keeps a buffer's bytes when its size changes.
    - Without soft updates, a directory moved over another no longer loses its new parent a link.
  - Turning soft updates on, as FreeBSD does:
    - FreeBSD has no `softdep` mount option; the kernel follows `FS_DOSOFTDEP` (`newfs -U`,
      FreeBSD 15's `newfs` default) and reports `MNT_SOFTDEP`;
    - an unclean file system with soft updates mounts read-write, its summary counted again from
      the groups;
    - a journal flag is cleared (compat-deviations.md).
  - The kernel side:
    - the syncer does the work each second, through in-core inodes the vnodes share;
    - `fsync` writes the buffers the file needs, and `sync` and unmount repeat until nothing
      waits;
    - `statfs` counts frees on the way, and out of space, frees are done and the write or create
      tried again;
    - past 20,000 items the syncer writes everything, and past 80,000 writers wait.
  - Host crash tests: random workloads over a cache that writes buffers in random order and file
    data late, crashing at random points.
    - On the server, FreeBSD's `fsck_ffs -p` preened 8,054 crash images from 60 workloads (rerun
      until `-f -n` is clean). Each settled image matches its model and is clean.
    - Locally, 180 workloads pass the same checks done without fsck.
    - 760 `fsync`s, each followed straight away by a crash, kept the file's name and data.
  - `cargo xtask crash-test` kills QEMU mid-workload on a `newfs -U` disk, and every other round
    boots on the crashed image as it is.
    - 30 rounds on x86_64 and 3 on aarch64: `fsck_ffs -p` repaired every image.
    - It repaired leaked inodes (unreferenced, and empty new directories), leaked blocks (maps and
      summaries), link counts too high, block counts, and file sizes past the last indirect block.
    - Directories caught mid-rename had their `..` fixed or an extra name removed. FreeBSD's
      non-journaled soft updates leaves these same kinds.
  - The kernel's read-write UFS test runs with `newfs -u` and with `newfs -U`. With soft updates it
    also checks:
    - the "soft-updates" mount flag;
    - space coming back after a removal;
    - 4,000 files made and removed;
    - fsx, then `fsck_ffs -f -n` clean.

**P5.8 Root on UFS2.** Done 2026-10-20.
- The kernel mounts the root from `vfs.root.mountfrom`, devfs on `/dev`, tmpfs on `/tmp`, and the
  ESP on `/boot/efi`. Both architectures (and bhyve) boot from an NVMe disk to `/rescue/sh` on UFS2,
  with the boot-to-shell expect test in CI moved onto it.
- The full `base.txz` userland on the root, and its tests that only need P5 features run: `ls`,
  `find`, `tar`, `cp -R`, `mtree`.
- Done 2026-10-20:
  - The loader sets `vfs.root.mountfrom`. It uses the `/` line of `/etc/fstab` on the UFS2 file
    system the kernel came from, as FreeBSD's loader does, or else `ufs:/dev/gptid/<uuid>`.
  - The kernel mounts that root read-only, as FreeBSD does first. It waits up to 3 seconds for
    the disk, and falls back to the memory disk. devfs gains FreeBSD's `gpt/<label>` and
    `gptid/<uuid>` names for partitions.
  - The ESP mounted on `/boot/efi` is now the root disk's.
  - `mount -u -o rw /` (`MNT_UPDATE`) moves UFS2 between read-only and read-write. Soft
    updates are set up at open, so `MNT_SOFTDEP` shows once the root is read-write.
  - At power-off every file system is unmounted, the root last, and a read-write root is left
    clean.
  - The server's shell cases (`<arch>-shell`, `x86_64-bhyve-shell`) boot from NVMe on an image of
    all of `base.txz` that `makefs` builds, with owners and file flags from the archive's own
    records. They replace `-ufs-shell`.
  - After the P4 script, `UFS_ROOT` checks the root:
    - it starts read-only, and `mount -u` makes it read-write with soft updates;
    - `ls -lo` shows `su`'s setuid bit and `schg` flag;
    - `mtree` checks every entry of the base: owner, mode, flags, link, size, and SHA-256;
    - `tar` and `cp -R` copy a tree that `mtree` then finds the same;
    - after power-off, `dumpfs` must show the root clean and `fsck_ffs -f -n` must find nothing.
  - `find` and `mtree` are static builds from the FreeBSD tree until base's own can run (they
    are dynamically linked; `rtld` is P6). `/rescue` provides `ls`, `tar`, and `cp`.
  - Fixed along the way:
    - the `vget` lock now ranks below the file systems' locks it is held over (witness had
      reported the order);
    - the vnode reclaimer could doom a vnode a lookup had just found (a `chdir` into it left
      `mtree` in a dead directory, every open `EBADF`). It now claims a vnode under the table
      lock, and a name-cache hit on one being claimed is a miss, as FreeBSD's `vget` fails on a
      doomed vnode;
    - out of space with soft updates, waiting for pending frees now writes file pages too (a
      free can wait for the data of the block replacing it), and three more paths wait and try
      again, as `write` and `create` did: a hole a mapping filled, a truncation that ends inside
      a hole, and `link`. fsx, now built on the server too, had hit `EIO` and `ENOSPC` there on
      the 32 MiB test disk;
    - soft updates' scan of an inode block for roots over empty indirect blocks read a short
      symlink's target as pointers, and could overflow; it skips inodes without blocks;
    - the virtio entropy test allows the harvester's own reads to interrupt in between;
    - emulated aarch64 boot cases have 600 seconds, now that the file-system tests take minutes
      under TCG.

**P5.9 Verification and performance.** — done 2026-10-23
- fsx (unmodified) on UFS2 with soft updates completes, and the image passes `fsck_ffs -f`.
- pjdfstest on tmpfs and UFS2 with the same results as FreeBSD.
- The P5 performance suite within 2% of FreeBSD on bhyve, including the lockless name-cache path
  (VFS.md §6.2) if measurement asks for it.
- Progress 2026-10-20:
  - fsx on UFS2 with soft updates, then `fsck_ffs -f -n` clean, has run in the kernel's UFS test
    since P5.7.
  - pjdfstest (`cargo xtask --remote pjdfstest`) runs all 230 scripts on tmpfs and on UFS2 with
    soft updates at once, from the UFS2 root in bhyve. Every script's result is FreeBSD's
    (recorded with the same static program and scripts on the server: 10,002 tests on tmpfs,
    12,349 on UFS2, all passing there), except chflags/09's 117 tests that run in jails, which
    are P6's; `tests/pjdfstest/known-differences.txt` lists them. The scripts take about as long
    as on FreeBSD (in sum, 185 s against 250 s on tmpfs, and 509 s against 672 s on UFS2).
  - What it took:
    - Per-process credentials: real, effective, and saved ids and supplementary groups, with
      FreeBSD's `setuid`/`seteuid`/`setgid`/`setegid`/`setre*id`/`setres*id`/`setgroups`
      rules, `issetugid`, `kern.ngroups`, and `access` checking with the real ids.
    - md(4) (`/dev/mdctl`: malloc and swap memory disks), which the UFS-only scripts make, `newfs`,
      mount, remount read-only, and detach; `kldload` answers `EEXIST` for `geom_md`.
    - Named pipes (open waits for the other side, `ENXIO` for a lone non-blocking writer), block
      device nodes, UNIX-domain `socket`/`bind`/`connect` (socket nodes only), `O_SHLOCK` and
      `O_EXLOCK`, `ETXTBSY` for a running program, `FIODTYPE`, `__realpathat`, and UFS's
      `pathconf`.
    - FreeBSD's rules where ours differed: a new file takes its directory's group; immutable and
      append-only files keep their attributes and refuse writes; `chmod` refuses the sticky bit
      on files (`EFTYPE`) and setgid without the group (`EPERM`); a non-root write clears setuid
      and setgid; flags rules for non-root and `SF_SNAPSHOT`; `O_CREAT|O_EXCL` does not follow
      a final symbolic link; `chown` to -1 and -1 changes nothing; `rename` reports `EROFS`
      before `EXDEV`; "now" times without permission are `EACCES`; and an older modification
      time pulls the birth time back.
    - The console dropped typed input past 255 bytes when the tty thread fell behind an emulated
      UART; it now leaves the rest in the FIFO, its interrupt masked, until the thread catches up.
    - The root image carries static `awk`, `grep`, `wc`, `cut`, `dirname`, `tr`, `mkfifo`,
      `touch`, `which`, and `sleep` from the FreeBSD tree, and a stand-in for `openssl md5`.
  - The P5 suite runs on AbyssBSD (`abyss.bench=p5` copies `ubench`, the tree, and fsx off the
    bench disk, as FreeBSD's `rc.local` does). First run: geometric mean 2.94 times FreeBSD.
    After the first round of fixes, 1.27 (fsync, the random and warm reads, readdir, and tmpfs's
    sequential write and fsx now win):
    - A tmpfs file's object counted as its program's only mapping, so a program's exit freed the
      file's pages (a second run read zeros); tmpfs objects now count as shared.
    - A name-cache purge scanned the whole cache: each vnode now keeps its entries' keys
      (FreeBSD's `v_cache_src`/`v_cache_dst`). UFS create and unlink went from 380 and 600 µs
      to 10 and 5.
    - Cylinder-group check-hashes are checked as groups are read from the disk and set as they
      are written (a buffer-cache hook), not recomputed on every change.
    - `fsync` no longer flushes the disk's cache: FreeBSD's UFS never does (it sends
      `BIO_FLUSH` only for the journal, behind `debug.softdep.flushcache`).
    - Writes no longer read pages they overwrite or that lie past the old end, and new blocks
      are cleared in the page cache, written with the data, instead of zeroed on the disk at
      once (FreeBSD's cleared buffers). Write-back goes in clusters of contiguous blocks, and a
      fill or write-back sends all its requests before waiting.
    - `abyss.prof`, a sampling profiler with disk-wait accounting
      (`cargo xtask --remote perf --abyss-only --prof`), found these.
    - Left: the lockless lookup (VFS.md §6.2) for `stat`/`open` (2.1 times), create and
      rename, asynchronous read-ahead for cold reads, and extract.
- Done 2026-10-23. The verdict (`cargo xtask --remote perf --suite p5 --runs 10`, alternating with
  FreeBSD 15.1 in bhyve, at 1ced6ff): UFS2 0.839 and tmpfs 0.860 of FreeBSD's time (geometric
  means; 0.849 over all 31 benchmarks), so both file systems pass, as ADR-0014 now judges parity:
  per component, a single microbenchmark being a diagnostic. 15 benchmarks win outright; extract,
  find, fsx, readdir, rename, and unlink are at or below FreeBSD. Five microbenchmarks are slower:
  UFS create (1.18, soft updates' bookkeeping per file), and `open`/`close` and `stat` of a missing
  name (3–8%, a few tens of nanoseconds a call). The work, in the order it landed:
  - Write-back without waiting: asynchronous buffer writes with FreeBSD's running-space limits,
    cache limits scaled to memory, and soft updates' item limits sized from the vnode limit.
  - Reads without copies: pages filled in place by the disk, two read-ahead windows in flight,
    block maps read a run at a time.
  - Hashed tables in place of ordered trees (`lib/intmap`, open addressing with a tag byte per
    slot): soft updates' maps, the in-core inode and vnode tables, tmpfs directories, and UFS's
    dirhash.
  - Locks that no interrupt handler takes hold off preemption only (masking interrupts is costly
    in a VM); a process's credential, directory, and limits borrowed, not referenced.
  - FreeBSD's choices where ours cost more: rename and tmpfs create leave the new name for a
    lookup to cache; a tmpfs write stamps its times when they are next read; a UFS write's new
    blocks are not zeroed under the data; UFS's create holds the summary only to allocate; inodes
    load without a mount-wide lock; vnode recency is a generation the reclaimer advances.
  - Lookups and `stat`: the name cache's heads apart from its padded locks, nodes laid out for the
    list walk, `stat` reading an inode's attributes without its lock and building its result in
    place, word-at-a-time `memcmp` and path copy-in.
  - Tools: `abyss.prof` reports contended locks by class, and `perf --freebsd-only --prof`
    profiles FreeBSD's side with DTrace, to set the two profiles side by side.

## Track: USB and input (parallel, starts after P3) (M) — in progress

**Goal:** the machine can be used from its own screen and keyboard, and boots or mounts from a USB
disk.

- xHCI (rings, contexts, MSI-X), hub driver, device enumeration, HID boot protocol, then report
  parser (keyboard, mouse), USB mass storage (BBB, then UAS), and virtio-input.
- Framebuffer virtual terminals (`ttyv*`) with a `vt(4)`-compatible keyboard layer.

**Exit (merges into P6):** a physical machine without a serial port can be driven entirely from the
GOP console and a USB keyboard. Physical machines stay untouched until FreeBSD userland runs end to
end in QEMU ([ADR-0010](adr/0010-qemu-first-development.md)), so the track exits in two steps:
- **In QEMU, now:** on both arches, `/rescue/sh` on `ttyv0` is driven entirely by a USB keyboard
  (keystrokes injected over QMP, the result read back from the screen), with a USB mouse and a
  virtio keyboard present and working. A UFS2 file system on a USB disk (`da0`) passes fsx and then
  `fsck_ffs`, over BBB and over UAS. The amd64 bhyve configuration (`xhci,tablet`) attaches cleanly.
- **On hardware, at P6 bring-up:** the sentence above, on both reference machines, with the X870E's
  PCI xHCIs and the Sky1's ACPI (`PNP0D10`) ones.

**Design.** [USB.md](USB.md) ([ADR-0015](adr/0015-usb-input-vt.md)): xHCI rings whose types say who
owns each slot, transfers completed exactly once, interfaces as the children drivers match, HID as
a bus, one input class under kbd and evdev, and `vt` over a pure terminal emulator.

**Behavior references.** FreeBSD's `sys/dev/usb` (controller/xhci.c, usb_hub.c, input/usbhid.c,
storage/umass.c), `sys/dev/hid` (hidbus.c, hkbd.c, hms.c), `sys/dev/vt` with `sys/teken`, `sys/dev/kbd` and `sys/dev/kbdmux`, and
`sys/dev/evdev`. FreeBSD has no virtio-input driver and no UAS: there, the specifications (virtio
1.2 §5.8, T10 UAS-2) are the reference, and the user interface is the same `kbd`/evdev one.

### Milestones

Each milestone ends green on both architectures, with CI passing.

```
U.0 ─► U.1 ─► U.2 ─┬─► U.3 ─► U.4 ─┬─► U.6
                   └─► U.5 ────────┘
```

**U.0 Design and harness.**
- `docs/USB.md` with an ADR: the xHCI ring model, the USB bus on the driver model (`UsbIdent`,
  interfaces as children, hubs as buses), transfers as owned requests completed exactly once, and
  the input and console classes (`KbdSource`, `PointerSource`, kbd, evdev, vt).
- QEMU machines get `qemu-xhci` with `usb-kbd`, `usb-mouse`/`usb-tablet`, `usb-hub`, `usb-storage`,
  `usb-uas`, and `virtio-keyboard`/`virtio-tablet`, on both arches. xtask injects keystrokes and
  reads the screen over QMP (`input-send-event`, `screendump`) for boot tests.
- FreeBSD 15.1's view of the same QEMU machine recorded as ground truth: `dmesg` names (`uhub0`,
  `usbhid0`, `hkbd0`, `hms0`, `umass0`, `da0`), `usbconfig` output, `devctl` events, and `/dev` nodes.
- The track's performance suite, by ADR (see below).

**U.1 xHCI.**
- The ring and context code in a host-tested `lib/xhci` (TRB encoding, cycle bits, link TRBs,
  event-ring dequeue, input and device contexts in 32- and 64-byte forms), fuzzed on event TRBs.
- The controller driver in `drivers/xhci`: attach on PCI (class 0c0330) and on ACPI (`PNP0D10`),
  BIOS handoff (USB legacy support capability), reset, scratchpad buffers, the command ring, an
  event ring per interrupter on MSI-X (MSI, then INTx, as fallbacks), and the root hub's ports
  (status change, reset, speed).
- Slots: enable, address, configure and evaluate endpoints; control, bulk, and interrupt transfers;
  stalls and endpoint reset; transfer timeouts and command abort; detach that leaves no DMA
  outstanding.

**U.2 The USB bus and hubs.**
- `sys/usb`: descriptors parsed in a host-tested `lib/usb` (device, configuration, interface,
  endpoint, SuperSpeed companion, BOS, strings), enumeration (address, configure, interfaces as
  children), and matching on vendor/product, class/subclass/protocol, and interface.
- The hub driver (USB 2 and USB 3 hubs, transaction translators), connect, disconnect, and hot
  unplug through the framework's `Gone` path with requests cancelled.
- FreeBSD's names and strings: `uhub*`, `usbus*`, pnpinfo/location, and `devctl` `USB` events. The
  `ugen` nodes and `/dev/usb` ioctls that `usbconfig` uses.

**U.3 HID and the input class.**
- A HID report-descriptor parser in a host-tested, fuzzed `lib/hid`. Keyboards (boot protocol,
  then report protocol), mice and tablets (relative and absolute), and `virtio-input`.
- The input class: the kbd layer (scancode and keycode modes, `kbdmux`, the `KDGKBMODE`/`KDSKBMODE`
  family, keymaps, LEDs, autorepeat) and evdev (`/dev/input/event*`, FreeBSD's evdev ABI, the
  `kern.evdev` sysctls).

**U.4 Virtual terminals.** Done 2026-10-24.
- `vt`: `ttyv0`–`ttyv7` on the framebuffer console, a terminal emulator that matches `teken`'s
  behavior as `TERM=xterm`, switching (Alt-F*n*), scrollback, the cursor, and the bell.
- Kernel messages and `/dev/console` on `ttyv0` as well as the serial port. The ioctls `rc` and the
  tools use: `kbdcontrol` (keymap, repeat rate), `vidcontrol` (font, colors, history), and
  `VT_*`/`KD*`.
- Text rendering that keeps up with output: dirty-rectangle redraw and glyph caching, measured by
  the suite.
- Progress (2026-10-23): `lib/vtterm` (teken and `vt_buf`, checked against FreeBSD's `teken` on
  1000 streams), `sys/vt` with all twelve windows (`ttyv0`–`ttyvb`, as FreeBSD has), kernel
  messages on window 0, switching with `vt`'s open and `VT_PROCESS` rules, Scroll Lock scrollback,
  per-window keyboard modes, and the `VT_*`/`KD*`/`CONS_*` requests (USB.md §7). The `vt` suite
  (`cargo xtask --remote perf --suite vt`, 10 runs a side in bhyve) gives 0.695 of FreeBSD's time:
  `vt.cat` 0.642, `vt.color` 0.693, `vt.hidden` 0.668, `vt.scroll` 0.786, all wins (the same with
  fonts). Since 2026-10-24: fonts per window (`PIO_VFONT`, so `vidcontrol -f`),
  `CONS_SETKBD`/`CONS_RELKBD`, `KDENABIO`, the `FBIO_*` requests, and the bell (devctl
  `VT BELL RING`, `kern.vt.enable_bell`, the PC speaker on amd64).
- `kbdcontrol` and `vidcontrol` run: built static from FreeBSD's sources onto the memory disk until
  dynamic linking (P6) lets base's own run, `tests/user/vtools.c` loads a font, the history size,
  colors, a keymap, and the repeat rate with them, and passes on FreeBSD 15.1 with base's.
- The framebuffer maps through `ttyv*` for X's `scfb` (`vt_fb_mmap`): device objects in `sys/vm`
  (the device pager's, write-combining) and `d_mmap` through the tty layer.

**U.5 USB mass storage.** Done 2026-10-24.
- `umass` over Bulk-Only Transport, and UAS (streams on USB 3, the command and status pipes on
  USB 2), on a small SCSI layer (`INQUIRY`, `TEST UNIT READY`, `REQUEST SENSE`, `READ CAPACITY`,
  `READ`/`WRITE` (10 and 16), `SYNCHRONIZE CACHE`) that publishes `da*` disks to the storage class.
- Error recovery (BBB reset recovery, sense handling, `UNIT ATTENTION` after a reset), and hot
  unplug of a mounted disk failing its I/O without a panic.
- Progress (2026-10-24): `umass` over Bulk-Only and UAS on `lib/scsi` (UAS IUs, `REPORT LUNS`),
  one `da*` per LUN, with `da`'s retries. xHCI bulk streams (a ring per stream, one stream cancelled
  at a time): over USB 3 each UAS tag is a stream, seven commands at once; over USB 2 the
  `READ READY`/`WRITE READY` handshake. A broken UAS command gets `ABORT TASK`, then `LOGICAL UNIT
  RESET`. In QEMU, on both arches, UFS2 with soft updates passes fsx and `fsck_ffs` on the
  Bulk-Only disk, the UAS disk on streams, and a UAS disk on USB 2 (ktest `ufs_on_usb_disks`);
  the UAS disk's second LUN is `da2`. The boot tier passes with `--usb` on both arches and bhyve
  (with USB buses holding the root mount until first explored, as FreeBSD's). A mounted disk pulled out during fsx (QMP `device_del`, or `umass0`
  detached in a ktest) fails its I/O without a panic and is unmounted forcibly, as FreeBSD's
  `ffs_fsfail_cleanup` does.

**U.6 Exit.** QEMU exit met 2026-10-24.
- The QEMU exit above, in the boot tier on both arches and bhyve, and the suite's verdict.
- Every boot-tier machine now has the USB and input set (`--no-usb` drops it). The new
  `<arch>-console` cases boot with `boot_multicons=YES`, so `/rescue/sh` is on `ttyv0`
  (`/dev/console` follows FreeBSD's `boothowto` rules, USB.md §7.4). Over QMP, the USB keyboard
  alone types `echo` and then `exit`, which powers the machine off. The virtio keyboard types
  too, and the USB mouse's evdev node is read while QMP moves it. Each result is read back from a
  `screendump`, cell by cell against the console font. UFS2 with soft updates passes fsx and
  `fsck_ffs` on the Bulk-Only disk, on UAS over streams, and on UAS over USB 2 (ktest
  `ufs_on_usb_disks`). bhyve's `xhci,tablet` tree matches FreeBSD's recording. All twelve
  boot-tier cases pass on the build server.
- Suite verdict: `vt` text output takes 0.695 of FreeBSD 15.1's time (U.4), inside the gate. USB
  mass-storage throughput joins the suite on the physical machines at P6, with the hardware half
  of this exit.

**Performance** ([ADR-0015](adr/0015-usb-input-vt.md)). The track adds a component that can be
measured fairly before hardware: `vt` text output (a `cat` of a large text file to `ttyv0`,
and a scrolling workload), against FreeBSD's `vt_efifb` in the same bhyve VM with a framebuffer.
USB mass-storage throughput (sequential and random, BBB on both sides) cannot be compared fairly in
an emulator, since FreeBSD's host cannot run QEMU with acceleration and bhyve has no USB disk; it
joins the suite on the physical machines at P6.

## Phase 6: FreeBSD multi-user (XL) — in progress

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

**Exit:** unmodified FreeBSD 15.1 `base.txz` boots **multi-user** from UFS2 on both arches (QEMU
and bhyve; the physical machines follow in P8). Login works on the serial console and on `ttyv0`.
`syslogd`, `cron`, and `devd` run. The kyua pass rate meets the agreed target for kern/, lib/libc,
bin/, and usr.bin/ in bhyve. The P6 performance suite (boot time, `libthr`/umtx, `kqueue`, UNIX
sockets, ptys, dynamic exec) is within 2% of FreeBSD in bhyve.

**Design.** [MULTIUSER.md](MULTIUSER.md) ([ADR-0016](adr/0016-multi-user.md)): FreeBSD's own boot
recorded as the test plan, processes split into threads as FreeBSD's are, umtx in a host-tested
`sys/umtx`, one readiness type under `poll` and kqueue, a protocol-neutral socket layer with UNIX
domain first, a device trait under the line discipline, typed privilege and Capsicum rights, a
jail tree, and no networking until P7 (`AF_INET` behaves as on a kernel without `INET`).

**Reference configuration.** The P5 machines (QEMU `q35` and `virt`, bhyve) with the USB and input
set, and an NVMe disk holding a GPT with the ESP and a UFS2 root with all of FreeBSD 15.1's
`base.txz`, `/etc` as `bsdinstall` leaves it (`rc.conf` with a hostname, the time zone, `fstab`).
The FreeBSD 15.1 guests boot the same disks for the baselines. The X870E and the Orange Pi 6 Plus
join in P8.

**kyua target** (ADR-0016): for each of `kern/`, `lib/libc/`, `bin/`, and `usr.bin/`, at least 95%
of the test cases that pass on FreeBSD 15.1 in the same machine, with every other failure listed
with its reason.

### Milestones

Each milestone ends green on both architectures, with CI passing.

```
P6.0 ─► P6.1 ─┬─► P6.2 ─┬─► P6.3 ───────────────────┐
              │         └─► P6.8 ───────────────────┤
              ├─► P6.4 ─► P6.5 ─► P6.6 ─────────────┼─► P6.9 ─► P6.10 ─► P6.11
              └─► P6.7 ─────────────────────────────┘
```

**P6.0 Design and harness.**
- MULTIUSER.md and ADR-0016.
- First-use telemetry (MULTIUSER.md §2): every unimplemented system call, sysctl node, ioctl,
  `fcntl`, `procctl`, socket option, and `sysarch` request is logged once per program, in one
  greppable form.
  - Done 2026-10-24: `kernel/src/firstuse.rs` (`first use: <kind> <what> by <program>`), hooked
    into system calls, sysctl lookups, reads and writes, device and `FIO*` ioctls, `fcntl`, and
    `sysarch`; `procctl` and socket options join as they land.
- `cargo xtask fs-image --base` builds the reference disk (all of `base.txz`, `bsdinstall`'s `/etc`);
  the `<arch>-multiuser` boot cases exist, expected to fail, with the serial and `ttyv0` login
  scripts.
  - Done 2026-10-24: the installed root (`image::build_installed_root`: the UFS2 root with
    `bsdinstall`'s `rc.conf` and time zone; root keeps base's empty password), the
    `x86_64-multiuser`, `aarch64-multiuser`, and `x86_64-bhyve-multiuser` cases (`XFAIL` until
    P6.9), and `cargo xtask --remote freebsd-multiuser`, whose run of the same script passes on
    FreeBSD 15.1 in bhyve, QEMU amd64, and QEMU arm64.
- kyua built and its FreeBSD 15.1 results recorded per test case on each machine.
  - Done 2026-10-25 (bhyve): the release image ships `kyua` and `/usr/tests`, so nothing is
    built; `cargo xtask --remote kyua-baseline` runs the suite in bhyve one directory at a time,
    surviving panics, and its results are in `tests/kyua/freebsd/`. Of 9,845 cases: 8,442
    passed, 1,241 skipped, 38 expected failures, 107 broken, 6 failed, and 11 crashed (the guest
    went down in `usr.sbin/ctladm`); about 42 minutes on 8 vCPUs. Not under QEMU: emulated, the
    suite would take days, and its timing tests would measure the emulator. The Orange Pi 6 Plus
    records arm64's in P8.
    AbyssBSD's runs will need `tests.txz` in the installed root (it is not in `base.txz`).
- The P6 suite in `ubench` (MULTIUSER.md §16) with FreeBSD's baseline in bhyve.
  - Done 2026-10-24: suite `p6` (`thr`, `event`, `unix`, `pty`, `exec`; tests/perf/README.md), a
    second `ubench` build with `libthr`; `perf --suite p6 --freebsd-only` records the baseline.
    `boot` is measured by the harness once AbyssBSD reaches `login:` (P6.9).

**P6.1 Run-time linking.**
- `ET_DYN` executables and `PT_INTERP` (MULTIUSER.md §6): PIE and the interpreter at random bases,
  the full auxiliary vector, `ld-elf.so.1` run directly, the feature-control note.
  - Done 2026-10-26: `exec` follows
    `imgact_elf.c`'s order (the PIE's base, its sections and limits, `anon_loc`, the interpreter,
    the stack), and the boot shell is base's `/bin/sh` when the root has it. The shell cases'
    `DYNAMIC` script (base's `ls`, `ps`, `tar`, `find`, `ldd`, rtld run directly, `LD_BIND_NOW`,
    `LD_PRELOAD`, `LD_LIBRARY_PATH`, `kbdcontrol`, `vidcontrol`) passes on both arches and against
    FreeBSD. On arm64, `AT_HWCAP`/`AT_HWCAP2` come from FreeBSD's user view of the ID registers
    (host-tested `lib/idregs`: merged over every CPU, unexposed fields hidden), which user mode
    reads with an emulated `mrs`. On amd64 the signal trampoline is the code of a vdso shaped as
    FreeBSD's (`elf-vdso.so.1`, `__vdso_sigcode@@FBSD_1.7`, its call frame information; written by
    host-tested `lib/elf`'s `vdso`), which rtld preloads as `[vdso]` from `AT_KPRELOAD`; an
    unwinder in a handler walks through it back to the interrupted code (`tests/user/kpreload.c`).
- The VM rtld relies on: reservations, `MAP_EXCL`, `MAP_ALIGNED_SUPER`, `PROT_MAX`, RELRO, `ETXTBSY`,
  and a grow-down stack to `RLIMIT_STACK` with the guard page and stack gap.
  - Grow-down stacks done 2026-10-26: exec reserves `MAXSSIZ` and maps `SGROWSIZ` of it, the rest a
    gap the stack grows into on fault, to `RLIMIT_STACK` (FreeBSD's `DFLSSIZ` by default, applied
    at once by `setrlimit`) and never into the guard page; `MAP_STACK` makes the same for thread
    stacks, `MAP_GUARD` a guard `mprotect` passes over (host-tested in `sys/vm`,
    `tests/user/stack.c`).
  - The rest done 2026-10-26, as `kern_mmap` and `vm_map_protect` have them: `PROT_MAX` on
    `mprotect` too (it only lowers a maximum), `MAP_FIXED | MAP_EXCL`, `MAP_ALIGNED(n)` and
    `MAP_ALIGNED_SUPER` (a fixed address need only be a page's), `MAP_32BIT`, the two reserved
    flags ignored, and rtld's loading of an object (a `PROT_NONE` reservation, segments mapped
    fixed into it, RELRO, bss) in `tests/user/mapflags.c`, which passes on FreeBSD 15.1 too.
    `ETXTBSY` came with the interpreter held as text.
- `utrace` (to ktrace later; accepted and dropped until P6.8), the `kern.elf64.*` and `kern.proc.auxv`
  nodes.
  - Done 2026-10-26: `utrace` succeeds and drops the record, as for a process nobody traces;
    `kern.elf64.*` as 15.1 has them (read-only until sysctl writes),
    `kern.ps_strings`, `kern.usrstack`, `kern.stackprot`, and `kern.proc.auxv`
    (`tests/user/auxv.c`).
- Exit: base's own dynamic `/bin/sh` runs as the boot shell from the UFS2 root, `ls`, `ps`, `tar`,
  `kbdcontrol`, and `vidcontrol` from `/bin` and `/usr/bin` run, rtld's `LD_*` variables and
  `ldd` work, and the P4–P5 user tests pass built dynamic.
  - The user tests built dynamic done 2026-10-26: each `tests/user` program (but `vdso`, which
    reaches into libc) is built again as a PIE into the UFS2 root's `/usr/tests/abyss-dynamic`, and
    the shell cases run them all in a session of their own (`DYNAMIC_TESTS`); all pass on both
    arches but `tty`, left out until ptys (it uses the console the shell is on).

**P6.2 Threads.**
- The process/thread split (MULTIUSER.md §4): per-thread signal state, TLS bases (FSGSBASE on
  amd64), user contexts, rusage, names, `cpuset`, and rtprio; FreeBSD's tid allocation.
  - Done 2026-10-26: `kernel/src/uthread.rs`'s `UserThread` (the scheduler thread's owner) has
    the mask, the thread-directed pending signals, the alternate stack, the `sigfastblock` word,
    its name, class and priority (`rtprio_thread`, any thread by tid), its CPUs (`cpuset` per
    thread; a process's is their union), and amd64's TLS base; tids come from FreeBSD's
    `tid_alloc` order from 100000, with `kern.maxthread` and `kern.threads.*`. The scheduler
    keeps both user thread registers per thread (amd64's FS and GS bases, written only when they
    differ from what the CPU holds); `AMD64_GET/SET_GSBASE` and `AMD64_GET_XFPUSTATE` are in.
    `kern.proc` has an entry per thread for `_td` selections and `all`; the process CPU clock sums
    its threads, those gone included. `getrusage` waits for §4.6.
- `thr_new`, `thr_create`, `thr_exit`, `thr_kill`, `thr_kill2`, `thr_suspend`, `thr_wake`,
  `thr_set_name`; single-threading for exit, exec, and core dumps; `fork` of a threaded process.
  - Done 2026-10-26, as `kern_thr.c` and `thread_single` have them: a new thread copies its
    creator's registers, mask, and class (amd64 its FP state too; arm64 starts it afresh), the
    last thread's `thr_exit` returns for libthr's `exit`, exit single-threads the others out and
    exec parks them at the boundary until its image is in (`ERESTART` from their sleeps), and
    `fork` copies the calling thread. There are no core dumps to single-thread for yet.
- Signals to threads (`sigtd`), per-thread masks and `sigwait`, stops of every thread.
  - Done 2026-10-26: a process-directed signal goes to `sigtd`'s pick and moves to another thread
    if that one blocks it first or leaves (`reschedule_signals`); faults, `SIGPIPE`, `SIGSYS`, and
    `thr_kill` are thread-directed; a stop parks every thread (in place in a sleep) and the parent
    hears once all have. `tests/user/threads.c` passes on FreeBSD 15.1 on both arches.
- The arm64 SVE signal context.
  - Done 2026-10-26: the frame carries a `struct sve_context` at the vector length, as
    `sendsig_ctx_sve` writes it, and `sigreturn` loads one back with `mc_fpregs` setting the low
    128 bits (`tests/user/sve.c`, on FreeBSD 15.1 arm64 through `freebsd-usertests`). FreeBSD
    writes the state it last saved there, ours is the interrupted one; the test checks only
    what both do.
- Exit: libthr's own tests that need only the P4 umtx operations, a ktest of single-threading under
  load (threads in sleeps, faults, and system calls while another execs and another exits), and the
  "Signal state" deviation closed.
  - The ktest done 2026-10-26 (`single_threading_under_load`: four `threads stress` at once), and
    the deviation closed.
  - libthr's tests done 2026-10-27: the UFS2 root carries `tests.txz`'s `lib/libthr`, and the shell
    cases run each case with `run-atf` (no kyua until interval timers) and hold it to FreeBSD
    15.1's recorded result: 60 cases on amd64 and 59 on arm64 end as FreeBSD's, and the 10 that
    need P6.3's kernel condition variables, rwlocks, and timed mutexes or P6.4's `setitimer` are
    listed in `tests/kyua/known-failures.txt`. Getting there brought `UMTX_OP_NWAKE_PRIVATE`, `#!`
    scripts (`exec_shell_imgact`), `sigqueue` to a thread, `sched_get_priority_{min,max}`,
    `kern.sched.cpusetsize{,min}`, `kern.vm_guest`, `kern.hostname`, FreeBSD's `cpuset` sizes,
    and `getcontext` without the bases.

**P6.3 umtx and libthr.**
- `sys/umtx` (MULTIUSER.md §5): keys, chains, every `_umtx_op` operation, priority inheritance and
  protection, robust lists, `UMTX_OP_SHM`, timeouts on any clock; host tests against a model.
- POSIX shared memory (`shm_open2`, `memfd_create`, seals, `shm_rename`) and System V IPC.
- Exit: kyua's `lib/libthr` and `lib/libc/gen` thread tests at the target, process-shared and robust
  mutexes across processes in a ktest, and the `thr` component measured.
  - `sys/umtx` done 2026-10-27: FreeBSD's protocols operation by operation behind an `Env` the
    kernel implements (user words by atomic single accesses and compare-and-swap, keys by address
    space or by memory object, sleeping, the clocks), with priority lending through the scheduler's
    new user-lent priority; host tests run libthr's user-space halves against it with random
    interleavings. Thread exit, process exit, and exec release robust mutexes; a fork keeps the
    caller's robust lists, as FreeBSD's.
  - POSIX shared memory, `UMTX_OP_SHM`, `fspacectl` on it, write mappings counted per object (write
    seals), sized objects (`SIGBUS` past the end), and System V shared memory, semaphores, and
    message queues done 2026-10-27.
  - The shell cases run `tests.txz`'s `lib/libthr`, `lib/libc/gen`, `sys/posixshm`, and the System V
    tests (`sys/kern/sysv_test`, `lib/libc/sys/msg*_test`) as kyua would (each case's own `TMPDIR`,
    `require.user=unprivileged` cases as `tests`): every case ends as on FreeBSD but those listed in
    `tests/kyua/known-failures.txt` for P6.4, P6.7, and P6.8. Getting there brought `rfork`
    (`RFSPAWN`), `getpriority`/`setpriority`, the `sched_*` policy calls, `execve`'s empty-`argv`
    check, the permission check before `#!`, `getdirentries`' `ENOTDIR`, and a vfork parent released
    only once the child is a zombie.
  - The ktest `pshared_across_processes` (two `tests/user/pshared` at once).
  - The `thr` component measured 2026-10-27 (`perf --suite p6.thr`, its 19 benchmarks against
    FreeBSD 15.1 in bhyve, 10 runs a side): geometric mean 0.899, within the gate. Mutexes,
    process-shared mutexes, rwlocks, and both ping-pongs match or beat FreeBSD (ping-pongs about
    4x). Open diagnostics: `thr.mutex.pi.t4` and `t8` take about 40x FreeBSD's time, the CPUs
    busy on one chain's lock and busy mark rather than sleeping, where FreeBSD's word is free in
    user space most of the time (a spin before sleeping on a busy chain, as FreeBSD's
    `BUSY_SPINS`, helped PI a little and hurt the rest, and was dropped); `thr.create` 1.28x.

**P6.4 Events.**
- `Notify` (MULTIUSER.md §7.2) on every object that can wait: pipes, FIFOs, terminals, devices
  (`evdev`, `devctl`), vnodes, processes, process descriptors.
- `poll`, `ppoll`, `select`, `pselect`; kqueue with `kqueuex` and every filter of §7.4; the VFS's
  `EVFILT_VNODE` notes.
- Timers: `setitimer`, POSIX timers, `timerfd`, `eventfd`, `clock_nanosleep`, clock setting.
- Exit: kyua's `kern/kqueue_*`, `lib/libc/sys` poll and select tests, and the timer tests at the
  target, and the `event` component measured.
  - `sys/event` done 2026-10-27: `Notify` (pollers and knotes of one object), knotes through
    FreeBSD's states, and kqueue registration, scans (a scan stops at what was queued after it
    began, FreeBSD's marker), descriptor close, close, and `KQUEUE_CPONFORK` copies; host tests
    include notifications racing scans. Pipes, FIFOs (FreeBSD's named-pipe end of file and
    `f_pipegen`), terminals, evdev clients, vnodes (`EVFILT_VNODE` notes from the generic VFS
    layer, as the `vop_*_post` hooks raise them), processes and process descriptors (`NOTE_EXIT`,
    `FORK`, `EXEC`, `TRACK`; FreeBSD 16's `NOTE_REAP` is kept as a hint, not in 15's mask), signals, `EVFILT_TIMER`, `EVFILT_USER`, `EVFILT_FS`, and
    kqueues in kqueues have filters and poll.
  - `poll`, `ppoll`, `select`, `pselect`, `kqueue`, `kqueuex`, `kevent`; `setitimer`, POSIX
    timers, `timerfd`, `eventfd`, `clock_nanosleep`, `clock_settime`, `settimeofday`,
    `adjtime` (stepping), `clock_getcpuclockid2`, and `wait6` done 2026-10-27; `abi-check` holds
    (the 16-only `NOTE_PDSIGCHLD`, `NOTE_SIGNAL`, `NOTE_REAP`, and P6.3's `CVWAIT_UMTX_TIME` are
    left out of it, and `NOTE_PCTRLMASK` is 15's). User timers run on
    callouts whose handlers run on a softclock thread, as FreeBSD's do.
  - The shell cases run `sys/kqueue`, `sys/kern/pipe`, `sys/kern/timerfd`, and `lib/libc/sys`'s
    clock, timer, `kevent`, `poll`, `select`, `nanosleep`, and `sigtimedwait` tests as kyua does
    (programs from their Kyuafiles, plain programs as kyua reports them, `SIGINT` and `SIGQUIT`
    at their defaults): 417 cases on amd64 (QEMU and bhyve) end as on FreeBSD, and arm64's too,
    but those `tests/kyua/known-failures.txt` lists (`socketpair` for P6.5, two `timerfd` cases that
    pass where FreeBSD's fail, and one arm64 artifact). The 12 cases P6.2 and P6.3 left for
    P6.4 pass. Getting there brought `rfork(RFPROC)` (a shared descriptor table), a partial
    `F_KINFO`, and a larger ESP in the test images (the debug kernel had outgrown it).
  - The `event` component measured 2026-10-27 (`perf --suite p6.event`, 10 runs a side):
    geometric mean 0.756, within the gate. `poll` and `select` on 1000 descriptors, the
    `EVFILT_TIMER` round trip, and the `EVFILT_USER` ping-pong beat FreeBSD (0.70, 0.83, 0.79,
    0.26). Open diagnostic: `kevent` on pipe readiness, 1.23 (one pipe) and 1.28 (1000).

**P6.5 UNIX-domain sockets.**
- `sys/socket` (MULTIUSER.md §8): the socket layer, socket buffers, the protocol trait, every
  socket system call, control messages.
- UNIX domain: stream, seqpacket, datagram, `socketpair`, `SCM_RIGHTS` with the collector,
  `SCM_CREDS`/`SCM_CREDS2`, `LOCAL_PEERCRED`; the "UNIX-domain sockets" deviation closed.
- Exit: kyua's `kern/unix_*` and `lib/libc/sys` socket tests at the target, `libcasper` services
  work, `logger` reaches a `syslogd` started by hand, and the `unix` component measured.
  - `sys/socket` done 2026-10-27: FreeBSD 15.1's UNIX-domain protocol on its own socket buffers
    (mbufs carved and charged as `mc_get` does, so buffers fill where FreeBSD's do): stream and
    seqpacket sends straight into the peer's receive buffer, datagram queues per connected
    sender, listen queues, `socketpair`, shutdown, `SO_*` and `LOCAL_*` options, `SCM_RIGHTS`
    with the collector, `SCM_CREDS`/`SCM_CREDS2` (`LOCAL_CREDS`, `LOCAL_CREDS_PERSISTENT`),
    the timestamps, `LOCAL_PEERCRED`, poll and kqueue filters; host tests. Every socket system
    call, `net.local.*`, `kern.ipc.maxsockbuf` and kin, `kern.openfiles`, `kern.proc.nfds`, and
    `fchmod` on sockets; the "UNIX-domain sockets" deviation narrowed to what is left.
  - The shell cases run `sys/kern/unix_*`, `socket_msg_*`, `shutdown_dgram`, `listener_wakeup`,
    and `lib/libc/sys`'s socket tests as kyua does: 531 cases on amd64 (QEMU, bhyve) and 527
    on arm64 end as on FreeBSD but those `tests/kyua/known-failures.txt` lists (`AF_INET`, P7;
    jails, P6.7; ktrace, P6.8; AIO). The shell case also runs base's `head` through Casper
    and `logger` to a `syslogd` started by hand. Getting there brought process descriptors'
    `FREAD | FWRITE`, kyua's 300-second case timeout (with a watchdog that takes its `sleep`
    along), and a 2 GiB shell VM (FreeBSD's own; `random_eor_and_waitall` takes 100 MiB).
  - The `unix` component measured 2026-10-27 (`perf --suite p6.unix`, 10 runs a side):
    geometric mean 0.846, within the gate. Connect (0.31), stream and seqpacket ping-pong
    (0.29), and datagrams (0.70) beat FreeBSD. Open diagnostics: stream bandwidth (4 KiB
    writes 4.1x, 64 KiB 2.7x: an allocation per mbuf and a poller per wait) and
    `SCM_RIGHTS` passing (1.53x).

**P6.6 Terminals.**
- The tty device trait (MULTIUSER.md §9), ptys (`posix_openpt`, `/dev/pts`, `/dev/ptmx`, packet
  mode), `revoke` and devfs revocation, the console UART as `ttyu0` and `cuau0`, other UARTs as
  terminals, `/dev/console` by `boothowto`.
- Exit: `script`, `expect`-style tests (`tests/user/pty.c`), and kyua's tty tests at the target, the
  "Terminals", "Session end", and "Consoles without `RB_MULTIPLE`" deviations closed, and the
  `pty` component measured.
  - Terminals done 2026-10-28: output is queued and the device drains it (`Driver::outwakeup`,
    FreeBSD's `ttydevsw`), with FreeBSD's block queues sized from the speed, its watermarks, and
    refused input when lines wait (`sys/tty`, host-tested); a terminal's first open takes its init
    state, its last close drains for up to a second and empties it; `cuauN`, the `.init` and
    `.lock` nodes, `TIOCEXCL`, the carrier wait, `TIOCDRAIN` and the drain wait. Pseudo-terminals
    (`posix_openpt`, `/dev/pts/N` owned by the caller with group `tty`, packet mode, `TIOCSIG`,
    `FIODGNAME`; no `/dev/ptmx`, as GENERIC has none). `revoke(2)` and devfs revocation (a revoked
    node's files are dead as on FreeBSD: end of file for a terminal's read, `ENXIO` to write,
    `ENOTTY` to `ioctl`); a session leader's exit revokes the node it took its terminal through.
    Every 16550 but the console's is an interrupt-driven `ttyuN` (units wired as GENERIC's hints
    wire them; x86 QEMU gains a COM2, tested by loopback); kernel messages go to the best console
    alone without `RB_MULTIPLE`. `RLIMIT_NPROC` came with `forkpty_test`. The "Terminals",
    "Session end", and "Consoles without `RB_MULTIPLE`" deviations are closed; what is left is in
    "Second PL011s" and "Terminal requests".
  - `tests/user/pty.c` (naming, echo, signals, `SIGWINCH`, packet mode, a megabyte each way, the
    ends from both sides, revocation) passes on FreeBSD 15.1 and on both arches; the shell cases
    run `script` and kyua's `sys/kern/tty`, `tty_pts`, `lib/libc/ttyio`, and
    `lib/libutil/forkpty_test` (porch's scripts skipped as on FreeBSD, `run-atf` now honoring
    `required_programs`): 543 cases on amd64 (QEMU, bhyve) and 539 on arm64 end as on FreeBSD.
  - The `pty` component measured 2026-10-28 (`perf --suite p6.pty`, 10 runs a side): geometric
    mean 0.436, within the gate; raw writes (0.25), the `script`-style relay (0.38), and canonical
    lines (0.88) all beat FreeBSD.

**P6.7 Credentials and security.**
- `priv_check` (MULTIUSER.md §10.1) replacing every `euid == 0`; set-id exec, `P_SUGID`, `nosuid`,
  `setlogin`, login classes, `setcred`; every `RLIMIT_*` enforced.
- Capsicum: descriptor rights, capability mode with the generated table, `TRAPCAP`.
- Jails (`jail`, `jail_set`, `jail_get`, `jail_attach`, `jail_remove`, `chroot`, `fchroot`), securelevel,
  every `procctl` command.
- UFS2 extended attributes and ACLs (VFS.md §1.2's P6 item), checked by `fsck_ffs`.
- Exit: kyua's `sys/capsicum`, `kern/` jail and procctl tests, `lib/libc` set-id tests, and pjdfstest's
  permission and ACL tests at the target; `jail -c` runs a shell with its own hostname and root.
- Progress 2026-10-28:
  - Credentials are `sys/security`'s `Cred` (FreeBSD's `ucred`): `priv_check` with FreeBSD's
    privilege numbers and `prison_priv_check`, securelevel, and `cr_cansee`, `cansignal`,
    `cansched`, and `candebug`, used everywhere a uid was compared with 0. Set-id exec (not on a
    `nosuid` mount, in capability mode, or after `PROC_NO_NEW_PRIVS`) sets `P_SUGID` and checks
    descriptors 0 to 2; `setlogin`/`getlogin` per session, login classes, and `setcred`. Every
    `RLIMIT_*` is enforced (`CPU` by a watch, `FSIZE` with `SIGXFSZ`, `MEMLOCK` and `VMEM`,
    `NPROC`, `NOFILE`, `KQUEUES`, `NPTS`).
  - Capsicum: `cap_enter`, descriptor rights on every descriptor call, the `ioctl` and `fcntl`
    limits, capability mode from the generated table (`ECAPMODE`), lookups beneath a directory
    descriptor, and `PROC_TRAPCAP`.
  - Jails: the prison tree with libjail's `security.jail.param` sysctls, `jail_set`/`jail_get`
    (with `meta` and `env` and their keys, and a writable `meta_maxbufsize`), jail descriptors
    (`JAIL_USE_DESC` and the rest, `jail_attach_jd`, `jail_remove_jd`), `jail_attach`,
    `jail_remove`, the old `jail`, `chroot`/`fchroot`, per-jail hostname and securelevel, and
    `enforce_statfs`. A directory passed into another jail's tree takes `UF_RESOLVE_BENEATH`. The
    shell case runs `jail -c path=… host.hostname=inside.jail command=/bin/sh`. Every `procctl`
    command, with reapers (`PROC_REAP_*` as `reap_kill` answers; inherited zombies announced as
    their parent would have been) and `PROC_PDEATHSIG_CTL`. `fexecve` and `pdwait` came along.
  - UFS2 extended attributes in the inode's area, as FreeBSD lays them out, with soft-updates
    ordering; POSIX.1e ACLs in them on an `acls` mount (`vfs::acl`: access checks, default ACL
    inheritance, the mask kept with the mode, the `__acl_*` calls). The UFS read-write test sets
    access and default ACLs with `setfacl` and reads them back before and after `fsck_ffs -f -n`,
    on both arches. NFSv4 ACLs are not kept (the "Extended attributes and ACLs" deviation).
  - `tests/user/capsicum.c` and `tests/user/jail.c` pass on FreeBSD 15.1 and on both arches.
  - kyua (the shell cases): `sys/capsicum`, `sys/kern`'s reaper, pdeathsig, procdesc,
    logsigexit, jail, jaildesc, and jailmeta programs, `usr.sbin/jail`, and libc's set-id,
    `getlogin`, `setrlimit`, `kill`, `getgroups`, `chroot`, and `mlock` tests join the run: 628
    cases on amd64 and 624 on arm64 end as on FreeBSD, but those `tests/kyua/known-failures.txt` lists (`AF_INET` and
    `ifconfig`, P7; `ptrace`, P6.8; nullfs; audit). `run-atf` now runs cleanup routines and
    honors a program's `required_kmods`, as kyua does.
  - pjdfstest: every script as on FreeBSD on tmpfs and UFS2 but `chflags/09`'s 117 `jexpect` tests,
    whose `jail -s` gives the jail an IPv4 address (P7).

**P6.8 Observability.**
- `kern.proc` complete (threads, sizes, wmesg, CPU), `kinfo_file`, `kinfo_vmentry`, kernel stacks
  and the other `procstat` nodes, `vm.stats`, `cp_times`, load average, `kern.devname`, the `^T`
  line (MULTIUSER.md §12.1).
  - Done 2026-10-28: the scheduler keeps each sleep's wait message and channel (sleep sites
    named as FreeBSD's: `nanslp`, `wait`, `piperd`, `select`, `kqread`, `sigsusp`, `uwait`,
    `ttyin`, ...), whether it is interruptible, when it began, voluntary and involuntary
    switches, and ULE's `%CPU` window (`sched_pctcpu_update`, host-tested); a statistics
    clock at FreeBSD's 127 Hz, stopped while its CPU idles, charges each tick to
    `kern.cp_time`/`cp_times`' user, nice, system, interrupt, or idle and to the running
    thread, whose run time it divides into user and system time (`calcru`); the load average
    is `loadav`'s. `kern.proc` (`kernel/src/kinfo/`) fills sizes and resident pages (counted
    in the page tables), text, data, and stack sizes, wait channels, states, priorities in
    FreeBSD 15's ranges, rusage and the children's times, the flags, and jails; `kern.devname`
    (a sysctl answering what is written), `kern.proc.filedesc` and `kern.proc.cwd` (every
    descriptor type, paths from the name cache, Capsicum rights; `F_KINFO` shares it),
    `kern.ipc.posix_shm_list`, `vmmap` (object types, files' attributes and paths, the shared
    page as physical memory), `kstack` (symbolized from switched-out threads' frame
    pointers), `env` (read from the process's memory), `rlimit`, `rlimit_usage`, `umask`,
    `osrel`, `sigtramp`, `ps_strings`, `groups`, `sigfastblk`, `vm_layout`, `sv_name`, `kq`,
    and `kern.lockf`; `vm.stats.*`, `vm.vmtotal`, `vm.loadavg`, `vm.swap_info` (no swap),
    `hw.physmem` and kin, `kern.boottime`, `lastpid`, `fscale`, `ccpu`, `clockrate`,
    `argmax`, `bootfile`, `kern.smp.*`, and `machdep.consdev`. `^T` and `TIOCSTAT` print
    `tty_info`'s line. `cpuset_getid` came along; a file takes its name in the cache at its
    first lookup, not at its creation, as FreeBSD's (its `kf_path` is empty until then).
    `abi-check` covers the new structures. What is left is in compat-deviations.md's
    "Process information" row.
  - The differential harness (BUILD-AND-TEST.md §4, `xtask/src/diff.rs`, `tests/diff/`):
    scenario scripts, one normalizer run in each guest, FreeBSD 15.1's outputs recorded by
    `cargo xtask --remote freebsd-diff` into `tests/corpus/freebsd/diff/`, and the shell cases
    comparing AbyssBSD's line for line. Scenarios `ps` (several `-o` formats, `-l`, `-j`,
    `-u`, `-v`, `-auxww`, `-O`, `-c`), `psthreads` (`-H`), `top` (`-b`, `-H`), `fstat`,
    `procstat` (default, `-a`, `-b`, `-c`, `-e`, `-f`, `-fC`, `-i`, `-j`, `-k`, `-kk`, `-l`,
    `-r`, `-s`, `-S`, `-t`, `-x`, `kqueue`, `advlock`, `pwdx`, `pargs`, `penv`,
    `sigfastblock`, `rlimitusage`, `binary`), `vmmap` (`-v`), `status` (`^T`, `TIOCSTAT`),
    and `w` (`w`, `uptime`). kyua's `usr.bin/procstat` joins the shell cases' run. On both
    arches, every scenario but `vmmap` prints what FreeBSD's does (vmmap's differences, in
    `known-differences.txt`: AbyssBSD maps an executable's partial last data page from the
    file, and places `mmap`s by its own policy); the five `procstat_test` cases pass.
    `Signals`' dispositions moved to the heap: the 4 KiB array, copied twice in `fork`'s frame,
    overflowed arm64's kernel stack, and `statfs` keeps its `struct statfs` on the heap (a
    7 KiB frame left a timer interrupt too little stack).
- ktrace with every record type and a writer thread; `kdump` unmodified. `ptrace` for `truss` and
  `ptrace_test` (§12.3). Core dumps of threaded processes.
  - ktrace done 2026-10-28 (`kernel/src/ktrace.rs`, layouts in `lib/abi`'s `ktrace` module):
    `ktrace(2)` with `KTROP_SET`, `CLEAR`, `CLEARFILE`, `KTRFLAG_DESCEND`, process groups, and
    `KTRFAC_INHERIT`, `KTRFAC_ROOT` and `ktrcanset`; the file opened as `vn_open` does and written
    with the tracer's `RLIMIT_FSIZE`; tracing stops at a set-id exec unless the tracer may debug
    any credential. Records as FreeBSD's: written in the traced thread after what it queued
    (`ktr_submitrequest`), or queued on the process where it may not sleep and written on the way
    back to user mode (`ktr_enqueuerequest`, `ktruserret`); a request pool
    (`kern.ktrace.request_pool`, `KTR_DROP`), `kern.ktrace.genio_size`,
    `kern.ktrace.filesize_limit_signal`. Every record type: `SYSCALL` (all `sy_narg` arguments, the
    stack's past the sixth on amd64, which also fixes indirect `syscall(2)`'s sixth), `SYSRET`
    (`EJUSTRETURN` for `execve`, and the child's of `fork` and `thr_new`), `NAMEI`, `GENIO` (the
    read and write family, `sendmsg`/`recvmsg`), `PSIG`, `USER` (`utrace`), `STRUCT` (`stat`,
    `sockaddr`, `caprights`, `itimerval`, `cpuset_t`, `thrparam`), `STRUCT_ARRAY` (`kevent`,
    `pollfd`), `SYSCTL` (`sysctl.name`'s form), `PROCCTOR`/`PROCDTOR`, `ARGS`/`ENVS`, `FAULT`/
    `FAULTEND`, `CSW` (each named sleep, from the scheduler's wait messages), and `CAPFAIL` with FreeBSD's tracing of what capability mode would refuse
    (system calls, `F_KINFO`, `sysarch`, signals to others, `AT_FDCWD` and absolute lookups,
    `bindat`/`connectat`, `sendto` addresses, `shm_open` paths, rights and their widening, other
    processes' CPU sets, exec paths). `kern/ktrace_test` passes but its two `AF_INET` cases (P7).
  - ptrace done 2026-10-28 (`kernel/src/ptrace.rs`, layouts in `lib/abi`'s `ptrace` module): every
    request (`PT_COREDUMP` too: served by the stopped thread, `PC_ALL`, `PC_COMPRESS` as
    `kern.compress_user_cores` says); trace stops as stops of every thread, reported to the
    debugger's `wait` (`CLD_TRAPPED`, `WTRAPPED`); `PT_ATTACH` reparents and stops the process at
    once, sleepers sleeping on (`kern.ptrace_attach_transparent`); the real parent's `wait` sees
    its traced children (`p_orphans`), which go back to it once the debugger has seen them exit or
    detaches; `kern.kill_on_debugger_exit`. Stops at signals (as `sigprocess`, debugger-chosen
    signals marked `KSI_PTRACE`), system call entry and exit (`PT_TO_SCE`, `PT_TO_SCX`,
    `PT_SYSCALL`, `PT_GET_SC_ARGS`, `PT_GET_SC_RET`, `PT_SC_REMOTE` at the boundary), exec, followed
    forks and vforks (`PT_FOLLOW_FORK`, `PTRACE_VFORK`), and thread birth and exit
    (`PT_LWP_EVENTS`, exits on single-threading too); `PT_SUSPEND`/`PT_RESUME`; registers through
    each thread's entry frame and saved FPU state (`PT_GETREGS`, `PT_GETFPREGS`, `PT_GETREGSET`
    with `NT_PRSTATUS`, `NT_FPREGSET`, arm64's `NT_ARM_TLS` and `NT_ARM_ADDR_MASK`; amd64's FS, GS,
    and TLS bases); memory (`PT_IO`, `PT_READ_*`, `PT_WRITE_*`, writes giving a private mapping its
    own copy as `VM_PROT_COPY`); `PT_STEP` (amd64's TF; arm64's software step, `MDSCR_EL1.SS` kept
    to the returning thread's `SPSR.SS`, the OS lock opened at boot); `int3` from user mode.
    Signals now queue as FreeBSD's do (every one sent with information, up to
    `kern.sigqueue.max_pending_per_proc`, a child's `SIGCHLD` once). Numbered CPU sets
    (`cpuset(2)`, `cpuset_getid`, `cpuset_setid`, `CPU_LEVEL_CPUSET`) came with the tests that pin
    threads. `kern/ptrace_test` matches FreeBSD 15.1 on amd64 (`PT_ATTACH_no_EINTR` passes where
    FreeBSD's run timed out); arm64 too but `PT_KILL_competing_stop`.
  - Core dumps done 2026-10-28 (`kernel/src/coredump.rs`): a core-dumping signal stops every
    thread at the boundary and writes `kern.corefile` (`%N.core`, `%P`, `%U`, `%H`, `%S`, `%I`),
    mode 0600, as `coredump_vnode` checks it, within `RLIMIT_CORE`, unless `kern.coredump`,
    `kern.sugid_coredump`, or `PROC_TRACE_CTL` say otherwise; ELF64 `ET_CORE` with `PT_LOAD` for
    each mapping with memory and FreeBSD's notes (`NT_PRPSINFO`; per thread, the dumping one first,
    `NT_PRSTATUS`, `NT_FPREGSET`, `NT_THRMISC`, `NT_PTLWPINFO`, amd64's `NT_X86_SEGBASES` and
    `NT_X86_XSTATE`, arm64's `NT_ARM_TLS` and `NT_ARM_ADDR_MASK`; the `NT_PROCSTAT_*` notes
    `procstat` reads from cores). The wait status carries `WCOREFLAG` (`CLD_DUMPED`), and
    `kern.logsigexit` says "core dumped" or why not. `kinfo_proc` reports tracing (`P_TRACED`,
    `P_STOPPED_TRACE`, `ki_tracer`, `ki_traceflag`), and `PT_VM_ENTRY` mapped files' paths.
  - The differential scenarios `kdump` (ktrace and kdump on a fixed run of system calls, its
    forked child's records, the sleep's context switches) and `truss` (`truss -f` on the same).
    `__realpathat` names a file with several links by the directory and name it was found under
    (`vn_fullpath_hardlink`): with files named in the cache at lookup, `realpath /rescue/sh` had
    come back as whichever of `/rescue`'s links was looked up last. truss found that a `vfork`
    child's `PT_TRACE_ME` kept its signals from the debugger past exec (`P_PPTRACE` now goes
    there), and kdump that a forked child kept `AMD64_GET_TLSBASE`'s base (`cpu_fork` clears it)
    and that arm64's `sigreturn` returns as call -1.
  - Results: on both arches all nine scenarios but `vmmap` print what FreeBSD 15.1's do; the
    amd64 shell cases (QEMU and bhyve) have 702 of FreeBSD's test cases as on FreeBSD (697
    before); arm64's `ptrace_test`, `ktrace_test`, and `procstat_test` as on FreeBSD, but
    `PT_KILL_competing_stop`, which hangs in some runs.
    The `user_memory` ktests' page counts now wait for every CPU to let go of the test's space
    (`settled_free_pages`): one emulated arm64 run had counted 12 pages "leaked" while another CPU
    the thread had left still held it.
- Exit: `ps`, `top`, `fstat`, `procstat` (every mode), `w`, `kdump`, and `truss` output matches
  FreeBSD's in differential tests, kyua's `kern/ptrace_test` and `kern/ktrace_test` at the target, and
  the `kern.proc` deviation closed.

**P6.9 init and multi-user in QEMU.**
- `init` from `init_path`, `kenv`, single-user (`boot -s`), and the fallback to `/rescue/sh`
  (MULTIUSER.md §11.1).
- What `rc` needs (§11.2): `fsck -p` of the root, `mount -a`, devfs rules, `/dev/devctl` and
  `/dev/devctl2`, `/dev/klog`, the sysctl nodes `rc` and the daemons read, `utmpx`, `dumpon`/`savecore`
  with no dump device; the kernel stops mounting the ESP itself.
- `devd`, `syslogd`, `cron`, `getty`, `login` with PAM on stock configuration; `reboot`/`shutdown`
  with every file system unmounted clean.
  - init done 2026-10-29 (`kernel/src/init.rs`, `kenv.rs`, `shutdown.rs`): `kern.init_path`
    (the kernel environment's `init_path` first), each path tried as `start_init` does, `-s` for
    `boot_single`; "Going nowhere without my init!" when it exits. `kenv(2)` (get, set, unset,
    dump) over the boot command line's `name=value` words. `reboot(2)` as `kern_reboot` (sync and
    unmount unless `RB_NOSYNC`, `Uptime:`, then halt, ACPI power-off, or reset; `RB_REROOT` is
    `EOPNOTSUPP`), and `shutdown_nice` (the power button and console keys signal init). Without
    an init, the kernel's `/rescue/sh` stays the fallback, and the boot tier's shell cases ask for
    it with `abyss.shell=1`; with init, `rc` mounts `/tmp` and the ESP from `fstab`.
  - What `rc` needs: `/dev/devctl` (FreeBSD's event lines from the driver model, `SIGIO` and
    `FIOASYNC`), `/dev/devctl2`, `/dev/klog` with the message buffer's priorities; GEOM's
    `kern.geom.confxml`/`conftxt`/`confdot` and `kern.disks` (`lib/geom`, for `gpart` and
    `fsck`'s lookups); devfs rules (`DEVFSIO_*`, per mount, `nmount` of devfs); the module
    syscalls answering as a kernel with every module built in (`kldstat` shows FreeBSD's names);
    `kern.console`, `kern.random.harvest.*`, `kern.module_path`, `user.localbase`,
    `kern.boottrace.*`, `kern.proc.args` writes (`setproctitle`), and the other nodes `rc` and
    the daemons read.
  - Loopback `INET`/`INET6` from P7 (`sys/inet`, MULTIUSER.md §8.4), since `syslogd -s` exits
    without them: UDP and TCP to local addresses, `lo0` configured at boot as `rc.d/netif`
    leaves it, netlink's `NETLINK_ROUTE` and `net.routetable` for `ifconfig`, `route`, and
    `getifaddrs`, the `pcblist` sysctls for `netstat`, and the jail parameters `jls` asks for
    once `kern.features.inet` is there (`ip4`, `ip6`, their `addr` and `saddrsel`).
    `/rescue/ifconfig lo0` prints what FreeBSD's does.
  - Bugs found on the way: `rc.shutdown` hung writing to the console behind the login shell's
    read on the same terminal (device I/O now skips the offset lock, as FreeBSD's `FOF_NOLOCK`);
    `TIOCSCTTY` refused a terminal whose session's node had been revoked, so `rc.shutdown`'s
    output went to `init.log`; two spin locks taken together could unmask interrupts while one
    was still held (`sync::IrqOff`, for UNIX and internet sockets); getty flushes input typed
    before its prompt (the harness retypes the `ttyv0` login).
  - Results: `x86_64-multiuser`, `aarch64-multiuser`, and `x86_64-bhyve-multiuser` pass the
    whole script (serial and `ttyv0` logins, syslogd, cron, devd, `logger`, a pty, `procstat`,
    a cron job, `shutdown -p now`, a clean root) with empty first-use logs.
- Exit: on both arches and bhyve, the `<arch>-multiuser` boot cases pass: `login:` on the serial
  console and on `ttyv0`, login as root, the command script, `syslogd`, `cron`, and `devd`
  running, `shutdown -p now`, then `fsck_ffs -f -n` clean. The boot's first-use log holds nothing outside `absent.txt`. FreeBSD userland now runs end to end in QEMU (ADR-0010).

**P6.10 Conformance.**
- kyua nightly in bhyve, per test case against FreeBSD's results, with
  `tests/kyua/known-failures.txt` (not under QEMU: emulated, the suite takes days; arm64's run is
  on the Orange Pi 6 Plus from P8).
- `abi-check` over every structure crossing the boundary, for amd64 and arm64 headers
  (MULTIUSER.md §13), failing on unchecked types.
- Exit: the kyua target met in all four areas in bhyve; `abi-check` complete.
- `sys/kern/ktls_test` is out of the target until P7 (ADR-0016, amended 2026-10-29): it is 566 of
  `sys/kern`'s 972 FreeBSD passes, and needs TCP and kernel TLS.
- Progress 2026-10-29: `cargo xtask --remote kyua` runs the four areas in one 8-CPU bhyve boot in
  about an hour. Of FreeBSD's passes, `bin` passes 97.9%, `lib/libc` 97.3%, `usr.bin` 98.5%, and
  `sys/kern` 94.3–95.3% from run to run: about a dozen of its cases race with each other in a
  parallel run (a global clock, global sysctls, cpusets), on FreeBSD too but less often. Getting
  there brought inotify (`sys/vfs/src/inotify.rs`, which 15.1 has though MULTIUSER.md first
  counted it 16-CURRENT's), `SO_SPLICE` (`sys/inet/src/splice.rs`), `pdrfork`, `sched_getcpu`
  and `IA32_TSC_AUX`, a reaped child's resource use from `wait`, `kern.sig_discard_ign`,
  `kern.wait_dequeue_sigchld`, and fixes found on the way: a self-deadlock in
  `SO_ACCEPTFILTER` that froze the guest, a double fault freeing page tables, `module_stat`'s
  1024-byte name, `accept`'s order of checks, and more. Every other failure is in
  `tests/kyua/known-failures.txt`. `abi-check` covers every `#[repr(C)]` type in `lib/abi`
  (4804 assertions on amd64, 4717 on arm64) and fails on one it does not check. NMIs now print
  each CPU's backtrace and return, which is how the freeze was found.
- Exit met 2026-10-30: `bin` 98.2%, `lib/libc` 97.6%, `sys/kern` 95.6% (388 of 406), `usr.bin`
  98.5%, with no failure unlisted. The cpuset cases had not been racing: `cpuset_test:jail_nproc`
  narrows its jail's root set, and with no root set of their own, jails narrowed set 1 for every
  process after it. Jails now have root sets (`cpuset.id`), attaching rebases onto them, and a
  thread's own mask is tracked apart from its set's, as `kern_cpuset.c` does with anonymous sets.

**P6.11 Performance and exit.**
- The P6 suite (MULTIUSER.md §16) in bhyve.
  - Done 2026-10-30: `perf --suite p6` boots both systems multi-user from the root as the
    installer leaves it (NVMe, the same `rc.conf`), times the boot on the serial console, logs
    in as root, and runs `ubench p6` (tests/perf/README.md). Against FreeBSD 15.1, 10 runs a
    side, every component is within the bound: `boot` 0.166, `event` 0.841, `exec` 0.907, `pty`
    0.431, `thr` 0.855, `unix` 0.930 (geometric mean of all 40 benchmarks 0.761). FreeBSD's
    `boot.kernel` (12.6 s against 1.8 s) includes 6.5 s attaching `pci0` in bhyve and 1.7 s
    probing the PS/2 keyboard and mouse; `boot.rc` (4.25 s against 0.84 s) is `rc` alone, where
    FreeBSD's `devd` takes 1.7 s to start. Open diagnostics, as when each component was first
    measured: `thr.mutex.pi.t4` and `t8` (24x, 42x), `unix.stream.bw4k` and `bw64k` (4.2x,
    2.7x), `unix.rights` (2.0x), `kevent` on pipes (1.5x), and `thr.create` (1.38x).
- Exit: the phase's exit criteria above, in CI on both arches, and every component within the
  bound in bhyve.

## Phase 7: Networking (XL)

**Goal:** a networked FreeBSD system, in QEMU and bhyve, before the physical machines: board
bring-up (P8) then starts with `ssh`, `scp`, and `pkg` on both of them.

- `ifnet`, packet buffers, Ethernet, ARP, IPv4, ICMP, IPv6 + ND + SLAAC, UDP, TCP (RFC 9293
  baseline, SACK, window scaling, timestamps, NewReno + CUBIC), loopback, `bpf` (for `dhclient`
  and `tcpdump`).
- Socket API completeness for INET/INET6, `sendfile`, multicast basics.
- Kernel TLS in software (`TCP_TXTLS_ENABLE`, `TCP_RXTLS_ENABLE`; TLS 1.0–1.3 records with
  AES-CBC + HMAC, AES-GCM, and ChaCha20-Poly1305, written in-tree) and `sendfile` over it. kyua's
  `sys/kern/ktls_test` rejoins the `sys/kern` target here (ADR-0016).
- Control plane: routing tables (FIB), routing socket, **netlink** (`NETLINK_ROUTE` +
  `NETLINK_GENERIC` as used by 15.x `ifconfig`/`route`/`netstat`), and interface ioctls.
- Drivers: virtio-net (multi-queue, checksum/TSO offload). The physical machines' NICs come with
  them in P8.
- Differential tests against the build server for TCP behavior (packet captures).

**Exit:** in QEMU on both arches and in bhyve, the system obtains an address with `dhclient` and
SLAAC, `sshd` accepts logins, `scp` copies both ways, and `fetch` and
`pkg update && pkg install <curated set>` succeed. Sustained `iperf3` runs complete without stalls
or leaks. The kyua target is met in `sys/kern` with `ktls_test`. The P7 performance suite (TCP/UDP
throughput, request/response latency, connection rate, `sendfile`, CPU per gigabit) is within 2% of
FreeBSD over loopback and virtio-net in bhyve.

## Phase 8: Board bring-up (XL)

**Goal:** the P7 system, networked, on both physical reference machines, with AbyssBSD's own
drivers for their firmware, SoC, and NICs, and both machines gating every phase from here on
([ADR-0010](adr/0010-qemu-first-development.md)).

This is the first contact with real firmware. The clean-room specifications
(`~/Projects/OS/chinese-wall/specs/`) and the Q8B experience show that arm64 SoC firmware describes
clocks, resets, and power partly outside standard ACPI, and is sometimes wrong. The driver model
gains what that needs ([DRIVER-MODEL.md §16](DRIVER-MODEL.md#16-phasing)), held to the board
corpus rather than designed in the abstract.

- **Ground truth.** The Orange Pi 6 Plus's Linux and FreeBSD state captured into
  `tests/corpus/opi6p/` (MULTIUSER.md §15) before anything else runs on it; the X870E's tables,
  `pciconf`, and `devinfo` from FreeBSD 15.1 beside Fedora. Both machines' ACPI tables in the AML
  corpus, loading without warnings.
- **Deploy.** The Orange Pi 6 Plus boots AbyssBSD from USB, then its NVMe is reimaged with AbyssBSD
  and a FreeBSD baseline. The X870E: `cargo xtask deploy --machine x870e` onto the media NVMe only,
  by serial number; FreeBSD 15.1 beside it for baselines.
- **Driver-model extensions** (DRIVER-MODEL.md amendments of 2026-10-27), each host-tested against
  the corpus before it touches a board:
  - the platform description (§5.5), with the Sky1 entries (CIX's `CLKT`/`RSTL`/`RSVL` tables) and
    any the X870E needs;
  - the Order and Power edge kinds, and the new edge sources (`_DSD` references, `FixedDMA`, pin
    resources, serial-bus and GPIO operation regions) (§6.1);
  - the `Clock`, `Reset`, and `PowerDomain` provider traits and their first providers, including
    SCMI over whichever transport the Sky1 DSDT uses (§8.5, §9.3, §17 question 7);
  - `PciFunction::acpi_companion`, and firmware-shared and declared-shared ranges (§5.2, §8.1);
  - the firmware store (§8.6), if a P8 driver needs an image.
- **Drivers** for what both machines need to reach multi-user: storage, USB (PCI and ACPI xHCI),
  console, watchdogs, RTC, and their NICs (HARDWARE.md §4, §5; P7's stack over e1000e, igc, and
  Realtek RTL8126, the Orange Pi 6 Plus's only NIC), and the Sky1 controllers (GPIO, I2C, pin
  control) the dependency graph shows they depend on. No node may stay in `Waiting` without a
  named reason.
- The USB track's hardware exit on both machines.
- `cargo xtask perf --platform x870e|opi6p`, and kyua's arm64 baseline on the Orange Pi 6 Plus.
- The Radxa Dragon Q8B is Tier 2: its platform-description entries are welcome but not gating.

**Exit:** the `<arch>-multiuser` scenario passes on both machines (on the X870E from the
framebuffer and a USB keyboard alone). fsx with `fsck_ffs` passes on their NVMe and on a USB disk.
Their device trees match FreeBSD's `devinfo` wherever FreeBSD has a driver. The kyua target is met
on the Orange Pi 6 Plus. Every component of every suite so far is within 2% of FreeBSD on bare
metal on both machines (the Orange Pi 6 Plus per core class), P7's over their physical NICs too.

### Milestones

```
P8.0 ─► P8.1 ─┬─► P8.2 (X870E) ──────────┐
              └─► P8.3 ─► P8.4 (Sky1) ───┴─► P8.5
```

**P8.0 Ground truth.** The captures above, into the corpus. Every corpus DSDT loads, and the graph
builder reports each machine's edges and unresolved references.

**P8.1 Driver-model extensions.** The items above, host-tested against both corpora: the platform
description, edge kinds and sources, provider traits, the ACPI companion, shared ranges.

**P8.2 X870E.** Deploy to the media NVMe; multi-user from the framebuffer and a USB keyboard;
`amdsbwd`; the persistent log the loader saves to the ESP.

**P8.3 Sky1 providers.** SCMI (clocks, resets, power domains) and the FCH GPIO, I2C, and pin
controllers, each brought up against the captured tables and the board notes before it runs on the
board.

**P8.4 Orange Pi 6 Plus.** USB boot, then multi-user from USB, then the NVMe reimaged.

**P8.5 Conformance, performance, and exit.** kyua on the Orange Pi 6 Plus; every suite on both
machines against FreeBSD on bare metal; the exit criteria above.

## Phase 9: Self-hosting and hardening = **v1.0 / Day 1 complete** (L)

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
  beating FreeBSD rather than matching it. The P9 suite adds self-hosted build time, SU+J
  metadata workloads, and swap throughput.
- Documentation: installation guide ([INSTALLER.md](INSTALLER.md) (draft)), supported-hardware list,
  compat-deviations list. Forward design for the installed manuals: [MANUALS.md](MANUALS.md) (draft).

**Exit (v1.0):** the success criteria in [VISION.md](VISION.md#success-criteria-for-day-1-v10) are
met.

## Phase 10: RV64, Day 2 (L)

- `arch/riscv64`: S-mode entry from UEFI, Sv48 paging, traps, SBI (timer, IPI, HSM, SRST), and the
  AIA interrupt controllers (IMSIC/APLIC), all from ACPI (RHCT, MADT RINTC/IMSIC/APLIC).
- `abi` for FreeBSD riscv64 (struct layouts, signal frames, and the syscall convention:
  `ecall`, `t0` number, `a0..a7` arguments).
- Boot FreeBSD riscv64 `base.txz` multi-user on QEMU virt (`acpi=on`).
- **Board bring-up** on the Banana Pi BPI-SM10 (SpacemiT K3,
  [ADR-0017](adr/0017-rv64-reference-board-bpi-sm10.md), [HARDWARE.md §8](HARDWARE.md#8-board-profile-rv64-banana-pi-bpi-sm10-spacemit-k3)),
  once the QEMU exit holds, in the order P8 used for the other boards:
  - save the shipped state (SPI NOR dump, U-Boot environment) and Linux's view of the hardware;
  - get SpacemiT's K3 edk2 port running (UEFI with ACPI), and its tables into the AML corpus;
  - boot from NVMe in the M.2 2280 slot, console on UART0;
  - an RPMI provider for the firmware services, and the platform description (DRIVER-MODEL.md §5.5)
    for whatever the K3 tables get wrong.
- A PERFORMANCE.md rule for RV64 parity, since FreeBSD does not run on the board (ADR-0017).

**Exit:** the P6 exit criteria for RV64 on QEMU, and on the BPI-SM10 if its UEFI+ACPI firmware
works (otherwise the board is Tier 2 and the exit is QEMU's alone). Every HAL change the port needed
is recorded as an ADR or follow-up, which tells us how well the seams held.

---

## Later milestones (post Day 1)

### M-U: Userland

- Decide which base components to reimplement in Rust: init/rc replacement, libc (FreeBSD-ABI
  compatible), core utilities, and the pkg toolchain. Keep binary compatibility as the contract so
  stock and new components can mix.
- Rust `std` support for an AbyssBSD target (or keep `x86_64-unknown-freebsd` as the userland
  target, since we are ABI-compatible).

### M-G: Graphics

Forward design: [GRAPHICS.md](GRAPHICS.md) (draft; it proposes the DRM user interface as FreeBSD's
ports see it, implemented natively, and a step order for this milestone).

- Kernel modesetting interface (KMS-equivalent: CRTCs, planes, connectors, atomic commits, dma-buf
  style buffer sharing, sync files). Decide between FreeBSD/Linux DRM ioctl compatibility and a
  new interface. The decision drives which userland GPU stacks can be reused.
- Drivers: GOP-backed simple KMS, then virtio-gpu (2D, then 3D/venus), then a native GPU: AMD
  RDNA2, which the desktop's metal target and all three amd64 Tier 1 machines have
  ([ADR-0018](adr/0018-desktop-architecture.md)).
- evdev-compatible input interface: done in the USB track.
- The desktop (M-D) is the acceptance workload: its `undertow` compositor on `gop-kms`, then
  accelerated on virtio-gpu, then on RDNA2 ([DESKTOP.md](DESKTOP.md) §4).

### M-D: Desktop

Plan: [DESKTOP.md](DESKTOP.md) under [ADR-0018](adr/0018-desktop-architecture.md).

- **The desktop is the AbyssBSD Swift desktop** (`../AbyssBSD/desktop`): the `undertow`
  compositor over wlroots, the Aqua toolkit and shell, portals, its applications, the login window,
  application jails, and its installer. AbyssBSD designs no desktop of its own. It runs this one
  as FreeBSD userland, installed from packages built for FreeBSD 15.1, unmodified.
- The work here is the kernel beneath it. Most of what the desktop uses is P4–P6 ABI and is done.
  Still to schedule: nullfs, the MAC framework with `mac_priority`, `sound(4)` (OSS, `sndstat`,
  `snd_dummy`, then HDA), ACPI sleep and buttons, and ZFS (M-Z) for the installer's default
  layout and boot environments (DESKTOP.md §3.2).
- The desktop's own harness becomes a conformance suite, each lane compared with FreeBSD 15.1 as
  kyua is. Its headless lanes need no graphics, so **M-D.1 can start once P6 exits**, before
  M-G, and does not gate Day 1. First light follows M-G.1, acceleration M-G.3, and metal M-G.6.
- Changes the desktop itself needs (a 15.1 package build, a UFS install layout, arm64 packages)
  are made in the monorepo's plan, not carried here (DESKTOP.md §8).
- Prerequisites by step: P6 (headless), M-G (display), P7 (network panes, Firefox), audio, P9 power
  management (sleep), M-Z (ZFS install), INSTALLER.md (the boot loader for installed systems).

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
| Performance below FreeBSD's | AbyssBSD is not a credible replacement | The 2% bound gates every phase from P2 ([ADR-0011](adr/0011-performance-parity.md)), so losses are caught in the layer that causes them. Nightly cumulative suites. Hot-path designs weighed against ARM64 costs before P8 makes arm64 gating |
| Physical hardware variety | Slow v1.0 | A small set of Tier 1 machines, SystemReady-only on ARM64 |
| The X870E holds user data (Fedora NVMe, RAID array) | Catastrophic data loss | Non-designated disks attach read-only until P9. `xtask deploy` identifies the media NVMe by serial number. AbyssBSD never writes the Fedora ESP or boot entries |
| No serial console on the X870E | Blind debugging of early hangs | Framebuffer console, persistent RAM log saved by the loader, FCH watchdog, an optional PCIe serial card or polled network log |
| Bring-up on the Orange Pi 6 Plus costs power cycles | Slow iteration | QEMU first. Serial console from day one, the watchdog in P3. Capture the Linux/FreeBSD ground truth (ACPI dumps, register state) before the board is reimaged, and read it before experimenting |
| The desktop is another project's plan, on FreeBSD `main` | M-D waits on changes made elsewhere; the desktop drifts toward 16-only interfaces | Its harness runs against FreeBSD 15.1 here, so drift shows as a failing lane. The asks are listed in DESKTOP.md §8 and tracked in the monorepo's plan |

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
8. ~~Desktop~~: decided. The AbyssBSD Swift desktop, run as FreeBSD userland, with its packages
   built for FreeBSD 15.1 ([ADR-0018](adr/0018-desktop-architecture.md)). 16-CURRENT follows once
   the desktop is stable here.
