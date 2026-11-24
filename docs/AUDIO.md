# Audio

- Status: **Draft**. A forward design for sound on AbyssBSD. Decisions here become an ADR when
  audio enters the roadmap (its first step is M-D.1's sound lanes).
- Date: 2026-11-23
- Scope: the kernel's sound engine, its two user interfaces (FreeBSD's `sound(4)` and a native
  stream interface), the driver class, and the order of drivers. Video, codecs for compressed
  audio, Bluetooth, and effects hosting are out of scope (§1.2).
- Inputs: [DESKTOP.md](DESKTOP.md) (§3.2, §9 question 4), [WINDOWS-GAMES.md](WINDOWS-GAMES.md)
  (§7.5), [DRIVER-MODEL.md](DRIVER-MODEL.md), [USB.md](USB.md), [HARDWARE.md](HARDWARE.md),
  [ADR-0004](adr/0004-abi-target.md), [ADR-0007](adr/0007-license-and-clean-implementation.md),
  [ADR-0011](adr/0011-performance-parity.md),
  [ADR-0014](adr/0014-performance-parity-per-component.md), the desktop's own findings
  (`../AbyssBSD/desktop/docs/API-STUDY.md`, Phase 14), the clean-room specification
  `~/Projects/OS/chinese-wall/specs/sky1-audio`, and FreeBSD's `sound(4)` as the behavioral
  reference (`../freebsd-src/sys/dev/sound/`, `sys/sys/soundcard.h`, `usr.sbin/{mixer,sndctl}`,
  `lib/libmixer`).

The short version: **one sound engine in the kernel, with two faces.** The first face is
FreeBSD's `sound(4)`, reproduced as userland sees it: `/dev/dsp`, `/dev/mixer`, `/dev/sndstat`,
the `hw.snd` and `dev.pcm` trees, virtual channels. Everything in FreeBSD's base and ports plays
and records unmodified. The second face is a small native stream interface for what the Open
Sound System interface cannot express and macOS's audio system is valued for: timestamped I/O
cycles, shared-memory streams with no system call per buffer, control of another program's
stream, live rerouting, and aggregate devices. Both faces are views of the same streams in the
same device graph, mixed once, so there is never a second authority over a device.

---

## 1. Goals and non-goals

### 1.1 Goals

| # | Goal | Why |
|---|---|---|
| G1 | **FreeBSD's sound userland runs unmodified**: `mixer(8)`, `sndctl(8)`, `beep(1)`, `cat /dev/sndstat`, and every port that speaks the Open Sound System interface (Firefox through cubeb, SDL, mpv, the sndio, PipeWire, PulseAudio, and JACK back ends) | The ABI target is FreeBSD userland ([ADR-0004](adr/0004-abi-target.md)); the desktop is built on it ([DESKTOP.md](DESKTOP.md) §3.2) |
| G2 | **The capabilities of macOS's audio system** (§3), offered natively, without a sound server in user space | The request; and the desktop's study concluded that a server which mixes again "is a second authority, not a first" |
| G3 | **The I/O cycle cannot block or allocate, by construction** | A late cycle is an audible defect. This is audio's version of the fence rule in [GRAPHICS.md](GRAPHICS.md) G3 |
| G4 | **Time is first class**: every device has a clock that relates sample position to system time, and every latency the system reports is the whole path's | Synchronization with video, aggregate devices, and honest latency all follow from it |
| G5 | **Devices come and go**: unplugging a device, or changing the default, never kills a stream that could continue elsewhere, and never panics | [DRIVER-MODEL.md](DRIVER-MODEL.md) G2, G7 |
| G6 | **Performance parity** with FreeBSD 15.1's `sound(4)` on the same machine, per component | [ADR-0011](adr/0011-performance-parity.md), [ADR-0014](adr/0014-performance-parity-per-component.md) |
| G7 | **Drivers are safe Rust**, and the signal-processing code is pure, host-tested library code | DRIVER-MODEL.md G2; ENGINEERING.md |
| G8 | **Both instruction sets from the first driver**; nothing assumes a PCI sound card | The Sky1's audio is an island of a controller, serial audio ports, a DMA engine, and a signal processor |

### 1.2 Non-goals

- **A sound server.** No daemon mixes, routes, or holds device state. Ports that are sound
  servers still run, as clients.
- **ALSA, or any Linux sound interface, in the kernel.** Linux programs under the Linux
  personality reach sound as they do on FreeBSD, through user-space libraries that speak the
  Open Sound System interface.
- **Effects hosting, spatial rendering, and compressed formats.** The equivalents of macOS's
  Audio Units and codec services are libraries above this design. The engine carries linear
  samples.
- **Bluetooth audio.** There is no Bluetooth stack. When there is one, a Bluetooth device is
  one more device in the graph.
- **`virtual_oss(8)` in the first version.** It needs `cuse(3)`, which the tree lacks, and the
  engine provides natively what the desktop wanted it for (§3 rows C4, C5, C7). Supporting it
  for compatibility is §12 question 3.
- **The FreeBSD drivers for hardware no reference machine has** (the `sys/dev/sound/pci` back
  catalog). Drivers are added for hardware in hand
  ([ADR-0023](adr/0023-hardware-scope-consumer-decade.md)).

---

## 2. One engine, two faces

**Decision proposed: `sound(4)` stays the system's sound interface, and the native interface is
an extension of the same engine, not a layer over or under it.**

| Alternative | Why not |
|---|---|
| `sound(4)` only | Meets G1. Fails G2: the desktop's study found that another program's volume cannot be set, that reported delay leaves out the mixing stage, and that small periods behave badly. Every desktop on FreeBSD works around these with a server |
| A user-space server over raw devices, with an Open Sound System emulation on top (the macOS and Linux shape) | Reaches G2. Breaks G1's fidelity (emulated `/dev/dsp` needs `cuse` and never quite matches), adds a process whose death silences the machine, and puts mixing behind scheduling of an ordinary process |
| Two engines, old and new, side by side | Two authorities over one device: which one owns the hardware, the volume, the default? |
| **One engine, two faces** | A `/dev/dsp` channel and a native stream are the same object. FreeBSD already mixes in the kernel (virtual channels), so this is FreeBSD's own architecture, extended |

**Additions policy.** Everything new is additive and reachable only through new names: one new
device node (`/dev/sound`, §6), new `hw.snd.*` and `dev.pcm.*` entries, and new `devd` events.
No existing request number, structure, or default changes. Each addition is recorded in
[compat-deviations.md](compat-deviations.md). A program that never opens `/dev/sound` cannot
tell this system from FreeBSD, with the one exception argued in §5.3 (reported delay). Rerouting
of `sound(4)` streams is off unless `hw.snd.follow_default` is set.

---

## 3. What "the power of macOS" means, row by row

| # | Capability on macOS | `sound(4)` today | Mechanism here | Test |
|---|---|---|---|---|
| C1 | Every I/O cycle carries a timestamp relating sample time to host time | Position and delay requests, per call; no shared clock | A **device clock** per device: (sample position, system time, rate ratio) pairs taken at the hardware interrupt and published in a mapped page, read without a system call, like the timehands in `shared_page` (§4.3) | Host tests of the clock filter; in QEMU, recorded position against the capture file |
| C2 | Shared-memory I/O, pulled by the device's cycle, at buffer sizes of a few milliseconds or less | `read`/`write` copies; `mmap` of a channel's buffer, with restrictions | **Native streams**: a ring mapped into the client, filled or drained by the engine in the cycle; the client is woken once per cycle (§6) | Loopback latency and missed-cycle counts under load (§10) |
| C3 | Honest latency: device, safety offset, buffer, and stream latency reported separately | The delay request counts the program's channel buffer and not the mixing parent's | Latency is a property of each graph edge and is summed along the path (§4.3, §5.3) | Reported delay against measured loopback delay |
| C4 | Another program (the system's sound panel) sees and sets each stream's volume and mute | Readable through `sndstat`; settable only by the stream's own process | Stream control through `/dev/sound`, with an ownership rule (§6.3) | A test client of ours; then the desktop's Sound pane, once the desktop adopts it (§6.4) |
| C5 | Streams follow the default device; a device can be unplugged under a running stream | A channel is bound to its unit at open; a detached device fails its channels | **Rerouting**: the graph is replaced at a cycle boundary, with the stream's ring kept and a rate converter inserted if the rates differ (§4.4) | Unplug and default-change tests under continuous playback, checked for gaps in the capture |
| C6 | Aggregate devices: several devices as one, with drift correction | None in the kernel | An **aggregate** node: one member is the clock; the others are resampled by the measured ratio of their clocks (§4.5) | Two emulated devices with a deliberate rate offset: no drift over an hour of simulated time on the host |
| C7 | System and per-program capture ("taps"), with user consent | Loopback through `virtual_oss` | **Tap** edges in the graph, readable as capture streams, gated by a privilege (§6.3) | Tap capture equals the mix, sample for sample |
| C8 | Real-time I/O threads with a known deadline | `rtprio(2)` for the superuser | `rtprio` and `mlockall` exist in the tree; unprivileged use needs `mac_priority` (planned for M-D.2). The cycle's deadline is published in the stream's page. A scheduler deadline class is §12 question 5 | Missed-cycle count with the system under load |
| C9 | Exclusive, unconverted access ("hog mode") | Bit-perfect mode and exclusive open | The same, as FreeBSD has them: an exclusive stream bypasses mixing and conversion | Output bytes equal input bytes |
| C10 | Property listeners: devices, defaults, jacks, formats | `devd` events for connection changes; polling otherwise | The same `devd` events, plus `kqueue` on `/dev/sound` for graph changes (§6.2) | Events recorded against FreeBSD's for the shared set |
| C11 | Multichannel layouts, floating-point samples, high rates | Up to eight channels with a channel matrix, 32-bit float, rate conversion at several qualities | The same formats and matrix, in `lib/pcm` (§4.6) | Host tests against outputs recorded from FreeBSD |
| C12 | MIDI with timestamps | Raw MIDI devices | Raw MIDI devices as FreeBSD 15.1 has them, later (§11 A6). Timestamps are §12 question 6 | |
| C13 | An idle device sleeps; silence costs nothing | A channel stops when not triggered | A device with no running stream stops its cycle and enters runtime power management (DRIVER-MODEL.md §8) | No interrupts from an idle device |

---

## 4. The engine (`sys/sound`)

### 4.1 Overview

```
 userland     mixer(8) sndctl(8) cubeb SDL mpv sndio …        the desktop · games runtime · pro audio
──────────────────────┬──────────────────────────────────────────┬──────────────────────────────
 faces                │ sound(4): /dev/dsp* /dev/mixer*          │ native: /dev/sound
                      │ /dev/sndstat  hw.snd.*  dev.pcm.*        │ stream descriptors: mapped ring,
                      │ devd SND events                          │ mapped clock page, kqueue
──────────────────────┴──────────────────────────────────────────┴──────────────────────────────
 sys/sound    Stream (one object behind both faces) · Graph (immutable snapshot per cycle)
              Device · DeviceClock · Mixer node · Converter node · Aggregate · Tap
              control plane (may sleep)            ║  I/O cycle (IoCycleCx: cannot sleep or allocate)
──────────────────────────────────────────────────╨───────────────────────────────────────────────
 lib/pcm      formats · channel matrix · volume · equalizer · rate conversion · clock filter
 lib/hda      High Definition Audio: verbs, widget graph, pin and association parsing
──────────────────────────────────────────────────────────────────────────────────────────────────
 drivers      snd-dummy   hda (controller) + hda codecs   uaudio   sky1-audio (serial audio + DMA)
```

### 4.2 Objects

- **Device**: a set of hardware streams with one clock. What FreeBSD names `pcmN`.
- **Stream**: a ring buffer, a format, a gain, a state (an enum: prepared, running, draining,
  rerouting, orphaned, closed), and an owner (process, credential, jail). A `/dev/dsp` open
  creates one or two (play and record); so does a native open.
- **Graph**: for each device, the nodes between its streams and its hardware: per-stream
  converters, a mixer for playback, a splitter for capture, taps, the device's own volume and
  equalizer. The graph the cycle runs is an **immutable snapshot**. The control plane builds the
  next snapshot, with all memory allocated in advance, and publishes it. The cycle picks it up at
  its next boundary, and the old one is reclaimed through `sys/epoch`.

### 4.3 The I/O cycle and the clock

- A device's cycle runs in its interrupt thread at real-time priority, once per hardware period:
  read the hardware position, update the clock, pull each running stream through its converters,
  mix, and write to the hardware buffer (the reverse for capture).
- **`IoCycleCx`** is the context every function on that path takes. Nothing that sleeps,
  allocates, or takes a sleepable lock accepts it, so a blocking call in the cycle does not
  compile (G3). Client rings are single-producer, single-consumer and lock-free. A client that
  has not filled its ring contributes silence and an underrun count. It never stalls the cycle.
- **The device clock** filters (position, time) pairs into a rate estimate and publishes them in
  a read-only page under a sequence count. From it any party computes the time at which a given
  sample will be heard, or was captured (C1).
- **Latency** is carried per edge: hardware, converter delay, mix period, ring fill. A stream's
  reported latency is the sum along its path (C3).
- **Period size** is per device and follows the smallest period any running stream asks for,
  within the device's limits and `hw.snd.latency`'s profiles for `sound(4)` clients. When the
  low-latency stream leaves, the period grows again and idle cost with it.

### 4.4 Rerouting

Moving a stream between devices is a graph change like any other: the stream keeps its ring, a
rate converter is inserted or retuned, and the first cycle on the new device continues from the
sample where the old one stopped. Three things trigger it: a control request (C4's panel), a
device's departure (the stream becomes **orphaned**, and is moved to the default device or held
silent until one exists), and a change of default for streams opened on the default.

Native streams are always rerouted. A `sound(4)` stream is rerouted only when
`hw.snd.follow_default` is set (§5.3). Otherwise it behaves as on FreeBSD: it stays on its unit,
and when the device detaches its requests fail as FreeBSD's do, which FreeBSD's hot-unload test
expects.

### 4.5 Aggregates

An aggregate is a virtual device whose channels are the members' channels side by side. One
member is its clock. For every other member the engine compares the two device clocks and drives
an asynchronous rate converter by their ratio, so the members stay sample-aligned indefinitely
(C6). The same converter serves USB devices that run on their own clock, and rerouting between
devices at different rates.

### 4.6 Signal processing (`lib/pcm`)

Pure functions over sample slices, with no `unsafe`, tested on the host: format conversion
(integer widths, endianness, 32-bit float, the legacy companded formats), the channel matrix up
to eight channels with FreeBSD's layouts, volume, the equalizer, mixing with saturation, and
rate conversion at FreeBSD's quality levels. Outputs are compared with ones recorded from FreeBSD
where exact equality is meaningful (formats, matrix, volume) and by measured distortion where it
is not (rate conversion). Vector implementations are an optimization behind the same tests.

---

## 5. The `sound(4)` face

### 5.1 What is reproduced

- **Device nodes**: `/dev/dsp`, `/dev/dspN`, `/dev/mixerN`, `/dev/sndstat`, with FreeBSD's
  cloning and default-unit behavior.
- **Requests**: the `SNDCTL_*`, `SOUND_MIXER_*`, and `SNDSTIOC_*` families as FreeBSD 15.1
  implements them, including `mmap` of a channel buffer, triggers, synchronization groups, and
  the `sndstat` nvlist interface.
- **Sysctls**: `hw.snd.*` (default unit, virtual channel counts, latency profiles, rate
  quality, verbosity), `dev.pcm.N.*` (play and record virtual channels, bit-perfect, the
  per-device mixer and equalizer controls), and the codec drivers' own trees (`dev.hdaa.N.*`).
- **Events**: the `SND` connection events to `devd`.
- **Text**: `/dev/sndstat`'s lines and `dmesg`'s attach lines, which tools parse. The desktop
  parses the sentence a machine with no device prints.

A virtual channel is a stream on the device's mixer. `dev.pcm.N.play.vchans` limits how many
exist. Bit-perfect mode is an exclusive stream (C9). What `sndstat` reports per channel (process,
command, volume, format) comes from the stream's own fields.

### 5.2 Ground truth

`../freebsd-src` is 16-CURRENT, and `sound(4)` has changed quickly: `snd_dummy`, `sndctl(8)`,
and the `sndstat` nvlist requests are recent, and the sequencer is gone. Details this document
takes from the 16-CURRENT tree (32-bit float samples, the `SND` connection events, the test
list) are confirmed against 15.1 at A0. The reference is
**FreeBSD 15.1 as recorded** on the build server: node lists, `sndstat` text and nvlists,
`mixer(8)` and `sndctl(8)` output, the sysctl trees, and request results, for `snd_dummy` and
for `snd_hda` under bhyve, kept in `tests/corpus/freebsd/sound/`. Recording it is the first work
of the first stage (§11 A0).

### 5.3 Where exact behavior is in question

1. **Reported delay.** FreeBSD's delay request leaves out the mixing parent's buffer. With G4
   the engine knows the true figure. Proposal: report the true delay, and list it as a
   deviation. Clients use this number to synchronize sound with picture, and the request's
   definition is the true delay. To settle in the ADR (§12 question 1).
2. **Following the default device, and surviving a detach.** On FreeBSD a channel stays on the
   unit it opened and fails when that device goes. Proposal: `hw.snd.follow_default` (new), off
   by default so behavior matches FreeBSD exactly, and turned on by the desktop's profile, where
   it is then a listed deviation (§12 question 2).

---

## 6. The native face (`/dev/sound`)

### 6.1 Streams

`open("/dev/sound")` gives a control descriptor. A request on it creates a **stream descriptor**
for a device (or "the default"), a direction, a format, and a requested period. The descriptor
supports:

- `mmap` of the **ring** (read-write) and of the **stream page** (read-only: the device clock,
  the ring's engine-side position, the next cycle's deadline, underrun counts);
- a wake once per cycle, by `kqueue` or by a `_umtx_op` wait on a word in the stream page, so a
  client's real-time thread sleeps and wakes with no other system call;
- start, stop, drain, and gain requests.

A client's steady state is: wake, read the clock, write samples into the ring, advance its
position, sleep. No system call moves audio.

### 6.2 The graph, visible

Requests on the control descriptor enumerate devices, streams, and their latencies and formats;
`kqueue` on it reports changes (C10). This is `sndstat`'s content with identities that stay
stable while things move.

### 6.3 Control and privilege

- **Own streams**: always controllable.
- **Others' streams** (gain, mute, move): the same user, or a holder of the privilege the
  desktop's settings helper runs with; never across jails, except from a parent jail.
- **Devices** (aggregates, default, format): the privilege that owns `hw.snd.default_unit` on
  FreeBSD.
- **Taps and capture**: a tap on another program's stream, or on a device's mix, needs an
  explicit privilege. Whether capture from a microphone needs consent beyond permission on the
  device node is a desktop policy question, and the kernel's part is §12 question 4.
- Request structures follow the rules [GRAPHICS.md](GRAPHICS.md) §2 adopted: sized, with
  shorter callers zero-extended and longer ones required to have zero tails, so the interface
  can grow.

### 6.4 Who uses it

- **The desktop**: its Sound pane and menu item (C4, C5, C10) and its media framework. Adopting
  `/dev/sound` is a change in the desktop's tree, so it is an ask of the desktop's plan in the
  sense of [DESKTOP.md](DESKTOP.md) §8, not work here.
- **The Windows games runtime**: the audio endpoint of [WINDOWS-GAMES.md](WINDOWS-GAMES.md)
  §7.5 is a native stream. Its event-driven mode is the per-cycle wake, and its position and
  latency queries read the stream page. This answers the question that document left here.
- **Low-latency programs**: a JACK or PipeWire back end over native streams is possible and is
  the ports' business. Their Open Sound System back ends work from the first stage.

A small client library in Rust, with a C interface, wraps the requests. Where it lives is the
userland question M-U already has.

---

## 7. Drivers

### 7.1 The class

A sound device is usually an **aggregate of drivers**, as a display is in GRAPHICS.md §7.2: a
controller that moves samples (a High Definition Audio controller, or a serial audio port with a
direct memory access (DMA) channel), one or more codecs that convert and route them, and sometimes a clock provider or
an amplifier. The class traits are:

- `PcmHw`: hardware streams with their formats and period limits, start and stop, and the
  position read the clock needs. Its cycle-path methods take `IoCycleCx`.
- `MixerHw`: the controls a codec exposes, mapped onto FreeBSD's mixer device names.
- `JackHw`: presence detection, feeding the connection events and rerouting.

Drivers are `forbid(unsafe)` and use the DMA layer's buffers for hardware rings.

### 7.2 Order

| Order | Driver | Hardware | Baseline | Why here |
|---|---|---|---|---|
| 1 | `snd-dummy` | None | FreeBSD 15.1 `snd_dummy` | The whole `sound(4)` face with no hardware; what M-D.1's lanes need |
| 2 | `hda` controller + generic codec (`lib/hda`) | QEMU `ich9-intel-hda` with `hda-duplex`, on both instruction sets; bhyve's `hda` | FreeBSD 15.1 `snd_hda` | The QEMU-first vehicle: QEMU writes what was played to a file, and bhyve gives the performance baseline. FreeBSD 15.1 has no virtio sound driver, so there is nothing to compare one with |
| 3 | `hda` on metal: Realtek codecs, analog | X870E (`1022:15e3`), Z690-A (ALC897) | `snd_hda` on the same machine | The desktop's metal target |
| 4 | Digital audio over DisplayPort and HDMI | The RDNA2 outputs: X870E (`1002:1640`), Z690-A (`1002:ab28`) | `snd_hda` with `drm-kmod` | Needs the display driver to hand over the monitor's audio capabilities; follows M-G.6. The X870E's NVIDIA card (`10de:22e9`) waits for a display driver, as GRAPHICS.md §13 question 1 defers it |
| 5 | `uaudio` (USB Audio Class 1 and 2) | Any | FreeBSD `snd_uaudio` | Needs isochronous transfers, which [USB.md](USB.md) left out. The first consumer of §4.5's converter |
| 6 | `sky1-audio`: the High Definition Audio controller and ALC269VC, then I²S serial audio with the DMA-350 into the DisplayPort transmitters | Orange Pi 6 Plus | None: FreeBSD has no driver | From `specs/sky1-audio`. The specification marks the signal processor optional, so the first driver leaves it in reset |

High Definition Audio codecs are parsed the way FreeBSD's `hdaa` does it: walk the widget graph,
group pins into associations, and derive devices and mixer controls from them, with per-board
corrections as data. The device names, `dev.hdaa` trees, and `sndstat` lines must match
`snd_hda` on the same hardware.

---

## 8. Crates

| Crate | Contents | Tested | `unsafe` |
|---|---|---|---|
| `lib/pcm` | Formats, channel matrix, volume, equalizer, mixing, rate conversion, the clock filter | host, fuzzed | **forbidden** |
| `lib/hda` | Verb encoding, widget graph, pin and association parsing, board corrections | host, against codec dumps | **forbidden** |
| `lib/abi` (extended) | `soundcard.h` and `sndstat` layouts and request numbers; the native interface's structures | host, `abi-check` | forbidden |
| `sys/sound` | Streams, graph snapshots, the cycle, device clocks, aggregates, taps, the class traits | host (engine under a simulated device), kernel | small (rings, mapped pages) |
| `kernel/src/sound/` | Device nodes for both faces, `sndstat`, sysctls, `devd` events, privilege checks | kernel (`#[ktest]`), QEMU | minimal (user access) |
| `drivers/snd-dummy`, `drivers/hda`, `drivers/uaudio`, `drivers/sky1-audio` | Hardware | QEMU, hardware | **forbidden** |

---

## 9. Testing

| Layer | Method |
|---|---|
| **Layouts** | `cargo xtask abi-check` for `soundcard.h` and `sndstat` against FreeBSD 15.1's headers |
| **Signal processing** | Host tests in `lib/pcm` against recorded FreeBSD outputs and distortion bounds (§4.6) |
| **Engine** | `sys/sound` runs on the host under a simulated device with a scripted clock: underruns, rerouting with no lost or repeated sample, aggregate drift, snapshot replacement under load |
| **`sound(4)` face** | Differential against the recorded corpus (§5.2). FreeBSD's own tests through kyua, compared with 15.1's results as every suite is (16-CURRENT has `tests/sys/sound` with read and write, `mmap`, polling, `sndstat`, and hot unload; which of them 15.1 has is recorded at A0). Unmodified `mixer`, `sndctl`, and `beep` |
| **End to end in QEMU** | Play known signals through the emulated controller with QEMU's file back end; check the file for level, frequency, channel order, and gaps. Capture content needs a back end that supplies a known signal (A2) |
| **Ports** | `sox`, `mpv`, and Firefox's cubeb back end play in the desktop's lanes |
| **Hot plug** | Detach and attach a device under playback in QEMU through the driver model (C5), and unplug a USB device once `uaudio` exists: no panic, no leaked buffers; the stream continues or fails according to `hw.snd.follow_default` |
| **Robustness** | Request fuzzing on both faces; hostile codec responses in `lib/hda` |

Tests that depend on wall-clock timing are not compared with FreeBSD's results, by the standing
rule. Timing is measured in §10.

---

## 10. Performance

Components ([ADR-0014](adr/0014-performance-parity-per-component.md)), each against FreeBSD
15.1's `sound(4)` on the same machine (in bhyve with its emulated controller, then on the X870E
and the Z690-A):

- `snd.write`: processor time per second of audio played through `/dev/dsp`, with and without
  format and rate conversion;
- `snd.mix`: the same with 1, 8, and 64 concurrent streams on one device;
- `snd.record`: capture, likewise;
- `snd.ctl`: mixer and `sndstat` request rates;
- `snd.open`: open, configure, first sample, close.

Diagnostics, not gates: round-trip loopback latency on hardware, the smallest period that runs
an hour with no missed cycle while the system is loaded, and native-stream cost against
`sound(4)` for the same work. FreeBSD has no counterpart to native streams, so they are
measured against the `sound(4)` path here, and against published macOS figures only as context.
Where FreeBSD has no driver (Sky1), results are recorded under whatever rule GRAPHICS.md §13
question 7 arrives at.

---

## 11. Prerequisites and phasing

| Needs | From | For |
|---|---|---|
| `devd` events, device nodes, sysctl trees, `kqueue` | Done (P3 to P6) | A1 |
| `mmap` of a device node's buffer by an unprivileged client | To confirm in the tree (`sys/drm` maps buffers; the general device path is unverified) | A1 (`sound(4)` `mmap`), A3 |
| `mac_priority`, for unprivileged real-time threads | M-D.2 | A3 on the desktop |
| Isochronous transfers in the xHCI driver | USB track, unscheduled | A5 |
| Runtime power management | DRIVER-MODEL.md §8 | A4 (idle devices) |
| The display's audio capabilities from the graphics driver | M-G.6 | A4 (digital outputs) |

| Stage | Work | Exit criterion |
|---|---|---|
| **A0** | FreeBSD 15.1's behavior recorded (§5.2); the ADR with §12 questions 1 and 2 | The corpus exists; the ADR is accepted |
| **A1** | `lib/pcm`, `sys/sound` (streams, graph, cycle, mixer, the device clock driven by `snd-dummy`'s timer), the `sound(4)` face, `snd-dummy` | M-D.1's sound lanes pass as on FreeBSD 15.1; `mixer`, `sndctl`, and `cat /dev/sndstat` match the corpus; FreeBSD's sound tests match 15.1 under kyua |
| **A2** | `lib/hda`, the `hda` driver, clocks from hardware position | In QEMU on both instruction sets, a played signal arrives intact in the file QEMU writes; capture is opened, configured, and read (a known input signal needs a QEMU back end that supplies one, to find at A0; if none, capture content is first checked in bhyve or at A4); `snd.*` within the gate in bhyve |
| **A3** | The native face: streams, the stream page, control, rerouting, taps | A native test client plays in QEMU with one wait and no other system call per cycle; a second test client sets the first one's gain and moves it; detaching the device under playback (a driver-model detach, since the tree has no PCI hot unplug) moves the stream to `snd-dummy`; a tap equals the mix |
| **A4** | Metal: Realtek codecs, jack detection, idle power; digital outputs once M-G.6 has the RDNA2 driver | Analog sound on the X870E and the Z690-A, with `sndstat` and the mixer matching FreeBSD on the same machine: M-D.4's built-in audio. Sound over the RDNA2 DisplayPort output when M-G.6 is there |
| **A5** | `uaudio`, the asynchronous converter, aggregates | A USB headset works and survives unplugging; an aggregate of two devices holds alignment |
| **A6** | `sky1-audio`; raw MIDI | Sound from the Orange Pi 6 Plus's jack and over DisplayPort; a USB MIDI keyboard is readable as on FreeBSD |

A1 is all that M-D.1 waits on. A3 is what [WINDOWS-GAMES.md](WINDOWS-GAMES.md) stage W4 uses,
though the games runtime can start on the `sound(4)` face after A2.

---

## 12. Open questions (to settle in the ADR)

1. **Reported delay** (§5.3): the true figure, as a listed deviation, or FreeBSD's.
2. **Following the default device** for `sound(4)` clients (§5.3): the sysctl's default, and
   whether the desktop's profile turns it on.
3. **`virtual_oss` and `cuse`.** FreeBSD 15.1 ships both. Unmodified `virtual_oss` would need
   `cuse` in the kernel. Is it wanted for compatibility once the engine does natively what it
   was used for?
4. **Capture consent.** What the kernel offers the desktop's permission model: a second device
   node for capture, a privilege, or a hook in the MAC framework when that arrives.
5. **A deadline class in the scheduler.** `rtprio` is enough to start. Whether client threads
   should declare their cycle deadline to `sys/sched`, as macOS's time-constraint threads do,
   depends on what the missed-cycle diagnostics show.
6. **MIDI timestamps**, and whether MIDI gets a native face at all.
7. **Signal processors.** The Sky1's HiFi5 and the Q8B's audio processor can mix and process in
   hardware. The graph leaves room for a device whose mixer is offloaded. Whether to use them is
   a question for when a board needs it (the Q8B's audio appears to run through its processor;
   to confirm from `specs/qcom-sc8280xp`).
8. **Per-jail devices.** Whether a jail sees the host's devices, a subset, or an aggregate made
   for it, and how that fits the desktop's application jails.
