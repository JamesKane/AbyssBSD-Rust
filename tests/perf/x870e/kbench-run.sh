#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# The in-kernel suite p2 on the X870E's stock FreeBSD 15.1, as
# tests/perf/board/kbench-run.sh does on the Orange Pi 6 Plus: powerd off,
# every CPU at its top frequency level, idle held at C1, kbench.ko loaded
# for the run. Stock 15.1 drives this CPU through hwpstate (two P-states,
# the lower one at boot); AbyssBSD drives it through CPPC at its highest
# performance. Prints the kbench lines.
#
# Usage: kbench-run.sh CPUS KBENCH.KO: the coordinator's CPU, then the
# workers' (tests/perf/README.md, "Settings"), e.g. 0,2,4,6,8,10,12,14,16.
set -eu
CPUS=${1:?usage: kbench-run.sh CPUS KBENCH.KO}
KO=${2:?usage: kbench-run.sh CPUS KBENCH.KO}
if pgrep -x powerd >/dev/null; then
	service powerd onestop >/dev/null 2>&1 || kill "$(cat /var/run/powerd.pid)"
fi
top=$(sysctl -n dev.cpu.0.freq_levels | cut -d/ -f1)
for c in $(jot "$(sysctl -n hw.ncpu)" 0); do
	sysctl -q dev.cpu.$c.freq=$top >/dev/null 2>&1 || true
done
sysctl -q hw.acpi.cpu.cx_lowest=C1 >/dev/null
for i in $(jot 60); do
	[ "$(echo "$(sysctl -n vm.loadavg | awk '{ print $2 }') < 0.3" | bc)" = 1 ] && break
	sleep 5
done
echo "kbench env freq=$(sysctl -n dev.cpu.0.freq) levels=$(sysctl -n dev.cpu.0.freq_levels | tr ' ' ,) cx=$(sysctl -n hw.acpi.cpu.cx_lowest) ident=$(sysctl -n kern.ident)"
kldstat -q -m kbench && kldunload kbench
kldload "$KO"
trap 'kldunload kbench' EXIT
sysctl -q debug.kbench.cpus="$CPUS" >/dev/null
sysctl -q kern.msgbuf_clear=1 >/dev/null
sysctl -q debug.kbench.run=p2 >/dev/null
dmesg | grep -a '^kbench '
