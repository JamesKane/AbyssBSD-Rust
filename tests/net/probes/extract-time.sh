#!/bin/sh
# On AbyssBSD or FreeBSD: /usr/share and /usr/include as a tar on tmpfs,
# extracted onto the root's UFS by a tar bound to CPU 1, timed: three times
# with the tree before it removed and synced first, then three times with
# it removed and the extraction started at once (as suite p5's
# fs.ufs.extract does), to see what the removal leaves for the next
# extraction to wait behind.
umount /mnt 2> /dev/null
mkdir -p /wt && mount -t tmpfs tmpfs /wt
tar -cf /wt/tree.tar -C / usr/share usr/include 2> /dev/null
x() {
	t0=$(date +%s.%N)
	cpuset -l 1 tar -xf /wt/tree.tar -C /var/tmp/x
	t1=$(date +%s.%N)
	echo "probe extract $1 $(echo "$t1 - $t0" | bc) s"
}
mkdir /var/tmp/x
x first
for i in 1 2 3; do
	rm -rf /var/tmp/x; mkdir /var/tmp/x; sync; sleep 2
	x synced$i
done
for i in 1 2 3; do
	rm -rf /var/tmp/x; mkdir /var/tmp/x
	x atonce$i
done
rm -rf /var/tmp/x
shutdown -p now
