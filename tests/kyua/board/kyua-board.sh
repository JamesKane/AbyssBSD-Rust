#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# Drives kyua-run.sh on the Orange Pi 6 Plus to the end, across panics,
# from the workstation with the board's serial adapter, then copies the
# run off the board for `cargo xtask kyua-board` (docs/BUILD-AND-TEST.md).
# Usage: kyua-board.sh fbsd16|abyss WORKDIR
# WORKDIR holds the serial log, and the run as run-<system>/ at the end.
# AbyssBSD's host key is kept apart from FreeBSD's, in
# WORKDIR/abyss_known_hosts.
sys=$1
S=${2:?usage: kyua-board.sh fbsd16|abyss WORKDIR}
B=${ABYSS_BOARD:-192.168.0.25}
LOG=$S/kyua-$sys-serial.log
SSHOPT="-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o ServerAliveInterval=15 -o ServerAliveCountMax=4"
mkdir -p "$S"
fb() { timeout 300 ssh $SSHOPT jkane@$B "su root -c '$*' < /dev/null" 2>/dev/null; }
ab() { timeout 300 ssh $SSHOPT -o UserKnownHostsFile=$S/abyss_known_hosts root@$B "$*" 2>/dev/null; }
which_os() {
	if timeout 60 ssh $SSHOPT jkane@$B 'uname -v' 2>/dev/null | grep -q orangepi; then echo fbsd16
	elif ab 'uname -v' | grep -q AbyssBSD; then echo abyss
	else echo down; fi
}
if [ "$sys" = abyss ]; then on() { ab "$@"; }; else on() { fb "$@"; }; fi

stty -F /dev/ttyUSB0 115200 raw -echo 2>/dev/null
cat /dev/ttyUSB0 >> "$LOG" &
serial=$!
trap 'kill $serial' EXIT

down=0
last=""
boots=0
while :; do
	os=$(which_os)
	if [ "$os" = down ]; then
		down=$((down + 1))
		[ $down -eq 1 ] && echo "$(date +%T) board down"
		if [ $down -gt 20 ]; then echo "$(date +%T) board down 20 min: giving up"; exit 1; fi
		sleep 60
		continue
	fi
	[ $down -gt 0 ] && echo "$(date +%T) board up in $os"
	down=0
	if [ "$os" != "$sys" ]; then
		if [ "$sys" = abyss ] && [ "$os" = fbsd16 ]; then
			boots=$((boots + 1))
			[ $boots -gt 20 ] && { echo "20 boots: giving up"; exit 1; }
			echo "$(date +%T) BootNext AbyssBSD (boot $boots)"
			fb 'efibootmgr -n -b 0007 > /dev/null && shutdown -r now' > /dev/null
			sleep 90
			continue
		fi
		echo "$(date +%T) unexpected system $os"; exit 1
	fi
	on 'df /mnt | grep -q kyuawork || { fsck_ffs -p /dev/gpt/kyuawork; mount /dev/gpt/kyuawork /mnt; }'
	if on "test -f /mnt/run-$sys/end"; then
		echo "$(date +%T) done"
		on "tail -1 /mnt/run-$sys/progress.log; cat /mnt/run-$sys/crashed.txt 2>/dev/null"
		rm -rf "$S/run-$sys"
		on "tar -C /mnt -cf - --exclude \"*.db\" run-$sys" | tar -xf - -C "$S" || exit 1
		echo "$(date +%T) the run is in $S/run-$sys"
		exit 0
	fi
	if ! on "test -f /mnt/run-$sys/pid && kill -0 \$(cat /mnt/run-$sys/pid)"; then
		echo "$(date +%T) starting kyua-run.sh $sys"
		on "daemon -o /mnt/run-$sys.daemon.log /mnt/kyua-run.sh $sys"
		sleep 5
	fi
	cur=$(on "grep -v -e \" free \" -e \" running\" /mnt/run-$sys/progress.log | tail -1")
	if [ -n "$cur" ] && [ "$cur" != "$last" ]; then echo "$(date +%T) $cur"; last=$cur; fi
	sleep 60
done
