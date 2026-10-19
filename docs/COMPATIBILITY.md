# FreeBSD Userland Compatibility Contract

The kernel is judged by one question: **does unmodified FreeBSD userland run correctly on it?**
This document says which interfaces we must provide and how we prove that we do.

## 1. Target

| Item | Decision |
|---|---|
| Primary binary target | FreeBSD **15.x-RELEASE** `base.txz` / `kernel`-less installs for amd64 and arm64 ([ADR-0004](adr/0004-abi-target.md)) |
| Reference source | `../freebsd-src` (16.0-CURRENT, `__FreeBSD_version` 1600028 as of 2026-10-13) |
| Forward tracking | Syscalls and sysctls that 16-CURRENT libc adds are implemented as they appear. We switch the primary target to 16.x once it is released |
| Reported identity | `kern.ostype = FreeBSD`, with `kern.osrelease` / `kern.osreldate` matching the targeted userland. `kern.abyss.version` identifies us honestly |
| ABIs | Native ELF64 only: `amd64` (LP64, SysV ABI), `aarch64` (LP64, AAPCS64). Later `riscv64` (LP64D). No 32-bit compat |

## 2. Interface surfaces

The ABI is much more than the syscall table. Each surface below has an owner crate and a
compatibility test suite.

### 2.1 System calls

- `sys/kern/syscalls.master` has **606** slots (ranges expanded). **415** of them are live (not
  OBSOL, UNIMPL, RESERVED, COMPAT*, or the `lkmnosys` module slots), as counted by `syscallgen` at
  `fe90f180047b`. We implement only the live slots, and only the ones current libc
  can reach.
- **Calling convention**:
  - amd64: `syscall` instruction, number in `rax`, arguments in `rdi, rsi, rdx, r10, r8, r9`.
    Errors set CF and return errno in `rax`.
  - arm64: `svc #0`, number in `x8`, arguments in `x0..x7`. Errors set PSTATE.C and return errno in
    `x0`.
  - `syscall(2)` / `__syscall(2)` indirection is supported.
- Code generation: `tools/syscallgen` parses `syscalls.master` and generates the Rust tables, so
  slot numbers can never drift.
- Coverage tiers (see [ROADMAP.md](ROADMAP.md)):
  - **Tier A, static `/rescue`** (Phase 4): `exit`, `read`, `write`, `openat`, `close`, `fstat`,
    `fstatat`, `lseek`, `mmap`, `munmap`, `mprotect`, `sigaction`, `sigprocmask`, `sigreturn`,
    `fork`, `pdfork`, `execve`, `wait4`, `getpid`, `ioctl` (tty subset), `__sysctl`/`__sysctlbyname`,
    `issetugid`, `getrandom`, `clock_gettime`, `nanosleep`, `getdirentries`, `chdir`, `fchdir`,
    `__getcwd`, `pipe2`, `dup2`, `fcntl`, `getrlimit`, `sysarch` (amd64 TLS), `thr_self`,
    `_umtx_op` (minimal), `sigfastblock`, `minherit`, `madvise`, `kill`, `setsid`, `getuid` family.
    FreeBSD 15.1's static libc also calls `readlink` (`/etc/malloc.conf`), `exterrctl`, and
    `cpuset_getaffinity` before `main`, and `ubench` binds itself with `cpuset_setaffinity`.
  - **Tier B, dynamic multi-user boot** (Phase 6): the full process, credential, file, socket
    (UNIX), kqueue, `thr_*`, `_umtx_op`, `cap_*`, `procctl`, `jail_*`, `kenv`, `reboot`, `mount`
    family, `ptrace` (basic), `posix_openpt`, `utimensat`, `copy_file_range`, `__realpathat`, and
    related calls.
  - **Tier C, network and daemons** (Phase 7): INET/INET6 sockets, `sendfile`, `sctp_*` →
    `ENOSYS`, and `aio_*` as needed.

### 2.2 Data structure layouts

Every structure that crosses the boundary (`stat`, `statfs`, `dirent`, `sigaction`, `siginfo_t`,
`ucontext_t`/`mcontext_t`, `kevent`, `timespec`, `rusage`, `termios`, `sockaddr_*`, `kinfo_proc`,
`kinfo_file`, `xvnode`, `if_data`, `rt_msghdr`, netlink messages, and others) is defined
`#[repr(C)]` in `lib/abi` with compile-time size and offset assertions.

**`cargo xtask abi-check`** checks that those layouts are correct, rather than merely
self-consistent:

1. `lib/abi::layout::LAYOUTS` lists every mirrored struct, with sizes and offsets taken from the
   Rust definitions (`size_of`, `offset_of!`).
2. `abi-check` turns them into C11 `_Static_assert`s on `sizeof`, `_Alignof`, `offsetof`, and
   field sizes.
3. The C is compiled with `-fsyntax-only` on the build server against real FreeBSD 15.1 headers:
   natively for amd64, and with `--target=aarch64-unknown-freebsd15.1` plus an arm64 sysroot
   extracted from `base.txz` for arm64. Nothing runs, so no arm64 machine is needed. Any mismatch
   is a compile error and fails CI.

### 2.3 sysctl

libc, rtld, and the base utilities read sysctls heavily. These are required early:

- **Boot-critical**: `kern.osreldate`, `kern.ostype`, `kern.osrelease`, `kern.version`,
  `kern.arandom`, `kern.usrstack`, `kern.ps_strings`, `kern.proc.pathname`, `hw.pagesize`,
  `hw.pagesizes`, `hw.ncpu`, `hw.machine`, `hw.machine_arch`, `kern.smp.cpus`, `vm.overcommit`,
  `kern.hostname`, `kern.maxfiles`, `kern.iov_max`, `kern.ngroups`, `user.*`, `security.bsd.*`.
- **Tooling**: `kern.proc.*` (`ps`, `top`, `procstat`), `vm.stats.*`, `kern.cp_times`,
  `kern.boottime`, `kern.geom.*` (substitute subset), `net.*`, `dev.*` (device tree), and
  `kern.devname`.
- The OID numbering for fixed-number nodes matches FreeBSD. Dynamic nodes are assigned at runtime
  and discoverable through `sysctl.name2oid`, as in FreeBSD.

### 2.4 ioctl and devices

- tty/pty: `TIOCGETA`/`TIOCSETA*`, `TIOCGWINSZ`, `TIOCSCTTY`, `TIOCSPGRP`, and `posix_openpt` →
  `/dev/pts/N`.
- Console: `/dev/console`, `/dev/ttyv*` (framebuffer virtual terminals), `/dev/ttyu*` (serial),
  `vt(4)`-compatible keyboard/console ioctls used by `kbdcontrol`/`vidcontrol` (subset).
- Disk: `DIOCGMEDIASIZE`, `DIOCGSECTORSIZE`, `DIOCGIDENT`, and enough for `newfs`, `fsck_ffs`,
  `gpart show` (GEOM confxml via sysctl), and `mount`.
- Standard nodes: `/dev/null`, `zero`, `random`/`urandom`, `full`, `mem`/`kmem` (restricted), `klog`,
  `devctl`, `fd/*`, `stdin/out/err`.
- Network: `SIOC*` ioctls used by `ifconfig`, plus netlink (`NETLINK_ROUTE`, `NETLINK_GENERIC`).

### 2.5 Process image and runtime contracts

- ELF: FreeBSD brand note / `EI_OSABI`, PT_INTERP, PT_GNU_STACK, PT_GNU_RELRO, PT_TLS, and the
  `NT_FREEBSD_FEATURE_CTL` note (ASLR/W^X opt-outs).
- auxv: `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_BASE`, `AT_ENTRY`, `AT_EXECPATH`,
  `AT_CANARY`, `AT_PAGESIZES`, `AT_OSRELDATE`, `AT_NCPUS`, `AT_TIMEKEEP`, `AT_STACKPROT`,
  `AT_EHDRFLAGS`, `AT_HWCAP`/`AT_HWCAP2`, `AT_BSDFLAGS`, `AT_ARGC`/`AT_ARGV`/`AT_ENVC`/`AT_ENVV`,
  `AT_PS_STRINGS`, `AT_FXRNG`, `AT_KPRELOAD`, and `AT_USRSTACKBASE`/`LIM`.
- **Shared page**: signal trampoline, `vdso_timehands` (see `sys/kern/kern_tc.c`,
  `sys/kern/kern_sharedpage.c`), and the fxrng generation counter.
- TLS: amd64 `FSBASE` via `sysarch(AMD64_SET_FSBASE)` and `wrfsbase` when enabled. arm64
  `TPIDR_EL0`.
- Signal delivery: frame layouts exactly as in `sys/<arch>/include/{signal,ucontext,frame}.h`.
- Threads: `thr_new` and the full `_umtx_op` operation set used by `libthr` (mutexes, condvars,
  rwlocks, semaphores, robust mutexes, `UMTX_OP_SHM`).

## 3. Verification strategy

| Layer | Method |
|---|---|
| Struct layout | `cargo xtask abi-check` against real FreeBSD headers (amd64 natively, arm64 via cross-sysroot) |
| Syscall numbering | Generated from `syscalls.master`. A CI check confirms the generator ran on the pinned revision |
| Behavioral | **FreeBSD's own test suite** (`/usr/tests`, run with `kyua`) inside an AbyssBSD VM. Pass rates are tracked per directory |
| Differential | `tests/diff/`: the same program and inputs run on AbyssBSD (QEMU) and real FreeBSD (build server). stdout, exit status, and `ktrace`/`kdump` syscall traces are compared, with known-nondeterministic fields normalized |
| Boot | Scripted QEMU boots to `/rescue/sh`, then single-user, then multi-user `login:`, with serial-console expect scripts |
| Real-world | `pkg install` of a curated package set, `buildkernel` of AbyssBSD on AbyssBSD, and `portsnap`/`git` clones |

## 4. Compatibility policy

- **FreeBSD semantics win.** Where POSIX is ambiguous, we do what FreeBSD does, including errno
  choices and edge cases, unless doing so is a security defect. Each deliberate deviation is listed
  in `docs/compat-deviations.md`.
- **No silent stubs.** An unimplemented syscall, ioctl, or sysctl returns the documented
  "not supported" errno (`ENOSYS`, `ENOTTY`, `ENOENT`) and logs once with the caller's process name.
  A stub that falsely reports success requires a tracking issue.
- **Additive extensions** go under `kern.abyss.*` sysctls or new syscall numbers in a reserved
  range above FreeBSD's table. They never repurpose FreeBSD numbers.
