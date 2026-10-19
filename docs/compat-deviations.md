# Deliberate deviations from FreeBSD

[COMPATIBILITY.md](COMPATIBILITY.md) §4 says FreeBSD's behavior wins unless this list says
otherwise. Each entry says what differs, why, and when it ends. Temporary entries end in the phase
named; the rest are permanent.

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
