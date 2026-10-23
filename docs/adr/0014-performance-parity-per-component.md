# ADR-0014: Performance parity is judged per component

- Status: Accepted
- Date: 2026-10-23
- Amends: [ADR-0011](0011-performance-parity.md)

## Context

ADR-0011 made parity with FreeBSD an exit criterion and judged it per benchmark: "a 10% win in one
place does not pay for a 5% loss in another." That was not the intent. The rule is meant to keep a
component (a file system, the allocator, the scheduler) from being slower than FreeBSD's, not to
make every microbenchmark a gate.

Applied per benchmark, it gated on numbers that say little about the component. At P5.9 the file
system suite stood at 0.846 (UFS) and 0.864 (tmpfs) of FreeBSD's time, with every workload-level
benchmark (`extract`, `find`, `fsx`) at or below parity, while five microbenchmarks sat 3-18% over:
fixed costs of tens of nanoseconds a call (`stat` of a missing name, `open`/`close`) and UFS's
create, where soft updates' bookkeeping costs 0.8 µs a file. Chasing those moved the components'
standing by under 1%.

## Decision

- Parity is judged **per component**: the geometric mean of the ratios of the component's
  benchmarks in a phase's suite must be **no more than 1.02** (at most 2% slower than FreeBSD). The
  goal is still to be faster.
- A component is what a benchmark's name says it measures: its first part (`malloc`, `ctxsw`,
  `vm`), or for file systems its first two (`fs.ufs`, `fs.tmpfs`).
- Individual benchmarks are diagnostics. Each is still reported with its ratio, confidence interval
  and verdict, so a component's slow paths stay visible, but none gates by itself.
- A benchmark that fails to produce a number fails its component.
- The rest of ADR-0011 stands: suites are cumulative, a regression past the bound fails the build
  (now a component's), the method is in [PERFORMANCE.md](../PERFORMANCE.md), and changes to a bound
  or a suite's makeup need an ADR the owner accepts.

## Consequences

- A component's suite should include workload-level benchmarks, not only microbenchmarks, so the
  mean reflects how the component is used.
- A large win in one microbenchmark can mask a loss in another within a component. Reports list
  every benchmark, and a loss on a workload-level benchmark remains worth investigating even when
  its component passes.
- `cargo xtask perf` exits non-zero only when a component is over the bound (or a benchmark is
  missing), and prints each component's mean after the table.
