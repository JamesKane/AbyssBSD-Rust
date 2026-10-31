# Build, Run, and Test

## 1. Environments

| Role | Machine | Notes |
|---|---|---|
| Primary dev | Linux workstation (Fedora) | Rust 1.99 via rustup, `qemu-system-{x86_64,aarch64}`, edk2 firmware (`/usr/share/edk2/{ovmf,aarch64,riscv}`) |
| Build/test server | FreeBSD 15.1-RELEASE amd64, ssh alias `abyss-build` (currently 192.168.0.2; i7-12700KF, 20 threads, 128 GiB) | SSH key auth, `doas`. Provides `makefs`, `mkimg`, `newfs`, `fsck_ffs`, `kyua`, `bhyve`, real FreeBSD headers, and differential-testing reference |
| ARM64 reference board | Orange Pi 6 Plus (CIX P1), Ubuntu at `192.168.0.27`; FreeBSD 16-CURRENT on the same NVMe | Untouched until P6's exit; reimaged in P8 for AbyssBSD. Serial console on the 3.3 V debug header (UART2). Linux and FreeBSD there are the ground truth ([HARDWARE.md §4](HARDWARE.md#4-board-profile-orange-pi-6-plus-cix-p1--sky1)) |
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

For the networked bhyve cases (docs/NETWORKING.md §15), once, from a workstation:

```sh
ssh abyss-build 'doas sh -s' < tools/netlab/setup-server.sh
```

It makes `abyssnet0`, a bridge of the server's own (not `bridge0`, which joins the LAN), with
10.77.0.1/24 and fd77:77:77::1/64; IPv4 forwarding with `pf` NAT through `igc0`, so guests reach
the internet but not the LAN; dnsmasq on the bridge only (DHCPv4, DNS); and `rtadvd` advertising
fd77:77:77::/64, with IPv6 forwarding on but `pf` keeping IPv6 on the test link. Everything is in
`rc.conf`; `tools/netlab/teardown-server.sh` undoes it. Each networked guest gets a tap on the
bridge for its run.

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
cargo xtask --remote perf [--suite p2|p3|p4|p5|p6|p7|vt|diag] [--runs N] [--prepare]
                          [--freebsd-only | --abyss-only [--prof [--prof-cpu N]]]
cargo xtask --remote fetch-base [--arch <a|all>]   # base.txz, SHA-256 pinned: /rescue image, sysroot
cargo xtask --remote freebsd-shell                 # the boot-to-shell script against FreeBSD
cargo xtask --remote freebsd-multiuser             # the multi-user login script against FreeBSD
cargo xtask --remote freebsd-net [--vmm qemu|bhyve]  # the network script against FreeBSD
cargo xtask --remote netdiff [--freebsd] [--scenario NAME]  # packet captures vs FreeBSD's
cargo xtask --remote freebsd-usertests             # tests/user on FreeBSD 15.1 itself, either arch
cargo xtask --remote kyua-baseline [--resume]      # FreeBSD's kyua results, per test case, in bhyve
cargo xtask --remote kyua [--resume] [--only DIR,...]  # AbyssBSD's, compared with them
```

`--freebsd-only` records FreeBSD's numbers alone, for a suite AbyssBSD cannot run yet (`p4`
until P4.7). `--abyss-only` runs AbyssBSD alone, for investigating; with `--prof` it boots with
`abyss.prof`, the kernel's sampling profiler (`kernel/src/prof.rs`), and prints its report for
each benchmark: the functions sampled most (every CPU every 100 µs), the hottest with their
callers, and the time spent waiting for the disk, by the waiter's call chain, with each CPU's
busy samples. `--prof-cpu N` (`abyss.prof.cpu=N`) lists the functions of CPU N's samples only:
`ubench` binds itself to CPU 1, so `--prof-cpu 1` leaves out what other threads do meanwhile. Under
the profiler `ubench` runs each create/rename/unlink storm phase 30 rounds with only that phase
sampled (`kprof pause` and `kprof resume` lines). `fetch-base` keeps each architecture's `base.txz` under
`~/abyss/dist/15.1-RELEASE/<arch>/`: the archive, `root/` (`/rescue`, `/lib`, `/usr/include`,
`/usr/lib`; a sysroot for static test binaries), and `rescue.tar`. Run without `--remote` on a
workstation, it copies `rescue.tar` and a trimmed sysroot (headers, start files, static libraries)
into `build/dist/`, so clang and lld build the same programs locally. Each image's memory disk is
`rescue.tar`, an empty `/dev` for devfs, and every `tests/user/*.c`, built static and appended as
`/usr/tests/abyss/<name>` (rebuilt when its source or xtask changes); the boot tests run them. The
programs check FreeBSD's behavior, not ours, so each also passes on FreeBSD 15.1 itself
(`cc -static` on the server): a check that fails there is wrong. A source that needs a library
says so on a line `Links with: -lthr` in its header comment, and both builds pass it on.
`cargo xtask --remote freebsd-usertests [--arch A] [--test NAME,...]` runs the programs, static and
dynamic, on FreeBSD 15.1's release image itself (arm64 in QEMU with the `max` CPU, so with SVE), so
arm64's checks are held to FreeBSD too.
`freebsd-shell` boots FreeBSD single-user in bhyve and runs `xtask/src/expect.rs`'s `SHELL`
script against its `/rescue/sh`. The boot tier runs the same script against AbyssBSD: each
architecture's `<arch>-shell` case (and `x86_64-bhyve-shell` on the server) boots a normal kernel
to its boot shell on the serial console, runs it, and checks that `exit` powers the machine off.
On the server (which has `makefs`) those cases boot from an NVMe disk whose ESP holds only the
loader, with the root on a `freebsd-ufs` partition (soft updates): all of `base.txz`, with owners
and file flags from its own records, the kernel at `/boot/kernel/kernel`, an `/etc/fstab` naming
the partitions by GPT label, the memory disk's `/usr/tests`, and in `/usr/tests/static` static
`find` and `mtree` (built from `../freebsd-src`, which `--remote` mirrors in part) with the
base's specifications. The loader must load the kernel from UFS2 and the kernel mount it as
root. After `SHELL`, the `UFS_ROOT` script remounts the root read-write (`mount -u -o rw /`),
checks every entry of the base with `mtree` (owners, modes, flags, links, sizes, SHA-256), and
copies a tree with `tar` and `cp -R`. The boot shell there is base's own dynamic `/bin/sh` (the
kernel falls back to `/rescue/sh`), and the `DYNAMIC` script runs base's dynamically linked programs,
`ldd`, rtld itself, and rtld's `LD_*` variables; `freebsd-shell` runs it too, after `SHELL`.
`DYNAMIC_TESTS` then runs every `tests/user` program again, built dynamic (a PIE, against the
server's full base) into `/usr/tests/abyss-dynamic`, under `daemon -f` so that each runs as the
kernel tests run them, in a session of its own; a source that says "Built static only" is left out. The
image's entries carry no file flags but those `base.mtree` names, as an installed root (`makefs`
would otherwise copy the extracted files' own). After `exit` powers the machine off, the root must
be clean (`dumpfs`) and `fsck_ffs -f -n` must find nothing.

File systems (on the build server; VFS.md §14):

```
cargo xtask --remote fs-image --fs ufs2|msdos --out IMG [--from DIR] [--size S] [--softdep]
cargo xtask --remote fs-check --fs ufs2|msdos IMG   # fsck read-only; fails unless clean
cargo xtask --remote ufs-verify [--keep]            # lib/ufs against FreeBSD's images of base.txz
cargo xtask crash-test [--rounds N] [--arch A] [--seed S]   # soft updates against killed VMs
cargo xtask --remote pjdfstest [--groups chmod,open] [--debug group/NN.t|command]
cargo xtask --remote pjdfstest --baseline           # FreeBSD's results, on the server itself
```

`pjdfstest` boots the UFS2 root (all of `base.txz`, in bhyve on the server) and runs FreeBSD's
pjdfstest, built static from the FreeBSD tree, on `/tmp` (tmpfs) and in `/var/tmp` (UFS2 with
soft updates) at once, through `tests/pjdfstest/run.sh`. Every script's result (its plan, how
many passed, which failed) must be FreeBSD's, as `tests/pjdfstest/freebsd-<fs>.txt` records it,
or a difference `tests/pjdfstest/known-differences.txt` lists with its reason; then the root must
be left clean. `--baseline` reruns FreeBSD's side as root on the server (a new tmpfs and a
`newfs -U` memory disk) with the same program and scripts. `--debug` runs one script under
`sh -x` (or a command of its own) on both file systems, its trace in the serial log. The scripts
need tools `/rescue` lacks; the root image carries static builds of them in `/usr/tests/static`,
and a stand-in for `openssl md5` (which makes the test names) until base's own run (P6).

`crash-test` boots the shell image in QEMU with a UFS2 disk `newfs -U` made, starts a workload
of creates, writes, links, renames, `fsync`s, and removals on it from `/rescue`, and kills QEMU
at a random moment. FreeBSD's `fsck_ffs -p` (on the server, through an `md` device) must then
repair the image alone, run again until `fsck_ffs -f -n` finds nothing; the rounds list what it
repaired. Every other round boots on the crashed image as it is, so the kernel mounts an unclean
file system with soft updates, as FreeBSD does.

`lib/ufs`'s soft-updates tests replay random workloads through a cache that writes buffers back
in random order, and crash at random points. On FreeBSD (`cargo xtask --remote test --tier
unit`), `fsck_ffs -p` preens every crash image; `UFS_SOFT_SEEDS` (workloads per case) and
`UFS_SOFT_CRASHES` (one crash in that many steps, 100) run more. Elsewhere, they check each image
themselves, on `soft-test.img` from `fetch-base` (or the images `UFS_SOFT_IMG` lists).

`ufs-verify` unpacks all of `base.txz`, images it with `makefs` and (through FreeBSD's UFS, on an
`md` device) with `newfs -U` plus a sparse file. It then walks both images with `lib/ufs` and
compares every entry's mode, owner, size, modification time, contents, and link target with the
tree. `fetch-base` also makes the UFS2 test disk: `ufs-test.img` (`makefs` of `/rescue`, `/lib`,
`/usr/include`) and FreeBSD's `ls -lRnT`, file list, and `md5 -r` of it, mounted read-only. The
boot tests attach the disk read-only as vtbd2, nda1, and ada1, and the kernel test mounts each
with `/rescue/mount` and must print the same.

Planned for later phases:

```
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
| **Compat** | FreeBSD `kyua` suite inside AbyssBSD, with pass/fail tracked per test against a baseline | bhyve (arm64: the Orange Pi 6 Plus, from P8) | Nightly |
| **Differential** | Same program on AbyssBSD vs FreeBSD 15.1 (server). Compare output and `kdump` traces | Server | Nightly |
| **Performance** | Each phase's benchmark suite on AbyssBSD vs FreeBSD 15.1, same machine and configuration. Fails if any benchmark is more than 2% slower ([PERFORMANCE.md](PERFORMANCE.md)) | bhyve on the server; physical machines from P8 | Nightly, and at each phase exit |
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

After these come the shell cases (`x86_64-shell`, `aarch64-shell`, `x86_64-bhyve-shell`, above), and
`x86_64-console` and `aarch64-console`: a normal kernel with `boot_multicons=YES`, so
`/rescue/sh` is on `ttyv0`, driven over QMP by the USB keyboard alone and read back from the
screen (`screendump`, each cell matched against the console font). It also types on the virtio
keyboard, reads a USB mouse's evdev node while QMP moves it, and types `exit`, which must power
the machine off.

Last come the multi-user cases (docs/MULTIUSER.md §14), on FreeBSD hosts only (`makefs`):
`x86_64-multiuser` and `aarch64-multiuser` in QEMU, and `x86_64-bhyve-multiuser`. Each boots a
UFS2 root holding all of `base.txz` as FreeBSD's installer leaves it (`rc.conf`, time zone,
`fstab`; root with base's empty password) with `boot_multicons=YES`, and runs
`xtask/src/multiuser.rs`'s script: root logs in on `ttyv0` (typed over QMP, in QEMU) and on the
serial console, checks that `syslogd`, `cron`, and `devd` run, uses a pty, `logger`, and a cron
job, and shuts down with `shutdown -p now`; the root must be left clean. The console's `first use:`
lines outside `tests/multiuser/absent.txt` fail the case, and are written to
`build/<arch>/multiuser*-first-use.txt` either way. Until P6.9 these cases are expected to fail
(`XFAIL`), which does not fail the tier. `cargo xtask --remote freebsd-multiuser` runs the same
script against FreeBSD 15.1's release image configured the same way, on bhyve and QEMU (amd64)
and QEMU (arm64).

The network cases (docs/NETWORKING.md §16.4) follow, also on FreeBSD hosts only: `x86_64-net` and
`aarch64-net` on QEMU's user-mode network, and `x86_64-bhyve-net` on the server's test bridge.
Each boots the installed root with `bsdinstall`'s lines for a DHCP and SLAAC `vtnet0` and `sshd`,
and runs `xtask/src/net.rs`'s script: the DHCP and SLAAC addresses and default routes, `ping` and
`ping -6` to the router, DNS, `fetch` over HTTPS, `pkg install`, then `ssh` and an `scp` round trip
from the host with a key made for the run. Until P7.6 they are expected to fail (`XFAIL`).
`cargo xtask --remote freebsd-net` runs the same script against FreeBSD 15.1.

### Differential captures

What a guest puts on the wire must match what FreeBSD 15.1 puts there (docs/NETWORKING.md §16.3;
`xtask/src/netdiff.rs`, `tests/net/`). `cargo xtask --remote netdiff` boots the installed root on
bhyve with a tap on the test bridge, waits for its DHCP and SLAAC addresses, and runs each
scenario in `tests/net/scenarios/NAME.scn` while `tcpdump` captures the tap. A scenario is a
`#` description and `key: value` lines: `filter:` (the capture's filter), `guest:` (the command
on the guest's console), `peer:` (an optional listener the host starts first, such as `nc -l`),
and `ports:` (the fixed ports, kept; the others are ephemeral). The capture is decoded with
`tcpdump -n -e -t -v`, one packet per line, and normalized: addresses and MACs become roles
(`GUEST`, `GUEST6`, `GUEST-LL`, `PEER`, ...), ephemeral ports `.eph`, and IP ids, checksums,
flow labels, ICMP echo ids, TCP timestamps, and the absolute sequence and acknowledgement numbers
of SYNs and resets `_`. Each trace goes to `~/abyss/net/<os>/NAME.txt` on the server and is
compared line by line with FreeBSD's, kept in `tests/net/freebsd/`; the first differing line is
reported. `--freebsd` runs FreeBSD's release image instead (to record or check the baseline:
copy the traces into `tests/net/freebsd/`), and `--scenario NAME` runs one. Until AbyssBSD has
`vtnet0` (P7.3) its run fails at the addresses.

`cargo xtask --remote kyua-baseline` records FreeBSD 15.1's own kyua results in bhyve, where
AbyssBSD's are compared (8 CPUs, `parallelism=8`). Not under QEMU: emulated, the suite takes days
and its timing-sensitive tests measure the emulator; arm64's baseline is taken on the Orange Pi 6
Plus. The release image ships `kyua` and `/usr/tests`. An `rc.local` in a copy of the
multi-user image runs the suite one directory at a time (`bin/cat`, `sys/kern`, ...), keeping each
report on the guest's disk. A panic reboots the guest, the harness boots it again, and the
directory it was in is recorded as `crashed`. The results, one `<program>:<case>` per line with
kyua's result and reason, go to `~/abyss/kyua/` on the server and are kept in
`tests/kyua/freebsd/`. It holds perf's lock, since its eight busy vCPUs would skew a benchmark run.

`cargo xtask --remote kyua` runs the same harness on AbyssBSD: the multi-user image with all of
`tests.txz` under `/usr/tests`, in the same 8-CPU bhyve VM. AbyssBSD halts on a panic, so the
harness watches the console for one (`panic: panicked at`, `fatal trap:`, or the
`ABYSS-TEST-RESULT: FAIL` that ends every report), stops bhyve, and boots again; the cases of the
directory it was in that had not ended are recorded as `crashed`. Every 30 seconds the guest
prints its free pages and the test programs running, so a starved or hung guest shows what it was
doing. On amd64 an NMI makes the kernel print that CPU's backtrace and go on: for a hung guest,
`doas bhyvectl --vm=abyss-<pid> --inject-nmi --cpu=N` for each vCPU (`ls /dev/vmm` names the
VM; QEMU's monitor has `nmi`). `--only bin,sys/kern` runs those directories alone (each prefix matches itself and
everything under it; a test program or a case runs on its own), and `--resume` continues a run
that was stopped. The results go to `~/abyss/kyua/abyss-x86_64-bhyve.txt`, kyua's verbose
report of each directory's failures to `abyss-x86_64-bhyve/failed/`, and the comparison with
FreeBSD's to `abyss-x86_64-bhyve-compare.txt`. For each of ADR-0016's four areas (`sys/kern`,
`lib/libc`, `bin`, `usr.bin`) it counts the cases FreeBSD passes and how many of them AbyssBSD
passes, and the command fails if an area is below 95%, or if a case FreeBSD passes in one of
them fails on AbyssBSD without a line in `tests/kyua/known-failures.txt` (`<program>:<case>`, a
tab, and the reason). Such failures outside the four areas are reported but don't fail it, and
lines for cases that now pass are reported as stale. Test programs whose subject comes in a later
phase (`DEFERRED` in `xtask/src/kyua.rs`: `sys/kern/ktls_test` until P7) are counted on a line of
their own and left out of the target.

### Differential tests of command output

The same programs on AbyssBSD and on FreeBSD 15.1 must print the same thing, once what
legitimately differs between two machines is masked (`xtask/src/diff.rs`, `tests/diff/`):

- **Scenarios**: `tests/diff/scenarios/NAME.sh`, shell scripts that set up a known situation
  and print the output of the programs under test. They source `tests/diff/lib.sh`: `spawn
  NAME CMD...` starts a process in the background and names it (its pid is shown as
  `<NAME>`), `helper NAME MODE` starts `diffproc` (`tests/diff/helpers/diffproc.c`, built
  static, the same binary on both systems) in a known state (threads in known sleeps, a
  descriptor of each kind, mappings, a kqueue, a zombie, a reader on an empty pipe, the
  terminal status line, ...) and waits until it is set up, `waitfor PID WCHAN` waits for a
  sleep, and `show CMD...` runs a command and prints it, its output, and its exit status.
  `sorted FIELD HEADER_RE CMD...` does the same but sorts the lines after the header by a
  field (0: the last). Use it where the program sorts by a key the scenario leaves tied (`ps
  -u` by `%CPU`, `top` by `WCPU`): FreeBSD keeps ties in the order it found them, and that
  order changes from run to run, and between amd64 and arm64.
- **The runner**, `tests/diff/run.sh`, runs each scenario in an empty directory under
  `/var/tmp`, under `script(1)` (so it leads a session on a pseudo-terminal of its own), with a
  fixed environment, host name, umask, and login class `daemon`'s limits, and prints its
  output twice between markers: through the normalizer (`diff: | `) and as printed
  (`diff-raw: | `, for reading a mismatch).
- **The normalizer**, `tests/diff/normalize.awk`, one place for every rule, run in each guest
  with FreeBSD's own `awk`. It masks pids (a scenario's processes by name, the others by
  first appearance), thread ids, addresses, times, load averages, CPU percentages, sizes the
  kernel decides (VSZ, RSS, ...), inode numbers, CPU numbers, priorities, kernel stacks,
  reference counts, pseudo-terminal numbers, idle against asleep (`MAXSLP`), `top`'s summary
  of the other processes and memory, and the login class; it keeps structure, columns,
  flags, names, states, and wait channels. Tables are recognized by their header lines, so a
  column is masked by name.
- **FreeBSD's outputs**: `cargo xtask --remote freebsd-diff [--arch A] [--scenario N,...]`
  boots FreeBSD 15.1's release image as the installer leaves it (bhyve on amd64, QEMU on
  arm64) with the scenarios in its root and an `rc.local` that runs them, and writes each
  scenario's normalized output to `tests/corpus/freebsd/diff/ARCH/NAME.txt` in the server's
  mirror; copy them back (`scp 'abyss-build:abyss/src/tests/corpus/freebsd/diff/ARCH/*.txt'
  tests/corpus/freebsd/diff/ARCH/`) and commit them.
- **AbyssBSD's**: the shell cases on the server run every scenario from the boot shell after
  `DYNAMIC` (`diff::DIFF`) and compare each with FreeBSD's recording line for line; a
  scenario whose output differs fails the case, showing the differing lines, unless
  `tests/diff/known-differences.txt` lists it (`NAME` or `ARCH:NAME`, `#` and why). A
  scenario with no recording for the architecture is reported and skipped.

Adding a scenario is dropping its script into `tests/diff/scenarios/` and recording FreeBSD's
output with `freebsd-diff --scenario NAME`. A change to the normalizer needs FreeBSD's
outputs recorded again, as both sides normalize in the guest. `cargo xtask test --tier boot
--case x86_64-bhyve-shell --quick` runs the scenarios without the shell cases' test programs
and `mtree` checks, for a quicker look.

Every machine in the boot tier has the USB and input set ([USB.md](USB.md) §9; bhyve's
`xhci,tablet`). `--no-usb` leaves it out for a quicker run, and `--case NAME` runs one case.

While working on a feature, three environment variables shorten the loop (and pass through
`--remote`):

- `ABYSS_ATF="usr/tests/sys/kern/ptrace_test ..."`: the shell case's `run-atf` runs only those of
  FreeBSD's test directories or programs (from `image::FREEBSD_TESTS`), after making the root
  read-write, and skips the rest of the UFS2 steps. Its verdict covers only what ran.
- `ABYSS_ATF_TIMEOUT=30`: each case's limit instead of kyua's 300 seconds, so that a hang costs
  little.
- `ABYSS_REMOTE_DIR=abyss/src-other`: the server mirror to use instead of `abyss/src`, for a second
  worktree whose remote runs would otherwise overwrite the first's.

None of them is for a milestone's runs.

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

1. CI runs at milestone exits, not on every push: a full run takes about an hour, too long to
   gate each change. Pushing a tag `exit/<milestone>` (`exit/u6`, `exit/p6`) to the server's bare
   repo (`~/abyss/repo.git`) triggers `tools/ci/post-receive`, which runs
   `cargo xtask test --tier all` (unit tests, then boot tests on both arches in QEMU and bhyve)
   on the tagged commit in a persistent work tree, `~/abyss/ci-work`. Runs are serialized, and
   each one writes `~/abyss/ci/<time>-<milestone>-<rev>.log` ending in `PASS` or `FAIL`. A push
   to `main` only updates the repository. Between exits, a change runs the tests that cover it:
   `cargo xtask check`, the unit tests of the crates it touches, and the boot tier (or one arch
   of it) when it changes kernel behavior. Only CI and `perf` take the lock; `freebsd-record`
   runs beside them. To set it up locally: `git remote add build abyss-build:abyss/repo.git`;
   an exit is `git tag -a exit/u6 -m "U.6 exit" && git push build exit/u6`. To reinstall the
   hook, copy `tools/ci/post-receive` to `~/abyss/repo.git/hooks/post-receive`.
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
- A QEMU boot case that times out is paused and its log ends with every CPU's registers and its
  frame-pointer chain (`xtask: CPU#n at PC, returns to ...`), read through the monitor. Subtract
  the `slide` the loader logged and symbolize them with `llvm-addr2line -f -i -e` on the kernel
  (`target/ktest/<target>/debug/kernel`).
