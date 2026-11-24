# Desktop

- Status: **Draft**. The design for M-D under [ADR-0026](adr/0026-desktop-scoria.md) (Accepted),
  which supersedes [ADR-0018](adr/0018-desktop-architecture.md)
- Date: 2026-11-23
- Scope: **Scoria**, the desktop AbyssBSD ships: its compositor and Vulkan renderer, session
  supervisor, IPC, configuration, shell components, interaction rules, and what it needs from this
  kernel. Applications and a general-purpose toolkit are out of scope
- Inputs: the Scoria design (`~/Projects/desktop/scoria/docs/design.md`, 2026-11, written for
  Odin), [ADR-0002](adr/0002-dependency-policy.md), [ADR-0003](adr/0003-stable-rust-builtin-targets.md),
  [ADR-0014](adr/0014-performance-parity-per-component.md),
  [ADR-0025](adr/0025-graphics-abi-and-native-drivers.md), [GRAPHICS.md](GRAPHICS.md),
  [AUDIO.md](AUDIO.md), [MULTIUSER.md](MULTIUSER.md), [USB.md](USB.md),
  [ENGINEERING.md](ENGINEERING.md)

The short version: **a tiling Wayland desktop for FreeBSD, written in Rust in this workspace, for
people who play games, rice their desktop, and stream.** The compositor (`scoriad`) renders
with Vulkan 1.3 only, sets modes through the Direct Rendering Manager's atomic interface, and
imports client buffers without copies. A small supervisor (`scoria-sup`) owns the session, the
configuration, and every process it starts. Components talk over one typed protocol on
`SOCK_SEQPACKET` sockets. There is no D-Bus, udev daemon, logind, systemd, or PipeWire in the core
session. The interaction rules come from the 1978–1997 research on human pointing, perception and
memory, and they are tested like any other requirement. Scoria is ordinary FreeBSD 15.1 userland:
the same package runs on stock FreeBSD 15.1 with `drm-kmod`, which is how it is measured here.

> **Note.** Scoria began as an Odin project. This document is its design carried into Rust and
> into this workspace (ADR-0026). Binary names, the wire protocol, the configuration format, and
> the interaction rules are unchanged, except that the OBS plugin cannot be written here
> (§4.2 item 7). What Odin's sections covered (language, bindings, the experiment
> gate) is replaced by §2.

## 1. Aims

### 1.1 Tenets

| # | Tenet | Consequence |
|---|---|---|
| T1 | **One system, separate processes** | Every core component lives in `desk/`, speaks one IPC protocol (§7), and reads one configuration, which reloads atomically (§5) |
| T2 | **FreeBSD interfaces, not Linux plumbing** | kqueue, devd, Capsicum, `setrlimit`, `mixer(3)`, evdev, `pam_xdg`, and small libraries that fit them (libseat, libinput, libucl, xkbcommon, libliftoff, libdisplay-info). §3 maps each Linux daemon to its replacement |
| T3 | **Vulkan 1.3 only** | No OpenGL or EGL in anything Scoria renders. Atomic mode setting over scanout buffers, explicit synchronization where the kernel has it, dma-buf import with format modifiers. Vulkan 1.4 features are used where present |
| T4 | **The research is a requirement** | Fitts, Hick–Hyman, the keystroke-level model, Xerox Star, Apple's 1987 guidelines, IBM CUA, Rooms, marking menus, and the mode-error studies measured limits that have not changed. Conventions that agree with them are adopted, others adapted or rejected (§8, §10) |
| T5 | **Desktop input, not tablet input** | Keyboard, mouse, and trackpad. Hamburger menus, chrome hidden behind gestures, finger-sized padding, full-screen app grids, edge swipes, and unlabeled toggle tiles are rejected. Touchscreens are an absolute pointer |
| T6 | **Measured** | Scoria is a component under ADR-0014: each benchmark against the same package on FreeBSD 15.1 on the same machine (§12) |

### 1.2 The user

Scoria is designed for one composite user who **plays** demanding games, **rices** the desktop
until every pixel is deliberate, and **streams** the result:

| Facet | Demands | What it forces on Scoria |
|---|---|---|
| Gamer | Lowest input-to-photon latency, even frame pacing, variable refresh and tearing on request, nothing stealing focus | The desktop can leave the frame path entirely: direct scanout, suspended effects, held notifications (§9.1) |
| Ricer | Every color, gap, curve, font, and shader addressable and changeable live from one configuration | Rendering and theming are data-driven, hot-reloadable, and safe to experiment with: timed revert and contrast floors (§9.2) |
| Streamer | Control over what the audience sees: private overlays, redacted windows, hotkeys that work in game | Capture is a compositor concern with its own render pass, not a portal added later (§9.3) |

*The content is the star. The desktop is the stage crew: fast, invisible when it should be, and
never on stage by accident.*

When the facets disagree, the higher item wins:

1. **Safety and privacy.** The lock screen never leaks. Capture exclusions and privacy indicators
   cannot be themed away.
2. **Input latency and frame pacing.**
3. **Interface floors** (§8.2): legibility, visible focus, discoverability, forgiveness.
4. **Expression**, inside whatever room the three items above leave.

### 1.3 Non-goals

- An X11 server or an X11 window manager inside `scoriad`. X11 clients run through
  `xwayland-satellite` (§4.2).
- OpenGL rendering, and any software renderer other than Vulkan on lavapipe.
- D-Bus or PipeWire in the core session. The opt-in bridge (§4.2) brings them back for programs
  that need them, and nothing else depends on it.
- Linux. Scoria is FreeBSD userland; it runs on AbyssBSD and on stock FreeBSD 15.1.
- A ten-foot (television) interface (§14 question 6).

## 2. Constraints from AbyssBSD

### 2.1 Dependencies

ADR-0002's rule (no external crates) stays: nothing in `desk/` comes from crates.io or any other
registry. What a compositor needs that ADR-0002 did not foresee is **C libraries**: libwayland,
Mesa's Vulkan drivers, libinput. Rewriting them adds no design value and yields code less tested
than what it replaces. ADR-0026 therefore amends ADR-0002 for `desk/` only:

1. **C libraries from FreeBSD packages or `base.txz`, linked dynamically**, each reached through one
   FFI crate under `desk/ffi/` whose `unsafe` blocks carry `// SAFETY:` comments.
2. **Bindings are written or generated here and checked in.** No `bindgen`, no `-sys` crates.
   `tools/waylandgen` generates Rust bindings from the Wayland protocol XML (MIT), and `tools/vkgen`
   generates Vulkan bindings from `vk.xml` (Apache-2.0 or MIT), both checked like `syscallgen`
   (`cargo xtask waylandgen --check`). Every bound C structure has a layout test against a C
   compiler on the build server, as `abi-check` does for the kernel ABI.
3. **The inventory**, `desk/ffi/inventory.txt` (a plain table `xtask` reads), lists every C library with its version, license,
   transitive closure, and the processes that load it. `cargo xtask check` fails on a link outside
   it, and adding an entry is a reviewed change.
4. **Untrusted input meets C parsers only in a sandbox.** libucl, image decoders, template
   rendering, and `glslang` run in the Capsicum worker (§5), never in `scoriad` or `scoria-sup`.

The kernel, the loader, and every `lib/` crate stay under ADR-0002 unchanged. A crate that wraps a
C library lives in `desk/`.

| Library | License | Used for | Loaded by |
|---|---|---|---|
| libwayland-server, libwayland-client | MIT | Wayland wire protocol and object lifetimes | `scoriad`; satellites |
| Vulkan loader and Mesa's drivers (RADV, panvk, Venus, lavapipe) | MIT, Apache-2.0 | All rendering | `scoriad`; satellites |
| libdrm, libgbm | MIT | Mode setting, scanout buffer allocation | `scoriad` |
| libliftoff | MIT | Plane allocation for direct scanout (§6.1) | `scoriad` |
| libdisplay-info | MIT | EDID and DisplayID: output identity, variable refresh range, high dynamic range metadata | `scoriad` |
| libseat | MIT | Seat and device access through seatd | `scoriad` |
| libinput (with libevdev, mtdev, libudev-devd) | MIT | Pointer acceleration, touchpads, tablets | `scoriad` |
| libxkbcommon | MIT | Keymaps | `scoriad`; satellites |
| libucl | BSD-2-Clause | Configuration syntax | Parser worker only |
| FreeType, HarfBuzz, fontconfig | FreeType License, MIT | Fonts, shaping, glyphs | `scoria-ui`; `scoriad` (banners, HUD) |
| glslang | BSD-3-Clause and others (recorded in the inventory) | User shaders to SPIR-V (M-D.6) | Parser worker only |
| libpam (base) | BSD | Unlocking | `scoria-lock` |

libinput links `libudev` even through its path backend, so FreeBSD's `libudev-devd` is in its
closure. Scoria never calls `libudev` and runs no udev daemon; devices come from devd and libinput's
path context (§3).

Not used: **wlroots**, because Scoria's differences live in the four layers wlroots would provide
(renderer, scene, frame scheduling, input routing), its Vulkan renderer is secondary, and its API
breaks every release. **Smithay**, because it is a crates.io tree with GLES and pixman renderers
only. `xwayland-satellite` and Xwayland are separate packages that `scoria-sup` supervises, not
code Scoria links.

### 2.2 Rust

A compositor's object graph is cyclic (surfaces, views, outputs, seats), which in Rust usually
becomes `Rc<RefCell<…>>` or an entity system. Scoria does neither:

- **One owner.** Compositor state lives in typed slabs (`Slab<Surface>`, `Slab<View>`,
  `Slab<Output>`, `Slab<Seat>`) owned by one `State` on the event-loop thread. References between
  objects are `Handle<T>`: an index and a generation in a newtype. A stale handle is a lookup that
  fails, never a dangling pointer, and a client protocol error rather than a panic.
- **One place where C calls back.** libwayland's request handlers are trampolines generated by
  `waylandgen`. A resource's user data holds a handle, never a Rust reference. The trampolines
  recover `&mut State` from the display's user data for the length of one dispatch, and that
  aliasing argument is made once, in `desk/ffi/wayland`.
- **Arenas with lifetimes.** A per-frame bump arena for the render graph and damage, reset every
  frame, and a per-reload arena for configuration snapshots.
- **ENGINEERING.md applies.** Enums for states, newtypes for IDs and units, typestate for
  lifecycles (a `Session<Active>` is the only thing that can open a device), `Result` on every path
  that touches client, device, or file input, and no panics on untrusted input: every Wayland
  client, IPC peer, configuration file, font, image, EDID, and input report is untrusted. `unsafe` is
  allowed only in `desk/ffi/*`, the generated bindings, and the shared-memory module. Satellite
  crates are `#![forbid(unsafe_code)]`.
- **Fuzzing.** The CBOR codec, every IPC decoder, the configuration snapshot decoder, and the
  request handlers behind each FFI crate are fuzzed on the host.

### 2.3 Toolchain and layout

- Pinned stable Rust (ADR-0003). `desk/` uses `std` and targets FreeBSD 15.1. It links package
  libraries, so it builds on FreeBSD: on the build server (`cargo xtask --remote`), and later on
  AbyssBSD itself. The Linux host type-checks `desk/` and runs the host tests of its `lib/` crates.
- `x86_64-unknown-freebsd` ships a prebuilt `std`. **`aarch64-unknown-freebsd` does not** (Tier 3),
  and ADR-0003 rules out `-Zbuild-std`. Until that is settled (§14 question 2), `desk/` builds for
  amd64 only, and the rule that every change builds for both architectures applies to its `lib/`
  crates.
- Pure code lives in `lib/`, `no_std` with `alloc`, host-tested: `lib/cbor` (the wire codec),
  `lib/tiling` (the layout solver), and `lib/contrast` (the contrast floors of §8.2 rule 10).
- Shaders are GLSL, compiled to SPIR-V when they change, and the SPIR-V is checked in, so a build
  needs no shader compiler.

```
desk/
  scoriad/      compositor: event loop, session, backends (kms, headless, nested), scene,
                layout, input policy, protocols, renderer, capture
  sup/          scoria-sup: supervisor, configuration authority, launch profiles
  worker/       scoria-worker: the Capsicum parser (libucl, templates, palettes, glslang)
  ipc/          the bus: framing, roles, message types generated from one schema
  ctl/          scoriactl
  ui/           scoria-ui: the satellites' toolkit (signed-distance-field primitives, text,
                the widget host)
  bar/ launch/ notify/ lock/      the satellites
  greetd/ greet/                  login: the root PAM daemon and the greeter (INSTALLER.md §6)
  install/ installd/              the installer and its root service (INSTALLER.md §5)
  obs/          scoria-obs: OBS control over its WebSocket protocol
  ffi/<lib>/    one crate per C library, and inventory.txt
  shaders/      GLSL sources and their SPIR-V
tools/waylandgen, tools/vkgen
```

### 2.4 Distribution

`cargo xtask pkg scoria` produces a FreeBSD package that installs under `/usr/local` (binaries in
`/usr/local/bin` and `/usr/local/libexec/scoria/`, data in `/usr/local/share/scoria/`, the system
configuration in `/usr/local/etc/scoria/`) and depends on the inventory's packages. The same
package installs on AbyssBSD and on stock FreeBSD 15.1, which is what makes §12's comparison
possible. Whether it also becomes part of a distribution set is §14 question 5.

## 3. Linux plumbing and its replacements

| Subsystem | Linux desktop | Scoria | Notes |
|---|---|---|---|
| Event loop | epoll, signalfd, timerfd | **kqueue** | Sockets, `EVFILT_TIMER`, signals, `EVFILT_PROC`, vnode watches. libwayland's loop descriptor (an epoll-shim kqueue) is nested in the main kqueue with `EVFILT_READ` |
| Hotplug and input | udevd, netlink | **devd's socket** (`/var/run/devd.seqpacket.pipe`, non-blocking) and **libinput's path context** | Direct Rendering Manager hotplug and `DEVFS` events for `/dev/input/event*`; devices are added with `libinput_path_add_device()` |
| Seat and terminal switching | logind | **seatd and libseat** | Opens `/dev/dri/card*` and `/dev/input/event*` without D-Bus |
| Runtime directory | logind's `/run/user/<uid>` | **base `pam_xdg(8)`** | `/var/run/xdg/<user>`, removed when the last session ends, so `scoria-sup` holds its login session open for the desktop's lifetime |
| IPC | D-Bus | **CBOR over `SOCK_SEQPACKET`** | Peers checked with `LOCAL_PEERCRED`. Large payloads go as sealed shared memory over `SCM_RIGHTS` (§7) |
| Sandboxing and limits | bubblewrap, cgroups | **Capsicum and `setrlimit(2)`** | Parsers enter capability mode. Launched programs get lowered limits; `rctl(8)` rules stay the administrator's |
| Audio control | PipeWire, PulseAudio | **`mixer(3)`** over the Open Sound System devices, `virtual_oss(8)` for routing | `/dev/mixer*` posts no kqueue events, so the bar combines compositor pushes, devd attach events, and a slow timer |
| Notifications, portals | freedesktop services | **Scoria IPC**, and an opt-in bridge | The bridge brings back D-Bus, and its screencast portal brings back PipeWire. Neither is needed by the core session |
| Configuration | scattered files | **`~/.config/scoria/config.ucl`** over `/usr/local/etc/scoria/` | libucl from packages; base's `libprivateucl` is private to base |

## 4. Architecture

### 4.1 Overview

`libvulkan` and Mesa open device nodes, driver manifests, and shader caches by path, and
`execve(2)` by path is forbidden in capability mode. So Scoria separates privileges instead of
putting the compositor in capability mode. Two sockets carry the IPC: the session bus,
`$XDG_RUNTIME_DIR/scoria.sock`, owned by `scoria-sup`, and the compositor's endpoint,
`scoria-compositor.sock`, owned by `scoriad` and found through the bus, so compositor queries never
pass through the supervisor.

```
                ┌───────────────────────────────────────────────────────┐
  config.ucl ──▶│ scoria-sup: supervisor and configuration authority    │──▶ programs (pdfork,
  (kqueue vnode)│ no GPU, no Wayland; spawns and restarts everything    │    setpgid, setrlimit,
                └──┬──────────────┬─────────────────────────────────┬───┘    cpuset)
                   │ fork, cap_enter, rights-limited directory fds   │ SOCK_SEQPACKET bus
                   ▼                                                 │ (CBOR, sealed shm)
           ┌───────────────┐                                         │
           │ scoria-worker │ libucl, templates, palettes, glslang    │
           └───────────────┘                                         ▼
  ┌──────────────────────────────────────────────────────────────────────────────┐
  │ scoriad: Wayland server · layout engine · Vulkan 1.3 renderer · capture       │
  │          kqueue loop · devd · libinput · libseat · libliftoff                 │
  └──────┬────────────────────────────┬───────────────────────────────┬──────────┘
         │ /dev/dri/*                 │ /dev/input/event*             │ Wayland
         ▼                            ▼                               ▼
     sys/drm (ADR-0025)           sys/evdev                 scoria-bar · scoria-launch ·
                                                            scoria-notify · scoria-lock ·
                                                            applications, xwayland-satellite
```

Each satellite is an ordinary Wayland client with its own `VkDevice`. Satellites share the
`scoria-ui` crate, not the compositor's GPU pipeline.

### 4.2 Components

| Process | Privilege | Sandbox | If it dies |
|---|---|---|---|
| `scoria-sup` | User; the session's root of trust | None (no graphics, no Wayland, no parsing) | The session ends |
| `scoria-worker` | User, short-lived | Capsicum, always | The reload fails; the old configuration stays |
| `scoriad` | User, with device descriptors from seatd | None (Mesa opens files by path); its parsers are generated and fuzzed | Clients lose the display; `scoria-sup` restarts it, and satellites reconnect |
| Satellites | User | None yet; capability mode once `scoria-ui` loads nothing by path after start | Restarted with backoff, compositor state kept |
| `scoria-lock` | User; PAM through a setuid helper | — | The session **stays locked** (§4.2 item 6) |
| `xwayland-satellite`, providers | User | Limits from their profile | Restarted under the same policy |
| `scoria-greetd` | root, from `rc(8)` | None (small: PAM, sessions, one socket) | `rc` restarts it; running sessions continue |
| `scoria-greet`, greeter `scoriad` | `_scoriagreet` | Restricted session: no bus, no `exec` | `scoria-greetd` restarts them |
| `scoria-installd` | root, live media only | None (small; validates every plan) | The install stops at the failed step and reports it |

1. **`scoria-sup`** supervises `scoriad`, the satellites, `xwayland-satellite`, and widget data
   providers (§9.2), with `pdfork(2)` and a restart policy that keeps compositor state. It runs
   bound commands (`exec …`) in their own process group with optional per-profile `setrlimit(2)`
   caps and `cpuset(1)` affinity. It opens the configuration directories, limits them with
   `cap_rights_limit(2)` to `CAP_LOOKUP | CAP_READ | CAP_FSTAT`, and hands them to the worker (§5).
2. **`scoriad`** is the Wayland server, layout engine, renderer, and capture path. It owns the
   window tree, workspaces, keyboard shortcuts, libinput devices, and output hotplug. Its
   protocols, with the step that brings each (§15):

   | Group | Protocols |
   |---|---|
   | Core and windows | `wl_compositor`, `wl_shm`, `wl_seat`, `wl_data_device`, `xdg_shell` [M-D.1]; `zxdg_decoration_manager_v1`, `xdg-activation-v1`, `xdg_dialog_v1`, `wp_cursor_shape_manager_v1`, `xdg_toplevel_icon_manager_v1` [M-D.2]; `zwp_text_input_v3` [M-D.6] |
   | Games and input | `zwp_pointer_constraints_v1`, `zwp_relative_pointer_manager_v1`, `zwp_keyboard_shortcuts_inhibit_manager_v1` (with reserved chords), `zwp_idle_inhibit_manager_v1`, `wp_content_type_manager_v1`, `zwp_pointer_gestures_v1` [M-D.2]; `wp_pointer_warp_v1` once stable [M-D.3]; `zwp_tablet_manager_v2` [M-D.6] |
   | Frame timing | `wp_presentation` [M-D.1, used internally from the start]; `wp_fifo_manager_v1`, `wp_commit_timing_manager_v1` [M-D.2] |
   | Desktop and security | `zwlr_layer_shell_v1` [M-D.2]; `ext-foreign-toplevel-list-v1`, `ext-workspace-v1`, `ext-session-lock-v1`, `ext-idle-notify-v1` [M-D.3]; `ext-image-capture-source-v1`, `ext-image-copy-capture-v1` [M-D.3]; `ext-data-control-v1`, `wp_security_context_v1`, `zwp_input_method_v2`, `zwp_virtual_keyboard_v1` [M-D.6] |
   | Graphics and display | `wp_viewporter`, `wp_single_pixel_buffer_manager_v1`, `wp_fractional_scale_v1`, `wp_alpha_modifier_v1` [M-D.2]; `wp_linux_dmabuf_v1`, `wp_linux_drm_syncobj_v1` (only when the kernel has it, §6.2) [M-D.3]; `wp_tearing_control_v1` (only with atomic asynchronous flips) [M-D.4]; `wp_color_management_v1` [M-D.6] |
   | X11 | Rootless Xwayland through `xwayland-satellite`, which runs Xwayland as an ordinary Wayland client in its own process, so `scoriad` has no X11 window manager [M-D.3; most Wine titles are still X11 clients] |

3. **`scoria-bar`** draws on a `zwlr_layer_shell_v1` surface through `scoria-ui`. It is one instance
   of the widget host (§9.2) and also hosts the quick-settings menu as a popup. Its collectors read
   the kernel directly, with no shell scripts:
   - processor and memory: `kern.cp_time` and `vm.stats.vm.*`, on a timer;
   - battery: `hw.acpi.battery.*`, refreshed on devd's `ACPI` `ACAD` and `CMBAT` notifications and
     a slow timer;
   - network: `ifi_link_state` in the `AF_LINK` entries of `getifaddrs(3)` (`SIOCGIFMEDIA` for
     media detail), pushed by a `PF_ROUTE` socket (`RTM_IFINFO`, `RTM_NEWADDR`, `RTM_DELADDR`) in
     kqueue;
   - volume: `mixer(3)`, updated on the compositor's hotkey events and a slow timer.
4. **`scoria-launch`** indexes `.desktop` files, `$PATH`, and configured aliases, and asks
   `scoria-sup` to start what is chosen. It is also the **command palette**: every bindable action
   is listed by name with its current keybinding, so using the palette teaches the shortcut.
   Results are ranked by frecency.
5. **`scoria-notify`** shows notifications and the transient volume and brightness displays, and
   applies the active focus profile (`normal`, `dnd`, `game`, `stream`). Under `game` and
   `stream`, notifications are held, stripped of content, or shown only on another output. They
   never take keyboard focus.
6. **`scoria-lock`** uses `ext-session-lock-v1`, so `scoriad` guarantees no frame leaks while
   locked. If the locker crashes, the session stays locked (a solid fill) until `scoria-sup`
   restarts it and it takes the lock again. A crash never unlocks. Idle locking comes from
   `ext-idle-notify-v1`. OpenPAM's `pam_unix` cannot read `/etc/master.passwd` unprivileged, so the
   locker's PAM policy (`/usr/local/etc/pam.d/scoria-lock`) uses `unix-selfauth-helper`, a small
   setuid helper run through `pam_exec`, as FreeBSD's `swaylock` port does.
7. **OBS integration** (`scoria-obs`). The Odin design's `obs-scoria` was an OBS source plugin
   that captured over `ext-image-copy-capture-v1` as dma-bufs, exposed OBS's controls (scenes,
   start and stop, replay save, markers) as Scoria actions, and reported OBS's state back to drive
   the `stream` profile. OBS's plugin interface is GPL, and this project never consults GPL code
   (ADR-0007), so that plugin cannot be written here. How Scoria reaches OBS instead is §14
   question 9; the capture side is the standard protocol either way, so it needs neither PipeWire
   nor the portal bridge.
8. **`scoria-dbus-bridge`** (opt-in, M-D.6) serves `org.freedesktop.Notifications`, the tray
   (StatusNotifierItem), portals, global shortcuts, and accessibility (AT-SPI) for GTK and Qt
   programs.
9. **`scoria-greetd`** and **`scoria-greet`** are the login path: a root daemon that runs PAM
   (with `pam_xdg`), opens sessions, and logs the owner in automatically after the loader unlocked
   an encrypted disk, and an unprivileged greeter in a restricted `scoriad`. They also keep the
   owner's password and the disk passphrase one ([INSTALLER.md](INSTALLER.md) §6, §7).
10. **`scoria-install`** and **`scoria-installd`** install AbyssBSD from the live medium: an
    unprivileged `scoria-ui` interface that builds a plan, and a root service that validates and
    runs it ([INSTALLER.md](INSTALLER.md) §5).

## 5. Configuration

No component parses its own file. `scoria-sup` owns `~/.config/scoria/config.ucl`, falling back to
`/usr/local/etc/scoria/config.ucl`.

1. **Watch.** `scoria-sup` watches the directory and the files in use with `EVFILT_VNODE`
   (`NOTE_WRITE | NOTE_RENAME | NOTE_DELETE`). Watching the directory catches an editor's atomic
   save (write a temporary file, `rename(2)`), and the file is reopened with `openat(2)`.
2. **Parse in a sandbox.** On a change or `scoriactl reload`, `scoria-sup` forks `scoria-worker`
   with the rights-limited directory descriptors. The worker enters `cap_enter(2)` and parses and
   validates the whole tree against the schema. libucl's own `.include` opens absolute paths, which
   capability mode forbids, so the worker registers its own `.include` macro that resolves paths
   with `openat(2)` beneath the directory descriptors. If validation fails, the running
   configuration is untouched and the error (file, line, key) is shown by `scoria-notify`, or by
   `scoriad` as a banner that takes no focus when the notifier is not running.
3. **Seal and distribute.** The validated snapshot is written to sealable anonymous memory
   (`memfd_create(MFD_ALLOW_SEALING)`, or `shm_open2(SHM_ANON, …, SHM_ALLOW_SEALING)`; a plain
   `SHM_ANON` object cannot be sealed). With every writable mapping gone, `scoria-sup` adds
   `F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL`, limits the descriptor to
   `CAP_MMAP_R | CAP_FSTAT`, and sends it over `SCM_RIGHTS` in **two phases**: each component
   decodes and applies it in trial (PREPARE) and answers OK or REJECT; on all OKs within 500 ms it
   is committed, otherwise aborted. Themes, fonts, keybindings, and layouts change together.

```ucl
theme {
    font = "JetBrains Mono:size=11";      # resolved through fontconfig
    gaps_inner = 8; gaps_outer = 12; border_width = 2;
    palette { bg0 = "#181a1f"; fg0 = "#d8dee9"; cyan = "#88c0d0"; red = "#bf616a"; }
    roles   { surface = "bg0"; on_surface = "fg0"; accent = "cyan"; urgent = "red"; }
}

compositor {
    layout = "scroll";            # scroll | master_stack | bsp | monocle (§10)
    focus_follows_mouse = true;   # suspended while a pointer constraint is active
    vrr = "game";                 # off | on | game
    tearing = "if_requested";     # never | if_requested; needs atomic asynchronous flips
    direct_scanout = true;
}

input {
    keyboard { layout = "us"; repeat_rate = 35; repeat_delay = 250; }
    pointer  { accel_profile = "flat"; speed = 0.0; }
}

binds {
    "Mod4+Return" = "exec foot";          "Mod4+d"     = "launcher toggle";
    "Mod4+Shift+q" = "window close";      "Mod4+Space" = "palette toggle";
    "Mod4+h" = "focus left";              "Mod4+l"     = "focus right";
    "Mod4+Tab" = "overview toggle";       "Mod4+z"     = "layout undo";
    "XF86AudioRaiseVolume" = "audio volume +5%";
}

# Chords no client can inhibit (§8.2 rule 3). Mod4+Escape is built in.
reserved_binds {
    "Mod4+Escape" = "input release";
    "Mod4+F9" = "obs scene next";         "Mod4+F10" = "mic toggle";
}

bar {                                     # a widget tree (§9.2)
    position = "top"; height = 28;
    left   { workspaces {} mode_indicator {} }
    center { label { bind = "window.title"; } }
    right  { meter { bind = "audio.volume"; } label { bind = "net.link"; }
             meter { bind = "acpi.battery"; } label { bind = "clock"; format = "%H:%M"; } }
}
```

## 6. Rendering

### 6.1 Display paths

`scoriad` uses only what [GRAPHICS.md](GRAPHICS.md) defines: the Direct Rendering Manager interface
as FreeBSD's ports see it, implemented natively (ADR-0025), with Mesa's Vulkan drivers from
packages. It never assumes a driver or a kernel version. Every feature is **probed at run time**
(`drmGetCap()`, plane properties, test-only atomic commits), so the same package behaves correctly
on AbyssBSD and on FreeBSD with `drm-kmod`. `scoriad --probe` prints the result, and `tests/desk/`
holds the expected table per machine.

| Machine | Display driver | Vulkan driver | Arrives |
|---|---|---|---|
| Headless, nested | none | lavapipe | P6 |
| QEMU and the reference machines' firmware framebuffers | `gop-kms` | lavapipe, copied into a dumb buffer | M-G.1 |
| QEMU | `virtio-gpu` 2D | lavapipe, copied | M-G.2 |
| QEMU | `virtio-gpu` 3D | Venus | M-G.3 |
| Orange Pi 6 Plus | Sky1 display | panvk (`panthor`) | M-G.4, M-G.5 |
| X870E, Z690-A | RDNA2 | RADV | M-G.6 |

- **Hardware path.** Output modes are set with atomic commits (`drmModeAtomicCommit`). `scoriad`
  renders into scanout buffers allocated with GBM, exported as dma-bufs, and imported into Vulkan
  with `VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier`.
- **Software path.** With no render device (`gop-kms`, `virtio-gpu` 2D), `scoriad` renders with
  lavapipe into a host-visible image and copies the damaged rectangles into a dumb buffer for
  scanout. It is the software tier of the one Vulkan renderer, not a second renderer, and the
  only path that copies.
- **Direct scanout.** A fullscreen opaque client dma-buf with a usable modifier goes straight onto a
  primary or overlay plane, bypassing composition. libliftoff chooses planes by trying layouts
  with test-only commits.
- **Cursor.** The pointer goes on a cursor plane, so moving it never redraws a frame.
- **Output identity.** libdisplay-info parses EDID and DisplayID for the serial that keys each
  output's remembered layout (§8.2 rule 8), the variable refresh range, and high dynamic range
  metadata.
- **Client buffers.** `wp_linux_dmabuf_v1` buffers become `VkImage`s with no copy. `wl_shm` buffers
  are uploaded through a ring of host-visible, host-coherent staging buffers.

### 6.2 Frames, synchronization, damage

- **Dynamic rendering** (Vulkan 1.3 core): no render passes or framebuffers to rebuild when outputs
  and surfaces change.
- **Explicit synchronization.** `wp_linux_drm_syncobj_v1` maps to Vulkan timeline semaphores
  imported from and exported to the kernel's syncobjs, with `drmSyncobjTransfer`, and with
  `DRM_IOCTL_SYNCOBJ_EVENTFD` and `eventfd(2)` for wakeups in kqueue. The protocol is offered only
  after `DRM_CAP_SYNCOBJ_TIMELINE` is set and a trial `SYNCOBJ_EVENTFD` succeeds; otherwise
  `scoriad` uses implicit synchronization. `compositor.explicit_sync = false` overrides it for a
  driver regression.
- **Tearing.** `wp_tearing_control_v1` needs asynchronous flips on atomic commits
  (`DRM_MODE_PAGE_FLIP_ASYNC`, gated by `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`). Without that
  capability the protocol is not offered.
- **Damage.** `scoriad` tracks surface damage, layout changes, and the age of each buffer in its own
  ring of scanout buffers (the mode-setting path has no Vulkan swapchain to do it). With no damage,
  the loop sleeps in `kevent()` and uses no processor or GPU time. With damage, rendering is
  scissored to the union of dirty rectangles, which also go to the kernel in the
  `FB_DAMAGE_CLIPS` plane property where the driver has it. Blurred surfaces widen their damage by
  the blur radius.
- **Shader library.** Borders, rounded corners, shadows, and dual-Kawase blur are one bindless
  signed-distance-field shader, so a frame changes pipelines rarely. The satellites are separate
  processes with their own devices; they share the shader sources through `scoria-ui`, not the
  pipeline.
- **Text.** fontconfig resolves fonts; FreeType and HarfBuzz fill glyph atlases, color emoji
  included. Subpixel antialiasing is used only for opaque, untransformed text at integer scale;
  translucent, blurred, rotated, or fractionally scaled surfaces get grayscale.

## 7. IPC and `scoriactl`

- **Framing.** A control request or event fits in one `SOCK_SEQPACKET` record. Records are capped at
  4 KiB inline, whatever `net.local.seqpacket.recvspace` and `maxseqpacket` are; anything larger
  (state dumps, configuration snapshots, the launcher's index) goes as sealed, rights-limited
  shared memory in `SCM_RIGHTS`.
- **Record boundaries need `MSG_EOR`.** On FreeBSD a `SOCK_SEQPACKET` socket joins consecutive sends
  into one record unless each is sent with `MSG_EOR` (measured on 15.1: sends of 4, 2, and 1 bytes
  arrive as one 7-byte record without it, three records with it). Every Scoria sender sets
  `MSG_EOR`, and no receiver assumes one record per `recv`: a decoder that finds bytes left over or
  missing closes the connection. devd's records are split on newlines the same way. This kernel
  must behave as FreeBSD does here (§11.2).
- **Peers and roles.** Each connection is checked on `accept(2)` with `LOCAL_PEERCRED` for the same
  UID. A peer declares a role (`satellite`, `provider`, `ctl`, `plugin`), verified by process ID
  for processes `scoria-sup` started, and each role has an allowlist of message types; only
  satellites may claim the privacy layer, for example.
- **Schema.** Message types are defined once and generated into Rust encoders and decoders over
  `lib/cbor`. Decoders ignore unknown map keys. A subscriber that falls behind is dropped with
  `lagged` instead of slowing the compositor.

| Command | Does |
|---|---|
| `scoriactl state --json` | The live window tree, outputs, and workspaces |
| `scoriactl reload` | Validate in the worker, then apply atomically everywhere (§5) |
| `scoriactl dispatch <action>`, `dispatch --list` | Run any bindable action; list every action with its name and keybinding |
| `scoriactl subscribe <event>` | Stream `window_focus`, `workspace_change`, `output_hotplug`, `profile_change`, `capture_start`, `capture_stop` |
| `scoriactl try <file|key=value> [--revert-after 15s]` | Apply, and revert unless confirmed: "Keep these display settings?" for bindings, outputs, and input |
| `scoriactl set`, `get` | One configuration key; bindings, outputs, and input go through `try` |
| `scoriactl undo`, `redo` | Walk the layout history |
| `scoriactl theme set <token>=<value>`, `commit`, `discard` | Live, unsaved theme changes |
| `scoriactl publish <key> <json>` | A provider's data, converted to CBOR |
| `scoriactl keys` | The effective keymap, as the hold-Mod cheat sheet shows it |
| `scoriactl hud toggle` | The frame-timing display (§9.1) |
| `scoriactl rules test <app_id|pid>` | Which rules and profile apply to a window, and why |
| `scoriactl debug hitmap` | Draw every input hit region, to audit target sizes (§8.5) |
| `scoriactl showcase` | Screenshot mode (§9.2) |

## 8. Interaction

Desktop interface research peaked between about 1978 and 1997, when every decision was tested on
real users with stopwatches. It measured human motor control, perception, and memory, which have not
changed; pixel density, refresh rates, and fashion have. Scoria treats a rule derived from that
research as a requirement and tests it (§8.5).

### 8.1 The canon

| Principle | Source | In Scoria |
|---|---|---|
| **Fitts's law**: acquisition time grows with distance and shrinks with target width | Fitts 1954; Card, English & Burr 1978; MacKenzie 1992 | Screen edges and corners are infinitely large targets, so the most used targets go there. Hit regions are never smaller than they look |
| **Hick–Hyman law**: decision time grows with the log of the number of choices | Hick 1952; Hyman 1953 | The launcher and palette rank by frecency and preselect the top result. Menus are short and grouped |
| **Steering law**: moving through a tunnel costs its length over its width | Accot & Zhai 1997 | No cascading submenus. Hierarchy is radial menus or flat searchable lists |
| **Pie and marking menus** | Callahan et al. 1988; Kurtenbach & Buxton 1993 | The action wheel (§9.1): hold to see it after 300 ms, flick to choose without it |
| **Response-time limits**: 0.1 s feels instant, 1 s keeps the flow, 10 s loses attention; below 400 ms productivity rises sharply | Miller 1968; Doherty & Thadani 1982 | Feedback within one frame; progress for anything over 1 s; animations capped and never delaying input |
| **Keystroke-level model** | Card, Moran & Newell 1980, 1983 | Core tasks have keystroke budgets that tests check |
| **Direct manipulation** | Shneiderman 1983; Hutchins, Hollan & Norman 1985 | Drags preview their drop; gestures track 1:1; every layout operation can be undone |
| **Apple's guidelines**: control, feedback, forgiveness, perceived stability, consistency, modelessness | Apple 1987 | **Windows do not move unless the user moved them** (rule 8), which is why `scroll` is the default layout |
| **Universal commands** | Smith et al. 1982 (Xerox Star) | One verb vocabulary for bindings, `scoriactl dispatch`, the palette, and menus |
| **Keyboard completeness** | IBM CUA 1987, 1991 | Every pointer or gesture action has a binding and an IPC verb |
| **Tiled versus overlapping windows** | Bly & Rosenberg 1986 | Tiling by default, with floating and monocle first-class. Fixed-size content (games, dialogs, picture-in-picture) is never forced into a tile |
| **Rooms** | Henderson & Card 1986 | Workspaces are named task contexts, each with its own layout; windows can be shared across them |
| **Mode errors** | Sellen, Kurtenbach & Buxton 1992; Raskin 2000 | Held-modifier quasimodes (Mod+drag) over latched modes; latched modes impossible to miss (rule 9) |
| **Golden rules and heuristics** | Shneiderman 1986; Nielsen & Molich 1990; Nielsen 1994 | The release checklist (§8.5); notifications never take focus; status always visible |
| **Mapping, feedback, constraints** | Norman 1988 | `Mod+h/j/k/l` and the arrows move in the direction pressed, in every layout and in the overview |
| **Discoverability** | Sullivan 1996; Tognazzini 1992 | A visible launcher target in the top-left corner. Holding Mod alone for 600 ms (no other input, never under Game Focus) shows a cheat sheet generated from the live configuration |

### 8.2 Floors

`scoriad` and the validator enforce these. Themes, rules, and plugins can only raise a floor. The
one exception is showcase mode (§9.2), which is temporary.

1. **Edges are infinite.** A layer-shell surface anchored to an edge receives pointer input up to
   the edge pixel, even when drawn with a margin: events in the margin go to it, clamped to its
   edge. Always on for satellites, opt-in (`edge_hit = true`) for other clients. Where two outputs
   share an edge, an edge resistance (8 px by default) keeps an inner bar reachable without
   trapping the pointer.
2. **Corners are prime.** Top-left click opens the launcher; an optional top-left push opens the
   overview. Top-right opens quick settings, whose first section is the notification center.
   Bottom corners are the user's. Clicks need no dwell. Hover activation needs a pressure or
   velocity threshold and is off under Game Focus. Hit zones extend into a display's rounded
   corners.
3. **Focus is never stolen.** Activation needs an `xdg-activation-v1` token tied to recent input;
   without one, a window only gets an urgency hint. While a fullscreen or pointer-constrained client
   has focus, nothing takes the keyboard, Scoria's own interface included, unless the user asks. Reserved
   chords (`Mod4+Escape` built in) cannot be inhibited by any client: like a virtual machine's host
   key, the user can always take the machine back.
4. **Animations never delay input.** Layout changes are atomic and immediate; animation is only a
   render-side interpolation toward the new state, and input targets the new state at once.
   Animations are springs that can be retargeted mid-flight, are skipped on auto-repeat, are capped
   by `animations.max_duration` (at most 400 ms), and are off under `reduced_motion`.
5. **Feedback within one frame.** Every binding changes something visible or shows an on-screen
   display. A binding that does nothing (`focus left` at the leftmost window) pulses the border.
6. **Three ways to every action**: a binding, an IPC verb, and a visible path (palette, bar, or
   menu). Gestures are accelerators, never the only path. Icon-only controls show a tooltip within
   500 ms, and holding Mod shows keybinding labels.
7. **Forgiveness.** Layout operations go into an undo ring (`Mod4+z`). Risky changes (bindings,
   outputs, input devices) revert after 15 s unless confirmed. Irreversible actions (`window kill`,
   `session exit`, disabling the last output) use **hold-to-confirm** (600 ms with a visible fill),
   not dialogs. `window close` asks the client (`xdg_toplevel.close`); `window kill` sends
   `SIGKILL`.
8. **Perceived stability.** A new window never moves or resizes windows the user did not touch,
   except in layouts that say so (binary space partition, master-stack), where the change is previewed. Title changes
   never reflow. Output hotplug restores each monitor's workspaces by EDID serial, not connector
   name. A game's resolution change is emulated by scaling through `wp_viewporter` (Xwayland's
   RandR emulation for X11 games), never by a real mode set, so switching away never blanks the
   screen.
9. **Modes are visible.** A latched mode (`resize`, `passthrough`, `stage-edit`) shows a bar
   indicator, recolors the focused border, and changes the cursor. `Escape` always leaves it, and so
   does 10 s of idleness.
10. **Legibility and focus.** The validator rejects text below 4.5:1 contrast against its worst-case
    background, and a focus indicator below 3:1 against unfocused borders or thinner than one
    physical pixel. Translucent surfaces are judged against their worst-case backdrop, and
    `contrast_guard` raises their opacity instead of rejecting the theme.
11. **Privacy status is always visible.** The capture and microphone indicators (§9.3) are drawn by
    `scoriad` on a reserved layer that clients cannot cover and themes cannot hide.
12. **Desktop density.** Bars are 24–28 px at 1× scale, menus have labeled rows, and there are no
    grids of unlabeled toggles, full-screen app grids, or hamburger menus.

### 8.3 Latency budget

The frame-timing display (§9.1) measures this continuously:

| Path | Latency the compositor adds |
|---|---|
| Fullscreen game, direct scanout, tearing allowed | About zero: the client's buffer goes to the plane with an asynchronous flip |
| Fullscreen game, direct scanout, variable refresh | ≤ 1 ms: the flip is issued when the client commits |
| Composited window | ≤ 1 refresh interval. Composition starts as late as possible: the render deadline is predicted from GPU timestamp history plus a margin, as Mutter and KWin do |
| Cursor | Decoupled: the cursor plane moves at input rate |

Input is handled as soon as `kevent()` returns it, never batched to frame boundaries. Each output
has its own frame clock, so a 60 Hz monitor never paces a 240 Hz one. The event-loop and render
thread may run at real-time priority through `mac_priority(4)`'s `realtime` group when the
administrator allows it.

### 8.4 Input devices and gestures

- **Trackpad gestures, the common set only.** Two-finger scroll and pinch pass to clients
  (`zwp_pointer_gestures_v1`). Three-finger horizontal swipe moves between workspaces (or columns
  in `scroll`), tracking 1:1 with momentum. Three-finger vertical swipe opens and closes the
  overview. Tap-to-click and two-finger secondary click are libinput's. Enabling libinput's
  three-finger drag moves the swipes to four fingers.
- **Not supported:** edge swipes, custom or drawn gestures, gesture-only actions, and touch modes
  that inflate the interface.
- **Touchscreens** are an absolute pointer.
- **Game controllers** are game input first. FreeBSD's `hgame(4)`, `xb360gp(4)`, and `ps4dshock(4)`
  provide evdev nodes, and Scoria installs a `devfs.rules` snippet giving the session user access.
  The launcher, overview, and action wheel can optionally be driven with a controller at normal
  desktop density.

### 8.5 Validation

- **Hit map.** `scoriactl debug hitmap`, and a test that edge and corner targets reach the physical
  edge and that resize margins extend at least 8 logical px outside the visible border.
- **Keystroke budgets.** Scripted tests with chord limits: any of the 50 most-used programs launched
  by name in at most 4 keystrokes plus Enter; a window sent to workspace *N* in one chord; any action
  reached in the palette in at most 5 keystrokes.
- **Latency rig.** A photodiode input-to-photon harness on the reference machines, gated on
  regressions against §8.3.
- **Heuristic review.** Every user-visible feature is reviewed against §8.2 before release.

## 9. The three facets

### 9.1 Gamer: out of the frame path

**Game Focus** is a compositor state, entered when a window reports `wp_content_type_v1` `game`,
matches a rule (`app_id = "steam_app_*"`, or an `app_id` that `xwayland-satellite` derived from an
X11 `WM_CLASS` such as `*.exe`), or is fullscreen with a pointer constraint. Rules can force it
either way. While it is active on an output:

| Effect | Why |
|---|---|
| Direct scanout preferred; blur, shadows, corner masks, and post-process shaders suspended on that output | Latency outranks expression (§1.2) |
| Variable refresh per policy (`compositor.vrr = "game"`); tearing honored when the client asks and the rule allows | The gamer's choice, where the kernel can do it |
| Notifications held, or shown as a strip on another output that takes no focus | §8.2 rule 3 |
| Hot corners and edge reveals off; focus-follows-mouse suspended under a pointer constraint | No accidental triggers mid-flick |
| The launch profile applies `cpuset(1)` affinity and, where `mac_priority(4)` allows, higher priority | FreeBSD's own tuning, per user, unprivileged by default |
| Idle locking inhibited while the game holds an idle inhibitor, or controller input arrived within `idle.gamepad_grace` | Playing with a controller is not idle (§14 question 3) |

**Frame-timing display.** The compositor sees every commit and presentation, so it draws an
overlay without injecting anything into the client: commit interval, frame-time graph,
presentation latency, the scanout path in use and why it is not direct, the variable refresh range
and current rate, and whether tearing is on. It answers "why is my game stuttering?" for every
game, native or Wine. Its capture default is `allow` (§9.3).

**Action wheel.** A radial marking menu on a held chord or button, configured per profile
(workspaces, OBS scenes, audio devices, microphone mute). It works in game because it is drawn on
the compositor's overlay layer and takes no keyboard focus; while open it reads relative pointer
motion to pick a sector, even under a pointer constraint.

```ucl
profiles {
    game { vrr = "on"; notifications = "hold"; effects = "suspend"; cpuset = "2-15"; }
}
rules = [
    { match { app_id = "steam_app_*"; } profile = "game"; },
    { match { app_id = "org.prismlauncher.*"; } profile = "game"; tearing = "never"; },
]
```

### 9.2 Ricer: every pixel addressable, every change safe

- **Design tokens** in three layers: the palette (raw colors); semantic roles (`surface`,
  `surface_variant`, `on_surface`, `accent`, `on_accent`, `urgent`, `warning`, `success`,
  `border_focused`, `border_unfocused`, `shadow`, `scrim`); and per-component overrides
  (`bar.surface`). Components read only roles, so changing one reskins everything together, and
  the contrast floor is checked on the resolved roles.
- **Palettes from the wallpaper** (`theme.palette.source = "wallpaper"`), by HCT tonal palettes or
  k-means. Image decoding runs in the worker, which gets the image as a rights-limited descriptor
  and returns only colors. Light and dark variants can follow a schedule.
- **Exporting to other programs.** Logic-less templates render the resolved tokens into allowlisted
  targets (foot, kitty, and alacritty configurations, GTK 4 CSS, Qt color schemes, editor themes) in
  the worker; `scoria-sup` only writes the results atomically. A target may have a reload hook (a
  named signal to a named process), declared only in the user's own configuration.
- **Live editing.** `scoriactl theme set accent=#f5a97f` applies in one frame without saving;
  `theme commit` saves it. Editing `config.ucl` reloads atomically (§5). Risky changes go through
  `try`.
- **Animations.** Named curves (`cubic-bezier(…)` or `spring { damping; stiffness; mass; }`) bound
  per event (`window_open`, `window_close`, `window_move`, `workspace_switch`, `overview`,
  `notification`), always under §8.2 rule 4.
- **User shaders.** GLSL compiled to SPIR-V by `glslang` in the worker, in three stages: per window
  (a fragment stage with signed-distance inputs), per output (post-processing), and wallpaper. Each
  shader has a GPU-time budget measured with timestamp queries; one that overruns for N frames in a
  row is disabled with a notification. A post-process shader forces composition, so it is
  suspended under Game Focus unless the profile keeps it.
- **Widgets.** Bars, desktop widgets, quick settings, and stream overlays are declarative UCL trees
  (`box`, `label`, `icon`, `graph`, `meter`, `image`, `slider`, `button`, `list`, `text_input`,
  `workspaces`, `mode_indicator`, and `tray`, which needs the bridge) drawn by `scoria-ui` on
  layer-shell surfaces. Data comes from the collectors (§4.2), `scoriactl subscribe` events, and
  **providers**: processes `scoria-sup` starts with limits, which publish CBOR, or JSON through
  `scoriactl publish`. There is no scripting language in the compositor.
- **Theme bundles.** A `.scoria-theme` archive holds tokens, templates, shader sources, fonts, and
  images, and is validated against the schema and the floors on install. Bundles **cannot contain
  executables, symbolic links, reload hooks, providers, bindings, or launch profiles**: themes are
  data.
- **Showcase mode.** `scoriactl showcase` hides personal data (host name, user name, notification
  content, clipboard), may apply a saved layout, lifts the showcase-eligible floors for its
  duration, captures through `ext-image-copy-capture`, and restores everything.

### 9.3 Streamer: what the audience sees is deliberate

**Native capture.** `ext-image-capture-source-v1` sources (outputs, toplevels, and Scoria's
workspace and stage sources) go to capture clients as dma-bufs through `ext-image-copy-capture-v1`.
PipeWire remains only in the bridge, for browser screen sharing.

**Two passes.** While any capture is active, `scoriad` renders a **display pass** (what the streamer
sees) and a **capture pass** (what the audience sees) from one scene graph, differing only in
per-surface visibility and redaction. The capture pass reuses the display pass's surface textures.
Night light and gamma apply to the display pass only (through the display controller's gamma
table where there is one). A directly scanned-out game stays on its plane for the display; the
capture pass composites its buffer separately at the capture rate, so streaming costs the capture
path, not the player's frames.

| `capture =` | Display | Capture | Default for |
|---|---|---|---|
| `allow` | shown | shown | ordinary windows, the frame-timing display |
| `redact` | shown | solid fill or blur with the app icon, same geometry | password managers (by `app_id`), clipboard history, the launcher's history |
| `exclude` | shown | absent | `scoria-lock` (always), notifications under `stream` |
| `display_only` | shown | absent | private chat, stream dashboard, alert previews |
| `capture_only` | absent | shown | webcam frame, input visualizer, "be right back" card |

**Stages.** A stage is a virtual output with a fixed mode (1920×1080@60) that is also a workspace:
windows are placed and laid out on it, and the streamer captures the stage rather than a 3440×1440
desktop. A stage can be watched on a physical monitor as a scaled `display_only` window, as a
broadcast switcher shows program output.

**The `stream` profile** starts with a capture or when `scoria-obs` reports streaming or
recording. Notifications are held or shown without content on the display only; clipboard history
pauses; the capture indicator (a red dot on the privacy layer) shows which sources are captured.
Streamer bindings in `reserved_binds` survive shortcut inhibition and reach `scoria-obs` over IPC.

**Audio.** The Open Sound System has no per-stream routing, so Scoria composes it from FreeBSD's pieces: a
`virtual_oss(8)` topology in the configuration (microphone, game, desktop, music, stream mix),
launch profiles pointing each program at a bus device (`/dev/dsp.game`), the bar's mixer widget
over `mixer(3)`, and a reserved microphone-mute binding with a persistent indicator. On AbyssBSD
this waits on `cuse`, or uses AUDIO.md's native taps instead (§14 question 4).

```ucl
stream {
    auto_profile = true;
    stages { main { mode = "1920x1080@60"; workspace = "stage"; monitor_on = "DP-2"; } }
}
rules = [
    { match { app_id = "org.keepassxc.KeePassXC"; } capture = "redact"; },
    { match { app_id = "chatterino"; } capture = "display_only"; },
]
```

## 10. Current conventions: adopt, adapt, reject

Each post-2000 convention is checked against §8: adopted if it already agrees with the research,
adapted if a constraint makes it agree, rejected if nothing can.

| Adopted | Prior art | Why |
|---|---|---|
| **Scrollable tiling** (`scroll`, the default): an endless strip of columns under a moving viewport | PaperWM, niri | Opening a window never resizes the others (perceived stability), which binary space partitioning breaks by design; each window keeps its place for spatial memory |
| **Overview**: a live, zoomed-out view with drag and drop, fully keyboard-driven | Exposé, GNOME, niri | Recognition over recall; direct manipulation |
| **Named, per-output, dynamic workspaces** and scratchpads, optionally held as a quasimode | Rooms, i3, Hyprland | Working sets; quasimodes avoid mode errors |
| **Command palette** with keybindings shown | NeXTSTEP Services, Raycast | Hick–Hyman; shown accelerators teach recall, as 1984 Mac menus did |
| **Tabs and groups** in one tile | Ion, i3 | Bly & Rosenberg's "content does not fit" case, without overlap |
| **Interruptible spring animation** | libadwaita, niri | Under rule 4 it serves feedback, not delay |
| **Notification center with focus profiles** | macOS Focus, Windows Focus Assist | Locus of control; no focus theft |
| **Privacy indicators** | iOS, Android, macOS | Visible system status |
| **Wallpaper palettes** | Material You, pywal, matugen | Expression, with floors applied at generation |
| **High dynamic range, color management, variable refresh, fractional scaling, tearing control** | KDE Plasma 6, gamescope | The gamer facet, each probed |
| **Clipboard history**, skipping password-manager hints and redacted from capture | Windows, Klipper | Forgiveness |
| **Session restore** by `app_id` and rules until `xdg-session-management` is standard | macOS Resume, GNOME | Stability across reboots |
| **Pinned floating windows** across workspaces | Browsers, Rooms | Fixed-size content is not tiled |
| **Night light**, display pass only | f.lux, GNOME | — |
| **Magnifier, sticky keys, reduced motion, high contrast** | Every major desktop | Legibility; screen readers through the bridge |

| Adapted | Problem | Scoria's form |
|---|---|---|
| Floating "island" bars | Breaks the infinite edge | Keep the look; the hit region reaches the edge |
| Rounded screen and window corners | Eats corner targets and resize handles | Hit zones extend under the rounding; resize margins sit outside the border |
| Hot corners | Accidental activation | Pressure or velocity barrier; off under Game Focus |
| Auto-hiding bars | Hidden status, hard to discover | Reveal on an edge push past resistance; privacy and mode indicators never hide |
| Icon-only bars | Unreadable icons | Tooltips within 500 ms; hold Mod for labels |
| Blur everywhere | Legibility depends on the backdrop | `contrast_guard`; suspended under Game Focus |
| Quick-settings panels | Grids of unlabeled tiles | A dense, labeled menu from the top-right corner |
| Trackpad gestures | Gesture-only actions | The common set, each with a binding and a verb |
| Window swallowing | Windows vanish without the user moving them | Opt-in per rule; the terminal returns in place |

| Rejected | Why |
|---|---|
| Tabletization: touch padding, hamburger menus, app grids, edge swipes, gesture-only actions, unlabeled tiles | Trades density, discoverability, and pointing efficiency for an input device the desktop lacks |
| Focus-stealing dialogs and popups | Locus of control; disastrous mid-game |
| Animations that delay input or cannot be turned off | Doherty threshold; user control |
| Latched modes without indicators | Mode-error research |
| Focus shown only by a tint or shadow | The focus floor |
| Real mode sets on request ("exclusive fullscreen") | The screen blanks on every switch; emulated instead |
| Confirmation dialogs as the safety net | Users click through them; undo, timed revert, and hold-to-confirm instead |
| Executable themes and dotfile bundles | Supply-chain risk; themes are data |

## 11. What it needs from the kernel

From Scoria's code, the inventory's libraries, and the commands it runs. Most of it is FreeBSD ABI
already in place; the rest is scheduled below or named as missing.

### 11.1 In place

| Need | Used by | Where here |
|---|---|---|
| rtld, ELF TLS, libthr and umtx | Rust's `std`, every process | P6.1–P6.3 |
| kqueue (`EVFILT_READ`, `TIMER`, `USER`, `VNODE` with `NOTE_RENAME`, `PROC`, `PROCDESC`), `eventfd` | Every event loop, configuration watching, libwayland's epoll-shim | P6.4 |
| UNIX sockets: `SOCK_SEQPACKET`, `SCM_RIGHTS`, `LOCAL_PEERCRED`, `getpeereid`, `MSG_NOSIGNAL` | Wayland, the bus, seatd | P6.5 |
| `memfd_create`, `shm_open2` with `SHM_ALLOW_SEALING`, `F_ADD_SEALS`, `shm_open(SHM_ANON)` | Configuration snapshots, `wl_shm`, keymaps | P6.3 |
| `pdfork`, `pdkill`, `setpgid` | `scoria-sup` | P4.5, P6.4 |
| Capsicum: `cap_enter`, `cap_rights_limit` | The worker, descriptor limits | P6.7 |
| `setrlimit`, `setusercontext`, `cpuset` | Launch profiles | P6 |
| `rtprio` | The render thread, as root | P6.2 |
| `kern.cp_time`, `vm.stats.vm.*`, `kinfo_proc` | The bar's collectors | P6.8 |
| devd and its sockets (`devd.pipe`, `devd.seqpacket.pipe`), devctl events | Hotplug | P6 (MULTIUSER.md) |
| PAM (`login`, `pam_exec`, base `pam_xdg`), `crypt` | Login, the locker | Base libraries over P6 |
| evdev (`/dev/input/event*`, `EVIOCGRAB`, `EVIOCREVOKE`), `hms`, `ukbd` | libinput through seatd | Track U |
| `vt` switching (`VT_SETMODE` with `VT_PROCESS`, `VT_RELDISP`, `KDSETMODE`, `KDSKBMODE`) | seatd | Track U (ADR-0015) |
| `getifaddrs` with `AF_LINK`, `PF_ROUTE` (`RTM_IFINFO`, `RTM_NEWADDR`, `RTM_DELADDR`) | The network collector | P7 |
| Direct Rendering Manager core, `gop-kms`, dumb buffers, atomic commits | Display, software path | M-G.1 (ADR-0025) |

Each row is in place as far as kyua and the phase tests show. Scoria's own tests (§12) are the check
that it is in place for this program.

### 11.2 To check

| Need | Why it might differ | How |
|---|---|---|
| `SOCK_SEQPACKET` record joining without `MSG_EOR`, and `net.local.seqpacket.*` sysctls | §7 relies on FreeBSD 15.1's exact behavior | A differential test in kyua's comparison set |
| devd notifications for Direct Rendering Manager hotplug and `/dev/input/event*` | `scoriad` filters on FreeBSD's strings | Record them from FreeBSD 15.1 (GRAPHICS.md §13 question 2) |
| Cursor and overlay planes, `IN_FORMATS` with modifiers | Direct scanout and the cursor plane | `scoriad --probe` per driver against FreeBSD's table |

### 11.3 Not yet scheduled

| Need | Used by | Proposed home |
|---|---|---|
| **Syncobj timelines and `DRM_IOCTL_SYNCOBJ_EVENTFD`** (today `DRM_CAP_SYNCOBJ_TIMELINE` answers 0) | Explicit synchronization | M-G.2 (syncobj; GRAPHICS.md §5 already plans the eventfd wait) |
| **Atomic asynchronous flips** (`DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`, answers 0) | Tearing control | With M-G.4–M-G.6, beside GRAPHICS.md §13 question 5 |
| **Variable refresh** (`VRR_ENABLED`, `vrr_capable`), **`FB_DAMAGE_CLIPS`**, high dynamic range metadata, `Colorspace`, `max bpc` | Game Focus, damage, high dynamic range | GRAPHICS.md §13 question 5, to schedule with M-G.4–M-G.6 |
| **MAC framework with `mac_priority`** | Real-time render thread for an unprivileged user | Before M-D.2 (MULTIUSER.md answers "no MAC" today) |
| **`sound(4)`**: the Open Sound System's `/dev/dsp`, `/dev/mixer`, `snd_dummy`, then HDA | The volume collector, programs' sound | [AUDIO.md](AUDIO.md) A1 before M-D.3; A2 and A4 before M-D.4 |
| **`cuse`** for `virtual_oss`, or AUDIO.md's taps | Stream audio buses | AUDIO.md question 3; before M-D.6 |
| **Game-controller drivers** (`hgame`, `xb360gp`, `ps4dshock`) | Games, controller navigation, idle | USB track; before M-D.4 |
| **ACPI sleep, buttons, lid** (`acpiconf -s 3`, devd notifies) | Power keys, lid, sleep | P11; the sysctls and notifies earlier |
| **Battery and AC adapter drivers** (`acpi_cmbat`, `acpi_acad`): `hw.acpi.battery.*` is a stub today (compat-deviations.md), and their devd notifies | The battery collector | Deferred to M-D (ROADMAP P9); before M-D.4 |

## 12. Testing and performance

### 12.1 Testing

QEMU first (ADR-0010), as everywhere else:

- **Host.** `lib/cbor`, `lib/tiling`, and `lib/contrast` are host-tested; the CBOR codec, IPC and
  snapshot decoders, and each FFI crate's request handlers are fuzzed.
- **Headless.** `scoriad`'s headless backend on lavapipe runs scripted sessions (test clients
  driving `xdg_shell`, `wl_shm`, `wp_presentation`, the layout engine, the floors' hit tests) and
  compares pixels with stored images. Lavapipe cannot exercise format modifiers, planes, or
  syncobjs; those need QEMU's `virtio-gpu` 3D and the reference machines.
- **Differential.** Every suite runs on AbyssBSD and on FreeBSD 15.1, on the same QEMU machine
  (bhyve on the build server for FreeBSD's side), from the same package. As with kyua, a test that
  passes on FreeBSD must pass here or be listed in `tests/desk/known-differences.txt` with its
  reason and the step that removes it.
- **Probes.** `scoriad --probe` on each machine against its table in `tests/desk/` (§6.1).
- **QEMU tier.** `cargo xtask test --tier desktop` boots an image with the package and test
  clients and drives a session from the serial console: `gop-kms` from M-D.2, Venus from M-D.3.
- **Physical.** The latency rig (§8.5) once the native drivers land.

### 12.2 Performance

Under ADR-0014 Scoria is a component, `desk`, measured against **the same Scoria package on FreeBSD
15.1 on the same machine**: in bhyve for the headless benchmarks from M-D.1, and on metal with
`drm-kmod` from M-D.4. The geometric mean is within 2%, as for every component.

| Benchmark | Measures |
|---|---|
| `desk.frame` | Composited frame time with 1, 8, and 32 windows, tiled and floating, with and without blur |
| `desk.latency` | evdev event to presentation for a pointer-locked client; to the cursor plane |
| `desk.flip` | Commit to flip, direct scanout and composited |
| `desk.ipc` | `scoriactl` round trip, configuration reload |
| `desk.startup` | Login to first frame, satellite start |
| `desk.capture` | GPU cost and latency of a 1080p60 capture (from M-D.4) |

Where FreeBSD has no driver (`panthor`), results are recorded and do not gate, until GRAPHICS.md
§13 question 7 is decided. §8.3's budget is Scoria's own contract, checked by the rig.

## 13. Hardware

- **amd64.** The X870E (RDNA2 iGPU) and the Z690-A (Navi 22, HARDWARE.md §7), both RDNA2, which
  M-G.6 drives. On the Z690-A, desktop runs boot from USB outside CI and benchmark windows and never
  write its internal drives. NVIDIA does not gate the desktop.
- **arm64.** The Orange Pi 6 Plus with the Sky1 display and `panthor` (M-G.4, M-G.5), once `desk/`
  builds for aarch64 (§14 question 2).
- **rv64.** Waits for P12, the BPI-SM10's graphics, and a `riscv64gc-unknown-freebsd` answer to the
  same question.

## 14. Open questions

1. **`xwayland-satellite` for games.** Does it pass pointer constraints, relative motion,
   fullscreen, and emulated modes well enough for Wine titles, and does it build on FreeBSD 15.1?
   The deciding spike in M-D.2. If it falls short, a Rust X11 window manager in `scoriad` becomes
   M-D.6 work.
2. **The aarch64 target.** `aarch64-unknown-freebsd` has no prebuilt `std`, and ADR-0003 rules out
   `-Zbuild-std`. Build natively with FreeBSD's `lang/rust` on arm64 (proposed), or let M-U settle
   an AbyssBSD userland target first (ROADMAP M-U).
3. **Controller idle.** Should `scoriad` open controller evdev nodes read-only, without grabbing,
   to see activity, or rely on idle inhibitors alone?
4. **Stream audio.** `virtual_oss` needs `cuse`, which the tree lacks (AUDIO.md question 3).
   Implement `cuse`, or route through AUDIO.md's native taps and keep `virtual_oss` for FreeBSD?
5. **Packaging.** A package under `/usr/local` (§2.4) installs the same on both systems. Should
   AbyssBSD's media also carry it as a distribution set, and does M-U change that?
6. **A ten-foot mode.** Out of scope, since it conflicts with §8.2 rule 12. Revisit as a separate
   session type?
7. ~~Login and the installer~~: decided 2026-11-23. Scoria has its own greeter (`scoria-greetd`,
   `scoria-greet`) and installer (`scoria-install`, `scoria-installd`), and the medium is built by
   `cargo xtask live-image` ([INSTALLER.md](INSTALLER.md) §4–§6, §9).
8. **Edge hits for other layer-shell clients.** Clamped events at a client's own edge go beyond the
   protocol's stated meaning. Keep it opt-in per rule, or propose a layer-shell extension upstream?
9. **OBS.** OBS's plugin interface is GPL-2.0, so no plugin is written here (§4.2 item 7). For
   capture: a third-party OBS source that speaks `ext-image-copy-capture-v1`, a request to OBS
   upstream, or the screencast portal through the bridge (which brings PipeWire back). For
   control: OBS's WebSocket protocol, if its specification can be used without consulting GPL
   sources, or no control beyond what OBS's own global hotkeys give. The `stream` profile still
   starts on any capture session either way.

   **Or replace OBS** (likely, if no route avoids GPL code). A Scoria-native `scoria-cast` in
   `desk/` takes the capture pass's dma-bufs and the audio buses and does what OBS does: scenes
   (stages already are), encoding, muxing, recording, a replay buffer, and streaming, with its
   controls as ordinary Scoria actions. The design to adapt is Sable's `sable-cast`
   (`git show 8ff58ee:docs/DESKTOP.md`, §5.6–§5.7), under the same license rules:
   - **Video:** Vulkan Video on the GPU (VCN on RDNA2, the Sky1's codec), which needs the encode
     engines from GRAPHICS.md §13 question 3. The software fallback is SVT-AV1 and OpenH264
     (both BSD). x264 and x265 are GPL and are never linked.
   - **Audio:** libopus (BSD), and ffmpeg's native AAC encoder.
   - **Containers and outputs:** libavformat from ffmpeg's LGPL configuration, linked
     dynamically: fragmented MP4 and Matroska files, RTMP and RTMPS (TLS from base's `libssl`).
     SRT (libsrt, MPL-2.0) and WHIP later.
   - **The ffmpeg catch:** FreeBSD's packaged ffmpeg may be built with GPL options, which makes
     the whole library GPL. Either the inventory carries an LGPL ffmpeg build of its own, or
     `scoria-cast` links SVT-AV1, OpenH264, and libopus directly and keeps only libavformat
     (and libavutil) from ffmpeg.
   - **A/V sync and latency:** capture timestamps from presentation feedback and the audio
     device clock, one clock for both, as Sable §5.7.3 laid out.
10. **Capture cost.** Measure the capture pass at 1080p60 and 1440p60 with a game on a plane, and
    decide whether capture may drop its rate under load.

## 15. Phasing within M-D

Latency and game-input plumbing come first because they cannot be added later. Interface floors ship
with the subsystem they govern. Expression comes last because it depends on everything below it.
The headless foundation needs only P6 and starts before M-G finishes.

| Step | Work | Needs | Exit |
|---|---|---|---|
| **M-D.0** | This document finalized (ADR-0026 accepted 2026-11-23); the `desk/` dependency rule in `cargo xtask check`; `desk/ffi/inventory.txt`; `tools/waylandgen` and `tools/vkgen`; §14 question 2 answered | — | The generators' `--check` passes; layout tests pass for every bound structure |
| **M-D.1** | Headless: the kqueue loop, devd listener, capability probe, the Vulkan renderer on lavapipe, `wp_presentation`, core protocols over `wl_shm`, the layout engine (`scroll`, BSP, master-stack, monocle, floating, tabs), the undo ring | P6 | The headless suite passes on AbyssBSD in QEMU as on FreeBSD 15.1, or is listed; `desk` (headless) within 2% in bhyve |
| **M-D.2** | First light: the mode-setting backend on `gop-kms` (software path), libseat, libinput, `vt` switching, the cursor plane, the input-routing floors (§8.2 rules 1–5), the game-input and frame-timing protocols, layer shell, the `xwayland-satellite` spike | M-G.1, P8, `mac_priority` | A tiled session used with a real keyboard and mouse in QEMU and on the X870E and Orange Pi 6 Plus framebuffers |
| **M-D.3** | Accelerated in QEMU, and the session: dma-buf import, explicit synchronization, direct scanout; `scoria-sup`, the bus, `scoriactl`, the worker and configuration reload, rules and profiles, `try`; `xwayland-satellite`; `scoria-ui`, the bar, launcher and palette, notifier, locker; the overview; capture (`ext-image-copy-capture`, the two passes, the privacy layer, the `stream` profile) | M-G.2, M-G.3, P7, AUDIO A1 | A complete session on Venus with explicit sync: `foot`, Firefox, and an X11 program through `xwayland-satellite`, configured live, with the floors' tests passing; a capture client records a desktop with one window redacted and one display-only |
| **M-D.4** | Metal: direct scanout and planes on real hardware, variable refresh, tearing, Game Focus, the frame-timing display, the action wheel, controllers, the latency rig | M-G.4–M-G.6, AUDIO A2/A4, controller drivers, asynchronous flips and variable refresh (§11.3), P11 for sleep | `desk` (metal) within 2% of FreeBSD 15.1 with `drm-kmod` on the X870E and the Z690-A; §8.3's budget holds on the rig; on the Orange Pi 6 Plus with `panthor`, the session runs accelerated and its `desk` results are recorded without gating (§12.2) |
| **M-D.5** | Installed: the live medium, the greeter, and the installer ([INSTALLER.md](INSTALLER.md) I.3, I.4): installs to UFS, then ZFS, then encrypted ZFS | INSTALLER.md I.1, I.2; M-Z for ZFS; `geom_eli` | An installed disk boots to a Scoria session in QEMU and from the X870E's second NVMe |
| **M-D.6** | Streaming and expression: OBS integration or `scoria-cast` (§14 question 9), stages, stream audio; wallpaper palettes, templates, user shaders, bundles, showcase; input methods, color management, clipboard history, tablets, accessibility, session restore, `scoria-dbus-bridge` | `cuse` or taps (§14 question 4) | A 1080p60 stream of a stage with capture-only overlays and per-program audio |

## References

- Accot, J., & Zhai, S. (1997). Beyond Fitts' law: models for trajectory-based HCI tasks. *CHI '97*.
- Apple Computer (1987). *Human Interface Guidelines: The Apple Desktop Interface*.
- Bly, S. A., & Rosenberg, J. K. (1986). A comparison of tiled and overlapping windows. *CHI '86*.
- Callahan, J., Hopkins, D., Weiser, M., & Shneiderman, B. (1988). An empirical comparison of pie
  vs. linear menus. *CHI '88*.
- Card, S. K., English, W. K., & Burr, B. J. (1978). Evaluation of mouse, rate-controlled isometric
  joystick, step keys, and text keys for text selection on a CRT. *Ergonomics* 21(8).
- Card, S. K., Moran, T. P., & Newell, A. (1980). The keystroke-level model for user performance
  time with interactive systems. *CACM* 23(7); (1983) *The Psychology of Human-Computer
  Interaction*.
- Doherty, W. J., & Thadani, A. J. (1982). *The Economic Value of Rapid Response Time*. IBM
  GE20-0752.
- Fitts, P. M. (1954). The information capacity of the human motor system in controlling the
  amplitude of movement. *J. Experimental Psychology* 47(6).
- Henderson, D. A., & Card, S. K. (1986). Rooms. *ACM TOG* 5(3).
- Hick, W. E. (1952). On the rate of gain of information. *Q. J. Experimental Psychology* 4.
- Hutchins, E., Hollan, J., & Norman, D. (1985). Direct manipulation interfaces. *Human-Computer
  Interaction* 1(4).
- Hyman, R. (1953). Stimulus information as a determinant of reaction time. *J. Experimental
  Psychology* 45.
- IBM (1987, 1991). *Systems Application Architecture: Common User Access*.
- Kurtenbach, G., & Buxton, W. (1993). The limits of expert performance using hierarchic marking
  menus. *INTERCHI '93*.
- MacKenzie, I. S. (1992). Fitts' law as a research and design tool in human-computer interaction.
  *Human-Computer Interaction* 7(1).
- Miller, R. B. (1968). Response time in man-computer conversational transactions. *AFIPS FJCC*.
- Nielsen, J., & Molich, R. (1990). Heuristic evaluation of user interfaces. *CHI '90*; Nielsen, J.
  (1994). *Usability Engineering*.
- Norman, D. A. (1988). *The Psychology of Everyday Things*.
- Raskin, J. (2000). *The Humane Interface*.
- Sellen, A., Kurtenbach, G., & Buxton, W. (1992). The prevention of mode errors through sensory
  feedback. *Human-Computer Interaction* 7(2).
- Shneiderman, B. (1983). Direct manipulation. *IEEE Computer* 16(8); (1986) *Designing the User
  Interface*.
- Smith, D. C., Irby, C., Kimball, R., Verplank, B., & Harslem, E. (1982). Designing the Star user
  interface. *Byte* 7(4).
- Sullivan, K. (1996). The Windows 95 user interface: a case study in usability engineering.
  *CHI '96*.
- Tognazzini, B. (1992). *Tog on Interface*.
