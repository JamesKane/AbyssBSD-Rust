#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# The in-kernel suite p2 on the Orange Pi 6 Plus, from the workstation with
# the board's serial adapter (docs/BUILD-AND-TEST.md, "Performance on the
# Orange Pi 6 Plus"): RUNS runs on each side, alternating which goes
# first, each run every core class once. FreeBSD's side is kbench.ko on its
# NODEBUG kernel (kbench-run.sh, over ssh); AbyssBSD's is a boot with
# `abyss.bench=p2 abyss.bench.reboot abyss.bench.cpus=...` (one class a
# boot), read off the serial console, after which the board resets into
# FreeBSD, its default boot. Each run's output goes to
# WORKDIR/<side>-<run>.log, in perf-board.sh's form, so `cargo xtask perf
# --platform opi6p --dir WORKDIR` makes the report. Rerun with the same
# WORKDIR to resume: runs with a complete log are kept.
#
# Usage: p2-board.sh WORKDIR RUNS
#
# AbyssBSD's command line (EFI/ABYSS/CMDLINE on its ESP, the Samsung's
# abyssefi) is set for each of its boots from FreeBSD and put back to
# CMDLINE.noprof at the end. Nothing is written to the Crucial.
set -u
S=${1:?usage: p2-board.sh WORKDIR RUNS}
RUNS=${2:?usage: p2-board.sh WORKDIR RUNS}
B=${ABYSS_BOARD:-192.168.0.19}
TTY=${ABYSS_TTY:-/dev/ttyUSB0}
# The coordinator's CPU, then the class's (tests/perf/README.md,
# "Settings"): the coordinator off the class, on an A520 for the A720
# classes and on an A720 for the A520s; `all` as in bhyve, 0 then 1-7.
CLASSES=${P2_CLASSES:-"big:2,0,1,10,11 mid:2,6,7,8,9 little:6,2,3,4,5 all:0,1,2,3,4,5,6,7"}
NCLASS=$(echo $CLASSES | wc -w)
SSHOPT="-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
mkdir -p "$S"
fb() { timeout "${T:-120}" ssh $SSHOPT jkane@$B "su root -c '$*' < /dev/null" 2>/dev/null; }
# AbyssBSD's host key is kept apart from FreeBSD's, in WORKDIR/abyss_known_hosts.
ab() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/abyss_known_hosts root@$B "$*" 2>/dev/null; }
log() { echo "$(date +%T) $*"; }

# freebsd (the NODEBUG kernel), freebsd-debug (the default kernel), or
# not (AbyssBSD, or down).
which_os() {
	case $(timeout 30 ssh $SSHOPT jkane@$B 'sysctl -n kern.ident' 2>/dev/null) in
	*NODEBUG*) echo freebsd ;;
	GENERIC*) echo freebsd-debug ;;
	*) echo other ;;
	esac
}

# Brings FreeBSD's NODEBUG kernel up (AbyssBSD resets into FreeBSD after a
# suite; a FreeBSD boot always leaves nextboot set to the NODEBUG kernel).
to_freebsd() {
	local os tries=0
	while :; do
		os=$(which_os)
		[ "$os" = freebsd ] && return 0
		tries=$((tries + 1))
		[ $tries -gt 60 ] && { log "board not in FreeBSD after 60 tries (last: $os)"; exit 1; }
		if [ "$os" = freebsd-debug ]; then
			log "FreeBSD (debug kernel): nextboot kernel.nodebug, reboot"
			fb 'nextboot -k kernel.nodebug && shutdown -r now' > /dev/null
		elif [ "$(T=30 ab 'sysctl -n kern.ident')" = ABYSSBSD ]; then
			log "AbyssBSD: reboot (to FreeBSD)"
			ab 'shutdown -r now' > /dev/null
		fi
		sleep 30
	done
}

# Writes AbyssBSD's command line: CMDLINE.noprof and the extra options.
set_cmdline() {
	fb "df /mnt | grep -q kyuawork && umount /mnt; mount -t msdosfs /dev/gpt/abyssefi /mnt && printf %s \"\$(cat /mnt/EFI/ABYSS/CMDLINE.noprof)$1\" > /mnt/EFI/ABYSS/CMDLINE; r=\$?; umount /mnt; exit \$r"
}

restore() {
	if [ "$(which_os)" = freebsd ] || [ "$(which_os)" = freebsd-debug ]; then
		set_cmdline "" && log "AbyssBSD's command line put back"
	else
		log "WARNING: the board is not in FreeBSD: put CMDLINE.noprof back as CMDLINE on abyssefi"
	fi
}
trap restore EXIT

complete() { [ -f "$1" ] && [ "$(grep -c '^kbench end p2$' "$1")" -eq "$NCLASS" ]; }

for run in $(seq 0 $((RUNS - 1))); do
	if [ $((run % 2)) -eq 0 ]; then order="freebsd abyss"; else order="abyss freebsd"; fi
	for side in $order; do
		out=$(printf '%s/%s-%02d.log' "$S" "$side" "$run")
		complete "$out" && continue
		rm -f "$out.tmp"
		start=$(date +%s)
		for class in $CLASSES; do
			serial=
			name=${class%%:*}
			cpus=${class#*:}
			to_freebsd
			log "run $((run + 1))/$RUNS: $side, $name ($cpus)"
			echo "kbench class $name $cpus" >> "$out.tmp"
			if [ $side = freebsd ]; then
				fb 'df /mnt | grep -q kyuawork || { fsck_ffs -p /dev/gpt/kyuawork; mount /dev/gpt/kyuawork /mnt; }'
				T=3600 fb "sh /mnt/perf/kbench-run.sh $cpus" | tr -d '\r' >> "$out.tmp"
			else
				if [ -c "$TTY" ]; then
					set_cmdline " abyss.bench=p2 abyss.bench.reboot abyss.bench.cpus=$cpus" ||
					    { log "could not set AbyssBSD's command line"; exit 1; }
					serial="$out.serial-$name"
					stty -F "$TTY" 115200 raw -echo
					cat "$TTY" > "$serial" &
					cat_pid=$!
					fb 'nextboot -k kernel.nodebug && efibootmgr -n -b 0007 > /dev/null && shutdown -r now' > /dev/null
					# Up to an hour for the boot and the suite.
					for i in $(seq 720); do
						grep -aq '^kbench end p2' "$serial" && break
						sleep 5
					done
					kill $cat_pid
					tr -d '\r' < "$serial" | grep -a '^kbench ' | sed -n '/^kbench begin p2/,/^kbench end p2/p' >> "$out.tmp"
				else
					# No serial: the suite runs, the boot carries on to
					# multi-user, and the kernel log is read over ssh.
					set_cmdline " abyss.bench=p2 abyss.bench.boot abyss.bench.cpus=$cpus" ||
					    { log "could not set AbyssBSD's command line"; exit 1; }
					serial="$out.log-$name"
					fb 'nextboot -k kernel.nodebug && efibootmgr -n -b 0007 > /dev/null && shutdown -r now' > /dev/null
					for i in $(seq 720); do
						sleep 5
						[ "$(T=15 ab 'sysctl -n kern.ident')" = ABYSSBSD ] || continue
						T=30 ab 'dmesg' | tr -d '\r' > "$serial"
						grep -aq '^kbench end p2' "$serial" && break
					done
					grep -a '^kbench ' "$serial" | sed -n '/^kbench begin p2/,/^kbench end p2/p' >> "$out.tmp"
					# Back to FreeBSD, the default boot.
					ab 'shutdown -r now' > /dev/null
				fi
				# The reset follows the suite: wait for FreeBSD before the next.
				sleep 60
			fi
			if [ "$(grep -c '^kbench end p2$' "$out.tmp")" -ne "$(grep -c '^kbench class' "$out.tmp")" ]; then
				log "run $((run + 1)) on $side, $name: no complete suite (see $out.tmp${serial:+, $serial})"
				exit 1
			fi
		done
		mv "$out.tmp" "$out"
		log "run $((run + 1)) on $side done in $(( $(date +%s) - start )) s"
	done
done
log "all $RUNS runs done: cargo xtask perf --platform opi6p --dir $S"
