# Z690-A ground truth

The build server (MSI PRO Z690-A, Core i7-12700KF, Radeon RX 6700 XT) as FreeBSD 15.1 sees it.

## `freebsd/amdgpu/` (2026-11-24)

How FreeBSD 15.1-RELEASE with drm-kmod's `amdgpu` (Display Core v3.2.301) sees the Navi 22
(`1002:73df`, subsystem `1458:2408`, revision `c0`, at `03:00.0`), for the amd-rdna2 specification:
`pciconf -lvbc` (its BARs and capabilities, Resizable BAR on), `devinfo -rv`, the whole `dmesg` (the driver's
initialisation: IP blocks, firmware loads, VRAM and GART, PSP TMR, SMU and DMUB versions), the decoded ACPI
tables (`acpidump -t`, which lists a `VFCT`), the firmware module hashes, and what `linsysfs` shows.

Not captured: the video BIOS and the driver's debugfs files. A second capture that read those (with the
driver's `lindebugfs` mounted, plus `acpidump -dt`) **reset the build server**, so no dump of the ROM, the
firmware information, or the sensors exists for this card; do not read those files on this machine.
