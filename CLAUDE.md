# AbyssBSD: working notes for Claude

A from-scratch Rust reimplementation of modern 64-bit FreeBSD kernel subsystems (UEFI + ACPI only)
that runs unmodified FreeBSD userland. Read `docs/ROADMAP.md` for the current phase, and
`docs/ENGINEERING.md` before writing code. Development is **QEMU first** through P5 (ADR-0010).

## Hard rules

- **No external crates.** Kernel/loader: `core` + `alloc` only. Host tools/tests: `std` allowed.
  Every Cargo dependency must be a `path =` inside the workspace (ADR-0002).
- Pinned **stable** Rust, edition 2024, built-in targets only. No nightly, no `-Zbuild-std` (ADR-0003).
- Every change must build for **both** `x86_64` and `aarch64`.
- Algebraic types: enums for states, newtypes for addresses/IDs/units, typestate for lifecycles,
  `Result<_, Errno>` on syscall paths, no panics on untrusted input.
- Every `unsafe` block has a `// SAFETY:` comment. Inline asm only in `arch/*`.
- **Don't touch the physical machines** (no ssh, no captures, no boots) until FreeBSD userland
  runs end to end in QEMU (P6, ADR-0010).
- FreeBSD source is a behavioral **reference**, not something to transliterate. Never consult GPL
  code.

## Environment

- FreeBSD reference source: `../freebsd-src` (16.0-CURRENT). The ABI target is FreeBSD 15.x
  userland (ADR-0004).
- Build/test server: `ssh abyss-build` (alias in ~/.ssh/config, currently 192.168.0.2; FreeBSD 15.1 amd64, 20 threads, 128 GiB, `doas`). Used
  for UFS2 images (`makefs`), `fsck_ffs`, `abi-check`, bhyve, kyua, and differential tests.
- Local: Rust 1.99, `qemu-system-{x86_64,aarch64}`, edk2 firmware under `/usr/share/edk2/`.
- ARM64 reference board: **Orange Pi 6 Plus** (CIX P1/Sky1), Ubuntu at `192.168.0.27` (key login;
  root steps go through a script the user runs). FreeBSD 16-CURRENT is on the same NVMe. Board
  facts: `docs/HARDWARE.md` §4. Prior bring-up notes are in the sibling project
  `../AbyssBSD/docs/boards/orangepi-6-plus/`, plus `../AbyssBSD/docs/boards/radxa-dragon-q8b/lessons.md`.
  - **Read before experimenting on the board.** Hangs cost power cycles.
  - Until it is reimaged for AbyssBSD (planned at P6), don't write to its NVMe; boot from USB.
    Capture its ground truth into `tests/corpus/opi6p/` before reimaging.
  - "Nothing special": no board-specific config knobs. A board setting means a driver bug.
- AMD64 reference machine: **Ryzen 9 7900X / Gigabyte X870E AORUS ELITE WIFI7**, Fedora 44 at
  `192.168.0.30` (key login; `sudo` needs the user's password, so root steps go through a script
  the user runs). Board facts: `docs/HARDWARE.md` §5.
  - AbyssBSD may write **only to the second NVMe** (the media disk), identified by serial number.
    The Samsung 980 PRO is Fedora's system disk. The WD 8 TB ×2 (`md0`) and HGST 6 TB (`md1`) hold
    user data. Never touch those disks, the Fedora ESP, or Fedora's boot entry.
  - There is no serial console. Use the framebuffer, the persistent log the loader saves to the
    ESP, and the watchdog.
- Entry point for everything: `cargo xtask …` (see `docs/BUILD-AND-TEST.md`).

## Common commands

```sh
cargo xtask test --tier unit          # host unit tests (bare `cargo test` does the same)
cargo xtask test --tier boot          # boot the ktest loader in QEMU, both arches
cargo xtask run --arch aarch64        # interactive QEMU (Ctrl-A X quits)
cargo xtask check                     # fmt, clippy for host and UEFI targets, policy
cargo xtask syscallgen --check        # generated syscall table matches ../freebsd-src
cargo xtask abi-check                 # lib/abi layouts vs FreeBSD headers (on the build server)
cargo xtask --remote test --tier unit # anything, on the build server
```

`lib/abi/src/syscall/generated.rs` is generated; edit `tools/syscallgen` instead.
