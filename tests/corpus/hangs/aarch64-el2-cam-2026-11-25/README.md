# `aarch64-el2` hang in `tests/user/cam`, captured live (2026-11-25)

The boot case `aarch64-el2` (QEMU, EL2, one attach worker, `hw.pci.enable_msix=0`, so INTx) timed
out in `tests/user/cam` (the user tests run alphabetically: `bpfwriter` is the last line printed).
Seen in runs 10, 11 and 14 of 14 (runs 10 and 11 while other builds loaded the machine; run 14 on an
otherwise idle one, which is what this captures, five minutes into the hang, over QMP).

- `serial.log`: the guest's console up to the hang (nothing after `bpfwriter: ok`).
- `registers-all-cpus.txt`: `info registers -a`. CPUs 1 to 3 are in `sched::idle_loop`
  (`abyss_hal_wait_for_interrupt`); CPU 0 is not.
- `cpu0-samples.txt`: sixty samples of CPU 0 at 50 ms. The program counter is in
  `arch_aarch64::gic::{ack,eoi}` and the register accessors, `abyss_el1_exit`, and
  `abyss_hal_interrupt_nest`; the link register in `irq::start_ithread`'s closure, `irq::dispatch`,
  `kernel::intc::dispatch`, `virtio_blk::filter` and `virtio_input::filter`. The stack pointer stays on
  one kernel stack. Interrupts are unmasked at the sampled points. In short, CPU 0 takes interrupts
  as fast as it can return from them: an interrupt storm on a shared INTx line, with the virtio
  filters on it being asked each time.
- `symbolized.txt`: the addresses (slide 0x1e800000 taken off) against the kernel built for the run.

What this does not show: which device keeps the line asserted. The monitor's reads of the GIC
distributor came back zero (the secure view), so they were dropped. `cam` drives the SCSI, SATA, and
NVMe pass devices, so the NVMe controller and the AHCI one are the first to look at: under INTx a
level interrupt stays asserted until the driver's filter or handler quiets the device.
