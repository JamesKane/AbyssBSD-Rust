# Vision and Scope

## One-sentence vision

AbyssBSD is a FreeBSD-compatible kernel written from scratch in Rust for today's 64-bit UEFI/ACPI
machines. It keeps FreeBSD's proven designs, drops thirty years of legacy, and uses Rust's type
system to rule out whole classes of kernel bugs.

## Why

- **Memory safety where it counts.** Most serious kernel CVEs are memory-safety or
  lifetime bugs. A kernel whose `unsafe` code is confined, audited, and small changes the odds.
- **A clean slate for modern hardware.** UEFI, ACPI, PCIe, MSI-X, NVMe, xHCI, GICv3, x2APIC are the
  baseline. No BIOS, no ISA, no PIC, no 32-bit, no device-tree board zoo.
- **FreeBSD as the specification.** FreeBSD has a coherent, documented, stable userland ABI and
  a cohesive base system. Running its binaries unmodified gives us a full OS on day one and a test
  oracle forever.
- **Architecture-neutral by construction.** AMD64 and ARM64 are built together from the start, so
  every abstraction is tested against two very different ISAs before it hardens.

## Goals

1. **Binary compatibility** with unmodified FreeBSD 64-bit native userland (see
   [COMPATIBILITY.md](COMPATIBILITY.md)) on AMD64 and ARM64.
2. **Boot on UEFI + ACPI only**, with our own Rust UEFI loader.
3. **Modern hardware baseline**: PCIe (ECAM), MSI/MSI-X, NVMe, AHCI, virtio (modern), xHCI,
   x2APIC/IOAPIC, GICv3/ITS, generic timers, UEFI GOP framebuffer.
4. **File systems**: UFS2 (read/write, then soft updates + journaling), FAT32 (ESP and removable
   media), tmpfs, devfs.
5. **Networking** good enough to run `sshd`, `dhclient`, and `pkg` over IPv4 and IPv6.
6. **SMP and preemptive from the start**, with a scheduler designed for heterogeneous cores (P/E
   cores, big.LITTLE).
7. **Zero third-party crates.** Everything except the Rust toolchain's `core` and `alloc` lives in
   this repository.
8. **Idiomatic modern Rust**: algebraic data types, typestate, newtypes, `Result`-based errors, and
   `unsafe` confined to a small number of audited modules.
9. **RV64 as the Day-2 port**, proving that the architecture seams were drawn correctly.
10. **At least as fast as FreeBSD**: never more than 2% slower than the FreeBSD component being
    replaced, measured like for like, and faster where we can be.

## Non-goals (Day 1)

| Dropped | Rationale |
|---|---|
| i386, armv6/v7, powerpc, 32-bit compat (`COMPAT_FREEBSD32`) | 64-bit only |
| Legacy BIOS / CSM boot, MBR-only disks | UEFI + GPT only |
| Flattened Device Tree (FDT) platforms | ACPI only. FDT support may come later for RV64 boards that lack ACPI |
| ISA, PC-card, legacy PIC (8259), PIT as primary timer, i8042 | Not present on modern machines |
| `COMPAT_FREEBSD4..14` syscalls | Only the syscalls current libc actually uses |
| Linuxulator | May come later as a separate ABI personality |
| ZFS | Too large for Day 1. UFS2 is the native FS. Later, host upstream OpenZFS through a porting layer rather than rewrite it (ROADMAP M-Z) |
| LinuxKPI (FreeBSD's Linux driver compatibility layer) | Leaning against it (ROADMAP open decision 7). Drivers such as GPU and Wi-Fi are rebuilt in Rust for our kernel's semantics, from published specifications and clean-room reverse engineering where required, treating vendor firmware blobs as part of the hardware |
| Loadable C kernel modules (KLD binary compat) | Kernel-internal ABI is Rust and not stable. See [ADR-0005](adr/0005-monolithic-static-kernel.md) |
| Xen, Hyper-V enlightenments, bhyve as a *host* | Can come later |
| Netgraph, IPsec, SCTP, InfiniBand/OFED, NFS | Can come later, each on its own merits |

## Definition of "modern 64-bit subsystems"

We implement the parts of `freebsd-src/sys` that a 64-bit UEFI/ACPI system needs, behind
FreeBSD-compatible userland interfaces:

- `kern/`: processes, threads, signals, scheduling, sysctl, kqueue, pipes, sockets, ttys,
  credentials, jails, capsicum, time, umtx, exec
- `vm/`: physical memory, virtual address spaces, objects, paging, UMA-equivalent slab allocator
- `amd64/`, `arm64/`, `x86/`, `riscv/` (later): only the 64-bit, UEFI/ACPI paths
- `dev/acpica` (our own ACPI tables and AML interpreter), `dev/pci`, `dev/nvme`, `dev/ahci`,
  `dev/virtio`, `dev/usb` (xHCI only), `dev/uart`, `dev/vt` (framebuffer only), selected NICs
- `fs/`: devfs, tmpfs, msdosfs; `ufs/`: ffs/ufs (UFS2 only)
- `geom/`: a much smaller storage stack (disks, GPT partitions)
- `net/`, `netinet/`, `netinet6/`, `netlink/`: core TCP/IP, routing, interfaces

## Long-term horizon (after Day 1)

1. **RV64 port** (Day 2).
2. **Userland milestone**: gradually replace base components with new Rust implementations, while
   keeping FreeBSD binary compatibility as the contract.
3. **Graphics milestone**: KMS/DRM-equivalent kernel interface, virtio-gpu, then native GPU drivers.
4. **Desktop milestone**: a new Wayland compositor with dynamic tiling, animations, and IPC-driven
   configuration, in the spirit of Hyprland but with original code and design, plus a toolkit,
   shell, and core apps.

## Success criteria for Day 1 (v1.0)

- Boots on QEMU (q35 + OVMF, virt + AAVMF), bhyve (UEFI), and at least one physical AMD64 and one
  physical ARM64 SystemReady machine.
- Stock FreeBSD 15.x `base.txz` boots multi-user from a UFS2 root on NVMe, with the ESP mounted as
  msdosfs.
- `sshd` reachable over the network. `pkg install` works.
- AbyssBSD can **build itself** on AbyssBSD (using FreeBSD's `rust` package).
- At least 90% of FreeBSD's kyua test suite passes for supported subsystems, and every failure is
  triaged.
- No known memory-safety bugs. All `unsafe` blocks are documented and reviewed.
- Every benchmark in the cumulative suite ([PERFORMANCE.md](PERFORMANCE.md)) is **no more than 2%
  slower** than stock FreeBSD 15.1 on the same machine, on bhyve and on both physical reference
  machines. The aim is to be faster ([ADR-0011](adr/0011-performance-parity.md)).
