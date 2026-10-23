# USB, input, and virtual terminals

The design for the USB and input track ([ROADMAP.md](ROADMAP.md#track-usb-and-input-parallel-starts-after-p3-m--in-progress)):
the xHCI driver, the USB bus and hubs, HID and the input class, `vt` virtual terminals, and USB
mass storage. [ADR-0015](adr/0015-usb-input-vt.md) records the decision.

Behavior reference: sys/dev/usb (controller/xhci.c, usb_hub.c, usb_device.c, usb_transfer.c,
input/usbhid.c, storage/umass.c), sys/dev/hid (hidbus.c, hkbd.c, hms.c), sys/dev/kbd,
sys/dev/kbdmux, sys/dev/evdev, sys/dev/vt and sys/teken. FreeBSD is the specification of what
userland sees, not the source of the code ([ADR-0007](adr/0007-license-and-clean-implementation.md)).
Where FreeBSD has no driver (UAS, virtio-input), the specifications are the reference: xHCI 1.2,
USB 2.0 and 3.2, HID 1.11 and its usage tables, T10 SAM/SPC/SBC and UAS-2, and virtio 1.2 §5.8.

## 1. Goals and non-goals

### 1.1 Goals

- **FreeBSD's userland view.** The names (`xhci0`, `usbus0`, `uhub0`, `usbhid0`, `hidbus0`,
  `hkbd0`, `hms0`, `umass0`, `da0`, `ttyv0`–`ttyv7`), `devinfo`/`pnpinfo` strings, `devctl` events,
  `/dev` nodes, `usbconfig` output, the kbd, `vt`, and `consio` ioctls, and FreeBSD's evdev ABI.
  `kbdcontrol`, `vidcontrol`, `moused`, `usbconfig`, `getty`, and `login` run unmodified. The
  ground truth is FreeBSD 15.1 on the same QEMU machines (`tests/corpus/freebsd/usb/`).
- **Hardware the reference machines have.** xHCI on PCI (the X870E's AMD and ASMedia hosts) and on
  ACPI (`PNP0D10`, the Sky1's ten hosts), USB 2 and 3 hubs, HID keyboards and mice, and USB disks.
- **Hot plug is ordinary.** A device can leave at any moment, mid-transfer, with its file system
  mounted. That fails I/O with `ENXIO`; it never panics or leaks DMA.
- **Testable on the host.** TRB and context encoding, descriptor parsing, HID report descriptors,
  SCSI CDBs and sense data, and the terminal emulator live in `lib/` crates, tested and fuzzed on
  the host.
- **Performance parity** for the component measured before hardware: `vt` text output (§10).

### 1.2 Non-goals for the track

- Controllers other than xHCI (EHCI, OHCI, UHCI, DWC3 device mode). Every reference machine and
  emulator here has xHCI. USB4 tunneling, isochronous transfers (audio, video), and the device side.
- HID devices beyond keyboards, mice, and tablets (touchpads with gestures, game controllers,
  consumer and system controls arrive as later drivers on the same `hidbus`).
- USB serial, network, and Wi-Fi drivers (with P7 and later). `usbdump`/`usb_pf`.
- Graphics beyond the GOP framebuffer (M-G). `vt` draws into the boot framebuffer only.
- Mouse cut and paste in `vt`, and splash screens.

## 2. Principles

1. **The hardware owns the rings; the types say whose turn it is.** A TRB slot is either the
   producer's or the consumer's, decided by the cycle bit. The ring types make writing a slot the
   controller owns impossible, and the event ring's dequeue pointer only moves past what was read.
2. **A request is completed exactly once** (DRIVER-MODEL.md §9.1). A USB `Transfer` is an owned
   value handed to an endpoint and handed back in a `Completion`. Cancelling, timing out, stalling,
   and unplugging are all ways to complete it, never to lose it.
3. **Matching is on interfaces, as FreeBSD does.** A device's interfaces are the children drivers
   match. A driver that wants the whole device (a hub) matches on its device class.
4. **Transports below, classes above.** HID is a bus (`hidbus`) whose transport is USB today and can
   be I2C later. Input devices publish to the input class, disks to the storage class, terminals to
   the tty layer. A class never knows the transport.
5. **One thread explores a bus.** Connects, disconnects, resets, and enumeration on one USB bus
   are serialized by its explore thread, as FreeBSD's `usb_explore`. Transfers do not go through
   it.

## 3. Crates

| Crate | Kind | Contents |
|---|---|---|
| `lib/xhci` | pure, host-tested, fuzzed | Register layouts, TRB types and encoding, rings (command, transfer, event; link TRBs, cycle state), input/device/endpoint/slot contexts in 32- and 64-byte forms, the event decoder |
| `lib/usb` | pure, host-tested, fuzzed | Standard descriptors (device, configuration, interface, endpoint, SuperSpeed companion, BOS, string), standard requests, speeds and max-packet rules, route strings |
| `lib/hid` | pure, host-tested, fuzzed | Report-descriptor parser into typed collections, report fields and usages; report extraction; the usage tables used (generic desktop, keyboard, buttons, LEDs) |
| `lib/scsi` | pure, host-tested | CDBs, `INQUIRY`/`READ CAPACITY` data, fixed and descriptor sense data, BBB CBW/CSW, UAS IUs |
| `lib/vtterm` | pure, host-tested, fuzzed | The terminal emulator: an ECMA-48 parser with `teken`'s xterm behavior, a screen of cells, scrollback, and a damage set |
| `sys/usb` | kernel | The USB bus: devices, endpoints and pipes, transfers, the explore thread, enumeration, hubs (root and external), `ugen` and `/dev/usb` |
| `sys/hid` | kernel | `hidbus`: children by top-level collection, report routing, `hidraw` |
| `sys/input` | kernel | The input class: kbd and `kbdmux`, keymaps, evdev, `sysmouse` |
| `sys/vt` | kernel | Virtual terminals: windows over `lib/vtterm`, the framebuffer renderer, switching, the console sink |
| `drivers/xhci` | driver | The controller, on PCI and ACPI |
| `drivers/usbhid`, `drivers/hkbd`, `drivers/hms` | drivers | The USB transport for `hidbus`; the keyboard and mouse drivers on `hidbus` |
| `drivers/umass` | driver | USB mass storage, BBB and UAS, publishing `da*` |
| `drivers/virtio-input` | driver | virtio keyboards and tablets, publishing to the input class directly |

`drivers/fb-console` stays the early console. `sys/vt` takes the framebuffer over from it when it
starts, and the early console's contents become `ttyv0`'s first screen.

## 4. xHCI

### 4.1 Attach

- **PCI** (class `0c0330`) and **ACPI** (`PNP0D10`, `_CRS` memory and interrupt) attach the same
  core; the two front ends differ only in how they get registers and interrupts. QEMU's `virt`
  exposes xHCI on PCI only, so the ACPI front end is host-tested with fake resources and first runs
  on the Sky1 at P6.
- **Handoff.** The USB legacy support extended capability is claimed from firmware (OS-owned
  semaphore, wait for the BIOS-owned bit to clear, SMI enables off) before reset.
- **Reset and setup.** Halt, reset, wait for `CNR`; program `MaxSlotsEn`, the DCBAA, scratchpad
  buffers (count from `HCSPARAMS2`), the command ring, and one event ring per interrupter. The
  context size (32 or 64 bytes, `HCCPARAMS1.CSZ`) is a type parameter of the context views.
- **Interrupts.** MSI-X with one interrupter per vector up to the CPU count (a few transfers per
  interrupter are steered by endpoint), MSI with one, then INTx. The filter acknowledges `IMAN.IP`
  and wakes the interrupter's thread; the thread drains the event ring and moves `ERDP` once per
  batch.
- **DMA.** One `DmaTag` from the controller's constraints (`AC64`; 4 KiB boundary rules for rings
  and contexts; 64 KiB TRB boundary rule for data). Rings and contexts are `DmaCoherent`; data
  buffers are mapped per transfer and split into TRBs at 64 KiB boundaries.

### 4.2 Rings

`lib/xhci` defines `ProducerRing` (command and transfer rings) and `EventRing`. A producer ring
hands out a `Reservation` of n TRBs, which the caller fills; committing writes the first TRB's
cycle bit last, after a release fence, so the controller never sees a half-written chain. Link
TRBs and the toggle-cycle bit are the ring's business. Each enqueued TRB's physical address is
returned so completions can be matched to it. The event ring reads TRBs while their cycle bit
matches the consumer state and yields typed `Event`s (`CommandCompletion`, `Transfer`,
`PortStatusChange`, `HostController`, `MfindexWrap`, …).

### 4.3 Commands

The command ring is shared; a command is a future-like `CommandTicket` completed by the matching
`CommandCompletion` event (by TRB address). Callers in thread context sleep on it with a timeout
(5 s, as FreeBSD's command timeout); on timeout the driver aborts the ring (`CRCR.CA`) and the
command completes with `Aborted`. Enable/disable slot, address device (with and without `BSR`),
configure and evaluate endpoint, reset and stop endpoint, set TR dequeue pointer, and reset device
are the commands used.

### 4.4 Endpoints and transfers

Each endpoint context has a transfer ring and a table from TRB address to the in-flight
`Transfer`. Submitting a transfer builds its TRB chain (setup, data, status for control; normal TRBs
for bulk and interrupt; event data TRB at the end, so a short packet still reports the whole
transfer), commits it, and rings the doorbell. A `Transfer` event completes the transfer whose
last TRB it names, with the actual length from the residue.

Errors: a **stall** completes the transfer with `Stalled` and leaves the endpoint halted until the
owning driver clears it (`CLEAR_FEATURE(ENDPOINT_HALT)` then reset endpoint, which `sys/usb` does
as one call); **babble** and **transaction errors** complete with their status and the endpoint is
reset. **Timeouts and cancellation** stop the endpoint, complete every transfer the stop left
pending with `Cancelled` or `TimedOut`, and move the dequeue pointer past them. Bulk streams (for
UAS) give each stream its own ring under one endpoint.

### 4.5 Root hub

The controller's ports are presented to `sys/usb` as a hub, through the same `Ports` trait an
external hub implements (§5.3): port status and change bits, reset (warm reset on USB 3), power,
and the port's speed. `PortStatusChange` events kick the bus's explore thread. USB 2 and USB 3
ports come from the supported-protocol capabilities, so a USB 3 device's port pairs are known.

## 5. The USB bus

### 5.1 Devices and identity

`sys/usb` keeps, per bus, a table of `UsbDevice`s: speed, route string, parent hub and port, slot,
address, the device descriptor and the active configuration, and its endpoints. A device's
interfaces become children of the framework node of the device's parent (the hub), each with
`Ident::Usb(UsbIdent)`:

```rust
pub struct UsbIdent {
    pub vendor: u16, pub product: u16, pub release: u16,
    pub class: u8, pub subclass: u8, pub protocol: u8,          // device
    pub if_class: u8, pub if_subclass: u8, pub if_protocol: u8, // interface
    pub interface: u8, pub config: u8,
}
```

Drivers match on vendor and product, on interface class/subclass/protocol, or on device class,
with the ranks FreeBSD's `usb_lookup` gives each (vendor-specific wins over class). The child's bus
data is a `UsbInterface` handle: its descriptors, its endpoints as typed pipes, and the device's
default control pipe.

### 5.2 Enumeration

On a connect, the explore thread resets the port, reads the speed, enables a slot, addresses the
device (with `BSR` first for full-speed devices, to read the first 8 bytes and set ep0's max
packet), reads the device descriptor, the configuration descriptors (sized by `wTotalLength`), the
string descriptors FreeBSD reads (language, manufacturer, product, serial), and the BOS on USB 3.
It picks configuration 1 unless a quirk says otherwise, configures the endpoints, sets the
configuration, and adds each interface as a child. Attaching drivers to them is the framework's
job; enumeration does not wait for it.

A disconnect marks the device `Gone`: its pipes fail new transfers with `Gone`, every in-flight
transfer completes with `Gone`, the framework detaches the children (DRIVER-MODEL.md §10.3), and the
slot is disabled once the last pipe is dropped.

### 5.3 Hubs

`uhub` is a driver on device class 9 that implements `Ports` over hub-class requests and a status
change interrupt endpoint. USB 2 hubs with transaction translators set the slot's TT fields (and
`MTT` for multi-TT hubs); USB 3 hubs get `SET_HUB_DEPTH` and the route string. The root hub is
`uhub0` on `usbus0`, as on FreeBSD; a hub's children are its ports' devices' interfaces.

### 5.4 Pipes and transfers

```rust
pub struct Transfer { /* buffer (DmaBuf or a mapped Uio), length, flags (short ok, zero packet), timeout */ }
pub struct Completion { pub transfer: Transfer, pub status: UsbStatus, pub actual: usize }

impl ControlPipe   { pub fn request(&self, s: &Sleepable, setup: Setup, data: DataStage<'_>) -> Result<usize, UsbError>; }
impl BulkPipe      { pub fn submit(&self, t: Transfer, done: Done) -> Result<(), Rejected>; }
impl InterruptPipe { pub fn submit(&self, t: Transfer, done: Done) -> Result<(), Rejected>; }
```

`Done` is a completion callback run in the interrupter's thread, or a `Sleepable` wait for
synchronous use. Interrupt pipes keep a fixed number of transfers queued; a HID driver re-submits
in its completion. `Rejected` hands the transfer back, so even a failed submission cannot lose it.

### 5.5 `ugen` and `/dev/usb`

`usbconfig` and `libusb` use `/dev/usb/B.D.E` nodes and `/dev/ugenB.D` symlinks with the `USB_*`
ioctls of `usb_ioctl.h`. The track implements the read-only set `usbconfig` uses (`USB_GET_DEVICEINFO`,
`USB_GET_RX/TX_*` descriptor reads, `USB_DEVICEENUMERATE` refused, `USB_GET_IFACE_DRIVER`, the
`usbconfig dump_*` requests), with the layouts in `lib/abi` checked by `abi-check`. Generic
transfers from userland (`libusb` I/O) follow later.

## 6. HID and the input class

### 6.1 `hidbus`

`usbhid` attaches to interface class 3, reads the report descriptor, and becomes a `hidbus`: one
child per top-level application collection (keyboard, mouse, consumer control, …), with the
collection's usage as its identity, as FreeBSD's `hidbus` does. The transport interface
(`HidTransport`: get and set report, set idle, set protocol, the interrupt-in stream, and output
reports) is what an I2C-HID transport would implement later.

`lib/hid` parses the report descriptor into typed fields (usage page and range, logical and
physical limits, report ID, size, count, flags) and extracts values from reports. It never panics
on a bad descriptor; it returns an error and `usbhid` falls back to the boot protocol where the
interface supports it.

### 6.2 Keyboards and mice

- `hkbd` turns keyboard reports into key down/up events (with rollover detection), LEDs from the
  kbd layer back into an output report, and FreeBSD's autorepeat (the kbd layer's, not the
  device's).
- `hms` turns mouse and tablet reports into relative or absolute pointer events and buttons, wheel
  and horizontal wheel included.
- `virtio-input` reads the device's name, ID, and event bits from its configuration space and its
  events, which are already evdev codes, into the same class interfaces.

### 6.3 The input class

`KbdSource` and `PointerSource` are the driver traits. Above them `sys/input` provides what
FreeBSD's userland expects:

- **kbd.** Each keyboard is a `kbd` (`kbd1`, …) under `kbdmux` (`kbd0`), which merges them, as
  FreeBSD configures by default. HID usages are translated to AT set-1 scancodes for `K_RAW`,
  keycodes for `K_CODE`, and characters through the active keymap for `K_XLATE` (the default
  US keymap is built in; `kbdcontrol -l` loads others). The `KDGKBMODE`/`KDSKBMODE`, `KDGETLED`/
  `KDSETLED`, `KDSETREPEAT`, `GIO_KEYMAP`/`PIO_KEYMAP`, and `KDGKBINFO` ioctls.
- **evdev.** `/dev/input/eventN` per device with FreeBSD's evdev ABI (`EVIOCG*`, `struct
  input_event`, `kern.evdev.rcpt_mask`), which FreeBSD 15's `moused` reads.
- **sysmouse.** `/dev/sysmouse` for programs that use it; `vt` gets pointer events for nothing yet
  (no cut and paste in this track).

## 7. Virtual terminals

### 7.1 Model

`sys/vt` owns the framebuffer once it starts. It has eight windows (`ttyv0`–`ttyv7`), each a `Tty`
(the line discipline from `sys/tty`, generalized from the UART-only `kernel/src/tty.rs`) whose
output feeds a `lib/vtterm` emulator. One window is visible. Keyboard input from `kbdmux` goes to the
visible window's tty after the kbd layer; Alt-F1…F8 (and `VT_ACTIVATE`) switch windows; Scroll
Lock enters scrollback.

### 7.2 The emulator

`lib/vtterm` implements `teken`'s behavior in its xterm mode, since `/etc/ttys` gives `ttyv*` the
type `xterm`: ECMA-48 control sequences, the DEC private modes FreeBSD supports, 8 and 256 colors
with bold and reverse, UTF-8 input, line drawing, tabs, scroll regions, insert and delete, and
`teken`'s answers to device-status and attribute queries. Its tests compare screens with what
FreeBSD's `vt` shows for the same byte streams (captured with `vidcontrol`'s screen dumps).

### 7.3 Rendering

The screen is cells (character, foreground, background, attributes). Writes mark damage per row;
the renderer draws damaged cells only, from a glyph cache of pre-rendered cells at the
framebuffer's pixel format, and scrolls by copying rows of the framebuffer where that is cheaper
than redrawing. Rendering runs on the writer's thread up to a budget, then defers to a timer, so a
burst of output costs one redraw. The font is 8×16 (FreeBSD's default `vt` font face metrics) unless
`vidcontrol -f` loads another.

### 7.4 The console

The kernel console is the serial port and `ttyv0`, both receiving kernel messages, as on FreeBSD
with `boot_multicons`. `/dev/console` writes go to both; input is taken from both. Panics write to
the framebuffer directly, bypassing locks, as the early console does now.

## 8. USB mass storage

`umass` attaches to interface class 8 with subclass 6 (SCSI) and protocol `0x50` (BBB) or `0x62`
(UAS). It publishes one `da*` disk per logical unit to the storage class (`DiskQueues`), without
CAM: there is no `pass*` device or `camcontrol` in this track.

- **BBB.** One command at a time: CBW, data, CSW, with residue and phase-error handling, and the
  bulk-only reset recovery (class reset, clear both halts) on errors. `REQUEST SENSE` after a
  failed command, and retries for `UNIT ATTENTION` and `NOT READY` as FreeBSD's `da` does.
- **UAS.** Command, status, data-in, and data-out pipes; tagged commands, up to the device's queue
  depth, on bulk streams over USB 3 and on the command/status pipes alone over USB 2. Task
  management (abort, LUN reset) for timeouts.
- **Commands.** `INQUIRY`, `TEST UNIT READY`, `READ CAPACITY(10)` then `(16)` past 2 TiB,
  `READ`/`WRITE(10)` and `(16)`, `SYNCHRONIZE CACHE`, `MODE SENSE` for write protection. Quirks
  (FreeBSD's `usb_quirk` table for common sticks) as data.
- **Unplug.** Pending bios complete with `ENXIO`; the storage class revokes the disk, and a mounted
  file system sees errors, as on FreeBSD.

## 9. Testing

- **Host.** `lib/xhci`, `lib/usb`, `lib/hid`, `lib/scsi`, and `lib/vtterm` have unit tests and
  fuzz targets (bad descriptors, event-ring garbage, hostile report descriptors, random byte
  streams to the emulator against a reference screen model).
- **Boot tier.** QEMU machines on both arches with `qemu-xhci`, `usb-kbd`, `usb-tablet`,
  `usb-mouse`, a `usb-hub`, `usb-storage`, `usb-uas`, and `virtio-keyboard`/`virtio-tablet`. Tests
  check the device tree and names against FreeBSD's capture, type into `ttyv0` over QMP, and read the
  screen back with `screendump`. bhyve's `xhci,tablet` on amd64.
- **Storage.** fsx on a UFS2 file system on `da0`, over BBB and over UAS, then `fsck_ffs` on the
  server. Hot unplug (`device_del` over QMP) of a mounted disk during I/O.
- **Differential.** The same `usbconfig`, `devinfo`, `ls /dev`, and `kbdcontrol`/`vidcontrol` runs on
  FreeBSD 15.1 and AbyssBSD in the same QEMU machine, compared.

## 10. Performance

The track adds the `vt` component ([ADR-0015](adr/0015-usb-input-vt.md)), measured in bhyve
against FreeBSD's `vt_efifb` with the same framebuffer (bhyve's `fbuf`), resolution, and 8×16 font:

| Benchmark | Workload |
|---|---|
| `vt.cat` | Write a 4 MiB text file (80-column lines) to `/dev/ttyv1` while it is visible |
| `vt.scroll` | Write 100,000 short lines, so every line scrolls the screen |
| `vt.color` | Write a stream with SGR color changes every few characters |
| `vt.hidden` | `vt.cat` to a window that is not visible |

USB mass-storage throughput (`usb.umass`: sequential and random read and write over BBB) joins the
suite on the physical machines at P6, since no emulator compares it fairly with FreeBSD.
