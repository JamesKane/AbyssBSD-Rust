# ADR-0002: Dependency policy

- Status: Accepted
- Date: 2026-10-17

## Context

The project mandate is "no crates outside the project and Rust core". Taken literally, `core`
alone excludes `alloc` (`Box`, `Vec`, `Arc`, `BTreeMap`), which ships in the same Rust
distribution and has no external provenance. Host-side tools (build orchestration, image creation,
test drivers) also need files, processes, and networking, which come from `std`.

## Decision

1. **Kernel and loader** may use `core` and `alloc`, plus the toolchain-provided
   `compiler_builtins`. Collections from `alloc` are used through fallible APIs (`try_reserve`,
   `Box::try_new` when stable, or our own fallible wrappers).
2. **Host tooling and host-side tests** (`xtask`, `tools/*`, `#[cfg(test)]`) may use `std`.
3. **Nothing from crates.io or any other registry or git source**, anywhere in the workspace,
   including build-dependencies and dev-dependencies.
4. CI enforces this: every dependency in every `Cargo.toml` must be a `path =` inside the
   workspace.

## Consequences

- We write our own bitflags, register access, intrusive collections, CRC/hash functions, a test
  harness, argument parsing for `xtask`, and parsers for ELF, GPT, FAT, UFS2, and ACPI. This is
  extra work, but the code is small, focused, and fully understood.
- Supply-chain exposure is limited to the Rust toolchain itself.
