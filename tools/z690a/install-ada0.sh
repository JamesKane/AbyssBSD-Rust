#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# AbyssBSD on the build server's Intel SSD (ada0, the MSI PRO Z690-A box).
#
# The disk held an old Windows install, imaged to the NAS first (see
# tools/nas/image-disk.sh). This puts a GPT on it with a 512 MiB ESP
# (`abyssefi`) and a 100 GiB UFS2 root (`abyssroot`), copies the disk image
# `cargo xtask --remote image --arch x86_64 --release --root installed` made
# into them, and adds a boot entry that stays out of BootOrder: AbyssBSD
# boots only through BootNext, and FreeBSD stays the default. The other
# disks (nda0: Windows, nda1: FreeBSD's ZFS) are never touched, and the
# labels differ from the image's (`efiboot0`, `rootfs0`) because FreeBSD's
# own ESP here is `efiboot0`.
#
# Modes (all work on the server over ssh, with doas):
#   --prepare             a copy of the image with its fstab naming the new
#                         labels and its command line turning the Radeon
#                         driver's display card and debug triggers off (the
#                         Navi 22 is not supported yet), in ~/abyss/z690a:
#                         nothing is written to any disk. Run again after a
#                         new image.
#   --install --wipe S    DESTRUCTIVE: ada0 loses what it holds. S must be the
#                         disk's serial number, as a check that you meant it.
#   --entry               the boot entry for ada0's ESP, outside BootOrder (--install
#                         runs it last; safe to run again).
#   --update-kernel       the server's newest release kernel (from its last
#                         build) onto the installed root, the old one kept as
#                         kernel.prev: no other disk or partition changes.
#   --board-files         what `image --root board` adds, onto the installed root:
#                         DHCP on every interface, sshd with root let in by key
#                         only, and ~/.ssh/id_ed25519.pub (or $SSH_PUB) as root's
#                         authorized_keys. Safe to run again.
#   --back                from the running AbyssBSD (ssh as root): BootNext to
#                         FreeBSD's entry, then reboot. The firmware's default
#                         is the Windows Boot Manager on nda0, not FreeBSD, so
#                         a plain reboot from AbyssBSD lands in Windows.
#   --boot-once           BootNext to the AbyssBSD entry. It does not reboot.
#   --status              what is there: the partitions, the boot entries.
# Add --dry-run to --install to see its preflight and plan only.
set -eu
disk=ada0
serial=BTDA3330088F2403GN
esp_size=512m
root_size=100g
work='$HOME/abyss/z690a'
image='$HOME/abyss/src/build/x86_64/disk-installed.img'
label='AbyssBSD (Intel SSD)'
mode=
wipe=
dry=0
while [ $# -gt 0 ]; do
	case $1 in
	--prepare | --install | --entry | --update-kernel | --board-files | --boot-once | --back | --back | --status) mode=${1#--} ;;
	--wipe) wipe=$2; shift ;;
	--dry-run) dry=1 ;;
	-h | --help) sed -n '2,/^set -eu/p' "$0" | sed 's/^# \{0,1\}//; /^set -eu/d'; exit 0 ;;
	*) echo "unknown option $1" >&2; exit 2 ;;
	esac
	shift
done
[ -n "$mode" ] || { echo "usage: $0 --prepare | --install --wipe SERIAL [--dry-run] | --entry | --update-kernel | --board-files | --boot-once | --status" >&2; exit 2; }

srv() { ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 abyss-build "$@"; }
say() { echo "$(date +%T) $*"; }
die() { echo "install-ada0: $*" >&2; exit 1; }

case $mode in
status)
	srv "doas -n gpart show -p -l $disk; echo; doas -n efibootmgr -v | head -12; ls -la $work 2> /dev/null"
	;;

prepare)
	say "preparing the image copy on the server"
	srv "sh -s" << EOF
set -eu
mkdir -p $work
img=$image
[ -f \$img ] || { echo "no image at \$img: cargo xtask --remote image --arch x86_64 --release --root installed" >&2; exit 1; }
cp \$img $work/disk-z690a.img
md=\$(doas -n mdconfig -a -t vnode -f $work/disk-z690a.img)
trap 'doas -n umount $work/mnt 2> /dev/null || true; doas -n umount $work/esp 2> /dev/null || true; doas -n mdconfig -d -u \${md#md} 2> /dev/null || true' EXIT
mkdir -p $work/mnt $work/esp
doas -n mount /dev/\${md}p2 $work/mnt
doas -n sed -i '' -e 's#/dev/gpt/rootfs0#/dev/gpt/abyssroot#' -e 's#/dev/gpt/efiboot0#/dev/gpt/abyssefi#' $work/mnt/etc/fstab
cat $work/mnt/etc/fstab
doas -n umount $work/mnt
# This box's GPU is a Navi 22, which the Radeon driver's display card was not
# built for (its register code is the X870E's Raphael); probing it has reset
# this server before. So no display card from it and no debug triggers.
doas -n mount -t msdosfs /dev/\${md}p1 $work/esp
cl=\$(cat $work/esp/EFI/ABYSS/CMDLINE)
printf '%s hw.drmn.kms=0 hw.drmn.debug=0' "\$cl" | doas -n tee $work/esp/EFI/ABYSS/CMDLINE > /dev/null
echo "command line: \$(cat $work/esp/EFI/ABYSS/CMDLINE)"
doas -n umount $work/esp
# What goes onto the disk, and its hashes, from the image's own devices.
for p in p1 p2; do
	doas -n diskinfo /dev/\${md}\$p | awk -v p=\$p '{print p, \$3}' >> $work/sizes.new
done
for p in p1 p2; do
	echo "\$p \$(doas -n dd if=/dev/\${md}\$p bs=1m 2> /dev/null | sha256 -q)" >> $work/sums.new
done
mv $work/sizes.new $work/sizes
mv $work/sums.new $work/sums
cat $work/sizes $work/sums
EOF
	say "prepared: ~/abyss/z690a/disk-z690a.img (sizes and hashes beside it)"
	;;

install)
	say "preflight on the server"
	srv "sh -s" << EOF
set -eu
got=\$(doas -n camcontrol identify $disk | sed -n 's/^serial number[[:space:]]*//p' | head -1 | tr -d '[:space:]')
[ "\$got" = "$serial" ] || { echo "$disk has serial '\$got', not $serial" >&2; exit 1; }
if mount | grep -q '/dev/$disk'; then echo "something on $disk is mounted" >&2; exit 1; fi
if swapinfo | grep -q '/dev/$disk'; then echo "$disk is a swap device" >&2; exit 1; fi
[ -f $work/disk-z690a.img ] && [ -f $work/sums ] || { echo "no prepared image: run --prepare" >&2; exit 1; }
echo "$disk is $serial, not in use; the image is prepared:"
cat $work/sizes
doas -n gpart show -p $disk
echo "boot entries now:"
doas -n efibootmgr | head -6
EOF
	if [ "$dry" = 1 ]; then
		say "dry run: would destroy the table of $disk; make a GPT with $esp_size abyssefi and $root_size abyssroot; copy the image's two partitions; grow the root; check both; add the boot entry '$label' outside BootOrder"
		exit 0
	fi
	[ "$wipe" = "$serial" ] || die "this destroys what $disk holds: pass --wipe $serial"
	say "writing: this is the point of no return for $disk"
	srv "sh -s" << EOF
set -eu
md=\$(doas -n mdconfig -a -t vnode -o readonly -f $work/disk-z690a.img)
trap 'doas -n mdconfig -d -u \${md#md} 2> /dev/null || true' EXIT
doas -n gpart destroy -F $disk
doas -n gpart create -s gpt $disk
doas -n gpart add -t efi -s $esp_size -l abyssefi $disk
doas -n gpart add -t freebsd-ufs -s $root_size -l abyssroot $disk
for p in p1 p2; do
	size=\$(awk -v p=\$p '\$1 == p {print \$2}' $work/sizes)
	doas -n dd if=/dev/\${md}\$p of=/dev/${disk}\$p bs=1m 2> /dev/null
	want=\$(awk -v p=\$p '\$1 == p {print \$2}' $work/sums)
	have=\$(doas -n dd if=/dev/${disk}\$p bs=1m 2> /dev/null | head -c \$size | sha256 -q)
	[ "\$want" = "\$have" ] || { echo "\$p on $disk differs from the image" >&2; exit 1; }
	echo "\$p copied and verified (\$size bytes)"
done
doas -n growfs -y /dev/${disk}p2
doas -n fsck_ffs -f -n /dev/${disk}p2
EOF
	say "installed; now its boot entry"
	exec "$0" --entry
	;;

entry)
	srv "sh -s" << EOF
set -eu
order=\$(doas -n efibootmgr | sed -n 's/^BootOrder *: *//p' | tr -d ' ')
echo "BootOrder is: \$order"
trap 'doas -n umount /mnt/abyssefi-z690a 2> /dev/null || true; doas -n rmdir /mnt/abyssefi-z690a 2> /dev/null || true' EXIT
doas -n mkdir -p /mnt/abyssefi-z690a
doas -n mount -t msdosfs -o ro /dev/${disk}p1 /mnt/abyssefi-z690a
ls /mnt/abyssefi-z690a/EFI/BOOT/BOOTX64.EFI /mnt/abyssefi-z690a/EFI/ABYSS/CMDLINE
doas -n umount /mnt/abyssefi-z690a
if doas -n efibootmgr -v | grep -q '$label'; then
	echo "the entry '$label' is already there"
else
	doas -n mount -t msdosfs /dev/${disk}p1 /mnt/abyssefi-z690a
	doas -n efibootmgr -c -a -L '$label' -l /mnt/abyssefi-z690a/EFI/BOOT/BOOTX64.EFI
	doas -n umount /mnt/abyssefi-z690a
	# A new entry may be put first in BootOrder: put the order back as it was.
	doas -n efibootmgr -o "\$order"
fi
doas -n efibootmgr -v | head -14
EOF
	say "BootOrder is as it was; --boot-once sets BootNext (it does not reboot)"
	;;

update-kernel)
	srv "sh -s" << EOF
set -eu
k=\$HOME/abyss/src/target/x86_64-unknown-none/release/kernel
[ -f \$k ] || { echo "no kernel at \$k: cargo xtask --remote build --arch x86_64 --release" >&2; exit 1; }
ls -l \$k
trap 'doas -n umount /mnt/abyssroot-z690a 2> /dev/null || true; doas -n rmdir /mnt/abyssroot-z690a 2> /dev/null || true' EXIT
doas -n mkdir -p /mnt/abyssroot-z690a
doas -n mount /dev/gpt/abyssroot /mnt/abyssroot-z690a
if [ -f /mnt/abyssroot-z690a/boot/kernel/kernel ]; then
	doas -n cp -p /mnt/abyssroot-z690a/boot/kernel/kernel /mnt/abyssroot-z690a/boot/kernel/kernel.prev
fi
doas -n cp \$k /mnt/abyssroot-z690a/boot/kernel/kernel
doas -n sync
echo "installed: \$(sha256 -q /mnt/abyssroot-z690a/boot/kernel/kernel) (the build's: \$(sha256 -q \$k))"
doas -n umount /mnt/abyssroot-z690a
EOF
	say "the kernel is on abyssroot; BootNext is unchanged (--boot-once sets it)"
	;;

board-files)
	pub=${SSH_PUB:-$HOME/.ssh/id_ed25519.pub}
	[ -r "$pub" ] || die "no public key at $pub (SSH_PUB names another)"
	key=$(head -1 "$pub")
	case $key in ssh-* | ecdsa-*) ;; *) die "$pub is not an OpenSSH public key" ;; esac
	srv "sh -s" << EOF
set -eu
trap 'doas -n umount /mnt/abyssroot-z690a 2> /dev/null || true; doas -n rmdir /mnt/abyssroot-z690a 2> /dev/null || true' EXIT
doas -n mkdir -p /mnt/abyssroot-z690a
doas -n mount /dev/gpt/abyssroot /mnt/abyssroot-z690a
rc=/mnt/abyssroot-z690a/etc/rc.conf
if ! grep -q '^sshd_enable=' \$rc; then
	printf '%s\n' 'ifconfig_DEFAULT="DHCP"' 'sshd_enable="YES"' 'sshd_flags="-o PermitRootLogin=prohibit-password"' | doas -n tee -a \$rc > /dev/null
fi
doas -n mkdir -p /mnt/abyssroot-z690a/root/.ssh
doas -n chmod 700 /mnt/abyssroot-z690a/root/.ssh
printf '%s\n' '$key' | doas -n tee /mnt/abyssroot-z690a/root/.ssh/authorized_keys > /dev/null
doas -n chmod 600 /mnt/abyssroot-z690a/root/.ssh/authorized_keys
doas -n sync
cat \$rc
doas -n ls -la /mnt/abyssroot-z690a/root/.ssh
doas -n umount /mnt/abyssroot-z690a
EOF
	say "the root has DHCP, sshd, and your key for root"
	;;

back)
	kh=${KNOWN_HOSTS:-$HOME/.ssh/known_hosts.z690a}
	ssh -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="$kh" "root@$server" 'sh -s' << 'EOF'
set -eu
num=$(efibootmgr -v | sed -n 's/^[+ ]*Boot\([0-9A-Fa-f]*\)[* ] FreeBSD .*/\1/p' | head -1)
[ -n "$num" ] || { echo "no FreeBSD boot entry" >&2; exit 1; }
efibootmgr -n -b "$num"
efibootmgr | grep BootNext
echo "BootNext is Boot$num: rebooting"
shutdown -r now
EOF
	;;

boot-once)
	srv "sh -s" << EOF
set -eu
num=\$(doas -n efibootmgr -v | sed -n "s/^[+ ]*Boot\\([0-9A-Fa-f]*\\)[* ] $label.*/\\1/p" | head -1)
[ -n "\$num" ] || { echo "no boot entry '$label'" >&2; exit 1; }
doas -n efibootmgr -n -b \$num
doas -n efibootmgr | head -5
echo "BootNext is Boot\$num; reboot when the server can go down"
EOF
	;;
esac
