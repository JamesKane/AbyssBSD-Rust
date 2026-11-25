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

## What it was (found the same day, fixed in `drivers/nvme`)

Later captures of the same hang, over QEMU's gdb stub and the monitor:

- The storm is on interrupt ID 35 (PCI INTx pin A). On it, `virtio_blk` and `virtio_input` answer
  stray every time; `nvme`'s filter claims every interrupt and schedules its thread.
- Of every PCI function, only `00:04.0`, the first NVMe controller, has its config-space Interrupt
  Status bit set; its `INTMS` is 0.
- The driver has taken everything: the admin completion queue holds only attach's 7 completions;
  I/O queue 1 has head = tail (42), nothing in flight, the entry at the head from the previous lap.
  Its reaper, single-stepped, finds nothing new and so writes no head doorbell.
- Writing the queue's head doorbell again, with the same value, from the debugger dropped the
  controller's INTx at once: it was holding the input with nothing new posted. Nothing in the
  driver's INTx path wrote a doorbell or the mask in that state. That the interrupt thread then
  outran the `cam` process for good is consistent with `cam` being pinned to the same CPU (the
  user tests are spawned bound to the runner's CPU); it was read from the code, not seen on the
  thread.
- One later probe, after the guest had moved on, wrote a doorbell value that was stale by then and
  then `INTMS` (the input dropped) and `INTMC` (it came back): it does not show whether unmasking
  alone clears the stuck state. How the fix works is therefore not established.
- A build with a 2000-identify burst in `cam` hung on its first two runs, but the admin queue showed
  none of those identifies had run: the hang came before the burst, which played no part.

The fix is FreeBSD's INTx handling (`nvme_ctrlr_shared_handler`): under INTx the interrupt thread
masks the controller (`INTMS`) while it reaps and unmasks it (`INTMC`) after; a polled admin
command holds the same mask until its completion is taken, so an admin completion never holds the
input the I/O queue shares. Before it, 5 of 16 runs hung; after it, 10 of 10 passed on a build
that still had the debugging (see the roadmap for the runs on the committed code). `cam` keeps a
200-identify burst as coverage of polled admin commands under INTx, not as a reproducer.

Not covered (FreeBSD services the admin queue in the shared handler, which covers both): an admin
command that times out releases the mask, so a late completion would hold the input again with no
reset to follow; and a queue in recovery is not reaped, so the input storms until the reset lands.

QEMU's NVMe model is GPL and was not read: what it does is known only by these experiments.
