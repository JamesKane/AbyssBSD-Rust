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
doas pkg install rustup-init qemu edk2-qemu-x64 edk2-bhyve git gmake rsync
rustup-init -y --default-toolchain none     # toolchain comes from rust-toolchain.toml
```

The pkg `rust` (1.98 as of writing) is behind the dev workstation, so we use rustup on both hosts
to get the identical pinned toolchain. For arm64 QEMU on the server, the AArch64 edk2 firmware
ships inside the `qemu` package (`/usr/local/share/qemu/edk2-aarch64-code.fd`).

## 2. Build orchestration: `cargo xtask`

`xtask` is a workspace crate (host `std`, no external crates). It is the single entry point.
Implemented so far:

```
cargo xtask build   --arch <x86_64|aarch64|all> [--release] [--ktest]   # loader + kernel
cargo xtask image   --arch <a> [--release] [--ktest] [--cmdline "..."]  # GPT disk, FAT32 ESP -> build/<arch>/
cargo xtask run     --arch <a> [--release] [--ktest] [--cmdline "..."] [--smp N] [--mem SIZE]
                    [--gdb] [--display] [--el2] [--vmm qemu|bhyve]
cargo xtask test    [--tier unit|boot|stress|all] [--arch <a|all>] [--duration S] [--verbose]
cargo xtask check                                        # rustfmt, clippy (host, loader, kernel), policy
cargo xtask policy                                       # path-only dependencies, SPDX headers
cargo xtask syscallgen [--src ../freebsd-src] [--check]  # regenerate lib/abi syscall tables
cargo xtask abi-check  [--arch <a|all>]                  # layouts vs FreeBSD headers, on the build server
cargo xtask unsafe-report
cargo xtask --remote <command> ...                       # run any command on the build server
```

The ESP holds the loader at the removable-media path (`EFI/BOOT/BOOTX64.EFI`,
`EFI/BOOT/BOOTAA64.EFI`), the kernel at `EFI/ABYSS/KERNEL`, and the kernel command line at
`EFI/ABYSS/CMDLINE`. Names are 8.3 until `lib/fat` writes long names. `--el2` starts aarch64
firmware at EL2 (`virtualization=on`, TCG only), so the kernel takes its EL2 exit path.
`--vmm bhyve` runs amd64 under bhyve and needs a FreeBSD host (use `--remote`).

Kernel command-line words understood so far:

| Word | Effect |
|---|---|
| `kaslr=0` | The loader uses slide 0 (the default with `run --gdb`) |
| `abyss.halt=1` | Halt at the end instead of powering off, so the framebuffer stays visible |
| `abyss.qemu_exit=1` | Test kernels report their verdict as QEMU's exit status (set by `xtask test`) |
| `abyss.test.warm_reset=1` | Test kernels warm-reset once to exercise the persistent log ring |
| `abyss.stress=S` | Run the Phase 2 stress test for S seconds, then report and power off |

`--tier stress` runs the Phase 2 stress test on both architectures with 8 CPUs and 2 GiB,
for `--duration` seconds (default 600). It uses 64 threads doing lock ping-pong, ordered
locking, sx locking, allocator churn, and timed sleeps. It passes when every thread made
progress, the witness saw no violations, and heap, page, and kernel-stack accounting balance.
The tier is not part of `all`. Boot tests run with 4 CPUs.

Setting `ABYSS_QEMU_GDB=1` gives test VMs a gdb stub on `localhost:1234` without waiting
at start, so you can attach to a hung run. Test images use KASLR, so subtract the slide the
loader prints before symbolizing addresses.

Performance comparison against FreeBSD (on the build server; PERFORMANCE.md §6):

```
cargo xtask --remote perf [--suite p2|p3|p4|p5|diag] [--runs N] [--prepare] [--freebsd-only]
cargo xtask --remote fetch-base [--arch <a|all>]   # base.txz, SHA-256 pinned: /rescue image, sysroot
cargo xtask --remote freebsd-shell                 # the boot-to-shell script against FreeBSD
```

`--freebsd-only` records FreeBSD's numbers alone, for a suite AbyssBSD cannot run yet (`p4`
until P4.7). `fetch-base` keeps each architecture's `base.txz` under
`~/abyss/dist/15.1-RELEASE/<arch>/`: the archive, `root/` (`/rescue`, `/lib`, `/usr/include`,
`/usr/lib`; a sysroot for static test binaries), and `rescue.tar`. Run without `--remote` on a
workstation, it copies `rescue.tar` and a trimmed sysroot (headers, start files, static libraries)
into `build/dist/`, so clang and lld build the same programs locally. Each image's memory disk is
`rescue.tar`, an empty `/dev` for devfs, and every `tests/user/*.c`, built static and appended as
`/usr/tests/abyss/<name>` (rebuilt when its source or xtask changes); the boot tests run them. The
programs check FreeBSD's behavior, not ours, so each also passes on FreeBSD 15.1 itself
(`cc -static` on the server): a check that fails there is wrong.
`freebsd-shell` boots FreeBSD single-user in bhyve and runs `xtask/src/expect.rs`'s `SHELL`
script against its `/rescue/sh`. The boot tier runs the same script against AbyssBSD: each
architecture's `<arch>-shell` case (and `x86_64-bhyve-shell` on the server) boots a normal kernel
to `/rescue/sh` on the serial console, runs it, and checks that `exit` powers the machine off.
On the server, `<arch>-ufs-shell` does the same from a disk whose ESP holds only the loader: the
kernel and the memory disk are at `/boot/kernel/kernel` and `/boot/abyss/md` on a `freebsd-ufs`
partition that `makefs` made, and the loader must say it loaded them from UFS2.

File systems (on the build server; VFS.md §14):

```
cargo xtask --remote fs-image --fs ufs2|msdos --out IMG [--from DIR] [--size S] [--softdep]
cargo xtask --remote fs-check --fs ufs2|msdos IMG   # fsck read-only; fails unless clean
cargo xtask --remote ufs-verify [--keep]            # lib/ufs against FreeBSD's images of base.txz
```

`ufs-verify` unpacks all of `base.txz`, images it with `makefs` and (through FreeBSD's UFS, on an
`md` device) with `newfs -U` plus a sparse file. It then walks both images with `lib/ufs` and
compares every entry's mode, owner, size, modification time, contents, and link target with the
tree. `fetch-base` also makes the UFS2 test disk: `ufs-test.img` (`makefs` of `/rescue`, `/lib`,
`/usr/include`) and FreeBSD's `ls -lRnT`, file list, and `md5 -r` of it, mounted read-only. The
boot tests attach the disk read-only as vtbd2, nda1, and ada1, and the kernel test mounts each
with `/rescue/mount` and must print the same.

Planned for later phases:

```
cargo xtask image   ... [--root base.txz] [--fs ufs2|md]   # UFS2 root of the base system (P5.8)
cargo xtask deploy  --board {opi6p|x870e} [--usb <serial> | --media]   # install, set BootNext, collect logs
                    # targets are identified by disk serial number; anything else is refused
cargo xtask test    [--tier compat|full]
cargo xtask perf    [--platform x870e|opi6p] [--compare <rev>]   # PERFORMANCE.md §6
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
On arm64, `-device ramfb` gives edk2 a GOP with a linear framebuffer (virtio-gpu's GOP is
blit-only).

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
| **Performance** | Each phase's benchmark suite on AbyssBSD vs FreeBSD 15.1, same machine and configuration. Fails if any benchmark is more than 2% slower ([PERFORMANCE.md](PERFORMANCE.md)) | bhyve on the server; physical machines from P6 | Nightly, and at each phase exit |
| **Corpus** | `acpidump` DSDT/SSDT blobs from real machines through the AML interpreter. `fsck_ffs` validation of images AbyssBSD wrote | Host + server | Every commit |
| **Hardware** | Boot matrix on physical machines | Lab | Per milestone |

The boot tier currently runs these configurations, each booting the test loader and test kernel,
running both sets of in-image tests, and ending with a deliberate fatal page fault on the boot
stack's guard page. xtask checks the serial log for the decoded fault, the register dump and a
symbolized backtrace, and checks the VMM's exit status:

| Case | Platform | Also checks |
|---|---|---|
| `x86_64` | QEMU q35 + OVMF | |
| `aarch64` | QEMU virt + edk2, firmware at EL1 | |
| `aarch64-el2` | QEMU virt + edk2, firmware at EL2 | the EL2 stub and drop to EL1 |
| `aarch64-warm-reset`, `x86_64-warm-reset` | QEMU, two boots | the loader saved the first boot's log to `EFI/ABYSS/PREVLOG.TXT` |
| `x86_64-bhyve` | bhyve + `BHYVE_UEFI` (FreeBSD hosts only; skipped elsewhere) | power-off through UEFI runtime services |

### Rules

- Tests are deterministic. Time, randomness, and SMP interleavings are seeded and logged.
- Each bug fix comes with a regression test at the lowest tier that can catch it.
- The `kyua` pass rate is a ratchet: a nightly run that regresses a previously passing test fails
  the build.
- Performance is a ratchet too: once a benchmark has gated a phase, a nightly run where it is more
  than 2% slower than FreeBSD fails the build.

## 5. Continuous integration

The project starts with a **self-hosted pipeline on the build server** (no external CI service is
required):

1. A `git push build main` to the server's bare repo (`~/abyss/repo.git`) triggers
   `tools/ci/post-receive`, which runs `cargo xtask test --tier all` (unit tests, then boot tests
   on both arches in QEMU) in a persistent work tree, `~/abyss/ci-work`. Runs are serialized, and
   each one writes `~/abyss/ci/<time>-<rev>.log` ending in `PASS` or `FAIL`. To set it up locally:
   `git remote add build abyss-build:abyss/repo.git`. To reinstall the hook, copy
   `tools/ci/post-receive` to `~/abyss/repo.git/hooks/post-receive`.
2. A nightly `cron` job runs `cargo xtask test --tier full` plus fuzzing for a fixed time budget,
   then `cargo xtask perf --suite all` with nothing else running on the server.
3. Results go to `~/abyss/ci/<date>/`, with a simple HTML summary generated by `xtask`.

If the project moves to a hosted forge later, the same `xtask` entry points are used and the
pipeline definition stays thin.

## 6. Debugging

- `cargo xtask run --gdb` starts QEMU with `-s -S`, boots with `kaslr=0`, and prints a ready-to-use
  `gdb` command line with the kernel symbol file.
- The kernel mirrors its console into a persistent RAM ring. After a reset, the loader saves the
  previous boot's log to `EFI/ABYSS/PREVLOG.TXT` on the ESP (the X870E has no serial port).
- Panics print a symbolized backtrace. Frame pointers are kept in all kernel builds
  (`-C force-frame-pointers=yes`).
- Serial logs from all test runs are kept as artifacts.
