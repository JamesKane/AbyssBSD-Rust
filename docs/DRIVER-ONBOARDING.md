# Driver Development and Porting Guide

This guide onboards developers writing new device drivers for AbyssBSD or porting existing drivers
from FreeBSD. Read [docs/DRIVER-MODEL.md](DRIVER-MODEL.md) for the full design of the device
framework and [docs/ENGINEERING.md](ENGINEERING.md) for the workspace-wide Rust standards.

---

## 1. Golden Rules for Driver Authors

Before writing a line of code, internalize the project's non-negotiable constraints:

1. **Zero `unsafe` in leaf drivers.** Every crate under `drivers/*` begins with `#![no_std]` and
   `#![forbid(unsafe_code)]`. All hardware access goes through safe, typed grants issued by the
   bus and platform broker (`sys/bus`, `lib/regs`, `sys/vm::dma`, `sys/irq`).
2. **Zero external crates.** Drivers may depend only on `core`, `alloc`, and workspace crates
   ([ADR-0002](adr/0002-dependency-policy.md)).
3. **Both architectures from day one.** Every driver must compile and pass checks on both
   `x86_64` and `aarch64`. Never assume x86 port I/O exists without gating or abstracting it, never
   assume x86 Total Store Ordering (TSO) for DMA or lockless rings, and never assume 4 KiB-only
   IOMMU pages.
4. **Device input is untrusted.** Anything read from an MMIO register or a DMA-shared memory region
   (completion queue entries, ring indices, status words, transfer lengths, capability offsets) may
   be corrupt, malicious, or `0xffff_ffff` (PCIe surprise removal). Validate every index and length
   and return a typed `Result`—never panic, index out of bounds, or spin forever.
5. **The framework owns the lifecycle.** Drivers do not write teardown loops. You implement
   `attach`, `start`, and `stop`; the framework handles revoking userland handles, draining
   interrupts, freeing grants, and checking the resource ledger.
6. **Exact FreeBSD userland view.** Driver names (`NAME`), unit numbering, descriptions (`%desc`),
   `pnpinfo`, location strings, and `/dev` nodes must match FreeBSD 15.1 byte for byte. If you are
   writing a driver for hardware FreeBSD does not support, record its name in
   [docs/compat-deviations.md](compat-deviations.md).
7. **2% performance parity gate.** Nothing on the I/O data path hops threads, takes the topology
   lock, or performs steady-state heap allocations ([docs/PERFORMANCE.md](PERFORMANCE.md)).
8. **Clean provenance.** FreeBSD (`../freebsd-src`) is a behavioral specification, not C code to
   transliterate line by line. **Never open or read GPL/LGPL source code (such as the Linux kernel)
   while writing AbyssBSD code.** When hardware is only documented in GPL sources, follow the
   clean-room reverse engineering protocol in [§6](#6-clean-room-reverse-engineering-for-gpl-only-hardware).

---

## 2. Mental Model: FreeBSD `newbus` vs. AbyssBSD `sys/bus`

If you are coming from FreeBSD kernel work or porting a driver from `../freebsd-src/sys/dev/`, use
this translation table. Userland sees FreeBSD's exact `newbus` hierarchy, but the internal
mechanics are closer to Windows WDF.

| FreeBSD (`newbus` in C) | AbyssBSD (`sys/bus` in Rust) | Key Difference |
|---|---|---|
| `DEVMETHOD(device_probe, …)` | `Driver::IDS` (`const` match table) + `Driver::describe` | Matching is pure data in `lib/devtree`, evaluated without running driver code. `BUS_PROBE_*` ranks are `devtree::matching::Rank`. Ties are rejected at build time. |
| `DEVMETHOD(device_attach, …)` | Split into `Driver::attach` and `Driver::start` | `attach` claims resources, resets hardware, and returns `Self` by value. Once pinned in its `Binding<Self>`, `start(&self, …)` installs interrupt handlers, adds children, and publishes to classes. |
| `DEVMETHOD(device_detach, …)` | `Driver::stop` + `Drop` | **There is no `detach` callback.** The framework detaches children, revokes gates, calls `stop` (where you halt DMA and mask device interrupts), removes IRQ handlers synchronously, drops `Self` (releasing all grants), and checks the ledger. |
| `struct foo_softc` | `pub struct Foo` implementing `Driver` | Owned by the framework's `Binding<Foo>`. Never exported via `Arc<Foo>`. Callbacks receive `&Foo`. |
| `BUS_READ_IVAR` / `pci_get_vendor(dev)` | `cx.ident()` and `cx.bus_data::<T>()` | Typed per-bus child data (e.g., `PciChild`, `VirtioDevice`, `UsbInterface`) instead of untyped ivar keys. |
| `bus_alloc_resource_any(..., SYS_RES_MEMORY, ...)` | `cx.mmio::<B>(index)` or `cx.mmio_bytes(index)` | Returns an owned `MmioGrant<B>` that unmaps and releases its claim on `Drop`. |
| `bus_alloc_resource_any(..., SYS_RES_IOPORT, ...)` | `cx.io_ports(index)` | Returns an owned `IoGrant` (x86 I/O ports; no-op/0xff on non-x86). |
| `pci_alloc_msix` / `pci_alloc_msi` | `pci::bus::msix_lines(cx, n)` / `msi_line(cx)` | Returns `(Vec<IrqLine>, pci::bus::Messages)`. Dropping `Messages` after handlers are removed disables MSI/MSI-X automatically. |
| `bus_setup_intr` / `bus_teardown_intr` | `StartCx::interrupt(line, filter, thread, arg)` | Handlers are plain `fn(&Self, usize, &IrqCx)` and `fn(&Self, usize, &Sleepable)`—never closures that could capture state past detach. Removed automatically by the framework at detach. |
| `bus_dma_tag_create` / `bus_dmamem_alloc` / `bus_dmamap_load` | `cx.dma_tag(DmaConstraints)` → `tag.coherent::<T>(len)` / `tag.map(buf)` | `DmaCoherent<T: DeviceShared>` for rings/queues; `Mapped<B, Dir>` moves the buffer by value while the device owns it, making missing syncs or CPU-device data races unrepresentable. |
| `make_dev` / `destroy_dev` / `disk_alloc` | Class publication (`storage::publish`, `watchdog::register`, `Gate`) | User-visible handles reach the driver through a revocable `Gate` (`sys/bus/src/gate.rs`). |
| `EARLY_DRIVER_MODULE(..., BUS_PASS_*)` | Firmware dependency graph (`_DEP`, `_CRS`, IORT) | Attach, shutdown, suspend, and resume ordering are derived from the topological graph in `lib/devtree::graph`, not numeric bus passes. |
| `DRIVER_MODULE(foo, pci, ...)` | `pub const DRIVER: DriverDecl = DriverDecl::of::<Foo>();` | Listed explicitly in `kernel/table/src/lib.rs` (`DRIVERS`). No linker-section magic. |

---

## 3. Anatomy of an AbyssBSD Driver

Every driver lives in `drivers/<name>/` and follows a consistent structure:

1. **Pure protocol / hardware logic**, parameterized over a small register trait (or `lib/regs::RegisterIo`) so that unit tests on the host can exercise the state machine against a fake in-memory model.
2. **The `Driver` implementation**, wiring `AttachCx`, `StartCx`, and `StopCx` to the hardware logic and the appropriate device class (`sys/storage`, `sys/watchdog`, `sys/tty`, `sys/input`, etc.).
3. **Host unit tests** (`#[cfg(test)] mod tests`) that run under `cargo test`.

### 3.1 Minimal Template

```rust
// SPDX-License-Identifier: BSD-2-Clause
//! Short summary of the hardware and how this driver operates it.
//!
//! Behavior reference: sys/dev/<foo>/<foo>.c; <Hardware Specification Name>.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use bus::{
    AttachCx, AttachError, Concurrency, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant,
    StartCx, StopCx, StopReason,
};
use devtree::ids::PciId;
use devtree::matching::Rank;
use irq::Filter;
use pci::bus::{Messages, PciChild};
use sched::{IrqCx, Sleepable};
use sync::SpinLock;

regs::register_block! {
    /// Register map from Hardware Spec §X.Y.
    pub struct Regs: 0x100 {
        CTRL   @ 0x00 => u32, Rw;
        STATUS @ 0x04 => u32, Ro;
    }
}

const CTRL_EN: u32 = 1 << 0;
const CTRL_IRQ_EN: u32 = 1 << 1;
const STATUS_PENDING: u32 = 1 << 0;

pub struct MyDev {
    regs: MmioGrant<Regs>,
    irq: SpinLock<Option<IrqLine>>,
    _msi: Option<Messages>,
}

impl core::fmt::Debug for MyDev {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MyDev").finish_non_exhaustive()
    }
}

fn irq_filter(d: &MyDev, _arg: usize, _cx: &IrqCx) -> Filter {
    if d.regs.read(Regs::STATUS) & STATUS_PENDING == 0 {
        return Filter::Stray;
    }
    // Mask or acknowledge at the device and wake the interrupt thread.
    Filter::Schedule
}

fn irq_thread(d: &MyDev, _arg: usize, _s: &Sleepable) {
    // Process completions in thread context.
    let _ = d;
}

impl Driver for MyDev {
    const NAME: &'static str = "mydev";
    const IDS: Ids = Ids::Pci(&[
        PciId::device(0x1234, 0x5678).rank(Rank::DEFAULT),
    ]);
    const CONCURRENCY: Concurrency = Concurrency::Serialized;

    fn describe(_ident: &Ident) -> String {
        "Example PCI Controller".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let pci = cx.bus_data::<PciChild>().ok_or(AttachError::Decline)?;
        pci.enable_decoding();
        pci.set_bus_master(true);

        let regs = cx.mmio::<Regs>(0)?;
        let (line, msi) = match pci::bus::msi_line(cx) {
            Ok((line, msi)) => (line, Some(msi)),
            Err(_) => (cx.irq(0)?, None),
        };

        // Reset hardware before returning. If anything below fails with `?`,
        // `regs`, `line`, and `msi` drop automatically and release all grants.
        regs.write(Regs::CTRL, 0);

        Ok(Self {
            regs,
            irq: SpinLock::new(Some(line)),
            _msi: msi,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        // Take the IrqLine in its own statement so the SpinLock is not held across
        // `cx.interrupt`, which may sleep.
        let line = self.irq.lock().take();
        if let Some(line) = line {
            cx.interrupt(line, Some(irq_filter), Some(irq_thread), 0)?;
        }
        self.regs.write(Regs::CTRL, CTRL_EN | CTRL_IRQ_EN);
        Ok(())
    }

    fn stop(&self, _cx: &mut StopCx<'_>, _why: StopReason) {
        // Quiesce the hardware: stop DMA and mask device interrupts.
        self.regs.write(Regs::CTRL, 0);
    }
}

pub const DRIVER: DriverDecl = DriverDecl::of::<MyDev>();
```

### 3.2 Canonical Reference Drivers in the Tree

When in doubt, read an existing driver of the same shape:

| Shape | Crate | What it demonstrates |
|---|---|---|
| Simple I/O-port / MMIO watchdog | [`drivers/ichwd`](../drivers/ichwd/src/lib.rs), [`drivers/amdsbwd`](../drivers/amdsbwd/src/lib.rs), [`drivers/sbsa-gwdt`](../drivers/sbsa-gwdt/src/lib.rs) | Abstracting registers behind a trait for host unit tests, `watchdog::register`, quirk detection (`sbsa-gwdt` WOR refresh on Sky1). |
| ACPI leaf driver | [`drivers/uart`](../drivers/uart/src/lib.rs), [`drivers/timers`](../drivers/timers/src/lib.rs) | `Ids::Acpi`, matching multiple `_HID`/`_CID`s, adopting early console resources. |
| Virtio leaf driver | [`drivers/virtio-random`](../drivers/virtio-random/src/lib.rs), [`drivers/virtio-blk`](../drivers/virtio-blk/src/lib.rs) | `VirtioDevice` bus data, feature negotiation, `DmaTag::map`, `Gate` revocation (`virtio-random`), `storage::publish` (`virtio-blk`). |
| Multi-queue PCIe storage | [`drivers/nvme`](../drivers/nvme/src/lib.rs) | `PciChild`, MSI-X per CPU, admin queue polling during `attach`, PRP lists with `DmaCoherent`, `Concurrency::Concurrent`, `DiskQueues`. |
| Controller + channel children | [`drivers/ahci`](../drivers/ahci/src/lib.rs) | Adding per-channel `Ident::Channel` (`ahcich`) children via `StartCx::add_child` and `add_disabled_child`. |
| Dual-bus (PCI + ACPI) host controller | [`drivers/xhci`](../drivers/xhci/src/lib.rs) + [`lib/xhci`](../lib/xhci/src/lib.rs) | Splitting pure protocol/ring logic into `lib/*` and exposing both `PCI` and `ACPI` (`PNP0D10`) `DriverDecl`s from `drivers/*`. |

---

## 4. Core Abstractions Reference

### 4.1 Register Access (`lib/regs`)

Never cast addresses to raw pointers. Choose one of three mechanisms in [`lib/regs`](../lib/regs/src/lib.rs):

1. **Typed register blocks (`register_block!` + `cx.mmio::<B>(bar_index)`):**
   Preferred for fixed-offset MMIO blocks. Checks offsets, alignment, and total block size at
   compile time, and enforces `Ro` / `Wo` / `Rw` in the type system. Use [`Field<T>`](../lib/regs/src/block.rs)
   and the [`Bits`](../lib/regs/src/block.rs) trait (`.field(F)`, `.has(F)`, `.with(F, val)`) for bitfields.
2. **Untyped byte-offset MMIO (`cx.mmio_bytes(bar_index)` → `MmioGrant<Bytes>`):**
   Use when register offsets are reported dynamically by the hardware (e.g., PCIe or xHCI capability
   chains, or variable doorbell strides). Every `.read32(off)` / `.write32(off, val)` bounds-checks
   and alignment-checks `off` against the mapped BAR length.
3. **Dual MMIO / I/O-port byte registers (`RegisterIo` / `StridedMmio` / `IoGrant`):**
   Use when hardware (such as a 16550 UART) can be wired in x86 I/O space or MMIO with a configurable
   stride.

### 4.2 DMA and Shared Memory (`sys/vm::dma` and `lib/regs::DeviceShared`)

1. **Create a tag in `attach`:**
   ```rust
   let tag = cx.dma_tag(DmaConstraints {
       addr_bits: 64,
       align: PAGE as u64,
       boundary: 0,
       max_segment: PAGE as u64,
       max_segments: 32,
   })?;
   ```
2. **Long-lived rings and descriptors (`DmaCoherent<T>`):**
   Allocate with `tag.coherent::<T>(count)`. `T` must implement [`DeviceShared`](../lib/regs/src/shared.rs)
   via `regs::device_shared!`.
   - `device_shared!` verifies at compile time that `T` is `#[repr(C)]`, has **zero implicit padding**,
     and contains only integers and arrays of `DeviceShared` types (no pointers, references, `bool`,
     or `enum`s that a misbehaving device could write invalid bit patterns into).
   - Access elements with `.read(idx)`, `.write(idx, val)`, `.read_at::<U>(byte_off)`, and
     `.write_at::<U>(byte_off, val)`. Pass `.bus_addr()` to the hardware register.
3. **Streaming I/O buffers (`tag.map` / `Mapped`):**
   Map a `Bio`'s `IoBuf` with `tag.map::<dir::FromDevice, _>(buf)` or `tag.map::<dir::ToDevice, _>(buf)`.
   Ownership of `IoBuf` moves into `Mapped`, performing the pre-transfer cache/bounce sync and
   exposing `.segments()`. When the hardware completes the transfer, call `mapped.unmap()` to perform
   the post-transfer sync and recover the `IoBuf` before calling `bio.complete(...)`.

### 4.3 Interrupts, Concurrency, and Locks

- **Context tokens:**
  - Interrupt filters receive `&IrqCx`. They may acquire a `sync::SpinLock` and read/write MMIO,
    and must return `Filter::Stray`, `Filter::Handled`, or `Filter::Schedule`. They **cannot**
    sleep or acquire a `sched::lock::Mutex`.
  - Interrupt threads (`ithread`s), `attach`, `start`, `stop`, and class callbacks receive
    `&Sleepable`. Sleepable locks (`sched::lock::Mutex`) require `&Sleepable` as proof that the
    caller is in thread context.
- **Never hold a `SpinLock` across a call that takes `&Sleepable`.** Extract the value in a separate
  statement first (`let item = self.lock.lock().take();`).
- **Lock classes:** Every `sched::lock::Mutex` requires a static `LockClass` with a witness hierarchy
  rank (driver locks use rank `>= 450`, above `SERIALIZER` at `400`). The kernel's lock witness
  checks lock ordering on every acquisition.
- **`Concurrency::Serialized` vs. `Concurrency::Concurrent`:**
  - Leave `CONCURRENCY` at the default (`Concurrency::Serialized`) for simple drivers; the framework
    acquires the binding's serializer mutex around thread-context entry points.
  - Set `Concurrency::Concurrent` for multi-queue storage or network drivers that manage their own
    per-queue locks.

---

## 5. Step-by-Step Workflow: Porting a Driver from FreeBSD

### Step 1: Study the Specification and the FreeBSD Reference

1. Locate the hardware specification (e.g., NVMe Base Spec, AHCI, xHCI, VirtIO, Intel/AMD/Arm
   datasheet) and the corresponding FreeBSD driver in `../freebsd-src/sys/dev/<name>/`.
2. Identify the **compatibility contract** in the FreeBSD driver:
   - What is the driver's devclass name (`driver_t.name`)?
   - What PCI IDs, PCI class codes, ACPI `_HID`s, or USB IDs does it match, and at what
     `BUS_PROBE_*` priority?
   - What exact string does `device_set_desc()` set for `%desc`?
   - Does it create child devices (`device_add_child`) or `/dev` nodes (`make_dev`)? What are their
     names?
   - What hardware quirks or errata workarounds does the FreeBSD driver apply?
3. Add a module-level doc comment at the top of `src/lib.rs` citing both the specification and the
   FreeBSD behavioral reference:
   ```rust
   //! Behavior reference: sys/dev/<name>/<file>.c; <Hardware Spec>.
   ```

### Step 2: Scaffold the Crate

1. Create `drivers/<name>/Cargo.toml` and `drivers/<name>/src/lib.rs`:
   - Inherit `version.workspace = true`, `edition.workspace = true`, `license.workspace = true`,
     `publish.workspace = true`, and `[lints] workspace = true`.
   - Use only `path = "../../..."` dependencies.
   - Start `src/lib.rs` with `// SPDX-License-Identifier: BSD-2-Clause`, `#![no_std]`, and
     `#![forbid(unsafe_code)]`.
2. Add `"drivers/<name>"` to both `members` and `default-members` in the root
   [`Cargo.toml`](../Cargo.toml).

### Step 3: Write Host-Testable Hardware Logic First

Before touching `AttachCx`, model the register interface or command rings so they can be tested in
isolation on the host:

- For register-driven devices, either define a `register_block!` (which can be backed by a `Vec<u64>`
  in unit tests via `Mmio::block` inside `lib/regs`, or wrapped in a small register trait like
  [`TcoRegs`](../drivers/ichwd/src/lib.rs#L48-L53) in `drivers/ichwd` or
  [`Frame`](../drivers/sbsa-gwdt/src/lib.rs#L49-L54) in `drivers/sbsa-gwdt`).
- For complex ring/descriptor protocols (like `lib/xhci` or `lib/scsi`), consider putting the pure
  state machine and descriptor parsers in `lib/<name>` with `#![forbid(unsafe_code)]` and thorough
  unit tests.
- Run `cargo test -p <name>` on the host until edge cases, boundary math, and error paths are
  covered.

### Step 4: Implement `Driver` and Register in `kernel/table`

1. Implement `bus::Driver` for your driver struct and export:
   ```rust
   pub const DRIVER: DriverDecl = DriverDecl::of::<MyDriver>();
   ```
2. Add your crate as a dependency in [`kernel/table/Cargo.toml`](../kernel/table/Cargo.toml) and add
   `<name>::DRIVER` to `DRIVERS` in [`kernel/table/src/lib.rs`](../kernel/table/src/lib.rs).
3. Run the match-table ambiguity and corpus tests:
   ```sh
   cargo test -p driver-table
   ```
   If your driver ties with another driver on any PCI/ACPI/Virtio/USB ID at the same `Rank`,
   `the_driver_table_has_no_ties` will fail. Fix the rank (e.g., `Rank::GENERIC` for class-code
   matches vs. `Rank::DEFAULT` for specific vendor/device matches) or declare `const YIELDS`.

### Step 5: Verify Across the Full Test Pyramid

Run the verification suite via `cargo xtask` ([docs/BUILD-AND-TEST.md](BUILD-AND-TEST.md)):

```sh
cargo xtask check                       # fmt, clippy (host + x86_64 + aarch64), SPDX & dep policy
cargo xtask test --tier unit            # all host unit tests
cargo xtask test --tier boot            # boot tests in QEMU on x86_64 and aarch64 (EL1 & EL2)
```

 When testing in QEMU (`cargo xtask run --arch x86_64 --cmdline "abyss.verifier=1 abyss.verifier.faults=8"`):
- **Verifier mode (`abyss.verifier=1`)** checks that every `Bio` is completed exactly once, stalls
  on detach are caught, and the binding's grant ledger returns to `0` on detach.
- **Fault injection (`abyss.verifier.faults=N`)** fails one resource grant in `N` during `attach` to
  prove that your error-unwind paths reset the hardware and leak no grants.

---

## 6. Clean-Room Reverse Engineering for GPL-Only Hardware

AbyssBSD is licensed under **BSD-2-Clause** ([ADR-0007](adr/0007-license-and-clean-implementation.md)).
All code in this repository is original Rust and may be shared back with the BSD community.

Some modern hardware—particularly ARM64 SoC blocks (such as CIX Sky1 or Qualcomm SC8280XP clocks,
power domains, display controllers, and GPUs) or certain NICs—lacks public programming manuals and
has no FreeBSD, NetBSD, or OpenBSD driver, leaving a GPLv2 Linux driver as the only public reference.

**You must never read GPL or LGPL code (including the Linux kernel, U-Boot GPL files, or GPL ACPICA)
and then write code for AbyssBSD.** Reading a Linux `.c` or `.h` file while implementing an AbyssBSD
driver—even if you rewrite it in Rust—creates copyright taint risk.

### 6.1 Source Hierarchy (Check Before Resorting to Clean Room)

Always check permissive and primary sources in this order:

1. **Published hardware specifications and standards:** PCIe, NVMe, AHCI, xHCI, USB, HID, VIRTIO,
   UEFI, ACPI, Arm ARM / BSA / SBSA / GIC / SMMU / SCMI (Arm DEN0056), Intel SDM, AMD APM, vendor
   datasheets.
2. **Permissively licensed OS trees (BSD / MIT / ISC / Apache-2.0):**
   - **FreeBSD** (`../freebsd-src`, BSD-2-Clause)
   - **OpenBSD** and **NetBSD** (often have clean BSD drivers for NICs, Wi-Fi, and ARM64 SoC blocks
     written from specs or vendor docs)
   - **illumos**, **Fuchsia**, **Tock**, **Hubris**, **Asterinas**, **EDK2** (BSD-2-Clause-Patent)
   - **BSD/MIT vendor HALs or reference code** (check the SPDX header on every file before reading!)
3. **Captured hardware ground truth (`tests/corpus/`):**
   ACPI tables (`acpidump` / DSDT / SSDT AML, which our own AML interpreter in `lib/aml` reads), PCI
   config space dumps (`pciconf -lv`, `lspci -xxxx`), and black-box hardware register dumps.
4. **Two-Team Clean-Room Reverse Engineering** (when only GPL sources exist).

### 6.2 The Two-Team Clean-Room Protocol ("Chinese Wall")

When a hardware block is documented only in a GPL driver (for example, in the Linux kernel), we
strictly separate **Specification Extraction (Dirty Room)** from **Implementation (Clean Room)**:

```
 ┌───────────────────────────────────┐
 │  GPL Source (e.g. Linux driver)   │
 └─────────────────┬─────────────────┘
                   │  read by
                   ▼
 ┌───────────────────────────────────┐
 │     Analyst ("Dirty Room")        │
 │  Extracts pure hardware facts:    │
 │  register offsets, bitfields,     │
 │  timing bounds, state transitions │
 └─────────────────┬─────────────────┘
                   │  produces audited
                   ▼
 ┌───────────────────────────────────┐      ┌───────────────────────────────────┐
 │   Hardware Specification Doc      │◄─────┤  Black-Box Hardware Observations  │
 │   (Pure facts, zero code/names)   │      │  (ACPI DSDT, PCIe traces, dumps)  │
 └─────────────────┬─────────────────┘      └───────────────────────────────────┘
                   │  read by
                   ▼
 ┌───────────────────────────────────┐
 │   Implementer ("Clean Room")      │
 │   Has NEVER seen the GPL source.  │
 │   Writes idiomatic Rust driver.   │
 └───────────────────────────────────┘
```

#### Role 1: The Analyst (Dirty Room)
The Analyst may read the GPL source code, vendor device trees, and commit logs solely to understand
**how the physical hardware works**. The Analyst writes a **Hardware Specification Document** and
never writes, edits, or reviews the AbyssBSD Rust implementation for that device.

- **What the Analyst MAY include in the Hardware Spec:**
  - Physical facts required by hardware interoperability (which are not copyrightable under the
    *scènes à faire* and merger doctrines):
    - PCI Vendor/Device IDs, ACPI `_HID` strings, MMIO BAR indices.
    - Register byte offsets, widths (`8`/`16`/`32`/`64`-bit), reset values, and access semantics
      (read-only, write-only, write-1-to-clear, self-clearing strobe).
    - Bitfield positions (`bits [m:n]`) and what each hardware bit controls or reports.
    - Descriptor / ring buffer binary layouts in memory (byte offsets, endianness, alignment, chain
      termination bits, ownership/phase bits).
    - Required hardware state transitions and timing constraints (e.g., *"Set bit 0 of register
      `0x14` to `0`, then poll bit 0 of register `0x1c` every 1 ms for up to 500 ms until it reads
      `0`"*).
    - Hardware errata and silicon bugs (e.g., *"On revision `0x10`, writing the refresh frame at
      `0x000` does not update `WCV`; write the offset value to `WOR` at `0x008` instead"*).
- **What the Analyst MUST NOT include in the Hardware Spec:**
  - Copied code snippets, macros, or comments from the GPL source.
  - Software architecture, helper function breakdowns, lock hierarchies, or control-flow structure
    of the GPL driver.
  - Linux-internal struct names, function names, or creative identifier naming schemes (describe
    registers by their hardware function or datasheet name, or by offset if no datasheet name
    exists).

#### Role 2: The Implementer (Clean Room)
The Implementer must have no exposure to the GPL source for that device. Working **only** from:
1. The Analyst's Hardware Specification Document,
2. Public standards (PCIe, ACPI, etc.) and black-box hardware captures (`tests/corpus/`), and
3. AbyssBSD's own [DRIVER-MODEL.md](DRIVER-MODEL.md) and class traits,

the Implementer designs and writes the Rust driver from scratch using AbyssBSD's idioms (`Driver`,
`register_block!`, `device_shared!`, typestate lifecycles, and host unit tests).

### 6.3 Black-Box Observation Techniques

Often you can avoid reading GPL driver code altogether—or verify an Analyst spec independently—by
observing the hardware or firmware directly:

- **Read the machine's ACPI AML (`DSDT` / `SSDT`):** On ACPI platforms (including both our AMD64
  and ARM64 reference machines), firmware methods (`_INI`, `_PS0`, `_DSM`, operation regions, PCC
  channels) frequently contain the exact register offsets and initialization sequences the vendor
  intended. Decompile or inspect the captured tables in `tests/corpus/`.
- **QEMU device models when BSD/MIT-licensed:** Note that most of QEMU is GPLv2 (treat it as Dirty
  Room!), though specific headers or specs in standard committees (VIRTIO OASIS spec, PCI SIG) are
  open standards. Always prefer the OASIS VIRTIO specification over QEMU's `.c` files.
- **Hardware register snapshots and traces:** Capturing PCIe configuration space (`lspci -xxxx`),
  BAR layouts, and pre-/post-boot register states (captured into `tests/corpus/` before reimaging a
  board, as noted in [docs/HARDWARE.md](HARDWARE.md) §4) establishes ground-truth hardware state
  without reading driver source code.

### 6.4 Provenance Documentation in Every Driver

Every driver file must maintain clear provenance ([ENGINEERING.md §6–§7](ENGINEERING.md#7-clean-implementation-and-licensing)):

1. Start line 1 with `// SPDX-License-Identifier: BSD-2-Clause`.
2. In the top-level `//!` module doc comment, include a `Behavior reference:` line naming the
   FreeBSD files (`sys/dev/...`), the published hardware specification (with version and section
   numbers), or the clean-room hardware specification document used to write the driver.
3. Run `cargo xtask policy` before committing to verify SPDX headers and dependency rules.
