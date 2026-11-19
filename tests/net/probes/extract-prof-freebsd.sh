#!/bin/sh
# FreeBSD's side of extract-prof.sh: the same tar extracted onto the
# root's UFS three times by a tar bound to CPU 1, each timed, with DTrace's
# samples of CPU 1 by kernel function over the extractions only, and of
# every thread by name.
umount /mnt 2> /dev/null
mkdir -p /wt && mount -t tmpfs tmpfs /wt
tar -cf /wt/tree.tar -C / usr/share usr/include 2> /dev/null
ls -l /wt/tree.tar | sed 's/^/probe tar /'
mount | sed 's/^/probe mount /'
kldload dtraceall 2> /dev/null
: > /tmp/on
dtrace -q -x aggsize=8m -n 'BEGIN { on = 0; }
    syscall::execve:return /execname == "tar"/ { on = 1; }
    proc:::exit /execname == "tar"/ { on = 0; }
    profile-4999 /on && cpu == 1 && arg0 != 0/ { @fn[func(arg0)] = count(); @k = count(); }
    profile-4999 /on && cpu == 1 && arg0 == 0/ { @u = count(); }
    profile-4999 /on/ { @td[stringof(curthread->td_name), cpu] = count(); }
    tick-1s /done/ { exit(0); }
    END { trunc(@fn, 70); printa("probe fbsd kern-samples %@d\n", @k); printa("probe fbsd user-samples %@d\n", @u);
    printa("probe fbsd td %s cpu%d %@d\n", @td); printa("probe fbsd fn %@8d %a\n", @fn); }' -p $$ > /tmp/dt.out 2>&1 &
dt=$!
sleep 3
for i in 1 2 3; do
	rm -rf /var/tmp/x; mkdir /var/tmp/x; sync
	t0=$(date +%s.%N)
	cpuset -l 1 tar -xf /wt/tree.tar -C /var/tmp/x
	t1=$(date +%s.%N)
	echo "probe extract $i $(echo "$t1 - $t0" | bc) s"
done
kill -INT $dt; wait $dt
cat /tmp/dt.out
rm -rf /var/tmp/x
shutdown -p now
