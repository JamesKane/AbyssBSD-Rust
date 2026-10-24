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
| `sys/input` | kernel | The input class: kbd and `kbdmux`, keymaps, `sysmouse` |
| `sys/evdev` | kernel | evdev: `/dev/input/eventN`, per-open client queues, the `EVIOC*` ioctls, the `kern.evdev` sysctls |
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

The interrupt-in stream runs only while some child wants it, as `hid_intr_start`/`hid_intr_stop`
do: a child calls `intr_start` when it has a consumer and `intr_stop` when the last goes away,
reports go only to started children, and the transport pauses (`HidTransport::pause`, which must
not sleep) once none is started. `hkbd` starts at attach, since the kbd layer always listens;
`hms` starts when its evdev node is first opened and stops at the last close.

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
- `virtio-input` (`vtinputN`, after FreeBSD's `vtblk` and `vtrnd`) reads the device's name,
  serial, ID, properties, event bits, and axis ranges from its configuration space into an evdev
  device as it stands, and keeps the event queue full of buffers. Events are already evdev events.
  A device with keyboard keys (keys a HID keyboard can name) is also a `kbd`, like `hkbd`: its
  keys go through the input class, which repeats them itself (the device's repeats are dropped),
  and the lock LEDs go back on the status queue. Any other device reports straight to evdev, like
  `hms`.

### 6.3 The input class

`KbdSource` and `PointerSource` are the driver traits. Above them `sys/input` provides what
FreeBSD's userland expects:

- **kbd.** It follows `kbd.c`, `kbdmux.c`, and `hkbd.c`:
  - Each keyboard is `kbdN` at the lowest free index, with a node named for its driver
    (`/dev/hkbd0`, `/dev/vtinput0`) and the link `/dev/kbdN`. `kbdmux0` is `kbd0`: we have no
    `atkbd`, which is FreeBSD's `kbd0` on q35.
  - Keyboard drivers report HID usages. The class repeats the last key pressed (`kb_delay1`, then
    `kb_delay2`) and reads keys in the keyboard's mode, as `hkbd` emulates an AT keyboard:
    - `K_RAW`: AT set-1 scancodes, with prefixes and PrintScreen's and Pause's fake Shift and Ctrl.
    - `K_CODE`: keycodes.
    - `K_XLATE`: actions through the keyboard's own keymap (`genkbd_keyaction`), with dead keys
      and Alt-keypad compose.
    - The default keymap, accent map, and function key strings are `kbdtables.h`'s.
  - Ownership is `kbd_allocate`'s: one owner at a time.
    - `kbdmux0` takes each keyboard as it registers, in `K_RAW`, and decodes the scancodes itself.
    - The console owns `kbdmux0`, as `vt` does, and makes its actions terminal input as
      `vt_processkey` and `teken` do. Ctrl-Alt-Delete signals `init`, as `shutdown_nice` does.
    - Otherwise, an open node owns its keyboard and reads it as `genkbd` does. Every node is
      therefore busy (`EBUSY`) on a running system, as on FreeBSD.
  - Requests: `KDGKBMODE`/`KDSKBMODE`, `KDGETLED`/`KDSETLED`, `KDGKBSTATE`/`KDSKBSTATE`,
    `KDSETREPEAT`/`KDGETREPEAT`/`KDSETRAD`, `KDGKBINFO`, `KDGKBTYPE`, `KBADDKBD`/`KBRELKBD`,
    `GIO_`/`PIO_KEYMAP`, `GIO_`/`PIO_KEYMAPENT`, `GIO_`/`PIO_DEADKEYMAP`, `GETFKEY`/`SETFKEY`.
    `kbdmux0` passes the lock, LED, repeat, and keymap requests on to its keyboards, and lock keys
    light every keyboard's LEDs through it.
  - The ioctl path copies the keymap and accent map (too big for an argument) by their pointer.
  - Not yet: `vt`'s tty requests, which are how `kbdcontrol` reaches the keyboard (U.4).
    - FreeBSD's `PRIV_KEYBOARD` check on remapping the reboot and debugger keys (the nodes are
      root's, and `vt` must bring credentials).
    - `kbdmux`'s evdev key events under `rcpt_mask` `KBDMUX` (`evdev_scancode2key`).
    - Scroll Lock scrollback and cursor-key application mode, which are `vt`'s.
- **evdev** (`sys/evdev`). `/dev/input/eventN` per device with FreeBSD's evdev ABI (`EVIOCG*`,
  `struct input_event`, `kern.evdev.rcpt_mask`), which FreeBSD 15's `moused` reads. It follows
  `evdev.c` and `cdev.c`:
  - A driver builds a device (name, phys, uniq, `input_id`, event bits, absinfo, properties) and
    registers it at the lowest free unit. Pushed events go through FreeBSD's filters: a key
    pressed again becomes a repeat where the device has `EV_REP` without soft repeat, absolute
    values are smoothed by their fuzz, and unchanged key, LED, switch, and absolute states, zero
    relative motion, and empty reports are dropped.
  - Each open is a client with its own ring of eight reports' worth of events (the report size
    estimated as `evdev_estimate_report_size` does). Readers see only whole reports, stamped at
    `SYN_REPORT` with the realtime or monotonic clock (`EVIOCSCLOCKID`); an overflow empties the
    ring and leaves `SYN_DROPPED`. `EVIOCGRAB` and `EVIOCREVOKE` as FreeBSD has them.
  - The driver's `open` and `close` methods run at the first open and last close (`hms` starts
    and stops its interrupts there); writes inject events through the driver's `event` method.
  - `kern.evdev.input.N` has `name`, `id`, `phys`, `uniq`, `props`, and the per-type bit sets, in
    FreeBSD 15.1's formats (16-CURRENT's `devnum` is left out).
  - Keyboards publish through the kbd layer when `rcpt_mask` has `HW_KBD`, and `kbdmux` has its
    own device ("System keyboard multiplexer"), as FreeBSD's does.
- **sysmouse** (`sys/input/src/sysmouse.rs`). `/dev/sysmouse` and `/dev/consolectl`, as `vt` has
  them (`vt_sysmouse.c`, `vt_consolectl.c`). `moused` reads the mice through evdev and reports to
  `CONS_MOUSECTL` on `/dev/consolectl`; `/dev/sysmouse` serves what it reports as MouseSystems
  packets (level 0, five bytes) or sysmouse's (level 1, eight bytes, with the wheel and buttons 4
  to 10), with `MOUSE_GETHWINFO`, `GETMODE`/`SETMODE`, `GETLEVEL`/`SETLEVEL`, and `GETSTATUS`.
  The packet buffer lives from the first open to the last close. The evdev device "System mouse"
  registers right after `kbdmux`'s (`event1`, as on FreeBSD) and reports under `rcpt_mask`
  `SYSMOUSE`, with `sysmouse_t_axis` 0. `tests/user/mouse.c` checks it, and passes on FreeBSD 15.1.
  - Not yet: `poll` on the node and `SIGIO` (devfs has neither for devices), and the `vt` cursor
    and cut and paste (not in this track). `moused` itself, dynamically linked, runs at P6.

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
FreeBSD's own `teken` ends with for the same byte streams: `tests/corpus/vtterm/teken-drive.c`,
built with `sys/teken` on the server, prints a digest of the screen and cursor after each of 1000
pseudo-random streams, and `teken_differential` runs the same streams through `lib/vtterm`.

`lib/vtterm` departs from `teken` only where `teken` would leave the cursor outside the window
(its assertions catch these in debug builds): a resize that grows one dimension and shrinks the
other, and wrapping in a window no wider than the character. The screen buffer is `vt_buf`'s: a
ring of 500 history rows in which a whole-screen scroll moves the ring, and `vt`'s 32-bit cells
(`term_char_t`), with damage kept per row. As in `vt`, answers to queries are dropped
(`termteken_respond` is a no-op).

### 7.3 Rendering

The screen is cells (character, foreground, background, attributes). Writes mark damage per row;
the renderer draws damaged cells only, from a glyph cache of pre-rendered cells at the
framebuffer's pixel format, and scrolls by copying rows of the framebuffer where that is cheaper
than redrawing. Rendering runs on the writer's thread up to a budget, then defers to a timer, so a
burst of output costs one redraw. The font is 8×16 (FreeBSD's default `vt` font face metrics) unless
`vidcontrol -f` loads another.

### 7.4 The console

Kernel messages go to the serial port and to window 0 (`VT_CONSWINDOW`) in the kernel rendition,
each NL written as CR NL (`cnputc`), as on FreeBSD with both consoles. What the early console
showed before `vt` started becomes window 0's first screen. `/dev/console` is the serial port's
terminal (`ttyu0`) when the loader found one, and `ttyv0` otherwise, as FreeBSD's `comconsole`
comes before `vidconsole`; its output goes to that terminal alone. The keyboard types into the
visible window's terminal. Panics write to window 0 and draw at once, if the code that panicked
holds none of `vt`'s locks.

Implemented (`sys/vt`, `kernel/src/vtcons.rs`):

- The twelve windows (`VT_MAXWINDOWS`), `ttyv0` to `ttyvb`, each sized to the framebuffer in 8×16
  cells, centered.
- Drawing in the `vtflush` thread, at most 25 frames a second (`VT_TIMERFREQ`), of the cells that
  differ from what is on the screen (`vd_drawn`), in `vt`'s palette (`vt_termcolors.c`); output to
  a hidden window draws nothing.
- Switching with Alt-F*n* and the next and previous screen keys.
- Not yet: the `VT_*`, `KD*`, and `CONS_*` requests on the windows, per-window keyboard modes,
  `VT_PROCESS` switching, refusing to switch to a window nobody has open, and Scroll Lock
  scrollback.

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
  screen back with `screendump`. bhyve's `xhci,tablet` on amd64. The virtio devices are bound to the
  display (`video0`), so QMP input goes to the USB devices unless it names the display
  (`cargo xtask qmp --device video0`).
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
