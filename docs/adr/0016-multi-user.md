# ADR-0016: FreeBSD multi-user

- Status: Accepted
- Date: 2026-10-24

## Context

Phase 6 must boot unmodified FreeBSD 15.1 `base.txz` to a multi-user `login:` on both
architectures, in QEMU and bhyve and then on both physical reference machines, which gate from this
phase on ([ADR-0010](0010-qemu-first-development.md)). After P5 the kernel runs static programs
with one thread per process; it has no run-time linking, threads, kqueue, `poll`, sockets beyond
binding a UNIX name, ptys, privilege framework, Capsicum, or jails. Base reaches all of them on the
way to `login:`: every program is a dynamic PIE, `syslogd` and dozens of tools enter capability
mode, `devd` and `syslogd` listen on UNIX sockets, `login` loads PAM modules.

The breadth of the FreeBSD ABI is the roadmap's named P6/P7 risk. ADR-0011 and ADR-0014 require a
performance suite per phase.

## Decision

Adopt the design in [MULTIUSER.md](../MULTIUSER.md). Its load-bearing parts:

1. **FreeBSD's boot is the test plan.** P6.0 records every system call, sysctl, and ioctl FreeBSD
   15.1's multi-user boot and its kyua suite make, in the same QEMU machines. The boot test fails
   on any first use of an unimplemented interface that the record contains.
2. **Processes and threads split as FreeBSD's do**, with per-thread signal masks, TLS bases, and
   umtx state, and `thread_single`'s modes for exit, exec, and core dumps, done in one milestone.
3. **umtx in a host-tested `sys/umtx`**: private and shared keys, hashed chains with a busy mark
   around user-memory access, priority inheritance through the scheduler's lent-priority
   interface, priority protection, robust lists, and `UMTX_OP_SHM`.
4. **One readiness type (`Notify`) per object**, under both `poll`/`select` and kqueue; knote filters
   run under the object's lock and only read state.
5. **A protocol-neutral socket layer** with UNIX domain as its first protocol and a mark-and-sweep
   collector for descriptors in flight; P7's protocols use the same trait.
6. **A device trait under the line discipline**, with ptys, `vt` windows, and UARTs as devices.
7. **Typed privilege and rights**: `priv_check` with FreeBSD's `PRIV_*`, Capsicum rights checked at
   descriptor lookup, capability mode from a table generated from `syscalls.master`, and a jail tree
   whose policy feeds `priv_check` and visibility.
8. **No networking in P6.** `AF_INET`/`AF_INET6` behave as on a FreeBSD kernel without `INET`, and
   that boot, recorded on FreeBSD, is the ground truth for `rc`'s network scripts. MAC, audit, and
   `rctl` answer as on a kernel without them.
9. **Hardware after QEMU.** The physical machines are touched only once P6.9 runs userland end to
   end in QEMU; the Orange Pi 6 Plus's ground truth is captured before it is reimaged.
10. **Performance.** P6 adds six components, each gated on its geometric mean in bhyve and on both
    physical machines: `boot`, `thr`, `event`, `unix`, `pty`, `exec` (MULTIUSER.md §16). kyua's wall
    time is a diagnostic. Every earlier suite also gates on bare metal from P6's exit.
11. **kyua target.** For each of `kern/`, `lib/libc/`, `bin/`, and `usr.bin/`, at least 95% of the
    test cases that pass on FreeBSD 15.1 in the same machine pass on AbyssBSD, and every other
    failure is listed with its reason in `tests/kyua/known-failures.txt`.

## Consequences

- Phase 6 is the largest phase. Its milestones are ordered so base's dynamic programs run first
  (P6.1), which turns the whole of `/bin`, `/sbin`, and `/usr/bin` into test material for the rest.
- Without networking, `rc` prints the lines a FreeBSD kernel without `INET` would (netif, sendmail).
  They are recorded in compat-deviations.md until P7.
- The P4 one-thread shortcuts (process-wide signal mask, the `tls_base` on the process, the umtx
  generation queues) are replaced, not extended.
- ptrace arrives with ktrace (for `truss` and kyua's `ptrace_test`), beyond the roadmap's original
  P6 list; debuggers themselves wait for M-U.
- Jail descriptors, VNET, and hierarchical jail limits are deferred; if a later phase needs them,
  they extend `sys/prison`.
