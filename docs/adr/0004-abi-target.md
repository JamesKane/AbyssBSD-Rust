# ADR-0004: FreeBSD 15.x native 64-bit userland as the primary ABI target

- Status: Accepted
- Date: 2026-10-17

## Context

The reference tree (`../freebsd-src`) is 16.0-CURRENT, whose ABI is still moving. FreeBSD 15.x is
the current release line: its `base.txz` sets for amd64 and arm64 are fixed, signed, and easy to
get, and the build server runs 15.1-RELEASE. That makes it ideal for differential testing.

The FreeBSD kernel ABI is backward compatible, so a kernel that serves 16-CURRENT's syscalls also
serves 15.x binaries, as long as the 15.x-era calls are implemented.

## Decision

- The primary test target is **FreeBSD 15.x-RELEASE native 64-bit userland** (amd64, arm64; later
  riscv64).
- Use 16-CURRENT source as the reference for semantics, and adopt its additions when they are cheap,
  so the eventual switch is incremental.
- Switch the primary target to 16.x after 16.0-RELEASE ships, via a superseding ADR.
- No `COMPAT_FREEBSD*` syscalls older than what 15.x libc can call.

## Consequences

- Stable binaries to test against, with a real FreeBSD machine of the same version as the oracle.
- `kern.osreldate` reports a 15.x value initially. Ports and packages built for 15.x install
  unchanged.
