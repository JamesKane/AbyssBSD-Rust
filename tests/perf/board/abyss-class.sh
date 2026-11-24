#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# One AbyssBSD p2 boot per core class on the Orange Pi 6 Plus, for
# investigation (no serial console needed; p2-board.sh's AbyssBSD half):
# installs KERNEL on abyssroot (the old one stays as kernel.prev), boots
# it with `abyss.bench=p2 abyss.bench.boot abyss.bench.cpus=...`, reads the
# kbench lines from the kernel log over ssh, and lets the board reset into
# FreeBSD. Starts from FreeBSD.
#
# Usage: abyss-class.sh WORKDIR KERNEL CLASS...   (CLASS = name:cpus, as in p2-board.sh)
set -u
S=${1:?usage: abyss-class.sh WORKDIR KERNEL CLASS...}
KERNEL=${2:?}
shift 2
B=${ABYSS_BOARD:-192.168.0.19}
SSHOPT="-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
mkdir -p "$S"
fb() { timeout "${T:-120}" ssh $SSHOPT jkane@$B "su root -c '$*' < /dev/null" 2>/dev/null; }
ab() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/abyss_known_hosts root@$B "$*" 2>/dev/null; }
log() { echo "$(date +%T) $*" >&2; }
kind() { timeout 30 ssh $SSHOPT jkane@$B 'sysctl -n kern.ident' 2>/dev/null; }
wait_freebsd() {
	for i in $(seq 120); do
		case $(kind) in *NODEBUG*) return 0 ;; GENERIC*) fb 'nextboot -k kernel.nodebug && shutdown -r now' >/dev/null ;; esac
		sleep 20
	done
	return 1
}
wait_freebsd || { log "board not in FreeBSD"; exit 1; }
# Install the kernel (never via /tmp: that is the Crucial).
fb 'mount /dev/gpt/abyssroot /mnt && cp -p /mnt/boot/kernel/kernel /mnt/boot/kernel/kernel.prev' || { log "mount failed"; exit 1; }
ssh $SSHOPT jkane@$B "su root -c 'cat > /mnt/boot/kernel/kernel.new'" < "$KERNEL" &&
	fb 'mv /mnt/boot/kernel/kernel.new /mnt/boot/kernel/kernel && sync && umount /mnt' || { log "install failed"; exit 1; }
cmdline() {
	fb "mount -t msdosfs /dev/gpt/abyssefi /mnt && printf %s \"\$(cat /mnt/EFI/ABYSS/CMDLINE.noprof)$1\" > /mnt/EFI/ABYSS/CMDLINE; r=\$?; umount /mnt; exit \$r"
}
trap 'wait_freebsd && cmdline ""' EXIT
for class in "$@"; do
	name=${class%%:*}
	cpus=${class#*:}
	wait_freebsd || { log "board not in FreeBSD"; exit 1; }
	cmdline " abyss.bench=p2 abyss.bench.boot abyss.bench.cpus=$cpus" || exit 1
	log "$name ($cpus)"
	fb 'nextboot -k kernel.nodebug && efibootmgr -n -b 0007 > /dev/null && shutdown -r now' >/dev/null
	out=$S/$name.log
	for i in $(seq 120); do
		sleep 5
		[ "$(T=15 ab 'sysctl -n kern.ident')" = ABYSSBSD ] || continue
		T=30 ab 'dmesg' | tr -d '\r' > "$out"
		grep -aq '^kbench end p2' "$out" && break
	done
	echo "== $name"
	grep -a '^kbench ' "$out" | sed -n '/^kbench begin p2/,/^kbench end p2/p'
	ab 'shutdown -r now' >/dev/null
	sleep 45
done
