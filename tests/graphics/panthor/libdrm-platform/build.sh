#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
#
# A test harness, not part of what AbyssBSD ships: FreeBSD 15.1's libdrm 2.4.134 with the
# platform-device patch (patch-xf86drm.c, from https://github.com/JamesKane/libdrm branch
# freebsd-missing-node), built on a board into /root/harness/libdrm. Stock libdrm describes only
# PCI devices on FreeBSD, so stock Mesa cannot find a GPU that is not on PCI; this lets Mesa run
# on the Sky1's GPU to test the kernel driver. Programs get it only through
#   LD_LIBRARY_PATH=/root/harness/libdrm
# (docs/GRAPHICS.md §17.5).
#
#   build.sh libdrm-2.4.134.tar.xz root@board [known_hosts]
set -eu

tarball=$1
board=$2
hosts=${3:-$HOME/.ssh/known_hosts}
here=$(cd "$(dirname "$0")" && pwd)
want=ac5e74d157830eb8bee44c6a6bf3ad49774ef0dd2a72bdad74a8f20308b52a95

got=$(sha256sum "$tarball" | cut -d' ' -f1)
[ "$got" = "$want" ] || { echo "build.sh: $tarball: sha256 $got, want $want" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tar -C "$work" -xJf "$tarball"
src=$work/libdrm-2.4.134
patch -s -d "$src" -p0 < "$here/patch-xf86drm.c"
python3 -I "$src/gen_table_fourcc.py" "$src/include/drm/drm_fourcc.h" \
    "$src/generated_static_table_fourcc.h"
cp "$here/config.h" "$src/config.h"
tar -C "$work" -czf "$work/src.tgz" libdrm-2.4.134

ssh_="ssh -o BatchMode=yes -o UserKnownHostsFile=$hosts"
$ssh_ "$board" 'rm -rf /root/harness/libdrm-src && mkdir -p /root/harness/libdrm-src /root/harness/libdrm'
$ssh_ "$board" 'tar -C /root/harness/libdrm-src -xzf -' < "$work/src.tgz"
$ssh_ "$board" 'cd /root/harness/libdrm-src/libdrm-2.4.134 &&
    cc -O2 -fPIC -shared -fvisibility=hidden -include config.h -I. -Iinclude/drm \
        -Wl,-soname,libdrm.so.2 -o /root/harness/libdrm/libdrm.so.2 \
        xf86drm.c xf86drmHash.c xf86drmRandom.c xf86drmSL.c xf86drmMode.c &&
    ln -sf libdrm.so.2 /root/harness/libdrm/libdrm.so &&
    echo "build.sh: /root/harness/libdrm/libdrm.so.2 built"'
