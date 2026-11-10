#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
#
# Captures how Linux sees a reference machine, for tests/corpus/<machine>/linux
# and tests/corpus/acpi/<machine> (ROADMAP P8.0, MULTIUSER.md §15).
#
#   sudo sh linux-ground-truth.sh <machine>
#
# Read-only: sysfs and procfs reads and the standard listing tools. It never
# touches /dev/mem, MSRs, or device registers, never writes to a disk, and
# never loads a module. The output goes to a fresh directory under $TMPDIR
# (default /tmp) and into <machine>-linux.tar next to it, owned by the user
# who ran sudo.
# Commands that are missing or fail leave a note in their file and the
# capture goes on.

set -u

machine=${1:?usage: sudo sh linux-ground-truth.sh <machine>}
if [ "$(id -u)" -ne 0 ]; then
	echo "run it as root (sudo): the ACPI tables, dmesg, and lspci -vvv need it" >&2
	exit 1
fi

tmp=${TMPDIR:-/tmp}
top=$(mktemp -d "$tmp/${machine}-ground-truth.XXXXXX") || exit 1
out=$top/linux
acpi=$top/acpi
mkdir -p "$out" "$acpi" || exit 1

# run FILE COMMAND...: COMMAND's output (and errors) into $out/FILE.
run() {
	f=$1
	shift
	echo "== $f"
	"$@" >"$out/$f" 2>&1 || echo "(exit $?)" >>"$out/$f"
}

# The tables, as the firmware handed them over: <signature>.dat, lower case,
# repeated signatures numbered as Linux numbers them (ssdt1.dat, ...).
# Tables the OS loaded later (dynamic/, e.g. CPU SSDTs) go in dynamic/.
for t in /sys/firmware/acpi/tables/*; do
	[ -f "$t" ] || continue
	n=$(basename "$t" | tr 'A-Z' 'a-z')
	cp "$t" "$acpi/$n.dat"
done
if [ -d /sys/firmware/acpi/tables/dynamic ]; then
	mkdir -p "$acpi/dynamic"
	for t in /sys/firmware/acpi/tables/dynamic/*; do
		[ -f "$t" ] || continue
		cp "$t" "$acpi/dynamic/$(basename "$t" | tr 'A-Z' 'a-z').dat"
	done
fi
echo "== acpi: $(ls "$acpi" | wc -l) entries"

run uname.txt sh -c 'uname -a; grep PRETTY /etc/os-release; cat /proc/cmdline'
run dmesg.txt dmesg
run lspci.txt sh -c 'lspci -vvvnn; echo; lspci -tvnn'
run lsusb.txt sh -c 'lsusb -v; echo; lsusb -tv'
run cpuinfo.txt cat /proc/cpuinfo
run interrupts.txt cat /proc/interrupts
run iomem.txt cat /proc/iomem
run ioports.txt cat /proc/ioports
run lsmod.txt lsmod
run dmidecode.txt dmidecode
run efi.txt efibootmgr -v
run storage.txt sh -c 'lsblk -o NAME,SIZE,TYPE,MODEL,SERIAL,FSTYPE,PARTLABEL,MOUNTPOINTS; echo; for d in /sys/class/nvme/nvme*; do echo "$d: $(cat $d/model) serial $(cat $d/serial) fw $(cat $d/firmware_rev) at $(readlink -f $d/device)"; done'
run net.txt sh -c 'for i in /sys/class/net/*; do n=${i##*/}; echo "== $n"; ethtool -i "$n"; done; ip -d addr'
run acpi-devices.txt sh -c 'cd /sys/bus/acpi/devices && for d in *; do
	printf "%s path=%s hid=%s uid=%s status=%s adr=%s" "$d" \
	    "$(cat $d/path 2>/dev/null)" "$(cat $d/hid 2>/dev/null)" \
	    "$(cat $d/uid 2>/dev/null)" "$(cat $d/status 2>/dev/null)" \
	    "$(cat $d/adr 2>/dev/null)"
	for p in $d/physical_node*; do
		[ -e "$p" ] && printf " phys=%s" "$(readlink -f $p)"
	done
	echo
done'
run cpu-sysfs.txt sh -c 'cd /sys/devices/system/cpu && grep -r . cpu*/cpufreq/ cpu*/acpi_cppc/ \
    cpu*/cpu_capacity cpu*/cpuidle/state*/name cpu*/cpuidle/state*/desc \
    cpu*/cpuidle/state*/latency cpu*/cpuidle/state*/residency cpu*/topology/ 2>/dev/null'

user=${SUDO_USER:-root}
tar -C "$top" -cf "$tmp/${machine}-linux.tar" linux acpi
chown -R "$user" "$top" "$tmp/${machine}-linux.tar"
echo "done: $tmp/${machine}-linux.tar ($top)"
