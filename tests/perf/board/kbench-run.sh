#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# The in-kernel suite p2 on the Orange Pi 6 Plus's FreeBSD (its NODEBUG
# kernel), under perf-run.sh's conditions: powerd stopped, every cluster at
# its top frequency, idle held at C1. kbench.ko comes from the kyuawork
# partition's perf/, built against the board's kernel tree
# (docs/BUILD-AND-TEST.md); it is loaded for the run and unloaded after.
# Prints the kbench lines. AbyssBSD's side boots with
# `abyss.bench=p2 abyss.bench.reboot abyss.bench.cpus=<the same list>`.
#
# Usage: kbench-run.sh CPUS: the coordinator's CPU, then one core class's,
# e.g. 2,0,1,10,11 (the big cores, the coordinator on an A520); see
# tests/perf/README.md, "Settings".
set -eu
CPUS=${1:?usage: kbench-run.sh CPUS}
IN=/mnt/perf
CLUSTERS="2 8 6 10 0"	# as perf-run.sh: slowest top level first

case $(sysctl -n kern.ident) in
*NODEBUG*) ;;
*) echo "kbench-run: not the NODEBUG kernel" >&2; exit 1 ;;
esac
df /mnt | grep -q kyuawork || { echo "kbench-run: kyuawork is not on /mnt" >&2; exit 1; }
if pgrep -x powerd >/dev/null; then
	service powerd onestop >/dev/null 2>&1 || kill "$(cat /var/run/powerd.pid)"
fi
for c in $CLUSTERS; do
	sysctl -q dev.cpu.$c.freq=$(sysctl -n dev.cpu.$c.freq_levels | cut -d/ -f1) >/dev/null
done
for c in $CLUSTERS; do
	max=$(sysctl -n dev.cpu.$c.freq_levels | cut -d/ -f1)
	[ "$(sysctl -n dev.cpu.$c.freq)" = "$max" ] || { echo "kbench-run: cpu$c not at $max" >&2; exit 1; }
done
sysctl -q hw.acpi.cpu.cx_lowest=C1 >/dev/null
for i in $(jot 60); do
	[ "$(echo "$(sysctl -n vm.loadavg | awk '{ print $2 }') < 0.3" | bc)" = 1 ] && break
	sleep 5
done
kldstat -q -m kbench && kldunload kbench
kldload $IN/kbench.ko
trap 'kldunload kbench' EXIT
sysctl -q debug.kbench.cpus="$CPUS" >/dev/null
sysctl -q kern.msgbuf_clear=1 >/dev/null
sysctl -q debug.kbench.run=p2 >/dev/null
dmesg | grep -a '^kbench '
