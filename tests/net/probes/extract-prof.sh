#!/bin/sh
# AbyssBSD under the profiler (`--prof`, `--prof-cpu 1`): /usr/share and
# /usr/include as a tar on tmpfs, extracted onto the root's UFS three
# times by a tar bound to CPU 1, each extraction timed and only the
# extractions profiled (the `rm -rf` between them is not).
umount /mnt 2> /dev/null
mkdir -p /wt && mount -t tmpfs tmpfs /wt
echo "kprof pause"
tar -cf /wt/tree.tar -C / usr/share usr/include 2> /dev/null
ls -l /wt/tree.tar | sed 's/^/probe tar /'
mount | sed 's/^/probe mount /'
for i in 1 2 3; do
	rm -rf /var/tmp/x; mkdir /var/tmp/x; sync
	echo "kprof resume"
	t0=$(date +%s.%N)
	cpuset -l 1 tar -xf /wt/tree.tar -C /var/tmp/x
	t1=$(date +%s.%N)
	echo "kprof pause"
	echo "probe extract $i $(echo "$t1 - $t0" | bc) s"
done
echo "kprof resume"
echo "kbench x.extract 1 ns/op"
echo "kprof pause"
rm -rf /var/tmp/x
shutdown -p now
