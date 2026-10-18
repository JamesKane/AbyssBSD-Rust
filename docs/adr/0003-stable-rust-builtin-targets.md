# ADR-0003: Pinned stable Rust with built-in targets only

- Status: Accepted
- Date: 2026-10-17

## Context

Many hobby kernels use nightly Rust for custom target JSON files and `-Zbuild-std`. Nightly
features churn, though, and they make reproducible builds and long-term maintenance harder. Stable
Rust now has everything a kernel fundamentally needs: `asm!`, `global_asm!`, naked functions,
`#[no_std]`/`#[no_main]`, `GlobalAlloc`, and `#[panic_handler]`. It also ships prebuilt `core` and
`alloc` for the bare-metal and UEFI targets we need.

## Decision

- Pin a stable toolchain in `rust-toolchain.toml` (initially the current stable on the dev
  workstation, 1.99) and move it forward deliberately in its own commits.
- Use only built-in targets: `x86_64-unknown-uefi`, `aarch64-unknown-uefi`, `x86_64-unknown-none`,
  `aarch64-unknown-none-softfloat`, and later `riscv64gc-unknown-none-elf` (plus a RISC-V UEFI
  target when one is available with prebuilt `core`).
- Adapt to target-spec limitations with linker scripts and `-C` flags (code model, relocation
  model, frame pointers) set in `xtask`, not with custom target JSON.
- Any nightly feature requires a superseding ADR that justifies it.

## Consequences

- Reproducible builds on both hosts (Linux workstation, FreeBSD server) via rustup.
- Some hardening features that are nightly-only (e.g. `-Zbranch-protection` for arm64 PAC/BTI)
  wait until they stabilize, or they get a narrowly scoped exception.
- The kernel's ABI details (soft-float, red zone disabled) are whatever the built-in `*-none`
  targets define. Those are kernel-appropriate.
