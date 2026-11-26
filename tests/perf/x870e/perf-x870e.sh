#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# The userland perf comparison (suites p4 to p7) on the X870E, from the
# workstation, as tests/perf/board/perf-board.sh does for the Orange Pi:
# RUNS runs of perf-run.sh on each side, alternating stock FreeBSD 15.1
# (Boot0002) and AbyssBSD (Boot0003), which side goes first alternating
# too. Each run's output goes to WORKDIR/<side>-<run>.log; `cargo xtask
# perf --platform x870e --dir WORKDIR` makes the report. Rerun with the
# same WORKDIR to resume: runs with a complete log are kept.
#
# Both sides run at 3000 MHz: stock at its fixed P-state, AbyssBSD with
# abyss.bench.pin_perf=88 on its command line for the runs (put back at
# the end). AbyssBSD's command line is in EFI/ABYSS/CMDLINE on its ESP.
#
# Needs: the inputs from the build server's ~/abyss/perf (ubench, ubench-p6,
# tree.tar, fsx, and for p7 iperf3-3.22), the GPT partition `abyssbench` on
# the media NVMe (p5's scratch), and /bench and /benchtmp on AbyssBSD's
# root. With p7 the build server (ABYSS_PEER) is the far end, as on the
# board.
#
# Usage: perf-x870e.sh WORKDIR RUNS SUITE...
set -u
S=${1:?usage: perf-x870e.sh WORKDIR RUNS SUITE...}
RUNS=${2:?usage: perf-x870e.sh WORKDIR RUNS SUITE...}
shift 2
SUITES="$*"
[ -n "$SUITES" ] || { echo "usage: perf-x870e.sh WORKDIR RUNS SUITE..." >&2; exit 2; }
B=${ABYSS_BOARD:-192.168.0.23}
PEER=${ABYSS_PEER:-192.168.0.4}
PIN_MHZ=${P2_PIN_MHZ:-3000}
PIN_PERF=${P2_PIN_PERF:-88}
HERE=$(cd "$(dirname "$0")" && pwd)
BUILD="abyss-build"
SSHOPT="-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
mkdir -p "$S"
fb() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/freebsd_known_hosts root@$B "$*" 2>/dev/null; }
ab() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/abyss_known_hosts root@$B "$*" 2>/dev/null; }
on() { if [ "$1" = abyss ]; then shift; ab "$@"; else shift; fb "$@"; fi; }
log() { echo "$(date +%T) $*"; }
CMDLINE_NORMAL='boot_multicons=YES boot_serial=YES'

which_os() {
	case $(T=15 fb 'sysctl -n kern.ident') in
	GENERIC) echo freebsd; return ;;
	esac
	if [ "$(T=15 ab 'sysctl -n kern.ident')" = ABYSSBSD ]; then echo abyss; else echo down; fi
}
# Brings the machine up in `want`, rebooting as needed (BootNext chooses).
boot_to() {
	local want=$1 os tries=0
	while :; do
		os=$(which_os)
		[ "$os" = "$want" ] && return 0
		tries=$((tries + 1))
		[ $tries -gt 60 ] && { log "machine not in $want after 60 tries (last: $os)"; exit 1; }
		case $os in
		down) sleep 20; continue ;;
		freebsd) fb "efibootmgr -n -b $([ "$want" = abyss ] && echo 0003 || echo 0002) >/dev/null; shutdown -r now" >/dev/null ;;
		abyss) ab "efibootmgr -n -b $([ "$want" = freebsd ] && echo 0002 || echo 0003) >/dev/null; shutdown -r now" >/dev/null ;;
		esac
		sleep 45
	done
}
# AbyssBSD's command line on its ESP: with the clock pin for the runs.
set_cmdline() {
	on $1 "mkdir -p /mnt/e; mount -t msdosfs /dev/gpt/abyssefi /mnt/e && printf %s '$2' > /mnt/e/EFI/ABYSS/CMDLINE; r=\$?; umount /mnt/e; exit \$r"
}
restore() {
	case $(which_os) in
	freebsd) set_cmdline freebsd "$CMDLINE_NORMAL" && log "AbyssBSD's command line put back" ;;
	abyss) set_cmdline abyss "$CMDLINE_NORMAL" && log "AbyssBSD's command line put back" ;;
	*) log "WARNING: the machine is down: AbyssBSD's command line still has the clock pin" ;;
	esac
	peer_stop
}

# p7's peers on the build server, started under CI's lock and stopped after
# each run; their pids are in ~/abyss/perf/p7peer.pids.
case " $SUITES " in *" p7 "*) p7=1 ;; *) p7= ;; esac
peer_stop() {
	[ -n "$p7" ] || return 0
	timeout 60 ssh $SSHOPT $BUILD 'f=~/abyss/perf/p7peer.pids; [ -f $f ] && kill $(cat $f) 2>/dev/null; rm -f $f' || true
}
peer_start() {
	[ -n "$p7" ] || return 0
	peer_stop
	timeout 60 ssh $SSHOPT $BUILD "cd ~/abyss/perf && rm -f p7peer.pids && \
	    nohup lockf -k ~/abyss/ci/.lock sh -c ' \
		LD_LIBRARY_PATH=iperf3-3.22/usr/local/lib cpuset -l 0-15 iperf3-3.22/usr/local/bin/iperf3 -s -B $PEER > /dev/null & echo \$! > p7peer.pids.new; \
		cpuset -l 0-15 ubench-p6/ubench p7.peer $PEER & echo \$! >> p7peer.pids.new; \
		mv p7peer.pids.new p7peer.pids; wait' > p7peer.log 2>&1 < /dev/null &" || { log "p7 peers: ssh failed"; exit 1; }
	local i
	for i in $(seq 360); do
		[ "$(timeout 30 ssh $SSHOPT $BUILD 'wc -l < ~/abyss/perf/p7peer.pids 2>/dev/null')" -eq 2 ] 2>/dev/null && break
		[ $i -eq 1 ] && log "p7 peers: waiting for the build server's CI lock"
		sleep 10
	done
	sleep 2
	timeout 30 ssh $SSHOPT $BUILD 'kill -0 $(cat ~/abyss/perf/p7peer.pids)' || { log "p7 peers did not start: ~/abyss/perf/p7peer.log on the server"; exit 1; }
}
trap restore EXIT

# Runs CMD as root on SIDE with its output and errors in OUT, for up to two
# hours, and fails when OUT has not grown for ABYSS_STALL seconds.
run_watched() {
	local side=$1 cmd=$2 out=$3 pid last=-1 size idle=0 stall=${ABYSS_STALL:-900}
	local kh=$S/freebsd_known_hosts
	[ "$side" = abyss ] && kh=$S/abyss_known_hosts
	timeout 7200 ssh $SSHOPT -o UserKnownHostsFile=$kh root@$B "$cmd" > "$out" 2>&1 < /dev/null &
	pid=$!
	while kill -0 $pid 2> /dev/null; do
		sleep 15
		size=$(stat -c %s "$out" 2> /dev/null || echo 0)
		if [ "$size" = "$last" ]; then idle=$((idle + 15)); else idle=0; last=$size; fi
		if [ $idle -ge $stall ]; then
			log "no output for $stall s: stopping the run and what it left on the machine"
			kill $pid 2> /dev/null
			wait $pid 2> /dev/null
			on $side 'pkill -f "[/]tmp/ubench"; pkill -f "[p]erf-run.sh"; pkill -f "[p]7-run.sh"; pkill -f "[/]tmp/iperf3"; true' > /dev/null
			return 124
		fi
	done
	wait $pid
}

# The inputs on each side's own root.
push_inputs() {
	local side=$1 kh=$S/freebsd_known_hosts
	[ "$side" = abyss ] && kh=$S/abyss_known_hosts
	on $side 'mkdir -p /root/perf /bench /benchtmp' || return 1
	for f in ubench tree.tar fsx; do
		scp -q $SSHOPT -o UserKnownHostsFile=$kh $IND/$f root@$B:/root/perf/ || return 1
	done
	scp -q $SSHOPT -o UserKnownHostsFile=$kh $IND/ubench-p6 root@$B:/root/perf/ubench-p6 || return 1
	scp -q $SSHOPT -o UserKnownHostsFile=$kh $HERE/perf-run.sh $HERE/p7-run.sh root@$B:/root/perf/ || return 1
	if [ -n "$p7" ]; then
		scp -q $SSHOPT -o UserKnownHostsFile=$kh $IND/iperf3 $IND/libiperf.so.0 root@$B:/root/perf/ || return 1
	fi
}

# Stage the inputs from the build server once.
IND=$S/inputs
if [ ! -f $IND/.done ]; then
	mkdir -p $IND
	P=abyss-build:abyss/perf
	for f in ubench tree.tar fsx; do scp -q $BUILD:abyss/perf/$f $IND/ || { log "cannot fetch $f"; exit 1; }; done
	scp -q $BUILD:abyss/perf/ubench-p6/ubench $IND/ubench-p6 || { log "cannot fetch ubench-p6"; exit 1; }
	scp -q $BUILD:abyss/perf/iperf3-3.22/usr/local/bin/iperf3 $IND/iperf3 || exit 1
	scp -q $BUILD:abyss/perf/iperf3-3.22/usr/local/lib/libiperf.so.0 $IND/libiperf.so.0 || exit 1
	touch $IND/.done
fi

last=${SUITES##* }
need=$#
for run in $(seq 0 $((RUNS - 1))); do
	if [ $((run % 2)) -eq 0 ]; then order="freebsd abyss"; else order="abyss freebsd"; fi
	for side in $order; do
		out=$(printf '%s/%s-%02d.log' "$S" "$side" "$run")
		if [ -f "$out" ] && grep -q "^kbench end $last\$" "$out" && [ "$(grep -c '^kbench end' "$out")" -eq $need ]; then
			continue
		fi
		rm -f "$out.tmp"
		# Each run on a fresh boot; AbyssBSD's with the clock pin.
		boot_to freebsd
		set_cmdline freebsd "$CMDLINE_NORMAL abyss.bench.pin_perf=$PIN_PERF" || { log "could not set AbyssBSD's command line"; exit 1; }
		if [ "$side" = abyss ]; then
			boot_to abyss
		fi
		os=$(which_os)
		[ "$os" = "$side" ] || { log "machine is $os, not $side"; exit 1; }
		push_inputs $side || { log "could not push the inputs to $side"; exit 1; }
		log "run $((run + 1))/$RUNS: $side ($SUITES)"
		start=$(date +%s)
		peer_start
		run_watched $side "env PERF_PEER=$PEER PERF_PIN_MHZ=$PIN_MHZ sh /root/perf/perf-run.sh $SUITES" "$out.tmp"
		rc=$?
		peer_stop
		ends=$(grep -c '^kbench end' "$out.tmp")
		if [ $rc -ne 0 ] || [ "$ends" -ne $need ]; then
			log "run $((run + 1)) on $side failed (exit $rc, $ends of $need suites): $out.tmp"
			tail -5 "$out.tmp"
			exit 1
		fi
		mv "$out.tmp" "$out"
		log "run $((run + 1)) on $side done in $(( $(date +%s) - start )) s"
	done
done
log "all $RUNS runs done: cargo xtask perf --platform x870e --dir $S"
