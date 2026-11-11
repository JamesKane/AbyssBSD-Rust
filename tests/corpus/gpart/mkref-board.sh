#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# board.img: a disk laid out as the Orange Pi's first NVMe disk, smaller
# (an ESP, an ext4-looking Linux partition, swap, UFS left empty, and a
# vendor partition whose FAT boot sector FreeBSD tastes as an empty MBR),
# made by FreeBSD 15.1's gpart and newfs_msdos on a vnode md disk; then
# what FreeBSD's gpart shows of it. Run on the build server: sh
# mkref-board.sh DIR; keep board.img's nonzero sectors as board.sectors.
set -eu
cd "$1"
doas rm -f board.img board.txt blk magic one s0
truncate -s 256m board.img
md=$(doas mdconfig -a -t vnode -f "$PWD/board.img")
g() { doas gpart "$@" > /dev/null; }
g create -s gpt $md
g add -t efi -s 40m $md
g add -t linux-data -s 64m $md
g add -t freebsd-swap -s 32m $md
g add -t freebsd-ufs -s 64m $md
g add -t '!a04d3adf-b82f-49fd-a235-f347b70355d4' -l vectra $md
doas newfs_msdos -F 32 -c 1 /dev/${md}p1 > /dev/null
dd if=/dev/zero of=blk bs=2048 count=1 2> /dev/null
printf '\123\357' > magic
dd if=magic of=blk bs=1 seek=1080 conv=notrunc 2> /dev/null
doas dd if=blk of=/dev/${md}p2 bs=2048 count=1 2> /dev/null
doas newfs_msdos -F 16 -L VECTRAFS /dev/${md}p5 > /dev/null
doas dd if=/dev/${md}p5 of=s0 bs=512 count=1 2> /dev/null
printf '\001' > one
doas dd if=one of=s0 bs=1 seek=447 conv=notrunc 2> /dev/null
doas dd if=s0 of=/dev/${md}p5 bs=512 count=1 2> /dev/null
for p in $md ${md}p1 ${md}p2 ${md}p3 ${md}p4 ${md}p5; do
	echo "+ gpart show $p"
	gpart show $p 2>&1 || true
done > board.txt
gpart list ${md}p5 | grep -e fwsectors -e fwheads -e first -e last >> board.txt
gpart list $md | grep -e fwsectors -e fwheads >> board.txt
doas mdconfig -d -u ${md#md}
