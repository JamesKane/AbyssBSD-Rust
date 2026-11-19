#!/bin/sh
# On AbyssBSD or FreeBSD: how fast a file's pages take data, on tmpfs and
# on the root's UFS, rewriting 64 MiB five times each in 64 KiB writes
# (`dd`'s own rate lines), with a read back for the copy the other way.
umount /mnt 2> /dev/null
mkdir -p /wt && mount -t tmpfs tmpfs /wt
df -h / /wt | sed 's/^/probe df /'
for d in /wt /var/tmp; do
	for i in 1 2 3 4 5; do
		r=$(dd if=/dev/zero of=$d/f bs=64k count=1024 2>&1 | awk '/bytes/ { print $(NF-1) }' | tr -d '(')
		echo "probe write $d $r bytes/s"
	done
	r=$(dd if=$d/f of=/dev/null bs=64k 2>&1 | awk '/bytes/ { print $(NF-1) }' | tr -d '(')
	echo "probe read $d $r bytes/s"
	r=$(dd if=/dev/zero of=$d/f bs=64k count=1024 conv=notrunc 2>&1 | awk '/bytes/ { print $(NF-1) }' | tr -d '(')
	echo "probe rewrite-in-place $d $r bytes/s"
	rm -f $d/f
done
sysctl hw.physmem vm.stats.vm.v_free_count 2>&1 | sed 's/^/probe /'
shutdown -p now
