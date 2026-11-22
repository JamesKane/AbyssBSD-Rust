#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# Suite p7 on the Orange Pi 6 Plus, for one core class: perf-run.sh runs it
# through its run(), so on FreeBSD inside AbyssBSD's root, with UBENCH_CPUS
# set to the class (empty for `all`: ubench's own CPUs 0-7). The far end is
# the build server on the LAN (PEER), where perf-board.sh runs `iperf3 -s`
# and `ubench p7.peer`. The benchmarks and their units are those of the
# bhyve suite (xtask/src/perf/p7.rs, RUN), but with no interrupt pinning:
# each kernel's NIC interrupts stay where it binds them, and iperf3 runs on
# the class's CPUs.
#
# Usage: p7-run.sh PEER. Prints `kbench begin p7` ... `kbench end p7`.
set -eu
peer=${1:?usage: p7-run.sh PEER}
cpus=${UBENCH_CPUS:-0,1,2,3,4,5,6,7}
export LD_LIBRARY_PATH=/tmp
rate() {
	awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Kbits/sec") v = $(i - 1) } END { print v + 0 }'
}
ip() {
	name=$1 scale=$2
	shift 2
	cpuset -l "$cpus" /tmp/iperf3 -f k "$@" | rate | awk -v n="$name" -v s="$scale" \
	    '$1 > 0 { printf "kbench %s %.3f ns/op\n", n, s / $1 }'
}
cpu() {
	name=$1
	shift
	a=$(sysctl -n kern.cp_time)
	r=$(cpuset -l "$cpus" /tmp/iperf3 -f k -t 20 -b 1000M "$@" | rate)
	b=$(sysctl -n kern.cp_time)
	hz=$(sysctl -n kern.clockrate | sed 's/.*stathz = \([0-9]*\).*/\1/')
	echo $a $b $hz $r | awk -v n="$name" '$12 > 0 {
		busy = 0
		for (i = 1; i <= 4; i++)
			busy += $(i + 5) - $i
		kib = $12 * 1000 / 8 * 20 / 1024
		printf "kbench %s %.3f ns/op\n", n, busy / $11 * 1e9 / kib
	}'
}
echo "kbench begin p7 cpus=$(sysctl -n hw.ncpu)"
/tmp/ubench-p6 p7 "$peer" | grep -v -e '^kbench begin' -e '^kbench end'
cpuset -l "$cpus" /tmp/iperf3 -s -D -B 127.0.0.1 -I /tmp/iperf3.lo.pid
trap 'kill "$(cat /tmp/iperf3.lo.pid)" 2>/dev/null; rm -f /tmp/iperf3.lo.pid' EXIT
sleep 1
ip tcp.lo.s1 8.192e9 -c 127.0.0.1 -t 10 -O 2
ip tcp.lo.s4 8.192e9 -c 127.0.0.1 -t 10 -O 2 -P 4
ip tcp.tx.s1 8.192e9 -c "$peer" -t 10 -O 2
ip tcp.tx.s4 8.192e9 -c "$peer" -t 10 -O 2 -P 4
ip tcp.rx.s1 8.192e9 -c "$peer" -t 10 -O 2 -R
ip tcp.rx.s4 8.192e9 -c "$peer" -t 10 -O 2 -R -P 4
cpu tcp.cpu.tx -c "$peer"
cpu tcp.cpu.rx -c "$peer" -R
ip udp.tx.bulk 8.192e9 -c "$peer" -t 10 -O 2 -u -b 0 -l 1448
ip udp.rx.bulk 8.192e9 -c "$peer" -t 10 -O 2 -u -b 0 -l 1448 -R
ip udp.tx.small 5.12e8 -c "$peer" -t 10 -O 2 -u -b 0 -l 64
ip udp.rx.small 5.12e8 -c "$peer" -t 10 -O 2 -u -b 0 -l 64 -R
echo "kbench end p7"
