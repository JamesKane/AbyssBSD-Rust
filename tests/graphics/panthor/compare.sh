#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# The same graphics measurement on the Orange Pi 6 Plus under AbyssBSD or FreeBSD, run as root
# with nothing else on the display: gl_frames (frames with no display), then Sway (two windows,
# as sway.conf) with glmark2's jellyfish for 30 s at 800x600, its frame rate logged, unpinned and
# then with Sway and its clients on the big cores (A720s: CPUs 0-1, 6-11). Results go to
# /tmp/compare/. Under AbyssBSD set LD_LIBRARY_PATH=/root/harness/libdrm first
# (libdrm-platform/build.sh); FreeBSD's libdrm port already has the platform patch.
#
# Usage: compare.sh [gl_frames binary]
GLF=${1:-./gl_frames}
OUT=/tmp/compare
BIG=0-1,6-11
# A running seatd (FreeBSD's service) is used; otherwise seatd-launch starts one, and a socket an
# earlier one left would stop it.
if pgrep -q -x seatd; then
	LAUNCH=
else
	rm -f /var/run/seatd.sock
	LAUNCH=seatd-launch
fi
# The display card: the one that sets modes (komeda is card0 under FreeBSD, card1 under AbyssBSD).
CARD=/dev/dri/card$(sysctl hw.dri | sed -n 's/^hw\.dri\.\([0-9]*\)\.modesetting: 1$/\1/p' | head -1)
mkdir -p $OUT /var/run/xdg
chmod 700 /var/run/xdg
echo "$(uname -v) display $CARD" > $OUT/system.txt

"$GLF" /dev/dri/renderD128 3000 > $OUT/gl_frames.txt 2>&1
"$GLF" /dev/dri/renderD128 2000 upload > $OUT/gl_frames-upload.txt 2>&1

run_sway() { # name, cpuset prefix (or empty)
	cat > $OUT/sway.conf <<EOF
output * bg #1d2733 solid_color
default_border pixel 2
exec foot -T fetch -e sh -c 'fastfetch; sleep 100000'
exec sh -c 'sleep 3; glmark2-es2-wayland -s 800x600 -b jellyfish:duration=30 > $OUT/$1-glmark2.txt 2>&1; swaymsg exit'
EOF
	env XDG_RUNTIME_DIR=/var/run/xdg WLR_RENDERER=gles2 WLR_NO_HARDWARE_CURSORS=1 \
	    SEATD_VTBOUND=0 LIBSEAT_BACKEND=seatd WLR_DRM_DEVICES=$CARD \
	    WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128 \
	    $2 $LAUNCH sway -c $OUT/sway.conf > $OUT/$1-sway.log 2>&1
}
run_sway unpinned ""
run_sway bigcores "cpuset -l $BIG"

echo "== $(cat $OUT/system.txt)"
for f in gl_frames gl_frames-upload; do echo "$f: $(tail -1 $OUT/$f.txt)"; done
for f in unpinned bigcores; do
	echo "jellyfish $f: $(grep -E 'FPS|Score' $OUT/$f-glmark2.txt | tr '\n' ' ')"
done
