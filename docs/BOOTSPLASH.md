# Boot splash and boot time

- Status: **Draft**. Not yet on the roadmap (§12 question 1). It becomes an ADR when
  implementation starts
- Date: 2026-11-10
- Scope: what the screen shows from the firmware's logo to the `vt` login prompt or the desktop's
  login window, with the boot's stages shown as icons that light up in turn; and the boot-time
  work that makes that sequence short: timeline instrumentation from power-on, budgets per stage,
  and the known places where boots wait. Shutdown gets the same treatment in reverse (§5.6)
- Inputs: [GRAPHICS.md](GRAPHICS.md) §7.5 and §7.6 (firmware handover, the kernel console),
  [DESKTOP.md](DESKTOP.md) (the login window), [DRIVER-MODEL.md](DRIVER-MODEL.md) §6.4 (parallel
  attach), [MULTIUSER.md](MULTIUSER.md) §16 and [PERFORMANCE.md](PERFORMANCE.md) (the `boot`
  component), [ADR-0015](adr/0015-usb-input-vt.md) (the early console and `vt`)

The short version: **one picture, drawn once by the loader and kept alive until the login, that
never makes the boot wait.** The loader decodes the theme, converts it to the framebuffer's pixel
format and scale, and draws the first frame before it reads the kernel. It hands the prepared
sprites to the kernel in `BootInfo`. A low-priority kernel thread animates them as the boot
reports its stages: a row of icons, dim until their stage is done, each lighting up with a short
cross-fade and glow, the active one breathing. The boot reports stages through a typed
`boot::Stage` event; nothing in the boot waits for a frame. The switch is FreeBSD's: `boot_mute`
(`RB_MUTE`, `boot -m`) and the `splash` loader variable, with FreeBSD's semantics where they exist
(§4). Serial output is untouched unless FreeBSD's own rule mutes it.

Boot time is the other half. A splash shows the boot's length; it doesn't hide it. The design
adds FreeBSD's `TSLOG` (loader and kernel, `kern.tslog`), reads the firmware's own timeline from
the ACPI FPDT, and sets budgets per stage on the reference machines, measured before anything is
tuned (§8, [audit before tuning](PERFORMANCE.md)).

Behavior reference: sys/dev/vt/vt_core.c (`vtterm_splash`, `VDF_SPLASH`, the keypress and
`rebooting` paths), sys/dev/vt/vt_cpulogos.c (`kern.vt.splash_cpu*`), sys/kern/kern_cons.c
(`cn_mute`, `kern.consmute`, the `RB_MUTE` rule), stand/common/gfx_fb.c (the loader's `splash`
and `shutdown_splash` images, `MODINFOMD_SPLASH`, `MODINFOMD_SHTDWNSPLASH`),
stand/defaults/loader.conf, sys/kern/kern_tslog.c and stand/common/tslog.c (`TSLOG` across the
handoff), sys/kern/kern_boottrace.c. The hardware references are the UEFI specification (GOP,
§12.9) and the ACPI specification (FPDT §5.2.24, BGRT §5.2.23). The look is AbyssBSD's own: the
BeOS boot screen is the inspiration for the idea (icons for stages, lit in turn), not a source of
artwork, icons, or layout.

## 1. What, and why

### 1.1 What is there today

- The loader prints a few `loader:` lines on the firmware's console, finds the GOP framebuffer
  (`boot/loader-efi/src/devices.rs`), maps it write-combining, and passes it in
  `bootproto::Framebuffer`. It never sets a mode: the kernel inherits the firmware's.
- The kernel draws text on it from the first line of `kmain` (`drivers/fb-console`, 8×16 cells),
  and `vt` takes it over when it attaches (`kernel/src/vtcons.rs`, `sys/vt`).
- `kern.boottrace` exists as FreeBSD's nodes, off. There is no `TSLOG`, and nothing reads the
  FPDT.
- Boot time is the P6 `boot` component (MULTIUSER.md §16): handoff to `login:` on the serial
  console, and `rc` alone. In bhyve AbyssBSD is well inside the bound (0.532 at P6's correction,
  2026-10-30). Nothing measures firmware or loader time, and nothing has been measured on the
  Orange Pi 6 Plus or the X870E.

So the screen today is: the firmware's logo, a mode-less blank or the firmware's text, a few
loader lines, then kernel text scrolling until `vt`'s login. That is right for development, and
stays the default there (§4.3).

### 1.2 Goals

- **One continuous picture.** No mode set, no blank, no flash of text between the firmware's
  logo and the login prompt or desktop. The first AbyssBSD frame is on screen within 50 ms of the
  loader starting, before the kernel is read.
- **The stages are visible.** A row of icons, one per boot stage (§3), lit as each completes,
  with transitions: a cross-fade from dim to lit, a short glow, the active icon breathing. A thin
  progress line under the row moves smoothly, calibrated by the previous boot's timeline (§5.4).
- **Free.** The splash never delays the boot. The `boot` component's gate is measured with the
  splash on, and the splash costs under 1% of it (§9).
- **FreeBSD's switches.** `boot_mute`, `-m`, `splash`, `shutdown_splash`, `kern.consmute`,
  `kern.vt.splash_cpu*` behave as on FreeBSD; a FreeBSD `loader.conf` that sets them gets what it
  asked for.
- **Safe.** A panic, `ddb`, or a fatal trap takes the screen back for text, as FreeBSD does,
  and so does a keypress. The splash never hides why a boot stopped (§6).
- **A shorter boot**, measured from power-on, with budgets per stage on each reference machine
  (§8).

### 1.3 Non-goals

- A boot menu or loader UI. AbyssBSD's loader has none and gets none here.
- Changing FreeBSD's userland. `rc` stays serial and unmodified; the kernel infers `rc`'s
  progress (§3.2).
- Native display drivers. The splash runs on the GOP framebuffer; KMS takes over through
  GRAPHICS.md §7.5's adoption, without a mode set.
- Audio (a boot chime) and a graphical password prompt for GELI. Both later (§11).

## 2. FreeBSD's behavior

- **`boot_mute`** (`loader.conf`, or `boot -m`) sets `RB_MUTE` in `boothowto`. The kernel then
  sets `cn_mute` (`kern.consmute`), which drops console output on every console, serial
  included, unless `RB_VERBOSE` or `RB_ASKNAME` is also set (`kern_cons.c`).
- **`splash`** names an image the loader loads (`stand/common/gfx_fb.c`, PNG). The loader draws it
  while it runs and passes it as `MODINFOMD_SPLASH` metadata; `shutdown_splash` likewise as
  `MODINFOMD_SHTDWNSPLASH`. The defaults are `/boot/images/freebsd-logo-rev.png`.
- **`vt`** draws the splash at attach when `RB_MUTE` is set (`vtterm_splash`), sets `VDF_SPLASH`,
  and clears it, redrawing text, when a terminal is opened (`vtterm_opened`: `getty` on `ttyv0`),
  when the console is read (`vtterm_cngetc`), and on entering the debugger (`vtterm_cngrab`).
  While it is set, the window is not drawn. At `reboot` it draws the shutdown image.
- **`kern.vt.splash_cpu`** draws one logo per CPU along the top for
  `kern.vt.splash_cpu_duration` seconds, in one of three styles.
- **`TSLOG`**: the loader logs timestamped enter/exit events into a buffer it passes as the
  `TSLOG data` preload; the kernel's `kern_tslog.c` appends its own (`TSENTER`, `TSEXIT`,
  `TSWAIT`, `TSUNWAIT`, `TSHOLD`, `TSRELEASE`) and exports both through `kern.tslog` and
  `kern.tslog_user`, which FreeBSD's boot flame-chart tooling reads.

FreeBSD has a still picture and no notion of stages. AbyssBSD adds the stages and the motion
behind the same switches, and keeps FreeBSD's still behavior when `splash` names a plain PNG
(§4.2).

## 3. Stages

### 3.1 The stages

Seven, chosen so each is a real wait a user could see, and each ends at an event the kernel
already has:

| # | Stage | Starts | Ends (the icon lights) |
|---|---|---|---|
| 1 | Firmware | power-on (FPDT) | the loader's first frame |
| 2 | Loader | loader entry | `ExitBootServices` |
| 3 | Kernel | handoff | CPUs started, timers and memory up (`smp::start_cpus` returns) |
| 4 | Devices | `devices::init` | the device tree settles: no attach in flight, no hold outstanding |
| 5 | Storage | root mount begins | root mounted (`vfs_mountroot` done; with `fsck`, when `rc`'s `fsck` exits clean) |
| 6 | Services | `init` executes | `rc` exits (init's `runcom` reaps it) |
| 7 | Ready | | the first `getty` opens a `ttyv`, or a DRM master takes the display |

Network is not a stage. It overlaps 6 and 7 (`dhclient` backgrounds, `rc` does not wait for a
lease unless asked), and making it a stage would show a wait the boot doesn't have. A theme may
add a status glyph for it (§5.2).

### 3.2 The event

```rust
/// A boot stage, in order. `#[non_exhaustive]` so themes can't rely on the count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Stage { Firmware, Loader, Kernel, Devices, Storage, Services, Ready }

/// What the boot tells the splash. Monotonic: a stage never un-completes.
pub enum BootEvent {
    Begin(Stage),
    Done(Stage),
    /// Progress inside a stage, when the stage can count its work (devices attached of devices
    /// known; rc scripts run of scripts listed).
    Within { stage: Stage, done: u32, of: u32 },
    /// The boot has stopped for a reason a person must see: the screen goes back to text.
    Stopped(StopReason),
}
```

`boot::report(event)` is one atomic store into a sequence-numbered state word and a wakeup;
it never takes a lock the boot holds and never blocks (§5.3). The same call writes the `TSLOG`
mark (§8.1), so the timeline and the icons can't disagree.

Inside `rc`, FreeBSD already reports each script to the kernel, unmodified, when
`kern.boottrace.enabled` is 1: `rc.subr` then runs every script through `boottrace(8)`
(`boottrace_fn`), which writes `<script> start` and `<script> done` to
`kern.boottrace.boottrace` around a `fork` and `execvp` of it, and `init` logs its transitions the
same way. AbyssBSD already has those nodes (`kernel/src/mib.rs`); the splash turns boottrace on
and counts `done` records, which gives `Within { Services, .. }`. The total is the previous
boot's count (§5.4). The cost is FreeBSD's: one more `fork` and `exec` per script (about 60 on a
default install), which §9 measures; if it is more than the splash's budget, the progress line
inside Services follows time calibration alone. The icons do not depend on either.

## 4. Switches and defaults

### 4.1 FreeBSD's knobs

| Knob | Where | Effect |
|---|---|---|
| `boot_mute="YES"`, `boot -m` | `loader.conf`, the loader's command line | `RB_MUTE`: splash on, and FreeBSD's `cn_mute` rule for every console |
| `boot_verbose`, `-v` | as above | `RB_VERBOSE`: no `cn_mute`, so serial stays loud while the screen shows the splash |
| `splash="/boot/images/…"` | `loader.conf` | the boot image or theme (§4.2) |
| `shutdown_splash` | `loader.conf` | the shutdown image or theme |
| `kern.consmute` | sysctl | FreeBSD's; unmuting also ends the splash, as on FreeBSD |
| `kern.vt.splash_cpu`, `_style`, `_duration` | tunables | FreeBSD's CPU logos, drawn by the same compositor (§5.2) |

The loader reads these from `/boot/loader.conf` on the root it boots from, as it already reads
`/boot/kernel/kernel` there, and from `\EFI\ABYSS\CMDLINE`.

### 4.2 Themes

`splash` names either a PNG, which gives FreeBSD's behavior (one still image, centered), or a
theme directory (`/boot/images/abyss/`), which gives the animated splash. A theme is a small
manifest (`theme.conf`, `key=value` lines like `loader.conf`) and PNG files: background colour or
image, logo, one icon per stage in dim and lit forms at 1× and 2×, the glow, the progress line's
colours, the layout (anchors and margins relative to the screen, never pixels), and timings.

Rejected: a binary sprite format built by `xtask`. It would be faster to load by a few
milliseconds, but `splash=` must take PNG anyway for FreeBSD's `loader.conf`, and a second format
is a second parser. Decode cost is bounded by the loader's budget instead (§8.3).

### 4.3 Defaults

- Development, QEMU, CI, bhyve: off. Every test reads the console, and `boot_mute` would mute it.
- Installs from the installer ([INSTALLER.md](INSTALLER.md)): the installer writes
  `boot_mute="YES"` for desktop installs, and leaves it off for server installs, as FreeBSD's
  installer leaves it off.
- The X870E (no serial console): never on while it is being brought up (P10).

## 5. Design

### 5.1 Pieces and crates

| Piece | Where | What |
|---|---|---|
| `lib/inflate` | new, `no_std`, `forbid(unsafe_code)` | RFC 1951 inflate and the zlib wrapper, host-tested and fuzzed |
| `lib/png` | new, `no_std`, `forbid(unsafe_code)` | PNG decode to premultiplied RGBA8 (8-bit truecolour, alpha, palette, grey; no interlace needed but accepted), CRC-checked, size-limited |
| `lib/splash` | new, `no_std`, `forbid(unsafe_code)` | Theme manifest, layout, sprite atlas, the compositor, the animation state machine. Pure: given a time and a stage state it writes pixels into a slice. Host-tested with golden frames |
| `boot/loader-efi` | `splash.rs` | Read the theme, decode, scale, convert to the GOP's pixel format, draw the first frame, place the atlas in `LoaderData` memory, pass `SplashInfo` |
| `boot/bootproto` | `SplashInfo` | The atlas's physical range, its sprite table, the layout, and the loader's frame state, so the kernel continues the same animation without a jump |
| `kernel/src/splash.rs` | glue | The splash thread, `boot::report`, the takeover rules with `fb-console` and `vt` |

PNG decoding is the loader's alone. The kernel never parses an image: it receives sprites
already in its pixel format, so a malformed theme can only fail the loader's decode, which falls
back to no splash (§6).

### 5.2 The compositor

Layers, back to front: background (a colour, or an image drawn once), the logo, the icon row, the
progress line, optional status glyphs (network, a CPU row for `kern.vt.splash_cpu`). Each layer
is a set of sprites at fixed positions; motion is opacity, a small scale (the breathing icon,
±3%), and a glow sprite's opacity, never movement across the screen.

Rules that keep it cheap on any framebuffer:

- **Never read the framebuffer.** It is write-combining; reads are uncached. The compositor keeps
  what each damaged rectangle needs (background under every sprite) in its atlas and composes in
  a small scratch buffer, then writes whole lines.
- **Damage only.** A frame redraws the rectangles whose inputs changed: one icon (256×256 at 2×
  is 256 KiB) and the progress line. A 60 Hz frame is well under 1 MiB of writes.
- **Integer arithmetic.** Premultiplied RGBA8, `(a·x + (255−a)·y) / 255` by the usual
  multiply-shift, no floating point, so the kernel thread needs no FPU state.
- **Scale chosen once.** 2× assets when the framebuffer is 1440 lines or more, else 1×; layout in
  fractions of the screen so 1024×768 and 3840×2160 both look composed.
- No vertical blank on GOP. Updates are small and short, so tearing on one icon for one frame is
  the worst case; after `gop-kms` exists and a native driver adopts the display, the thread waits
  for vblank through it.

Transitions (defaults; the theme sets the timings):

| Event | Animation |
|---|---|
| `Begin(stage)` | the icon starts breathing (1.2 s period) |
| `Done(stage)` | cross-fade dim → lit over 180 ms, glow up then down over 300 ms |
| `Within` | the progress line eases toward the new fraction |
| `Done(Ready)` | the row and logo fade out over 200 ms, the background stays; then text or the desktop |

A stage that completes faster than its fade still shows the fade, and the next stage's begins
while it runs: animations overlap, the boot doesn't queue behind them.

### 5.3 The kernel thread

`splash` is a kernel thread at the lowest kernel priority above idle, started right after the
console in `kmain` (it needs only the atlas, a clock, and sleep). It runs a frame, then sleeps
until the next frame time or a `boot::report`, whichever is first. With nothing animating it
sleeps until the next event.

- **Frame rate:** 60 Hz while something animates, nothing while nothing does. Under load it
  drops frames (it computes the frame for the current time, so a late frame is a skipped frame,
  never a slow animation).
- **Contention:** it holds no lock the boot takes. The framebuffer is shared with `fb-console`
  and `vt` through one ownership flag (§5.5); `boot::report` is a store and a wakeup.
- **Exit:** after `Done(Ready)` and its fade, the thread frees the atlas pages and exits.

The loader runs the same compositor (`lib/splash`) between its steps, so the breathing of the
Loader icon is continuous: `SplashInfo` carries the animation's clock offset (the loader's
timestamp counter value at its first frame, in the same units the kernel's `TSLOG` uses) and
which stages are done.

### 5.4 The progress line

Within-stage counts are real progress but uneven. The line is calibrated by time instead, as a
second input: the loader reads the previous boot's stage durations and `rc` script count from an
EFI variable (`AbyssBootTimeline`, AbyssBSD's vendor GUID, 7 × u32 milliseconds and a u32 count)
and the kernel writes this
boot's at `Done(Ready)` through `efirt`. The line's target is the elapsed fraction of the
expected total, corrected by `Within` counts where a stage has them, and it never moves backward.
With no history, it follows the counts alone.

One variable write per boot is within any firmware's NVRAM wear budget. If `efirt` is
unavailable or writing fails, nothing is saved and the next boot uses counts.

### 5.5 Who owns the screen

One state, in `kernel/src/splash.rs`:

```rust
enum Screen { Splash(SplashOwner), Text, Graphics }
```

- `fb-console` draws only in `Text`. While `Splash`, kernel messages go to the message buffer and
  serial (subject to `cn_mute`), never to the framebuffer.
- `vt` attaches under `Splash` without drawing, as FreeBSD's `VDF_SPLASH` does, and draws its
  window when the state leaves `Splash`.
- `Splash → Text`: FreeBSD's three (a `ttyv` opened, which is `Ready`; the console read; the
  debugger), plus a keypress on any keyboard, `kern.consmute=0`, and `Stopped`. The splash fades
  the row (200 ms) unless the cause is `Stopped` or the debugger, which are immediate.
- `Splash → Graphics`: a DRM master sets `KD_GRAPHICS`. The splash's last frame stays as the
  scanout content (no clear), so the desktop's login window fades in from it (§5.7).
- Panics, `ddb`, and fatal traps take `Text` immediately from any state, as GRAPHICS.md §7.6
  says, without waiting for the thread.

### 5.6 Shutdown

`shutdown_splash`, or the theme's shutdown variant: the row lit, icons dimming in reverse as
`shutdown`'s phases pass (`rc.shutdown` running, file systems synced, devices stopped), then the
firmware. The same thread restarts at `reboot(2)` under `RB_MUTE`, as `vtterm_splash` does with
`rebooting`. It stops drawing before devices detach, so a native display driver's detach never
races it.

### 5.7 Into the desktop

When the desktop exists (M-D), the handover is one picture: `gop-kms` (GRAPHICS.md §7.5) keeps
the GOP buffer as its first plane without clearing it; the login window's first frame cross-fades
from that content. The desktop reads the splash's background colour from a sysctl (or the theme
file) so its first frame matches. Nothing here waits for M-D; until then `Ready` is the `ttyv0`
`getty`.

### 5.8 Firmware logo

UEFI firmware usually leaves its logo up, and describes it in the ACPI BGRT (image address,
offset, status). A theme may keep it (`keep_firmware_logo=YES`): the loader does not draw the
background over the BGRT rectangle, and the icon row sits below it, so the firmware's logo
becomes the splash's. This is what makes the transition from power-on seamless on the reference
machines, and is the default theme's choice when the BGRT is valid and its image fits the
current mode.

## 6. Failure and safety

- A theme that fails to load, decode, or fit the mode: no splash, a one-line note in the loader's
  log, text boot. Never a half-drawn screen.
- `Stopped(reason)` from the places a boot stops for a person: the mountroot prompt, `fsck`
  failing in `rc` (init sees `rc` exit non-zero), single-user, `RB_ASKNAME`, a root mount timeout.
  The screen goes to text with the message buffer's tail drawn, as if the splash had never run.
- A stage stuck far past its expected time (four times the calibrated duration and at least 30
  s): the icon pulses amber, and pressing any key shows text. It does not switch by itself: a slow
  `fsck` is not a failure.
- The splash thread never panics on its input. Its only inputs are the atlas the loader wrote
  (trusted after the loader's checks), the clock, and the stage word.
- `cn_mute` is FreeBSD's rule exactly, including that `-v` keeps serial loud. Under `boot_mute`
  without `-v`, the serial console is silent, as on FreeBSD; that is why the defaults (§4.3)
  leave it off wherever tests read serial.

## 7. Testing

- **Host:** `lib/inflate` and `lib/png` against the PNG suite's images (PngSuite, freely
  licensed test images; to confirm licence before vendoring) and fuzzed; `lib/splash` with golden
  frames: a fixed theme, a fixed event sequence, a fixed clock, compared pixel for pixel at both
  scales and all three pixel layouts.
- **QEMU:** a boot case `splash` (both arches): `boot_mute="YES"` with `-v`, the serial log
  checked for every `Done(stage)` mark in order, and screendumps through QMP at `Done(Devices)`
  and `Ready` compared with the golden frames by region (the icon row, not the whole screen).
  Plus: a keypress mid-boot gives text; a forced root mount failure gives text with the prompt; a
  corrupt theme boots in text.
- **Shutdown:** the same case's `shutdown -r` checks the reverse sequence's marks.
- **Boot time:** `perf --suite p6`'s `boot` component, run once with the splash on to confirm §9.

## 8. Boot time

### 8.1 Measure from power-on

- **`TSLOG`**, FreeBSD's: the loader keeps a `TSLOG` buffer and passes it as the `TSLOG data`
  preload's equivalent in `BootInfo`; the kernel adds `TSENTER`/`TSEXIT` around init steps,
  driver attaches (the manager does this, not each driver), root mount, and waits (`TSWAIT` on
  `pause`, sleeps in attach, and root mount holds). `kern.tslog` and `kern.tslog_user` give the
  same format, so FreeBSD's flame-chart scripts read AbyssBSD's boots unchanged.
- **The firmware's part**, from the ACPI FPDT: reset end, loader load and start, and
  `ExitBootServices` timestamps. Shown as the Firmware stage and logged with the boot, so a slow
  firmware setting (a long POST, a network boot option tried first) is visible.
- **`xtask boottime`**: boots a target, collects `kern.tslog` and the FPDT, and writes the
  stage table and a flame chart, so one command answers "where did this boot's time go".

### 8.2 Where boots wait

Known waits, from FreeBSD's own experience and the code paths AbyssBSD shares with it. None is
tuned before the measurement says it matters (PERFORMANCE.md):

| Wait | Typical | What AbyssBSD can do |
|---|---|---|
| Firmware POST, memory training | 1–30 s | Nothing in the OS; the FPDT shows it |
| Firmware mode set | 0.1–1 s | Never set a mode (already true) |
| Loader reading the kernel | 10–200 ms | Read in large contiguous requests; measure first |
| PCI enumeration | 10–500 ms (6.5 s in FreeBSD under bhyve) | Already fast in bhyve; confirm on hardware |
| Controller resets (NVMe `CAP.TO`, xHCI) | 50–500 ms each | Parallel attach (DRIVER-MODEL.md §6.4), already designed |
| USB enumeration (port power-on, debounce, reset per USB 2.0 §7.1.7.3) | 100 ms+ per port, serial per hub | Ports of one hub in parallel where the spec allows; root mount does not wait for USB unless the root is on USB |
| PS/2 probes | 1.7 s in FreeBSD under bhyve | Probe only what ACPI lists |
| Root mount hold | until every holder releases | Only the root's own chain holds, FreeBSD's `root_mount_hold` model |
| Entropy | until seeded | The loader's seed (already passed), so `random` never blocks the boot |
| `fsck` after an unclean shutdown | seconds to minutes | Soft updates journaling (SU+J) as FreeBSD; P11 |
| `rc` | 0.84 s in bhyve | Serial and unmodified; the kernel side (`fork`, `exec`, `rtld`) is already gated (P6 `exec`) |
| Network link and DHCP | 1–3 s link, then the lease | `rc` already backgrounds `dhclient`; the driver never waits for link at attach |

### 8.3 Budgets

Set per reference machine once `xtask boottime` has measured them, and recorded in
PERFORMANCE.md. A first guess, to replace with measurement, for the time AbyssBSD controls
(loader start to `Ready`), on NVMe:

| Stage | Budget |
|---|---|
| Loader (including the splash's decode and first frame) | 150 ms, of which the splash at most 30 ms |
| Kernel | 150 ms |
| Devices | 500 ms |
| Storage (clean) | 50 ms |
| Services | 1.5 s |
| **Loader to `Ready`** | **under 2.5 s** |

The gate stays ADR-0014's: the `boot` component within 2% of FreeBSD on the same machine. The
budgets are targets inside it.

## 9. Performance

The splash's cost to the boot is its thread's CPU time on a busy boot CPU plus the loader's
decode. Bounds:

- Loader: decode and first frame within 30 ms for the default theme at 2× (asset total under
  1 MiB of PNG).
- Kernel: the thread's total CPU time from handoff to `Ready` under 1% of the `boot` component's
  kernel time, measured as `boot` with and without `boot_mute` (10 runs a side, as P6 measured).
- The `boot` component is measured with the splash off for the FreeBSD comparison (FreeBSD's
  `boot_mute` draws a still image, a different workload), and the splash's own cost is a
  separate diagnostic, `boot.splash`.

## 10. Implementation plan

Each stage can land on its own and is useful without the next.

1. **Measurement.** `TSLOG` in the loader and kernel (`kern.tslog`, `kern.tslog_user`), the FPDT
   read, `xtask boottime`. Run on QEMU, bhyve, and (in their phases) the Orange Pi 6 Plus and the
   X870E. The stage table in PERFORMANCE.md.
2. **`boot::Stage` and `boot::report`**, wired to the marks in §3.1, writing `TSLOG` marks. No
   picture yet; the stage table comes from them.
3. **`lib/inflate`, `lib/png`**, host-tested and fuzzed.
4. **`lib/splash`**: theme, layout, compositor, animation, golden frames.
5. **Loader**: `boot_mute`, `splash`, theme load, first frame, `SplashInfo`; FreeBSD's still
   image for a plain PNG.
6. **Kernel**: the thread, the `Screen` state, `cn_mute`, `vt`'s `VDF_SPLASH` behavior, the keypress
   and `Stopped` paths, the QEMU `splash` case.
7. **Calibration** (`AbyssBootTimeline`) and boottrace-driven progress inside `rc`.
8. **Shutdown splash**, BGRT logo keeping, `kern.vt.splash_cpu`.
9. **Boot-time work** from the measurements in step 1, each change measured before and after.
10. Later, with M-G and M-D: vblank through `gop-kms`, the desktop handover.

The default theme's artwork (logo, seven icons in two states at two scales) is its own task,
original to AbyssBSD, BSD-licensed, with sources (SVG) in the tree and PNGs built by `xtask`.

## 11. Deferred

- A graphical GELI passphrase prompt in the loader (DESKTOP.md's encrypted installs). The splash
  would host it; the loader's GELI support comes first.
- A boot chime (needs audio before `Ready`, which nothing has).
- Multiple displays: GOP gives one. A native driver may later mirror the splash.
- Suspend and resume animations (P11 power management).

## 12. Open questions

1. **Where on the roadmap.** Recommendation: §10 steps 1–2 (measurement) as a Phase 9 and 10
   item, since bringing up hardware is when boot time is first seen; steps 3–8 as a track after
   P10, in QEMU first, and default-on only for installs (§4.3). Alternatively all of it as a
   track now, in QEMU, beside P9.
2. **Default on for desktop installs, or opt-in everywhere?** Recommendation: on for desktop
   installs, off for server installs and development, as §4.3.
3. **`boot_mute` muting serial.** FreeBSD's rule mutes every console. Keep it exactly
   (recommended: a FreeBSD `loader.conf` means what it means on FreeBSD; `-v` keeps serial), or
   add an AbyssBSD knob for a splash with serial left on, which would be a documented deviation.
4. **Seven stages, or fewer.** Seven reads well in a row and each is a real wait; five (merging
   Firmware into Loader and Storage into Devices) is calmer. The theme decides how many icons it
   shows, but `Stage` is fixed.
5. **Progress inside `rc`** (§3.2): turn on FreeBSD's boottrace under the splash for per-script
   progress, at one extra `fork`/`exec` per script, or time calibration alone? Recommendation:
   measure it in step 7 and keep it only if `boot.splash` stays within §9's bound.
6. **The calibration variable** (§5.4): an EFI variable, or a file on the ESP next to the
   loader's persistent log? The variable survives a read-only ESP; the file is easier to inspect.
