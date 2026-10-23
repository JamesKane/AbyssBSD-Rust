# ADR-0015: USB, input, and virtual terminals

- Status: Accepted
- Date: 2026-10-23

## Context

The USB and input track runs before Phase 6, whose exit needs login on `ttyv0` and both physical
reference machines driven without a serial port. Neither has a usable serial console in daily use,
the Sky1 attaches its xHCI hosts through ACPI rather than PCI, and every keyboard involved is a USB
HID device. FreeBSD 15.1 defines what userland sees: `usbhid`/`hidbus`/`hkbd`/`hms` (the default
in 15), `kbdmux`, evdev (which 15's `moused` reads), `vt` with `teken` in xterm mode, `umass` over
CAM, and `usbconfig`'s `/dev/usb` ioctls. FreeBSD has no UAS or virtio-input driver.

ADR-0011 and ADR-0014 require a performance suite for each phase's components. Before P6, only
bhyve gates, and bhyve has an xHCI with a tablet and a framebuffer, but no USB disk.

## Decision

Adopt the design in [USB.md](../USB.md). Its load-bearing parts:

1. **xHCI only**, attached on PCI and on ACPI (`PNP0D10`) from one core. TRB, ring, and context
   encoding live in a pure, fuzzed `lib/xhci`; the ring types encode cycle-bit ownership.
2. **USB transfers are owned requests completed exactly once**, including on stall, timeout,
   cancellation, and unplug. A rejected submission hands the transfer back.
3. **Interfaces are the children drivers match**, with a typed `UsbIdent`; hubs (root and
   external) implement one `Ports` trait; one explore thread per bus serializes enumeration.
4. **HID is a bus** (`hidbus`, one child per top-level collection) with USB as its first
   transport, matching FreeBSD 15's names and leaving room for I2C-HID.
5. **One input class** under kbd/`kbdmux` and evdev, fed by `hkbd`, `hms`, and `virtio-input`.
6. **`vt` in the kernel** over a pure, fuzzed terminal emulator (`lib/vtterm`) with `teken`'s xterm
   behavior, rendering damaged cells from a glyph cache.
7. **`umass` publishes `da*` disks to the storage class directly**, with BBB and UAS on a small
   SCSI layer, and no CAM.
8. **Performance.** The track adds the `vt` component (`vt.cat`, `vt.scroll`, `vt.color`,
   `vt.hidden`), gated in bhyve against FreeBSD's `vt_efifb` with the same framebuffer and font.
   USB mass-storage throughput (`usb.umass`) joins the suite on the physical machines at P6.

## Consequences

- No CAM means no `pass*` devices or `camcontrol` for USB disks. If a later need (optical drives,
  SCSI tape, `camcontrol` workflows) appears, a CAM-compatible layer is a separate decision.
- UAS goes beyond FreeBSD, so its behavior is checked against the specification and QEMU's
  `usb-uas`, not against FreeBSD.
- The early framebuffer console stays for boot and panics; `vt` takes the screen over when it
  starts.
- The track's exit is split: QEMU now, the physical machines at P6 bring-up
  ([ADR-0010](0010-qemu-first-development.md)).
