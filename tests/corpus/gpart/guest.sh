# SPDX-License-Identifier: BSD-2-Clause
# FreeBSD's gpart on a memory disk, malloc then vnode: create, add,
# show, newfs and mount, what is refused while mounted, resize, modify,
# undo, delete, destroy, bootcode, backup and restore. The kernel test
# runs it from /rescue in the guest (T=/mnt, a tmpfs) and compares what
# it prints with guest-freebsd.txt, which FreeBSD 15.1 printed: run
# there as root, T=DIR sh guest.sh.
# It leaves disk.img with a table and a file system.
T=${T:-/mnt}
cd $T || exit 1
U=47
md=md$U
run() {
	echo "+ $*"
	"$@" 2>&1
	echo "= $?"
}
quiet() {
	echo "+ $*"
	"$@" > /dev/null 2>&1
	echo "= $?"
}
phases() {
	echo "+ fsck_ffs -n $1"
	fsck_ffs -n "$1" 2>&1 | sed -n -e '/^\*\* Phase/p' -e '/MODIFIED/p' -e '/UNREF/p'
}
labels() {
	echo "+ ls /dev/gpt/gctl-*"
	for f in /dev/gpt/gctl-*; do echo "${f#/dev/gpt/}"; done
}
sequence() {
	run gpart create -s gpt $md
	run gpart create -s gpt $md
	run gpart add -t efi -s 40m -l gctl-efi $md
	run gpart add -t bogus -s 1m $md
	run gpart add -t freebsd-ufs -s 16m -l gctl-test $md
	run gpart add -t freebsd-ufs -b 100 -s 1m $md
	run gpart set -a bootme -i 2 $md
	run gpart show $md
	run gpart show -l $md
	run gpart show -p $md
	run gpart show -r $md
	run gpart backup $md
	labels
	quiet newfs -U /dev/gpt/gctl-test
	mkdir -p m
	run mount /dev/gpt/gctl-test m
	echo hello > m/f
	run gpart delete -i 2 $md
	run gpart resize -i 2 -s 8m $md
	run gpart resize -i 2 $md
	run gpart show $md
	quiet dd if=/dev/$md of=/dev/$md count=1
	run sysctl kern.geom.debugflags=16
	quiet dd if=/dev/$md of=/dev/$md count=1
	run sysctl kern.geom.debugflags=0
	run umount m
	phases /dev/gpt/gctl-test
	run gpart resize -i 2 -s 16m $md
	run gpart modify -i 2 -l gctl-renamed -t freebsd-zfs $md
	labels
	run gpart modify -i 2 -t freebsd-ufs $md
	run mount -o ro /dev/gpt/gctl-renamed m
	run cat m/f
	run umount m
	run gpart add -t freebsd-swap -s 1m -f x $md
	run gpart show -l $md
	run gpart undo $md
	run gpart show -l $md
	run gpart commit $md
	run gpart unset -a bootme -i 2 $md
	run gpart delete -i 1 $md
	run gpart destroy $md
	run gpart destroy -F $md
	run gpart show $md
}
run mdconfig -a -t malloc -s 64m -u $U
sequence
run mdconfig -d -u $U
quiet dd if=/dev/zero of=disk.img bs=1m count=0 seek=64
run mdconfig -a -t vnode -f disk.img -u $U
sequence
run gpart create -s gpt $md
run gpart add -t efi -s 1m -l gctl-esp $md
run gpart add -t freebsd-ufs -s 8m -l gctl-keep $md
printf 'ABYSS-GCTL-BOOT' > pmbr
run gpart bootcode -b pmbr $md
run gpart bootcode -p pmbr -i 1 $md
quiet newfs -U /dev/gpt/gctl-keep
gpart backup $md > bk
run cat bk
run gpart destroy -F $md
run gpart restore -l $md < bk
run gpart show -l $md
run mdconfig -d -u $U
run mdconfig -a -t vnode -f disk.img -u $U
run gpart show -l $md
phases /dev/gpt/gctl-keep
run mdconfig -d -u $U
