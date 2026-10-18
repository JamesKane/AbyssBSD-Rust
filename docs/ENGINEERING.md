# Engineering Standards

## 1. Language and toolchain

- **Rust edition 2024**, using a **pinned stable toolchain** via `rust-toolchain.toml`
  ([ADR-0003](adr/0003-stable-rust-builtin-targets.md)). Nightly features are not allowed. If one
  becomes compelling, it needs an ADR.
- **Built-in targets only**, with no custom target JSON and no `-Zbuild-std`:

  | Role | AMD64 | ARM64 | RV64 (Day 2) |
  |---|---|---|---|
  | Loader | `x86_64-unknown-uefi` | `aarch64-unknown-uefi` | `riscv64gc-unknown-uefi`* |
  | Kernel | `x86_64-unknown-none` | `aarch64-unknown-none-softfloat` | `riscv64gc-unknown-none-elf` |
  | Host tests | host triple | host triple | host triple |

  \* If the RV64 UEFI target is still tier 3 without prebuilt `core` at that point, we revisit this
  in an ADR.
- Kernel profile: `panic = "abort"`, `opt-level = 2` in debug and `3` in release, `debug = true`
  (symbols are always kept and stripped at install time), LTO thin in release, and
  `overflow-checks = true` in debug.
- The kernel is compiled with soft-float / no-SIMD codegen. FP/SIMD state is user-only and is saved
  and restored by the arch layer.

## 2. Dependency policy

See [ADR-0002](adr/0002-dependency-policy.md).

- **Kernel and loader**: `core` and `alloc` only. The `compiler_builtins` crate that the toolchain
  links implicitly is accepted. Nothing from crates.io.
- **Host tooling** (`xtask`, `tools/*`, `#[cfg(test)]` code): may use `std`, but still no crates.io.
- Things we would normally import, such as bitflags, register macros, intrusive collections, CRC,
  hashing, ELF/GPT/FAT parsing, a test harness, and an argument parser for `xtask`, are written here
  as small, focused, well-tested crates under `lib/`.
- `Cargo.lock` is committed. CI rejects any `[dependencies]` entry that is not a `path =`
  dependency inside the workspace.

## 3. Type design: algebraic types first

The type system is our first line of defense. Design rules:

### 3.1 Make illegal states unrepresentable

```rust
// Not: struct Vnode { kind: u8, rdev: u32, link_target: Option<..>, ... }
pub enum VnodeKind {
    Regular(RegularData),
    Directory(DirData),
    Symlink(SymlinkTarget),
    CharDevice(DeviceRef),
    Fifo(FifoRef),
    Socket(SocketRef),
}
```

### 3.2 Newtypes for every unit and address space

`PhysAddr`, `VirtAddr`, `UserAddr`, `Pfn`, `Lba`, `ByteCount`, `Pid`, `Tid`, `Uid`, `Gid`, `Fd`,
`IrqNumber`, `CpuId`, `Nanoseconds`, and so on. Arithmetic is only implemented where it makes
sense (`VirtAddr + ByteCount → VirtAddr`, never `PhysAddr + VirtAddr`). Conversions between
address spaces go through explicit, audited functions (`dmap::phys_to_virt`).

### 3.3 Typestate for lifecycles

```rust
pub struct Nvme<S: State> { regs: Mmio<NvmeRegs>, state: S }
impl Nvme<Reset>     { pub fn configure(self, cfg: &Config) -> Result<Nvme<Configured>, Error> }
impl Nvme<Configured>{ pub fn enable(self) -> Result<Nvme<Ready>, Error> }
impl Nvme<Ready>     { pub fn submit(&self, cmd: Command) -> Result<Completion, Error> }
```

This pattern applies to device bring-up, CPU startup, page-table construction, socket state
machines (TCP state is an `enum` with per-state data), and process lifecycle
(`Embryo → Running → Zombie`).

### 3.4 Errors are values

- Syscall paths return `Result<T, Errno>`. `Errno` is a `#[repr(i32)] enum` with FreeBSD's values.
- Subsystems define their own error enums and convert them with `From` at boundaries. The
  conversion to `Errno` is explicit and reviewed, never `as i32`.
- **No panics on input-driven paths.** Data from user space, disk, network, firmware (ACPI/AML,
  SMBIOS), or devices is untrusted. Parsing returns `Result`. `unwrap`/`expect` are allowed only on
  invariants the code itself established, with a comment saying why the invariant holds.
- A `panic` means a kernel bug. It halts the system with diagnostics.

### 3.5 Ownership mirrors resource lifetime

Owned handles (`IrqHandle`, `DmaBuffer`, `MmioRegion`, `FileRef`, `VnodeRef`) release in `Drop`.
Reference counts use `Arc`-like types tuned for the kernel (`KArc`, with weak references and
intrusive variants) and avoid hidden allocation.

### 3.6 Allocation is fallible

Use `try_new`, `try_reserve`, and zone allocation that returns `Result`. Infallible `alloc` APIs
that abort on OOM are banned in kernel crates by a clippy `disallowed_methods` list, except in
early boot and init-only code.

## 4. `unsafe` policy

1. Every crate starts with `#![deny(unsafe_op_in_unsafe_fn)]`. Crates that do not need `unsafe`
   use `#![forbid(unsafe_code)]`. That is the default for `lib/*` except `lib/regs` and
   `lib/collections`.
2. Every `unsafe` block carries a `// SAFETY:` comment that states the invariant relied on. Clippy
   `undocumented_unsafe_blocks` is set to deny.
3. Every `unsafe fn` documents its contract under `# Safety`.
4. `unsafe` is concentrated behind safe abstractions in: `arch/*`, `lib/regs` (MMIO and system
   registers), `sys/vm` (pmap, DMAP, user copy), `sys/sync` (lock internals), and the
   intrusive-collection primitives.
5. Changes that add or modify `unsafe` need a second reviewer (or, for solo work, a written
   self-review note in the PR), plus a Miri run where the code is host-testable.
6. `cargo xtask unsafe-report` counts `unsafe` per crate. The total is tracked in CI and
   should trend down relative to code size.

## 5. Concurrency rules

- Lock ordering is encoded in types ([ARCHITECTURE.md §5.1](ARCHITECTURE.md#51-synchronization-syssync)).
- Interrupt-context code cannot sleep. This is enforced by context tokens.
- Atomics must use the weakest correct `Ordering`, and every non-`SeqCst` choice is commented. All
  concurrent code is reviewed for ARM64's weak memory model; x86 TSO is not an excuse.
- Per-CPU data is accessed through guards that pin the thread to the CPU (preemption disabled) for
  the guard's lifetime.

## 6. Style and lints

- `rustfmt` with a checked-in `rustfmt.toml`. `clippy` in pedantic mode for `lib/*`, and the
  default set plus a curated deny list everywhere else.
- No `as` casts between integer types in new code. Use `From`/`TryFrom`, or `lib/abi` helpers that
  make the truncation explicit.
- Public items are documented. Each module starts with a doc comment that explains its role and,
  where relevant, the FreeBSD file(s) whose behavior it reimplements (for example
  `//! Behavior reference: sys/kern/kern_umtx.c`).
- Limit generics in hot kernel paths where they bloat code. Prefer `&dyn Trait` for driver
  interfaces that are not performance-critical.

## 7. Clean implementation and licensing

See [ADR-0007](adr/0007-license-and-clean-implementation.md).

- FreeBSD source is a **behavioral reference and specification**. We read it to understand
  semantics, ABI layouts, and hardware quirks. We do **not** translate it line by line. All code is
  newly written.
- ABI facts (numbers, layouts, constant values) are interfaces and are reproduced exactly.
- Hardware programming knowledge comes from vendor specs first (UEFI, ACPI, PCIe, NVMe, AHCI, xHCI,
  VIRTIO, Arm ARM, Intel SDM, AMD APM, GIC, SBSA/BSA). FreeBSD drivers are consulted for errata and
  quirks.
- Never consult GPL-licensed sources (Linux, GPL ACPICA) when writing code.

## 8. Version control

- Trunk-based development on `main`, with short-lived branches.
- Conventional commit subjects scoped by area (`vm: …`, `arch/aarch64: …`, `fs/ufs2: …`).
- `main` must always build for **both** first-class architectures and pass the fast test tier.
  A change that builds on only one ISA is not mergeable.
