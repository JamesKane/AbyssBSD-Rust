#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# The reference tables in this directory: FreeBSD 15.1's gpart on vnode md
# disks (on the build server, with doas). Run there: sh mkref.sh DIR; then
# keep each DIR/NAME.img's nonzero 512-byte sectors as NAME.sectors (a
# "# NAME.img: BYTES bytes" line, then "SECTOR HEX" lines) and NAME.txt.
set -e
dir=$1
mkdir -p "$dir"
cd "$dir"
rm -f *.img *.txt

attach() { # name size [sectorsize]
	truncate -s "$2" "$1.img"
	if [ -n "$3" ]; then
		doas mdconfig -a -t vnode -S "$3" -f "$PWD/$1.img"
	else
		doas mdconfig -a -t vnode -f "$PWD/$1.img"
	fi
}
detach() { doas mdconfig -d -u "$1"; }
g() { echo "+ gpart $*" >> "$log"; doas gpart "$@" >> "$log" 2>&1 || echo "exit $?" >> "$log"; }
show() { echo "+ gpart show $*" >> "$log"; gpart show "$@" >> "$log" 2>&1; }

# 1. The basic sequence on 64 MiB, with the pmbr and attributes.
log=$PWD/basic.txt; md=$(attach basic 64m)
g create -s gpt $md
g add -t efi -s 40m -l efiboot $md
g add -t freebsd-ufs -l test $md
g set -a bootme -i 2 $md
g set -a bootonce -i 1 $md
g unset -a bootme -i 1 $md
g bootcode -b /boot/pmbr $md
g set -a active $md
show $md; show -l $md; show -r $md; show -p $md
gpart backup $md >> $log
gpart list $md | grep -v -e rawuuid -e efimedia -e Name: >> $log
detach $md

# 2. Few entries, an odd size, explicit places, and changes.
log=$PWD/edits.txt; md=$(attach edits 5000k)
g create -s gpt -n 4 $md
g add -t freebsd-swap -b 100 -s 1000 $md
g add -t freebsd-ufs -a 1m -l data $md
g add -t '!0fc63daf-8483-4772-8e79-3d69d8477de4' -i 4 -s 200 $md
g add -t freebsd-zfs $md
g add -t freebsd-zfs $md
g resize -i 1 -s 500 $md
g modify -i 2 -l 'renamed label' -t freebsd-vinum $md
g delete -i 4 $md
g add -t ms-basic-data -i 3 -s 64 $md
g resize -i 2 $md
g set -a lenovofix $md
show $md; show -l $md
gpart backup $md >> $log
detach $md

# 3. 4096-byte sectors.
log=$PWD/4kn.txt; md=$(attach 4kn 64m 4096)
g create -s gpt $md
g add -t freebsd-boot -s 512k $md
g add -t freebsd-ufs -l root $md
show $md
detach $md

# 4. Destroyed: the first two sectors and the last wiped.
log=$PWD/destroy.txt; md=$(attach destroy 8m)
g create -s gpt $md
g add -t freebsd-ufs $md
g destroy $md
g destroy -F $md
g create -s gpt $md
g destroy $md
detach $md

# 5. Recovered: the backup header gone, then put back.
log=$PWD/recover.txt; md=$(attach recover 8m)
g create -s gpt $md
g add -t freebsd-ufs -s 4m $md
detach $md
dd if=/dev/zero of=recover.img bs=512 seek=16383 count=1 conv=notrunc 2>/dev/null
cp recover.img recover-broken.img
md=$(doas mdconfig -a -t vnode -f "$PWD/recover.img")
show $md
g add -t freebsd-swap $md
g recover $md
show $md
detach $md

# 6. Uncommitted changes, undone; then committed by hand.
log=$PWD/undo.txt; md=$(attach undo 8m)
g create -s gpt $md
g add -t freebsd-ufs -s 1m -f x $md
show $md
gpart list $md | grep -e modified >> $log
g undo $md
g add -t freebsd-ufs -s 2m -l later -f x $md
g commit $md
show -l $md
detach $md

ls -l
