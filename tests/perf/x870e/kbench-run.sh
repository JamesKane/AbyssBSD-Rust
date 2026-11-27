#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# The in-kernel suite p2 on the X870E's stock FreeBSD 15.1, as
# tests/perf/board/kbench-run.sh does on the Orange Pi 6 Plus: powerd off,
# every CPU at its top frequency level, idle held at C1, kbench.ko loaded
# for the run. Stock 15.1 drives this CPU through hwpstate (two P-states,
# the lower one at boot); AbyssBSD drives it through CPPC at its highest
# performance. Prints the kbench lines.
#
# Usage: kbench-run.sh CPUS KBENCH.KO [SUITE]: the coordinator's CPU, then the
# workers' (tests/perf/README.md, "Settings"), e.g. 0,2,4,6,8,10,12,14,16;
# SUITE is p2 unless another is asked for (callout, for investigations).
set -eu
CPUS=${1:?usage: kbench-run.sh CPUS KBENCH.KO}
KO=${2:?usage: kbench-run.sh CPUS KBENCH.KO}
SUITE=${3:-p2}
if pgrep -x powerd >/dev/null; then
	service powerd onestop >/dev/null 2>&1 || kill "$(cat /var/run/powerd.pid)"
fi
# P2_PIN_MHZ pins every CPU at that level (the fixed 3000 MHz P-state,
# which has no boost) instead of the top one; AbyssBSD's side pins its
# cores to the same clock (p2-x870e.sh, abyss.bench.pin_perf).
top=$(sysctl -n dev.cpu.0.freq_levels | tr ' ' '\n' | cut -d/ -f1 | { if [ -n "${P2_PIN_MHZ:-}" ]; then grep -x "$P2_PIN_MHZ"; else head -1; fi; })
# Another level first: cpufreq skips a write of the level it believes is
# current, and after boot it can believe the pinned one while the package
# boosts (2026-11-27: freq read 3000 with the cores at 5.47 GHz).
other=$(sysctl -n dev.cpu.0.freq_levels | tr ' ' '\n' | cut -d/ -f1 | grep -vx "$top" | head -1)
for c in $(jot "$(sysctl -n hw.ncpu)" 0); do
	[ -z "$other" ] || sysctl -q dev.cpu.$c.freq=$other >/dev/null 2>&1 || true
	sysctl -q dev.cpu.$c.freq=$top >/dev/null 2>&1 || true
done
sysctl -q hw.acpi.cpu.cx_lowest=C1 >/dev/null
for i in $(jot 60); do
	[ "$(echo "$(sysctl -n vm.loadavg | awk '{ print $2 }') < 0.3" | bc)" = 1 ] && break
	sleep 5
done
echo "kbench env freq=$(sysctl -n dev.cpu.0.freq) levels=$(sysctl -n dev.cpu.0.freq_levels | tr ' ' ,) cx=$(sysctl -n hw.acpi.cpu.cx_lowest) ident=$(sysctl -n kern.ident)"
unload() { for id in $(kldstat | awk '/kbench/ { print $1 }'); do kldunload -i "$id"; done; }
unload
kldload "$KO"
trap unload EXIT
sysctl -q debug.kbench.cpus="$CPUS" >/dev/null
sysctl -q kern.msgbuf_clear=1 >/dev/null
sysctl -q debug.kbench.run=$SUITE >/dev/null
dmesg | grep -a '^kbench '
