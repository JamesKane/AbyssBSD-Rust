#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# One perf run on the Orange Pi 6 Plus, on FreeBSD or AbyssBSD
# (docs/PERFORMANCE.md §4, docs/BUILD-AND-TEST.md). Run as root by
# perf-board.sh; prints the kbench lines, each suite's tagged with the core
# class it ran on:
#
#	kbench class <class> <cpus>
#	kbench begin <suite> cpus=<n>
#	kbench <name> <value> <unit>
#	kbench end <suite>
#
# Usage: perf-run.sh SUITE... (p4, p5, p6). Inputs come from /mnt/perf on
# the kyuawork partition: ubench, ubench-p6 (the p6 build), tree.tar, fsx.
#
# Both kernels run the same userland: AbyssBSD's root (FreeBSD 15.1's
# base). AbyssBSD runs from it; FreeBSD mounts it read-only and runs the
# suites chrooted into it. Both get a fresh tmpfs /tmp, every CPU cluster
# held at its highest frequency with powerd stopped, and FreeBSD's idle at
# C1 (WFI), the only idle state AbyssBSD has yet.
set -eu
IN=/mnt/perf
SCRATCH=/dev/gpt/abyssbench	# p5's scratch partition, on the Samsung
# The core classes (docs/HARDWARE.md §4): the A720 clusters at 2.6 and
# 2.5 GHz, those at 2.3 and 2.2 GHz, and the A520s. `all` is ubench's own
# CPUs 0-7, across the three.
CLASSES=${PERF_CLASSES:-"big:0,1,10,11 mid:6,7,8,9 little:2,3,4,5 all:"}
# One CPU of each cluster (a cluster shares its frequency), slowest top
# level first. A write to dev.cpu.N.freq sets every cluster to its level
# nearest the value on upstream FreeBSD and AbyssBSD (cpufreq_curr_sysctl),
# but only N's cluster on the board's branch: writing each cluster's top
# level in this order, the fastest last, leaves every cluster at its top
# either way.
CLUSTERS="2 8 6 10 0"

df /mnt | grep -q kyuawork || { echo "perf-run: kyuawork is not on /mnt" >&2; exit 1; }
case " $* " in *" p5 "*) [ -e $SCRATCH ] || { echo "perf-run: no $SCRATCH" >&2; exit 1; } ;; esac
ident=$(sysctl -n kern.ident)
case $ident in
ABYSSBSD) side=abyss; root=/ ;;
*NODEBUG*)
	side=freebsd; root=/mnt/aroot
	if sysctl -n debug.witness.watch >/dev/null 2>&1; then
		echo "perf-run: $ident has WITNESS" >&2; exit 1
	fi ;;
*) echo "perf-run: kernel $ident is neither AbyssBSD nor a NODEBUG FreeBSD" >&2; exit 1 ;;
esac

# The same conditions on both sides.
if pgrep -x powerd >/dev/null; then
	service powerd onestop >/dev/null 2>&1 || kill "$(cat /var/run/powerd.pid)"
fi
for c in $CLUSTERS; do
	max=$(sysctl -n dev.cpu.$c.freq_levels | cut -d/ -f1)
	sysctl -q dev.cpu.$c.freq=$max >/dev/null
done
for c in $CLUSTERS; do
	max=$(sysctl -n dev.cpu.$c.freq_levels | cut -d/ -f1)
	cur=$(sysctl -n dev.cpu.$c.freq)
	[ "$cur" = "$max" ] || { echo "perf-run: cpu$c at $cur MHz, not $max" >&2; exit 1; }
done
if [ $side = freebsd ]; then
	sysctl -q hw.acpi.cpu.cx_lowest=C1 >/dev/null
	mkdir -p $root
	df $root | grep -q abyssroot || mount -r /dev/gpt/abyssroot $root
	df $root/dev | grep -q devfs || mount -t devfs devfs $root/dev
fi
for d in tmp bench benchtmp; do
	[ -d $root/$d ] || { echo "perf-run: no $root/$d" >&2; exit 1; }
done
mount -t tmpfs tmpfs $root/tmp
trap 'umount $root/tmp' EXIT
cp $IN/ubench $IN/tree.tar $IN/fsx $root/tmp/
cp $IN/ubench-p6 $root/tmp/ubench-p6

# A quiet start: boot's own work (fsck, periodic, devmatch) done, up to
# five minutes.
for i in $(jot 60); do
	load=$(sysctl -n vm.loadavg | awk '{ print $2 }')
	[ "$(echo "$load < 0.3" | bc)" = 1 ] && break
	sleep 5
done
echo "kbench env side=$side ident=$ident $(uname -r)"
for c in $CLUSTERS; do
	echo "kbench env cpu$c freq=$(sysctl -n dev.cpu.$c.freq) cx=$(sysctl -n dev.cpu.$c.cx_lowest 2>/dev/null || echo none)"
done
echo "kbench env processes=$(ps -ax | wc -l | tr -d ' ') load=$(sysctl -n vm.loadavg)"

run() {
	if [ $side = freebsd ]; then
		chroot $root env UBENCH_DISK=$SCRATCH UBENCH_CPUS="$cpus" "$@"
	else
		env UBENCH_DISK=$SCRATCH UBENCH_CPUS="$cpus" "$@"
	fi
}
for class in $CLASSES; do
	name=${class%%:*}
	cpus=${class#*:}
	for suite in "$@"; do
		echo "kbench class $name ${cpus:-0-7}"
		case $suite in
		p6) run /tmp/ubench-p6 p6 ;;
		*) run /tmp/ubench $suite ;;
		esac
	done
done
