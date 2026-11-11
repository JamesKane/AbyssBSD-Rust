#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# The MBR and BSD reference images here: FreeBSD 15.1's gpart, newfs_msdos,
# and newfs on vnode md disks (on the build server, with doas). Run there:
# sh mkref-mbr.sh DIR; keep each image's nonzero sectors as NAME.sectors
# (mbr-bsd's below sector 64), as mkref.sh says.
set -u
dir=$1
mkdir -p "$dir"
cd "$dir"
rm -f *.img *.txt
attach() { truncate -s "$2" "$1.img"; doas mdconfig -a -t vnode -f "$PWD/$1.img"; }
detach() { doas mdconfig -d -u "$1"; }
g() { echo "+ gpart $*" >> "$log"; doas gpart "$@" >> "$log" 2>&1 || echo "exit $?" >> "$log"; }
show() { echo "+ gpart show $*" >> "$log"; gpart show "$@" >> "$log" 2>&1; }

# 1. A USB stick as desktops make it: one FAT32 slice, active, with boot code.
log=$PWD/mbr-fat.txt; md=$(attach mbr-fat 64m)
g create -s mbr $md
g add -t fat32lba $md
g set -a active -i 1 $md
g bootcode -b /boot/mbr $md
doas newfs_msdos -F 32 -c 1 -L STICK /dev/${md}s1 >> $log 2>&1 || echo "exit $?" >> $log
show $md; show -r $md; show -p $md
gpart backup $md >> $log
gpart list $md | grep -v -e Name: >> $log
detach $md

# 2. Several types, explicit places, changes.
log=$PWD/mbr-edits.txt; md=$(attach mbr-edits 32m)
g create -s mbr $md
g add -t fat32 -b 63 -s 4m $md
g add -t ntfs -s 4m $md
g add -t linux-data -i 4 -s 2m $md
g add -t freebsd $md
g add -t '!12' -s 1m $md
g delete -i 2 $md
g add -t ms-basic-data -i 2 -s 1m $md
g set -a active -i 3 $md
g unset -a active -i 3 $md
g set -a active -i 4 $md
g resize -i 4 -s 3m $md
show $md; show -r $md
gpart backup $md >> $log
detach $md

# 3. A FreeBSD install the old way: a freebsd slice with a bsdlabel.
log=$PWD/mbr-bsd.txt; md=$(attach mbr-bsd 64m)
g create -s mbr $md
g add -t freebsd $md
g set -a active -i 1 $md
g create -s bsd ${md}s1
g add -t freebsd-ufs -s 40m ${md}s1
g add -t freebsd-swap -s 8m ${md}s1
g add -t freebsd-ufs ${md}s1
doas newfs -U /dev/${md}s1a >> $log 2>&1 || echo "exit $?" >> $log
show $md; show ${md}s1; show -r ${md}s1
gpart backup ${md}s1 >> $log
gpart list ${md}s1 | grep -v -e Name: >> $log
detach $md

# 4. A bsdlabel on a whole disk ("dangerously dedicated").
log=$PWD/bsd-whole.txt; md=$(attach bsd-whole 16m)
g create -s bsd $md
g add -t freebsd-ufs -s 8m $md
g add -t freebsd-swap $md
show $md
detach $md

# 5. glabel: its metadata in the provider's last sector.
log=$PWD/glabel.txt; md=$(attach glabel 8m)
echo "+ glabel label gctltest $md" >> $log
doas glabel label gctltest $md >> $log 2>&1 || echo "exit $?" >> $log
glabel status | grep gctltest >> $log
glabel list gctltest | grep -v -e Name: >> $log
detach $md

ls -l
