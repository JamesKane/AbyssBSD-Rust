#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
#
# Runs the differential scenarios (tests/diff/scenarios/*.sh, or those
# named) and prints each one's normalized output between markers on
# standard output:
#
#   diff: begin NAME
#   diff: | LINE            (normalized, tests/diff/normalize.awk)
#   diff: end NAME
#   diff-raw: | LINE        (as printed, for reading a mismatch)
#   diff: done
#
# FreeBSD 15.1 runs it from rc.local (cargo xtask --remote freebsd-diff),
# AbyssBSD from its boot shell (the shell cases): as root, on the console.
# Each scenario runs in a new empty directory under /var/tmp, under
# script(1), leading a session on a terminal of its own, with a fixed
# environment, host name, umask, and limits.

D=/usr/tests/diff
PATH=/bin:/usr/bin:/sbin:/usr/sbin
export PATH
umask 022
# As init leaves them for rc: hangups and the terminal's job-control stops
# ignored.
trap '' HUP TSTP TTIN TTOU
hostname abyss
# A swap device would add a line to top's header; AbyssBSD has none.
swapoff -a > /dev/null 2>&1

if [ $# -eq 0 ]; then
	set -- $(cd $D/scenarios && ls *.sh | sed 's/\.sh$//')
fi
for s in "$@"; do
	w=/var/tmp/diff.$s
	rm -rf $w
	mkdir -p $w
	cd $w || continue
	# Its input never ends and never comes (an end of file would type
	# ^D at the scenario's terminal).
	mkfifo $w.in
	# The limits of login class `daemon`, which FreeBSD's init gives rc.
	env -i HOME=/root TERM=xterm USER=root LOGNAME=root BLOCKSIZE=K \
	    PATH=/bin:/usr/bin:/sbin:/usr/sbin \
	    limits -C daemon script -q /dev/null /bin/sh -c ". $D/lib.sh; . $D/scenarios/$s.sh" \
	    <> $w.in > $w.raw 2>&1
	# Whatever the scenario left running goes.
	if [ -s .pids ]; then
		kill -9 $(awk '{print $1}' .pids) 2> /dev/null
	fi
	echo "diff: begin $s"
	tr -d '\r' < $w.raw | awk -v pids=$w/.pids -f $D/normalize.awk | sed 's/^/diff: | /'
	echo "diff: end $s"
	tr -d '\r' < $w.raw | sed 's/^/diff-raw: | /'
	cd /
	rm -rf $w $w.raw $w.in
done
echo "diff: done"
