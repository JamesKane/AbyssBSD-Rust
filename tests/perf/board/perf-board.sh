#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# The perf comparison on the Orange Pi 6 Plus, from the workstation
# (docs/BUILD-AND-TEST.md, "Performance on the Orange Pi 6 Plus"): RUNS
# runs of perf-run.sh on each side, alternating FreeBSD (its NODEBUG
# kernel, /boot/kernel.nodebug, booted once at a time with nextboot) and
# AbyssBSD, which side goes first alternating too. Each run's output goes
# to WORKDIR/<side>-<run>.log; `cargo xtask perf --platform opi6p --dir
# WORKDIR` makes the report. Rerun with the same WORKDIR to resume: runs
# with a complete log are kept.
#
# FreeBSD's side is a golden set (tests/perf/board/golden/<suites>/, its MANIFEST
# says what it was made from): a run whose FreeBSD log is there is copied
# into WORKDIR, not run again, so only AbyssBSD's side costs board time.
# The set is stale when the board's FreeBSD kernel, the benchmark inputs
# (/mnt/perf's files that the suites use), or the userland they run against change: perf-board.sh
# refuses to use it then (ABYSS_GOLDEN=0 ignores the set and runs FreeBSD;
# `ABYSS_GOLDEN=refresh` runs FreeBSD and writes its logs and MANIFEST
# into golden/).
#
# Usage: perf-board.sh WORKDIR RUNS SUITE...
# AbyssBSD's host key is kept apart from FreeBSD's, in
# WORKDIR/abyss_known_hosts.
#
# With p7, the build server (ABYSS_PEER) is the far end: for each run
# `iperf3 -s` and `ubench p7.peer` run there under CI's lock, so no CI or
# bhyve perf run shares the server, from the server's own copies
# (~/abyss/perf, which `cargo xtask --remote perf --suite p7 --prepare`
# makes), on its P-cores.
set -u
S=${1:?usage: perf-board.sh WORKDIR RUNS SUITE...}
RUNS=${2:?usage: perf-board.sh WORKDIR RUNS SUITE...}
shift 2
SUITES="$*"
[ -n "$SUITES" ] || { echo "usage: perf-board.sh WORKDIR RUNS SUITE..." >&2; exit 2; }
B=${ABYSS_BOARD:-192.168.0.19}
PEER=${ABYSS_PEER:-192.168.0.14}
# The build server by its address now (DHCP moves it), under the name ~/.ssh/config knows.
BUILD="-o HostName=$PEER abyss-build"
SSHOPT="-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
mkdir -p "$S"
GOLDEN_DIR=$(cd "$(dirname "$0")" && pwd)/golden/$(echo $SUITES | tr ' ' '-')
# What the golden set depends on besides FreeBSD's kernel: the files of
# /mnt/perf its runs use.
INPUTS="perf-run.sh"
for s in $SUITES; do
	case $s in
	p4 | p5 | p6) INPUTS="$INPUTS fsx tree.tar ubench ubench-p6" ;;
	p7) INPUTS="$INPUTS p7-run.sh iperf3 libiperf.so.0 ubench-p6" ;;
	esac
done
INPUTS=$(echo $INPUTS | tr ' ' '\n' | sort -u | tr '\n' ' ')
GOLDEN=${ABYSS_GOLDEN:-1}
fb() { timeout "${T:-120}" ssh $SSHOPT jkane@$B "su root -c '$*' < /dev/null" 2>/dev/null; }
ab() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/abyss_known_hosts root@$B "$*" 2>/dev/null; }
# freebsd (the NODEBUG kernel), freebsd-debug (the default kernel),
# abyss, or down.
which_os() {
	local id
	id=$(timeout 30 ssh $SSHOPT jkane@$B 'sysctl -n kern.ident' 2>/dev/null)
	case $id in
	*NODEBUG*) echo freebsd; return ;;
	GENERIC*) echo freebsd-debug; return ;;
	esac
	if [ "$(T=30 ab 'sysctl -n kern.ident')" = ABYSSBSD ]; then echo abyss; else echo down; fi
}
log() { echo "$(date +%T) $*"; }

# Brings the board up in `want`, rebooting as needed; FreeBSD always leaves
# nextboot set to the NODEBUG kernel, so the next FreeBSD boot uses it.
boot_to() {
	local want=$1 os tries=0
	while :; do
		os=$(which_os)
		[ "$os" = "$want" ] && return 0
		tries=$((tries + 1))
		[ $tries -gt 40 ] && { log "board not in $want after 40 tries (last: $os)"; exit 1; }
		case $os in
		down) sleep 20; continue ;;
		abyss)
			log "AbyssBSD: reboot (to FreeBSD)"
			ab 'shutdown -r now' > /dev/null ;;
		freebsd | freebsd-debug)
			if [ "$want" = abyss ]; then
				log "FreeBSD ($os): nextboot kernel.nodebug, BootNext AbyssBSD"
				fb 'nextboot -k kernel.nodebug && efibootmgr -n -b 0007 > /dev/null && shutdown -r now' > /dev/null
			else
				log "FreeBSD ($os): nextboot kernel.nodebug, reboot"
				fb 'nextboot -k kernel.nodebug && shutdown -r now' > /dev/null
			fi ;;
		esac
		sleep 60
	done
}

# Mounts the benchmark inputs (kyuawork) on `side`: the disk's label can
# come after sshd answers, so it tries for a minute.
mount_inputs() {
	local side=$1 i
	for i in $(seq 12); do
		on $side 'df /mnt | grep -q kyuawork || { fsck_ffs -p /dev/gpt/kyuawork; mount /dev/gpt/kyuawork /mnt; }; df /mnt | grep -q kyuawork' > /dev/null && return 0
		sleep 5
	done
	log "kyuawork would not mount on $side"
	exit 1
}

on() {
	if [ "$1" = abyss ]; then shift; ab "$@"; else shift; fb "$@"; fi
}

# Runs CMD as root on SIDE with its output and errors in OUT (an error from
# a benchmark is what says why it stopped), for up to two hours, and fails
# when OUT has not grown for ABYSS_STALL seconds (15 minutes): a benchmark
# that died can leave a helper holding the output open, and the run would
# wait out the two hours. What it left on the board is killed then.
run_watched() {
	local side=$1 cmd=$2 out=$3 pid last=-1 size idle=0 stall=${ABYSS_STALL:-900}
	if [ "$side" = abyss ]; then
		timeout 7200 ssh $SSHOPT -o UserKnownHostsFile=$S/abyss_known_hosts root@$B "$cmd" > "$out" 2>&1 < /dev/null &
	else
		timeout 7200 ssh $SSHOPT jkane@$B "su root -c '$cmd' < /dev/null" > "$out" 2>&1 < /dev/null &
	fi
	pid=$!
	while kill -0 $pid 2> /dev/null; do
		sleep 15
		size=$(stat -c %s "$out" 2> /dev/null || echo 0)
		if [ "$size" = "$last" ]; then idle=$((idle + 15)); else idle=0; last=$size; fi
		if [ $idle -ge $stall ]; then
			log "no output for $stall s: stopping the run and what it left on the board"
			kill $pid 2> /dev/null
			wait $pid 2> /dev/null
			on $side 'pkill -f "[/]tmp/ubench"; pkill -f "[p]erf-run.sh"; pkill -f "[p]7-run.sh"; pkill -f "[/]tmp/iperf3"; true' > /dev/null
			return 124
		fi
	done
	wait $pid
}

# p7's peers on the build server, started under CI's lock (waiting for
# it) and stopped after each run; their pids are in ~/abyss/perf/p7peer.pids.
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
trap peer_stop EXIT

last=${SUITES##* }

# The golden set: usable when it has this run's suites and its inputs are
# the ones on the board now.
inputs_hash() {
	local os=$(which_os)
	[ "$os" = down ] && { boot_to abyss >&2; os=abyss; }
	local sum="cd /mnt/perf && sha256 -q $INPUTS | md5 -q"
	case $os in abyss) mount_inputs abyss >&2; ab "$sum" ;; *) mount_inputs freebsd >&2; fb "$sum" ;; esac | tail -n 1
}
golden_ok=
if [ "$GOLDEN" = 1 ] && [ -f "$GOLDEN_DIR/MANIFEST" ]; then
	have=$(sed -n 's/^suites //p' "$GOLDEN_DIR/MANIFEST")
	want=$(sed -n 's/^inputs //p' "$GOLDEN_DIR/MANIFEST")
	if [ "$have" != "$SUITES" ]; then
		log "golden set is of '$have', not '$SUITES': running FreeBSD"
	else
		now=$(inputs_hash)
		if [ "$now" = "$want" ]; then
			golden_ok=1
		else
			log "golden set is stale: the benchmark inputs on the board are $now, it was made with $want (ABYSS_GOLDEN=refresh makes a new one, ABYSS_GOLDEN=0 ignores it)"
			exit 1
		fi
	fi
fi
for run in $(seq 0 $((RUNS - 1))); do
	if [ $((run % 2)) -eq 0 ]; then order="freebsd abyss"; else order="abyss freebsd"; fi
	for side in $order; do
		out=$(printf '%s/%s-%02d.log' "$S" "$side" "$run")
		if [ -f "$out" ] && grep -q "^kbench end $last\$" "$out" && [ "$(grep -c '^kbench end' "$out")" -eq $((4 * $#)) ]; then
			continue
		fi
		[ "$GOLDEN" = refresh ] && [ "$side" = abyss ] && continue
		g=$(printf '%s/freebsd-%02d.log' "$GOLDEN_DIR" "$run")
		if [ "$side" = freebsd ] && [ -n "$golden_ok" ]; then
			# More runs than the set has: AbyssBSD's side only.
			if [ -f "$g" ]; then
				log "run $((run + 1))/$RUNS: freebsd from the golden set"
				cp "$g" "$out"
			fi
			continue
		fi
		# Each run on a fresh boot (state left by an earlier suite counts); from
		# AbyssBSD that is a reboot into itself: it sets BootNext as FreeBSD does.
		if [ "$side" = abyss ] && [ "$(which_os)" = abyss ]; then
			log "AbyssBSD: fresh boot"
			ab 'efibootmgr -n -b 0007 > /dev/null; shutdown -r now' > /dev/null
			sleep 30
		fi
		boot_to $side
		mount_inputs $side
		log "run $((run + 1))/$RUNS: $side ($SUITES)"
		start=$(date +%s)
		peer_start
		run_watched $side "env PERF_PEER=$PEER sh /mnt/perf/perf-run.sh $SUITES" "$out.tmp"
		rc=$?
		peer_stop
		ends=$(grep -c '^kbench end' "$out.tmp")
		if [ $rc -ne 0 ] || [ "$ends" -ne $((4 * $#)) ]; then
			log "run $((run + 1)) on $side failed (exit $rc, $ends of $((4 * $#)) suites): $out.tmp"
			tail -5 "$out.tmp"
			exit 1
		fi
		mv "$out.tmp" "$out"
		if [ "$GOLDEN" = refresh ] && [ "$side" = freebsd ]; then
			mkdir -p "$GOLDEN_DIR"
			cp "$out" "$GOLDEN_DIR/freebsd-$(printf %02d $run).log"
			{
				echo "suites $SUITES"
				echo "inputs $(inputs_hash)"
				echo "kernel $(sed -n 's/^kbench env side=freebsd ident=//p' "$out" | head -1)"
				echo "date $(date +%F)"
				echo "runs $((run + 1))"
			} > "$GOLDEN_DIR/MANIFEST"
		fi
		log "run $((run + 1)) on $side done in $(( $(date +%s) - start )) s"
	done
done
log "all $RUNS runs done: cargo xtask perf --platform opi6p --dir $S"
