#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# One userland perf run (suites p4 to p7) on the X870E, on stock FreeBSD
# 15.1 or on AbyssBSD (docs/PERFORMANCE.md §4); the on-machine half of
# perf-x870e.sh, as tests/perf/board/perf-run.sh is of the Orange Pi's.
# Prints the kbench lines, the suites once for the class `all`.
#
# Usage: perf-run.sh SUITE... (p4, p5, p6, p7). The inputs are in /root/perf
# on the side's own root: ubench, ubench-p6, tree.tar, fsx, and for p7
# p7-run.sh, iperf3 and libiperf.so.0. p5's scratch disk is the GPT
# partition `abyssbench` on the media NVMe. p7's far end is PERF_PEER (the
# build server), where perf-x870e.sh starts the peers.
#
# Both sides run the same userland, AbyssBSD's root (FreeBSD 15.1's base):
# AbyssBSD runs from it; stock FreeBSD mounts it read-only and runs the
# suites chrooted into it. Both get a fresh tmpfs /tmp, every core at the
# one clock PERF_PIN_MHZ (stock's fixed P-state; AbyssBSD booted with
# abyss.bench.pin_perf), powerd stopped, and idle at C1.
set -eu
IN=/root/perf
SCRATCH=/dev/gpt/abyssbench
PEER=${PERF_PEER:-192.168.0.4}
PIN=${PERF_PIN_MHZ:-3000}
# One thread of each of eight cores (SMT siblings are 2n and 2n+1 on both
# kernels), the class `all`.
CPUS=${PERF_CPUS:-0,2,4,6,8,10,12,14}

[ -e $SCRATCH ] || { echo "perf-run: no $SCRATCH" >&2; exit 1; }
ident=$(sysctl -n kern.ident)
case $ident in
ABYSSBSD) side=abyss; root=/ ;;
GENERIC)
	side=freebsd; root=/mnt/aroot
	if sysctl -n debug.witness.watch >/dev/null 2>&1; then
		echo "perf-run: $ident has WITNESS" >&2; exit 1
	fi ;;
*) echo "perf-run: kernel $ident is neither AbyssBSD nor stock FreeBSD" >&2; exit 1 ;;
esac

if pgrep -x powerd >/dev/null; then
	service powerd onestop >/dev/null 2>&1 || kill "$(cat /var/run/powerd.pid)"
fi
cron=
if pgrep -x cron >/dev/null; then
	service cron onestop >/dev/null 2>&1 || kill "$(cat /var/run/cron.pid)"
	cron=1
fi
cleanup() {
	[ -z "${root:-}" ] || ! df $root/tmp | grep -q '^tmpfs' || umount $root/tmp
	[ -z "$cron" ] || service cron onestart >/dev/null 2>&1
}
trap cleanup EXIT
if [ $side = freebsd ]; then
	# Another level first: cpufreq skips a write of the level it believes
	# is current, and after boot it can believe the pinned one while the
	# package boosts (2026-11-27: freq read 3000 with the cores at 5.47 GHz).
	other=$(sysctl -n dev.cpu.0.freq_levels | tr ' ' '\n' | cut -d/ -f1 | grep -vx "$PIN" | head -1)
	for c in $(jot "$(sysctl -n hw.ncpu)" 0); do
		[ -z "$other" ] || sysctl -q dev.cpu.$c.freq=$other >/dev/null 2>&1 || true
		sysctl -q dev.cpu.$c.freq=$PIN >/dev/null 2>&1 || true
	done
	sysctl -q hw.acpi.cpu.cx_lowest=C1 >/dev/null
	mkdir -p $root
	df $root | grep -q abyssroot || mount -r /dev/gpt/abyssroot $root
	df $root/dev | grep -q devfs || mount -t devfs devfs $root/dev
fi
# Both sides at the pinned clock, within 1%.
for c in 0 6 14; do
	# Stock has dev.cpu.N.freq for CPU 0 only (the P-state is the package's).
	cur=$(sysctl -n dev.cpu.$c.freq 2>/dev/null) || continue
	[ $((cur * 100)) -ge $((PIN * 99)) ] && [ $((cur * 100)) -le $((PIN * 101)) ] ||
	    { echo "perf-run: cpu$c at $cur MHz, not $PIN" >&2; exit 1; }
done
for d in tmp bench benchtmp; do
	[ -d $root/$d ] || { echo "perf-run: no $root/$d" >&2; exit 1; }
done
mount -t tmpfs tmpfs $root/tmp
cp $IN/ubench $IN/tree.tar $IN/fsx $root/tmp/
cp $IN/ubench-p6 $root/tmp/ubench-p6
case " $* " in *" p7 "*) cp $IN/p7-run.sh $IN/iperf3 $IN/libiperf.so.0 $root/tmp/ ;; esac

# A quiet start: boot's own work done, then no periodic run, and a load
# under 0.3.
for i in $(jot 720); do
	pgrep -f /usr/sbin/periodic >/dev/null || break
	sleep 5
done
pgrep -f /usr/sbin/periodic >/dev/null && { echo "perf-run: periodic still running" >&2; exit 1; }
for i in $(jot 60); do
	load=$(sysctl -n vm.loadavg | awk '{ print $2 }')
	[ "$(echo "$load < 0.3" | bc)" = 1 ] && break
	sleep 5
done
echo "kbench env side=$side ident=$ident $(uname -r)"
echo "kbench env cpu0 freq=$(sysctl -n dev.cpu.0.freq) cx=$(sysctl -n dev.cpu.0.cx_lowest 2>/dev/null || echo none)"
echo "kbench env processes=$(ps -ax | wc -l | tr -d ' ') load=$(sysctl -n vm.loadavg)"

run() {
	if [ $side = freebsd ]; then
		chroot $root env UBENCH_DISK=$SCRATCH UBENCH_CPUS="$CPUS" "$@"
	else
		env UBENCH_DISK=$SCRATCH UBENCH_CPUS="$CPUS" "$@"
	fi
}
for suite in "$@"; do
	echo "kbench class all $CPUS"
	case $suite in
	p6) run /tmp/ubench-p6 p6 ;;
	p7) run sh /tmp/p7-run.sh $PEER ;;
	*) run /tmp/ubench $suite ;;
	esac
done
