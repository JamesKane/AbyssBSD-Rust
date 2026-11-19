#!/bin/sh
# AbyssBSD under the profiler (`--prof`): a 64 MiB file rewritten in place
# in 64 KiB writes on tmpfs, then on the root's UFS, then rewritten from
# empty on UFS; each its own report.
umount /mnt 2> /dev/null
mkdir -p /wt && mount -t tmpfs tmpfs /wt
echo "kprof pause"
run() {
	name=$1 file=$2 conv=$3
	dd if=/dev/zero of=$file bs=64k count=1024 2> /dev/null
	echo "kprof resume"
	for i in 1 2 3 4 5 6 7 8; do
		dd if=/dev/zero of=$file bs=64k count=1024 $conv 2> /dev/null
	done
	echo "kbench $name 1 ns/op"
	echo "kprof pause"
}
run w.tmpfs.inplace /wt/f conv=notrunc
run w.ufs.inplace /var/tmp/f conv=notrunc
run w.ufs.fresh /var/tmp/f ""
shutdown -p now
