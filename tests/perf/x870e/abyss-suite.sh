#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# One AbyssBSD in-kernel suite on the X870E, for investigation: installs
# KERNEL on AbyssBSD's root (from stock FreeBSD; the old one stays as
# kernel.prev), boots it with `abyss.bench=SUITE abyss.bench.reboot
# abyss.bench.cpus=...` as p2-x870e.sh does, and prints the suite's kbench
# lines from the log AbyssBSD's loader saves. BootOrder is put back at the
# end. Either OS may be running at the start.
#
# EXTRA adds options to the command line (e.g. EXTRA="abyss.bench.detail").
#
# Usage: abyss-suite.sh WORKDIR SUITE [KERNEL]
set -u
S=${1:?usage: abyss-suite.sh WORKDIR SUITE [KERNEL]}
SUITE=${2:?usage: abyss-suite.sh WORKDIR SUITE [KERNEL]}
KERNEL=${3:-}
B=${ABYSS_BOARD:-192.168.0.16}
CPUS=${P2_CPUS:-0,2,4,6,8,10,12,14,16}
SSHOPT="-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
mkdir -p "$S"
fb() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/freebsd_known_hosts root@$B "$*" 2>/dev/null; }
ab() { timeout "${T:-120}" ssh $SSHOPT -o UserKnownHostsFile=$S/abyss_known_hosts root@$B "$*" 2>/dev/null; }
log() { echo "$(date +%T) $*" >&2; }
which_os() {
	[ "$(T=15 fb 'sysctl -n kern.ident')" = GENERIC ] && { echo freebsd; return; }
	[ "$(T=15 ab 'sysctl -n kern.ident')" = ABYSSBSD ] && { echo abyss; return; }
	echo down
}
boot_to() {
	local want=$1 os tries=0
	while :; do
		os=$(which_os)
		[ "$os" = "$want" ] && return 0
		tries=$((tries + 1))
		[ $tries -gt 60 ] && { log "board not in $want after 60 tries (last: $os)"; exit 1; }
		case $os in
		down) sleep 20; continue ;;
		freebsd) fb "efibootmgr -n -b 0003 >/dev/null; shutdown -r now" >/dev/null ;;
		abyss) ab "efibootmgr -n -b 0002 >/dev/null; shutdown -r now" >/dev/null ;;
		esac
		sleep 45
	done
}
restore() {
	case $(which_os) in
	freebsd) fb 'efibootmgr -B -b 0004 >/dev/null 2>&1; efibootmgr -o 0001,0003,0002 >/dev/null' ;;
	abyss) ab 'efibootmgr -B -b 0004 >/dev/null 2>&1; efibootmgr -o 0001,0003,0002 >/dev/null' ;;
	*) log "WARNING: the board is down: efibootmgr -o 0001,0003,0002" ;;
	esac
}
trap restore EXIT

boot_to freebsd
if [ -n "$KERNEL" ]; then
	scp -q $SSHOPT -o UserKnownHostsFile=$S/freebsd_known_hosts "$KERNEL" root@$B:/root/kernel.new || { log "scp failed"; exit 1; }
	fb '[ "$(geom disk list nda1 | awk "/ident:/{print \$2}")" = 24413C804394 ] && mkdir -p /mnt/aroot && mount /dev/gpt/abyssroot /mnt/aroot && cp -p /mnt/aroot/boot/kernel/kernel /mnt/aroot/boot/kernel/kernel.prev && cp /root/kernel.new /mnt/aroot/boot/kernel/kernel.new && chmod 755 /mnt/aroot/boot/kernel/kernel.new && mv /mnt/aroot/boot/kernel/kernel.new /mnt/aroot/boot/kernel/kernel; r=$?; sync; umount /mnt/aroot; exit $r' || { log "installing the kernel failed"; exit 1; }
fi
fb "mount -t msdosfs /dev/gpt/fbsdefi /mnt && printf %s 'boot_multicons=YES boot_serial=YES abyss.bench=$SUITE abyss.bench.reboot abyss.bench.cpus=$CPUS ${EXTRA:-}' > /mnt/EFI/ABYSS/CMDLINE; r=\$?; efibootmgr | grep -q Boot0004 || efibootmgr -c -a -L 'AbyssBSD bench' -l /mnt/EFI/ABYSS/LOADER.EFI >/dev/null; umount /mnt; exit \$r" || { log "bench setup failed"; exit 1; }
fb 'efibootmgr -o 0003,0004,0002,0001 >/dev/null; efibootmgr | grep -q Boot0004 && efibootmgr -n -b 0004 >/dev/null && shutdown -r now' >/dev/null || { log "no bench boot entry"; exit 1; }
sleep 90
up=
for i in $(seq 360); do
	[ "$(T=15 ab 'sysctl -n kern.ident')" = ABYSSBSD ] && { up=1; break; }
	sleep 10
done
[ -n "$up" ] || { log "AbyssBSD did not come back"; exit 1; }
ab 'mount -t msdosfs -o ro /dev/gpt/abyssefi /mnt && cat /mnt/EFI/ABYSS/PREVLOG.TXT; umount /mnt' | tr -d '\r' | tee "$S/last-$SUITE.log" | grep -a 'kbench '
