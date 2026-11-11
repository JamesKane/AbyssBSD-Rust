#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# kyua's suite on the Orange Pi 6 Plus, one directory at a time, resumable
# across panics (the same loop as xtask/src/kyua.rs's rc.local).
# Usage: kyua-run.sh <system tag>; kyuawork mounted at /mnt.
# Everything it writes is on kyuawork (Samsung): results, and kyua's work
# directories (TMPDIR).
sys=$1
w=/mnt
out=$w/run-$sys
PAR=12
PATH=/sbin:/bin:/usr/sbin:/usr/bin
export PATH
mkdir -p $out $w/tmp-$sys
chmod 1777 $w/tmp-$sys
TMPDIR=$w/tmp-$sys
export TMPDIR
mount -p | awk '$2 == "/usr/tests"' | grep -q . || mount -t nullfs $w/usr/tests /usr/tests || exit 1
sysctl debug.debugger_on_panic=0 > /dev/null 2>&1
kldload -n if_epair > /dev/null 2>&1
cd /usr/tests || exit 1
say() { echo "kyua-run: $*" > /dev/console; echo "$(date +%s) $*" >> $out/progress.log; }
conf=$w/kyua.conf
if [ ! -f $out/chunks.txt ]; then
	say "listing"
	kyua --config=$conf list -k /usr/tests/Kyuafile > $out/list.txt 2> $out/list.err
	cut -d: -f1 $out/list.txt | cut -d/ -f1-2 | sort -u > $out/chunks.all
	if [ -f $out/only.txt ]; then
		while read p; do
			if grep -qE "^$p(/|\$)" $out/chunks.all; then
				grep -E "^$p(/|\$)" $out/chunks.all
			elif grep -qE "^$p(:|/|\$)" $out/list.txt; then
				echo "$p"
			fi
		done < $out/only.txt | sort -u > $out/chunks.tmp
	else
		cp $out/chunks.all $out/chunks.tmp
	fi
	mv $out/chunks.tmp $out/chunks.txt
	date +%s > $out/start
	sync
fi
n=$(wc -l < $out/chunks.txt | tr -d ' ')
i=0
echo $$ > $out/pid
(while :; do
	sleep 30
	say "free $(sysctl -n vm.stats.vm.v_free_count) pages"
	say "running" $(ps -axo command= | grep -o '^/usr/tests/[^ ]*' | sort -u)
done) < /dev/null > /dev/null 2>&1 &
watcher=$!
trap 'kill $watcher' EXIT
while read c; do
	i=$((i + 1))
	f=$(echo "$c" | tr / _)
	[ -f "$out/$f.report" ] && continue
	if [ -f "$out/$f.started" ]; then
		say "crashed in $c"
		echo "$c" >> $out/crashed.txt
		grep -a ' ->  ' "$out/$f.out" > "$out/$f.report"
		rm "$out/$f.started"
		sync
		continue
	fi
	say "$i/$n $c"
	: > "$out/$f.started"
	sync
	kyua --config=$conf --logfile=$out/kyua.log -v parallelism=$PAR test -k /usr/tests/Kyuafile \
	    -r "$out/$f.db" "$c" < /dev/null > "$out/$f.out" 2>&1
	kyua report -r "$out/$f.db" \
	    --results-filter=passed,skipped,xfail,broken,failed \
	    > "$out/$f.tmp" 2>&1 < /dev/null
	kyua report -r "$out/$f.db" --verbose --results-filter=broken,failed \
	    > "$out/$f.failed" 2>&1 < /dev/null
	mv "$out/$f.tmp" "$out/$f.report"
	rm "$out/$f.started"
	sync
done < $out/chunks.txt
date +%s > $out/end
sync
say "done"
