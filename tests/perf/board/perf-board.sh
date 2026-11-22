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
SSHOPT="-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
mkdir -p "$S"
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

on() {
	if [ "$1" = abyss ]; then shift; ab "$@"; else shift; fb "$@"; fi
}

# p7's peers on the build server, started under CI's lock (waiting for
# it) and stopped after each run; their pids are in ~/abyss/perf/p7peer.pids.
case " $SUITES " in *" p7 "*) p7=1 ;; *) p7= ;; esac
peer_stop() {
	[ -n "$p7" ] || return 0
	timeout 60 ssh $SSHOPT abyss-build 'f=~/abyss/perf/p7peer.pids; [ -f $f ] && kill $(cat $f) 2>/dev/null; rm -f $f' || true
}
peer_start() {
	[ -n "$p7" ] || return 0
	peer_stop
	timeout 60 ssh $SSHOPT abyss-build "cd ~/abyss/perf && rm -f p7peer.pids && \
	    nohup lockf -k ~/abyss/ci/.lock sh -c ' \
		LD_LIBRARY_PATH=iperf3-3.22/usr/local/lib cpuset -l 0-15 iperf3-3.22/usr/local/bin/iperf3 -s -B $PEER > /dev/null & echo \$! > p7peer.pids.new; \
		cpuset -l 0-15 ubench-p6/ubench p7.peer $PEER & echo \$! >> p7peer.pids.new; \
		mv p7peer.pids.new p7peer.pids; wait' > p7peer.log 2>&1 < /dev/null &" || { log "p7 peers: ssh failed"; exit 1; }
	local i
	for i in $(seq 360); do
		[ "$(timeout 30 ssh $SSHOPT abyss-build 'wc -l < ~/abyss/perf/p7peer.pids 2>/dev/null')" -eq 2 ] 2>/dev/null && break
		[ $i -eq 1 ] && log "p7 peers: waiting for the build server's CI lock"
		sleep 10
	done
	sleep 2
	timeout 30 ssh $SSHOPT abyss-build 'kill -0 $(cat ~/abyss/perf/p7peer.pids)' || { log "p7 peers did not start: ~/abyss/perf/p7peer.log on the server"; exit 1; }
}
trap peer_stop EXIT

last=${SUITES##* }
for run in $(seq 0 $((RUNS - 1))); do
	if [ $((run % 2)) -eq 0 ]; then order="freebsd abyss"; else order="abyss freebsd"; fi
	for side in $order; do
		out=$(printf '%s/%s-%02d.log' "$S" "$side" "$run")
		if [ -f "$out" ] && grep -q "^kbench end $last\$" "$out" && [ "$(grep -c '^kbench end' "$out")" -eq $((4 * $#)) ]; then
			continue
		fi
		boot_to $side
		on $side 'df /mnt | grep -q kyuawork || { fsck_ffs -p /dev/gpt/kyuawork; mount /dev/gpt/kyuawork /mnt; }'
		log "run $((run + 1))/$RUNS: $side ($SUITES)"
		start=$(date +%s)
		peer_start
		T=7200 on $side "sh /mnt/perf/perf-run.sh $SUITES" > "$out.tmp" 2>&1
		rc=$?
		peer_stop
		ends=$(grep -c '^kbench end' "$out.tmp")
		if [ $rc -ne 0 ] || [ "$ends" -ne $((4 * $#)) ]; then
			log "run $((run + 1)) on $side failed (exit $rc, $ends of $((4 * $#)) suites): $out.tmp"
			tail -5 "$out.tmp"
			exit 1
		fi
		mv "$out.tmp" "$out"
		log "run $((run + 1)) on $side done in $(( $(date +%s) - start )) s"
	done
done
log "all $RUNS runs done: cargo xtask perf --platform opi6p --dir $S"
