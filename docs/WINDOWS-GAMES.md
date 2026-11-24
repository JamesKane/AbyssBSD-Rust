# Windows Games (NT Personality and Game Runtime)

- Status: **Draft**. A forward design for running unmodified Windows games on AbyssBSD. Decisions
  here become an ADR when Windows compatibility enters the roadmap.
- Date: 2026-11-23
- Scope: a Windows NT process personality in the kernel on `x86_64` and `aarch64` (objects and
  handles, waits, virtual memory, threads, exceptions, files, direct system calls), the user
  runtime above it (image loader, windowing on Wayland, input, Vulkan, audio), and the packaging
  that keeps one title's state from breaking another's. Games that need a kernel anti-cheat driver
  are out of scope.
- Inputs: [LINUXULATOR.md](LINUXULATOR.md) (the personality seam this design shares),
  [GRAPHICS.md](GRAPHICS.md), [DESKTOP.md](DESKTOP.md), [VFS.md](VFS.md),
  [COMPATIBILITY.md](COMPATIBILITY.md), [ADR-0004](adr/0004-abi-target.md),
  [ADR-0005](adr/0005-monolithic-static-kernel.md),
  [ADR-0007](adr/0007-license-and-clean-implementation.md),
  [ADR-0011](adr/0011-performance-parity.md),
  [ADR-0014](adr/0014-performance-parity-per-component.md),
  [ADR-0025](adr/0025-graphics-abi-and-native-drivers.md), Microsoft's published documentation of
  the Windows interfaces, and the behavior of Windows itself, recorded.

The short version: **the kernel implements Windows NT's process semantics exactly, as a third
personality beside FreeBSD and Linux; a small runtime we write connects Windows code to the
Wayland, Vulkan, and sound interfaces FreeBSD userland already uses; and everything above that is
an immutable, versioned bundle that a title pins.** Wine is fragile for three structural reasons:
it emulates kernel semantics from user space through a server process (unless the host kernel
grows features for it, which FreeBSD's has not), it lets every title mutate a shared prefix, and
it binds to whatever libraries the host happens to have. This design removes each cause by
construction (§3) and keeps the flexibility: any library in a bundle can be replaced, and several
bundles run side by side.

Two things the request assumes are not in the tree yet. There is **no audio stack**: it is
designed in [AUDIO.md](AUDIO.md) and unscheduled. Vulkan reaches hardware only as
[GRAPHICS.md](GRAPHICS.md) §14's steps land (Venus at M-G.3, panvk at M-G.5, RADV at M-G.6). Both
are prerequisites here (§12), not part of this design.

---

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal | Why |
|---|---|---|
| G1 | **Unmodified 64-bit Windows games run**, from their own installers or a store's files, when they need no kernel anti-cheat driver | The purpose |
| G2 | **A title that works keeps working.** Updating the kernel, the runtime, Mesa, or another title never changes what a pinned title sees | The fragility this design exists to remove (§3) |
| G3 | **NT semantics are kernel semantics**: handles, named objects, waits, sections, threads, and file sharing behave as on Windows across processes, with no server process on any hot path | The largest source of Wine's defects and of its performance workarounds |
| G4 | **Flexible above the kernel**: every user library is a replaceable unit in a bundle; a title's manifest states which bundle, which overrides, and which settings | Wine's real strength, kept |
| G5 | **Zero cost to native processes**: FreeBSD processes never touch NT state; the personality is one more `Abi` variant behind a kernel feature (`nt64`) | [ADR-0005](adr/0005-monolithic-static-kernel.md), [ADR-0011](adr/0011-performance-parity.md), LINUXULATOR.md principle 2 |
| G6 | **Performance**: each measured component no more than 2% slower, overall, than Wine from FreeBSD 15.1's packages running the same workload on the same machine, and ideally well ahead of it | [ADR-0014](adr/0014-performance-parity-per-component.md); FreeBSD with its Wine port is the only FreeBSD baseline there is (§11) |
| G7 | **Clean provenance**: BSD-2-Clause Rust written from published documentation and recorded Windows behavior | [ADR-0007](adr/0007-license-and-clean-implementation.md) |
| G8 | **No silent success**: an unimplemented call fails with the Windows status for "not implemented" and is logged once per title | [COMPATIBILITY.md](COMPATIBILITY.md) §4 |

### 1.2 Non-goals

- **Kernel anti-cheat and kernel-mode copy protection.** No Windows drivers are loaded, ever.
  A title that installs one fails at that step with a clear message.
- **A general Windows desktop.** Office suites, shell extensions, printing, and installers that
  expect a full Windows are not targets. Whatever a game or its launcher needs is.
- **32-bit and 16-bit titles in the first version.** This collides with "64-bit only"
  ([VISION.md](VISION.md), COMPATIBILITY.md §1) and is left as a decision (§13 question 2).
- **x86-64 titles on `aarch64` in the first version.** The personality and the runtime build and
  are tested on both instruction sets, as every change must. On `aarch64` they run Windows
  programs built for Arm. Translating x86-64 game code is a separate design (§13 question 3).
- **Reimplementing Direct3D, or a Vulkan driver.** Mesa provides the drivers
  ([GRAPHICS.md](GRAPHICS.md) G1), and Direct3D arrives as existing translation libraries (§2.2).
- **Wine or Proton themselves.** Both remain available without this design (§2.1). Nothing here
  depends on them or forbids them.

---

## 2. What "native" adds, and where the code comes from

### 2.1 The two paths that already exist

| Path | What it costs AbyssBSD | What it inherits |
|---|---|---|
| FreeBSD's Wine packages, unmodified | Nothing beyond the FreeBSD ABI ([ADR-0004](adr/0004-abi-target.md)), graphics, and sound | All of Wine's fragility, plus the FreeBSD port's own gaps (its 32-bit-in-64-bit mode has lagged upstream) |
| Linux Proton through the Linux personality | LINUXULATOR.md stage L4 and more of Linux's interfaces than L4 plans | The same fragility, a second translation layer under it, and a moving target tuned for Linux alone |

Both are worth having as fallbacks and as the comparison (§11). Neither can meet G2 or G3, because
their fragility is in their structure, not in their defects.

### 2.2 Sources

As in [GRAPHICS.md](GRAPHICS.md) §2, this design writes the part only we can write and reuses the
part that is far beyond us.

| Layer | Who provides it | License and rule |
|---|---|---|
| NT personality in the kernel | AbyssBSD, Rust | BSD-2-Clause. References: Microsoft's documentation of the native and Win32 interfaces, the Portable Executable format specification, and behavior recorded on Windows (§10) |
| System libraries: `ntdll`, the windowing and input system-call layer (`win32u`), the Vulkan loader library (`vulkan-1`), the audio endpoint, the socket provider | AbyssBSD, Rust | BSD-2-Clause, same references |
| Direct3D 8 to 11 | DXVK, unmodified, in the bundle | zlib license (to confirm at W0) |
| XAudio2 and X3DAudio | FAudio, unmodified, in the bundle | zlib license (to confirm at W0) |
| Vulkan drivers | Mesa from FreeBSD packages (Venus, panvk, RADV) | MIT, already the plan |
| Direct3D 12 | vkd3d-proton, unmodified, in the bundle | LGPL (to confirm at W0); see §13 question 1 |
| The rest of Win32 (`kernelbase`, `user32`, `gdi32`, `advapi32`, the component object model libraries, the C runtime, and many more) | **Undecided**: §13 question 1 | |

ReactOS is GPL and is never consulted. Whether LGPL code (Wine, vkd3d-proton) may be shipped
unmodified as a package, and whether it may ever be read, is not something ADR-0007 settles: it
names GPL only. Until that is decided this design treats LGPL sources as **not consulted**.

The behavioral reference is Windows, because nothing else can be. FreeBSD remains the reference for
everything the personality is built on, and FreeBSD with Wine is the performance baseline.

---

## 3. Fragility: each cause and what removes it

"Wine" below means Wine as FreeBSD runs it, the baseline of §11. Wine on Linux has closed some of
these with kernel features made for it: a synchronization driver with NT's wait rules (F1), a way
to intercept system calls by address (F7), and, since Wine 11, 32-bit Windows code with no 32-bit
host libraries (F10). Those are prior art for the mechanisms here, and confirm that the fix
belongs in the kernel. FreeBSD has none of them. F2 to F6 and F9 hold for Wine everywhere.

| # | Why Wine breaks | Mechanism here | How it is tested |
|---|---|---|---|
| F1 | Synchronization objects are emulated in user space or by a server round trip; the faster schemes each give up some semantics | Events, mutexes, semaphores, timers, and waits on any mix of them are kernel objects with Windows's exact rules (wait-all, alertable waits, abandoned mutexes) in `sys/ntobj` (§5) | Host model tests of the wait engine; a conformance suite compared with Windows (§10) |
| F2 | One single-threaded server holds all cross-process state; when it stalls or dies, every process in the prefix does | The kernel holds handles and objects. The one user service left (§7.3) is off every hot path, holds no handle state, and can restart | Kill and restart the service under a running title |
| F3 | A prefix is one mutable directory mixing system libraries, the registry, and saved games; a Wine update rewrites it in place | A read-only **runtime bundle**, plus one writable layer per title that can be snapshotted and rolled back (§8) | Update the bundle store under an installed title and compare what the title sees, byte for byte |
| F4 | Library overrides are folklore applied by scripts, different on every machine | A declarative **title manifest**: bundle, overrides, redistributables, settings. The same manifest gives the same environment (§8.2) | Rebuild a title's environment from its manifest alone on a clean machine |
| F5 | Wine's native half loads whatever versions of host libraries are installed | The host boundary is four interfaces: the kernel, the Vulkan loader, Wayland, and `sound(4)`. Everything else is inside the bundle (§7) | A bundle built against FreeBSD 15.1 runs on a later userland unchanged |
| F6 | One Wine serves every title, so a fix for one regresses another | Bundles are side by side and a title pins one. The kernel's NT interface is versioned and only grows (§4.4) | Old bundles stay in the test matrix |
| F7 | Direct system calls and fixed-address data need special kernel features that are often missing or slow | The personality answers the `syscall` instruction itself and maps the shared data page where Windows has it (§4.2, §4.3) | Conformance tests that issue system calls directly |
| F8 | Case-insensitive names, sharing modes, and delete-on-close are approximated by directory scans and server bookkeeping | Case-insensitive lookup in `namei` for this personality, and NT file-object rules in the kernel (§6) | Host tests in `sys/vfs`; conformance suite |
| F9 | Stubs return success, so failures surface far from their cause | G8: fail and log once; a per-title report of every unimplemented call reached | The report itself is a test artifact |
| F10 | 32-bit titles need a second set of 32-bit host libraries, or a mode FreeBSD's port has not kept working | No 32-bit host code under any option considered (§13 question 2) | With the decision, at W6 |
| F11 | Windows are mapped onto X11 with focus, fullscreen, and pointer-capture workarounds | A Wayland-native window layer using the relative-pointer and pointer-constraints protocols (§7.3) | The desktop harness's lanes, extended |

---

## 4. The NT personality

### 4.1 `Abi::Nt64`

LINUXULATOR.md §4.1 gives `Process` a typed `Abi`. This design adds a variant to the same enum and
uses the same seams (system-call entry and exit, signal and fault delivery, `execve`, core dumps),
the same `crate::firstuse` logging, and the same feature-gate pattern:

```rust
pub(crate) enum Abi {
    FreeBsd64,
    #[cfg(feature = "linux64")]
    Linux64(Arc<LinuxProcData>),
    #[cfg(feature = "nt64")]
    Nt64(Arc<NtProcData>), // handle table, object directory, session
}
```

If this work starts before the Linux personality, it builds that seam first, to LINUXULATOR.md's
design.

### 4.2 A process with two instruction-level interfaces

A Windows game process cannot be Windows code alone. Mesa's Vulkan driver, `libwayland-client`,
and FreeBSD's libc must run in the same address space, or every draw call would cross a process
boundary. So an `Nt64` process holds both kinds of code, and the kernel must tell their system
calls apart:

- **`x86_64`**: both use the `syscall` instruction with the number in `rax`, with different
  argument registers. Every mapping in an `Nt64` process carries an **origin**: a mapping made
  through the FreeBSD table (`mmap` from `rtld`, including Mesa's driver when the Vulkan loader
  opens it long after start) is native; a mapping made through the NT table is Windows. Origin is
  set by the kernel at the call that creates the mapping, so there is no list for a process to
  keep current and none to go stale. A system call whose instruction lies in a native mapping
  uses the FreeBSD table, untouched. Any other uses the NT table and convention. The check is one
  lookup in the address space's map, made only for `Nt64` processes. Code generated at run time
  inherits the origin of the mapping it is written into.
- **`aarch64`**: FreeBSD uses `svc #0` with the number in `x8`. Windows on Arm encodes the number
  in the `svc` immediate (to record at W0). The immediate separates them, with the range check
  for the ambiguous value.

Thread state follows the same split. FreeBSD's thread pointer stays in `FS` (`TPIDR_EL0` on
`aarch64`). The Windows thread environment block is addressed through `GS` on `x86_64`, which
FreeBSD userland does not use and the tree already carries per thread (`sysarch`'s
`AMD64_SET_GSBASE`, the signal context, and user-mode `wrgsbase` where the processor has it).

On `aarch64` Windows keeps it in `x18`, and there is a conflict. The kernel preserves a user
thread's `x18` across traps, but FreeBSD's userland is not built with `x18` reserved (only the
kernel is, in `sys/conf/kern.mk`; libc's own assembly uses it as a scratch register). Native code
may therefore clobber it. So `x18` is **reloaded on every return to Windows code**: by the thunks
from the thread's native thread-local storage, and by the kernel when it delivers an exception or
an asynchronous procedure call into Windows code.

Calls between the two worlds (a Windows library calling Vulkan, a Wayland callback reaching
Windows code) go through generated thunks in the runtime that convert calling conventions and
switch stacks. On `x86_64` the Windows convention differs from System V; on `aarch64` they nearly
coincide.

### 4.3 The system-call table and its placement

NT's native interface is a few hundred calls. Each one has a **placement**, recorded in a
generated table in `lib/nt-abi`:

| Placement | Meaning | Examples |
|---|---|---|
| **Kernel** | Implemented in `kernel/src/nt/` over native subsystems | Waits and synchronization objects, handles, virtual memory, threads, file I/O, section mapping, completion ports |
| **Reflected** | The kernel returns the call to a registered entry in the process's native runtime, which implements it with ordinary FreeBSD calls | Cold, process-local calls: locale tables, system information queries, most of the registry client |
| **Unimplemented** | Fails with the "not implemented" status and logs once (G8) | |

Reflection is what makes this tractable and flexible. A call can start life reflected, where it is
easy to write and debug, and move into the kernel when measurement or cross-process correctness
demands it, with no change visible to Windows code. Calls made by `ntdll`'s own stubs and calls
made directly by a game's code take the same path, which is F7.

System-call numbers on Windows change between builds. The personality presents **one Windows
build's identity** consistently (version numbers, system-call numbers, the layout of the shared
data page at its fixed address, which is fed from the same timehands as `shared_page`). Which
build is §13 question 5.

### 4.4 Interface stability

The NT table as AbyssBSD implements it is versioned. Within a version it only grows, and a
bundle records the version it was built against. This is the FreeBSD ABI contract applied one
layer up: an old bundle on a new kernel is a supported configuration and is tested (F6).

### 4.5 Starting a process

- A native launcher (`ntrun`) is an ordinary FreeBSD program. It enters the personality with a
  new `procctl` request, registers its reflection entry, maps `ntdll`, and jumps to the image's
  entry through it. Running a `.exe` by name is then a matter of FreeBSD's `binmiscctl(8)`
  mechanism (`imgact_binmisc`), not of a new image activator. The tree does not have
  `imgact_binmisc` yet; it is small, and is listed as work (§9, §12).
- The kernel does not parse Portable Executable images. `lib/pe` (host-tested, no `unsafe`) is
  used by the loader in user space. `ntrun` maps `ntdll` itself and marks it as Windows code
  through the same `procctl` request. From then on `ntdll`'s loader maps every image through the
  NT section calls, which the kernel serves from plain file and anonymous mappings: the image
  layout is computed in user space by the reflected half of the section call, and the kernel
  checks only ranges and protections, as it does for `mmap`.
- A Windows process creating another one becomes `fork` and `execve` of `ntrun` in the child,
  with handle inheritance done by the kernel's handle table, so a launcher starting its game,
  waiting on the process handle, and reading its exit code all work as on Windows.

---

## 5. Objects, handles, and waits (`sys/ntobj`)

A host-testable crate holding the part of NT's executive that must be exact:

- **Handle table** per process: typed entries with access masks and inheritance flags, duplicated
  across processes by the kernel.
- **Object directory** per session: named events, mutexes, semaphores, sections, and timers,
  visible to every process of that session and to no other (§8.3).
- **The wait engine**: one implementation of "wait for any" and "wait for all" over any mix of
  waitable objects (events with manual and automatic reset, mutexes with ownership, recursion,
  and abandonment, semaphores, waitable timers, threads, processes, files, completion ports),
  with alertable waits that deliver asynchronous procedure calls. Wait-all must acquire every
  object atomically or none. That requirement is why an exact engine cannot be composed from
  `sys/umtx` alone, and why F1 exists.
- **What it reuses**: sleep queues, priorities, and timeouts from `sys/sched` and the callout
  machinery; `sys/umtx` for the address-based waits (`WaitOnAddress`, slim reader-writer locks,
  condition variables), which map onto it directly; `sys/event` beneath I/O completion ports.
- **Objects that are also FreeBSD objects**: a file handle wraps a `File`; a process handle wraps
  a process descriptor; a section wraps a VM object (the `shm` machinery). The native half of the
  process can therefore pass descriptors to Mesa and Wayland with no copying.

Virtual memory maps onto `sys/vm` with NT's rules kept in the personality: reservation and commit
as separate steps, 64 KiB allocation granularity, guard pages, write watches (used by some
runtimes' garbage collectors), and placement control. `MAP_FIXED`, `MAP_EXCL`, `MAP_GUARD`,
aligned mappings, and `procctl` control of address randomization already exist; reserve-then-commit
and write watches are new work in `sys/vm`.

Exceptions: a fault or trap in Windows code is delivered by the signal seam to `ntdll`'s exception
dispatcher with a Windows context record, not as a FreeBSD signal frame. Faults in native mappings
stay FreeBSD signals. Structured exception handling, unwinding, and vectored handlers are then
`ntdll`'s business in user space.

---

## 6. Files and names

- **Namespace.** NT paths resolve through a per-session table of drive letters and devices to
  FreeBSD paths inside the title's root (§8). No symbolic-link farm in the file system.
- **Case.** A lookup flag in `sys/vfs/src/namei.rs`, set only for this personality: an exact match
  is tried first, then a case-insensitive match against a per-directory folded-name index held in
  the name cache and invalidated with the directory. Native processes never set the flag. Two
  names in one directory that differ only by case are resolved by a fixed, documented rule.
  Today the tree has no case-insensitive lookup at all (`_PC_CASE_INSENSITIVE` reports 0).
- **File objects.** Sharing modes, delete-on-close, delete-pending, byte-range locks, and
  rename-over-open follow NT's rules among `Nt64` processes, kept on the vnode.
- **Asynchronous I/O.** Overlapped I/O and completion ports sit on `sys/aio` and `sys/event`.
- **Named pipes and sockets.** Named pipes are a file type over the UNIX-domain socket machinery
  with message framing. Winsock's provider reaches `sys/socket` through the native runtime.
- **Extended attributes** carry what FreeBSD file systems have no field for (the hidden and
  system attributes, creation time where the file system lacks it).

---

## 7. The user runtime

### 7.1 Shape

```
 Windows code      game.exe · the bundle: Direct3D translation, audio libraries, Win32 libraries
───────────────────────────────┬────────────────────────────────────────────────────────────────
 system libraries (ours)       │ ntdll · win32u · vulkan-1 · audio endpoint · socket provider
───────────────────────────────┴───────────┬────────────────────────────────────────────────────
 NT system calls ───────────────────────────│── thunks (calling convention, stack) ─────────────
───────────────────────────────┬───────────┴────────────────────────────────────────────────────
 native runtime (ours, FreeBSD)│ libntrt.so: reflected calls · Wayland client · Vulkan loader
                               │ client · sound(4) client · evdev and sys/hid controllers
───────────────────────────────┴────────────────────────────────────────────────────────────────
 host boundary (F5)            kernel (FreeBSD + NT tables) · libvulkan + Mesa · Wayland · sound(4)
───────────────────────────────┬────────────────────────────────────────────────────────────────
 kernel                        │ kernel/src/nt · sys/ntobj · sys/vfs (case flag) · sys/vm
                               │ sys/umtx · sys/event · sys/aio · sys/socket · sys/drm · sound(4)
```

### 7.2 Loader and `ntdll`

`ntdll` is ours: system-call stubs generated from `lib/nt-abi`, the image loader (imports, delayed
imports, thread-local storage, interface-set redirection, side-by-side manifests), the heap, and
the run-time library. It is a Portable Executable library written in `no_std` Rust. The pinned
toolchain already links Portable Executable images for the UEFI targets; whether it can produce
Windows libraries with only built-in targets and the bundled linker, as
[ADR-0003](adr/0003-stable-rust-builtin-targets.md) requires, is to be shown at W0.

### 7.3 Windows, input, and the session service

- **Windows.** Each top-level window is a Wayland `xdg_toplevel`. Fullscreen, mode changes (served
  by scaling through the compositor, never by changing the display's mode), relative pointer
  motion, pointer confinement, and the clipboard use the standard protocols that wlroots and the
  desktop's `scoriad` implement ([DESKTOP.md](DESKTOP.md) §4.2).
- **The session service** (`ntsessd`, native, one per session) owns what on Windows belongs to the
  window manager and has no place in a kernel: the window tree, message routing between
  processes, the clipboard, and the registry's store. It is the only user service in the design.
  It holds no handles and no synchronization state, in-process message queues never pass through
  it, and its state is reconstructible, so a restart does not end the session (F2).
- **The registry** is a read-only base hive from the bundle plus a writable hive in the title's
  layer (F3). Whether its hot read path needs to be in the kernel is a measurement question.
- **Controllers and raw input** come from `sys/evdev` and `sys/hid`, mapped by the native runtime
  to XInput's and the game-input interfaces' device model, with hotplug from `devd`.

### 7.4 Graphics

- `vulkan-1` in the bundle is a thin library whose every entry is a thunk to the host's Vulkan
  loader. It adds the Windows surface extension, implemented on the Wayland surface extension,
  and translates only what differs (surface creation, a few structures carrying handles).
- Direct3D 8 to 11 is DXVK and Direct3D 12 is vkd3d-proton (§2.2). Both need only Vulkan and a
  small Win32 surface, and both are replaceable per title (G4).
- Presentation uses explicit synchronization and dma-buf as [GRAPHICS.md](GRAPHICS.md) §4.4 and
  §5 provide them. Variable refresh and high dynamic range wait on GRAPHICS.md §13 question 5.
- Pipeline and shader caches live in the title's layer, keyed by driver and bundle, so a Mesa
  update invalidates them cleanly.

### 7.5 Audio

- The Windows audio session interface (the endpoint beneath every other Windows audio interface)
  is ours, over `sound(4)`'s Open Sound System device with its virtual channels for mixing, as
  FreeBSD's own audio clients use it. XAudio2 is FAudio on top (§2.2). The older interfaces are
  thin layers over the endpoint.
- The endpoint starts on `sound(4)`'s Open Sound System device and moves to a native stream
  ([AUDIO.md](AUDIO.md) §6.4) for shared-memory I/O, event-driven wakes, and true latency.

---

## 8. Bundles, titles, and isolation

### 8.1 Runtime bundles

A bundle is a read-only, content-addressed directory tree: the Windows system directory (our
system libraries, the Win32 libraries, the Direct3D and audio libraries), the base registry hive,
fonts, and a manifest naming the NT interface version it needs (§4.4). Bundles are installed by
`pkg`, never modified after installation, and garbage-collected when no title pins them.

### 8.2 Titles

A title is a manifest plus one writable layer:

```toml
[title]
name    = "Example Game"
bundle  = "nt-runtime-2027.1"        # pinned; changing it is an explicit, reversible act
windows = "10.0.19045"

[overrides]                          # stated, not scripted (F4)
"d3d11.dll" = "bundle:dxvk-2.5"

[redistributables]
install = ["vcrun2022"]
```

The title's root is the bundle with the writable layer over it (`unionfs`, or `nullfs` mounts with
a private system directory: to settle at W5). Saved games, the title's registry hive, and caches
are all in the layer. Snapshot before any change of bundle or override; roll back by discarding.
Two titles share nothing writable.

### 8.3 Sessions and jails

Each running title is one NT **session**: its own object directory, session service, and drive
table. A session runs in a jail (`kernel/src/prison.rs`) whose file system is the title's root
and whose devices are the render node, the sound device, and the input devices the
desktop grants. The desktop itself does not jail programs ([DESKTOP.md](DESKTOP.md) §4.2), so
this model is the runtime's own. A game cannot read the user's home directory unless the manifest
grants a path.

---

## 9. Crates

| Crate / module | Contents | Tested | `unsafe` |
|---|---|---|---|
| `lib/nt-abi` | Generated system-call table with placements and versions, status codes and their mapping to and from `Errno`, `#[repr(C)]` structures for both instruction sets | host | **forbidden** |
| `lib/pe` | Portable Executable parsing and validation, relocations, imports, exception tables | host, fuzzed | **forbidden** |
| `sys/ntobj` | Handle tables, object directories, the wait engine, asynchronous procedure call queues | host (model tests), kernel | small (wait-queue links) |
| `sys/vfs` (extended) | Case-insensitive lookup flag and folded-name index, NT sharing and delete state | host | none added |
| `sys/vm` (extended) | Reserve and commit, write watches | host, kernel | yes (it is the VM) |
| `kernel/src/exec.rs` (extended) | `imgact_binmisc` and its `kern.binmisc` control, as FreeBSD has them | kernel, QEMU | none |
| `kernel/src/nt/` | `Abi::Nt64`, dual-table dispatch, mapping origin, reflection, exception delivery, files, sections, completion ports, sessions | kernel (`#[ktest]`), QEMU | minimal (user access, context records) |
| `arch/{x86_64,aarch64}` (extended) | NT register convention, context records, exception frames | QEMU | yes (arch boundary) |
| Runtime (a new top-level tree, to name at W0) | `ntdll`, `win32u`, `vulkan-1`, the audio endpoint, `libntrt.so`, `ntrun`, `ntsessd` | conformance suite | thunks only |

Nothing here loads code into the kernel at run time, in keeping with
[ADR-0005](adr/0005-monolithic-static-kernel.md).

---

## 10. Verification

| Layer | Method |
|---|---|
| **Layouts and tables** | `lib/nt-abi` structures checked against layouts recorded from Windows for both instruction sets, as `abi-check` does for FreeBSD |
| **Wait engine, handles** | Host model tests: randomized schedules against a simple reference model, including wait-all atomicity and abandonment |
| **Conformance suite** | Our own test programs, written in Rust and built as Windows executables, each printing a canonical result. Run on **Windows itself** to record ground truth, then on AbyssBSD, compared as kyua results are compared with FreeBSD's. Timing-dependent cases are excluded with a `timing:` reason |
| **Direct system calls** | Suite cases that bypass `ntdll` (F7) |
| **Isolation and bundles** | F3, F4, F6 as written in §3: update, rebuild-from-manifest, and old-bundle runs |
| **Graphics and audio** | Our Vulkan test program (W3), then Vulkan conformance subsets and `vkcube` built for Windows (W4), through the thunk library; Direct3D sample programs through DXVK; audio capture loopback through `snd_dummy` |
| **Title corpus** | A fixed list of freely redistributable games and engine demos per stage, each with a scripted run to a known frame and a screenshot comparison, on Venus in QEMU and on the reference machines |
| **Robustness** | `lib/pe` and the NT system-call entry are fuzzed: a hostile image or argument must never panic the kernel |

---

## 11. Performance

Components ([ADR-0014](adr/0014-performance-parity-per-component.md)), each against Wine from
FreeBSD 15.1's packages on the same machine, where FreeBSD drives the graphics processor (the
X870E's and the Z690-A's RDNA2 with `drm-kmod`):

- `nt.sync`: contended and uncontended waits, wait-any and wait-all, cross-process events;
- `nt.mem`: reserve, commit, protect, and free; write-watch scans;
- `nt.file`: open by a case-mismatched path, read, overlapped I/O through a completion port;
- `nt.call`: the cost of a kernel-placed and of a reflected system call, and of a thunk in each
  direction;
- `nt.frame`: frame times (mean and 99th percentile) of the title corpus;
- native components rerun with the `nt64` feature enabled, to show G5.

Where FreeBSD has no graphics driver (panvk on Sky1), results are recorded and do not gate, under
whatever rule GRAPHICS.md §13 question 7 arrives at. Windows on the same hardware is a useful
diagnostic for `nt.frame` and never a gate.

---

## 12. Prerequisites and phasing

| Needs | From | For |
|---|---|---|
| The `Abi` personality seam | LINUXULATOR.md L1, or built here first | W1 |
| Vulkan in QEMU (Venus), dma-buf, explicit synchronization | M-G.3 | W3 |
| Sound: `sound(4)` with a hardware driver, then native streams | [AUDIO.md](AUDIO.md) A2 to A4, unscheduled | W4 |
| RADV on RDNA2 | M-G.6 | W4 on hardware |
| `nullfs` or `unionfs` | Unscheduled (VFS.md non-goal today; no longer M-D work, ADR-0026) | W5 |

| Stage | Work | Exit criterion |
|---|---|---|
| **W0** | Surveys and decisions: a host tool that reads the imports of a title corpus and ranks the Win32 surface actually used; licenses confirmed at their sources; the toolchain question (§7.2); the Windows build to present; the ADR | The ADR, with §13 questions 1, 2, and 5 decided |
| **W1** | `Abi::Nt64`, dual-table dispatch, `lib/nt-abi`, `lib/pe`, `sys/ntobj` handles and waits, `ntrun`, `imgact_binmisc`, a minimal `ntdll` | A console "hello" and the synchronization conformance cases pass in QEMU on both instruction sets, with identical results to Windows |
| **W2** | Loader, threads, exceptions, virtual memory rules, files and case, named pipes, completion ports, process creation, reflection | The non-graphical conformance suite passes; a multi-process, multi-threaded stress program runs clean |
| **W3** | `win32u` over Wayland, input, the session service, `vulkan-1` thunks | A Vulkan test program of ours, built for Windows against `ntdll`, `win32u`, and `vulkan-1` alone (no Win32 libraries above them), draws in a Sway window on Venus in QEMU and answers keyboard and pointer |
| **W4** | The Win32 libraries chosen at W0 (`kernel32`, `user32`, and what the corpus imports), DXVK, the audio endpoint and FAudio, controllers | Stock `vkcube` built for Windows runs; the first corpus titles reach their scripted frame with sound, on the X870E with RADV; `nt.*` components measured against FreeBSD's Wine |
| **W5** | Bundles, title manifests, layers and snapshots, sessions in jails, the desktop's launcher integration (`scoria-launch`, Game Focus rules) | F3, F4, and F6 tests pass; installing, updating, and rolling back a title needs no manual step |
| **W6** | Per the decisions: 32-bit titles, x86-64 on `aarch64`, Direct3D 12, store clients | |

---

## 13. Open questions (to settle in the ADR)

1. **Who provides the rest of Win32.** The system libraries (§7.2 to §7.5) are ours either way.
   For the large remainder there are three options:
   - (a) *Write it*, in Rust, in the order the W0 survey ranks it. Clean provenance, exact fit
     to our system libraries, and years of work before the long tail of titles runs.
   - (b) *Ship Wine's Windows-side libraries unmodified* in a bundle, over our `ntdll` and
     `win32u`. Far faster to breadth. It needs a ruling that LGPL packages may be shipped (and
     how the private interfaces those libraries expect can be learned without reading LGPL
     source, which may make this option unworkable), and it ties bundles to Wine's releases.
   - (c) *Both*: (b) as the first bundle line, (a) replacing it library by library. Bundles make
     the mix legal per title (G4).

   Leaning (a) for anything a game calls every frame and (c) overall, if the license ruling
   allows. This is the largest decision in the document, and it is a policy decision first.
2. **32-bit titles.** Much of the back catalog is 32-bit. The mechanism that fits is the one
   Windows itself uses: 32-bit Windows code inside a 64-bit process, with thunks to the 64-bit
   system libraries. The kernel's share is small (a 32-bit user code segment on `x86_64`, and
   allocation below 4 GiB, for which `MAP_32BIT` exists), with no 32-bit system-call table and no
   32-bit host libraries. It is still an exception to "no 32-bit" and needs the user's decision.
3. **x86-64 titles on `aarch64`.** Needs a binary translator in the process. Candidates and their
   licenses are to be surveyed. Until then the Orange Pi 6 Plus runs the conformance suite and
   Arm-built programs only.
4. **Store clients and user-mode anti-cheat.** Whether a store's Windows client is a supported
   title, and whether user-mode anti-cheat components that have vendor-supplied builds for Wine
   can work here, where there is no Wine for them to detect.
5. **Which Windows build to present** (§4.3), and how often that identity may change given §4.4.
6. **Recording Windows ground truth.** Where the reference Windows runs (a guest under bhyve on
   the build server is the obvious place) and what its license permits.
7. **Registry placement** (§7.3) and whether any window-manager state must move into the kernel
   for latency.
8. **Media playback.** Many titles play cutscenes through Windows's media interfaces. This needs
   codecs and ties to the media document GRAPHICS.md §13 question 3 calls for.
9. **Feature default.** Whether `nt64` is on in the desktop kernel profiles once W2 passes.
