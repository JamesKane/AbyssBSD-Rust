# FreeBSD multi-user

The design for Phase 6 ([ROADMAP.md](ROADMAP.md#phase-6-freebsd-multi-user-xl--in-progress)): what it takes for
stock FreeBSD 15.1 `/sbin/init` to run `rc` and reach a multi-user `login:` on both architectures,
in QEMU and bhyve, and then on the two physical reference machines.
[ADR-0016](adr/0016-multi-user.md) records the decision.

Behavior reference: sys/kern (kern_thread.c, kern_thr.c, kern_umtx.c, kern_event.c, sys_generic.c,
kern_time.c, uipc_socket.c, uipc_sockbuf.c, uipc_usrreq.c, uipc_shm.c, sysv_*.c, tty.c, tty_pts.c,
kern_prot.c, kern_priv.c, kern_jail.c, sys_capability.c, subr_capability.c, kern_procctl.c,
kern_ktrace.c, sys_process.c, kern_proc.c, kern_shutdown.c, imgact_elf.c, kern_exec.c), sys/vm
(vm_map.c, vm_mmap.c), libexec/rtld-elf, lib/libthr, sbin/init, libexec/rc, sbin/devd,
usr.sbin/syslogd, usr.sbin/cron, libexec/getty, usr.bin/login. FreeBSD is the specification of
what userland sees, not the source of the code ([ADR-0007](adr/0007-license-and-clean-implementation.md)).

## 1. Goals and non-goals

### 1.1 Goals

- **Unmodified base.** `base.txz` from FreeBSD 15.1-RELEASE, extracted onto UFS2, boots multi-user:
  the run-time linker, `libc`, `libthr`, `init`, `rc` and its `rc.d` scripts, `devd`, `syslogd`,
  `cron`, `getty`, and `login` with PAM, on the serial console and on `ttyv0`. No program is
  patched or rebuilt. The image's `/etc` is stock except for the files `bsdinstall` itself writes
  (`rc.conf` with a hostname, `fstab`).
- **The interfaces those programs reach**, all of them, at FreeBSD's semantics: the Tier B system
  calls ([COMPATIBILITY.md §2.1](COMPATIBILITY.md#21-system-calls)), the sysctl nodes base and its
  tools read, the ioctls on the devices they open, and every structure that crosses the boundary
  checked by `abi-check` against FreeBSD's headers.
- **Known breadth.** What `init`, `rc`, the daemons, and libc call is read from their source in
  `../freebsd-src` as each milestone needs it, and the kernel logs the first use of anything it
  lacks, so the long tail is a list worked down, not a surprise. The kyua suite runs nightly
  against FreeBSD's results.
- **The physical machines follow.** P6 gates in QEMU and bhyve. The X870E and the Orange Pi 6 Plus
  join in P7 (board bring-up), with the USB track's hardware half, and every performance suite then
  holds on bare metal ([ADR-0010](adr/0010-qemu-first-development.md)).
- **Performance parity** for the P6 components (§16).

### 1.2 Non-goals for Phase 6

- Networking beyond UNIX-domain sockets. `AF_INET` and `AF_INET6` fail with `EAFNOSUPPORT`, as on a
  FreeBSD kernel built without `INET`/`INET6`, and `rc`'s network scripts behave as they do there.
  P8 brings `ifnet`, loopback, and the protocols onto the socket layer designed here (§8).
- The MAC framework, `audit`, and `rctl`/`racct`: their system calls answer as on a kernel without
  those options (`ENOSYS`, or `kern.racct.enable=0`), which base copes with.
- VNET jails, jail descriptors (`jail_attach_jd`), and hierarchical jail resource limits.
- Swap and the full pageout daemon (P9). Core dumps to swap (`dumpon`, `savecore`) find no dump
  device, as on a machine without one.
- Linux and 32-bit ABIs (permanent, [COMPATIBILITY.md §1](COMPATIBILITY.md#1-target)).
- POSIX AIO beyond what the base boot and kyua's `lib/libc` need; `EVFILT_AIO`, `EVFILT_LIO`,
  `EVFILT_SENDFILE` (P8, with `sendfile`), `inotify` (16-CURRENT only, outside the 15.x ABI).

## 2. Principles

1. **The source answers, the boot checks.** What a program needs from the kernel is read from its
   FreeBSD source (and libc's), not guessed or traced. At run time, a first use of anything
   unimplemented (a system call, sysctl node, ioctl, `fcntl`, `procctl`, socket option, or `sysarch`
   request) is logged once per program (`first use: <kind> <what> by <program>`), and the
   multi-user boot test fails on any such line not in `tests/multiuser/absent.txt`, which lists
   what FreeBSD lacks too and programs only probe, each with the source that shows it.
2. **A process is its threads.** The P4 process (one thread) splits into a process and its threads,
   with FreeBSD's division of state (§4). Every place that says "per thread once processes have
   several" is visited once, in P6.2, not patched as it is found.
3. **One readiness source per object.** Everything a descriptor can wait for (pipes, sockets,
   terminals, kqueues, devices, vnodes, processes, timers) publishes readiness through one type,
   `Notify`, that both `poll`/`select` waiters and kqueue's knotes hang off (FreeBSD's `selinfo`
   with its `knlist`). A new kind of object implements readiness once.
4. **Rights are types at the descriptor boundary.** A descriptor lookup states the Capsicum rights
   it needs, and fails with `ENOTCAPABLE` before a file is in hand (§10.3). Privilege is a typed
   `priv_check`, never `euid == 0` (§10.1).
5. **The socket layer is protocol-neutral.** UNIX domain is its first protocol; P8's are its next.
   Nothing UNIX-specific lives in the socket or socket-buffer code (§8).
6. **Host-testable cores.** The parts with tricky state (umtx queues and priority inheritance,
   knote state, socket buffers and the rights collector, Capsicum rights algebra, the jail tree,
   ktrace records) are written to run in host tests against models, as P5's on-disk engines were.

## 3. Crates

| Crate | Kind | Contents |
|---|---|---|
| `lib/abi` | pure | Grows to every Tier B structure and constant: `kevent`, `sockaddr_un`, `cmsghdr`, `xucred`, `cap_rights_t`, `jail` parameters, `kinfo_file`, `kinfo_vmentry`, ktrace records, `ptrace` requests, `umutex`/`ucond`/`urwlock`/`_usem2`, `thr_param` |
| `lib/elf` | pure, fuzzed | `ET_DYN`, `PT_INTERP`, `PT_GNU_RELRO`, `PT_TLS`, `PT_GNU_STACK`, note parsing for the FreeBSD feature control note (`NT_FREEBSD_FEATURE_CTL`) |
| `lib/caprights` | pure, host-tested | `cap_rights_t` encoding (version, index bits), the rights algebra (contains, merge, remove), the system-call rights table generated from `syscalls.master` (`CAPENABLED`) |
| `lib/ktrace` | pure, host-tested | ktrace record encoding (`ktr_header` and every `KTR_*` payload) |
| `sys/umtx` | kernel, host-tested | umtx keys, hashed wait chains, the mutex, condvar, rwlock, and semaphore protocols, priority-inheritance and -protection chains, robust lists (§5) |
| `sys/event` | kernel, host-tested | `Notify`, knotes and the kqueue state machine, filter registration, `poll`/`select` collection (§7) |
| `sys/socket` | kernel, host-tested | Sockets, socket buffers, the protocol trait, control messages, UNIX domain, the in-flight rights collector (§8) |
| `sys/tty` | kernel | Already the line discipline; gains the device trait and the pseudo-terminal driver (§9) |
| `sys/prison` | kernel, host-tested | The jail tree, parameters, and visibility rules (§10.4) |

The process, thread, credential, exec, and ktrace glue stays in `kernel/` beside the P4 code it
replaces. Crate boundaries follow testability: a core is split out when it can run on the host.

## 4. Processes and threads

### 4.1 State

FreeBSD's division, kept:

| Process (`struct proc`) | Thread (`struct thread`) |
|---|---|
| Address space, descriptors, cwd and root, credential, limits, process group and session, children, `P_*` flags, the signal actions, the process-wide pending set, the ktrace vnode and mask, its jail, `cpuset` default, rusage of exited threads | tid, signal mask, the thread-directed pending set, alternate stack, TLS base, the user context, rusage, name (`thr_set_name`), `cpuset`, rtprio, umtx state (robust lists, contested PI mutexes, the inherited priority), `sigfastblock` word, the system call in progress (`td_sa`) |

A user thread is a `sched::Thread` with a `UserThread` part owned by its process. Thread IDs are
handed out as FreeBSD's `tid_alloc` does, from `NO_PID` (100000) up, the lowest free one after the
last given, bounded by `kern.maxthread`, so a tid is never a pid. The process holds its threads in a list under its lock; a
`ThreadState` enum (`Running`, `Suspended(Why)`, `Exiting`) replaces flag words.

### 4.2 Creation and exit

- `thr_new` builds a thread from `struct thr_param`: its stack, TLS base, entry and argument, and
  where to store its tid (in both the parent's and the child's view, as `child_tid` and
  `parent_tid`); `thr_create` is the old form.
- `thr_exit(state)` stores 1 at `state` and wakes it with `UMTX_OP_WAKE` (libthr joins on it), walks
  the robust lists (§5.4), and leaves. The last thread's `thr_exit` returns instead, as FreeBSD's
  `kern_thr_exit` does, and libthr then calls `exit(0)`.
- `thr_kill`, `thr_kill2`, `thr_suspend`, `thr_wake`, `thr_set_name`, `thr_self`.
- `kern.threads.max_threads_per_proc` (1500) and `kern.maxthread` limit creation, as FreeBSD's.

### 4.3 Single-threading

FreeBSD's `thread_single` with its modes, as one operation:

- **Exit** (`SINGLE_EXIT`): every other thread is woken from interruptible sleeps and exits at the
  user boundary; uninterruptible sleeps finish first.
- **Exec and core dumps** (`SINGLE_BOUNDARY`): the others stop at the boundary; exec then makes
  them exit, a dump lets them go on.
- **`SINGLE_NO_EXIT`** for the few operations that need the process quiet without killing threads.
- `fork` copies only the calling thread (and `vfork` suspends only it), as FreeBSD's. A thread
  being single-threaded out holds no sleepable lock at the boundary, so the exiting side never
  waits on itself.

### 4.4 Signals

- A signal sent to the process goes to its pending set and is delivered to one thread that does
  not block it (FreeBSD's `sigtd`: the current thread if it qualifies, else the first that does);
  a thread-directed signal (`thr_kill`, faults, `pthread_kill`) to that thread's set.
- `sigprocmask` is per thread. `sigwait`/`sigtimedwait`/`sigwaitinfo` take from both sets.
- Stops stop every thread (`P_STOPPED_SIG`), continuing resumes them; `SIGKILL` reaches threads
  in any sleep.
- The deviation "the mask, pending set, and alternate stack belong to the process" closes.

### 4.5 TLS and the user context

- amd64: each thread's FS base (and GS base) is part of its user context. When the CPU has
  FSGSBASE, the kernel enables it for user mode (as FreeBSD does), so user code may change the base
  itself; the switch saves with `rdfsbase` and restores with `wrfsbase`, else with the MSRs, and
  only when the incoming thread's base differs. `sysarch(AMD64_SET_FSBASE, ...)` and its friends
  stay.
- arm64: `TPIDR_EL0` is saved and restored per thread (EL0 writes it directly); `TPIDRRO_EL0` is 0.
- The arm64 SVE signal context ([compat-deviations.md](compat-deviations.md)) lands with threads
  (P6.2), well before the Orange Pi 6 Plus is brought up (P7), since its cores have SVE2.

### 4.6 Limits and accounting

Every `RLIMIT_*` is enforced where FreeBSD enforces it: `NPROC` at fork (per real uid, per jail),
`NOFILE` at descriptor allocation, `DATA` and `AS` in the map, `STACK` as the stack grows (§6.2),
`FSIZE` with `SIGXFSZ`, `CPU` with `SIGXCPU` then `SIGKILL`, `CORE` for dumps, `MEMLOCK` for
`mlock`. `getrusage` and `wait6` report per-thread time summed into the process. Login classes
(`setloginclass`/`getloginclass`) are recorded on the credential.

## 5. umtx

`_umtx_op` with all of FreeBSD 15's operations (`UMTX_OP_WAIT` through `UMTX_OP_SET_MIN_TIMEOUT`),
in `sys/umtx`, host-tested with threads against a model.

### 5.1 Keys

A key names a word in memory. A **private** key is (address space, address); a **shared** key is
(memory object, offset), found by walking the map at the word's address with the map lock held and
taking a reference to the object. An operation uses a shared key when the object (the `umutex`'s
`UMUTEX_ROBUST`/`USYNC_PROCESS_SHARED` flags, the condvar's, the semaphore's) or the operation
(`UMTX_OP_WAIT_UINT` versus `_PRIVATE`) says so, and the word is in a shared mapping; otherwise a
private one, as FreeBSD's `umtx_key_get` decides.

### 5.2 Queues

Keys hash to a fixed array of chains (FreeBSD's `umtxq_chains`), each a sleepable lock and a list
of queues, one per key with waiters. A chain's `busy` mark serializes operations that must read
and write user memory around a sleep (a fault may sleep), so the user word is never read under a
spin lock. The P4 generation-count queues are replaced; `UMTX_OP_WAIT`'s lost-wake rule stays: the
waiter is queued before the word is compared.

### 5.3 Mutexes

- **Normal** (`UMUTEX_*` without protocol bits): the contested bit, sleep, and handoff on unlock.
- **Priority inheritance** (`UMUTEX_PRIO_INHERIT`): a `UmtxPi` per contested mutex records its
  owner; a blocked thread lends its priority along the chain of owners, through the scheduler's
  lent-priority interface (the same one kernel turnstiles use), and gets it back when the mutex is
  released. Chains are walked with a bound and cycles are `EDEADLK`.
- **Priority protection** (`UMUTEX_PRIO_PROTECT`): ceilings, `UMTX_OP_SET_CEILING`, and the
  privilege check for ceilings above the caller's priority.
- **Robust** (§5.4).

### 5.4 Robust mutexes

`UMTX_OP_ROBUST_LISTS` registers a thread's three list heads. When a thread exits (or the process
execs), the kernel walks each list with a bound, marks every mutex the thread still owns
`UMUTEX_RB_OWNERDEAD`, and wakes one waiter; a mutex its owner never made consistent becomes
`UMUTEX_RB_NOTRECOV` on unlock. Shared robust mutexes work across processes.

### 5.5 Condvars, rwlocks, semaphores, and shared memory

`UMTX_OP_CV_WAIT` (unlock-and-wait atomically with respect to `CV_SIGNAL`, any clock),
`CV_SIGNAL`/`CV_BROADCAST`; reader and writer locks with writer preference flags; `SEM2_WAIT`/
`SEM2_WAKE` (the semaphores `sem_open` and `sem_init` build); `NWAKE_PRIVATE`; `UMTX_OP_SHM`, which
gives libthr an anonymous shared-memory object per key for process-shared pthread objects, freed
with the key's object. Timeouts take any clock, absolute or relative, with the minimum-timeout
operations.

### 5.6 POSIX and System V IPC

- `shm_open2` (with `SHM_ANON`, so `memfd_create`, and sealing via `F_ADD_SEALS`/`F_GET_SEALS`),
  `shm_unlink`, `shm_rename`: memory objects named in a per-jail table, mapped like any object.
- System V shared memory, semaphores, and message queues with FreeBSD's `kern.ipc` limits and
  `ipcs` views; kyua's `kern/` tests and some ports use them.
- `ksem_*` are not implemented: 15's libc builds POSIX semaphores on `SEM2` and files.

## 6. Run-time linking

### 6.1 Dynamic executables

The image activator loads `ET_DYN` executables (base is built PIE) at a random base when
`kern.elf64.aslr.pie_enable` says so, and the interpreter named by `PT_INTERP` at another, then
starts the interpreter. The auxiliary vector carries what rtld reads: `AT_BASE`, `AT_ENTRY`,
`AT_PHDR`/`AT_PHENT`/`AT_PHNUM`, `AT_EXECPATH`, `AT_EHDRFLAGS`, `AT_BSDFLAGS`, `AT_HWCAP`/`AT_HWCAP2`,
`AT_OSRELDATE`, `AT_CANARY`, `AT_PAGESIZES`, `AT_TIMEKEEP`, `AT_STACKPROT`, `AT_ARGC`/`AT_ARGV`/
`AT_ENVC`/`AT_ENVV`/`AT_PS_STRINGS`, `AT_FXRNG`, `AT_KPRELOAD`, and `AT_USRSTACKBASE`/`LIM`.
Running `ld-elf.so.1` directly as a program works, since rtld supports it. The FreeBSD feature
control note (`NT_FREEBSD_FEATURE_CTL`) sets ASLR, `PROT_MAX`, and stack-gap opt-outs per binary.

### 6.2 What rtld needs from the VM

- Mapping objects at a reserved range: `MAP_GUARD` reservations replaced by `MAP_FIXED` mappings,
  `MAP_EXCL`, `MAP_ALIGNED_SUPER`, and `PROT_MAX` (most of which P4 has; P6.1 finishes and tests
  them against rtld's patterns).
- RELRO: `mprotect` splitting entries, and the write-protection of a private file mapping after
  relocation, with its pages staying private copies.
- A grow-down stack: `MAP_STACK` regions growing on fault to `RLIMIT_STACK` (up to `MAXSSIZ`), with
  `security.bsd.stack_guard_page` and the stack gap. The deviation "the stack is mapped whole at
  8 MiB" closes.
- `ETXTBSY` for a file mapped as text and opened for writing, and text shared between processes
  through the file's memory object (P5's page cache).

### 6.3 The rest

- TLS is rtld's and libc's business; the kernel keeps the thread's base (§4.5).
- `utrace` (rtld's `LD_UTRACE`, and `malloc`'s) goes to ktrace (§12.2).
- `ldconfig` writes its hints in userland; `kern.elf64.*`, `kern.osreldate`, `kern.ps_strings`,
  `kern.usrstack`, and `kern.proc.auxv` are served.
- `dlopen` of PAM modules (`login`, `su`, `cron`) is ordinary dynamic loading.

## 7. Descriptors and events

### 7.1 Files

`FileKind` gains `Socket`, `Kqueue`, `PtsMaster`, `Shm`, `Timerfd`, and `Eventfd` (FreeBSD's
`__specialfd`), beside P4's vnode, pipe, FIFO, device, and process-descriptor files. Each kind
implements one operations trait with `read`, `write`, `ioctl`, `stat`, `poll`, `kqfilter`, `close`,
and `fill_kinfo` (for `kinfo_file`, §12.1). Descriptors are allocated with their Capsicum rights
(§10.3). `/dev/fd/0`–`2` exist in devfs (and `fdescfs` mounts, for scripts that want it).

### 7.2 `Notify`

A `Notify` belongs to an object and holds two lists: threads blocked in `poll`/`select` on it, and
knotes attached to it. The object calls `notify(hint)` when its state changes, with its own lock
held (FreeBSD's `selwakeup` plus `KNOTE_LOCKED`); knote filters run then with that lock held and
only read the object's state. Waiters re-collect after waking. The rule keeps the event layer out
of every object's lock order except as a leaf.

### 7.3 `poll` and `select`

`poll`, `ppoll`, `select`, and `pselect` collect each descriptor's state with `poll`, register on its
`Notify` if none is ready, and sleep once; signal masks for the `p`-forms are swapped atomically
with the sleep. Results and errors (`POLLNVAL`, `EBADF` for `select`, `POLLHUP` versus `POLLIN` at
end of file per object type) follow FreeBSD.

### 7.4 kqueue

- `kqueue`, `kqueuex` (`KQUEUE_CLOEXEC`, `KQUEUE_CPONFORK`), and `kevent`, with FreeBSD's knote
  states (active, queued, disabled, detached) and flags (`EV_ADD`, `EV_DELETE`, `EV_ENABLE`,
  `EV_DISABLE`, `EV_ONESHOT`, `EV_CLEAR`, `EV_DISPATCH`, `EV_RECEIPT`, `EV_KEEPUDATA`, `EV_EOF`,
  `EV_ERROR`) as a state machine in `sys/event`, host-tested against a model.
- Filters base uses: `EVFILT_READ`, `EVFILT_WRITE`, and `EVFILT_EMPTY` on every descriptor kind;
  `EVFILT_VNODE` (the VFS raises `NOTE_WRITE`, `EXTEND`, `ATTRIB`, `LINK`, `RENAME`, `DELETE`,
  `REVOKE`, `OPEN`, `CLOSE`, `CLOSE_WRITE`, `READ` from its generic layer); `EVFILT_PROC` (`NOTE_EXIT`,
  `FORK`, `EXEC`, `TRACK`); `EVFILT_PROCDESC`; `EVFILT_SIGNAL`; `EVFILT_TIMER` (units, `NOTE_ABSTIME`,
  periodic and one-shot, on the callout wheel); `EVFILT_USER`; `EVFILT_FS` (mount and unmount).
- A kqueue is itself pollable and can sit in another kqueue. Closing a descriptor detaches its
  knotes; `fork` drops kqueues unless `KQUEUE_CPONFORK`.

### 7.5 Timers and clocks

`setitimer`/`getitimer` (`ITIMER_REAL`, `VIRTUAL`, `PROF`), POSIX timers (`ktimer_*`,
`SIGEV_SIGNAL` and `SIGEV_THREAD_ID`), `timerfd_*`, `eventfd` (via `__specialfd`), `clock_nanosleep`
on every clock with `TIMER_ABSTIME`, `settimeofday`/`clock_settime`/`adjtime`/`ntp_adjtime`
(privileged, securelevel-limited), and `clock_getcpuclockid2`. The shared page's timehands (P4)
already serve `clock_gettime` without a system call.

## 8. Sockets

### 8.1 The socket layer

A socket is a `Socket` with a typed state (`Unconnected`, `Connecting`, `Connected`, `Listening`
with its queues of incomplete and complete connections, `Disconnecting`, `Disconnected`), a send
and a receive buffer, generic options (`SO_*`), and a protocol. A protocol implements a trait with
FreeBSD's `pr_*` operations (`attach`, `bind`, `connect`, `listen`, `accept`, `send`, `rcvd`,
`shutdown`, `control`, `sockaddr`, `peeraddr`, `soreceive` and `sosend` overrides) and declares its
flags (`PR_ATOMIC`, `PR_ADDR`, `PR_CONNREQUIRED`, `PR_RIGHTS`). The system calls (`socket`,
`socketpair`, `bind`, `listen`, `accept`/`accept4`, `connect`, `send*`/`recv*` with `msghdr` and
control messages, `shutdown`, `getsockopt`/`setsockopt`, `getsockname`/`getpeername`) know only this
layer. Socket buffers hold data and control records, with FreeBSD's high and low water marks and
`SO_RCVBUF`/`SO_SNDBUF` limits (`kern.ipc.maxsockbuf`). Readiness goes through `Notify` (§7.2).

### 8.2 UNIX domain

- `SOCK_STREAM`, `SOCK_SEQPACKET`, and `SOCK_DGRAM`, named in the file system (P5's bind) or
  unnamed (`socketpair`). Connect finds the bound vnode, checks write permission on it, and queues
  on the listener (`EAGAIN`/`ECONNREFUSED` past the backlog as FreeBSD's `solisten` limits).
- Stream and seqpacket sends append to the **peer's** receive buffer directly, flow-controlled by
  its space, as FreeBSD's `uipc_send`; datagrams carry the sender's address.
- Control messages: `SCM_RIGHTS`, `SCM_CREDS`, `SCM_CREDS2`, the timestamp family; options
  `LOCAL_PEERCRED` (`struct xucred`), `LOCAL_CREDS`, `LOCAL_CREDS_PERSISTENT`, `LOCAL_CONNWAIT`.
- What base needs: `syslogd`'s `/var/run/log` and `/var/run/logpriv` datagram sockets, `devd`'s
  `/var/run/devd.pipe` (stream) and `devd.seqpacket.pipe`, `cron`'s, and `login`'s and `su`'s
  through PAM.

### 8.3 Passing descriptors

A descriptor in flight holds a reference to its file and its Capsicum rights. Cycles (a socket
whose own descriptor is in flight to itself, directly or through others) are found by a
mark-and-sweep collector over in-flight UNIX sockets, run when such a socket closes, as FreeBSD's
`unp_gc`; host tests build the cycles. `kern.ipc.maxfdsinflight`-style limits apply, and receiving
past `RLIMIT_NOFILE` truncates with `MSG_CTRUNC`, as FreeBSD does.

## 9. Terminals

### 9.1 The device trait

`sys/tty`'s discipline gets a device below it, FreeBSD's `ttydevsw`: `open`, `close`,
`outwakeup` (output is queued; the device drains it), `inwakeup`, `ioctl`, `param` (line
settings), `modem`, and `free`. The console UART, each `vt` window, and each pseudo-terminal are
devices. Output is queued and drained by the device, so writers wait on queue space (the
"Terminal queues" deviation closes for ptys and `vt`; the polled console UART keeps it until it is
interrupt-driven).

### 9.2 Pseudo-terminals

`posix_openpt` returns a master file and creates `/dev/pts/N`; `/dev/ptmx` opens one too, for
programs that still use it. libc's `grantpt`, `unlockpt`, and `ptsname` are ioctls on the master
(`TIOCPTMASTER`, `FIODGNAME`), which set the slave's owner and the `tty` group. The master reads
what the slave's discipline output and writes its input; packet mode (`TIOCPKT`), window size and
`SIGWINCH`, and `EIO`/`POLLHUP` on the master when the last slave closes, follow FreeBSD.

### 9.3 Revocation and the console

- `revoke(2)` and devfs revocation: a revoked device vnode's files fail with `ENXIO` or read end of
  file, and its terminal is free. A session leader's exit revokes its controlling terminal's other
  descriptors (closing the "Session end" deviation).
- `/dev/console` and kernel messages follow `boothowto` (`RB_SERIAL`, `RB_MULTIPLE`), closing the
  "Consoles without `RB_MULTIPLE`" deviation, once the boot tier logs from the console it chose.
- The console UART becomes `ttyu0` (with `ttyu0.init`/`.lock` and `cuau0`) so `getty` runs on it
  from FreeBSD's stock `/etc/ttys` (`onifconsole`); other UARTs become terminals too.
- `vt`'s Ctrl-Alt-Del, Ctrl-Alt-Backspace, and the power-button path send `init` FreeBSD's signals.

## 10. Credentials and security

### 10.1 Privilege

`priv_check(cred, Priv)` with a `Priv` enum carrying FreeBSD's `PRIV_*` numbers: the superuser
policy (euid 0, `security.bsd.suser_enabled`), the jail policy (§10.4), and securelevel (§10.5).
Every P4–P5 check of `euid == 0` moves onto it. `security.bsd.see_other_uids`,
`see_other_gids`, `see_jail_proc`, `unprivileged_proc_debug`, and `hardlink_check_*` are honored.

### 10.2 Set-id execution

Executing a set-uid or set-gid file changes the effective and saved ids and marks the process
`P_SUGID` (so `issetugid` and rtld's secure mode see it), unless the mount is `nosuid`, the process
is traced, or it set `PROC_NO_NEW_PRIVS`. Descriptors 0–2 are opened on `/dev/null` if closed
(`fdcheckstd`). `setlogin`/`getlogin` belong to the session; `setcred` (15) is supported.

### 10.3 Capsicum

- Each descriptor carries FreeBSD's `filecaps`: a `cap_rights_t`, an optional ioctl list, and a
  `fcntl` mask. Lookups state the rights the operation needs (`fget(fd, &[CAP_READ])`), and an
  operation on a descriptor without them fails with `ENOTCAPABLE`; `cap_rights_limit`,
  `cap_ioctls_limit`, `cap_fcntls_limit` only narrow. Rights travel with `dup`, `fork`, and
  `SCM_RIGHTS`.
- `cap_enter` sets capability mode on the credential (`CRED_FLAG_CAPMODE`): system calls not marked
  `CAPENABLED` in `syscalls.master` fail with `ECAPMODE` (from a generated table), path lookups are
  relative to a directory descriptor only, with no absolute paths and no `..` above it, and
  `sysctl` is limited to nodes marked capability-safe. `PROC_TRAPCAP_CTL` sends `SIGTRAP`
  instead, and ktrace records `KTR_CAPFAIL`.
- The deviation "Capsicum calls return `ENOSYS`" closes. Capability mode is on the boot's path:
  15.1's `syslogd`, `dumpon`, `savecore`, and `logger` enter it, as do `cat`, `echo`, `dd`, `head`,
  `tail`, `wc`, `uniq`, `kdump`, `truss`, `usbconfig`, and some sixty more, many through
  `libcasper` services (`cap_syslog`, `cap_pwd`, `cap_sysctl`), which talk over `SCM_RIGHTS`-passing
  socket pairs to helper processes forked by `cap_init`.

### 10.4 Jails

- A `Prison` tree under `prison0`, each with its id, name, path (a root vnode, as `chroot`),
  hostname and domain name, `securelevel`, `enforce_statfs`, `children.max`, the `allow.*` flags
  that matter without networking (`set_hostname`, `sysvipc`, `raw_sockets` reserved, `chflags`,
  `mount` with its per-type flags), `persist`, and `devfs_ruleset`.
- `jail`, `jail_set` and `jail_get` (with FreeBSD's name-value `iovec` parameters), `jail_attach`,
  and `jail_remove`, so `jail(8)`, `jls`, and `jexec` work.
- Isolation: process visibility (`p_cansee`, `p_cansignal`), path lookups that never leave the
  root, `statfs`/`getfsstat` filtered by `enforce_statfs`, hostname sysctls, SysV and POSIX IPC
  namespaces, and the jail privilege policy (most `PRIV_*` denied inside).

### 10.5 Securelevel

`kern.securelevel` only rises at run time (jails cap their own). At 1 and above: system-immutable
and append-only flags cannot be cleared, mounted disks' raw devices cannot be written, and
`/dev/mem`/`/dev/io` refuse writes; at 2, no disk device can be opened for writing while mounted
and the clock cannot step backwards; at 3, as FreeBSD's.

### 10.6 Process control

`procctl` with every FreeBSD 15 command: the reaper set (`PROC_REAP_ACQUIRE`, `RELEASE`, `STATUS`,
`GETPIDS`, `KILL`), `TRACE_CTL`, `TRAPCAP_CTL`, `PDEATHSIG_CTL`, `ASLR_CTL`, `PROTMAX_CTL`,
`STACKGAP_CTL`, `NO_NEW_PRIVS_CTL`, `WXMAP_CTL`, `LOGSIGEXIT_CTL`, and their `STATUS` queries.

### 10.7 ACLs and extended attributes

UFS2's extended-attribute area (`di_extb`) and POSIX.1e and NFSv4 ACLs stored in it, behind P5's
system calls (which return `EOPNOTSUPP` today), so `getfacl`/`setfacl` and `tar --acls` work on a
UFS2 mounted with `acls`/`nfsv4acls`; `fsck_ffs` checks the result.

## 11. The system: init, rc, and services

### 11.1 Boot

The kernel executes `init` from `init_path` (`/sbin/init:/sbin/oinit:/sbin/init.bak:/rescue/init`),
as pid 1 with `/dev/console` on descriptors 0–2. `boot -s` (`RB_SINGLE`) makes init ask for a
shell. The loader's environment is the kernel environment (`kenv(2)`), from which `init_path`,
`vfs.root.mountfrom`, and the tunables come. The P4 path that starts `/rescue/sh` directly becomes
the fallback when no init can be executed, as FreeBSD's.

### 11.2 What `rc` needs

The `rc.d` scripts FreeBSD 15.1's default `rc.conf` runs, read from `libexec/rc`, and the kernel
side of each, by area:

| Area | Scripts | Kernel |
|---|---|---|
| File systems | `fsck`, `root`, `mountcritlocal`, `tmp`, `var`, `cleanvar` | `fsck -p` on the read-only root (raw reads of a mounted disk), `mount -u -o rw /`, `mount -a` from `fstab`, `mount -t tmpfs`, `fstab`'s ESP on `/boot/efi` (the kernel stops mounting it itself) |
| Devices | `devfs`, `devd`, `devmatch`, `kldxref`, `moused` | devfs rules (`DEVFSIO_*` ioctls, `devfs.rules`), `/dev/devctl` events and `/dev/devctl2`, `kldstat`/`kldnext` and `kern.module_path` answering as a kernel with every module built in |
| System state | `hostid`, `random`, `adjkerntz`, `sysctl`, `hostname`, `dumpon`, `savecore`, `ldconfig`, `motd`, `osrelease`, `dmesg`, `utx` | `kern.hostuuid`/`hostid`, writes to `/dev/random` and `kern.random.*`, `machdep.adjkerntz`/`wall_cmos_clock`, the `sysctl.conf` nodes, `kern.msgbuf`, `dumpon`'s `DIOCSKERNELDUMP` failing as without a dump device |
| Services | `syslogd`, `newsyslog`, `cron`, `virecover`, `sendmail` | UNIX sockets (§8.2), `/dev/klog`, `kqueue`, capability mode and `libcasper` (§10.3), `setlogin`, and the absent-network behavior of a kernel without `INET` |
| Logins | `init` with `/etc/ttys` | `getty` on `ttyu0` and `ttyv0`–`ttyv7`, `login` with PAM (`dlopen`), `setusercontext` (class, limits, priority, umask, `cpuset`), `utmpx` |

### 11.3 devd and devctl

`/dev/devctl` is a read-only queue of FreeBSD's event lines (`!system=...`, `+name at ... on bus`,
`-name ...`, `?` for unmatched devices) from the driver model's events (P3 already produces them for
tests); it has one reader, with FreeBSD's queue limit (`hw.bus.devctl_queue`) and disable sysctl.
`/dev/devctl2` serves `devctl(8)`'s ioctls (attach, detach, enable, disable, suspend, resume,
set driver, rescan, delete, reset) onto the driver model's operations. devd runs on base's stock
rules.

### 11.4 Shutdown

`reboot(2)` with `RB_HALT`, `RB_POWEROFF`, `RB_POWERCYCLE`, `RB_NOSYNC`, and `RB_REROOT`
(`reboot -r`), from init's sequence (`rc.shutdown`, `SIGTERM` then `SIGKILL` to all, sync): the
kernel syncs, unmounts every file system (UFS2 clean, so the next boot's `fsck -p` has nothing to
do), and powers off or resets through ACPI or PSCI. `shutdown -p now`, `halt`, `reboot`, and
Ctrl-Alt-Del work.

## 12. Observability

### 12.1 Process information

- `kern.proc` complete: threads (`KERN_PROC_INC_THREAD`), sizes and resident pages, `ki_wmesg`,
  `ki_pctcpu`/`ki_estcpu` from the scheduler, the login, class, and jail id, terminal names through
  `kern.devname`, so `ps`, `top`, `w`, and `pgrep` show what FreeBSD's show.
- `kern.proc.filedesc` (`kinfo_file`, also `F_KINFO`), `vmmap` (`kinfo_vmentry`), `kstack` (symbolized
  kernel stacks of each thread), `rlimit`, `umask`, `osrel`, `sigtramp`, `auxv`, `ps_strings`, `cwd`,
  `groups`, `env`, `args`, and `sigfastblk`, so `procstat` (every mode), `fstat`, and `lsof`-style
  readers work.
- `vm.stats.*`, `vm.vmtotal`, `vm.loadavg`, `kern.cp_time`/`cp_times`, `vm.swap_info` (none), and
  `hw.physmem` and friends for `top`, `vmstat`, `systat`, and `uptime`; the `^T` status line.

### 12.2 ktrace

`ktrace(2)` attaches a trace file and facility mask to processes (with `KTRFAC_INHERIT` and
descendants), and records `KTR_SYSCALL`, `SYSRET`, `NAMEI`, `GENIO`, `PSIG`, `CSW`, `USER`, `STRUCT`,
`STRUCT_ARRAY`, `SYSCTL`, `PROCCTOR`/`PROCDTOR`, `CAPFAIL`, `FAULT`, and `FAULTEND`, encoded by
`lib/ktrace`. Records are queued per thread and written by a writer thread, so a traced call never
does file I/O in its own path, as FreeBSD's `ktr_writerequest`. Unmodified `kdump` reads them;
differential tests compare AbyssBSD's trace of a program with FreeBSD's.

### 12.3 ptrace

`ptrace` enough for `truss` and kyua's `ptrace_test`: attach and detach, `PT_TRACE_ME`, continue
and step, `PT_TO_SCE`/`PT_TO_SCX`, `PT_LWPINFO`, `PT_GETNUMLWPS`/`PT_GETLWPLIST`, register access
(general, FP, and on amd64 XSAVE), `PT_IO`, `PT_VM_ENTRY`, `PT_FOLLOW_FORK`, `PT_GET_SC_ARGS`/`RET`,
and `PT_SUSPEND`/`PT_RESUME`; process descriptors' `pdptrace` is outside 15.x. Debuggers (`lldb`,
`gdb`) are exercised by M-U, not gated here.

## 13. ABI checking

`cargo xtask abi-check` covers every structure that crosses the boundary. Each `#[repr(C)]` type in
`lib/abi` carries the FreeBSD header and name it mirrors; the tool generates a C program on the
build server that compares size, alignment, and every field's offset against FreeBSD 15.1's
headers for both amd64 and arm64 (the arm64 headers from the release's `src.txz`). A type without a
check fails.

## 14. Testing

- **Host.** `sys/umtx` (mutex, condvar, rwlock, and semaphore protocols against a model under
  random thread interleavings, PI chains), `sys/event` (knote states against a model),
  `sys/socket` (socket buffers, the stream and datagram paths, the rights collector with built
  cycles), `lib/caprights`, `sys/prison`, `lib/ktrace`, `lib/elf` fuzzing.
- **Kernel tests.** Threads (single-threading under load, signals to threads), umtx across
  processes, kqueue on every filter, socket pairs and `SCM_RIGHTS`, ptys, revoke, set-id exec,
  capability mode, jails.
- **User tests.** `tests/user/*.c`, built static and dynamic on the server, each passing on
  FreeBSD 15.1 too, as P4–P5's do.
- **Boot tier.** New `<arch>-multiuser` cases: the UFS2 image with all of `base.txz` boots to
  `login:` on the serial console and on `ttyv0`, root logs in on both (base's empty password, as
  in FreeBSD's VM images; on `ttyv0` typed over QMP, the shell proving itself by writing its
  terminal's name to `/dev/console`), checks that the daemons run, uses a pty, `procstat`,
  `logger` (found in `/var/log/messages`), and a cron job, and shuts down with
  `shutdown -p now`; the image then passes `fsck_ffs -f -n` with zero errors. The first-use log
  holds nothing outside `absent.txt` (§2). `freebsd-multiuser` runs the same script on FreeBSD.
- **Compat.** kyua's suite runs nightly in bhyve, per test case, against FreeBSD 15.1's results in
  the same VM, with every expected failure listed with a reason in `tests/kyua/known-failures.txt`.
  Not under QEMU: emulated, the suite takes days, and its timing-sensitive tests measure the
  emulator. arm64's results come from the Orange Pi 6 Plus once it runs AbyssBSD (P7), against
  FreeBSD on the same board.
- **Differential.** Programs and their `ktrace`/`kdump` traces on AbyssBSD and FreeBSD.

## 15. Hardware bring-up

The physical machines are brought up in P7, the phase after this one, and gate from then on
([ADR-0010](adr/0010-qemu-first-development.md)). This section records what P6 prepares for it. They are
touched only after P6.9 (FreeBSD userland end to end in QEMU), and every root step on them goes
through a script the user runs.

- **X870E.** `cargo xtask deploy --machine x870e` writes the loader, kernel, and UFS2 image only to
  the media NVMe, identified by serial number, with its own ESP and a `BootNext` entry. Logs come
  from the framebuffer and the persistent log the loader saves to the media ESP. FreeBSD 15.1 is
  installed on the same disk for baselines.
- **Orange Pi 6 Plus.** First, the Linux and FreeBSD ground truth (`acpidump`, `devinfo -rv`,
  `pciconf -lv`, `usbconfig`, `sysctl -a`, `dmesg`, register dumps listed in HARDWARE.md §4) is
  captured into `tests/corpus/opi6p/`; then the board boots AbyssBSD from USB, and only then is the
  NVMe reimaged, with partitions for AbyssBSD and a FreeBSD baseline (15.1 if it runs on the Sky1,
  otherwise the closest release, named in each report, per PERFORMANCE.md §4).
- Both machines' ACPI tables join the AML corpus, and each machine's device tree is compared with
  FreeBSD's on the same hardware.
- The USB track's hardware exit: the machine driven entirely from the GOP console and a USB
  keyboard, a USB disk passing fsx and `fsck_ffs` (`usb.umass` joins the suite).

## 16. Performance

The P6 components ([ADR-0014](adr/0014-performance-parity-per-component.md)), each gated on its
geometric mean, in bhyve and on both physical machines (where every earlier suite also runs against
FreeBSD on bare metal):

| Component | Benchmarks |
|---|---|
| `boot` | Kernel time from loader handoff to the first `login:` on the serial console (FreeBSD: the same, from its own loader handoff timestamp), and `rc` alone (init's start to `login:`) |
| `thr` | `pthread_mutex` and `pthread_cond` ping-pong and contention at 1, 2, 4, and 8 threads (normal, PI, and process-shared mutexes); `pthread_rwlock` read-mostly; thread create and join; `sem_post`/`sem_wait` |
| `event` | `kevent` on pipe readiness (1 and 1000 descriptors), `EVFILT_USER` round trip, `EVFILT_TIMER` delivery; `poll` and `select` on 1000 descriptors |
| `unix` | Stream and seqpacket ping-pong latency, stream bandwidth (4 KiB and 64 KiB writes), datagram rate, `SCM_RIGHTS` pass rate, connect and accept rate |
| `pty` | Throughput through a pty pair (raw and canonical), and a `script`-style relay |
| `exec` | Dynamic `execve` of `/usr/bin/true` (rtld with `libc` alone) and of a program linking ten libraries; `sh -c` running a shell-heavy script (`rc.subr` functions over the `rc.d` tree, timed) |

kyua's wall time is recorded and reported as a diagnostic, not gated (its tests sleep).
The exact workloads are in [tests/perf/README.md](../tests/perf/README.md#suite-p6) (suite `p6`).
