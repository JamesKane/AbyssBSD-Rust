# Build, Run, and Test

## 1. Environments

| Role | Machine | Notes |
|---|---|---|
| Primary dev | Linux workstation (Fedora) | Rust 1.99 via rustup, `qemu-system-{x86_64,aarch64}`, edk2 firmware (`/usr/share/edk2/{ovmf,aarch64,riscv}`) |
| Build/test server | FreeBSD 15.1-RELEASE amd64, ssh alias `abyss-build` (currently 192.168.0.2; i7-12700KF, 20 threads, 128 GiB) | SSH key auth, `doas`. Provides `makefs`, `mkimg`, `newfs`, `fsck_ffs`, `kyua`, `bhyve`, real FreeBSD headers, and differential-testing reference |
| ARM64 reference board | Orange Pi 6 Plus (CIX P1), Ubuntu at `192.168.0.27`; FreeBSD 16-CURRENT on the same NVMe | Untouched until P6, when it is reimaged for AbyssBSD. Serial console on the 3.3 V debug header (UART2). Linux and FreeBSD there are the ground truth ([HARDWARE.md §4](HARDWARE.md#4-board-profile-orange-pi-6-plus-cix-p1--sky1)) |
| AMD64 reference machine | Ryzen 9 7900X / X870E AORUS ELITE WIFI7, Fedora 44 at `192.168.0.30` | AbyssBSD lives only on the **second NVMe** (own ESP, own boot entry, booted once via `efibootmgr -n`). No serial port, so logs come from the framebuffer and the persistent log the loader saves to the ESP ([HARDWARE.md §5](HARDWARE.md#5-board-profile-amd64-ryzen-9-7900x--x870e-aorus-elite-wifi7)) |

### Build server setup (one time)

On each workstation, add an ssh alias so tooling never hard-codes the address:

```
# ~/.ssh/config
Host abyss-build
    HostName 192.168.0.2
    User jkane
    ServerAliveInterval 30
```

On the server:

```sh
doas pkg install rustup-init qemu edk2-qemu-x64 edk2-bhyve git gmake
rustup-init -y --default-toolchain none     # toolchain comes from rust-toolchain.toml
```

The pkg `rust` (1.98 as of writing) is behind the dev workstation, so we use rustup on both hosts
to get the identical pinned toolchain. For arm64 QEMU on the server, the AArch64 edk2 firmware
ships inside the `qemu` package (`/usr/local/share/qemu/edk2-aarch64-code.fd`).

## 2. Build orchestration: `cargo xtask`

`xtask` is a workspace crate (host `std`, no external crates). It is the single entry point.
Implemented in Phase 0:

```
cargo xtask build   --arch <x86_64|aarch64|all> [--release] [--ktest]
cargo xtask image   --arch <a> [--release] [--ktest]     # GPT disk with a FAT32 ESP -> build/<arch>/
cargo xtask run     --arch <a> [--release] [--ktest] [--smp N] [--mem SIZE] [--gdb] [--display]
cargo xtask test    [--tier unit|boot|all] [--arch <a|all>] [--verbose]
cargo xtask check                                        # rustfmt, clippy (host + UEFI targets), policy
cargo xtask policy                                       # path-only dependencies, SPDX headers
cargo xtask syscallgen [--src ../freebsd-src] [--check]  # regenerate lib/abi syscall tables
cargo xtask abi-check  [--arch <a|all>]                  # layouts vs FreeBSD headers, on the build server
cargo xtask unsafe-report
cargo xtask --remote <command> ...                       # run any command on the build server
```

Planned for later phases:

```
cargo xtask image   ... [--root base.txz] [--fs ufs2|md]   # UFS2 root (P4/P5)
cargo xtask run     ... [--vmm bhyve]                       # bhyve on the build server (P1)
cargo xtask deploy  --board {opi6p|x870e} [--usb <serial> | --media]   # install, set BootNext, collect logs
                    # targets are identified by disk serial number; anything else is refused
cargo xtask test    [--tier compat|full]
cargo xtask fetch-base --version 15.1 --arch <a>   # download & verify base.txz (sha256 pinned)
```

- `--remote` mirrors the working tree to the build server with rsync (`~/abyss/src`) and runs
  `cargo xtask` there over `ssh`. The server is `$ABYSS_BUILD_HOST` (default: the ssh alias `abyss-build`). Building UFS2 images (`makefs -t ffs -o version=2`),
  running `fsck_ffs` on images, `abi-check`, and bhyve runs default to remote.
- Until our own `newfs`/`makefs` equivalents exist in Rust, UFS2 root images are built on the server
  from `base.txz` plus our kernel. FAT32 ESPs are built locally by our own `lib/fat` writer, which
  dogfoods that crate.

## 3. Virtual platforms

| Platform | Command shape | Purpose |
|---|---|---|
| QEMU q35 + OVMF | `-machine q35 -cpu max -smp 4 -m 4G -drive if=pflash,...OVMF_CODE.fd` | Primary amd64 dev loop |
| QEMU virt + AAVMF | `-machine virt,gic-version=3,acpi=on -cpu max -smp 4 -m 4G -bios QEMU_EFI.fd` | Primary arm64 dev loop (ACPI on, not DT) |
| QEMU virt (RV64) | `-machine virt,acpi=on,aia=aplic-imsic -bios RISCV_VIRT_CODE.fd` | Day 2 |
| bhyve (UEFI) | `bhyve -c 4 -m 4G -l bootrom,/usr/local/share/uefi-firmware/BHYVE_UEFI.fd …` on the server | Second amd64 hypervisor: different virtio/ACPI quirks |

Devices used in CI configurations: virtio-blk, virtio-net, virtio-rng, NVMe (`-device nvme`), AHCI
(`-device ahci` + `ide-hd`), xHCI + usb-kbd/usb-storage, and `-device isa-debug-exit` (amd64) or
semihosting / PSCI `SYSTEM_OFF` (arm64) so tests can report pass/fail through the exit code.

The serial console is wired to stdio or a socket for expect-style test drivers written in Rust
inside `tests/`.

## 4. Test pyramid

| Tier | What | Where | When |
|---|---|---|---|
| **Unit** | Pure crates (`lib/*`): parsers, AML, collections, on-disk formats, TCP state machine, allocator algorithms | Host `cargo test`, plus Miri for `unsafe` crates | Every commit |
| **Property / fuzz** | ELF, GPT, FAT, UFS2, AML, ACPI tables, packet parsers. In-house harness: deterministic PRNG, coverage-free mutation, minimization | Host | Nightly on the server (20 threads) |
| **Kernel unit** | `#[ktest]` functions compiled into a test kernel, run on boot, results over serial | QEMU, both arches | Every commit |
| **Boot** | Loader → kernel → `/rescue/sh` → single-user → multi-user `login:` with expect scripts | QEMU both arches, bhyve | Every commit (fast subset), nightly (full) |
| **Compat** | FreeBSD `kyua` suite inside AbyssBSD, with pass/fail tracked per test against a baseline | QEMU + bhyve | Nightly |
| **Differential** | Same program on AbyssBSD vs FreeBSD 15.1 (server). Compare output and `kdump` traces | Server | Nightly |
| **Corpus** | `acpidump` DSDT/SSDT blobs from real machines through the AML interpreter. `fsck_ffs` validation of images AbyssBSD wrote | Host + server | Every commit |
| **Hardware** | Boot matrix on physical machines | Lab | Per milestone |

### Rules

- Tests are deterministic. Time, randomness, and SMP interleavings are seeded and logged.
- Each bug fix comes with a regression test at the lowest tier that can catch it.
- The `kyua` pass rate is a ratchet: a nightly run that regresses a previously passing test fails
  the build.

## 5. Continuous integration

The project starts with a **self-hosted pipeline on the build server** (no external CI service is
required):

1. A `git push build main` to the server's bare repo (`~/abyss/repo.git`) triggers
   `tools/ci/post-receive`, which runs `cargo xtask test --tier all` (unit tests, then boot tests
   on both arches in QEMU) in a persistent work tree, `~/abyss/ci-work`. Runs are serialized, and
   each one writes `~/abyss/ci/<time>-<rev>.log` ending in `PASS` or `FAIL`. To set it up locally:
   `git remote add build abyss-build:abyss/repo.git`. To reinstall the hook, copy
   `tools/ci/post-receive` to `~/abyss/repo.git/hooks/post-receive`.
2. A nightly `cron` job runs `cargo xtask test --tier full` plus fuzzing for a fixed time budget.
3. Results go to `~/abyss/ci/<date>/`, with a simple HTML summary generated by `xtask`.

If the project moves to a hosted forge later, the same `xtask` entry points are used and the
pipeline definition stays thin.

## 6. Debugging

- `cargo xtask run --gdb` starts QEMU with `-s -S` and prints a ready-to-use `gdb`/`lldb` command
  line with the kernel symbol file and the KASLR slide.
- Panics print a symbolized backtrace. Frame pointers are kept in all kernel builds
  (`-C force-frame-pointers=yes`).
- Serial logs from all test runs are kept as artifacts.
