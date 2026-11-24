# Linuxulator (Linux ABI Personality)

- Status: **Draft**. A forward design for running unmodified 64-bit Linux binaries on AbyssBSD.
  Decisions here become an ADR when Linux compatibility enters the roadmap.
- Date: 2026-11-23
- Scope: the in-kernel Linux ELF64 ABI personality on `x86_64` and `aarch64` (and later `riscv64`):
  ELF branding, process/thread emulation state, syscall dispatch, VFS altroot, vDSO, signals,
  futexes, `epoll`/special descriptors, sockets/netlink, `ioctl`/device translation, and Linux
  pseudo-filesystems (`linprocfs`, `linsysfs`, `lindebugfs`).
- Inputs: [COMPATIBILITY.md](COMPATIBILITY.md), [VFS.md](VFS.md), [GRAPHICS.md](GRAPHICS.md),
  [ADR-0004](adr/0004-abi-target.md), [ADR-0005](adr/0005-monolithic-static-kernel.md),
  [ADR-0007](adr/0007-license-and-clean-implementation.md),
  [ADR-0011](adr/0011-performance-parity.md), and FreeBSD's Linuxulator as a behavioral reference
  (`../freebsd-src/sys/compat/linux/`, `sys/{amd64,arm64}/linux/`,
  `sys/compat/{linprocfs,linsysfs,lindebugfs}/`).

The short version: **an in-kernel Linux ELF64 personality modelled on FreeBSD's modern
Linuxulator, implemented in safe Rust as a thin edge-translation layer over AbyssBSD's existing
kernel subsystems.** Native FreeBSD processes pay zero overhead on the syscall, VFS, and signal
paths. Linux binaries are recognized at `execve` by ELF brand, switched to the `Linux64`
personality, given a VFS `altroot` (`/compat/linux`) and a small per-ISA Linux vDSO reading the
existing `shared_page` timehands, and translated at the syscall boundary onto the same `Process`,
`UserThread`, `sys/vfs`, `sys/umtx`, `sys/event`, `sys/socket`, `netlink`, and `sys/drm`
machinery that serves FreeBSD userland.

---

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal | Why |
|---|---|---|
| G1 | **Unmodified 64-bit Linux binaries run on both `x86_64` and `aarch64`**: static musl/glibc tools, dynamic glibc userland under `/compat/linux`, and standard Linux runtimes (Python, Go, Rust, Node, JVM, Wayland/Mesa clients) | Matches the utility of FreeBSD's `linux64` ABI across AbyssBSD's supported 64-bit ISAs |
| G2 | **Zero regression on native FreeBSD paths**: native syscall dispatch, `namei`, `fork`, and signal delivery must remain within the 2% performance-parity bound | [ADR-0011](adr/0011-performance-parity.md), [ADR-0014](adr/0014-performance-parity-per-component.md) |
| G3 | **Edge translation, not duplicated subsystems**: futexes use `sys/umtx`, `epoll` uses `sys/event` (`kqueue`), `eventfd`/`timerfd`/`inotify`/`netlink` reuse existing kernel implementations, and path redirection uses VFS `altroot` | Avoids maintaining parallel kernel engines and keeps behavioral bugs localized to the translation edge |
| G4 | **Clean-room, BSD-licensed Rust**: FreeBSD's Linuxulator and public Linux UAPI contracts serve as behavioral references; GPL kernel code is never consulted | [ADR-0007](adr/0007-license-and-clean-implementation.md) |
| G5 | **Generated syscall tables and host-verified layouts**: Linux syscall numbers and `#[repr(C)]` struct layouts are generated/checked in CI just like FreeBSD's `lib/abi` | Prevents silent ABI drift across `x86_64` and `aarch64` |
| G6 | **Compile-time selectable**: gated behind a kernel Cargo feature (`linux64`) in keeping with the monolithic static kernel model | [ADR-0005](adr/0005-monolithic-static-kernel.md) |

### 1.2 Non-goals

- **32-bit Linux (`linux32` / i386 / arm32).** Consistent with [COMPATIBILITY.md §1](COMPATIBILITY.md#1-target), AbyssBSD is 64-bit only. No 32-bit syscall tables, compat `ioctl` thunks, or `PCB_32BIT` paths.
- **LinuxKPI or Linux kernel drivers.** The Linuxulator provides the *userspace* Linux ABI only. Kernel drivers remain native Rust ([GRAPHICS.md §1.2](GRAPHICS.md#12-non-goals)).
- **Linux container isolation primitives (`CLONE_NEW*` namespaces, cgroups v2, `seccomp-bpf` full JIT, `io_uring`)** in the initial implementation. FreeBSD jails (`kernel/src/prison.rs`) provide process isolation; unsupported `clone`/`clone3` flags return `EINVAL` and log once.
- **Dynamic `.ko` modules (`linux_common.ko`, `linux64.ko`).** Until post-v1.0 Rust-native modules exist ([ADR-0005](adr/0005-monolithic-static-kernel.md)), the Linuxulator is statically linked behind the `linux64` Cargo feature and reported as built-in to `kldfind`/`modfind`.
- **Silent stubs.** Following [COMPATIBILITY.md §4](COMPATIBILITY.md#4-compatibility-policy), unimplemented syscalls return `-ENOSYS` and log once via `crate::firstuse` rather than faking success.

---

## 2. Principles

1. **Personality as a typed value, not scattered conditionals.** Every `Process` carries an
   `Abi` enum (`FreeBsd64` | `Linux64(Arc<LinuxProcData>)`)—the Rust equivalent of FreeBSD's
   `p_sysent`. ABI-specific behavior dispatches at well-defined seams (syscall entry/exit,
   `execve`, signal delivery, trap handling, coredump), never via ad-hoc checks deep inside
   `sys/vm`, `sys/vfs`, or `sys/tcp`.
2. **Native fast path stays cold-free.** Checking `proc.is_linux()` is a single relaxed load or
   enum tag check; native `FreeBsd64` processes never touch `LinuxProcData`, `LinuxThreadData`,
   or VFS `altroot` (`Option<VnodeUse>` is `None`).
3. **Pure ABI definitions live in `lib/`, kernel glue in `kernel/src/linux/`.** Struct layouts,
   flag translators, errno tables, signal maps, and generated syscall tables live in `lib/linux-abi`
   with zero `unsafe` and full host `cargo test` coverage.
4. **Reuse native kernel subroutines.** Just as FreeBSD's `sys/compat/linux/` calls `kern_*`
   functions from `<sys/syscallsubr.h>`, `kernel/src/linux/` calls shared internal helpers in
   `kernel/src/{proc,exec,fs,syscall,net,umtx,event}.rs` rather than duplicating validation,
   locking, or resource accounting.
5. **Both ISAs from day one.** `x86_64` and `aarch64` have distinct Linux syscall tables
   (`x86_64` retains legacy syscalls like `open`/`fork`/`poll`; `aarch64` uses the generic
   `asm-generic` table with `*at` and `p*` variants), distinct `stat`/`sigframe` layouts, and
   distinct register conventions. Both are built and tested together in CI.

---

## 3. Overview & Crates

```
 userland         Unmodified Linux ELF64 binaries (glibc / musl under /compat/linux)
──────────────────────────┬──────────────────────────────────────────┬──────────────────────────
 boundary                 │ syscall / svc #0 · Linux vDSO · rt_sig   │ /proc, /sys, /dev/dri/*
──────────────────────────┴──────────────────────────────────────────┴──────────────────────────
 kernel/src/linux/        personality dispatch · LinuxProcData / LinuxThreadData · clone/clone3
                          futex · epoll · signal/sigframe · socket · netlink · ioctl · vdso
──────────────────────────┬─────────────────────────┬────────────────┴──────────────────────────
 kernel/src/ (shared)     │ exec.rs (brand + auxv)  │ fs/{eventfd,inotify,shm,pseudofs}.rs
                          │ shared_page.rs          │ net/netlink.rs · prison.rs · timers.rs
──────────────────────────┴─────────────────────────┴───────────────────────────────────────────
 sys/* (native engines)   sys/vfs (+ altroot) · sys/umtx · sys/event · sys/socket · sys/drm
──────────────────────────┬─────────────────────────────────────────────────────────────────────
 lib/linux-abi            │ generated syscall tables (x86_64, aarch64) · #[repr(C)] structs
                          │ errno / signal / flag / ioctl translators (host-tested, no unsafe)
──────────────────────────┴─────────────────────────────────────────────────────────────────────
 tools/syscallgen         parses freebsd-src/sys/{amd64,arm64}/linux/syscalls.master
```

| Crate / Module | Contents | Tested | `unsafe` |
|---|---|---|---|
| `lib/linux-abi` | Generated `x86_64` and `aarch64` Linux syscall tables, `#[repr(C)]` Linux ABI structs (`stat`, `statx`, `sigaction`, `siginfo`, `ucontext`, `epoll_event`, `clone_args`, `termios`, `sockaddr`, `rusage`, etc.), bidirectional errno/signal/flag converters, `_IOC` decoder | host (`cargo test`), `abi-check` | **forbidden** |
| `tools/syscallgen` (extended) | Adds `--abi linux-x86_64` and `--abi linux-aarch64` modes parsing FreeBSD's `sys/{amd64,arm64}/linux/syscalls.master` | host, CI `--check` | forbidden |
| `sys/vfs` (extended) | `altroot: Option<VnodeUse>` lookup fallback in `namei.rs`; shared lightweight `pseudofs` tree helper | host, kernel | none added |
| `kernel/src/linux/` | Linux personality glue: syscall dispatch, `emul` state, `clone`, `futex`, `epoll`, `signal`, `file`, `mmap`, `socket`, `netlink`, `ioctl`, `vdso`, `mib` | kernel (`#[ktest]`), QEMU | minimal (uaccess/sigframe) |
| `kernel/src/fs/{linprocfs,linsysfs,lindebugfs}.rs` | Linux pseudo-filesystems over the `pseudofs` helper | kernel, QEMU | none |
| `arch/{x86_64,aarch64}` (extended) | Linux syscall register fetch/retval (negative errno, `x86_64` full-iret preservation), Linux `rt_sigframe` build/restore, embedded `linux-vdso.so.1` image | QEMU (`x86_64` + `aarch64`) | yes (arch boundary) |

---

## 4. ABI Personality & Process Lifecycle

### 4.1 `Abi` personality on `Process`

FreeBSD attaches `struct sysentvec *p_sysent` and `void *p_emuldata` to `struct proc`, and
`void *td_emuldata` to `struct thread`. In AbyssBSD, `Process` (`kernel/src/proc.rs`) and
`UserThread` (`kernel/src/uthread.rs`) hold typed enums instead of `void *`:

```rust
pub(crate) enum Abi {
    FreeBsd64,
    #[cfg(feature = "linux64")]
    Linux64(Arc<LinuxProcData>),
}

pub(crate) struct LinuxProcData {
    state: Mutex<LinuxProcState>, // persona, ptrace_flags, oom_score_adj, so_timestamp*
    flags: AtomicU32,             // one-shot warning latches (requeue, epoll, futex_pi)
    // arch-specific state (e.g., x86_64 PKRU init state)
}

pub(crate) struct LinuxThreadData {
    pub(crate) em_tid: i32,                  // == p_pid for main thread, td_tid for siblings
    pub(crate) child_set_tid: Option<u64>,   // written in schedtail on first run
    pub(crate) child_clear_tid: Option<u64>, // zeroed + futex_wake(1) on thread exit
    pub(crate) robust_list: Option<u64>,     // walked on thread exit for dead owner cleanup
}
```

### 4.2 ELF branding and `execve` transitions

`kernel/src/exec.rs` currently requires `ELFOSABI_FREEBSD`. We generalize image activation into a
small brand table checked in order:

1. **FreeBSD ELF64**: `EI_OSABI == ELFOSABI_FREEBSD` or `NT_FREEBSD_ABI_TAG` note.
2. **Linux ELF64** (when `feature = "linux64"`): `EI_OSABI == ELFOSABI_LINUX`, OR
   `EI_OSABI == ELFOSABI_NONE` with either a `GNU` `NT_GNU_ABI_TAG` note (OS = Linux) or a
   recognized `PT_INTERP` (`/lib64/ld-linux-x86-64.so.2`, `/lib/ld-linux-aarch64.so.1`,
   `/lib/ld-musl-x86_64.so.1`, `/lib/ld-musl-aarch64.so.1`).

Exec transitions mirror FreeBSD's `linux_on_exec` and `linux_common_execve`:

- **FreeBSD → Linux**: allocates `LinuxProcData` and `LinuxThreadData` (`em_tid = pid`), sets the
  process VFS `altroot` to `compat.linux.emul_path` (§6), maps the Linux vDSO (§7), populates
  Linux `AT_*` auxv entries, and caps unbounded soft `RLIMIT_NOFILE` to `1024` and `RLIMIT_STACK`
  to `8 MiB` (configurable via `compat.linux.default_{openfiles,stacksize}`, matching FreeBSD's
  `linux_emul.c` so Linux apps that iterate `0..RLIMIT_NOFILE` do not stall).
- **Linux → Linux**: resets `LinuxProcData` (`persona`, `oom_score_adj`) and `LinuxThreadData`
  (`em_tid = pid`, clears `child_*_tid` and `robust_list`), keeping `altroot` active.
- **Linux → FreeBSD**: clears VFS `altroot` and drops `LinuxProcData` / `LinuxThreadData`.

### 4.3 `fork`, `vfork`, `clone`, and `clone3`

Following FreeBSD's `linux_fork.c`, `clone` and `clone3` split cleanly into two paths:

- **Process creation (`CLONE_THREAD` clear)**: delegates to `Process::fork` with flag mapping:
  `CLONE_VM` (shared `AddressSpace`), `CLONE_FILES` (shared `FdTable`), `CLONE_FS` (shared
  cwd/root), `CLONE_SIGHAND` (shared `Signals`), `CLONE_VFORK` (parent blocks until child execs
  or exits), `CLONE_PARENT` (reparent to caller's parent), and custom translated `exit_signal`.
  Copies `LinuxProcData`, allocates `LinuxThreadData` with `em_tid = child.pid`, applies optional
  `stack` and `CLONE_SETTLS` (`FSBASE` on `x86_64`, `TPIDR_EL0` on `aarch64`), and handles
  `CLONE_PARENT_SETTID`.
- **Thread creation (`CLONE_THREAD` set)**: requires a non-null user stack and delegates to the
  native thread-creation path (`UserThread` in the same `Process`), allocating `LinuxThreadData`
  with `em_tid = new_thread.tid`, setting TLS and user stack, and returning `em_tid`.
- **Lifecycle hooks**:
  - **First run (`schedtail`)**: if `child_set_tid` is set, writes `em_tid` to user memory.
  - **Thread exit (`thread_detach`)**: walks `robust_list` (marking dead-owner futexes with
    `FUTEX_OWNER_DIED` and waking waiters), then if `child_clear_tid` is set, writes `0` to
    user memory and calls `futex_wake(addr, 1)` so `pthread_join` wakes reliably.

---

## 5. Syscall Dispatch, Calling Convention & Errno

### 5.1 Dispatch without penalizing native processes

At the top of `kernel/src/syscall/mod.rs`, dispatch branches on the current process's `Abi`:

- `Abi::FreeBsd64` runs the existing `SYSENT` table untouched.
- `Abi::Linux64(_)` delegates to `kernel/src/linux/syscall.rs`, which decodes the trap frame using
  Linux's register convention, looks up `linux_abi::syscall::SYSENT[nr]`, invokes the handler,
  and writes the result back in Linux's return convention.

### 5.2 Calling convention differences

| Aspect | FreeBSD 64-bit | Linux 64-bit |
|---|---|---|
| `x86_64` entry | `syscall`: `rax` = nr; `rdi, rsi, rdx, r10, r8, r9` | Same entry registers |
| `x86_64` return | `rax` = val (CF=0) or `+errno` (CF=1); `rcx`/`r11` clobbered | `rax` = val (`>= 0`) or **`-linux_errno`** (`-4095..-1`); CF unused; **full register restore** (`iretq` or preserving all callee/caller regs except `rcx`/`r11`) |
| `aarch64` entry | `svc #0`: `x8` = nr; `x0..x7` | `svc #0`: `x8` = nr; `x0..x5` |
| `aarch64` return | `x0` = val (PSTATE.C=0) or `+errno` (C=1); `x1` written | `x0` = val or **`-linux_errno`**; PSTATE.C unused; `x1..x7` preserved |
| `ERESTART` | Rewind PC by `syscall`/`svc` instruction length (2 / 4 bytes) | Same PC rewind; `x86_64` restores original `rax` (`orig_rax`) before retry |

### 5.3 Code generation & errno translation

- `tools/syscallgen` gains a Linux mode that reads `../freebsd-src/sys/amd64/linux/syscalls.master`
  and `../freebsd-src/sys/arm64/linux/syscalls.master` and emits `lib/linux-abi/src/syscall/{x86_64,aarch64}.rs`.
  `cargo xtask syscallgen --check` verifies all tables in CI.
- `lib/linux-abi::errno` provides `bsd_to_linux(Errno) -> i32` (returning negative Linux errno)
  and `linux_to_bsd(i32) -> Errno`, backed by `const` lookup tables with compile-time and unit
  tests asserting every live FreeBSD `Errno` maps to a non-zero Linux errno.
- Unimplemented syscalls return `Err(Errno::ENOSYS)` (translated to `-38`) and log once per
  `(process_name, syscall_name)` via `crate::firstuse`.

---

## 6. Path Resolution: VFS Altroot

Modern FreeBSD replaced per-syscall `LCONVPATH` rewriting with VFS-level `pwd_altroot`. AbyssBSD
adopts the same design directly in `sys/vfs/src/namei.rs`:

- Each `Process` gains `altroot: Option<VnodeUse>` alongside `cwd` and `root` (or `None` for all
  native FreeBSD processes).
- When `namei` resolves an **absolute path** (not `AT_FDCWD`-relative, and not bypassing via
  `AT_BENEATH` or an explicit `NO_ALTROOT` flag) and `altroot` is `Some(ar)`:
  1. Look up the path starting from `ar` (e.g., `/compat/linux`).
  2. If the lookup succeeds—or fails with an error other than `ENOENT` (e.g., `EACCES`, `ENOTDIR`,
     `ELOOP`)—return that result.
  3. On `ENOENT`, fall back to the process's normal `root` (and for `O_CREAT` / `mkdir` / `mknod`,
     follow FreeBSD's semantics: create in real root unless the parent directory already exists
     under `altroot`).
- **Symlinks**: an absolute symlink encountered while under `altroot` re-enters the same
  `altroot`-first resolution rule so `/compat/linux/lib64/ld-linux-x86-64.so.2` and symlinks to
  `/usr/lib/...` stay inside `/compat/linux` without breaking `/etc/resolv.conf` or `/tmp`
  fallback to the host root.
- **Cost for native processes**: one `if let Some(altroot) = ctx.altroot` branch on absolute
  lookups, always `None` for `FreeBsd64`.

---

## 7. vDSO & Shared Page

AbyssBSD already maintains a read-only `shared_page` (`kernel/src/shared_page.rs`) with
`abi::vdso::Timekeep` and four lockless `Timehands` slots updated by the timecounter windup.
The Linuxulator builds on this directly:

1. **Embedded `linux-vdso.so.1`**: a tiny freestanding shared ELF object per ISA (`x86_64` and
   `aarch64`) built in `arch/{x86_64,aarch64}/linux_vdso/` and included via `include_bytes!`.
   It exports the standard Linux vDSO symbols:
   - `x86_64`: `__vdso_clock_gettime`, `__vdso_gettimeofday`, `__vdso_time`, `__vdso_getcpu`,
     and the RT signal return trampoline (`linux_rt_sigcode` calling `rt_sigreturn`).
   - `aarch64`: `__kernel_clock_gettime`, `__kernel_gettimeofday`, `__kernel_clock_getres`,
     and `__kernel_rt_sigreturn`.
2. **One-time boot relocation & patching**: at kernel init, the vDSO bytes are copied into a
   read-only `vm::object::Object`, `R_*_RELATIVE` relocations are applied for its fixed/mapped
   layout, and its internal `kern_timekeep_base` pointer is patched to point at the native
   `shared_page`'s `Timekeep` offset.
3. **Exec mapping**: when `execve` activates a `Linux64` image, it maps both the native
   `shared_page` (read-only) and the Linux vDSO `Object` (read-execute) at the top of user space
   and emits `AT_SYSINFO_EHDR = vdso_base` in the Linux auxiliary vector.
4. **Legacy `x86_64` `vsyscall` page** (`0xffffffffff600000`): optional compatibility via the
   trap handler—if a `Linux64` process faults at one of the three legacy 1024-byte slots
   (`gettimeofday`, `time`, `getcpu`), the kernel pops the return address from `%rsp` and
   dispatches directly to the corresponding Linux syscall handler (matching FreeBSD's
   `linux_vsyscall`).

---

## 8. Signals & Signal Frames

Linux and FreeBSD differ in signal numbering (`SIGUSR1`, `SIGUSR2`, `SIGCHLD`, `SIGCONT`,
`SIGSTOP`, `SIGTSTP`, `SIGURG`, `SIGIO`, `SIGSYS`, and realtime signals `32..=64`), `sigset_t`
width (64-bit vs. 128-bit), `SA_*` flags, `siginfo_t` layout and `si_code` values, and signal
frame layouts.

- **Translation tables (`lib/linux-abi::signal`)**:
  - `linux_to_bsd_sig(i32) -> Option<Signal>` and `bsd_to_linux_sig(Signal) -> i32`, mapping
    standard signals `1..=31` and realtime signals `32..=64` (noting that glibc/musl reserve
    signals 32–34 for NPTL cancellation and `setxid`).
  - `linux_to_bsd_sigset` / `bsd_to_linux_sigset`, `linux_to_bsd_sigaction` /
    `bsd_to_linux_sigaction`, and `bsd_to_linux_siginfo`.
- **Delivery (`sendsig`) and `rt_sigreturn`**:
  - `kernel/src/signal/` dispatches frame construction by `proc.abi()`.
  - `x86_64`: builds Linux `rt_sigframe` (`pretcode` pointing to `sa_restorer` or vDSO
    `linux_rt_sigcode`, `l_ucontext` with `l_sigcontext`, `fpstate` / XSAVE area, and
    `l_siginfo`), passing `(linux_sig, &sf.si, &sf.uc)` in `rdi, rsi, rdx`.
  - `aarch64`: builds Linux `rt_sigframe` (`l_siginfo`, `l_ucontext` with `fpsimd_context` and
    optional `esr_context`), setting `x0..x2`, `x29`, `sp`, and `lr = __kernel_rt_sigreturn`.
  - `rt_sigreturn` validates and restores the user register context, FP/SIMD state, and signal
    mask, returning `EJUSTRETURN` so the trap exit path does not overwrite `rax`/`x0`.

---

## 9. Synchronization, Events & Special Descriptors

AbyssBSD already implements the native engines that modern FreeBSD's Linuxulator relies on:

| Linux Interface | AbyssBSD Backing Engine | Notes |
|---|---|---|
| **`futex`** (`WAIT`, `WAKE`, `WAIT_BITSET`, `WAKE_BITSET`, `REQUEUE`, `CMP_REQUEUE`, `WAKE_OP`, `LOCK_PI`, `UNLOCK_PI`, `TRYLOCK_PI`, `set_robust_list`, `get_robust_list`) | `sys/umtx` (`wait.rs`, `pi.rs`, `robust.rs`) | Uses the same `UmtxKey` (private process key vs. shared VM-object+offset key) and wait queues as FreeBSD `_umtx_op`. `FUTEX_CLOCK_REALTIME` vs. `CLOCK_MONOTONIC` maps onto `sys/umtx` timeout clocks. |
| **`epoll`** (`epoll_create1`, `epoll_ctl`, `epoll_wait`, `epoll_pwait`, `epoll_pwait2`) | `sys/event` (`kqueue`) | An epoll fd is a `kqueue` file descriptor. `EPOLLIN`/`EPOLLOUT` map to `EVFILT_READ`/`EVFILT_WRITE`; `EPOLLET` → `EV_CLEAR`; `EPOLLONESHOT` → `EV_DISPATCH`; `epoll_event.data` rides in `kevent.udata` / `ext[0]`. Note: `epoll_event` is `#[repr(C, packed)]` on `x86_64` and naturally aligned on `aarch64`. |
| **`eventfd` / `eventfd2`** | `kernel/src/fs/eventfd.rs` | Translate `EFD_CLOEXEC`, `EFD_NONBLOCK`, `EFD_SEMAPHORE` and delegate directly. |
| **`timerfd_*`** | `kernel/src/timers.rs` | Translate clock IDs (`CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME`) and flags (`TFD_CLOEXEC`, `TFD_NONBLOCK`, `TFD_TIMER_ABSTIME`). |
| **`signalfd` / `signalfd4`** | `File` over `sys/event` (`EVFILT_SIGNAL`) or dedicated `SignalFd` | Translates `l_sigset_t` and formats `struct signalfd_siginfo` on `read()`. |
| **`inotify_*`** | `kernel/src/fs/inotify.rs` & `sys/vfs/src/inotify.rs` | Translate `IN_*` flags/masks directly onto the existing native inotify engine. |
| **`memfd_create` / `/dev/shm`** | `kernel/src/fs/shm.rs` | Anonymous `SHM_ANON` VM objects with Linux seal flags (`F_ADD_SEALS`/`F_GET_SEALS`) and `/dev/shm` tmpfs mount. |
| **`pidfd_open` / `pidfd_send_signal`** | `ProcDesc` (`kernel/src/proc.rs`) | Reuses FreeBSD process descriptors (`pdfork`/`pdkill`) with Linux flag translation. |

---

## 10. Networking, Netlink & Ioctl / Device Translation

### 10.1 Sockets & Netlink

- **Sockets (`kernel/src/linux/socket.rs`)**: translates address families (`AF_INET6` is 10 on
  Linux vs. 28 on FreeBSD; `AF_NETLINK` is 16 vs. 38), `SOCK_*` flags (`SOCK_NONBLOCK`,
  `SOCK_CLOEXEC`), `MSG_*` flags, `SOL_*` / `SO_*` / `IP_*` / `IPV6_*` / `TCP_*` socket options,
  and `cmsghdr` ancillary messages (`SCM_RIGHTS`, `SCM_CREDENTIALS` ↔ `SCM_CREDS2`,
  `SO_TIMESTAMP*`), then delegates to `kernel/src/syscall/socket.rs` and `sys/socket`.
- **Netlink (`kernel/src/linux/netlink.rs`)**: AbyssBSD already has a native netlink subsystem
  (`kernel/src/net/netlink.rs`, `lib/abi/src/netlink.rs`). Linux `AF_NETLINK` sockets attach to
  the same netlink engine with a message-translation hook that rewrites `rtnetlink` address
  families (`rtmsg`, `ifaddrmsg`, `ndmsg`, `ifinfomsg`), interface flags, and `NLMSG_ERROR`
  errno values between BSD and Linux encodings (mirroring FreeBSD's `compat/linux/linux_netlink.c`).

### 10.2 Ioctls & Device Numbering

Linux's `_IOC(dir, type, nr, size)` bit layout differs from FreeBSD's `<sys/ioccom.h>`
([GRAPHICS.md §2](GRAPHICS.md#2-the-abi-decision)). `kernel/src/linux/ioctl.rs` decodes the
Linux request number and dispatches by command range through a typed handler table:

- **TTY / PTY (`0x5400..=0x54FF`)**: `TCGETS`/`TCSETS*` (`termios` ↔ `l_termios`, preserving
  `VSTATUS` when `compat.linux.preserve_vstatus = 1`), `TIOCGWINSZ`, `TIOCGPGRP`/`TIOCSPGRP`,
  `TIOCGPTN`, `TIOCSPTLCK`, `FIONREAD`, `FIONBIO`.
- **Sockets (`0x8900..=0x89FF`)**: `SIOCGIFCONF`, `SIOCGIFFLAGS`, `SIOCGIFADDR`, `SIOCGIFHWADDR`,
  `SIOCGIFINDEX`, `SIOCGIFMTU` (translating FreeBSD interface names like `vtnet0`/`rge0` to
  Linux-friendly names if configured, or passing through as FreeBSD does).
- **DRM (`/dev/dri/cardN`, `/dev/dri/renderDN`, type `'d'` = `0x64`)**: translates Linux `_IOC`
  direction/size encoding to FreeBSD's `_IOC` encoding and forwards directly into `sys/drm`—the
  DRM command numbers and payload structs are already identical ([GRAPHICS.md §2](GRAPHICS.md#2-the-abi-decision)).
- **Evdev (`/dev/input/eventN`, type `'E'` = `0x45`)**: forwards into `sys/evdev` with `_IOC`
  translation.
- **Device major/minor synthesis**: Linux userspace (`glibc` `isatty`/`ptsname`, `libdrm`, Mesa)
  inspects `st_rdev` major/minor numbers on `stat`/`fstat`. `linux_stats` synthesizes Linux
  major/minor numbers for known devfs paths (`pts/N` → major `136`, minor `N`; `dri/cardN` /
  `dri/renderDN` → major `226`, minor `N`; `null` → `1:3`; `zero` → `1:5`; `random`/`urandom` →
  `1:8`/`1:9`; `tty` → `5:0`; `ptmx` → `5:2`).

---

## 11. Pseudo-filesystems & Jails

Linux programs—especially glibc, Go, Java, Python, `libdrm`, and dynamic linkers—rely heavily on
`/proc` and `/sys`:

- **Shared `pseudofs` helper**: a small read-only virtual-tree helper over `sys/vfs::VnodeOps`
  (analogous to FreeBSD's `pseudofs(9)`) supporting static directories, dynamic per-PID
  directories, symlinks (like `/proc/self` and `/proc/self/exe`), and `sbuf`/string-backed
  generated files.
- **`linprocfs`** (mounted at `/compat/linux/proc`):
  - System nodes: `cpuinfo`, `meminfo`, `uptime`, `loadavg`, `stat`, `version`, `mounts`,
    `filesystems`, `cmdline`, `sys/kernel/{osrelease,ostype,pid_max,random/uuid}`.
  - Per-process nodes (`/proc/<pid>/` and `/proc/self/`): `exe` (symlink to executable path),
    `cmdline`, `environ`, `status`, `stat`, `statm`, `maps`, `auxv`, `limits`, `cwd`, `root`,
    `fd/` (symlinks to open files), `fdinfo/`, `task/`, `oom_score_adj`, `mountinfo`.
- **`linsysfs`** (mounted at `/compat/linux/sys`):
  - `devices/system/cpu/{online,possible,present,cpuN/topology/*}`
  - `class/drm/cardN/device/{vendor,device,subsystem_vendor,subsystem_device,uevent}` (enabling
    Linux Mesa / Vulkan ICD device discovery)
  - `class/net/<if>/...` and `bus/pci/devices/...`
- **`lindebugfs`** (optional, mounted at `/compat/linux/sys/kernel/debug`): minimal stub tree
  where tools expect the mount point.
- **Per-jail Linux metadata (`kernel/src/prison.rs`)**: each jail carries a `LinuxPrisonData`
  (`osname = "Linux"`, `osrelease = "6.x.y"`, `oss_version`), exposed through `compat.linux.*`
  sysctls and returned by `uname(2)` and `/proc/sys/kernel/osrelease`.

---

## 12. Verification, Testing & Performance

| Layer | Method |
|---|---|
| **Struct layouts** | `cargo xtask abi-check` extended to verify `lib/linux-abi` `#[repr(C)]` layouts against Linux UAPI / FreeBSD `compat/linux` headers for both `x86_64` and `aarch64` |
| **Syscall tables & translators** | Host `cargo test` in `lib/linux-abi` (bijective errno checks, signal translation, `_IOC` round-trips, flag converters) and `cargo xtask syscallgen --check` |
| **VFS `altroot` & `pseudofs`** | Host unit tests in `sys/vfs` covering `altroot` hits, `ENOENT` fallbacks, symlink crossings, `O_CREAT`, and native `altroot = None` behavior |
| **Kernel unit tests (`#[ktest]`)** | In-kernel tests for `LinuxProcData`/`LinuxThreadData` lifecycle, `futex` wait/wake/requeue/PI over `sys/umtx`, `epoll` over `kqueue`, and vDSO relocation |
| **Differential & real workloads** | Static and dynamic Linux test binaries (musl + glibc, LTP subsets, Go/Rust/Python test scripts) run in QEMU on both `x86_64` and `aarch64`, compared against FreeBSD 15.1's Linuxulator on the build server |
| **Performance parity** | Re-run the native P4/P5/P7 benchmark suites on bhyve with `feature = "linux64"` enabled to confirm **≤ 2% (target 0%)** impact on native FreeBSD processes ([ADR-0011](adr/0011-performance-parity.md)) |

---

## 13. Phasing & Open Questions

### 13.1 Proposed rollout

| Stage | Milestone | Exit criterion |
|---|---|---|
| **L1** | Personality seam, `lib/linux-abi` + `syscallgen`, ELF64 brand, `altroot`, minimal syscalls (`write`, `exit_group`, `brk`, `mmap`, `arch_prctl`/`set_tid_address`, `clock_gettime`, `uname`) | Unmodified static Linux `x86_64` and `aarch64` musl `busybox` / hello-world runs in QEMU |
| **L2** | Dynamic loader (`ld-linux-*.so.*`), vDSO (`clock_gettime` + signal trampoline), `clone`/`clone3` threads, `futex` over `sys/umtx`, signals & `rt_sigframe` | Dynamic glibc shell + multi-threaded pthread/futex/signal stress suite passes on both ISAs |
| **L3** | `epoll`, `eventfd`/`timerfd`/`signalfd`/`inotify`, sockets + `rtnetlink`, TTY/PTY ioctls, `linprocfs` + `linsysfs` | Unmodified Linux `curl`, `python3`, and a Go HTTP server run under `/compat/linux` in QEMU |
| **L4** | DRM & evdev ioctl translation, `linsysfs` `/sys/class/drm`, `dma-buf` / `memfd` / Wayland UNIX socket interop | Linux Wayland/Mesa client (`kmscube` / `vkcube` / `foot`) renders on native AbyssBSD compositor |

### 13.2 Open questions (to settle in the ADR)

1. **Kernel feature default**: should `linux64` be enabled by default in desktop/general kernel
   profiles once L2 passes, or opt-in per platform crate?
2. **Linux kernel version reported (`compat.linux.osrelease`)**: glibc 2.38+ and Go/Rust runtimes
   gate modern syscalls (`clone3`, `statx`, `rseq`, `faccessat2`, `openat2`) on `uname().release`.
   Targeting `6.6.0` (LTS) matches modern distros while keeping the required syscall surface
   bounded.
3. **`rseq` (restartable sequences)**: glibc 2.35+ invokes `rseq(2)` at thread startup and
   gracefully falls back when it returns `-ENOSYS`. Decide whether L2 keeps `rseq` returning
   `-ENOSYS` (as FreeBSD did before `linux_rseq.c`) or implements minimal registration immediately.
4. **Interface naming in Linux sockets/netlink**: expose native FreeBSD interface names (`vtnet0`,
   `rge0`) directly (FreeBSD's default) vs. optional `ethN` aliasing.
5. **vDSO build integration**: whether `linux-vdso.so.1` is compiled by `xtask` / `build.rs`
   from Rust + minimal assembly using the existing pinned toolchain (avoiding any external C
   cross-compiler dependency, per [ADR-0002](adr/0002-dependency-policy.md) and
   [ADR-0003](adr/0003-stable-rust-builtin-targets.md)).
