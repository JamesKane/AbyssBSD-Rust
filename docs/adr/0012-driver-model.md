# ADR-0012: Driver model

- Status: Accepted
- Date: 2026-10-18

## Context

Phase 3 brings the device framework: ACPI and PCI enumeration, interrupt domains, DMA, and the
first storage drivers. The driver model is hard to change once dozens of drivers depend on it.

FreeBSD's newbus defines what userland sees: names and units, `devinfo`, `devctl`, devd events,
`sysctl dev.*`, `pciconf`. That view must be reproduced exactly. Its internals are another matter.
Its topology lock is still Giant, drivers drain their own asynchronous sources by hand, ordering is
by numeric bus passes, and kobj dispatch is untyped.

A survey of FreeBSD, NetBSD/OpenBSD, illumos, Linux (including Rust for Linux), macOS
IOKit/DriverKit, Windows WDM/WDF, Fuchsia DFv2, Rust kernels (Asterinas, RedLeaf, Theseus, Tock,
Hubris, Redox), and microkernel systems is recorded in
[DRIVER-MODEL-SURVEY.md](../DRIVER-MODEL-SURVEY.md). It finds the same failure classes wherever
drivers own lifecycle and teardown, and it measures the cost of address-space or thread-hop
isolation at far more than our 2% performance bound
([ADR-0011](0011-performance-parity.md)).

## Decision

Adopt the driver model specified in [DRIVER-MODEL.md](../DRIVER-MODEL.md). Its load-bearing parts:

1. **FreeBSD's userland view, byte for byte**: `hw.bus.*`, `dev.*`, `/dev/devctl`, `/dev/devctl2`,
   `/dev/pci`, and the pnpinfo and location strings, checked by abi-check and differential tests.
2. **The framework owns the lifecycle.** Drivers implement `attach`, `start`, and `stop`. Detach is
   a fixed framework sequence: revoke publications, stop, tear down async sources, drop, check the
   ledger.
3. **Every way into a driver is created through its binding**, so detach can drain all of them.
4. **Three lifetimes in the types**: device node (`DeviceRef`), binding (owned by the manager), and
   user-visible handles that reach the driver only through a revocation `Gate`.
5. **Matching is `const` data**, ranked with FreeBSD's `BUS_PROBE_*` values. Ambiguity is rejected
   at build time.
6. **One dependency graph** derived from ACPI (`_DEP`, `_CRS` resource sources, power resources,
   PCC, IORT) orders attach, shutdown, suspend, and resume. It replaces bus passes and deferred
   probe. A waiting device always names what it waits for.
7. **Declared concurrency** (serialized by default, concurrent by opt-in), context tokens for
   interrupt and sleepable context, and no thread hops on the data path.
8. **A platform resource broker** grants typed, vetted MMIO, interrupt, and DMA handles. Driver
   crates are `#![forbid(unsafe_code)]`.
9. **Typed DMA ownership** (`DmaCoherent<T: DeviceShared>`, a `DmaStream` typestate, pools), with
   no kernel pointers in DMA-visible memory.
10. **Classes with batch-first, queue-shaped traits** own protocol logic and user interfaces.
    Requests are completed exactly once, by move.
11. **No stable driver ABI**, consistent with [ADR-0005](0005-monolithic-static-kernel.md).

## Consequences

- P3 grows by a new pure crate (`lib/devtree`), a `sys/irq` crate split out of the arch crates,
  the DMA layer in `sys/vm`, and a verifier mode with a detach stress test. P3's benchmark suite
  gains gate cost, attach time, DMA map throughput, and detach/attach cycling.
- [ARCHITECTURE.md §6](../ARCHITECTURE.md#6-device-model-sysbus) now summarizes DRIVER-MODEL.md
  and points to it for the specification.
- Drivers are smaller than their FreeBSD counterparts: no teardown code, no cancellation code,
  no resource-release paths. The framework carries that complexity once, in one tested place.
- Some FreeBSD behaviors become impossible to reproduce exactly, for example a driver detaching
  while still holding a callout. Each such case is a FreeBSD bug, not ABI.
- User-space or isolated drivers stay out of scope. The grant-and-trait boundary keeps that option
  open for low-rate classes after v1.0.
