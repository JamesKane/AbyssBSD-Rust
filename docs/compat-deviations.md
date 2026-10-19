# Deviations from FreeBSD

FreeBSD's names and behavior are the compatibility contract (DRIVER-MODEL.md §4.3). This file lists
where AbyssBSD differs, and why. A difference not listed here is a bug.

## Driver names for hardware FreeBSD has no driver for

| Name | Device | Why |
|---|---|---|
| `sbsa_gwdt` | Arm SBSA generic watchdog (GTDT) | FreeBSD 15.1 has no driver for it. The name follows the earlier FreeBSD bring-up branch for the Orange Pi 6 Plus, where it was first written. |

## Device tree

| What | FreeBSD | AbyssBSD | Why |
|---|---|---|---|
| arm64 `virtio_pci` on QEMU `virt` | FreeBSD would assign the unassigned legacy I/O BAR | The modern interface | Same driver and description FreeBSD 16 picks for transitional devices; no I/O BARs are assigned on arm64. |
| ACPI devices whose `_STA` says absent | Added, disabled | Not added (except `PNP0C0F` and `PNP0B00`) | `devinfo -v` shows neither; the recorded trees match. |

## System calls and process behavior

FreeBSD's behavior wins unless this table says otherwise ([COMPATIBILITY.md](COMPATIBILITY.md)
§4). Temporary entries end at the point named in the last column.

| Area | What differs | Why | Until |
|---|---|---|---|
| Unimplemented system calls | A call FreeBSD has but we do not implement yet returns `ENOSYS` without `SIGSYS`. Numbers that are not system calls at all get `SIGSYS`, as on FreeBSD | libc probes some calls (Capsicum) and copes with `ENOSYS`. A signal would kill programs that run fine on FreeBSD, for a call FreeBSD would have answered | Each call as it lands |
| Capsicum | `cap_enter`, `cap_rights_limit`, `cap_ioctls_limit`, and `cap_fcntls_limit` return `ENOSYS`, so libc runs as on a kernel without `CAPABILITY_MODE` | Enforcement needs the descriptor layer and the VFS | P6 |
| Credentials | Every process is root (uid and gid 0) | No credential model yet | P6 |
| Core dumps | A signal whose default action dumps core only terminates. `wait4` reports `CLD_KILLED` without `WCOREFLAG`, as FreeBSD does when a dump fails | No writable file system yet | P5 |
| Signal state | The mask, pending set, and alternate stack belong to the process | Processes have one thread | Threads (`thr_new`) |
| arm64 signal frames | No SVE context is written: a handler sees the V registers, and `sigreturn` restores them and clears the rest of the Z registers. A user-built SVE context is skipped | The SVE context list is not implemented | Before P6 (the Orange Pi 6 Plus has SVE2) |
| amd64 extended context | `getcontextx`'s `sysarch(AMD64_GET_XFPUSTATE)` is not implemented; signal frames do carry the full XSAVE state | Not reached by `/rescue` | When a program needs it |
| Stack | The stack is mapped whole at 8 MiB, and `RLIMIT_STACK` is 8 MiB. FreeBSD reserves `MAXSSIZ` (512 MiB on amd64, 1 GiB on arm64) and grows the stack on demand | No grow-down mappings yet | P5 |
| Writable files | The root file system (the memory disk, as tarfs) is read-only and is the only one holding files: creating, writing, or truncating a file fails with `EROFS`, as on a read-only mount. `MAP_SHARED` file mappings are private copies, which no one can tell apart while nothing is writable | No writable file system yet | P5 (tmpfs, UFS2) |
| devfs | Only `console`, `tty`, `ttyu0`, `null`, `zero`, `random`, and `urandom`; no `/dev/fd`, `/dev/stdin` and the like, no `cuau0` or `ttyu0.init`, and no disk nodes | The shell needs no more | P5 (devfs complete) |
| Advisory locks | `fcntl`'s `F_GETLK`, `F_SETLK`, and `F_SETLKW` fail with `EOPNOTSUPP`; `O_SHLOCK` and `O_EXLOCK` are ignored; `flock` returns `ENOSYS` | Locking comes with the full VFS | P5 |
| `SIGIO` | `F_GETOWN` and `F_SETOWN` fail with `EINVAL`; `O_ASYNC` is recorded but sends nothing | Nothing in the base system's shell path uses it | When a program needs it |
| `fcntl(F_KINFO)` | Fails with `EINVAL` | No `kinfo_file` yet | With `kern.proc.filedesc` (`procstat`) |
| `statfs` | `f_blocks`, `f_bfree`, `f_files`, and the I/O counters are 0 | Not tracked for the memory disk and devfs | P5 |
| Pipes | Buffers do not shrink when pipe memory runs short, and large writes are copied rather than mapped (no direct-write path); the `kern.ipc` pipe counters are absent | What a program sees is the same | P4.7 measures whether direct writes matter |
| Process descriptors | `fstat`'s times are 0 (FreeBSD reports when the process started) | Start times are not recorded yet | With `kern.proc` (P4.7, `ps`) |
| Terminals | Only the console's UART is a terminal: other UARTs attach but are not `ttyuN`. There are no pseudo-terminals (`posix_openpt` returns `ENOSYS`) | The shell on the console needs no more | ptys with sshd (P6) |
| Console line | The console UART keeps the speed, framing, and flow control firmware set: `termios`'s speeds and `c_cflag` are kept but not applied, `HUPCL` and carrier loss do nothing, and `TIOCMGET` reports DTR, RTS, CTS, DSR, and DCD | The console is shared with kernel messages | When a UART other than the console is a terminal |
| `^T` | `VSTATUS` and `TIOCSTAT` send `SIGINFO` but print no load and process line | That line needs the load average and `kern.proc` | With `kern.proc` (P4.7) |
| Terminal queues | Input holds 4096 bytes (beyond that the terminal rings and drops); output is written to the UART as it is queued, and only `^S` makes writers wait. FreeBSD sizes both from the speed and has the driver drain output | A polled console UART | When output is interrupt-driven |
| Session end | When a session leader exits, its controlling terminal is freed and its foreground group gets `SIGHUP`, but descriptors still open on the terminal keep working; FreeBSD revokes them | No vnode revocation yet | P5 (`revoke`) |
| Wide characters | Erasing a two-column UTF-8 character backs over one column | No character width tables | When a program needs it |
| Boot | The kernel starts `/rescue/sh` on the console itself, as `-sh` with `HOME=/` and `PATH=/rescue`, where FreeBSD starts init, which asks for a shell in single-user mode | The memory disk holds only `/rescue`; init needs a real root | P6 (init) |
| `kern.proc` | `ki_size`, `ki_rssize` and the other sizes, `ki_wmesg`, `ki_pctcpu`, and `ki_estcpu` are 0 or empty; `ki_runtime` and `ki_rusage.ru_utime` are the thread's run time (user and system together); every process reports `root`, class `default`. `kern.devname` is not served, so `ps` shows terminals by number | Not tracked yet | `top` (P6) |
