# ADR-0005: Monolithic, statically linked kernel

- Status: Accepted
- Date: 2026-10-17

## Context

FreeBSD is monolithic, with loadable modules (KLDs) that share an unstable C ABI. Rust has no stable
ABI, and building loadable Rust modules means either pinning the exact compiler and kernel build or
designing a C-like FFI boundary. Microkernel designs would put FreeBSD-compatible performance and
semantics (VM/VFS/socket coupling) at risk.

## Decision

- One statically linked kernel image. Drivers and file systems are selected at build time through
  Cargo features and per-platform configuration crates.
- Isolation comes from crate boundaries, the type system, and the `unsafe` policy, not from address
  spaces.
- Revisit loadable modules after v1.0 with a Rust-native design: modules built by the identical
  toolchain against an exported kernel interface crate, with versioned symbol checks.

## Consequences

- Simpler boot, linking, and debugging. LTO works across the whole kernel.
- `kldload`/`kldstat` return `ENOSYS` or report the single built-in "kernel" module. Base utilities
  that probe for modules (e.g. `ifconfig` auto-loading) must tolerate this. We provide a
  `kldfind`/`modfind` answer that reports built-in drivers as already loaded.
- Kernel images include every driver for the platform. That is acceptable, given the small modern
  driver set.
