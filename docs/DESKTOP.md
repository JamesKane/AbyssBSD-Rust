# Desktop

- Status: **Draft**. A forward design for M-D. Decisions here become an ADR at the start of M-D
- Date: 2026-10-27
- Scope: the desktop shell AbyssBSD ships: the Wayland compositor and its Vulkan renderer, input
  and controller routing, audio routing, screen capture and streaming, the IPC that replaces D-Bus,
  configuration, and the shell's components (panels, docks, launcher, tray, notifications, lock
  screen, greeter, settings). Applications and a general-purpose toolkit are out of scope beyond §8.9
- Inputs: [VISION.md](VISION.md) (goals 7 and 11), [ROADMAP.md](ROADMAP.md) (M-U, M-G, M-D),
  [GRAPHICS.md](GRAPHICS.md), [USB.md](USB.md) (evdev, hidraw), [MULTIUSER.md](MULTIUSER.md)
  (kqueue, sockets, shared memory, Capsicum, jails), [ENGINEERING.md](ENGINEERING.md),
  [PERFORMANCE.md](PERFORMANCE.md)

> "Sable" is a working name for the desktop. Crate and binary names use it as a prefix and can be
> renamed mechanically (§12 question 1).

The short version: **a Wayland desktop built for AbyssBSD, not ported to it, for gamers,
streamers, and developers.** The compositor talks to the kernel only through the DRM/KMS and Vulkan
interfaces GRAPHICS.md defines. All rendering is Vulkan, including the software path, which is Mesa's
lavapipe. Components talk over **Conduit**: capability channels on `SOCK_SEQPACKET` socket pairs
that a session broker hands out, with no bus, no names, and no listening sockets. Every component
except the session root runs in Capsicum capability mode. All configuration is one typed KDL tree
with a strict cascade, and every setting has exactly one path. Shell UI is a retained scene graph
that sandboxed component processes submit and the compositor renders, so themes, effects, and
animations apply everywhere at once. Everything the desktop designs is Rust in this workspace. The
foreign code is Mesa's Vulkan drivers, FreeBSD's base libraries, and a short inventory of
shallow-dependency libraries for standards there is no reason to reimplement: media codecs and
containers, font shaping, image formats, and keymaps (§2.1).

## 1. Executive overview

### 1.1 Purpose

ROADMAP M-D asks for "an original Wayland compositor with dynamic tiling, workspaces, animations,
IPC, and declarative configuration … built for BSD, not ported to it." This document is that
desktop's design. It has four aims:

1. **The lowest input-to-photon latency** we can reach, with first-class high-polling-rate mice,
   game controllers, and specialized peripherals.
2. **Zero-copy capture and encoding** of outputs, windows, and per-application audio for streaming
   and recording, without a separate media server in the path.
3. **Deep visual customization ("ricing")**: composable, GPU-rendered components, themes, and user
   shaders, all driven from one configuration tree.
4. **A capability security model** built on AbyssBSD's FreeBSD primitives (Capsicum, process
   descriptors, jails, kqueue) instead of a global message bus.

### 1.2 Principles

| # | Principle | Consequence |
|---|---|---|
| P1 | **GUI-first** | Every user task has a graphical path: settings, permissions, profiles, rices, recovery, login. The settings UI is generated from the schemas that validate config files, so the two cannot drift. `sablectl` exists for scripting, but nothing *requires* a terminal. |
| P2 | **Vulkan everywhere** | One renderer, one shader language, one memory model. Composition, shell UI, effects, color management, capture conversion, and video encode all run on Vulkan. Machines with no GPU driver yet use lavapipe on `gop-kms` (§4.8). |
| P3 | **One source of truth** | Configuration is one typed tree. Each setting has one path, layers override by path, and runtime state is a view of that tree, never a parallel store (§7). |
| P4 | **One canonical way** | One IPC (Conduit), one component model (Facet), one binding mechanism (actions bound in config), one capture path, one shader language. Paths for running foreign software are labelled *compatibility*, and nothing new is built on them (§7.6). |
| P5 | **User sovereignty** | No hard-coded deny lists, no telemetry, no confirmation walls. Defaults are least-privilege, but the user can grant anything to anything, and the system says plainly what a grant means. The user is the policy authority. Vendors and rice authors are not. |
| P6 | **AbyssBSD-native** | VISION goal 11 applies here most of all. kqueue is the event core, Capsicum the sandbox, `pdfork(2)` the supervisor, devd the hotplug source, jails the application sandbox, and sysctl and rc the system configuration. No epoll, udev, logind, D-Bus, cgroups, or inotify. Portability is a non-goal. |
| P7 | **Measured** | The desktop is a component under ADR-0014 like any other. Its FreeBSD baseline is sway on FreeBSD 15.1 with `drm-kmod` on the same machine (§10). |

### 1.3 Non-goals

- An X11 session or X11 window manager. Xwayland (from packages) is supported as a client (§4.10).
- Portability to Linux or other BSDs.
- OpenGL rendering, and any software renderer other than Vulkan on lavapipe.
- D-Bus anywhere in the canonical architecture. §8.8 has the compatibility bridge.
- A Linuxulator. VISION lists it as a non-goal. Linux-only games (Steam's runtime) are therefore
  out of reach until a Linuxulator exists as its own ABI personality. Gaming targets native
  FreeBSD games and FreeBSD's Wine (§5.3.6).
- Web-app wrappers or embedded webviews. A browser from packages is an ordinary client for reading
  web pages, not an application runtime ([STYLE.md](STYLE.md) S2).

## 2. Constraints from AbyssBSD

### 2.1 Dependencies

ADR-0002's **no external crates** rule exists for the kernel, where every line runs privileged and
has to be fully understood. The desktop is userland, and much of what it needs implements standards
that mature libraries already implement well: media codecs, container formats, font shaping, image
formats. Rewriting those adds no design value, costs years, and yields code that is less tested and
less fuzzed than what it replaces. The desktop therefore takes its own rule, which the M-D ADR
(M-D.0) records as an amendment to ADR-0002 that applies to `desk/` only.

**The line is the dependency tree, not where the code comes from.** A foreign library may be used
when all of these hold:

1. **Shallow closure.** The library and everything it pulls in can be listed by name in the
   inventory: generally a handful of libraries, each of which meets these rules itself. A library
   that brings a framework with it (GLib, Boost, an async runtime, a proc-macro stack) does not
   qualify, however good it is.
2. **It implements a standard, not this design.** Codecs, containers, font tables, image formats,
   and protocols defined elsewhere qualify. What this document exists to design stays in-house: the
   compositor and its Wayland protocol layer, frame scheduling and the input path (aim 1), the
   scene graph, rendering, and configuration (aim 3), and Conduit and the security model (aim 4).
3. **License.** Permissive (BSD, MIT, ISC, zlib, Apache-2.0), file-level copyleft (MPL-2.0), or
   LGPL when linked dynamically from a package. GPL code is never linked, and, as everywhere in the
   project, never consulted.
4. **Maintained and fuzzed.** An active upstream and continuous fuzzing (OSS-Fuzz or equivalent),
   since every library here parses untrusted input.
5. **Contained.** Foreign code that parses untrusted data runs only in capability-mode components
   (§6), never in `sabled` or `sable-sysd`.

How each kind arrives:

- **C and C++ libraries** come from packages (`pkg`) or `base.txz` and are linked dynamically. Each
  is reached through one FFI module with `// SAFETY:` comments. Bindings are written or generated
  in-house and checked in: no `bindgen`, no `-sys` crate trees.
- **Rust crates** are vendored and pinned under `desk/third_party/<crate>/` with their licenses and
  used by `path =`, so the build stays offline and `Cargo.lock` never names a registry. The rule
  exists to keep out crates.io's deep trees: `tokio`, `wgpu`, `smithay`, `image`, `resvg`, `naga`,
  and anything built on `syn`.
- **The inventory**, `desk/third_party/inventory.kdl`, lists every foreign library with its version,
  license, complete closure, the components that load it, and why it qualifies. `cargo xtask check`
  fails on any link or `path =` outside it, and adding an entry is a reviewed change.

The kernel, the loader, and every `lib/` crate stay under ADR-0002 unchanged. `lib/` holds pure
`no_std` code and does not take foreign dependencies. A crate that wraps a foreign library lives in
`desk/`.

Applied to the questions an ordinary Rust desktop answers with crates.io:

| Need | Ordinary choice | Here |
|---|---|---|
| Wayland wire protocol | libwayland, `wayland-rs` | **Built**: `lib/wayland-wire` (pure `no_std` message codec and fd accounting) + code generated from the protocol XML by `tools/waylandgen` and checked in, as `syscallgen` does (`cargo xtask waylandgen --check`). The XML is MIT. The protocol layer is the compositor's core and its fault boundary. |
| Vulkan bindings | `ash` | **Built**: `lib/vk`, generated from `vk.xml` (Apache-2.0 OR MIT) by `tools/vkgen` and checked in. The generator is small. The loader (`libvulkan.so.1`) is opened with `dlopen` at run time. |
| DRM/KMS | `drm-rs`, libdrm | **Built**: `lib/drm-uapi`: DRM ioctl structures and FreeBSD request numbers (GRAPHICS.md §2), shared with `sys/drm`'s tests. No libdrm, no GBM: buffers are allocated with Vulkan and exported as dma-bufs. |
| Seats | seatd/libseat, logind | **Built**: the `seat` provider in `sable-sysd` (§6.8) |
| Input devices | libinput | **Built**: `sable-input` reads evdev directly. Touchpad gestures, palm rejection, and tablet tools are implemented in `lib/gesture` (§5.2). The input path is aim 1, and libinput's closure (libevdev, mtdev, libudev-devd, libwacom with GLib) is not shallow. |
| Keymaps | xkbcommon | **xkbcommon** (MIT, built without its optional libxml2 registry), the library every Wayland client already links, with xkeyboard-config data |
| Text shaping | HarfBuzz | **HarfBuzz** (MIT, no required dependencies): shaping for every script, font tables, and glyph outlines and color glyphs through `hb-draw` and `hb-paint` |
| Text rendering, layout | FreeType, swash, cosmic-text | **Built**: `lib/raster` (area-coverage rasterizer for HarfBuzz's outlines, into the compositor's atlas, §4.7.3), `lib/textlayout` (line breaking and bidi, UAX #14 and #9; ICU is the only complete alternative, and it is not shallow) |
| Images and icons | image, resvg | **libpng** (with zlib from base) and **libjpeg-turbo**. **Built**: `lib/svg` (the icon subset: paths, gradients, transforms, no text or filters), since resvg's tree is deep |
| Layout | taffy | **Built**: `lib/flexgrid` (flexbox and a grid subset), part of the scene graph |
| Shaders | naga, glslang | **Built**: `lib/shade`: a compiler for **Sable Shading Language**, a WGSL subset, to SPIR-V (§4.7.4). It compiles the compositor's own shaders at build time and user shaders at run time. |
| Config syntax | kdl, serde | **Built**: `lib/kdl` (format-preserving KDL v2) and the `Schema` derive in `lib/schema-derive` |
| Async runtime | tokio, mio | **Built**: `sable-rt`, directly on kqueue (§6.3) |
| Audio resampling, mixing | rubato, cpal | **Built**: `lib/dsp`, inside the real-time graph (§5.5) |
| Video encode | x264, SVT-AV1 | Vulkan Video in the canonical path. The software fallback is **libavcodec** with **SVT-AV1** and **OpenH264** (both BSD), §5.7.1. x264 and x265 are GPL and excluded. |
| Audio encode | libopus, fdk-aac | **libopus** (BSD) and libavcodec's native AAC encoder |
| Containers, streaming | ffmpeg | **libavformat** from ffmpeg's LGPL configuration: fragmented MP4, Matroska, RTMP and RTMPS (TLS from base's `libssl`). SRT (**libsrt**, MPL-2.0) and WHIP come later (§5.7.2). |

**Foreign code at run time** is therefore of three kinds:

1. **The Vulkan loader and Mesa's drivers from packages** (`vulkan-loader`, `mesa-libs`,
   `mesa-dri`). GRAPHICS.md G1 already settles that Vulkan drivers are Mesa's: panvk, RADV, Venus,
   and lavapipe.
2. **FreeBSD base libraries** from `base.txz`: `libc` (through `std`), `libpam` for authentication,
   `libssl`/`libcrypto` for TLS (RTMPS, the remote-control Facet), and `libz`. M-U may later replace
   them. The desktop uses them the way every base program does.
3. **The inventory**: xkbcommon, HarfBuzz, libpng, libjpeg-turbo, ffmpeg's libavcodec,
   libavformat, and libavutil, SVT-AV1, OpenH264, libopus, and later libsrt.

Programs from packages that the desktop *runs* as ordinary clients (Xwayland, Wine, a private
`dbus-daemon` for §8.8) are applications, not dependencies.

What remains in-house is still real work. `lib/raster`, `lib/textlayout`, `lib/svg`, and
`lib/shade` are kept to what the desktop needs (ROADMAP risk "no external crates"), and the
inventory is kept short: a new entry has to show why the in-house alternative adds nothing.

### 2.2 Toolchain and layout

- Desktop crates use `std` and build for `x86_64-unknown-freebsd` and `aarch64-unknown-freebsd`,
  since AbyssBSD runs FreeBSD binaries (M-U's open target question). `x86_64-unknown-freebsd` ships
  a prebuilt `std` through rustup. **`aarch64-unknown-freebsd` does not** (it is Tier 3), and
  ADR-0003 rules out `-Zbuild-std`. The aarch64 desktop therefore builds natively, with FreeBSD's
  `rust` package on FreeBSD or AbyssBSD arm64, which is the self-hosting path VISION already names.
  §12 question 2.
- Pure libraries (`lib/wayland-wire`, `lib/kdl`, `lib/raster`, `lib/shade`, `lib/svg`, …)
  are `no_std` + `alloc`, host-tested, and live in `lib/` with the kernel's pure libraries.
  Process crates live under a new top-level `desk/`:

```
desk/
  comp/         sable-comp: compositor (protocol, scene, render, kms, schedule, policy modules)
  sabled/       session root: supervisor, broker, config authority
  sysd/         sable-sysd: root supervisor and system providers (seat, power, net, auth, …)
  input/        sable-input: device classes, raw pointer path, controller router
  audiod/       sable-audiod: real-time audio graph
  cast/         sable-cast: capture consumer, encode, mux, stream
  xwm/          sable-xwm: Xwayland window management
  rt/           sable-rt: kqueue executor
  conduit/      Conduit runtime and the #[interface] macro (macro crate: desk/conduit-derive)
  facet/        Facet SDK (and desk/facet-derive)
  facets/*      built-in Facets (panel, dock, launcher, tray, notify, osd, lock, greet, settings, …)
  ctl/          sablectl
```

- ENGINEERING.md applies unchanged: algebraic types, newtypes for IDs and units, typestate for
  lifecycles, `Result` on every path that touches client or device input, no panics on untrusted
  input (every Wayland client, Conduit peer, config file, font, image, EDID, and HID report is
  untrusted), and `unsafe` only in the FFI, ioctl, and shared-memory modules, each with
  `// SAFETY:`. Facet crates are `#![forbid(unsafe_code)]`.
- Every change builds for both architectures. `cargo xtask check` covers `desk/`.

### 2.3 Distribution

The desktop is part of AbyssBSD's distribution, not a port:

- `cargo xtask dist --set desktop` produces **`desktop.txz`**, a distribution set beside FreeBSD's
  `base.txz`, installed with the same `mtree`-checked tooling. Binaries go in `/usr/bin` and
  `/usr/libexec/sable/`, data in `/usr/share/sable/`, the system config layer in `/etc/sable/`, and
  `sable_sysd` in `/etc/rc.d/` (enabled with `sable_sysd_enable="YES"` in `rc.conf`).
- Its package dependencies (`mesa-libs`, `mesa-dri`, `vulkan-loader`, `xkeyboard-config`, and
  optionally `xwayland` and `wine`) are installed with `pkg`. The installer lists them, and the
  graphical first-boot flow (§8.6, `sable-greet`) checks them.

## 3. Architecture

### 3.1 Overview

```
                               ┌──────────────────────────────────────────┐
                               │                sabled                    │
         config (KDL)  ───────▶│  Config authority · Broker · Supervisor  │◀──── sable-sysd (root, rc.d)
         /etc/sable, ~/.config │                                          │      seat · power · net ·
                               └──┬─────────┬──────────┬──────────┬───────┘      auth · storage · backlight
                    typed diffs   │ grants  │ pdfork   │          │
         ┌───────────────┬────────┴──────┬──┴──────────┴───┬──────┴────────┐
         ▼               ▼               ▼                 ▼               ▼
  ┌─────────────┐ ┌─────────────┐ ┌──────────────┐ ┌─────────────┐ ┌──────────────┐
  │ sable-comp  │ │ sable-input │ │ sable-audiod │ │ sable-cast  │ │ Facets       │
  │ Wayland ·   │◀│ evdev ·     │ │ RT graph ·   │ │ capture ·   │ │ panel, dock, │
  │ Vulkan · KMS│ │ uinput pads │ │ OSS · cuse   │ │ encode ·mux │ │ tray, …      │
  └──────┬──────┘ └──────┬──────┘ └──────┬───────┘ └──────▲──────┘ └──────┬───────┘
         │ /dev/dri/*    │ /dev/input/*  │ /dev/dsp*      │ dma-bufs       │ scenes
         ▼               ▼ /dev/hidraw*  ▼                └── sable-comp ◀─┘ (Conduit)
     sys/drm         sys/evdev, sys/hid  sound(4)
         ▲
         │ Wayland (xdg-shell, dmabuf, syncobj, …)
   applications and games (native Wayland, Xwayland via sable-xwm, FreeBSD Wine)
```

### 3.2 Processes

| Process | Privilege | Sandbox | If it dies |
|---|---|---|---|
| `sable-sysd` supervisor | root, from rc(8) | None (small, supervises providers) | rc restarts it. Sessions keep their fds |
| `sable-sysd` providers | root, one per domain | Capsicum, Casper services limited per provider | Restarted by the supervisor |
| `sabled` | User, started by `sable-greet` through the `auth` provider | None (session root of trust) | The session ends |
| `sable-comp` | User + DRM master fd and input fds from the `seat` provider | Capsicum after init | Clients lose the display. Facets reconnect when it restarts |
| `sable-input` | User + evdev, hidraw, and uinput fds from `seat` | Capsicum | Transparent restart, virtual pads recreated. The compositor falls back to reading keyboards and pointers itself |
| `sable-audiod` | User + `rtprio` | Capsicum | Restart. OSS clients see one xrun |
| `sable-cast` | User | Capsicum | Restart. Streams reconnect |
| Facets | User | Capsicum, always | Restart with backoff. The compositor shows the last scene meanwhile |
| Applications | User, or jailed | Optional jail profile (§6.7) | Not supervised unless asked |

## 4. Compositor & graphics pipeline

### 4.1 Structure and threads

`sable-comp` is one crate with internal modules in strict dependency order: `wayland` (protocol state
machine), `scene` (immutable snapshots), `render` (Vulkan render graph), `kms`, `schedule` (frame
timing), and `policy` (window management, §4.9).

| Thread | Work | Priority |
|---|---|---|
| Protocol | Wayland client I/O and dispatch, surface commits, Conduit to Facets and `sabled` | Normal |
| Render | Builds and submits command buffers from snapshots | `rtprio` realtime (configurable) |
| KMS | Atomic commits, flip events, vblank timestamps | `rtprio` realtime |
| Input | Batches from `sable-input` (or evdev directly in fallback mode) | `rtprio` realtime |

Each thread has its own kqueue. They communicate through SPSC rings and `EVFILT_USER` triggers. The
render thread never locks protocol state. It reads immutable snapshots (§4.3).

### 4.2 The Wayland state machine

Each `wl_surface` follows Wayland's double-buffered model: pending state → (cached, for synchronized
subsurfaces) → current. Sable gates the step to *current* on **readiness**:

1. **Buffer readiness.** With `wp_linux_drm_syncobj_v1`, the acquire point is waited on through
   `DRM_IOCTL_SYNCOBJ_EVENTFD`. The eventfd is registered with kqueue, so nothing blocks
   (GRAPHICS.md §5.1). For implicit-sync clients, the dma-buf's exported sync file
   (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`) is polled with `EVFILT_READ`.
2. **Timing.** A `wp_commit_timing_v1` target becomes an `EVFILT_TIMER` with `NOTE_ABSTIME`.
3. **Subsurface order.** Synchronized children apply with their parent.

A commit that isn't ready by the frame deadline simply misses that frame. The previous buffer stays
on screen, and the compositor never samples a buffer still being written or stalls a frame for one.

**Transactions.** Multi-surface changes (tiling relayouts, interactive resize) are grouped. Every
participant must be ready or time out (default one frame) before any becomes current, so tiled
layouts never tear apart during a resize.

**Untrusted input.** Every request is decoded by generated code into typed values. A malformed
message is a protocol error for that client, never a panic. Per-client resource ledgers (objects,
buffers, pending commits, shm bytes) bound what one client can make the compositor hold.

#### 4.2.1 Protocols (first release)

| Category | Protocols |
|---|---|
| Core and shell | `wl_compositor`, `wl_subcompositor`, `xdg_wm_base`, `xdg-decoration` (server-side by default, drawn from theme styles), `xdg-activation-v1`, `xdg-output` |
| Buffers and sync | `wl_shm`, `zwp_linux_dmabuf_v1` v5 with feedback, `wp_linux_drm_syncobj_v1`, `wp_single_pixel_buffer_v1` |
| Timing | `wp_presentation`, `wp_fifo_v1`, `wp_commit_timing_v1`, `wp_tearing_control_v1` |
| Scale and color | `wp_viewporter`, `wp_fractional_scale_v1`, `wp_color_management_v1`, `wp_color_representation_v1` |
| Input | `wl_seat`, `zwp_relative_pointer_v1`, `zwp_pointer_constraints_v1`, `wp_pointer_gestures`, `zwp_text_input_v3`, `zwp_input_method_v2`, `wp_cursor_shape_v1`, `zwp_tablet_v2`, `zwp_keyboard_shortcuts_inhibit_v1` |
| Clipboard and drag and drop | `wl_data_device_manager` v3, `zwp_primary_selection_device_manager_v1` ([CLIPBOARD.md](CLIPBOARD.md)) |
| Capture | `ext_image_capture_source_v1`, `ext_image_copy_capture_v1` (compatibility front door, §5.6) |
| Grant-gated (§6.6) | `ext_session_lock_v1`, `zwlr_layer_shell_v1` (compatibility), `ext_foreign_toplevel_list_v1`, `ext_data_control_v1`, `wp_security_context_v1` |
| Idle | `ext_idle_notify_v1`, `zwp_idle_inhibit_v1` |

The `linux-` in some protocol names is naming only. They are DRM protocols, and AbyssBSD's DRM
interface is the one these protocols describe (GRAPHICS.md §2).

### 4.3 Scene snapshots

After each dispatch cycle with changes, the protocol thread publishes a **`SceneSnapshot`**: a
structurally shared tree (`Arc` nodes, with unchanged subtrees shared) of outputs → workspaces →
windows → surfaces, plus Facet scenes and overlays (cursor, OSD, lock). Each surface node holds its
imported buffer, readiness marker, damage, transform, viewport, color description, and effect
bindings. It also holds a **sink visibility mask**: one bit per render sink (each output and each
capture stream), which is how stream-only and screen-only content works (§5.6.4).

Snapshots are published through an atomic pointer swap and never mutated. The render thread takes
the newest one at its deadline.

### 4.4 Buffer import

| Buffer | Path |
|---|---|
| dma-buf | `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier`, imported once per `wl_buffer`. dmabuf feedback offers scanout-capable tranches first for fullscreen surfaces |
| shm | Mapped read-only. Damage is uploaded through a staging ring into a per-surface image on the transfer queue where there is one |
| single-pixel | A push-constant fill |

Acquire and release points are timeline `VkSemaphore`s imported and exported as DRM syncobj fds
(`VK_KHR_external_semaphore_fd`). This needs GRAPHICS.md M-G.2 (syncobj, sync files, dma-buf).

### 4.5 Frame scheduling

The scheduler starts each frame **as late as it can** and still make the vblank:

- **Render estimate R**: the p95 of the last 120 frames' GPU time from timestamp queries, per
  output and per composition mode.
- **Margin m**: 0.5 ms by default. It grows after a miss and decays after 300 good frames.
- **Start** = predicted vblank − R − m − measured commit latency.
- **Cursor late-latch.** The cursor plane moves in its own atomic commit just before the deadline,
  from the newest input batch.
- **VRR.** For a fullscreen client on a VRR output, presentation follows the client: a frame is
  committed as soon as its buffer is ready, within the panel's range.
- **Tearing.** A `wp_tearing_control_v1` `async` request on a directly scanned-out surface uses
  `DRM_MODE_PAGE_FLIP_ASYNC`. The user can force tearing on or off per application. The user
  decides, not the protocol.
- **Feedback.** `wp_presentation` reports the flip event's `CLOCK_MONOTONIC` timestamp, refresh,
  and `VSYNC | HW_CLOCK | HW_COMPLETION | ZERO_COPY` as they apply.

VRR, async flips, HDR metadata, and `content type` are KMS properties GRAPHICS.md §13 question 5
leaves for after the first drivers. The desktop needs them on the reference machines' native
drivers (M-G.4–M-G.6) (§9).

### 4.6 Composition modes

| Mode | When | Cost |
|---|---|---|
| Direct scanout | One opaque fullscreen surface with a scanout-capable format and modifier, no effects over it on the output sink, explicit sync | No composition. The client buffer is the primary plane |
| Plane offload | Surfaces that fit overlay planes | Partial |
| Full composition | Everything else | The render graph (§4.7) |

Game Mode (§5.2.5) steers toward direct scanout.

### 4.7 The Vulkan render graph

#### 4.7.1 Requirements

- **Vulkan 1.3**: dynamic rendering, synchronization2, timeline semaphores, maintenance4.
- **Required**: `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`,
  `VK_KHR_external_semaphore_fd`, `VK_KHR_external_memory_fd`, `VK_EXT_queue_family_foreign`,
  descriptor indexing, `VK_KHR_push_descriptor`.
- **Optional**: `VK_KHR_video_encode_queue` and the H.264/H.265/AV1 encode extensions (§5.7),
  `VK_EXT_physical_device_drm` (matching Vulkan devices to DRM nodes).
- **Drivers**, in GRAPHICS.md's order: lavapipe on `gop-kms` (any UEFI machine and QEMU), Venus on
  `virtio-gpu` (QEMU, M-G.3), panvk on the Immortalis-G720 (Orange Pi 6 Plus, M-G.5), and RADV on
  the RDNA2 iGPU (X870E, M-G.6). The RTX 4070 follows GRAPHICS.md §13 question 1.

The compositor renders into its own **pool of images allocated with DRM format modifiers, exported
as dma-bufs, and attached as KMS framebuffers** (`DRM_IOCTL_MODE_ADDFB2` with modifiers). There is
no `VkSwapchainKHR` on KMS. Each output has 3 images, or 4 while it is captured.

#### 4.7.2 The graph

Each frame, `render` builds a graph of passes and resources from the snapshot, and derives barriers
from it:

```
 import/upload ──▶ background ─▶ windows (SDF shadows, rounded clips, backdrop blur) ─▶ Facet scenes
                   (text, shapes, effects) ─▶ overlays ─▶ post shaders ─▶ color transform ─┬▶ output framebuffers
                                                                                           └▶ capture sinks (§5.6)
   async compute queue: blur pyramids (dual Kawase), upscaling and sharpening
```

- **Blending space.** Linear `R16G16B16A16_SFLOAT` whenever an output is HDR, a color-managed
  surface is visible, or blur or post effects are active. Otherwise a fast path composes straight
  into the output format. The last pass applies the output's transfer function, gamut mapping from
  client color descriptions, and the calibration LUT if there is one.
- **Damage.** Per sink, with that sink's buffer age. Passes scissor to damage. Blur widens it by
  its radius.
- **Bindless.** Sampled images sit in one descriptor array. Draws are instanced quads with
  per-instance data in a storage buffer, so N windows plus a shell are a handful of draws.
- **Effects are node properties** (blur, shadow, corner radius, dim, color filter), not separate
  code paths. One blur serves windows and panels.
- **Pipelines** are created at startup or shader load, never on the frame path. The pipeline cache
  is persisted per device UUID and driver version.

#### 4.7.3 Text and vectors

Facet UI is rendered by the compositor (§8.2), so text is too. HarfBuzz shapes runs and supplies
glyph outlines (`hb-draw`) and color glyph paint lists (`hb-paint`). `lib/textlayout` breaks lines
and resolves bidi. `lib/raster` rasterizes glyphs into a GPU atlas per size and subpixel offset,
and coverage is blended in linear space. Rectangles,
rounded rectangles, circles, and lines are analytic SDFs in the fragment shader. Other paths are
tessellated by `lib/raster` and cached by hash. Images are decoded off-thread (libpng,
libjpeg-turbo, `lib/svg`), and SVG icons are re-rasterized when the scale changes.

#### 4.7.4 Shaders

There is one shader language: **Sable Shading Language (SSL)**, a subset of WGSL (scalars, vectors,
matrices, structs, fixed-size arrays, functions, control flow, texture sampling, no pointers to
storage), compiled to SPIR-V by `lib/shade`.

- The compositor's own shaders are SSL, compiled at build time by `tools/shadec` and checked in as
  SPIR-V with `cargo xtask shadec --check`, so the build needs no external shader compiler.
- User shaders compile in-process at run time. A kind fixes each one's interface:

| Kind | Inputs | Typical use |
|---|---|---|
| `post` | Output color (linear), size, time, frame | Color grading, CRT, grain |
| `backdrop` | Content behind the node, node SDF, time | Custom glass and distortion |
| `fill` | Node UV and size, theme tokens as uniforms, time | Animated gradients and backgrounds |
| `transition` | From-image, to-image, progress | Workspace and window transitions |

- `lib/shade` emits only constructs whose SPIR-V it validates itself, because invalid SPIR-V can
  crash a driver. Its output is checked against Khronos `spirv-val` in tests (test-time only, from
  packages). The compiler and validator are fuzzed.
- A shader that exceeds its GPU-time budget for N frames is turned off. The settings GUI shows why
  and offers a one-click re-enable. The budget is informational and the user may raise it (P5).

### 4.8 KMS

- **Devices** are `/dev/dri/card*` (modesetting) and `renderD*` (rendering), opened by the `seat`
  provider and handed to the compositor (§6.8). Hotplug arrives as devd notifications on the
  `devd.seqpacket.pipe` socket. GRAPHICS.md §7.4 records their strings.
- **Atomic only.** Planes are assigned per frame by a bounded search with
  `DRM_MODE_ATOMIC_TEST_ONLY` (default three tests per frame).
- **Properties**: `VRR_ENABLED`, `HDR_OUTPUT_METADATA`, `Colorspace`, `max bpc`, `content type`
  (*Game* in Game Mode), `IN_FENCE_FD`, `OUT_FENCE_PTR`.
- **gop-kms.** On GRAPHICS.md's `gop-kms` (one CRTC, one plane, the firmware mode), the compositor
  runs with lavapipe and copies its final image into the dumb framebuffer. This is the "software"
  step ROADMAP M-D names, and it keeps one renderer (P2).
- **Multi-GPU.** Each output is rendered on the GPU that drives it. A foreign buffer is imported
  directly if its modifier allows, otherwise copied once on the source GPU.
- **VT switching.** The compositor holds the display in `KD_GRAPHICS` with `VT_SETMODE`
  `VT_PROCESS` (GRAPHICS.md §7.6). On release, the `seat` provider drops DRM master and revokes
  the input fds (`EVIOCREVOKE`, USB.md). On acquire, it does a full modeset and repaints every sink.
- **Device loss** (`VK_ERROR_DEVICE_LOST`, or the GPU driver's hang recovery, GRAPHICS.md §6)
  rebuilds the renderer. Client buffers are re-imported from their retained dma-buf fds, and
  clients miss frames but stay connected.

### 4.9 Window management

The `policy` module provides the dynamic tiling ROADMAP M-D asks for:

- **Workspaces** belong to outputs, and they are created on demand and named or numbered in config.
- **Layouts** are per workspace and selected from config: `tile` (binary-split dynamic tiling with
  configurable split rules), `stack` (master and stack), `columns` (a scrolling row of columns),
  `float`, and `monocle`. Every layout is an implementation of one `Layout` trait inside the
  compositor. A layout is not a plugin: new layouts are new code in the tree (P4).
- **Rules** (`app "<id>" { … }`, §7.2) set the workspace, layout role, floating, size, opacity,
  tearing, and Game Mode per application.
- **Animations** for open, close, move, resize, and workspace switches are declared in the theme
  (curves, durations, or `transition` shaders) and run in the compositor at the output's refresh
  rate.
- **Everything the window manager does is an action** (§7.6): focus, move, swap, resize, layout
  change, workspace switch. Actions are what keys bind to and what `sablectl` and Facets invoke.

### 4.10 Nested and headless backends, Xwayland

- **Nested**: a Wayland client of another compositor, presenting through `VkSwapchainKHR`, for
  developing the desktop under the build server's or workstation's compositor.
- **Headless**: offscreen sinks only, for tests (§10) and streaming-only sessions. Every backend
  shares the whole render graph.
- **Xwayland** (from packages) starts on the first X11 connection. Its window management is
  **`sable-xwm`**, a separate, supervised client granted the Xwayland association protocol, so X11
  never enters the compositor. Wine's Wayland driver is preferred wherever it works.

## 5. Input & multimedia subsystem

### 5.1 Overview

`sable-input` owns input devices so that report parsing, a large attack surface, stays out of the
compositor, and so that controllers can be routed:

```
 /dev/input/event*, /dev/hidraw*  (fds from the seat provider; hotplug from devd)
            │
            ▼
 sable-input ── classify ─┬─ keyboards ─ xkbcommon state, shortcuts ───┐
                          ├─ pointers ── raw path (gaming) / lib/gesture ┼─▶ shm ring ─▶ sable-comp
                          ├─ tablets, touch ── lib/gesture ─────────────┘
                          ├─ controllers ─ router ─▶ /dev/uinput virtual pads ─▶ games
                          └─ peripherals ─ hidraw grants ─▶ Facets, apps (RGB, haptics, decks)
```

If `sable-input` is down, the compositor reads keyboards and pointers itself, so the user is never
locked out.

### 5.2 Keyboards and pointers

#### 5.2.1 Devices

Devices are AbyssBSD's `sys/evdev` nodes (USB.md): `/dev/input/event*` from `hkbd`, `hms`,
virtio-input, and the HID class drivers, plus `hidraw` from `hidbus`. Each fd is narrowed with
`cap_rights_limit(CAP_READ | CAP_IOCTL | CAP_EVENT)` and `cap_ioctls_limit` to the evdev requests
used. Each device's clock is set to `CLOCK_MONOTONIC` with `EVIOCSCLOCKID`.

#### 5.2.2 Keyboards, touchpads, tablets

Keymaps are compiled by xkbcommon from xkeyboard-config's data according to the `input.keyboard`
config, and they are sent to clients as sealed memfds (`memfd_create`, `F_ADD_SEALS`). Repeat comes
from config per keyboard and is announced with `wl_keyboard.repeat_info`. `lib/gesture` handles
touchpad motion, tap, scroll, palm rejection, and swipe and pinch gestures, with tablet tools
following `zwp_tablet_v2`. The algorithms are designed from the device behavior we observe. No GPL
input-stack code is read (ADR-0007).

#### 5.2.3 Gaming mice: the raw path

Any pointer tagged `class "gaming"` in config, or matched by a user-editable table (over 1000 Hz, or
by vendor and product), takes the **raw path**:

1. A realtime thread waits with `EVFILT_READ` and reads every queued event in one `read(2)`.
2. Reports are split at `SYN_REPORT`, and each keeps its kernel timestamp.
3. Acceleration is **flat by default**. Optional curves are piecewise-linear and evaluated in fixed
   point, so they are deterministic.
4. Batches go to the compositor through an SPSC shared-memory ring and an eventfd.

| Situation | Delivery |
|---|---|
| Focused client holds a locked pointer and relative pointer | **Every report** as `relative_motion` with its microsecond timestamp. Events are flushed once per batch, not once per event |
| Ordinary client | Coalesced per batch into one `motion` + `frame`. Per-app config can request full rate |
| Cursor plane | The newest position at late-latch time, independent of delivery |

#### 5.2.4 Shortcuts

Bindings (§7.6) are matched in `sable-input`. A matched binding is consumed, unless the focused
client holds a shortcut inhibit (fullscreen games do by default). One user-configured escape chord
always works.

#### 5.2.5 Game Mode

Game Mode is a **config profile** (§7.3), not special code. It activates when a window matching a
`game` rule is fullscreen and focused, or through an action. The default profile:

- prefers direct scanout, turns on VRR, and sets `content type` to *Game*;
- forwards pointer reports at full rate and grants the shortcut inhibit;
- raises the game's main thread with `rtprio_thread` (when `sabled` launched it, through its
  process descriptor);
- pauses background Facet animations and holds notifications, or shows them only on the
  capture-excluded overlay;
- asks the `power` provider for the performance CPU and GPU policy, if granted.

Each of these is a key the user can override per game.

### 5.3 Controller routing

#### 5.3.1 Ownership

`sable-input` grabs each physical controller (`EVIOCGRAB`). AbyssBSD's devfs ruleset for the
desktop gives physical controller nodes to the `seat` provider only, so applications cannot open
them.

#### 5.3.2 Virtual pads

For each slot (P1–P8), the router creates a virtual device through **uinput**. By default it copies
the physical device's identity (vendor, product, name), so SDL's controller database still
recognizes it. **uinput is not yet in AbyssBSD** (§9). FreeBSD has it as part of evdev, so it is
part of the ABI to implement.

#### 5.3.3 The routing table

```kdl
input {
    controllers {
        slot 1 { device "serial:3a:f2:…" ; follow "focus" }                  // the default
        slot 2 { device "usb:054c:0ce6#2" ; target app="org.example.Couchgame" }
        remap "swap-confirm" { swap "BTN_SOUTH" "BTN_EAST" }
    }
}
```

`follow "focus"` feeds a slot only while a window of its owning process tree has focus. Otherwise
the pad holds neutral state and stays silent. `follow "always"` turns this off.

#### 5.3.4 Remapping and layers

The router's realtime thread applies remaps, chords, radial menus (drawn by an OSD Facet as a
compositor overlay), gyro-to-pointer, and deadzone and response curves, with a fixed time budget
per report.

#### 5.3.5 Haptics and hidraw passthrough

`EV_FF` effects uploaded to a virtual pad are replayed on the physical pad. Features evdev does not
carry (adaptive triggers, lightbars, touchpads on some pads) need **hidraw passthrough**: a
rights-limited fd to the physical `/dev/hidraw*`, granted per application (§6.4) and revoked when
the game exits or loses the slot.

#### 5.3.6 Games on AbyssBSD

Without a Linuxulator, the targets are native FreeBSD games from packages and Windows games through
FreeBSD's Wine (`emulators/wine`, through Xwayland or Wine's Wayland driver) on Mesa's Vulkan
drivers. The router, Game Mode, and capture work the same for all of them. Controller support also
needs HID game-controller class drivers in `sys/hid` (FreeBSD's `hgame`, `xb360gp`, `ps4dshock`
roles), which do not exist yet (§9).

### 5.4 Peripherals

- Wheels, pedals, and flight sticks are controllers to the router, with axis shaping and
  force-feedback passthrough.
- Configuration-only devices (RGB, DPI, stream decks) are reached through **hidraw grants** to the
  Facet or provider that drives them. Device matches are declared in the component's manifest
  (§8.4) and confirmed once in the GUI.
- MIDI devices are granted the same way, for example as a hardware mixer for `sable-audiod`.

### 5.5 Audio (`sable-audiod`)

#### 5.5.1 Rationale

`sound(4)`'s OSS interface is FreeBSD's audio ABI, so AbyssBSD will implement it, and every audio
application from packages speaks it. It lacks **per-application routing and capture**, which
streamers need ("stream the game and microphone, not the voice chat"). `sable-audiod` adds that as a
realtime graph, without a desktop bus or session manager.

#### 5.5.2 Design

- **Hardware endpoints.** `/dev/dspX.Y` is opened exclusively at the graph's rate and format, with
  small fragments (`SNDCTL_DSP_SETFRAGMENT`, for example 2 × 128 frames at 48 kHz). Bit-perfect
  mode is used when the graph runs at the device's native rate.
- **OSS clients, through CUSE.** `audiod` publishes a virtual DSP node per application with
  FreeBSD's userspace character devices (`cuse(3)`, as `virtual_oss` does). Applications launched by
  Sable get their own node, mapped to `/dev/dsp` in their devfs view (jails) or named through
  `OSS_AUDIODEV`. Each application is therefore its own source in the graph. Opens of the shared
  default node are attributed through the CUSE peer's process and its ancestry.
- **Native clients** (Facets, `sable-cast`) use Conduit's `audio.Stream`: a shared-memory ring
  (`CAP_MMAP_RW`), an eventfd, and a negotiated period, with no copy into the graph.
- **The graph** runs on one realtime thread with locked memory, and with no allocation or locks in
  the callback. Edits arrive as commands applied at period boundaries. The nodes are sources,
  sinks, buses, gain, the `lib/dsp` resampler, and a CLAP/LV2 effect host (for example noise
  suppression on the microphone). The effect host is a separate sandboxed process with
  shared-memory I/O, because plugins are foreign code.
- **Routing** lives in config and is edited by the patchbay Facet (both write `audio.routing`).
- **Clock.** The primary device's DMA position (`SNDCTL_DSP_GETOPTR`) drives the graph, and every
  buffer is stamped against `CLOCK_MONOTONIC` for A/V sync (§5.7.3).

#### 5.5.3 Prerequisites

AbyssBSD has no audio yet. This needs `sound(4)` with the OSS ioctls, HDA (both reference
machines have HDA codecs, HARDWARE.md), USB audio (which needs isochronous transfers, excluded
from Track U), and CUSE (§9).

### 5.6 Screen capture

#### 5.6.1 Principles

Capture is **zero-copy** (dma-bufs from compositor to encoder), **a render sink** (one more
target of the frame's graph, not a screenshot API), and **granular**: an output, one window
(rendered even when occluded or on another workspace), a region, or a **virtual output** (a
headless output with its own size and rate, used as a stream canvas).

#### 5.6.2 Two front doors, one mechanism

| Interface | Consumer |
|---|---|
| Conduit `capture.Session` (canonical) | `sable-cast` and Sable-aware tools: sink masks, several streams, cursor as metadata, damage, audio timestamps |
| `ext_image_copy_capture_v1` (compatibility) | Foreign tools such as OBS and screenshot utilities. Built on the same sinks, and requires the `capture` grant on the client's connection (§6.6) |

There is no xdg-desktop-portal and no PipeWire. Portals require D-Bus. Their job, letting the user
decide what may be captured, is done by grants and the compositor's own source picker.

#### 5.6.3 Frame flow

1. The consumer lists the formats and modifiers its encoder can import (from the Vulkan video
   format properties). The compositor allocates the sink's image pool in one of them, usually NV12
   or P010 in an encoder-friendly tiling, and sends the pool's dma-buf fds **once**.
2. Each frame, the compositor renders or copies into slot *k*, converts RGB to YUV in a compute
   pass, and signals point *T* on a shared syncobj timeline. It sends `{slot, damage, pts, T}`.
3. The consumer waits on *T* on its own queue, encodes, and signals the slot's release point.
   Neither side waits on the CPU.

When the game is directly scanned out, the sink samples the client's buffer in one pass, so the
screen stays composition-free while the stream still gets frames. An occluded captured window
still receives frame callbacks at the capture rate. The cursor is either drawn into the sink or
sent as metadata.

#### 5.6.4 Stream-safe rendering

The per-node sink mask (§4.3) gives:

- **Privacy**: notifications, password managers, and a configurable list of applications are kept
  out of every capture sink by default but remain on screen.
- **Overlays**: a Facet can draw alerts, chat, or a webcam frame that only the stream shows, or
  only the screen.
- **Several compositions**: a 1080p stream with overlays and a clean 4K recording are two sinks
  with different masks, rendered from the same snapshot in one graph.

### 5.7 Encoding and streaming (`sable-cast`)

#### 5.7.1 Encoder

The canonical encoder is **Vulkan Video** (`VK_KHR_video_encode_queue` with H.264, H.265, or AV1),
using the sink's dma-bufs directly, with rate control in config. On the reference machines this
needs the GPU's video engine in the kernel driver: VCN under the RDNA2 driver (M-G.6), and the
Sky1's Mali-V codec, which GRAPHICS.md §13 question 3 assigns to a separate media design. Machines
without hardware encode fall back to software encoders through libavcodec: SVT-AV1 for AV1 and
OpenH264 for H.264 (§2.1), fed from the sink's buffers by a GPU-to-host copy. There is one
`Encoder` trait and one config key, `cast.encoder`.

#### 5.7.2 Outputs

- **Recording**: fragmented MP4 (so a crash loses no more than the last fragment) or Matroska,
  through libavformat.
- **Streaming**: RTMP and RTMPS (libavformat, with TLS from base's `libssl`) first. SRT (libsrt)
  and WHIP (WebRTC ingest: ICE, DTLS-SRTP) are later steps, each through libavformat where its
  support suffices.
- **Replay buffer**: a ring of encoded packets in memory, saved by an action ("save the last
  60 s").

#### 5.7.3 A/V sync

Video frames carry the presentation timestamp of the flip they show (or render completion for
off-screen sinks). Audio buffers carry graph time mapped to `CLOCK_MONOTONIC`. The muxer aligns
both and corrects drift by resampling audio (±0.1%), never by dropping video frames.

#### 5.7.4 Scenes and webcams

Streaming scenes (arrangements of sources, overlays, and cameras) are a config subtree, and
switching scenes is an action, so a stream deck key, a pad chord, or a keyboard binding can do it.
Webcams need a capture interface AbyssBSD lacks. FreeBSD's is `webcamd`'s V4L2 compatibility in
userland, which needs USB isochronous transfers and CUSE. This is a later step.

### 5.8 Latency targets

| Path | Added by the desktop (p99) |
|---|---|
| Mouse report → locked-pointer client | ≤ 150 µs |
| Mouse report → hardware cursor on screen | ≤ one vblank + 0.5 ms |
| Game buffer ready → scanout (direct scanout, tearing allowed) | ≤ 0.5 ms |
| Game buffer ready → scanout (composited, VRR) | ≤ R + m |
| Application write → DAC (low-latency profile, 48 kHz) | ≤ 8 ms total |
| Frame presented → encoder input ready | ≤ 1 ms of GPU time |

These are design targets. The gate is §10.

## 6. IPC & security model

### 6.1 Why not D-Bus

| D-Bus property | Problem |
|---|---|
| A global namespace of names | Any process on the bus can find and call any service. Authority comes from who you are, not what you were given. |
| Policy in separate XML, enforced by a central daemon | Opaque to users, easy to get wrong, and a choke point that sees all traffic |
| Monitoring and eavesdropping | Historically allowed. The broker is a high-value target |
| Broadcast signals | Attack surface, and wakeups across the whole session |
| systemd activation, polkit | Linux-specific, and not AbyssBSD's model (P6) |

Sable uses an **object-capability** model instead: **holding a reference is the authority**. There
are no names to resolve and no bus to sniff.

### 6.2 Conduit transport

| Aspect | Choice | AbyssBSD primitive (MULTIUSER.md) |
|---|---|---|
| Socket | `AF_UNIX` `SOCK_SEQPACKET` | Message boundaries are kept, so there is no framing to get wrong |
| Topology | Point-to-point channels made with `socketpair(2)` by the broker | No listening sockets and no paths for components |
| Descriptors | `SCM_RIGHTS` | Every passed fd is narrowed first with `cap_rights_limit`, `cap_ioctls_limit`, and `cap_fcntls_limit` |
| Peer identity | `LOCAL_PEERCRED` (`xucred`), `getpeereid(2)` | Only on the one external socket (§6.5). On internal channels, the channel is the authority |
| Bulk data | `memfd_create` / `shm_open(SHM_ANON)` with seals, mapped read-only by readers | Scenes, audio and input rings, thumbnails |
| Wakeups | `EVFILT_READ` on channels, eventfd for rings, `EVFILT_USER` in-process | — |
| Supervision | `pdfork(2)` and `EVFILT_PROCDESC` `NOTE_EXIT` | No PID races and no SIGCHLD handling |

### 6.3 Conduit protocol

**Objects.** As in Wayland, each channel carries objects with connection-local 32-bit IDs. Object
0 is the channel's **root interface**, fixed when it is granted. Methods may return sub-objects on
the same channel, or **new channels** (delegated capabilities) as fds.

**Wire format.**

```
 0               4               8              12              16
 ├───────────────┼───────────────┼───────────────┼───────────────┤
 │ object_id u32 │ opcode u16 │fl│ fd_count u8   │ payload_len   │
 ├───────────────┴───────────────┴───────────────┴───────────────┤
 │ payload: little-endian, fixed-width scalars, LEB128 lengths,  │
 │ no self-description                                           │
 └───────────────────────────────────────────────────────────────┘
 flags: REQUEST | REPLY | EVENT | ERROR | MORE.  At most 64 KiB; larger payloads go in a sealed memfd.
```

**Interfaces are Rust** in their definition crate. That crate is the single source of truth, and
there is no separate IDL:

```rust
#[conduit::interface(name = "sable.power.Battery", version = 2)]
pub trait Battery {
    async fn state(&self) -> BatteryState;
    #[event]
    fn changed(&self) -> BatteryState;                 // coalesced to ≤ 1 Hz unless critical
    #[since(2)]
    async fn set_charge_limit(&self, percent: Percent) -> Result<(), PowerError>;
}
```

The macro (`desk/conduit-derive`, a proc-macro written in-house with no `syn` or `quote`)
generates the client proxy, server dispatch, version negotiation, a schema descriptor embedded in
the binary (read by `sablectl` and the capability inspector), and a fuzz target for every decoder.

**Semantics.** Calls are asynchronous and pipelined, correlated per object. Events go only to
subscribers, with no broadcast. Each channel has a bounded send window. A peer that stops reading
is disconnected after a stall (2 s for UI channels; never for audio and input rings, which are lossy
by design), so one stuck component can't grow memory in its peers. Errors are typed per method.

**Runtime.** `sable-rt` is a small single-threaded executor directly on kqueue. It exposes
`EVFILT_PROCDESC`, `EVFILT_VNODE`, `EVFILT_USER`, `EVFILT_TIMER` with `NOTE_ABSTIME`, and channels.
Components that need parallelism spawn threads that talk to the executor over channels. It is the
one runtime for components.

### 6.4 The broker

`sabled`'s broker is the only party that can create a channel between processes that don't already
share one.

1. **Declaration.** A component's manifest (§8.4) lists the interfaces it wants, with reasons.
2. **Authorization.** Policy is the user's config (§7.7). When a component is first installed, the
   settings GUI shows its requests, and accepting writes them under `grants.<component>`. There is
   no hidden policy database.
3. **Spawn.** `sabled` `pdfork`s, places the granted channels at fds 3..N, `closefrom`s the rest,
   applies resource limits (`setrlimit`, and `rctl` once AbyssBSD has it), and executes. The SDK
   calls `cap_enter(2)` **before any component code runs**. From then on, the process cannot open
   paths, make sockets by address, or read sysctls except through the Casper services it was given.
4. **Delegation.** A holder may **attenuate** a capability (read-only, a subtree, a rate limit) and
   pass it on. It can never amplify one, because providers enforce each channel's rights.
5. **Revocation.** Removing a grant makes the broker send `revoke(grant_id)` to the provider, which
   closes that channel. The holder sees `EV_EOF`. Revocation does not depend on the holder
   cooperating.

**Requests at run time.** A component asks `broker.Request(interface)`. If config has no grant,
the broker asks the user in the GUI (Allow once / Always, which writes config / Deny).
`security.prompts` can be set to `"deny"` or `"allow"` to answer for every request. The user picks
the posture.

### 6.5 The one external socket

Programs `sabled` did not start (scripts, `sablectl`, a developer's tools in a terminal) get in
through exactly one socket, `$XDG_RUNTIME_DIR/sable/control` (directory mode 0700):

- The broker accepts only peers whose `getpeereid(2)` uid is the session's.
- By default, such peers get full authority (`security.control`). Any unsandboxed process running
  as the user can already edit the user's config, so pretending otherwise would be theater. The
  design says so plainly.
- `security.control.mode = "token"` requires a token from a user-controlled file, or a `control`
  capability delegated by a Sable launcher. `"off"` removes the socket.
- Capsicum-sandboxed components and jailed applications **cannot reach it**: capability mode forbids
  path lookups, and jails don't see the path.

| Actor | Can do |
|---|---|
| A Facet, even fully compromised | What its grants allow, nothing more |
| A jailed application | Its Wayland connection (baseline protocols), its audio node, its controller slots, its nullfs view |
| Unsandboxed code running as the user | What the user can do, as on any Unix. Sable's job is to make sandboxing the easy default, not to pretend this boundary exists |
| The network | Nothing listens, except an opt-in remote-control Facet over TLS with client certificates |

### 6.6 Wayland protocol gating

| Connection | How the compositor knows | Protocols |
|---|---|---|
| Public socket `$XDG_RUNTIME_DIR/wayland-1` | Default | **Baseline**: core, xdg-shell, buffers and sync, timing, input, text input, scale, color, idle inhibit. Nothing that can observe or control other clients |
| Socket pair from `sabled` (`WAYLAND_SOCKET`) | `sabled` registers the compositor end with a grant set over Conduit before passing the other end to the child | Baseline plus exactly the protocols granted in config (`capture`, `layer-shell`, `foreign-toplevel`, `data-control`, `input-method`, `session-lock`) |
| `wp_security_context_v1` | `sabled`'s jail launcher attaches app and instance IDs | Baseline, possibly reduced by the jail profile |

A baseline client reads the clipboard **only while it has keyboard focus**. Background access needs
`data-control`, which clipboard-manager Facets are granted. [CLIPBOARD.md](CLIPBOARD.md) has the
full rules, including the ones for writing.

### 6.7 Application jails

Jailing is **per application and opt-in** (P5), with launcher presets that make it one click. It is
meant for untrusted games, Wine prefixes, and binary-only software. A jail profile (config, §7.2)
produces:

- a non-persistent jail (`jail_set(2)`), sharing the host's network by default;
- a **nullfs** view: the base system read-only, the application's directory and its own home
  subtree read-write, nothing else of the user's home;
- a **devfs ruleset**: the null, zero, and random devices, its GPU **render node only** (no KMS),
  its virtual controller nodes, its CUSE audio node as `/dev/dsp`, and any hidraw grants;
- a **pre-connected Wayland socket** with security-context metadata. The jail has no path to the
  public socket;
- supervision through a process descriptor held by `sabled`, so force quit is `pdkill(2)`, which
  cannot hit the wrong process.

"No network" and "no LAN" profiles need VNET jails, which MULTIUSER.md leaves out of P6 (§9).

### 6.8 System providers (`sable-sysd`)

Seats, power and suspend, networking, storage, backlight, and login authentication need root.
They come from **`sable-sysd`**, started by rc(8). It is the desktop's only root component, and it
is split into one Capsicum-sandboxed provider per domain:

| Provider | Holds | Gives sessions |
|---|---|---|
| `seat` | DRM primary nodes, evdev, hidraw, and uinput nodes, the VT | Fds for the active session's compositor and input service. It drops master and revokes input on VT switch (§4.8), as seatd does on FreeBSD |
| `auth` | `libpam` in a helper process | Password checks for `sable-greet` and `sable-lock`. Neither Facet sees the PAM stack |
| `power` | `/dev/acpi`, `cap_sysctl` limited to `hw.acpi.*`, `dev.hwpstate*`, and `powerd` settings | Battery state, suspend, CPU policy |
| `net` | A route socket, `wpa_supplicant` control sockets | Interface state, Wi-Fi selection (P8 onward) |
| `storage` | devd, `/dev/devctl2`, mount | Removable media |
| `backlight` | `/dev/backlight/*` (GRAPHICS.md §7.7) | Brightness |

`sabled` connects to `/var/run/sable/sysd` (root, group `sable`, 0660). `sable-sysd` checks
`LOCAL_PEERCRED` and grants **per-session capabilities from the system config layer**
(`/etc/sable/system.kdl`). For example, members of `operator` may suspend, and members of `network`
may change Wi-Fi. This is polkit's role, in the same config language as everything else. `sabled`
then gives Facets attenuated sub-capabilities: a battery widget gets `power.Battery` read-only, not
`power.Suspend`.

### 6.9 Hardening

- Every process except `sabled` and `sable-sysd`'s supervisor runs in capability mode after init.
- Every decoder (Wayland, Conduit, KDL, fonts, images, SPIR-V, EDID passed through from KMS, HID
  reports) is generated or written as bounds-checked safe Rust, and fuzzed.
- Binaries are PIE with RELRO and stack protection, under AbyssBSD's ASLR, with overflow checks
  kept in release builds for parsers.
- No component accepts code from the network. Rices (§8.7) are data: config, SSL shaders, and
  assets. SSL compiles to validated SPIR-V that runs in the GPU's memory model and can reach no file
  or channel.

## 7. Configuration & state management

### 7.1 Goals

1. **One tree.** Every setting in the desktop, from compositor to input, audio, Facets, and grants,
   has exactly one path.
2. **Cascade, never duplicate.** Layers override by path. Setting a path twice *within* a layer is
   an error, not "last one wins".
3. **Typed and self-describing.** Schemas come from Rust types in the component that owns the keys.
   One schema drives validation, the settings GUI, the generated manual pages and reference
   (MANUALS.md), the editor language server, and migrations.
4. **Live.** Changes apply within a frame with no restarts, except keys the schema marks `restart`.
5. **Made for people.** Files can be hand-edited, versioned, and shared. The GUI edits the same
   files and keeps their comments and formatting.

### 7.2 Syntax: KDL v2

KDL is node-based, which fits a tree of named things (outputs → workspaces, panels → slots →
Facets) better than TOML's tables, and it has none of YAML's ambiguity. `lib/kdl` parses it
preserving format, which the GUI's write-back needs. Type annotations carry units and kinds:
`(px)36`, `(color)"#1e1e2e"`.

```kdl
// ~/.config/sable/sable.kdl — the user layer
include "rices/abyssal"                  // a rice layer (§7.3); data only

theme {
    token "accent" (color)"oklch(0.72 0.14 290)"
    token "radius" (px)10
}

output "DP-1" {
    mode 2560 1440 refresh=(hz)240
    vrr "auto"
    hdr #true
    position 0 0
}

input {
    pointer "Viper V3 Pro" { class "gaming" ; accel "flat" ; sensitivity 1.0 }
    keyboard "*" { layout "us" ; options "caps:escape" ; repeat delay=(ms)220 rate=40 }
}

workspace "1" { output "DP-1" ; layout "tile" }
workspace "2" { output "DP-1" ; layout "columns" }

bind {
    key "Super+Return"   action="apps.launch" app="sable-term"
    key "Super+Space"    action="launcher.toggle"
    key "Super+H"        action="wm.focus" dir="left"
    key "Super+Shift+R"  action="cast.replay.save" seconds=60
    pad "Guide+Start"    action="shell.overview"
}

panel "top" {
    output "DP-1" ; anchor "top" ; height (px)36 ; style "glass"
    slot "start"  { facet "sable-workspaces" ; facet "sable-window-title" }
    slot "center" { facet "sable-clock" format="%H:%M" }
    slot "end"    { facet "sable-tray" ; facet "sable-audio" ; facet "sable-battery" }
}

app "org.winehq.Wine" { jail "games" ; profile "game" }

grants {
    facet "sable-battery" { allow "sable.power.Battery" mode="read" }
    app "com.obsproject.Studio" { wayland "capture" }
}
```

### 7.3 The cascade

```
 Defaults ─▶ System ─▶ Rice(s) ─▶ User ─▶ Profiles ─▶ Runtime
 (schemas)  /etc/sable  (data)   ~/.config  overlays   in memory ──"persist"──▶ User file
```

| Layer | Source | Written by | Limits |
|---|---|---|---|
| Defaults | `#[derive(Schema)]` defaults | Component authors | Read-only |
| System | `/etc/sable/*.kdl` (from `desktop.txz`, then the administrator) | Administrator | May **lock** keys only for `sable-sysd` policy (§6.8). Session keys can't be locked: the administrator controls the machine, the user their session |
| Rice | `~/.local/share/sable/rices/<name>/rice.kdl` | Rice authors | **Only `theme`, `panel`, `dock`, `layout`, `animation`, `shader`.** A rice that touches `grants`, `bind`, `app`, `security`, `input`, or `output` fails validation, so installing one can't grant capabilities or rebind keys |
| User | `~/.config/sable/sable.kdl` and its includes | The user, or the GUI | None |
| Profiles | `profile "<name>" { … }` blocks | The user | Overlays over the same paths, activated by rules (`when focused-fullscreen "game"`, `when power "battery"`) or actions, applied in declared order |
| Runtime | In `sabled` | GUI sliders, `sablectl set`, Facets granted `config.write` on a subtree | Not saved unless the `config.persist` action runs. The GUI marks overridden keys with "persist" and "revert" |

**Resolution.** The value at path *p* comes from the highest layer that sets *p*. **A list is one
value**: a higher layer that sets `slot "end"` replaces the whole list. There are no merge
operators, because they would be a second, less predictable way to say the same thing. The only
structure is **named nodes** (`panel "top"`, `output "DP-1"`, `app "<id>"`), whose children are
separate paths, such as `panel.top.height`.

**Uniqueness in a layer.** After includes are expanded, each layer may set each path once. `sable
check` and the GUI report both places:

```
error[C0102]: `theme.token.accent` is set twice in the user layer
  --> ~/.config/sable/sable.kdl:4:5
  --> ~/.config/sable/colors.kdl:12:1
  = help: keep one; the rice → user cascade already overrides the rice's value
```

### 7.4 Schemas

```rust
#[derive(Schema, Clone, PartialEq)]
#[schema(path = "panel.*", reload = "live")]
pub struct PanelConfig {
    pub output: OutputRef,
    pub anchor: Anchor,
    #[schema(default = "36px", range = 16..=256)]
    pub height: Px,
    #[schema(default = "\"default\"", r#ref = "theme.style.*")]
    pub style: StyleRef,
    pub slot: OrderedMap<SlotName, Vec<FacetInstance>>,
}
```

- Schemas are embedded in each binary (an ELF note, `.note.sable.schema`) and read by `sabled`
  **without running it**.
- **Each key has one owner component.** Two owners for one path is a load error. There is one
  `theme.token.accent`, owned by the theme, and everything that needs an accent refers to it.
- **References** (`ref`) are typed links (`style "glass"` → `theme.style.glass`). Shared values are
  referenced, never copied.
- The schema generates the settings GUI (color pickers for `Color`, sliders for ranges, device
  pickers for `OutputRef` and `DeviceMatch`, key-capture fields for bindings), the reference
  manual, and the language server's completion and diagnostics.

### 7.5 State synchronization

`sabled`'s config authority owns the effective tree:

1. **Change.** A file changes (`EVFILT_VNODE` `NOTE_WRITE | NOTE_RENAME | NOTE_DELETE` on each file
   and its directory, which catches editors' atomic renames), or a runtime write arrives.
2. **Build.** Parse the changed layers, expand includes, validate against schemas, resolve the
   cascade into candidate generation *n+1*, and compute per-subtree diffs. If anything is invalid,
   generation *n* stays, and the GUI shows the diagnostics with file and line.
3. **Prepare.** Each subscriber of an affected subtree gets a typed `Diff<T>` of its own schema type
   and checks it semantically (for example, that a mode exists on that monitor). It answers ACK or
   NACK with a reason within 250 ms.
4. **Commit** if every subscriber agreed. Otherwise abort, keep generation *n*, and report the
   reason.

- Components report the generation they run, and the inspector shows any that lag.
- Components never parse KDL. There is one parser.
- A Facet instance subscribes to its own subtree and the theme tokens it references, nothing else.
- **Risky keys** (marked `confirm`: modes, scale, GPU choice) apply with a revert timer ("Keep this
  display configuration? Reverting in 15 s"). This is the one built-in safety net, because a bad
  mode can leave no working screen. It can be turned off per key.
- **Safe mode.** A key held at login, or `sabled --safe`, starts with only Defaults and System. The
  GUI then offers to open the config, bisect includes, or turn off the rice. Recovery never needs a
  terminal.

### 7.6 One canonical way

| Task | The one way | Deliberately absent |
|---|---|---|
| Bind keys, pad chords, buttons, gestures | Components **export actions**. The user binds them in the one `bind` subtree | Components registering global shortcuts, per-component keybinding settings |
| Script the desktop | `sablectl action <name> [args]`, over the same action registry | Per-component scripting interfaces |
| Change a setting | Its path in KDL, or the generated GUI, which edits that path | Hidden state files, settings databases, a dconf analogue |
| Build shell UI | A Facet (§8) | Layer-shell in the SDK. Foreign layer-shell clients work as compatibility |
| Talk between components | Conduit channels from the broker | Ad-hoc sockets, files as IPC, D-Bus |
| Capture the screen | `capture.Session` (or its `ext_image_copy_capture` front door) | Side channels |
| Theme anything | Tokens and styles in `theme` | Per-component colors |
| Write a shader | SSL against the fixed shader interface | GLSL, HLSL, or raw SPIR-V input |
| Rename a key | A **migration** shipped with the component: `sable migrate` rewrites the file, keeping comments, and the GUI shows the diff | Aliases. Old spellings are not accepted, so there is only ever one |

`sable check --strict` (run on every reload) also flags **redundancy**: a user value equal to what
it overrides is reported as "this line has no effect".

### 7.7 Grants are config

Security policy lives in the same tree under `grants` and `security`, under the same rules: one path
per grant, the cascade, typed diffs, and immediate revocation when a grant is removed. Users can
`git diff` their security posture and see, for every capability a component holds, the file and
line that granted it. Rices can't touch these subtrees.

## 8. Component ecosystem

### 8.1 Facets

Panels, docks, the launcher, tray, notifications, OSDs, widgets, the lock screen, the greeter, and
settings are all **Facets**. A Facet is:

- a crate under `desk/facets/` (or a third party's) on the `desk/facet` SDK, built as its own binary;
- **its own process**, because the process is the unit of privilege. It is spawned and supervised by
  `sabled`, in capability mode from its first line of code;
- **declarative**: it sends a retained scene that the compositor lays out, styles, animates,
  renders, and hit-tests;
- configured by its subtree of the one tree, and themed by the shared tokens and styles.

```
 Facet process (sandboxed): state + update(msg) + view() → Node tree
        │  the SDK diffs successive views and sends patches over Conduit `scene.Surface`
        ▼  (shared memory for large batches); events come back by node key
 sable-comp: scene store → style resolution → lib/flexgrid layout → animation → render graph → hit-test
```

### 8.2 Why the compositor renders Facet UI

| Concern | Each Facet draws its own Wayland surface | **The compositor draws Facet scenes (chosen)** |
|---|---|---|
| Consistent theming | Every toolkit reads themes its own way | One renderer and one style engine |
| Blur, shadows, shaders | Reimplemented per component, without access to the backdrop | Node properties |
| Smooth animation at 240 Hz and up | Each process wakes and renders every frame | Animations are declared and run in the compositor, so the Facet sleeps |
| GPU memory | Buffers per widget | Shared atlases and one graph |
| Attack surface | Each widget needs a GPU and a Wayland connection | A Facet needs neither, only a scene channel |
| Crash isolation | Good | Equal. A crashed Facet's last scene stays up (marked stale after 2 s) until it restarts |
| Custom rendering | Native | A `Canvas` node: the Facet is granted a render node and draws with Vulkan into a dma-buf. It is part of the same model, not a second path |

### 8.3 The scene model

| Node | Purpose |
|---|---|
| `Box` | Container: flex or grid layout, padding, background, border, radius, shadow, backdrop effect |
| `Text` | Shaped text with font and color tokens, truncation, rich spans |
| `Icon` | An icon by name (the compositor looks up the icon theme) or by asset |
| `Image` | Raster or SVG from a granted asset or a sealed memfd |
| `Shape` | Paths, rounded rectangles, arcs (progress rings, graphs) |
| `Slot` | **Embeds another Facet's scene** here (§8.5) |
| `Surface` | A live or thumbnail view of a window, by an opaque handle from the `foreign-toplevel` grant |
| `Canvas` | A Facet-rendered dma-buf |
| `Effect` | An SSL `fill` or `backdrop` shader over its subtree |
| `Input` | A text field run by the compositor, so input methods, selection, and clipboard behave the same everywhere |
| `List` | A virtualized list or grid that materializes only the rows in its viewport plus a margin ([TOOLKIT.md](TOOLKIT.md) §4.2, [FILEMANAGER.md](FILEMANAGER.md) §4.1) |

**Styles** are defined only in `theme`:

```kdl
theme {
    token "surface" (color)"#1e1e2ecc"
    token "font.ui" "Inter"

    style "glass" {
        background "@surface"
        backdrop blur=(px)24 saturation=1.2
        radius "@radius"
        border (px)1 "@accent" alpha=0.25
    }
    style "workspace-button" {
        padding (px)4 (px)10
        state "active" { background "@accent" ; text-color "@base" }
        state "hover"  { background "@accent" alpha=0.3 }
        transition "background" (ms)120 curve="ease-out"
    }
    animation "window-open" { shader "transitions/pop.ssl" duration=(ms)180 }
}
```

The style cascade is **class → state → inline-from-config**, with no descendant selectors and no
specificity rules. A class looks the same wherever it appears, which keeps rices predictable and
resolution linear in the number of nodes.

**Events** (enter, leave, press, release, scroll, activate, and keys while the Facet has keyboard
focus) arrive tagged with node keys.

### 8.4 The Facet SDK

```rust
use sable_facet::prelude::*;

#[derive(Schema, Clone, PartialEq)]
#[schema(path = "facet.sable-battery")]
pub struct Config {
    #[schema(default = "true")]
    show_percent: bool,
    #[schema(default = "15", range = 1..=50)]
    warn_below: Percent,
}

pub enum Msg { Changed(BatteryState), Clicked }

#[facet(
    name = "sable-battery",
    requires("sable.power.Battery", mode = "read", reason = "show the battery level"),
    actions(open_power_settings),
)]
pub struct Battery { cfg: Config, state: Option<BatteryState> }

impl Facet for Battery {
    type Config = Config;
    type Msg = Msg;

    fn init(cx: &mut Init<Self>) -> Self {
        let battery = cx.grant::<power::BatteryProxy>();
        cx.subscribe(battery.changed(), Msg::Changed);
        Battery { cfg: cx.config().clone(), state: None }
    }

    fn update(&mut self, msg: Msg, cx: &mut Cx<Self>) {
        match msg {
            Msg::Changed(s) => self.state = Some(s),
            Msg::Clicked => cx.invoke("settings.open", ("power",)),
        }
    }

    fn view(&self) -> Node {
        let Some(s) = &self.state else { return Node::empty() };
        row().class("battery").state_if(s.percent < self.cfg.warn_below, "warn")
            .child(icon(s.icon_name()))
            .child_if(self.cfg.show_percent, text(format!("{}%", s.percent)))
            .on_activate(Msg::Clicked)
    }
}

sable_facet::main!(Battery);
```

- An Elm-style `init` / `update` / `view`. The SDK diffs views by node key and sends minimal
  patches. Facets never see Conduit framing or scene IDs.
- `#[facet]` generates the **manifest** (name, version, requested grants with reasons, actions with
  typed arguments, slots, the config schema) into an ELF note, `.note.sable.manifest`.
- `cap_enter()` runs before `init`, always. A Facet that needs files asks a provider, such as
  `fs.Picker`, which shows a compositor-drawn chooser and returns one rights-limited fd.
- `sable_facet::test` renders a Facet headlessly through the real render graph (headless backend,
  lavapipe) for snapshot tests, with fake providers standing in for grants.

### 8.5 Composition by slots

- A container Facet (panel, dock, sidebar, overview) declares named `Slot` nodes.
- The user's config places Facet **instances** in slots (§7.2).
- `sabled` spawns each instance, and the compositor **stitches** its scene into the slot at layout
  time. Neither process sees the other's scene.
- Constraints flow down (slot size, axis, theme scope) and size requests flow up, all in one layout
  pass.

A crash in the network widget can't touch the clock. A compromised weather widget holds only the
`net.Http` capability it was granted, which config scopes to one host.

**Cost.** The SDK target is **≤ 3 MiB resident** for a simple widget: it has no GPU context, no fonts,
and no toolkit, since rendering happens in the compositor. That budget is enforced in CI. There is no
second, in-process plugin path (P4).

### 8.6 Built-in components

| Crate | Kind | Notes |
|---|---|---|
| `sable-greet` | Facet (privileged) | The graphical login on the console VT, through the `auth` provider. It starts `sabled` for the user, and runs first-boot setup |
| `sable-panel` | Container | Bars on any edge, several per output, auto-hide, exclusive zones in the layout policy |
| `sable-dock` | Container | Pinned apps from config, running indicators and live previews through `foreign-toplevel` |
| `sable-launcher` | Facet | Fuzzy search of `.desktop` entries (an `apps.Catalog` provider scans `/usr/local/share/applications` and `/usr/share/applications`), a game library (native and Wine), and a command palette over the action registry |
| `sable-tray` | Container | Native tray items are Facets in its slots (§8.8 covers legacy apps) |
| `sable-notify` | Facet + provider | The native `notify.Post` interface, notification center, do-not-disturb by profile, kept out of capture by default |
| `sable-osd` | Facet | Volume, brightness, capture state, the controller radial menu |
| `sable-lock` | Facet (privileged) | Lock screen UI over `ext_session_lock_v1`. Authentication goes through the `auth` provider |
| `sable-settings` | Facet app | Schema-generated settings, capability inspector, profiles, rice manager, latency dashboard |
| `sable-cast-ui` | Facet | Stream and recording dashboard, scenes, replay buffer |
| `sable-patchbay` | Facet | Audio routing editor, writing `audio.routing` |
| Widgets | Facets | `sable-workspaces`, `-clock`, `-battery`, `-audio`, `-network`, `-window-title`, `-sysmon`, `-media` |

Built-ins use **exactly** the SDK and grants third parties use. A built-in that needs something new
gets a public, versioned interface, not a private one.

### 8.7 Distribution and ricing

- **Facets** from AbyssBSD ship in `desktop.txz`. Third-party Facets install into
  `~/.local/share/sable/facets/` (built with `cargo` from their own source) or as packages under
  `/usr/local/libexec/sable/facets/`. `sabled` looks in exactly these places and reads each
  manifest without running the binary. A new Facet appears in settings with its requested grants
  **before** it can run.
- **Rices** are directories:

  ```
  abyssal/
    rice.kdl        # theme, layout, animation, and shader keys only (§7.3)
    shaders/*.ssl
    assets/         # wallpapers, icons, fonts
    preview.png
  ```

  The rice manager installs one from a git URL or an archive, previews it live as a runtime-layer
  include, and reverts on cancel. Since rices are data, installing one is safe by construction.
- **Sharing a setup** means sharing `sable.kdl`, its includes, and the rice. `sable check` lists
  referenced Facets that aren't installed and offers to install them.

### 8.8 Legacy interoperability

Many applications show tray icons and notifications only through D-Bus (StatusNotifierItem,
`org.freedesktop.Notifications`). The canonical architecture has no D-Bus. An optional
compatibility Facet, off by default, **`sable-compat-freedesktop`**, runs a **private, jailed
`dbus-daemon`** from packages. Only applications the user opts in can reach it, because their jail
profile mounts its socket. It translates tray items into native tray Facet scenes and notifications
into `notify.Post`, and it holds only `tray.Host` and `notify.Post`. Foreign layer-shell clients
(waybar and the like) work through the grant-gated `zwlr_layer_shell_v1`, and they don't follow
Sable themes. These paths run existing software. Nothing new is built on them.

### 8.9 Applications and the toolkit

ROADMAP M-D also asks for a terminal emulator and a minimal toolkit. GUI-first does not mean
"no terminal": developers need one. The toolkit reuses the Facet scene model with a different
host: **`lib/scene-render`** is the compositor's scene renderer as a library. An application builds
the same `Node` trees and renders them client-side into its own Vulkan surface as an ordinary
`xdg_toplevel`. Applications and the shell therefore share one UI model, one style system, and one
theme. The first applications on it are `sable-term`, `sable-settings` as a window, and a file
picker. Their designs are separate documents: the toolkit is [TOOLKIT.md](TOOLKIT.md) (draft), the
visual and interaction rules are [STYLE.md](STYLE.md) (draft), `sable-term` is
[TERMINAL.md](TERMINAL.md) (draft), and `sable-files`, the file manager that also serves
`fs.Picker` (§8.4), is [FILEMANAGER.md](FILEMANAGER.md) (draft).

## 9. Kernel and platform requirements

The desktop asks for these of AbyssBSD. Most are FreeBSD ABI, and so they are already in the
plan:

| Need | Used for | Status |
|---|---|---|
| `SOCK_SEQPACKET`, `SCM_RIGHTS`, `LOCAL_PEERCRED` | Conduit, Wayland | P6.5, done |
| kqueue: `EVFILT_PROCDESC`, `EVFILT_TIMER` `NOTE_ABSTIME`, `EVFILT_USER`, `EVFILT_VNODE`; eventfd | Every event loop | P6.4, done (`EVFILT_VNODE` to check against config-watching tests) |
| `memfd_create`, `shm_open(SHM_ANON)`, seals | Rings, keymaps, scenes | P6.3, done |
| `pdfork`, `pdkill` | Supervision | P4.5, done |
| `rtprio`, `rtprio_thread` | Realtime threads | P6.2, done |
| Capsicum, Casper (`cap_sysctl`), jails, devfs rules | Sandboxing | P6.7, planned |
| evdev with `EVIOCGRAB`, `EVIOCREVOKE`, `EVIOCSCLOCKID`; `hidraw` | Input | Track U, done |
| **uinput** | Virtual controllers | **Missing**. FreeBSD ABI (evdev's `uinput`), to add to `sys/evdev` |
| **HID game controllers** (`hgame`, `xb360gp`, `ps4dshock` roles) | Controllers | **Missing**. HID class drivers in `sys/hid` |
| DRM/KMS core, `gop-kms` | Display | M-G.1 |
| Syncobj, sync files, dma-buf, `virtio-gpu` 3D | Explicit sync, Venus | M-G.2, M-G.3 |
| Async flips, VRR, HDR metadata, `Colorspace`, `content type`, `max bpc` | Gaming and HDR | GRAPHICS.md §13 question 5, **to schedule** with M-G.4–M-G.6 |
| GPU video encode engines (VCN; the Sky1's codec) | `sable-cast` | GRAPHICS.md §13 question 3 (media design) |
| `sound(4)` OSS, HDA, USB audio (isochronous) | Audio | ROADMAP "audio later", **to schedule** |
| **CUSE** (`/dev/cuse`, `libcuse` in base) | Per-app audio nodes, webcams | **Missing**. FreeBSD ABI |
| VNET jails | "No network" jail profiles | Left out of P6 by MULTIUSER.md |
| `rctl` | Component resource limits | Answers as absent in P6. `setrlimit` until then |
| `backlight(9)`, ACPI battery and `hwpstate` sysctls | Providers | GRAPHICS.md §7.7, P9 |
| Networking, `wpa_supplicant` | `net` provider, streaming | P8 |

## 10. Testing and performance

### 10.1 Testing

QEMU first (ADR-0010), as everywhere else:

- **Host**: every `lib/` crate (`wayland-wire`, `kdl`, `raster`, `textlayout`, `svg`, `shade`,
  `flexgrid`, `dsp`) is host-tested and fuzzed, and Conduit decoders are fuzzed through their
  generated targets. Inventory libraries (§2.1) rely on their upstream fuzzing, and the FFI module
  in front of each is fuzzed here. `lib/shade`'s output is checked with `spirv-val`.
- **Headless**: the compositor's headless backend on lavapipe renders scripted sessions (test
  clients driving xdg-shell, dmabuf, syncobj, presentation) and compares pixels with stored images.
- **QEMU**: the compositor on `gop-kms` with lavapipe, then `virtio-gpu` with Venus, booted by
  `cargo xtask test --tier desktop` on both architectures.
- **Differential**: the same client test programs, including weston's (MIT), against sway on
  FreeBSD 15.1, comparing protocol behavior event for event where the protocols specify it.
- **Physical**: the latency harness of §5.8 on the reference machines once their native drivers
  land (M-G.4–M-G.6), measuring evdev timestamps against presentation feedback, plus a photodiode
  rig for end-to-end checks.

### 10.2 Performance

Under ADR-0014, the desktop is measured as components, each against **sway on FreeBSD 15.1 with
`drm-kmod`, on the same machine**:

| Component | Measures |
|---|---|
| `desk.frame` | Compositor frame time with 1, 8, and 32 windows, tiled and floating, with and without blur |
| `desk.latency` | Input report to presentation for a locked-pointer client, and to the cursor plane |
| `desk.flip` | Client commit to flip, direct scanout and composited |
| `desk.capture` | GPU cost and latency of a 1080p60 capture, against `wf-recorder` on sway |
| `desk.startup` | Login to first frame, and Facet spawn time |
| `desk.audio` | Round-trip latency and xruns against `virtual_oss` on FreeBSD (once audio exists) |

The bound is ADR-0014's: the geometric mean no more than 2% worse than sway, and the aim is to be
better. Where FreeBSD has no driver for the GPU (panthor), results are recorded and do not gate,
until GRAPHICS.md §13 question 7 is decided.

## 11. Relationship to the roadmap

| ROADMAP M-D item | Here |
|---|---|
| Dynamic tiling, workspaces, animations | §4.9, §8.3 |
| IPC | Conduit, §6 |
| Declarative configuration | §7 |
| Wayland protocol library | `lib/wayland-wire` and `tools/waylandgen`, §2.1 |
| Renderer, software → Vulkan | Vulkan only, with lavapipe as the software step, §4.7, §4.8 |
| Panel, launcher, notifications | §8.6 |
| Terminal emulator, minimal toolkit | §8.9 |
| kqueue, devd, Capsicum, sysctl and rc | §4.8, §6, §6.8 |
| Prerequisites: M-G, audio, power management | §9 |

## 12. Open questions

1. **The name.** "Sable" is a placeholder. Something in AbyssBSD's register may suit better.
2. **The aarch64 userland target.** `aarch64-unknown-freebsd` has no prebuilt `std`, and ADR-0003
   rules out `-Zbuild-std`. Build natively with FreeBSD's `rust` package (proposed), or ask M-U to
   settle an AbyssBSD userland target first.
3. **Color glyphs.** HarfBuzz shapes every script, but color emoji still need `lib/raster` to
   draw `COLRv1` paint lists and place `CBDT`/`sbix` bitmaps. Do they come with M-D.3 or later?
4. **ffmpeg's configuration.** The desktop needs ffmpeg's LGPL configuration with only the
   inventory's externals enabled. A packaged ffmpeg built with GPL options (x264, x265) is GPL as a
   whole. Should AbyssBSD ship its own LGPL ffmpeg build in `desktop.txz`'s dependencies, require a
   package flavor, or link SVT-AV1, OpenH264, and libopus directly and keep only libavformat?
5. **Base libraries.** Linking `libpam` and `libssl` from `base.txz` is proposed (§2.1). The M-D ADR
   should record that rule, and whether M-U's Rust replacements will take their place.
6. **Install prefix.** `desktop.txz` installs base-style (`/usr`, `/etc/sable`), while its package
   dependencies live in `/usr/local`. Is the desktop part of the base system, or is it a set layered
   over it?
7. **Compositor restart survival.** A thin proxy holding client sockets would let clients outlive a
   compositor crash, at the cost of one hop. Proposed for later, off by default.
8. **Remote desktop.** WHIP in reverse, with input injected through the router, is natural but
   widens the network surface. It would be an opt-in Facet, later.

## 13. Phasing within M-D

The foundations need only P6 and can start before M-G finishes. The compositor steps follow M-G's.

| Step | Work | Needs | Exit |
|---|---|---|---|
| **M-D.0** | This document finalized, and the M-D ADR (dependencies and base libraries, §12 questions 2, 4, 5, 6) | — | ADR accepted |
| **M-D.1** | `sable-rt`, Conduit (with fuzzing), `sabled` (supervisor, broker, config authority), `lib/kdl` and `Schema`, `sable-sysd` with `seat` and `auth` | P6.7 | Two sandboxed processes exchange typed messages over brokered channels in QEMU, and config reloads with typed diffs and ACK/NACK |
| **M-D.2** | `sable-comp` on `gop-kms` with lavapipe: xdg-shell, shm, `wp_presentation`, tiling and workspaces, xkbcommon keymaps, keyboards and pointers, headless and nested backends | M-G.1 | A tiled session of Wayland clients from packages in QEMU and on both reference machines' firmware framebuffers |
| **M-D.3** | dmabuf, syncobj, the render graph with blur, text (HarfBuzz, `lib/raster`, `lib/textlayout`), `lib/shade`, the Facet SDK, slots, the panel, launcher, workspaces and clock widgets, the generated settings GUI, `sable-greet`, `sable-lock` | M-G.2, M-G.3 | A complete graphical session in QEMU on Venus, with no terminal needed for any configuration task |
| **M-D.4** | Direct scanout, VRR, tearing, the raw pointer path, the controller router, Game Mode, `sable-xwm` | M-G.4–M-G.6, uinput, HID controllers | `desk.frame`, `desk.latency`, and `desk.flip` at parity with sway, and full-rate pointer delivery to a locked-pointer client |
| **M-D.5** | `sable-audiod`, capture sinks and masks, `sable-cast` (Vulkan Video, replay buffer, RTMP) | Audio, CUSE, video engines | A 1080p60 stream with stream-only overlays and per-application audio, with `desk.capture` at parity |
| **M-D.6** | Jail profiles, the remaining providers, rice manager, `sable-compat-freedesktop`, `sable-term`, `desktop.txz` in the installer | P8, P9 | The desktop installs from AbyssBSD media and passes its suites on both reference machines |
