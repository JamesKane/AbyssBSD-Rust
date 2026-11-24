# The AbyssBSD Manifesto

AbyssBSD is a FreeBSD-compatible operating system built from scratch in Rust for modern 64-bit
hardware. It keeps FreeBSD's coherent architecture and stable userland ABI, drops thirty years of
legacy hardware and compatibility baggage, and uses Rust's type system to eliminate whole classes of
kernel bugs—without surrendering a single percent of performance.

This document states what AbyssBSD believes, what it refuses to accept, and the system it is being
built to become.

---

## I. FreeBSD Is the Specification

An operating system without software is a curiosity. FreeBSD offers something rare: a coherent,
documented, stable userland ABI and a cohesive base system designed as a single artifact rather than
assembled from competing daemons.

Unmodified FreeBSD 15.x 64-bit binaries run directly on AbyssBSD from day one. That gives us a
complete base system, a package ecosystem, and a permanent differential test oracle. Where POSIX is
ambiguous, FreeBSD semantics win. Where our behavior disagrees with FreeBSD without a documented
reason, we have a bug.

**We reject Linuxisms.** Where FreeBSD and Linux differ, AbyssBSD follows FreeBSD's design and
idioms:

- `kqueue` rather than `epoll`
- `devfs` and `devd` rather than `udev`
- `sysctl` rather than `procfs` and `sysfs`
- Jails and Capsicum rather than namespaces and `seccomp`
- `sound(4)` rather than a user-space sound server as a second authority
- Native Rust drivers rather than LinuxKPI

We do not adopt a Linux interface, daemon, or driver model because it is familiar. The only
exceptions are interfaces FreeBSD itself has adopted as part of its ABI (`evdev`, `netlink`,
`inotify`, and the user-facing DRM interface). Compatibility with other operating systems—whether a
Linux personality or a Windows NT personality—belongs behind a clean, opt-in boundary where native
FreeBSD processes pay zero cost.

---

## II. A Clean Slate for Modern 64-Bit Hardware

Thirty years of backward compatibility exact a permanent tax on every subsystem, every lock, and
every driver. AbyssBSD chooses a clean modern baseline:

- **64-bit only.** No i386, no 32-bit ARM, no `COMPAT_FREEBSD32`, no legacy syscall shims.
- **UEFI, ACPI, and GPT only.** No legacy BIOS, no CSM, no MBR-only disks, no ISA, no 8259 PIC, no
  i8042, and no device-tree board zoo.
- **Modern hardware from the floor up.** PCIe (ECAM), MSI/MSI-X, NVMe, AHCI, xHCI, x2APIC/IOAPIC,
  GICv3/ITS, IOMMUs (SMMUv3, VT-d, AMD-Vi), UEFI GOP framebuffers, and heterogeneous multi-core
  processors (P/E cores and big.LITTLE) are first-class citizens from the start.
- **One kernel per architecture.** No per-board kernel configurations. One `x86_64` image and one
  `aarch64` image boot every supported machine of their ISA.
- **Architecture-neutral by construction.** AMD64 and ARM64 are developed and gated side by side
  from the very first commit. Every abstraction is tested against two radically different
  architectures before it hardens—so x86 assumptions (port I/O, strongly ordered TSO memory, fixed
  4 KiB pages) never leak into the core. RV64 is the Day-2 proof that the seams were drawn right.

---

## III. Safety by Construction, Not by Discipline

Most critical kernel vulnerabilities are memory-safety, concurrency, and lifetime bugs. Human
discipline alone has never eliminated them in C kernels, and it never will. AbyssBSD puts the
compiler in charge of invariants:

- **Illegal states are unrepresentable.** Algebraic data types replace flag soup. Newtypes
  (`PhysAddr`, `VirtAddr`, `UserAddr`, `Pfn`, `Pid`, `CpuId`) prevent unit and address-space
  confusion at compile time. Typestate enforces hardware and object lifecycles.
- **Execution context is typed.** Interrupt handlers cannot sleep because the type system refuses to
  give them a sleepable context token. User memory is reachable only through `UserPtr<T>` and
  audited fault-recovering copies—the kernel cannot dereference a user pointer by construction.
- **Drivers are safe Rust.** Every crate under `drivers/*` is `#![forbid(unsafe_code)]`. The device
  framework owns driver lifecycles, typed hardware grants, revocation gates, and the dependency
  graph derived from ACPI. Detach, surprise removal, and shutdown drain every entry point safely by
  construction.
- **`unsafe` is small, confined, and accounted for.** It lives behind safe abstractions in `arch/*`,
  `lib/regs`, `sys/vm`, and `sys/sync`. Every `unsafe` block states its invariant in a `// SAFETY:`
  comment, and `unsafe` counts are tracked in CI.
- **Untrusted input never panics the kernel.** Data from user space, disks, networks, ACPI/AML
  firmware, and hostile or broken hardware produces typed `Result` errors. A `panic` means a kernel
  bug, never bad input.

---

## IV. Zero External Dependencies, Total Ownership

An operating system kernel should not rest on a shifting tower of third-party packages.

- **Stable Rust, built-in targets, zero crates.io dependencies.** The kernel and UEFI loader depend
  only on `core` and `alloc`. Host tooling uses `std` and nothing else. Everything we need—our UEFI
  loader, ACPI table parser, AML interpreter, ELF/GPT/FAT/UFS2 engines, TCP/IP stack, in-kernel TLS,
  collections, and build orchestration—lives in this repository as small, focused, host-testable
  crates.
- **Clean-room BSD-2-Clause provenance.** All code is newly written from hardware specifications,
  published protocols, and FreeBSD as a behavioral reference. GPL kernel code is never consulted.
  Vendor firmware blobs are treated as part of the hardware: loaded and driven, never reimplemented.
- **Monolithic and static by default.** The kernel is a single, statically linked ELF image with no
  unstable C KLD ABI. Loadable modules exist only as same-build Rust modules for large, rarely
  present drivers such as native GPUs, safely unloadable through the driver framework.
- **Pragmatism where userland demands it.** In the desktop layer (`desk/`), rewriting Mesa,
  `libwayland`, or `libinput` would yield code less tested than what it replaces. There—and only
  there—a strictly audited inventory of dynamically linked C libraries from FreeBSD packages is
  permitted behind checked FFI, with all untrusted C parsers confined inside Capsicum sandboxes.

---

## V. Speed Is Not Negotiable

A safe kernel that is slower than the C kernel it replaces is not a credible replacement.

From Phase 2 onward, every phase is gated on **performance parity**: every measured component must
be **no more than 2% slower** than stock FreeBSD 15.1 on the same machine, same configuration, and
same security posture—and the goal is to be faster.

- **Fair comparisons only.** Release build against release build, identical FreeBSD 15.1 userland
  binaries, identical speculative-execution mitigations, identical semantics. No winning by skipping
  `fsync`, weakening security, or doing less work.
- **Measure both systems the same way.** Profiles of our kernel alone only tell us where our time
  goes, not what FreeBSD avoids doing. We interleave runs, enforce noise bounds, and track
  cumulative suites across every phase and every core class on bare metal.
- **No "giant lock" era.** AbyssBSD is SMP and fully preemptible from the beginning, with per-CPU
  run queues, per-CPU allocator caches, lock-free epoch reclamation, and topology-aware scheduling
  built for heterogeneous cores.

---

## VI. Verification Over Confidence

Code is not done when it compiles; a phase is done when its exit criteria pass in CI on both
architectures.

- **QEMU and bhyve first; bare metal when ready.** Pure logic lives in `lib/*` and is tested on the
  host with `cargo test`, fuzzers, and Miri. Full subsystems are proven in QEMU and bhyve before
  touching physical machines, and physical reference machines gate merges once brought up.
- **Differential proof.** Struct layouts are verified against real FreeBSD 15.1 headers for both
  architectures (`abi-check`). Syscall tables are generated directly from `syscalls.master`.
  File-system images are stress-tested with `fsx` and validated by FreeBSD's `fsck_ffs`. Network
  behavior is checked by packet capture. FreeBSD's own `kyua` test suite runs against our kernel.
- **No silent stubs.** Unimplemented syscalls, ioctls, and sysctls return their documented error and
  log their first caller. Nothing pretends to succeed when it does not.

---

## VII. Beyond the Kernel: A Cohesive Whole

AbyssBSD begins as a kernel, but its ambition is a complete, cohesive operating system whose upper
layers honor the same principles as its lowest ones:

- **Native Graphics without LinuxKPI.** The kernel exposes the Direct Rendering Manager interface
  that FreeBSD's ports expect, backed by native safe-Rust display and GPU drivers (`gop-kms`,
  `virtio-gpu`, Sky1 display and `panthor`, AMD RDNA2) with fence-deadlock freedom by construction,
  so stock Mesa and Wayland run unmodified.
- **One Audio Engine, Two Faces.** No user-space sound daemon competing for authority. One in-kernel
  audio engine exposes both FreeBSD's `sound(4)` for universal compatibility and a native,
  timestamped, shared-memory stream interface whose real-time I/O cycle cannot block or allocate—
  bringing live rerouting, per-stream control, taps, and drift-corrected aggregate devices directly
  into the kernel.
- **Scoria: A Desktop for Gamers, Ricers, and Streamers.** Built in Rust in this workspace as
  ordinary FreeBSD userland, Scoria is a Vulkan 1.3 tiling Wayland desktop running on `kqueue`,
  `devd`, `Capsicum`, `mixer(3)`, and `SOCK_SEQPACKET` IPC—with no D-Bus, `udevd`, `logind`, or
  PipeWire in its core. It treats classical human-computer interaction research (Fitts's law,
  Hick–Hyman, direct manipulation, perceived stability, forgiveness, and contrast floors) as
  testable engineering requirements. *The content is the star; the desktop is the stage crew: fast,
  invisible when it should be, and never on stage by accident.*
- **Silent, Flicker-Free Boot and Honest Installation.** One mode set from firmware power-on to
  login, crash logs preserved in persistent RAM and readable on screen when no serial port exists,
  and a privilege-separated graphical installer that validates a complete plan before touching a
  disk and writes standard FreeBSD layouts.
- **Multiple Personalities, Zero Native Tax.** Clean-room `bhyve` (`vmm`) virtualization for
  unmodified `bhyve(8)` guests, an edge-translated 64-bit Linuxulator, and a native Windows NT
  personality for games with immutable, versioned per-title bundles—removing user-space emulation
  fragility while keeping the native FreeBSD path untouched.
- **Authoritative Manuals.** Modeled on the Amiga ROM Kernel Reference Manuals, AbyssBSD ships
  versioned, code-checked volumes—narrative guides and strict reference entries generated from and
  verified against the kernel itself—readable through `man`, a terminal reader, or the desktop.
- **The Long Horizon.** Self-hosting v1.0 on physical AMD64 and ARM64 machines, RV64 as Day 2,
  OpenZFS integration, and a gradual Rust userland that keeps FreeBSD binary compatibility as its
  permanent contract.

---

## VIII. The Commitment

AbyssBSD is built on a simple wager: that an operating system can be **memory-safe without being
slow**, **modern without being incompatible**, **principled without being dogmatic**, and
**complete without surrendering to complexity**.

We keep what has stood the test of decades. We discard what only survives out of inertia. We write
the rest cleanly, measure it honestly, and let the machine be the judge.
