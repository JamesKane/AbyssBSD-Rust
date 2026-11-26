#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# The in-kernel suite p2 on the X870E AORUS (Ryzen 9 7900X), from the
# workstation: RUNS runs on each side, alternating which goes first.
# FreeBSD's side is stock 15.1 on the media NVMe's second half, running
# kbench.ko (kbench-run.sh); AbyssBSD's is a boot with
# `abyss.bench=p2 abyss.bench.reboot abyss.bench.cpus=...`. The board has
# no serial console, so the suite's output comes from the log AbyssBSD's
# loader saves (EFI/ABYSS/PREVLOG.TXT on its ESP) at the next boot:
#
#   - the bench boot is its own boot entry (Boot0004: a copy of the loader
#     and a command line of its own on stock FreeBSD's ESP, so AbyssBSD's
#     normal command line is never changed);
#   - the reset after the suite goes to the first entry of BootOrder,
#     which for the runs is AbyssBSD's own (Boot0003): it saves the log and
#     boots to a login, where ssh finds it.
#
# BootOrder is set for the runs (AbyssBSD, stock, Fedora) and put back to
# Fedora, AbyssBSD, stock at the end. Each run's output goes to
# WORKDIR/<side>-<run>.log, in perf-board.sh's form, so
# `cargo xtask perf --platform x870e --dir WORKDIR` makes the report.
# Rerun with the same WORKDIR to resume: runs with a complete log are kept.
#
# With P2_PIN_MHZ=3000 (and P2_PIN_PERF=88, CPPC's level for 3000 MHz on
# this part) both sides run at that fixed clock, below the boost range, so
# the comparison is not between two different boost behaviors.
#
# Usage: p2-x870e.sh WORKDIR RUNS
set -u
S=${1:?usage: p2-x870e.sh WORKDIR RUNS}
RUNS=${2:?usage: p2-x870e.sh WORKDIR RUNS}
B=${ABYSS_BOARD:-192.168.0.16}
HERE=$(cd "$(dirname "$0")" && pwd)
# The coordinator's CPU, then the workers': one thread of each of eight
# cores (SMT siblings are 2n and 2n+1 on both kernels).
CPUS=${P2_CPUS:-0,2,4,6,8,10,12,14,16}
SSHOPT="-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
mkdir -p "$S"
fb() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/freebsd_known_hosts root@$B "$*" 2>/dev/null; }
ab() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/abyss_known_hosts root@$B "$*" 2>/dev/null; }
log() { echo "$(date +%T) $*"; }

# freebsd, abyss, or down.
which_os() {
	case $(T=15 fb 'sysctl -n kern.ident') in
	GENERIC) echo freebsd; return ;;
	esac
	if [ "$(T=15 ab 'sysctl -n kern.ident')" = ABYSSBSD ]; then echo abyss; else echo down; fi
}

# Brings the board up in `want`, rebooting as needed (BootNext does the
# choosing; a boot of the other side sets it).
boot_to() {
	local want=$1 os tries=0
	while :; do
		os=$(which_os)
		[ "$os" = "$want" ] && return 0
		tries=$((tries + 1))
		[ $tries -gt 60 ] && { log "board not in $want after 60 tries (last: $os)"; exit 1; }
		case $os in
		down) sleep 20; continue ;;
		freebsd) fb "efibootmgr -n -b $([ "$want" = abyss ] && echo 0003 || echo 0002) >/dev/null; shutdown -r now" >/dev/null ;;
		abyss) ab "efibootmgr -n -b $([ "$want" = freebsd ] && echo 0002 || echo 0003) >/dev/null; shutdown -r now" >/dev/null ;;
		esac
		sleep 45
	done
}

restore() {
	if [ "$(which_os)" != down ]; then
		case $(which_os) in
		freebsd) fb 'efibootmgr -B -b 0004 >/dev/null 2>&1; efibootmgr -o 0001,0003,0002 >/dev/null' ;;
		abyss) ab 'efibootmgr -B -b 0004 >/dev/null 2>&1; efibootmgr -o 0001,0003,0002 >/dev/null' ;;
		esac && log "BootOrder put back: Fedora, AbyssBSD, stock"
	else
		log "WARNING: the board is down: put BootOrder back to 0001,0003,0002 (efibootmgr -o)"
	fi
}
trap restore EXIT

boot_to freebsd
fb "mkdir -p /root/perf"
for f in kbench-run.sh; do scp -q $SSHOPT -o UserKnownHostsFile=$S/freebsd_known_hosts "$HERE/$f" root@$B:/root/perf/; done
[ -n "${KBENCH_KO:-}" ] && scp -q $SSHOPT -o UserKnownHostsFile=$S/freebsd_known_hosts "$KBENCH_KO" root@$B:/root/perf/kbench.ko
fb 'test -f /root/perf/kbench.ko' || { log "no kbench.ko on the board: set KBENCH_KO"; exit 1; }
# After a suite AbyssBSD resets: its own entry first.
# The firmware drops a boot option that is not in BootOrder, so the bench
# entry (Boot0004) stays in it, behind AbyssBSD's own; ensure_bench_entry
# makes it again if it is gone.
ORDER=0003,0004,0002,0001
ensure_bench_entry() {
	fb "efibootmgr | grep -q 'Boot0004' || { mount -t msdosfs /dev/gpt/fbsdefi /mnt && efibootmgr -c -a -L 'AbyssBSD bench' -l /mnt/EFI/ABYSS/LOADER.EFI >/dev/null; umount /mnt; }; efibootmgr -o $ORDER >/dev/null; efibootmgr | grep -q 'Boot0004'"
}

complete() { [ -f "$1" ] && [ "$(grep -c '^kbench end p2$' "$1")" -eq 1 ]; }

for run in $(seq 0 $((RUNS - 1))); do
	if [ $((run % 2)) -eq 0 ]; then order="freebsd abyss"; else order="abyss freebsd"; fi
	for side in $order; do
		out=$(printf '%s/%s-%02d.log' "$S" "$side" "$run")
		complete "$out" && continue
		rm -f "$out.tmp"
		start=$(date +%s)
		boot_to freebsd
		log "run $((run + 1))/$RUNS: $side"
		if [ $side = freebsd ]; then
			echo "kbench class workers $CPUS" >> "$out.tmp"
			T=7200 fb "P2_PIN_MHZ=${P2_PIN_MHZ:-} sh /root/perf/kbench-run.sh $CPUS /root/perf/kbench.ko" | tr -d '\r' | sed 's/^kbench env /kbench env side=freebsd /' >> "$out.tmp"
		else
			fb "mount -t msdosfs /dev/gpt/fbsdefi /mnt && printf %s 'boot_multicons=YES boot_serial=YES abyss.bench=p2 abyss.bench.reboot abyss.bench.cpus=$CPUS${P2_PIN_PERF:+ abyss.bench.pin_perf=$P2_PIN_PERF}' > /mnt/EFI/ABYSS/CMDLINE; r=\$?; umount /mnt; exit \$r" ||
			    { log "could not set the bench command line"; exit 1; }
			ensure_bench_entry || { log "no bench boot entry"; exit 1; }
			fb 'efibootmgr -n -b 0004 >/dev/null && shutdown -r now' > /dev/null
			sleep 90
			# The suite, then the reset into AbyssBSD's own entry: up to
			# two hours.
			up=
			for i in $(seq 720); do
				[ "$(T=15 ab 'sysctl -n kern.ident')" = ABYSSBSD ] && { up=1; break; }
				sleep 10
			done
			[ -n "$up" ] || { log "AbyssBSD did not come back after its suite"; exit 1; }
			echo "kbench class workers $CPUS" >> "$out.tmp"
			ab "echo \"kbench env side=abyss ident=\$(sysctl -n kern.ident) freq=\$(sysctl -n dev.cpu.0.freq)\"" >> "$out.tmp"
			ab 'mount -t msdosfs -o ro /dev/gpt/abyssefi /mnt && cat /mnt/EFI/ABYSS/PREVLOG.TXT; umount /mnt' |
				tr -d '\r' | grep -a 'kbench ' | sed 's/^.*\(kbench \)/\1/' | sed -n '/^kbench begin p2/,/^kbench end p2/p' >> "$out.tmp"
		fi
		if [ "$(grep -c '^kbench end p2$' "$out.tmp")" -ne 1 ]; then
			log "run $((run + 1)) on $side: no complete suite (see $out.tmp)"
			exit 1
		fi
		mv "$out.tmp" "$out"
		log "run $((run + 1)) on $side done in $(( $(date +%s) - start )) s"
	done
done
log "all $RUNS runs done: cargo xtask perf --platform x870e --dir $S"
