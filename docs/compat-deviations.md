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
| Credentials | Each process has FreeBSD's credential (real, effective, and saved ids, and supplementary groups), and the set-id calls follow FreeBSD's rules, with the superuser as euid 0. Running a setuid or setgid program does not change ids yet, and there is no privilege framework (`priv_check`), jails, or securelevel | Set-id `exec`, jails, and securelevel come with the rest of multi-user support | P6 |
| Core dumps | A signal whose default action dumps core only terminates. `wait4` reports `CLD_KILLED` without `WCOREFLAG`, as FreeBSD does when a dump fails | No writable file system yet | P5 |
| Consoles without `RB_MULTIPLE` | Kernel messages go to the serial port and `vt` window 0 whatever `boothowto` says; FreeBSD writes them to the best console alone unless `boot_multicons` is set. `/dev/console` follows FreeBSD | The serial port is the test harness's log on every machine | P6 (hardware bring-up) |
| arm64 pointer authentication | Not enabled for user mode: the ID registers user mode reads hide it (`API`, `APA`, `GPA`, `GPI`, `APA3`) and `AT_HWCAP` has no `HWCAP_PACA`, as on FreeBSD with pointer authentication disabled; `PAC` instructions run as no-ops | The kernel sets no keys | When a program needs it |
| UFS2 journal | Journaled soft updates (`FS_SUJ`: `newfs -j`, `tunefs -j enable`) are not kept: a read-write mount writes with soft updates alone and clears the flag, so FreeBSD's `fsck_ffs` checks the file system in full instead of replaying a journal that would be stale. An unclean journaled file system mounts read-write only when forced, as on FreeBSD | No journal yet | SU+J |
| `nmount` | `tmpfs` (options `mode`, `uid`, `gid`, `ro`), `ufs` (`ro`, `force`), and `msdosfs` (`uid`, `gid`, `mask`, `dirmask`, `shortnames`, `ro`); other options are ignored, and other types fail with `ENODEV`, as an unknown one does on FreeBSD. `update` changes only read-only and read-write, and only for `ufs` (tmpfs and msdosfs answer `EOPNOTSUPP`); going read-only does not wait for files open for writing (FreeBSD: `EBUSY` unless forced), whose later writes fail with `EROFS` | Each type arrives with its milestone; no count of writers per vnode yet | — |
| msdosfs | No `kiconv` (`-L`, `-D`, `-W`): names are ISO 8859-1 bytes, as FreeBSD's msdosfs without them. A file renamed while in use keeps its old `st_ino` until its vnode goes (FreeBSD's changes to the new entry's place at once); a file made where a renamed in-use file's number points gets another. The ESP is mounted on `/boot/efi` by the kernel at boot (FreeBSD: by `fstab`), from the root's disk, or else the first disk with one | No locale conversion tables; numbers follow vnodes here, entries there; no `init` or `fstab` yet | P6 (`init`, `fstab`) |
| `rmdir("a/b/..")` | Always `EINVAL`; FreeBSD answers `EINVAL` or `ENOTEMPTY`, apparently by which lookup path ran | `vfs_lookup`'s rule for `..` as the last component of a removal | Permanent |
| devfs | No `/dev/fd`, `/dev/stdin` and the like, no `cuau0` or `ttyu0.init`, no `devctl`/`pci` nodes, no rules (`devfs.rules`); removing a device node deletes it (FreeBSD hides it). Disks have no serial number for `DIOCGIDENT`. A partition's `gpt/` and `gptid/` names stay while it is open by another name (FreeBSD's `glabel` withdraws them then) | Not needed yet | When a program needs them |
| UNIX-domain sockets | A peek (`MSG_PEEK`) at a stream or seqpacket socket returns no control messages (FreeBSD's copies them); a socket's `fcntl(F_SETOWN)`, `SIGIO`, and `SO_ACCEPTFILTER` are not there; `connectat` takes a path only; `AF_INET` and `AF_INET6` fail with `EAFNOSUPPORT`, as without `INET` | Nothing in base needs them yet; IP is P8 | When a program needs them; P8 |
| md(4) | `mdconfig` attaches malloc and swap disks (both kept in kernel memory, allocated as written); vnode, preload, and null disks fail with `EOPNOTSUPP`, and `-l` lists nothing. Detaching a disk that is mounted is `EBUSY` unless forced, but one only open by its device node detaches | No swap; vnode-backed disks are not needed yet | When a program needs them |
| `rfork` | `RFSPAWN`, `RFPROC \| RFFDG`, `RFPROC \| RFFDG \| RFMEM \| RFPPWAIT` (what `posix_spawn`, `fork`, and `vfork` are), and `RFPROC` (a shared descriptor table) work; other flag sets (shared memory without waiting, changes to the caller) are `EINVAL`. A process sharing its descriptor table keeps sharing it across `execve`, where FreeBSD unshares it first (`fdunshare`) | Nothing in base uses them | When a program needs them |
| `F_KINFO` | `fcntl(F_KINFO)` fills a `kinfo_file`'s type, descriptor, reference count, flags, and offset; its per-type parts and `kf_path` are empty | They come with `kern.proc.filedesc` | P6.8 |
| Nice values | `setpriority` and `getpriority` keep each process's nice value with FreeBSD's rules, but it does not change the threads' scheduling | The time-sharing scheduler has no nice input below its base yet | P6.12 |
| umtx | COMPAT_FREEBSD10's `UMTX_OP_LOCK` and `UMTX_OP_UNLOCK` from a 32-bit caller (`UMTX_OP__32BIT`, `UMTX_OP__I386`) return `EOPNOTSUPP`; on arm64 an unaligned lock word is `EFAULT` (as FreeBSD's arm64), on amd64 any word works. `RLIMIT_UMTXP` does not limit `UMTX_OP_SHM` pages | No 32-bit processes run; limits come with credentials | P6.7 (the limit); permanent (32-bit) |
| POSIX shared memory | `SHM_LARGEPAGE` objects are `ENOTTY` (as on a machine without superpages). A mapping's pages past the object's size read as zeros rather than `SIGBUS`. `kern.ipc.posix_shm_list` and descriptors' `kinfo_file` (`procstat -f`) do not show them | Superpage objects need the VM's large pages; the rest comes with process information | P6.8 |
| System V IPC | The limits are FreeBSD's defaults and fixed (`kern.ipc.*` read-only); `kern.ipc.shmsegs`, `sema`, and `msqids` (what `ipcs` reads) are absent; nothing is per jail | Tunables and the `ipcs` views come with the sysctl tree's writable nodes and jails | P6.7 |
| CPU-time timers | `ITIMER_VIRTUAL` and `ITIMER_PROF` (and `CLOCK_VIRTUAL`, `CLOCK_PROF`) count all of a process's CPU time, and fire on a poll at most every 10 ms: FreeBSD counts `VIRTUAL` in user time only, from the statistics clock | The scheduler keeps one run time per thread, not user and system apart | When the run time is split |
| Timers of stopped processes | `setitimer`'s, POSIX timers, and `EVFILT_TIMER` go on firing while their process is stopped; FreeBSD holds them (`P2_ITSTOPPED`, `ITF_PSTOPPED`, `p_kqtim_stop`) and fires what came due when it continues | Nothing has needed it | When a test does |
| Clock adjustment | `adjtime` steps the clock by the delta at once and reports no delta outstanding; FreeBSD slews it. `ntp_adjtime` and `ntp_gettime` are absent | No `ntpd` runs yet | P6.9 (with `ntpd`) |
| `kldload` | No loadable modules: a module built into the kernel (`geom_md`) is `EEXIST`, as FreeBSD answers for one compiled in, and any other is `ENOENT`; `kldnext` lists no files | The kernel is static (ADR-0005) | Permanent |
| `SIGIO` | `F_GETOWN` and `F_SETOWN` fail with `EINVAL`; `O_ASYNC` is recorded but sends nothing | Nothing in the base system's shell path uses it | When a program needs it |
| `statfs` | The I/O counters (`f_syncwrites` and the like) are 0; tarfs reports its size as used blocks and nothing free | Not tracked | P5.3 (block I/O) |
| Pipes | Buffers do not shrink when pipe memory runs short, and large writes are copied rather than mapped (no direct-write path); the `kern.ipc` pipe counters are absent | What a program sees is the same | P4.7 measures whether direct writes matter |
| Process descriptors | `fstat`'s times are 0 (FreeBSD reports when the process started) | Start times are not recorded yet | With `kern.proc` (P4.7, `ps`) |
| Terminals | Only the console's UART is a terminal: other UARTs attach but are not `ttyuN`. There are no pseudo-terminals (`posix_openpt` returns `ENOSYS`) | The shell on the console needs no more | ptys with sshd (P6) |
| Console line | The console UART keeps the speed, framing, and flow control firmware set: `termios`'s speeds and `c_cflag` are kept but not applied, `HUPCL` and carrier loss do nothing, and `TIOCMGET` reports DTR, RTS, CTS, DSR, and DCD | The console is shared with kernel messages | When a UART other than the console is a terminal |
| `^T` | `VSTATUS` and `TIOCSTAT` send `SIGINFO` but print no load and process line | That line needs the load average and `kern.proc` | With `kern.proc` (P4.7) |
| Terminal queues | Input holds 4096 bytes (beyond that the terminal rings and drops); output is written to the UART as it is queued, and only `^S` makes writers wait. FreeBSD sizes both from the speed and has the driver drain output | A polled console UART | When output is interrupt-driven |
| Session end | When a session leader exits, its controlling terminal is freed and its foreground group gets `SIGHUP`, but descriptors still open on the terminal keep working; FreeBSD revokes them | No vnode revocation yet | P5 (`revoke`) |
| Wide characters | Erasing a two-column UTF-8 character backs over one column | No character width tables | When a program needs it |
| Boot | The kernel starts `/rescue/sh` on the console itself, as `-sh` with `HOME=/` and `PATH=/rescue`, where FreeBSD starts init, which asks for a shell in single-user mode. The root is mounted read-only either way; `mount -u -o rw /` makes it writable | init and `rc` come with multi-user; most of the base system needs the run-time linker | P6 (init) |
| `kern.proc` | `ki_size`, `ki_rssize` and the other sizes, `ki_wmesg`, `ki_pctcpu`, and `ki_estcpu` are 0 or empty; `ki_runtime` and `ki_rusage.ru_utime` are the thread's run time (user and system together); every process reports `root`, class `default`. `kern.devname` is not served, so `ps` shows terminals by number | Not tracked yet | `top` (P6) |
