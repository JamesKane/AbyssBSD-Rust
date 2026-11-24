# bhyve Hypervisor (`vmm`)

- Status: **Draft**. A forward design for hosting hardware-accelerated virtual machines on AbyssBSD
  with full FreeBSD `bhyve(8)` / `libvmmapi` ABI compatibility. Decisions here become an ADR when
  host virtualization enters the roadmap.
- Date: 2026-11-23
- Scope: the in-kernel `vmm` hypervisor subsystem on `x86_64` (Intel VT-x / VMX and AMD-V / SVM)
  and `aarch64` (Armv8.1+ EL2 VHE, Stage-2 translation, vGICv3, vTimer), plus the `/dev/vmmctl` and
  `/dev/vmm/<name>` ioctl ABI, guest memory management, in-kernel interrupt/timer emulation, and
  IOMMU-backed PCI passthrough (`ppt`).
- Inputs: [COMPATIBILITY.md](COMPATIBILITY.md), [HARDWARE.md](HARDWARE.md), [IOMMU.md](IOMMU.md),
  [ARCHITECTURE.md](ARCHITECTURE.md), [ADR-0004](adr/0004-abi-target.md),
  [ADR-0005](adr/0005-monolithic-static-kernel.md),
  [ADR-0007](adr/0007-license-and-clean-implementation.md),
  [ADR-0011](adr/0011-performance-parity.md), and FreeBSD's `vmm(4)` / `bhyve(8)` as a behavioral
  reference (`../freebsd-src/sys/dev/vmm/`, `sys/{amd64,arm64}/vmm/`, `lib/libvmmapi/`,
  `usr.sbin/bhyve/`).

The short version: **a clean-room Rust `vmm` kernel hypervisor exposing FreeBSD 15.1's exact
`/dev/vmmctl` and `/dev/vmm/<name>` ioctl/mmap ABI so unmodified FreeBSD `bhyve(8)`, `bhyvectl(8)`,
and `libvmmapi` run directly on AbyssBSD.** Machine-independent VM lifecycle, vCPU state
management, and guest memory segments live in a host-testable `sys/vmm` crate; tiny, audited
world-switch assembly and hardware-virtualization backends live in `arch/x86_64` (VMX + SVM) and
`arch/aarch64` (EL2 VHE). Existing AbyssBSD subsystems—`sys/vm`, `lib/pt`, `sys/iommu`, `sys/pci`,
`sys/net` (`tap`/`bridge`), `sys/event` (`kqueue`), and `kernel/src/capsicum.rs`—provide the rest.

---

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal | Why |
|---|---|---|
| G1 | **Unmodified FreeBSD 15.1 `bhyve(8)` and `bhyvectl(8)` run as userspace VMMs** | AbyssBSD ships FreeBSD 15.1 userland ([ADR-0004](adr/0004-abi-target.md)); implementing the kernel `vmm` ABI unlocks the entire `bhyve` ecosystem (`vm-bhyve`, `bhyve` UEFI, virtio-blk/net/console/input, NVMe, AHCI, fbuf/RFB, GDB stub) without rewriting userspace device models |
| G2 | **Both Tier 1 ISAs and all reference CPUs**: `x86_64` Intel VT-x (Alder Lake i7-12700KF) + AMD-V SVM (Ryzen 7900X / 9700X), and `aarch64` EL2 VHE (Orange Pi 6 Plus CIX Sky1, QEMU `virt` EL2) | Matches AbyssBSD's hardware matrix ([HARDWARE.md §2](HARDWARE.md#2-reference-platforms)) |
| G3 | **Zero overhead when no VMs are running**: `vmm` leaves host syscall, page-fault, and interrupt hot paths untouched until a VM is created; VM-exit latency and guest I/O throughput within 2% of FreeBSD 15.1 `vmm` | [ADR-0011](adr/0011-performance-parity.md), [ADR-0014](adr/0014-performance-parity-per-component.md) |
| G4 | **Clean-room, BSD-licensed Rust with minimal `unsafe`**: pure ABI definitions and instruction decoders host-tested without `unsafe`; `unsafe` restricted to world-switch asm, VMCS/VMCB/EL2 register access, and physical memory pinning | [ADR-0007](adr/0007-license-and-clean-implementation.md) |
| G5 | **Reuse native kernel subsystems**: guest memory backed by `sys/vm::VmObject`, second-stage page tables built in `lib/pt`, passthrough DMA mapped through `sys/iommu`, virtual networking over `sys/net` (`tap`/`if_bridge`), Capsicum + jail `allow.vmm` isolation | Avoids duplicating VM, page-table, or IOMMU engines |
| G6 | **Compile-time selectable**: gated behind a kernel Cargo feature (`vmm`) in the monolithic static kernel and reported as built-in to `kldfind`/`modfind` | [ADR-0005](adr/0005-monolithic-static-kernel.md) |

### 1.2 Non-goals

- **Legacy x86 CPUs without EPT/NPT or Unrestricted Guest.** AbyssBSD's x86-64-v2+ consumer-decade floor ([HARDWARE.md §1](HARDWARE.md#1-platform-requirements)) guarantees EPT + Unrestricted Guest + VPID on Intel and NPT + ASID on AMD. Shadow paging and real-mode-without-UG emulation are out of scope.
- **Non-VHE Armv8.0 split-world EL2 (`nvhe`).** AbyssBSD's `aarch64` floor is Armv8.1-A with VHE ([HARDWARE.md §1](HARDWARE.md#1-platform-requirements)). Running the host kernel at EL2 under VHE (`HCR_EL2.E2H | TGE`) eliminates dual-address-space EL2 trampolines and `HVC` stub round-trips. (Systems where firmware hands over at EL1, such as Qualcomm SC8280XP, simply report `vmm` unavailable.)
- **Nested virtualization** (running a guest hypervisor inside a guest) in the initial rollout.
- **VM live migration and checkpoint/snapshot (`BHYVE_SNAPSHOT`)** in v1; `VM_SNAPSHOT_REQ` returns `EOPNOTSUPP` until core VM execution and passthrough are mature.
- **Rewriting `bhyve(8)` userspace in Rust.** FreeBSD's `bhyve(8)` runs unmodified against AbyssBSD's `/dev/vmm*` ABI. A Rust VMM binary can always be built later on the same ABI if desired.
- **Legacy `COMPAT_FREEBSD12` / `COMPAT_FREEBSD14` `vmm` ioctls.** Target FreeBSD 15.1's current `vmm` ABI directly (`VMMCTL_VM_CREATE` via `/dev/vmmctl` rather than the deprecated `hw.vmm.create` sysctl).

---

## 2. Principles

1. **Binary-compatible kernel boundary, idiomatic Rust internals.** The `/dev/vmmctl`, `/dev/vmm/<name>`, and `/dev/vmm.io/<name>.<seg>` ioctl structs and command numbers match FreeBSD 15.1 byte-for-byte (`lib/vmm-abi`, verified by `cargo xtask abi-check`). Inside the kernel, `Vm`, `Vcpu`, `VmMem`, and backend traits use algebraic Rust types, `Result<_, Errno>`, and explicit ownership rather than raw `void *` cookies.
2. **All inline assembly stays in `arch/*`.** `VMXON`/`VMLAUNCH`/`VMRESUME`, `VMRUN`, `ERET` world-switch loops, and VMCS/VMCB/EL2 system-register access live strictly in `arch/x86_64/src/vmm/` and `arch/aarch64/src/vmm/`. `sys/vmm` is `#![no_std]` and host-testable.
3. **Host-side safety invariant: a malicious guest can never corrupt the host.** Every guest-controlled exit payload (GPA, register index, MMIO width, instruction bytes, hypercall arguments) is bounds-checked and decoded without panicking. Guest memory accesses from the kernel go through checked `VmMem` lookups with pinned pages.
4. **In-kernel only what belongs in the kernel.** Following FreeBSD's split, high-frequency CPU-coupled devices live in the kernel (`vlapic`, `vioapic`, `vatpic`, `vatpit`, `vhpet`, `vpmtmr`, `vrtc` on `x86_64`; `vgic_v3`, `vtimer`, PSCI on `aarch64`), while complex I/O devices (PCIe fabric, VirtIO, NVMe, AHCI, xHCI, framebuffer, UART, ACPI/SMBIOS generation) stay in userspace `bhyve(8)` inside Capsicum capability mode.
5. **Both ISAs share the MI core.** `sys/vmm` owns VM registry, vCPU state transitions, memory segments/mappings, rendezvous, statistics, and jail/Capsicum checks; `x86_64` and `aarch64` implement a clean `VmmArch` trait and their own machdep ioctl tables.

---

## 3. Overview & Crates

```
 userland         Unmodified FreeBSD 15.1 bhyve(8) · bhyvectl(8) · libvmmapi.so (Capsicum)
──────────────────────────┬──────────────────────────────────────────┬──────────────────────────
 boundary                 │ /dev/vmmctl · /dev/vmm/<name> · mmap     │ tap(4) · nmdm(4) · kqueue
──────────────────────────┴──────────────────────────────────────────┴──────────────────────────
 kernel/src/vmm/          devfs nodes · ioctl dispatch & lock flags · jail/Capsicum checks
                          VM_RUN loop · host FPU/preemption · hw.vmm.* sysctls · ppt driver
──────────────────────────┬──────────────────────────────────────────┬──────────────────────────
 sys/vmm (MI core)        │ Vm / Vcpu state machine · VmMem segments │ x86: vlapic/vioapic/vhpet
                          │ rendezvous · stats · instruction decoder │ arm64: vgic_v3/vtimer/psci
──────────────────────────┴─────────────────────────┬────────────────┴──────────────────────────
 arch/{x86_64,aarch64}    x86_64: VMX (VT-x) + SVM (AMD-V) world switch, VMCS/VMCB, APICv/AVIC
                          aarch64: EL2 VHE boot & world switch, VTTBR_EL2/VMID, ICH_LR<n>_EL2
──────────────────────────┬─────────────────────────┴───────────────────────────────────────────
 shared crates            │ lib/pt (EPT, NPT, Arm Stage-2) · sys/vm (VmObject, phys pinning)
                          │ sys/iommu (Stage-2 / IOVA domains) · sys/pci (BARs, MSI/MSI-X, FLR)
──────────────────────────┴─────────────────────────────────────────────────────────────────────
 lib/vmm-abi              FreeBSD 15.1 #[repr(C)] ioctl structs, VM_EXITCODE_*, VM_REG_*,
                          VM_CAP_*, ioctl command constants (x86_64 + aarch64, no unsafe)
```

| Crate / Module | Contents | Tested | `unsafe` |
|---|---|---|---|
| `lib/vmm-abi` | FreeBSD 15.1 `vmm` UAPI structs (`vmmctl_vm_create`, `vm_run`, `vm_exit`, `vm_memseg`, `vm_memmap`, `vm_register`, `vm_capability`, `vm_vgic_descr`, `vm_pptdev_*`, etc.) and `_IO*` command numbers for `x86_64` and `aarch64` | host (`cargo test`), `abi-check` | **forbidden** |
| `lib/pt` (extended) | Second-stage page-table walkers & formatters: Intel EPT (4-level), AMD NPT (4-level), Arm Stage-2 (`VTCR_EL2` / `VTTBR_EL2`, 4-level, 4 KiB / 2 MiB / 1 GiB) | host (`cargo test`) | **forbidden** |
| `sys/vmm` | MI `Vm`, `Vcpu` state machine, `VmMem` segment/map table, rendezvous, `vmm_stat` counters, x86/arm64 instruction-emulation decoders, in-kernel virtual devices (`vlapic`, `vioapic`, `vatpic`, `vatpit`, `vhpet`, `vpmtmr`, `vrtc`; `vgic_v3`, `vtimer`, PSCI) | host (`cargo test`), `#[ktest]` | **forbidden** |
| `arch/x86_64/src/vmm/` | Intel VMX (`vmx.rs`, `vmcs.rs`, `ept.rs`, `vmx_asm.rs`) and AMD SVM (`svm.rs`, `vmcb.rs`, `npt.rs`, `svm_asm.rs`) backends implementing `VmmArch` | QEMU, bare metal (Intel + AMD) | yes (arch boundary) |
| `arch/aarch64/src/vmm/` | EL2 VHE boot transition (`entry.rs`), `HypCtx` world switch (`hyp.rs`, `hyp_asm.rs`), Stage-2 TLB/VMID management, GICv3 `ICH_*_EL2` register accessors | QEMU `virt` EL2, Orange Pi 6 Plus | yes (arch boundary) |
| `kernel/src/vmm/` | `/dev/vmmctl`, `/dev/vmm/<name>`, `/dev/vmm.io/<name>.<seg>` cdevs, ioctl precondition wrapper, `sys/vm` memory backing, `ppt` PCI passthrough driver + `sys/iommu` hookup, `hw.vmm.*` sysctls, `allow.vmm` jail flags | `#[ktest]`, QEMU, bare metal | minimal (uaccess/DMA) |

---

## 4. Device Model, Lifecycle & FreeBSD `vmm` ABI

### 4.1 `/dev/vmmctl` and `/dev/vmm/<name>`

FreeBSD 15+ replaced the legacy `hw.vmm.create` / `hw.vmm.destroy` sysctl mechanism with a
control device `/dev/vmmctl` so VM creation integrates cleanly with jails, permissions, and
automatic cleanup on crash:

- **`/dev/vmmctl`**:
  - `VMMCTL_VM_CREATE`: validates `name` (`VM_MAX_NAMELEN`), enforces `hw.vmm.maxvmms` per-user
    quota and jail `allow.vmm` permission, allocates a `Vm`, and creates `/dev/vmm/<name>` in
    `devfs` tagged with the creator's `ucred` (so `cr_cansee` hides VMs across jails/users).
  - `VMMCTL_CREATE_DESTROY_ON_CLOSE`: records the VM on the `/dev/vmmctl` file-descriptor's
    private list so closing the fd (or `bhyve(8)` crashing) automatically tears down the VM and
    releases all guest RAM and passthrough devices.
  - `VMMCTL_VM_DESTROY`: tears down `/dev/vmm/<name>`, any associated devmem cdevs, and frees the
    `Vm` once all open descriptors drain.
- **`/dev/vmm/<name>`**:
  - `ioctl(2)`: dispatches generic MI ioctls (`VM_RUN`, `VM_ALLOC_MEMSEG`, `VM_MMAP_MEMSEG`,
    `VM_MUNMAP_MEMSEG`, `VM_GET_MEMSEG`, `VM_MMAP_GETNEXT`, `VM_SET_REGISTER`, `VM_GET_REGISTER`,
    `VM_SET_REGISTER_SET`, `VM_GET_REGISTER_SET`, `VM_SET_CAPABILITY`, `VM_GET_CAPABILITY`,
    `VM_SUSPEND`, `VM_REINIT`, `VM_ACTIVATE_CPU`, `VM_GET_CPUS`, `VM_SET_TOPOLOGY`,
    `VM_GET_TOPOLOGY`, `VM_STATS`, `VM_STAT_DESC`, `VM_GLA2GPA_NOFAULT`) and ISA-specific ioctls
    (x86: `VM_LAPIC_*`, `VM_IOAPIC_*`, `VM_ISA_*`, `VM_PPTDEV_*`, `VM_RTC_*`, `VM_HPET_*`,
    `VM_SET_DESC`/`VM_GET_DESC`, `VM_SET_X2APIC_STATE`, `VM_INJECT_EXCEPTION`/`VM_INJECT_NMI`;
    arm64: `VM_ATTACH_VGIC`, `VM_ASSERT_IRQ`, `VM_DEASSERT_IRQ`, `VM_RAISE_MSI`,
    `VM_INJECT_EXCEPTION`).
  - `read(2)` / `write(2)`: allows `dd(1)` / debugging tools to read/write guest physical memory
    (returning zeroes for unmapped holes below `vmm_sysmem_maxaddr`).
  - `mmap(2)`: maps `VM_SYSMEM` into `bhyve(8)`'s host virtual address space so userspace device
    models (virtio rings, DMA descriptors, bootrom loading) access guest RAM directly at native
    memory speed with zero copies.
- **`/dev/vmm.io/<name>.<segname>`**:
  - Created when `VM_ALLOC_MEMSEG` specifies a non-empty `name` (used by `bhyve(8)` for
    `VM_BOOTROM`, `VM_FRAMEBUFFER`, and `VM_PCIROM`), allowing `bhyve(8)` to `mmap` device memory
    segments separately from guest RAM.

### 4.2 Ioctl Precondition Enforcement

Just like FreeBSD's `struct vmmdev_ioctl` flags in `sys/dev/vmm/vmm_dev.h`, AbyssBSD's ioctl
dispatch table declares locking and vCPU preconditions declaratively so individual handlers cannot
forget to freeze vCPUs or lock memory segments:

```rust
bitflags! {
    pub struct IoctlPrecond: u8 {
        const SLOCK_MEMSEGS    = 0x01; // shared lock on VmMem
        const XLOCK_MEMSEGS    = 0x02; // exclusive lock on VmMem
        const LOCK_ONE_VCPU    = 0x04; // allocate if needed, freeze target vCPU
        const LOCK_ALL_VCPUS   = 0x08; // serialize & freeze every vCPU in VM
        const ALLOC_VCPU       = 0x10; // allocate vCPU on first reference
        const MAYBE_ALLOC_VCPU = 0x20; // allocate unless vcpuid == -1
        const PPT              = 0x40; // require PRIV_DRIVER / allow.vmm_ppt
    }
}
```

RAII guard types (`VcpuFreezeGuard`, `AllVcpusFreezeGuard`, `MemSegReadGuard`, `MemSegWriteGuard`)
guarantee cleanup on every error return.

### 4.3 Jails, Capsicum & Sysctls

- **Jails (`kernel/src/prison.rs`)**: add `allow.vmm` and `allow.vmm_ppt` prison flags. Jailed
  processes without `allow.vmm` receive `EPERM` on `/dev/vmmctl`; passthrough ioctls require
  `allow.vmm_ppt`.
- **Capsicum (`kernel/src/capsicum.rs`)**: `bhyve(8)` enters capability mode (`cap_enter(2)`) after
  device initialization and limits its `/dev/vmm/<name>` descriptor with `cap_ioctls_limit(2)` to
  the runtime ioctl subset. AbyssBSD's existing Capsicum ioctl-command allowlist supports this
  directly.
- **Sysctls**: `hw.vmm.maxcpu`, `hw.vmm.maxvmms`, `hw.vmm.ipinum`, plus ISA-specific tunables
  (`hw.vmm.vmx.*`, `hw.vmm.svm.*`, `hw.vmm.ppt.*`).

---

## 5. vCPU State Machine, Scheduling & Rendezvous

### 5.1 vCPU State Machine

Each vCPU is driven by a dedicated host `pthread` in `bhyve(8)` calling `ioctl(fd, VM_RUN, ...)`.
In `sys/vmm`, each `Vcpu` owns a spin-protected state machine matching FreeBSD's semantics:

```
         ┌────────────────────────────────────────────┐
         │                                            ▼
    ┌─────────┐  ioctl / lock   ┌──────────┐   enter guest   ┌───────────┐
    │  Idle   │ ──────────────► │  Frozen  │ ──────────────► │  Running  │
    │         │ ◄────────────── │          │ ◄────────────── │ (hostcpu) │
    └─────────┘  ioctl return   └──────────┘    VM exit      └───────────┘
                                  │      ▲
                     HLT / WFI    │      │  IRQ / event / reqidle
                                  ▼      │
                                ┌──────────┐
                                │ Sleeping │
                                └──────────┘
```

- **State transitions**: only `Idle ↔ Frozen`, `Frozen ↔ Running`, and `Frozen ↔ Sleeping` are
  valid; invalid transitions return `Errno::EBUSY`.
- **External preemption (`reqidle`)**: when another thread needs to freeze a vCPU (e.g.
  `LOCK_ALL_VCPUS` for `VM_MMAP_MEMSEG`, `VM_SUSPEND`, or `bhyvectl` inspection), it sets
  `reqidle = true` and kicks the vCPU:
  - If `Running` on `hostcpu`, sends the dedicated `vmm_ipinum` IPI to `hostcpu`, forcing an
    immediate VM exit (external-interrupt exit on x86, physical IRQ exit on arm64).
  - If `Sleeping` (blocked in `HLT` / `WFI`), wakes its wait channel immediately.
- **Host scheduler cooperation**: before re-entering guest mode inside `VM_RUN`, the loop checks
  `vcpu_should_yield()` (pending AST, host preemption requested, or pending signal on the
  `bhyve(8)` vCPU thread). On signal delivery or `VM_SUSPEND`, `VM_RUN` returns cleanly to
  userspace.

### 5.2 SMP Rendezvous

Certain guest operations (e.g. `x86_64` TSC/MTRR updates or memory-map changes) require all active
vCPUs to pause and run a short non-blocking callback (`vm_smp_rendezvous`). In FreeBSD,
`vcpu_set_state_all()` carries an explicit comment noting deadlock subtlety between freezing all
vCPUs and an in-flight rendezvous. In AbyssBSD's `sys/vmm`:

- A single VM-wide `RendezvousState` mutex establishes a strict lock order:
  `vcpus_init_lock` → `rendezvous_mtx` → `vcpu.mtx` (in ascending `vcpuid` order).
- `lock_all_vcpus` checks for an active rendezvous before blocking, executing the rendezvous
  callback on behalf of any `Idle` vCPUs, and kicks `Running`/`Sleeping` vCPUs with `vmm_ipinum`
  so no circular wait is possible.

### 5.3 Host FPU / SIMD / SVE State

- On guest entry, the host thread's FPU/AVX/SVE state is saved (if dirty) and the vCPU's
  `GuestFpuState` (plus `XCR0` on `x86_64` or `FPCR`/`FPSR`/NEON/SVE regs on `aarch64`) is loaded
  with host interrupts disabled around the switch.
- On VM exit back to kernel/userspace, guest FPU state is saved and host FPU state is restored
  lazily via the existing `arch/{x86_64,aarch64}/src/fpu.rs` hooks.

---

## 6. Guest Memory & Second-Stage Translation

### 6.1 `VmMem` Segments and Mappings

`sys/vmm` mirrors FreeBSD's two-level memory model (`sys/dev/vmm/vmm_mem.h`):

1. **Memory segments (`mem_segs[VM_MAX_MEMSEGS]`)**:
   - `VM_SYSMEM` (`0..VM_MAXSYSMEM`, up to 8 NUMA domains), `VM_BOOTROM`, `VM_FRAMEBUFFER`,
     `VM_PCIROM`.
   - Each segment is backed by a native `sys/vm::VmObject` (anonymous zero-filled pages, optionally
     wired when `VM_MEMMAP_F_WIRED` or PCI passthrough is active).
   - Segments can be mapped simultaneously into:
     - the guest physical address space (via `VM_MMAP_MEMSEG`), and
     - `bhyve(8)`'s host virtual address space (via `mmap(2)` on `/dev/vmm/<name>` or
       `/dev/vmm.io/<name>.<segname>`), sharing the exact same underlying `VmObject` pages.
2. **Guest physical mappings (`mem_maps[VM_MAX_MEMMAPS]`)**:
   - Maps a `(gpa, len)` range to `(segid, segoff, prot, flags)`.
   - On `VM_REINIT` (guest reboot), device mappings (`!sysmem`) are cleared while `VM_SYSMEM`
     mappings persist across reset, matching FreeBSD's `vm_mem_cleanup()`.

### 6.2 Second-Stage Page Tables (`lib/pt`)

Rather than building a separate page-table engine, `lib/pt` (which already provides host-tested,
`unsafe`-free `x86_64`, `aarch64`, and `smmu` formatters) is extended with three second-stage
table formats:

| Format | Used by | Key differences from host Stage-1 |
|---|---|---|
| **Intel EPT** (`lib/pt/src/ept.rs`) | `arch/x86_64` VMX | R/W/X in bits 0–2, memory type (WB/UC/WC) in bits 3–5, `EPTP` with 4-level walk & accessed/dirty enable, `INVEPT` invalidation |
| **AMD NPT** (`lib/pt/src/npt.rs`) | `arch/x86_64` SVM | Standard x86-64 PTE layout with `USER` bit always set for guest access, `PAT` combined with guest PAT, `INVLPGA` / ASID flush |
| **Arm Stage-2** (`lib/pt/src/stage2.rs`) | `arch/aarch64` VHE | `S2AP[1:0]` permissions, `MemAttr[3:0]` (Normal WB / Device-nGnRE), `XN[1:0]`, `VTTBR_EL2` + `VTCR_EL2`, `TLBI IPAS2E1IS` + `TLBI VMALLE1IS` |

- **Demand faulting & superpages**: guest second-stage faults (EPT violation, Nested Page Fault,
  Stage-2 Data/Instruction Abort) look up `(gpa)` in `VmMem`. If the GPA falls in a mapped segment,
  the kernel faults/pins the backing page(s) from the `VmObject` (promoting to 2 MiB / 1 GiB
  superpages when aligned and contiguous) and installs the second-stage PTE without exiting to
  userspace. If the GPA falls outside all mapped segments, the exit is routed to in-kernel MMIO
  handlers (`vlapic`/`vioapic`/`vhpet`/`vgic_v3`) or returned to `bhyve(8)` as `VM_EXITCODE_INST_EMUL`.
- **Kernel access to guest memory**: `vm_gpa_hold` / `vm_gpa_release` pins the backing host page
  for instruction emulation and `read`/`write` on `/dev/vmm/<name>`. `vm_gla2gpa_nofault` walks
  guest Stage-1 page tables (x86 4-level / PAE / 32-bit or Arm EL1 `TTBR0`/`TTBR1`) without
  mutating guest state.

---

## 7. `x86_64` Hypervisor Backend (Intel VT-x & AMD-V)

### 7.1 Backend Selection

At kernel init, `arch/x86_64/src/vmm/mod.rs` probes `CPUID` and selects one of:

- `VmxBackend` (Intel `CPUID.1:ECX.VMX[5]`, verifying `IA32_FEATURE_CONTROL` and required EPT /
  Unrestricted Guest / VPID controls), or
- `SvmBackend` (AMD `CPUID.80000001h:ECX.SVM[2]`, verifying `VM_CR.SVMDIS == 0` and NPT support).

Both implement the `VmmArch` trait (replacing FreeBSD's `vmm_ops_intel` / `vmm_ops_amd` ifuncs).

### 7.2 Intel VT-x (`arch/x86_64/src/vmm/vmx/`)

- **Per-host-CPU initialization**: allocate a 4 KiB aligned `VMXON` region per physical CPU, set
  `CR4.VMXE`, and execute `VMXON` lazily on first VM creation (and `VMXOFF` when the last VM is
  destroyed, keeping idle hosts clean and supporting host suspend/resume).
- **Per-vCPU state**: a 4 KiB `VMCS` page plus saved guest GPRs, `CR2`, `DR0`–`DR3`/`DR6`,
  `XCR0`, and `MSR` save/load areas. Track `last_hostcpu` per vCPU so migrating a vCPU across host
  cores runs `VMCLEAR` on the old core (via IPI) and `VMPTRLD` on the new core.
- **Hardware acceleration**:
  - **VPID**: 16-bit per-vCPU VPID allocator (avoiding host/guest TLB flushes on every VM exit).
  - **Unrestricted Guest + EPT**: guests boot straight from 16-bit real mode (UEFI reset vector)
    to 64-bit long mode without software real-mode emulation.
  - **APICv & Posted Interrupts** (when supported, e.g. Alder Lake): virtual-APIC page, APIC-access
    page, EOI-exit bitmap, and posted-interrupt descriptor so virtual IPIs and timer/MSI interrupts
    deliver directly into running guests without a VM exit.
  - **MSR & I/O bitmaps**: pass through safe MSRs (`FS_BASE`, `GS_BASE`, `KERNEL_GS_BASE`, `SYSCALL`
    MSRs, `TSC` with `TSC_OFFSET`); trap sensitive MSRs and PIO ports.
- **World switch (`vmx_asm.rs`)**: saves callee-saved host registers, loads guest GPRs, executes
  `VMLAUNCH` (first entry after `VMCLEAR`) or `VMRESUME`, and on exit saves guest GPRs and restores
  host state with `HOST_RIP` pointing at the exit stub.

### 7.3 AMD-V / SVM (`arch/x86_64/src/vmm/svm/`)

- **Per-host-CPU initialization**: set `EFER.SVME` and program `VM_HSAVE_PA` to a per-CPU 4 KiB
  host-save page (matching AMD Zen 4/5 on the X870E and B650E reference machines).
- **Per-vCPU state**: a 4 KiB `VMCB` (control area + state-save area) + guest GPRs + `MSRPM`
  (8 KiB) + `IOPM` (12 KiB).
- **Hardware acceleration**:
  - **ASID**: per-host-CPU ASID generation counter (`asid_gen`) matching FreeBSD's `svm.c` so TLB
    flushes only happen on ASID wraparound (`TLB_CONTROL_FLUSH_ALL_ASIDS`).
  - **NPT**: `N_CR3` points to the VM's `lib/pt` NPT root.
  - **AVIC / x2AVIC** (Zen 4 / Zen 5): virtual APIC backing page, physical/logical APIC ID tables,
    and doorbell register for exitless interrupt delivery.
  - **TSC Ratio & Offset**: `TSC_OFFSET` in VMCB control area; `VMLOAD`/`VMSAVE` for `FS`/`GS`/
    `TR`/`LDTR`/`STAR`/`LSTAR`/`CSTAR`/`SFMASK`/`SYSENTER_*`.
- **World switch (`svm_asm.rs`)**: `CLGI`, load guest GPRs, `VMLOAD`, `VMRUN`, `VMSAVE`, save
  guest GPRs, `STGI`.

### 7.4 In-Kernel x86 Devices & Instruction Emulation

All x86 virtual platform devices live in `sys/vmm/src/x86/` (pure Rust, host-testable):

| Device | Role |
|---|---|
| **`vlapic`** | Per-vCPU Local APIC (xAPIC MMIO at `0xFEE0_0000` and x2APIC MSRs `0x800..=0x83F`), IRR/ISR/TMR, TPR/PPR, LVT, ICR (INIT/SIPI/IPI), one-shot/periodic/TSC-deadline timer backed by host callouts |
| **`vioapic`** | 24/32-pin I/O APIC at `0xFEC0_0000` routing level/edge GSIs and `VM_IOAPIC_*` ioctls to `vlapic` |
| **`vatpic`** | Dual 8259A PIC (`0x20`/`0xA0`) for early boot and legacy ISA IRQs |
| **`vatpit`** | 8254 PIT (`0x40..=0x43`) timer channels |
| **`vhpet`** | High Precision Event Timer (`0xFED0_0000`) |
| **`vpmtmr`** | 24-bit ACPI PM timer (3.579545 MHz) |
| **`vrtc`** | MC146818 CMOS RTC (`0x70`/`0x71`) + `VM_RTC_*` ioctls |
| **`emul`** | Clean-room x86-64 MMIO/PIO instruction decoder (`MOV`, `MOVZX`, `MOVSX`, `MOVS`, `STOS`, `CMP`, `TEST`, `AND`/`OR`/`ADD`/`SUB`/`BT`, `XCHG`, `CMPXCHG`, `IN`/`OUT`/`INS`/`OUTS`) feeding both in-kernel MMIO devices and userspace `VM_EXITCODE_INST_EMUL` / `VM_EXITCODE_INOUT` |

`CPUID` exits sanitize host features (hiding `VMX`/`SVM`, advertising hypervisor leaf `0x4000_0000`
`"bhyve bhyve "` and guest topology from `VM_SET_TOPOLOGY`, clamping `XCR0` and physical address
bits).

---

## 8. `aarch64` Hypervisor Backend (EL2 VHE, vGICv3 & vTimer)

### 8.1 Running AbyssBSD at EL2 with VHE

Currently, [`arch/aarch64/src/entry.rs`](../arch/aarch64/src/entry.rs) detects `CurrentEL == EL2`,
installs a minimal `abyss_el2_vectors` stub, drops to EL1, and records `booted_at_el2()`. Because
AbyssBSD's `aarch64` floor is **Armv8.1-A with VHE** ([HARDWARE.md §1](HARDWARE.md#1-platform-requirements)):

- When `CurrentEL == EL2` and `ID_AA64MMFR1_EL1.VH != 0` (true on the Orange Pi 6 Plus CIX Sky1
  and QEMU `virt` with `virtualization=on`), `_start` and `abyss_ap_start` enable **VHE**
  (`HCR_EL2.E2H = 1`, `HCR_EL2.TGE = 1`), keeping the host kernel running directly at EL2!
- Under VHE, the host kernel's `TTBR0_EL1`/`TTBR1_EL1`/`TCR_EL1`/`SCTLR_EL1`/`VBAR_EL1` accesses
  transparently alias to `*_EL2`, so `arch/aarch64` VM, trap, and syscall code works unchanged with
  zero performance penalty—and `vmm` can enter guests directly via `ERET` without `HVC` trampolines
  or a separate EL2 page table.
- When firmware hands over at EL1 (e.g. Qualcomm SC8280XP under its vendor hypervisor, or QEMU
  without `virtualization=on`), `_start` continues booting at EL1 as today and `vmm` reports
  `ENXIO` / unavailable cleanly.

### 8.2 World Switch & Stage-2 Context (`arch/aarch64/src/vmm/`)

- **Per-VM state (`ArmVm`)**: Stage-2 page-table root (`lib/pt::stage2`), allocated 8-bit or 16-bit
  **VMID** (with generation-based rollover), `VTCR_EL2`, and registered in-kernel MMIO / special
  register trap tables (matching `mmio_region[]` and `special_reg[]` in FreeBSD's `sys/arm64/vmm`).
- **Per-vCPU state (`HypCtx`)**:
  - Guest GPRs `x0`–`x30`, `sp_el0`, `elr_el2` (guest PC), `spsr_el2` (guest PSTATE).
  - Guest EL1 system registers accessed via the VHE `*_EL12` / `*_EL02` aliases (`SCTLR_EL12`,
    `TTBR0_EL12`, `TTBR1_EL12`, `TCR_EL12`, `MAIR_EL12`, `VBAR_EL12`, `ESR_EL12`, `FAR_EL12`,
    `ELR_EL12`, `SPSR_EL12`, `SP_EL1`, `TPIDR_EL1`, `TPIDR_EL0`, `TPIDRRO_EL0`, `CNTKCTL_EL12`,
    `CONTEXTIDR_EL12`, `AFSR0/1_EL12`, `AMAIR_EL12`, `PAR_EL1`, `CPACR_EL12`).
  - Virtual ID registers (`VPIDR_EL2`, `VMPIDR_EL2` reflecting guest `vcpuid` topology).
- **World switch (`hyp_asm.rs`)**:
  1. Disable host IRQs (`DAIF`), save host callee-saved registers, switch `VBAR_EL2` to the guest
     exit vector table.
  2. Program `VTTBR_EL2` (VMID + Stage-2 root), `HCR_EL2` (clear `TGE`, set `VM | SWIO | FMO |
     IMO | AMO | RW | TWE | TWI | TSC | TACR | TIDCP | TSW`), restore guest `*_EL12` and vGICv3
     `ICH_*_EL2` + vTimer registers.
  3. Load guest `x0`–`x30`, `ELR_EL2`, `SPSR_EL2`, and execute `ERET` into guest EL1/EL0.
  4. On exit (Stage-2 abort, `WFI`/`WFE`, `HVC`/`SMC`, trapped `MRS`/`MSR`, physical IRQ/FIQ/SError):
     save guest GPRs and `ESR_EL2`/`FAR_EL2`/`HPFAR_EL2`, re-enable `HCR_EL2.TGE`, restore host
     `VBAR_EL2` and host context, and dispatch the exit in Rust.
- **Exit decoding**:
  - **Stage-2 Data Abort (`EC == 0x24`) with `ISV == 1`**: extracts access size, sign-extend, read/write
    direction, and target register (`SRT`) directly from `ESR_EL2`—no guest instruction fetch
    required—and routes to in-kernel MMIO (`vgic_v3`) or userspace `VM_EXITCODE_INST_EMUL`.
  - **Trapped `MRS`/`MSR` (`EC == 0x18`)**: dispatches to `vmm_special_reg` handlers (e.g.
    `ICC_SGI1R_EL1`, `ICC_ASGI1R_EL1`, debug/PMU traps) or userspace `VM_EXITCODE_REG_EMUL`.

### 8.3 In-Kernel vGICv3 & vTimer (`sys/vmm/src/arm64/`)

- **vGICv3 (`vgic_v3.rs`)**:
  - Attached by `bhyve(8)` via `VM_ATTACH_VGIC` with guest IPA ranges for Distributor (`GICD`) and
    Redistributors (`GICR`), registered as in-kernel MMIO regions.
  - Emulates `GICD_*` (distributor control, `TYPER`, `IIDR`, enable/pending/active/priority/config/
    router registers) and per-vCPU `GICR_*` (`RD_base` + `SGI_base`, `WAKER`, `TYPER`).
  - Supports SGIs (0–15, via trapped `ICC_SGI1R_EL1`), PPIs (16–31, e.g. virtual timer PPI 27),
    SPIs (32–1019, via `VM_ASSERT_IRQ` / `VM_DEASSERT_IRQ`), and MSIs (`VM_RAISE_MSI` mapped onto
    SPIs or virtual ITS LPIs).
  - Syncs pending virtual interrupts into hardware GICv3 List Registers (`ICH_LR0_EL2`–`ICH_LR15_EL2`)
    and `ICH_VMCR_EL2` / `ICH_AP*R_EL2` on guest entry, using `ICH_HCR_EL2` maintenance interrupts
    when more interrupts are pending than list registers.
- **Virtual Generic Timer (`vtimer.rs`)**:
  - Guests use the Arm Virtual Timer (`CNTV_*_EL0` / `CNTVCT_EL0`) directly in hardware with
    `CNTVOFF_EL2` providing per-VM monotonic offset—zero VM exits for `CNTVCT_EL0` reads or timer
    reprogramming while the vCPU is running.
  - When a vCPU traps on `WFI` and sleeps (`VCPU_SLEEPING`), `vtimer` inspects `CNTV_CTL_EL02` and
    `CNTV_CVAL_EL02` and arms a host kernel timer (`kernel/src/timers.rs`) to inject the virtual
    timer PPI and wake the vCPU when the guest deadline expires.

### 8.4 PSCI & SMCCC

`HVC` / `SMC` traps (`EC == 0x16` / `0x17`) with PSCI v0.2/v1.0 function IDs are handled in-kernel
(or forwarded as `VM_EXITCODE_HVC` where userspace coordination is needed):
- `PSCI_VERSION`, `PSCI_FEATURES`, `PSCI_AFFINITY_INFO`
- `PSCI_CPU_ON`: initializes target vCPU's `PC` and `x0` context ID and marks it active
- `PSCI_CPU_OFF`, `PSCI_SYSTEM_OFF` (`VM_SUSPEND_POWEROFF`), `PSCI_SYSTEM_RESET` (`VM_SUSPEND_RESET`)

---

## 9. Userspace VMM Integration (`bhyve(8)` & Host Subsystems)

Because AbyssBSD targets FreeBSD 15.1 userland ABI compatibility ([ADR-0004](adr/0004-abi-target.md)),
running unmodified `bhyve(8)` exercises several existing AbyssBSD subsystems alongside `vmm`:

| `bhyve(8)` component | AbyssBSD kernel dependency | Status in AbyssBSD |
|---|---|---|
| `mevent` event loop | `kqueue(2)` (`EVFILT_READ`, `EVFILT_WRITE`, `EVFILT_TIMER`, `EVFILT_SIGNAL`, `EVFILT_USER`) | Exists (`sys/event`, `kernel/src/event.rs`) |
| vCPU threads & pinning | `thr_*` / `pthread`, `cpuset_setaffinity(2)` | Exists (`kernel/src/uthread.rs`, `kernel/src/cpuset.rs`) |
| `virtio-net` / `e82545` networking | `tap(4)` + `if_bridge(4)` | Exists (`sys/net/src/{tuntap,bridge}.rs`) |
| `virtio-blk` / `nvme` / `ahci` storage | `preadv`/`pwritev`, `fsync`, `DIOCGMEDIASIZE` / raw disk & UFS files | Exists (`sys/vfs`, `kernel/src/geom_kern.rs`) |
| Serial console (`com1` / `pl011`) | PTYs and `nmdm(4)` null-modem pairs | PTYs exist; `nmdm(4)` is a small follow-up char device |
| `fbuf` (VNC/RFB) & `xhci,tablet` | Pure userspace over `VM_FRAMEBUFFER` memseg + TCP sockets | Works once `devmem` `mmap` works |
| UEFI bootrom (`BHYVE_UEFI.fd`) | `VM_BOOTROM` read-only `devmem` segment mapped at top of 4 GiB (x86) or `0x0` (arm64) | Covered by `VmMem` (§6.1) |
| Capsicum sandbox | `cap_enter(2)`, `cap_rights_limit(2)`, `cap_ioctls_limit(2)` | Exists (`kernel/src/capsicum.rs`) |

---

## 10. PCI Passthrough (`ppt`) & IOMMU Integration

[IOMMU.md §10](IOMMU.md) explicitly marks Stage-2 / `ppt` passthrough as the natural next consumer
of AbyssBSD's `sys/iommu` and `drivers/smmu` (and future Intel VT-d / AMD-Vi) work.

- **Device reservation (`ppt` driver)**:
  - A stub PCI driver (`kernel/src/vmm/ppt.rs`) claims devices listed in the `pptdevs` loader
    tunable (e.g. `pptdevs="1/0/0 2/0/0"`) during PCI probe before normal drivers attach, keeping
    the host from touching them.
  - Exposes `VM_BIND_PPTDEV`, `VM_UNBIND_PPTDEV`, `VM_MAP_PPTDEV_MMIO`, `VM_UNMAP_PPTDEV_MMIO`,
    `VM_SETUP_PPTDEV_MSI`, `VM_SETUP_PPTDEV_MSIX`, `VM_DISABLE_PPTDEV_MSI`,
    `VM_DISABLE_PPTDEV_MSIX`, and `VM_PPTDEV_RESET` (gated by `IoctlPrecond::PPT`).
- **IOMMU domain per VM**:
  - When the first passthrough device binds to a VM, `vmm` creates a dedicated `sys/iommu::Domain`
    (Stage-2 / IOVA domain) and maps every `VM_SYSMEM` segment with `VM_MEMMAP_F_IOMMU` (wiring
    backing pages so DMA targets never migrate or page out).
  - Attaching the PCI device binds its requester ID (`sys/pci/src/requester.rs`)—and any
    co-grouped devices in the same IOMMU isolation group—to the VM's domain.
  - Unbinding or VM teardown executes a PCI Function-Level Reset (FLR) via `sys/pci`, tears down
    host interrupts, and detaches the device from the IOMMU domain before releasing pinned guest
    RAM.
- **MMIO BARs & MSI-X table splitting**:
  - Device BARs are mapped directly into the guest's EPT/NPT/Stage-2 tables (`VM_MAP_PPTDEV_MMIO`)
    as UC/Device memory so guest register reads/writes run at bare-metal speed without VM exits.
  - As in FreeBSD's `sys/amd64/vmm/io/ppt.c`, if a BAR contains the MSI-X table, `bhyve(8)` splits
    the BAR around the MSI-X table page so accesses to the MSI-X table trap to userspace and
    reprogram host-to-guest interrupt routing safely via `VM_SETUP_PPTDEV_MSIX`.
- **Interrupt forwarding**:
  - Host MSI/MSI-X handlers registered for the passthrough device inject the configured virtual
    MSI directly into `vlapic` (x86) or `vgic_v3` (arm64) from interrupt context—and on Intel
    VT-d + APICv or Arm GICv4, can later upgrade to hardware posted interrupts without changing the
    `vmm` ABI.

---

## 11. Verification, Testing & Performance

| Layer | Method |
|---|---|
| **ABI layout verification** | `cargo xtask abi-check` verifies `lib/vmm-abi` `#[repr(C)]` struct sizes, field offsets, and `_IO*` command numbers against FreeBSD 15.1 `sys/{dev,amd64,arm64}/vmm` headers on both `x86_64` and `aarch64` |
| **MI core & virtual devices** | Host `cargo test` in `lib/pt` (EPT/NPT/Stage-2 walks, superpage split/merge), `lib/vmm-abi`, and `sys/vmm` (vCPU state machine, rendezvous ordering, `vlapic`/`vioapic`/`vhpet`/`vgic_v3`/`vtimer`, x86 instruction decoder fuzz/unit cases) |
| **In-kernel tests (`#[ktest]`)** | VM create/destroy, `VmMem` segment allocation & `mmap`, `VM_RUN` minimal guest payloads (real-mode/long-mode x86 & EL1 arm64 assembly blobs) executing MMIO/PIO/HVC exits |
| **Virtual CI (QEMU)** | `aarch64` QEMU `virt,virtualization=on` (EL2 VHE) boots an AbyssBSD host that launches a nested minimal guest via `bhyve(8)`; `x86_64` QEMU with nested VMX/SVM boots a guest via `bhyve(8)` |
| **Bare-metal reference hardware** | Boot FreeBSD 15.1 and Linux UEFI guests under `bhyve(8)` on all three hardware virtualization backends: **AMD SVM** (Ryzen 9 7900X X870E), **Intel VMX** (Core i7-12700KF Z690), and **Arm EL2 VHE** (Orange Pi 6 Plus CIX Sky1) |
| **Performance parity** | Compare VM-exit latency (`CPUID` / `HVC` / MMIO round-trip), guest `iperf3` over `virtio-net` + `tap`/`bridge`, and guest `fio` over `virtio-blk` against FreeBSD 15.1 `bhyve` on the same machine (**≤ 2%** regression bound, [ADR-0011](adr/0011-performance-parity.md)) |

---

## 12. Phasing & Open Questions

### 12.1 Proposed rollout

| Stage | Milestone | Exit criterion |
|---|---|---|
| **V1** | `lib/vmm-abi` + `abi-check`, `lib/pt` (EPT, NPT, Arm Stage-2), `sys/vmm` (`Vm`, `Vcpu`, `VmMem`, rendezvous), `/dev/vmmctl` + `/dev/vmm/<name>` | Host tests pass; `#[ktest]` creates a VM, maps segments, and validates state transitions |
| **V2** | `x86_64` Intel VMX + AMD SVM world switch, `vlapic`/`vioapic`/`vatpic`/`vatpit`/`vhpet`/`vpmtmr`/`vrtc`, x86 instruction emulator | Unmodified FreeBSD 15.1 `bhyve(8)` boots a multi-vCPU UEFI FreeBSD guest (serial console, `virtio-blk`, `virtio-net` over `tap`) on Intel VMX and AMD SVM |
| **V3** | `aarch64` EL2 VHE kernel boot (`arch/aarch64/src/entry.rs`), `HypCtx` world switch, `vgic_v3`, `vtimer`, PSCI | Unmodified FreeBSD 15.1 `bhyve(8)` boots a multi-vCPU UEFI FreeBSD/Linux guest on QEMU `virt` EL2 and Orange Pi 6 Plus |
| **V4** | `ppt` PCI passthrough over `sys/iommu` (SMMUv3, VT-d, AMD-Vi), APICv / AVIC posted-interrupt fast paths, `nmdm(4)`, performance parity audit | Passthrough NVMe/NIC works inside a `bhyve(8)` guest; VM-exit and virtio I/O within 2% of FreeBSD 15.1 |

### 12.2 Open questions (to settle in the ADR)

1. **`aarch64` EL2 VHE transition timing**: switching `arch/aarch64` to stay at EL2 under VHE
   (`HCR_EL2.E2H | TGE`) benefits the host kernel even without `vmm` (matches FreeBSD's own
   `arm64` boot model and simplifies `FEAT_HCX` / `SCTLR_EL2` handling). Should the VHE boot
   change land independently ahead of `V3`?
2. **IOMMU dependency for `V4`**: `drivers/smmu` is already underway for `aarch64` ([ADR-0022](adr/0022-smmuv3-in-p9.md)),
   whereas Intel VT-d (DMAR) and AMD-Vi (IVRS) are planned for P11. Should `V4` ship `ppt` on
   `aarch64` SMMUv3 first and add x86 `ppt` as VT-d/AMD-Vi land?
3. **`nmdm(4)` vs. PTY consoles**: `vm-bhyve` and many `bhyve(8)` scripts expect `/dev/nmdm*`
   null-modem devices. Include a small `nmdm` char-device driver in `V2`?
4. **Kernel feature default**: should `feature = "vmm"` be enabled by default in general kernel
   builds once `V2`/`V3` pass (matching `LINUXULATOR.md` §13.2), or opt-in?
5. **RISC-V H-extension (`V5` / Day 2)**: FreeBSD already has `sys/riscv/vmm/` using the RISC-V
   Hypervisor extension (`vs*` CSRs, `hgatp`, AIA IMSIC guest files). Keeping `VmmArch` and
   `lib/vmm-abi` arch-partitioned from `V1` ensures RV64 slots in cleanly when P12 arrives.
