#!/bin/bash
# SPDX-License-Identifier: BSD-2-Clause
# Images one disk of the build server onto the NAS, then checks the image.
#
# Run this yourself, on the workstation, from a terminal that stays open (or
# in tmux): it mounts the NAS share with `gio mount`, which asks YOU for the
# user name and password on the terminal. Nothing here reads, stores, or logs
# them, and the build server never sees them: the disk goes over ssh to this
# workstation, which writes it to the mounted share.
#
# What it does, in order:
#   1. Preflight on the build server (read only): the disk's serial number is
#      the expected one, nothing on it is mounted, a swap, or the boot disk.
#   2. Mounts the share and checks it has room.
#   3. Saves the disk's description (partition table, `camcontrol identify`,
#      its first MiB) beside the image.
#   4. Read 1: the whole disk, compressed, to <share>/<dir>/<disk>.img.zst.
#      `dd` stops at the first read error, so a clean exit means every sector
#      was read.
#   5. Read 2: the whole disk again, hashed on the server (SHA-256).
#   6. Check: the image decompressed here must have the disk's size and hash.
# Nothing writes to the disk. Nothing is deleted anywhere.
#
# Usage: tools/nas/image-disk.sh [--dry-run] [--disk ada0]
#            [--serial BTDA3330088F2403GN] [--server 192.168.0.4]
#            [--share smb://kane-nas.local/nas/] [--dir images/abyss-build-intel-ssd]
#            [--force]
#   --dry-run  the preflight and the plan only: no mount, no reads of the disk.
#   --force    run although a perf run (perf-board.sh) is going: the copy uses
#              the server's CPU and network, which that run shares.
set -eu
disk=ada0
serial=BTDA3330088F2403GN
server=192.168.0.4
share=smb://kane-nas.local/nas/
dir=images/abyss-build-intel-ssd-$(date +%Y-%m-%d)
dry=0
force=0
while [ $# -gt 0 ]; do
	case $1 in
	--dry-run) dry=1 ;;
	--force) force=1 ;;
	--disk) disk=$2; shift ;;
	--serial) serial=$2; shift ;;
	--server) server=$2; shift ;;
	--share) share=$2; shift ;;
	--dir) dir=$2; shift ;;
	-h | --help) sed -n '2,/^set -eu/p' "$0" | sed 's/^# \{0,1\}//; /^set -eu/d'; exit 0 ;;
	*) echo "unknown option $1" >&2; exit 2 ;;
	esac
	shift
done

# The server by address (DHCP moves it) under the name ~/.ssh/config knows.
srv() { ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o HostName="$server" abyss-build "$@"; }
say() { echo "$(date +%T) $*"; }
die() { echo "image-disk: $*" >&2; exit 1; }

# 1. The server.
say "preflight on $server"
if [ "$force" = 0 ] && pgrep -f perf-board.sh > /dev/null; then
	die "a perf run is going (perf-board.sh): wait for it, or use --force"
fi
srv "test -c /dev/$disk" || die "no /dev/$disk on $server"
ident=$(srv "doas -n camcontrol identify $disk" 2>&1) || die "camcontrol identify failed: $ident"
got=$(echo "$ident" | sed -n 's/^serial number[[:space:]]*//p' | head -1 | tr -d '[:space:]')
[ "$got" = "$serial" ] || die "/dev/$disk has serial '$got', not '$serial'"
srv "mount | grep -q '/dev/$disk'" && die "something on $disk is mounted"
srv "swapinfo | grep -q '/dev/$disk'" && die "$disk is a swap device"
bytes=$(srv "doas -n diskinfo $disk" | awk '{print $3}')
[ -n "$bytes" ] || die "no size for $disk"
# One MiB, to see that the server may read the disk (as root, with doas).
got_mib=$(srv "doas -n dd if=/dev/$disk bs=1m count=1 2> /dev/null" | wc -c)
[ "$got_mib" = 1048576 ] || die "a test read of $disk gave $got_mib bytes, not 1048576"
command -v zstd > /dev/null || die "zstd is not installed here"
srv "command -v zstd > /dev/null" || die "zstd is not installed on $server"
say "$disk is $serial, $bytes bytes, not in use"
srv "doas -n gpart show -p $disk" || true
if [ "$dry" = 1 ]; then
	say "dry run: would mount $share, write $dir/$disk.img.zst there, hash the disk, and check the image"
	exit 0
fi

# 2. The share. `gio mount` asks for the credentials on this terminal.
gvfs=/run/user/$(id -u)/gvfs
host=${share#smb://}
host=${host%%/*}
name=${share#smb://*/}
name=${name%/}
path=$gvfs/smb-share:server=$host,share=$name
if [ ! -d "$path" ]; then
	say "mounting $share (enter the user name and password when asked)"
	gio mount "$share"
fi
[ -d "$path" ] || die "$path is not there after the mount (is gvfs-fuse running?)"
mkdir -p "$path/$dir"
avail=$(df -B1 --output=avail "$path" 2> /dev/null | tail -1 | tr -d ' ')
say "share has ${avail:-unknown} bytes free; the disk is $bytes (the image will be smaller)"
if [ -n "${avail:-}" ] && [ "$avail" -lt $((bytes / 2)) ]; then
	die "less than half the disk's size is free on the share"
fi
out=$path/$dir

# 3. What it is.
say "saving the disk's description to $dir"
{
	echo "disk $disk, serial $serial, $bytes bytes, imaged $(date -u +%FT%TZ) from $server"
	echo
	srv "doas -n gpart show -p $disk"
	echo
	echo "$ident"
} > "$out/$disk.txt"
srv "doas -n dd if=/dev/$disk bs=1m count=1 2> /dev/null" > "$out/$disk.first-mib.bin"

# 4. Read 1.
say "read 1: $disk to $dir/$disk.img.zst (compressed; this takes a while)"
srv "doas -n dd if=/dev/$disk bs=1m 2> /dev/null | nice -n 19 zstd -T2 -6 -c" > "$out/$disk.img.zst"
ls -l "$out/$disk.img.zst"

# 5. Read 2.
say "read 2: hashing $disk on the server"
srv "doas -n dd if=/dev/$disk bs=1m 2> /dev/null | sha256 -q" > "$out/$disk.raw.sha256"
disk_hash=$(tr -d '[:space:]' < "$out/$disk.raw.sha256")
say "the disk hashes to $disk_hash"

# 6. The check.
say "check: decompressing the image here"
tmp=$(mktemp -d)
mkfifo "$tmp/f"
wc -c < "$tmp/f" > "$tmp/size" &
counter=$!
hash=$(zstdcat "$out/$disk.img.zst" | tee "$tmp/f" | sha256sum | cut -d' ' -f1)
wait "$counter"
size=$(tr -d '[:space:]' < "$tmp/size")
rm -rf "$tmp"
[ "$size" = "$bytes" ] || die "the image decompresses to $size bytes, the disk is $bytes"
[ "$hash" = "$disk_hash" ] || die "the image's hash $hash is not the disk's $disk_hash"
{
	echo "$disk_hash  $disk.raw (the decompressed $disk.img.zst)"
} > "$out/$disk.sha256"
cat > "$out/README.txt" << EOF
$disk of the AbyssBSD build server (MSI PRO Z690-A), serial $serial, $bytes bytes.
Imaged $(date -u +%FT%TZ). The image is zstd-compressed and byte for byte the disk.
Restore to a disk of the same size or larger (careful: the target is overwritten):
  zstdcat $disk.img.zst | sudo dd of=/dev/sdX bs=1M status=progress
Check: zstdcat $disk.img.zst | sha256sum   should print the hash in $disk.sha256.
$disk.txt has the partition table; $disk.first-mib.bin the first MiB (the MBR).
EOF
say "done: the image has the disk's size and hash ($disk_hash)"
say "nothing was written to $disk. The share stays mounted: gio mount -u $share"
