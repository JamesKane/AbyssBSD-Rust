# ADR-0024: Zones are exact-layout classes of the one heap, declared at build time; `Zone<T>` and `ZBox<T>` are not built

- Status: Proposed
- Date: 2026-11-12

## Context

On the Orange Pi 6 Plus, `poll` and `select` over a thousand pipes and `kevent` with a thousand
registered are about a quarter slower once the file-system suite has run in the same boot
(`poll1000` 150 to 200 µs), and stay so until a reboot. FreeBSD on the same board shows nothing
(282 and 280 µs). What was measured (2026-11-12):

- Only the thousand-descriptor benchmarks slow down; the single-descriptor and ping-pong ones do
  not, so it is not frequency, the scheduler, or background work.
- Creating and deleting a tree of files brings it on in proportion: three rounds on tmpfs 9%,
  three more on UFS 13%, the whole suite 27%.
- The time goes to the per-descriptor work: registering with each object's `Notify`, and the
  file and descriptor-table lookups around it.
- The direct map is 1 GiB leaves, so it is not the TLB.
- Keeping an object's first poller in the object (`6c269e8`) made both states faster by 5 to 10%
  and left the ratio between them where it was.

The likely cause, not proven (the board has no cache-miss counters): the heap
(`sys/vm/src/slab.rs`) is fifteen size classes shared by every type. Deleting tens of thousands
of files leaves holes in those classes among objects that stay (cached vnodes, names, inodes),
and the next thousand `File`s and `Pipe`s fill the holes, so objects walked together no longer
lie together and the hardware prefetcher no longer follows the walk. FreeBSD's `uma` gives
each hot type a zone (`Files`, `pipe`, `socket`, `VNODE`, ...): whatever the file systems free,
the slabs that hold files hold only files.

ARCHITECTURE.md §5.2 plans "typed zones (`Zone<T>`) return `ZBox<T>`". Nothing of it is built,
and on this project's Rust it cannot serve the objects that matter:

- Files, pipes, sockets, vnodes, and processes are `Arc`s. `Arc::new_in` needs the allocator
  API, which is not stable (ADR-0003).
- A pointer type of our own cannot become a `dyn Trait` pointer on stable Rust (`CoerceUnsized`
  is unstable), and most of these objects are also held as `Arc<dyn …>`, with `Weak`s.

The global allocator sees only a `Layout`, never a type. What it can tell apart is an exact size
and alignment.

## Decision

1. **A zone is a dedicated class of the heap for one exact layout.** A zone names a size
   (rounded up to the heap's 16-byte granule) and an alignment. An allocation whose `Layout` has
   that granule and that alignment is served from the zone; every other allocation goes to its
   size class as now. `dealloc` gets the same `Layout` and makes the same choice. A zone has what
   a class has, from the same code: its own slabs, its own depot, a pair of magazines per CPU.
2. **Zones are declared at build time**, in one table in the kernel crate handed to
   `Heap::new`, each with FreeBSD's zone name and the layout computed from the type in a
   constant expression. The granule-to-class table the heap already indexes is built from it in a
   `const fn`, so an allocation's path is the same single table load; the alignment is compared
   only when the granule is a zone's. There is no registration at run time: an object allocated
   before its zone existed would be freed into it.
3. **No call site changes.** `Arc::new(File { … })` lands in `Files` because of its layout. The
   layout of an `Arc`'s allocation is two counters and the value (`ArcInner`, `repr(C)` in the
   standard library but not promised): the table computes it as
   `Layout::new::<[usize; 2]>().extend(Layout::new::<T>())`, and a kernel test allocates one of
   each zone's type and fails if it did not come from its zone, so a change in the standard
   library is a failed test, not a silent return to the size classes.
4. **Two types with the same granule and alignment share a zone.** The table is checked at build
   time for such pairs among the declared types; an undeclared type that happens to match is
   tolerated (it is as likely to be short-lived as anything in a size class). A type that must
   be alone can take an alignment of its own.
5. **The first zones are `Files` and `pipe`**, the two objects a descriptor walk touches. A
   type gets a zone when many are alive at once and walked together, FreeBSD has a zone for it,
   and a measurement shows the gain: sockets and their PCBs, knotes, vnodes and the name cache's
   entries, processes and threads are the candidates, each on its own evidence.
6. **`vm.zone_stats` lists them** beside the mbuf zones (`kernel/src/net/memstat.rs`) with
   FreeBSD's names, item size, and counts, so `vmstat -z` shows `Files` and `pipe` as on FreeBSD.
7. **Not taken from `uma`:** constructors, destructors, `init` and `fini` (Rust's own
   construction and `Drop`); type-stable memory (`UMA_ZONE_NOFREE`; nothing here reads a freed
   object); per-zone limits (`kern.maxfiles` and the like stay where they are enforced);
   per-domain kegs and cache coloring. Each can be added to a class later without changing this
   decision.
8. **`Zone<T>` and `ZBox<T>` are not built.** ARCHITECTURE.md §5.2's sentence is replaced by this
   record when it is accepted. Should the allocator API become stable, a typed front end can sit
   on the same classes.

## Staging

0. **The measurement first.** A prototype with the two zones and nothing else, on the board:
   the event suite before and after the file-system suite. If the after-effect does not shrink to
   a few percent, the cause is something else, this record is withdrawn, and the prototype is
   dropped.
1. The heap: the zone table, the merged class table, zone classes, host tests (a zone's objects
   share no slab with any class's; churn in the size classes does not spread a zone's objects;
   the existing magazine, trim, and reclaim tests over zones too).
2. `Files` and `pipe` declared; the kernel test of point 3; `vm.zone_stats`; the board's event
   suite and `p2`'s `malloc` benchmarks, which must not move.
3. Further zones, one measurement each.

## Consequences

- **Memory.** Each zone keeps up to two magazines per CPU (a page each) and at least one slab;
  two zones on twelve CPUs are under 200 KiB at most. A zone's free objects cannot serve another
  type; `Heap::trim` and `Heap::reclaim` give its magazines and empty slabs back as they do a
  class's.
- **The fast path is unchanged** for allocations that are not a zone's: the same table load. A
  zone's allocation adds one comparison.
- **It depends on `ArcInner`'s layout**, held by the test of point 3.
- **Fragmentation within a zone stays**, as on FreeBSD: a zone whose own objects churn (vnodes)
  still scatters. This decision keeps types apart; it does not order a class's free objects.
- **`realloc`** between a zone's layout and another moves the object, as between two classes.

## Alternatives considered

- **`Zone<T>` with pointer types of our own.** Cannot be `dyn`, cannot be `Arc` or `Weak`; every
  holder of a file, pipe, or vnode would change, and trait objects would need hand-made vtables.
- **The allocator API.** Not on stable Rust (ADR-0003).
- **Fixed tables** of files and pipes (an array indexed by a handle). Keeps them together, but
  puts a ceiling on each type and changes every holder.
- **Ordering each class's free objects by address**, so holes fill from one end. Helps every type
  without a table, but the magazines are what make the fast path lock-free and they are last in,
  first out; address order would put a search on that path, and types would still interleave.
- **Leaving it.** After the file-system suite `poll1000` is still ahead of FreeBSD (176 µs
  against 280) and `kevent.pipe1000` level with it, so the gate may hold without this. The cost is
  an advantage that shrinks with uptime, on the workloads (servers with many descriptors) where
  it is most visible.
